use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sea_orm::{DatabaseTransaction, TransactionTrait};
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
    config::{DEFAULT_MAX_PUSH_COMMITS, PushPolicy},
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        blob_path_index::BlobPathIndexMode,
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

/// Deterministic attach command fingerprint input (Branch commands only).
///
/// Fingerprint covers wire intent (ref + tip ids), not derived `default_branch`
/// (smart.rs sets that from repo state and would break identical-retry adopt).
pub fn normalize_attach_commands(cmds: &[(String, String, String, String)]) -> String {
    // tuples: (ref_name, command_type, old_id, new_id)
    let mut rows: Vec<_> = cmds.to_vec();
    rows.sort_by(|a, b| (&a.0, &a.1, &a.2, &a.3).cmp(&(&b.0, &b.1, &b.2, &b.3)));
    rows.into_iter()
        .map(|(ref_name, ctype, old_id, new_id)| format!("{ref_name}:{ctype}:{old_id}:{new_id}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serializable attach payload (trunk-push 1.4 / 1.9).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AttachPayload {
    pub repo_id: i64,
    pub repo_path: String,
    pub commands: Vec<AttachCommand>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AttachCommand {
    pub ref_name: String,
    pub old_id: String,
    pub new_id: String,
    pub command_type: String,
    pub ref_type: String,
    #[serde(default)]
    pub default_branch: bool,
}

impl AttachPayload {
    pub fn normalize_fingerprint_input(&self) -> String {
        let rows: Vec<_> = self
            .commands
            .iter()
            .filter(|c| c.ref_type == "branch")
            .map(|c| {
                (
                    c.ref_name.clone(),
                    c.command_type.clone(),
                    c.old_id.clone(),
                    c.new_id.clone(),
                )
            })
            .collect();
        normalize_attach_commands(&rows)
    }
}

/// Context required to execute a claimed `kind=attach` round under B3.
pub struct AttachExecContext {
    pub storage: crate::jupiter::storage::Storage,
    pub git_object_cache: std::sync::Arc<crate::ceres::api_service::cache::GitObjectCache>,
}

/// Serializable merge payload (trunk-push 1.4 / 1.9).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MergePayload {
    pub cl_link: String,
    pub authz_principal: String,
    pub execution_actor: String,
    /// When true, B3 re-runs UN-17 (`decide_queue_execution`) like the legacy
    /// merge-queue processor. Direct `/merge` leaves this false.
    #[serde(default)]
    pub apply_queue_execution_decision: bool,
    /// Recorded requester for UN-17 (`None` = anonymous / missing).
    #[serde(default)]
    pub requester: Option<String>,
}

/// Serializable push descriptor (trunk-push 1.3). `n` is the client-baseline
/// first-parent distance `new_id → old_id` and is never recomputed from
/// object-store knownness.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PushPayload {
    pub commits: Vec<String>,
    pub fork_base: Option<String>,
    pub n: u32,
}

impl PushPayload {
    pub fn from_chain(
        old_id: &str,
        new_id: &str,
        chain: &crate::ceres::pack::push_chain::PushChain,
    ) -> Self {
        let n = if old_id == new_id {
            0
        } else {
            u32::try_from(chain.ordered_commits.len()).unwrap_or(u32::MAX)
        };
        Self {
            commits: chain
                .ordered_commits
                .iter()
                .map(|c| c.id.to_string())
                .collect(),
            fork_base: Some(chain.base.clone()),
            n,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        serde_json::to_value(self).unwrap_or(JsonValue::Null)
    }
}

/// Context required to execute a claimed `kind=push` round under B3.
pub struct PushExecContext {
    pub storage: crate::jupiter::storage::Storage,
    pub git_object_cache: std::sync::Arc<crate::ceres::api_service::cache::GitObjectCache>,
}

/// Context required to execute a claimed `kind=merge` round under B3.
pub struct MergeExecContext {
    pub storage: crate::jupiter::storage::Storage,
    pub git_object_cache: std::sync::Arc<crate::ceres::api_service::cache::GitObjectCache>,
    /// Test-only: roll back after refs/commits land and before CL status write.
    pub abort_before_cl_status: bool,
    /// Test-only: hold the B3 lock after apply so a concurrent rebase can race.
    pub pause_after_apply: Duration,
    /// Test-only: sync point when `pause_after_apply` begins (race window open).
    pub pause_after_apply_barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
}

/// A `refs/heads/main` row whose `ref_tree_hash` does not match `resolve(root, P)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleMainRef {
    pub path: String,
    pub last_commit_hash: String,
    pub stored_tree_hash: String,
    pub resolved_tree_hash: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelQueuedOutcome {
    Cancelled,
    NotFound,
    NotQueued { status: PushQueueStatusEnum },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct QueueSnapshotItem {
    pub id: i64,
    pub path: String,
    pub kind: String,
    pub requester: Option<String>,
    pub status: String,
    pub wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct QueueControlSnapshot {
    pub paused: bool,
    pub hard_stopped: bool,
    pub depth: u64,
    pub head: Option<QueueSnapshotItem>,
    pub running: Option<QueueSnapshotItem>,
    pub items: Vec<QueueSnapshotItem>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct QueueMetricsSnapshot {
    pub depth: u64,
    pub wait_ms_p50: Option<f64>,
    pub wait_ms_p99: Option<f64>,
    pub round_ms_p50: Option<f64>,
    pub round_ms_p99: Option<f64>,
    pub failure_rate: f64,
    pub cas_assert_failures: u64,
    pub claim_lost: u64,
    pub lock_timeout_alarms: u64,
    pub tree_hash_assert_failures: u64,
    pub inspect_tombstoned: u64,
    pub reconcile_tombstoned: u64,
}

fn percentile(xs: &[f64], p: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(idx).copied()
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

/// Process-local counters for TP-06 observability (no extra metrics crate).
#[derive(Clone, Default)]
pub struct PushQueueMetrics {
    pub cas_assert_failures: Arc<AtomicU64>,
    pub claim_lost: Arc<AtomicU64>,
    pub lock_timeout_alarms: Arc<AtomicU64>,
    pub tree_hash_assert_failures: Arc<AtomicU64>,
    pub inspect_tombstoned: Arc<AtomicU64>,
    pub reconcile_tombstoned: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct PushQueueService {
    push_queue_storage: PushQueueStorage,
    mono_storage: MonoStorage,
    push_policy: PushPolicy,
    max_push_commits: usize,
    wait_timeout: Duration,
    poll_interval: Duration,
    metrics: PushQueueMetrics,
}

impl PushQueueService {
    pub fn new(base: BaseStorage, push_policy: PushPolicy) -> Self {
        Self {
            push_queue_storage: PushQueueStorage::new(base.clone()),
            mono_storage: MonoStorage { base },
            push_policy,
            max_push_commits: DEFAULT_MAX_PUSH_COMMITS,
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            metrics: PushQueueMetrics::default(),
        }
    }

    pub fn with_timeouts(mut self, wait_timeout: Duration, poll_interval: Duration) -> Self {
        self.wait_timeout = wait_timeout;
        self.poll_interval = poll_interval;
        self
    }

    pub fn with_max_push_commits(mut self, max_push_commits: usize) -> Self {
        self.max_push_commits = max_push_commits;
        self
    }

    pub fn push_policy(&self) -> PushPolicy {
        self.push_policy.clone()
    }

    pub fn storage(&self) -> &PushQueueStorage {
        &self.push_queue_storage
    }

    pub(crate) fn mono_storage_for_reaper(
        &self,
    ) -> &crate::jupiter::storage::mono_storage::MonoStorage {
        &self.mono_storage
    }

    pub fn reaper(&self) -> crate::jupiter::service::push_queue_reaper::PushQueueReaper {
        crate::jupiter::service::push_queue_reaper::PushQueueReaper::from_service(self.clone())
    }

    pub fn audit(&self) -> crate::jupiter::service::mono_write_audit::MonoWriteAudit {
        crate::jupiter::service::mono_write_audit::MonoWriteAudit::from_service(self.clone())
    }

    pub fn blob_path_compensator(
        &self,
    ) -> crate::jupiter::storage::blob_path_index::BlobPathCompensator {
        crate::jupiter::storage::blob_path_index::BlobPathCompensator::new(
            self.mono_storage.clone(),
        )
    }

    /// C-segment (ADR-TP-11): after B3 commit, index `main@path` off the lock.
    async fn run_c_segment_index(&self, id: i64, path: &str) {
        match self
            .mono_storage
            .index_blob_paths_c_segment(path, BlobPathIndexMode::Queue { push_id: id })
            .await
        {
            Ok(stats) if stats.skipped => {
                tracing::info!(
                    id,
                    path,
                    "C-segment skipped; later same-path Done already indexed"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    id,
                    path,
                    error = %e,
                    "C-segment blob_path index failed; compensator will converge"
                );
            }
        }
    }

    pub fn metrics(&self) -> &PushQueueMetrics {
        &self.metrics
    }

    fn outcome_claim_lost(&self, id: i64) -> ExecuteOutcome {
        self.metrics.claim_lost.fetch_add(1, Ordering::Relaxed);
        tracing::info!(event = "push_queue_claim_lost", id, "B3 fencing ClaimLost");
        ExecuteOutcome::ClaimLost { id }
    }

    fn note_cas_assert_failure(&self) {
        self.metrics
            .cas_assert_failures
            .fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            event = "push_queue_cas_assert_failure",
            "root CAS affected 0 rows (should be 0 in production)"
        );
    }

    pub async fn pause(&self) -> Result<(), MegaError> {
        self.push_queue_storage
            .set_control_flags(Some(true), None, None)
            .await
    }

    pub async fn resume(&self) -> Result<(), MegaError> {
        self.push_queue_storage
            .set_control_flags(Some(false), None, None)
            .await
    }

    /// Clear `hard_stopped` only. Does not touch `paused`.
    pub async fn clear_hard_stop(&self, actor: &str) -> Result<(), MegaError> {
        self.push_queue_storage
            .set_control_flags(None, Some(false), None)
            .await?;
        tracing::warn!(
            event = "push_queue_clear_hard_stop",
            actor = %actor,
            "operator cleared queue_control.hard_stopped"
        );
        Ok(())
    }

    pub async fn cancel_queued(&self, id: i64) -> Result<CancelQueuedOutcome, MegaError> {
        let Some(row) = self.push_queue_storage.get_by_id(id).await? else {
            return Ok(CancelQueuedOutcome::NotFound);
        };
        if row.status != PushQueueStatusEnum::Queued {
            return Ok(CancelQueuedOutcome::NotQueued { status: row.status });
        }
        if self.push_queue_storage.cancel_if_queued(id).await? {
            Ok(CancelQueuedOutcome::Cancelled)
        } else {
            let current = self.push_queue_storage.get_by_id(id).await?;
            Ok(CancelQueuedOutcome::NotQueued {
                status: current
                    .map(|r| r.status)
                    .unwrap_or(PushQueueStatusEnum::Cancelled),
            })
        }
    }

    pub async fn control_snapshot(&self) -> Result<QueueControlSnapshot, MegaError> {
        let ctrl = self.push_queue_storage.get_control().await?;
        let active = self.push_queue_storage.list_active().await?;
        let now = chrono::Utc::now().fixed_offset();
        let items: Vec<QueueSnapshotItem> = active
            .iter()
            .map(|row| QueueSnapshotItem {
                id: row.id,
                path: row.path.clone(),
                kind: format!("{:?}", row.kind),
                requester: row.requester.clone(),
                status: format!("{:?}", row.status),
                wait_ms: (now - row.enqueued_at).num_milliseconds().max(0) as u64,
            })
            .collect();
        let head = items.first().cloned();
        let running = items.iter().find(|i| i.status == "Running").cloned();
        Ok(QueueControlSnapshot {
            paused: ctrl.paused,
            hard_stopped: ctrl.hard_stopped,
            depth: items.len() as u64,
            head,
            running,
            items,
        })
    }

    pub async fn metrics_snapshot(&self) -> Result<QueueMetricsSnapshot, MegaError> {
        let queued = self
            .push_queue_storage
            .count_by_status(PushQueueStatusEnum::Queued)
            .await?;
        let running = self
            .push_queue_storage
            .count_by_status(PushQueueStatusEnum::Running)
            .await?;
        let done = self
            .push_queue_storage
            .count_by_status(PushQueueStatusEnum::Done)
            .await?;
        let failed = self
            .push_queue_storage
            .count_by_status(PushQueueStatusEnum::Failed)
            .await?;
        let finished = self.push_queue_storage.list_recent_finished(200).await?;
        let mut waits: Vec<f64> = finished
            .iter()
            .filter_map(|r| {
                let started = r.started_at?;
                Some((started - r.enqueued_at).num_milliseconds().max(0) as f64)
            })
            .collect();
        waits.extend(
            self.push_queue_storage
                .list_active()
                .await?
                .into_iter()
                .filter(|r| r.status == PushQueueStatusEnum::Queued)
                .map(|r| {
                    (chrono::Utc::now().fixed_offset() - r.enqueued_at)
                        .num_milliseconds()
                        .max(0) as f64
                }),
        );
        let rounds: Vec<f64> = finished
            .iter()
            .filter_map(|r| {
                let started = r.started_at?;
                let finished_at = r.finished_at?;
                Some((finished_at - started).num_milliseconds().max(0) as f64)
            })
            .collect();
        let terminal = done + failed;
        let failure_rate = if terminal == 0 {
            0.0
        } else {
            failed as f64 / terminal as f64
        };
        Ok(QueueMetricsSnapshot {
            depth: queued + running,
            wait_ms_p50: percentile(&waits, 0.50),
            wait_ms_p99: percentile(&waits, 0.99),
            round_ms_p50: percentile(&rounds, 0.50),
            round_ms_p99: percentile(&rounds, 0.99),
            failure_rate,
            cas_assert_failures: self.metrics.cas_assert_failures.load(Ordering::Relaxed),
            claim_lost: self.metrics.claim_lost.load(Ordering::Relaxed),
            lock_timeout_alarms: self.metrics.lock_timeout_alarms.load(Ordering::Relaxed),
            tree_hash_assert_failures: self
                .metrics
                .tree_hash_assert_failures
                .load(Ordering::Relaxed),
            inspect_tombstoned: self.metrics.inspect_tombstoned.load(Ordering::Relaxed),
            reconcile_tombstoned: self.metrics.reconcile_tombstoned.load(Ordering::Relaxed),
        })
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

        if req.is_delete || req.new_id == ZERO_ID {
            return Err(MegaError::Other(
                "trunk push rejects delete commands; remove content via a parent-path commit"
                    .into(),
            ));
        }
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
        let chain_limit = match self.push_policy {
            PushPolicy::Trunk => self.max_push_commits,
            PushPolicy::Review => crate::ceres::merge_checker::MAX_CL_CHAIN_COMMITS,
        };
        if let Some(n) = req.payload.get("n").and_then(JsonValue::as_u64)
            && n > chain_limit as u64
        {
            return Err(MegaError::Other(format!(
                "push introduces more than {chain_limit} commits in one chain; split the changes into smaller pushes or squash and re-push"
            )));
        }
        Ok(())
    }

    /// B0: a ZERO_ID create on a path that still has a tombstone would fork
    /// history (I1). Refuse with the two-step revive hint.
    pub async fn b0_reject_create_on_tombstone(
        &self,
        req: &EnqueueRequest,
    ) -> Result<(), MegaError> {
        if req.kind != PushQueueKindEnum::Push || req.old_id != ZERO_ID {
            return Ok(());
        }
        if self
            .mono_storage
            .get_tombstone(&req.path, MEGA_BRANCH_NAME)
            .await?
            .is_none()
        {
            return Ok(());
        }
        Err(MegaError::Other(format!(
            "tombstone exists for {}: recreate the parent directory, advertise to continue from the tombstone tip, fetch to align, then push",
            req.path
        )))
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
        self.b0_reject_create_on_tombstone(&req).await?;
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

    /// B3 SAVEPOINT repair: kind writes already rolled back; persist a tombstone
    /// for `path`'s main ref, delete that row, and mark Failed. No `pending_action`.
    pub async fn b3_tombstone_repair_after_savepoint(
        &self,
        txn: sea_orm::DatabaseTransaction,
        id: i64,
        path: &str,
        message: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        self.mono_storage
            .tombstone_and_delete_main_ref_in_txn(path, &txn)
            .await?;
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, "Conflict", message).await?;
        if !updated {
            tracing::error!(
                id,
                "B3 tombstone repair: Failed update hit 0 rows (reaper raced?)"
            );
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: "Conflict".into(),
            message: message.to_owned(),
        })
    }

    /// Shared TP-11 predicate: `main@P.ref_tree_hash == resolve(root, P)`.
    /// Only `refs/heads/main` is considered. Missing rows are not stale
    /// (push create mid-state / merge "Main ref not found"). `path == "/"`
    /// is skipped (`main@/` is the resolve source).
    pub async fn assert_main_path_tree_hash_in_txn(
        &self,
        txn: &DatabaseTransaction,
        path: &str,
        root_tree_hash: &str,
    ) -> Result<Option<StaleMainRef>, MegaError> {
        if path.is_empty() || path == "/" {
            return Ok(None);
        }
        let Some(pref) = self.mono_storage.get_main_ref_in_txn(path, txn).await? else {
            return Ok(None);
        };
        let resolved = self
            .mono_storage
            .resolve_path_tree_hash_in_txn(root_tree_hash, path, txn)
            .await?;
        if resolved.as_deref() == Some(pref.ref_tree_hash.as_str()) {
            return Ok(None);
        }
        Ok(Some(StaleMainRef {
            path: pref.path,
            last_commit_hash: pref.ref_commit_hash,
            stored_tree_hash: pref.ref_tree_hash,
            resolved_tree_hash: resolved,
        }))
    }

    async fn b3_refuse_stale_main_tree(
        &self,
        txn: DatabaseTransaction,
        id: i64,
        path: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        self.metrics
            .tree_hash_assert_failures
            .fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            event = "push_queue_tree_hash_assert",
            id,
            "stale materialized main ref; tombstone repair"
        );
        PushQueueStorage::savepoint(&txn, "b3_kind").await?;
        PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
        self.b3_tombstone_repair_after_savepoint(
            txn,
            id,
            path,
            "stale materialized main ref; advertise then fetch",
        )
        .await
    }

    /// Reaper I3: when `expected_*` still matches the live root, tombstone a
    /// stale `main@path` and terminalize a `Running` row (crash after SAVEPOINT
    /// writes that never committed).
    pub async fn reaper_i3_tombstone_repair(&self, id: i64) -> Result<bool, MegaError> {
        let Some(row) = self.push_queue_storage.get_by_id(id).await? else {
            return Ok(false);
        };
        if row.status != PushQueueStatusEnum::Running {
            return Ok(false);
        }
        let conn = self.mono_storage.get_connection();
        let txn = conn.begin().await?;
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
            txn.rollback().await?;
            return Ok(false);
        }
        self.mono_storage
            .tombstone_and_delete_main_ref_in_txn(&row.path, &txn)
            .await?;
        PushQueueStorage::mark_failed_if_running_in_txn(
            &txn,
            id,
            "SystemError",
            "I3 tombstone repair after Running crash",
        )
        .await?;
        txn.commit().await?;
        Ok(true)
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

    /// B3 execution skeleton + B4 failure/requeue.
    ///
    /// `attach_ctx` is required when the claimed row is `kind=attach` (TP-08);
    /// `merge_ctx` is required when the claimed row is `kind=merge` and the
    /// caller wants the real merge writer (TP-07). `push_ctx` is required when
    /// the claimed row is `kind=push` and the caller wants the real push writer
    /// (TP-12). Tests that enqueue `kind=merge` without a context keep the B3
    /// skeleton stub.
    pub async fn execute_b3(
        &self,
        req: ExecuteRequest,
        attach_ctx: Option<&AttachExecContext>,
        merge_ctx: Option<&MergeExecContext>,
        push_ctx: Option<&PushExecContext>,
    ) -> Result<ExecuteOutcome, MegaError> {
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
            return Ok(self.outcome_claim_lost(req.id));
        }

        if PushQueueStorage::is_hard_stopped_in_txn(&txn).await? {
            txn.rollback().await?;
            if !self
                .push_queue_storage
                .reset_running_to_queued(req.id)
                .await?
            {
                return Ok(self.outcome_claim_lost(req.id));
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

        // Kind dispatch (1.10 deliverable 9).
        match row.kind {
            PushQueueKindEnum::Attach => {
                let Some(ctx) = attach_ctx else {
                    let msg = "execute_b3 attach requires AttachExecContext";
                    tracing::error!(id = req.id, "{msg}");
                    // Release the B3 txn first; terminalize on a fresh connection
                    // (same pattern as b3_execute_attach Err recovery).
                    txn.rollback().await?;
                    return self.terminalize_attach_failure(req.id, msg).await;
                };
                return self
                    .b3_execute_attach(txn, &row, ctx, cur_commit, cur_tree, root.as_ref())
                    .await;
            }
            PushQueueKindEnum::Push => {
                if let Some(root) = root.as_ref()
                    && self
                        .assert_main_path_tree_hash_in_txn(&txn, &row.path, &root.ref_tree_hash)
                        .await?
                        .is_some()
                {
                    return self.b3_refuse_stale_main_tree(txn, req.id, &row.path).await;
                }
                if let Some(ctx) = push_ctx {
                    return self
                        .b3_execute_push(txn, &row, ctx, cur_commit, cur_tree, root.as_ref())
                        .await;
                }
                tracing::trace!(policy = ?self.push_policy, id = req.id, "B3 push stub");
            }
            PushQueueKindEnum::Merge => {
                if let Some(ctx) = merge_ctx {
                    return self
                        .b3_execute_merge(txn, &row, ctx, cur_commit, cur_tree, root.as_ref())
                        .await;
                }
                tracing::trace!(policy = ?self.push_policy, id = req.id, "B3 merge stub");
            }
        }

        // Push/merge stubs: exactly one root CAS write (net-zero or advance).
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
            self.note_cas_assert_failure();
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
            return Ok(self.outcome_claim_lost(req.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        self.run_c_segment_index(req.id, &row.path).await;
        Ok(ExecuteOutcome::Done {
            id: req.id,
            landed_commit_id: new_commit,
            root_cas_writes,
        })
    }

    /// TP-08: attach kind under held `MONO_WRITE_LOCK` — sole root write is
    /// `attach_to_monorepo_parent_in_txn` (no generic CAS stub, no retry loop).
    ///
    /// Unexpected `Err` paths terminalize the claimed row as `AttachFailure` on
    /// a fresh txn so a dropped B3 txn cannot leave the queue head `Running`.
    async fn b3_execute_attach(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &AttachExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        let id = row.id;
        match self
            .b3_execute_attach_inner(txn, row, ctx, cur_commit, cur_tree, root)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                tracing::error!(
                    id,
                    error = %e,
                    "B3 attach aborted with Err; terminalizing AttachFailure"
                );
                self.terminalize_attach_failure(id, &e.to_string()).await
            }
        }
    }

    async fn terminalize_attach_failure(
        &self,
        id: i64,
        message: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        use sea_orm::TransactionTrait;

        let conn = self.push_queue_storage.get_connection();
        let txn = conn.begin().await?;
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, "AttachFailure", message)
                .await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id,
                "B3 attach Err recovery: Failed update hit 0 rows (already terminal?)"
            );
            return Ok(self.outcome_claim_lost(id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: "AttachFailure".into(),
            message: message.to_owned(),
        })
    }

    async fn b3_execute_attach_inner(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &AttachExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        use std::path::PathBuf;

        use git_internal::{
            hash::{ObjectHash, get_hash_kind},
            internal::object::{commit::Commit, tree::Tree},
        };

        use crate::{
            callisto::sea_orm_active_enums::RefTypeEnum,
            ceres::{
                api_service::{mono_api_service::MonoApiService, tree_ops},
                protocol::import_refs::{CommandType, RefCommand},
            },
            common::utils::canonicalize_mono_ref_path,
            contract::policy::notify::{
                after_b3_commit_authz, authz_blob_id, insert_b3_authz_outbox_if_builds,
            },
            jupiter::utils::converter::{FromGitModel, FromMegaModel},
        };

        let payload: AttachPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| MegaError::Other(format!("attach payload deserialize failed: {e}")))?;
        let repo_path = canonicalize_mono_ref_path(&payload.repo_path)?;
        // AC allows P=/ through the materialization precheck (main@/ does not
        // participate). ImportRepo attach still mounts a named leaf under the
        // root tree via search_and_create_tree, which requires a non-root path.
        if repo_path == "/" {
            return Err(MegaError::Other(
                "attach to monorepo path '/' is not supported (no leaf name for tree mount)".into(),
            ));
        }

        let Some(root) = root else {
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                row.id,
                "AttachFailure",
                "attach requires an existing root ref",
            )
            .await?;
            if !updated {
                tracing::error!(id = row.id, "B3 attach missing-root failure hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: row.id,
                failure: "AttachFailure".into(),
                message: "missing root".into(),
            });
        };

        let expected_commit = cur_commit
            .ok_or_else(|| MegaError::Other("attach root commit missing".into()))?
            .to_owned();
        let expected_tree = cur_tree
            .ok_or_else(|| MegaError::Other("attach root tree missing".into()))?
            .to_owned();

        // Lock-held redo of materialization precheck (intervening writes).
        if let Err(e) = self
            .mono_storage
            .attach_materialization_precheck_in_txn(&repo_path, &txn)
            .await
        {
            let msg = e.to_string();
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                row.id,
                "AttachFailure",
                &msg,
            )
            .await?;
            if !updated {
                tracing::error!(id = row.id, "B3 attach precheck failure hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: row.id,
                failure: "AttachFailure".into(),
                message: msg,
            });
        }

        let mono_api = MonoApiService {
            storage: ctx.storage.clone(),
            git_object_cache: ctx.git_object_cache.clone(),
        };
        let path = PathBuf::from(&repo_path);

        PushQueueStorage::savepoint(&txn, "b3_kind").await?;

        let (save_trees, gitkeep_blob) =
            match tree_ops::search_and_create_tree(&mono_api, &path).await {
                Ok(v) => v,
                Err(e) => {
                    PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                    let msg = e.to_string();
                    let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                        &txn,
                        row.id,
                        "AttachFailure",
                        &msg,
                    )
                    .await?;
                    if !updated {
                        tracing::error!(id = row.id, "B3 attach tree build failure hit 0 rows");
                    }
                    PushQueueStorage::notify_mono_write_queue(&txn).await?;
                    txn.commit().await?;
                    return Ok(ExecuteOutcome::Failed {
                        id: row.id,
                        failure: "AttachFailure".into(),
                        message: msg,
                    });
                }
            };

        let tip_commit_id = payload
            .commands
            .iter()
            .find(|c| c.ref_type == "branch" && c.new_id != ZERO_ID)
            .map(|c| c.new_id.clone())
            .ok_or_else(|| MegaError::Other("attach payload has no branch tip".into()))?;

        let latest_commit: Commit = Commit::from_git_model(
            ctx.storage
                .git_db_storage()
                .get_commit_by_hash(payload.repo_id, &tip_commit_id)
                .await?
                .ok_or_else(|| MegaError::Other(format!("commit {tip_commit_id} not found")))?,
        );
        let commit_msg = latest_commit.format_message();

        let new_commit = Commit::from_tree_id(
            save_trees
                .back()
                .ok_or_else(|| MegaError::Other("no tree generated".into()))?
                .id,
            vec![
                ObjectHash::from_hex_for_kind(get_hash_kind(), &expected_commit)
                    .map_err(|e| MegaError::Other(format!("invalid expected commit hash: {e}")))?,
            ],
            &format!("\n{commit_msg}"),
        );
        let new_root_tree_hash = new_commit.tree_id.to_string();
        let landed_commit_id = new_commit.id.to_string();

        // Object-store write is outside the DB SAVEPOINT (same as pre-queue attach).
        if let Err(e) = ctx
            .storage
            .mono_service
            .save_blobs(&landed_commit_id, vec![gitkeep_blob])
            .await
        {
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            let msg = e.to_string();
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                row.id,
                "AttachFailure",
                &msg,
            )
            .await?;
            if !updated {
                tracing::error!(id = row.id, "B3 attach gitkeep failure hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: row.id,
                failure: "AttachFailure".into(),
                message: msg,
            });
        }

        let git_db = ctx.storage.git_db_storage();
        for cmd in &payload.commands {
            if cmd.ref_type != "branch" {
                continue;
            }
            let command_type = match cmd.command_type.as_str() {
                "Create" => CommandType::Create,
                "Delete" => CommandType::Delete,
                "Update" => CommandType::Update,
                other => {
                    return Err(MegaError::Other(format!(
                        "unknown attach command_type '{other}'"
                    )));
                }
            };
            let ref_cmd = RefCommand {
                ref_name: cmd.ref_name.clone(),
                old_id: cmd.old_id.clone(),
                new_id: cmd.new_id.clone(),
                status: "ok".into(),
                error_msg: String::new(),
                command_type: command_type.clone(),
                ref_type: RefTypeEnum::Branch,
                default_branch: cmd.default_branch,
            };
            match command_type {
                CommandType::Create => {
                    git_db
                        .save_ref_in_txn(payload.repo_id, ref_cmd.into(), &txn)
                        .await?;
                    if cmd.default_branch {
                        git_db
                            .set_default_branch_in_txn(payload.repo_id, &cmd.ref_name, &txn)
                            .await?;
                    }
                }
                CommandType::Delete => {
                    git_db
                        .remove_ref_in_txn(payload.repo_id, &cmd.ref_name, &txn)
                        .await?;
                }
                CommandType::Update => {
                    git_db
                        .update_ref_in_txn(payload.repo_id, &cmd.ref_name, &cmd.new_id, &txn)
                        .await?;
                    if cmd.default_branch {
                        git_db
                            .set_default_branch_in_txn(payload.repo_id, &cmd.ref_name, &txn)
                            .await?;
                    }
                }
            }
        }

        // Ensure a sole default survives the batch (e.g. Delete of the old
        // default + Create of a replacement without a precomputed flag).
        if !git_db
            .default_branch_exist_in_txn(payload.repo_id, &txn)
            .await?
            && let Some(first) = git_db
                .list_branch_refs_in_txn(payload.repo_id, &txn)
                .await?
                .into_iter()
                .next()
        {
            git_db
                .set_default_branch_in_txn(payload.repo_id, &first.ref_name, &txn)
                .await?;
        }

        let trees: Vec<Tree> = save_trees.into_iter().collect();
        match self
            .mono_storage
            .attach_to_monorepo_parent_in_txn(
                &txn,
                root.id,
                &expected_commit,
                &expected_tree,
                new_commit,
                trees,
            )
            .await
        {
            Ok(()) => {}
            Err(MegaError::StaleMonorepoRootRef) => {
                // Under the queue this is a bypass tripwire, not a retry signal.
                self.note_cas_assert_failure();
                PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
                let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                    &txn,
                    row.id,
                    "QueueBypassDetected",
                    "attach root CAS affected 0 rows",
                )
                .await?;
                if !updated {
                    tracing::error!(
                        id = row.id,
                        "B3 attach CAS fail-closed: Failed update hit 0 rows"
                    );
                }
                PushQueueStorage::notify_mono_write_queue(&txn).await?;
                txn.commit().await?;
                return Ok(ExecuteOutcome::BypassDetected { id: row.id });
            }
            Err(e) => {
                PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                let msg = e.to_string();
                let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                    &txn,
                    row.id,
                    "AttachFailure",
                    &msg,
                )
                .await?;
                if !updated {
                    tracing::error!(id = row.id, "B3 attach failure hit 0 rows");
                }
                PushQueueStorage::notify_mono_write_queue(&txn).await?;
                txn.commit().await?;
                return Ok(ExecuteOutcome::Failed {
                    id: row.id,
                    failure: "AttachFailure".into(),
                    message: msg,
                });
            }
        }

        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, &landed_commit_id).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id = row.id,
                "B3 attach Done update hit 0 rows after fencing"
            );
            return Ok(self.outcome_claim_lost(row.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        insert_b3_authz_outbox_if_builds(&ctx.storage, &txn, row.id).await?;
        txn.commit().await?;
        let blob_ids = async {
            let old_blob_id = self
                .mono_storage
                .get_tree_by_hash(&expected_tree)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            let new_blob_id = self
                .mono_storage
                .get_tree_by_hash(&new_root_tree_hash)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            Ok::<_, MegaError>((old_blob_id, new_blob_id))
        }
        .await;
        match blob_ids {
            Ok((old_blob_id, new_blob_id)) => {
                after_b3_commit_authz(&ctx.storage, old_blob_id.as_deref(), new_blob_id.as_deref())
                    .await;
            }
            Err(e) => {
                ctx.storage.entity_store().mark_dirty();
                tracing::error!(
                    id = row.id,
                    error = %e,
                    "attach authz blob resolve failed after Done; marked entity store dirty"
                );
            }
        }

        self.run_c_segment_index(row.id, &row.path).await;
        Ok(ExecuteOutcome::Done {
            id: row.id,
            landed_commit_id,
            root_cas_writes: 1,
        })
    }

    async fn b3_execute_push(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &PushExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        let id = row.id;
        match self
            .b3_execute_push_inner(txn, row, ctx, cur_commit, cur_tree, root)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                tracing::error!(id, error = %e, "B3 push aborted with Err; terminalizing PushFailure");
                self.terminalize_push_failure(id, &e.to_string()).await
            }
        }
    }

    async fn terminalize_push_failure(
        &self,
        id: i64,
        message: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        let conn = self.push_queue_storage.get_connection();
        let txn = conn.begin().await?;
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, "PushFailure", message)
                .await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id,
                "B3 push Err recovery: Failed update hit 0 rows (already terminal?)"
            );
            return Ok(self.outcome_claim_lost(id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: "PushFailure".into(),
            message: message.to_owned(),
        })
    }

    async fn b3_execute_push_inner(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &PushExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        use std::path::PathBuf;

        use git_internal::{
            errors::GitError,
            hash::{ObjectHash, get_hash_kind},
            internal::object::{commit::Commit, tree::Tree},
        };

        use crate::{
            ceres::{
                api_service::{
                    mono_api_service::{MonoApiService, MonoServiceLogic, PushApplyArgs},
                    tree_ops,
                },
                pack::trunk_provenance::{self, TrunkProvenance},
            },
            jupiter::{
                storage::mono_storage::DescendantCommitStyle, utils::converter::FromMegaModel,
            },
        };

        let payload: PushPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| MegaError::Other(format!("push payload deserialize failed: {e}")))?;

        let n0 = row.old_id == row.new_id;
        if n0 != (payload.n == 0) {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "SystemError",
                    "push descriptor n invariant: n=0 iff old_id == new_id".into(),
                )
                .await;
        }

        let Some(root) = root else {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "PushFailure",
                    "push requires an existing root ref".into(),
                )
                .await;
        };

        let path_row = self
            .mono_storage
            .get_main_ref_in_txn(&row.path, &txn)
            .await?;

        if path_row.is_none() {
            if self
                .mono_storage
                .get_tombstone_in_txn(&row.path, MEGA_BRANCH_NAME, &txn)
                .await?
                .is_some()
            {
                return self
                    .b3_fail_merge(
                        txn,
                        row.id,
                        "Conflict",
                        "tombstone exists; advertise then fetch".into(),
                    )
                    .await;
            }
            if row.old_id != ZERO_ID {
                return self
                    .b3_fail_merge(
                        txn,
                        row.id,
                        "PushFailure",
                        "cannot update missing path; create requires old_id=ZERO_ID".into(),
                    )
                    .await;
            }
            if payload.n == 0 {
                return self
                    .b3_fail_merge(
                        txn,
                        row.id,
                        "PushFailure",
                        "create is unreachable for n=0".into(),
                    )
                    .await;
            }
        } else if row.old_id == ZERO_ID {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "PushFailure",
                    "path materialized while waiting; fetch then re-push".into(),
                )
                .await;
        }

        if payload.n == 0 {
            let Some(pref) = path_row.as_ref() else {
                return self
                    .b3_fail_merge(
                        txn,
                        row.id,
                        "PushFailure",
                        "n=0 requires an existing path tip".into(),
                    )
                    .await;
            };
            if row.new_id != pref.ref_commit_hash {
                return self
                    .b3_fail_merge(
                        txn,
                        row.id,
                        "PushFailure",
                        "non-fast-forward: new_id does not match current tip".into(),
                    )
                    .await;
            }
            PushQueueStorage::savepoint(&txn, "b3_kind").await?;
            let cas_ok = self
                .mono_storage
                .cas_update_root_main_ref_in_txn(
                    &txn,
                    cur_commit,
                    cur_tree,
                    &root.ref_commit_hash,
                    &root.ref_tree_hash,
                )
                .await?;
            if !cas_ok {
                self.note_cas_assert_failure();
                PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
                let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                    &txn,
                    row.id,
                    "QueueBypassDetected",
                    "root CAS affected 0 rows",
                )
                .await?;
                if !updated {
                    tracing::error!(id = row.id, "B3 push N=0 CAS fail-closed: 0 rows");
                }
                PushQueueStorage::notify_mono_write_queue(&txn).await?;
                txn.commit().await?;
                return Ok(ExecuteOutcome::BypassDetected { id: row.id });
            }
            let updated =
                PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, &pref.ref_commit_hash)
                    .await?;
            if !updated {
                txn.rollback().await?;
                return Ok(self.outcome_claim_lost(row.id));
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            self.run_c_segment_index(row.id, &row.path).await;
            return Ok(ExecuteOutcome::Done {
                id: row.id,
                landed_commit_id: pref.ref_commit_hash.clone(),
                root_cas_writes: 1,
            });
        }

        if let Some(pref) = path_row.as_ref()
            && pref.ref_commit_hash != row.old_id
        {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "PushFailure",
                    "non-fast-forward: ref_commit_hash does not match old_id".into(),
                )
                .await;
        }

        let tip_model = self
            .mono_storage
            .get_commit_by_hash(&row.new_id)
            .await?
            .ok_or_else(|| MegaError::Other(format!("push new_id {} not found", row.new_id)))?;
        let tip_commit = Commit::from_mega_model(tip_model);
        let tip_tree_id = tip_commit.tree_id;
        let creating = path_row.is_none();

        let resolved = self
            .mono_storage
            .resolve_path_tree_hash_in_txn(&root.ref_tree_hash, &row.path, &txn)
            .await?;
        if creating
            && let Some(resolved) = resolved.as_deref()
            && resolved != tip_tree_id.to_string()
        {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "PushFailure",
                    "create tree must match resolve(root, P) or the path must be absent from the root tree".into(),
                )
                .await;
        }

        let mono_api = MonoApiService {
            storage: ctx.storage.clone(),
            git_object_cache: ctx.git_object_cache.clone(),
        };
        let root_tree = Tree::from_mega_model(
            self.mono_storage
                .get_tree_by_hash(&root.ref_tree_hash)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(format!("root tree {} not found", root.ref_tree_hash))
                })?,
        );
        let normalized = MonoServiceLogic::clean_path_str(&row.path);
        let path = PathBuf::from(&normalized);
        let parent = path
            .parent()
            .ok_or_else(|| MegaError::Other(format!("Invalid push path: {normalized}")))?;

        let (update_chain, extra_blob) = if creating {
            tree_ops::search_tree_for_update_or_create_from_root(&mono_api, parent, root_tree)
                .await
                .map_err(|e| MegaError::Other(e.to_string()))?
        } else {
            let chain = tree_ops::search_tree_for_update_from_root(&mono_api, parent, root_tree)
                .await
                .map_err(|e| MegaError::Other(e.to_string()))?;
            (chain, None)
        };

        let result = if creating {
            MonoServiceLogic::build_result_by_chain_inserting(path, update_chain, tip_tree_id)
        } else {
            MonoServiceLogic::build_result_by_chain(path, update_chain, tip_tree_id)
        }
        .map_err(|e| MegaError::Other(e.to_string()))?;

        let land_ts = chrono::Utc::now().timestamp() as usize;
        let signing = mono_api.server_signing_context()?;
        let signing_key = std::sync::Arc::new(signing.active_key().await?);
        let sign_git: crate::ceres::api_service::mono_api_service::TrunkGitSign = {
            let signing = signing.clone();
            let signing_key = std::sync::Arc::clone(&signing_key);
            std::sync::Arc::new(move |c: &Commit| {
                signing
                    .sign_commit_preserving_identities(&signing_key, c)
                    .map_err(|e| GitError::CustomError(e.to_string()))
            })
        };
        let sign_mega: crate::ceres::pack::trunk_provenance::TrunkMegaSign = {
            let signing = signing.clone();
            let signing_key = std::sync::Arc::clone(&signing_key);
            std::sync::Arc::new(move |c: &Commit| {
                signing.sign_commit_preserving_identities(&signing_key, c)
            })
        };

        let mut tip_first: Vec<Commit> = Vec::with_capacity(payload.commits.len());
        if payload.commits.is_empty() {
            tip_first.push(tip_commit.clone());
        } else {
            for id in &payload.commits {
                let model = self
                    .mono_storage
                    .get_commit_by_hash(id)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(format!("push payload commit {id} not found"))
                    })?;
                tip_first.push(Commit::from_mega_model(model));
            }
        }
        let topo_asc = trunk_provenance::load_topo_asc(&tip_first)?;

        let prev_p = if creating {
            None
        } else {
            self.mono_storage
                .get_commit_by_hash(&row.old_id)
                .await?
                .map(Commit::from_mega_model)
                .map(|c| c.committer)
        };
        let range = if creating {
            None
        } else {
            Some((row.old_id.clone(), row.new_id.clone()))
        };

        let (landed_at_p, extra_commits) = if payload.n == 1 {
            (row.new_id.clone(), Vec::new())
        } else {
            let parents = if creating {
                Vec::new()
            } else {
                let parent_hash = ObjectHash::from_hex_for_kind(get_hash_kind(), &row.old_id)
                    .map_err(|e| MegaError::Other(e.to_string()))?;
                vec![parent_hash]
            };
            let draft = TrunkProvenance::from_tip(
                &tip_commit,
                payload.n,
                &normalized,
                String::new(),
                range.clone(),
                land_ts,
            );
            let unsigned = trunk_provenance::synthesize(
                draft.author.clone(),
                draft.committer(prev_p.as_ref()),
                tip_tree_id,
                parents,
                &draft.squash_message(&topo_asc),
            );
            let squash = sign_mega(&unsigned)?;
            (squash.id.to_string(), vec![squash])
        };

        let plan = TrunkProvenance::from_tip(
            &tip_commit,
            payload.n,
            &normalized,
            landed_at_p.clone(),
            range,
            land_ts,
        );

        PushQueueStorage::savepoint(&txn, "b3_kind").await?;
        let apply = mono_api
            .apply_push_in_txn(
                &txn,
                PushApplyArgs {
                    result: &result,
                    path_p: &normalized,
                    landed_at_p: &landed_at_p,
                    expected_root_commit: cur_commit,
                    expected_root_tree: cur_tree,
                    extra_commits,
                    extra_blob,
                    provenance: Some(&plan),
                    sign_trunk: Some(std::sync::Arc::clone(&sign_git)),
                },
            )
            .await;
        let (landed_commit_id, root_cas_writes) = match apply {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                if msg.contains(MonoApiService::MERGE_ROOT_CAS_MISS) {
                    self.note_cas_assert_failure();
                    PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
                    let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                        &txn,
                        row.id,
                        "QueueBypassDetected",
                        "root CAS affected 0 rows",
                    )
                    .await?;
                    if !updated {
                        tracing::error!(id = row.id, "B3 push CAS fail-closed: 0 rows");
                    }
                    PushQueueStorage::notify_mono_write_queue(&txn).await?;
                    txn.commit().await?;
                    return Ok(ExecuteOutcome::BypassDetected { id: row.id });
                }
                return self.b3_fail_merge(txn, row.id, "PushFailure", msg).await;
            }
        };

        let old_tree = path_row.as_ref().map(|r| r.ref_tree_hash.clone());
        let landed_tree = self
            .mono_storage
            .get_main_ref_in_txn(&normalized, &txn)
            .await?
            .map(|r| r.ref_tree_hash)
            .unwrap_or_else(|| tip_tree_id.to_string());
        self.mono_storage
            .advance_descendant_refs_with(
                &normalized,
                &landed_tree,
                old_tree.as_deref(),
                &txn,
                DescendantCommitStyle::Trunk {
                    plan: Box::new(plan.clone()),
                    sign: std::sync::Arc::clone(&sign_mega),
                },
            )
            .await?;

        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, &landed_commit_id).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(id = row.id, "B3 push Done update hit 0 rows after fencing");
            return Ok(self.outcome_claim_lost(row.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        self.run_c_segment_index(row.id, &row.path).await;
        Ok(ExecuteOutcome::Done {
            id: row.id,
            landed_commit_id,
            root_cas_writes,
        })
    }

    async fn b3_execute_merge(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &MergeExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        let id = row.id;
        match self
            .b3_execute_merge_inner(txn, row, ctx, cur_commit, cur_tree, root)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                tracing::error!(
                    id,
                    error = %e,
                    "B3 merge aborted with Err; terminalizing MergeFailure"
                );
                self.terminalize_merge_failure(id, &e.to_string()).await
            }
        }
    }

    async fn terminalize_merge_failure(
        &self,
        id: i64,
        message: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        use sea_orm::TransactionTrait;

        let conn = self.push_queue_storage.get_connection();
        let txn = conn.begin().await?;
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, "MergeFailure", message)
                .await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id,
                "B3 merge Err recovery: Failed update hit 0 rows (already terminal?)"
            );
            return Ok(self.outcome_claim_lost(id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: "MergeFailure".into(),
            message: message.to_owned(),
        })
    }

    async fn b3_fail_merge(
        &self,
        txn: sea_orm::DatabaseTransaction,
        id: i64,
        failure: &str,
        message: String,
    ) -> Result<ExecuteOutcome, MegaError> {
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, failure, &message).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(id, "B3 merge fail-close hit 0 rows");
            return Ok(self.outcome_claim_lost(id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: failure.to_owned(),
            message,
        })
    }

    async fn b3_execute_merge_inner(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &MergeExecContext,
        cur_commit: Option<&str>,
        cur_tree: Option<&str>,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<ExecuteOutcome, MegaError> {
        use std::path::PathBuf;

        use git_internal::internal::object::{commit::Commit, tree::Tree};
        use sea_orm::TransactionTrait;

        use crate::{
            callisto::sea_orm_active_enums::{ConvTypeEnum, MergeStatusEnum},
            ceres::api_service::{
                mono_api_service::{
                    MonoApiService, QueueExecutionDecision, authz_freeze_message,
                    decide_queue_execution, emit_authz_frozen_alert,
                },
                tree_ops,
            },
            contract::policy::{
                enforcement::Enforcement,
                notify::{after_b3_commit_authz, authz_blob_id, insert_b3_authz_outbox_if_builds},
            },
            jupiter::utils::converter::FromMegaModel,
        };

        let payload: MergePayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| MegaError::Other(format!("merge payload deserialize failed: {e}")))?;

        let Some(root) = root else {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "MergeFailure",
                    "merge requires an existing root ref".into(),
                )
                .await;
        };

        let mono_api = MonoApiService {
            storage: ctx.storage.clone(),
            git_object_cache: ctx.git_object_cache.clone(),
        };
        // TP-11: tree-hash assertion precedes get_cl / UN-17 / conflict recheck.
        if mono_api
            .assert_merge_tree_hash_tp11(&txn, &row.path, &root.ref_tree_hash)
            .await?
            .is_some()
        {
            return self.b3_refuse_stale_main_tree(txn, row.id, &row.path).await;
        }

        let Some(cl) = ctx
            .storage
            .cl_storage()
            .get_cl_in_txn(&payload.cl_link, &txn)
            .await?
        else {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "MergeFailure",
                    format!("CL {} no longer exists, cannot merge", payload.cl_link),
                )
                .await;
        };
        if cl.status == MergeStatusEnum::Closed {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "MergeFailure",
                    "CL has been closed, cannot merge".into(),
                )
                .await;
        }
        if cl.status == MergeStatusEnum::Draft {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "MergeFailure",
                    "CL is in draft status, cannot merge".into(),
                )
                .await;
        }
        if cl.status == MergeStatusEnum::Merged {
            return self
                .b3_fail_merge(txn, row.id, "MergeFailure", "CL is already merged".into())
                .await;
        }

        let mut authz_principal = payload.authz_principal.clone();
        if payload.apply_queue_execution_decision {
            let enforcement = Enforcement::parse(&ctx.storage.config().cedar.enforcement)
                .unwrap_or(Enforcement::Off);
            let snapshot = ctx.storage.entity_store().snapshot();
            match decide_queue_execution(
                enforcement,
                snapshot.as_deref(),
                payload.requester.as_deref(),
            ) {
                QueueExecutionDecision::Execute {
                    authz_principal: decided,
                } => {
                    authz_principal = decided;
                }
                QueueExecutionDecision::Freeze { reason } => {
                    emit_authz_frozen_alert(
                        &payload.cl_link,
                        payload.requester.as_deref(),
                        &reason,
                    );
                    return self
                        .b3_fail_merge(txn, row.id, "SystemError", authz_freeze_message(&reason))
                        .await;
                }
            }
        }

        if let Err(error) = mono_api
            .enforce_acl_change_authorization(&cl.link, &authz_principal)
            .await
        {
            let message = error.to_string();
            let failure = if message.contains("[code:503]") {
                "SystemError"
            } else {
                "MergeFailure"
            };
            return self.b3_fail_merge(txn, row.id, failure, message).await;
        }

        if let Err(error) = mono_api.ensure_gpg_check_passed(&cl.link).await {
            return self
                .b3_fail_merge(txn, row.id, "MergeFailure", error.to_string())
                .await;
        }

        let path_main = self
            .mono_storage
            .get_main_ref_in_txn(&cl.path, &txn)
            .await?;
        let Some(path_main) = path_main else {
            return self
                .b3_fail_merge(
                    txn,
                    row.id,
                    "MergeFailure",
                    format!("Main ref not found at {}", cl.path),
                )
                .await;
        };
        if cl.from_hash != path_main.ref_commit_hash {
            return self
                .b4_conflict_requeue_holding_lock(txn, row.id, row)
                .await;
        }

        let commit_model = self
            .mono_storage
            .get_commit_by_hash(&cl.to_hash)
            .await?
            .ok_or_else(|| MegaError::Other(format!("Commit not found: {}", cl.to_hash)))?;
        let commit: Commit = Commit::from_mega_model(commit_model);

        let root_tree = Tree::from_mega_model(
            self.mono_storage
                .get_tree_by_hash(&root.ref_tree_hash)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(format!("root tree {} not found", root.ref_tree_hash))
                })?,
        );

        let normalized_path =
            crate::ceres::api_service::mono_api_service::MonoServiceLogic::clean_path_str(&cl.path);
        let (path, update_chain) = if normalized_path == "/" {
            (PathBuf::from("/"), Vec::new())
        } else {
            let path = PathBuf::from(&normalized_path);
            let parent = path
                .parent()
                .ok_or_else(|| MegaError::Other(format!("Invalid CL path: {normalized_path}")))?;
            let update_chain =
                tree_ops::search_tree_for_update_from_root(&mono_api, parent, root_tree)
                    .await
                    .map_err(|e| MegaError::Other(e.to_string()))?;
            (path, update_chain)
        };
        let result =
            crate::ceres::api_service::mono_api_service::MonoServiceLogic::build_result_by_chain(
                path,
                update_chain,
                commit.tree_id,
            )
            .map_err(|e| MegaError::Other(e.to_string()))?;

        PushQueueStorage::savepoint(&txn, "b3_kind").await?;

        let apply = mono_api
            .apply_update_result_in_txn(
                &txn,
                &result,
                "cl merge generated commit",
                &cl,
                cur_commit,
                cur_tree,
            )
            .await;
        let (landed_commit_id, root_cas_writes) = match apply {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
                if msg.contains(MonoApiService::MERGE_ROOT_CAS_MISS) {
                    self.note_cas_assert_failure();
                    PushQueueStorage::set_hard_stopped_in_txn(&txn, true).await?;
                    let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                        &txn,
                        row.id,
                        "QueueBypassDetected",
                        "root CAS affected 0 rows",
                    )
                    .await?;
                    if !updated {
                        tracing::error!(
                            id = row.id,
                            "B3 merge CAS fail-closed: Failed update hit 0 rows"
                        );
                    }
                    PushQueueStorage::notify_mono_write_queue(&txn).await?;
                    txn.commit().await?;
                    return Ok(ExecuteOutcome::BypassDetected { id: row.id });
                }
                return self.b3_fail_merge(txn, row.id, "MergeFailure", msg).await;
            }
        };

        let old_tree_p = path_main.ref_tree_hash.clone();
        let landed_tree = self
            .mono_storage
            .get_main_ref_in_txn(&normalized_path, &txn)
            .await?
            .map(|r| r.ref_tree_hash)
            .unwrap_or_else(|| commit.tree_id.to_string());
        self.mono_storage
            .advance_descendant_refs(
                &normalized_path,
                &landed_tree,
                Some(old_tree_p.as_str()),
                &txn,
            )
            .await?;

        if !ctx.pause_after_apply.is_zero() {
            if let Some(barrier) = &ctx.pause_after_apply_barrier {
                barrier.wait().await;
            }
            tokio::time::sleep(ctx.pause_after_apply).await;
        }
        if ctx.abort_before_cl_status {
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            txn.rollback().await?;
            return Err(MegaError::Other("test abort before CL status write".into()));
        }

        ctx.storage
            .conversation_storage()
            .add_conversation_in_txn(
                &cl.link,
                &payload.execution_actor,
                None,
                ConvTypeEnum::Merged,
                &txn,
            )
            .await?;

        if !ctx
            .storage
            .cl_storage()
            .merge_cl_in_txn(cl.clone(), &txn)
            .await?
        {
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            txn.rollback().await?;
            if !self
                .push_queue_storage
                .reset_running_to_queued(row.id)
                .await?
            {
                return Ok(self.outcome_claim_lost(row.id));
            }
            let conn = self.push_queue_storage.get_connection();
            let ntxn = conn.begin().await?;
            PushQueueStorage::notify_mono_write_queue(&ntxn).await?;
            ntxn.commit().await?;
            return Ok(self.outcome_claim_lost(row.id));
        }

        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, &landed_commit_id).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(id = row.id, "B3 merge Done update hit 0 rows after fencing");
            return Ok(self.outcome_claim_lost(row.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        insert_b3_authz_outbox_if_builds(&ctx.storage, &txn, row.id).await?;
        txn.commit().await?;

        let blob_ids = async {
            let old_blob_id = self
                .mono_storage
                .get_tree_by_hash(&root.ref_tree_hash)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            let new_commit = self
                .mono_storage
                .get_commit_by_hash(&landed_commit_id)
                .await?
                .ok_or_else(|| MegaError::Other("new commit not found".into()))?;
            let new_blob_id = self
                .mono_storage
                .get_tree_by_hash(&new_commit.tree)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            Ok::<_, MegaError>((old_blob_id, new_blob_id))
        }
        .await;
        match blob_ids {
            Ok((old_blob_id, new_blob_id)) => {
                after_b3_commit_authz(&ctx.storage, old_blob_id.as_deref(), new_blob_id.as_deref())
                    .await;
            }
            Err(e) => {
                ctx.storage.entity_store().mark_dirty();
                tracing::error!(
                    id = row.id,
                    error = %e,
                    "merge authz blob resolve failed after Done; marked entity store dirty"
                );
            }
        }

        mono_api.maybe_invalidate_admin_cache(&cl.link).await;

        self.run_c_segment_index(row.id, &row.path).await;
        Ok(ExecuteOutcome::Done {
            id: row.id,
            landed_commit_id,
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
                return Ok(self.outcome_claim_lost(id));
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
            return Ok(self.outcome_claim_lost(id));
        }

        // Intent persisted; recheck hard_stop before successor INSERT.
        if PushQueueStorage::is_hard_stopped_in_txn(&txn).await? {
            txn.rollback().await?;
            if !self.push_queue_storage.reset_running_to_queued(id).await? {
                return Ok(self.outcome_claim_lost(id));
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
        callisto::{mega_refs, sea_orm_active_enums::PushQueueFailureEnum},
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
        assert!(del.to_string().contains("parent-path"));

        let other = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: "refs/heads/dev→0".into(),
                path: "/project/x".into(),
                old_id: "a".repeat(40),
                new_id: ZERO_ID.into(),
                requester: None,
                payload: json!({}),
                ref_name: Some("refs/heads/dev".into()),
                is_delete: true,
            })
            .await
            .expect_err("non-main delete");
        assert!(other.to_string().contains("parent-path"));
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

    #[test]
    fn b0_trunk_uses_configured_max_push_commits() {
        let trunk =
            PushQueueService::new(BaseStorage::mock(), PushPolicy::Trunk).with_max_push_commits(2);
        let over = EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: "a→c".into(),
            path: "/project/x".into(),
            old_id: "a".repeat(40),
            new_id: "c".repeat(40),
            requester: None,
            payload: json!({"n": 3}),
            ref_name: Some(MEGA_BRANCH_NAME.into()),
            is_delete: false,
        };
        let err = trunk
            .b0_reject_push(&over)
            .expect_err("n=3 must exceed max_push_commits=2");
        assert!(err.to_string().contains("2"), "{err}");
        let mut at_limit = over.clone();
        at_limit.payload = json!({"n": 2});
        assert!(
            trunk.b0_reject_push(&at_limit).is_ok(),
            "n equal to max_push_commits must pass B0"
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
                svc.execute_b3(
                    ExecuteRequest {
                        id,
                        pre_lock_delay: Duration::from_millis(80),
                        ..Default::default()
                    },
                    None,
                    None,
                    None,
                )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    force_cas_miss: true,
                    kind_root_write: KindRootWrite::Advance,
                    advance_commit: Some("e".repeat(40)),
                    advance_tree: Some("f".repeat(40)),
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    force_conflict: true,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    force_failure: Some("injected boom".into()),
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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
            .execute_b3(
                ExecuteRequest {
                    id,
                    kind_root_write: KindRootWrite::Advance,
                    advance_commit: Some(new_c.clone()),
                    advance_tree: Some(new_t.clone()),
                    ..Default::default()
                },
                None,
                None,
                None,
            )
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

    #[tokio::test]
    async fn tp09_b0_rejects_zero_id_create_when_tombstone_exists() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        svc.mono_storage()
            .upsert_tombstone(
                "/project/x",
                MEGA_BRANCH_NAME,
                &"a".repeat(40),
                &"b".repeat(40),
            )
            .await
            .unwrap();
        let err = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(ZERO_ID, &"c".repeat(40)),
                path: "/project/x".into(),
                old_id: ZERO_ID.into(),
                new_id: "c".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: false,
            })
            .await
            .expect_err("create on tombstone");
        let msg = err.to_string();
        assert!(msg.contains("tombstone"), "{msg}");
        assert!(msg.contains("advertise"), "{msg}");
        assert!(msg.contains("fetch"), "{msg}");
    }

    #[tokio::test]
    async fn tp09_kill9_before_savepoint_commit_leaves_running_then_reaper_i3() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-TB-K9").await;
        let path = "/CL-TB-K9".to_string();
        svc.mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    &path,
                    MEGA_BRANCH_NAME.to_owned(),
                    "s".repeat(40),
                    "t".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();

        let conn = svc.mono_storage().get_connection();
        let txn = conn.begin().await.unwrap();
        PushQueueStorage::savepoint(&txn, "b3_kind").await.unwrap();
        svc.mono_storage()
            .upsert_tombstone_in_txn(
                &path,
                MEGA_BRANCH_NAME,
                &"s".repeat(40),
                &"t".repeat(40),
                &txn,
            )
            .await
            .unwrap();
        txn.rollback().await.unwrap();

        assert!(
            svc.mono_storage()
                .get_tombstone(&path, MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_none(),
            "uncommitted SAVEPOINT repair must not be visible"
        );
        assert_eq!(
            svc.storage().get_by_id(id).await.unwrap().unwrap().status,
            PushQueueStatusEnum::Running
        );

        assert!(svc.reaper_i3_tombstone_repair(id).await.unwrap());
        let tomb = svc
            .mono_storage()
            .get_tombstone(&path, MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .expect("reaper wrote tombstone");
        assert_eq!(tomb.last_commit_hash, "s".repeat(40));
        assert!(
            svc.mono_storage()
                .get_main_ref(&path)
                .await
                .unwrap()
                .is_none()
        );
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(row.failure_type, Some(PushQueueFailureEnum::SystemError));
        assert!(row.pending_action.is_none());
    }

    #[tokio::test]
    async fn tp09_b3_savepoint_repair_commits_tombstone_and_terminal() {
        let (_t, svc) = service(PushPolicy::Trunk).await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-TB-SP").await;
        let path = "/CL-TB-SP".to_string();
        svc.mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    &path,
                    MEGA_BRANCH_NAME.to_owned(),
                    "s".repeat(40),
                    "t".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();

        let conn = svc.mono_storage().get_connection();
        let txn = conn.begin().await.unwrap();
        PushQueueStorage::savepoint(&txn, "b3_kind").await.unwrap();
        svc.mono_storage()
            .upsert_tombstone_in_txn(&path, MEGA_BRANCH_NAME, "x", "y", &txn)
            .await
            .unwrap();
        PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind")
            .await
            .unwrap();
        let outcome = svc
            .b3_tombstone_repair_after_savepoint(txn, id, &path, "stale materialized ref")
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ExecuteOutcome::Failed {
                failure,
                ..
            } if failure == "Conflict"
        ));
        let tomb = svc
            .mono_storage()
            .get_tombstone(&path, MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tomb.last_commit_hash, "s".repeat(40));
        assert!(
            svc.mono_storage()
                .get_main_ref(&path)
                .await
                .unwrap()
                .is_none()
        );
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(row.failure_type, Some(PushQueueFailureEnum::Conflict));
        assert!(row.pending_action.is_none());
    }
}
