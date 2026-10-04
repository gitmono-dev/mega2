//! In-process MST/2 runtime state: snapshot contexts, leases and cursor MAC.
//!
//! Slice scope: contexts/leases live in memory and are lost on restart —
//! clients re-resolve (spec 04 §4 permits this for `latest`). Durable lease
//! and retention integration arrives with T06 (spec 09/10).
//!
//! A context is the pinned descriptor/root data; leases are separate
//! `lease_id -> snapshot_id` records so several clients (or repeated
//! resolves) can hold independent retention claims on one fixed view. A
//! snapshot is servable while at least one of its leases is unexpired.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, OnceLock},
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

/// Pinned view data shared by every lease on one snapshot.
#[derive(Debug, Clone)]
struct ContextData {
    built: BuiltDescriptor,
    commit_oid: String,
    root_tree_oid: String,
}

#[derive(Debug, Clone)]
struct LeaseRec {
    snapshot_id: String,
    expires_at_unix: u64,
}

pub struct Mst2Runtime {
    hmac_key: [u8; 32],
    contexts: Mutex<HashMap<String, ContextData>>,
    leases: Mutex<HashMap<String, LeaseRec>>,
    /// Retention graph anchor (spec 10): resolve/lease/pin/prepare roots
    /// keep derived pages and chunk maps reachable; release lets the
    /// collector reclaim them. Fail-closed: retention errors surface,
    /// never silently drop coverage.
    retention: crate::ceres::snapshot::retention::RetentionCoordinator<
        crate::ceres::snapshot::retention::mem::InMemoryRetentionStore,
    >,
    /// Provisional publication sequence: bumped whenever the served main tip
    /// changes. Replaced by the real publication model (spec 09) in T05.
    tip_state: Mutex<(String, u64)>,
}

static RUNTIME: OnceLock<Mst2Runtime> = OnceLock::new();

