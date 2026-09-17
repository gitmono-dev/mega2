//! Atomic publication coordinator core (spec 09 §2/§3).
//!
//! This is the transactional state machine behind a namespace
//! publication: a prepare fixes a canonical operation id, an expected
//! current head (CAS) and a read-set predicate; a short publish step
//! advances a monotonic sequence, stores an immutable receipt and emits
//! an outbox event — all under one store transaction. A second prepare
//! against the same expected-old loses the CAS, and replaying the same
//! operation id returns the original receipt.
//!
//! Persistence is behind [`PublicationStore`] so the guarantees can be
//! proved deterministically with [`mem::InMemoryPublicationStore`]
//! without a database. The Postgres backend and the receive-pack
//! interception are the remaining integration seam (spec 09 steps A–I
//! at every writer entry point); until then the coordinator exists but
//! is not wired into ref updates and is gated off by configuration.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// Monotonic publication position for one namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PublicationPosition {
    /// 1-based sequence; 0 means "nothing published yet".
    pub sequence: u64,
    /// Writer epoch that owns this position; older epochs are fenced.
    pub epoch: u64,
}

/// A prepare request from one writer (spec 09 §2 MutationPlan).
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    pub operation_id: String,
    pub namespace: String,
    /// OID the writer read as current. Empty means "create from empty".
    pub expected_old: String,
    /// OID the writer wants to publish.
    pub expected_new: String,
    /// Predicates the publish re-checks (path -> predicate digest),
    /// proving the read-set has not changed since prepare.
    pub read_set: Vec<ReadPredicate>,
    pub writer_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPredicate {
    pub path: String,
    /// Predicate the read-set asserts (e.g. "absent" or a content digest).
    pub assertion: String,
}

/// Immutable proof a publication happened (spec 09 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationReceipt {
    pub operation_id: String,
    pub namespace: String,
    pub sequence: u64,
    pub old_oid: String,
    pub new_oid: String,
    pub writer_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEvent {
    pub operation_id: String,
    pub namespace: String,
    pub sequence: u64,
    /// 0 = not yet dispatched; the transaction appends it at sequence.
    pub dispatched: bool,
}

/// Result of a prepare: a fresh lease or the existing receipt on replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    Prepared {
        /// Stable id distinct from the business operation id.
        prepare_id: String,
    },
    /// Idempotent replay (PUB-11): the operation was already committed.
    AlreadyCommitted(OperationReceipt),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub receipt: OperationReceipt,
    pub outbox: OutboxEvent,
}

/// The durable seam a deployment must implement (Postgres in production).
pub trait PublicationStore {
    /// Read the current position of a namespace.
    fn current(&self, namespace: &str) -> PublicationPosition;
    /// Read the head OID recorded at the current position ("" if none).
    fn head_oid(&self, namespace: &str) -> String;
    /// Return the receipt for a committed operation, if present.
    fn receipt(&self, operation_id: &str) -> Option<OperationReceipt>;
    /// Evaluate every read-set predicate against current state; all must hold.
    fn read_set_holds(&self, predicates: &[ReadPredicate]) -> bool;
    /// Atomically advance the position, persist the receipt and append an
    /// outbox event. Implementations must serialize concurrent commits so
    /// exactly one writer wins the CAS on `expected_old`.
    fn commit(&self, req: &PrepareRequest) -> Result<CommitOutcome, SnapshotError>;
}

/// Orchestrates prepares over a [`PublicationStore`].
pub struct PublicationCoordinator<S: PublicationStore> {
    store: S,
}

