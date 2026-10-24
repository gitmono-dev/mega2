use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt, stream};
use object_store::{
    MultipartUpload, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion,
    aws::AmazonS3, gcp::GoogleCloudStorage, local::LocalFileSystem, signer::Signer,
};
use reqwest::Method;

use super::error::{IoOrbitError, OrbitResult};
use crate::orbit_api::{
    log_storage::{LogManifest, LogSegmentMeta, LogStorage},
    object_storage::{MegaObjectStorage, ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

/// Strategy used for uploading objects to the underlying [`BackendStore`].
///
/// This controls whether data is sent in a single request (`SinglePut`) or
/// split into multiple parts (`Multipart`) when supported by the backend.
/// Callers select a strategy based on object size, latency requirements,
/// and backend capabilities.
pub enum UploadStrategy {
    /// Upload the object using a multipart/streaming upload when supported.
    Multipart,
    /// Upload the entire object using a single `PUT`-style request.
    SinglePut,
}

/// Adapter that exposes an [`ObjectStore`] backend through the
/// [`MegaObjectStorage`] trait.
///
/// This type holds a concrete [`BackendStore`] implementation and an
/// [`UploadStrategy`] that determines how uploads are performed. It is the
/// main integration point between Mega's storage abstraction and the
/// `object_store` crate backends such as S3, GCS, or the local filesystem.
pub struct ObjectStoreAdapter {
    /// The concrete backend store used for all object operations.
    pub store: BackendStore,
    /// The upload strategy used when writing new objects.
    pub upload_strategy: UploadStrategy,
    /// S3-compatible only: a store used just to sign presigned URLs against a
    /// client-reachable endpoint (`object_storage.s3.public_endpoint_url`).
    /// `None` signs with `store`.
    pub presign_store: Option<Arc<AmazonS3>>,
}

/// Supported backend implementations for object storage.
///
/// Each variant wraps a specific `object_store` backend in an [`Arc`] so that
/// a single instance can be cheaply shared across multiple adapters or tasks.
/// New backends should be added as additional enum variants.
pub enum BackendStore {
    /// Amazon S3-compatible object storage backend.
    S3(Arc<AmazonS3>),
    /// Google Cloud Storage backend.
    Gcs(Arc<GoogleCloudStorage>),
    /// Local filesystem backend, primarily for development and testing.
    Local(Arc<LocalFileSystem>),
}

/// Maps a backend `head` result to an existence check with correct error
/// semantics: a successful `head` means the object exists, a `NotFound` error
/// means it does not, and **any other error is propagated**. This ensures
/// auth/permission/network/service failures are never silently reported as
/// "missing".
///
/// Shared by [`ObjectStoreAdapter::exists`] and the `migrate_local_to_s3` tool
/// so both apply identical semantics.
pub fn head_result_to_exists<T>(res: Result<T, object_store::Error>) -> OrbitResult<bool> {
    match res {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(IoOrbitError::from(e)),
    }
}

/// Builds our [`ObjectMeta`] from the backend's object metadata, populating the
/// object `size` and, when the backend provides them, the ETag and version into
/// `extra` (`"etag"` / `"version"`).
///
/// - `ObjectMeta::size` is `i64` while the backend reports `u64`; a size that
///   would overflow `i64` is saturated to `i64::MAX` (objects that large are not
///   expected).
/// - `extra["etag"]` is the backend ETag, which is **not** a general content
///   checksum (e.g. S3 multipart ETags are not MD5 of the content), so
///   `checksum` is intentionally left unset.
fn build_object_meta(m: &object_store::ObjectMeta) -> ObjectMeta {
    let mut extra = std::collections::HashMap::new();
    if let Some(etag) = &m.e_tag {
        extra.insert("etag".to_string(), etag.clone());
    }
    if let Some(version) = &m.version {
        extra.insert("version".to_string(), version.clone());
    }
    ObjectMeta {
        size: i64::try_from(m.size).unwrap_or(i64::MAX),
        checksum: None,
        content_type: None,
        extra,
    }
}

mod log;
mod object;

/// Derives the manifest [`ObjectKey`] for a log identified by `key`.
/// Manifest is stored at `{key.key}/manifest`.
fn log_manifest_key(key: &ObjectKey) -> ObjectKey {
    ObjectKey {
        namespace: key.namespace,
        key: format!("{}/manifest", key.key),
    }
}

/// Derives the segment [`ObjectKey`] for a log segment `[start, end)`.
/// Segment is stored at `{log_key.key}/segments/{start}-{end}-{ts}`.
///
/// `ts` is a best-effort timestamp suffix (millis since epoch) used to avoid
/// key collisions under concurrent writers.
fn log_segment_key(log_key: &ObjectKey, start: u64, end: u64) -> ObjectKey {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_millis(0))
        .as_millis();
    ObjectKey {
        namespace: log_key.namespace,
        key: format!("{}/segments/{}-{}-{}", log_key.key, start, end, ts),
    }
}

/// Maximum size of a single log segment in bytes.
/// Segments larger than this will be split into multiple segments.
const MAX_SEGMENT_SIZE: u64 = 16 * 1024 * 1024; // 16 MB

/// Maximum size of a single `append` / `append_concurrently` payload.
///
/// The current implementation buffers the whole input before splitting it into
/// [`MAX_SEGMENT_SIZE`] segments, so this bounds peak memory and rejects
/// oversized appends instead of risking OOM. A future streaming implementation
/// can relax this.
const MAX_APPEND_BYTES: u64 = 128 * 1024 * 1024; // 128 MB

/// Maximum number of bytes a single `read_range` / `read_lines_range` will
/// aggregate and return. The read path is not yet streaming, so this bounds its
/// peak memory; larger reads must be split into smaller ranges.
const MAX_READ_RANGE_BYTES: u64 = 128 * 1024 * 1024; // 128 MB

/// Fixed multipart part size for [`MegaObjectStorage::put_stream_bounded`].
pub(crate) const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

async fn finish_multipart(
    data: ObjectByteStream,
    upload: &mut dyn MultipartUpload,
) -> OrbitResult<()> {
    let res = async {
        copy_stream_as_fixed_parts(data, upload).await?;
        upload.complete().await.map_err(IoOrbitError::from)?;
        Ok::<(), IoOrbitError>(())
    }
    .await;

    if res.is_err() {
        upload.abort().await.map_err(IoOrbitError::from)?;
    }

    res
}

async fn copy_stream_as_fixed_parts(
    mut data: ObjectByteStream,
    upload: &mut dyn MultipartUpload,
) -> OrbitResult<()> {
    let mut part = BytesMut::with_capacity(MULTIPART_PART_SIZE);
    while let Some(chunk) = data.try_next().await.map_err(IoOrbitError::Io)? {
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let space = MULTIPART_PART_SIZE - part.len();
            let take = remaining.len().min(space);
            let piece = remaining.split_to(take);
            part.extend_from_slice(&piece);
            if part.len() == MULTIPART_PART_SIZE {
                let payload = std::mem::take(&mut part).freeze();
                upload
                    .put_part(payload.into())
                    .await
                    .map_err(IoOrbitError::from)?;
                part.reserve(MULTIPART_PART_SIZE);
            }
        }
    }
    if !part.is_empty() {
        upload
            .put_part(part.freeze().into())
            .await
            .map_err(IoOrbitError::from)?;
    }
    Ok(())
}

