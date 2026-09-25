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
        errors::{ImportRepoError, MegaError},
        utils::{
            MEGA_BRANCH_NAME, ZERO_ID, commit_body_subject, format_commit_msg, is_protocol_zero_id,
            split_commit_message,
        },
    },
    config::{DEFAULT_MAX_PUSH_COMMITS, PushPolicy},
    jupiter::storage::{
        audit_storage::{AuditStorage, IMPORT_REPO_REMOVE_KIND},
        base_storage::{BaseStorage, StorageConnector},
        blob_path_index::BlobPathIndexMode,
        git_db_storage::GitDbStorage,
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

/// Message of the monorepo root commit that mounts an ImportRepo leaf: the
/// imported tip's subject taken from its body, so signature headers of any
/// kind (`gpgsig`, `gpgsig-sha256`, PGP or SSH) never leak (plan-20260923
/// FU-02). Framed with the header/body blank line.
fn attach_root_message(tip_message: &str) -> String {
    format_commit_msg(
        commit_body_subject(split_commit_message(tip_message).body),
        None,
    )
}

pub fn attach_operation_id(repo_id: &str, normalized_commands: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(normalized_commands.as_bytes());
    hex::encode(hasher.finalize())
}

/// Queue identity of detaching ImportRepo `repo_id` from `canonical_path`
/// (plan-20260923 ADR-FU-09 item 1). The domain prefix keeps it apart from
/// every attach id; a re-import gets a new `repo_id` and so a new id.
pub fn detach_operation_id(repo_id: i64, canonical_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"import_repo.detach\0");
    hasher.update(repo_id.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_path.as_bytes());
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

/// What an ImportRepo row of the attach queue does (plan-20260923 ADR-FU-09
/// item 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachOp {
    #[default]
    Attach,
    Detach,
}

impl AttachOp {
    fn is_attach(&self) -> bool {
        *self == AttachOp::Attach
    }
}

/// Serializable attach payload (trunk-push 1.4 / 1.9).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AttachPayload {
    pub repo_id: i64,
    pub repo_path: String,
    pub commands: Vec<AttachCommand>,
    /// Absent from rows written before FU-16, which are all attaches; an
    /// attach still serializes without it.
    #[serde(default, skip_serializing_if = "AttachOp::is_attach")]
    pub op: AttachOp,
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

/// Apply one push's ImportRepo branch commands inside the B3 transaction with
/// receive-pack CAS (plan-20260923 ADR-FU-08 items 2 and 4): `Create` needs
/// the ref to be absent, `Update` needs it to still point at `old_id`, and
/// `Delete` keeps its lease. The first command that fails its CAS comes back
/// as `IMPORT_REPO_STALE_REF`; the caller rolls the whole batch back, so no
/// branch moves.
pub(crate) async fn apply_import_branch_commands_in_txn(
    git_db: &GitDbStorage,
    repo_id: i64,
    commands: &[AttachCommand],
    txn: &DatabaseTransaction,
) -> Result<Result<(), ImportRepoError>, MegaError> {
    use crate::{
        callisto::sea_orm_active_enums::RefTypeEnum,
        ceres::protocol::import_refs::{CommandType, RefCommand},
    };

    for cmd in commands.iter().filter(|cmd| cmd.ref_type == "branch") {
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
        let applied = match command_type {
            CommandType::Create => {
                let ref_cmd = RefCommand {
                    ref_name: cmd.ref_name.clone(),
                    old_id: cmd.old_id.clone(),
                    new_id: cmd.new_id.clone(),
                    status: "ok".into(),
                    error_msg: String::new(),
                    command_type: CommandType::Create,
                    ref_type: RefTypeEnum::Branch,
                    default_branch: cmd.default_branch,
                };
                git_db
                    .create_ref_if_absent(repo_id, ref_cmd.into(), txn)
                    .await?
            }
            CommandType::Update => {
                git_db
                    .update_ref_if_unchanged(repo_id, &cmd.ref_name, &cmd.old_id, &cmd.new_id, txn)
                    .await?
            }
            CommandType::Delete => {
                git_db
                    .remove_ref_if_unchanged(repo_id, &cmd.ref_name, &cmd.old_id, txn)
                    .await?
            }
        };
        if !applied {
            return Ok(Err(ImportRepoError::StaleRef {
                ref_name: cmd.ref_name.clone(),
                expected: cmd.old_id.clone(),
            }));
        }
        if cmd.default_branch && command_type != CommandType::Delete {
            git_db
                .set_default_branch_in_txn(repo_id, &cmd.ref_name, txn)
                .await?;
        }
    }
    Ok(Ok(()))
}

/// Where an ImportRepo leaf path stands in a monorepo root tree
/// (plan-20260923 ADR-FU-08 item 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportLeaf {
    Absent,
    GitkeepOnly,
    Directory,
    NotDirectory,
}

async fn import_leaf_in_txn(
    mono: &MonoStorage,
    root_tree: &str,
    path: &str,
    txn: &DatabaseTransaction,
) -> Result<ImportLeaf, MegaError> {
    use git_internal::internal::object::tree::{Tree, TreeItemMode};

    use crate::jupiter::utils::converter::FromMegaModel;

    let missing = |hash: &str| MegaError::Other(format!("tree {hash} not found"));
    let model = mono
        .get_tree_by_hash_in_txn(root_tree, txn)
        .await?
        .ok_or_else(|| missing(root_tree))?;
    let mut tree = Tree::from_mega_model(model);
    for component in path.split('/').filter(|c| !c.is_empty()) {
        let Some(item) = tree.tree_items.iter().find(|x| x.name == component) else {
            return Ok(ImportLeaf::Absent);
        };
        if item.mode != TreeItemMode::Tree {
            return Ok(ImportLeaf::NotDirectory);
        }
        let hash = item.id.to_string();
        let next = mono
            .get_tree_by_hash_in_txn(&hash, txn)
            .await?
            .ok_or_else(|| missing(&hash))?;
        tree = Tree::from_mega_model(next);
    }
    Ok(
        if crate::ceres::api_service::tree_ops::is_gitkeep_only(&tree) {
            ImportLeaf::GitkeepOnly
        } else {
            ImportLeaf::Directory
        },
    )
}

/// Whether ImportRepo `repo_id` has at least one branch ref, without reading
/// them (the legacy-mount rule of plan-20260923 ADR-FU-08 item 1).
async fn has_branch_ref_in_txn(txn: &DatabaseTransaction, repo_id: i64) -> Result<bool, MegaError> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT EXISTS (SELECT 1 FROM import_refs WHERE repo_id = $1 AND ref_type = 'branch') AS v",
            [repo_id.into()],
        ))
        .await?;
    Ok(row
        .map(|row| row.try_get::<bool>("", "v"))
        .transpose()?
        .unwrap_or(false))
}

/// Trees from `root_tree` down to the parent of the leaf at `path`, read in
/// the B3 transaction, with the name of each step down.
async fn import_leaf_chain_in_txn(
    mono: &MonoStorage,
    root_tree: &str,
    path: &str,
    txn: &DatabaseTransaction,
) -> Result<(Vec<git_internal::internal::object::tree::Tree>, Vec<String>), MegaError> {
    use git_internal::internal::object::tree::{Tree, TreeItemMode};

    use crate::jupiter::utils::converter::FromMegaModel;

    let load = |hash: String| async move {
        mono.get_tree_by_hash_in_txn(&hash, txn)
            .await?
            .map(Tree::from_mega_model)
            .ok_or_else(|| MegaError::Other(format!("tree {hash} not found")))
    };
    let names: Vec<String> = path
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect();
    let (_, down) = names
        .split_last()
        .ok_or_else(|| MegaError::Other("ImportRepo path has no leaf".into()))?;
    let mut chain = Vec::with_capacity(names.len());
    let mut tree = load(root_tree.to_owned()).await?;
    for name in down {
        let next = tree
            .tree_items
            .iter()
            .find(|item| item.name == *name && item.mode == TreeItemMode::Tree)
            .map(|item| item.id.to_string())
            .ok_or_else(|| MegaError::Other(format!("tree entry {name:?} not found")))?;
        chain.push(tree);
        tree = load(next).await?;
    }
    chain.push(tree);
    Ok((chain, names))
}

