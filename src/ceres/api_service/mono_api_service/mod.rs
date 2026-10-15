//! # Mono API Service
//!
//! This module provides the API service implementation for monorepo operations in the Mega system.
//! The `MonoApiService` struct implements the `ApiHandler` trait to provide comprehensive
//! monorepo management capabilities including file operations, merge request handling,
//! and Git-like version control functionality.
//!
//! ## Key Features
//!
//! - **File Management**: Create files and directories within the monorepo structure
//! - **Tree Operations**: Handle Git tree objects for version control
//! - **Merge Requests**: Process and merge pull/merge requests with conflict resolution
//! - **Diff Operations**: Generate content differences between commits using libra
//! - **Commit Management**: Retrieve and manage commit objects and their relationships
//! - **Storage Integration**: Seamless integration with the underlying storage layer
//!
//! ## Core Components
//!
//! - `MonoApiService`: Main service struct that wraps storage functionality
//! - `ApiHandler` implementation: Provides standardized API operations
//! - Merge request processing with automated conflict detection
//! - Tree traversal and blob extraction utilities
//!
//! ## Dependencies
//!
//! This module relies on several core components:
//! - `git_internal`: Git object handling and version control primitives
//! - `jupiter`: Storage layer abstraction and data persistence
//! - `callisto`: Database models and ORM functionality
//! - `libra`: External Git-compatible command-line tool for diff operations
//!
//! ## Usage
//!
//! The service is typically instantiated with a storage backend and used to handle
//! API requests for monorepo operations. All operations are asynchronous and return
//! appropriate error types for robust error handling.

mod prelude;

mod authz;
mod branch;
mod buck;
mod diff_ops;
mod editing;
mod handler;
mod logic;
mod merge;
mod provision;

#[cfg(test)]
mod server_signed_commit_tests;
#[cfg(test)]
mod test;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use authz::LARGE_CL_RENAME_DETECTION_THRESHOLD;
// Kept at the historical module path. The non-test crate reaches the queue
// decisions above; these ACL helpers are the same public surface, called from tests.
#[cfg_attr(not(test), allow(unused_imports))]
pub use authz::{
    AclChangeDecision, QUEUE_MISSING_REQUESTER_GUIDANCE, decide_acl_change,
    emit_merge_authz_unavailable,
};
pub use authz::{
    QueueExecutionDecision, authz_freeze_message, decide_queue_execution, emit_authz_frozen_alert,
};
pub use logic::MonoServiceLogic;
use prelude::*;
use provision::collect_page_blobs;

#[derive(Clone)]
pub struct MonoApiService {
    pub storage: Storage,
    pub git_object_cache: Arc<GitObjectCache>,
}

impl From<&Monorepo> for MonoApiService {
    fn from(mono_repo: &Monorepo) -> Self {
        MonoApiService {
            storage: mono_repo.storage.clone(),
            git_object_cache: mono_repo.git_object_cache.clone(),
        }
    }
}

impl From<&ImportRepo> for MonoApiService {
    fn from(import_repo: &ImportRepo) -> Self {
        MonoApiService {
            storage: import_repo.storage.clone(),
            git_object_cache: import_repo.git_object_cache.clone(),
        }
    }
}

pub struct TreeUpdateResult {
    pub updated_trees: Vec<Tree>,
    pub ref_updates: Vec<RefUpdate>,
}

pub(crate) struct PushApplyArgs<'a> {
    pub result: &'a TreeUpdateResult,
    pub path_p: &'a str,
    pub landed_at_p: &'a str,
    pub expected_root_commit: Option<&'a str>,
    pub expected_root_tree: Option<&'a str>,
    pub extra_commits: Vec<Commit>,
    pub extra_blob: Option<Blob>,
    pub provenance: Option<&'a crate::ceres::pack::trunk_provenance::TrunkProvenance>,
    pub sign_trunk: Option<TrunkGitSign>,
}

pub(crate) type TrunkGitSign =
    std::sync::Arc<dyn Fn(&Commit) -> Result<Commit, GitError> + Send + Sync>;

