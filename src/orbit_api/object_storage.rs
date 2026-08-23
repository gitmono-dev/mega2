use std::{collections::HashMap, fmt, pin::Pin, time::Duration};

use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use reqwest::Method;

use super::error::{IoOrbitError, OrbitResult};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectKey {
    pub namespace: ObjectNamespace,

    // hash: String,
    /// content hash / logical path
    /// - git: sha1/sha256
    /// - lfs: sha256
    /// - log: path like 2025/03/worker.log
    pub key: String,
}

impl ObjectKey {
    /// Maps this key to its backend object path.
    ///
    /// The output is a **stable storage-format contract** (see
    /// `docs/compatibility.md`); any change is a breaking storage-format change
    /// that requires a migration. There are two stable branches:
    ///
    /// - keys shorter than 6 bytes are stored unsharded as `<namespace>/<key>`;
    /// - otherwise the key is sharded as `<namespace>/aa/bb/cc/<rest>`.
    ///
    /// The namespace prefix must never change for either branch.
    pub fn default_sharding(&self) -> String {
        let id = &self.key;
        if id.len() < 6 {
            // For short keys, don't shard or use a different strategy
            return format!("{}/{}", self.namespace, id);
        }
        format!(
            "{}/{}/{}/{}/{}",
            self.namespace,
            &id[0..2],
            &id[2..4],
            &id[4..6],
            &id[6..]
        )
    }

    pub fn to_object_store_path(&self) -> object_store::path::Path {
        object_store::path::Path::from(self.default_sharding())
    }

    /// Validates that this key is safe to turn into a backend object path.
    ///
    /// Rules (kept minimal so existing valid keys are unaffected):
    /// - non-empty;
    /// - ASCII only, so the byte-based sharding in [`default_sharding`] never
    ///   splits a multi-byte UTF-8 character and panics;
    /// - no ASCII control characters and no backslash;
    /// - no empty path segments (`//`, or a leading/trailing `/`);
    /// - no `.` or `..` path-traversal segments.
    ///
    /// Note: a 6-byte key shards to `<ns>/aa/bb/cc/`, whose empty trailing
    /// segment `object_store` normalizes away; this is a documented, benign
    /// layout characteristic (see `docs/compatibility.md`) and is not rejected.
    ///
    /// [`default_sharding`]: ObjectKey::default_sharding
    pub fn validate(&self) -> OrbitResult<()> {
        let key = &self.key;
        if key.is_empty() {
            return Err(invalid_key("key must not be empty"));
        }
        if !key.is_ascii() {
            return Err(invalid_key("key must be ASCII"));
        }
        if let Some(pos) = key.bytes().position(|b| b.is_ascii_control()) {
            return Err(invalid_key(format!(
                "key must not contain control characters (byte offset {pos})"
            )));
        }
        if key.contains('\\') {
            return Err(invalid_key("key must not contain a backslash"));
        }
        for seg in key.split('/') {
            if seg.is_empty() {
                return Err(invalid_key(
                    "key must not contain empty path segments (no leading/trailing '/' or '//')",
                ));
            }
            if seg == "." || seg == ".." {
                return Err(invalid_key(
                    "key must not contain '.' or '..' path-traversal segments",
                ));
            }
        }
        Ok(())
    }
}

