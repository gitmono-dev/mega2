//! Log storage abstraction over [`MegaObjectStorage`].
//!
//! Each log is identified by an [`ObjectKey`] (e.g. `{task_id}/{repo_name}/{build_id}`).
//! Data is stored as segments; a manifest per log holds `len` and segment metadata.

use serde::{Deserialize, Serialize};

use super::{
    error::{IoOrbitError, OrbitResult},
    object_storage::{MegaObjectStorage, ObjectByteStream, ObjectKey, ObjectMeta},
};

/// Manifest for a single log stream.
///
/// Serialized (e.g. JSON) and stored as one object per log. Tracks current length
/// and the list of segments that make up the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogManifest {
    /// Current log length in bytes (= next append offset). Incremented on each append.
    pub len: u64,

    /// Ordered list of segments. Each segment covers `[offset, offset + len)`.
    /// TODO: Future extensions to manifest (e.g., checksum, compression flags, etc.).
    pub segments: Vec<LogSegmentMeta>,
}

/// Metadata for one segment of a log.
///
/// Segment data is stored as a separate object; this struct records its position
/// in the logical log and its storage key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSegmentMeta {
    /// Start offset of this segment in the log (bytes).
    pub offset: u64,

    /// Length of this segment in bytes.
    pub len: u64,

    /// Object storage key for this segment (e.g. `{log_key}/segments/{offset}-{end}` or custom).
    pub key: String,
}

impl LogManifest {
    /// Validates structural invariants before the read path trusts the manifest.
    ///
    /// This append-only log stores contiguous segments, so a valid manifest has:
    /// - segments starting at offset 0, contiguous and ascending (no gaps, no
    ///   overlap, no reordering);
    /// - each segment with non-zero length and a non-empty, path-safe key;
    /// - the last segment's end equal to `len` (and `len == 0` iff there are no
    ///   segments).
    ///
    /// A corrupt manifest is rejected with an error instead of silently
    /// producing truncated or duplicated reads.
    pub fn validate(&self) -> OrbitResult<()> {
        if self.segments.is_empty() {
            if self.len != 0 {
                return Err(manifest_err(format!(
                    "manifest has no segments but len is {}",
                    self.len
                )));
            }
            return Ok(());
        }

        let mut expected_offset: u64 = 0;
        for (i, seg) in self.segments.iter().enumerate() {
            if seg.offset != expected_offset {
                return Err(manifest_err(format!(
                    "segment {i} offset {} is not contiguous (expected {expected_offset}); \
                     gaps, overlaps, and reordering are not allowed",
                    seg.offset
                )));
            }
            if seg.len == 0 {
                return Err(manifest_err(format!("segment {i} has zero length")));
            }
            validate_segment_key(i, &seg.key)?;
            expected_offset = expected_offset.checked_add(seg.len).ok_or_else(|| {
                manifest_err(format!("segment {i} length overflows total log length"))
            })?;
        }

        if expected_offset != self.len {
            return Err(manifest_err(format!(
                "segments cover {expected_offset} bytes but manifest len is {}",
                self.len
            )));
        }
        Ok(())
    }
}

fn manifest_err(msg: String) -> IoOrbitError {
    IoOrbitError::Other(format!("invalid log manifest: {msg}"))
}

fn validate_segment_key(i: usize, key: &str) -> OrbitResult<()> {
    if key.is_empty() {
        return Err(manifest_err(format!("segment {i} has an empty key")));
    }
    if !key.is_ascii() {
        return Err(manifest_err(format!("segment {i} key is not ASCII")));
    }
    if key.bytes().any(|b| b.is_ascii_control()) {
        return Err(manifest_err(format!(
            "segment {i} key contains control characters"
        )));
    }
    if key.split('/').any(|s| s == "." || s == "..") {
        return Err(manifest_err(format!(
            "segment {i} key contains a path-traversal component"
        )));
    }
    Ok(())
}