impl<S: PublicationStore> PublicationCoordinator<S> {
    pub fn new(store: S) -> Self {
        PublicationCoordinator { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Prepare/fence (spec 09 §2). Validates epoch and the expected-old CAS
    /// up front and records the read-set for final verification at commit.
    pub fn prepare(&self, req: &PrepareRequest) -> Result<PrepareOutcome, SnapshotError> {
        // Idempotent replay wins before anything else (PUB-11).
        if let Some(existing) = self.store.receipt(&req.operation_id) {
            return Ok(PrepareOutcome::AlreadyCommitted(existing));
        }
        let current = self.store.current(&req.namespace);
        if req.writer_epoch < current.epoch {
            // An older writer must not publish (epoch fence).
            return Err(SnapshotError::new(
                SnapshotErrorCode::Conflict,
                format!(
                    "writer epoch {} fenced by epoch {}",
                    req.writer_epoch, current.epoch
                ),
            ));
        }
        let head = self.store.head_oid(&req.namespace);
        if head != req.expected_old {
            // Including the phantom predicate: someone else advanced the ref.
            return Err(SnapshotError::new(
                SnapshotErrorCode::Conflict,
                format!(
                    "CAS lost: expected old {}, current {}",
                    req.expected_old, head
                ),
            ));
        }
        Ok(PrepareOutcome::Prepared {
            prepare_id: format!("prep-{}", req.operation_id),
        })
    }

    /// Final publish (spec 09 steps E–H): re-check the read-set and let the
    /// store perform the atomic CAS + receipt + outbox transaction. A
    /// commit retried after a previous success returns the original
    /// receipt/outbox (PUB-11) rather than rolling it back.
    pub fn commit(&self, req: &PrepareRequest) -> Result<CommitOutcome, SnapshotError> {
        if !self.store.read_set_holds(&req.read_set)
            && self.store.receipt(&req.operation_id).is_none()
        {
            // Read-set phantom: a predicate changed between prepare and
            // commit. An already-committed replay still succeeds below.
            return Err(SnapshotError::new(
                SnapshotErrorCode::Conflict,
                "publication read-set no longer holds",
            ));
        }
        self.store.commit(req)
    }
}

/// Deterministic in-memory store used to prove the publication guarantees.
pub mod mem {
    use super::*;

    #[derive(Default)]
    struct NamespaceState {
        position: PublicationPosition,
        head: String,
    }

    /// Thread-safe in-memory implementation; the single Mutex serializes
    /// commits exactly as a serializable DB transaction would.
    pub struct InMemoryPublicationStore {
        inner: Mutex<MemInner>,
    }

    #[derive(Default)]
    struct MemInner {
        namespaces: HashMap<String, NamespaceState>,
        receipts: HashMap<String, OperationReceipt>,
        outbox: HashMap<String, OutboxEvent>,
        /// Predicate table: path -> current assertion value.
        paths: HashMap<String, String>,
    }

    impl Default for InMemoryPublicationStore {
        fn default() -> Self {
            InMemoryPublicationStore {
                inner: Mutex::new(MemInner::default()),
            }
        }
    }

    impl InMemoryPublicationStore {
        pub fn set_path(&self, path: &str, assertion: &str) {
            let mut g = self.inner.lock().unwrap();
            g.paths.insert(path.to_string(), assertion.to_string());
        }

        pub fn bump_epoch(&self, namespace: &str) {
            let mut g = self.inner.lock().unwrap();
            let st = g.namespaces.entry(namespace.to_string()).or_default();
            st.position.epoch += 1;
        }
    }

    impl PublicationStore for InMemoryPublicationStore {
        fn current(&self, namespace: &str) -> PublicationPosition {
            let g = self.inner.lock().unwrap();
            g.namespaces
                .get(namespace)
                .map(|s| s.position)
                .unwrap_or(PublicationPosition {
                    sequence: 0,
                    epoch: 0,
                })
        }

        fn head_oid(&self, namespace: &str) -> String {
            let g = self.inner.lock().unwrap();
            g.namespaces
                .get(namespace)
                .map(|s| s.head.clone())
                .unwrap_or_default()
        }

        fn receipt(&self, operation_id: &str) -> Option<OperationReceipt> {
            let g = self.inner.lock().unwrap();
            g.receipts.get(operation_id).cloned()
        }

        fn read_set_holds(&self, predicates: &[ReadPredicate]) -> bool {
            let g = self.inner.lock().unwrap();
            predicates.iter().all(|p| {
                g.paths
                    .get(&p.path)
                    .map(|v| v == &p.assertion)
                    .unwrap_or(false)
            })
        }

        fn commit(&self, req: &PrepareRequest) -> Result<CommitOutcome, SnapshotError> {
            let mut g = self.inner.lock().unwrap();
            // Idempotent replay (PUB-11): a retried commit after success
            // returns the original receipt/outbox and never advances twice.
            if let Some(receipt) = g.receipts.get(&req.operation_id) {
                return Ok(CommitOutcome {
                    receipt: receipt.clone(),
                    outbox: g
                        .outbox
                        .get(&req.operation_id)
                        .cloned()
                        .expect("outbox event for committed operation"),
                });
            }
            let st = g.namespaces.entry(req.namespace.clone()).or_default();
            if req.writer_epoch < st.position.epoch {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::Conflict,
                    "commit epoch fenced",
                ));
            }
            if st.head != req.expected_old {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::Conflict,
                    "commit CAS lost",
                ));
            }
            st.position.sequence += 1;
            // Committing establishes the winning writer's epoch.
            st.position.epoch = st.position.epoch.max(req.writer_epoch);
            st.head = req.expected_new.clone();
            let receipt = OperationReceipt {
                operation_id: req.operation_id.clone(),
                namespace: req.namespace.clone(),
                sequence: st.position.sequence,
                old_oid: req.expected_old.clone(),
                new_oid: req.expected_new.clone(),
                writer_epoch: req.writer_epoch,
            };
            let outbox = OutboxEvent {
                operation_id: req.operation_id.clone(),
                namespace: req.namespace.clone(),
                sequence: st.position.sequence,
                dispatched: false,
            };
            g.receipts.insert(req.operation_id.clone(), receipt.clone());
            g.outbox.insert(req.operation_id.clone(), outbox.clone());
            Ok(CommitOutcome { receipt, outbox })
        }
    }
}