/// Keep exactly one default branch after a batch that deleted the old one
/// (e.g. Delete of the old default + Create of a replacement without a
/// precomputed flag).
async fn ensure_default_branch_in_txn(
    git_db: &GitDbStorage,
    repo_id: i64,
    txn: &DatabaseTransaction,
) -> Result<(), MegaError> {
    if !git_db.default_branch_exist_in_txn(repo_id, txn).await?
        && let Some(first) = git_db
            .list_branch_refs_in_txn(repo_id, txn)
            .await?
            .into_iter()
            .next()
    {
        git_db
            .set_default_branch_in_txn(repo_id, &first.ref_name, txn)
            .await?;
    }
    Ok(())
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
    /// Test-only: two-phase sync inside the B3 txn right before
    /// `apply_push_in_txn` (after the baseline read): the executor signals
    /// "race window open" on the enter barrier, then blocks on the release
    /// barrier until the test's queue-bypassing writer has committed — a
    /// deterministic apply-time root CAS miss (WH-03).
    pub pre_apply_enter_barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
    /// Test-only: the executor resumes only after this barrier completes.
    pub pre_apply_release_barrier: Option<std::sync::Arc<tokio::sync::Barrier>>,
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

        // T05 (spec 09 §1): record the publication receipt, outbox event and
        // per-namespace sequence in the *same* transaction as the root CAS
        // above, so the visible change, its sequence and its outbox commit
        // atomically. Gated off by default (`mst2.publication_enabled`), so
        // an unconfigured deployment keeps the exact prior behavior.
        if let Some(ctx) = push_ctx
            && ctx.storage.config().mst2.publication_enabled
        {
            let old_oid = cur_commit.unwrap_or(ZERO_ID);
            let namespace = self.mono_storage.normalize_namespace(&row.path);
            self.mono_storage
                .record_publication_in_txn(
                    &txn,
                    &row.operation_id,
                    &namespace,
                    old_oid,
                    &new_commit,
                    "trunk_push",
                )
                .await?;
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
                let op = row
                    .payload
                    .get("op")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("attach");
                tracing::error!(
                    id,
                    op,
                    error = %e,
                    "B3 attach aborted with Err; terminalizing AttachFailure"
                );
                self.terminalize_attach_failure(id, &e.to_string()).await
            }
        }
    }

    /// Refuse an attach round inside its transaction: undo the round's writes
    /// (SAVEPOINT `b3_kind`) and persist the row `Failed`.
    async fn b3_attach_fail(
        &self,
        txn: sea_orm::DatabaseTransaction,
        id: i64,
        message: String,
    ) -> Result<ExecuteOutcome, MegaError> {
        PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
        self.b3_attach_fail_unsaved(txn, id, message).await
    }

    /// Refuse an attach round that has written nothing yet (before SAVEPOINT
    /// `b3_kind`): persist the row `Failed`.
    async fn b3_attach_fail_unsaved(
        &self,
        txn: sea_orm::DatabaseTransaction,
        id: i64,
        message: String,
    ) -> Result<ExecuteOutcome, MegaError> {
        let updated =
            PushQueueStorage::mark_failed_if_running_in_txn(&txn, id, "AttachFailure", &message)
                .await?;
        if !updated {
            tracing::error!(id, "B3 attach refusal hit 0 rows");
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Failed {
            id,
            failure: "AttachFailure".into(),
            message,
        })
    }

    /// B3 detach of an ImportRepo (plan-20260923 ADR-FU-09 items 2 and 3),
    /// inside the attach SAVEPOINT: lock the repository's own row, refuse
    /// while other ImportRepos live below it, remove the mount when this
    /// repository owns it, then delete its refs and row and record the detach
    /// in the audit log and the cleanup ledger. Its objects stay for the
    /// out-of-lock sweep (FU-17).
    async fn b3_execute_detach(
        &self,
        txn: DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &AttachExecContext,
        payload: &AttachPayload,
        repo_path: &str,
        root: &crate::callisto::mega_refs::Model,
    ) -> Result<ExecuteOutcome, MegaError> {
        use git_internal::{
            hash::{ObjectHash, get_hash_kind},
            internal::object::commit::Commit,
        };
        use sea_orm::{
            ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement,
        };

        use crate::{
            callisto::{
                git_repo, import_refs,
                sea_orm_active_enums::{ActorTypeEnum, AuditActionEnum, TargetTypeEnum},
            },
            ceres::api_service::tree_ops,
            common::utils::canonicalize_mono_ref_path,
        };

        let expected_commit = root.ref_commit_hash.clone();
        let expected_tree = root.ref_tree_hash.clone();
        PushQueueStorage::savepoint(&txn, "b3_kind").await?;

        // The repository's own row first, then its children (ADR-FU-09 item 2).
        let live = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT repo_path FROM git_repo WHERE id = $1 FOR UPDATE",
                [payload.repo_id.into()],
            ))
            .await?
            .map(|live| live.try_get::<String>("", "repo_path"))
            .transpose()?;
        if live
            .and_then(|live| canonicalize_mono_ref_path(&live).ok())
            .as_deref()
            != Some(repo_path)
        {
            // Already detached, or the path is another import now: no writes.
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            return self
                .b3_detach_finish(txn, row, ctx, &expected_commit, None)
                .await;
        }
        if ctx
            .storage
            .git_db_storage()
            .import_repo_has_children(payload.repo_id, repo_path, &txn)
            .await?
        {
            let refused = ImportRepoError::HasChildren {
                path: repo_path.to_owned(),
            };
            return self.b3_attach_fail(txn, row.id, refused.to_string()).await;
        }

        // The mount goes only when it is this repository's (a provenance
        // record, or the legacy `.gitkeep`-only shape of a repository with
        // branches); an ordinary directory or a missing leaf stays as it is.
        // Only a strict descendant of `import_dir` has a mount at all: a
        // legacy row at `import_dir` itself, or outside it, loses its rows
        // and nothing else (the import root is never removed).
        let git_db = ctx.storage.git_db_storage();
        let import_dir = canonicalize_mono_ref_path(
            &ctx.storage.config().monorepo.import_dir.to_string_lossy(),
        )?;
        let under_import_dir =
            repo_path.starts_with(&format!("{}/", import_dir.trim_end_matches('/')));
        let owned = under_import_dir
            && match import_leaf_in_txn(&self.mono_storage, &expected_tree, repo_path, &txn).await?
            {
                ImportLeaf::Absent | ImportLeaf::NotDirectory => false,
                leaf @ (ImportLeaf::GitkeepOnly | ImportLeaf::Directory) => {
                    AuditStorage::has_import_repo_attach_in_txn(&txn, payload.repo_id, repo_path)
                        .await?
                        || (leaf == ImportLeaf::GitkeepOnly
                            && has_branch_ref_in_txn(&txn, payload.repo_id).await?)
                }
            };
        let mut landed_commit_id = expected_commit.clone();
        let mut new_root_tree = None;
        if owned {
            let (chain, names) =
                import_leaf_chain_in_txn(&self.mono_storage, &expected_tree, repo_path, &txn)
                    .await?;
            let floor = import_dir.split('/').filter(|c| !c.is_empty()).count();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let (trees, gitkeep) = tree_ops::remove_import_leaf(&chain, &names, floor)?;
            let new_root = trees
                .last()
                .ok_or_else(|| MegaError::Other("no tree generated".into()))?;
            let new_commit = Commit::from_tree_id(
                new_root.id,
                vec![
                    ObjectHash::from_hex_for_kind(get_hash_kind(), &expected_commit).map_err(
                        |e| MegaError::Other(format!("invalid expected commit hash: {e}")),
                    )?,
                ],
                &format_commit_msg(&format!("Remove ImportRepo {repo_path}"), None),
            );
            landed_commit_id = new_commit.id.to_string();
            new_root_tree = Some(new_commit.tree_id.to_string());
            if let Some(blob) = gitkeep {
                use git_internal::internal::metadata::EntryMeta;
                use sea_orm::IntoActiveModel;

                use crate::{callisto::mega_blob, jupiter::utils::converter::IntoMegaModel};

                // The bytes go to the object store outside the SAVEPOINT (as in
                // attach); the `mega_blob` row rides the transaction, so a
                // refused round leaves no row behind.
                ctx.storage
                    .git_service
                    .put_objects(vec![blob.clone()])
                    .await?;
                let mut model: mega_blob::Model = blob.into_mega_model(EntryMeta::default());
                model.commit_id = landed_commit_id.clone();
                mega_blob::Entity::insert(model.into_active_model())
                    .exec(&txn)
                    .await?;
            }
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
                        "detach root CAS affected 0 rows",
                    )
                    .await?;
                    if !updated {
                        tracing::error!(
                            id = row.id,
                            "B3 detach CAS fail-closed: Failed update hit 0 rows"
                        );
                    }
                    PushQueueStorage::notify_mono_write_queue(&txn).await?;
                    txn.commit().await?;
                    return Ok(ExecuteOutcome::BypassDetected { id: row.id });
                }
                Err(e) => return Err(e),
            }
        }

        import_refs::Entity::delete_many()
            .filter(import_refs::Column::RepoId.eq(payload.repo_id))
            .exec(&txn)
            .await?;
        git_repo::Entity::delete_by_id(payload.repo_id)
            .exec(&txn)
            .await?;
        let requester = row
            .requester
            .clone()
            .unwrap_or_else(|| "anonymous".to_owned());
        AuditStorage::log_audit_in_txn(
            &txn,
            // Reserved actor: storage-only has no numeric user id.
            0,
            ActorTypeEnum::Human,
            AuditActionEnum::Delete,
            TargetTypeEnum::Repository,
            payload.repo_id,
            Some(serde_json::json!({
                "kind": IMPORT_REPO_REMOVE_KIND,
                "cleanup_id": row.id,
                "path": repo_path,
                "requester": requester,
                "phase": "detached",
            })),
        )
        .await?;
        git_db
            .insert_cleanup_in_txn(row.id, repo_path, payload.repo_id, &requester, &txn)
            .await?;
        self.b3_detach_finish(
            txn,
            row,
            ctx,
            &landed_commit_id,
            new_root_tree.map(|new_tree| (expected_tree, new_tree)),
        )
        .await
    }

    /// `Done` for a detach round. `root_trees` is `(old, new)` when the round
    /// moved the root, which then gets the same post-commit steps as attach.
    async fn b3_detach_finish(
        &self,
        txn: DatabaseTransaction,
        row: &push_queue::Model,
        ctx: &AttachExecContext,
        landed_commit_id: &str,
        root_trees: Option<(String, String)>,
    ) -> Result<ExecuteOutcome, MegaError> {
        use git_internal::internal::object::tree::Tree;

        use crate::{
            contract::policy::notify::{
                after_b3_commit_authz, authz_blob_id, insert_b3_authz_outbox_if_builds,
            },
            jupiter::utils::converter::FromMegaModel,
        };

        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, landed_commit_id).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id = row.id,
                "B3 detach Done update hit 0 rows after fencing"
            );
            return Ok(self.outcome_claim_lost(row.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        let Some((old_tree, new_tree)) = root_trees else {
            txn.commit().await?;
            return Ok(ExecuteOutcome::Done {
                id: row.id,
                landed_commit_id: landed_commit_id.to_owned(),
                root_cas_writes: 0,
            });
        };
        insert_b3_authz_outbox_if_builds(&ctx.storage, &txn, row.id).await?;
        txn.commit().await?;
        let blob_ids = async {
            let old_blob_id = self
                .mono_storage
                .get_tree_by_hash(&old_tree)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            let new_blob_id = self
                .mono_storage
                .get_tree_by_hash(&new_tree)
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
                    "detach authz blob resolve failed after Done; marked entity store dirty"
                );
            }
        }
        self.run_c_segment_index(row.id, &row.path).await;
        Ok(ExecuteOutcome::Done {
            id: row.id,
            landed_commit_id: landed_commit_id.to_owned(),
            root_cas_writes: 1,
        })
    }

    /// Update push to a mounted ImportRepo (ADR-FU-08 item 1): branch refs
    /// only, with CAS; the monorepo root does not move, so the row lands on the
    /// current root commit.
    async fn b3_attach_update_refs(
        &self,
        txn: sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        payload: &AttachPayload,
        git_db: &GitDbStorage,
        root_commit: &str,
    ) -> Result<ExecuteOutcome, MegaError> {
        if let Err(stale) =
            apply_import_branch_commands_in_txn(git_db, payload.repo_id, &payload.commands, &txn)
                .await?
        {
            return self.b3_attach_fail(txn, row.id, stale.to_string()).await;
        }
        ensure_default_branch_in_txn(git_db, payload.repo_id, &txn).await?;
        let updated =
            PushQueueStorage::mark_done_if_running_in_txn(&txn, row.id, root_commit).await?;
        if !updated {
            txn.rollback().await?;
            tracing::error!(
                id = row.id,
                "B3 attach update Done hit 0 rows after fencing"
            );
            return Ok(self.outcome_claim_lost(row.id));
        }
        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(ExecuteOutcome::Done {
            id: row.id,
            landed_commit_id: root_commit.to_owned(),
            root_cas_writes: 0,
        })
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
            ceres::api_service::{mono_api_service::MonoApiService, tree_ops},
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
        if payload.op == AttachOp::Detach {
            return self
                .b3_execute_detach(txn, row, ctx, &payload, &repo_path, root)
                .await;
        }

        // plan-20260923 ADR-FU-09 item 5: the repository must still be live,
        // whatever the round (first mount, update, delete-only). Its row stays
        // share-locked until this round commits; a detached repository is
        // refused before any other check and before any write.
        let git_db = ctx.storage.git_db_storage();
        if let Err(error) = git_db
            .lock_live_import_repo(&txn, payload.repo_id, &repo_path)
            .await
        {
            let MegaError::ImportRepo(removed) = error else {
                return Err(error);
            };
            return self
                .b3_attach_fail_unsaved(txn, row.id, removed.to_string())
                .await;
        }

        // A delete-only batch never touches the mount, so neither the
        // materialization precheck nor the ownership gate below protects
        // anything for it (and both would refuse deletes that worked before):
        // refs only, still inside B3 (GC-FU-03).
        let delete_only = !payload
            .commands
            .iter()
            .any(|c| c.ref_type == "branch" && !is_protocol_zero_id(&c.new_id));

        // Lock-held redo of materialization precheck (intervening writes).
        if !delete_only
            && let Err(e) = self
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

        // plan-20260923 ADR-FU-08 item 1: a leaf that is already a directory is
        // this ImportRepo's mount only with a provenance record, or in the
        // legacy `.gitkeep`-only shape of a repository that already has
        // branches (backfilled here); such a push writes refs only.
        if delete_only {
            return self
                .b3_attach_update_refs(txn, row, &payload, &git_db, &expected_commit)
                .await;
        }
        match import_leaf_in_txn(&self.mono_storage, &expected_tree, &repo_path, &txn).await? {
            ImportLeaf::Absent => {}
            leaf @ (ImportLeaf::GitkeepOnly | ImportLeaf::Directory) => {
                let recorded =
                    AuditStorage::has_import_repo_attach_in_txn(&txn, payload.repo_id, &repo_path)
                        .await?;
                let legacy = !recorded
                    && leaf == ImportLeaf::GitkeepOnly
                    && has_branch_ref_in_txn(&txn, payload.repo_id).await?;
                if !recorded && !legacy {
                    let occupied = ImportRepoError::PathOccupied { path: repo_path };
                    return self.b3_attach_fail(txn, row.id, occupied.to_string()).await;
                }
                if legacy {
                    AuditStorage::log_import_repo_attach_in_txn(&txn, payload.repo_id, &repo_path)
                        .await?;
                }
                return self
                    .b3_attach_update_refs(txn, row, &payload, &git_db, &expected_commit)
                    .await;
            }
            ImportLeaf::NotDirectory => {
                let occupied = ImportRepoError::PathOccupied { path: repo_path };
                return self.b3_attach_fail(txn, row.id, occupied.to_string()).await;
            }
        }
        // First mount. The provenance record is inside the SAVEPOINT, so any
        // later refusal (tree build, branch CAS, `.gitkeep`, root CAS) drops
        // it together with the mount. A re-mount after the leaf went away
        // keeps the record it already has.
        if !AuditStorage::has_import_repo_attach_in_txn(&txn, payload.repo_id, &repo_path).await? {
            AuditStorage::log_import_repo_attach_in_txn(&txn, payload.repo_id, &repo_path).await?;
        }

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
            .find(|c| c.ref_type == "branch" && !is_protocol_zero_id(&c.new_id))
            .map(|c| c.new_id.clone())
            .ok_or_else(|| MegaError::Other("attach payload has no branch tip".into()))?;

        let latest_commit: Commit = Commit::from_git_model(
            ctx.storage
                .git_db_storage()
                .get_commit_by_hash(payload.repo_id, &tip_commit_id)
                .await?
                .ok_or_else(|| MegaError::Other(format!("commit {tip_commit_id} not found")))?,
        );
        let commit_msg = attach_root_message(&latest_commit.message);

        let new_commit = Commit::from_tree_id(
            save_trees
                .back()
                .ok_or_else(|| MegaError::Other("no tree generated".into()))?
                .id,
            vec![
                ObjectHash::from_hex_for_kind(get_hash_kind(), &expected_commit)
                    .map_err(|e| MegaError::Other(format!("invalid expected commit hash: {e}")))?,
            ],
            &commit_msg,
        );
        let new_root_tree_hash = new_commit.tree_id.to_string();
        let landed_commit_id = new_commit.id.to_string();

        // Branch CAS first: a stale batch must not leave the `.gitkeep` blob
        // written below (object storage is outside the SAVEPOINT).
        if let Err(stale) =
            apply_import_branch_commands_in_txn(&git_db, payload.repo_id, &payload.commands, &txn)
                .await?
        {
            PushQueueStorage::rollback_to_savepoint(&txn, "b3_kind").await?;
            let msg = stale.to_string();
            let updated = PushQueueStorage::mark_failed_if_running_in_txn(
                &txn,
                row.id,
                "AttachFailure",
                &msg,
            )
            .await?;
            if !updated {
                tracing::error!(id = row.id, "B3 attach stale ref hit 0 rows");
            }
            PushQueueStorage::notify_mono_write_queue(&txn).await?;
            txn.commit().await?;
            return Ok(ExecuteOutcome::Failed {
                id: row.id,
                failure: "AttachFailure".into(),
                message: msg,
            });
        }
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

        ensure_default_branch_in_txn(&git_db, payload.repo_id, &txn).await?;

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
        // plan-20260923 ADR-FU-04 item 5: a true creation (no row, absent
        // from the root tree) is classified again under the lock.
        if creating
            && resolved.is_none()
            && let Err(err) = crate::ceres::pack::path_policy::classify_creation_path(
                &ctx.storage.config().monorepo,
                &row.path,
            )
        {
            return self
                .b3_fail_merge(txn, row.id, "PushFailure", err.to_string())
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

        // Test-only seam (WH-03): with the baseline read behind us and the
        // apply CAS ahead, signal the open race window, then wait until the
        // test's queue-bypassing writer has committed its root-row update.
        if let Some(barrier) = &ctx.pre_apply_enter_barrier {
            barrier.wait().await;
        }
        if let Some(barrier) = &ctx.pre_apply_release_barrier {
            barrier.wait().await;
        }
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

        // T05 (spec 09 §1/§7): record the publication receipt, outbox event
        // and per-namespace sequence in the *same* transaction as the ref CAS
        // above, so the visible change, its sequence and its outbox commit
        // atomically (trunk push = the primary default-namespace writer of
        // spec 09 §5). Gated off by default (`mst2.publication_enabled`):
        // a deployment that has not passed the §9 shadow comparison keeps
        // the exact prior behavior. Replaying the same operation id lands
        // the receipt once (PUB-11).
        if ctx.storage.config().mst2.publication_enabled {
            let namespace = self.mono_storage.normalize_namespace(&normalized);
            self.mono_storage
                .record_publication_in_txn(
                    &txn,
                    &row.operation_id,
                    &namespace,
                    cur_commit.unwrap_or(ZERO_ID),
                    &landed_commit_id,
                    "trunk_push",
                )
                .await?;
        }

        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        // WH-03 (plan-20260912 ADR-WH-04): the single `repo.push` emission
        // point — after the real n>0 B3 commit, before the C segment. The
        // snapshot comes from this round's row/result only (GC-08/GC-10); a
        // builder or emitter failure must never affect the committed push.
        let config = ctx.storage.config();
        if config.git.storage_only()
            && let Some(installation_id) = config.storage_events.installation_id.as_deref()
        {
            let emitter = &ctx.storage.storage_event_emitter;
            match crate::jupiter::service::storage_event_emitter::repo_push_event(
                installation_id,
                &normalized,
                crate::jupiter::service::storage_event_emitter::RepoPushData {
                    push_id: row.id.to_string(),
                    operation_id: row.operation_id.clone(),
                    ref_name: MEGA_BRANCH_NAME.to_owned(),
                    old_oid: row.old_id.clone(),
                    requested_oid: row.new_id.clone(),
                    landed_oid: landed_commit_id.clone(),
                },
            ) {
                Ok(event) => {
                    let _ = emitter.try_emit(event);
                }
                // A snapshot the builder rejects is a dropped event, not a
                // failed push (WH-15 / plan-20260912「固定 wire schema」).
                Err(_) => emitter.record_invalid_event(
                    crate::jupiter::service::storage_event::EventType::RepoPush,
                ),
            }
        }
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

    #[test]
    fn attach_root_message_uses_body_subject() {
        let ssh_signed = "gpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQ\n -----END SSH SIGNATURE-----\n\nimport: vendored subject\n\nbody\n";
        assert_eq!(
            attach_root_message(ssh_signed),
            "\nimport: vendored subject"
        );
        let pgp_sha256 = "gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n x\n -----END PGP SIGNATURE-----\n\nsha256 subject\n";
        assert_eq!(attach_root_message(pgp_sha256), "\nsha256 subject");
        assert_eq!(attach_root_message("\nplain subject\n"), "\nplain subject");
    }

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

    // ------------------------------------------------------------------
    // WH-03 (plan-20260912 / ADR-WH-04): `repo.push` is emitted exactly once,
    // at the real n>0 B3 push commit point; every other round shape delivers
    // nothing, and emitter failure never changes the push result.
    // ------------------------------------------------------------------

    const WH03_INSTALLATION: &str = "it-wh03";

    /// Recording fake transport (WH-09 test seam): captures exact body bytes
    /// and a call counter; never touches the network.
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<bytes::Bytes>>,
        fail: bool,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<bytes::Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: bytes::Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(crate::jupiter::service::storage_event_transport::TransportError::Timeout)
                } else {
                    Ok(
                        crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                            status: 200,
                        },
                    )
                }
            })
        }
    }

    /// The single compiled target used by the WH-03 emitters
    /// (`git_paths = ["/"]` covers every repo path).
    fn wh03_compiled_target() -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let target_config = crate::config::StorageEventsTargetConfig {
            id: "ops-main".to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
                .to_string(),
            events: vec!["repo.push".to_string()],
            git_paths: vec!["/".to_string()],
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = crate::jupiter::service::storage_event_transport::EventTarget::compile(
            &target_config.id,
            &target_config.url,
            &secret,
        )
        .expect("compile target");
        (target_config, compiled)
    }

    /// Enabled/disabled emitter over the recording transport with the shared
    /// compiled target.
    fn wh03_emitter(
        config: &crate::config::Config,
        transport: Arc<RecordingTransport>,
    ) -> crate::jupiter::service::storage_event_emitter::StorageEventEmitter {
        crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
            config,
            transport,
            vec![wh03_compiled_target()],
        )
    }

    /// Bounded wait for the blocking transport's recorded calls.
    async fn wh03_wait_calls_block(transport: &BlockingRecordingTransport, n: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }

    /// Storage-only trunk storage (`push_auth=none`) with the recording
    /// emitter installed as the application owner.
    async fn wh03_storage(
        events_enabled: bool,
        transport: Arc<RecordingTransport>,
    ) -> (tempfile::TempDir, crate::jupiter::storage::Storage) {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = events_enabled;
        config.storage_events.installation_id = Some(WH03_INSTALLATION.to_string());
        let mut storage =
            crate::jupiter::tests::test_storage_with_config(temp.path(), config.clone()).await;
        storage.set_storage_event_emitter(wh03_emitter(&config, transport));
        let storage = crate::jupiter::tests::with_test_vault(storage, temp.path()).await;
        (temp, storage)
    }

    fn wh03_blob_item(name: &str, hex: &str) -> git_internal::internal::object::tree::TreeItem {
        use std::str::FromStr;

        git_internal::internal::object::tree::TreeItem::new(
            git_internal::internal::object::tree::TreeItemMode::Blob,
            git_internal::hash::ObjectHash::from_str(hex).unwrap(),
            name.to_string(),
        )
    }

    /// Root + `main@/{dir}` seeded with one commit each (the tp12 fixture
    /// shape, so the main-path tree hash assertion holds).
    async fn wh03_path_fixture(
        storage: &crate::jupiter::storage::Storage,
        dir: &str,
    ) -> (git_internal::internal::object::commit::Commit, String) {
        use git_internal::internal::object::{
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        };

        let mono = storage.mono_storage();
        let child = Tree::from_tree_items(vec![wh03_blob_item(
            "x.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .expect("child");
        let root_tree = Tree::from_tree_items(vec![
            wh03_blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
        ])
        .expect("root");
        let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
        let path_commit = Commit::from_tree_id(child.id, vec![], "path tip");
        mono.save_mega_trees(vec![child.clone(), root_tree.clone()], root_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![root_commit.clone(), path_commit.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root_commit.id.to_string(),
                root_tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let path = format!("/{dir}");
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                path_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        (path_commit, path)
    }

    /// Persist a new child tree + n=1 commit on top of `parent_id`; returns
    /// the new commit id and its push descriptor.
    async fn wh03_save_n1_commit(
        storage: &crate::jupiter::storage::Storage,
        parent_id: git_internal::hash::ObjectHash,
        blob_hex: &str,
        msg: &str,
    ) -> (String, PushPayload) {
        use git_internal::internal::object::{commit::Commit, tree::Tree};

        let new_child = Tree::from_tree_items(vec![wh03_blob_item("y.txt", blob_hex)]).unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![parent_id], msg);
        storage
            .mono_storage()
            .save_mega_trees(vec![new_child], new_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![new_commit.clone()], None)
            .await
            .unwrap();
        let new_id = new_commit.id.to_string();
        (
            new_id.clone(),
            PushPayload {
                commits: vec![new_id],
                fork_base: Some(parent_id.to_string()),
                n: 1,
            },
        )
    }

    /// Variant fixture whose path tree carries a `sub/` subtree; returns the
    /// path tip, the path, and the v1 subtree (for the descendant row).
    async fn wh03_path_fixture_with_sub(
        storage: &crate::jupiter::storage::Storage,
        dir: &str,
    ) -> (
        git_internal::internal::object::commit::Commit,
        String,
        git_internal::internal::object::tree::Tree,
    ) {
        use git_internal::internal::object::{commit::Commit, tree::Tree};

        let mono = storage.mono_storage();
        let sub = Tree::from_tree_items(vec![wh03_blob_item(
            "z.txt",
            "0123456789012345678901234567890123456789",
        )])
        .expect("sub tree");
        let child = Tree::from_tree_items(vec![
            wh03_blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd"),
            git_internal::internal::object::tree::TreeItem::new(
                git_internal::internal::object::tree::TreeItemMode::Tree,
                sub.id,
                "sub".to_string(),
            ),
        ])
        .expect("child");
        let root_tree = Tree::from_tree_items(vec![
            wh03_blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            git_internal::internal::object::tree::TreeItem::new(
                git_internal::internal::object::tree::TreeItemMode::Tree,
                child.id,
                dir.to_string(),
            ),
        ])
        .expect("root");
        let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
        let path_commit = Commit::from_tree_id(child.id, vec![], "path tip");
        mono.save_mega_trees(
            vec![sub.clone(), child.clone(), root_tree.clone()],
            root_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(vec![root_commit.clone(), path_commit.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root_commit.id.to_string(),
                root_tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let path = format!("/{dir}");
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                path_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        (path_commit, path, sub)
    }

    /// Persist an n=1 commit whose tree keeps `sub/` with changed content.
    async fn wh03_save_n1_commit_sub(
        storage: &crate::jupiter::storage::Storage,
        parent_id: git_internal::hash::ObjectHash,
        sub_blob_hex: &str,
        msg: &str,
    ) -> (String, PushPayload) {
        use git_internal::internal::object::{commit::Commit, tree::Tree};

        let sub_v2 = Tree::from_tree_items(vec![wh03_blob_item("z.txt", sub_blob_hex)]).unwrap();
        let new_child = Tree::from_tree_items(vec![
            wh03_blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd"),
            git_internal::internal::object::tree::TreeItem::new(
                git_internal::internal::object::tree::TreeItemMode::Tree,
                sub_v2.id,
                "sub".to_string(),
            ),
        ])
        .unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![parent_id], msg);
        storage
            .mono_storage()
            .save_mega_trees(vec![sub_v2, new_child], new_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![new_commit.clone()], None)
            .await
            .unwrap();
        let new_id = new_commit.id.to_string();
        (
            new_id.clone(),
            PushPayload {
                commits: vec![new_id],
                fork_base: Some(parent_id.to_string()),
                n: 1,
            },
        )
    }

    async fn wh03_enqueue_push(
        storage: &crate::jupiter::storage::Storage,
        path: &str,
        old_id: &str,
        new_id: &str,
        payload: &PushPayload,
    ) -> i64 {
        let outcome = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(old_id, new_id),
                path: path.into(),
                old_id: old_id.into(),
                new_id: new_id.into(),
                requester: None,
                payload: payload.to_json(),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("insert push {outcome:?}");
        };
        assert_eq!(
            storage
                .push_queue_service
                .storage()
                .claim_for_execution(id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        id
    }

    async fn wh03_exec(storage: &crate::jupiter::storage::Storage, id: i64) -> ExecuteOutcome {
        let ctx = PushExecContext {
            storage: storage.clone(),
            git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
            pre_apply_enter_barrier: None,
            pre_apply_release_barrier: None,
        };
        storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
                Some(&ctx),
            )
            .await
            .unwrap()
    }

    /// Same as `wh03_exec`, with an explicit request override (force flags).
    async fn wh03_exec_with(
        storage: &crate::jupiter::storage::Storage,
        id: i64,
        force_conflict: bool,
    ) -> ExecuteOutcome {
        let ctx = PushExecContext {
            storage: storage.clone(),
            git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
            pre_apply_enter_barrier: None,
            pre_apply_release_barrier: None,
        };
        storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    force_conflict,
                    ..Default::default()
                },
                None,
                None,
                Some(&ctx),
            )
            .await
            .unwrap()
    }

    /// Attach rounds without an `AttachExecContext` terminalize as Failed.
    async fn wh03_exec_attach_no_ctx(
        storage: &crate::jupiter::storage::Storage,
        id: i64,
    ) -> ExecuteOutcome {
        storage
            .push_queue_service
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
            .unwrap()
    }

    async fn wh03_row_status(
        storage: &crate::jupiter::storage::Storage,
        id: i64,
    ) -> PushQueueStatusEnum {
        storage
            .push_queue_service
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .expect("row")
            .status
    }

    /// `try_emit` admits synchronously inside B3, so once the round returned,
    /// any accepted delivery lands within a bounded window.
    async fn wh03_wait_calls(transport: &RecordingTransport, n: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }

    /// Bounded settle window for the zero-delivery assertions.
    async fn wh03_assert_no_delivery(transport: &RecordingTransport, expected: usize) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            transport.calls(),
            expected,
            "no repo.push delivery expected (bodies: {:?})",
            transport.bodies()
        );
    }

    async fn wh03_shutdown(storage: &crate::jupiter::storage::Storage) {
        tokio::time::timeout(
            Duration::from_secs(3),
            storage.storage_event_emitter.shutdown(),
        )
        .await
        .expect("emitter shutdown within 3s");
    }

    #[tokio::test]
    async fn storage_event_commit_matrix() {
        let _lock = crate::ceres::pack::materialize::lock_materialize_tests().await;

        // Builder identity invariants (event_id derivation, unit-level):
        // stable for identical inputs; distinct when installation_id, repo,
        // operation or landed commit differ. The queue i64 id plays no role.
        {
            use crate::jupiter::service::storage_event_emitter::{RepoPushData, repo_push_event};
            let base = repo_push_event(
                WH03_INSTALLATION,
                "/wh03id",
                RepoPushData {
                    push_id: "7".to_owned(),
                    operation_id: "op-1".to_owned(),
                    ref_name: MEGA_BRANCH_NAME.to_owned(),
                    old_oid: "a".repeat(40),
                    requested_oid: "b".repeat(40),
                    landed_oid: "c".repeat(40),
                },
            )
            .expect("builder");
            let same = repo_push_event(
                WH03_INSTALLATION,
                "/wh03id",
                RepoPushData {
                    push_id: "8".to_owned(),
                    operation_id: "op-1".to_owned(),
                    ref_name: MEGA_BRANCH_NAME.to_owned(),
                    old_oid: "a".repeat(40),
                    requested_oid: "b".repeat(40),
                    landed_oid: "c".repeat(40),
                },
            )
            .expect("builder");
            assert_eq!(base.event_id, same.event_id);
            for (installation, path, op, landed) in [
                ("other-install", "/wh03id", "op-1", "c".repeat(40)),
                (WH03_INSTALLATION, "/wh03id/sub", "op-1", "c".repeat(40)),
                (WH03_INSTALLATION, "/wh03id", "op-2", "c".repeat(40)),
                (WH03_INSTALLATION, "/wh03id", "op-1", "d".repeat(40)),
            ] {
                let other = repo_push_event(
                    installation,
                    path,
                    RepoPushData {
                        push_id: "7".to_owned(),
                        operation_id: op.to_string(),
                        ref_name: MEGA_BRANCH_NAME.to_owned(),
                        old_oid: "a".repeat(40),
                        requested_oid: "b".repeat(40),
                        landed_oid: landed.clone(),
                    },
                )
                .expect("builder");
                assert_ne!(base.event_id, other.event_id);
            }
        }

        // Section 1: a successful n>0 push round delivers exactly one event.
        let transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_temp, storage) = wh03_storage(true, Arc::clone(&transport)).await;
        let (path_commit, path) = wh03_path_fixture(&storage, "wh03a").await;
        let old_id = path_commit.id.to_string();
        let (new_id, payload) = wh03_save_n1_commit(
            &storage,
            path_commit.id,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "n1",
        )
        .await;
        let id = wh03_enqueue_push(&storage, &path, &old_id, &new_id, &payload).await;
        let outcome = wh03_exec(&storage, id).await;
        assert_eq!(
            outcome,
            ExecuteOutcome::Done {
                id,
                landed_commit_id: new_id.clone(),
                root_cas_writes: 1,
            }
        );
        wh03_wait_calls(&transport, 1).await;
        assert_eq!(transport.calls(), 1);
        let body = &transport.bodies()[0];
        let envelope: serde_json::Value = serde_json::from_slice(body).expect("envelope json");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "repo.push");
        assert_eq!(envelope["source"], "git");
        assert_eq!(envelope["scope"]["repo_path"], path.as_str());
        assert!(envelope["scope"]["tenant_id"].is_null());
        assert!(envelope["scope"]["oci_repository"].is_null());
        let operation_id = push_operation_id(&old_id, &new_id);
        assert_eq!(envelope["data"]["push_id"], id.to_string());
        assert_eq!(envelope["data"]["operation_id"], operation_id);
        assert_eq!(envelope["data"]["ref_name"], MEGA_BRANCH_NAME);
        assert_eq!(envelope["data"]["old_oid"], old_id);
        assert_eq!(envelope["data"]["requested_oid"], new_id);
        assert_eq!(envelope["data"]["landed_oid"], new_id);
        assert!(envelope["occurred_at"].as_u64().unwrap() > 0);
        let expected = crate::jupiter::service::storage_event_emitter::repo_push_event(
            WH03_INSTALLATION,
            &path,
            crate::jupiter::service::storage_event_emitter::RepoPushData {
                push_id: id.to_string(),
                operation_id: operation_id.clone(),
                ref_name: MEGA_BRANCH_NAME.to_owned(),
                old_oid: old_id.clone(),
                requested_oid: new_id.clone(),
                landed_oid: new_id.clone(),
            },
        )
        .expect("builder");
        assert_eq!(envelope["event_id"], expected.event_id.to_string());

        // Section 2: replaying the Done operation_id delivers nothing more.
        let replay = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: operation_id.clone(),
                path: path.clone(),
                old_id: old_id.clone(),
                new_id: new_id.clone(),
                requester: None,
                payload: payload.to_json(),
                ref_name: Some(MEGA_BRANCH_NAME.into()),
                is_delete: false,
            })
            .await
            .unwrap();
        assert!(
            matches!(
                replay,
                EnqueueOutcome::Replay {
                    landed_commit_id: Some(ref landed),
                    ..
                } if *landed == new_id
            ),
            "Done replay expected, got {replay:?}"
        );
        wh03_assert_no_delivery(&transport, 1).await;

        // Section 3: an n=0 no-op round commits Done but delivers nothing.
        let n0_payload = PushPayload {
            commits: Vec::new(),
            fork_base: None,
            n: 0,
        };
        let n0_id = wh03_enqueue_push(&storage, &path, &new_id, &new_id, &n0_payload).await;
        let outcome = wh03_exec(&storage, n0_id).await;
        assert_eq!(
            outcome,
            ExecuteOutcome::Done {
                id: n0_id,
                landed_commit_id: new_id.clone(),
                root_cas_writes: 1,
            }
        );
        wh03_assert_no_delivery(&transport, 1).await;

        // Section 4: fencing ClaimLost delivers nothing.
        let tip = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        use std::str::FromStr;
        let tip_id = git_internal::hash::ObjectHash::from_str(&tip.ref_commit_hash).unwrap();
        let (lost_new, lost_payload) = wh03_save_n1_commit(
            &storage,
            tip_id,
            "2222222222222222222222222222222222222222",
            "claim lost",
        )
        .await;
        let lost_id = wh03_enqueue_push(
            &storage,
            &path,
            &tip.ref_commit_hash,
            &lost_new,
            &lost_payload,
        )
        .await;
        storage
            .push_queue_service
            .storage()
            .mark_failed_for_test(lost_id)
            .await
            .unwrap();
        let outcome = wh03_exec(&storage, lost_id).await;
        assert_eq!(outcome, ExecuteOutcome::ClaimLost { id: lost_id });
        wh03_assert_no_delivery(&transport, 1).await;

        // Section 5: a queue-external root writer trips the baseline/CAS
        // fail-close (BypassDetected); nothing is delivered. This hard-stops
        // the queue, so it is the last round on this storage. The apply-time
        // root CAS inside the real push executor compares against the
        // same-transaction snapshot under the held MonoWriteLock, so an
        // external writer cannot slip between the B3 read and the CAS — the
        // baseline fail-close exercised here is the reachable tripwire of the
        // same mark_failed+BypassDetected family.
        let (bypass_new, bypass_payload) = wh03_save_n1_commit(
            &storage,
            tip_id,
            "3333333333333333333333333333333333333333",
            "bypass",
        )
        .await;
        let bypass_id = wh03_enqueue_push(
            &storage,
            &path,
            &tip.ref_commit_hash,
            &bypass_new,
            &bypass_payload,
        )
        .await;
        {
            use sea_orm::{ConnectionTrait, Statement, Value};
            storage
                .mono_storage()
                .get_connection()
                .execute_raw(Statement::from_sql_and_values(
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
        }
        let outcome = wh03_exec(&storage, bypass_id).await;
        assert_eq!(outcome, ExecuteOutcome::BypassDetected { id: bypass_id });
        wh03_assert_no_delivery(&transport, 1).await;
        wh03_shutdown(&storage).await;
        // Drain-first final count: exactly the section-1 delivery, nothing late.
        assert_eq!(transport.calls(), 1, "no late delivery after drain");

        // Section 6 (AC5): storage-only but `[storage_events]` disabled — the
        // emitter drops before any delivery while the push still lands.
        let disabled_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t2, storage2) = wh03_storage(false, Arc::clone(&disabled_transport)).await;
        let (path_commit2, path2) = wh03_path_fixture(&storage2, "wh03b").await;
        let old_id2 = path_commit2.id.to_string();
        let (new_id2, payload2) = wh03_save_n1_commit(
            &storage2,
            path_commit2.id,
            "5555555555555555555555555555555555555555",
            "n1 disabled",
        )
        .await;
        let id2 = wh03_enqueue_push(&storage2, &path2, &old_id2, &new_id2, &payload2).await;
        let outcome = wh03_exec(&storage2, id2).await;
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        wh03_assert_no_delivery(&disabled_transport, 0).await;
        wh03_shutdown(&storage2).await;
        assert_eq!(
            disabled_transport.calls(),
            0,
            "no late delivery after drain"
        );

        // Section 7 (AC5): review morphology never emits — even with an
        // enabled emitter installed, merge rounds carry no repo.push hook and
        // the storage-only gate is closed (push enqueue is B0-rejected under
        // review; covered by b0_review_rejects_push_enqueue).
        let review_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let temp3 = tempfile::TempDir::new().unwrap();
        let mut review_config =
            crate::config::testing::isolated_config(temp3.path().join("config"));
        review_config.monorepo.push_policy = PushPolicy::Review;
        review_config.storage_events.enabled = true;
        review_config.storage_events.installation_id = Some(WH03_INSTALLATION.to_string());
        let mut review_storage =
            crate::jupiter::tests::test_storage_with_config(temp3.path(), review_config.clone())
                .await;
        review_storage
            .set_storage_event_emitter(wh03_emitter(&review_config, Arc::clone(&review_transport)));
        review_storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/",
                    MEGA_BRANCH_NAME.to_owned(),
                    "a".repeat(40),
                    "b".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let merge_round = review_storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-WH03".into(),
                path: "/wh03c".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: merge_id } = merge_round else {
            panic!("insert merge {merge_round:?}");
        };
        assert_eq!(
            review_storage
                .push_queue_service
                .storage()
                .claim_for_execution(merge_id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        let outcome = review_storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: merge_id,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        wh03_assert_no_delivery(&review_transport, 0).await;
        wh03_shutdown(&review_storage).await;
        assert_eq!(review_transport.calls(), 0, "no late delivery after drain");

        // Section 8 (AC6): a failing transport never changes the push result —
        // the round still lands Done and the tip advances.
        let failing_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: true,
        });
        let (_t4, storage4) = wh03_storage(true, Arc::clone(&failing_transport)).await;
        let (path_commit4, path4) = wh03_path_fixture(&storage4, "wh03d").await;
        let old_id4 = path_commit4.id.to_string();
        let (new_id4, payload4) = wh03_save_n1_commit(
            &storage4,
            path_commit4.id,
            "6666666666666666666666666666666666666666",
            "n1 failing transport",
        )
        .await;
        let id4 = wh03_enqueue_push(&storage4, &path4, &old_id4, &new_id4, &payload4).await;
        let outcome = wh03_exec(&storage4, id4).await;
        assert_eq!(
            outcome,
            ExecuteOutcome::Done {
                id: id4,
                landed_commit_id: new_id4.clone(),
                root_cas_writes: 1,
            }
        );
        wh03_wait_calls(&failing_transport, 1).await;
        let pref = storage4
            .mono_storage()
            .get_main_ref(&path4)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, new_id4);
        wh03_shutdown(&storage4).await;
        // Drain-first final count: no late delivery may appear after shutdown.
        assert_eq!(failing_transport.calls(), 1);

        // Section 9: a business failure inside the executor (here: the pushed
        // tip commit was never persisted) terminalizes the round as Failed
        // after rolling back; the tip does not move and nothing is delivered.
        let failure_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t5, storage5) = wh03_storage(true, Arc::clone(&failure_transport)).await;
        let (path_commit5, path5) = wh03_path_fixture(&storage5, "wh03e").await;
        let old_id5 = path_commit5.id.to_string();
        let missing_new = "7".repeat(40);
        let missing_payload = PushPayload {
            commits: vec![missing_new.clone()],
            fork_base: Some(old_id5.clone()),
            n: 1,
        };
        let id5 =
            wh03_enqueue_push(&storage5, &path5, &old_id5, &missing_new, &missing_payload).await;
        let outcome = wh03_exec(&storage5, id5).await;
        assert!(
            matches!(
                outcome,
                ExecuteOutcome::Failed { ref failure, .. } if failure == "PushFailure"
            ),
            "missing tip commit must terminalize as PushFailure, got {outcome:?}"
        );
        assert_eq!(
            wh03_row_status(&storage5, id5).await,
            PushQueueStatusEnum::Failed,
            "the round must be persisted Failed"
        );
        let pref5 = storage5
            .mono_storage()
            .get_main_ref(&path5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pref5.ref_commit_hash, old_id5,
            "a failed push must not move the tip"
        );
        wh03_assert_no_delivery(&failure_transport, 0).await;
        wh03_shutdown(&storage5).await;
        assert_eq!(failure_transport.calls(), 0, "no delivery after drain");

        // Section 10: a Conflict requeue re-arms the round and delivers
        // nothing for the requeued execution.
        let requeue_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t6, storage6) = wh03_storage(true, Arc::clone(&requeue_transport)).await;
        let (path_commit6, path6) = wh03_path_fixture(&storage6, "wh03f").await;
        let old_id6 = path_commit6.id.to_string();
        let (new_id6, payload6) = wh03_save_n1_commit(
            &storage6,
            path_commit6.id,
            "8888888888888888888888888888888888888888",
            "requeue round",
        )
        .await;
        let id6 = wh03_enqueue_push(&storage6, &path6, &old_id6, &new_id6, &payload6).await;
        let outcome = wh03_exec_with(&storage6, id6, true).await;
        let ExecuteOutcome::Requeued { successor_id, .. } = outcome else {
            panic!("forced conflict must requeue, got {outcome:?}");
        };
        assert_eq!(
            wh03_row_status(&storage6, successor_id).await,
            PushQueueStatusEnum::Queued,
            "the requeue successor must be persisted Queued"
        );
        wh03_assert_no_delivery(&requeue_transport, 0).await;
        wh03_shutdown(&storage6).await;
        assert_eq!(requeue_transport.calls(), 0, "no delivery after drain");

        // Section 11: attach rounds carry no repo.push hook — an attach round
        // terminalizing without its context still delivers nothing.
        let attach_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t7, storage7) = wh03_storage(true, Arc::clone(&attach_transport)).await;
        let attach_round = storage7
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: "attach-wh03".into(),
                path: "/wh03g".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id: attach_id } = attach_round else {
            panic!("insert attach {attach_round:?}");
        };
        assert_eq!(
            storage7
                .push_queue_service
                .storage()
                .claim_for_execution(attach_id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        let outcome = wh03_exec_attach_no_ctx(&storage7, attach_id).await;
        assert!(
            matches!(outcome, ExecuteOutcome::Failed { .. }),
            "attach without context must terminalize Failed, got {outcome:?}"
        );
        wh03_assert_no_delivery(&attach_transport, 0).await;
        wh03_shutdown(&storage7).await;
        assert_eq!(attach_transport.calls(), 0, "no delivery after drain");

        // Section 12 (AC4): a delivery blocked past a later round still
        // carries ITS OWN round's snapshot — the emitter never re-reads
        // latest state at send time.
        let blocking = BlockingRecordingTransport::new();
        let mut delayed_config = crate::config::testing::isolated_config(
            tempfile::tempdir().unwrap().path().join("config"),
        );
        delayed_config.monorepo.push_policy = PushPolicy::Trunk;
        delayed_config.git.push_auth = Some(crate::config::PushAuth::None);
        delayed_config.git.ssh_receive_pack = Some(false);
        delayed_config.storage_events.enabled = true;
        delayed_config.storage_events.installation_id = Some(WH03_INSTALLATION.to_string());
        delayed_config.storage_events.max_in_flight = 2;
        let temp8 = tempfile::TempDir::new().unwrap();
        let mut storage8 =
            crate::jupiter::tests::test_storage_with_config(temp8.path(), delayed_config.clone())
                .await;
        storage8.set_storage_event_emitter(
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &delayed_config,
                blocking.clone(),
                vec![wh03_compiled_target()],
            ),
        );
        let storage8 = crate::jupiter::tests::with_test_vault(storage8, temp8.path()).await;
        let (path_commit8, path8) = wh03_path_fixture(&storage8, "wh03h").await;

        // Round A lands Done while its send is still blocked in transport.
        let old_a = path_commit8.id.to_string();
        let (new_a, payload_a) = wh03_save_n1_commit(
            &storage8,
            path_commit8.id,
            "9999999999999999999999999999999999999999",
            "round A",
        )
        .await;
        let id_a = wh03_enqueue_push(&storage8, &path8, &old_a, &new_a, &payload_a).await;
        let outcome = wh03_exec(&storage8, id_a).await;
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        wh03_wait_calls_block(&blocking, 1).await;

        // Round B advances the same path while round A's send is in flight.
        let tip_a = storage8
            .mono_storage()
            .get_main_ref(&path8)
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash;
        assert_eq!(tip_a, new_a);
        let tip_a_id = git_internal::hash::ObjectHash::from_str(&tip_a).unwrap();
        let (new_b, payload_b) = wh03_save_n1_commit(
            &storage8,
            tip_a_id,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbbbbbb",
            "round B",
        )
        .await;
        let id_b = wh03_enqueue_push(&storage8, &path8, &new_a, &new_b, &payload_b).await;
        let outcome = wh03_exec(&storage8, id_b).await;
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        wh03_wait_calls_block(&blocking, 2).await;

        blocking.release();
        wh03_shutdown(&storage8).await;
        let bodies = blocking.bodies();
        assert_eq!(bodies.len(), 2, "exactly two deliveries after drain");
        let find = |landed: &str| {
            bodies
                .iter()
                .map(|body| serde_json::from_slice::<serde_json::Value>(body).expect("envelope"))
                .find(|env| env["data"]["landed_oid"] == landed)
                .unwrap_or_else(|| panic!("missing delivery for landed {landed}"))
        };
        let env_a = find(&new_a);
        assert_eq!(env_a["data"]["old_oid"], old_a);
        assert_eq!(env_a["data"]["requested_oid"], new_a);
        let env_b = find(&new_b);
        assert_eq!(env_b["data"]["old_oid"], new_a);
        assert_eq!(env_b["data"]["requested_oid"], new_b);
        assert_ne!(env_a["event_id"], env_b["event_id"]);

        // Section 13: a queue-bypassing writer moves the root row between the
        // baseline read and the apply-time CAS (the MonoWriteLock is advisory;
        // the pre_apply seam opens that window deterministically). The apply's
        // business writes roll back to the savepoint, the round fail-closes
        // with hard-stop, and nothing is delivered.
        let cas_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t9, storage9) = wh03_storage(true, Arc::clone(&cas_transport)).await;
        let (path_commit9, path9) = wh03_path_fixture(&storage9, "wh03i").await;
        let old_id9 = path_commit9.id.to_string();
        let (new_id9, payload9) = wh03_save_n1_commit(
            &storage9,
            path_commit9.id,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "cas race round",
        )
        .await;
        let id9 = wh03_enqueue_push(&storage9, &path9, &old_id9, &new_id9, &payload9).await;
        let root_before = storage9
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash;
        let enter_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let release_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let ctx = PushExecContext {
            storage: storage9.clone(),
            git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
            pre_apply_enter_barrier: Some(Arc::clone(&enter_barrier)),
            pre_apply_release_barrier: Some(Arc::clone(&release_barrier)),
        };
        let exec_storage = storage9.clone();
        let mut exec = tokio::spawn(async move {
            exec_storage
                .push_queue_service
                .execute_b3(
                    ExecuteRequest {
                        id: id9,
                        ..Default::default()
                    },
                    None,
                    None,
                    Some(&ctx),
                )
                .await
        });
        // Two-phase handshake: the executor signals the open window, the
        // bypassing writer commits its root-row update on a separate
        // connection, and only then releases the executor into the CAS. No
        // timing assumption: the ordering is explicit, every wait is bounded,
        // and any handshake failure aborts the executor with a bounded join
        // so the test can never hang or detach the task.
        let handshake = async {
            tokio::time::timeout(Duration::from_secs(5), enter_barrier.wait())
                .await
                .map_err(|_| "executor never reached the pre-apply window".to_string())?;
            {
                use sea_orm::{ConnectionTrait, Statement, Value};
                tokio::time::timeout(
                    Duration::from_secs(5),
                    storage9.mono_storage().get_connection().execute_raw(
                        Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE mega_refs SET ref_commit_hash = $1, ref_tree_hash = $2 WHERE path = '/' AND ref_name = $3",
                            [
                                Value::from("9".repeat(40)),
                                Value::from("8".repeat(40)),
                                Value::from(MEGA_BRANCH_NAME.to_owned()),
                            ],
                        ),
                    ),
                )
                .await
                .map_err(|_| "bypassing writer update stalled".to_string())?
                .map_err(|error| format!("bypassing writer update failed: {error}"))?;
            }
            tokio::time::timeout(Duration::from_secs(5), release_barrier.wait())
                .await
                .map_err(|_| "executor release stalled".to_string())
        };
        if let Err(error) = handshake.await {
            exec.abort();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut exec).await;
            panic!("CAS race handshake failed: {error}");
        }
        let outcome = match tokio::time::timeout(Duration::from_secs(10), &mut exec).await {
            Ok(joined) => joined.expect("executor task").expect("execute_b3 result"),
            Err(_) => {
                exec.abort();
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut exec).await;
                panic!("executor did not finish within the bound");
            }
        };
        assert_eq!(outcome, ExecuteOutcome::BypassDetected { id: id9 });
        // Rollback evidence: the failed apply's writes are gone — the path tip
        // is unmoved and the root row keeps the bypassing writer's values.
        let pref9 = storage9
            .mono_storage()
            .get_main_ref(&path9)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref9.ref_commit_hash, old_id9, "path tip must not move");
        let root9 = storage9
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root9.ref_commit_hash, "9".repeat(40));
        assert_ne!(root9.ref_commit_hash, root_before);
        // Hard-stop persisted as the queue control-plane state.
        let control = storage9
            .push_queue_service
            .storage()
            .get_control()
            .await
            .unwrap();
        assert!(
            control.hard_stopped,
            "CAS fail-close must hard-stop the queue"
        );
        wh03_assert_no_delivery(&cas_transport, 0).await;
        wh03_shutdown(&storage9).await;
        assert_eq!(cas_transport.calls(), 0, "no late delivery after drain");

        // Section 14: a real successful attach round lands its mount but
        // carries no repo.push hook (attach kind is excluded by design).
        let attach_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t10, storage10) = wh03_storage(true, Arc::clone(&attach_transport)).await;
        // Rewire the import service onto the real test connections (same
        // wiring as import_repo's wired_storage_with_monorepo) — the default
        // test storage builds it over a mock.
        let git_service = crate::jupiter::service::git_service::GitService {
            obj_storage: crate::jupiter::storage::object_storage::mock_object_storage(),
        };
        let mut storage10 = storage10;
        storage10.git_service = git_service.clone();
        storage10.mono_service = crate::jupiter::service::mono_service::MonoService {
            mono_storage: storage10.mono_storage(),
            git_service: git_service.clone(),
        };
        storage10.import_service = crate::jupiter::service::import_service::ImportService {
            git_db_storage: storage10.git_db_storage(),
            git_service,
        };
        // Attach needs the initialized monorepo root ref.
        storage10
            .mono_service
            .init_monorepo(&storage10.config().monorepo)
            .await
            .unwrap();
        let (repo, commit, command) =
            wh03_seed_import_repo(&storage10, "/third-party/wh03-attach").await;
        let root = storage10
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: repo.repo_id,
            repo_path: repo.repo_path.clone(),
            commands: vec![AttachCommand {
                ref_name: command.ref_name.clone(),
                old_id: command.old_id.clone(),
                new_id: command.new_id.clone(),
                command_type: "Create".into(),
                ref_type: "branch".into(),
                default_branch: command.default_branch,
            }],
        };
        let operation_id = attach_operation_id(
            &repo.repo_id.to_string(),
            &normalize_attach_commands(&[(
                command.ref_name.clone(),
                "Create".into(),
                command.old_id.clone(),
                commit.id.to_string(),
            )]),
        );
        let EnqueueOutcome::Inserted { id: attach_id } = storage10
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id,
                path: repo.repo_path.clone(),
                old_id: root.ref_commit_hash,
                new_id: commit.id.to_string(),
                requester: None,
                payload: serde_json::to_value(&payload).unwrap(),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        else {
            panic!("attach insert");
        };
        assert_eq!(
            storage10
                .push_queue_service
                .storage()
                .claim_for_execution(attach_id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        let attach_ctx = AttachExecContext {
            storage: storage10.clone(),
            git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
        };
        let outcome = storage10
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: attach_id,
                    ..Default::default()
                },
                Some(&attach_ctx),
                None,
                None,
            )
            .await
            .unwrap();
        let ExecuteOutcome::Done {
            landed_commit_id: attach_landed,
            ..
        } = outcome
        else {
            panic!("real attach must land Done, got {outcome:?}");
        };
        assert_eq!(
            wh03_row_status(&storage10, attach_id).await,
            PushQueueStatusEnum::Done,
            "the attach round must be persisted Done"
        );
        assert!(
            storage10
                .mono_storage()
                .get_commit_by_hash(&attach_landed)
                .await
                .unwrap()
                .is_some(),
            "the attach must persist its landed commit row"
        );
        wh03_assert_no_delivery(&attach_transport, 0).await;
        wh03_shutdown(&storage10).await;
        assert_eq!(attach_transport.calls(), 0, "no late delivery after drain");

        // Section 15: a failure AFTER apply's transactional writes (the
        // descendant continuation hits a corrupt stored commit hash) drops
        // the whole B3 txn — root CAS and path writes roll back, the row is
        // persisted Failed, and nothing is delivered.
        let post_transport = Arc::new(RecordingTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            bodies: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let (_t11, storage11) = wh03_storage(true, Arc::clone(&post_transport)).await;
        let (path_commit11, path11, sub_v1) = wh03_path_fixture_with_sub(&storage11, "wh03j").await;
        // A materialized descendant whose stored commit hash is not valid hex.
        storage11
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    format!("{path11}/sub"),
                    MEGA_BRANCH_NAME.to_owned(),
                    "not-a-valid-hex-hash".to_string(),
                    sub_v1.id.to_string(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let root_before11 = storage11
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash;
        let old_id11 = path_commit11.id.to_string();
        let (new_id11, payload11) = wh03_save_n1_commit_sub(
            &storage11,
            path_commit11.id,
            "0202020202020202020202020202020202020202",
            "post-write failure",
        )
        .await;
        let id11 = wh03_enqueue_push(&storage11, &path11, &old_id11, &new_id11, &payload11).await;
        let outcome = wh03_exec(&storage11, id11).await;
        assert!(
            matches!(outcome, ExecuteOutcome::Failed { .. }),
            "descendant continuation failure must terminalize Failed, got {outcome:?}"
        );
        assert_eq!(
            wh03_row_status(&storage11, id11).await,
            PushQueueStatusEnum::Failed
        );
        // Rollback evidence after transactional writes: the root CAS and the
        // path advance are both gone.
        let root_after11 = storage11
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash;
        assert_eq!(root_after11, root_before11, "root CAS must roll back");
        let pref11 = storage11
            .mono_storage()
            .get_main_ref(&path11)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref11.ref_commit_hash, old_id11, "path tip must not move");
        wh03_assert_no_delivery(&post_transport, 0).await;
        wh03_shutdown(&storage11).await;
        assert_eq!(post_transport.calls(), 0, "no late delivery after drain");
    }

    /// Minimal import-repo fixture for the attach case (mirrors
    /// `import_repo::tests::seed_import_repo_with_main_tip`).
    async fn wh03_seed_import_repo(
        storage: &crate::jupiter::storage::Storage,
        path: &str,
    ) -> (
        crate::ceres::protocol::repo::Repo,
        git_internal::internal::object::commit::Commit,
        crate::ceres::protocol::import_refs::RefCommand,
    ) {
        use git_internal::internal::{
            metadata::{EntryMeta, MetaAttached},
            object::{
                blob::Blob,
                commit::Commit,
                tree::{Tree, TreeItem, TreeItemMode},
            },
        };

        use crate::ceres::protocol::{import_refs::RefCommand, repo::Repo};

        let repo = Repo::new(std::path::PathBuf::from(path), false).unwrap();
        let repo_id = repo.repo_id;
        let readme = Blob::from_content("wh03 attach readme");
        let tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: readme.id,
            name: "README.md".to_string(),
        }])
        .unwrap();
        let commit = Commit::from_tree_id(tree.id, vec![], "wh03 import commit");
        storage
            .git_db_storage()
            .register_import_repo(repo.clone().into())
            .await
            .unwrap();
        storage
            .import_service
            .save_entry(
                repo_id,
                &repo.repo_path,
                vec![
                    MetaAttached {
                        inner: readme.into(),
                        meta: EntryMeta::new(),
                    },
                    MetaAttached {
                        inner: tree.into(),
                        meta: EntryMeta::new(),
                    },
                    MetaAttached {
                        inner: commit.clone().into(),
                        meta: EntryMeta::new(),
                    },
                ],
            )
            .await
            .unwrap();
        let mut command = RefCommand::new(
            ZERO_ID.to_string(),
            commit.id.to_string(),
            "refs/heads/main".to_string(),
        );
        command.default_branch = true;
        (repo, commit, command)
    }

    /// Blocking recording transport for the AC4 delayed-send case: records the
    /// exact body when the post is called, then holds the future until released.
    struct BlockingRecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<bytes::Bytes>>,
        release: tokio::sync::watch::Sender<bool>,
    }

    impl BlockingRecordingTransport {
        fn new() -> Arc<Self> {
            let (release, _) = tokio::sync::watch::channel(false);
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                bodies: std::sync::Mutex::new(Vec::new()),
                release,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<bytes::Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }

        fn release(&self) {
            let _ = self.release.send(true);
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport
        for BlockingRecordingTransport
    {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: bytes::Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            let mut release = self.release.subscribe();
            Box::pin(async move {
                if !*release.borrow() {
                    let _ = release.changed().await;
                }
                Ok(
                crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                    status: 200,
                },
            )
            })
        }
    }

    // plan-20260923 FU-12: ImportRepo branch commands apply with receive-pack
    // CAS inside the B3 transaction; one stale command refuses the batch.

    fn fu12_ref(repo_id: i64, name: &str, git_id: &str) -> crate::callisto::import_refs::Model {
        crate::callisto::import_refs::Model {
            id: crate::common::utils::generate_id(),
            repo_id,
            ref_name: name.to_owned(),
            ref_git_id: git_id.to_owned(),
            ref_type: crate::callisto::sea_orm_active_enums::RefTypeEnum::Branch,
            default_branch: name == "refs/heads/main",
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn fu12_cmd(name: &str, command_type: &str, old_id: &str, new_id: &str) -> AttachCommand {
        AttachCommand {
            ref_name: name.to_owned(),
            old_id: old_id.to_owned(),
            new_id: new_id.to_owned(),
            command_type: command_type.to_owned(),
            ref_type: "branch".to_owned(),
            default_branch: false,
        }
    }

    async fn fu12_ref_id(git_db: &GitDbStorage, repo_id: i64, name: &str) -> Option<String> {
        git_db
            .get_ref(repo_id)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.ref_name == name)
            .map(|r| r.ref_git_id)
    }

    #[tokio::test]
    async fn fu12_branch_update_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let git_db = storage.git_db_storage();
        let repo_id = crate::common::utils::generate_id();
        let (current, stale) = ("1".repeat(40), "2".repeat(40));
        git_db
            .save_ref(repo_id, fu12_ref(repo_id, "refs/heads/main", &current))
            .await
            .unwrap();
        let txn = git_db.get_connection().begin().await.unwrap();
        let cmds = [fu12_cmd(
            "refs/heads/main",
            "Update",
            &stale,
            &"3".repeat(40),
        )];
        let result = apply_import_branch_commands_in_txn(&git_db, repo_id, &cmds, &txn)
            .await
            .unwrap();
        assert_eq!(
            result,
            Err(ImportRepoError::StaleRef {
                ref_name: "refs/heads/main".to_owned(),
                expected: stale,
            })
        );
        txn.commit().await.unwrap();
        assert_eq!(
            fu12_ref_id(&git_db, repo_id, "refs/heads/main").await,
            Some(current)
        );
    }

    #[tokio::test]
    async fn fu12_branch_create_existing() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let git_db = storage.git_db_storage();
        let repo_id = crate::common::utils::generate_id();
        let current = "1".repeat(40);
        git_db
            .save_ref(repo_id, fu12_ref(repo_id, "refs/heads/main", &current))
            .await
            .unwrap();
        let txn = git_db.get_connection().begin().await.unwrap();
        let cmds = [fu12_cmd(
            "refs/heads/main",
            "Create",
            ZERO_ID,
            &"3".repeat(40),
        )];
        let result = apply_import_branch_commands_in_txn(&git_db, repo_id, &cmds, &txn)
            .await
            .unwrap();
        assert!(
            matches!(result, Err(ImportRepoError::StaleRef { .. })),
            "{result:?}"
        );
        // ON CONFLICT DO NOTHING leaves the transaction usable.
        let refs = git_db.list_branch_refs_in_txn(repo_id, &txn).await.unwrap();
        assert_eq!(refs.len(), 1);
        txn.commit().await.unwrap();
        assert_eq!(
            fu12_ref_id(&git_db, repo_id, "refs/heads/main").await,
            Some(current)
        );
    }

    #[tokio::test]
    async fn fu12_branch_batch_atomic() {
        use sea_orm::{EntityTrait, PaginatorTrait};

        let temp = tempfile::TempDir::new().unwrap();
        let mut storage = crate::jupiter::tests::test_storage(temp.path()).await;
        // Same wiring as import_repo's `wired_storage_with_monorepo`.
        let git_service = crate::jupiter::service::git_service::GitService {
            obj_storage: crate::jupiter::storage::object_storage::mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = crate::jupiter::service::mono_service::MonoService {
            mono_storage: storage.mono_storage(),
            git_service: git_service.clone(),
        };
        storage.import_service = crate::jupiter::service::import_service::ImportService {
            git_db_storage: storage.git_db_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .unwrap();
        let (repo, commit, _create) =
            wh03_seed_import_repo(&storage, "/third-party/fu12-batch").await;
        let git_db = storage.git_db_storage();
        let (main_old, dev_old, stale) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
        git_db
            .save_ref(
                repo.repo_id,
                fu12_ref(repo.repo_id, "refs/heads/main", &main_old),
            )
            .await
            .unwrap();
        git_db
            .save_ref(
                repo.repo_id,
                fu12_ref(repo.repo_id, "refs/heads/dev", &dev_old),
            )
            .await
            .unwrap();
        let tip = commit.id.to_string();
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: std::sync::Arc::new(
                crate::ceres::api_service::cache::GitObjectCache {
                    connection: crate::jupiter::tests::test_redis_manager().await,
                    prefix: String::new(),
                },
            ),
        };
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        // main's CAS holds, dev's does not (stale Update, then stale Delete):
        // neither branch may move, and no `.gitkeep` blob is written.
        let batches = [
            vec![
                fu12_cmd("refs/heads/main", "Update", &main_old, &tip),
                fu12_cmd("refs/heads/dev", "Update", &stale, &tip),
            ],
            vec![
                fu12_cmd("refs/heads/main", "Update", &main_old, &tip),
                fu12_cmd("refs/heads/dev", "Delete", &stale, ZERO_ID),
            ],
        ];
        for commands in batches {
            let payload = AttachPayload {
                op: AttachOp::Attach,
                repo_id: repo.repo_id,
                repo_path: repo.repo_path.clone(),
                commands,
            };
            let blobs_before = crate::callisto::mega_blob::Entity::find()
                .count(storage.mono_storage().get_connection())
                .await
                .unwrap();
            let EnqueueOutcome::Inserted { id } = storage
                .push_queue_service
                .enqueue(EnqueueRequest {
                    kind: PushQueueKindEnum::Attach,
                    operation_id: attach_operation_id(
                        &repo.repo_id.to_string(),
                        &payload.normalize_fingerprint_input(),
                    ),
                    path: repo.repo_path.clone(),
                    old_id: root.ref_commit_hash.clone(),
                    new_id: tip.clone(),
                    requester: None,
                    payload: serde_json::to_value(&payload).unwrap(),
                    ref_name: None,
                    is_delete: false,
                })
                .await
                .unwrap()
            else {
                panic!("attach insert");
            };
            assert_eq!(
                storage
                    .push_queue_service
                    .storage()
                    .claim_for_execution(id)
                    .await
                    .unwrap(),
                ClaimOutcome::Claimed
            );
            let outcome = storage
                .push_queue_service
                .execute_b3(
                    ExecuteRequest {
                        id,
                        ..Default::default()
                    },
                    Some(&ctx),
                    None,
                    None,
                )
                .await
                .unwrap();
            let ExecuteOutcome::Failed {
                failure, message, ..
            } = outcome
            else {
                panic!("stale batch must fail, got {outcome:?}");
            };
            assert_eq!(failure, "AttachFailure");
            assert!(
                message.starts_with("IMPORT_REPO_STALE_REF: \"refs/heads/dev\""),
                "{message}"
            );
            assert_eq!(
                wh03_row_status(&storage, id).await,
                PushQueueStatusEnum::Failed
            );
            assert_eq!(
                fu12_ref_id(&git_db, repo.repo_id, "refs/heads/main").await,
                Some(main_old.clone())
            );
            assert_eq!(
                fu12_ref_id(&git_db, repo.repo_id, "refs/heads/dev").await,
                Some(dev_old.clone())
            );
            let blobs_after = crate::callisto::mega_blob::Entity::find()
                .count(storage.mono_storage().get_connection())
                .await
                .unwrap();
            assert_eq!(
                blobs_after, blobs_before,
                "no .gitkeep blob on a stale batch"
            );
            let root_after = storage
                .mono_storage()
                .get_main_ref("/")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(root_after.ref_commit_hash, root.ref_commit_hash);
            assert_eq!(root_after.ref_tree_hash, root.ref_tree_hash);
        }
    }

    // plan-20260923 FU-13: a mounted ImportRepo takes further pushes (refs
    // only, root untouched); the first mount records provenance in the same
    // transaction; anything else at the leaf is IMPORT_REPO_PATH_OCCUPIED.

    async fn fu13_storage(temp: &std::path::Path) -> crate::jupiter::storage::Storage {
        let mut storage = crate::jupiter::tests::test_storage(temp).await;
        let git_service = crate::jupiter::service::git_service::GitService {
            obj_storage: crate::jupiter::storage::object_storage::mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = crate::jupiter::service::mono_service::MonoService {
            mono_storage: storage.mono_storage(),
            git_service: git_service.clone(),
        };
        storage.import_service = crate::jupiter::service::import_service::ImportService {
            git_db_storage: storage.git_db_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .unwrap();
        storage
    }

    async fn fu13_attach(
        storage: &crate::jupiter::storage::Storage,
        repo: &crate::ceres::protocol::repo::Repo,
        commands: Vec<AttachCommand>,
        tip: &str,
    ) -> ExecuteOutcome {
        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: repo.repo_id,
            repo_path: repo.repo_path.clone(),
            commands,
        };
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let EnqueueOutcome::Inserted { id } = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: attach_operation_id(
                    &repo.repo_id.to_string(),
                    &payload.normalize_fingerprint_input(),
                ),
                path: repo.repo_path.clone(),
                old_id: root.ref_commit_hash,
                new_id: tip.to_owned(),
                requester: None,
                payload: serde_json::to_value(&payload).unwrap(),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        else {
            panic!("attach insert");
        };
        assert_eq!(
            storage
                .push_queue_service
                .storage()
                .claim_for_execution(id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: std::sync::Arc::new(
                crate::ceres::api_service::cache::GitObjectCache {
                    connection: crate::jupiter::tests::test_redis_manager().await,
                    prefix: String::new(),
                },
            ),
        };
        storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap()
    }

    /// A child of `parent` (same tree) stored in the ImportRepo tables.
    async fn fu13_next_commit(
        storage: &crate::jupiter::storage::Storage,
        repo: &crate::ceres::protocol::repo::Repo,
        parent: &git_internal::internal::object::commit::Commit,
        message: &str,
    ) -> git_internal::internal::object::commit::Commit {
        use git_internal::internal::{
            metadata::{EntryMeta, MetaAttached},
            object::commit::Commit,
        };
        let commit = Commit::from_tree_id(parent.tree_id, vec![parent.id], message);
        storage
            .import_service
            .save_entry(
                repo.repo_id,
                &repo.repo_path,
                vec![MetaAttached {
                    inner: commit.clone().into(),
                    meta: EntryMeta::new(),
                }],
            )
            .await
            .unwrap();
        commit
    }

    async fn fu13_has_provenance(
        storage: &crate::jupiter::storage::Storage,
        repo_id: i64,
        path: &str,
    ) -> bool {
        AuditStorage::has_import_repo_attach_in_txn(
            storage.mono_storage().get_connection(),
            repo_id,
            path,
        )
        .await
        .unwrap()
    }

    async fn fu13_root(storage: &crate::jupiter::storage::Storage) -> (String, String) {
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        (root.ref_commit_hash, root.ref_tree_hash)
    }

    #[tokio::test]
    async fn fu13_update_keeps_root_ref() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-update").await;
        let c1_id = c1.id.to_string();
        let first = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1_id)],
            &c1_id,
        )
        .await;
        assert!(
            matches!(
                first,
                ExecuteOutcome::Done {
                    root_cas_writes: 1,
                    ..
                }
            ),
            "{first:?}"
        );
        let root = fu13_root(&storage).await;

        let c2 = fu13_next_commit(&storage, &repo, &c1, "fu13 second").await;
        let c2_id = c2.id.to_string();
        let second = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Update", &c1_id, &c2_id)],
            &c2_id,
        )
        .await;
        let ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes,
            ..
        } = second
        else {
            panic!("update push must land, got {second:?}");
        };
        assert_eq!(root_cas_writes, 0, "an update does not rewrite the root");
        assert_eq!(landed_commit_id, root.0);
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&storage.git_db_storage(), repo.repo_id, "refs/heads/main").await,
            Some(c2_id.clone())
        );

        // A nested child mounted under the leaf turns it into a directory; the
        // parent's record still claims it, so the parent keeps updating.
        let (child, k1, _) =
            wh03_seed_import_repo(&storage, "/third-party/fu13-update/child").await;
        let k1_id = k1.id.to_string();
        let nested = fu13_attach(
            &storage,
            &child,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &k1_id)],
            &k1_id,
        )
        .await;
        assert!(matches!(nested, ExecuteOutcome::Done { .. }), "{nested:?}");
        let root = fu13_root(&storage).await;
        let c3 = fu13_next_commit(&storage, &repo, &c2, "fu13 third").await;
        let c3_id = c3.id.to_string();
        let third = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Update", &c2_id, &c3_id)],
            &c3_id,
        )
        .await;
        assert!(
            matches!(
                third,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{third:?}"
        );
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&storage.git_db_storage(), repo.repo_id, "refs/heads/main").await,
            Some(c3_id)
        );
    }

    #[tokio::test]
    async fn fu13_first_attach_records_provenance() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-prov").await;
        let c1_id = c1.id.to_string();
        assert!(!fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);
        let first = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1_id)],
            &c1_id,
        )
        .await;
        assert!(matches!(first, ExecuteOutcome::Done { .. }), "{first:?}");
        assert!(fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);

        // A first mount refused after the provenance write (here: a stale
        // branch in the batch) rolls the record back with the mount.
        let (other, c2, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-prov-stale").await;
        let c2_id = c2.id.to_string();
        let root = fu13_root(&storage).await;
        let refused = fu13_attach(
            &storage,
            &other,
            vec![
                fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c2_id),
                fu12_cmd("refs/heads/dev", "Update", &"3".repeat(40), &c2_id),
            ],
            &c2_id,
        )
        .await;
        assert!(
            matches!(&refused, ExecuteOutcome::Failed { message, .. }
                if message.starts_with("IMPORT_REPO_STALE_REF: ")),
            "{refused:?}"
        );
        assert!(!fu13_has_provenance(&storage, other.repo_id, &other.repo_path).await);
        assert_eq!(fu13_root(&storage).await, root, "no mount either");
    }

    /// Put a directory holding a README (not a mount placeholder), or a file
    /// when `as_file`, at `/<parent>/<name>` in the monorepo root.
    async fn fu13_plant(
        storage: &crate::jupiter::storage::Storage,
        parent: &str,
        name: &str,
        as_file: bool,
    ) {
        use std::str::FromStr;

        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                commit::Commit,
                tree::{Tree, TreeItem, TreeItemMode},
            },
        };

        use crate::jupiter::utils::converter::FromMegaModel;

        let mono = storage.mono_storage();
        let root_ref = mono.get_main_ref("/").await.unwrap().unwrap();
        let root = Tree::from_mega_model(
            mono.get_tree_by_hash(&root_ref.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        let parent_item = root
            .tree_items
            .iter()
            .find(|item| item.name == parent)
            .unwrap()
            .clone();
        let parent_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&parent_item.id.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        let leaf = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(&"a".repeat(40)).unwrap(),
            "README.md".to_owned(),
        )])
        .unwrap();
        let mut items = parent_tree.tree_items.clone();
        if as_file {
            items.push(TreeItem::new(
                TreeItemMode::Blob,
                ObjectHash::from_str(&"b".repeat(40)).unwrap(),
                name.to_owned(),
            ));
        } else {
            items.push(TreeItem::new(TreeItemMode::Tree, leaf.id, name.to_owned()));
        }
        let new_parent = Tree::from_tree_items(items).unwrap();
        let mut root_items: Vec<TreeItem> = root
            .tree_items
            .iter()
            .filter(|item| item.name != parent)
            .cloned()
            .collect();
        root_items.push(TreeItem::new(
            TreeItemMode::Tree,
            new_parent.id,
            parent.to_owned(),
        ));
        let new_root = Tree::from_tree_items(root_items).unwrap();
        let commit = Commit::from_tree_id(
            new_root.id,
            vec![ObjectHash::from_str(&root_ref.ref_commit_hash).unwrap()],
            "fu13 plant directory",
        );
        mono.save_mega_trees(vec![leaf, new_parent, new_root.clone()], commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .unwrap();
        let mut updated = root_ref;
        updated.ref_commit_hash = commit.id.to_string();
        updated.ref_tree_hash = new_root.id.to_string();
        mono.update_ref(updated, None).await.unwrap();
    }

    #[tokio::test]
    async fn fu13_directory_collision_occupied() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        fu13_plant(&storage, "third-party", "fu13-occ", false).await;
        let root = fu13_root(&storage).await;
        let (repo, _c1, create) = wh03_seed_import_repo(&storage, "/third-party/fu13-occ").await;
        let import_repo = crate::ceres::pack::import_repo::ImportRepo {
            storage: storage.clone(),
            repo: repo.clone(),
            command_list: std::sync::Mutex::new(vec![create]),
            git_object_cache: std::sync::Arc::new(
                crate::ceres::api_service::cache::GitObjectCache {
                    connection: crate::jupiter::tests::test_redis_manager().await,
                    prefix: String::new(),
                },
            ),
            receive_pack_extra_timings_ms: std::sync::Mutex::new(vec![]),
        };
        let err = import_repo
            .attach_to_monorepo_parent()
            .await
            .expect_err("an ordinary directory is not this ImportRepo's mount");
        let text = err.to_string();
        assert!(
            text.starts_with("IMPORT_REPO_PATH_OCCUPIED: \"/third-party/fu13-occ\""),
            "{text}"
        );
        assert!(!text.contains("cannot attach leaf"), "{text}");
        assert!(!text.contains("Failed { id"), "{text}");
        assert_eq!(fu13_root(&storage).await, root, "root tree unchanged");
        assert!(!fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);

        // A file at the leaf is occupied too.
        fu13_plant(&storage, "third-party", "fu13-file", true).await;
        let root = fu13_root(&storage).await;
        let (file_repo, f1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-file").await;
        let f1_id = f1.id.to_string();
        let refused = fu13_attach(
            &storage,
            &file_repo,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &f1_id)],
            &f1_id,
        )
        .await;
        assert!(
            matches!(&refused, ExecuteOutcome::Failed { message, .. }
                if message.starts_with("IMPORT_REPO_PATH_OCCUPIED: \"/third-party/fu13-file\"")),
            "{refused:?}"
        );
        assert_eq!(fu13_root(&storage).await, root, "root tree unchanged");
    }

    #[tokio::test]
    async fn fu13_legacy_mount_backfilled() {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let conn = storage.mono_storage().get_connection().clone();
        let forget = |repo_id: i64| {
            crate::callisto::audit_logs::Entity::delete_many()
                .filter(crate::callisto::audit_logs::Column::TargetId.eq(repo_id))
                .exec(&conn)
        };

        // A mount made before FU-13: `.gitkeep`-only leaf, branches, no record.
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-legacy").await;
        let c1_id = c1.id.to_string();
        let first = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1_id)],
            &c1_id,
        )
        .await;
        assert!(matches!(first, ExecuteOutcome::Done { .. }), "{first:?}");
        forget(repo.repo_id).await.unwrap();
        assert!(!fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);
        let root = fu13_root(&storage).await;
        let c2 = fu13_next_commit(&storage, &repo, &c1, "fu13 legacy second").await;
        let c2_id = c2.id.to_string();
        let update = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Update", &c1_id, &c2_id)],
            &c2_id,
        )
        .await;
        assert!(
            matches!(
                update,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{update:?}"
        );
        assert!(fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);
        assert_eq!(fu13_root(&storage).await, root);

        // Without branches the `.gitkeep`-only leaf is not claimed.
        let (bare, b1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-bare").await;
        let b1_id = b1.id.to_string();
        let mounted = fu13_attach(
            &storage,
            &bare,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &b1_id)],
            &b1_id,
        )
        .await;
        assert!(
            matches!(mounted, ExecuteOutcome::Done { .. }),
            "{mounted:?}"
        );
        forget(bare.repo_id).await.unwrap();
        crate::callisto::import_refs::Entity::delete_many()
            .filter(crate::callisto::import_refs::Column::RepoId.eq(bare.repo_id))
            .exec(&conn)
            .await
            .unwrap();
        let root_before_bare = fu13_root(&storage).await;
        let refused = fu13_attach(
            &storage,
            &bare,
            vec![fu12_cmd("refs/heads/again", "Create", ZERO_ID, &b1_id)],
            &b1_id,
        )
        .await;
        assert!(
            matches!(&refused, ExecuteOutcome::Failed { message, .. }
                if message.starts_with("IMPORT_REPO_PATH_OCCUPIED: ")),
            "{refused:?}"
        );
        let root_now = fu13_root(&storage).await;
        assert_eq!(
            root_now, root_before_bare,
            "the refusal leaves the root alone"
        );

        // Only a `.gitkeep`-only leaf is claimed by the legacy rule: an
        // ordinary directory stays occupied even for a repository with
        // branches.
        fu13_plant(&storage, "third-party", "fu13-legacy-dir", false).await;
        let root_planted = fu13_root(&storage).await;
        assert_ne!(root_planted, root_now);
        let (dir_repo, d1, _) =
            wh03_seed_import_repo(&storage, "/third-party/fu13-legacy-dir").await;
        let d1_id = d1.id.to_string();
        storage
            .git_db_storage()
            .save_ref(
                dir_repo.repo_id,
                fu12_ref(dir_repo.repo_id, "refs/heads/main", &d1_id),
            )
            .await
            .unwrap();
        let d2 = fu13_next_commit(&storage, &dir_repo, &d1, "fu13 dir second").await;
        let d2_id = d2.id.to_string();
        let occupied = fu13_attach(
            &storage,
            &dir_repo,
            vec![fu12_cmd("refs/heads/main", "Update", &d1_id, &d2_id)],
            &d2_id,
        )
        .await;
        assert!(
            matches!(&occupied, ExecuteOutcome::Failed { message, .. }
                if message.starts_with("IMPORT_REPO_PATH_OCCUPIED: ")),
            "{occupied:?}"
        );
        assert_eq!(fu13_root(&storage).await, root_planted);
        assert!(!fu13_has_provenance(&storage, dir_repo.repo_id, &dir_repo.repo_path).await);
    }

    #[tokio::test]
    async fn fu13_delete_only_batch_refs_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-delete").await;
        let c1_id = c1.id.to_string();
        let mounted = fu13_attach(
            &storage,
            &repo,
            vec![
                fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1_id),
                fu12_cmd("refs/heads/dev", "Create", ZERO_ID, &c1_id),
            ],
            &c1_id,
        )
        .await;
        assert!(
            matches!(mounted, ExecuteOutcome::Done { .. }),
            "{mounted:?}"
        );
        let root = fu13_root(&storage).await;
        // A delete-only batch on an owned mount: refs only, root untouched.
        let deleted = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/dev", "Delete", &c1_id, ZERO_ID)],
            ZERO_ID,
        )
        .await;
        assert!(
            matches!(
                deleted,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{deleted:?}"
        );
        assert_eq!(fu13_root(&storage).await, root);
        let git_db = storage.git_db_storage();
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/dev").await,
            None
        );
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/main").await,
            Some(c1_id)
        );

        // Deletes never touch the mount, so no ownership gate: a directory
        // leaf without a record (e.g. a legacy parent with a nested child)
        // still deletes, and the root stays put.
        fu13_plant(&storage, "third-party", "fu13-delete-occ", false).await;
        let root = fu13_root(&storage).await;
        let (occ, d1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-delete-occ").await;
        let d1_id = d1.id.to_string();
        git_db
            .save_ref(
                occ.repo_id,
                fu12_ref(occ.repo_id, "refs/heads/main", &d1_id),
            )
            .await
            .unwrap();
        let applied = fu13_attach(
            &storage,
            &occ,
            vec![fu12_cmd("refs/heads/main", "Delete", &d1_id, ZERO_ID)],
            ZERO_ID,
        )
        .await;
        assert!(
            matches!(
                applied,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{applied:?}"
        );
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&git_db, occ.repo_id, "refs/heads/main").await,
            None
        );
        assert!(!fu13_has_provenance(&storage, occ.repo_id, &occ.repo_path).await);
    }

    #[tokio::test]
    async fn fu13_delete_only_sha256_zero_id() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        fu13_plant(&storage, "third-party", "fu13-delete-256", false).await;
        let root = fu13_root(&storage).await;
        let (occ, d1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-delete-256").await;
        let d1_id = d1.id.to_string();
        let git_db = storage.git_db_storage();
        git_db
            .save_ref(
                occ.repo_id,
                fu12_ref(occ.repo_id, "refs/heads/main", &d1_id),
            )
            .await
            .unwrap();
        // A SHA-256 delete carries 64 zeroes; it is still a delete-only batch,
        // so the unrecorded directory leaf is not refused as occupied.
        let applied = fu13_attach(
            &storage,
            &occ,
            vec![fu12_cmd(
                "refs/heads/main",
                "Delete",
                &d1_id,
                &"0".repeat(64),
            )],
            ZERO_ID,
        )
        .await;
        assert!(
            matches!(
                applied,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{applied:?}"
        );
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&git_db, occ.repo_id, "refs/heads/main").await,
            None
        );
        assert!(!fu13_has_provenance(&storage, occ.repo_id, &occ.repo_path).await);
    }

    #[tokio::test]
    async fn fu13_delete_only_skips_materialization_precheck() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-mat").await;
        let c1_id = c1.id.to_string();
        let git_db = storage.git_db_storage();
        for name in ["refs/heads/main", "refs/heads/dev"] {
            git_db
                .save_ref(repo.repo_id, fu12_ref(repo.repo_id, name, &c1_id))
                .await
                .unwrap();
        }
        // An ancestor with a materialized main ref (DEFER-FU-13/20 shape).
        storage
            .mono_storage()
            .save_refs(
                crate::callisto::mega_refs::Model::new(
                    "/third-party",
                    MEGA_BRANCH_NAME.to_owned(),
                    "a".repeat(40),
                    "b".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let root = fu13_root(&storage).await;
        let deleted = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/dev", "Delete", &c1_id, ZERO_ID)],
            ZERO_ID,
        )
        .await;
        assert!(
            matches!(
                deleted,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{deleted:?}"
        );
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/dev").await,
            None
        );
        // A batch that mounts is still refused by the precheck.
        let updated = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/topic", "Create", ZERO_ID, &c1_id)],
            &c1_id,
        )
        .await;
        let ExecuteOutcome::Failed { message, .. } = updated else {
            panic!("{updated:?}");
        };
        assert!(message.contains("(I3)"), "{message}");
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/topic").await,
            None
        );
    }

    #[tokio::test]
    async fn fu13_first_mount_tip_skips_sha256_delete() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1, _) = wh03_seed_import_repo(&storage, "/third-party/fu13-tip-256").await;
        let c1_id = c1.id.to_string();
        let git_db = storage.git_db_storage();
        git_db
            .save_ref(
                repo.repo_id,
                fu12_ref(repo.repo_id, "refs/heads/old", &c1_id),
            )
            .await
            .unwrap();
        let root = fu13_root(&storage).await;
        // The tip is the first branch command that is not a delete: a
        // leading 64-zero delete is skipped, not looked up as a commit.
        let mounted = fu13_attach(
            &storage,
            &repo,
            vec![
                fu12_cmd("refs/heads/old", "Delete", &c1_id, &"0".repeat(64)),
                fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1_id),
            ],
            &c1_id,
        )
        .await;
        assert!(
            matches!(mounted, ExecuteOutcome::Done { .. }),
            "{mounted:?}"
        );
        assert_ne!(fu13_root(&storage).await, root, "the mount lands");
        assert!(fu13_has_provenance(&storage, repo.repo_id, &repo.repo_path).await);
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/old").await,
            None
        );
        assert_eq!(
            fu12_ref_id(&git_db, repo.repo_id, "refs/heads/main").await,
            Some(c1_id)
        );
    }

    /// Register `path` as an ImportRepo (`git_repo` row plus one commit), the
    /// way the protocol dispatch does before the first attach.
    async fn fu16_register(
        storage: &crate::jupiter::storage::Storage,
        path: &str,
    ) -> (crate::ceres::protocol::repo::Repo, String) {
        let (repo, c1, _) = wh03_seed_import_repo(storage, path).await;
        (repo, c1.id.to_string())
    }

    async fn fu16_mount(
        storage: &crate::jupiter::storage::Storage,
        path: &str,
    ) -> (crate::ceres::protocol::repo::Repo, String) {
        let (repo, c1) = fu16_register(storage, path).await;
        let mounted = fu13_attach(
            storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &c1)],
            &c1,
        )
        .await;
        assert!(
            matches!(mounted, ExecuteOutcome::Done { .. }),
            "{mounted:?}"
        );
        (repo, c1)
    }

    async fn fu16_enqueue_detach(
        storage: &crate::jupiter::storage::Storage,
        repo: &crate::ceres::protocol::repo::Repo,
        requester: Option<&str>,
    ) -> EnqueueOutcome {
        let payload = AttachPayload {
            op: AttachOp::Detach,
            repo_id: repo.repo_id,
            repo_path: repo.repo_path.clone(),
            commands: Vec::new(),
        };
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: detach_operation_id(repo.repo_id, &repo.repo_path),
                path: repo.repo_path.clone(),
                old_id: root.ref_commit_hash,
                new_id: ZERO_ID.to_owned(),
                requester: requester.map(str::to_owned),
                payload: serde_json::to_value(&payload).unwrap(),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
    }

    async fn fu16_execute(storage: &crate::jupiter::storage::Storage, id: i64) -> ExecuteOutcome {
        assert_eq!(
            storage
                .push_queue_service
                .storage()
                .claim_for_execution(id)
                .await
                .unwrap(),
            ClaimOutcome::Claimed
        );
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: std::sync::Arc::new(
                crate::ceres::api_service::cache::GitObjectCache {
                    connection: crate::jupiter::tests::test_redis_manager().await,
                    prefix: String::new(),
                },
            ),
        };
        storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap()
    }

    async fn fu16_detach(
        storage: &crate::jupiter::storage::Storage,
        repo: &crate::ceres::protocol::repo::Repo,
    ) -> (i64, ExecuteOutcome) {
        let EnqueueOutcome::Inserted { id } = fu16_enqueue_detach(storage, repo, None).await else {
            panic!("detach insert");
        };
        (id, fu16_execute(storage, id).await)
    }

    async fn fu16_leaf(storage: &crate::jupiter::storage::Storage, path: &str) -> ImportLeaf {
        let mono = storage.mono_storage();
        let root = mono.get_main_ref("/").await.unwrap().unwrap();
        let txn = mono.get_connection().begin().await.unwrap();
        let leaf = import_leaf_in_txn(&mono, &root.ref_tree_hash, path, &txn)
            .await
            .unwrap();
        txn.rollback().await.unwrap();
        leaf
    }

    async fn fu16_count(storage: &crate::jupiter::storage::Storage, sql: String) -> i64 {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        storage
            .mono_storage()
            .get_connection()
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }

    async fn fu16_rows(storage: &crate::jupiter::storage::Storage, repo_id: i64) -> (i64, i64) {
        (
            fu16_count(
                storage,
                format!("SELECT count(*) AS n FROM git_repo WHERE id = {repo_id}"),
            )
            .await,
            fu16_count(
                storage,
                format!("SELECT count(*) AS n FROM import_refs WHERE repo_id = {repo_id}"),
            )
            .await,
        )
    }

    #[tokio::test]
    async fn fu16_detach_removes_leaf_and_rows() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (a, _) = fu16_mount(&storage, "/third-party/org/fu16-a").await;
        let (b, _) = fu16_mount(&storage, "/third-party/org/fu16-b").await;
        assert_eq!(fu16_rows(&storage, a.repo_id).await, (1, 1));

        // A sibling keeps the shared parent.
        let before = fu13_root(&storage).await;
        let (_, detached) = fu16_detach(&storage, &a).await;
        let ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes: 1,
            ..
        } = detached
        else {
            panic!("{detached:?}");
        };
        let after = fu13_root(&storage).await;
        assert_eq!(after.0, landed_commit_id);
        let commit = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            commit.parents_id,
            serde_json::json!([before.0]),
            "the detach commit follows the old root"
        );
        assert_eq!(
            commit.content.as_deref(),
            Some(format_commit_msg("Remove ImportRepo /third-party/org/fu16-a", None).as_str()),
            "the detach commit names the path and is framed"
        );
        assert_eq!(fu16_leaf(&storage, &a.repo_path).await, ImportLeaf::Absent);
        assert_eq!(
            fu16_leaf(&storage, &b.repo_path).await,
            ImportLeaf::GitkeepOnly
        );
        assert_eq!(fu16_rows(&storage, a.repo_id).await, (0, 0));
        assert_eq!(fu16_rows(&storage, b.repo_id).await, (1, 1));

        // The last repository under org takes the emptied org with it; the
        // import root stays.
        let (_, detached) = fu16_detach(&storage, &b).await;
        assert!(
            matches!(
                detached,
                ExecuteOutcome::Done {
                    root_cas_writes: 1,
                    ..
                }
            ),
            "{detached:?}"
        );
        assert_eq!(
            fu16_leaf(&storage, "/third-party/org").await,
            ImportLeaf::Absent
        );
        assert_ne!(
            fu16_leaf(&storage, "/third-party").await,
            ImportLeaf::Absent,
            "the import root is never removed"
        );
        assert_eq!(fu16_rows(&storage, b.repo_id).await, (0, 0));
    }

    #[tokio::test]
    async fn fu16_detach_fault_injection_rolls_back() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, _) = fu16_mount(&storage, "/third-party/fu16-fault").await;
        let before = fu13_root(&storage).await;
        let EnqueueOutcome::Inserted { id } = fu16_enqueue_detach(&storage, &repo, None).await
        else {
            panic!("detach insert");
        };
        // The ledger insert is the last write of the round; a row already
        // holding this cleanup id makes it fail after every other write.
        storage
            .git_db_storage()
            .insert_cleanup_in_txn(
                id,
                "/elsewhere",
                1,
                "anonymous",
                storage.mono_storage().get_connection(),
            )
            .await
            .unwrap();
        let outcome = fu16_execute(&storage, id).await;
        assert!(
            matches!(outcome, ExecuteOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert_eq!(fu13_root(&storage).await, before, "tree unchanged");
        assert_eq!(
            fu16_leaf(&storage, &repo.repo_path).await,
            ImportLeaf::GitkeepOnly
        );
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (1, 1));
        assert_eq!(
            fu16_count(
                &storage,
                format!(
                    "SELECT count(*) AS n FROM audit_logs WHERE target_id = {} \
                     AND metadata->>'kind' = '{IMPORT_REPO_REMOVE_KIND}'",
                    repo.repo_id
                )
            )
            .await,
            0,
            "no audit row"
        );
        let row = storage
            .push_queue_service
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);

        // The generated `.gitkeep` of an emptied import root: its `mega_blob`
        // row rides the transaction, so a refused round leaves none behind.
        fu16_strip_import_root_gitkeep(&storage).await;
        let before = fu13_root(&storage).await;
        let blob_rows =
            fu16_count(&storage, "SELECT count(*) AS n FROM mega_blob".to_owned()).await;
        let EnqueueOutcome::Inserted { id } = fu16_enqueue_detach(&storage, &repo, None).await
        else {
            panic!("detach insert");
        };
        storage
            .git_db_storage()
            .insert_cleanup_in_txn(
                id,
                "/elsewhere",
                1,
                "anonymous",
                storage.mono_storage().get_connection(),
            )
            .await
            .unwrap();
        let outcome = fu16_execute(&storage, id).await;
        assert!(
            matches!(outcome, ExecuteOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert_eq!(fu13_root(&storage).await, before);
        assert_eq!(
            fu16_leaf(&storage, "/third-party").await,
            ImportLeaf::Directory,
            "still without .gitkeep"
        );
        assert_eq!(
            fu16_count(&storage, "SELECT count(*) AS n FROM mega_blob".to_owned()).await,
            blob_rows,
            "no mega_blob row from the refused round"
        );
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (1, 1));
    }

    #[tokio::test]
    async fn fu16_detach_writes_audit_in_txn() {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        use crate::callisto::{
            audit_logs, import_repo_cleanups,
            sea_orm_active_enums::{ActorTypeEnum, AuditActionEnum},
        };

        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, _) = fu16_mount(&storage, "/third-party/fu16-audit").await;
        let EnqueueOutcome::Inserted { id } =
            fu16_enqueue_detach(&storage, &repo, Some("ci-token")).await
        else {
            panic!("detach insert");
        };
        let outcome = fu16_execute(&storage, id).await;
        assert!(
            matches!(outcome, ExecuteOutcome::Done { .. }),
            "{outcome:?}"
        );
        let conn = storage.mono_storage().get_connection().clone();
        let removed: Vec<_> = audit_logs::Entity::find()
            .filter(audit_logs::Column::TargetId.eq(repo.repo_id))
            .filter(audit_logs::Column::Action.eq(AuditActionEnum::Delete))
            .all(&conn)
            .await
            .unwrap();
        assert_eq!(removed.len(), 1, "{removed:?}");
        let audit = &removed[0];
        assert_eq!(audit.actor_id, 0);
        assert_eq!(audit.actor_type, ActorTypeEnum::Human);
        assert_eq!(
            audit.metadata,
            Some(serde_json::json!({
                "kind": IMPORT_REPO_REMOVE_KIND,
                "cleanup_id": id,
                "path": repo.repo_path,
                "requester": "ci-token",
                "phase": "detached",
            }))
        );
        let ledger = import_repo_cleanups::Entity::find_by_id(id)
            .one(&conn)
            .await
            .unwrap()
            .expect("ledger row keyed by the queue row id");
        assert_eq!(ledger.state, import_repo_cleanups::CleanupState::Detached);
        assert_eq!(
            (
                ledger.path.as_str(),
                ledger.repo_id,
                ledger.requester.as_str()
            ),
            (repo.repo_path.as_str(), repo.repo_id, "ci-token")
        );
    }

    #[tokio::test]
    async fn fu16_detach_without_provenance_keeps_tree() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        // An ordinary directory at the path, never mounted by this repository.
        fu13_plant(&storage, "third-party", "fu16-plain", false).await;
        let (repo, _) = fu16_register(&storage, "/third-party/fu16-plain").await;
        let before = fu13_root(&storage).await;
        let (id, outcome) = fu16_detach(&storage, &repo).await;
        assert!(
            matches!(
                outcome,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(fu13_root(&storage).await, before, "the directory stays");
        assert_eq!(
            fu16_leaf(&storage, &repo.repo_path).await,
            ImportLeaf::Directory
        );
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0));
        assert_eq!(
            fu16_count(
                &storage,
                format!("SELECT count(*) AS n FROM import_repo_cleanups WHERE id = {id}")
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn fu16_detach_operation_identity() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, c1) = fu16_mount(&storage, "/third-party/fu16-id").await;
        let attach_id = attach_operation_id(
            &repo.repo_id.to_string(),
            &normalize_attach_commands(&[(
                "refs/heads/main".to_owned(),
                "Create".to_owned(),
                ZERO_ID.to_owned(),
                c1,
            )]),
        );
        let detach_id = detach_operation_id(repo.repo_id, &repo.repo_path);
        // sha256("import_repo.detach\0" ‖ repo_id ‖ "\0" ‖ canonical path).
        assert_eq!(
            detach_operation_id(7, "/third-party/x"),
            "875bdce235a6670d1956abc3e623debc820a565c74d237ec98373be08b8d886c"
        );
        assert_eq!(
            detach_operation_id(7, "/third-party/y"),
            "4643611487ddfbec1e7cb7d628027323d604c602b41f83be88c2530ab0055b54"
        );
        assert_ne!(detach_id, attach_id);
        assert_ne!(
            detach_id,
            attach_operation_id(&repo.repo_id.to_string(), "")
        );
        assert_ne!(
            detach_id,
            detach_operation_id(repo.repo_id + 1, &repo.repo_path),
            "a re-import has a new repo_id and so a new id"
        );

        let cache = std::sync::Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        });
        let first = crate::ceres::pack::import_repo::detach_import_repo(
            &storage,
            cache.clone(),
            repo.repo_id,
            &repo.repo_path,
            None,
        )
        .await
        .unwrap()
        .expect("a detach that removed the repository has a cleanup id");
        let root = fu13_root(&storage).await;
        // Retrying the same detach replays its Done row and writes nothing.
        let again = crate::ceres::pack::import_repo::detach_import_repo(
            &storage,
            cache,
            repo.repo_id,
            &repo.repo_path,
            None,
        )
        .await
        .unwrap();
        assert_eq!(again, Some(first));
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(
            fu16_count(
                &storage,
                format!(
                    "SELECT count(*) AS n FROM import_repo_cleanups WHERE repo_id = {}",
                    repo.repo_id
                )
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn fu16_detach_child_orderings() {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};

        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let has_children = |path: &str| {
            ImportRepoError::HasChildren {
                path: path.to_owned(),
            }
            .to_string()
        };
        let refused_with = |outcome: &ExecuteOutcome, path: &str| matches!(outcome, ExecuteOutcome::Failed { message, .. } if *message == has_children(path));

        // A child registered below a mounted parent refuses the parent's
        // detach, before and after the child's own attach ran; neither side
        // changes.
        let (parent, _) = fu16_mount(&storage, "/third-party/fu16-parent").await;
        let (child, child_c1) = fu16_register(&storage, "/third-party/fu16-parent/child").await;
        let before = fu13_root(&storage).await;
        let (_, refused) = fu16_detach(&storage, &parent).await;
        assert!(refused_with(&refused, &parent.repo_path), "{refused:?}");
        assert_eq!(fu13_root(&storage).await, before);
        assert_eq!(fu16_rows(&storage, parent.repo_id).await, (1, 1));
        assert_eq!(fu16_rows(&storage, child.repo_id).await, (1, 0));

        let mounted = fu13_attach(
            &storage,
            &child,
            vec![fu12_cmd("refs/heads/main", "Create", ZERO_ID, &child_c1)],
            &child_c1,
        )
        .await;
        assert!(
            matches!(mounted, ExecuteOutcome::Done { .. }),
            "{mounted:?}"
        );
        let before = fu13_root(&storage).await;
        // A retry of the refused detach is a new row with the same identity.
        let EnqueueOutcome::Inserted { id } = fu16_enqueue_detach(&storage, &parent, None).await
        else {
            panic!("detach retry insert");
        };
        let refused = fu16_execute(&storage, id).await;
        assert!(refused_with(&refused, &parent.repo_path), "{refused:?}");
        assert_eq!(fu13_root(&storage).await, before);
        assert_eq!(fu16_rows(&storage, parent.repo_id).await, (1, 1));
        assert_eq!(fu16_rows(&storage, child.repo_id).await, (1, 1));

        // The detach locks its own row before it looks for children: a child
        // registration still holding the parent's row (FU-18's ancestor
        // fence) makes it wait, and it then sees the child.
        let (p2, _) = fu16_mount(&storage, "/third-party/fu16-p2").await;
        let EnqueueOutcome::Inserted { id } = fu16_enqueue_detach(&storage, &p2, None).await else {
            panic!("detach insert");
        };
        let conn = storage.mono_storage().get_connection().clone();
        let registering = conn.begin().await.unwrap();
        registering
            .execute_unprepared(&format!(
                "SELECT id FROM git_repo WHERE id = {} FOR SHARE",
                p2.repo_id
            ))
            .await
            .unwrap();
        registering
            .execute_unprepared(&format!(
                "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
                 VALUES ({}, '/third-party/fu16-p2/late', 'late', now(), now())",
                crate::common::utils::generate_id()
            ))
            .await
            .unwrap();
        let detach_storage = storage.clone();
        let detaching = tokio::spawn(async move { fu16_execute(&detach_storage, id).await });
        let mut waiting = false;
        for _ in 0..200 {
            let row = registering
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    // Only this test's detach can be blocked by this backend: the
                    // rows it locks live in this test's own schema.
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock' \
                     AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v",
                ))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                waiting = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(waiting, "the detach waits on the parent's row");
        registering.commit().await.unwrap();
        let refused = detaching.await.unwrap();
        assert!(refused_with(&refused, &p2.repo_path), "{refused:?}");
        assert_eq!(fu16_rows(&storage, p2.repo_id).await, (1, 1));

        // A `P/` alias of the repository itself (left by the FU-15 migration)
        // is the same split identity, not a child; it is not deleted either.
        let (p3, _) = fu16_mount(&storage, "/third-party/fu16-p3").await;
        conn.execute_unprepared(&format!(
            "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
             VALUES ({}, '/third-party/fu16-p3/', 'alias', now(), now())",
            crate::common::utils::generate_id()
        ))
        .await
        .unwrap();
        let (_, detached) = fu16_detach(&storage, &p3).await;
        assert!(
            matches!(
                detached,
                ExecuteOutcome::Done {
                    root_cas_writes: 1,
                    ..
                }
            ),
            "{detached:?}"
        );
        assert_eq!(fu16_rows(&storage, p3.repo_id).await, (0, 0));
        assert_eq!(
            fu16_count(
                &storage,
                "SELECT count(*) AS n FROM git_repo WHERE repo_path = '/third-party/fu16-p3/'"
                    .to_owned()
            )
            .await,
            1
        );

        // A repository whose first push never mounted it has no leaf and can
        // still be detached.
        let (orphan, _) = fu16_register(&storage, "/third-party/fu16-orphan").await;
        let before = fu13_root(&storage).await;
        let (_, detached) = fu16_detach(&storage, &orphan).await;
        assert!(
            matches!(
                detached,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{detached:?}"
        );
        assert_eq!(fu13_root(&storage).await, before);
        assert_eq!(fu16_rows(&storage, orphan.repo_id).await, (0, 0));
    }

    /// Take the `.gitkeep` out of `/third-party`, leaving only its mounts:
    /// the shape of an import root that the first attach created lazily.
    async fn fu16_strip_import_root_gitkeep(storage: &crate::jupiter::storage::Storage) {
        use std::str::FromStr;

        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                commit::Commit,
                tree::{Tree, TreeItem, TreeItemMode},
            },
        };

        use crate::jupiter::utils::converter::FromMegaModel;

        let mono = storage.mono_storage();
        let root_ref = mono.get_main_ref("/").await.unwrap().unwrap();
        let root = Tree::from_mega_model(
            mono.get_tree_by_hash(&root_ref.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        let import_item = root
            .tree_items
            .iter()
            .find(|item| item.name == "third-party")
            .unwrap()
            .clone();
        let import_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&import_item.id.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        let items: Vec<TreeItem> = import_tree
            .tree_items
            .into_iter()
            .filter(|item| item.name != ".gitkeep")
            .collect();
        assert!(!items.is_empty(), "strip after a mount, not before");
        let new_import = Tree::from_tree_items(items).unwrap();
        let mut root_items: Vec<TreeItem> = root
            .tree_items
            .iter()
            .filter(|item| item.name != "third-party")
            .cloned()
            .collect();
        root_items.push(TreeItem::new(
            TreeItemMode::Tree,
            new_import.id,
            "third-party".to_owned(),
        ));
        let new_root = Tree::from_tree_items(root_items).unwrap();
        let commit = Commit::from_tree_id(
            new_root.id,
            vec![ObjectHash::from_str(&root_ref.ref_commit_hash).unwrap()],
            "fu16 strip import root gitkeep",
        );
        mono.save_mega_trees(vec![new_import, new_root.clone()], commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .unwrap();
        let mut updated = root_ref;
        updated.ref_commit_hash = commit.id.to_string();
        updated.ref_tree_hash = new_root.id.to_string();
        mono.update_ref(updated, None).await.unwrap();
    }

    #[tokio::test]
    async fn fu16_detach_restores_import_root_gitkeep() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, _) = fu16_mount(&storage, "/third-party/fu16-only").await;
        fu16_strip_import_root_gitkeep(&storage).await;
        assert_eq!(
            fu16_leaf(&storage, "/third-party").await,
            ImportLeaf::Directory
        );
        let (_, detached) = fu16_detach(&storage, &repo).await;
        let ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes: 1,
            ..
        } = detached
        else {
            panic!("{detached:?}");
        };
        // The emptied import root keeps a `.gitkeep`, and its blob is stored
        // under the detach commit like the attach placeholder is.
        assert_eq!(
            fu16_leaf(&storage, "/third-party").await,
            ImportLeaf::GitkeepOnly
        );
        assert_eq!(
            fu16_count(
                &storage,
                format!(
                    "SELECT count(*) AS n FROM mega_blob WHERE commit_id = '{landed_commit_id}'"
                )
            )
            .await,
            1
        );
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0));
    }

    #[tokio::test]
    async fn fu16_detach_replay_rechecks_live_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let (repo, _) = fu16_mount(&storage, "/third-party/fu16-replay").await;
        // A `Done` row under the detach identity that did not detach: what a
        // pre-FU-16 binary leaves when it runs a detach row as a delete-only
        // attach (the payload has no `op`).
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let EnqueueOutcome::Inserted { id: poisoned } = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: detach_operation_id(repo.repo_id, &repo.repo_path),
                path: repo.repo_path.clone(),
                old_id: root.ref_commit_hash,
                new_id: ZERO_ID.to_owned(),
                requester: None,
                payload: serde_json::json!({
                    "repo_id": repo.repo_id,
                    "repo_path": repo.repo_path,
                    "commands": [],
                }),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        else {
            panic!("poison insert");
        };
        let ran = fu16_execute(&storage, poisoned).await;
        assert!(
            matches!(
                ran,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{ran:?}"
        );
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (1, 1));

        // The detach does not take that replay for an answer.
        let cache = std::sync::Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        });
        let cleanup_id = crate::ceres::pack::import_repo::detach_import_repo(
            &storage,
            cache.clone(),
            repo.repo_id,
            &repo.repo_path,
            None,
        )
        .await
        .unwrap()
        .expect("a cleanup id");
        assert_ne!(cleanup_id, poisoned);
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0));
        assert_eq!(
            fu16_leaf(&storage, &repo.repo_path).await,
            ImportLeaf::Absent
        );
        assert_eq!(
            fu16_count(
                &storage,
                format!("SELECT count(*) AS n FROM import_repo_cleanups WHERE id = {cleanup_id}")
            )
            .await,
            1
        );
        // Once the repository is gone the poisoned row replays, but the
        // cleanup id handed out is the ledger row's, never the bare queue id.
        let again = crate::ceres::pack::import_repo::detach_import_repo(
            &storage,
            cache,
            repo.repo_id,
            &repo.repo_path,
            None,
        )
        .await
        .unwrap();
        assert_eq!(again, Some(cleanup_id));
    }

    #[tokio::test]
    async fn fu16_detach_import_root_row_keeps_tree() {
        use sea_orm::ConnectionTrait;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let cache = std::sync::Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        });
        // Legacy rows at `import_dir` itself and outside it (DEFER-FU-09): the
        // detach unregisters them and never touches the tree, even with a
        // provenance record claiming the path.
        for path in ["/third-party", "/project/fu16-outside"] {
            let (repo, _) = fu16_register(&storage, path).await;
            AuditStorage::log_import_repo_attach_in_txn(
                storage.mono_storage().get_connection(),
                repo.repo_id,
                path,
            )
            .await
            .unwrap();
            let before = fu13_root(&storage).await;
            let (_, detached) = fu16_detach(&storage, &repo).await;
            assert!(
                matches!(
                    detached,
                    ExecuteOutcome::Done {
                        root_cas_writes: 0,
                        ..
                    }
                ),
                "{path}: {detached:?}"
            );
            assert_eq!(fu13_root(&storage).await, before, "{path}");
            assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0), "{path}");
        }
        assert_eq!(
            fu16_leaf(&storage, "/third-party").await,
            ImportLeaf::GitkeepOnly,
            "the import root stays"
        );
        // A detach that had nothing to detach and left no ledger row hands
        // out no cleanup id.
        let (gone, _) = fu16_register(&storage, "/third-party/fu16-gone").await;
        storage
            .git_db_storage()
            .get_connection()
            .execute_unprepared(&format!("DELETE FROM git_repo WHERE id = {}", gone.repo_id))
            .await
            .unwrap();
        let none = crate::ceres::pack::import_repo::detach_import_repo(
            &storage,
            cache,
            gone.repo_id,
            &gone.repo_path,
            None,
        )
        .await
        .unwrap();
        assert_eq!(none, None);
    }

    /// The two lookups a detach makes while holding the global write lock,
    /// at scale (GC-10): the child check reads `git_repo`, one row per
    /// ImportRepo (never per object), and the branch check is driven by the
    /// `repo_id` index, so it touches one repository's refs at most.
    #[tokio::test]
    async fn fu16_lookups_bounded_at_scale() {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};

        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let conn = storage.mono_storage().get_connection().clone();
        let (repo, _) = fu16_register(&storage, "/third-party/fu16-scale").await;
        let (decoy, _) = fu16_register(&storage, "/third-party/fu16-scale-decoy").await;
        // 1000 other repositories, none below the path, and 1000 tags on a
        // decoy repository, so that the planner picks the repo_id index for
        // the subject on its own (a table made of one repository's refs
        // would be scanned).
        let mut rows = Vec::new();
        for i in 0..1000 {
            rows.push(format!(
                "({}, '/third-party/other-{i}', 'r', now(), now())",
                crate::common::utils::generate_id()
            ));
        }
        conn.execute_unprepared(&format!(
            "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) VALUES {}",
            rows.join(",")
        ))
        .await
        .unwrap();
        let mut tags = Vec::new();
        for i in 0..1000 {
            tags.push(format!(
                "({}, {}, 'refs/tags/v{i}', '{:040}', 'tag', false, now(), now())",
                crate::common::utils::generate_id(),
                decoy.repo_id,
                i
            ));
        }
        conn.execute_unprepared(&format!(
            "INSERT INTO import_refs (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) VALUES {}",
            tags.join(",")
        ))
        .await
        .unwrap();
        conn.execute_unprepared("ANALYZE git_repo; ANALYZE import_refs")
            .await
            .unwrap();

        let txn = conn.begin().await.unwrap();
        assert!(
            !storage
                .git_db_storage()
                .import_repo_has_children(repo.repo_id, &repo.repo_path, &txn)
                .await
                .unwrap()
        );
        assert!(!has_branch_ref_in_txn(&txn, repo.repo_id).await.unwrap());
        assert!(!has_branch_ref_in_txn(&txn, decoy.repo_id).await.unwrap());
        // The branch check is served through the repo_id index, not a scan
        // of the whole table.
        let plan: Vec<String> = txn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "EXPLAIN SELECT EXISTS (SELECT 1 FROM import_refs WHERE repo_id = $1 AND ref_type = 'branch')",
                [repo.repo_id.into()],
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
            .collect();
        assert!(
            plan.iter().any(|line| line.contains("repo_id = ")
                && (line.contains("Index Cond") || line.contains("Recheck Cond"))),
            "{plan:?}"
        );
        txn.rollback().await.unwrap();

        // One real child, and one branch, flip both answers.
        let (child, _) = fu16_register(&storage, "/third-party/fu16-scale/child").await;
        storage
            .git_db_storage()
            .save_ref(
                repo.repo_id,
                fu12_ref(repo.repo_id, "refs/heads/main", &"a".repeat(40)),
            )
            .await
            .unwrap();
        let txn = conn.begin().await.unwrap();
        assert!(
            storage
                .git_db_storage()
                .import_repo_has_children(repo.repo_id, &repo.repo_path, &txn)
                .await
                .unwrap()
        );
        assert!(
            !storage
                .git_db_storage()
                .import_repo_has_children(child.repo_id, &child.repo_path, &txn)
                .await
                .unwrap()
        );
        assert!(has_branch_ref_in_txn(&txn, repo.repo_id).await.unwrap());
        txn.rollback().await.unwrap();

        // The detach's ref deletion is one statement served through the same
        // repo_id index, so it touches this repository's refs only: the
        // branchless decoy with its 1000 tags detaches in one round and
        // leaves none of them behind.
        let txn = conn.begin().await.unwrap();
        let plan: Vec<String> = txn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "EXPLAIN DELETE FROM import_refs WHERE repo_id = $1",
                [repo.repo_id.into()],
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
            .collect();
        assert!(
            plan.iter().any(|line| line.contains("repo_id = ")
                && (line.contains("Index Cond") || line.contains("Recheck Cond"))),
            "{plan:?}"
        );
        txn.rollback().await.unwrap();
        let (_, detached) = fu16_detach(&storage, &decoy).await;
        assert!(
            matches!(
                detached,
                ExecuteOutcome::Done {
                    root_cas_writes: 0,
                    ..
                }
            ),
            "{detached:?}"
        );
        assert_eq!(fu16_rows(&storage, decoy.repo_id).await, (0, 0));
    }

    // ---------------------------------------------------------------------
    // plan-20260923 FU-18: the liveness fence of B3 attach.
    // ---------------------------------------------------------------------

    fn fu18_removed(path: &str) -> String {
        crate::common::errors::ImportRepoError::Removed {
            path: path.to_owned(),
        }
        .to_string()
    }

    /// A refused round: `Failed` / `AttachFailure` with `message`, persisted.
    async fn fu18_assert_refused(
        storage: &crate::jupiter::storage::Storage,
        outcome: &ExecuteOutcome,
        message: &str,
    ) {
        let ExecuteOutcome::Failed {
            id,
            failure,
            message: got,
        } = outcome
        else {
            panic!("expected a refused round, got {outcome:?}");
        };
        assert_eq!(failure, "AttachFailure");
        assert_eq!(got, message);
        let row = storage
            .push_queue_service
            .storage()
            .get_by_id(*id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(row.error_message.as_deref(), Some(message));
    }

    async fn fu18_audits(storage: &crate::jupiter::storage::Storage, repo_id: i64) -> i64 {
        fu16_count(
            storage,
            format!("SELECT count(*) AS n FROM audit_logs WHERE target_id = {repo_id}"),
        )
        .await
    }

    #[tokio::test]
    async fn fu18_stale_attach_after_detach_is_removed() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let path = "/third-party/fu18-stale";
        let (repo, c1) = fu16_mount(&storage, path).await;
        let (_, detached) = fu16_detach(&storage, &repo).await;
        assert!(
            matches!(detached, ExecuteOutcome::Done { .. }),
            "{detached:?}"
        );
        // No sweep: the commits are still there, so only the fence stops a
        // first mount from rebuilding the leaf.
        let root = fu13_root(&storage).await;
        let audits = fu18_audits(&storage, repo.repo_id).await;
        let stale = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/topic", "Create", ZERO_ID, &c1)],
            &c1,
        )
        .await;
        fu18_assert_refused(&storage, &stale, &fu18_removed(path)).await;
        assert!(matches!(
            fu16_leaf(&storage, path).await,
            ImportLeaf::Absent
        ));
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0));
        assert_eq!(fu18_audits(&storage, repo.repo_id).await, audits);
    }

    #[tokio::test]
    async fn fu18_stale_attach_after_reimport_is_removed() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let path = "/third-party/fu18-reimport";
        let (r1, c1) = fu16_mount(&storage, path).await;
        let (_, detached) = fu16_detach(&storage, &r1).await;
        assert!(
            matches!(detached, ExecuteOutcome::Done { .. }),
            "{detached:?}"
        );
        let (r2, _) = fu16_mount(&storage, path).await;
        let root = fu13_root(&storage).await;
        let r2_rows = fu16_rows(&storage, r2.repo_id).await;
        // R1's provenance record would pass the update-mode gate: only the
        // fence keeps an orphan ref of R1 out.
        let stale = fu13_attach(
            &storage,
            &r1,
            vec![fu12_cmd("refs/heads/topic2", "Create", ZERO_ID, &c1)],
            &c1,
        )
        .await;
        fu18_assert_refused(&storage, &stale, &fu18_removed(path)).await;
        assert_eq!(fu16_rows(&storage, r1.repo_id).await, (0, 0));
        assert_eq!(fu16_rows(&storage, r2.repo_id).await, r2_rows);
        assert_eq!(fu13_root(&storage).await, root);
    }

    #[tokio::test]
    async fn fu18_stale_delete_only_after_detach_is_removed() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let path = "/third-party/fu18-delete";
        let (repo, c1) = fu16_mount(&storage, path).await;
        let (_, detached) = fu16_detach(&storage, &repo).await;
        assert!(
            matches!(detached, ExecuteOutcome::Done { .. }),
            "{detached:?}"
        );
        let root = fu13_root(&storage).await;
        let stale = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/main", "Delete", &c1, ZERO_ID)],
            ZERO_ID,
        )
        .await;
        fu18_assert_refused(&storage, &stale, &fu18_removed(path)).await;
        assert_eq!(fu13_root(&storage).await, root);
        assert_eq!(fu16_rows(&storage, repo.repo_id).await, (0, 0));
    }

    #[tokio::test]
    async fn fu18_removed_outranks_materialization_precheck() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = fu13_storage(temp.path()).await;
        let path = "/third-party/fu18-prec";
        let (repo, c1) = fu16_mount(&storage, path).await;
        let (_, detached) = fu16_detach(&storage, &repo).await;
        assert!(
            matches!(detached, ExecuteOutcome::Done { .. }),
            "{detached:?}"
        );
        // An ancestor with a materialized main ref (DEFER-FU-13/20 shape):
        // the precheck would refuse with its I3 text.
        storage
            .mono_storage()
            .save_refs(
                crate::callisto::mega_refs::Model::new(
                    "/third-party",
                    MEGA_BRANCH_NAME.to_owned(),
                    "a".repeat(40),
                    "b".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let root = fu13_root(&storage).await;
        let stale = fu13_attach(
            &storage,
            &repo,
            vec![fu12_cmd("refs/heads/topic", "Create", ZERO_ID, &c1)],
            &c1,
        )
        .await;
        fu18_assert_refused(&storage, &stale, &fu18_removed(path)).await;
        assert_eq!(fu13_root(&storage).await, root);
    }
}
