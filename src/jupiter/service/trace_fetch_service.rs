//! Request-local, authorized raw objects. Protocol framing belongs to the caller.

use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::trace_object_service::{RawObjectType, RawReadControl, TraceObjectService};
use crate::{
    callisto::trace_object,
    ceres::{
        pack::traces_repo::TraceFetchGrant,
        traces::ingest::{RawTreeMode, verified_commit_links, verified_tree_entries},
    },
    common::errors::{MegaError, TracesError},
};

fn io_error(_: std::io::Error) -> MegaError {
    TracesError::QueryTimeout.into()
}

fn integrity() -> MegaError {
    TracesError::ObjectIntegrity.into()
}

fn stored_syntax_error(error: MegaError) -> MegaError {
    match error {
        MegaError::Traces(TracesError::InvalidParameter) => integrity(),
        other => other,
    }
}

struct FetchControl(CancellationToken);

impl RawReadControl for FetchControl {
    fn check(&self) -> Result<(), MegaError> {
        if self.0.is_cancelled() {
            Err(TracesError::QueryTimeout.into())
        } else {
            Ok(())
        }
    }

    async fn checkpoint(&self) -> Result<(), MegaError> {
        self.check()?;
        tokio::task::yield_now().await;
        self.check()
    }

    async fn run<T: Send>(
        &self,
        future: impl Future<Output = Result<T, MegaError>> + Send,
    ) -> Result<T, MegaError> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.0.cancelled() => Err(TracesError::QueryTimeout.into()),
            value = future => {
                self.check()?;
                value
            }
        }
    }
}

fn private_dir(path: &Path) -> Result<(), MegaError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io_error)
}

fn private_file(path: &Path) -> Result<File, MegaError> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(io_error)
}

struct Spool {
    path: PathBuf,
}

impl Spool {
    fn new() -> Result<Self, MegaError> {
        let path = std::env::temp_dir().join(format!("mega2-traces-fetch-{}", Uuid::new_v4()));
        private_dir(&path)?;
        Ok(Self { path })
    }

    fn node(&self, oid: &str) -> PathBuf {
        self.path.join(oid)
    }