pub struct RefUpdate {
    path: String,
    tree_id: ObjectHash,
}

struct PatchSections<'a> {
    header_lines: Vec<&'a str>,
    divider_line: Option<&'a str>,
    payload_lines: Vec<&'a str>,
    has_trailing_newline: bool,
}

struct PagedClDiffItem {
    item: DiffItem,
    old_path: Option<String>,
}

struct CreateEntryUpdate {
    update_result: TreeUpdateResult,
    blob: Blob,
    entry_oid: ObjectHash,
    repo_path: PathBuf,
    save_trees: Vec<Tree>,
}

/// The rewritten trees a directory change produced, plus everything
/// [`MonoApiService::commit_tree_update`] needs to turn them into one commit
/// and advance the tip (plan-20260917 ADR-LB-02; shared by create/delete).
struct TreeCommitInput {
    update_result: TreeUpdateResult,
    /// New tree objects to persist (leaf/parent trees the change built).
    save_trees: Vec<Tree>,
    /// New blob objects to persist (empty for a pure tree rewrite).
    blobs: Vec<Blob>,
    /// Path whose subtree the change rewrote; resolves the landing tip.
    repo_path: PathBuf,
    /// The path the user wrote (entry or move destination); names the path
    /// in path policy errors when no landing tip exists yet (ADR-FU-06).
    policy_path: String,
    /// Whether `policy_path` is a directory this write creates; otherwise a
    /// provisioning hint names its parent directory.
    policy_is_dir: bool,
    /// Path provisioning (ADR-FU-05): tree ids of the snapshot the update was
    /// built from, keyed by directory path. When the landing tip's tree
    /// differs, the round is stale ([`PROVISION_STALE_SNAPSHOT`]).
    expected_bases: Option<HashMap<String, ObjectHash>>,
    commit_msg: String,
    author_username: Option<String>,
    skip_build: bool,
    mode: EditCLMode,
}

/// Plan of one path provisioning round (plan-20260923 ADR-FU-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionPlan {
    /// Canonical target path.
    pub path: String,
    /// Highest component this request would create; `None` when the whole
    /// path already is a directory.
    pub highest_new: Option<String>,
}

const PROVISION_ATTEMPTS: u32 = 10;

/// Marker of a provisioning round whose snapshot no longer matches the
/// landing tip; the round is re-planned.
const PROVISION_STALE_SNAPSHOT: &str = "path provision snapshot is stale";

/// Queue refusals of a provisioning round that mean the tree or the queue
/// moved underneath it; fixed server texts (no user path), matched at the
/// start of the landing error after its fixed prefixes.
const PROVISION_QUEUE_RETRY: [&str; 5] = [
    "another push for this path is already Queued/Running (ADR-TP-10)",
    "non-fast-forward: new_id does not match current tip",
    "non-fast-forward: ref_commit_hash does not match old_id",
    "tombstone exists; advertise then fetch",
    "cannot update missing path; create requires old_id=ZERO_ID",
];

/// Why a failed provisioning landing should be re-planned, if it should.
/// Coded errors (`[code:…]`, path policy) never are.
fn provision_retry_reason(err: &GitError) -> Option<String> {
    let GitError::CustomError(text) = err else {
        return None;
    };
    if text.starts_with("[code:") {
        return None;
    }
    if text == PROVISION_STALE_SNAPSHOT {
        return Some(text.clone());
    }
    let mut body = text.strip_prefix("Other error: ").unwrap_or(text);
    if let Some(rest) = body.strip_prefix("push rejected for push_queue id ")
        && let Some((id, message)) = rest.split_once(": ")
        && id.bytes().all(|b| b.is_ascii_digit())
    {
        body = message;
    }
    PROVISION_QUEUE_RETRY
        .iter()
        .any(|refusal| body.starts_with(refusal))
        .then(|| text.clone())
}

/// Result of applying one provisioning round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    /// Landed commit id.
    Landed(String),
    /// The tree or the queue changed since the plan (a changed highest new
    /// component, a stale snapshot, a same-path queue conflict or a
    /// non-fast-forward): re-plan and re-authorize.
    Retry(String),
}