pub fn runtime() -> &'static Mst2Runtime {
    RUNTIME.get_or_init(|| Mst2Runtime {
        hmac_key: blake3_key(),
        contexts: Mutex::new(HashMap::new()),
        leases: Mutex::new(HashMap::new()),
        retention: crate::ceres::snapshot::retention::RetentionCoordinator::new(
            crate::ceres::snapshot::retention::mem::InMemoryRetentionStore::default(),
        ),
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

    /// Acquire the complete in-memory retention group before exposing a
    /// context or lease. These placeholder anchors do not yet represent
    /// the complete durable page/chunk graph required by T06.
    pub fn insert_context(
        &self,
        built: BuiltDescriptor,
        commit_oid: &str,
        root_tree_oid: &str,
        lease_seconds: u64,
    ) -> Result<SnapshotContext, SnapshotError> {
        let lease_id = Uuid::new_v4().to_string();
        let expires = now_unix() + lease_seconds.clamp(1, 3600);
        let snapshot_id = built.snapshot_id.clone();
        let ctx = SnapshotContext {
            built: built.clone(),
            commit_oid: commit_oid.to_string(),
            root_tree_oid: root_tree_oid.to_string(),
            lease_id: lease_id.clone(),
            lease_expires_at_unix: expires,
            authorization_epoch: 1,
        };
        // Serialize registration with release/expiry/renewal. No path holds
        // contexts while acquiring leases, so this lock order cannot cycle.
        let mut leases = self.leases.lock().unwrap();
        let root = crate::ceres::snapshot::retention::RetentionRoot::Lease(lease_id.clone());
        let page_node = crate::ceres::snapshot::retention::RetentionNode {
            id: format!("page:{}", built.metadata_root),
            kind: crate::ceres::snapshot::retention::RetainedKind::Page,
            state: crate::ceres::snapshot::retention::NodeState::Live,
            bytes: 0,
        };
        let projection_node = crate::ceres::snapshot::retention::RetentionNode {
            id: format!("projection:{}", commit_oid),
            kind: crate::ceres::snapshot::retention::RetainedKind::ChunkMap,
            state: crate::ceres::snapshot::retention::NodeState::Live,
            bytes: 0,
        };
        self.retention
            .pin_root(&root, &[page_node, projection_node])?;
        self.contexts.lock().unwrap().insert(
            snapshot_id.clone(),
            ContextData {
                built,
                commit_oid: commit_oid.to_string(),
                root_tree_oid: root_tree_oid.to_string(),
            },
        );
        leases.insert(
            lease_id,
            LeaseRec {
                snapshot_id,
                expires_at_unix: expires,
            },
        );
        Ok(ctx)
    }

    /// Look up a snapshot context by snapshot_id. It is servable while at
    /// least one unexpired lease references it; storage failure is never
    /// reported as absence.
    pub fn context(&self, snapshot_id: &str) -> Result<SnapshotContext, SnapshotError> {
        let data = self
            .contexts
            .lock()
            .unwrap()
            .get(snapshot_id)
            .cloned()
            .ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::SnapshotGone,
                    "unknown snapshot_id: resolve first; the slice keeps contexts in memory only",
                )
            })?;
        let (lease_id, expires) = self.active_lease(snapshot_id)?;
        Ok(SnapshotContext {
            built: data.built,
            commit_oid: data.commit_oid,
            root_tree_oid: data.root_tree_oid,
            lease_id,
            lease_expires_at_unix: expires,
            authorization_epoch: 1,
        })
    }

    /// One active (unexpired) lease for `snapshot_id`, pruning expired rows
    /// as it goes.
    fn active_lease(&self, snapshot_id: &str) -> Result<(String, u64), SnapshotError> {
        let mut leases = self.leases.lock().unwrap();
        self.prune_expired_leases(&mut leases, now_unix());
        leases
            .iter()
            .find(|(_, r)| r.snapshot_id == snapshot_id)
            .map(|(id, r)| (id.clone(), r.expires_at_unix))
            .ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::LeaseExpired,
                    "no active lease for this snapshot; re-resolve",
                )
            })
    }

    /// The leases lock serializes expiry with renewal and registration;
    /// releasing one expired lease never removes another lease's coverage.
    /// The current in-memory store's release operation is infallible.
    fn prune_expired_leases(&self, leases: &mut HashMap<String, LeaseRec>, now: u64) {
        leases.retain(|lease_id, rec| {
            if rec.expires_at_unix >= now {
                return true;
            }
            self.retention
                .release(&crate::ceres::snapshot::retention::RetentionRoot::Lease(
                    lease_id.clone(),
                ));
            false
        });
    }

    /// Validate the lease a request presented (spec 04 §1: identity lives in
    /// the `X-Mega-Snapshot-Lease` header, and a read path must check it —
    /// knowing the snapshot id alone is not a capability). Any lease that is
    /// not currently active — unknown, released or expired, or bound to a
    /// different snapshot — is LEASE_EXPIRED (spec 14 §5: 410, FUSE ESTALE);
    /// the client's only recovery is re-resolving. A lease id that never
    /// existed is answered identically: probing with lease ids must not
    /// distinguish released from forged.
    ///
    /// Expired-row pruning happens in `active_lease` and collection: this
    /// runs on every authenticated read and stays a single key lookup.
    pub fn validate_lease(&self, snapshot_id: &str, lease_id: &str) -> Result<(), SnapshotError> {
        let leases = self.leases.lock().unwrap();
        match leases.get(lease_id) {
            Some(r) if r.snapshot_id == snapshot_id && r.expires_at_unix >= now_unix() => Ok(()),
            _ => Err(SnapshotError::new(
                SnapshotErrorCode::LeaseExpired,
                "lease is not active for this snapshot; re-resolve",
            )),
        }
    }

    /// Extend the same snapshot's lease. An unknown lease is 404; an expired
    /// lease cannot be revived — the client must re-resolve (spec 04 §4:
    /// renewal never changes authorization or version).
    pub fn renew_lease(
        &self,
        lease_id: &str,
        lease_seconds: u64,
    ) -> Result<LeaseRenewed, SnapshotError> {
        Self::renew_lease_with_clock(
            lease_id,
            lease_seconds,
            || self.leases.lock().unwrap(),
            now_unix,
        )
    }

    /// Sample the clock only after acquiring the lease lock. Separate
    /// sources let tests force a deadline crossing during lock contention.
    fn renew_lease_with_clock<'a>(
        lease_id: &str,
        lease_seconds: u64,
        acquire_leases: impl FnOnce() -> MutexGuard<'a, HashMap<String, LeaseRec>>,
        clock: impl FnOnce() -> u64,
    ) -> Result<LeaseRenewed, SnapshotError> {
        let mut leases = acquire_leases();
        let now = clock();
        let Some(rec) = leases.get_mut(lease_id) else {
            return Err(SnapshotError::new(
                SnapshotErrorCode::LeaseUnknown,
                "unknown lease_id",
            ));
        };
        if rec.expires_at_unix < now {
            return Err(SnapshotError::new(
                SnapshotErrorCode::LeaseExpired,
                "lease expired; re-resolve instead of renewing",
            ));
        }
        // Renewal extends from the current deadline (never shortens it).
        rec.expires_at_unix = rec
            .expires_at_unix
            .max(now)
            .saturating_add(lease_seconds.clamp(1, 3600));
        Ok(LeaseRenewed {
            lease_id: lease_id.to_string(),
            snapshot_id: rec.snapshot_id.clone(),
            expires_at_unix: rec.expires_at_unix,
        })
    }

    /// Idempotent release (spec 04 §2): an unknown lease is already
    /// released, not an error. Releasing never deletes Git content; it
    /// only drops the retention root so a later collection pass may
    /// reclaim derived pages no other lease/pin covers.
    pub fn release_lease(&self, lease_id: &str) -> bool {
        let mut leases = self.leases.lock().unwrap();
        let removed = leases.remove(lease_id).is_some();
        self.retention
            .release(&crate::ceres::snapshot::retention::RetentionRoot::Lease(
                lease_id.to_string(),
            ));
        removed
    }

    /// Run one retention collection pass with the fail-closed reaper.
    /// Exposed for tests and the future maintenance endpoint; nothing
    /// deletes Git raw blobs here (spec 10 §7).
    pub fn collect_retention(
        &self,
    ) -> Result<crate::ceres::snapshot::retention::CollectionReport, SnapshotError> {
        self.prune_expired_leases(&mut self.leases.lock().unwrap(), now_unix());
        self.retention
            .collect(&crate::ceres::snapshot::retention::NoopReaper)
    }

    /// HMAC over the cursor payload (keyed BLAKE3): cursors are
    /// server-authenticated and reject client modification (spec 04 §6).
    pub fn sign_cursor(&self, payload: &str) -> String {
        let mut h = Hasher::new_keyed(&self.hmac_key);
        h.update(payload.as_bytes());
        h.finalize().to_hex().to_string()
    }
}