    fn state(&self, oid: &str) -> Result<Option<(u8, RawObjectType)>, MegaError> {
        match File::open(self.node(oid).join("state")) {
            Ok(mut file) => {
                let mut bytes = [0; 2];
                file.read_exact(&mut bytes).map_err(io_error)?;
                Ok(Some((bytes[0], decode_type(bytes[1])?)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    fn visit(&self, frame: &Frame) -> Result<bool, MegaError> {
        if let Some((state, kind)) = self.state(&frame.oid)? {
            if kind != frame.kind {
                return Err(integrity());
            }
            if frame.leave && state == 1 {
                let mut state_file = OpenOptions::new()
                    .write(true)
                    .open(self.node(&frame.oid).join("state"))
                    .map_err(io_error)?;
                state_file.write_all(&[2]).map_err(io_error)?;
            } else if state != 2 {
                return Err(integrity());
            }
            return Ok(true);
        }
        if frame.leave {
            return Err(integrity());
        }
        Ok(false)
    }

    fn mark(&self, oid: &str, name: &str) -> Result<bool, MegaError> {
        let path = self.node(oid).join(name);
        if path.try_exists().map_err(io_error)? {
            return Ok(false);
        }
        private_file(&path)?;
        Ok(true)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        // All handles are request-owned. Failed cleanup is visible without
        // including paths, object IDs, or payloads in a log message.
        if fs::remove_dir_all(&self.path).is_err() {
            tracing::warn!(
                reason = "fetch_spool_cleanup_failed",
                "traces fetch cleanup failed"
            );
        }
    }
}

fn encode_type(kind: RawObjectType) -> u8 {
    match kind {
        RawObjectType::Commit => 1,
        RawObjectType::Tree => 2,
        RawObjectType::Blob => 3,
        RawObjectType::Tag => 4,
    }
}

fn decode_type(kind: u8) -> Result<RawObjectType, MegaError> {
    match kind {
        1 => Ok(RawObjectType::Commit),
        2 => Ok(RawObjectType::Tree),
        3 => Ok(RawObjectType::Blob),
        4 => Ok(RawObjectType::Tag),
        _ => Err(integrity()),
    }
}

#[derive(Default)]
struct ResidentFrames {
    current: AtomicUsize,
    peak: AtomicUsize,
}

// Fixed-size records keep the in-memory frontier independent of graph size.
struct Frame {
    oid: String,
    kind: RawObjectType,
    leave: bool,
    residency: Arc<ResidentFrames>,
}

impl Frame {
    fn new(oid: String, kind: RawObjectType, leave: bool, residency: &Arc<ResidentFrames>) -> Self {
        let count = residency.current.fetch_add(1, Ordering::Relaxed) + 1;
        residency.peak.fetch_max(count, Ordering::Relaxed);
        Self {
            oid,
            kind,
            leave,
            residency: residency.clone(),
        }
    }

    fn write(&self, file: &mut File) -> Result<(), MegaError> {
        file.write_all(self.oid.as_bytes()).map_err(io_error)?;
        file.write_all(&[encode_type(self.kind), u8::from(self.leave)])
            .map_err(io_error)
    }

    fn read(
        file: &mut File,
        width: usize,
        residency: &Arc<ResidentFrames>,
    ) -> Result<Option<Self>, MegaError> {
        let mut record = [0; 66];
        let read = file.read(&mut record[..1]).map_err(io_error)?;
        if read == 0 {
            return Ok(None);
        }
        file.read_exact(&mut record[1..width + 2])
            .map_err(io_error)?;
        let oid = std::str::from_utf8(&record[..width])
            .map_err(|_| integrity())?
            .to_owned();
        if !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) || record[width + 1] > 1 {
            return Err(integrity());
        }
        Ok(Some(Self::new(
            oid,
            decode_type(record[width])?,
            record[width + 1] == 1,
            residency,
        )))
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.residency.current.fetch_sub(1, Ordering::Relaxed);
    }
}

struct DiskStack {
    file: File,
    width: usize,
    len: u64,
    high_water: u64,
    residency: Arc<ResidentFrames>,
}

impl DiskStack {
    fn new(path: &Path, width: usize, residency: &Arc<ResidentFrames>) -> Result<Self, MegaError> {
        Ok(Self {
            file: private_file(path)?,
            width,
            len: 0,
            high_water: 0,
            residency: residency.clone(),
        })
    }

    fn push(&mut self, frame: &Frame) -> Result<(), MegaError> {
        self.file.seek(SeekFrom::End(0)).map_err(io_error)?;
        frame.write(&mut self.file)?;
        self.len = self.len.checked_add(1).ok_or_else(integrity)?;
        self.high_water = self.high_water.max(self.len);
        Ok(())
    }

    fn pop(&mut self) -> Result<Option<Frame>, MegaError> {
        if self.len == 0 {
            return Ok(None);
        }
        self.len -= 1;
        let offset = self
            .len
            .checked_mul((self.width + 2) as u64)
            .ok_or_else(integrity)?;
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let frame =
            Frame::read(&mut self.file, self.width, &self.residency)?.ok_or_else(integrity)?;
        self.file.set_len(offset).map_err(io_error)?;
        Ok(Some(frame))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FetchWalkStats {
    pub(crate) raw_reads: u64,
    pub(crate) raw_bytes: u64,
    pub(crate) max_object_bytes: usize,
    pub(crate) disk_frontier_high_water: u64,
    /// Measured live Frame high water; excludes per-object parser buffers and RSS.
    pub(crate) peak_resident_frames: usize,
}

pub(crate) struct FetchRawObject {
    pub(crate) oid: String,
    pub(crate) object_type: RawObjectType,
    pub(crate) payload: Vec<u8>,
}

/// Owns the spool and never spawns a detached reader. Each next() is backpressured
/// by the consumer; cancellation, error and EOF close this request's resources.
pub(crate) struct TraceFetchStream {
    result: Option<File>,
    spool: Option<Spool>,
    control: FetchControl,
    count: u64,
    remaining: u64,
    width: usize,
    stats: FetchWalkStats,
    residency: Arc<ResidentFrames>,
}

impl TraceFetchStream {
    pub(crate) fn object_count(&self) -> u64 {
        self.count
    }

    pub(crate) fn stats(&self) -> FetchWalkStats {
        let mut stats = self.stats;
        stats.peak_resident_frames = self.residency.peak.load(Ordering::Relaxed);
        stats
    }

    fn close(&mut self) {
        self.result.take();
        self.spool.take();
    }

    pub(crate) async fn next(&mut self) -> Result<Option<FetchRawObject>, MegaError> {
        let result = self.next_inner().await;
        if result.is_err() || matches!(&result, Ok(None)) || self.remaining == 0 {
            self.close();
        }
        result
    }

    async fn next_inner(&mut self) -> Result<Option<FetchRawObject>, MegaError> {
        self.control.checkpoint().await?;
        if self.remaining == 0 {
            return Ok(None);
        }
        let result = self.result.as_mut().ok_or_else(integrity)?;
        let frame = Frame::read(result, self.width, &self.residency)?.ok_or_else(integrity)?;
        let spool = self.spool.as_ref().ok_or_else(integrity)?;
        let mut file = File::open(spool.node(&frame.oid).join("payload")).map_err(io_error)?;
        let size =
            usize::try_from(file.metadata().map_err(io_error)?.len()).map_err(|_| integrity())?;
        if size as u64 > crate::config::TracesProtocolLimits::MAX_OBJECT_BYTES {
            return Err(integrity());
        }
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(size)
            .map_err(|_| MegaError::Traces(TracesError::ResponseBudget))?;
        let mut chunk = [0; 64 * 1024];
        loop {
            self.control.checkpoint().await?;
            let n = file.read(&mut chunk).map_err(io_error)?;
            if n == 0 {
                break;
            }
            if payload.len().checked_add(n).is_none_or(|len| len > size) {
                return Err(integrity());
            }
            payload.extend_from_slice(&chunk[..n]);
        }
        if payload.len() != size {
            return Err(integrity());
        }
        self.remaining -= 1;
        Ok(Some(FetchRawObject {
            oid: frame.oid.clone(),
            object_type: frame.kind,
            payload,
        }))
    }
}

impl Drop for TraceFetchStream {
    fn drop(&mut self) {
        self.close();
    }
}

fn canonical_oid(oid: &str, width: usize) -> Result<String, MegaError> {
    if oid.len() != width || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TracesError::InvalidParameter.into());
    }
    Ok(oid.to_ascii_lowercase())
}

/// The grant is consumed, so it cannot be reused by another request.
pub(crate) async fn fetch_raw_objects(
    grant: TraceFetchGrant,
    objects: &TraceObjectService,
    wants: &[String],
    haves: &[String],
    cancel: CancellationToken,
) -> Result<TraceFetchStream, MegaError> {
    let control = FetchControl(cancel);
    control.check()?;
    fetch_into_spool(grant, objects, wants, haves, control, Spool::new()?).await
}

async fn fetch_into_spool(
    grant: TraceFetchGrant,
    objects: &TraceObjectService,
    wants: &[String],
    haves: &[String],
    control: FetchControl,
    spool: Spool,
) -> Result<TraceFetchStream, MegaError> {
    tracing::debug!(event = "fetch_raw_start", "traces fetch started");
    control.check()?;
    let width = grant.hash_kind().hex_len();
    let mut result = private_file(&spool.path.join("result"))?;
    let residency = Arc::new(ResidentFrames::default());
    let mut stats = FetchWalkStats::default();
    let mut count = 0u64;
    if !wants.is_empty() {
        let root = grant.tip_oid().ok_or(TracesError::UnknownScope)?;
        let mut stack = DiskStack::new(&spool.path.join("walk"), width, &residency)?;
        stack.push(&Frame::new(
            canonical_oid(root, width)?,
            RawObjectType::Commit,
            false,
            &residency,
        ))?;
        while let Some(frame) = stack.pop()? {
            control.checkpoint().await?;

            if spool.visit(&frame)? {
                continue;
            }
            let member = control
                .run(async {
                    trace_object::Entity::find()
                        .filter(trace_object::Column::StreamId.eq(grant.stream_id()))
                        .filter(trace_object::Column::HashKind.eq(grant.hash_kind().as_str()))
                        .filter(trace_object::Column::Oid.eq(&frame.oid))
                        .filter(trace_object::Column::Visible.eq(true))
                        .one(grant.db())
                        .await
                        .map_err(|_| MegaError::Traces(TracesError::QueryTimeout))
                })
                .await?;
            if member.is_none() {
                return Err(integrity());
            }
            let raw = objects
                .read_verified_raw(
                    grant.stream_id(),
                    grant.hash_kind().as_str(),
                    &frame.oid,
                    &control,
                )
                .await?
                .ok_or_else(integrity)?;
            stats.raw_reads = stats.raw_reads.checked_add(1).ok_or_else(integrity)?;
            stats.raw_bytes = stats
                .raw_bytes
                .checked_add(raw.bytes().len() as u64)
                .ok_or_else(integrity)?;
            stats.max_object_bytes = stats.max_object_bytes.max(raw.bytes().len());
            if raw.object_type() != frame.kind {
                return Err(integrity());
            }
            let node = spool.node(&frame.oid);
            private_dir(&node)?;
            private_file(&node.join("state"))?
                .write_all(&[1, encode_type(frame.kind)])
                .map_err(io_error)?;
            let mut payload = private_file(&node.join("payload"))?;
            for chunk in raw.bytes().chunks(64 * 1024) {
                control.checkpoint().await?;
                payload.write_all(chunk).map_err(io_error)?;
            }
            let mut edges = private_file(&node.join("edges"))?;
            stack.push(&Frame::new(frame.oid.clone(), frame.kind, true, &residency))?;
            match frame.kind {
                RawObjectType::Commit => {
                    let links = verified_commit_links(raw.bytes(), width, &control)
                        .map_err(stored_syntax_error)?;
                    for oid in std::iter::once(links.tree).chain(links.parents) {
                        control.checkpoint().await?;
                        // The first edge is the tree; subsequent edges are parents.
                        let kind = if edges.stream_position().map_err(io_error)? == 0 {
                            RawObjectType::Tree
                        } else {
                            RawObjectType::Commit
                        };
                        let edge = Frame::new(oid, kind, false, &residency);
                        edge.write(&mut edges)?;

                        stack.push(&edge)?;
                    }
                }
                RawObjectType::Tree => {
                    for entry in verified_tree_entries(raw.bytes(), width / 2, &control)
                        .map_err(stored_syntax_error)?
                    {
                        control.checkpoint().await?;
                        let kind = match entry.mode {
                            RawTreeMode::Tree => RawObjectType::Tree,
                            RawTreeMode::Blob => RawObjectType::Blob,
                            RawTreeMode::Gitlink => continue,
                        };
                        let edge = Frame::new(entry.oid, kind, false, &residency);
                        edge.write(&mut edges)?;

                        stack.push(&edge)?;
                    }
                }
                RawObjectType::Blob => {}
                RawObjectType::Tag => return Err(integrity()),
            }
        }
        stats.disk_frontier_high_water = stack.high_water;
        drop(stack);
        // Complete root validation precedes have subtraction and want selection.
        // Haves can only traverse this already-authorized on-disk graph.
        for (ids, marker, is_want) in [(haves, "excluded", false), (wants, "selected", true)] {
            for raw_oid in ids {
                control.checkpoint().await?;
                let oid = match canonical_oid(raw_oid, width) {
                    Ok(oid) => oid,
                    Err(error) if is_want => return Err(error),
                    Err(_) => continue,
                };
                let Some((2, kind)) = spool.state(&oid)? else {
                    if is_want {
                        return Err(TracesError::UnknownScope.into());
                    }
                    continue;
                };
                count = select_closure(
                    &spool,
                    &control,
                    Frame::new(oid, kind, false, &residency),
                    marker,
                    width,
                    &mut result,
                    count,
                )
                .await?;
            }
        }
    }
    result.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut stream = TraceFetchStream {
        result: Some(result),
        spool: Some(spool),
        control,
        count,
        remaining: count,
        width,
        stats,
        residency,
    };
    if count == 0 {
        stream.close();
    }
    Ok(stream)
}

async fn select_closure(
    spool: &Spool,
    control: &FetchControl,
    root: Frame,
    marker: &str,
    width: usize,
    result: &mut File,
    mut count: u64,
) -> Result<u64, MegaError> {
    let residency = root.residency.clone();
    let selected = marker == "selected";
    let stack_path = spool.path.join("selection");
    let mut stack = DiskStack::new(&stack_path, width, &residency)?;
    stack.push(&root)?;
    while let Some(frame) = stack.pop()? {
        control.checkpoint().await?;
        if selected
            && spool
                .node(&frame.oid)
                .join("excluded")
                .try_exists()
                .map_err(io_error)?
        {
            continue;
        }
        if !spool.mark(&frame.oid, marker)? {
            continue;
        }
        if selected {
            frame.write(result)?;
            count = count.checked_add(1).ok_or_else(integrity)?;
        }
        let mut edges = File::open(spool.node(&frame.oid).join("edges")).map_err(io_error)?;
        while let Some(edge) = Frame::read(&mut edges, width, &residency)? {
            control.checkpoint().await?;
            stack.push(&edge)?;
        }
    }
    drop(stack);
    fs::remove_file(stack_path).map_err(io_error)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use axum::{
        Router,
        body::Body,
        extract::{Request, State},
        response::Response,
    };
    use chrono::Utc;
    use git_internal::hash::HashKind;
    use sea_orm::{ActiveModelTrait, Set};
    use tracing::instrument::WithSubscriber;

    use super::*;
    use crate::{
        callisto::trace_stream,
        ceres::{
            pack::traces_repo::{resolve_trace_fetch, tests::DiscoveryFixture},
            traces::publication::tests::Fixture,
        },
        contract::git_protocol::path::TracesLocator,
        jupiter::storage::{
            object_storage::MegaObjectStorageWrapper, trace_object_storage::object_key,
        },
        orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig, S3Config},
    };

    #[derive(Clone)]
    struct ProxyState {
        client: reqwest::Client,
        upstream: String,
        gets: Arc<Mutex<Vec<String>>>,
        blocked: Arc<AtomicBool>,
        entered: Arc<AtomicUsize>,
        release: CancellationToken,
    }

    async fn proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        if parts.method == axum::http::Method::GET {
            state.gets.lock().unwrap().push(parts.uri.path().to_owned());
            if state.blocked.load(Ordering::Acquire) {
                state.entered.fetch_add(1, Ordering::AcqRel);
                state.release.cancelled().await;
            }
        }
        let bytes = axum::body::to_bytes(body, 70 * 1024 * 1024).await.unwrap();
        let response = state
            .client
            .request(parts.method, format!("{}{}", state.upstream, parts.uri))
            .headers(parts.headers)
            .body(bytes)
            .send()
            .await
            .unwrap();
        let mut builder = Response::builder().status(response.status());
        *builder.headers_mut().unwrap() = response.headers().clone();
        builder
            .body(Body::from_stream(response.bytes_stream()))
            .unwrap()
    }

    struct FetchFixture {
        auth: DiscoveryFixture,
        git: Fixture,
        store: MegaObjectStorageWrapper,
        objects: TraceObjectService,
        proxy: ProxyState,
        shutdown: CancellationToken,
        server: Option<tokio::task::JoinHandle<()>>,
    }

    impl FetchFixture {
        async fn new(kind: HashKind) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let proxy_state = ProxyState {
                client: reqwest::Client::new(),
                upstream: std::env::var("MEGA2_IT_RUSTFS_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:19000".into()),
                gets: Arc::new(Mutex::new(Vec::new())),
                blocked: Arc::new(AtomicBool::new(false)),
                entered: Arc::new(AtomicUsize::new(0)),
                release: CancellationToken::new(),
            };
            let shutdown = CancellationToken::new();
            let stop = shutdown.clone();
            let app = Router::new()
                .fallback(proxy)
                .with_state(proxy_state.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
                    .unwrap();
            });
            let store = crate::orbit::factory::ObjectStorageFactory::build(&ObjectStorageConfig {
                storage_type: ObjectStorageBackend::S3Compatible,
                s3: S3Config {
                    region: "us-east-1".into(),
                    bucket: "mega2".into(),
                    access_key_id: "rustfs".into(),
                    secret_access_key: "rustfs_secret".into(),
                    endpoint_url: endpoint,
                    public_endpoint_url: String::new(),
                },
                ..Default::default()
            })
            .await
            .expect("fixed local RustFS is required");
            Self {
                auth: DiscoveryFixture::new(kind, true).await,
                git: Fixture::new(kind),
                objects: TraceObjectService::new(store.clone()),
                store,
                proxy: proxy_state,
                shutdown,
                server: Some(server),
            }
        }

        async fn save(&self, tip: &str) {
            for (oid, _, bytes) in &self.git.objects {
                self.objects
                    .put_raw(&self.auth.stream_id, self.git.hash_kind(), oid, bytes)
                    .await
                    .unwrap();
                let present = trace_object::Entity::find()
                    .filter(trace_object::Column::StreamId.eq(&self.auth.stream_id))
                    .filter(trace_object::Column::Oid.eq(oid))
                    .one(&self.auth.db)
                    .await
                    .unwrap();
                if present.is_none() {
                    trace_object::ActiveModel {
                        id: Set(Uuid::new_v4().to_string()),
                        stream_id: Set(self.auth.stream_id.clone()),
                        hash_kind: Set(self.git.hash_kind().into()),
                        oid: Set(oid.clone()),
                        visible: Set(true),
                        created_at: Set(Utc::now().fixed_offset()),
                    }
                    .insert(&self.auth.db)
                    .await
                    .unwrap();
                }
            }
            self.tip(tip).await;
        }

        async fn tip(&self, oid: &str) {
            trace_stream::Entity::update_many()
                .col_expr(
                    trace_stream::Column::TipOid,
                    sea_orm::sea_query::Expr::value(oid),
                )
                .filter(trace_stream::Column::Id.eq(&self.auth.stream_id))
                .exec(&self.auth.db)
                .await
                .unwrap();
        }

        async fn grant(&self) -> TraceFetchGrant {
            resolve_trace_fetch(
                &self.auth.db,
                &self.auth.config,
                &TracesLocator::ByScope {
                    stream_id: self.auth.stream_id.clone(),
                },
                self.auth.caller(),
                None,
            )
            .await
            .unwrap()
        }

        async fn fetch(
            &self,
            wants: &[String],
            haves: &[String],
        ) -> Result<TraceFetchStream, MegaError> {
            self.proxy.gets.lock().unwrap().clear();
            fetch_raw_objects(
                self.grant().await,
                &self.objects,
                wants,
                haves,
                CancellationToken::new(),
            )
            .await
        }

        fn assert_reads(&self, expected: usize) {
            let gets = self.proxy.gets.lock().unwrap();
            assert_eq!(
                gets.len(),
                expected,
                "actual GET requests at the RustFS boundary"
            );
            let unique: std::collections::BTreeSet<_> = gets.iter().collect();
            assert_eq!(
                unique.len(),
                expected,
                "each reachable OID read at most once"
            );
        }

        async fn cleanup(mut self) {
            self.proxy.release.cancel();
            for (oid, _, _) in &self.git.objects {
                self.store
                    .inner
                    .delete(&object_key(&self.auth.stream_id, self.git.hash_kind(), oid).unwrap())
                    .await
                    .unwrap();
            }
            self.shutdown.cancel();
            self.server.take().unwrap().await.unwrap();
        }
    }