/// Rejects a read whose aggregated size would exceed [`MAX_READ_RANGE_BYTES`].
fn enforce_read_len(len: u64) -> OrbitResult<()> {
    if len > MAX_READ_RANGE_BYTES {
        return Err(IoOrbitError::Other(format!(
            "requested read of {len} bytes exceeds MAX_READ_RANGE_BYTES \
             ({MAX_READ_RANGE_BYTES}); read in smaller ranges"
        )));
    }
    Ok(())
}

impl ObjectStoreAdapter {
    /// Validates `key` and returns its backend path. Every object-storage method
    /// routes key->path through this, so no unvalidated key reaches the backend.
    fn checked_path(key: &ObjectKey) -> OrbitResult<object_store::path::Path> {
        key.validate()?;
        Ok(key.to_object_store_path())
    }

    fn to_store(&self) -> &dyn ObjectStore {
        let store: &dyn ObjectStore = match &self.store {
            BackendStore::S3(s3) => s3.as_ref(),
            BackendStore::Gcs(gcs) => gcs.as_ref(),
            BackendStore::Local(local) => local.as_ref(),
        };
        store
    }

    /// Buffers an [`ObjectByteStream`] into [`Bytes`]. Used for log appends (need length) and manifest handling.
    async fn buffer_stream(mut data: ObjectByteStream, max_bytes: u64) -> OrbitResult<Bytes> {
        // NOTE: this buffers the ENTIRE input in memory. `max_bytes` bounds the
        // peak allocation and turns an oversized payload into a clear error
        // instead of an unbounded allocation / OOM. A future streaming append can
        // remove this buffering (see the LogStorage "Memory" notes).
        let mut buf = BytesMut::new();
        while let Some(chunk) = data.try_next().await.map_err(IoOrbitError::Io)? {
            // Check BEFORE extending so an oversized chunk cannot force an
            // allocation past the cap before we error out.
            let projected = (buf.len() as u64).saturating_add(chunk.len() as u64);
            if projected > max_bytes {
                return Err(IoOrbitError::Other(format!(
                    "buffered payload exceeds the configured limit ({max_bytes} bytes); \
                     split it into smaller writes"
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// Loads the [`LogManifest`] for the log identified by `key` and returns its current [`UpdateVersion`]
    /// (based on the underlying store's `e_tag`/`version`), if any.
    ///
    /// Returns an empty manifest and `None` if not found.
    async fn read_log_manifest_with_version(
        &self,
        key: &ObjectKey,
    ) -> OrbitResult<(LogManifest, Option<UpdateVersion>)> {
        let mkey = log_manifest_key(key);
        let path = mkey.to_object_store_path();

        let head = match self.to_store().head(&path).await {
            Ok(h) => h,
            Err(object_store::Error::NotFound { .. }) => {
                return Ok((
                    LogManifest {
                        len: 0,
                        segments: Vec::new(),
                    },
                    None,
                ));
            }
            Err(e) => return Err(IoOrbitError::from(e)),
        };

        let ver = Some(UpdateVersion {
            e_tag: head.e_tag.clone(),
            version: head.version.clone(),
        });

        let mut s = self
            .to_store()
            .get(&path)
            .await
            .map_err(IoOrbitError::from)?
            .into_stream();

        let mut buf = BytesMut::new();
        while let Some(chunk) = s.next().await {
            let c = chunk
                .map_err(std::io::Error::other)
                .map_err(IoOrbitError::Io)?;
            buf.extend_from_slice(&c);
        }
        let bytes = buf.freeze();
        let manifest: LogManifest = serde_json::from_slice(&bytes)?;
        Ok((manifest, ver))
    }

    /// Writes the [`LogManifest`] for the log identified by `key` using a conditional update.
    ///
    /// - If `ver` is `None`, uses [`PutMode::Create`]
    /// - If `ver` is `Some`, uses [`PutMode::Update`] and fails with `Precondition` on version mismatch
    async fn write_log_manifest_conditional(
        &self,
        key: &ObjectKey,
        m: &LogManifest,
        ver: Option<UpdateVersion>,
    ) -> OrbitResult<()> {
        let mkey = log_manifest_key(key);
        let path = mkey.to_object_store_path();
        let bytes = serde_json::to_vec(m)?;

        // For backends that support conditional writes (S3/GCS etc.), use PutMode::Update for multi-writer safety.
        // For backends like LocalFileSystem that haven't implemented Update yet, fall back to Overwrite,
        // which only guarantees single-writer semantics (mainly used in test environments with Local).
        let mode = match (&self.store, ver) {
            (BackendStore::Local(_), Some(_v)) => PutMode::Overwrite,
            (_, Some(v)) => PutMode::Update(v),
            (_, None) => PutMode::Create,
        };
        let opts = PutOptions::from(mode);

        self.to_store()
            .put_opts(&path, PutPayload::from_bytes(Bytes::from(bytes)), opts)
            .await
            .map(|_| ())
            .map_err(|e| match e {
                object_store::Error::Precondition { .. }
                | object_store::Error::AlreadyExists { .. } => {
                    IoOrbitError::WriteManifestPreconditionFailed
                }
                other => IoOrbitError::from(other),
            })
    }

    async fn put_multipart(
        &self,
        path: &object_store::path::Path,
        data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let mut upload = self
            .to_store()
            .put_multipart(path)
            .await
            .map_err(IoOrbitError::from)?;

        finish_multipart(data, &mut *upload).await
    }

    /// Upload an object using a *single PUT* request.
    ///
    /// Why this method exists:
    ///
    /// object_store 0.13 changed the semantics of `ObjectStore::put`:
    /// - `put` no longer accepts a streaming body
    /// - it requires a fully-buffered `PutPayload`
    ///
    /// This helper adapts our internal `ObjectByteStream` abstraction
    /// (used throughout Mega for streaming object data)
    /// into a buffered upload suitable for backends that:
    /// - do NOT reliably support multipart upload (e.g. rustfs, some MinIO setups)
    /// - or where the object size is small enough to fit comfortably in memory
    ///
    /// Design trade-offs:
    /// - This method **buffers the entire object in memory**
    /// - It should ONLY be used for:
    ///   - small objects
    ///   - metadata-like payloads
    ///   - backends without stable multipart support
    ///
    /// For large objects (Git packfiles, LFS blobs, etc.),
    /// `put_stream` + `put_multipart` MUST be used instead.
    async fn put_single(
        &self,
        path: &object_store::path::Path,
        mut data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let mut buf = BytesMut::new();

        while let Some(chunk) = data.try_next().await? {
            buf.extend_from_slice(&chunk);
        }

        self.to_store()
            .put(path, PutPayload::from_bytes(buf.into()))
            .await
            .map_err(IoOrbitError::from)?;

        Ok(())
    }

    /// Uploads an object using a single `PUT` in **create-only** mode.
    ///
    /// This helper is currently used only for **Git objects** (blob/pack data)
    /// via `put_stream` when `ObjectNamespace::Git` + `UploadStrategy::SinglePut`
    /// are selected.
    ///
    /// Semantics:
    /// - Uses [`PutMode::Create`], so the backend will fail if the key already exists.
    /// - This makes writes *idempotent* for content-addressed Git blobs: the first
    ///   successful upload wins, and later attempts do not silently overwrite data.
    /// - Callers must ensure that `path` is a content-hash-based key (Git object id),
    ///   so that "already exists" is expected and safe to ignore at higher layers.
    async fn put_idempotent(
        &self,
        path: &object_store::path::Path,
        mut data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let mut buf = BytesMut::new();
        while let Some(chunk) = data.try_next().await? {
            buf.extend_from_slice(&chunk);
        }

        // Use `PutMode::Create` so we never overwrite an existing object.
        // For Git blobs (content-addressed by hash), an "already exists"
        // error is expected and treated as success by higher layers.
        match self
            .to_store()
            .put_opts(
                path,
                PutPayload::from_bytes(buf.into()),
                PutOptions::from(PutMode::Create),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(e) => Err(IoOrbitError::from(e)),
        }
    }
}

const TRACES_BUFFER_SEGMENT_BYTES: usize = 64 * 1024;

struct TracesCreateBuffer {
    segments: Vec<Vec<u8>>,
    len: usize,
}

impl TracesCreateBuffer {
    async fn read(mut data: ObjectByteStream) -> OrbitResult<Self> {
        let limit = crate::config::TracesProtocolLimits::MAX_OBJECT_BYTES;
        let mut buffer = Self {
            segments: Vec::new(),
            len: 0,
        };
        while let Some(chunk) = data.next().await {
            let chunk = chunk.map_err(|error| {
                IoOrbitError::Io(std::io::Error::new(
                    error.kind(),
                    "traces input stream failed",
                ))
            })?;
            if chunk.len() as u64 > limit - buffer.len as u64 {
                return Err(IoOrbitError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "traces object exceeds its fixed byte limit",
                )));
            }
            let mut remaining = chunk.as_ref();
            while !remaining.is_empty() {
                if buffer
                    .segments
                    .last()
                    .is_none_or(|part| part.len() == TRACES_BUFFER_SEGMENT_BYTES)
                {
                    buffer
                        .segments
                        .push(Vec::with_capacity(TRACES_BUFFER_SEGMENT_BYTES));
                }
                if let Some(part) = buffer.segments.last_mut() {
                    let take = remaining
                        .len()
                        .min(TRACES_BUFFER_SEGMENT_BYTES - part.len());
                    part.extend_from_slice(&remaining[..take]);
                    remaining = &remaining[take..];
                    buffer.len += take;
                }
            }
        }
        Ok(buffer)
    }

    fn into_payload(self) -> PutPayload {
        self.segments.into_iter().map(Bytes::from).collect()
    }
}

fn traces_backend_error(error: object_store::Error) -> IoOrbitError {
    let kind = match error {
        object_store::Error::NotFound { .. } => std::io::ErrorKind::NotFound,
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => std::io::ErrorKind::PermissionDenied,
        object_store::Error::NotSupported { .. } | object_store::Error::NotImplemented { .. } => {
            std::io::ErrorKind::Unsupported
        }
        _ => std::io::ErrorKind::Other,
    };
    IoOrbitError::Io(std::io::Error::new(
        kind,
        "traces conditional object storage operation failed",
    ))
}

impl ObjectStoreAdapter {
    async fn put_traces_create(
        &self,
        path: &object_store::path::Path,
        data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let payload = TracesCreateBuffer::read(data).await?.into_payload();
        match self
            .to_store()
            .put_opts(path, payload, PutOptions::from(PutMode::Create))
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                match self.to_store().head(path).await {
                    Ok(_) => Ok(()),
                    Err(object_store::Error::NotFound { .. }) => {
                        Err(IoOrbitError::Io(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "traces conditional creation is not yet visible",
                        )))
                    }
                    Err(error) => Err(traces_backend_error(error)),
                }
            }
            Err(error) => Err(traces_backend_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn traces_immutable_create_protocol_unsupported_does_not_fallback() {
        for multipart in [false, true] {
            let fixture = TracesProtocolFixture {
                put_status: 200,
                head_status: 200,
                stored: b"new",
                requests: Default::default(),
            };
            let (endpoint, server) = traces_protocol_server(fixture.clone()).await;
            let adapter = ObjectStoreAdapter {
                store: BackendStore::S3(Arc::new(
                    object_store::aws::AmazonS3Builder::new()
                        .with_bucket_name("fixture")
                        .with_region("us-east-1")
                        .with_endpoint(endpoint)
                        .with_access_key_id("fixture-only")
                        .with_secret_access_key("fixture-only")
                        .with_allow_http(true)
                        .with_skip_signature(true)
                        .with_conditional_put(object_store::aws::S3ConditionalPut::Disabled)
                        .build()
                        .unwrap(),
                )),
                upload_strategy: if multipart {
                    UploadStrategy::Multipart
                } else {
                    UploadStrategy::SinglePut
                },
                presign_store: None,
            };
            let error = adapter
                .put_stream(
                    &traces_test_key(false),
                    Box::pin(stream::iter([Ok(Bytes::from_static(b"new"))])),
                    ObjectMeta::default(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, IoOrbitError::Io(ref error) if error.kind() == std::io::ErrorKind::Unsupported)
            );
            assert!(!format!("{error:?}").contains("fixture"));
            assert!(fixture.requests.lock().unwrap().is_empty());
            server.0.abort();
            while !server.0.is_finished() {
                tokio::task::yield_now().await;
            }
        }
    }

    fn traces_test_key(staging: bool) -> ObjectKey {
        let id = uuid::Uuid::new_v4().to_string();
        if staging {
            crate::jupiter::storage::trace_object_storage::staging_object_key(&id, "sha1", "abcdef")
        } else {
            crate::jupiter::storage::trace_object_storage::object_key(&id, "sha1", "abcdef")
        }
        .unwrap()
    }

    #[tokio::test]
    async fn traces_immutable_create_buffer_owns_bounded_segments() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let limit = crate::config::TracesProtocolLimits::MAX_OBJECT_BYTES as usize;
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let input = Box::pin(stream::iter((0..1024).map(move |_| {
            observed.fetch_add(TRACES_BUFFER_SEGMENT_BYTES, Ordering::SeqCst);
            Ok(Bytes::from(vec![0xab; TRACES_BUFFER_SEGMENT_BYTES]))
        })));
        let buffered = TracesCreateBuffer::read(input).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), limit);
        assert_eq!(buffered.len, limit);
        assert_eq!(buffered.segments.len(), 1024);
        assert_eq!(
            buffered.segments.iter().map(Vec::capacity).sum::<usize>(),
            limit
        );
        assert!(
            buffered
                .segments
                .iter()
                .all(|part| part.len() == TRACES_BUFFER_SEGMENT_BYTES
                    && part.iter().all(|byte| *byte == 0xab))
        );
        let pointers: Vec<_> = buffered.segments.iter().map(|part| part.as_ptr()).collect();
        let payload = buffered.into_payload();
        assert_eq!(payload.content_length(), limit);
        assert_eq!(
            payload.iter().map(|part| part.as_ptr()).collect::<Vec<_>>(),
            pointers
        );
        let empty =
            TracesCreateBuffer::read(Box::pin(stream::iter((0..4096).map(|_| Ok(Bytes::new())))))
                .await
                .unwrap();
        assert_eq!(empty.len, 0);
        assert_eq!(empty.segments.capacity(), 0);
        let source = Bytes::from(vec![0x5a; limit * 2]);
        let pointer = source.as_ptr();
        let small = TracesCreateBuffer::read(Box::pin(stream::iter([Ok(source.slice(..1))])))
            .await
            .unwrap();
        assert_ne!(small.segments[0].as_ptr(), pointer);
        assert_eq!(small.segments[0].capacity(), TRACES_BUFFER_SEGMENT_BYTES);
        assert_eq!(small.segments[0], [0x5a]);
    }

    #[tokio::test]
    async fn traces_immutable_create_limit_and_input_failure_never_publish_partial() {
        let limit = crate::config::TracesProtocolLimits::MAX_OBJECT_BYTES as usize;
        for multipart in [false, true] {
            for staging in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let adapter = traces_local_adapter(directory.path(), multipart);
                let key = traces_test_key(staging);
                let at_limit = || {
                    Box::pin(stream::iter((0..1024).map(|_| {
                        Ok(Bytes::from(vec![0x5a; TRACES_BUFFER_SEGMENT_BYTES]))
                    }))) as ObjectByteStream
                };
                adapter
                    .put_stream(&key, at_limit(), ObjectMeta::default())
                    .await
                    .unwrap();
                let (mut input, meta) = adapter.get_stream(&key).await.unwrap();
                assert_eq!(meta.size, limit as i64);
                let mut total = 0;
                while let Some(part) = input.next().await {
                    let part = part.unwrap();
                    total += part.len();
                    assert!(part.iter().all(|byte| *byte == 0x5a));
                }
                assert_eq!(total, limit);
                for existing in [false, true] {
                    let target = if existing {
                        key.clone()
                    } else {
                        traces_test_key(staging)
                    };
                    let oversized =
                        Box::pin(at_limit().chain(stream::iter([Ok(Bytes::from_static(b"x"))])));
                    let error = adapter
                        .put_stream(&target, oversized, ObjectMeta::default())
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(error, IoOrbitError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput)
                    );
                    let broken = Box::pin(stream::iter([
                        Ok(Bytes::from_static(b"private-input")),
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "private-server-body/key",
                        )),
                    ]));
                    let error = adapter
                        .put_stream(&target, broken, ObjectMeta::default())
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(error, IoOrbitError::Io(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset)
                    );
                    assert!(!format!("{error:?}").contains("private"));
                    if existing {
                        let (mut bytes, meta) = adapter.get_stream(&target).await.unwrap();
                        assert_eq!(meta.size, limit as i64);
                        let mut read = 0;
                        while let Some(chunk) = bytes.next().await {
                            let chunk = chunk.unwrap();
                            read += chunk.len();
                            assert!(chunk.iter().all(|byte| *byte == 0x5a));
                        }
                        assert_eq!(read, limit);
                    } else {
                        assert!(!adapter.exists(&target).await.unwrap());
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn traces_immutable_create_other_namespaces_keep_existing_semantics() {
        for multipart in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let adapter = traces_local_adapter(directory.path(), multipart);
            for (namespace, name, keeps_first) in [
                (ObjectNamespace::Git, "abcdefabcdef", !multipart),
                (ObjectNamespace::Lfs, "abcdefabcdef", false),
                (
                    ObjectNamespace::Artifact,
                    "11111111-1111-4111-8111-111111111111",
                    true,
                ),
                (ObjectNamespace::Media, "abcdefabcdef", false),
            ] {
                let key = ObjectKey {
                    namespace,
                    key: name.into(),
                };
                for payload in [b"first".as_slice(), b"other".as_slice()] {
                    adapter
                        .put_stream(
                            &key,
                            Box::pin(stream::iter([Ok(Bytes::copy_from_slice(payload))])),
                            ObjectMeta::default(),
                        )
                        .await
                        .unwrap();
                }
                let (bytes, _) = adapter.get_stream(&key).await.unwrap();
                assert_eq!(
                    ObjectStoreAdapter::buffer_stream(bytes, 5)
                        .await
                        .unwrap()
                        .as_ref(),
                    if keeps_first { b"first" } else { b"other" }
                );
            }
        }
    }

    struct TracesProtocolRequest {
        method: String,
        condition: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct TracesProtocolFixture {
        put_status: u16,
        head_status: u16,
        stored: &'static [u8],
        requests: Arc<std::sync::Mutex<Vec<TracesProtocolRequest>>>,
    }

    async fn traces_protocol_response(
        axum::extract::State(state): axum::extract::State<TracesProtocolFixture>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let method = request.method().as_str().to_owned();
        let condition = request
            .headers()
            .get("if-none-match")
            .or_else(|| request.headers().get("x-goog-if-generation-match"))
            .map(|value| value.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(request.into_body(), 1024)
            .await
            .unwrap();
        let mut requests = state.requests.lock().unwrap();
        let initial_get =
            method == "GET" && !requests.iter().any(|request| request.method == "GET");
        requests.push(TracesProtocolRequest {
            method: method.clone(),
            condition,
            body: body.to_vec(),
        });
        let status = match method.as_str() {
            "PUT" => state.put_status,
            "HEAD" => state.head_status,
            "GET" if initial_get => 404,
            "GET" => 200,
            _ => 400,
        };
        let body = if method == "HEAD" || status == 304 {
            b"".as_slice()
        } else if status == 200 {
            state.stored
        } else {
            b"private-provider-body/key".as_slice()
        };
        let mut response = axum::http::Response::builder()
            .status(status)
            .header("etag", "\"fixture\"")
            .header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT")
            .header("x-goog-generation", "123");
        if method == "HEAD" && status == 200 {
            response = response.header("content-length", state.stored.len());
        }
        response.body(axum::body::Body::from(body)).unwrap()
    }

    fn traces_protocol_adapter(
        endpoint: &str,
        gcs: bool,
        multipart: bool,
        retries: usize,
    ) -> ObjectStoreAdapter {
        let retry = object_store::RetryConfig {
            max_retries: retries,
            retry_timeout: Duration::from_secs(2),
            backoff: object_store::BackoffConfig {
                init_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
                base: 2.0,
            },
        };
        let client = object_store::ClientOptions::new()
            .with_allow_http(true)
            .with_timeout(Duration::from_secs(2));
        let store = if gcs {
            BackendStore::Gcs(Arc::new(
                object_store::gcp::GoogleCloudStorageBuilder::new()
                    .with_bucket_name("fixture")
                    .with_base_url(endpoint)
                    .with_skip_signature(true)
                    .with_credentials(Arc::new(object_store::StaticCredentialProvider::new(
                        object_store::gcp::GcpCredential {
                            bearer: "fixture-only".into(),
                        },
                    )))
                    .with_client_options(client)
                    .with_retry(retry)
                    .build()
                    .unwrap(),
            ))
        } else {
            BackendStore::S3(Arc::new(
                object_store::aws::AmazonS3Builder::new()
                    .with_bucket_name("fixture")
                    .with_region("us-east-1")
                    .with_endpoint(endpoint)
                    .with_access_key_id("fixture-only")
                    .with_secret_access_key("fixture-only")
                    .with_skip_signature(true)
                    .with_virtual_hosted_style_request(false)
                    .with_client_options(client)
                    .with_retry(retry)
                    .build()
                    .unwrap(),
            ))
        };
        ObjectStoreAdapter {
            store,
            upload_strategy: if multipart {
                UploadStrategy::Multipart
            } else {
                UploadStrategy::SinglePut
            },
            presign_store: None,
        }
    }

    struct TracesProtocolServer(tokio::task::JoinHandle<()>);
    impl Drop for TracesProtocolServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn traces_protocol_server(
        state: TracesProtocolFixture,
    ) -> (String, TracesProtocolServer) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .fallback(traces_protocol_response)
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, TracesProtocolServer(task))
    }

    fn traces_protocol_assert_requests(
        state: &TracesProtocolFixture,
        gcs: bool,
        expected: &[&str],
    ) {
        let requests = state.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        for TracesProtocolRequest {
            method,
            condition,
            body,
        } in requests.iter()
        {
            if method == "PUT" {
                assert_eq!(condition.as_deref(), Some(if gcs { "0" } else { "*" }));
                assert_eq!(body, b"new");
            } else {
                assert!(body.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn traces_immutable_create_protocol_conflicts_use_real_sdk_and_raw_readback() {
        use crate::{
            common::errors::{MegaError, TracesError},
            jupiter::storage::trace_object_storage::{put_raw, put_staging},
            orbit_api::factory::MegaObjectStorageWrapper,
        };
        for gcs in [false, true] {
            for multipart in [false, true] {
                for staging in [false, true] {
                    for status in if gcs { vec![412] } else { vec![304, 409, 412] } {
                        for (head_status, stored) in [
                            (200, b"new".as_slice()),
                            (200, b"old".as_slice()),
                            (404, b"old".as_slice()),
                            (403, b"old".as_slice()),
                        ] {
                            let fixture = TracesProtocolFixture {
                                put_status: status,
                                head_status,
                                stored,
                                requests: Default::default(),
                            };
                            let (endpoint, server) = traces_protocol_server(fixture.clone()).await;
                            let store = MegaObjectStorageWrapper::new(Arc::new(
                                traces_protocol_adapter(&endpoint, gcs, multipart, 0),
                            ));
                            let id = uuid::Uuid::new_v4().to_string();
                            let result = if staging {
                                put_staging(&store, &id, "sha1", "abcdef", b"new").await
                            } else {
                                put_raw(&store, &id, "sha1", "abcdef", b"new").await
                            };
                            match head_status {
                                200 if stored == b"new" => assert!(result.is_ok(), "{result:?}"),
                                200 => assert!(
                                    matches!(
                                        result,
                                        Err(MegaError::Traces(TracesError::ObjectIntegrity))
                                    ),
                                    "{result:?}"
                                ),
                                404 => assert!(
                                    matches!(result, Err(MegaError::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock),
                                    "{result:?}"
                                ),
                                _ => assert!(
                                    matches!(result, Err(MegaError::Io(ref e)) if e.kind() == std::io::ErrorKind::PermissionDenied),
                                    "{result:?}"
                                ),
                            }
                            assert!(!format!("{result:?}").contains("private"));
                            assert!(!format!("{result:?}").contains(&id));
                            traces_protocol_assert_requests(
                                &fixture,
                                gcs,
                                if head_status == 200 {
                                    &["GET", "PUT", "HEAD", "GET"]
                                } else {
                                    &["GET", "PUT", "HEAD"]
                                },
                            );
                            server.0.abort();
                            while !server.0.is_finished() {
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn traces_immutable_create_protocol_errors_and_retries_never_overwrite() {
        for gcs in [false, true] {
            for multipart in [false, true] {
                for (status, retries) in [
                    (200, 0),
                    (409, 0),
                    (412, 0),
                    (401, 0),
                    (403, 0),
                    (501, 0),
                    (503, 1),
                ] {
                    let fixture = TracesProtocolFixture {
                        put_status: status,
                        head_status: 200,
                        stored: b"new",
                        requests: Default::default(),
                    };
                    let (endpoint, server) = traces_protocol_server(fixture.clone()).await;
                    let adapter = traces_protocol_adapter(&endpoint, gcs, multipart, retries);
                    let key = traces_test_key(false);
                    let result = adapter
                        .put_stream(
                            &key,
                            Box::pin(stream::iter([Ok(Bytes::from_static(b"new"))])),
                            ObjectMeta::default(),
                        )
                        .await;
                    if [200, 409, 412].contains(&status) {
                        assert!(result.is_ok(), "{result:?}");
                    } else {
                        assert!(result.is_err());
                        assert!(!format!("{result:?}").contains("private"));
                        assert!(!format!("{result:?}").contains(&key.key));
                    }
                    traces_protocol_assert_requests(
                        &fixture,
                        gcs,
                        match status {
                            409 | 412 => &["PUT", "HEAD"],
                            503 => &["PUT", "PUT"],
                            _ => &["PUT"],
                        },
                    );
                    server.0.abort();
                    while !server.0.is_finished() {
                        tokio::task::yield_now().await;
                    }
                }
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let endpoint = format!("http://{}", listener.local_addr().unwrap());
                drop(listener);
                let adapter = traces_protocol_adapter(&endpoint, gcs, multipart, 0);
                let key = traces_test_key(true);
                let error = adapter
                    .put_stream(
                        &key,
                        Box::pin(stream::iter([Ok(Bytes::from_static(b"new"))])),
                        ObjectMeta::default(),
                    )
                    .await
                    .unwrap_err();
                assert!(!error.is_not_found());
                assert!(
                    !matches!(&error, IoOrbitError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
                );
                assert!(!format!("{error:?}").contains(&endpoint));
                assert!(!format!("{error:?}").contains(&key.key));
            }
        }
    }

    struct TracesReadBarrier {
        adapter: ObjectStoreAdapter,
        directory: std::path::PathBuf,
        role: String,
        release: String,
        armed: std::sync::atomic::AtomicBool,
    }

    async fn traces_wait_for(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !path.try_exists().unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("owned traces child barrier timed out");
    }

    fn traces_local_adapter(root: &std::path::Path, multipart: bool) -> ObjectStoreAdapter {
        ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(LocalFileSystem::new_with_prefix(root).unwrap())),
            upload_strategy: if multipart {
                UploadStrategy::Multipart
            } else {
                UploadStrategy::SinglePut
            },
            presign_store: None,
        }
    }

    #[tokio::test]
    async fn traces_immutable_create_child() {
        use crate::{
            common::errors::{MegaError, TracesError},
            jupiter::storage::trace_object_storage::{put_raw, put_staging},
            orbit_api::factory::{
                MegaObjectStorageWrapper, ObjectStorageBackend, ObjectStorageConfig,
            },
        };
        let Ok(directory) = std::env::var("MEGA2_FIX07_CHILD_DIRECTORY") else {
            let directory = tempfile::tempdir().unwrap();
            let store = MegaObjectStorageWrapper::new(Arc::new(traces_local_adapter(
                directory.path(),
                false,
            )));
            let id = uuid::Uuid::new_v4().to_string();
            for staging in [false, true] {
                for payload in [b"first".as_slice(), b"first".as_slice()] {
                    if staging {
                        put_staging(&store, &id, "sha1", "abcdef", payload).await
                    } else {
                        put_raw(&store, &id, "sha1", "abcdef", payload).await
                    }
                    .unwrap();
                }
                let conflict = if staging {
                    put_staging(&store, &id, "sha1", "abcdef", b"other").await
                } else {
                    put_raw(&store, &id, "sha1", "abcdef", b"other").await
                };
                assert!(matches!(
                    conflict,
                    Err(MegaError::Traces(TracesError::ObjectIntegrity))
                ));
            }
            return;
        };
        tokio::time::timeout(Duration::from_secs(45), async {
            let directory = std::path::PathBuf::from(directory);
            let cfg: ObjectStorageConfig =
                serde_json::from_slice(&std::fs::read(directory.join("config.json")).unwrap())
                    .unwrap();
            let multipart = std::env::var("MEGA2_FIX07_MULTIPART").unwrap() == "true";
            let adapter = match cfg.storage_type {
                ObjectStorageBackend::Local => {
                    traces_local_adapter(std::path::Path::new(&cfg.local.root_dir), multipart)
                }
                ObjectStorageBackend::S3Compatible => ObjectStoreAdapter {
                    store: BackendStore::S3(Arc::new(
                        object_store::aws::AmazonS3Builder::new()
                            .with_bucket_name(&cfg.s3.bucket)
                            .with_region(&cfg.s3.region)
                            .with_access_key_id(&cfg.s3.access_key_id)
                            .with_secret_access_key(&cfg.s3.secret_access_key)
                            .with_endpoint(&cfg.s3.endpoint_url)
                            .with_allow_http(true)
                            .with_virtual_hosted_style_request(false)
                            .build()
                            .unwrap(),
                    )),
                    upload_strategy: if multipart {
                        UploadStrategy::Multipart
                    } else {
                        UploadStrategy::SinglePut
                    },
                    presign_store: None,
                },
                _ => panic!("unexpected child backend"),
            };
            let role = std::env::var("MEGA2_FIX07_ROLE").unwrap();
            let retry = std::env::var("MEGA2_FIX07_RETRY").unwrap() == "true";
            let store = MegaObjectStorageWrapper::new(Arc::new(TracesReadBarrier {
                adapter,
                directory: directory.clone(),
                role: role.clone(),
                release: std::env::var("MEGA2_FIX07_RELEASE").unwrap(),
                armed: std::sync::atomic::AtomicBool::new(!retry),
            }));
            let id = std::env::var("MEGA2_FIX07_ID").unwrap();
            let payload = std::env::var("MEGA2_FIX07_PAYLOAD").unwrap();
            let result = if std::env::var("MEGA2_FIX07_STAGING").unwrap() == "true" {
                put_staging(&store, &id, "sha1", "abcdef", payload.as_bytes()).await
            } else {
                put_raw(&store, &id, "sha1", "abcdef", payload.as_bytes()).await
            };
            let outcome = match result {
                Ok(()) => "ok",
                Err(MegaError::Traces(TracesError::ObjectIntegrity)) => "integrity",
                Err(MegaError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    "retryable"
                }
                Err(error) => panic!("unexpected raw result: {error:?}"),
            };
            std::fs::write(directory.join(format!("{role}.result")), outcome).unwrap();
        })
        .await
        .expect("owned traces child deadline");
    }
    #[async_trait::async_trait]
    impl MegaObjectStorage for TracesReadBarrier {
        fn supports_presigned_urls(&self) -> bool {
            self.adapter.supports_presigned_urls()
        }
        fn supports_chunk_map_retention(&self) -> bool {
            self.adapter.supports_chunk_map_retention()
        }
        async fn put_stream(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter.put_stream(key, data, _meta).await
        }
        async fn put_stream_bounded(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter.put_stream_bounded(key, data, _meta).await
        }
        async fn put_metadata_atomic(
            &self,
            key: &ObjectKey,
            bytes: Bytes,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter.put_metadata_atomic(key, bytes, _meta).await
        }
        async fn put_metadata_atomic_create(
            &self,
            key: &ObjectKey,
            bytes: Bytes,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter
                .put_metadata_atomic_create(key, bytes, _meta)
                .await
        }
        async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
            let result = self.adapter.get_stream(key).await;
            if result.as_ref().is_err_and(IoOrbitError::is_not_found)
                && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                std::fs::write(
                    self.directory.join(format!("{}.ready", self.role)),
                    b"real-not-found",
                )
                .unwrap();
                traces_wait_for(&self.directory.join(&self.release)).await;
            }
            result
        }
        async fn chunk_map_receipt_inventory(
            &self,
        ) -> OrbitResult<crate::orbit_api::object_storage::ChunkMapReceiptInventory> {
            self.adapter.chunk_map_receipt_inventory().await
        }
        async fn delete_chunk_map_receipt(
            &self,
            authority: &crate::orbit_api::object_storage::ChunkMapReceiptDeletion,
        ) -> OrbitResult<bool> {
            self.adapter.delete_chunk_map_receipt(authority).await
        }
        async fn get_range_stream(
            &self,
            key: &ObjectKey,
            start: u64,
            end: Option<u64>,
        ) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
            self.adapter.get_range_stream(key, start, end).await
        }
        async fn get_range_stream_exact(
            &self,
            key: &ObjectKey,
            start: u64,
            end: u64,
        ) -> OrbitResult<Option<(ObjectByteStream, ObjectMeta)>> {
            self.adapter.get_range_stream_exact(key, start, end).await
        }
        async fn signed_url(
            &self,
            key: &ObjectKey,
            method: Method,
            expires_in: Duration,
        ) -> OrbitResult<Option<String>> {
            self.adapter.signed_url(key, method, expires_in).await
        }
        async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
            self.adapter.exists(key).await
        }
        async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
            self.adapter.delete(key).await
        }
    }
    #[async_trait::async_trait]
    impl LogStorage for TracesReadBarrier {
        async fn append(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter.append(key, data, _meta).await
        }
        async fn read_range(
            &self,
            key: &ObjectKey,
            offset: u64,
            length: u64,
        ) -> OrbitResult<ObjectByteStream> {
            self.adapter.read_range(key, offset, length).await
        }
        async fn read_lines_range(
            &self,
            key: &ObjectKey,
            start_line: u64,
            end_line: u64,
        ) -> OrbitResult<ObjectByteStream> {
            self.adapter
                .read_lines_range(key, start_line, end_line)
                .await
        }
        async fn append_concurrently(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            _meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.adapter.append_concurrently(key, data, _meta).await
        }
        async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest> {
            self.adapter.load_manifest(key).await
        }
        async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
            self.adapter.log_exists(key).await
        }
    }

    #[test]
    fn head_ok_means_exists() {
        // A successful HEAD => the object exists.
        assert!(head_result_to_exists(Ok::<(), object_store::Error>(())).unwrap());
    }

    #[test]
    fn head_not_found_means_missing() {
        // NotFound => Ok(false), the object is absent.
        let err = object_store::Error::NotFound {
            path: "missing".to_string(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, "nf")),
        };
        assert!(!head_result_to_exists::<()>(Err(err)).unwrap());
    }

    #[test]
    fn head_other_error_propagates() {
        // Any non-NotFound backend error must propagate, never become "missing".
        let err = object_store::Error::Generic {
            store: "test",
            source: Box::new(std::io::Error::other("boom")),
        };
        let e = head_result_to_exists::<()>(Err(err))
            .expect_err("non-NotFound head error must propagate");
        assert!(
            !e.is_not_found(),
            "a generic backend error must not be classified as NotFound"
        );
    }

    #[tokio::test]
    async fn buffer_stream_enforces_max() {
        // Controlled "large input": 10 bytes against a 4-byte cap must error,
        // exercising the append memory bound without allocating anything huge.
        let data: ObjectByteStream = Box::pin(stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"0123456789"))
        }));
        assert!(ObjectStoreAdapter::buffer_stream(data, 4).await.is_err());

        let within: ObjectByteStream = Box::pin(stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"0123456789"))
        }));
        let buf = ObjectStoreAdapter::buffer_stream(within, 100)
            .await
            .unwrap();
        assert_eq!(buf.len(), 10);
    }

    #[test]
    fn enforce_read_len_rejects_oversize() {
        assert!(enforce_read_len(MAX_READ_RANGE_BYTES).is_ok());
        assert!(enforce_read_len(MAX_READ_RANGE_BYTES + 1).is_err());
    }

    #[derive(Debug)]
    struct RecordingUpload {
        parts: Vec<usize>,
        first_part: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl MultipartUpload for RecordingUpload {
        fn put_part(&mut self, data: PutPayload) -> object_store::UploadPart {
            let len: usize = data.iter().map(|b| b.len()).sum();
            self.parts.push(len);
            self.first_part
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        async fn complete(&mut self) -> object_store::Result<object_store::PutResult> {
            Ok(object_store::PutResult {
                e_tag: None,
                version: None,
                extensions: Default::default(),
            })
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            Ok(())
        }
    }

    struct GatedStream {
        state: u8,
        first_part: Arc<std::sync::atomic::AtomicBool>,
    }

    impl futures::Stream for GatedStream {
        type Item = Result<Bytes, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            match self.state {
                0 => {
                    self.state = 1;
                    std::task::Poll::Ready(Some(Ok(Bytes::from(vec![1u8; MULTIPART_PART_SIZE]))))
                }
                1 => {
                    assert!(
                        self.first_part.load(std::sync::atomic::Ordering::SeqCst),
                        "must finish an 8 MiB part before reading further input"
                    );
                    self.state = 2;
                    std::task::Poll::Ready(Some(Ok(Bytes::from_static(&[2u8]))))
                }
                _ => std::task::Poll::Ready(None),
            }
        }
    }

    #[tokio::test]
    async fn multipart_aggregates_fixed_parts_with_backpressure() {
        let first_part = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut upload = RecordingUpload {
            parts: Vec::new(),
            first_part: first_part.clone(),
        };
        let data: ObjectByteStream = Box::pin(GatedStream {
            state: 0,
            first_part: first_part.clone(),
        });
        copy_stream_as_fixed_parts(data, &mut upload).await.unwrap();
        assert_eq!(upload.parts, vec![MULTIPART_PART_SIZE, 1]);
    }

    #[tokio::test]
    async fn put_stream_bounded_uses_multipart_even_with_single_put() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFileSystem::new_with_prefix(dir.path()).unwrap();
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(store)),
            upload_strategy: UploadStrategy::SinglePut,
            presign_store: None,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "abcdef1234567890".to_string(),
        };
        let payload = Bytes::from_static(b"bounded-bytes");
        let data: ObjectByteStream =
            Box::pin(stream::iter([Ok::<Bytes, std::io::Error>(payload.clone())]));
        adapter
            .put_stream_bounded(&key, data, ObjectMeta::default())
            .await
            .unwrap();
        let (got, _) = adapter.get_stream(&key).await.unwrap();
        let bytes = ObjectStoreAdapter::buffer_stream(got, 1024).await.unwrap();
        assert_eq!(bytes, payload);
    }

    #[tokio::test]
    async fn put_stream_bounded_aborts_when_stream_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFileSystem::new_with_prefix(dir.path()).unwrap();
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(store)),
            upload_strategy: UploadStrategy::SinglePut,
            presign_store: None,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "ffffffffffffffff".to_string(),
        };
        let data: ObjectByteStream = Box::pin(stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("stream failed")),
        ]));
        assert!(
            adapter
                .put_stream_bounded(&key, data, ObjectMeta::default())
                .await
                .is_err()
        );
        assert!(!adapter.exists(&key).await.unwrap());
    }

    #[derive(Debug)]
    struct AbortingUpload {
        aborted: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl MultipartUpload for AbortingUpload {
        fn put_part(&mut self, _data: PutPayload) -> object_store::UploadPart {
            Box::pin(async { Ok(()) })
        }

        async fn complete(&mut self) -> object_store::Result<object_store::PutResult> {
            Err(object_store::Error::Generic {
                store: "test",
                source: Box::new(std::io::Error::other("complete failed")),
            })
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            self.aborted
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn multipart_aborts_when_complete_fails() {
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut upload = AbortingUpload {
            aborted: aborted.clone(),
        };
        let data: ObjectByteStream = Box::pin(stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"part"))
        }));
        assert!(finish_multipart(data, &mut upload).await.is_err());
        assert!(
            aborted.load(std::sync::atomic::Ordering::SeqCst),
            "complete failure must abort so a half-published object is not left behind"
        );
    }
}