/// The two halves of one provisioning round (plan-20260923 ADR-FU-05),
/// implemented by [`MonoApiService`]; a trait so the retry loop can be tested
/// without a stack.
#[async_trait]
pub(crate) trait PathProvisioner: Sync {
    async fn plan_provision(&self, path: &str) -> Result<ProvisionPlan, GitError>;
    async fn apply_provision(
        &self,
        plan: &ProvisionPlan,
        requester: Option<String>,
    ) -> Result<ApplyOutcome, GitError>;
}

/// ADR-FU-05 items 2–3: authorize the target (401/403 before any tree
/// read), then per round plan, authorize the highest new component and
/// apply. An apply that reports [`ApplyOutcome::Retry`] is re-planned and
/// re-authorized (at most [`PROVISION_ATTEMPTS`] rounds); every error is
/// returned at once. `path` must already be canonical.
pub(crate) async fn run_path_provision<E, P, Auth>(
    provisioner: &P,
    path: &str,
    mut authorize: Auth,
) -> Result<PathProvisionResult, E>
where
    E: From<GitError>,
    P: PathProvisioner + ?Sized,
    Auth: FnMut(&str) -> Result<Option<String>, E>,
{
    authorize(path)?;
    let mut last_reason = String::new();
    for attempt in 0..PROVISION_ATTEMPTS {
        let current = provisioner.plan_provision(path).await?;
        let scope = current
            .highest_new
            .clone()
            .unwrap_or_else(|| current.path.clone());
        let requester = authorize(&scope)?;
        if current.highest_new.is_none() {
            return Ok(PathProvisionResult {
                path: current.path,
                created: false,
                commit_id: None,
            });
        }
        match provisioner.apply_provision(&current, requester).await? {
            ApplyOutcome::Landed(commit_id) => {
                return Ok(PathProvisionResult {
                    path: current.path,
                    created: true,
                    commit_id: Some(commit_id),
                });
            }
            ApplyOutcome::Retry(reason) => {
                tracing::info!(path, attempt, reason = %reason, "path provision round is stale; re-planning");
                last_reason = reason;
                if attempt + 1 < PROVISION_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        100 * (u64::from(attempt) + 1),
                    ))
                    .await;
                }
            }
        }
    }
    Err(GitError::CustomError(format!(
        "path provision of {path} did not converge after {PROVISION_ATTEMPTS} rounds: {last_reason}"
    ))
    .into())
}

/// What [`MonoApiService::commit_tree_update`] landed.
struct TreeCommitOutcome {
    commit_id: String,
    cl_link: Option<String>,
}

/// The timestamped `.gitkeep` placeholder that represents an empty directory
/// — create-entry's new directories and delete-entry's emptied parents share
/// it (GC-02).
fn gitkeep_placeholder() -> (Blob, TreeItem) {
    let blob = generate_git_keep_with_timestamp();
    let item = TreeItem {
        mode: TreeItemMode::Blob,
        id: blob.id,
        name: String::from(".gitkeep"),
    };
    (blob, item)
}

struct ApplyChangeContext<'a> {
    components: &'a [String],
    chain_paths: &'a [PathBuf],
    chain_trees: &'a [Tree],
    tree_cache: &'a mut HashMap<PathBuf, Tree>,
    new_trees: &'a mut HashMap<ObjectHash, Tree>,
}

/// MC-04 R2: the constructed + server-signed output of a CL-ref update,
/// before any persistence. Persistence is the caller's job, inside one DB
/// transaction together with the CL hash move and the commit-listing
/// rebuild — a failure there must not leave ref/commit/tree pointing at a
/// range the CL row no longer claims.
struct PreparedClUpdate {
    commits: Vec<Commit>,
    cl_ref: mega_refs::Model,
    updates: Vec<RefUpdateData>,
    tree_models: Vec<mega_tree::ActiveModel>,
    new_commit_id: String,
}
