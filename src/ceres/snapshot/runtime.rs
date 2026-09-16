//! In-process MST/2 runtime state: snapshot contexts, leases and cursor MAC.
//!
//! Slice scope: contexts/leases live in memory and are lost on restart —
//! clients re-resolve (spec 04 §4 permits this for `latest`). Durable lease
//! and retention integration arrives with T06 (spec 09/10).

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blake3::Hasher;
use uuid::Uuid;

use crate::ceres::snapshot::{
    descriptor::BuiltDescriptor,
    error::{SnapshotError, SnapshotErrorCode},
};

/// One resolved snapshot. `descriptor` is the codec-built canonical
/// descriptor; everything else is the resolve envelope the client received.
#[derive(Debug, Clone)]
pub struct SnapshotContext {
    pub built: BuiltDescriptor,
    pub commit_oid: String,
    /// Root tree oid of the fixed view (pinned at resolve time).
    pub root_tree_oid: String,
    pub lease_id: String,
    pub lease_expires_at_unix: u64,
    pub authorization_epoch: u64,
}

pub struct Mst2Runtime {
    hmac_key: [u8; 32],
    contexts: Mutex<HashMap<String, SnapshotContext>>,
    /// Provisional publication sequence: bumped whenever the served main tip
    /// changes. Replaced by the real publication model (spec 09) in T05.
    tip_state: Mutex<(String, u64)>,
}

static RUNTIME: OnceLock<Mst2Runtime> = OnceLock::new();

pub fn runtime() -> &'static Mst2Runtime {
    RUNTIME.get_or_init(|| Mst2Runtime {
        hmac_key: blake3_key(),
        contexts: Mutex::new(HashMap::new()),
        tip_state: Mutex::new((String::new(), 0u64)),
    })
}

fn blake3_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    // Random per process; cursor validity is process-lifetime by design.
    let u1 = Uuid::new_v4();
    let u2 = Uuid::new_v4();
    k[..16].copy_from_slice(u1.as_bytes());
    k[16..].copy_from_slice(u2.as_bytes());
    k
}

impl Mst2Runtime {
    /// Provisional monotonic publication sequence keyed on the main tip.
    pub fn publication_sequence(&self, commit_oid: &str) -> u64 {
        let mut st = self.tip_state.lock().unwrap();
        if st.0 != commit_oid {
            st.0 = commit_oid.to_string();
            st.1 += 1;
        }
        st.1
    }

    /// Insert a context and create a lease. The in-memory lease carries no
    /// GC duties yet (no metadata GC exists in this slice); it only lets the
    /// client address the snapshot.
    pub fn insert_context(
        &self,
        built: BuiltDescriptor,
        commit_oid: &str,
        root_tree_oid: &str,
        lease_seconds: u64,
    ) -> SnapshotContext {
        let lease_id = Uuid::new_v4().to_string();
        let expires = now_unix() + lease_seconds.clamp(1, 3600);
        let snapshot_id = built.snapshot_id.clone();
        let ctx = SnapshotContext {
            built,
            commit_oid: commit_oid.to_string(),
            root_tree_oid: root_tree_oid.to_string(),
            lease_id: lease_id.clone(),
            lease_expires_at_unix: expires,
            authorization_epoch: 1,
        };
        self.contexts
            .lock()
            .unwrap()
            .insert(snapshot_id, ctx.clone());
        ctx
    }

    /// Look up a snapshot context by snapshot_id and validate its lease.
    pub fn context(&self, snapshot_id: &str) -> Result<SnapshotContext, SnapshotError> {
        let ctx = self.contexts.lock().unwrap().get(snapshot_id).cloned();
        match ctx {
            None => Err(SnapshotError::new(
                SnapshotErrorCode::SnapshotUnknown,
                "unknown snapshot_id: resolve first; the slice keeps contexts in memory only",
            )),
            Some(ctx) if ctx.lease_expires_at_unix < now_unix() => Err(SnapshotError::new(
                SnapshotErrorCode::LeaseExpired,
                "lease expired; re-resolve the snapshot",
            )),
            Some(ctx) => Ok(ctx),
        }
    }

    /// HMAC over the cursor payload (keyed BLAKE3): cursors are
    /// server-authenticated and reject client modification (spec 04 §6).
    pub fn sign_cursor(&self, payload: &str) -> String {
        let mut h = Hasher::new_keyed(&self.hmac_key);
        h.update(payload.as_bytes());
        h.finalize().to_hex().to_string()
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// RFC3339 UTC from a unix timestamp (no external chrono dependency needed
/// for the slice; second precision).
pub fn rfc3339(unix: u64) -> String {
    let days = unix / 86400;
    let secs_of_day = unix % 86400;
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_789_525_722), "2026-09-16T02:28:42Z");
    }

    #[test]
    fn cursor_signature_is_payload_bound() {
        let r = runtime();
        let s1 = r.sign_cursor("snap|/dir|last");
        let s2 = r.sign_cursor("snap|/dir|last");
        assert_eq!(s1, s2);
        assert_ne!(s1, r.sign_cursor("snap|/dir|last2"));
    }
}