/// Builds an "invalid object key" error (`IoOrbitError::Other`, so no new public
/// error variant is introduced).
fn invalid_key(msg: impl Into<String>) -> IoOrbitError {
    IoOrbitError::Other(format!("invalid object key: {}", msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectNamespace {
    Git,
    Lfs,
    Log,
    /// Artifact protocol objects (`docs/artifacts-protocol.md`), keyed by UUID string.
    Artifact,
    /// Chat attachments.
    Attachment,
}

impl ObjectNamespace {
    fn as_str(&self) -> &'static str {
        match self {
            ObjectNamespace::Git => "git",
            ObjectNamespace::Lfs => "lfs",
            ObjectNamespace::Log => "log",
            ObjectNamespace::Artifact => "artifact",
            ObjectNamespace::Attachment => "attachment",
        }
    }
}

impl fmt::Display for ObjectNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ObjectMeta {
    pub size: i64,
    pub checksum: Option<String>,
    pub content_type: Option<String>,
    /// （ETag / storage-class / custom）
    pub extra: HashMap<String, String>,
}

/// A streaming reader for a single object.
///
/// This represents the raw byte stream of an object, delivered incrementally.
/// Each item in the stream is a chunk of bytes.
///
/// Design notes:
/// - Uses streaming instead of `Vec<u8>` to avoid loading large objects into memory.
/// - Suitable for large blobs, pack files, or any content-addressed storage.
/// - The stream must be fully consumed by the caller.
pub type ObjectByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// A streaming source of multiple objects.
///
/// Each item yields:
/// - `ObjectKey`: identifies the object
/// - `ObjectByteStream`: streaming reader for the object's data
/// - `ObjectMeta`: metadata associated with the object
///
/// Design notes:
/// - Objects are produced lazily and may arrive out of order.
/// - Errors are propagated per-object using `Result`.
/// - This abstraction allows batching and backpressure-aware pipelines.
pub type MultiObjectByteStream<'a> = Pin<
    Box<
        dyn Stream<Item = Result<(ObjectKey, ObjectByteStream, ObjectMeta), IoOrbitError>>
            + Send
            + 'a,
    >,
>;

#[async_trait::async_trait]
pub trait MegaObjectStorage: Send + Sync {
    // fn as_any(&self) -> &dyn Any;

    /// Whether presigned GET/PUT URLs can be generated (e.g. S3/GCS). Local disk returns `false`.
    fn supports_presigned_urls(&self) -> bool {
        false
    }

    /// Upload a single object to the storage backend.
    ///
    /// # Parameters
    /// - `key`: Logical identifier of the object.
    /// - `reader`: Streaming reader providing the object contents.
    /// - `meta`: Object metadata (size, content type, checksums, etc).
    ///
    /// # Semantics
    /// - The implementation should consume the stream exactly once.
    /// - Callers should assume the stream is invalid after this call.
    /// - Implementations may buffer internally, but should prefer streaming.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The upload fails
    /// - The stream produces an I/O error
    /// - Backend-specific constraints are violated
    async fn put_stream(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()>;

    /// Retrieve a single object from the storage backend.
    ///
    /// # Returns
    /// - A streaming reader for the object data
    /// - The object's metadata
    ///
    /// # Semantics
    /// - The returned stream must be consumed by the caller.
    /// - Metadata is returned eagerly, data is streamed lazily.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The object does not exist
    /// - Access is denied
    /// - Backend I/O fails
    async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)>;

    /// Retrieve a range of bytes from an object.
    ///
    /// # Parameters
    /// - `key`: Object identifier
    /// - `start`: Starting byte offset (inclusive)
    /// - `end`: Ending byte offset (exclusive, None means to end of file)
    ///
    /// # Returns
    /// - A streaming reader for the object data range
    /// - The object's metadata
    ///
    /// # Semantics
    /// - Uses HTTP Range requests when supported by the backend.
    /// - For backends that don't support Range requests, falls back to full download.
    /// - The returned stream must be consumed by the caller.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The object does not exist
    /// - Access is denied
    /// - Backend I/O fails
    /// - Range is invalid (start >= end, or start >= file size)
    async fn get_range_stream(
        &self,
        key: &ObjectKey,
        start: u64,
        end: Option<u64>,
    ) -> OrbitResult<(ObjectByteStream, ObjectMeta)>;

    /// Check whether an object exists.
    async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool>;

    /// Generate a presigned download URL when supported by the backend.
    ///
    /// Returns `Ok(None)` if the storage does not support presigning.
    async fn signed_url(
        &self,
        key: &ObjectKey,
        method: Method,
        expires_in: Duration,
    ) -> OrbitResult<Option<String>>;

    /// Upload multiple objects concurrently.
    ///
    /// Objects are provided as a stream, allowing the caller to:
    /// - Generate objects lazily
    /// - Avoid holding all data in memory
    /// - Integrate with upstream pipelines (e.g. Git pack encoding)
    ///
    /// # Parameters
    /// - `objects`: Stream of objects to upload
    /// - `concurrency`: Maximum number of concurrent uploads
    ///
    /// # Semantics
    /// - Uploads are executed concurrently up to `concurrency`.
    /// - If any upload fails, the operation stops and returns the error.
    /// - Partial uploads may have already completed when an error occurs.
    ///
    /// # Errors
    /// Returns the first encountered `IoOrbitError`.
    async fn put_many(
        &self,
        objects: MultiObjectByteStream<'_>,
        concurrency: usize,
    ) -> OrbitResult<()> {
        objects
            .try_for_each_concurrent(concurrency, |(key, stream, meta)| async move {
                self.put_stream(&key, stream, meta)
                    .await
                    // Attach key/context so callers can see *which* object failed
                    // instead of only getting a bare storage error. This avoids
                    .map_err(|e| {
                        IoOrbitError::Other(format!(
                            "object_storage::put_many failed for key={} namespace={} error_chain:\n{}",
                            key.key,
                            key.namespace,
                            dump_error_chain(&e),
                        ))
                    })
            })
            .await
    }

    /// Retrieve multiple objects concurrently.
    ///
    /// # Parameters
    /// - `keys`: Object identifiers to fetch
    /// - `concurrency`: Maximum number of concurrent fetches
    ///
    /// # Returns
    /// A stream yielding objects as they become available.
    ///
    /// # Semantics
    /// - Objects may be yielded out of order.
    /// - Each object is fetched independently.
    /// - Errors are reported per object via `Result`.
    ///
    /// # Typical use cases
    /// - Bulk object export
    /// - Git blob streaming
    /// - Feeding downstream encoders or pack writers
    fn get_many(&self, keys: Vec<ObjectKey>, concurrency: usize) -> MultiObjectByteStream<'_> {
        Box::pin(
            futures::stream::iter(keys)
                .map(move |key| async move {
                    let (stream, meta) = self.get_stream(&key).await?;
                    Ok((key, stream, meta))
                })
                .buffer_unordered(concurrency),
        )
    }

    /// Delete the object at the specified location.
    ///
    /// # Parameters
    /// - `key`: Object identifier
    ///
    /// # Returns
    /// - `Ok(())` if the object is deleted successfully
    /// - `Err(IoOrbitError)` if the object does not exist or deletion fails
    async fn delete(&self, key: &ObjectKey) -> OrbitResult<()>;
}

pub fn dump_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = String::new();
    let mut cur: Option<&dyn std::error::Error> = Some(err);
    let mut level = 0;

    while let Some(e) = cur {
        out.push_str(&format!("[{}] {:?}\n", level, e));
        cur = e.source();
        level += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use ObjectNamespace;

    use super::*;

    #[test]
    fn test_s3_key_lfs() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: "abcdef1234567890".to_string(),
        };

        // Unified 3-level sharding for LFS objects.
        assert_eq!(key.default_sharding(), "lfs/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_s3_key_git() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: "abcdef1234567890".to_string(),
        };

        assert_eq!(key.default_sharding(), "git/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_s3_key_artifact_uuid() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Artifact,
            key: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        };
        assert_eq!(
            key.default_sharding(),
            "artifact/55/0e/84/00-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_default_sharding_basic() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: "abcdef1234567890".to_string(),
        };

        assert_eq!(key.default_sharding(), "git/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_to_object_store_path_basic() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: "abcdef1234567890".to_string(),
        };

        let path = key.to_object_store_path();

        assert_eq!(path.as_ref(), "git/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_sharding_log() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Log,
            key: "abcdef1234567890".to_string(),
        };

        assert_eq!(key.default_sharding(), "log/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_sharding_attachment() {
        let key = ObjectKey {
            namespace: ObjectNamespace::Attachment,
            key: "abcdef1234567890".to_string(),
        };

        assert_eq!(key.default_sharding(), "attachment/ab/cd/ef/1234567890");
    }

    #[test]
    fn test_sharding_short_key_is_unsharded() {
        // Keys shorter than 6 bytes are stored unsharded as `<namespace>/<key>`.
        // This branch is part of the stable storage format (docs/compatibility.md).
        for ns in [
            ObjectNamespace::Git,
            ObjectNamespace::Lfs,
            ObjectNamespace::Log,
            ObjectNamespace::Artifact,
            ObjectNamespace::Attachment,
        ] {
            let key = ObjectKey {
                namespace: ns,
                key: "abc".to_string(),
            };
            assert_eq!(key.default_sharding(), format!("{ns}/abc"));
        }
    }

    #[test]
    fn test_namespace_string_values_are_stable() {
        // These strings are baked into every object's on-disk path. Changing any
        // of them silently relocates existing objects, so they are a stable
        // storage-format contract (see docs/compatibility.md). New namespaces may
        // only be appended with new strings.
        assert_eq!(ObjectNamespace::Git.to_string(), "git");
        assert_eq!(ObjectNamespace::Lfs.to_string(), "lfs");
        assert_eq!(ObjectNamespace::Log.to_string(), "log");
        assert_eq!(ObjectNamespace::Artifact.to_string(), "artifact");
        assert_eq!(ObjectNamespace::Attachment.to_string(), "attachment");
    }

    #[test]
    fn validate_accepts_typical_and_multi_segment_keys() {
        let good = [
            (
                ObjectNamespace::Git,
                "abcdef1234567890abcdef1234567890abcdef12",
            ),
            (
                ObjectNamespace::Artifact,
                "550e8400-e29b-41d4-a716-446655440000",
            ),
            (ObjectNamespace::Log, "2025/03/worker.log"), // multi-segment path key
            (ObjectNamespace::Lfs, "abcde"),              // short (< 6 byte) key
        ];
        for (namespace, k) in good {
            let key = ObjectKey {
                namespace,
                key: k.to_string(),
            };
            assert!(key.validate().is_ok(), "expected {k:?} to validate");
        }
    }

    #[test]
    fn validate_rejects_unsafe_keys() {
        let bad = [
            "",          // empty
            "a/../b",    // traversal segment
            "..",        // traversal
            "/leading",  // leading slash -> empty segment
            "trailing/", // trailing slash -> empty segment
            "a//b",      // double slash -> empty segment
            "a\\b",      // backslash
            "a\nb",      // control character
            "naïve",     // non-ASCII (would byte-slice-panic in default_sharding)
        ];
        for k in bad {
            let key = ObjectKey {
                namespace: ObjectNamespace::Git,
                key: k.to_string(),
            };
            assert!(key.validate().is_err(), "expected {k:?} to be rejected");
        }
    }
}