/// Append-only log storage built on top of [`MegaObjectStorage`].
///
/// Notes:
/// - `key: &ObjectKey` identifies the **entire log stream** (e.g.
///   `{task_id}/{repo_name}/{build_id}`), not an individual segment object.
/// - Segment object keys are an implementation detail recorded in
///   [`LogSegmentMeta::key`]; callers should not construct or rely on them.
/// - Implementations may choose any manifest/segment layout as long as reads and
///   appends preserve the logical log semantics.
///
/// # Memory / buffering (current implementation)
///
/// The default `object_store`-backed implementation is **not** fully streaming
/// yet: `append` / `append_concurrently` buffer the entire input before splitting
/// it into segments, and `read_range` / `read_lines_range` aggregate their result
/// in memory before returning it. To keep this bounded, an append or read that
/// exceeds the implementation's configured size limit returns an error instead of
/// risking unbounded allocation / OOM. Callers handling very large logs should
/// append and read in chunks.
#[async_trait::async_trait]
pub trait LogStorage: MegaObjectStorage {
    /// Appends `data` to the end of the log identified by `key`.
    ///
    /// # Arguments
    /// * `key` - Log identifier (e.g. `task_id/repo_name/build_id`). Not a segment key.
    /// * `data` - Byte stream to append.
    /// * `meta` - Optional metadata for the append.
    ///
    /// # Returns
    /// * `Ok(())` - Append completed successfully.
    async fn append(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()>;

    /// Reads the byte range `[offset, offset + length)` from the log identified by `key`.
    ///
    /// # Arguments
    /// * `key` - Log identifier. Not a segment key.
    /// * `offset` - Start byte offset (inclusive).
    /// * `length` - Number of bytes to read.
    ///
    /// # Returns
    /// * `Ok(stream)` - Byte stream for the requested range.
    async fn read_range(
        &self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> OrbitResult<ObjectByteStream>;

    /// Reads a **line range** `[start_line, end_line)` from the log identified by `key`.
    ///
    /// - `start_line` is inclusive, `end_line` is exclusive.
    /// - Line counting is implementation-defined (typically `\n`-delimited).
    ///
    /// # Returns
    /// * `Ok(stream)` - Byte stream containing the requested lines.
    async fn read_lines_range(
        &self,
        key: &ObjectKey,
        start_line: u64,
        end_line: u64,
    ) -> OrbitResult<ObjectByteStream>;

    /// Appends `data` to the end of the log, safe under contention **only on
    /// backends that support conditional writes (CAS)**.
    ///
    /// Concurrency safety is backend-dependent, not universal:
    /// - S3 / GCS: an optimistic conditional manifest write (compare-and-swap)
    ///   prevents lost updates when multiple writers append concurrently.
    /// - Local filesystem: there is no conditional-write support, so this method
    ///   is **not** safe under contention. Implementations should return an error
    ///   rather than silently risk lost updates; use [`LogStorage::append`] for
    ///   single-writer local logs.
    ///
    /// Even where CAS is available, it only guards concurrent writers sharing the
    /// same backend; it is not a cross-process/cross-region coordination
    /// mechanism.
    ///
    /// # Arguments
    /// * `key` - Log identifier. Not a segment key.
    /// * `data` - Byte stream to append.
    /// * `meta` - Optional metadata.
    ///
    /// # Returns
    /// * `Ok(())` - Append completed successfully.
    async fn append_concurrently(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()>;

    /// Loads the manifest for the log identified by `key`.
    ///
    /// # Arguments
    /// * `key` - Log identifier. Not a segment key.
    ///
    /// # Returns
    /// * `Ok(manifest)` - The current manifest. Implementations may return an
    ///   "empty" manifest (e.g. `len = 0`) when the log does not exist yet.
    async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest>;

    /// Checks whether the log identified by `key` exists.
    ///
    /// Implementations are free to decide what "exists" means (e.g. manifest present,
    /// segments present, etc.). Callers should not rely on manifest/segment layout.
    async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool>;
}

#[cfg(test)]
mod tests {
    use super::{LogManifest, LogSegmentMeta};

    fn seg(offset: u64, len: u64, key: &str) -> LogSegmentMeta {
        LogSegmentMeta {
            offset,
            len,
            key: key.to_string(),
        }
    }

    #[test]
    fn valid_contiguous_manifest_ok() {
        let m = LogManifest {
            len: 6,
            segments: vec![seg(0, 3, "s/0"), seg(3, 3, "s/1")],
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn empty_manifest_ok() {
        let m = LogManifest {
            len: 0,
            segments: vec![],
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn empty_with_nonzero_len_rejected() {
        let m = LogManifest {
            len: 5,
            segments: vec![],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn overlap_rejected() {
        // Second segment starts before the first ends.
        let m = LogManifest {
            len: 6,
            segments: vec![seg(0, 4, "a"), seg(3, 3, "b")],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn gap_rejected() {
        let m = LogManifest {
            len: 7,
            segments: vec![seg(0, 3, "a"), seg(4, 3, "b")],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn reorder_rejected() {
        let m = LogManifest {
            len: 6,
            segments: vec![seg(3, 3, "b"), seg(0, 3, "a")],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn len_mismatch_rejected() {
        let m = LogManifest {
            len: 10,
            segments: vec![seg(0, 3, "a"), seg(3, 3, "b")],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn zero_len_segment_rejected() {
        let m = LogManifest {
            len: 3,
            segments: vec![seg(0, 0, "a"), seg(0, 3, "b")],
        };
        assert!(m.validate().is_err());
    }

    #[test]
    fn bad_segment_key_rejected() {
        let empty_key = LogManifest {
            len: 3,
            segments: vec![seg(0, 3, "")],
        };
        assert!(empty_key.validate().is_err());

        let traversal = LogManifest {
            len: 3,
            segments: vec![seg(0, 3, "a/../b")],
        };
        assert!(traversal.validate().is_err());
    }
}