    impl Drop for FetchFixture {
        fn drop(&mut self) {
            self.proxy.release.cancel();
            self.shutdown.cancel();
        }
    }

    async fn collect(stream: &mut TraceFetchStream, git: &Fixture) -> BTreeMap<String, Vec<u8>> {
        let count = stream.object_count();
        let mut output = BTreeMap::new();
        while let Some(object) = stream.next().await.unwrap() {
            assert_eq!(
                object.payload,
                git.git(&["cat-file", object.object_type.as_str(), &object.oid], b"")
            );
            let rehashed = git.git(
                &["hash-object", "-t", object.object_type.as_str(), "--stdin"],
                &object.payload,
            );
            assert_eq!(String::from_utf8(rehashed).unwrap().trim(), object.oid);
            assert!(output.insert(object.oid, object.payload).is_none());
        }
        assert_eq!(output.len() as u64, count);
        output
    }

    fn rejected<T>(result: Result<T, MegaError>, expected: TracesError) -> MegaError {
        match result {
            Err(MegaError::Traces(actual)) => {
                assert_eq!(actual.code(), expected.code());
                assert_eq!(actual.http_status(), expected.http_status());
                MegaError::Traces(actual)
            }
            Err(error) => panic!("unexpected untyped error: {error}"),
            Ok(_) => panic!("unexpected fetch grant"),
        }
    }

