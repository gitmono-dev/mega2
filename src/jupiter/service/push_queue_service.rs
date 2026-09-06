use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    callisto::{
        push_queue,
        sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
    },
    common::{
        errors::MegaError,
        utils::{MEGA_BRANCH_NAME, ZERO_ID},
    },
    config::PushPolicy,
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        mono_storage::MonoStorage,
        push_queue_storage::{
            ClaimOutcome, EnqueueOutcome, EnqueueParams, EnqueueRejectReason, PushQueueStorage,
        },
    },
};

/// Default B2 wait budget before the caller abandons (does not cancel the row).
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Default B2 poll interval.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Stable operation fingerprints (trunk-push.md §1.11).
pub fn push_operation_id(old_id: &str, new_id: &str) -> String {
    format!("{old_id}→{new_id}")
}

pub fn merge_operation_id(cl_link: &str) -> String {
    cl_link.to_owned()
}

pub fn attach_operation_id(repo_id: &str, normalized_commands: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(normalized_commands.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub kind: PushQueueKindEnum,
    pub operation_id: String,
    pub path: String,
    pub old_id: String,
    pub new_id: String,
    pub requester: Option<String>,
    pub payload: JsonValue,
    /// For push kind B0: the ref name being updated.
    pub ref_name: Option<String>,
    /// For push kind B0: whether this is a delete command.
    pub is_delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueWaitResult {
    /// Caller should proceed to B3 (claim already committed).
    Ready { id: i64 },
    /// Done replay — no execution needed.
    Replayed {
        id: i64,
        landed_commit_id: Option<String>,
    },
    /// Caller abandoned waiting; row left untouched.
    Abandoned { id: i64 },
    /// Row reached a terminal reject state while waiting.
    Rejected { id: i64, message: String },
}

/// Outcome of B3/B4 execution (after a successful B2.5 claim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteOutcome {
    Done {
        id: i64,
        landed_commit_id: String,
        /// How many root CAS writes this round performed (must be 1 on success).
        root_cas_writes: u32,
    },
    /// Fencing lost the claim (reaper or other raced); caller may retry.
    ClaimLost { id: i64 },
    /// Hard-stop observed; row reset to Queued.
    HardStopped { id: i64 },
    /// Queue-bypass tripwire fired; hard_stopped set and row Failed.
    BypassDetected { id: i64 },
    /// Conflict requeue completed; follow `successor_id` into B2.
    Requeued { id: i64, successor_id: i64 },
    /// Non-conflict failure terminalized.
    Failed {
        id: i64,
        failure: String,
        message: String,
    },
}

/// How the B3 kind stub performs its single root write (TP-07/08/12 replace this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindRootWrite {
    /// Net-zero same-value CAS (tripwire present; root hashes unchanged).
    #[default]
    NetZero,
    /// Advance root to new tip hashes (tests CAS uniqueness / bypass paths).
    Advance,
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub id: i64,
    pub kind_root_write: KindRootWrite,
    /// When `kind_root_write = Advance`, the new root tip hashes.
    pub advance_commit: Option<String>,
    pub advance_tree: Option<String>,
    /// Injected delay after claim observation / before taking MONO_WRITE_LOCK
    /// (fencing regression).
    #[cfg(test)]
    pub pre_lock_delay: Duration,
    /// Force the B4 Conflict requeue path instead of a successful kind stub.
    #[cfg(test)]
    pub force_conflict: bool,
    /// Force a non-Conflict failure (SystemError) after baseline/hard-stop checks.
    #[cfg(test)]
    pub force_failure: Option<String>,
    /// Force the root CAS tripwire to miss (exercises SAVEPOINT fail-close).
    #[cfg(test)]
    pub force_cas_miss: bool,
}

impl Default for ExecuteRequest {
    fn default() -> Self {
        Self {
            id: 0,
            kind_root_write: KindRootWrite::NetZero,
            advance_commit: None,
            advance_tree: None,
            #[cfg(test)]
            pre_lock_delay: Duration::ZERO,
            #[cfg(test)]
            force_conflict: false,
            #[cfg(test)]
            force_failure: None,
            #[cfg(test)]
            force_cas_miss: false,
        }
    }
}

impl ExecuteRequest {
    fn pre_lock_delay(&self) -> Duration {
        #[cfg(test)]
        {
            self.pre_lock_delay
        }
        #[cfg(not(test))]
        {
            Duration::ZERO
        }
    }

