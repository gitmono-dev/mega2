//! Snapshot cursor for one bounded traces query (MTR-42).
//!
//! A query pins the accepted and indexed watermarks when it starts. The
//! cursor carries that snapshot, so a later page reads the same snapshot even
//! when a new push arrives or the background indexer catches up.

use sha2::{Digest, Sha256};

use crate::common::errors::TracesError;

/// The cursor version this module writes and accepts.
pub const SNAPSHOT_CURSOR_VERSION: &str = "v1";

/// The watermarks one query is pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuerySnapshot {
    /// The highest accepted journal sequence the query may see.
    pub accepted_seq: u64,
    /// The highest indexed journal sequence the query may see.
    pub indexed_seq: u64,
    /// The index version the query was served from.
    pub index_version: u64,
}

impl QuerySnapshot {
    /// Pin a new snapshot.
    pub fn new(accepted_seq: u64, indexed_seq: u64, index_version: u64) -> Self {
        Self {
            accepted_seq,
            indexed_seq,
            index_version,
        }
    }
}

/// What a cursor is bound to.
///
/// The binding covers the scope, the filters, the query text and the index
/// version. Changing any of them makes the cursor invalid instead of silently
/// reading a different result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBinding {
    /// The scope the query runs in.
    pub scope: String,
    /// The exact filter and query text the request used.
    pub filters: String,
    /// The index version the snapshot was taken from.
    pub index_version: u64,
}

impl SnapshotBinding {
    /// The stable binding digest of this cursor.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.scope.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.filters.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.index_version.to_string().as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The watermarks a cursor carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCursor {
    /// The pinned watermarks.
    pub snapshot: QuerySnapshot,
    /// The key the next page continues after.
    pub after_key: String,
}

/// Encode one snapshot cursor.
///
/// The cursor is `v1.<binding>.<accepted>.<indexed>.<index_version>.<after>`.
/// The binding keeps a cursor tied to its scope and filters, and the
/// watermarks keep it tied to the snapshot it was taken from.
pub fn encode_cursor(
    binding: &SnapshotBinding,
    snapshot: &QuerySnapshot,
    after_key: &str,
) -> String {
    format!(
        "{SNAPSHOT_CURSOR_VERSION}.{}.{}.{}.{}.{}",
        binding.digest(),
        snapshot.accepted_seq,
        snapshot.indexed_seq,
        snapshot.index_version,
        after_key
    )
}

/// Decode one snapshot cursor.
///
/// A cursor whose binding does not match the current request is rejected, and
/// so is a cursor that is malformed or carries a different index version.
pub fn decode_cursor(
    cursor: &str,
    binding: &SnapshotBinding,
) -> Result<SnapshotCursor, TracesError> {
    let rest = cursor
        .strip_prefix(&format!("{SNAPSHOT_CURSOR_VERSION}."))
        .ok_or(TracesError::InvalidCursor)?;
    let mut parts = rest.splitn(6, '.');
    let digest = parts.next().ok_or(TracesError::InvalidCursor)?;
    let accepted_seq = parts
        .next()
        .ok_or(TracesError::InvalidCursor)?
        .parse::<u64>()
        .map_err(|_| TracesError::InvalidCursor)?;
    let indexed_seq = parts
        .next()
        .ok_or(TracesError::InvalidCursor)?
        .parse::<u64>()
        .map_err(|_| TracesError::InvalidCursor)?;
    let index_version = parts
        .next()
        .ok_or(TracesError::InvalidCursor)?
        .parse::<u64>()
        .map_err(|_| TracesError::InvalidCursor)?;
    let after_key = parts.next().ok_or(TracesError::InvalidCursor)?;
    if parts.next().is_some() {
        return Err(TracesError::InvalidCursor);
    }
    if digest != binding.digest() || index_version != binding.index_version || after_key.is_empty()
    {
        return Err(TracesError::InvalidCursor);
    }
    Ok(SnapshotCursor {
        snapshot: QuerySnapshot::new(accepted_seq, indexed_seq, index_version),
        after_key: after_key.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> SnapshotBinding {
        SnapshotBinding {
            scope: "stream-1".to_owned(),
            filters: "query=needle".to_owned(),
            index_version: 1,
        }
    }

    #[test]
    fn snapshot_cursor_round_trips_the_pinned_watermarks() {
        let binding = binding();
        let snapshot = QuerySnapshot::new(7, 5, 1);
        let cursor = encode_cursor(&binding, &snapshot, "key-1");
        let decoded = decode_cursor(&cursor, &binding).expect("cursor");
        assert_eq!(decoded.snapshot, snapshot);
        assert_eq!(decoded.after_key, "key-1");
    }

    #[test]
    fn snapshot_cursor_rejects_a_changed_scope_filter_or_index_version() {
        let binding = binding();
        let snapshot = QuerySnapshot::new(7, 5, 1);
        let cursor = encode_cursor(&binding, &snapshot, "key-1");

        let other_scope = SnapshotBinding {
            scope: "stream-2".to_owned(),
            ..binding.clone()
        };
        assert!(decode_cursor(&cursor, &other_scope).is_err());

        let other_filters = SnapshotBinding {
            filters: "query=other".to_owned(),
            ..binding.clone()
        };
        assert!(decode_cursor(&cursor, &other_filters).is_err());

        let other_version = SnapshotBinding {
            index_version: 2,
            ..binding.clone()
        };
        assert!(decode_cursor(&cursor, &other_version).is_err());
    }

    #[test]
    fn snapshot_cursor_rejects_a_malformed_or_expired_cursor() {
        let binding = binding();
        assert!(decode_cursor("", &binding).is_err());
        assert!(decode_cursor("v2.a.b.c.d.e", &binding).is_err());
        assert!(decode_cursor("v1.notadigest.1.2.3.key", &binding).is_err());
        assert!(decode_cursor("v1.a.b.c.d.e", &binding).is_err());

        let snapshot = QuerySnapshot::new(7, 5, 1);
        let cursor = encode_cursor(&binding, &snapshot, "");
        assert!(decode_cursor(&cursor, &binding).is_err());
    }
}