    #[tokio::test]
    async fn scoped_raw_fetch_original_bytes_exact_closures_both_hashes() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut f = FetchFixture::new(kind).await;
            let blob = f
                .git
                .object("blob", b"original\0\xffsecret-fixture-marker\n");
            let tree = f.git.tree(&[
                ("100644", b"a", &blob),
                ("100755", b"b", &blob),
                ("160000", b"submodule", &"e".repeat(kind.hex_len())),
            ]);
            let first = f.git.commit(&tree, None);
            let second = f.git.commit(&tree, Some(&first));
            f.save(&second).await;
            let expected: BTreeMap<_, _> = f
                .git
                .objects
                .iter()
                .map(|(oid, _, bytes)| (oid.clone(), bytes.clone()))
                .collect();
            let mut all = f
                .fetch(
                    &[second.clone(), second.clone(), blob.clone()],
                    &["not-an-oid".into(), "f".repeat(kind.hex_len())],
                )
                .await
                .unwrap();
            assert_eq!(collect(&mut all, &f.git).await, expected);
            f.assert_reads(expected.len());
            let mut delta = f
                .fetch(std::slice::from_ref(&second), std::slice::from_ref(&first))
                .await
                .unwrap();
            let output = collect(&mut delta, &f.git).await;
            assert_eq!(output.keys().cloned().collect::<Vec<_>>(), vec![second]);
            f.assert_reads(expected.len());
            let mut trees = f.fetch(std::slice::from_ref(&tree), &[]).await.unwrap();
            assert_eq!(
                collect(&mut trees, &f.git)
                    .await
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                [tree, blob.clone()].into_iter().collect()
            );
            let mut blobs = f.fetch(std::slice::from_ref(&blob), &[]).await.unwrap();
            assert_eq!(collect(&mut blobs, &f.git).await.len(), 1);
            let mut empty = f.fetch(&[], &[first]).await.unwrap();
            assert_eq!(empty.object_count(), 0);
            assert!(empty.next().await.unwrap().is_none());
            assert!(empty.spool.is_none());
            f.assert_reads(0);
            f.cleanup().await;
        }
    }

    #[tokio::test]
    async fn scoped_raw_fetch_fixed_tip_unreachable_residue_and_cross_scope() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut f = FetchFixture::new(kind).await;
            let blob = f.git.object("blob", b"old");
            let tree = f.git.tree(&[("100644", b"a", &blob)]);
            let old = f.git.commit(&tree, None);
            f.save(&old).await;
            let grant = f.grant().await;
            let new = f.git.commit(&tree, Some(&old));
            let unreachable = f.git.object("blob", b"visible but unreachable");
            let residue = f.git.object("blob", b"raw without membership");
            f.save(&new).await;
            trace_object::Entity::delete_many()
                .filter(trace_object::Column::StreamId.eq(&f.auth.stream_id))
                .filter(trace_object::Column::Oid.eq(&residue))
                .exec(&f.auth.db)
                .await
                .unwrap();
            rejected(
                fetch_raw_objects(
                    grant,
                    &f.objects,
                    std::slice::from_ref(&new),
                    &[],
                    CancellationToken::new(),
                )
                .await,
                TracesError::UnknownScope,
            );
            let mut other = FetchFixture::new(kind).await;
            let foreign = other.git.object("blob", b"foreign namespace");
            let ft = other.git.tree(&[("100644", b"x", &foreign)]);
            let fc = other.git.commit(&ft, None);
            other.save(&fc).await;
            for invalid in [&unreachable, &residue, &foreign] {
                rejected(
                    f.fetch(std::slice::from_ref(invalid), &[]).await,
                    TracesError::UnknownScope,
                );
                f.assert_reads(4);
            }
            let mut stream = f
                .fetch(std::slice::from_ref(&new), &[unreachable, residue, foreign])
                .await
                .unwrap();
            assert_eq!(collect(&mut stream, &f.git).await.len(), 4);
            f.assert_reads(4);
            other.cleanup().await;
            f.cleanup().await;
        }
    }

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);
    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn scoped_raw_fetch_missing_corrupt_hidden_and_private_errors() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut f = FetchFixture::new(kind).await;
            let secret = b"sensitive-raw-fixture-marker";
            let blob = f.git.object("blob", secret);
            let tree = f.git.tree(&[("100644", b"private", &blob)]);
            let tip = f.git.commit(&tree, None);
            f.save(&tip).await;
            trace_object::Entity::update_many()
                .col_expr(
                    trace_object::Column::Visible,
                    sea_orm::sea_query::Expr::value(false),
                )
                .filter(trace_object::Column::StreamId.eq(&f.auth.stream_id))
                .filter(trace_object::Column::Oid.eq(&blob))
                .exec(&f.auth.db)
                .await
                .unwrap();
            rejected(
                f.fetch(std::slice::from_ref(&tip), &[]).await,
                TracesError::ObjectIntegrity,
            );
            trace_object::Entity::update_many()
                .col_expr(
                    trace_object::Column::Visible,
                    sea_orm::sea_query::Expr::value(true),
                )
                .filter(trace_object::Column::StreamId.eq(&f.auth.stream_id))
                .filter(trace_object::Column::Oid.eq(&blob))
                .exec(&f.auth.db)
                .await
                .unwrap();
            let key = object_key(&f.auth.stream_id, f.git.hash_kind(), &blob).unwrap();
            f.store.inner.delete(&key).await.unwrap();
            rejected(
                f.fetch(std::slice::from_ref(&tip), &[]).await,
                TracesError::ObjectIntegrity,
            );
            f.objects
                .put_raw(
                    &f.auth.stream_id,
                    f.git.hash_kind(),
                    &blob,
                    b"corrupt sensitive-raw-fixture-marker",
                )
                .await
                .unwrap();
            let logs = LogCapture(Arc::new(Mutex::new(Vec::new())));
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .with_target(true)
                .finish();
            let error = rejected(
                f.fetch(std::slice::from_ref(&tip), &[])
                    .with_subscriber(subscriber)
                    .await,
                TracesError::ObjectIntegrity,
            );
            let text = format!("{error} {error:?}");
            assert!(
                String::from_utf8(logs.0.lock().unwrap().clone())
                    .unwrap()
                    .contains("fetch_raw_start")
            );
            for marker in [
                String::from_utf8(secret.to_vec()).unwrap(),
                blob.clone(),
                key.key.clone(),
            ] {
                assert!(!text.contains(&marker));
                // Only this card's own events define its logging contract.
                for line in String::from_utf8(logs.0.lock().unwrap().clone())
                    .unwrap()
                    .lines()
                    .filter(|line| {
                        line.contains("trace_fetch_service")
                            || line.contains("trace_object_service")
                    })
                {
                    assert!(!line.contains(&marker));
                }
            }
            f.cleanup().await;
        }
    }

    #[tokio::test]
    async fn scoped_fetch_walk_disk_frontier_and_once_reads_scale_with_history() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut maxima = Vec::new();
            for commits in [16, 320] {
                let mut f = FetchFixture::new(kind).await;
                let blob = f.git.object("blob", b"shared");
                let tree = f
                    .git
                    .tree(&[("100644", b"a", &blob), ("100644", b"b", &blob)]);
                let mut tip = f.git.commit(&tree, None);
                for _ in 1..commits {
                    tip = f.git.commit(&tree, Some(&tip));
                }
                f.save(&tip).await;
                let mut stream = f.fetch(std::slice::from_ref(&tip), &[]).await.unwrap();
                let stats = stream.stats();
                assert_eq!(stream.residency.current.load(Ordering::Relaxed), 0);
                let path = stream.spool.as_ref().unwrap().path.clone();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                        0o700
                    );
                    assert_eq!(
                        fs::metadata(path.join("result"))
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o600
                    );
                }
                assert_eq!(stream.object_count(), (commits + 2) as u64);
                assert_eq!(stats.raw_reads, (commits + 2) as u64);
                assert_eq!(
                    stats.raw_bytes,
                    f.git
                        .objects
                        .iter()
                        .map(|(_, _, bytes)| bytes.len() as u64)
                        .sum::<u64>()
                );
                f.assert_reads(commits + 2);
                assert_eq!(collect(&mut stream, &f.git).await.len(), commits + 2);
                f.assert_reads(commits + 2);
                assert!(!path.exists());
                assert_eq!(
                    stats.max_object_bytes,
                    f.git
                        .objects
                        .iter()
                        .map(|(_, _, bytes)| bytes.len())
                        .max()
                        .unwrap(),
                    "measured raw-reader maximum matches the independent Git fixture"
                );
                assert!(stats.peak_resident_frames > 0 && stats.peak_resident_frames <= 3);
                maxima.push((stats.disk_frontier_high_water, stats.peak_resident_frames));
                println!("fetch walk {kind:?} commits={commits} stats={stats:?}");
                f.cleanup().await;
            }
            assert!(
                maxima[1].0 > maxima[0].0 && maxima[0].0 > 1,
                "actual external-memory frontier grows across the two history sizes"
            );
            assert_eq!(
                maxima[0].1, maxima[1].1,
                "measured live Frame high water stays bounded as history grows"
            );
        }
    }

    #[tokio::test]
    async fn scoped_fetch_walk_cancel_waiting_backend_drop_and_eof_cleanup() {
        let mut f = FetchFixture::new(HashKind::Sha1).await;
        let blob = f.git.object("blob", b"payload");
        let tree = f.git.tree(&[("100644", b"x", &blob)]);
        let tip = f.git.commit(&tree, None);
        f.save(&tip).await;
        let mut stream = f.fetch(std::slice::from_ref(&tip), &[]).await.unwrap();
        let path = stream.spool.as_ref().unwrap().path.clone();
        stream.control.0.cancel();
        rejected(stream.next().await, TracesError::QueryTimeout);
        assert!(!path.exists());
        let stream = f.fetch(std::slice::from_ref(&tip), &[]).await.unwrap();
        let path = stream.spool.as_ref().unwrap().path.clone();
        drop(stream);
        assert!(!path.exists());
        f.proxy.gets.lock().unwrap().clear();
        f.proxy.blocked.store(true, Ordering::Release);
        let grant = f.grant().await;
        let cancel = CancellationToken::new();
        let spool = Spool::new().unwrap();
        let pending_path = spool.path.clone();
        {
            let future = fetch_into_spool(
                grant,
                &f.objects,
                std::slice::from_ref(&tip),
                &[],
                FetchControl(cancel.clone()),
                spool,
            );
            tokio::pin!(future);
            tokio::select! {
                result = &mut future => { rejected(result, TracesError::QueryTimeout); panic!("backend gate was not observed"); }
                ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while f.proxy.entered.load(Ordering::Acquire) == 0 { tokio::task::yield_now().await; }
                }) => { ready.expect("real RustFS GET must reach the controlled proxy gate"); }
            }
            cancel.cancel();
            rejected(future.await, TracesError::QueryTimeout);
        }
        assert!(!pending_path.exists());
        assert_eq!(f.proxy.gets.lock().unwrap().len(), 1);
        f.proxy.release.cancel();
        f.cleanup().await;
    }
    #[test]
    fn scoped_fetch_walk_gray_cycle_and_wrong_edge_type_fail_closed() {
        let residency = Arc::new(ResidentFrames::default());
        // Content-addressed Git cannot construct a valid hash cycle. This tests
        // the production external-memory guard independently of raw hashing.
        let spool = Spool::new().unwrap();
        let oid = "a".repeat(40);
        let node = spool.node(&oid);
        private_dir(&node).unwrap();
        private_file(&node.join("state"))
            .unwrap()
            .write_all(&[1, encode_type(RawObjectType::Commit)])
            .unwrap();
        rejected(
            spool.visit(&Frame::new(
                oid.clone(),
                RawObjectType::Commit,
                false,
                &residency,
            )),
            TracesError::ObjectIntegrity,
        );
        assert!(
            spool
                .visit(&Frame::new(
                    oid.clone(),
                    RawObjectType::Commit,
                    true,
                    &residency
                ))
                .unwrap()
        );
        assert!(
            spool
                .visit(&Frame::new(
                    oid.clone(),
                    RawObjectType::Commit,
                    false,
                    &residency
                ))
                .unwrap()
        );
        rejected(
            spool.visit(&Frame::new(oid, RawObjectType::Blob, false, &residency)),
            TracesError::ObjectIntegrity,
        );
        assert_eq!(residency.current.load(Ordering::Relaxed), 0);
        let retained: Vec<_> = (0..8)
            .map(|_| Frame::new("a".repeat(40), RawObjectType::Commit, false, &residency))
            .collect();
        assert_eq!(residency.current.load(Ordering::Relaxed), 8);
        assert_eq!(residency.peak.load(Ordering::Relaxed), 8);
        drop(retained);
        assert_eq!(residency.current.load(Ordering::Relaxed), 0);
        let path = spool.path.clone();
        drop(spool);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn scoped_raw_fetch_wrong_reference_type_malformed_commit_and_tag_want() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut f = FetchFixture::new(kind).await;
            let tree = f.git.tree(&[]);
            let first = f.git.commit(&tree, None);
            let wrong_tree = f.git.tree(&[("100644", b"not-a-blob", &first)]);
            let wrong_tip = f.git.commit(&wrong_tree, None);
            f.save(&wrong_tip).await;
            rejected(
                f.fetch(std::slice::from_ref(&wrong_tip), &[]).await,
                TracesError::ObjectIntegrity,
            );
            let malformed = f.git.object(
                "commit",
                format!("tree {tree}\n\nmissing author and committer").as_bytes(),
            );
            f.save(&malformed).await;
            rejected(
                f.fetch(std::slice::from_ref(&malformed), &[]).await,
                TracesError::ObjectIntegrity,
            );
            let malformed_tree = f.git.object("tree", b"100644 broken\0short");
            let malformed_tree_tip = f.git.commit(&malformed_tree, None);
            f.save(&malformed_tree_tip).await;
            rejected(
                f.fetch(std::slice::from_ref(&malformed_tree_tip), &[])
                    .await,
                TracesError::ObjectIntegrity,
            );
            let tag = f.git.object("tag", format!("object {first}\ntype commit\ntag v1\ntagger Test <test@example.test> 0 +0000\n\ntag").as_bytes());
            f.save(&first).await;
            rejected(
                f.fetch(&["not-an-object-id".to_owned()], &[]).await,
                TracesError::InvalidParameter,
            );
            let cancel = CancellationToken::new();
            cancel.cancel();
            rejected(
                fetch_raw_objects(
                    f.grant().await,
                    &f.objects,
                    std::slice::from_ref(&first),
                    &[],
                    cancel,
                )
                .await,
                TracesError::QueryTimeout,
            );
            rejected(
                f.fetch(std::slice::from_ref(&tag), &[]).await,
                TracesError::UnknownScope,
            );
            f.cleanup().await;
        }
    }
}