    fn force_conflict(&self) -> bool {
        #[cfg(test)]
        {
            self.force_conflict
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn force_failure(&self) -> Option<&str> {
        #[cfg(test)]
        {
            self.force_failure.as_deref()
        }
        #[cfg(not(test))]
        {
            None
        }
    }

    fn force_cas_miss(&self) -> bool {
        #[cfg(test)]
        {
            self.force_cas_miss
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

#[derive(Clone)]
pub struct PushQueueService {
    push_queue_storage: PushQueueStorage,
    mono_storage: MonoStorage,
    push_policy: PushPolicy,
    wait_timeout: Duration,
    poll_interval: Duration,
}

impl PushQueueService {
    pub fn new(base: BaseStorage, push_policy: PushPolicy) -> Self {
        Self {
            push_queue_storage: PushQueueStorage::new(base.clone()),
            mono_storage: MonoStorage { base },
            push_policy,
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    pub fn with_timeouts(mut self, wait_timeout: Duration, poll_interval: Duration) -> Self {
        self.wait_timeout = wait_timeout;
        self.poll_interval = poll_interval;
        self
    }

    pub fn push_policy(&self) -> PushPolicy {
        self.push_policy.clone()
    }

    pub fn storage(&self) -> &PushQueueStorage {
        &self.push_queue_storage
    }

    #[cfg(test)]
    pub fn mono_storage(&self) -> &MonoStorage {
        &self.mono_storage
    }

    /// B0 early reject for push kind under the configured morphology.
    ///
    /// Optimistic NFF precheck is **telemetry only** (see [`b0_nff_telemetry`]):
    /// B0 never rejects on `old_id != tip`. B3 is the sole NFF authority.
    pub fn b0_reject_push(&self, req: &EnqueueRequest) -> Result<(), MegaError> {
        if req.kind != PushQueueKindEnum::Push {
            return Ok(());
        }

        if self.push_policy == PushPolicy::Review {
            return Err(MegaError::Other(
                "push_policy=review: push does not enter MonoWriteQueue (CL pipeline only)".into(),
            ));
        }

        // Trunk morphology gates (test switch).
        let ref_name = req.ref_name.as_deref().unwrap_or("");
        if ref_name != MEGA_BRANCH_NAME {
            return Err(MegaError::Other(format!(
                "trunk push requires ref_name={MEGA_BRANCH_NAME}, got '{ref_name}'"
            )));
        }
        if req.path == "/" {
            return Err(MegaError::Other(
                "trunk push rejects path=/ (root path would collapse root CAS with P landing)"
                    .into(),
            ));
        }
        if req.is_delete || req.new_id == ZERO_ID {
            return Err(MegaError::Other(
                "trunk push rejects delete commands; remove content via a parent-path commit"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Observational NFF precheck — never rejects (ADR-TP / trunk-push §1.5).
    pub fn b0_nff_telemetry(old_id: &str, path_tip: Option<&str>) {
        if let Some(tip) = path_tip
            && old_id != tip
        {
            tracing::debug!(
                old_id,
                tip,
                "optimistic NFF precheck (telemetry only; not rejected at B0)"
            );
        }
    }

    /// B0 + B1: admit or classify (adopt / replay / reject).
    pub async fn enqueue(&self, req: EnqueueRequest) -> Result<EnqueueOutcome, MegaError> {
        self.b0_reject_push(&req)?;
        self.push_queue_storage
            .enqueue_atomic(EnqueueParams {
                kind: req.kind,
                operation_id: &req.operation_id,
                path: &req.path,
                old_id: &req.old_id,
                new_id: &req.new_id,
                requester: req.requester.as_deref(),
                payload: req.payload,
            })
            .await
    }

    /// B2 wait loop + B2.5 claim. Does not run B3.
    ///
    /// `wait_timeout` only applies while the row is still `Queued` (ADR-TP-08 /
    /// trunk-push §1.9 2a). Once observed `Running`, the caller waits for a real
    /// terminal result and is not abandoned by the Queued wait budget.
    pub async fn wait_and_claim(&self, id: i64) -> Result<QueueWaitResult, MegaError> {
        let mut deadline = Instant::now() + self.wait_timeout;
        let mut seen_running = false;
        let mut id = id;
        loop {
            if let Some(row) = self.push_queue_storage.get_by_id(id).await? {
                match row.status {
                    PushQueueStatusEnum::Done => {
                        return Ok(QueueWaitResult::Replayed {
                            id,
                            landed_commit_id: row.landed_commit_id,
                        });
                    }
                    PushQueueStatusEnum::Failed => {
                        let msg = row.error_message.unwrap_or_else(|| {
                            format!("queue item {id} terminal {:?}", row.status)
                        });
                        return Ok(QueueWaitResult::Rejected { id, message: msg });
                    }
                    PushQueueStatusEnum::Cancelled => {
                        // Conflict requeue: follow the successor into B2 (trunk-push §1.5).
                        if let Some(successor) = row.superseded_by {
                            id = successor;
                            seen_running = false;
                            // Fresh Queued wait budget for the successor — time
                            // spent behind the predecessor Running must not burn it.
                            deadline = Instant::now() + self.wait_timeout;
                            continue;
                        }
                        let msg = row.error_message.unwrap_or_else(|| {
                            format!("queue item {id} cancelled without successor")
                        });
                        return Ok(QueueWaitResult::Rejected { id, message: msg });
                    }
                    PushQueueStatusEnum::Running => {
                        // Claimed (by us or another adopter) — wait for terminal;
                        // do not apply wait_timeout.
                        seen_running = true;
                    }
                    PushQueueStatusEnum::Queued => {}
                }
            }

            if !seen_running && Instant::now() >= deadline {
                return Ok(QueueWaitResult::Abandoned { id });
            }

            if let Err(err) = self.push_queue_storage.touch_heartbeat(id).await {
                tracing::warn!(
                    id,
                    error = %err,
                    "push_queue B2 heartbeat refresh failed; continuing wait"
                );
            }

            // Non-authoritative observation: try B2.5 when we look like head.
            match self.push_queue_storage.claim_for_execution(id).await? {
                ClaimOutcome::Claimed => return Ok(QueueWaitResult::Ready { id }),
                ClaimOutcome::HardStopped | ClaimOutcome::Missed => {
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
        }
    }

    /// B3 execution skeleton + B4 failure/requeue (kind branches are stubs until
    /// TP-07/08/12). Caller must already hold a B2.5 claim (`status=Running`).
    pub async fn execute_b3(&self, req: ExecuteRequest) -> Result<ExecuteOutcome, MegaError> {
        use sea_orm::{EntityTrait, TransactionTrait};

        if !req.pre_lock_delay().is_zero() {
            tokio::time::sleep(req.pre_lock_delay()).await;
        }

        let conn = self.push_queue_storage.get_connection();
        let txn = conn.begin().await?;
        PushQueueStorage::acquire_mono_write_lock(&txn).await?;

        // Fencing: hold-lock re-read (reaper may have terminalized between B2.5 and lock).
        let row = push_queue::Entity::find_by_id(req.id)
            .one(&txn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("push_queue id {} missing", req.id)))?;
        if row.status != PushQueueStatusEnum::Running {
            txn.rollback().await?;
            return Ok(ExecuteOutcome::ClaimLost { id: req.id });
        }

        if PushQueueStorage::is_hard_stopped_in_txn(&txn).await? {
            txn.rollback().await?;
            if !self
                .push_queue_storage
                .reset_running_to_queued(req.id)
                .await?
            {
                return Ok(ExecuteOutcome::ClaimLost { id: req.id });
            }
            let ntxn = conn.begin().await?;
            PushQueueStorage::notify_mono_write_queue(&ntxn).await?;
            ntxn.commit().await?;
            return Ok(ExecuteOutcome::HardStopped { id: req.id });
        }

        // Expected-root baseline compare (NULL-safe).
        let root = self.mono_storage.get_main_ref_in_txn("/", &txn).await?;
        let (cur_commit, cur_tree) = match &root {
            Some(r) => (
                Some(r.ref_commit_hash.as_str()),
                Some(r.ref_tree_hash.as_str()),
            ),
            None => (None, None),
        };
        let baseline_ok = cur_commit == row.expected_commit_hash.as_deref()
            && cur_tree == row.expected_tree_hash.as_deref();
        if !baseline_ok {
            // Do not ROLLBACK the outer txn — stay locked while fail-closing.
            // No kind writes precede this branch yet; SAVEPOINT is reserved for
            // post-baseline kind work (CAS path below / TP-07/08/12).
            PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                req.id,
                "QueueBypassDetected",
                "expected_* baseline mismatch (queue-external root writer)",
            )
            .await?;
            if !updated {
                tracing::error!(
                    id = req.id,
                    "B3 baseline fail-closed: Failed update hit 0 rows (reaper raced?)"
                );
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::BypassDetected { id: req.id });
        }

        if req.force_conflict() {
            return self
                .b4_conflict_requeue_holding_lock(txn, req.id, &row)
                .await;
        }

        if let Some(msg) = req.force_failure() {
            PushQueueStorage::savepoint(&txn, "b3_kind").await?;
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            let updated =
                PushQueueStorage::mark_failed_if_running_in_txn(&txn, req.id, "SystemError", msg)
                    .await?;
            if !updated {
                tracing::error!(id = req.id, "B3 Forced failure: Failed update hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: req.id,
                failure: "SystemError".into(),
                message: msg.to_owned(),
            });
        }

        // Kind dispatch skeleton (1.10 deliverable 9 / Description): consult
        // push_policy and branch on kind. Concrete push/merge/attach semantics
        // land in TP-12/07/08; all kinds share the single root CAS stub below.
        let kind_stub = match row.kind {
            PushQueueKindEnum::Push => "push",     // TP-12
            PushQueueKindEnum::Merge => "merge",   // TP-07
            PushQueueKindEnum::Attach => "attach", // TP-08
        };
        tracing::trace!(
            policy = ?self.push_policy,
            kind = kind_stub,
            id = req.id,
            "B3 kind dispatch stub"
        );

        // Kind stub: exactly one root CAS write (net-zero or advance).
        let (new_commit, new_tree) = match req.kind_root_write {
            KindRootWrite::NetZero => (
                cur_commit
                    .map(str::to_owned)
                    .unwrap_or_else(|| ZERO_ID.to_owned()),
                cur_tree
                    .map(str::to_owned)
                    .unwrap_or_else(|| ZERO_ID.to_owned()),
            ),
            KindRootWrite::Advance => (
                req.advance_commit.clone().unwrap_or_else(|| "e".repeat(40)),
                req.advance_tree.clone().unwrap_or_else(|| "f".repeat(40)),
            ),
        };

        // Missing root cannot CAS-update; skeleton treats that as SystemError
        // (create-root belongs to bootstrap / later kind wiring).
        if root.is_none() {
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                req.id,
                "SystemError",
                "B3 skeleton requires an existing root ref for CAS",
            )
            .await?;
            if !updated {
                tracing::error!(id = req.id, "B3 missing-root failure hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: req.id,
                failure: "SystemError".into(),
                message: "missing root".into(),
            });
        }

        // SAVEPOINT wraps all kind business writes + root CAS so fail-close
        // can undo them without releasing MONO_WRITE_LOCK (trunk-push B3/B4).
        PushQueueStorage::savepoint(&txn, "b3_kind").await?;
        let cas_expected_commit = if req.force_cas_miss() {
            Some("0".repeat(40))
        } else {
            cur_commit.map(str::to_owned)
        };
        let cas_ok = self
            .mono_storage
            .cas_update_root_main_ref_in_txn(
                &txn,
                cas_expected_commit.as_deref(),
                cur_tree,
                &new_commit,
                &new_tree,
            )
            .await?;
        let root_cas_writes = u32::from(cas_ok);

        if !cas_ok {
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                req.id,
                "QueueBypassDetected",
                "root CAS affected 0 rows",
            )
            .await?;
            if !updated {
                tracing::error!(
                    id = req.id,
                    "B3 CAS fail-closed: Failed update hit 0 rows; hard_stopped still set"
                );
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::BypassDetected { id: req.id });
        }

        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, req.id, &new_commit).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(id = req.id, "B3 Done update hit 0 rows after fencing");
            return Ok(ExecuteOutcome::ClaimLost { id: req.id });
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Done {
            id: req.id,
            landed_commit_id: new_commit,
            root_cas_writes,
        })
    }

    /// B4 Conflict requeue while still holding the B3 `MONO_WRITE_LOCK` txn.
    ///
    /// Phase 1 (intent) uses an independent short connection; phase 2 completes
    /// under the caller's lock via SAVEPOINT (trunk-push B4).
    async fn b4_conflict_requeue_holding_lock(
        &self,
        txn: sea_orm::DatabaseTransaction,
        id: i64,
        row: &push_queue::Model,
    ) -> Result<ExecuteOutcome, MegaError> {
        use sea_orm::TransactionTrait;

        // Recheck hard_stop before persisting intent (control plane may flip it
        // without holding MONO_WRITE_LOCK).
        if PushQueueStorage::is_hard_stopped_in_txn(&txn).await? {
            txn.rollback().await?;
            if !self.push_queue_storage.reset_running_to_queued(id).await? {
                return Ok(ExecuteOutcome::ClaimLost { id });
            }
            let conn = self.push_queue_storage.get_connection();
            let ntxn = conn.begin().await?;
            PushQueueStorage::notify_mono_write_queue(&ntxn).await?;
            ntxn.commit().await?;
            return Ok(ExecuteOutcome::HardStopped { id });
        }

        if !self
            .push_queue_storage
            .persist_requeue_conflict_intent(id)
            .await?
        {
            txn.rollback().await?;
            return Ok(ExecuteOutcome::ClaimLost { id });
        }

        // Intent persisted; recheck hard_stop before successor INSERT.
        if PushQueueStorage::is_hard_stopped_in_txn(&txn).await? {
            txn.rollback().await?;
            if !self.push_queue_storage.reset_running_to_queued(id).await? {
                return Ok(ExecuteOutcome::ClaimLost { id });
            }
            let conn = self.push_queue_storage.get_connection();
            let ntxn = conn.begin().await?;
            PushQueueStorage::notify_mono_write_queue(&ntxn).await?;
            ntxn.commit().await?;
            return Ok(ExecuteOutcome::HardStopped { id });
        }

        PushQueueStorage::savepoint(&txn, "b4_conflict").await?;
        let successor_id = PushQueueStorage::complete_conflict_requeue_in_txn(&txn, row).await?;
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Requeued { id, successor_id })
    }

    /// Convenience: enqueue then wait/claim (stops before B3).
    pub async fn enqueue_and_wait(
        &self,
        req: EnqueueRequest,
    ) -> Result<QueueWaitResult, MegaError> {
        match self.enqueue(req).await? {
            EnqueueOutcome::Inserted { id } | EnqueueOutcome::Adopted { id } => {
                self.wait_and_claim(id).await
            }
            EnqueueOutcome::Replay {
                id,
                landed_commit_id,
            } => Ok(QueueWaitResult::Replayed {
                id,
                landed_commit_id,
            }),
            EnqueueOutcome::Rejected { reason } => Err(reject_to_error(reason)),
        }
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        use crate::jupiter::storage::base_storage::StorageConnector;
        Self::new(BaseStorage::mock(), PushPolicy::Review)
    }
}

fn reject_to_error(reason: EnqueueRejectReason) -> MegaError {
    match reason {
        EnqueueRejectReason::Paused => MegaError::Other("push_queue is paused".into()),
        EnqueueRejectReason::HardStopped => MegaError::Other("push_queue is hard-stopped".into()),
        EnqueueRejectReason::CapacityExceeded { max_depth } => {
            MegaError::Other(format!("push_queue depth exceeds max_depth={max_depth}"))
        }
        EnqueueRejectReason::ActivePushPathConflict => MegaError::Other(
            "another push for this path is already Queued/Running (ADR-TP-10)".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        callisto::sea_orm_active_enums::PushQueueFailureEnum,
        jupiter::{
            migration::apply_migrations, storage::base_storage::StorageConnector,
            tests::test_db_connection,
        },
    };

    async fn service(policy: PushPolicy) -> (tempfile::TempDir, PushQueueService) {
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(std::sync::Arc::new(db));
        let svc = PushQueueService::new(base, policy)
            .with_timeouts(Duration::from_millis(400), Duration::from_millis(20));
        (temp, svc)
    }

    #[test]
    fn operation_id_fingerprints_are_stable_and_distinct() {
        assert_eq!(
            push_operation_id("aaa", "ccc"),
            push_operation_id("aaa", "ccc")
        );
        assert_ne!(
            push_operation_id("aaa", "ccc"),
            push_operation_id("bbb", "ccc")
        );
        assert_eq!(merge_operation_id("CL1"), "CL1");
        assert_ne!(
            attach_operation_id("repo", "cmd-a"),
            attach_operation_id("repo", "cmd-b")
        );
        assert_eq!(
            attach_operation_id("repo", "cmd-a"),
            attach_operation_id("repo", "cmd-a")
        );
    }

    #[tokio::test]
    async fn b0_review_rejects_push_enqueue() {
        let (_t, svc) = service(PushPolicy::Review).await;
        let err = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(&"a".repeat(40), &"b".repeat(40)),
                path: "/project/x".into(),
                old_id: "a".repeat(40),
                new_id: "b".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: false,
            })
            .await
            .expect_err("review must refuse push enqueue");
        assert!(err.to_string().contains("review"));
    }

    #[tokio::test]
    async fn b0_trunk_rejects_root_path_and_delete() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let root = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: "x→y".into(),
                path: "/".into(),
                old_id: "a".repeat(40),
                new_id: "b".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: false,
            })
            .await
            .expect_err("root path");
        assert!(root.to_string().contains("path=/"));

        let del = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: "x→0".into(),
                path: "/project/x".into(),
                old_id: "a".repeat(40),
                new_id: ZERO_ID.into(),
                requester: None,
                payload: json!({}),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: true,
            })
            .await
            .expect_err("delete");
        assert!(del.to_string().contains("delete"));
    }

