use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt, stream};
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion, aws::AmazonS3,
    gcp::GoogleCloudStorage, local::LocalFileSystem, signer::Signer,
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
}

// S3/GCS require non-final parts >= 5 MiB; R2 also requires equal part sizes.
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

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
        mut data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let mut upload = self
            .to_store()
            .put_multipart(path)
            .await
            .map_err(IoOrbitError::from)?;

        let res = async {
            while let Some(chunk) = data.try_next().await? {
                upload
                    .put_part(chunk.into())
                    .await
                    .map_err(IoOrbitError::from)?;
            }

            upload.complete().await.map_err(IoOrbitError::from)?;

            Ok::<(), IoOrbitError>(())
        }
        .await;

        if res.is_err() {
            upload.abort().await.map_err(IoOrbitError::from)?;
        }

        res
    }

    /// Uses one fixed-size upload buffer and waits for each part before reading
    /// more input. The backend publishes only on `complete`; a multipart failure
    /// is never an excuse to buffer the full object instead.
    async fn put_multipart_bounded(
        &self,
        path: &object_store::path::Path,
        data: ObjectByteStream,
    ) -> OrbitResult<()> {
        Self::put_multipart_bounded_to(self.to_store(), path, data).await
    }

    async fn put_multipart_bounded_to(
        store: &dyn ObjectStore,
        path: &object_store::path::Path,
        mut data: ObjectByteStream,
    ) -> OrbitResult<()> {
        let mut upload = store
            .put_multipart(path)
            .await
            .map_err(IoOrbitError::from)?;

        let res = async {
            let mut buffer = BytesMut::with_capacity(MULTIPART_PART_SIZE);
            let mut saw_data = false;
            while let Some(chunk) = data.try_next().await? {
                saw_data |= !chunk.is_empty();
                let mut remaining = chunk.as_ref();
                while !remaining.is_empty() {
                    let take = remaining.len().min(MULTIPART_PART_SIZE - buffer.len());
                    buffer.extend_from_slice(&remaining[..take]);
                    remaining = &remaining[take..];
                    if buffer.len() == MULTIPART_PART_SIZE {
                        upload
                            .put_part(std::mem::take(&mut buffer).freeze().into())
                            .await
                            .map_err(IoOrbitError::from)?;
                        buffer.reserve(MULTIPART_PART_SIZE);
                    }
                }
            }
            if !saw_data {
                return Ok::<bool, IoOrbitError>(false);
            }
            if !buffer.is_empty() {
                upload
                    .put_part(buffer.freeze().into())
                    .await
                    .map_err(IoOrbitError::from)?;
            }

            upload.complete().await.map_err(IoOrbitError::from)?;

            Ok(true)
        }
        .await;

        match res {
            Ok(true) => Ok(()),
            // S3 cannot complete a multipart upload with no parts. Abort its
            // staging session before publishing the permitted empty-object PUT.
            Ok(false) => {
                upload.abort().await.map_err(IoOrbitError::from)?;
                store
                    .put(path, PutPayload::new())
                    .await
                    .map_err(IoOrbitError::from)?;
                Ok(())
            }
            Err(error) => {
                if let Err(abort_error) = upload.abort().await {
                    return Err(IoOrbitError::Other(format!(
                        "{error}; failed to abort multipart upload: {abort_error}"
                    )));
                }
                Err(error)
            }
        }
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

#[cfg(test)]
mod tests {
    use std::{
        fmt, io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use futures::stream::BoxStream;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, PutMultipartOptions,
        PutResult, UploadPart, memory::InMemory, path::Path,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct UploadStats {
        started: usize,
        part_lengths: Vec<usize>,
        finished_parts: usize,
        single_lengths: Vec<usize>,
        complete_calls: usize,
        abort_calls: usize,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Failures {
        part: bool,
        complete: bool,
        abort: bool,
        unsupported: bool,
    }

    #[derive(Debug, Default)]
    struct StrictStore {
        inner: InMemory,
        stats: Arc<Mutex<UploadStats>>,
        failures: Failures,
    }

    impl fmt::Display for StrictStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("StrictStore")
        }
    }

    fn injected_error(message: &'static str) -> object_store::Error {
        object_store::Error::Generic {
            store: "StrictStore",
            source: Box::new(io::Error::other(message)),
        }
    }

    #[derive(Debug)]
    struct StrictUpload {
        inner: Box<dyn MultipartUpload>,
        stats: Arc<Mutex<UploadStats>>,
        failures: Failures,
    }

    #[async_trait::async_trait]
    impl MultipartUpload for StrictUpload {
        fn put_part(&mut self, payload: PutPayload) -> UploadPart {
            let length = payload.content_length();
            {
                let mut stats = self.stats.lock().unwrap();
                if let Some(previous) = stats.part_lengths.last() {
                    assert_eq!(*previous, MULTIPART_PART_SIZE, "short non-final part");
                }
                assert!(length > 0 && length <= MULTIPART_PART_SIZE);
                stats.part_lengths.push(length);
            }
            if self.failures.part {
                return Box::pin(async { Err(injected_error("injected part failure")) });
            }
            let part = self.inner.put_part(payload);
            let stats = Arc::clone(&self.stats);
            Box::pin(async move {
                part.await?;
                stats.lock().unwrap().finished_parts += 1;
                Ok(())
            })
        }

        async fn complete(&mut self) -> object_store::Result<PutResult> {
            self.stats.lock().unwrap().complete_calls += 1;
            if self.failures.complete {
                return Err(injected_error("injected complete failure"));
            }
            self.inner.complete().await
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            self.stats.lock().unwrap().abort_calls += 1;
            if self.failures.abort {
                return Err(injected_error("injected abort failure"));
            }
            self.inner.abort().await
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for StrictStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            // Bounded writes may use a single PUT only for an empty object.
            assert_eq!(payload.content_length(), 0);
            self.stats.lock().unwrap().single_lengths.push(0);
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.stats.lock().unwrap().started += 1;
            if self.failures.unsupported {
                return Err(object_store::Error::NotSupported {
                    source: Box::new(io::Error::other("multipart disabled")),
                });
            }
            Ok(Box::new(StrictUpload {
                inner: self.inner.put_multipart_opts(location, options).await?,
                stats: Arc::clone(&self.stats),
                failures: self.failures,
            }))
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
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

    #[tokio::test]
    async fn multipart_aggregates_fixed_parts_with_backpressure() {
        let bytes = Bytes::from(
            (0..2 * MULTIPART_PART_SIZE + 17)
                .map(|index| (index as u8).wrapping_mul(31))
                .collect::<Vec<_>>(),
        );
        for length in [2 * MULTIPART_PART_SIZE, bytes.len()] {
            for input_size in [4096, bytes.len()] {
                let store = StrictStore::default();
                let path = Path::from("object");
                let expected = bytes.slice(..length);
                let input = expected.clone();
                let stats = Arc::clone(&store.stats);
                let source = stream::unfold(0, move |offset| {
                    let input = input.clone();
                    let stats = Arc::clone(&stats);
                    async move {
                        // A full part must complete before the source is polled
                        // for more bytes, even for tiny reader-stream items.
                        assert_eq!(
                            stats.lock().unwrap().finished_parts,
                            offset / MULTIPART_PART_SIZE
                        );
                        if offset == input.len() {
                            None
                        } else {
                            let end = (offset + input_size).min(input.len());
                            Some((Ok(input.slice(offset..end)), end))
                        }
                    }
                });
                ObjectStoreAdapter::put_multipart_bounded_to(&store, &path, Box::pin(source))
                    .await
                    .unwrap();
                assert_eq!(
                    store.get(&path).await.unwrap().bytes().await.unwrap(),
                    expected
                );
                let stats = store.stats.lock().unwrap();
                let mut lengths = vec![MULTIPART_PART_SIZE; 2];
                if length % MULTIPART_PART_SIZE != 0 {
                    lengths.push(length % MULTIPART_PART_SIZE);
                }
                assert_eq!(stats.part_lengths, lengths);
                assert_eq!(stats.complete_calls, 1);
                assert_eq!(stats.abort_calls, 0);
                assert!(stats.single_lengths.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn multipart_empty_object_uses_empty_put() {
        for empty_items in [0, 3] {
            let store = StrictStore::default();
            let path = Path::from("empty");
            let source = stream::iter((0..empty_items).map(|_| Ok(Bytes::new())));
            ObjectStoreAdapter::put_multipart_bounded_to(&store, &path, Box::pin(source))
                .await
                .unwrap();
            assert_eq!(store.head(&path).await.unwrap().size, 0);
            let stats = store.stats.lock().unwrap();
            assert_eq!(stats.started, 1);
            assert_eq!(stats.abort_calls, 1);
            assert_eq!(stats.complete_calls, 0);
            assert_eq!(stats.single_lengths, [0]);
        }
    }

    #[tokio::test]
    async fn multipart_failures_abort_and_preserve_existing_object() {
        for failures in [
            Failures::default(),
            Failures {
                part: true,
                ..Default::default()
            },
            Failures {
                complete: true,
                ..Default::default()
            },
            Failures {
                abort: true,
                ..Default::default()
            },
        ] {
            let store = StrictStore {
                failures,
                ..Default::default()
            };
            let path = Path::from("existing");
            store.inner.put(&path, "keep me".into()).await.unwrap();
            let mut items = vec![Ok(Bytes::from(vec![1; MULTIPART_PART_SIZE]))];
            if !failures.part && !failures.complete {
                items.push(Err(io::Error::other("injected stream failure")));
            }
            let error = ObjectStoreAdapter::put_multipart_bounded_to(
                &store,
                &path,
                Box::pin(stream::iter(items)),
            )
            .await
            .unwrap_err()
            .to_string();
            let phase = if failures.part {
                "part"
            } else if failures.complete {
                "complete"
            } else {
                "stream"
            };
            assert!(
                error.contains(&format!("injected {phase} failure")),
                "{error}"
            );
            if failures.abort {
                assert!(error.contains("injected abort failure"), "{error}");
            }
            assert_eq!(
                store.get(&path).await.unwrap().bytes().await.unwrap(),
                "keep me"
            );
            let stats = store.stats.lock().unwrap();
            assert_eq!(stats.abort_calls, 1);
            assert_eq!(stats.complete_calls, usize::from(failures.complete));
            assert!(stats.single_lengths.is_empty());
        }
    }

    #[tokio::test]
    async fn bounded_first_stream_error_aborts_and_preserves_existing_object() {
        for abort_fails in [false, true] {
            let store = StrictStore {
                failures: Failures {
                    abort: abort_fails,
                    ..Default::default()
                },
                ..Default::default()
            };
            let path = Path::from("first-stream-error");
            store.inner.put(&path, "keep me".into()).await.unwrap();
            let source =
                stream::once(async { Err(io::Error::other("injected first stream failure")) });

            let error =
                ObjectStoreAdapter::put_multipart_bounded_to(&store, &path, Box::pin(source))
                    .await
                    .unwrap_err()
                    .to_string();

            assert!(error.contains("injected first stream failure"), "{error}");
            if abort_fails {
                assert!(error.contains("injected abort failure"), "{error}");
            }
            assert_eq!(
                store.get(&path).await.unwrap().bytes().await.unwrap(),
                "keep me"
            );
            let stats = store.stats.lock().unwrap();
            assert_eq!(stats.started, 1);
            assert_eq!(stats.abort_calls, 1);
            assert_eq!(stats.complete_calls, 0);
            assert!(stats.part_lengths.is_empty());
        }
    }

    #[tokio::test]
    async fn multipart_unsupported_does_not_fall_back_to_buffering() {
        let store = StrictStore {
            failures: Failures {
                unsupported: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = Path::from("unsupported");
        let source = stream::once(async { Ok(Bytes::from_static(b"data")) });
        assert!(
            ObjectStoreAdapter::put_multipart_bounded_to(&store, &path, Box::pin(source))
                .await
                .is_err()
        );
        let stats = store.stats.lock().unwrap();
        assert_eq!(stats.started, 1);
        assert!(stats.single_lengths.is_empty());
    }

    #[tokio::test]
    async fn ordinary_multipart_keeps_per_input_part_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::clone(&local)),
            upload_strategy: UploadStrategy::Multipart,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "b".repeat(64),
        };
        let path = key.to_object_store_path();
        let target = local.path_to_filesystem(&path).unwrap();
        let observed_parent = target.parent().unwrap().to_path_buf();
        let first = stream::once(async { Ok(Bytes::from_static(b"first")) });
        let second = stream::once(async move {
            assert!(std::fs::read_dir(observed_parent).unwrap().any(|entry| {
                std::fs::File::open(entry.unwrap().path())
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .len()
                    == 5
            }));
            Ok(Bytes::from_static(b"second"))
        });

        adapter
            .put_stream(&key, Box::pin(first.chain(second)), ObjectMeta::default())
            .await
            .unwrap();

        assert_eq!(
            local.get(&path).await.unwrap().bytes().await.unwrap(),
            "firstsecond"
        );
    }

    #[tokio::test]
    async fn bounded_local_upload_streams_then_atomically_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::clone(&local)),
            upload_strategy: UploadStrategy::SinglePut,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "a".repeat(64),
        };
        let path = key.to_object_store_path();
        local.put(&path, "old object".into()).await.unwrap();
        let target = local.path_to_filesystem(&path).unwrap();
        let observed_path = path.clone();
        let observed_local = Arc::clone(&local);
        let observed_parent = target.parent().unwrap().to_path_buf();
        let first = stream::once(async { Ok(Bytes::from(vec![7; MULTIPART_PART_SIZE])) });
        let last = stream::once(async move {
            assert_eq!(
                observed_local
                    .get(&observed_path)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
                "old object"
            );
            // A full part must already be staged before EOF; a SinglePut would
            // leave only the original small object until the whole stream ends.
            assert!(std::fs::read_dir(observed_parent).unwrap().any(|entry| {
                std::fs::File::open(entry.unwrap().path())
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .len()
                    == MULTIPART_PART_SIZE as u64
            }));
            Ok(Bytes::from_static(b"tail"))
        });
        adapter
            .put_stream_bounded(&key, Box::pin(first.chain(last)), ObjectMeta::default())
            .await
            .unwrap();
        let expected = local.get(&path).await.unwrap().bytes().await.unwrap();
        assert_eq!(
            &expected[..MULTIPART_PART_SIZE],
            vec![7; MULTIPART_PART_SIZE]
        );
        assert_eq!(&expected[MULTIPART_PART_SIZE..], b"tail");

        let failed = stream::iter(vec![
            Ok(Bytes::from(vec![3; MULTIPART_PART_SIZE])),
            Err(io::Error::other("injected stream failure")),
        ]);
        assert!(
            adapter
                .put_stream_bounded(&key, Box::pin(failed), ObjectMeta::default())
                .await
                .is_err()
        );
        assert_eq!(
            local.get(&path).await.unwrap().bytes().await.unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn bounded_write_validates_key_before_polling_input() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(
                LocalFileSystem::new_with_prefix(dir.path()).unwrap(),
            )),
            upload_strategy: UploadStrategy::SinglePut,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "../escape".to_owned(),
        };
        let polls = Arc::new(AtomicUsize::new(0));
        let stream_polls = Arc::clone(&polls);
        let data = stream::once(async move {
            stream_polls.fetch_add(1, Ordering::Relaxed);
            Ok::<Bytes, io::Error>(Bytes::from_static(b"must not be read"))
        });

        let error = adapter
            .put_stream_bounded(&key, Box::pin(data), ObjectMeta::default())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("invalid object key"));
        assert_eq!(polls.load(Ordering::Relaxed), 0);
    }
}