/// Result of a successful lease renewal.
#[derive(Debug, Clone)]
pub struct LeaseRenewed {
    pub lease_id: String,
    pub snapshot_id: String,
    pub expires_at_unix: u64,
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
    use std::{
        sync::{
            Arc, TryLockError,
            atomic::{AtomicU64, Ordering},
            mpsc::sync_channel,
        },
        thread,
    };

    use mst2_codec::descriptor::ServingDescriptor;

    use super::*;
    use crate::ceres::snapshot::retention::{
        NodeState, RetainedKind, RetentionRoot, RetentionStore as _,
    };

    fn isolated_runtime() -> Mst2Runtime {
        Mst2Runtime {
            hmac_key: blake3_key(),
            contexts: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            retention: crate::ceres::snapshot::retention::RetentionCoordinator::new(
                crate::ceres::snapshot::retention::mem::InMemoryRetentionStore::default(),
            ),
            tip_state: Mutex::new((String::new(), 0u64)),
        }
    }

    fn descriptor(root_byte: u8) -> BuiltDescriptor {
        let descriptor = ServingDescriptor {
            instance_uuid: [0x11; 16],
            namespace_view_id: [0x22; 32],
            scope: "/".into(),
            metadata_root: [root_byte; 32],
        };
        BuiltDescriptor {
            instance_id: Uuid::from_bytes(descriptor.instance_uuid).to_string(),
            snapshot_id: format!(
                "sha256:{}",
                crate::ceres::snapshot::view::hex(&descriptor.snapshot_id().unwrap())
            ),
            metadata_root: format!(
                "sha256:{}",
                crate::ceres::snapshot::view::hex(&descriptor.metadata_root)
            ),
            descriptor,
        }
    }

    #[test]
    fn pin_failure_exposes_no_context_lease_or_partial_graph_and_can_retry() {
        let r = isolated_runtime();
        let built = descriptor(3);
        r.retention.store().fail_next_retain_for_test();
        assert_eq!(
            r.insert_context(built.clone(), "commit", "tree", 60)
                .unwrap_err()
                .code,
            SnapshotErrorCode::Internal
        );
        assert!(r.contexts.lock().unwrap().is_empty());
        assert!(r.leases.lock().unwrap().is_empty());
        assert!(r.retention.store().all_live().is_empty());
        assert_eq!(
            r.context(&built.snapshot_id).unwrap_err().code,
            SnapshotErrorCode::SnapshotGone
        );

        let ctx = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        r.validate_lease(&built.snapshot_id, &ctx.lease_id).unwrap();
        assert!(
            r.retention
                .is_retained(&format!("page:{}", built.metadata_root))
        );
        assert!(r.retention.is_retained("projection:commit"));
    }