    #[tokio::test]
    async fn trunk_merge_enqueue_and_claim() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let outcome = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: merge_operation_id("CL-WAIT"),
                path: "/project/w".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: Some("bob".into()),
                payload: json!({"cl_link": "CL-WAIT"}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("expected insert");
        };
        let wait = svc.wait_and_claim(id).await.unwrap();
        assert_eq!(wait, QueueWaitResult::Ready { id });
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Running);
    }

    #[tokio::test]
    async fn wait_timeout_abandons_without_cancelling() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        // Insert two rows; claim the first so the second never becomes min(id).
        let first = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-A".into(),
                path: "/a".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: first_id } = first else {
            panic!("insert");
        };
        let second = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-B".into(),
                path: "/b".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: second_id } = second else {
            panic!("insert");
        };
        assert!(matches!(
            svc.wait_and_claim(first_id).await.unwrap(),
            QueueWaitResult::Ready { .. }
        ));
        let abandoned = svc.wait_and_claim(second_id).await.unwrap();
        assert_eq!(abandoned, QueueWaitResult::Abandoned { id: second_id });
        let row = svc.storage().get_by_id(second_id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Queued);
    }

    #[test]
    fn b0_nff_telemetry_never_rejects() {
        // Stale tip vs client old_id must not surface as a B0 error.
        PushQueueService::b0_nff_telemetry(&"a".repeat(40), Some(&"b".repeat(40)));
        PushQueueService::b0_nff_telemetry(&"a".repeat(40), None);
        let svc = PushQueueService::mock();
        let req = EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(&"a".repeat(40), &"c".repeat(40)),
            path: "/project/x".into(),
            old_id: "a".repeat(40),
            new_id: "c".repeat(40),
            requester: None,
            payload: json!({}),
            ref_name: Some(MEGA_BRANCH_NAME.into()),
            is_delete: false,
        };
        // Review policy still rejects push — but for NFF reasons alone, trunk
        // morphology would admit; prove B0 has no NFF branch by calling the
        // telemetry helper (above) and confirming trunk B0 accepts mismatched tips.
        drop(svc);
        drop(req);
        let trunk = PushQueueService::new(BaseStorage::mock(), PushPolicy::Trunk);
        assert!(
            trunk
                .b0_reject_push(&EnqueueRequest {
                    kind: PushQueueKindEnum::Push,
                    operation_id: "a→c".into(),
                    path: "/project/x".into(),
                    old_id: "a".repeat(40),
                    new_id: "c".repeat(40),
                    requester: None,
                    payload: json!({}),
                    ref_name: Some(MEGA_BRANCH_NAME.into()),
                    is_delete: false,
                })
                .is_ok(),
            "B0 must not reject when old_id differs from any tip"
        );
    }

    #[tokio::test]
    async fn b2_heartbeat_refreshes_while_waiting() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let first = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-HB-1".into(),
                path: "/hb/1".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: blocker } = first else {
            panic!("insert");
        };
        let second = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-HB-2".into(),
                path: "/hb/2".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = second else {
            panic!("insert");
        };
        // Claim the head so `id` stays Queued and B2 polls + heartbeats.
        assert!(matches!(
            svc.wait_and_claim(blocker).await.unwrap(),
            QueueWaitResult::Ready { .. }
        ));
        let before = svc
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .heartbeat_at;
        let wait = svc
            .clone()
            .with_timeouts(Duration::from_millis(250), Duration::from_millis(30));
        let abandoned = wait.wait_and_claim(id).await.unwrap();
        assert_eq!(abandoned, QueueWaitResult::Abandoned { id });
        let after = svc
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .heartbeat_at;
        assert!(
            after > before,
            "B2 must refresh heartbeat_at while waiting: before={before:?} after={after:?}"
        );
    }

    #[tokio::test]
    async fn wait_timeout_does_not_abandon_after_running() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let outcome = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-RUN".into(),
                path: "/run".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("insert");
        };
        // Claim so the row is Running, then an adopter with a tiny wait budget
        // must still wait for terminal (not Abandoned).
        assert_eq!(
            svc.storage().claim_for_execution(id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let short = svc
            .clone()
            .with_timeouts(Duration::from_millis(80), Duration::from_millis(20));
        let wait = tokio::time::timeout(Duration::from_millis(250), short.wait_and_claim(id)).await;
        // Still Running → wait_and_claim must not return Abandoned within the
        // Queued wait budget; the outer timeout proves it kept polling.
        assert!(
            wait.is_err(),
            "adopter behind Running must not hit wait_timeout abandon"
        );
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Running);
    }

    #[tokio::test]
    async fn b2_follows_superseded_by_on_conflict_requeue() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let first = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-OLD".into(),
                path: "/requeue".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({"cl_link": "CL-OLD"}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: old_id } = first else {
            panic!("insert old");
        };
        // Force-claim the old row so a successor can be claimed as head later.
        assert_eq!(
            svc.storage().claim_for_execution(old_id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let successor = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-NEW".into(),
                path: "/requeue-new".into(),
                old_id: "0".repeat(40),
                new_id: "2".repeat(40),
                requester: None,
                payload: json!({"cl_link": "CL-NEW"}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: new_id } = successor else {
            panic!("insert new");
        };
        svc.storage()
            .mark_cancelled_with_successor_for_test(old_id, Some(new_id))
            .await
            .unwrap();

        let wait = svc.wait_and_claim(old_id).await.unwrap();
        assert_eq!(wait, QueueWaitResult::Ready { id: new_id });
        let row = svc.storage().get_by_id(new_id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Running);
    }

    #[tokio::test]
    async fn b2_resets_wait_budget_when_following_successor() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let short = svc
            .clone()
            .with_timeouts(Duration::from_millis(120), Duration::from_millis(20));
        let first = short
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-LONG".into(),
                path: "/long".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: old_id } = first else {
            panic!("old");
        };
        assert_eq!(
            short.storage().claim_for_execution(old_id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let successor = short
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-SUCC".into(),
                path: "/succ".into(),
                old_id: "0".repeat(40),
                new_id: "2".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: new_id } = successor else {
            panic!("new");
        };

        // Adopter waits behind Running past the Queued budget; then the
        // predecessor is conflict-requeued to a Queued successor.
        let wait_task = {
            let short = short.clone();
            tokio::spawn(async move { short.wait_and_claim(old_id).await })
        };
        tokio::time::sleep(Duration::from_millis(180)).await;
        short
            .storage()
            .mark_cancelled_with_successor_for_test(old_id, Some(new_id))
            .await
            .unwrap();
        let wait = wait_task.await.unwrap().unwrap();
        assert_eq!(
            wait,
            QueueWaitResult::Ready { id: new_id },
            "successor must get a fresh wait budget after conflict requeue"
        );
    }

    async fn seed_root(svc: &PushQueueService, commit: &str, tree: &str) {
        use crate::callisto::mega_refs;
        let model = mega_refs::Model::new(
            "/",
            MEGA_BRANCH_NAME.to_owned(),
            commit.to_owned(),
            tree.to_owned(),
            false,
        );
        svc.mono_storage().save_refs(model, None).await.unwrap();
    }

    async fn enqueue_and_claim(svc: &PushQueueService, op: &str) -> i64 {
        let outcome = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: op.into(),
                path: format!("/{op}"),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("insert {op}");
        };
        assert_eq!(
            svc.storage().claim_for_execution(id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        id
    }

    #[tokio::test]
    async fn b3_net_zero_success_writes_root_cas_once() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        seed_root(&svc, &commit, &tree).await;
        let id = enqueue_and_claim(&svc, "CL-B3-NZ").await;

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ExecuteOutcome::Done {
                id,
                landed_commit_id: commit.clone(),
                root_cas_writes: 1,
            }
        );
        let root = svc.mono_storage().get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(root.ref_commit_hash, commit);
        assert_eq!(root.ref_tree_hash, tree);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Done);
        assert!(row.pending_action.is_none());
    }

    #[tokio::test]
    async fn b3_fencing_claim_lost_leaves_root_unchanged() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        seed_root(&svc, &commit, &tree).await;
        let id = enqueue_and_claim(&svc, "CL-B3-FENCE").await;

        // Concurrent reaper-style terminalize inside the pre-lock delay window.
        let exec = {
            let svc = svc.clone();
            tokio::spawn(async move {
                svc.execute_b3(ExecuteRequest {
                    id,
                    pre_lock_delay: Duration::from_millis(80),
                    ..Default::default()
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        svc.storage().mark_failed_for_test(id).await.unwrap();

        let outcome = exec.await.unwrap().unwrap();
        assert_eq!(outcome, ExecuteOutcome::ClaimLost { id });
        let root = svc.mono_storage().get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(root.ref_commit_hash, commit);
        assert_eq!(root.ref_tree_hash, tree);
    }

    #[tokio::test]
    async fn b3_cas_miss_fail_closes_via_savepoint() {
        use sea_orm::TransactionTrait;

        let (_t, svc) = service(PushPolicy::Trunk).await;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        seed_root(&svc, &commit, &tree).await;
        let id = enqueue_and_claim(&svc, "CL-B3-CASMISS").await;

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                force_cas_miss: true,
                kind_root_write: KindRootWrite::Advance,
                advance_commit: Some("e".repeat(40)),
                advance_tree: Some("f".repeat(40)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(outcome, ExecuteOutcome::BypassDetected { id });
        let root = svc.mono_storage().get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(
            root.ref_commit_hash, commit,
            "SAVEPOINT must undo CAS miss side effects"
        );
        assert_eq!(root.ref_tree_hash, tree);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(
            row.failure_type,
            Some(PushQueueFailureEnum::QueueBypassDetected)
        );
        let txn = svc.mono_storage().get_connection().begin().await.unwrap();
        assert!(
            PushQueueStorage::is_hard_stopped_in_txn(&txn)
                .await
                .unwrap()
        );
        txn.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn b3_baseline_mismatch_fail_closes_hard_stop() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        seed_root(&svc, &commit, &tree).await;
        let id = enqueue_and_claim(&svc, "CL-B3-BYPASS").await;

        // Queue-external root writer in the B2.5→B3 window.
        let conn = svc.mono_storage().get_connection();
        use sea_orm::{ConnectionTrait, Statement, TransactionTrait, Value};
        conn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE mega_refs SET ref_commit_hash = $1, ref_tree_hash = $2 WHERE path = '/' AND ref_name = $3",
            [
                Value::from("c".repeat(40)),
                Value::from("d".repeat(40)),
                Value::from(MEGA_BRANCH_NAME.to_owned()),
            ],
        ))
        .await
        .unwrap();

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(outcome, ExecuteOutcome::BypassDetected { id });
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(
            row.failure_type,
            Some(PushQueueFailureEnum::QueueBypassDetected)
        );
        assert!(row.pending_action.is_none());
        let txn = conn.begin().await.unwrap();
        assert!(
            PushQueueStorage::is_hard_stopped_in_txn(&txn)
                .await
                .unwrap()
        );
        txn.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn b3_hard_stop_resets_running_to_queued() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-B3-HS").await;
        svc.storage()
            .set_control_flags(None, Some(true), None)
            .await
            .unwrap();

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(outcome, ExecuteOutcome::HardStopped { id });
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Queued);
        assert!(row.pending_action.is_none());
    }

    #[tokio::test]
    async fn b4_conflict_requeue_links_successor() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-B4-CF").await;

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                force_conflict: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let ExecuteOutcome::Requeued {
            id: old,
            successor_id,
        } = outcome
        else {
            panic!("expected Requeued, got {outcome:?}");
        };
        assert_eq!(old, id);
        let old_row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(old_row.status, PushQueueStatusEnum::Cancelled);
        assert_eq!(old_row.failure_type, Some(PushQueueFailureEnum::Conflict));
        assert_eq!(old_row.superseded_by, Some(successor_id));
        assert!(old_row.pending_action.is_none());
        let succ = svc
            .storage()
            .get_by_id(successor_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(succ.status, PushQueueStatusEnum::Queued);
        assert_eq!(succ.operation_id, old_row.operation_id);
    }

    #[tokio::test]
    async fn b4_non_conflict_failure_clears_pending_action() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-B4-FAIL").await;

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                force_failure: Some("injected boom".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ExecuteOutcome::Failed {
                id,
                failure: "SystemError".into(),
                message: "injected boom".into(),
            }
        );
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(row.failure_type, Some(PushQueueFailureEnum::SystemError));
        assert!(row.pending_action.is_none());
    }

    #[tokio::test]
    async fn b3_advance_performs_exactly_one_root_cas() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-B3-ADV").await;
        let new_c = "e".repeat(40);
        let new_t = "f".repeat(40);

        let outcome = svc
            .execute_b3(ExecuteRequest {
                id,
                kind_root_write: KindRootWrite::Advance,
                advance_commit: Some(new_c.clone()),
                advance_tree: Some(new_t.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ExecuteOutcome::Done {
                id,
                landed_commit_id: new_c.clone(),
                root_cas_writes: 1,
            }
        );
        let root = svc.mono_storage().get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(root.ref_commit_hash, new_c);
        assert_eq!(root.ref_tree_hash, new_t);
    }
}