#[cfg(test)]
mod tests {
    use super::mem::InMemoryPublicationStore;
    use super::*;

    fn req(op: &str, old: &str, new: &str) -> PrepareRequest {
        PrepareRequest {
            operation_id: op.to_string(),
            namespace: "ns".to_string(),
            expected_old: old.to_string(),
            expected_new: new.to_string(),
            read_set: vec![],
            writer_epoch: 1,
        }
    }

    #[test]
    fn first_commit_advances_sequence_and_emits_outbox() {
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        let r = req("op-1", "", "A");
        assert!(matches!(
            coord.prepare(&r),
            Ok(PrepareOutcome::Prepared { .. })
        ));
        let out = coord.commit(&r).unwrap();
        assert_eq!(out.receipt.sequence, 1);
        assert_eq!(out.receipt.new_oid, "A");
        assert!(!out.outbox.dispatched);
        assert_eq!(out.outbox.sequence, 1);
        assert_eq!(coord.store().head_oid("ns"), "A");
    }

    #[test]
    fn competing_cas_only_one_wins() {
        // Two writers both prepared against the empty head. The store
        // serializes commits; the loser sees a conflict rather than
        // overwriting the winner.
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        let winner = req("win", "", "A");
        let loser = req("lose", "", "B");
        coord.prepare(&winner).unwrap();
        coord.prepare(&loser).unwrap();
        coord.commit(&winner).unwrap();
        let err = coord.commit(&loser).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Conflict);
        assert_eq!(coord.store().head_oid("ns"), "A");
    }

    #[test]
    fn stale_prepare_after_another_commit_conflicts() {
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        coord.commit(&req("first", "", "A")).unwrap();
        // A second writer still believes the head is empty.
        let stale = req("second", "", "B");
        let err = coord.prepare(&stale).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Conflict);
    }

    #[test]
    fn replay_returns_same_receipt() {
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        let r = req("op", "", "A");
        coord.prepare(&r).unwrap();
        let committed = coord.commit(&r).unwrap().receipt;
        // Replaying the same operation id after commit is idempotent and
        // does not allocate a new sequence.
        match coord.prepare(&r).unwrap() {
            PrepareOutcome::AlreadyCommitted(receipt) => assert_eq!(receipt, committed),
            other => panic!("expected replay, got {other:?}"),
        }
        assert_eq!(coord.store().current("ns").sequence, 1);
    }

    #[test]
    fn read_set_phantom_blocks_commit() {
        let store = InMemoryPublicationStore::default();
        store.set_path("/x", "digest-1");
        let coord = PublicationCoordinator::new(store);
        let mut r = req("op", "", "A");
        r.read_set = vec![ReadPredicate {
            path: "/x".to_string(),
            assertion: "digest-1".to_string(),
        }];
        coord.prepare(&r).unwrap();
        // Another writer changes /x before finalize.
        coord.store().set_path("/x", "digest-2");
        let err = coord.commit(&r).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Conflict);
        assert_eq!(coord.store().head_oid("ns"), "");
    }

    #[test]
    fn epoch_fence_rejects_stale_writer() {
        let store = InMemoryPublicationStore::default();
        let coord = PublicationCoordinator::new(store);
        coord.commit(&req("a", "", "A")).unwrap();
        coord.store().bump_epoch("ns");
        // A writer still on epoch 1 cannot prepare against epoch 2.
        let mut stale = req("b", "A", "B");
        stale.writer_epoch = 1;
        let err = coord.prepare(&stale).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Conflict);
    }

    #[test]
    fn commit_retry_after_success_returns_same_receipt() {
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        let r = req("op", "", "A");
        coord.prepare(&r).unwrap();
        let first = coord.commit(&r).unwrap();
        // A response-lost retry must not roll back or double-advance.
        let again = coord.commit(&r).unwrap();
        assert_eq!(again.receipt, first.receipt);
        assert_eq!(again.outbox, first.outbox);
        assert_eq!(coord.store().current("ns").sequence, 1);
    }

    #[test]
    fn sequential_commits_chain_old_to_new() {
        let coord = PublicationCoordinator::new(InMemoryPublicationStore::default());
        coord.commit(&req("a", "", "A")).unwrap();
        coord.commit(&req("b", "A", "B")).unwrap();
        let c = req("c", "B", "C");
        let out = coord.commit(&c).unwrap();
        assert_eq!(out.receipt.sequence, 3);
        assert_eq!(coord.store().head_oid("ns"), "C");
    }
}