    #[test]
    fn failed_resolve_preserves_an_existing_active_snapshot() {
        let r = isolated_runtime();
        let old = descriptor(3);
        let active = r
            .insert_context(old.clone(), "old-commit", "old-tree", 60)
            .unwrap();
        let new = descriptor(4);
        r.retention.store().fail_next_retain_for_test();
        assert!(
            r.insert_context(new.clone(), "new-commit", "new-tree", 60)
                .is_err()
        );
        assert_eq!(r.contexts.lock().unwrap().len(), 1);
        assert_eq!(r.leases.lock().unwrap().len(), 1);
        assert_eq!(
            r.context(&old.snapshot_id).unwrap().lease_id,
            active.lease_id
        );
        assert_eq!(
            r.context(&new.snapshot_id).unwrap_err().code,
            SnapshotErrorCode::SnapshotGone
        );
        assert!(
            r.retention
                .store()
                .node(&format!("page:{}", new.metadata_root))
                .is_none()
        );
        assert!(r.retention.store().node("projection:new-commit").is_none());
    }

    #[test]
    fn resolve_cannot_create_a_lease_for_a_deleting_group() {
        let r = isolated_runtime();
        let built = descriptor(3);
        let ctx = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        assert!(r.release_lease(&ctx.lease_id));
        assert!(r.collect_retention().unwrap().reaped.is_empty());
        assert_eq!(
            r.insert_context(built.clone(), "commit", "tree", 60)
                .unwrap_err()
                .code,
            SnapshotErrorCode::ObjectUnavailable
        );
        assert!(r.leases.lock().unwrap().is_empty());
        assert!(
            r.retention.store().all_live().is_empty(),
            "no new lease anchor leaked"
        );
        assert_eq!(
            r.context(&built.snapshot_id).unwrap_err().code,
            SnapshotErrorCode::LeaseExpired
        );
        assert_eq!(
            r.retention
                .store()
                .node(&format!("page:{}", built.metadata_root))
                .unwrap()
                .state,
            NodeState::Deleting
        );
    }

    #[test]
    fn context_expiry_releases_only_the_expired_lease_on_a_shared_snapshot() {
        let r = isolated_runtime();
        let built = descriptor(3);
        let expired = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        let active = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        r.leases
            .lock()
            .unwrap()
            .get_mut(&expired.lease_id)
            .unwrap()
            .expires_at_unix = 0;

        assert_eq!(
            r.context(&built.snapshot_id).unwrap().lease_id,
            active.lease_id
        );
        assert!(!r.leases.lock().unwrap().contains_key(&expired.lease_id));
        assert!(
            !r.retention
                .store()
                .root_covers(&format!("lease:{}", expired.lease_id))
        );
        assert!(
            r.retention
                .store()
                .root_covers(&format!("lease:{}", active.lease_id))
        );
        assert!(
            r.retention
                .is_retained(&format!("page:{}", built.metadata_root))
        );
        r.validate_lease(&built.snapshot_id, &active.lease_id)
            .unwrap();
        assert_eq!(
            r.validate_lease(&built.snapshot_id, &expired.lease_id)
                .unwrap_err()
                .code,
            SnapshotErrorCode::LeaseExpired
        );
        assert_eq!(
            r.collect_retention().unwrap().unreachable,
            vec![format!("lease:{}", expired.lease_id)]
        );
        assert_eq!(
            r.context(&built.snapshot_id).unwrap().lease_id,
            active.lease_id
        );

        assert!(r.release_lease(&active.lease_id));
        assert!(
            !r.retention
                .store()
                .root_covers(&format!("page:{}", built.metadata_root))
        );
        assert!(!r.retention.store().root_covers("projection:commit"));
    }

    #[test]
    fn collection_prunes_expired_roots_without_a_context_read() {
        let r = isolated_runtime();
        let built = descriptor(3);
        let ctx = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        r.leases
            .lock()
            .unwrap()
            .get_mut(&ctx.lease_id)
            .unwrap()
            .expires_at_unix = 0;
        let report = r.collect_retention().unwrap();
        assert_eq!(report.unreachable.len(), 3, "anchor, page and projection");
        assert!(report.reaped.is_empty(), "NoopReaper stays fail-closed");
        assert!(r.leases.lock().unwrap().is_empty());
        for id in [
            format!("lease:{}", ctx.lease_id),
            format!("page:{}", built.metadata_root),
            "projection:commit".into(),
        ] {
            assert!(report.unreachable.contains(&id));
            assert!(!r.retention.store().root_covers(&id));
            assert_eq!(
                r.retention.store().node(&id).unwrap().state,
                NodeState::Deleting
            );
        }
    }

    #[test]
    fn renewal_winning_before_expiry_keeps_its_root_and_expiry_winning_prevents_renewal() {
        let r = isolated_runtime();
        let built = descriptor(3);
        let ctx = r
            .insert_context(built.clone(), "commit", "tree", 60)
            .unwrap();
        let renewed = r.renew_lease(&ctx.lease_id, 60).unwrap();
        {
            let mut leases = r.leases.lock().unwrap();
            // Advance past the old deadline, before the renewed deadline.
            r.prune_expired_leases(&mut leases, ctx.lease_expires_at_unix + 1);
        }
        r.validate_lease(&built.snapshot_id, &ctx.lease_id).unwrap();
        assert!(
            r.retention
                .store()
                .root_covers(&format!("lease:{}", ctx.lease_id))
        );
        {
            let mut leases = r.leases.lock().unwrap();
            r.prune_expired_leases(&mut leases, renewed.expires_at_unix + 1);
        }
        assert!(
            !r.retention
                .store()
                .root_covers(&format!("lease:{}", ctx.lease_id))
        );
        assert_eq!(
            r.renew_lease(&ctx.lease_id, 60).unwrap_err().code,
            SnapshotErrorCode::LeaseUnknown
        );
        assert_eq!(
            r.context(&built.snapshot_id).unwrap_err().code,
            SnapshotErrorCode::LeaseExpired
        );
    }

    #[test]
    fn renewal_waiting_for_the_lease_lock_cannot_use_a_pre_expiry_clock_sample() {
        let r = Arc::new(isolated_runtime());
        let ctx = r
            .insert_context(descriptor(3), "commit", "tree", 60)
            .unwrap();
        let clock = Arc::new(AtomicU64::new(9));
        let (blocked_tx, blocked_rx) = sync_channel(1);
        let mut leases = r.leases.lock().unwrap();
        leases.get_mut(&ctx.lease_id).unwrap().expires_at_unix = 10;
        let renewal = {
            let r = Arc::clone(&r);
            let clock = Arc::clone(&clock);
            let lease_id = ctx.lease_id.clone();
            thread::spawn(move || {
                Mst2Runtime::renew_lease_with_clock(
                    &lease_id,
                    60,
                    || {
                        // Prove contention before allowing the clock to cross
                        // the deadline. A pre-lock sample would observe 9.
                        assert!(matches!(r.leases.try_lock(), Err(TryLockError::WouldBlock)));
                        blocked_tx.send(()).unwrap();
                        r.leases.lock().unwrap()
                    },
                    || clock.load(Ordering::SeqCst),
                )
            })
        };
        blocked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("renewal did not contend for the lease lock");
        clock.store(11, Ordering::SeqCst);
        drop(leases);

        assert_eq!(
            renewal.join().unwrap().unwrap_err().code,
            SnapshotErrorCode::LeaseExpired
        );
        assert_eq!(
            r.leases
                .lock()
                .unwrap()
                .get(&ctx.lease_id)
                .unwrap()
                .expires_at_unix,
            10
        );
        assert_eq!(r.leases.lock().unwrap().len(), 1, "no replacement lease");
        assert!(r.collect_retention().unwrap().reaped.is_empty());
        assert!(
            !r.retention
                .store()
                .root_covers(&format!("lease:{}", ctx.lease_id))
        );
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_789_525_722), "2026-09-16T02:28:42Z");
    }

    #[test]
    fn lease_lifecycle_drives_retention() {
        let r = isolated_runtime();
        // Cover a derived node directly with a manual test lease root.
        let lease_root = RetentionRoot::Lease("gc-lease-1".to_string());
        let page = crate::ceres::snapshot::retention::RetentionNode {
            id: "page:sha256:test".to_string(),
            kind: RetainedKind::Page,
            state: NodeState::Live,
            bytes: 100,
        };
        r.retention
            .store()
            .retain(page.clone(), &[], std::slice::from_ref(&lease_root))
            .unwrap();
        // While the lease root covers it, collection keeps the page.
        let report = r.collect_retention().unwrap();
        assert!(report.unreachable.is_empty());

        // Releasing the lease drops the root; the page becomes
        // unreachable and a pass marks it (NoopReaper never deletes).
        r.release_lease("gc-lease-1");
        let report = r.collect_retention().unwrap();
        assert_eq!(report.unreachable, vec!["page:sha256:test".to_string()]);
        assert_eq!(report.reclaimed_bytes, 100);
        assert!(report.reaped.is_empty(), "fail-closed reaper");
        // The node stays DELETING; a new acquire must not resurrect it.
        assert_eq!(
            r.retention.store().node("page:sha256:test").unwrap().state,
            NodeState::Deleting
        );
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
