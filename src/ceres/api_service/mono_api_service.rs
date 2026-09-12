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

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use cedar_policy::{Context as CedarRequestContext, EntityId, EntityTypeName, EntityUid};
use futures::{StreamExt, stream};
use git_internal::{
    DiffItem,
    diff::Diff as GitDiff,
    errors::GitError,
    hash::{ObjectHash, get_hash_kind},
    internal::{
        metadata::EntryMeta,
        object::{
            blob::Blob,
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    },
};
use regex::Regex;
use sea_orm::DatabaseTransaction;
use tracing::debug;

use crate::{
    bellatrix::Bellatrix,
    callisto::{
        mega_blob, mega_cl, mega_refs, mega_tag, mega_tree,
        sea_orm_active_enums::{
            CheckTypeEnum, ConvTypeEnum, MergeStatusEnum, PushQueueKindEnum, PushQueueStatusEnum,
        },
    },
    ceres::{
        api_service::{
            ApiHandler, buck_tree_builder::BuckCommitBuilder, cache::GitObjectCache,
            state::ProtocolApiState, tree_ops,
        },
        build_trigger::{BuildTriggerService, TriggerContext},
        code_edit::{on_edit::OneditCodeEdit, utils as edit_utils},
        diff::tree_diff,
        merge_checker::CheckerRegistry,
        model::{
            buck::{
                CompletePayload, CompleteResponse, DEFAULT_MODE, FileChange,
                FileToUpload as ApiFileToUpload, ManifestPayload, ManifestResponse,
            },
            change_list::{ClDiffFile, ClFilesChangedItemSchema, UpdateBranchStatusRes},
            git::{CreateEntryInfo, CreateEntryResult, EditFilePayload, EditFileResult},
            tag::TagInfo,
            third_party::{ThirdPartyClient, ThirdPartyRepoTrait},
        },
        pack::{api_tip_lander::land_api_tip_push, import_repo::ImportRepo, monorepo::Monorepo},
        protocol::{ServiceType, SmartSession, TransportProtocol},
    },
    common::{
        errors::{BuckError, MegaError},
        utils::{MEGA_BRANCH_NAME, ZERO_ID},
    },
    config::PushPolicy,
    contract::{
        api::common::Pagination,
        policy::{
            builder::EntitySnapshot,
            context::CedarContext,
            enforcement::Enforcement,
            entitystore::MEGA_CEDAR_PATH,
            notify::{
                AUTHZ_BARRIER_TIMEOUT, authz_blob_id, ensure_authz_snapshot_caught_up,
                notify_authz_changed_best_effort,
            },
            resource::resolve_resource,
            util::SaturnEUid,
        },
        vault::server_signing::ServerSigningContext,
    },
    jupiter::{
        service::{
            buck_service::{
                CommitArtifacts, CompletePayload as SvcCompletePayload,
                CompleteResponse as SvcCompleteResponse,
            },
            push_queue_service::{
                EnqueueRequest, ExecuteOutcome, ExecuteRequest, MergeExecContext, MergePayload,
                PushPayload, QueueWaitResult, merge_operation_id,
            },
        },
        storage::{
            Storage,
            base_storage::StorageConnector,
            buck_storage::{session_status, upload_status},
            mono_storage::RefUpdateData,
        },
        utils::converter::{FromMegaModel, IntoMegaModel, generate_git_keep_with_timestamp},
    },
};
#[rustfmt::skip]
use crate::orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace};

#[derive(Clone)]
pub struct MonoApiService {
    pub storage: Storage,
    pub git_object_cache: Arc<GitObjectCache>,
}

const LARGE_CL_RENAME_DETECTION_THRESHOLD: usize = 1000;

/// Outcome of the ACL-change check (UN-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclChangeDecision {
    /// Either the CL does not touch the authorization file, or this mode does
    /// not consume authorization data.
    Proceed,
    /// The CL changes the authorization file and this principal is not an
    /// admin.
    Refuse { reason: String },
    /// The check itself could not be completed, so nothing about the change is
    /// known. Under `enforce` that refuses the merge (fail-closed) rather than
    /// merging an unexamined ACL change.
    Unavailable { reason: String },
}

/// Decide whether a CL that (maybe) edits `/.mega_cedar.json` may be merged
/// (UN-19).
///
/// Editing the authorization file is how permissions are granted, so anyone who
/// can merge such a change can grant themselves anything. A maintainer holds
/// `approveMergeRequest` and would otherwise have had exactly that: a
/// self-promotion path. The check therefore demands `addAdmin`, which only the
/// admin group holds.
pub fn decide_acl_change(
    enforcement: Enforcement,
    snapshot: Option<&EntitySnapshot>,
    touches_acl: bool,
    authz_principal: &str,
) -> AclChangeDecision {
    // `off`: nothing is detected and nothing is consumed (GC-UN-01).
    if !enforcement.builds() || !touches_acl {
        return AclChangeDecision::Proceed;
    }

    let Some(snapshot) = snapshot else {
        return AclChangeDecision::Unavailable {
            reason: "the authorization snapshot is not built, so an ACL change cannot be reviewed"
                .to_owned(),
        };
    };

    let store = snapshot.store();
    let is_admin = match resolve_resource("/", store).resource() {
        None => false,
        Some(resource) => match CedarContext::new(store.clone()) {
            Err(_) => false,
            Ok(context) => match acl_change_euids(authz_principal) {
                None => false,
                Some((principal, action)) => context
                    .is_authorized(&principal, &action, resource, CedarRequestContext::empty())
                    .is_ok(),
            },
        },
    };

    if is_admin && !store.is_empty() {
        return AclChangeDecision::Proceed;
    }

    AclChangeDecision::Refuse {
        reason: format!(
            "`{authz_principal}` may not merge a change to {MEGA_CEDAR_PATH}:              editing the authorization file requires admin, since it is how permissions              are granted"
        ),
    }
}

/// Cedar ids for the ACL-change check: the `addAdmin` action is admin-only.
fn acl_change_euids(principal: &str) -> Option<(SaturnEUid, SaturnEUid)> {
    let user_type = EntityTypeName::from_str("User").ok()?;
    let action_type = EntityTypeName::from_str("Action").ok()?;
    Some((
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            user_type,
            EntityId::from_str(principal).ok()?,
        )),
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            action_type,
            EntityId::from_str("addAdmin").ok()?,
        )),
    ))
}

/// The single emit site of the `merge_authz_unavailable` alert (UN-19).
pub fn emit_merge_authz_unavailable(cl_link: &str, principal: &str, reason: &str) {
    tracing::error!(
        event = "merge_authz_unavailable",
        cl_link = %cl_link,
        principal = %principal,
        reason = %reason,
        "merge authorization could not be decided"
    );
}

/// What the queue's background worker should do with an item (UN-17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueExecutionDecision {
    /// Merge it, authorized as this principal. The execution actor stays
    /// `system` — the worker is what runs the merge, the requester is what
    /// justifies it (ADR-UN-06 ④).
    Execute { authz_principal: String },
    /// Do not merge: authorization for this item cannot proceed. The reason
    /// carries the operator guidance, since a frozen item needs a human
    /// decision rather than a blind retry.
    Freeze { reason: String },
}

/// Guidance stored and logged when a queued item has no recorded requester.
///
/// Retrying alone cannot fix this one: the item predates requester capture (or
/// was queued anonymously), so there is no subject to authorize it as. Saying
/// that plainly is the difference between an operator re-queueing it correctly
/// and repeatedly retrying something that can never succeed.
pub const QUEUE_MISSING_REQUESTER_GUIDANCE: &str = "the queued item has no recorded requester (it was queued before requester capture,      or queued anonymously), so there is no subject to authorize the merge as;      an operator must re-queue it as an identified user";

/// The queue's execution decision (UN-17).
///
/// `off` and `shadow` never change what runs, matching the three-state contract
/// (ADR-UN-01): `shadow` evaluates and records what `enforce` would have
/// refused. Under `enforce` an item is executed only if its recorded requester
/// is still authorized to approve the merge — the subject that asked for it,
/// re-checked at the moment it actually runs, which may be long after queueing.
pub fn decide_queue_execution(
    enforcement: Enforcement,
    snapshot: Option<&EntitySnapshot>,
    requester: Option<&str>,
) -> QueueExecutionDecision {
    let execute_as = |principal: &str| QueueExecutionDecision::Execute {
        authz_principal: principal.to_owned(),
    };

    if !enforcement.builds() {
        // `off`: no build, no consume — unchanged behavior (GC-UN-01). The
        // recorded requester is still passed through as the principal, which is
        // inert today (nothing evaluates it under `off`) and keeps the value
        // meaningful if the parameter ever reaches an audit trail; a legacy
        // item with no requester keeps running as the worker, exactly as before.
        return execute_as(requester.unwrap_or("system"));
    }

    let (would_deny, reason) = match (requester, snapshot) {
        (None, _) => (true, QUEUE_MISSING_REQUESTER_GUIDANCE.to_owned()),
        (Some(_), None) => (
            true,
            "the authorization snapshot is not built, so the queued merge cannot be decided;              retry once authorization is available"
                .to_owned(),
        ),
        (Some(requester), Some(snapshot)) => {
            let store = snapshot.store();
            let denied = match resolve_resource("/", store).resource() {
                None => true,
                Some(resource) => match CedarContext::new(store.clone()) {
                    Err(_) => true,
                    Ok(context) => {
                        match queue_euids(requester) {
                            None => true,
                            Some((principal, action)) => context
                                .is_authorized(&principal, &action, resource, CedarRequestContext::empty())
                                .is_err(),
                        }
                    }
                },
            } || store.is_empty();
            (
                denied,
                format!(
                    "the recorded requester `{requester}` is not authorized to approve this merge;                      an operator must re-queue it as an authorized user or grant that permission"
                ),
            )
        }
    };

    if enforcement.records_would_deny() && would_deny {
        tracing::warn!(
            event = "authz_would_deny",
            principal = %requester.unwrap_or("<none>"),
            principal_type = "User",
            action = "approveMergeRequest",
            resource = "/",
            "would-deny recorded: this queued merge would be frozen under enforce"
        );
    }

    if enforcement.enforces() && would_deny {
        return QueueExecutionDecision::Freeze { reason };
    }

    execute_as(requester.unwrap_or("system"))
}

/// Cedar ids for the queue's decision, or `None` when the requester is not a
/// valid entity id (treated as would-deny rather than as an allow).
fn queue_euids(requester: &str) -> Option<(SaturnEUid, SaturnEUid)> {
    let user_type = EntityTypeName::from_str("User").ok()?;
    let action_type = EntityTypeName::from_str("Action").ok()?;
    Some((
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            user_type,
            EntityId::from_str(requester).ok()?,
        )),
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            action_type,
            EntityId::from_str("approveMergeRequest").ok()?,
        )),
    ))
}

/// The single emit site of the `merge_queue_authz_frozen` alert (UN-25).
///
/// Synchronous and process-local by design: an `error` log cannot fail, so
/// alerting can never interfere with the freeze transaction that precedes it.
/// Keeping it in one function also means the field set is one thing to read —
/// and one thing to assert.
pub fn emit_authz_frozen_alert(cl_link: &str, requester: Option<&str>, reason: &str) {
    tracing::error!(
        event = "merge_queue_authz_frozen",
        cl_link = %cl_link,
        requester = %requester.unwrap_or("<none>"),
        reason = %reason,
        "merge queue item frozen: authorization could not be decided"
    );
}

/// Message stored on a queue item frozen by UN-25.
///
/// It names the condition for a successful retry, because a frozen item is not
/// a failed merge: nothing was wrong with the change, only with the ability to
/// decide it.
pub fn authz_freeze_message(reason: &str) -> String {
    format!(
        "merge frozen: authorization unavailable ({reason}). \
         The change was not rejected and no merge was attempted. \
         Retry this item once authorization is available again; \
         the recorded requester is reused as the authorization subject."
    )
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
// Key for storing the current CLA content in the object storage
const CLA_CONTENT_OBJECT_KEY: &str = "cla/content/current.txt";

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

/// `MonoServiceLogic` is a helper struct for `MonoApiService` containing stateless logic.
///
/// It encapsulates the pure logic methods of `MonoApiService` that do not depend on
/// databases, caches, or other external state, making them easy to unit test and reuse.
///
/// Usage:
/// - Methods in `MonoApiService` can delegate their core logic to these static methods.
/// - In tests, you can call `MonoServiceLogic` methods directly without initializing
///   `MonoApiService` or a database.
pub struct MonoServiceLogic;

static PATH_NOT_EXIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Path '([^']+)' not exist").expect("PATH_NOT_EXIST_RE must be valid")
});

impl MonoServiceLogic {
    pub fn clean_path_str(path: &str) -> String {
        let s = path.trim_end_matches('/');
        if s.is_empty() {
            "/".to_string()
        } else {
            s.to_string()
        }
    }

    /// Normalize and validate repository path.
    ///
    /// Rules: trim; reject empty or whitespace-only (validation error). Reject `..`, backslash,
    /// Windows drive letters (e.g. `C:`), and paths starting with `:`. Strip trailing `/`;
    /// input consisting only of slashes becomes `"/"`. Collapse middle repeated slashes and
    /// remove `.` segments (e.g. `//project//foo` -> `/project/foo`, `project/./foo` -> `/project/foo`).
    /// Paths that consist only of `.` and slashes (e.g. `"."`, `"./"`) are rejected so they do not
    /// silently resolve to root. Non-empty result gets a leading `"/"` if missing. Result matches
    /// mega_refs.path format.
    pub fn normalize_repo_path(path: &str) -> Result<String, MegaError> {
        let s = path.trim();
        if s.is_empty() {
            return Err(MegaError::Buck(BuckError::ValidationError(
                "Path cannot be empty".to_string(),
            )));
        }
        if s.contains("..") {
            return Err(MegaError::Buck(BuckError::ValidationError(format!(
                "Path traversal not allowed: {}",
                s
            ))));
        }
        if s.contains('\\') {
            return Err(MegaError::Buck(BuckError::ValidationError(format!(
                "Path must use '/' separator: {}",
                s
            ))));
        }
        if s.len() >= 2 {
            let mut chars = s.chars();
            if let (Some(c1), Some(':')) = (chars.next(), chars.next())
                && c1.is_ascii_alphabetic()
            {
                return Err(MegaError::Buck(BuckError::ValidationError(format!(
                    "Absolute path not allowed (Windows drive letter detected): {}",
                    s
                ))));
            }
        }
        if s.starts_with(':') {
            return Err(MegaError::Buck(BuckError::ValidationError(
                "Path must not start with ':'".to_string(),
            )));
        }
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            return Ok("/".to_string());
        }
        let parts: Vec<&str> = s
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        let s = parts.join("/");
        if s.is_empty() {
            return Err(MegaError::Buck(BuckError::ValidationError(
                "Path cannot be empty or consist only of '.' segments".to_string(),
            )));
        }
        Ok(format!("/{}", s))
    }

    /// Enumerate candidate repo roots from the deepest directory back to `/`.
    pub fn repo_root_candidates(path: &Path) -> Vec<String> {
        let mut current = PathBuf::from("/").join(path);
        let mut candidates = Vec::new();

        loop {
            candidates.push(Self::clean_path_str(&current.to_string_lossy()));
            if !current.pop() {
                break;
            }
        }

        candidates
    }

    pub fn subtree_ref_path(path: &Path) -> Result<String, MegaError> {
        Self::normalize_repo_path(&path.display().to_string())
    }

    pub fn update_tree_hash(
        tree: Arc<Tree>,
        name: &str,
        target_hash: ObjectHash,
    ) -> Result<Tree, GitError> {
        let index = tree
            .tree_items
            .iter()
            .position(|item| item.name == name)
            .ok_or_else(|| GitError::CustomError(format!("Tree item '{name}' not found")))?;
        let mut items = tree.tree_items.clone();
        items[index].id = target_hash;
        // Preserve stored item order: Tree::from_tree_items hashes in the given
        // sequence and does not sort. Re-sorting here would change review-path
        // tree ids (hard constraint 8). Sorting belongs only on the new
        // insert-or-replace create primitives via tree_from_items_checked.
        Tree::from_tree_items(items).map_err(|_| GitError::CustomError("Invalid tree".to_string()))
    }

    /// Insert or replace a named tree entry, then rebuild with Git tree sort order.
    ///
    /// - Existing same-name entry with matching mode: replace its `id` deterministically.
    /// - Existing same-name entry with a different mode: diagnostic error (no silent
    ///   blob↔tree conversion).
    /// - Missing entry: append a new `TreeItem`.
    /// - Duplicate names inside the resulting item set: diagnostic error.
    pub fn insert_or_replace_tree_item(
        tree: &Tree,
        name: &str,
        target_hash: ObjectHash,
        mode: TreeItemMode,
    ) -> Result<Tree, GitError> {
        let mut items = tree.tree_items.clone();
        if let Some(index) = items.iter().position(|item| item.name == name) {
            if items[index].mode != mode {
                return Err(GitError::CustomError(format!(
                    "Tree item '{name}' exists with mode {:?}, refusing mode change to {mode:?}",
                    items[index].mode
                )));
            }
            items[index].id = target_hash;
        } else {
            items.push(TreeItem {
                mode,
                id: target_hash,
                name: name.to_owned(),
            });
        }
        Self::tree_from_items_checked(items)
    }

    /// Like [`Self::update_tree_hash`], but inserts a `TreeItemMode::Tree` entry when missing.
    pub fn insert_or_replace_tree_hash(
        tree: Arc<Tree>,
        name: &str,
        target_hash: ObjectHash,
    ) -> Result<Tree, GitError> {
        Self::insert_or_replace_tree_item(&tree, name, target_hash, TreeItemMode::Tree)
    }

    /// Build a [`Tree`] after rejecting duplicate names and applying Git sort order.
    pub fn tree_from_items_checked(mut items: Vec<TreeItem>) -> Result<Tree, GitError> {
        let mut seen = std::collections::HashSet::with_capacity(items.len());
        for item in &items {
            if !seen.insert(item.name.clone()) {
                return Err(GitError::CustomError(format!(
                    "Duplicate tree item name '{}'",
                    item.name
                )));
            }
        }
        crate::jupiter::utils::converter::sort_git_tree_items(&mut items);
        Tree::from_tree_items(items).map_err(|_| GitError::CustomError("Invalid tree".to_string()))
    }

    /// Insert `leaf` at the end of `chain` path components, creating missing Tree
    /// entries for `missing_components`.
    ///
    /// `chain` is `[root, ..., deepest_existing]` (at least the root). Names linking
    /// consecutive chain entries are `existing_child_names` (length `chain.len() - 1`).
    /// `missing_components` must be non-empty and names the path under the deepest
    /// existing tree through the leaf slot whose content is `leaf`.
    pub fn ensure_tree_path_with_chain(
        chain: &[Tree],
        existing_child_names: &[&str],
        missing_components: &[&str],
        leaf: Tree,
    ) -> Result<(Tree, Vec<Tree>), GitError> {
        if chain.is_empty() {
            return Err(GitError::CustomError(
                "ensure_tree_path_with_chain requires a non-empty chain".into(),
            ));
        }
        if existing_child_names.len() + 1 != chain.len() {
            return Err(GitError::CustomError(
                "existing_child_names length must be chain.len() - 1".into(),
            ));
        }
        if missing_components.is_empty() {
            return Err(GitError::CustomError(
                "missing_components must name the leaf slot to insert or replace".into(),
            ));
        }

        let mut produced = Vec::new();
        produced.push(leaf.clone());
        let mut child_hash = leaf.id;
        let mut child_name = missing_components[missing_components.len() - 1];

        // Build brand-new intermediate trees for all but the first missing component
        // (the first missing attaches into the deepest existing tree).
        for name in missing_components.iter().rev().skip(1) {
            let intermediate = Self::tree_from_items_checked(vec![TreeItem {
                mode: TreeItemMode::Tree,
                id: child_hash,
                name: child_name.to_owned(),
            }])?;
            child_hash = intermediate.id;
            child_name = name;
            produced.push(intermediate);
        }

        // Walk existing chain upward, insert-or-replace at each level.
        let mut next_hash = child_hash;
        let mut next_name = child_name;
        let mut new_root = chain[0].clone();
        for (idx, tree) in chain.iter().enumerate().rev() {
            let updated =
                Self::insert_or_replace_tree_item(tree, next_name, next_hash, TreeItemMode::Tree)?;
            next_hash = updated.id;
            produced.push(updated.clone());
            if idx == 0 {
                new_root = updated;
                break;
            }
            next_name = existing_child_names[idx - 1];
        }

        Ok((new_root, produced))
    }

    /// Update parent trees along the given update chain with the new child tree hash.
    /// This function prepares all updated trees and their associated ref updates.
    /// Trees that do not depend on each other (e.g., sibling directories) can be updated in parallel.
    /// No new commits are created; only tree objects and ref updates are produced.
    pub fn build_result_by_chain(
        mut path: PathBuf,
        mut update_chain: Vec<Arc<Tree>>,
        mut updated_tree_hash: ObjectHash,
    ) -> Result<TreeUpdateResult, GitError> {
        let mut updated_trees = Vec::new();
        let mut ref_updates = Vec::new();
        let mut path_str = path.to_string_lossy().to_string();

        loop {
            let clean_path = MonoServiceLogic::clean_path_str(&path_str);
            let ref_path = if clean_path == "/" || clean_path.starts_with('/') {
                clean_path
            } else {
                format!("/{clean_path}")
            };

            ref_updates.push(RefUpdate {
                path: ref_path,
                tree_id: updated_tree_hash,
            });

            if update_chain.is_empty() {
                break;
            }

            let cloned_path = path.clone();
            let name = cloned_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| GitError::CustomError("Invalid path".into()))?;
            path.pop();
            path_str = path.to_string_lossy().to_string();

            let tree = update_chain
                .pop()
                .ok_or_else(|| GitError::CustomError("Empty update chain".into()))?;

            let new_tree = MonoServiceLogic::update_tree_hash(tree, name, updated_tree_hash)?;
            updated_tree_hash = new_tree.id;
            updated_trees.push(new_tree);
        }

        Ok(TreeUpdateResult {
            updated_trees,
            ref_updates,
        })
    }

    /// Like [`Self::build_result_by_chain`], but inserts a missing Tree entry
    /// (create semantics) instead of requiring the child name to already exist.
    pub fn build_result_by_chain_inserting(
        mut path: PathBuf,
        mut update_chain: Vec<Arc<Tree>>,
        mut updated_tree_hash: ObjectHash,
    ) -> Result<TreeUpdateResult, GitError> {
        let mut updated_trees = Vec::new();
        let mut ref_updates = Vec::new();
        let mut path_str = path.to_string_lossy().to_string();

        loop {
            let clean_path = MonoServiceLogic::clean_path_str(&path_str);
            let ref_path = if clean_path == "/" || clean_path.starts_with('/') {
                clean_path
            } else {
                format!("/{clean_path}")
            };

            ref_updates.push(RefUpdate {
                path: ref_path,
                tree_id: updated_tree_hash,
            });

            if update_chain.is_empty() {
                break;
            }

            let cloned_path = path.clone();
            let name = cloned_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| GitError::CustomError("Invalid path".into()))?;
            path.pop();
            path_str = path.to_string_lossy().to_string();

            let tree = update_chain
                .pop()
                .ok_or_else(|| GitError::CustomError("Empty update chain".into()))?;

            let new_tree =
                MonoServiceLogic::insert_or_replace_tree_hash(tree, name, updated_tree_hash)?;
            updated_tree_hash = new_tree.id;
            updated_trees.push(new_tree);
        }

        Ok(TreeUpdateResult {
            updated_trees,
            ref_updates,
        })
    }

    /// Processes all ref updates by creating new commits and updating refs accordingly.
    ///
    /// This method abstracts the entire loop logic for processing ref updates,
    /// creating commits for each update and managing the refs that need to be updated.
    pub fn process_ref_updates(
        result: &TreeUpdateResult,
        refs: &[mega_refs::Model],
        commit_msg: &str,
        commits: &mut Vec<Commit>,
        updates: &mut Vec<RefUpdateData>,
        new_commit_id: &mut String,
    ) -> Result<(), GitError> {
        for update in &result.ref_updates {
            // GAP-07 / ADR-MC-01 coupling — re-review both before changing
            // this lookup: the synthesized trunk commit's parent is the FIRST
            // candidate ref on the update's path, and
            // `get_refs_for_paths_and_cls` sorts by `ref_name` ascending, so
            // `refs/cl/<cl.link>` (when present in the candidate set) sorts
            // before `refs/heads/main` and the parent becomes the CL tip
            // instead of the pre-merge main head. Under GAP-07's naming
            // divergence the push-side CL ref name differs from the CL row's
            // link, so the candidate set contains no matching CL ref for a
            // never-updated CL and the parent lands on main's head — the MC-06
            // e2e pins that shape. A GAP-07 same-source fix makes the CL ref
            // always match, flipping every merge parent to the chain tip and
            // reversing ADR-MC-01's "chain commits never reach trunk"
            // invariant.
            if let Some(p_ref) = refs.iter().find(|r| r.path == update.path) {
                let commit = Commit::from_tree_id(
                    update.tree_id,
                    vec![
                        ObjectHash::from_hex_for_kind(get_hash_kind(), &p_ref.ref_commit_hash)
                            .unwrap(),
                    ],
                    commit_msg,
                );
                let commit_id = commit.id.to_string();
                *new_commit_id = commit_id.clone();

                commits.push(commit);

                let mut push_update = |ref_name: &str| {
                    updates.push(RefUpdateData {
                        path: p_ref.path.clone(),
                        ref_name: ref_name.to_string(),
                        commit_id: commit_id.to_string(),
                        tree_hash: update.tree_id.to_string(),
                    });
                };

                push_update(&p_ref.ref_name);
                if p_ref.ref_name.starts_with("refs/cl/") {
                    push_update(MEGA_BRANCH_NAME);
                }
            }
        }

        Ok(())
    }

    /// Processes ref updates but only for CL refs; never touches main and supports chaining parents.
    ///
    /// MC-09: every synthesized commit is signed with the server key inside
    /// this function — after construction, before its id is read (the parent
    /// chain via `prev_parent` likewise references the signed id) — so
    /// `commits`, `updates` and `new_commit_id` only ever carry the final
    /// signed hash. Callers must not post-process the signature. The signing
    /// precheck (vault read + key parse) runs before any output is produced,
    /// and the caller persists refs/commits/trees only after this returns, so
    /// a signing failure leaves zero persisted side effects.
    // too_many_arguments: the three output vecs mirror `process_ref_updates`;
    // `signing` is the MC-09 capability parameter the card requires.
    #[allow(clippy::too_many_arguments)]
    pub async fn process_ref_updates_cl_only(
        result: &TreeUpdateResult,
        cl_ref: &mega_refs::Model,
        commit_msg: &str,
        parent_override: Option<ObjectHash>,
        signing: &ServerSigningContext,
        commits: &mut Vec<Commit>,
        updates: &mut Vec<RefUpdateData>,
        new_commit_id: &mut String,
    ) -> Result<(), GitError> {
        let signing_key = signing
            .active_key()
            .await
            .map_err(|e| GitError::CustomError(format!("server signing unavailable: {e}")))?;

        let mut prev_parent: Option<ObjectHash> = None;

        for update in &result.ref_updates {
            let parent_ids = if let Some(prev) = prev_parent {
                vec![prev]
            } else if let Some(po) = parent_override {
                vec![po]
            } else {
                vec![
                    ObjectHash::from_hex_for_kind(get_hash_kind(), &cl_ref.ref_commit_hash)
                        .map_err(|_| {
                            GitError::CustomError(format!(
                                "Invalid CL ref hash: {}",
                                cl_ref.ref_commit_hash
                            ))
                        })?,
                ]
            };

            let commit = Commit::from_tree_id(update.tree_id, parent_ids, commit_msg);
            let commit = signing.sign_commit(&signing_key, &commit).map_err(|e| {
                GitError::CustomError(format!("failed to sign synthesized commit: {e}"))
            })?;
            let commit_id = commit.id;
            *new_commit_id = commit_id.to_string();

            commits.push(commit.clone());
            prev_parent = Some(commit_id);

            updates.push(RefUpdateData {
                path: cl_ref.path.clone(),
                ref_name: cl_ref.ref_name.clone(),
                commit_id: commit_id.to_string(),
                tree_hash: update.tree_id.to_string(),
            });
        }

        Ok(())
    }

    /// Maps each TreeItem in a Tree to its corresponding Commit, if available.
    ///
    /// # Arguments
    ///
    /// * `tree` - The tree containing the TreeItems to map.
    /// * `item_to_commit_id` - Mapping from TreeItem id (as string) to commit id.
    /// * `commit_map` - Mapping from commit id to Commit object.
    ///
    /// # Returns
    ///
    /// A HashMap where each TreeItem maps to an Option<Commit>. If a commit cannot
    /// be found, the value is None.
    pub fn map_tree_items_to_commits(
        tree: Tree,
        item_to_commit_id: &HashMap<String, String>,
        commit_map: &HashMap<String, Commit>,
    ) -> HashMap<TreeItem, Option<Commit>> {
        let mut result: HashMap<TreeItem, Option<Commit>> = HashMap::new();

        for item in tree.tree_items {
            if let Some(commit_id) = item_to_commit_id.get(&item.id.to_string()) {
                let commit = commit_map.get(commit_id).cloned();
                if commit.is_none() {
                    tracing::warn!(
                        item_name = %item.name,
                        item_mode = ?item.mode,
                        commit_id = %commit_id,
                        "failed fetch from commit map"
                    );
                }
                result.insert(item, commit);
            } else {
                result.insert(item, None);
            }
        }
        result
    }
}

#[async_trait]
impl ApiHandler for MonoApiService {
    fn get_context(&self) -> Storage {
        self.storage.clone()
    }

    fn object_cache(&self) -> &GitObjectCache {
        &self.git_object_cache
    }

    async fn get_root_commit(&self) -> Result<Commit, MegaError> {
        let storage = self.storage.mono_storage();
        let refs = storage.get_main_ref("/").await.unwrap().unwrap();
        self.get_commit_by_hash(&refs.ref_commit_hash).await
    }

    /// Save file edit in monorepo with optimistic concurrency check
    async fn save_file_edit(
        &self,
        payload: EditFilePayload,
        requester: Option<String>,
    ) -> Result<EditFileResult, GitError> {
        let file_path = PathBuf::from("/").join(PathBuf::from(&payload.path));
        let parent_path = file_path
            .parent()
            .ok_or_else(|| GitError::CustomError("Invalid file path".to_string()))?;
        let cl_root_path = MonoServiceLogic::subtree_ref_path(parent_path)
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let build_repo_path = match edit_utils::resolve_build_repo_root(
            &self.storage,
            &cl_root_path,
        )
        .await
        {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(
                    repo_path = %cl_root_path,
                    "Failed to resolve build repo root for edit, fallback to CL subtree root: {}",
                    e
                );
                cl_root_path.clone()
            }
        };

        let tip_path = if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            self.resolve_trunk_land_path(&cl_root_path).await?
        } else {
            build_repo_path.clone()
        };

        let parent_tree = tree_ops::search_tree_by_path(self, parent_path, None)
            .await?
            .ok_or(GitError::CustomError(format!(
                "invalid repo_path {}, Parent tree not found",
                cl_root_path
            )))?;

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| GitError::CustomError("Invalid file name".to_string()))?;

        let _current_item = parent_tree
            .tree_items
            .iter()
            .find(|x| x.name == file_name && x.mode == TreeItemMode::Blob)
            .ok_or_else(|| GitError::CustomError("[code:404] File not found".to_string()))?;

        // Create new blob and build update result up to root
        let new_blob = Blob::from_content(&payload.content);
        let new_tree = MonoServiceLogic::update_tree_hash(
            parent_tree.into(),
            file_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| GitError::CustomError("Invalid path".into()))?,
            new_blob.id,
        )?;

        let mut update_chain = self.search_tree_for_update(parent_path).await?;
        let _target_tree = update_chain
            .pop()
            .ok_or_else(|| GitError::CustomError("Empty update chain".to_string()))?;
        let update_result = MonoServiceLogic::build_result_by_chain(
            parent_path.to_path_buf(),
            update_chain,
            new_tree.id,
        )?;
        let target_tree_id = Self::ref_update_tree_id_for_path(&update_result, &tip_path)
            .ok_or_else(|| {
                GitError::CustomError(format!(
                    "Missing updated tree for build repo root {tip_path}"
                ))
            })?;

        let src_commit = edit_utils::get_repo_main_latest_commit(&self.storage, &tip_path).await?;
        let dst_commit = Commit::from_tree_id(
            target_tree_id,
            vec![
                ObjectHash::from_hex_for_kind(get_hash_kind(), &src_commit.id.to_string())
                    .map_err(|e| {
                        GitError::CustomError(format!("Invalid commit hash {}: {e}", src_commit.id))
                    })?,
            ],
            &payload.commit_message,
        );
        let new_commit_id = dst_commit.id.to_string();

        let username = payload
            .author_username
            .clone()
            .unwrap_or("Anonymous".to_string());

        self.storage
            .mono_service
            .mono_storage
            .save_mega_commits(vec![dst_commit], None)
            .await?;

        let mut all_trees = vec![new_tree];
        all_trees.extend(update_result.updated_trees);
        let save_trees: Vec<mega_tree::ActiveModel> = all_trees
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&new_commit_id);
                tree_model.into()
            })
            .collect();

        self.storage
            .mono_service
            .mono_storage
            .batch_save_model(save_trees)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        self.storage
            .mono_service
            .save_blobs(&new_commit_id, vec![new_blob.clone()])
            .await?;

        if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            let old_id = src_commit.id.to_string();
            let payload_n1 = PushPayload {
                commits: vec![new_commit_id.clone()],
                fork_base: Some(old_id.clone()),
                n: 1,
            };
            let landed = land_api_tip_push(
                &self.storage,
                self.git_object_cache.clone(),
                &tip_path,
                &old_id,
                &new_commit_id,
                requester,
                &payload_n1,
            )
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
            return Ok(EditFileResult {
                commit_id: landed,
                new_oid: new_blob.id.to_string(),
                path: tip_path,
                cl_link: None,
            });
        }

        let editor = OneditCodeEdit::from(
            &build_repo_path,
            MEGA_BRANCH_NAME
                .strip_prefix("refs/heads/")
                .unwrap_or(MEGA_BRANCH_NAME),
            &src_commit.id.to_string(),
            self,
            self.storage.mono_storage(),
        );
        let cl = editor
            .find_or_create_cl_for_edit(
                &self.storage,
                &editor,
                payload.mode,
                &new_commit_id,
                &username,
            )
            .await?;

        if !payload.skip_build {
            self.trigger_build_for_cl(&editor, &cl, &username).await?;
        }

        Ok(EditFileResult {
            commit_id: new_commit_id,
            new_oid: new_blob.id.to_string(),
            path: build_repo_path,
            cl_link: Some(cl.link),
        })
    }

    /// Creates a new file or directory in the monorepo based on the provided file information.
    ///
    /// # Arguments
    ///
    /// * `entry_info` - Information about the file or directory to create.
    /// * `requester` - Trunk push_auth identity; review passes `None`.
    ///
    /// # Returns
    ///
    /// Returns commit metadata on success, or a `GitError` on failure.
    async fn create_monorepo_entry(
        &self,
        entry_info: CreateEntryInfo,
        requester: Option<String>,
    ) -> Result<CreateEntryResult, GitError> {
        let storage = self.storage.mono_storage();
        let CreateEntryUpdate {
            update_result,
            blob,
            entry_oid,
            repo_path,
            mut save_trees,
        } = self.prepare_create_entry_update(&entry_info).await?;

        let repo_path_str = MonoServiceLogic::subtree_ref_path(&repo_path)
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let build_repo_path = match edit_utils::resolve_build_repo_root(
            &self.storage,
            &repo_path_str,
        )
        .await
        {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(
                    repo_path = %repo_path_str,
                    "Failed to resolve build repo root for create entry, fallback to CL subtree root: {}",
                    e
                );
                repo_path_str.clone()
            }
        };

        let tip_path = if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            self.resolve_trunk_land_path(&repo_path_str).await?
        } else {
            build_repo_path.clone()
        };

        let src_commit = edit_utils::get_repo_main_latest_commit(&self.storage, &tip_path).await?;
        let base_commit =
            ObjectHash::from_hex_for_kind(get_hash_kind(), &src_commit.id.to_string()).map_err(
                |e| GitError::CustomError(format!("Invalid commit hash {}: {e}", src_commit.id)),
            )?;
        let target_tree_id = Self::ref_update_tree_id_for_path(&update_result, &tip_path)
            .ok_or_else(|| {
                GitError::CustomError(format!(
                    "Missing updated tree for build repo root {tip_path}"
                ))
            })?;
        let dst_commit =
            Commit::from_tree_id(target_tree_id, vec![base_commit], &entry_info.commit_msg());
        let new_commit_id = dst_commit.id.to_string();

        let username = entry_info
            .author_username
            .clone()
            .unwrap_or("Anonymous".to_string());

        let new_oid = entry_oid.to_string();

        let mut all_trees = update_result.updated_trees;
        all_trees.append(&mut save_trees);
        let save_trees: Vec<mega_tree::ActiveModel> = all_trees
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&new_commit_id);
                tree_model.into()
            })
            .collect();
        self.storage
            .mono_service
            .save_blobs(&new_commit_id, vec![blob])
            .await?;

        storage
            .batch_save_model(save_trees)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        self.storage
            .mono_service
            .mono_storage
            .save_mega_commits(vec![dst_commit], None)
            .await?;

        let entry_path = Self::build_entry_path(&entry_info.path, &entry_info.name);

        if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            let old_id = src_commit.id.to_string();
            let payload_n1 = PushPayload {
                commits: vec![new_commit_id.clone()],
                fork_base: Some(old_id.clone()),
                n: 1,
            };
            let landed = land_api_tip_push(
                &self.storage,
                self.git_object_cache.clone(),
                &tip_path,
                &old_id,
                &new_commit_id,
                requester,
                &payload_n1,
            )
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
            return Ok(CreateEntryResult {
                commit_id: landed,
                new_oid,
                path: entry_path,
                cl_link: None,
            });
        }

        let editor = OneditCodeEdit::from(
            &build_repo_path,
            MEGA_BRANCH_NAME
                .strip_prefix("refs/heads/")
                .unwrap_or(MEGA_BRANCH_NAME),
            &src_commit.id.to_string(),
            self,
            self.storage.mono_storage(),
        );
        let cl = editor
            .find_or_create_cl_for_edit(
                &self.storage,
                &editor,
                entry_info.mode.clone(),
                &new_commit_id,
                &username,
            )
            .await?;

        if !entry_info.skip_build {
            self.trigger_build_for_cl(&editor, &cl, &username).await?;
        }

        Ok(CreateEntryResult {
            commit_id: new_commit_id,
            new_oid,
            path: entry_path,
            cl_link: Some(cl.link),
        })
    }

    fn strip_relative(&self, path: &Path) -> Result<PathBuf, MegaError> {
        Ok(path.to_path_buf())
    }

    async fn get_root_tree(&self, refs: Option<&str>) -> Result<Tree, MegaError> {
        let refs = refs.unwrap_or("").trim();

        // condition 1: empty refs, return default root tree
        if refs.is_empty() {
            let storage = self.storage.mono_storage();
            let refs = storage.get_main_ref("/").await.unwrap().unwrap();
            return self.get_tree_by_hash(&refs.ref_tree_hash).await;
        }

        // condition 2: commit hash
        if refs.len() == 40 && refs.chars().all(|c| c.is_ascii_hexdigit()) {
            let commit = self.get_commit_by_hash(refs).await?;
            return self.get_tree_by_hash(&commit.tree_id.to_string()).await;
        }

        // condition 3: tag name
        if let Ok(Some(tag)) = self.get_tag(None, refs.to_string()).await {
            let commit = self.get_commit_by_hash(&tag.object_id).await?;
            return self.get_tree_by_hash(&commit.tree_id.to_string()).await;
        }

        // condition 4: invalid refs
        Err(MegaError::Other(format!(
            "Invalid refs: '{}' is not a valid commit hash or tag",
            refs
        )))
    }

    async fn get_tree_by_hash(&self, hash: &str) -> Result<Tree, MegaError> {
        let model = self
            .storage
            .mono_storage()
            .get_tree_by_hash(hash)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("tree not found: {}", hash)))?;
        Ok(Tree::from_mega_model(model))
    }

    async fn get_commit_by_hash(&self, hash: &str) -> Result<Commit, MegaError> {
        let model = self
            .storage
            .mono_storage()
            .get_commit_by_hash(hash)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("commit not found: {}", hash)))?;
        Ok(Commit::from_mega_model(model))
    }

    async fn item_to_commit_map(
        &self,
        path: PathBuf,
        reference: Option<&str>,
    ) -> Result<HashMap<TreeItem, Option<Commit>>, GitError> {
        match tree_ops::search_tree_by_path(self, &path, reference).await? {
            Some(tree) => {
                let mut item_to_commit = HashMap::new();

                let storage = self.storage.mono_storage();
                let tree_hashes = tree
                    .tree_items
                    .iter()
                    .filter(|x| x.mode == TreeItemMode::Tree)
                    .map(|x| x.id.to_string())
                    .collect();
                let trees = storage.get_trees_by_hashes(tree_hashes).await.unwrap();
                for tree in trees {
                    // Skip invalid/empty commit ids to avoid noise and incorrect mapping
                    if !tree.commit_id.is_empty() {
                        item_to_commit.insert(tree.tree_id, tree.commit_id);
                    }
                }

                let blob_hashes = tree
                    .tree_items
                    .iter()
                    .filter(|x| x.mode == TreeItemMode::Blob)
                    .map(|x| x.id.to_string())
                    .collect();
                let blobs = storage.get_mega_blobs_by_hashes(blob_hashes).await.unwrap();
                for blob in blobs {
                    if !blob.commit_id.is_empty() {
                        item_to_commit.insert(blob.blob_id, blob.commit_id);
                    }
                }

                let commit_ids: HashSet<String> = item_to_commit.values().cloned().collect();
                let commits = self
                    .get_commits_by_hashes(commit_ids.into_iter().collect())
                    .await
                    .unwrap();

                let commit_map: HashMap<String, Commit> =
                    commits.into_iter().map(|x| (x.id.to_string(), x)).collect();

                Ok(MonoServiceLogic::map_tree_items_to_commits(
                    tree,
                    &item_to_commit,
                    &commit_map,
                ))
            }
            None => Ok(HashMap::new()),
        }
    }

    async fn get_commits_by_hashes(&self, c_hashes: Vec<String>) -> Result<Vec<Commit>, GitError> {
        let commits = self
            .storage
            .mono_storage()
            .get_commits_by_hashes(&c_hashes)
            .await
            .unwrap();
        Ok(commits.into_iter().map(Commit::from_mega_model).collect())
    }

    // helper to convert mega_tag model into TagInfo (defined on MonoApiService below)
    async fn create_tag(
        &self,
        repo_path: Option<String>,
        name: String,
        target: Option<String>,
        tagger_name: Option<String>,
        tagger_email: Option<String>,
        message: Option<String>,
    ) -> Result<TagInfo, GitError> {
        let mono_storage = self.storage.mono_storage();

        let is_annotated = message.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
        let tagger_info = match (tagger_name, tagger_email) {
            (Some(n), Some(e)) => format!("{} <{}>", n, e),
            (Some(n), None) => n,
            (None, Some(e)) => e,
            (None, None) => "unknown".to_string(),
        };

        // validate target commit presence
        self.validate_target_commit_mono(target.as_ref()).await?;

        let full_ref = format!("refs/tags/{}", name.clone());

        // Prevent duplicate tag/ref creation
        match mono_storage.get_tag_by_name(&name).await {
            Ok(Some(_)) => {
                return Err(GitError::CustomError(format!(
                    "[code:400] Tag '{}' already exists",
                    name
                )));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("DB error while checking tag existence: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        }

        if let Ok(Some(_)) = mono_storage.get_ref_by_name(&full_ref).await {
            return Err(GitError::CustomError(format!(
                "[code:400] Tag '{}' already exists",
                name
            )));
        }

        if is_annotated {
            return self
                .create_annotated_tag_mono(
                    repo_path.clone(),
                    name.clone(),
                    target.clone(),
                    tagger_info.clone(),
                    message.clone(),
                    full_ref.clone(),
                )
                .await;
        }

        // lightweight
        self.create_lightweight_tag_mono(
            repo_path.clone(),
            name.clone(),
            target.clone(),
            tagger_info.clone(),
            full_ref.clone(),
        )
        .await
    }

    async fn list_tags(
        &self,
        repo_path: Option<String>,
        pagination: Pagination,
    ) -> Result<(Vec<TagInfo>, u64), GitError> {
        let mono_storage = self.storage.mono_storage();
        // annotated tags from DB (paged)
        let (annotated_page, annotated_total) =
            match mono_storage.get_tags_by_page(pagination.clone()).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("DB error while listing tags: {}", e);
                    return Err(GitError::CustomError("[code:500] DB error".to_string()));
                }
            };

        let mut result: Vec<TagInfo> = annotated_page
            .into_iter()
            .map(|t| self.tag_model_to_info(t))
            .collect();

        // lightweight refs from refs table under path
        let repo_path = repo_path.as_deref().unwrap_or("/");
        let mut lightweight_refs: Vec<TagInfo> = vec![];
        if let Ok(refs) = mono_storage.get_all_refs(repo_path, false).await {
            for r in refs {
                if r.ref_name.starts_with("refs/tags/") {
                    let tag_name = r.ref_name.trim_start_matches("refs/tags/").to_string();
                    if result.iter().any(|t| t.name == tag_name) {
                        continue;
                    }
                    lightweight_refs.push(TagInfo {
                        name: tag_name.clone(),
                        tag_id: r.ref_commit_hash.clone(),
                        object_id: r.ref_commit_hash.clone(),
                        object_type: "commit".to_string(),
                        tagger: "".to_string(),
                        message: "".to_string(),
                        created_at: r.created_at.and_utc().to_rfc3339(),
                    });
                }
            }
        }

        let total = annotated_total + lightweight_refs.len() as u64;
        let per_page = if pagination.per_page == 0 {
            20
        } else {
            pagination.per_page
        } as usize;
        if result.len() < per_page {
            let need = per_page - result.len();
            for r in lightweight_refs.into_iter().take(need) {
                result.push(r);
            }
        }

        Ok((result, total))
    }

    async fn get_tag(
        &self,
        repo_path: Option<String>,
        name: String,
    ) -> Result<Option<TagInfo>, GitError> {
        let mono_storage = self.storage.mono_storage();
        // check annotated DB first
        match mono_storage.get_tag_by_name(&name).await {
            Ok(Some(tag)) => return Ok(Some(self.tag_model_to_info(tag))),
            Ok(None) => {}
            Err(e) => {
                tracing::error!("DB error while getting tag: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        }
        // check refs for lightweight tag
        let _repo_path = repo_path.unwrap_or_else(|| "/".to_string());
        let full_ref = format!("refs/tags/{}", name.clone());
        if let Ok(Some(r)) = mono_storage.get_ref_by_name(&full_ref).await {
            return Ok(Some(TagInfo {
                name: name.clone(),
                tag_id: r.ref_commit_hash.clone(),
                object_id: r.ref_commit_hash.clone(),
                object_type: "commit".to_string(),
                tagger: "".to_string(),
                message: "".to_string(),
                created_at: r.created_at.and_utc().to_rfc3339(),
            }));
        }
        Ok(None)
    }

    async fn delete_tag(&self, repo_path: Option<String>, name: String) -> Result<(), GitError> {
        let mono_storage = self.storage.mono_storage();
        // check annotated in DB first
        match mono_storage.get_tag_by_name(&name).await {
            Ok(Some(_tag)) => {
                // remove ref if exists
                let full_ref = format!("refs/tags/{}", name.clone());
                if let Ok(Some(r)) = mono_storage.get_ref_by_name(&full_ref).await {
                    mono_storage.remove_ref(r).await.map_err(|e| {
                        tracing::error!("Failed to remove ref while deleting annotated tag: {}", e);
                        GitError::CustomError("[code:500] Failed to remove ref".to_string())
                    })?;
                }
                mono_storage.delete_tag_by_name(&name).await.map_err(|e| {
                    tracing::error!("DB delete error when deleting annotated tag: {}", e);
                    GitError::CustomError("[code:500] DB delete error".to_string())
                })?;
                Ok(())
            }
            Ok(None) => {
                // try delete lightweight ref
                let _repo_path = repo_path.unwrap_or_else(|| "/".to_string());
                let full_ref = format!("refs/tags/{}", name.clone());
                // find ref by name and remove
                if let Ok(Some(r)) = mono_storage.get_ref_by_name(&full_ref).await {
                    mono_storage.remove_ref(r).await.map_err(|e| {
                        tracing::error!(
                            "Failed to remove ref while deleting lightweight tag: {}",
                            e
                        );
                        GitError::CustomError("[code:500] Failed to remove ref".to_string())
                    })?;
                    Ok(())
                } else {
                    Err(GitError::CustomError(
                        "[code:404] Tag not found".to_string(),
                    ))
                }
            }
            Err(e) => {
                tracing::error!("DB error while deleting tag: {}", e);
                Err(GitError::CustomError("[code:500] DB error".to_string()))
            }
        }
    }
}

impl MonoApiService {
    async fn prepare_create_entry_update(
        &self,
        entry_info: &CreateEntryInfo,
    ) -> Result<CreateEntryUpdate, GitError> {
        let path = PathBuf::from(&entry_info.path);
        let mut save_trees = vec![];
        let file_content = if entry_info.is_directory {
            None
        } else {
            Some(entry_info.content.as_deref().ok_or_else(|| {
                GitError::CustomError("content is required for file creation".to_string())
            })?)
        };

        // Try to get the update chain for the given path.
        // If the path exists, return an empty missing_parts and prefix.
        // If part of the path does not exist, extract the missing segments (missing_parts),
        // determine the valid existing prefix, and rebuild the update_chain from that prefix.
        let (missing_parts, prefix, mut update_chain) =
            match self.search_tree_for_update(&path).await {
                Ok(chain) => (Vec::new(), "", chain),
                Err(err) => {
                    // If search_tree_for_update failed, try to extract the
                    // portion of the path that does not exist from the
                    // error message. The error message is expected to
                    // contain a substring like: Path '.../missing' not exist
                    // We capture that substring to determine which segments
                    // need to be created.
                    let err_str = err.to_string();
                    let extracted = PATH_NOT_EXIST_RE
                        .captures(&err_str)
                        .map(|caps| caps[1].to_string())
                        .ok_or_else(|| {
                            GitError::CustomError(format!("Path resolution failed: {err_str}"))
                        })?;

                    // missing_parts: the trailing path segments after the
                    // first occurrence of the extracted non-existent path.
                    // Example: entry_info.path = "a/b/c/d" and extracted = "c/d"
                    // Then missing_parts = ["c", "d"]
                    let missing_parts = entry_info
                        .path
                        .find(&extracted)
                        .map(|pos| &entry_info.path[pos..])
                        .map(|sub| sub.split('/').collect::<Vec<_>>())
                        .unwrap_or_default();

                    if missing_parts.is_empty() {
                        return Err(GitError::CustomError(format!(
                            "Missing path segments for '{}': {err_str}",
                            entry_info.path
                        )));
                    }

                    // prefix: the valid existing path before the missing parts.
                    // Using the same example above, prefix = "a/b/"
                    let prefix = entry_info
                        .path
                        .find(&extracted)
                        .map(|pos| &entry_info.path[..pos])
                        .unwrap_or("");

                    // Rebuild the update chain starting from the valid prefix
                    // so subsequent operations only update from that known
                    // existing tree downward.
                    let chain = self.search_tree_for_update(Path::new(prefix)).await?;
                    (missing_parts, prefix, chain)
                }
            };

        let target_items = update_chain
            .pop()
            .ok_or_else(|| GitError::CustomError("Empty update chain".to_string()))?
            .tree_items
            .clone();

        // If there are no missing parts, we are inserting directly into an
        // existing tree. This branch handles both creating a new file or
        // creating a new directory in the target tree.
        let (update_result, blob, entry_oid, repo_path) = if missing_parts.is_empty() {
            let mut target_items = target_items;

            // Check for duplicate
            let is_tree_mode = if entry_info.is_directory {
                TreeItemMode::Tree
            } else {
                TreeItemMode::Blob
            };
            if target_items
                .iter()
                .any(|x| x.mode == is_tree_mode && x.name == entry_info.name)
            {
                return Err(GitError::CustomError("Duplicate name".to_string()));
            }

            // Create a new tree item based on whether it's a directory or file
            let (new_item, blob, entry_oid) = if entry_info.is_directory {
                // For a new directory, create a .gitkeep blob so the
                // directory can be represented as a tree with at least
                // one blob entry. The blob contains a timestamp so it's
                // unique.
                let blob = generate_git_keep_with_timestamp();
                let tree_item = TreeItem {
                    mode: TreeItemMode::Blob,
                    id: blob.id,
                    name: String::from(".gitkeep"),
                };
                let new_dir_tree = Tree::from_tree_items(vec![tree_item]).unwrap();
                save_trees.push(new_dir_tree.clone());
                let entry_oid = new_dir_tree.id;
                (
                    TreeItem {
                        mode: TreeItemMode::Tree,
                        id: new_dir_tree.id,
                        name: entry_info.name.clone(),
                    },
                    blob,
                    entry_oid,
                )
            } else {
                let content = file_content
                    .ok_or_else(|| GitError::CustomError("Missing file content".to_string()))?;
                let blob = Blob::from_content(content);
                let entry_oid = blob.id;
                (
                    TreeItem {
                        mode: TreeItemMode::Blob,
                        id: blob.id,
                        name: entry_info.name.clone(),
                    },
                    blob,
                    entry_oid,
                )
            };

            target_items.push(new_item);
            target_items.sort_by(|a, b| a.name.cmp(&b.name));
            let target_tree = Tree::from_tree_items(target_items).unwrap();
            save_trees.push(target_tree.clone());

            // Build update instructions for parent trees and refs.
            // build_result_by_chain walks the update_chain (parent trees)
            // and prepares the list of updated trees and ref updates
            // that must be applied to persist the change.
            let update_result = MonoServiceLogic::build_result_by_chain(
                if prefix.is_empty() {
                    path.clone()
                } else {
                    PathBuf::from(prefix)
                },
                update_chain,
                target_tree.id,
            )?;
            let repo_path = if prefix.is_empty() {
                path.clone()
            } else {
                PathBuf::from(prefix)
            };
            (update_result, blob, entry_oid, repo_path)
        } else {
            // If missing_parts is not empty, we must create intermediate
            // directories (trees) for each missing segment. This branch
            // constructs the leaf tree first and then wraps it with
            // additional trees for each missing path component up to the
            // existing prefix.
            // Create a new tree item based on whether it's a directory or file
            let (leaf_item, blob, entry_oid) = if entry_info.is_directory {
                // Create .gitkeep blob and an initial tree for the new
                // directory leaf. This represents the directory's own
                // tree object which will be nested under new parent trees.
                let blob = generate_git_keep_with_timestamp();
                let tree_item = TreeItem {
                    mode: TreeItemMode::Blob,
                    id: blob.id,
                    name: String::from(".gitkeep"),
                };
                let new_dir_tree = Tree::from_tree_items(vec![tree_item]).unwrap();
                save_trees.push(new_dir_tree.clone());
                let entry_oid = new_dir_tree.id;
                (
                    TreeItem {
                        mode: TreeItemMode::Tree,
                        id: new_dir_tree.id,
                        name: entry_info.name.clone(),
                    },
                    blob,
                    entry_oid,
                )
            } else {
                let content = file_content
                    .ok_or_else(|| GitError::CustomError("Missing file content".to_string()))?;
                let blob = Blob::from_content(content);
                let entry_oid = blob.id;
                (
                    TreeItem {
                        mode: TreeItemMode::Blob,
                        id: blob.id,
                        name: entry_info.name.clone(),
                    },
                    blob,
                    entry_oid,
                )
            };

            let mut current_tree = Tree::from_tree_items(vec![leaf_item]).unwrap();
            save_trees.push(current_tree.clone());

            // Wrap the leaf tree with trees for each missing parent segment.
            // We iterate the missing parts in reverse (from leaf's parent up
            // to the topmost missing segment) and create a tree object for
            // each level that points to the previously built child tree.
            let missing_len = missing_parts.len();
            for part in missing_parts.iter().rev().take(missing_len - 1) {
                let sub_item = TreeItem {
                    mode: TreeItemMode::Tree,
                    id: current_tree.id,
                    name: part.to_string(),
                };

                current_tree = Tree::from_tree_items(vec![sub_item]).unwrap();
                save_trees.push(current_tree.clone());
            }

            // top_part is the highest-level missing segment (closest to the
            // existing prefix). We'll insert this as a child into the
            // existing target_items collected from the update chain.
            let top_part = missing_parts
                .first()
                .expect("missing_parts is non-empty by branch condition")
                .to_string();
            let top_item = TreeItem {
                mode: TreeItemMode::Tree,
                id: current_tree.id,
                name: top_part.clone(),
            };

            let mut target_items = target_items;

            // Check for duplicate
            if target_items
                .iter()
                .any(|x| x.mode == TreeItemMode::Tree && x.name == top_part)
            {
                return Err(GitError::CustomError("Duplicate name".to_string()));
            }

            target_items.push(top_item);
            target_items.sort_by(|a, b| a.name.cmp(&b.name));
            let target_tree = Tree::from_tree_items(target_items).unwrap();
            save_trees.push(target_tree.clone());

            // After constructing the nested trees, build update instructions
            // and apply them to update the parent trees and refs so the
            // new nested directory/file is persisted in the repository.
            let update_result = MonoServiceLogic::build_result_by_chain(
                PathBuf::from(prefix),
                update_chain,
                target_tree.id,
            )?;
            let repo_path = PathBuf::from(prefix);
            (update_result, blob, entry_oid, repo_path)
        };

        Ok(CreateEntryUpdate {
            update_result,
            blob,
            entry_oid,
            repo_path,
            save_trees,
        })
    }

    fn build_entry_path(path: &str, name: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "/" {
            format!("/{name}")
        } else {
            format!("{trimmed}/{name}")
        }
    }

    fn ref_update_tree_id_for_path(
        result: &TreeUpdateResult,
        repo_path: &str,
    ) -> Option<ObjectHash> {
        let normalized = MonoServiceLogic::clean_path_str(repo_path);
        result
            .ref_updates
            .iter()
            .find(|update| update.path == normalized)
            .map(|update| update.tree_id)
    }

    /// Deepest existing non-root `mega_refs` tip covering `write_path` (AW-03).
    /// Buck-root resolution returns `/`, which B0 rejects for MonoWriteQueue.
    async fn resolve_trunk_land_path(&self, write_path: &str) -> Result<String, GitError> {
        let candidates = MonoServiceLogic::repo_root_candidates(Path::new(write_path));
        for candidate in candidates {
            if candidate == "/" {
                continue;
            }
            match self.storage.mono_storage().get_main_ref(&candidate).await {
                Ok(Some(_)) => return Ok(candidate),
                Ok(None) => continue,
                Err(e) => return Err(GitError::CustomError(e.to_string())),
            }
        }
        Err(GitError::CustomError(format!(
            "[code:400] no non-root path tip under {write_path} for trunk API write"
        )))
    }

    // helper to convert mega_tag model into TagInfo
    fn tag_model_to_info(&self, tag: mega_tag::Model) -> TagInfo {
        TagInfo {
            name: tag.tag_name,
            tag_id: tag.tag_id,
            object_id: tag.object_id,
            object_type: tag.object_type,
            tagger: tag.tagger,
            message: tag.message,
            created_at: tag.created_at.and_utc().to_rfc3339(),
        }
    }

    pub async fn get_or_init_cla_sign_status(
        &self,
        username: &str,
    ) -> Result<(bool, Option<chrono::NaiveDateTime>), MegaError> {
        let model = self
            .storage
            .cla_storage()
            .get_or_create_status(username)
            .await?;
        Ok((model.cla_signed, model.cla_signed_at))
    }

    pub async fn get_cla_content(&self) -> Result<String, MegaError> {
        let key = ObjectKey {
            namespace: ObjectNamespace::Log,
            key: CLA_CONTENT_OBJECT_KEY.to_string(),
        };

        let stream = self
            .storage
            .git_service
            .obj_storage
            .inner
            .get_stream(&key)
            .await
            .map_err(MegaError::from);
        let (mut stream, _meta) = match stream {
            Ok(result) => result,
            Err(MegaError::ObjStorageNotFound(_)) => return Ok(String::new()),
            Err(e) => return Err(e),
        };

        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            data.extend_from_slice(&chunk);
        }

        String::from_utf8(data).map_err(|e| {
            MegaError::Other(format!(
                "Invalid UTF-8 in CLA content from object storage: {e}"
            ))
        })
    }

    pub async fn update_cla_content(&self, content: &str) -> Result<(), MegaError> {
        let key = ObjectKey {
            namespace: ObjectNamespace::Log,
            key: CLA_CONTENT_OBJECT_KEY.to_string(),
        };

        let bytes = Bytes::from(content.as_bytes().to_vec());
        let stream = stream::once(async move { Ok::<Bytes, std::io::Error>(bytes) });
        let meta = ObjectMeta {
            size: content.len() as i64,
            content_type: Some("text/plain; charset=utf-8".to_string()),
            ..Default::default()
        };

        Ok(self
            .storage
            .git_service
            .obj_storage
            .inner
            .put_stream(&key, Box::pin(stream), meta)
            .await?)
    }

    pub async fn change_cla_sign_status(
        &self,
        username: &str,
    ) -> Result<(bool, Option<chrono::NaiveDateTime>), MegaError> {
        let model = self.storage.cla_storage().sign(username).await?;
        self.refresh_checks_for_open_cls_by_author(username).await?;
        Ok((model.cla_signed, model.cla_signed_at))
    }

    async fn refresh_checks_for_open_cls_by_author(&self, username: &str) -> Result<(), MegaError> {
        let open_cls = self
            .storage
            .cl_storage()
            .get_open_cls()
            .await?
            .into_iter()
            .filter(|cl| cl.username == username)
            .collect::<Vec<_>>();
        if open_cls.is_empty() {
            return Ok(());
        }

        let check_reg = CheckerRegistry::new(self.storage.clone().into(), username.to_string());
        for cl in open_cls {
            check_reg.run_checks(cl.into()).await?;
        }

        Ok(())
    }
    // This function is intended to be called before merging a CL to ensure it meets all required checks.
    // It gathers the required checks for the CL's path, retrieves the CL's check results, and returns an error if any required checks have failed.
    // Temporarily do not block the merge process when check fails.

    // async fn ensure_cl_mergeable(&self, cl: &mega_cl::Model) -> Result<(), MegaError> {
    //     let check_reg = CheckerRegistry::new(self.storage.clone().into(), cl.username.clone());
    //     check_reg.run_checks(cl.clone().into()).await?;

    //     let required_check_types = self
    //         .storage
    //         .cl_storage()
    //         .get_checks_config_by_path(&cl.path)
    //         .await?
    //         .into_iter()
    //         .filter(|cfg| cfg.required)
    //         .map(|cfg| cfg.check_type_code)
    //         .collect::<Vec<_>>();

    //     let failed_checks = self
    //         .storage
    //         .cl_storage()
    //         .get_check_result(&cl.link)
    //         .await?
    //         .into_iter()
    //         .filter(|result| {
    //             result.status == "FAILED"
    //                 && required_check_types
    //                     .iter()
    //                     .any(|required_type| required_type == &result.check_type_code)
    //         })
    //         .map(|result| format!("{:?}", result.check_type_code))
    //         .collect::<Vec<_>>();

    //     if failed_checks.is_empty() {
    //         Ok(())
    //     } else {
    //         Err(MegaError::Other(format!(
    //             "CL is unmergeable, failed checks: {}",
    //             failed_checks.join(", ")
    //         )))
    //     }
    // }

    async fn trigger_build_for_cl(
        &self,
        editor: &OneditCodeEdit,
        cl: &mega_cl::Model,
        username: &str,
    ) -> Result<(), GitError> {
        let config = self.storage.config();
        let bellatrix = Bellatrix::new(config.build.clone());
        let git_cache = self.git_object_cache.clone();
        editor
            .trigger_build_and_check(
                self.storage.clone(),
                git_cache,
                Arc::new(bellatrix),
                cl,
                username,
            )
            .await?;

        Ok(())
    }

    /// Triggers a build for Buck upload completion
    fn trigger_build_for_buck_upload(&self, response: &CompleteResponse, username: &str) {
        let config = self.storage.config();
        let bellatrix = Arc::new(Bellatrix::new(config.build.clone()));
        if !bellatrix.enable_build() {
            return;
        }
        let storage = self.storage.clone();
        let git_cache = self.git_object_cache.clone();
        let mut context = TriggerContext::from_buck_upload(
            response.repo_path.clone(),
            response.from_hash.clone(),
            response.commit_id.clone(),
            response.cl_link.clone(),
            Some(response.cl_id),
            Some(username.to_string()),
        );
        context.ref_name = Some("main".to_string());
        context.ref_type = Some("branch".to_string());
        tokio::spawn(async move {
            if let Err(e) =
                BuildTriggerService::build_by_context(storage, git_cache, bellatrix, context).await
            {
                tracing::error!("Failed to create build trigger for buck upload: {}", e);
            }
        });
    }

    async fn create_annotated_tag_mono(
        &self,
        repo_path: Option<String>,
        name: String,
        target: Option<String>,
        tagger_info: String,
        message: Option<String>,
        full_ref: String,
    ) -> Result<TagInfo, GitError> {
        let mono_storage = self.storage.mono_storage();

        // build git_internal/mega tag models
        let (tag_id_hex, object_id) = self.build_git_internal_tag_mono(
            name.clone(),
            target.clone(),
            tagger_info.clone(),
            message.clone(),
        )?;
        let tag_model = self.build_mega_tag_model(
            tag_id_hex.clone(),
            object_id.clone(),
            name.clone(),
            tagger_info.clone(),
            message.clone(),
        );

        match mono_storage.insert_tag(tag_model).await {
            Ok(saved_tag) => {
                // try to write ref; if ref write fails, rollback DB insert
                let path_str = repo_path.unwrap_or_else(|| "/".to_string());
                // Resolve tree hash from target commit so ref metadata is complete
                let tree_hash = self.resolve_tree_hash_for_commit(&object_id).await?;
                let refs =
                    mega_refs::Model::new(&path_str, full_ref.clone(), object_id, tree_hash, false);

                if let Err(e) = mono_storage.save_refs(refs, None).await {
                    // attempt to remove DB record
                    if let Err(del_e) = mono_storage.delete_tag_by_name(&name).await {
                        tracing::error!(
                            "Failed to rollback tag DB record after ref write failure: {}",
                            del_e
                        );
                    }
                    tracing::error!("Failed to write ref after DB insert: {}", e);
                    return Err(GitError::CustomError(
                        "[code:500] Failed to write ref".to_string(),
                    ));
                }
                Ok(self.tag_model_to_info(saved_tag))
            }
            Err(e) => {
                tracing::error!("DB insert error when creating annotated tag: {}", e);
                Err(GitError::CustomError(
                    "[code:500] DB insert error".to_string(),
                ))
            }
        }
    }

    async fn create_lightweight_tag_mono(
        &self,
        repo_path: Option<String>,
        name: String,
        target: Option<String>,
        tagger_info: String,
        full_ref: String,
    ) -> Result<TagInfo, GitError> {
        let mono_storage = self.storage.mono_storage();

        let path_str = repo_path.unwrap_or_else(|| "/".to_string());
        let object_id = target.clone().unwrap_or_default();
        if object_id.is_empty() {
            return Err(GitError::CustomError(
                "[code:400] Missing target commit for lightweight tag".to_string(),
            ));
        }
        // Resolve tree hash from target commit
        let tree_hash = self.resolve_tree_hash_for_commit(&object_id).await?;

        let refs = mega_refs::Model::new(
            &path_str,
            full_ref.clone(),
            object_id.clone(),
            tree_hash,
            false,
        );
        mono_storage.save_refs(refs, None).await.map_err(|e| {
            tracing::error!("Failed to write lightweight tag ref: {}", e);
            GitError::CustomError("[code:500] Failed to write lightweight tag ref".to_string())
        })?;
        // Fetch saved ref to use its creation time
        let saved_ref = mono_storage
            .get_ref_by_name(&full_ref)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .ok_or_else(|| GitError::CustomError("Ref not found after creation".to_string()))?;

        Ok(TagInfo {
            name: name.clone(),
            tag_id: object_id.clone(),
            object_id: object_id.clone(),
            object_type: "commit".to_string(),
            tagger: tagger_info.clone(),
            message: String::new(),
            created_at: saved_ref.created_at.and_utc().to_rfc3339(),
        })
    }

    /// Resolve the tree hash for a given commit id with proper error mapping/logging
    async fn resolve_tree_hash_for_commit(&self, commit_id: &str) -> Result<String, GitError> {
        let mono_storage = self.storage.mono_storage();
        match mono_storage.get_commit_by_hash(commit_id).await {
            Ok(Some(commit_model)) => Ok(commit_model.tree.clone()),
            Ok(None) => {
                tracing::error!(
                    "Target commit '{}' not found while resolving tree hash",
                    commit_id
                );
                Err(GitError::CustomError(format!(
                    "[code:404] Target commit '{}' not found",
                    commit_id
                )))
            }
            Err(e) => {
                tracing::error!(
                    "DB error fetching commit '{}' for tree hash resolution: {}",
                    commit_id,
                    e
                );
                Err(GitError::CustomError("[code:500] DB error".to_string()))
            }
        }
    }
    async fn validate_target_commit_mono(&self, target: Option<&String>) -> Result<(), GitError> {
        let mono_storage = self.storage.mono_storage();
        if let Some(ref t) = target {
            match mono_storage.get_commit_by_hash(t).await {
                Ok(commit_opt) => {
                    if commit_opt.is_none() {
                        return Err(GitError::CustomError(format!(
                            "[code:404] Target commit '{}' not found",
                            t
                        )));
                    }
                }
                Err(e) => {
                    tracing::error!("DB error while fetching commit by hash: {}", e);
                    return Err(GitError::CustomError("[code:500] DB error".to_string()));
                }
            }
        }
        Ok(())
    }

    fn build_git_internal_tag_mono(
        &self,
        name: String,
        target: Option<String>,
        tagger_info: String,
        message: Option<String>,
    ) -> Result<(String, String), GitError> {
        let tag_target = target
            .as_ref()
            .ok_or(GitError::InvalidCommitObject)
            .and_then(|t| {
                ObjectHash::from_hex_for_kind(get_hash_kind(), t)
                    .map_err(|_| GitError::InvalidCommitObject)
            })?;
        let tagger_sig = git_internal::internal::object::signature::Signature::new(
            git_internal::internal::object::signature::SignatureType::Tagger,
            tagger_info.clone(),
            String::new(),
        );
        let git_internal_tag = git_internal::internal::object::tag::Tag::new(
            tag_target,
            git_internal::internal::object::types::ObjectType::Commit,
            name.clone(),
            tagger_sig,
            message.clone().unwrap_or_default(),
        );
        Ok((
            git_internal_tag.id.to_string(),
            target.unwrap_or_else(|| "HEAD".to_string()),
        ))
    }

    fn build_mega_tag_model(
        &self,
        tag_id_hex: String,
        object_id: String,
        name: String,
        tagger_info: String,
        message: Option<String>,
    ) -> mega_tag::Model {
        mega_tag::Model {
            id: crate::common::utils::generate_id(),
            tag_id: tag_id_hex,
            object_id,
            object_type: "commit".to_string(),
            tag_name: name,
            tagger: tagger_info,
            message: message.unwrap_or_default(),
            pack_id: String::new(),
            pack_offset: 0,
            created_at: chrono::Utc::now().naive_utc(),
        }
    }
    /// Merges a CL after checking for conflicts.
    /// This is the public API that includes conflict checking.
    /// Merge a CL.
    ///
    /// The two subjects are deliberately separate (ADR-UN-06 ④):
    /// `authz_principal` is who the merge is *authorized as*, while
    /// `execution_actor` is who it is *recorded as* having run. They coincide
    /// for a normal merge, but not for an anonymous `merge-no-auth` (a reserved
    /// anonymous principal, executed as `system`) or a queued merge (the stored
    /// requester, executed as `system`). Collapsing them would either audit the
    /// wrong actor or authorize as the wrong subject.
    pub async fn merge_cl(
        &self,
        authz_principal: &str,
        execution_actor: &str,
        cl: mega_cl::Model,
    ) -> Result<(), GitError> {
        self.ensure_merge_entry_prechecks(&cl).await?;

        self.merge_cl_via_queue(
            authz_principal,
            execution_actor,
            cl,
            false,
            Some(authz_principal.to_owned()),
        )
        .await
        .map(|_| ())
    }

    /// Entry prechecks shared by `/merge`, `/merge-no-auth`, and queue-mode
    /// `/merge-queue/add` (TP-07): missing `main@P` refuse, GPG gate.
    /// `from_hash != main` is still refused here when the path tree hash is
    /// consistent. Queue mode defers that check to B3 when the tree hash is
    /// already stale so the TP-11 assertion repairs first.
    async fn ensure_merge_entry_prechecks(&self, cl: &mega_cl::Model) -> Result<(), GitError> {
        let storage = self.storage.mono_storage();
        let refs = storage
            .get_main_ref(&cl.path)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to get main ref: {}", e)))?
            .ok_or_else(|| GitError::CustomError("Main ref not found".to_string()))?;

        if cl.from_hash != refs.ref_commit_hash
            && !self
                .queue_merge_defers_from_hash_conflict_to_b3(&cl.path)
                .await?
        {
            return Err(GitError::CustomError("ref hash conflict".to_owned()));
        }

        self.ensure_gpg_check_passed(&cl.link).await
    }

    async fn queue_merge_defers_from_hash_conflict_to_b3(
        &self,
        path: &str,
    ) -> Result<bool, GitError> {
        if path.is_empty() || path == "/" {
            return Ok(false);
        }
        let txn = self
            .storage
            .begin_db_transaction()
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let root = self
            .storage
            .mono_storage()
            .get_main_ref_in_txn("/", &txn)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let Some(root) = root else {
            let _ = txn.rollback().await;
            return Ok(false);
        };
        let stale = self
            .assert_merge_tree_hash_tp11(&txn, path, &root.ref_tree_hash)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .is_some();
        let _ = txn.rollback().await;
        Ok(stale)
    }

    /// Enqueue a merge (`kind=merge`), wait/claim, and run B3 until Done or a
    /// non-conflict failure. Conflict successors are followed (B4).
    async fn merge_cl_via_queue(
        &self,
        authz_principal: &str,
        execution_actor: &str,
        cl: mega_cl::Model,
        apply_queue_execution_decision: bool,
        queue_requester: Option<String>,
    ) -> Result<i64, GitError> {
        let storage = self.storage.mono_storage();
        let old_id = storage
            .get_main_ref(&cl.path)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .map(|r| r.ref_commit_hash)
            .unwrap_or_else(|| ZERO_ID.to_owned());

        let payload = MergePayload {
            cl_link: cl.link.clone(),
            authz_principal: authz_principal.to_owned(),
            execution_actor: execution_actor.to_owned(),
            apply_queue_execution_decision,
            requester: queue_requester,
        };
        let wait = self
            .storage
            .push_queue_service
            .enqueue_and_wait(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: merge_operation_id(&cl.link),
                path: cl.path.clone(),
                old_id,
                new_id: cl.to_hash.clone(),
                requester: Some(authz_principal.to_owned()),
                payload: serde_json::to_value(&payload)
                    .map_err(|e| GitError::CustomError(format!("merge payload encode: {e}")))?,
                ref_name: None,
                is_delete: false,
            })
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let ctx = MergeExecContext {
            storage: self.storage.clone(),
            git_object_cache: self.git_object_cache.clone(),
            abort_before_cl_status: false,
            pause_after_apply: Duration::ZERO,
            pause_after_apply_barrier: None,
        };
        self.follow_merge_queue(wait, &ctx).await
    }

    async fn follow_merge_queue(
        &self,
        mut wait: QueueWaitResult,
        ctx: &MergeExecContext,
    ) -> Result<i64, GitError> {
        const MAX_ROUNDS: usize = 32;
        for _ in 0..MAX_ROUNDS {
            match wait {
                QueueWaitResult::Replayed { id, .. } => return Ok(id),
                QueueWaitResult::Abandoned { id } => {
                    return Err(GitError::CustomError(format!(
                        "merge wait abandoned for push_queue id {id}"
                    )));
                }
                QueueWaitResult::Rejected { id, message } => {
                    return Err(GitError::CustomError(format!(
                        "merge rejected for push_queue id {id}: {message}"
                    )));
                }
                QueueWaitResult::Ready { id } => {
                    let outcome = self
                        .storage
                        .push_queue_service
                        .execute_b3(
                            ExecuteRequest {
                                id,
                                ..Default::default()
                            },
                            None,
                            Some(ctx),
                            None,
                        )
                        .await
                        .map_err(|e| GitError::CustomError(e.to_string()))?;
                    match outcome {
                        ExecuteOutcome::Done { id, .. } => return Ok(id),
                        ExecuteOutcome::Requeued { id, .. } => {
                            wait = self
                                .storage
                                .push_queue_service
                                .wait_and_claim(id)
                                .await
                                .map_err(|e| GitError::CustomError(e.to_string()))?;
                        }
                        ExecuteOutcome::ClaimLost { id } => {
                            wait = self
                                .storage
                                .push_queue_service
                                .wait_and_claim(id)
                                .await
                                .map_err(|e| GitError::CustomError(e.to_string()))?;
                        }
                        ExecuteOutcome::Failed { message, .. } => {
                            return Err(GitError::CustomError(message));
                        }
                        ExecuteOutcome::HardStopped { id } => {
                            return Err(GitError::CustomError(format!(
                                "merge hard-stopped for push_queue id {id}"
                            )));
                        }
                        ExecuteOutcome::BypassDetected { id } => {
                            return Err(GitError::CustomError(format!(
                                "queue bypass detected for push_queue id {id}"
                            )));
                        }
                    }
                }
            }
        }
        Err(GitError::CustomError(
            "merge follow exceeded max conflict requeue rounds".into(),
        ))
    }

    /// MC-02 minimal merge gate (the full `ensure_cl_mergeable` below stays
    /// disabled): a CL with a FAILED GPG signature check cannot be merged.
    /// CLs without any check rows (never checked) are not blocked. Every
    /// merge entry point — `merge_cl` and `/merge-queue/add` — must call this
    /// before enqueueing.
    pub(crate) async fn ensure_gpg_check_passed(&self, link: &str) -> Result<(), GitError> {
        let gpg_failed = self
            .storage
            .cl_storage()
            .get_check_result(link)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to load check results: {e}")))?
            .into_iter()
            .any(|result| {
                result.check_type_code == CheckTypeEnum::GpgSignature && result.status == "FAILED"
            });
        if gpg_failed {
            return Err(GitError::CustomError(format!(
                "CL {link} cannot be merged: GPG signature check failed"
            )));
        }
        Ok(())
    }

    /// Apply all CL changes onto the target_head in-memory and construct +
    /// sign the CL-ref update — without persisting anything (MC-04 R2 split:
    /// persistence moved into `update_branch`'s single transaction). Was
    /// `apply_changes_as_single_commit`.
    async fn build_signed_cl_update(
        &self,
        cl: &mega_cl::Model,
        changes: &[ClDiffFile],
        target_head: &str,
    ) -> Result<PreparedClUpdate, GitError> {
        let mono_storage = self.storage.mono_storage();

        // Load base commit and its root tree
        let base_commit = mono_storage
            .get_commit_by_hash(target_head)
            .await?
            .ok_or_else(|| GitError::CustomError(format!("Commit not found: {target_head}")))?;

        let base_tree_model = mono_storage
            .get_tree_by_hash(&base_commit.tree)
            .await?
            .ok_or_else(|| GitError::CustomError("Root tree not found".to_string()))?;
        let mut root_tree = Tree::from_mega_model(base_tree_model);

        // Cache trees by path to reuse updated versions
        let mut tree_cache: HashMap<PathBuf, Tree> = HashMap::new();
        tree_cache.insert(PathBuf::from("/"), root_tree.clone());

        // Collect all new trees we generate (dedup by hash)
        let mut new_trees: HashMap<ObjectHash, Tree> = HashMap::new();

        for diff in changes {
            let operations: Vec<(PathBuf, Option<ObjectHash>)> = match diff {
                ClDiffFile::New(path, new_hash) => vec![(path.clone(), Some(*new_hash))],
                ClDiffFile::Modified(path, _old, new_hash) => {
                    vec![(path.clone(), Some(*new_hash))]
                }
                ClDiffFile::Deleted(path, _old) => vec![(path.clone(), None)],
                ClDiffFile::Renamed(old_path, new_path, _old_hash, new_hash, _similarity)
                | ClDiffFile::Moved(old_path, new_path, _old_hash, new_hash, _similarity) => {
                    vec![
                        (old_path.clone(), None),
                        (new_path.clone(), Some(*new_hash)),
                    ]
                }
            };

            for (file_path, op) in operations {
                // Reject absolute or parent-traversing paths to avoid writing outside repo root.
                if file_path.is_absolute()
                    || file_path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(GitError::CustomError(format!(
                        "Invalid path (traversal/absolute) in CL diff: {:?}",
                        file_path
                    )));
                }

                let file_name = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| GitError::CustomError("Invalid file name".to_string()))?;
                // Normalize root parent to "/".
                let parent_path = match file_path.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p,
                    _ => Path::new("/"),
                };

                // Build chain of trees from root to parent, using updated cache when available
                let components: Vec<String> = parent_path
                    .components()
                    .filter_map(|c| match c {
                        std::path::Component::RootDir => None,
                        other => other.as_os_str().to_str().map(|s| s.to_string()),
                    })
                    .collect();

                let mut chain_paths: Vec<PathBuf> = vec![PathBuf::from("/")];
                let mut chain_trees: Vec<Tree> = vec![
                    tree_cache
                        .get(&PathBuf::from("/"))
                        .cloned()
                        .ok_or_else(|| {
                            GitError::CustomError("Root tree cache missing".to_string())
                        })?,
                ];

                let mut cursor = PathBuf::from("/");
                let mut missing_components: Option<Vec<String>> = None;
                for (idx, comp) in components.iter().enumerate() {
                    let parent_tree = chain_trees
                        .last()
                        .ok_or_else(|| GitError::CustomError("Empty tree chain".to_string()))?;

                    let maybe_child = parent_tree.tree_items.iter().find(|it| it.name == *comp);
                    let child_tree = if let Some(child_item) = maybe_child {
                        if child_item.mode != TreeItemMode::Tree {
                            return Err(GitError::CustomError(format!(
                                "Type conflict: '{}' is not a directory",
                                comp
                            )));
                        }
                        cursor = cursor.join(comp);
                        let child_hash = child_item.id;
                        if let Some(cached) = tree_cache.get(&cursor) {
                            cached.clone()
                        } else {
                            let model = mono_storage
                                .get_tree_by_hash(&child_hash.to_string())
                                .await?
                                .ok_or_else(|| {
                                    GitError::CustomError(format!(
                                        "Tree not found for path '{}' with hash {}",
                                        cursor.to_string_lossy(),
                                        child_hash
                                    ))
                                })?;
                            Tree::from_mega_model(model)
                        }
                    } else {
                        missing_components = Some(components[idx..].to_vec());
                        break;
                    };

                    chain_paths.push(cursor.clone());
                    chain_trees.push(child_tree);
                }

                if let Some(missing) = missing_components {
                    let mut ctx = ApplyChangeContext {
                        components: &components,
                        chain_paths: &chain_paths,
                        chain_trees: &chain_trees,
                        tree_cache: &mut tree_cache,
                        new_trees: &mut new_trees,
                    };
                    if let Some(updated_root) =
                        Self::apply_missing_path_update(&cl.link, missing, op, file_name, &mut ctx)?
                    {
                        root_tree = updated_root;
                    }
                    continue;
                }

                let parent_dir_abs = cursor.clone();

                // Update parent tree with the file change
                let parent_tree = chain_trees
                    .pop()
                    .ok_or_else(|| GitError::CustomError("Parent tree missing".to_string()))?;
                chain_paths.pop();

                let mut items = parent_tree.tree_items.clone();
                match op {
                    Some(new_hash) => {
                        if let Some(idx) = items.iter().position(|it| it.name == file_name) {
                            items[idx].id = new_hash;
                        } else {
                            items.push(TreeItem::new(
                                TreeItemMode::Blob,
                                new_hash,
                                file_name.to_string(),
                            ));
                        }
                    }
                    None => {
                        items.retain(|it| it.name != file_name);
                    }
                }

                let updated_tree = Tree::from_tree_items(items)
                    .map_err(|e| GitError::CustomError(e.to_string()))?;
                // If parent tree id did not change (no-op), skip propagation for this diff.
                if updated_tree.id == parent_tree.id {
                    // keep cache consistent even for no-ops
                    tree_cache.insert(parent_dir_abs.clone(), parent_tree.clone());
                    debug!(
                        cl_link = %cl.link,
                        parent_dir = %parent_dir_abs.to_string_lossy(),
                        "apply_changes: no-op diff skipped"
                    );
                    continue;
                }
                Self::record_tree(
                    parent_dir_abs,
                    &updated_tree,
                    &mut tree_cache,
                    &mut new_trees,
                );

                // Propagate updated hashes up to root
                root_tree = Self::propagate_up(
                    &cl.link,
                    updated_tree,
                    &components,
                    &chain_paths,
                    &chain_trees,
                    &mut tree_cache,
                    &mut new_trees,
                )?;
            }
        }

        let result = TreeUpdateResult {
            updated_trees: new_trees.values().cloned().collect(),
            ref_updates: vec![RefUpdate {
                path: cl.path.clone(),
                tree_id: root_tree.id,
            }],
        };

        self.prepare_signed_cl_update(
            &result,
            "update-branch: rebase",
            &cl.link,
            Some(
                ObjectHash::from_hex_for_kind(get_hash_kind(), target_head).map_err(|e| {
                    GitError::CustomError(format!(
                        "Invalid target_head ObjectHash '{}': {}",
                        target_head, e
                    ))
                })?,
            ),
        )
        .await
    }

    fn apply_missing_path_update(
        cl_link: &str,
        missing: Vec<String>,
        op: Option<ObjectHash>,
        file_name: &str,
        ctx: &mut ApplyChangeContext<'_>,
    ) -> Result<Option<Tree>, GitError> {
        debug_assert!(
            !missing.iter().any(|c| c == file_name),
            "missing path components should not include file name"
        );
        if op.is_none() {
            debug!(
                cl_link,
                missing_path = %missing.join("/"),
                "apply_changes: delete on missing path (no-op)"
            );
            return Ok(None);
        }

        let new_hash = op.ok_or_else(|| {
            GitError::CustomError("Missing blob hash for new/modified file".to_string())
        })?;

        if missing.is_empty() {
            // No missing directories: update directly under the last existing parent.
            let parent_path = ctx.chain_paths.last().cloned().unwrap_or_else(PathBuf::new);
            let parent_tree = ctx
                .chain_trees
                .last()
                .cloned()
                .ok_or_else(|| GitError::CustomError("Root tree missing".to_string()))?;
            let updated_tree = Self::update_parent_tree(
                cl_link,
                &parent_tree,
                file_name,
                TreeItemMode::Blob,
                new_hash,
                None,
            )?;
            Self::record_tree(parent_path, &updated_tree, ctx.tree_cache, ctx.new_trees);

            return Ok(Some(Self::propagate_up(
                cl_link,
                updated_tree,
                ctx.components,
                ctx.chain_paths,
                ctx.chain_trees,
                ctx.tree_cache,
                ctx.new_trees,
            )?));
        }

        // Build missing subtree from leaf (parent dir) upward without empty trees.
        let file_item = TreeItem::new(TreeItemMode::Blob, new_hash, file_name.to_string());
        let mut updated_tree = Tree::from_tree_items(vec![file_item])
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let mut missing_paths: Vec<PathBuf> = Vec::new();
        let mut base = ctx.chain_paths.last().cloned().unwrap_or_else(PathBuf::new);
        for comp in &missing {
            base = base.join(comp);
            missing_paths.push(base.clone());
        }

        if let Some(parent_path) = missing_paths.last() {
            Self::record_tree(
                parent_path.clone(),
                &updated_tree,
                ctx.tree_cache,
                ctx.new_trees,
            );
        } else {
            Self::record_tree(PathBuf::new(), &updated_tree, ctx.tree_cache, ctx.new_trees);
        }

        for (child_name, path) in missing
            .iter()
            .rev()
            .skip(1)
            .zip(missing_paths.iter().rev().skip(1))
        {
            let wrapper = Tree::from_tree_items(vec![TreeItem::new(
                TreeItemMode::Tree,
                updated_tree.id,
                child_name.clone(),
            )])
            .map_err(|e| GitError::CustomError(e.to_string()))?;
            updated_tree = wrapper;
            Self::record_tree(path.clone(), &updated_tree, ctx.tree_cache, ctx.new_trees);
        }

        // Attach the newly built subtree to the last existing parent.
        let parent_tree = ctx
            .chain_trees
            .last()
            .cloned()
            .ok_or_else(|| GitError::CustomError("Root tree missing".to_string()))?;
        let attach_name = missing
            .first()
            .ok_or_else(|| GitError::CustomError("Missing component chain empty".to_string()))?;
        updated_tree = Self::update_parent_tree(
            cl_link,
            &parent_tree,
            attach_name,
            TreeItemMode::Tree,
            updated_tree.id,
            None,
        )?;
        let parent_path = ctx.chain_paths.last().cloned().unwrap_or_else(PathBuf::new);
        Self::record_tree(parent_path, &updated_tree, ctx.tree_cache, ctx.new_trees);

        Ok(Some(Self::propagate_up(
            cl_link,
            updated_tree,
            ctx.components,
            ctx.chain_paths,
            ctx.chain_trees,
            ctx.tree_cache,
            ctx.new_trees,
        )?))
    }

    fn update_parent_tree(
        cl_link: &str,
        parent_tree: &Tree,
        name: &str,
        mode: TreeItemMode,
        id: ObjectHash,
        debug_parent_path: Option<&PathBuf>,
    ) -> Result<Tree, GitError> {
        let mut parent_items = parent_tree.tree_items.clone();
        if let Some(pos) = parent_items.iter().position(|it| it.name == name) {
            parent_items[pos].id = id;
        } else {
            parent_items.push(TreeItem::new(mode, id, name.to_string()));
            parent_items.sort_by(|a, b| a.name.cmp(&b.name));
            if let Some(path) = debug_parent_path {
                debug!(
                    cl_link,
                    parent_path = %path.to_string_lossy(),
                    created_entry = %name,
                    "apply_changes: inserted missing parent entry"
                );
            }
        }

        Tree::from_tree_items(parent_items).map_err(|e| GitError::CustomError(e.to_string()))
    }

    fn record_tree(
        path: PathBuf,
        tree: &Tree,
        tree_cache: &mut HashMap<PathBuf, Tree>,
        new_trees: &mut HashMap<ObjectHash, Tree>,
    ) {
        tree_cache.insert(path, tree.clone());
        new_trees.insert(tree.id, tree.clone());
    }

    fn propagate_up(
        cl_link: &str,
        mut updated_tree: Tree,
        components: &[String],
        chain_paths: &[PathBuf],
        chain_trees: &[Tree],
        tree_cache: &mut HashMap<PathBuf, Tree>,
        new_trees: &mut HashMap<ObjectHash, Tree>,
    ) -> Result<Tree, GitError> {
        debug_assert!(
            components.len() >= chain_trees.len().saturating_sub(1),
            "components length must cover parent chain"
        );

        for parent_index in (0..chain_trees.len().saturating_sub(1)).rev() {
            let comp = components
                .get(parent_index)
                .ok_or_else(|| GitError::CustomError("Tree path chain underflow".to_string()))?;

            let parent_tree = Self::update_parent_tree(
                cl_link,
                &chain_trees[parent_index],
                comp,
                TreeItemMode::Tree,
                updated_tree.id,
                chain_paths.get(parent_index),
            )?;

            let parent_path_idx = chain_paths
                .get(parent_index)
                .cloned()
                .ok_or_else(|| GitError::CustomError("Tree path chain underflow".to_string()))?;
            Self::record_tree(parent_path_idx, &parent_tree, tree_cache, new_trees);
            updated_tree = parent_tree;
        }

        Ok(updated_tree)
    }

    pub(crate) async fn maybe_invalidate_admin_cache(&self, cl_link: &str) {
        if let Ok(files) = self.get_sorted_changed_file_list(cl_link, None).await {
            let admin_file_modified = files.iter().any(|file| {
                let normalized = file.replace('\\', "/");
                normalized.ends_with(crate::ceres::api_service::admin_ops::ADMIN_FILE)
            });
            if admin_file_modified {
                self.invalidate_admin_cache().await;
            }
        }
    }

    pub async fn apply_update_result(
        &self,
        result: &TreeUpdateResult,
        commit_msg: &str,
        cl_link: Option<&str>,
    ) -> Result<String, GitError> {
        let storage = self.storage.mono_storage();
        let mut new_commit_id = String::new();
        let mut commits: Vec<Commit> = Vec::new();

        // UN-16: capture the pre-update main tree so the authz notify can
        // compare the old/new `/.mega_cedar.json` blob IDs after the ref write.
        let old_main_tree_hash = storage.get_main_ref("/").await?.map(|r| r.ref_tree_hash);

        let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();

        let cl_refs_formatted = cl_link.map(|cl| format!("refs/cl/{}", cl));
        let cl_refs: Option<Vec<&str>> = cl_refs_formatted
            .as_ref()
            .map(|formatted| vec![formatted.as_str(), MEGA_BRANCH_NAME]);

        let refs = storage
            .get_refs_for_paths_and_cls(&paths, cl_refs.as_deref())
            .await?;

        let mut updates: Vec<RefUpdateData> = Vec::new();

        MonoServiceLogic::process_ref_updates(
            result,
            &refs,
            commit_msg,
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        )?;

        if new_commit_id.is_empty() {
            return Err(GitError::CustomError(
                "no commit_id generated: no matching refs found for the update paths".into(),
            ));
        }

        storage
            .batch_update_by_path_concurrent(updates)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        // UN-16: the main ref is now written. If a subsequent step fails, the
        // shared authz snapshot is stale — mark dirty (fail-closed) so enforce
        // mode rejects all (ADR-UN-01).
        if let Err(e) = storage.save_mega_commits(commits, None).await {
            self.storage.entity_store().mark_dirty();
            return Err(GitError::CustomError(e.to_string()));
        }

        let save_trees: Vec<mega_tree::ActiveModel> = result
            .updated_trees
            .clone()
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&new_commit_id);
                tree_model.into()
            })
            .collect();

        if let Err(e) = storage.batch_save_model(save_trees).await {
            self.storage.entity_store().mark_dirty();
            return Err(GitError::CustomError(e.to_string()));
        }

        // UN-16: these reads happen after the ref write, so a failure here
        // leaves the snapshot stale just like a failed save above: mark dirty.
        self.notify_authz_after_main_write(&new_commit_id, old_main_tree_hash.as_deref())
            .await?;

        Ok(new_commit_id)
    }

    /// trunk-push.md 2.7: path/root writes and descendant continuation share one
    /// commit. Unlike [`Self::apply_update_result_in_txn`] this keeps the
    /// legacy last-write-wins ref update (no root CAS). Authz notify is the
    /// caller's after `txn.commit()`.
    async fn apply_update_result_in_open_txn(
        &self,
        txn: &DatabaseTransaction,
        result: &TreeUpdateResult,
        commit_msg: &str,
        cl_link: Option<&str>,
    ) -> Result<String, GitError> {
        let storage = self.storage.mono_storage();
        let mut new_commit_id = String::new();
        let mut commits: Vec<Commit> = Vec::new();

        let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();

        let cl_refs_formatted = cl_link.map(|cl| format!("refs/cl/{}", cl));
        let cl_refs: Option<Vec<&str>> = cl_refs_formatted
            .as_ref()
            .map(|formatted| vec![formatted.as_str(), MEGA_BRANCH_NAME]);

        let refs = storage
            .get_refs_for_paths_and_cls_in_txn(&paths, cl_refs.as_deref(), txn)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let mut updates: Vec<RefUpdateData> = Vec::new();

        MonoServiceLogic::process_ref_updates(
            result,
            &refs,
            commit_msg,
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        )?;

        if new_commit_id.is_empty() {
            return Err(GitError::CustomError(
                "no commit_id generated: no matching refs found for the update paths".into(),
            ));
        }

        storage
            .batch_update_by_path_in_txn(txn, updates)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        storage
            .save_mega_commits(commits, Some(txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let save_trees: Vec<mega_tree::ActiveModel> = result
            .updated_trees
            .clone()
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&new_commit_id);
                tree_model.into()
            })
            .collect();

        storage
            .batch_save_model_with_txn(save_trees, Some(txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        Ok(new_commit_id)
    }

    async fn notify_authz_after_main_write(
        &self,
        new_commit_id: &str,
        old_main_tree_hash: Option<&str>,
    ) -> Result<(), GitError> {
        let storage = self.storage.mono_storage();
        let blob_ids = async {
            let new_commit = storage
                .get_commit_by_hash(new_commit_id)
                .await?
                .ok_or_else(|| MegaError::Other("new commit not found".into()))?;
            let new_blob_id = storage
                .get_tree_by_hash(&new_commit.tree)
                .await?
                .and_then(|t| authz_blob_id(&Tree::from_mega_model(t)));
            let old_blob_id = match old_main_tree_hash {
                Some(hash) => storage
                    .get_tree_by_hash(hash)
                    .await?
                    .and_then(|t| authz_blob_id(&Tree::from_mega_model(t))),
                None => None,
            };
            Ok::<_, MegaError>((old_blob_id, new_blob_id))
        }
        .await;
        let (old_blob_id, new_blob_id) = match blob_ids {
            Ok(ids) => ids,
            Err(e) => {
                self.storage.entity_store().mark_dirty();
                return Err(GitError::CustomError(e.to_string()));
            }
        };
        notify_authz_changed_best_effort(
            &self.storage,
            old_blob_id.as_deref(),
            new_blob_id.as_deref(),
        )
        .await;
        Ok(())
    }

    /// TP-11: `refs/heads/main` tree-hash assertion (shared with push).
    pub(crate) async fn assert_merge_tree_hash_tp11(
        &self,
        txn: &DatabaseTransaction,
        path: &str,
        root_tree_hash: &str,
    ) -> Result<Option<crate::jupiter::service::push_queue_service::StaleMainRef>, MegaError> {
        self.storage
            .push_queue_service
            .assert_main_path_tree_hash_in_txn(txn, path, root_tree_hash)
            .await
    }

    /// Transactional apply used by merge B3 (TP-07): refs (exactly one root
    /// CAS for `main@/`), commits, and trees share `txn`. Missing `main` at
    /// the CL path is refused (no upsert).
    pub(crate) const MERGE_ROOT_CAS_MISS: &'static str = "root CAS affected 0 rows";

    pub(crate) async fn apply_update_result_in_txn(
        &self,
        txn: &DatabaseTransaction,
        result: &TreeUpdateResult,
        commit_msg: &str,
        cl: &mega_cl::Model,
        expected_root_commit: Option<&str>,
        expected_root_tree: Option<&str>,
    ) -> Result<(String, u32), GitError> {
        let storage = self.storage.mono_storage();
        let mut new_commit_id = String::new();
        let mut commits: Vec<Commit> = Vec::new();

        let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();
        let cl_refs_formatted = format!("refs/cl/{}", cl.link);
        let cl_refs = [cl_refs_formatted.as_str(), MEGA_BRANCH_NAME];

        let refs = storage
            .get_refs_for_paths_and_cls_in_txn(&paths, Some(&cl_refs), txn)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let cl_path = MonoServiceLogic::clean_path_str(&cl.path);
        if !refs
            .iter()
            .any(|r| r.path == cl_path && r.ref_name == MEGA_BRANCH_NAME)
        {
            return Err(GitError::CustomError(format!(
                "Main ref not found at {cl_path}"
            )));
        }

        let mut updates: Vec<RefUpdateData> = Vec::new();
        MonoServiceLogic::process_ref_updates(
            result,
            &refs,
            commit_msg,
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        )?;

        if new_commit_id.is_empty() {
            return Err(GitError::CustomError(
                "no commit_id generated: no matching refs found for the update paths".into(),
            ));
        }

        let mut root_cas_writes = 0u32;
        let mut other_updates = Vec::new();
        let mut landed_commit_id = new_commit_id.clone();
        for update in updates {
            if update.path == "/" && update.ref_name == MEGA_BRANCH_NAME {
                let cas_ok = storage
                    .cas_update_root_main_ref_in_txn(
                        txn,
                        expected_root_commit,
                        expected_root_tree,
                        &update.commit_id,
                        &update.tree_hash,
                    )
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?;
                if !cas_ok {
                    return Err(GitError::CustomError(Self::MERGE_ROOT_CAS_MISS.into()));
                }
                root_cas_writes += 1;
                landed_commit_id = update.commit_id.clone();
            } else {
                other_updates.push(update);
            }
        }
        if root_cas_writes != 1 {
            return Err(GitError::CustomError(format!(
                "merge B3 expected exactly one root CAS, got {root_cas_writes}"
            )));
        }

        storage
            .batch_update_by_path_in_txn(txn, other_updates)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        storage
            .save_mega_commits(commits, Some(txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let save_trees: Vec<mega_tree::ActiveModel> = result
            .updated_trees
            .clone()
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&landed_commit_id);
                tree_model.into()
            })
            .collect();

        storage
            .batch_save_model_with_txn(save_trees, Some(txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        Ok((landed_commit_id, root_cas_writes))
    }

    /// Transactional apply used by push B3 (TP-12): upserts `main@P` and
    /// ancestor refs, synthesizes ancestor roll-up commits only when the
    /// tree actually changed, and performs exactly one root CAS (advance or
    /// net-zero same-value write).
    pub(crate) async fn apply_push_in_txn(
        &self,
        txn: &DatabaseTransaction,
        args: PushApplyArgs<'_>,
    ) -> Result<(String, u32), GitError> {
        use sea_orm::IntoActiveModel;

        let PushApplyArgs {
            result,
            path_p,
            landed_at_p,
            expected_root_commit,
            expected_root_tree,
            extra_commits,
            extra_blob,
            provenance,
            sign_trunk,
        } = args;
        let storage = self.storage.mono_storage();
        let path_p = MonoServiceLogic::clean_path_str(path_p);
        let mut commits = extra_commits;
        let mut other_updates: Vec<RefUpdateData> = Vec::new();
        let mut root_tree_new: Option<ObjectHash> = None;

        let (Some(plan), Some(sign)) = (provenance, sign_trunk.as_ref()) else {
            return Err(GitError::CustomError(
                "push B3 missing trunk provenance/signing".into(),
            ));
        };
        let trunk_commit = |tree: ObjectHash,
                            parents: Vec<ObjectHash>,
                            ref_path: &str,
                            prev: Option<&git_internal::internal::object::signature::Signature>|
         -> Result<Commit, GitError> {
            let unsigned = crate::ceres::pack::trunk_provenance::synthesize(
                plan.author.clone(),
                plan.committer(prev),
                tree,
                parents,
                &plan.layer_message(ref_path),
            );
            sign(&unsigned)
        };

        for update in &result.ref_updates {
            let clean = MonoServiceLogic::clean_path_str(&update.path);
            if clean == path_p {
                other_updates.push(RefUpdateData {
                    path: path_p.clone(),
                    ref_name: MEGA_BRANCH_NAME.to_owned(),
                    commit_id: landed_at_p.to_owned(),
                    tree_hash: update.tree_id.to_string(),
                });
                continue;
            }
            if clean == "/" {
                root_tree_new = Some(update.tree_id);
                continue;
            }
            let existing = storage
                .get_main_ref_in_txn(&clean, txn)
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            if let Some(row) = existing.as_ref()
                && row.ref_tree_hash == update.tree_id.to_string()
            {
                continue;
            }
            let parent_ids = match existing.as_ref() {
                Some(row) => vec![
                    ObjectHash::from_hex_for_kind(get_hash_kind(), &row.ref_commit_hash)
                        .map_err(|e| GitError::CustomError(e.to_string()))?,
                ],
                None => Vec::new(),
            };
            let prev = match existing.as_ref() {
                Some(row) => storage
                    .get_commit_by_hash(&row.ref_commit_hash)
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?
                    .map(Commit::from_mega_model)
                    .map(|c| c.committer),
                None => None,
            };
            let commit = trunk_commit(update.tree_id, parent_ids, &clean, prev.as_ref())?;
            other_updates.push(RefUpdateData {
                path: clean,
                ref_name: MEGA_BRANCH_NAME.to_owned(),
                commit_id: commit.id.to_string(),
                tree_hash: update.tree_id.to_string(),
            });
            commits.push(commit);
        }

        let Some(new_root_tree) = root_tree_new else {
            return Err(GitError::CustomError(
                "push B3 expected a root tree update in the roll-up".into(),
            ));
        };
        let new_root_tree_str = new_root_tree.to_string();
        let root_unchanged = expected_root_tree == Some(new_root_tree_str.as_str());
        let root_commit = if root_unchanged {
            expected_root_commit
                .ok_or_else(|| {
                    GitError::CustomError("push B3 missing expected root commit".into())
                })?
                .to_owned()
        } else {
            let parent_ids = match expected_root_commit {
                Some(c) => vec![
                    ObjectHash::from_hex_for_kind(get_hash_kind(), c)
                        .map_err(|e| GitError::CustomError(e.to_string()))?,
                ],
                None => Vec::new(),
            };
            let prev = match expected_root_commit {
                Some(c) => storage
                    .get_commit_by_hash(c)
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?
                    .map(Commit::from_mega_model)
                    .map(|c| c.committer),
                None => None,
            };
            let commit = trunk_commit(new_root_tree, parent_ids, "/", prev.as_ref())?;
            let id = commit.id.to_string();
            commits.push(commit);
            id
        };

        let cas_ok = storage
            .cas_update_root_main_ref_in_txn(
                txn,
                expected_root_commit,
                expected_root_tree,
                &root_commit,
                &new_root_tree_str,
            )
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        if !cas_ok {
            return Err(GitError::CustomError(Self::MERGE_ROOT_CAS_MISS.into()));
        }

        storage
            .batch_upsert_by_path_in_txn(txn, other_updates)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        if !commits.is_empty() {
            storage
                .save_mega_commits(commits, Some(txn))
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
        }

        let save_trees: Vec<mega_tree::ActiveModel> = result
            .updated_trees
            .clone()
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id = landed_at_p.to_owned();
                tree_model.into()
            })
            .collect();
        if !save_trees.is_empty() {
            storage
                .batch_save_model_with_txn(save_trees, Some(txn))
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
        }

        if let Some(blob) = extra_blob {
            let mut model: mega_blob::Model = blob.into_mega_model(EntryMeta::new());
            model.commit_id = landed_at_p.to_owned();
            storage
                .batch_save_model_with_txn(vec![model.into_active_model()], Some(txn))
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
        }

        Ok((landed_at_p.to_owned(), 1))
    }

    /// The signing capability for server-synthesized commits that enter a CL
    /// chain (MC-09). Fail-closed: without the vault handle no synthetic
    /// commit may leave this service unsigned. The Redis manager backs the
    /// RedLock that guards first-time key initialization.
    pub(crate) fn server_signing_context(&self) -> Result<ServerSigningContext, MegaError> {
        let vault = self.storage.vault().ok_or_else(|| {
            MegaError::Other(
                "server signing unavailable: vault handle is not configured".to_string(),
            )
        })?;
        Ok(ServerSigningContext::new(
            vault.clone(),
            self.git_object_cache.connection.clone(),
        ))
    }

    /// Apply update result but only update the CL ref (never main).
    /// Optionally override the parent commit for the first created commit (used by rebase).
    /// Construct and sign a CL-ref update without persisting anything (MC-04
    /// R2 split of the former `apply_update_result_cl_only`): the MC-09
    /// signing precheck runs before any output is produced, and every output
    /// carries only the final signed hash (see `process_ref_updates_cl_only`).
    async fn prepare_signed_cl_update(
        &self,
        result: &TreeUpdateResult,
        commit_msg: &str,
        cl_link: &str,
        parent_override: Option<ObjectHash>,
    ) -> Result<PreparedClUpdate, GitError> {
        let storage = self.storage.mono_storage();
        let mut new_commit_id = String::new();
        let mut commits: Vec<Commit> = Vec::new();

        let cl_ref_name = format!("refs/cl/{}", cl_link);
        let cl_ref = storage
            .get_ref_by_name(&cl_ref_name)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .ok_or_else(|| GitError::CustomError("CL ref not found".to_string()))?;

        let mut updates: Vec<RefUpdateData> = Vec::new();

        // MC-09: fail-closed when the server-signing vault is not wired in.
        // Signing happens inside `process_ref_updates_cl_only`, before any
        // persistence by the caller.
        let signing = self
            .server_signing_context()
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        MonoServiceLogic::process_ref_updates_cl_only(
            result,
            &cl_ref,
            commit_msg,
            parent_override,
            &signing,
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        )
        .await?;

        if new_commit_id.is_empty() {
            debug!(
                cl_link,
                ref_name = %cl_ref.ref_name,
                ref_path = %cl_ref.path,
                commit_msg,
                "prepare_signed_cl_update: no commit_id generated"
            );
            return Err(GitError::CustomError(
                "no commit_id generated: no matching refs found for the update paths".into(),
            ));
        }

        let tree_models: Vec<mega_tree::ActiveModel> = result
            .updated_trees
            .clone()
            .into_iter()
            .map(|save_t| {
                let mut tree_model: mega_tree::Model = save_t.into_mega_model(EntryMeta::new());
                tree_model.commit_id.clone_from(&new_commit_id);
                tree_model.into()
            })
            .collect();

        Ok(PreparedClUpdate {
            commits,
            cl_ref,
            updates,
            tree_models,
            new_commit_id,
        })
    }

    /// Fetches the content difference for a merge request, paginated by page_id and page_size.
    /// # Arguments
    /// * `cl_link` - The link to the merge request.
    /// * `page_id` - The page number to fetch. (id out of bounds will return empty)
    /// * `page_size` - The number of items per page.
    /// # Returns
    ///  a `Result` containing `ClDiff` on success or a `GitError` on failure.
    /// Build paged CL diff items with optional relocation metadata for CL views.
    async fn paged_content_diff_items(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<PagedClDiffItem>, u64), GitError> {
        let per_page = page.per_page as usize;
        let page_id = page.page as usize;

        let stg = self.storage.cl_storage();
        let cl =
            stg.get_cl(cl_link).await.unwrap().ok_or_else(|| {
                GitError::CustomError(format!("Merge request not found: {cl_link}"))
            })?;
        let old_blobs = self
            .get_commit_blobs(&cl.from_hash)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to get old commit blobs: {e}")))?;
        let new_blobs = self
            .get_commit_blobs(&cl.to_hash)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to get new commit blobs: {e}")))?;

        let sorted_changed_files = self
            .cl_files_list(old_blobs, new_blobs)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let start = (page_id.saturating_sub(1)) * per_page;
        let end = (start + per_page).min(sorted_changed_files.len());

        let page_slice: &[ClDiffFile] = if start < sorted_changed_files.len() {
            let start_idx = start;
            let end_idx = end;
            &sorted_changed_files[start_idx..end_idx]
        } else {
            &[]
        };

        let non_relocated_items: Vec<ClDiffFile> = page_slice
            .iter()
            .filter(|item| {
                !matches!(
                    item,
                    ClDiffFile::Renamed(_, _, _, _, _) | ClDiffFile::Moved(_, _, _, _, _)
                )
            })
            .cloned()
            .collect();

        let mut page_old_blobs = Vec::new();
        let mut page_new_blobs = Vec::new();
        collect_page_blobs(
            &non_relocated_items,
            &mut page_old_blobs,
            &mut page_new_blobs,
        );

        let raw_diff_output = if non_relocated_items.is_empty() {
            Vec::new()
        } else {
            self.get_diff_by_blobs(page_old_blobs, page_new_blobs)
                .await?
        };

        let mut raw_diff_by_path: HashMap<String, Vec<DiffItem>> = HashMap::new();
        for item in raw_diff_output {
            raw_diff_by_path
                .entry(item.path.clone())
                .or_default()
                .push(item);
        }

        let mut diff_output: Vec<PagedClDiffItem> = Vec::with_capacity(page_slice.len());
        for item in page_slice {
            match item {
                ClDiffFile::Renamed(old_path, new_path, old_hash, new_hash, similarity)
                | ClDiffFile::Moved(old_path, new_path, old_hash, new_hash, similarity) => {
                    diff_output.push(PagedClDiffItem {
                        item: self
                            .format_relocated_diff_item(
                                old_path,
                                new_path,
                                *old_hash,
                                *new_hash,
                                *similarity,
                            )
                            .await?,
                        old_path: Some(old_path.to_string_lossy().replace('\\', "/")),
                    });
                }
                _ => {
                    let key = item.path().to_string_lossy().replace('\\', "/");
                    if let Some(items) = raw_diff_by_path.get_mut(&key)
                        && !items.is_empty()
                    {
                        diff_output.push(PagedClDiffItem {
                            item: items.remove(0),
                            old_path: None,
                        });
                    }
                }
            }
        }

        let total = sorted_changed_files.len().div_ceil(per_page);

        Ok((diff_output, total as u64))
    }

    /// Return the legacy paged diff shape without CL-specific metadata.
    pub async fn paged_content_diff(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<DiffItem>, u64), GitError> {
        let (items, total) = self.paged_content_diff_items(cl_link, page).await?;
        Ok((items.into_iter().map(|item| item.item).collect(), total))
    }

    /// Return paged diff items tailored for the CL files-changed API.
    pub async fn paged_content_diff_for_cl(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<ClFilesChangedItemSchema>, u64), GitError> {
        let (items, total) = self.paged_content_diff_items(cl_link, page).await?;
        Ok((
            items
                .into_iter()
                .map(|item| ClFilesChangedItemSchema::new(item.item, item.old_path))
                .collect(),
            total,
        ))
    }

    async fn get_diff_by_blobs(
        &self,
        old_blobs: Vec<(PathBuf, ObjectHash)>,
        new_blobs: Vec<(PathBuf, ObjectHash)>,
    ) -> Result<Vec<DiffItem>, GitError> {
        let mut blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();

        // Collect all unique hashes
        let mut all_hashes = HashSet::new();
        for (_, hash) in &old_blobs {
            all_hashes.insert(*hash);
        }
        for (_, hash) in &new_blobs {
            all_hashes.insert(*hash);
        }

        // Fetch all blobs with better error handling and logging
        let mut failed_hashes = Vec::new();
        for hash in all_hashes {
            match self.get_raw_blob_by_hash(&hash.to_string()).await {
                Ok(data) => {
                    blob_cache.insert(hash, data);
                }
                Err(e) => {
                    tracing::error!("Failed to fetch blob {}: {}", hash, e);
                    failed_hashes.push(hash);
                    blob_cache.insert(hash, Vec::new());
                }
            }
        }

        if !failed_hashes.is_empty() {
            tracing::warn!(
                "Failed to fetch {} blob(s): {:?}",
                failed_hashes.len(),
                failed_hashes
            );
        }

        // Enhanced content reader with better error handling
        let read_content = |file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
            match blob_cache.get(hash) {
                Some(content) => content.clone(),
                None => {
                    tracing::warn!("Missing blob content for file: {:?}, hash: {}", file, hash);
                    Vec::new()
                }
            }
        };

        // Use the unified diff function with configurable algorithm
        let diff_output = GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
            .into_iter()
            .map(Self::normalize_diff_item)
            .collect();

        Ok(diff_output)
    }

    async fn format_relocated_diff_item(
        &self,
        old_path: &Path,
        new_path: &Path,
        old_hash: ObjectHash,
        new_hash: ObjectHash,
        similarity: u8,
    ) -> Result<DiffItem, GitError> {
        let mut patch = Self::format_relocated_patch_header(old_path, new_path, similarity);

        if old_hash != new_hash {
            let raw_items = self
                .get_diff_by_blobs(
                    vec![(old_path.to_path_buf(), old_hash)],
                    vec![(old_path.to_path_buf(), new_hash)],
                )
                .await?;
            if let Some(item) = raw_items.into_iter().next() {
                patch.push_str(&Self::relocate_patch_body(&item.data, old_path, new_path));
            }
        }

        Ok(DiffItem {
            path: new_path.to_string_lossy().replace('\\', "/"),
            data: patch,
        })
    }

    fn format_relocated_patch_header(old_path: &Path, new_path: &Path, similarity: u8) -> String {
        let old_path = old_path.to_string_lossy().replace('\\', "/");
        let new_path = new_path.to_string_lossy().replace('\\', "/");
        format!(
            "diff --git a/{old_path} b/{new_path}\nsimilarity index {similarity}%\nrename from {old_path}\nrename to {new_path}\n"
        )
    }

    fn normalize_diff_item(mut item: DiffItem) -> DiffItem {
        item.path = item.path.replace('\\', "/");
        item.data = Self::normalize_patch_header_paths(&item.data);
        item
    }

    fn normalize_patch_header_paths(raw_patch: &str) -> String {
        let sections = Self::split_patch_sections(raw_patch);
        let header_lines = sections
            .header_lines
            .into_iter()
            .map(Self::normalize_patch_header_line)
            .collect();
        let divider_line = sections.divider_line.map(|line| {
            if line.starts_with("Binary files ") {
                line.replace('\\', "/")
            } else {
                line.to_string()
            }
        });

        Self::join_patch_sections(
            header_lines,
            divider_line,
            sections.payload_lines,
            sections.has_trailing_newline,
        )
    }

    fn relocate_patch_body(raw_patch: &str, old_path: &Path, new_path: &Path) -> String {
        let old_path = old_path.to_string_lossy().replace('\\', "/");
        let new_path = new_path.to_string_lossy().replace('\\', "/");
        let sections = Self::split_patch_sections(raw_patch);
        let header_lines = sections
            .header_lines
            .into_iter()
            .filter(|line| !line.starts_with("diff --git "))
            .map(|line| {
                if line.starts_with("--- a/") {
                    format!("--- a/{old_path}")
                } else if line.starts_with("+++ b/") {
                    format!("+++ b/{new_path}")
                } else {
                    line.to_string()
                }
            })
            .collect();
        let divider_line = sections.divider_line.map(|line| {
            if line.starts_with("Binary files ") {
                format!("Binary files a/{old_path} and b/{new_path} differ")
            } else {
                line.to_string()
            }
        });

        Self::join_patch_sections(
            header_lines,
            divider_line,
            sections.payload_lines,
            sections.has_trailing_newline,
        )
    }

    fn normalize_patch_header_line(line: &str) -> String {
        if line.starts_with("diff --git ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
        {
            line.replace('\\', "/")
        } else {
            line.to_string()
        }
    }

    fn split_patch_sections(raw_patch: &str) -> PatchSections<'_> {
        let mut header_lines = Vec::new();
        let mut divider_line = None;
        let mut payload_lines = Vec::new();
        let mut in_payload = false;

        for line in raw_patch.lines() {
            if in_payload {
                payload_lines.push(line);
                continue;
            }

            if line.starts_with("@@")
                || line.starts_with("Binary files ")
                || line.starts_with("GIT binary patch")
            {
                divider_line = Some(line);
                in_payload = true;
                continue;
            }

            header_lines.push(line);
        }

        PatchSections {
            header_lines,
            divider_line,
            payload_lines,
            has_trailing_newline: raw_patch.ends_with('\n'),
        }
    }

    fn join_patch_sections(
        header_lines: Vec<String>,
        divider_line: Option<String>,
        payload_lines: Vec<&str>,
        has_trailing_newline: bool,
    ) -> String {
        let mut lines = header_lines;
        if let Some(line) = divider_line {
            lines.push(line);
        }
        lines.extend(payload_lines.into_iter().map(String::from));

        let rendered = lines.join("\n");
        if rendered.is_empty() {
            rendered
        } else if has_trailing_newline {
            format!("{rendered}\n")
        } else {
            rendered
        }
    }

    pub async fn get_sorted_changed_file_list(
        &self,
        cl_link: &str,
        path: Option<&str>,
    ) -> Result<Vec<String>, MegaError> {
        let normalized_prefix = path.map(|prefix| prefix.replace('\\', "/"));
        // UN-19: this runs on the merge path now, so a storage failure must
        // propagate rather than panic the request.
        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {cl_link}")))?;

        let old_files = self.get_commit_blobs(&cl.from_hash.clone()).await?;
        let new_files = self.get_commit_blobs(&cl.to_hash.clone()).await?;

        // calculate pages
        let sorted_changed_files = self.cl_files_list(old_files, new_files).await?;
        let file_paths: Vec<String> = sorted_changed_files
            .iter()
            .map(|f| f.path().to_string_lossy().replace('\\', "/"))
            .filter(|file_path| {
                if let Some(prefix) = &normalized_prefix {
                    file_path.starts_with(prefix)
                } else {
                    true
                }
            })
            .collect();

        Ok(file_paths)
    }

    /// Return Update Branch status for a CL: only checks whether main/trunk moved past the CL base.
    pub async fn update_branch_status(
        &self,
        cl_link: &str,
    ) -> Result<UpdateBranchStatusRes, MegaError> {
        let stg = self.storage.cl_storage();
        let cl = stg
            .get_cl(cl_link)
            .await?
            .ok_or_else(|| MegaError::Other("CL Not Found".to_string()))?;

        let main_ref = self
            .storage
            .mono_storage()
            .get_main_ref(&cl.path)
            .await?
            .ok_or_else(|| MegaError::Other("Main ref not found".to_string()))?;
        let target_head = main_ref.ref_commit_hash;

        Ok(UpdateBranchStatusRes {
            base_commit: cl.from_hash.clone(),
            target_head: target_head.clone(),
            outdated: cl.from_hash != target_head,
        })
    }

    /// Update Branch (rebase-like) for Open CL: applies CL file changes onto latest target head
    /// and updates CL's base/head commits. Returns new head commit id on success.
    pub async fn update_branch(&self, username: &str, cl_link: &str) -> Result<String, GitError> {
        let stg = self.storage.cl_storage();
        let conv_stg = self.storage.conversation_storage();

        let cl = stg
            .get_cl(cl_link)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .ok_or_else(|| GitError::CustomError("CL Not Found".to_string()))?;

        if cl.status != MergeStatusEnum::Open {
            return Err(GitError::CustomError(
                "Only Open CL can update branch".to_string(),
            ));
        }

        let main_ref = self
            .storage
            .mono_storage()
            .get_main_ref(&cl.path)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?
            .ok_or_else(|| GitError::CustomError("Main ref not found".to_string()))?;
        let target_head = main_ref.ref_commit_hash;

        if target_head == cl.from_hash {
            return Ok("Already up-to-date".to_string());
        }

        // Detect file-level conflicts
        let conflicts = self.detect_update_conflicts(&cl, &target_head).await?;

        if !conflicts.is_empty() {
            // Record conflict info on the CL conversation for visibility.
            let conflict_msg = format!(
                "{} failed to update branch: conflicts on {}",
                username,
                conflicts.join(", ")
            );
            if let Err(e) = conv_stg
                .add_conversation(cl_link, username, Some(conflict_msg), ConvTypeEnum::Comment)
                .await
            {
                tracing::warn!("Failed to add conflict comment to conversation: {}", e);
            }
            return Err(GitError::CustomError(format!(
                "Update conflict on files: {}",
                conflicts.join(", ")
            )));
        }

        // Apply CL diffs onto latest target head
        let old_blobs = self
            .get_commit_blobs(&cl.from_hash)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let new_blobs = self
            .get_commit_blobs(&cl.to_hash)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let cl_changed = self
            .cl_files_list(old_blobs.clone(), new_blobs.clone())
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        if cl_changed.is_empty() {
            // No-op rebase: the CL's aggregate diff is empty — its content is
            // already contained in the target head's tree.
            //
            // MC-04 R3: represent that as a self-consistent empty range
            // `(from, to) = (target_head, target_head)`. The previous shape
            // (move from to target_head, keep the old tip) was *not* an empty
            // range: the old tip's tree can lack files the target has, so a
            // merge of such a CL would adopt the stale tip tree and roll back
            // target files, and the GPG chain walk requires `from` reachable
            // from `to` (it would fail closed). The active CL ref follows to
            // the target head and the commit listing is cleared (an empty
            // listing reads as an empty list); the old tip stays in the
            // object store for audit, and the conversation below records the
            // move.
            //
            // Merge/GPG推演 for such an empty-range CL (from == to), both
            // content-safe by construction: merge_cl passes the
            // from==main-head check and synthesizes a no-op trunk commit with
            // the unchanged tree (the diff is empty by construction); if a
            // GPG check runs on the degenerate `(from == to]` range, MC-02
            // verifies the single tip — a trunk commit, unsigned by design
            // (MC-09 scope) — and fails closed, blocking the merge. Neither
            // path can lose content; merge/GPG logic is deliberately
            // untouched (out of this card's scope).
            let mono = self.storage.mono_storage();
            let cl_ref_name = format!("refs/cl/{cl_link}");
            let mut cl_ref = mono
                .get_ref_by_name(&cl_ref_name)
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?
                .ok_or_else(|| GitError::CustomError("CL ref not found".to_string()))?;
            cl_ref.ref_commit_hash = target_head.clone();
            cl_ref.ref_tree_hash = main_ref.ref_tree_hash.clone();
            let txn = self
                .storage
                .begin_db_transaction()
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            mono.update_ref(cl_ref, Some(&txn))
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            stg.update_cl_hash_in_txn(cl.clone(), &target_head, &target_head, &txn)
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            stg.delete_cl_commits_in_txn(cl_link, &txn)
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            txn.commit()
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            conv_stg
                .add_conversation(
                    cl_link,
                    username,
                    Some(format!(
                        "{} updated branch (no changes) to {}",
                        username,
                        &target_head[..6]
                    )),
                    ConvTypeEnum::Comment,
                )
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
            return Ok(target_head);
        }

        // Construct and sign the CL-ref update in memory first (MC-09
        // precheck semantics: zero persistence before signing succeeds), then
        // persist commit rows, tree rows, the CL ref, the CL hash move and
        // the commit-listing rebuild in ONE transaction (MC-04 R3) — a
        // failure anywhere leaves every sink untouched. The listing content
        // is the constructed chain itself: `collect_cl_chain`'s DB walk cannot
        // see the not-yet-persisted commit, and the parent threading in
        // `process_ref_updates_cl_only` (parent_override = target_head)
        // already guarantees the (target_head, new_head] topology.
        let prepared = self
            .build_signed_cl_update(&cl, &cl_changed, &target_head)
            .await?;
        let new_head = prepared.new_commit_id.clone();
        let mono = self.storage.mono_storage();
        let mut cl_ref = prepared.cl_ref.clone();
        for update in &prepared.updates {
            if update.ref_name == cl_ref.ref_name {
                cl_ref.ref_commit_hash = update.commit_id.clone();
                cl_ref.ref_tree_hash = update.tree_hash.clone();
            }
        }
        let txn = self
            .storage
            .begin_db_transaction()
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        mono.update_ref(cl_ref, Some(&txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        mono.save_mega_commits(prepared.commits.clone(), Some(&txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        mono.batch_save_model_with_txn(prepared.tree_models.clone(), Some(&txn))
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        stg.update_cl_hash_in_txn(cl.clone(), &target_head, &new_head, &txn)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        stg.save_cl_commits_in_txn(cl_link, &prepared.commits, &txn)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        txn.commit()
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        conv_stg
            .add_conversation(
                cl_link,
                username,
                Some(format!(
                    "{} updated branch to {}",
                    username,
                    &target_head[..6]
                )),
                ConvTypeEnum::Comment,
            )
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        Ok(new_head)
    }

    /// Detect file-level update conflicts between the CL changes and target head.
    /// A conflict is reported if any file path modified by the CL is also changed
    /// between `from_hash` and `target_head`.
    async fn detect_update_conflicts(
        &self,
        cl: &mega_cl::Model,
        target_head: &str,
    ) -> Result<Vec<String>, GitError> {
        let old_blobs = self
            .get_commit_blobs(&cl.from_hash)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let new_blobs = self
            .get_commit_blobs(&cl.to_hash)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        // Keep conflict checks path-based so renames cover both old and new paths.
        let cl_changed = edit_utils::cl_files_list(old_blobs.clone(), new_blobs.clone())
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let target_blobs = self
            .get_commit_blobs(target_head)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let base_vs_target = edit_utils::cl_files_list(old_blobs.clone(), target_blobs.clone())
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let cl_paths: std::collections::HashSet<String> = cl_changed
            .iter()
            .map(|f| f.path().to_string_lossy().replace('\\', "/"))
            .collect();
        let target_paths: std::collections::HashSet<String> = base_vs_target
            .iter()
            .map(|f| f.path().to_string_lossy().replace('\\', "/"))
            .collect();

        Ok(cl_paths.intersection(&target_paths).cloned().collect())
    }

    pub async fn cl_files_list(
        &self,
        old_files: Vec<(PathBuf, ObjectHash)>,
        new_files: Vec<(PathBuf, ObjectHash)>,
    ) -> Result<Vec<ClDiffFile>, MegaError> {
        let base_diff = tree_diff::calculate_tree_diff_basic(old_files.clone(), new_files.clone())?;
        let mut blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();
        let mut failed_hashes = Vec::new();
        let candidate_hashes: HashSet<ObjectHash> = base_diff
            .iter()
            .filter_map(|item| match item {
                ClDiffFile::Deleted(_, hash) | ClDiffFile::New(_, hash) => Some(*hash),
                _ => None,
            })
            .collect();

        if base_diff.len() > LARGE_CL_RENAME_DETECTION_THRESHOLD
            || candidate_hashes.len() > LARGE_CL_RENAME_DETECTION_THRESHOLD
        {
            tracing::info!(
                diff_files = base_diff.len(),
                candidate_hashes = candidate_hashes.len(),
                threshold = LARGE_CL_RENAME_DETECTION_THRESHOLD,
                "Skipping rename detection for large CL diff and returning path-level results."
            );
            return Ok(base_diff);
        }

        for hash in candidate_hashes {
            match self.get_raw_blob_by_hash(&hash.to_string()).await {
                Ok(data) => {
                    blob_cache.insert(hash, data);
                }
                Err(err) => {
                    failed_hashes.push(hash);
                    tracing::warn!(
                        "rename detection skipped blob {} and will fall back to path-level diff: {}",
                        hash,
                        err
                    );
                }
            }
        }

        if !failed_hashes.is_empty() {
            tracing::warn!(
                "rename detection degraded for {} candidate blob(s)",
                failed_hashes.len()
            );
        }

        let rename_config = self.storage.config().monorepo.rename.clone();
        tree_diff::calculate_tree_diff_with_blobs(old_files, new_files, &rename_config, &blob_cache)
    }

    pub async fn get_commit_blobs(
        &self,
        commit_hash: &str,
    ) -> Result<Vec<(PathBuf, ObjectHash)>, MegaError> {
        let mut res = vec![];
        let mono_storage = self.storage.mono_storage();
        let commit = mono_storage.get_commit_by_hash(commit_hash).await?;
        if let Some(commit) = commit {
            let tree = mono_storage.get_tree_by_hash(&commit.tree).await?;
            if let Some(tree) = tree {
                let tree: Tree = Tree::from_mega_model(tree);
                res = self.traverse_tree(tree).await?;
            }
        }
        Ok(res)
    }

    pub async fn sync_third_party_repo(
        &self,
        owner: &str,
        repo: &str,
        mega_path: PathBuf,
    ) -> Result<Bytes, MegaError> {
        // Additional Path Parameter for Mega
        let url = format!("https://github.com/{owner}/{repo}.git");
        let remote_client = ThirdPartyClient::new(&url);

        let import_dir = self.storage.config().monorepo.import_dir.clone();
        let fetch_depth = if mega_path.starts_with(&import_dir) {
            None
        } else {
            Some(1)
        };

        let (ref_name, ref_hash) = remote_client.fetch_refs().await?;

        let res = remote_client
            .fetch_packs(std::slice::from_ref(&ref_hash), fetch_depth)
            .await?;
        let pack_data = remote_client
            .process_pack_stream(res)
            .await
            .map_err(|e| MegaError::Other(format!("{e}")))?;
        if pack_data.is_empty() {
            return Err(MegaError::Other(
                "GitHub sync failed: remote returned no pack data".to_string(),
            ));
        }

        let repo_path_str = mega_path
            .to_str()
            .ok_or_else(|| MegaError::Other("Invalid UTF-8 in mega_path".to_string()))?
            .to_string();

        let state = ProtocolApiState {
            storage: self.storage.clone(),
            git_object_cache: self.git_object_cache.clone(),
            entity_store: self.storage.entity_store(),
        };
        let mut protocol = SmartSession::from_state(
            mega_path,
            ServiceType::ReceivePack,
            TransportProtocol::Http,
            &state,
        )
        .map_err(|e| MegaError::Other(format!("{e}")))?;

        // Populate commands so `git_receive_pack_stream` can update import refs.
        // old_id: current ref hash if exists; otherwise ZERO_ID (create).
        let storage = self.storage.git_db_storage();
        let old_id = match storage.find_git_repo_exact_match(&repo_path_str).await? {
            Some(repo_model) => storage
                .get_ref(repo_model.id)
                .await?
                .into_iter()
                .find(|r| r.ref_name == ref_name)
                .map(|r| r.ref_git_id)
                .unwrap_or_else(|| ZERO_ID.to_string()),
            None => ZERO_ID.to_string(),
        };

        let commands = vec![crate::ceres::protocol::import_refs::RefCommand::new(
            old_id,
            ref_hash.clone(),
            ref_name.clone(),
        )];
        let bytes = protocol
            .git_receive_pack_stream(
                &state,
                commands,
                Box::pin(tokio_stream::once(Ok(Bytes::from(pack_data)))),
            )
            .await
            .map_err(|e| MegaError::Other(format!("{e}")))?;

        Ok(bytes)
    }

    async fn traverse_tree(
        &self,
        root_tree: Tree,
    ) -> Result<Vec<(PathBuf, ObjectHash)>, MegaError> {
        let mut result = vec![];
        let mut stack = vec![(PathBuf::new(), root_tree)];

        while let Some((base_path, tree)) = stack.pop() {
            for item in tree.tree_items {
                let path = base_path.join(&item.name);
                if item.is_tree() {
                    // UN-19: this walk runs on the merge path, where a missing
                    // subtree must become an undecidable check (503) rather
                    // than a panicked request.
                    let child = self
                        .storage
                        .mono_storage()
                        .get_tree_by_hash(&item.id.to_string())
                        .await?
                        .ok_or_else(|| {
                            MegaError::Other(format!(
                                "subtree `{}` of `{}` is missing",
                                item.id,
                                path.display()
                            ))
                        })?;
                    stack.push((path.clone(), Tree::from_mega_model(child)));
                } else {
                    result.push((path, item.id));
                }
            }
        }
        Ok(result)
    }

    // ========== Merge Queue Methods ==========

    /// Enqueue a CL merge via MonoWriteQueue, recording the subject that requested
    /// the merge (UN-20). `None` = anonymous.
    pub async fn add_to_merge_queue_as(
        &self,
        cl_link: String,
        requester: Option<String>,
    ) -> Result<i64, MegaError> {
        // Validate CL exists and is in Open status
        let cl = self.storage.cl_storage().get_cl(&cl_link).await?;
        let model = cl.ok_or(MegaError::Other("CL not found".to_string()))?;

        if model.status != MergeStatusEnum::Open {
            return Err(MegaError::Other(format!(
                "CL is not in Open status, current status: {:?}",
                model.status
            )));
        }

        self.ensure_merge_entry_prechecks(&model)
            .await
            .map_err(|e| MegaError::Other(e.to_string()))?;
        let execution_actor = requester.clone().unwrap_or_else(|| "system".into());
        let authz_principal = requester.clone().unwrap_or_else(|| "system".into());
        self.merge_cl_via_queue(&authz_principal, &execution_actor, model, true, requester)
            .await
            .map_err(|e| MegaError::Other(e.to_string()))
    }

    /// Refuse a merge that edits the authorization file unless the subject is
    /// an admin (UN-19).
    ///
    /// Three things can go wrong while deciding, and all of them mean the same
    /// thing — the change was not examined: the changed-file list cannot be
    /// read, main's copy of the authorization file cannot be resolved, or it is
    /// absent from main entirely. Under `enforce` each refuses the merge rather
    /// than letting an unexamined ACL change through; under `shadow` each is
    /// recorded and the merge proceeds.
    pub async fn enforce_acl_change_authorization(
        &self,
        cl_link: &str,
        authz_principal: &str,
    ) -> Result<(), GitError> {
        let enforcement = Enforcement::parse(&self.storage.config().cedar.enforcement)
            .unwrap_or(Enforcement::Off);
        if !enforcement.builds() {
            return Ok(());
        }

        if let Err(error) =
            ensure_authz_snapshot_caught_up(&self.storage, AUTHZ_BARRIER_TIMEOUT).await
        {
            return self.acl_check_unavailable(
                enforcement,
                cl_link,
                authz_principal,
                &error.to_string(),
            );
        }

        // An unresolvable change set reads back as "changed nothing", which
        // would silently pass an ACL change through. Establish that the CL's
        // endpoints exist before trusting the diff.
        if let Err(reason) = self.ensure_change_set_resolvable(cl_link).await {
            return self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason);
        }

        let touches_acl = match self.cl_touches_authz_file(cl_link).await {
            Ok(touches) => touches,
            Err(error) => {
                return self.acl_check_unavailable(
                    enforcement,
                    cl_link,
                    authz_principal,
                    &format!("the changed-file list could not be read: {error}"),
                );
            }
        };

        if touches_acl {
            // The change touches the ACL, so main's own copy has to be
            // resolvable — without it there is no baseline to review against.
            if let Err(reason) = self.resolve_main_acl_blob().await {
                return self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason);
            }
        }

        let snapshot = self.storage.entity_store().snapshot();
        match decide_acl_change(
            enforcement,
            snapshot.as_deref(),
            touches_acl,
            authz_principal,
        ) {
            AclChangeDecision::Proceed => Ok(()),
            AclChangeDecision::Unavailable { reason } => {
                self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason)
            }
            AclChangeDecision::Refuse { reason } => {
                if enforcement.records_would_deny() {
                    tracing::warn!(
                        event = "authz_would_deny",
                        principal = %authz_principal,
                        principal_type = "User",
                        action = "addAdmin",
                        resource = %MEGA_CEDAR_PATH,
                        "would-deny recorded: this ACL change would be refused under enforce"
                    );
                }
                if enforcement.enforces() {
                    return Err(GitError::CustomError(format!("[code:403] {reason}")));
                }
                Ok(())
            }
        }
    }

    /// Shared handling of the three "could not decide" classes.
    fn acl_check_unavailable(
        &self,
        enforcement: Enforcement,
        cl_link: &str,
        authz_principal: &str,
        reason: &str,
    ) -> Result<(), GitError> {
        emit_merge_authz_unavailable(cl_link, authz_principal, reason);
        if enforcement.enforces() {
            // 503, not 500: the change was not rejected, it was not examined —
            // the same request can succeed once authorization is available
            // again (UN-25's contract).
            return Err(GitError::CustomError(format!(
                "[code:503] merge authorization unavailable: {reason}"
            )));
        }
        Ok(())
    }

    /// Whether a CL touches `/.mega_cedar.json` on either side of its diff.
    ///
    /// Both sides matter: a rename reports only its *new* path, so looking at
    /// that alone would let someone move the authorization file out of the way
    /// — deleting it from where it is read — without ever tripping the check.
    async fn cl_touches_authz_file(&self, cl_link: &str) -> Result<bool, MegaError> {
        let acl_file = MEGA_CEDAR_PATH.trim_start_matches('/');
        let is_acl = |path: &std::path::Path| {
            let normalized = path.to_string_lossy().replace('\\', "/");
            normalized == acl_file || normalized.ends_with(&format!("/{acl_file}"))
        };

        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {cl_link}")))?;
        let old_files = self.get_commit_blobs(&cl.from_hash).await?;
        let new_files = self.get_commit_blobs(&cl.to_hash).await?;

        Ok(self
            .cl_files_list(old_files, new_files)
            .await?
            .iter()
            .any(|file| match file {
                ClDiffFile::Renamed(old_path, new_path, ..)
                | ClDiffFile::Moved(old_path, new_path, ..) => is_acl(old_path) || is_acl(new_path),
                other => is_acl(other.path()),
            }))
    }

    /// Both endpoints of the CL's diff must exist, or the changed-file list is
    /// not evidence of anything: a missing commit yields an empty list, which
    /// is indistinguishable from "this CL changes nothing".
    async fn ensure_change_set_resolvable(&self, cl_link: &str) -> Result<(), String> {
        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await
            .map_err(|e| format!("the CL could not be read: {e}"))?
            .ok_or_else(|| format!("CL `{cl_link}` not found"))?;

        let storage = self.storage.mono_storage();
        for (label, hash) in [("base", &cl.from_hash), ("tip", &cl.to_hash)] {
            let commit = storage
                .get_commit_by_hash(hash)
                .await
                .map_err(|e| format!("the CL's {label} commit could not be read: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "the CL's {label} commit `{hash}` is missing, so its changed-file list \
                         cannot be computed"
                    )
                })?;

            // The tree matters as much as the commit: `get_commit_blobs`
            // returns an empty list for a commit whose tree is gone, which
            // reads back as "changed nothing" — the same fail-open shape.
            let tree_missing = storage
                .get_tree_by_hash(&commit.tree)
                .await
                .map_err(|e| format!("the CL's {label} tree could not be read: {e}"))?
                .is_none();
            if tree_missing {
                return Err(format!(
                    "the tree `{}` of the CL's {label} commit is missing, so its changed-file \
                     list cannot be computed",
                    commit.tree
                ));
            }
        }
        Ok(())
    }

    /// Resolve the blob id of main's `/.mega_cedar.json`, or say why not.
    async fn resolve_main_acl_blob(&self) -> Result<String, String> {
        let storage = self.storage.mono_storage();
        let main_ref = storage
            .get_main_ref("/")
            .await
            .map_err(|e| format!("main ref could not be read: {e}"))?
            .ok_or_else(|| "main ref not found".to_owned())?;
        let tree = storage
            .get_tree_by_hash(&main_ref.ref_tree_hash)
            .await
            .map_err(|e| format!("main tree could not be read: {e}"))?
            .ok_or_else(|| "main tree not found".to_owned())?;
        authz_blob_id(&Tree::from_mega_model(tree)).ok_or_else(|| {
            format!("{MEGA_CEDAR_PATH} is absent from main, so there is no baseline to review")
        })
    }

    /// Freeze a queued CL because authorization could not be decided (UN-25).
    ///
    /// "Frozen" reuses the existing terminal state rather than inventing one:
    /// `Failed` + `SystemError`, which the queue already understands and the
    /// existing retry entry point already accepts. Two properties matter for a
    /// frozen item:
    ///
    /// * the recorded `requester` is **kept** — it is the authorization subject
    ///   the merge will be re-decided against, and losing it would turn a retry
    ///   into an unattributed merge;
    /// * `error_message` states the condition under which a retry can succeed,
    ///   so an operator is not left guessing whether to retry or to escalate.
    ///
    /// The alert is a process-local `error` log written synchronously in the
    /// same call: there is no external call that could fail and no way for
    /// alerting to interfere with the freeze itself.
    pub async fn freeze_merge_queue_item_for_authz(
        &self,
        cl_link: &str,
        reason: &str,
    ) -> Result<bool, MegaError> {
        let rows = self
            .storage
            .push_queue_storage()
            .list_by_kind_and_operation(PushQueueKindEnum::Merge, cl_link)
            .await?;
        let requester = rows
            .iter()
            .find(|row| {
                matches!(
                    row.status,
                    PushQueueStatusEnum::Queued | PushQueueStatusEnum::Running
                )
            })
            .and_then(|row| row.requester.clone());

        let frozen = self
            .storage
            .push_queue_storage()
            .freeze_merge_for_authz(cl_link, &authz_freeze_message(reason))
            .await?;

        if frozen {
            emit_authz_frozen_alert(cl_link, requester.as_deref(), reason);
        }

        Ok(frozen)
    }

    /// Retry a CL merge via MonoWriteQueue, recording the subject that requested
    /// the retry (UN-20). `None` = anonymous.
    pub async fn retry_merge_queue_item_as(
        &self,
        cl_link: &str,
        requester: Option<String>,
    ) -> Result<bool, MegaError> {
        self.add_to_merge_queue_as(cl_link.to_owned(), requester)
            .await?;
        Ok(true)
    }

    // ========== Buck Upload API Methods ==========

    /// Create buck upload session.
    ///
    /// # Arguments
    /// * `username` - User creating the session
    /// * `path` - Repository path (may be with or without leading `/`; normalized to match mega_refs format)
    ///
    /// # Returns
    /// Returns `SessionResponse` on success
    pub async fn create_buck_session(
        &self,
        username: &str,
        path: &str,
    ) -> Result<crate::jupiter::service::buck_service::SessionResponse, MegaError> {
        let normalized_path = MonoServiceLogic::normalize_repo_path(path)?;
        let refs = self
            .storage
            .mono_storage()
            .get_main_ref(&normalized_path)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Path not found: {}", normalized_path)))?;
        let base_branch = refs
            .ref_name
            .strip_prefix("refs/heads/")
            .unwrap_or(refs.ref_name.as_str())
            .to_string();
        // Use canonical path from mega_refs as the single source of truth for repository path
        let canonical_path = refs.path.clone();
        let response = self
            .storage
            .buck_service
            .create_session(
                username,
                &canonical_path,
                &base_branch,
                refs.ref_commit_hash,
            )
            .await?;

        Ok(response)
    }

    /// Process buck upload manifest.
    ///
    /// # Arguments
    /// * `username` - User processing the manifest
    /// * `cl_link` - CL link
    /// * `payload` - Manifest payload
    ///
    /// # Returns
    /// Returns `ManifestResponse` on success
    pub async fn process_buck_manifest(
        &self,
        username: &str,
        cl_link: &str,
        payload: ManifestPayload,
    ) -> Result<ManifestResponse, MegaError> {
        let session = self
            .storage
            .buck_storage()
            .get_session(cl_link)
            .await?
            .ok_or_else(|| MegaError::Buck(BuckError::SessionNotFound(cl_link.to_string())))?;

        if session.user_id != username {
            return Err(MegaError::Buck(BuckError::Forbidden(
                "Session belongs to another user".to_string(),
            )));
        }

        let manifest_paths: Vec<PathBuf> = payload
            .files
            .iter()
            .map(|f| PathBuf::from(&f.path))
            .collect();

        // Get content hashes (raw SHA-1) and blob IDs
        let (existing_file_hashes, existing_blob_ids_map) =
            crate::ceres::api_service::blob_ops::get_files_content_hashes_with_blob_ids(
                self,
                &manifest_paths,
                session.from_hash.as_deref(),
            )
            .await
            .map_err(MegaError::Git)?;

        // Convert ObjectHash to String for storage
        let existing_blob_ids: HashMap<PathBuf, String> = existing_blob_ids_map
            .into_iter()
            .map(|(path, blob_hash)| (path, blob_hash.to_string()))
            .collect();

        // Convert payload to service layer type
        let service_payload = crate::jupiter::service::buck_service::ManifestPayload {
            files: payload
                .files
                .iter()
                .map(|f| crate::jupiter::service::buck_service::ManifestFile {
                    path: f.path.clone(),
                    size: f.size,
                    hash: f.hash.clone(),
                })
                .collect(),
            commit_message: payload.commit_message.clone(),
        };

        let svc_resp = self
            .storage
            .buck_service
            .process_manifest(
                username,
                cl_link,
                service_payload,
                existing_file_hashes,
                existing_blob_ids,
            )
            .await?;

        // Convert back to API layer response
        let api_resp = ManifestResponse {
            total_files: svc_resp.total_files,
            total_size: svc_resp.total_size,
            files_to_upload: svc_resp
                .files_to_upload
                .into_iter()
                .map(|f| ApiFileToUpload {
                    path: f.path,
                    reason: f.reason,
                })
                .collect(),
            files_unchanged: svc_resp.files_unchanged,
            upload_size: svc_resp.upload_size,
        };

        Ok(api_resp)
    }

    /// Complete buck upload.
    ///
    /// Commit message is read from session.commit_message which is set during Manifest phase.
    /// The payload is intentionally unused (empty struct).
    ///
    /// # Arguments
    /// * `username` - User completing the upload
    /// * `cl_link` - CL link
    /// * `_payload` - Empty payload (unused). Commit message is read from session.commit_message
    ///   which is set during Manifest phase.
    ///
    /// # Returns
    /// Returns `CompleteResponse` on success
    pub async fn complete_buck_upload(
        &self,
        username: &str,
        cl_link: &str,
        _payload: CompletePayload,
    ) -> Result<CompleteResponse, MegaError> {
        let session = self
            .storage
            .buck_storage()
            .get_session(cl_link)
            .await?
            .ok_or_else(|| MegaError::Buck(BuckError::SessionNotFound(cl_link.to_string())))?;

        if session.user_id != username {
            return Err(MegaError::Buck(BuckError::Forbidden(
                "Session belongs to another user".to_string(),
            )));
        }

        if ![session_status::MANIFEST_UPLOADED, session_status::UPLOADING]
            .contains(&session.status.as_str())
        {
            return Err(MegaError::Buck(BuckError::InvalidSessionStatus {
                expected: format!(
                    "{} or {}",
                    session_status::MANIFEST_UPLOADED,
                    session_status::UPLOADING
                ),
                actual: session.status.clone(),
            }));
        }

        let pending = self
            .storage
            .buck_storage()
            .count_pending_files(cl_link)
            .await?;
        if pending > 0 {
            return Err(MegaError::Buck(BuckError::FilesNotFullyUploaded {
                missing_count: pending as u32,
            }));
        }

        let all_files = self.storage.buck_storage().get_all_files(cl_link).await?;
        for file in &all_files {
            if file.blob_id.is_none() {
                return Err(MegaError::Buck(BuckError::ValidationError(format!(
                    "Missing blob_id for file: {} (status: {})",
                    file.file_path, file.upload_status
                ))));
            }
        }

        // Build commit
        let file_changes: Vec<FileChange> = all_files
            .iter()
            .filter(|f| f.upload_status == upload_status::UPLOADED)
            .map(|f| {
                let blob_id = f.blob_id.as_ref().unwrap();
                let normalized_blob_id =
                    format!("sha1:{}", blob_id.strip_prefix("sha1:").unwrap_or(blob_id));
                FileChange::new(
                    f.file_path.clone(),
                    normalized_blob_id,
                    f.file_mode
                        .clone()
                        .unwrap_or_else(|| DEFAULT_MODE.to_string()),
                )
            })
            .collect();

        // Use commit_message from session
        let commit_message = session
            .commit_message
            .clone()
            .unwrap_or_else(|| "Upload via buck push".to_string());

        let commit_result = if file_changes.is_empty() {
            None
        } else {
            // MC-09: signing precheck (vault read + key parse) runs before
            // any ref/commit/tree/CL persistence in this request; a failure
            // here leaves zero side effects.
            let signing = self.server_signing_context()?;
            let signing_key = signing.active_key().await?;

            let builder = BuckCommitBuilder::new(self.storage.mono_storage());
            let mut result = builder
                .build_commit(
                    session.from_hash.as_deref().unwrap_or_default(),
                    &file_changes,
                    &commit_message,
                )
                .await?;

            // Sign the synthesized commit and recompute its id, then backfill
            // every derived reference: each tree model's `commit_id` was
            // stamped at construction (`buck_tree_builder`) and does not
            // follow `res.commit`/`res.commit_id`, so update them one by one.
            // Everything downstream (`buck_service::complete_upload`) consumes
            // this single signed hash.
            let signed_commit = signing.sign_commit(&signing_key, &result.commit)?;
            let signed_id = signed_commit.id.to_string();
            result.commit = signed_commit;
            result.commit_id = signed_id.clone();
            for tree_model in &mut result.new_tree_models {
                tree_model.commit_id = signed_id.clone();
            }
            Some(result)
        };

        // Convert to artifacts acceptable by BuckService
        let artifacts = commit_result.map(|res| {
            let commit_model: crate::callisto::mega_commit::ActiveModel = res
                .commit
                .clone()
                .into_mega_model(git_internal::internal::metadata::EntryMeta::default())
                .into();
            let new_tree_models: Vec<crate::callisto::mega_tree::ActiveModel> =
                res.new_tree_models.into_iter().map(|m| m.into()).collect();
            CommitArtifacts {
                commit_id: res.commit_id,
                tree_hash: res.tree_hash,
                new_tree_models,
                commit_model,
            }
        });

        let svc_resp: SvcCompleteResponse = self
            .storage
            .buck_service
            .complete_upload(username, cl_link, SvcCompletePayload {}, artifacts)
            .await?;

        // Calculate uploaded files count
        let uploaded_files_count = file_changes.len() as u32;

        let response = CompleteResponse {
            cl_id: session.id,
            cl_link: session.session_id.clone(),
            commit_id: svc_resp.commit_id,
            files_count: uploaded_files_count,
            created_at: session.created_at.to_string(),
            repo_path: session.repo_path.clone(),
            from_hash: session.from_hash.clone().unwrap_or_default(),
        };

        self.trigger_build_for_buck_upload(&response, username);

        Ok(response)
    }
}

fn collect_page_blobs(
    items: &[ClDiffFile],
    old_out: &mut Vec<(PathBuf, ObjectHash)>,
    new_out: &mut Vec<(PathBuf, ObjectHash)>,
) {
    old_out.reserve(items.len());
    new_out.reserve(items.len());

    for item in items {
        match item {
            ClDiffFile::New(p, h_new) => {
                new_out.push((p.clone(), *h_new));
            }
            ClDiffFile::Deleted(p, h_old) => {
                old_out.push((p.clone(), *h_old));
            }
            ClDiffFile::Modified(p, h_old, h_new) => {
                old_out.push((p.clone(), *h_old));
                new_out.push((p.clone(), *h_new));
            }
            ClDiffFile::Renamed(_, _, _, _, _) | ClDiffFile::Moved(_, _, _, _, _) => {
                // Relocated items are filtered out before this helper is called.
                debug_assert!(false, "collect_page_blobs only accepts non-relocated items");
            }
        }
    }
}
#[cfg(test)]
mod test {
    use std::{path::PathBuf, str::FromStr, sync::Arc};

    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            signature::{Signature, SignatureType},
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };

    use super::*;
    use crate::ceres::model::change_list::ClDiffFile;

    #[test]
    fn test_clean_path_str_edges() {
        assert_eq!(MonoServiceLogic::clean_path_str(""), "/");
        assert_eq!(MonoServiceLogic::clean_path_str("/"), "/");
        assert_eq!(MonoServiceLogic::clean_path_str("abc/"), "abc");
        assert_eq!(MonoServiceLogic::clean_path_str("abc///"), "abc");
    }

    #[test]
    fn test_normalize_repo_path() {
        use crate::common::errors::{BuckError, MegaError};

        // Normalization: add leading slash, strip trailing
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("project").unwrap(),
            "/project"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("/project").unwrap(),
            "/project"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("project/").unwrap(),
            "/project"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("/project/").unwrap(),
            "/project"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("  /project  ").unwrap(),
            "/project"
        );
        assert_eq!(MonoServiceLogic::normalize_repo_path("/").unwrap(), "/");

        // Empty / whitespace-only -> ValidationError
        assert!(MonoServiceLogic::normalize_repo_path("").is_err());
        assert!(MonoServiceLogic::normalize_repo_path("   ").is_err());
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path(""),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));

        // Path traversal and invalid chars -> ValidationError
        assert!(MonoServiceLogic::normalize_repo_path("project/../foo").is_err());
        assert!(MonoServiceLogic::normalize_repo_path("project\\foo").is_err());

        // Middle slashes and "." segments are collapsed
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("//project//foo//").unwrap(),
            "/project/foo"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("project/./foo").unwrap(),
            "/project/foo"
        );
        assert_eq!(
            MonoServiceLogic::normalize_repo_path("/project/./foo").unwrap(),
            "/project/foo"
        );

        // Dot-only paths are rejected (do not silently resolve to root)
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path("."),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path("./"),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path("./."),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));

        // Leading colon is rejected
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path(":/test"),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path(":"),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));

        // Windows drive letters are rejected
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path("C:"),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));
        assert!(matches!(
            MonoServiceLogic::normalize_repo_path("D:/project"),
            Err(MegaError::Buck(BuckError::ValidationError(_)))
        ));
    }

    #[test]
    fn test_repo_root_candidates_walk_from_leaf_to_root() {
        assert_eq!(
            MonoServiceLogic::repo_root_candidates(Path::new("/project/buck2_test/src")),
            vec![
                "/project/buck2_test/src".to_string(),
                "/project/buck2_test".to_string(),
                "/project".to_string(),
                "/".to_string(),
            ]
        );
    }

    #[test]
    fn test_repo_root_candidates_normalize_relative_paths() {
        assert_eq!(
            MonoServiceLogic::repo_root_candidates(Path::new("project/buck2_test/src")),
            vec![
                "/project/buck2_test/src".to_string(),
                "/project/buck2_test".to_string(),
                "/project".to_string(),
                "/".to_string(),
            ]
        );
    }

    #[test]
    fn test_subtree_ref_path_keeps_parent_directory_for_file_edits() {
        assert_eq!(
            MonoServiceLogic::subtree_ref_path(Path::new("/project/buck2_test/src")).unwrap(),
            "/project/buck2_test/src".to_string()
        );
    }

    #[test]
    fn test_subtree_ref_path_normalizes_relative_create_paths() {
        assert_eq!(
            MonoServiceLogic::subtree_ref_path(Path::new("project/buck2_test/src")).unwrap(),
            "/project/buck2_test/src".to_string()
        );
    }

    #[test]
    fn test_save_file_edit_uses_build_repo_root_tree_from_ref_updates() {
        let build_root_tree =
            ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let nested_tree = ObjectHash::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let result = TreeUpdateResult {
            updated_trees: vec![],
            ref_updates: vec![
                RefUpdate {
                    path: "/project/buck2_test/src".to_string(),
                    tree_id: nested_tree,
                },
                RefUpdate {
                    path: "/project/buck2_test".to_string(),
                    tree_id: build_root_tree,
                },
            ],
        };

        let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test");
        assert_eq!(selected, Some(build_root_tree));
    }

    #[test]
    fn test_create_monorepo_entry_uses_normalized_build_repo_root_tree() {
        let build_root_tree =
            ObjectHash::from_str("cccccccccccccccccccccccccccccccccccccccc").unwrap();
        let result = TreeUpdateResult {
            updated_trees: vec![],
            ref_updates: vec![RefUpdate {
                path: "/project/buck2_test".to_string(),
                tree_id: build_root_tree,
            }],
        };

        let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test/");
        assert_eq!(selected, Some(build_root_tree));
    }

    #[test]
    fn test_update_tree_hash() {
        let item = TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            "path".to_string(),
        );

        let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
        let tree = Arc::new(tree);

        let new_hash = ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();

        let new_tree = MonoServiceLogic::update_tree_hash(tree, "path", new_hash)
            .expect("update_tree_hash should succeed");

        assert_eq!(new_tree.tree_items.len(), 1);
        assert_eq!(new_tree.tree_items[0].id, new_hash);
    }

    #[test]
    fn test_build_result_by_chain_logic() {
        let item = TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            "path".to_string(),
        );

        let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
        let tree_id = tree.id;

        let update_chain = vec![Arc::new(tree)];
        let path = PathBuf::from("/test/path");

        let result = MonoServiceLogic::build_result_by_chain(path, update_chain, tree_id)
            .expect("build_result_by_chain should succeed");

        assert_eq!(result.updated_trees.len(), 1);
        assert_eq!(result.ref_updates.len(), 2);

        let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/test/path"));
        assert!(paths.contains(&"/test"));
    }

    #[test]
    fn test_build_result_by_chain_normalizes_relative_paths_for_ref_updates() {
        let old_hash = ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap();
        let updated_child_hash =
            ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap();
        let item = TreeItem::new(TreeItemMode::Tree, old_hash, "src".to_string());
        let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
        let update_chain = vec![Arc::new(tree)];

        let result = MonoServiceLogic::build_result_by_chain(
            PathBuf::from("project/buck2_test/src"),
            update_chain,
            updated_child_hash,
        )
        .expect("build_result_by_chain should succeed");

        let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/project/buck2_test/src"));
        assert!(paths.contains(&"/project/buck2_test"));

        let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test");
        assert!(selected.is_some());
    }

    #[tokio::test]
    async fn test_process_ref_updates_logic() {
        let ref_update = RefUpdate {
            path: "/test".to_string(),
            tree_id: ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        };

        let tree_update_result = TreeUpdateResult {
            updated_trees: vec![],
            ref_updates: vec![RefUpdate {
                path: ref_update.path.clone(),
                tree_id: ref_update.tree_id,
            }],
        };

        let refs = vec![mega_refs::Model {
            id: 1,
            path: "/test".to_string(),
            ref_name: "refs/heads/main".to_string(),
            ref_commit_hash: "0987654321098765432109876543210987654321".to_string(),
            ref_tree_hash: "1111111111111111111111111111111111111111".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        }];

        let mut commits: Vec<Commit> = Vec::new();
        let mut updates: Vec<RefUpdateData> = Vec::new();
        let mut new_commit_id = String::new();

        let result = MonoServiceLogic::process_ref_updates(
            &tree_update_result,
            &refs,
            "test commit message",
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        );

        assert!(result.is_ok());
        assert_eq!(commits.len(), 1);
        assert_eq!(updates.len(), 1);
        assert!(!new_commit_id.is_empty());

        let created_commit = &commits[0];

        assert_eq!(
            created_commit.tree_id,
            tree_update_result.ref_updates[0].tree_id
        );
        let expected_parent = ObjectHash::from_str(&refs[0].ref_commit_hash).unwrap();
        assert_eq!(created_commit.parent_commit_ids, vec![expected_parent]);

        assert_eq!(updates[0].ref_name, refs[0].ref_name);
        assert_eq!(updates[0].commit_id, new_commit_id);
    }

    #[tokio::test]
    async fn test_root_merge_flow_updates_cl_ref_and_main() {
        let merged_tree_id =
            ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let root_result =
            MonoServiceLogic::build_result_by_chain(PathBuf::from("/"), vec![], merged_tree_id)
                .expect("root path should build a valid update result");

        assert_eq!(root_result.ref_updates.len(), 1);
        assert_eq!(root_result.ref_updates[0].path, "/");

        let refs = vec![
            mega_refs::Model {
                id: 1,
                path: "/".to_string(),
                ref_name: "refs/cl/abcd1234".to_string(),
                ref_commit_hash: "1111111111111111111111111111111111111111".to_string(),
                ref_tree_hash: "2222222222222222222222222222222222222222".to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: true,
            },
            mega_refs::Model {
                id: 2,
                path: "/".to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: "3333333333333333333333333333333333333333".to_string(),
                ref_tree_hash: "4444444444444444444444444444444444444444".to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
        ];

        let mut commits = Vec::new();
        let mut updates = Vec::new();
        let mut new_commit_id = String::new();

        MonoServiceLogic::process_ref_updates(
            &root_result,
            &refs,
            "merge root cl",
            &mut commits,
            &mut updates,
            &mut new_commit_id,
        )
        .expect("root merge flow should produce ref updates");

        assert_eq!(commits.len(), 1);
        assert_eq!(updates.len(), 2);
        assert!(!new_commit_id.is_empty());

        assert_eq!(updates[0].ref_name, "refs/cl/abcd1234");
        assert_eq!(updates[1].ref_name, MEGA_BRANCH_NAME);
        assert!(updates.iter().all(|u| u.path == "/"));
        assert!(
            updates
                .iter()
                .all(|u| u.tree_hash == merged_tree_id.to_string())
        );
    }

    #[test]
    fn test_map_tree_items_to_commits() {
        let id1 = ObjectHash::Sha1([1u8; 20]);
        let id2 = ObjectHash::Sha1([2u8; 20]);
        let commit_hash = ObjectHash::Sha1([3u8; 20]);

        let item1 = TreeItem {
            id: id1,
            name: "file1.txt".into(),
            mode: TreeItemMode::Blob,
        };
        let item2 = TreeItem {
            id: id2,
            name: "file2.txt".into(),
            mode: TreeItemMode::Blob,
        };

        let tree = Tree {
            id: ObjectHash::Sha1([9u8; 20]),
            tree_items: vec![item1.clone(), item2.clone()],
        };

        let mut item_to_commit_id = HashMap::new();
        item_to_commit_id.insert(id1.to_string(), commit_hash.to_string());

        let fake_sig = Signature {
            signature_type: SignatureType::Committer,
            name: "tester".into(),
            email: "tester@example.com".into(),
            timestamp: 0,
            timezone: "+0000".into(),
        };

        let commit_a = Commit {
            id: commit_hash,
            tree_id: ObjectHash::Sha1([8u8; 20]),
            parent_commit_ids: vec![],
            author: fake_sig.clone(),
            committer: fake_sig.clone(),
            message: "test commit".into(),
        };

        let mut commit_map = HashMap::new();
        commit_map.insert(commit_hash.to_string(), commit_a.clone());

        let result =
            MonoServiceLogic::map_tree_items_to_commits(tree, &item_to_commit_id, &commit_map);

        assert_eq!(result.get(&item1), Some(&Some(commit_a)));
        assert_eq!(result.get(&item2), Some(&None));
    }

    #[test]
    fn test_path_traversal_with_pop() {
        let mut full_path = PathBuf::from("/project/rust/mega");
        for _ in 0..3 {
            let cloned_path = full_path.clone(); // Clone full_path
            let name = cloned_path.file_name().unwrap().to_str().unwrap();
            full_path.pop();
            println!("name: {name}, path: {full_path:?}");
        }
    }

    #[test]
    fn test_paging_calculation_basic() {
        let files: Vec<ClDiffFile> = vec![
            ClDiffFile::New(
                PathBuf::from("file1.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ),
            ClDiffFile::Modified(
                PathBuf::from("file2.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
                ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
            ),
            ClDiffFile::Deleted(
                PathBuf::from("file3.txt"),
                ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
            ),
        ];

        let page_size = 2u32;
        let page_id = 1u32;

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 0);
        assert_eq!(end, 2);

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 2);
    }

    #[test]
    fn test_paging_calculation_second_page() {
        let files: Vec<ClDiffFile> = vec![
            ClDiffFile::New(
                PathBuf::from("file1.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ),
            ClDiffFile::Modified(
                PathBuf::from("file2.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
                ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
            ),
            ClDiffFile::Deleted(
                PathBuf::from("file3.txt"),
                ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
            ),
            ClDiffFile::New(
                PathBuf::from("file4.txt"),
                ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap(),
            ),
        ];

        let page_size = 2u32;
        let page_id = 2u32;

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 2);
        assert_eq!(end, 4);

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 2);
        assert_eq!(page_slice[0].path(), &PathBuf::from("file3.txt"));
        assert_eq!(page_slice[1].path(), &PathBuf::from("file4.txt"));
    }

    #[test]
    fn test_paging_calculation_partial_page() {
        let files: Vec<ClDiffFile> = vec![
            ClDiffFile::New(
                PathBuf::from("file1.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ),
            ClDiffFile::Modified(
                PathBuf::from("file2.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
                ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
            ),
            ClDiffFile::Deleted(
                PathBuf::from("file3.txt"),
                ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
            ),
        ];

        let page_size = 5u32;
        let page_id = 1u32;

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 0);
        assert_eq!(end, 3);

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 3);
    }

    #[test]
    fn test_paging_calculation_out_of_bounds() {
        let files: Vec<ClDiffFile> = vec![ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        )];

        let page_size = 2u32;
        let page_id = 3u32; // Page that doesn't exist

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 4);
        assert_eq!(end, 1); // end is clamped to files.len()

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 0);
    }

    #[test]
    fn test_paging_calculation_edge_case_zero_page_size() {
        let files: Vec<ClDiffFile> = vec![ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        )];

        let page_size = 0u32;
        let page_id = 1u32;

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 0);
        assert_eq!(end, 0);

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 0);
    }

    #[test]
    fn test_paging_calculation_zero_page_id() {
        let files: Vec<ClDiffFile> = vec![
            ClDiffFile::New(
                PathBuf::from("file1.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ),
            ClDiffFile::Modified(
                PathBuf::from("file2.txt"),
                ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
                ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
            ),
        ];

        let page_size = 2u32;
        let page_id = 0u32; // Should be treated as page 1 due to saturating_sub

        let start = (page_id.saturating_sub(1)) * page_size;
        let end = (start + page_size).min(files.len() as u32);

        assert_eq!(start, 0);
        assert_eq!(end, 2);

        let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
            let start_idx = start as usize;
            let end_idx = end as usize;
            &files[start_idx..end_idx]
        } else {
            &[]
        };

        assert_eq!(page_slice.len(), 2);
    }

    #[test]
    fn test_paging_algorithm() {
        let total_files = 10usize;
        let current_page = 2u32;
        let page_size = 3u32;

        let total_pages = total_files.div_ceil(page_size as usize);
        let current_page = current_page as usize;
        let page_size = page_size as usize;

        assert_eq!(total_pages, 4);
        assert_eq!(current_page, 2);
        assert_eq!(page_size, 3);
    }

    #[test]
    fn test_collect_page_blobs_new_files() {
        let files = vec![ClDiffFile::New(
            PathBuf::from("new_file.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        )];

        let mut old_blobs = Vec::new();
        let mut new_blobs = Vec::new();

        collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

        assert_eq!(old_blobs.len(), 0);
        assert_eq!(new_blobs.len(), 1);
        assert_eq!(new_blobs[0].0, PathBuf::from("new_file.txt"));
    }

    #[test]
    fn test_collect_page_blobs_deleted_files() {
        let files = vec![ClDiffFile::Deleted(
            PathBuf::from("deleted_file.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        )];

        let mut old_blobs = Vec::new();
        let mut new_blobs = Vec::new();

        collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

        assert_eq!(old_blobs.len(), 1);
        assert_eq!(new_blobs.len(), 0);
        assert_eq!(old_blobs[0].0, PathBuf::from("deleted_file.txt"));
    }

    #[test]
    fn test_file_lists_with_roots() {
        let all_files = vec![
            "src/main.rs".to_string(),
            "src/utils/math.rs".to_string(),
            "src/utils/io.rs".to_string(),
            "README.md".to_string(),
        ];

        let root: Option<&str> = None;
        let filtered_none: Vec<String> = all_files
            .iter()
            .filter(|file_path| {
                if let Some(prefix) = root {
                    file_path.starts_with(prefix)
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        assert_eq!(filtered_none.len(), 4);
        assert_eq!(filtered_none, all_files);

        let filtered_some: Vec<String> = all_files
            .iter()
            .filter(|file_path| {
                if let Some(prefix) = Some("src/utils") {
                    file_path.starts_with(prefix)
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        assert_eq!(filtered_some.len(), 2);
        assert_eq!(
            filtered_some,
            vec![
                "src/utils/math.rs".to_string(),
                "src/utils/io.rs".to_string()
            ]
        );
    }

    #[test]
    fn test_collect_page_blobs_modified_files() {
        let files = vec![ClDiffFile::Modified(
            PathBuf::from("modified_file.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
        )];

        let mut old_blobs = Vec::new();
        let mut new_blobs = Vec::new();

        collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

        assert_eq!(old_blobs.len(), 1);
        assert_eq!(new_blobs.len(), 1);
        assert_eq!(old_blobs[0].0, PathBuf::from("modified_file.txt"));
        assert_eq!(new_blobs[0].0, PathBuf::from("modified_file.txt"));
    }

    #[test]
    fn test_collect_page_blobs_mixed_files() {
        let files = vec![
            ClDiffFile::New(
                PathBuf::from("new.txt"),
                ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
            ),
            ClDiffFile::Deleted(
                PathBuf::from("deleted.txt"),
                ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap(),
            ),
            ClDiffFile::Modified(
                PathBuf::from("modified.txt"),
                ObjectHash::from_str("3333333333333333333333333333333333333333").unwrap(),
                ObjectHash::from_str("4444444444444444444444444444444444444444").unwrap(),
            ),
        ];

        let mut old_blobs = Vec::new();
        let mut new_blobs = Vec::new();

        collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

        assert_eq!(old_blobs.len(), 2); // deleted + modified
        assert_eq!(new_blobs.len(), 2); // new + modified

        assert_eq!(old_blobs[0].0, PathBuf::from("deleted.txt"));
        assert_eq!(old_blobs[1].0, PathBuf::from("modified.txt"));
        assert_eq!(new_blobs[0].0, PathBuf::from("new.txt"));
        assert_eq!(new_blobs[1].0, PathBuf::from("modified.txt"));
    }

    #[test]
    fn test_relocate_patch_body_rewrites_paths_and_keeps_hunk() {
        let raw_patch = "\
diff --git a/old/name.txt b/old/name.txt\n\
index 1111111..2222222 100644\n\
--- a/old/name.txt\n\
+++ b/old/name.txt\n\
@@ -1 +1 @@\n\
-old line\n\
+new line\n";

        let relocated = MonoApiService::relocate_patch_body(
            raw_patch,
            Path::new("old/name.txt"),
            Path::new("new/name.txt"),
        );

        assert!(!relocated.contains("diff --git"));
        assert!(relocated.contains("--- a/old/name.txt"));
        assert!(relocated.contains("+++ b/new/name.txt"));
        assert!(relocated.contains("@@ -1 +1 @@"));
        assert!(relocated.contains("-old line"));
        assert!(relocated.contains("+new line"));
        assert!(!relocated.contains("deleted file mode"));
    }

    #[test]
    fn test_relocate_patch_body_preserves_hunk_backslashes() {
        let raw_patch = "\
diff --git a/old/name.txt b/old/name.txt\n\
index 1111111..2222222 100644\n\
--- a/old/name.txt\n\
+++ b/old/name.txt\n\
@@ -1 +1 @@\n\
-let path = \"C:\\\\temp\\\\old\";\n\
+let path = \"C:\\\\temp\\\\new\";\n";

        let relocated = MonoApiService::relocate_patch_body(
            raw_patch,
            Path::new("old/name.txt"),
            Path::new("new/name.txt"),
        );

        assert!(relocated.contains("--- a/old/name.txt"));
        assert!(relocated.contains("+++ b/new/name.txt"));
        assert!(relocated.contains("-let path = \"C:\\\\temp\\\\old\";"));
        assert!(relocated.contains("+let path = \"C:\\\\temp\\\\new\";"));
    }

    #[test]
    fn test_normalize_diff_item_path_uses_forward_slashes() {
        let item = DiffItem {
            path: "dir\\nested\\file.txt".to_string(),
            data: "diff --git a/dir\\nested\\file.txt b/dir\\nested\\file.txt\n".to_string(),
        };

        let normalized = MonoApiService::normalize_diff_item(item);
        assert_eq!(normalized.path, "dir/nested/file.txt");
        assert!(
            normalized
                .data
                .contains("diff --git a/dir/nested/file.txt b/dir/nested/file.txt")
        );
    }

    #[test]
    fn test_normalize_patch_header_paths_preserves_hunk_content() {
        let raw_patch = "\
diff --git a/dir\\nested\\file.txt b/dir\\nested\\file.txt\n\
--- a/dir\\nested\\file.txt\n\
+++ b/dir\\nested\\file.txt\n\
@@ -1 +1 @@\n\
-let path = \"C:\\\\temp\\\\old\";\n\
+let path = \"C:\\\\temp\\\\new\";\n";

        let normalized = MonoApiService::normalize_patch_header_paths(raw_patch);

        assert!(normalized.contains("diff --git a/dir/nested/file.txt b/dir/nested/file.txt"));
        assert!(normalized.contains("--- a/dir/nested/file.txt"));
        assert!(normalized.contains("+++ b/dir/nested/file.txt"));
        assert!(normalized.contains("-let path = \"C:\\\\temp\\\\old\";"));
        assert!(normalized.contains("+let path = \"C:\\\\temp\\\\new\";"));
    }

    #[tokio::test]
    async fn test_content_diff_functionality() {
        use std::collections::HashMap;

        use git_internal::internal::object::blob::Blob;

        // Test basic diff generation with sample data
        let old_content = "Hello World\nLine 2\nLine 3";
        let new_content = "Hello Universe\nLine 2\nLine 3 modified";

        let old_blob = Blob::from_content(old_content);
        let new_blob = Blob::from_content(new_content);

        let old_blobs = vec![(PathBuf::from("test_file.txt"), old_blob.id)];
        let new_blobs = vec![(PathBuf::from("test_file.txt"), new_blob.id)];

        // Create a blob cache for the test
        let mut blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();
        blob_cache.insert(old_blob.id, old_content.as_bytes().to_vec());
        blob_cache.insert(new_blob.id, new_content.as_bytes().to_vec());

        // Test the diff engine directly
        let read_content = |_file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
            blob_cache.get(hash).cloned().unwrap_or_default()
        };

        let diff_output: Vec<DiffItem> =
            GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
                .into_iter()
                .collect();

        // Verify diff output contains expected content
        assert!(!diff_output.is_empty(), "Diff output should not be empty");
        assert_eq!(diff_output.len(), 1, "Should have diff for one file");

        let diff_item = &diff_output[0];
        assert_eq!(diff_item.path, "test_file.txt");
        assert!(
            diff_item.data.contains("diff --git"),
            "Should contain git diff header"
        );
        assert!(
            diff_item.data.contains("-Hello World"),
            "Should show removed line"
        );
        assert!(
            diff_item.data.contains("+Hello Universe"),
            "Should show added line"
        );
        assert!(diff_item.data.contains("-Line 3"), "Should show old line 3");
        assert!(
            diff_item.data.contains("+Line 3 modified"),
            "Should show new line 3"
        );
    }

    #[tokio::test]
    async fn test_get_diff_by_blobs_with_empty_content() {
        // Test diff generation with empty content (simulating missing blobs)
        let old_hash = ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap();
        let new_hash = ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();

        let old_blobs = vec![(PathBuf::from("empty_file.txt"), old_hash)];
        let new_blobs = vec![(PathBuf::from("empty_file.txt"), new_hash)];

        // Create empty blob cache to simulate missing blobs
        let blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();

        let read_content = |_file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
            blob_cache.get(hash).cloned().unwrap_or_default()
        };

        // Test the diff engine with empty content
        let diff_output: Vec<DiffItem> =
            GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
                .into_iter()
                .collect();

        assert!(
            !diff_output.is_empty(),
            "Should generate diff even with empty blobs"
        );
        assert_eq!(diff_output[0].path, "empty_file.txt");
        assert!(
            diff_output[0].data.contains("diff --git"),
            "Should contain git diff header"
        );
    }
}

#[test]
fn test_parse_github_link() {
    let url = "https://github.com/web3infra-foundation/libra/";
    let url = url
        .trim_end_matches(".git")
        .trim_end_matches("/")
        .strip_prefix("https://github.com/")
        .expect("Invalid GitHub URL");
    let (owner, repo) = url.rsplit_once('/').unwrap();
    assert_eq!(owner, "web3infra-foundation");
    assert_eq!(repo, "libra");
}

#[tokio::test]
async fn test_third_party_trait() {
    let url = "https://github.com/aidcheng/mega.git";
    let third_party_client = ThirdPartyClient::new(url);
    let remote_timeout = std::time::Duration::from_secs(30);

    let (_, refs) =
        match tokio::time::timeout(remote_timeout, third_party_client.fetch_refs()).await {
            Ok(Ok(refs)) => refs,
            Ok(Err(err)) => {
                tracing::warn!(
                    "Skipping test_third_party_trait because remote refs are unavailable: {}",
                    err
                );
                return;
            }
            Err(_) => {
                tracing::warn!("Skipping test_third_party_trait because remote refs timed out");
                return;
            }
        };

    let res = match tokio::time::timeout(
        remote_timeout,
        third_party_client.fetch_packs(&[refs], Some(1)),
    )
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(err)) => {
            tracing::warn!(
                "Skipping test_third_party_trait because pack fetch failed: {}",
                err
            );
            return;
        }
        Err(_) => {
            tracing::warn!("Skipping test_third_party_trait because pack fetch timed out");
            return;
        }
    };

    match tokio::time::timeout(remote_timeout, third_party_client.process_pack_stream(res)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                "Skipping test_third_party_trait because pack processing failed: {}",
                err
            );
        }
        Err(_) => {
            tracing::warn!("Skipping test_third_party_trait because pack processing timed out");
        }
    }
}

// --- UN-16: merge funnel notify + dirty fail-closed (real test DB) ---
//
// These tests drive `apply_update_result` against a real Postgres test
// schema (`crate::jupiter::tests::test_storage`, ER-01 test stack). The
// `git_object_cache` is a lazy Redis connection that is never used by
// `apply_update_result`, so no live Redis is required.

async fn save_authz_json(storage: &Storage, json: &str) -> String {
    storage
        .git_service
        .save_object_from_raw(bytes::Bytes::from(json.to_string()))
        .await
        .expect("save authz blob to object storage")
}

fn blob_item(name: &str, hex: &str) -> TreeItem {
    TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_hex_for_kind(get_hash_kind(), hex).unwrap(),
        name.to_string(),
    )
}

async fn setup_main_ref(storage: &Storage, old_tree: &Tree, old_commit_id: &str) {
    storage
        .mono_storage()
        .save_mega_trees(
            vec![old_tree.clone()],
            ObjectHash::from_hex_for_kind(get_hash_kind(), old_commit_id).unwrap(),
            None,
        )
        .await
        .expect("save old tree");
    let main_ref = mega_refs::Model {
        id: 1,
        path: "/".to_string(),
        ref_name: MEGA_BRANCH_NAME.to_string(),
        ref_commit_hash: old_commit_id.to_string(),
        ref_tree_hash: old_tree.id.to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
        is_cl: false,
    };
    storage
        .mono_storage()
        .save_refs(main_ref, None)
        .await
        .expect("save main ref");
}

fn test_service(storage: &Storage) -> MonoApiService {
    MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "test".to_string(),
        }),
    }
}

#[tokio::test]
async fn apply_update_result_rebuilds_snapshot_when_authz_blob_changes() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    let old_json =
        crate::contract::policy::entitystore::generate_entity(&["admin".to_string()], "old-repo")
            .expect("generate old authz");
    let new_json =
        crate::contract::policy::entitystore::generate_entity(&["admin".to_string()], "new-repo")
            .expect("generate new authz");
    let old_blob_id = save_authz_json(&storage, &old_json).await;
    let new_blob_id = save_authz_json(&storage, &new_json).await;

    let old_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        &old_blob_id,
    )])
    .unwrap();
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    let new_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        &new_blob_id,
    )])
    .unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    let new_commit_id = service
        .apply_update_result(&result, "update authz", None)
        .await
        .expect("apply_update_result should succeed");
    assert!(!new_commit_id.is_empty());

    assert!(!storage.entity_store().is_dirty());
    let snap = storage.entity_store().snapshot().expect("snapshot built");
    assert!(
        snap.store()
            .contains_repository(&r#"Repository::"new-repo""#.parse().unwrap()),
        "snapshot should reflect the new authz content"
    );
}

#[tokio::test]
async fn apply_update_result_marks_dirty_when_authz_blob_unreadable() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    // Old main tree has no `/.mega_cedar.json` (old_blob_id = None).
    // `from_tree_items` rejects empty trees, so build the empty tree via the
    // struct literal (id is arbitrary; only the tree_items matter here).
    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    // New tree references a `/.mega_cedar.json` blob that does not exist in
    // object storage: the notify read fails -> snapshot marked dirty
    // (fail-closed). The write itself is already committed, so
    // `apply_update_result` still returns Ok (best-effort notify).
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let new_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        missing,
    )])
    .unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    service
        .apply_update_result(&result, "update authz", None)
        .await
        .expect("apply_update_result succeeds; notify is best-effort");
    assert!(
        storage.entity_store().is_dirty(),
        "unreadable authz blob must mark the snapshot dirty (fail-closed)"
    );
}

#[tokio::test]
async fn apply_update_result_marks_dirty_when_tree_save_fails_after_ref_write() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    // Fault injection: a `BEFORE INSERT` trigger on `mega_tree` that raises
    // an exception, so the tree save (`batch_save_model`, which propagates
    // errors) fails AFTER the main ref write (`batch_update_by_path_concurrent`)
    // succeeds. The ref write targets `mega_refs` (intact); the commit save
    // targets `mega_commit` (intact); the tree save hits the trigger -> a
    // non-RecordNotInserted error that propagates to the dirty-marking
    // branch. (A plain `DROP TABLE` would not work: the search_path includes
    // `public`, so the insert would fall back to the public schema's table.)
    let mono = storage.mono_storage();
    let conn = mono.get_connection();
    sea_orm::ConnectionTrait::execute_raw(
            conn,
            sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "CREATE OR REPLACE FUNCTION fault_inject_block_tree_insert() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'fault injection: mega_tree insert blocked'; END; $$ LANGUAGE plpgsql".to_string(),
            ),
        )
        .await
        .expect("create fault-injection trigger function");
    sea_orm::ConnectionTrait::execute_raw(
            conn,
            sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "CREATE TRIGGER fault_inject_mega_tree_trigger BEFORE INSERT ON mega_tree FOR EACH ROW EXECUTE FUNCTION fault_inject_block_tree_insert()".to_string(),
            ),
        )
        .await
        .expect("create fault-injection trigger on mega_tree");

    let new_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "2222222222222222222222222222222222222222",
        )
        .unwrap(),
        tree_items: vec![],
    };
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    let err = service
        .apply_update_result(&result, "update", None)
        .await
        .expect_err("tree save must fail after the ref write");
    assert!(!err.to_string().is_empty());
    assert!(
        storage.entity_store().is_dirty(),
        "ref written but subsequent step failed -> dirty (fail-closed)"
    );
}

// --- MC-02: minimal GPG merge gate (real test DB) ---
//
// `merge_cl` rejects a CL whose `check_result` rows contain a FAILED
// GpgSignature entry, and only that one. The full `ensure_cl_mergeable`
// gate stays disabled; these tests pin the minimal wiring.

#[cfg(test)]
fn gate_test_cl(link: &str, from_hash: &str, to_hash: &str) -> mega_cl::Model {
    mega_cl::Model {
        id: 42,
        link: link.to_string(),
        title: "gpg gate test".to_string(),
        merge_date: None,
        status: MergeStatusEnum::Open,
        path: "/".to_string(),
        from_hash: from_hash.to_string(),
        to_hash: to_hash.to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
        username: "gate-tester".to_string(),
        base_branch: "main".to_string(),
        revision: 0,
    }
}

#[cfg(test)]
async fn insert_check_result(
    storage: &Storage,
    link: &str,
    check_type: CheckTypeEnum,
    status: &str,
) {
    let model =
        crate::callisto::check_result::Model::new("/", link, "deadbeef", check_type, status, "msg");
    storage
        .cl_storage()
        .save_check_results(vec![model])
        .await
        .expect("save check result");
}

#[cfg(test)]
async fn gate_test_service() -> (tempfile::TempDir, Storage, MonoApiService) {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);
    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await;
    (temp, storage, service)
}

#[tokio::test]
async fn merge_cl_rejects_cl_with_failed_gpg_signature_check() {
    let (_temp, storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGFAIL1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "FAILED").await;

    let err = service
        .merge_cl("gate-tester", "gate-tester", cl.clone())
        .await
        .expect_err("a FAILED GPG signature check must block the merge");
    assert!(
        err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
    assert!(err.to_string().contains(&cl.link), "{err}");
}

#[tokio::test]
async fn merge_cl_passes_gate_without_any_check_rows() {
    let (_temp, _storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGNONE1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    // The gate must not block; the merge then fails later because the tip
    // commit does not exist in this fixture — that error proves the gate let
    // the CL through.
    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("merge fails on the missing tip commit, not on the gate");
    assert!(
        !err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
}

#[tokio::test]
async fn merge_cl_passes_gate_with_passed_gpg_and_other_failed_checks() {
    let (_temp, storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGPASS1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "PASSED").await;
    insert_check_result(&storage, &cl.link, CheckTypeEnum::ClSync, "FAILED").await;

    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("merge fails after the GPG gate, not on the gate");
    assert!(
        !err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
}

// --- MC-09: server-side signing of synthetic commits entering a CL chain ---
//
// Both synthesis points — the `update_branch` chain (via
// `process_ref_updates_cl_only`) and the buck upload chain
// (`complete_buck_upload`) — must emit commits signed with the server key
// from the vault. Every derived reference must carry the final signed hash:
// the `mega_commit.commit_id` row, the `mega_tree.commit_id` stamps, the CL
// ref's `ref_commit_hash` and the CL's `to_hash`; and the ref-pointed commit
// must pass the MC-02 chain verifier (`GpgSignatureChecker`).

#[cfg(test)]
mod mc09_tests {
    use std::time::Duration;

    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

    use super::*;
    use crate::{
        ceres::merge_checker::{
            Checker, ConditionResult, gpg_signature_checker::GpgSignatureChecker,
        },
        contract::vault::{
            integration::vault_core::VaultCore, server_signing::SERVER_SIGNING_EMAIL,
        },
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    async fn test_redis() -> ::redis::aio::ConnectionManager {
        let url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let client = ::redis::Client::open(url).expect("redis client");
        ::redis::aio::ConnectionManager::new(client)
            .await
            .expect("redis connection")
    }

    /// A standalone test vault (own schema, own key file) for signing.
    async fn test_vault(dir: &Path) -> VaultCore {
        let conn = Arc::new(test_db_connection(dir).await);
        apply_migrations(&conn, true).await.expect("migrations");
        VaultCore::config(
            crate::jupiter::storage::vault_storage::VaultStorage {
                base: BaseStorage::new(conn),
            },
            dir.join("mc09_core_key.json"),
        )
        .await
        .expect("vault core should initialize")
    }

    /// A service whose `git_object_cache` connection is the real test Redis —
    /// the RedLock backend for first-time signing-key initialization. The
    /// object cache itself is disabled (empty prefix).
    fn signing_service(
        storage: &Storage,
        redis: ::redis::aio::ConnectionManager,
    ) -> MonoApiService {
        MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: redis,
                prefix: String::new(),
            }),
        }
    }

    /// Run the MC-02 chain verifier over `(from, to]` and demand PASSED.
    async fn assert_chain_verifies(storage: &Storage, from: &str, to: &str) {
        let checker = GpgSignatureChecker {
            storage: Arc::new(storage.clone()),
        };
        let res = checker
            .run(&serde_json::json!({"cl_from": from, "cl_to": to}))
            .await;
        assert_eq!(
            res.status,
            ConditionResult::PASSED,
            "MC-02 chain verification must pass for the signed commit: {}",
            res.message
        );
    }

    fn assert_server_commit(commit: &crate::callisto::mega_commit::Model) {
        let content = commit.content.as_deref().unwrap_or_default();
        assert!(
            content.starts_with("gpgsig -----BEGIN PGP SIGNATURE-----"),
            "synthesized commit must embed the gpgsig header: {content}"
        );
        assert!(
            commit
                .author
                .as_deref()
                .unwrap_or_default()
                .contains(SERVER_SIGNING_EMAIL),
            "author must be the reserved server identity"
        );
        assert!(
            commit
                .committer
                .as_deref()
                .unwrap_or_default()
                .contains(SERVER_SIGNING_EMAIL),
            "committer must be the reserved server identity"
        );
    }

    async fn commit_count(storage: &Storage) -> usize {
        crate::callisto::mega_commit::Entity::find()
            .all(storage.mono_storage().get_connection())
            .await
            .expect("count commits")
            .len()
    }

    async fn tree_count(storage: &Storage) -> usize {
        crate::callisto::mega_tree::Entity::find()
            .all(storage.mono_storage().get_connection())
            .await
            .expect("count trees")
            .len()
    }

    /// `update_branch` fixture: three commits — `from` (old CL base),
    /// `cl_tip` (CL head adding `cl.txt`) and `target` (current main, adding
    /// `other.txt`; disjoint from the CL change so no conflict) — plus the
    /// main ref, the CL ref and the open CL.
    async fn chain_fixture(storage: &Storage, link: &str) -> (String, String, String) {
        let blob_base = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let blob_other = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        let blob_cl = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "3333333333333333333333333333333333333333",
        )
        .unwrap();

        let from_tree = Tree::from_tree_items(vec![blob_item("base.txt", &blob_base.to_string())])
            .expect("from tree");
        let target_tree = Tree::from_tree_items(vec![
            blob_item("base.txt", &blob_base.to_string()),
            blob_item("other.txt", &blob_other.to_string()),
        ])
        .expect("target tree");
        let cl_tree = Tree::from_tree_items(vec![
            blob_item("base.txt", &blob_base.to_string()),
            blob_item("cl.txt", &blob_cl.to_string()),
        ])
        .expect("cl tree");

        let from_commit = Commit::from_tree_id(from_tree.id, vec![], "mc09 base");
        let target_commit = Commit::from_tree_id(target_tree.id, vec![], "mc09 main advanced");
        let cl_commit = Commit::from_tree_id(cl_tree.id, vec![from_commit.id], "mc09 cl change");

        let mono = storage.mono_storage();
        mono.save_mega_commits(
            vec![
                from_commit.clone(),
                target_commit.clone(),
                cl_commit.clone(),
            ],
            None,
        )
        .await
        .expect("fixture commits");
        mono.save_mega_trees(vec![from_tree.clone()], from_commit.id, None)
            .await
            .expect("from tree");
        mono.save_mega_trees(vec![cl_tree.clone()], cl_commit.id, None)
            .await
            .expect("cl tree");
        // Saves the target tree and the main ref.
        setup_main_ref(storage, &target_tree, &target_commit.id.to_string()).await;

        let now = chrono::Utc::now().naive_utc();
        let cl_ref = mega_refs::Model {
            id: 2,
            path: "/".to_string(),
            ref_name: format!("refs/cl/{link}"),
            ref_commit_hash: cl_commit.id.to_string(),
            ref_tree_hash: cl_tree.id.to_string(),
            created_at: now,
            updated_at: now,
            is_cl: true,
        };
        mono.save_refs(cl_ref, None).await.expect("cl ref");

        storage
            .cl_storage()
            .new_cl(
                "/",
                link,
                "mc09 update-branch",
                "main",
                &from_commit.id.to_string(),
                &cl_commit.id.to_string(),
                "alice",
            )
            .await
            .expect("cl");

        (
            from_commit.id.to_string(),
            cl_commit.id.to_string(),
            target_commit.id.to_string(),
        )
    }

    // AC⑥ (update_branch chain): the synthesized rebase commit is
    // server-signed and all four derived references carry the final signed
    // hash; the ref-pointed commit passes the MC-02 chain verifier.
    #[tokio::test]
    async fn update_branch_signs_commit_and_backfills_references() {
        let temp = tempfile::tempdir().unwrap();
        let vault = test_vault(temp.path()).await;
        let redis = test_redis().await;
        let storage = crate::jupiter::tests::test_storage(temp.path())
            .await
            .with_vault(vault.clone());
        let service = signing_service(&storage, redis.clone());

        let link = "MC09UPD1";
        let (_from_hash, cl_tip, target_head) = chain_fixture(&storage, link).await;

        let new_head = service
            .update_branch("alice", link)
            .await
            .expect("update_branch with signing");
        assert_ne!(new_head, cl_tip, "a new rebased commit must be created");

        let mono = storage.mono_storage();
        let commit = mono
            .get_commit_by_hash(&new_head)
            .await
            .expect("load commit")
            .expect("signed commit row");
        let cl_ref = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref")
            .expect("cl ref row");
        let cl = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("load cl")
            .expect("cl row");
        let tree = mono
            .get_tree_by_hash(&cl_ref.ref_tree_hash)
            .await
            .expect("load tree")
            .expect("new root tree row");

        // Four sinks, one final signed hash.
        assert_eq!(commit.commit_id, new_head);
        assert_eq!(tree.commit_id, new_head, "mega_tree.commit_id");
        assert_eq!(cl_ref.ref_commit_hash, new_head, "CL ref");
        assert_eq!(cl.to_hash, new_head, "mega_cl.to_hash");
        assert_eq!(cl.from_hash, target_head, "rebase advances the CL base");
        assert_server_commit(&commit);

        // MC-10 test hygiene: no `gpg_key` registration — the chain must
        // verify through the server keyring routing alone (MC-11).
        assert_chain_verifies(&storage, &target_head, &new_head).await;
    }

    // MC-04 R2: update_branch's CL hash move and the commit-listing rebuild
    // share one transaction — after a rebase the listing is readable (the
    // read-side coverage check must not fail closed on the moved range) and
    // equals the new `(target_head, new_head]` chain: exactly the synthesized
    // commit.
    #[tokio::test]
    async fn update_branch_rebuilds_cl_commits_listing() {
        let temp = tempfile::tempdir().unwrap();
        let vault = test_vault(temp.path()).await;
        let redis = test_redis().await;
        let storage = crate::jupiter::tests::test_storage(temp.path())
            .await
            .with_vault(vault.clone());
        let service = signing_service(&storage, redis.clone());

        let link = "MC04UB1";
        let (_from_hash, cl_tip, _target_head) = chain_fixture(&storage, link).await;

        // Pre-stage a stale listing (the pre-rebase tip) so the rebuild's
        // delete+insert is exercised.
        let cl_stg = storage.cl_storage();
        let stale = [Commit::from_mega_model(
            storage
                .mono_storage()
                .get_commit_by_hash(&cl_tip)
                .await
                .expect("load cl tip")
                .expect("cl tip row"),
        )];
        let txn = storage.begin_db_transaction().await.expect("begin txn");
        cl_stg
            .save_cl_commits_in_txn(link, &stale, &txn)
            .await
            .expect("stage stale listing");
        txn.commit().await.expect("commit staging txn");

        let new_head = service
            .update_branch("alice", link)
            .await
            .expect("update_branch with signing");

        let listing = cl_stg
            .get_cl_commits(link)
            .await
            .expect("listing must be readable after update_branch");
        assert_eq!(
            listing.len(),
            1,
            "the rebase emits exactly one synthesized commit"
        );
        assert_eq!(
            listing[0].commit_sha, new_head,
            "the listing is the new (target_head, new_head] chain"
        );
        assert_eq!(listing[0].message, "update-branch: rebase");
        assert!(
            listing[0].author_email.contains(SERVER_SIGNING_EMAIL),
            "the listing member is the server-signed synthesized commit"
        );
    }

    // MC-04 R2 (no-change branch): a no-op rebase synthesizes no commit, so
    // the new range has no valid chain — the listing must be cleared with the
    // base move (atomically), and reads return an empty list rather than
    // failing the coverage check.
    #[tokio::test]
    async fn update_branch_no_change_clears_cl_commits_listing() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        // No vault: the no-change branch synthesizes nothing, so no signing
        // capability is needed.
        let service = test_service(&storage);

        let link = "MC04UB2";
        // Fixture like chain_fixture, but the CL commit shares the base tree —
        // the CL diff (from → to) is empty.
        let blob_base = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let blob_other = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        let from_tree = Tree::from_tree_items(vec![blob_item("base.txt", &blob_base.to_string())])
            .expect("from tree");
        let target_tree = Tree::from_tree_items(vec![
            blob_item("base.txt", &blob_base.to_string()),
            blob_item("other.txt", &blob_other.to_string()),
        ])
        .expect("target tree");
        let from_commit = Commit::from_tree_id(from_tree.id, vec![], "mc04 base");
        let cl_commit = Commit::from_tree_id(from_tree.id, vec![from_commit.id], "mc04 cl no-op");
        let target_commit = Commit::from_tree_id(target_tree.id, vec![], "mc04 main advanced");

        let mono = storage.mono_storage();
        mono.save_mega_commits(
            vec![
                from_commit.clone(),
                cl_commit.clone(),
                target_commit.clone(),
            ],
            None,
        )
        .await
        .expect("fixture commits");
        mono.save_mega_trees(vec![from_tree.clone()], from_commit.id, None)
            .await
            .expect("from/cl tree");
        setup_main_ref(&storage, &target_tree, &target_commit.id.to_string()).await;
        let now = chrono::Utc::now().naive_utc();
        mono.save_refs(
            mega_refs::Model {
                id: 2,
                path: "/".to_string(),
                ref_name: format!("refs/cl/{link}"),
                ref_commit_hash: cl_commit.id.to_string(),
                ref_tree_hash: from_tree.id.to_string(),
                created_at: now,
                updated_at: now,
                is_cl: true,
            },
            None,
        )
        .await
        .expect("cl ref");
        storage
            .cl_storage()
            .new_cl(
                "/",
                link,
                "mc04 no-change update-branch",
                "main",
                &from_commit.id.to_string(),
                &cl_commit.id.to_string(),
                "alice",
            )
            .await
            .expect("cl");
        // Pre-stage the pre-rebase listing (the CL tip).
        let cl_stg = storage.cl_storage();
        let txn = storage.begin_db_transaction().await.expect("begin txn");
        cl_stg
            .save_cl_commits_in_txn(link, std::slice::from_ref(&cl_commit), &txn)
            .await
            .expect("stage listing");
        txn.commit().await.expect("commit staging txn");

        let returned = service
            .update_branch("alice", link)
            .await
            .expect("no-change update_branch");

        // MC-04 R3: the empty-range state is self-consistent —
        // from == to == target_head, the CL ref follows, the listing is
        // cleared, and the old tip stays in the object store for audit.
        assert_eq!(
            returned,
            target_commit.id.to_string(),
            "the empty range's head is the target head"
        );
        let cl = cl_stg.get_cl(link).await.expect("get_cl").expect("cl row");
        assert_eq!(cl.from_hash, target_commit.id.to_string(), "base advanced");
        assert_eq!(
            cl.to_hash,
            target_commit.id.to_string(),
            "head follows the base: the empty range is self-consistent"
        );
        let cl_ref = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref")
            .expect("cl ref row");
        assert_eq!(
            cl_ref.ref_commit_hash,
            target_commit.id.to_string(),
            "the active CL ref follows to the target head"
        );
        assert_eq!(
            cl_ref.ref_tree_hash,
            target_tree.id.to_string(),
            "the CL ref tree follows to the target tree"
        );
        let listing = cl_stg
            .get_cl_commits(link)
            .await
            .expect("listing must be readable (empty) after a no-change rebase");
        assert!(
            listing.is_empty(),
            "a no-change rebase clears the listing (the new range has no chain)"
        );
        assert!(
            mono.get_commit_by_hash(&cl_commit.id.to_string())
                .await
                .expect("lookup old tip")
                .is_some(),
            "the old tip commit stays in the object store for audit"
        );
    }

    // MC-04 R3 fault injection: if the commit-listing write fails inside
    // update_branch's single transaction, every sink stays untouched — the CL
    // ref, commit rows, tree rows and the CL row all keep the old range. The
    // transaction body below mirrors `update_branch`'s statement sequence.
    #[tokio::test]
    async fn update_branch_listing_failure_leaves_all_sinks_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let vault = test_vault(temp.path()).await;
        let redis = test_redis().await;
        let storage = crate::jupiter::tests::test_storage(temp.path())
            .await
            .with_vault(vault.clone());
        let service = signing_service(&storage, redis.clone());

        let link = "MC04UB3";
        let (from_hash, cl_tip, target_head) = chain_fixture(&storage, link).await;

        let cl = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("get_cl")
            .expect("cl row");
        let old_blobs = service
            .get_commit_blobs(&cl.from_hash)
            .await
            .expect("old blobs");
        let new_blobs = service
            .get_commit_blobs(&cl.to_hash)
            .await
            .expect("new blobs");
        let changes = service
            .cl_files_list(old_blobs, new_blobs)
            .await
            .expect("cl changes");
        assert!(!changes.is_empty(), "the fixture CL has a real change");

        let prepared = service
            .build_signed_cl_update(&cl, &changes, &target_head)
            .await
            .expect("construct + sign the update");

        // Mirror update_branch's single-transaction persistence body, with a
        // poisoned listing batch (duplicate (link, sha) primary key) — the
        // write must fail and roll everything back.
        let mono = storage.mono_storage();
        let mut cl_ref = prepared.cl_ref.clone();
        for update in &prepared.updates {
            if update.ref_name == cl_ref.ref_name {
                cl_ref.ref_commit_hash = update.commit_id.clone();
                cl_ref.ref_tree_hash = update.tree_hash.clone();
            }
        }
        let poisoned: Vec<Commit> = vec![prepared.commits[0].clone(), prepared.commits[0].clone()];
        let txn = storage.begin_db_transaction().await.expect("begin txn");
        mono.update_ref(cl_ref, Some(&txn))
            .await
            .expect("ref update in txn");
        mono.save_mega_commits(prepared.commits.clone(), Some(&txn))
            .await
            .expect("commits in txn");
        mono.batch_save_model_with_txn(prepared.tree_models.clone(), Some(&txn))
            .await
            .expect("trees in txn");
        storage
            .cl_storage()
            .update_cl_hash_in_txn(cl.clone(), &target_head, &prepared.new_commit_id, &txn)
            .await
            .expect("cl hashes in txn");
        storage
            .cl_storage()
            .save_cl_commits_in_txn(link, &poisoned, &txn)
            .await
            .expect_err("the poisoned listing write must fail");
        drop(txn); // rolls back

        // All four sinks unchanged.
        let cl_ref_after = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref")
            .expect("cl ref row");
        assert_eq!(cl_ref_after.ref_commit_hash, cl_tip, "CL ref unchanged");
        assert_eq!(commit_count(&storage).await, 3, "no new commit persisted");
        assert_eq!(tree_count(&storage).await, 3, "no new tree persisted");
        let cl_after = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("get_cl")
            .expect("cl row");
        assert_eq!(cl_after.from_hash, from_hash, "CL from_hash unchanged");
        assert_eq!(cl_after.to_hash, cl_tip, "CL to_hash unchanged");
        assert!(
            storage
                .cl_storage()
                .get_cl_commits(link)
                .await
                .expect("listing read")
                .is_empty(),
            "no listing rows persisted"
        );
    }

    // AC③/AC④ (update_branch chain): without the vault handle the synthesis
    // fails closed and nothing is persisted — ref, commits, trees and CL
    // hashes all unchanged.
    #[tokio::test]
    async fn update_branch_without_vault_fails_closed_with_zero_side_effects() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        // `test_service`'s lazy Redis connection is never reached: the
        // fail-closed vault check happens first.
        let service = test_service(&storage);

        let link = "MC09UPD2";
        let (from_hash, cl_tip, _target_head) = chain_fixture(&storage, link).await;

        let err = service
            .update_branch("alice", link)
            .await
            .expect_err("update_branch must fail closed without a vault");
        assert!(
            err.to_string().contains("server signing unavailable"),
            "{err}"
        );

        let mono = storage.mono_storage();
        let cl = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("load cl")
            .expect("cl row");
        assert_eq!(cl.from_hash, from_hash, "CL from_hash unchanged");
        assert_eq!(cl.to_hash, cl_tip, "CL to_hash unchanged");
        let cl_ref = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref")
            .expect("cl ref row");
        assert_eq!(cl_ref.ref_commit_hash, cl_tip, "CL ref unchanged");
        assert_eq!(commit_count(&storage).await, 3, "no new commit persisted");
        assert_eq!(tree_count(&storage).await, 3, "no new tree persisted");
    }

    /// Buck-upload fixture: base commit + root tree, the draft CL consumed by
    /// `get_and_update_cl_in_txn`, and a session with one uploaded file.
    /// Returns the base commit hash (`from_hash`).
    async fn buck_fixture(storage: &Storage, link: &str, username: &str) -> String {
        let blob_old = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "4444444444444444444444444444444444444444",
        )
        .unwrap();
        let base_tree = Tree::from_tree_items(vec![blob_item("old.txt", &blob_old.to_string())])
            .expect("base tree");
        let base_commit = Commit::from_tree_id(base_tree.id, vec![], "mc09 buck base");
        let base_hash = base_commit.id.to_string();

        let mono = storage.mono_storage();
        mono.save_mega_commits(vec![base_commit.clone()], None)
            .await
            .expect("base commit");
        mono.save_mega_trees(vec![base_tree.clone()], base_commit.id, None)
            .await
            .expect("base tree");

        storage
            .cl_storage()
            .new_cl(
                "/",
                link,
                "mc09 buck",
                "main",
                &base_hash,
                &base_hash,
                username,
            )
            .await
            .expect("cl");

        let now = chrono::Utc::now().naive_utc();
        crate::callisto::buck_session::ActiveModel {
            id: Set(1),
            session_id: Set(link.to_string()),
            user_id: Set(username.to_string()),
            repo_path: Set("/".to_string()),
            status: Set(session_status::MANIFEST_UPLOADED.to_string()),
            commit_message: Set(Some("buck upload".to_string())),
            from_hash: Set(Some(base_hash.clone())),
            expires_at: Set(now + chrono::Duration::hours(1)),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(storage.buck_storage().get_connection())
        .await
        .expect("buck session");

        let new_blob = "5555555555555555555555555555555555555555";
        crate::callisto::buck_session_file::ActiveModel {
            id: Set(1),
            session_id: Set(link.to_string()),
            file_path: Set("new.txt".to_string()),
            file_size: Set(3),
            file_hash: Set(format!("sha1:{new_blob}")),
            file_mode: Set(Some("100644".to_string())),
            upload_status: Set(upload_status::UPLOADED.to_string()),
            upload_reason: Set(None),
            blob_id: Set(Some(format!("sha1:{new_blob}"))),
            uploaded_at: Set(Some(now)),
            created_at: Set(now),
        }
        .insert(storage.buck_storage().get_connection())
        .await
        .expect("buck file");

        base_hash
    }

    /// `test_storage` mocks `BuckService` with a disconnected connection;
    /// rewire it onto the test database so `complete_upload` persists.
    fn wire_buck_service(storage: &mut Storage) {
        let conn = storage.mono_storage().get_connection().clone();
        let base = BaseStorage::new(Arc::new(conn));
        storage.buck_service = crate::jupiter::service::buck_service::BuckService::new(
            base,
            crate::jupiter::service::cl_service::CLService::mock(),
            Arc::new(tokio::sync::Semaphore::new(10)),
            Arc::new(tokio::sync::Semaphore::new(5)),
            crate::config::BuckConfig::default(),
            crate::jupiter::service::git_service::GitService::mock(),
        )
        .expect("buck service");
    }

    // AC⑥ (buck chain): the upload-synthesized commit is server-signed and
    // all four derived references — including every tree model's commit_id,
    // backfilled one by one — carry the final signed hash; the ref-pointed
    // commit passes the MC-02 chain verifier.
    #[tokio::test]
    async fn complete_buck_upload_signs_commit_and_backfills_references() {
        let temp = tempfile::tempdir().unwrap();
        let vault = test_vault(temp.path()).await;
        let redis = test_redis().await;
        let mut storage = crate::jupiter::tests::test_storage(temp.path())
            .await
            .with_vault(vault.clone());
        wire_buck_service(&mut storage);
        let service = signing_service(&storage, redis.clone());

        let link = "MC09BUP1";
        let base_hash = buck_fixture(&storage, link, "alice").await;

        let response = service
            .complete_buck_upload("alice", link, CompletePayload {})
            .await
            .expect("complete_buck_upload with signing");
        let signed = response.commit_id;
        assert_ne!(signed, base_hash, "a new upload commit must be created");

        let mono = storage.mono_storage();
        let commit = mono
            .get_commit_by_hash(&signed)
            .await
            .expect("load commit")
            .expect("signed commit row");
        let cl_ref = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref")
            .expect("cl ref created by complete_upload");
        let cl = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("load cl")
            .expect("cl row");

        // Four sinks, one final signed hash.
        assert_eq!(commit.commit_id, signed);
        assert_eq!(cl_ref.ref_commit_hash, signed, "CL ref");
        assert_eq!(cl.to_hash, signed, "mega_cl.to_hash");
        assert_eq!(cl.from_hash, base_hash);
        assert_server_commit(&commit);

        // Every new tree model was backfilled with the signed commit_id (the
        // root tree is reachable through the ref; assert the whole set).
        let stamped = crate::callisto::mega_tree::Entity::find()
            .filter(crate::callisto::mega_tree::Column::CommitId.eq(&signed))
            .all(mono.get_connection())
            .await
            .expect("stamped trees");
        assert!(
            !stamped.is_empty(),
            "new tree models must carry the signed commit_id"
        );
        assert!(
            stamped.iter().any(|t| t.tree_id == cl_ref.ref_tree_hash),
            "the ref-pointed root tree must carry the signed commit_id"
        );
        assert_eq!(
            tree_count(&storage).await,
            1 + stamped.len(),
            "only the base tree plus the stamped new trees exist"
        );

        // MC-10 test hygiene: no `gpg_key` registration — the chain must
        // verify through the server keyring routing alone (MC-11).
        assert_chain_verifies(&storage, &base_hash, &signed).await;
    }

    // AC③/AC④ (buck chain): without the vault handle the precheck fails
    // closed before `build_commit`; session, CL, refs, commits and trees are
    // all untouched.
    #[tokio::test]
    async fn complete_buck_upload_without_vault_fails_closed_with_zero_side_effects() {
        let temp = tempfile::tempdir().unwrap();
        let mut storage = crate::jupiter::tests::test_storage(temp.path()).await;
        wire_buck_service(&mut storage);
        let service = test_service(&storage);

        let link = "MC09BUP2";
        let base_hash = buck_fixture(&storage, link, "alice").await;

        let err = service
            .complete_buck_upload("alice", link, CompletePayload {})
            .await
            .expect_err("complete_buck_upload must fail closed without a vault");
        assert!(
            err.to_string().contains("server signing unavailable"),
            "{err}"
        );

        let mono = storage.mono_storage();
        let cl = storage
            .cl_storage()
            .get_cl(link)
            .await
            .expect("load cl")
            .expect("cl row");
        assert_eq!(cl.from_hash, base_hash, "CL from_hash unchanged");
        assert_eq!(cl.to_hash, base_hash, "CL to_hash unchanged");
        let cl_ref = mono
            .get_ref_by_name(&format!("refs/cl/{link}"))
            .await
            .expect("load cl ref");
        assert!(cl_ref.is_none(), "no CL ref may be created");
        assert_eq!(commit_count(&storage).await, 1, "no new commit persisted");
        assert_eq!(tree_count(&storage).await, 1, "no new tree persisted");
        let session = storage
            .buck_storage()
            .get_session(link)
            .await
            .expect("load session")
            .expect("session row");
        assert_eq!(
            session.status,
            session_status::MANIFEST_UPLOADED,
            "session status unchanged"
        );
    }

    /// TP-04: root CAS tripwire is the sole root write at the storage boundary
    /// that future MonoWriteQueue writers (merge/attach/push) will call through.
    #[tokio::test]
    async fn root_cas_update_is_exactly_one_write_per_success() {
        use sea_orm::TransactionTrait;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let root = mega_refs::Model::new(
            "/",
            MEGA_BRANCH_NAME.to_owned(),
            "a".repeat(40),
            "b".repeat(40),
            false,
        );
        mono.save_refs(root.clone(), None).await.unwrap();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        let first = mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                Some(&root.ref_commit_hash),
                Some(&root.ref_tree_hash),
                &"c".repeat(40),
                &"d".repeat(40),
            )
            .await
            .unwrap();
        assert!(first, "successful CAS must affect exactly one root row");
        let second = mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                Some(&root.ref_commit_hash),
                Some(&root.ref_tree_hash),
                &"e".repeat(40),
                &"f".repeat(40),
            )
            .await
            .unwrap();
        assert!(
            !second,
            "stale expected_* must miss — no second root write in the same success round"
        );
        txn.commit().await.unwrap();

        let after = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(after.ref_commit_hash, "c".repeat(40));
        assert_eq!(after.ref_tree_hash, "d".repeat(40));
    }

    async fn queue_merge_fixture(
        path: &str,
        link: &str,
        extra_child: Option<(String, Tree)>,
    ) -> (
        tempfile::TempDir,
        Storage,
        MonoApiService,
        mega_cl::Model,
        String,
    ) {
        use git_internal::internal::object::commit::Commit;

        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
        let service = test_service(&storage);
        let mono = storage.mono_storage();

        let mut root_items = vec![blob_item(
            ".gitkeep",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )];
        let path_tree_hash = extra_child.as_ref().map(|(_, t)| t.id.to_string());
        if let Some((name, child)) = extra_child {
            mono.save_mega_trees(
                vec![child.clone()],
                ObjectHash::from_hex_for_kind(get_hash_kind(), &"1".repeat(40)).unwrap(),
                None,
            )
            .await
            .unwrap();
            root_items.push(TreeItem::new(TreeItemMode::Tree, child.id, name));
        }
        let old_tree = Tree::from_tree_items(root_items).expect("old tree");
        let new_tree = Tree::from_tree_items(vec![blob_item(
            "queued.txt",
            "cccccccccccccccccccccccccccccccccccccccc",
        )])
        .expect("new tree");

        let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
        let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
        mono.save_mega_trees(
            vec![old_tree.clone(), new_tree.clone()],
            old_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
        if path != "/" {
            mono.save_refs(
                mega_refs::Model {
                    id: crate::callisto::entity_ext::generate_id(),
                    path: path.to_string(),
                    ref_name: MEGA_BRANCH_NAME.to_string(),
                    ref_commit_hash: old_commit.id.to_string(),
                    ref_tree_hash: path_tree_hash
                        .clone()
                        .unwrap_or_else(|| old_tree.id.to_string()),
                    created_at: chrono::Utc::now().naive_utc(),
                    updated_at: chrono::Utc::now().naive_utc(),
                    is_cl: false,
                },
                None,
            )
            .await
            .unwrap();
        }
        mono.save_or_update_cl_ref(
            path,
            &format!("refs/cl/{link}"),
            &new_commit.id.to_string(),
            &new_tree.id.to_string(),
        )
        .await
        .unwrap();
        let cl = storage
            .cl_storage()
            .new_cl_model(
                path,
                link,
                "queue merge",
                "main",
                &old_commit.id.to_string(),
                &new_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
        (temp, storage, service, cl, new_commit.id.to_string())
    }

    #[tokio::test]
    async fn queue_merge_new_id_differs_from_landed_commit() {
        let (_temp, storage, service, cl, to_hash) = queue_merge_fixture("/", "QMRG01", None).await;
        service
            .merge_cl("gate-tester", "gate-tester", cl.clone())
            .await
            .expect("queue merge should succeed");
        let merged = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.status, MergeStatusEnum::Merged);
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(root.ref_commit_hash, to_hash, "landed tip is synthesized");
        let landed = storage
            .mono_storage()
            .get_commit_by_hash(&root.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let parents: Vec<String> = serde_json::from_value(landed.parents_id).unwrap_or_default();
        assert_eq!(
            parents,
            vec![to_hash.clone()],
            "parent is execute-time to_hash"
        );
        let row = crate::callisto::push_queue::Entity::find()
            .filter(crate::callisto::push_queue::Column::OperationId.eq(&cl.link))
            .one(storage.push_queue_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.new_id, to_hash);
        assert_eq!(
            row.landed_commit_id.as_deref(),
            Some(root.ref_commit_hash.as_str())
        );
        assert_ne!(row.new_id, row.landed_commit_id.clone().unwrap());
    }

    #[tokio::test]
    async fn queue_merge_missing_path_main_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
        let service = test_service(&storage);
        let old_tree = Tree {
            id: ObjectHash::from_hex_for_kind(
                get_hash_kind(),
                "1111111111111111111111111111111111111111",
            )
            .unwrap(),
            tree_items: vec![],
        };
        setup_main_ref(
            &storage,
            &old_tree,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await;
        let cl = gate_test_cl(
            "QMISS1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let mut cl = cl;
        cl.path = "/missing".into();
        let err = service
            .merge_cl("gate-tester", "gate-tester", cl)
            .await
            .expect_err("missing main@P must refuse at entry");
        assert!(err.to_string().to_lowercase().contains("main ref"), "{err}");
    }

    #[tokio::test]
    async fn queue_merge_nested_descendant_continues_then_second_merge_lands() {
        let _lock = crate::ceres::pack::materialize::lock_materialize_tests().await;
        let blob_old = blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd");
        let b_old = Tree::from_tree_items(vec![blob_old]).expect("b old");
        let a_old = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Tree,
            b_old.id,
            "b".into(),
        )])
        .expect("a old");
        let blob_new = blob_item("x.txt", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        let b_mid = Tree::from_tree_items(vec![blob_new]).expect("b mid");
        let a_new = Tree::from_tree_items(vec![
            TreeItem::new(TreeItemMode::Tree, b_mid.id, "b".into()),
            blob_item("queued.txt", "cccccccccccccccccccccccccccccccccccccccc"),
        ])
        .expect("a new");

        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
        let service = test_service(&storage);
        let mono = storage.mono_storage();

        let root = Tree::from_tree_items(vec![
            blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, a_old.id, "a".into()),
        ])
        .expect("root");
        let old_commit = Commit::from_tree_id(root.id, vec![], "base");
        let cl_a_commit = Commit::from_tree_id(a_new.id, vec![old_commit.id], "cl /a");
        let b_commit = Commit::from_tree_id(b_old.id, vec![], "main@/a/b");
        let a_commit = Commit::from_tree_id(a_old.id, vec![], "main@/a");
        mono.save_mega_trees(
            vec![
                b_old.clone(),
                a_old.clone(),
                b_mid.clone(),
                a_new.clone(),
                root.clone(),
            ],
            old_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(
            vec![
                old_commit.clone(),
                cl_a_commit.clone(),
                b_commit.clone(),
                a_commit.clone(),
            ],
            None,
        )
        .await
        .unwrap();
        setup_main_ref(&storage, &root, &old_commit.id.to_string()).await;
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: "/a".into(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: a_commit.id.to_string(),
                ref_tree_hash: a_old.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: "/a/b".into(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: b_commit.id.to_string(),
                ref_tree_hash: b_old.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
        mono.save_or_update_cl_ref(
            "/a",
            "refs/cl/QNEST1",
            &cl_a_commit.id.to_string(),
            &a_new.id.to_string(),
        )
        .await
        .unwrap();
        let cl_a = storage
            .cl_storage()
            .new_cl_model(
                "/a",
                "QNEST1",
                "queue merge /a",
                "main",
                &a_commit.id.to_string(),
                &cl_a_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();

        service
            .merge_cl("gate-tester", "gate-tester", cl_a)
            .await
            .expect("/a merge");

        let after_a = storage
            .mono_storage()
            .get_main_ref("/a/b")
            .await
            .unwrap()
            .expect("descendant must continue, not be deleted");
        assert_eq!(after_a.ref_tree_hash, b_mid.id.to_string());
        let cont = storage
            .mono_storage()
            .get_commit_by_hash(&after_a.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let parents: Vec<String> = serde_json::from_value(cont.parents_id).unwrap();
        assert_eq!(parents, vec![b_commit.id.to_string()]);

        let blob_tip = blob_item("x.txt", "ffffffffffffffffffffffffffffffffffffffff");
        let b_tip = Tree::from_tree_items(vec![blob_tip]).expect("b tip");
        let parent =
            ObjectHash::from_hex_for_kind(get_hash_kind(), &after_a.ref_commit_hash).unwrap();
        let cl_b_commit = Commit::from_tree_id(b_tip.id, vec![parent], "cl /a/b");
        mono.save_mega_trees(vec![b_tip.clone()], cl_b_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![cl_b_commit.clone()], None)
            .await
            .unwrap();
        let cl_b = storage
            .cl_storage()
            .new_cl_model(
                "/a/b",
                "QNEST2",
                "queue merge /a/b",
                "main",
                &after_a.ref_commit_hash,
                &cl_b_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
        service
            .merge_cl("gate-tester", "gate-tester", cl_b)
            .await
            .expect("/a/b merge after parent continuation");
        let landed = storage
            .mono_storage()
            .get_main_ref("/a/b")
            .await
            .unwrap()
            .unwrap();
        let landed_c = storage
            .mono_storage()
            .get_commit_by_hash(&landed.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let landed_parents: Vec<String> = serde_json::from_value(landed_c.parents_id).unwrap();
        assert_eq!(
            landed_parents,
            vec![after_a.ref_commit_hash],
            "second merge parent must equal post-/a continuation tip"
        );
    }

    #[tokio::test]
    async fn queue_merge_add_to_merge_queue_executes_synchronously() {
        let (_temp, storage, service, cl, to_hash) = queue_merge_fixture("/", "QADD01", None).await;
        let id = service
            .add_to_merge_queue_as(cl.link.clone(), Some("gate-tester".into()))
            .await
            .expect("queue-mode add is sync merge");
        assert!(id > 0);
        let merged = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.status, MergeStatusEnum::Merged);
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(root.ref_commit_hash, to_hash);
    }

    #[tokio::test]
    async fn queue_merge_twenty_non_nested_paths_serialize() {
        use git_internal::internal::object::commit::Commit;

        const N: usize = 20;
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
        let service = test_service(&storage);
        let mono = storage.mono_storage();

        let mut child_trees = Vec::new();
        for i in 0..N {
            let blob = blob_item("base.txt", &format!("{:040x}", i + 1));
            let tree = Tree::from_tree_items(vec![blob]).expect("child tree");
            child_trees.push(tree);
        }
        let root_items: Vec<TreeItem> = child_trees
            .iter()
            .enumerate()
            .map(|(i, t)| TreeItem::new(TreeItemMode::Tree, t.id, format!("p{i:02}")))
            .collect();
        let old_root = Tree::from_tree_items(root_items).expect("root");
        let old_commit = Commit::from_tree_id(old_root.id, vec![], "base");
        for t in &child_trees {
            mono.save_mega_trees(vec![t.clone()], old_commit.id, None)
                .await
                .unwrap();
        }
        mono.save_mega_trees(vec![old_root.clone()], old_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![old_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &old_root, &old_commit.id.to_string()).await;

        let mut cls = Vec::new();
        for (i, child) in child_trees.iter().enumerate() {
            let path = format!("/p{i:02}");
            let link = format!("Q20{i:02}");
            let tip_tree =
                Tree::from_tree_items(vec![blob_item("new.txt", &format!("{:040x}", 1000 + i))])
                    .expect("tip tree");
            let tip = Commit::from_tree_id(tip_tree.id, vec![old_commit.id], "cl tip");
            mono.save_mega_trees(vec![tip_tree.clone()], tip.id, None)
                .await
                .unwrap();
            mono.save_mega_commits(vec![tip.clone()], None)
                .await
                .unwrap();
            mono.save_refs(
                mega_refs::Model::new(
                    path.clone(),
                    MEGA_BRANCH_NAME.to_owned(),
                    old_commit.id.to_string(),
                    child.id.to_string(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
            mono.save_or_update_cl_ref(
                &path,
                &format!("refs/cl/{link}"),
                &tip.id.to_string(),
                &tip_tree.id.to_string(),
            )
            .await
            .unwrap();
            let cl = storage
                .cl_storage()
                .new_cl_model(
                    &path,
                    &link,
                    "q20",
                    "main",
                    &old_commit.id.to_string(),
                    &tip.id.to_string(),
                    "gate-tester",
                )
                .await
                .unwrap();
            cls.push(cl);
        }

        let mut joins = Vec::new();
        for cl in cls.clone() {
            let svc = service.clone();
            joins.push(tokio::spawn(async move {
                svc.merge_cl("gate-tester", "gate-tester", cl).await
            }));
        }
        for j in joins {
            j.await.unwrap().expect("concurrent merge");
        }

        for cl in &cls {
            let row = storage
                .cl_storage()
                .get_cl(&cl.link)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, MergeStatusEnum::Merged, "{}", cl.link);
        }
        let final_root = mono.get_main_ref("/").await.unwrap().unwrap();
        let final_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&final_root.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(final_tree.tree_items.len(), N, "all path trees remain");

        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
        let qrows = crate::callisto::push_queue::Entity::find()
            .filter(
                crate::callisto::push_queue::Column::Kind
                    .eq(crate::callisto::sea_orm_active_enums::PushQueueKindEnum::Merge),
            )
            .filter(
                crate::callisto::push_queue::Column::Status
                    .eq(crate::callisto::sea_orm_active_enums::PushQueueStatusEnum::Done),
            )
            .order_by_asc(crate::callisto::push_queue::Column::Id)
            .all(storage.push_queue_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(qrows.len(), N);
        assert_eq!(
            qrows.last().unwrap().landed_commit_id.as_deref(),
            Some(final_root.ref_commit_hash.as_str()),
            "highest queue id is the final root tip"
        );
        let mut seen = std::collections::HashSet::new();
        for row in &qrows {
            assert!(seen.insert(row.landed_commit_id.clone().unwrap()));
        }
        let mut hash = final_root.ref_commit_hash.clone();
        for row in qrows.iter().rev() {
            assert_eq!(
                row.landed_commit_id.as_deref(),
                Some(hash.as_str()),
                "root roll-up parent chain must follow push_queue.id order"
            );
            let commit = mono.get_commit_by_hash(&hash).await.unwrap().unwrap();
            let parents: Vec<String> =
                serde_json::from_value(commit.parents_id).unwrap_or_default();
            hash = parents
                .into_iter()
                .next()
                .expect("root roll-up must have a parent");
        }
        assert_eq!(
            hash,
            old_commit.id.to_string(),
            "oldest roll-up parent is the original root"
        );
    }

    fn merge_exec_ctx(storage: &Storage) -> MergeExecContext {
        MergeExecContext {
            storage: storage.clone(),
            git_object_cache: test_service(storage).git_object_cache,
            abort_before_cl_status: false,
            pause_after_apply: Duration::ZERO,
            pause_after_apply_barrier: None,
        }
    }

    async fn enqueue_merge_row(storage: &Storage, cl: &mega_cl::Model) -> i64 {
        use crate::jupiter::storage::push_queue_storage::EnqueueOutcome;

        let payload = MergePayload {
            cl_link: cl.link.clone(),
            authz_principal: "gate-tester".into(),
            execution_actor: "gate-tester".into(),
            apply_queue_execution_decision: false,
            requester: Some("gate-tester".into()),
        };
        let old_id = storage
            .mono_storage()
            .get_main_ref(&cl.path)
            .await
            .unwrap()
            .map(|r| r.ref_commit_hash)
            .unwrap_or_else(|| ZERO_ID.to_owned());
        match storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: merge_operation_id(&cl.link),
                path: cl.path.clone(),
                old_id,
                new_id: cl.to_hash.clone(),
                requester: Some("gate-tester".into()),
                payload: serde_json::to_value(&payload).unwrap(),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        {
            EnqueueOutcome::Inserted { id } | EnqueueOutcome::Adopted { id } => id,
            other => panic!("expected insert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queue_merge_add_rejects_failed_gpg_like_merge_cl() {
        let (_temp, storage, service, cl, _) = queue_merge_fixture("/", "QADDGPG", None).await;
        insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "FAILED").await;
        let err = service
            .add_to_merge_queue_as(cl.link.clone(), Some("gate-tester".into()))
            .await
            .expect_err("queue-mode add must run GPG entry precheck");
        assert!(
            err.to_string().contains("GPG signature check failed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn queue_merge_add_rejects_from_hash_mismatch() {
        let (_temp, storage, service, cl, _) = queue_merge_fixture("/", "QADDFH", None).await;
        let txn = storage.begin_db_transaction().await.unwrap();
        let current = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert!(
            storage
                .cl_storage()
                .cas_update_cl_hashes_in_txn(&current, &"f".repeat(40), &current.to_hash, &txn)
                .await
                .unwrap()
        );
        txn.commit().await.unwrap();
        let err = service
            .add_to_merge_queue_as(cl.link, Some("gate-tester".into()))
            .await
            .expect_err("from_hash != main must refuse at entry");
        assert!(err.to_string().to_lowercase().contains("conflict"), "{err}");
    }

    #[tokio::test]
    async fn queue_merge_abort_before_cl_status_rolls_back() {
        let (_temp, storage, _service, cl, _) = queue_merge_fixture("/", "QABORT", None).await;
        let root_before = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let id = enqueue_merge_row(&storage, &cl).await;
        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let mut ctx = merge_exec_ctx(&storage);
        ctx.abort_before_cl_status = true;
        let outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, ExecuteOutcome::Failed { .. }),
            "abort should terminalize after rollback, got {outcome:?}"
        );
        let root_after = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_after.ref_commit_hash, root_before.ref_commit_hash);
        let still = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.status, MergeStatusEnum::Open);
    }

    #[tokio::test]
    async fn queue_merge_intervening_other_path_is_preserved() {
        use git_internal::internal::object::commit::Commit;

        const N: usize = 2;
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
        let mono = storage.mono_storage();
        let mut child_trees = Vec::new();
        for i in 0..N {
            let blob = blob_item("base.txt", &format!("{:040x}", i + 1));
            child_trees.push(Tree::from_tree_items(vec![blob]).expect("child"));
        }
        let root_items: Vec<TreeItem> = child_trees
            .iter()
            .enumerate()
            .map(|(i, t)| TreeItem::new(TreeItemMode::Tree, t.id, format!("p{i:02}")))
            .collect();
        let old_root = Tree::from_tree_items(root_items).expect("root");
        let old_commit = Commit::from_tree_id(old_root.id, vec![], "base");
        for t in &child_trees {
            mono.save_mega_trees(vec![t.clone()], old_commit.id, None)
                .await
                .unwrap();
        }
        mono.save_mega_trees(vec![old_root.clone()], old_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![old_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &old_root, &old_commit.id.to_string()).await;

        let mut cls = Vec::new();
        for (i, child) in child_trees.iter().enumerate() {
            let path = format!("/p{i:02}");
            let link = format!("QINT{i:02}");
            let tip_tree =
                Tree::from_tree_items(vec![blob_item("new.txt", &format!("{:040x}", 1000 + i))])
                    .expect("tip");
            let tip = Commit::from_tree_id(tip_tree.id, vec![old_commit.id], "cl tip");
            mono.save_mega_trees(vec![tip_tree.clone()], tip.id, None)
                .await
                .unwrap();
            mono.save_mega_commits(vec![tip.clone()], None)
                .await
                .unwrap();
            mono.save_refs(
                mega_refs::Model::new(
                    path.clone(),
                    MEGA_BRANCH_NAME.to_owned(),
                    old_commit.id.to_string(),
                    child.id.to_string(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
            mono.save_or_update_cl_ref(
                &path,
                &format!("refs/cl/{link}"),
                &tip.id.to_string(),
                &tip_tree.id.to_string(),
            )
            .await
            .unwrap();
            cls.push(
                storage
                    .cl_storage()
                    .new_cl_model(
                        &path,
                        &link,
                        "qint",
                        "main",
                        &old_commit.id.to_string(),
                        &tip.id.to_string(),
                        "gate-tester",
                    )
                    .await
                    .unwrap(),
            );
        }

        let first = enqueue_merge_row(&storage, &cls[0]).await;
        let second = enqueue_merge_row(&storage, &cls[1]).await;
        let ctx = merge_exec_ctx(&storage);
        assert_eq!(
            storage
                .push_queue_service
                .wait_and_claim(first)
                .await
                .unwrap(),
            QueueWaitResult::Ready { id: first }
        );
        let first_out = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: first,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        let ExecuteOutcome::Done {
            landed_commit_id: tip_after_first,
            ..
        } = first_out
        else {
            panic!("first merge Done, got {first_out:?}");
        };
        assert_eq!(
            storage
                .push_queue_service
                .wait_and_claim(second)
                .await
                .unwrap(),
            QueueWaitResult::Ready { id: second }
        );
        let second_out = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: second,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(second_out, ExecuteOutcome::Done { .. }),
            "second merge Done after intervening, got {second_out:?}"
        );
        let final_root = mono.get_main_ref("/").await.unwrap().unwrap();
        let final_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&final_root.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(final_tree.tree_items.len(), N);
        let names: Vec<_> = final_tree
            .tree_items
            .iter()
            .map(|i| i.name.clone())
            .collect();
        assert!(names.contains(&"p00".into()) && names.contains(&"p01".into()));
        assert_ne!(final_root.ref_commit_hash, tip_after_first);
    }

    #[tokio::test]
    async fn queue_merge_conflict_requeue_then_succeeds() {
        let blob = blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd");
        let child = Tree::from_tree_items(vec![blob.clone()]).expect("child");
        let (_temp, storage, _service, cl, _) =
            queue_merge_fixture("/p00", "QCF01", Some(("p00".into(), child))).await;
        let id = enqueue_merge_row(&storage, &cl).await;
        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let path_main = storage
            .mono_storage()
            .get_main_ref("/p00")
            .await
            .unwrap()
            .unwrap();
        let mut advanced = path_main.clone();
        advanced.ref_commit_hash = "e".repeat(40);
        storage
            .mono_storage()
            .update_ref(advanced, None)
            .await
            .unwrap();
        let ctx = merge_exec_ctx(&storage);
        let outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        let ExecuteOutcome::Requeued { successor_id, .. } = outcome else {
            panic!("expected B4 requeue, got {outcome:?}");
        };
        let old_row = storage
            .push_queue_storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old_row.superseded_by, Some(successor_id));
        assert!(successor_id > id);

        let current = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        let txn = storage.begin_db_transaction().await.unwrap();
        assert!(
            storage
                .cl_storage()
                .cas_update_cl_hashes_in_txn(&current, &"e".repeat(40), &current.to_hash, &txn)
                .await
                .unwrap()
        );
        txn.commit().await.unwrap();

        assert_eq!(
            storage
                .push_queue_service
                .wait_and_claim(successor_id)
                .await
                .unwrap(),
            QueueWaitResult::Ready { id: successor_id }
        );
        let done = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: successor_id,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(done, ExecuteOutcome::Done { .. }),
            "successor must merge, got {done:?}"
        );
        let merged = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.status, MergeStatusEnum::Merged);
    }

    #[tokio::test]
    async fn queue_merge_revision_cas_miss_is_claim_lost_then_retries() {
        let (_temp, storage, _service, cl, _) = queue_merge_fixture("/", "QCAS1", None).await;
        let id = enqueue_merge_row(&storage, &cl).await;
        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let mut ctx = merge_exec_ctx(&storage);
        ctx.pause_after_apply = Duration::from_millis(200);
        ctx.pause_after_apply_barrier = Some(barrier.clone());
        let storage_racer = storage.clone();
        let cl_link = cl.link.clone();
        let racer = tokio::spawn(async move {
            barrier.wait().await;
            let current = storage_racer
                .cl_storage()
                .get_cl(&cl_link)
                .await
                .unwrap()
                .unwrap();
            let txn = storage_racer.begin_db_transaction().await.unwrap();
            let ok = storage_racer
                .cl_storage()
                .cas_update_cl_hashes_in_txn(&current, &current.from_hash, &current.to_hash, &txn)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            ok
        });
        let outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        let raced = racer.await.unwrap();
        if raced {
            assert_eq!(outcome, ExecuteOutcome::ClaimLost { id });
            let still = storage
                .cl_storage()
                .get_cl(&cl.link)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(still.status, MergeStatusEnum::Open);
            let ctx = merge_exec_ctx(&storage);
            assert_eq!(
                storage.push_queue_service.wait_and_claim(id).await.unwrap(),
                QueueWaitResult::Ready { id }
            );
            let retry = storage
                .push_queue_service
                .execute_b3(
                    ExecuteRequest {
                        id,
                        ..Default::default()
                    },
                    None,
                    Some(&ctx),
                    None,
                )
                .await
                .unwrap();
            assert!(
                matches!(retry, ExecuteOutcome::Done { .. }),
                "retry after ClaimLost, got {retry:?}"
            );
        } else {
            assert!(
                matches!(outcome, ExecuteOutcome::Done { .. }),
                "if the racer missed, the merge itself must Done, got {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn queue_merge_queue_entry_un17_freezes_anonymous_under_enforce() {
        use git_internal::internal::object::commit::Commit;

        use crate::config::testing::isolated_config;

        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.cedar.enforcement = "enforce".into();
        let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
        let blob = blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let old_tree = Tree::from_tree_items(vec![blob]).expect("old");
        let new_tree = Tree::from_tree_items(vec![blob_item(
            "queued.txt",
            "cccccccccccccccccccccccccccccccccccccccc",
        )])
        .expect("new");
        let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
        let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
        let mono = storage.mono_storage();
        mono.save_mega_trees(
            vec![old_tree.clone(), new_tree.clone()],
            old_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
        mono.save_or_update_cl_ref(
            "/",
            "refs/cl/QUN17",
            &new_commit.id.to_string(),
            &new_tree.id.to_string(),
        )
        .await
        .unwrap();
        let cl = storage
            .cl_storage()
            .new_cl_model(
                "/",
                "QUN17",
                "un17",
                "main",
                &old_commit.id.to_string(),
                &new_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
        let service = test_service(&storage);
        let err = service
            .add_to_merge_queue_as(cl.link.clone(), None)
            .await
            .expect_err("anonymous queue add under enforce must freeze");
        assert!(
            err.to_string().to_lowercase().contains("frozen")
                || err.to_string().to_lowercase().contains("requester"),
            "{err}"
        );
        let still = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.status, MergeStatusEnum::Open);
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };

    use super::*;
    use crate::{
        callisto::sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
        ceres::pack::materialize,
        config::{PushPolicy, testing::isolated_config},
        jupiter::{
            service::push_queue_service::{
                EnqueueRequest, ExecuteOutcome, ExecuteRequest, PushExecContext, PushPayload,
                push_operation_id,
            },
            storage::push_queue_storage::{ClaimOutcome, EnqueueOutcome},
            tests::test_storage_with_config,
        },
    };

    async fn tp11_storage(temp: &std::path::Path) -> Storage {
        let mut config = isolated_config(temp.join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        let storage = test_storage_with_config(temp, config).await;
        crate::jupiter::tests::with_test_vault(storage, temp).await
    }

    async fn nested_queue_fixture(
        path: &str,
        link: &str,
        dir: &str,
    ) -> (
        tempfile::TempDir,
        Storage,
        MonoApiService,
        mega_cl::Model,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let storage = tp11_storage(temp.path()).await;
        let service = test_service(&storage);
        let mono = storage.mono_storage();
        let child = Tree::from_tree_items(vec![blob_item(
            "x.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .expect("child");
        let path_tree_hash = child.id.to_string();
        mono.save_mega_trees(
            vec![child.clone()],
            ObjectHash::from_str(&"1".repeat(40)).unwrap(),
            None,
        )
        .await
        .unwrap();
        let old_tree = Tree::from_tree_items(vec![
            blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
        ])
        .expect("old tree");
        let new_tree = Tree::from_tree_items(vec![blob_item(
            "queued.txt",
            "cccccccccccccccccccccccccccccccccccccccc",
        )])
        .expect("new tree");
        let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
        let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
        mono.save_mega_trees(
            vec![old_tree.clone(), new_tree.clone()],
            old_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: path.to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: old_commit.id.to_string(),
                ref_tree_hash: path_tree_hash,
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
        mono.save_or_update_cl_ref(
            path,
            &format!("refs/cl/{link}"),
            &new_commit.id.to_string(),
            &new_tree.id.to_string(),
        )
        .await
        .unwrap();
        let cl = storage
            .cl_storage()
            .new_cl_model(
                path,
                link,
                "queue merge",
                "main",
                &old_commit.id.to_string(),
                &new_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
        (temp, storage, service, cl, old_commit.id.to_string())
    }

    async fn poison_main_tree(storage: &Storage, path: &str) -> String {
        let mut row = storage
            .mono_storage()
            .get_main_ref(path)
            .await
            .unwrap()
            .unwrap();
        let tip = row.ref_commit_hash.clone();
        row.ref_tree_hash = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into();
        storage.mono_storage().update_ref(row, None).await.unwrap();
        tip
    }

    #[tokio::test]
    async fn tp11_merge_stale_tree_hash_refuses_and_tombstones() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, service, cl, stale_tip) =
            nested_queue_fixture("/p11", "TP11M1", "p11").await;
        poison_main_tree(&storage, "/p11").await;
        let err = service
            .merge_cl("gate-tester", "gate-tester", cl)
            .await
            .expect_err("stale tree hash must refuse merge");
        assert!(err.to_string().contains("stale materialized"), "{err}");
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/p11")
                .await
                .unwrap()
                .is_none()
        );
        let tomb = storage
            .mono_storage()
            .get_tombstone("/p11", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .expect("tombstone");
        assert_eq!(tomb.last_commit_hash, stale_tip);
        assert!(
            storage
                .push_queue_service
                .metrics()
                .tree_hash_assert_failures
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 1
        );
    }

    #[tokio::test]
    async fn tp11_push_stale_tree_hash_refuses_and_tombstones() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _service, _cl, stale_tip) =
            nested_queue_fixture("/p11p", "TP11P1", "p11p").await;
        poison_main_tree(&storage, "/p11p").await;
        let outcome = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(&stale_tip, &"c".repeat(40)),
                path: "/p11p".into(),
                old_id: stale_tip.clone(),
                new_id: "c".repeat(40),
                requester: None,
                payload: serde_json::json!({}),
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
        let exec = storage
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
            .unwrap();
        match exec {
            ExecuteOutcome::Failed {
                failure, message, ..
            } => {
                assert_eq!(failure, "Conflict");
                assert!(message.contains("advertise"), "{message}");
                assert!(message.contains("fetch"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/p11p")
                .await
                .unwrap()
                .is_none()
        );
        let tomb = storage
            .mono_storage()
            .get_tombstone("/p11p", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .expect("tombstone");
        assert_eq!(tomb.last_commit_hash, stale_tip);
        let row = storage
            .push_queue_service
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert!(row.pending_action.is_none());
    }

    #[tokio::test]
    async fn tp11_merge_repair_advertise_continues_from_tombstone() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, service, cl, stale_tip) =
            nested_queue_fixture("/p11a", "TP11AD", "p11a").await;
        poison_main_tree(&storage, "/p11a").await;
        service
            .merge_cl("gate-tester", "gate-tester", cl)
            .await
            .expect_err("stale");
        let head = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/p11a")
            .await
            .unwrap();
        assert_ne!(head, ZERO_ID);
        let commit = storage
            .mono_storage()
            .get_commit_by_hash(&head)
            .await
            .unwrap()
            .expect("revived commit");
        let parents: Vec<String> = serde_json::from_value(commit.parents_id).unwrap();
        assert_eq!(parents, vec![stale_tip]);
    }

    #[tokio::test]
    async fn tp11_assertion_precedes_conflict_recheck() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, service, cl, stale_tip) =
            nested_queue_fixture("/p11c", "TP11CR", "p11c").await;
        poison_main_tree(&storage, "/p11c").await;
        let txn = storage.begin_db_transaction().await.unwrap();
        let current = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert!(
            storage
                .cl_storage()
                .cas_update_cl_hashes_in_txn(&current, &"f".repeat(40), &current.to_hash, &txn)
                .await
                .unwrap()
        );
        txn.commit().await.unwrap();
        let err = service
            .merge_cl("gate-tester", "gate-tester", cl)
            .await
            .expect_err("stale tree must refuse before from_hash conflict");
        assert!(err.to_string().contains("stale materialized"), "got {err}");
        assert!(
            storage
                .mono_storage()
                .get_tombstone("/p11c", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_some()
        );
        let rows = storage
            .push_queue_service
            .storage()
            .list_recent_finished(20)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|r| r.operation_id == "TP11CR")
            .expect("queued merge row");
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert!(row.pending_action.is_none());
        assert_eq!(
            storage
                .mono_storage()
                .get_tombstone("/p11c", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .unwrap()
                .last_commit_hash,
            stale_tip
        );
    }

    #[tokio::test]
    async fn tp11_cl_ref_is_not_asserted() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, service, cl, _tip) =
            nested_queue_fixture("/p11cl", "TP11CL", "p11cl").await;
        let mut cl_ref = storage
            .mono_storage()
            .get_ref_by_name(&format!("refs/cl/{}", cl.link))
            .await
            .unwrap()
            .unwrap();
        cl_ref.ref_tree_hash = "ffffffffffffffffffffffffffffffffffffffff".into();
        storage
            .mono_storage()
            .update_ref(cl_ref, None)
            .await
            .unwrap();
        service
            .merge_cl("gate-tester", "gate-tester", cl.clone())
            .await
            .expect("poisoned CL ref must not trip main tree-hash assertion");
        let merged = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.status, MergeStatusEnum::Merged);
        assert!(
            storage
                .mono_storage()
                .get_tombstone("/p11cl", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn tp11_reconcile_and_inspect_tombstone_stale_rows() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _service, _cl, stale_tip) =
            nested_queue_fixture("/p11r", "TP11RC", "p11r").await;
        poison_main_tree(&storage, "/p11r").await;
        let audit = storage.push_queue_service.audit().with_batch_size(8);
        let rec = audit.reconcile_once().await.unwrap();
        assert!(rec.lock_acquired);
        assert!(rec.tombstoned >= 1);
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/p11r")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            storage
                .push_queue_service
                .metrics()
                .reconcile_tombstoned
                .load(std::sync::atomic::Ordering::Relaxed),
            rec.tombstoned
        );
        storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/p11r",
                    MEGA_BRANCH_NAME.to_owned(),
                    stale_tip.clone(),
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let ins = audit.inspect_once().await.unwrap();
        assert!(ins.lock_acquired);
        assert!(ins.tombstoned >= 1);
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/p11r")
                .await
                .unwrap()
                .is_none()
        );
        let snap = storage.push_queue_service.metrics_snapshot().await.unwrap();
        assert!(snap.inspect_tombstoned >= 1);
        assert!(snap.reconcile_tombstoned >= 1);
    }

    #[tokio::test]
    async fn tp11_inspect_skips_root_and_cl_refs() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _service, cl, _) =
            nested_queue_fixture("/p11s", "TP11SK", "p11s").await;
        let mut cl_ref = storage
            .mono_storage()
            .get_ref_by_name(&format!("refs/cl/{}", cl.link))
            .await
            .unwrap()
            .unwrap();
        cl_ref.ref_tree_hash = "ffffffffffffffffffffffffffffffffffffffff".into();
        storage
            .mono_storage()
            .update_ref(cl_ref, None)
            .await
            .unwrap();
        let root_before = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let audit = storage.push_queue_service.audit();
        let rec = audit.reconcile_once().await.unwrap();
        assert_eq!(rec.tombstoned, 0);
        let root_after = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
        assert!(
            storage
                .mono_storage()
                .get_ref_by_name(&format!("refs/cl/{}", cl.link))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/p11s")
                .await
                .unwrap()
                .is_some()
        );
    }

    async fn tp12_ctx(storage: &Storage) -> PushExecContext {
        PushExecContext {
            git_object_cache: Arc::new(GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
            storage: storage.clone(),
        }
    }

    async fn tp12_enqueue_claim(
        storage: &Storage,
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

    async fn tp12_exec(storage: &Storage, id: i64) -> ExecuteOutcome {
        let ctx = tp12_ctx(storage).await;
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

    /// Root + `main@/{dir}` whose tree is `child` and tip is `path_commit`.
    async fn tp12_path_fixture(
        dir: &str,
        path_commit_msg: &str,
    ) -> (
        tempfile::TempDir,
        Storage,
        Tree,
        Commit,
        Tree,
        Commit,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let storage = tp11_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let child = Tree::from_tree_items(vec![blob_item(
            "x.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .expect("child");
        let root_tree = Tree::from_tree_items(vec![
            blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
        ])
        .expect("root");
        let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
        let path_commit = Commit::from_tree_id(child.id, vec![], path_commit_msg);
        mono.save_mega_trees(vec![child.clone(), root_tree.clone()], root_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![root_commit.clone(), path_commit.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &root_tree, &root_commit.id.to_string()).await;
        let path = format!("/{dir}");
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: path.clone(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: path_commit.id.to_string(),
                ref_tree_hash: child.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
        (
            temp,
            storage,
            root_tree,
            root_commit,
            child,
            path_commit,
            path,
        )
    }

    #[tokio::test]
    async fn tp12_n1_fast_forward_lands_client_tip() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
            tp12_path_fixture("p12n1", "path tip").await;
        let new_child = Tree::from_tree_items(vec![blob_item(
            "y.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "n1");
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
        let payload = PushPayload {
            commits: vec![new_commit.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(
            &storage,
            &path,
            &path_commit.id.to_string(),
            &new_commit.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        match exec {
            ExecuteOutcome::Done {
                landed_commit_id,
                root_cas_writes,
                ..
            } => {
                assert_eq!(landed_commit_id, new_commit.id.to_string());
                assert_eq!(root_cas_writes, 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, new_commit.id.to_string());
    }

    #[tokio::test]
    async fn tp14_push_continues_changed_descendant_and_skips_unchanged() {
        let _lock = materialize::lock_materialize_tests().await;
        let temp = tempfile::tempdir().unwrap();
        let storage = tp11_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let foo_old = Tree::from_tree_items(vec![blob_item(
            "a.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .unwrap();
        let foo_new = Tree::from_tree_items(vec![blob_item(
            "a.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let keep = Tree::from_tree_items(vec![blob_item(
            "k.txt",
            "ffffffffffffffffffffffffffffffffffffffff",
        )])
        .unwrap();
        let p_old = Tree::from_tree_items(vec![
            TreeItem::new(TreeItemMode::Tree, foo_old.id, "foo".into()),
            TreeItem::new(TreeItemMode::Tree, keep.id, "keep".into()),
        ])
        .unwrap();
        let p_new = Tree::from_tree_items(vec![
            TreeItem::new(TreeItemMode::Tree, foo_new.id, "foo".into()),
            TreeItem::new(TreeItemMode::Tree, keep.id, "keep".into()),
        ])
        .unwrap();
        let root = Tree::from_tree_items(vec![
            blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, p_old.id, "p14".into()),
        ])
        .unwrap();
        let root_c = Commit::from_tree_id(root.id, vec![], "root");
        let p_c = Commit::from_tree_id(p_old.id, vec![], "p14");
        let foo_c = Commit::from_tree_id(foo_old.id, vec![], "foo");
        let keep_c = Commit::from_tree_id(keep.id, vec![], "keep");
        let push_c = Commit::from_tree_id(p_new.id, vec![p_c.id], "push p14");
        mono.save_mega_trees(
            vec![
                foo_old.clone(),
                foo_new.clone(),
                keep.clone(),
                p_old.clone(),
                p_new.clone(),
                root.clone(),
            ],
            root_c.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(
            vec![
                root_c.clone(),
                p_c.clone(),
                foo_c.clone(),
                keep_c.clone(),
                push_c.clone(),
            ],
            None,
        )
        .await
        .unwrap();
        setup_main_ref(&storage, &root, &root_c.id.to_string()).await;
        for (path, commit, tree) in [
            ("/p14", &p_c, &p_old),
            ("/p14/foo", &foo_c, &foo_old),
            ("/p14/keep", &keep_c, &keep),
        ] {
            mono.save_refs(
                mega_refs::Model {
                    id: crate::callisto::entity_ext::generate_id(),
                    path: path.into(),
                    ref_name: MEGA_BRANCH_NAME.to_string(),
                    ref_commit_hash: commit.id.to_string(),
                    ref_tree_hash: tree.id.to_string(),
                    created_at: chrono::Utc::now().naive_utc(),
                    updated_at: chrono::Utc::now().naive_utc(),
                    is_cl: false,
                },
                None,
            )
            .await
            .unwrap();
        }
        let payload = PushPayload {
            commits: vec![push_c.id.to_string()],
            fork_base: Some(p_c.id.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(
            &storage,
            "/p14",
            &p_c.id.to_string(),
            &push_c.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        assert!(
            matches!(exec, ExecuteOutcome::Done { .. }),
            "push B3: {exec:?}"
        );

        let foo = mono.get_main_ref("/p14/foo").await.unwrap().unwrap();
        assert_eq!(foo.ref_tree_hash, foo_new.id.to_string());
        let cont = mono
            .get_commit_by_hash(&foo.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let parents: Vec<String> = serde_json::from_value(cont.parents_id).unwrap();
        assert_eq!(parents, vec![foo_c.id.to_string()]);
        let cont_msg = cont.content.as_deref().unwrap_or("");
        assert!(
            cont_msg.contains("push p14") || cont_msg.contains("gpgsig"),
            "N=1 descendant keeps the client message: {cont_msg}"
        );
        assert_has_gpgsig(cont_msg);

        let keep_row = mono.get_main_ref("/p14/keep").await.unwrap().unwrap();
        assert_eq!(keep_row.ref_commit_hash, keep_c.id.to_string());
    }

    #[tokio::test]
    async fn tp12_n2_known_objects_still_squash_and_done_replay() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
            tp12_path_fixture("p12n2", "path tip").await;
        let mid_tree = Tree::from_tree_items(vec![blob_item(
            "m.txt",
            "1111111111111111111111111111111111111111",
        )])
        .unwrap();
        let tip_tree = Tree::from_tree_items(vec![blob_item(
            "t.txt",
            "2222222222222222222222222222222222222222",
        )])
        .unwrap();
        let mid = Commit::from_tree_id(mid_tree.id, vec![path_commit.id], "mid");
        let tip = Commit::from_tree_id(tip_tree.id, vec![mid.id], "tip");
        storage
            .mono_storage()
            .save_mega_trees(vec![mid_tree, tip_tree], tip.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![mid.clone(), tip.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![tip.id.to_string(), mid.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 2,
        };
        let old_id = path_commit.id.to_string();
        let new_id = tip.id.to_string();
        let id = tp12_enqueue_claim(&storage, &path, &old_id, &new_id, &payload).await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("expected Done, got {exec:?}");
        };
        assert_ne!(landed_commit_id, new_id, "N>1 must squash, not FF");
        let squash = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .expect("squash");
        let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
        assert_eq!(parents, vec![old_id.clone()]);
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, landed_commit_id);
        for cid in &payload.commits {
            assert!(
                storage
                    .mono_storage()
                    .get_commit_by_hash(cid)
                    .await
                    .unwrap()
                    .is_some(),
                "payload must reconstruct the original chain"
            );
        }

        let replay = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(&old_id, &new_id),
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
        match replay {
            EnqueueOutcome::Replay {
                id: rid,
                landed_commit_id: replayed,
            } => {
                let row = storage
                    .push_queue_service
                    .storage()
                    .get_by_id(rid)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(row.status, PushQueueStatusEnum::Done);
                assert_eq!(replayed.as_deref(), Some(landed_commit_id.as_str()));
            }
            other => panic!("expected Done replay, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tp12_n0_matching_tip_is_noop_done() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
            tp12_path_fixture("p12n0", "path tip").await;
        let tip = path_commit.id.to_string();
        let payload = PushPayload {
            commits: vec![tip.clone()],
            fork_base: Some(tip.clone()),
            n: 0,
        };
        let root_before = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let id = tp12_enqueue_claim(&storage, &path, &tip, &tip, &payload).await;
        let exec = tp12_exec(&storage, id).await;
        match exec {
            ExecuteOutcome::Done {
                landed_commit_id,
                root_cas_writes,
                ..
            } => {
                assert_eq!(landed_commit_id, tip);
                assert_eq!(root_cas_writes, 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let root_after = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_before.ref_commit_hash, root_after.ref_commit_hash);
        assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, tip);
    }

    #[tokio::test]
    async fn tp12_net_zero_same_tree_cas_once() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, child, path_commit, path) =
            tp12_path_fixture("p12nz", "path tip").await;
        let new_commit = Commit::from_tree_id(child.id, vec![path_commit.id], "same tree n1");
        storage
            .mono_storage()
            .save_mega_commits(vec![new_commit.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![new_commit.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 1,
        };
        let root_before = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let id = tp12_enqueue_claim(
            &storage,
            &path,
            &path_commit.id.to_string(),
            &new_commit.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        match exec {
            ExecuteOutcome::Done {
                landed_commit_id,
                root_cas_writes,
                ..
            } => {
                assert_eq!(landed_commit_id, new_commit.id.to_string());
                assert_eq!(root_cas_writes, 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let root_after = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_before.ref_commit_hash, root_after.ref_commit_hash);
        assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, new_commit.id.to_string());
        assert_eq!(pref.ref_tree_hash, child.id.to_string());
    }

    #[tokio::test]
    async fn tp12_create_n1_and_n2_and_multilevel() {
        let _lock = materialize::lock_materialize_tests().await;
        let temp = tempfile::tempdir().unwrap();
        let storage = tp11_storage(temp.path()).await;
        let keep = Tree::from_tree_items(vec![blob_item(
            ".gitkeep",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )])
        .unwrap();
        let root_c = Commit::from_tree_id(keep.id, vec![], "root");
        storage
            .mono_storage()
            .save_mega_trees(vec![keep.clone()], root_c.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![root_c.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;

        let leaf = Tree::from_tree_items(vec![blob_item(
            "f.txt",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )])
        .unwrap();
        let n1 = Commit::from_tree_id(leaf.id, vec![], "create n1");
        storage
            .mono_storage()
            .save_mega_trees(vec![leaf.clone()], n1.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![n1.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![n1.id.to_string()],
            fork_base: Some(ZERO_ID.to_string()),
            n: 1,
        };
        let id =
            tp12_enqueue_claim(&storage, "/p12c1", ZERO_ID, &n1.id.to_string(), &payload).await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("create n1: {exec:?}");
        };
        assert_eq!(landed_commit_id, n1.id.to_string());
        let pref = storage
            .mono_storage()
            .get_main_ref("/p12c1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, n1.id.to_string());

        let mid_t = Tree::from_tree_items(vec![blob_item(
            "m.txt",
            "cccccccccccccccccccccccccccccccccccccccc",
        )])
        .unwrap();
        let tip_t = Tree::from_tree_items(vec![blob_item(
            "t.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .unwrap();
        let mid = Commit::from_tree_id(mid_t.id, vec![], "c2 mid");
        let tip = Commit::from_tree_id(tip_t.id, vec![mid.id], "c2 tip");
        storage
            .mono_storage()
            .save_mega_trees(vec![mid_t, tip_t], tip.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![mid.clone(), tip.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![tip.id.to_string(), mid.id.to_string()],
            fork_base: Some(ZERO_ID.to_string()),
            n: 2,
        };
        let id =
            tp12_enqueue_claim(&storage, "/p12c2", ZERO_ID, &tip.id.to_string(), &payload).await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("create n2: {exec:?}");
        };
        assert_ne!(landed_commit_id, tip.id.to_string());
        let squash = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .unwrap();
        let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
        assert!(parents.is_empty(), "create N>1 is parentless");
        for cid in &payload.commits {
            assert!(
                storage
                    .mono_storage()
                    .get_commit_by_hash(cid)
                    .await
                    .unwrap()
                    .is_some()
            );
        }

        let deep = Tree::from_tree_items(vec![blob_item(
            "z.txt",
            "ffffffffffffffffffffffffffffffffffffffff",
        )])
        .unwrap();
        let deep_c = Commit::from_tree_id(deep.id, vec![], "deep");
        storage
            .mono_storage()
            .save_mega_trees(vec![deep.clone()], deep_c.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![deep_c.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![deep_c.id.to_string()],
            fork_base: Some(ZERO_ID.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(
            &storage,
            "/a/b/c",
            ZERO_ID,
            &deep_c.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("multilevel: {exec:?}");
        };
        assert_eq!(landed_commit_id, deep_c.id.to_string());
        let pref = storage
            .mono_storage()
            .get_main_ref("/a/b/c")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, deep_c.id.to_string());
    }

    #[tokio::test]
    async fn tp12_tombstone_and_wait_materialize_and_assertions() {
        let _lock = materialize::lock_materialize_tests().await;
        let temp = tempfile::tempdir().unwrap();
        let storage = tp11_storage(temp.path()).await;
        let keep = Tree::from_tree_items(vec![blob_item(
            ".gitkeep",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )])
        .unwrap();
        let root_c = Commit::from_tree_id(keep.id, vec![], "root");
        storage
            .mono_storage()
            .save_mega_trees(vec![keep.clone()], root_c.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![root_c.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;

        let leaf = Tree::from_tree_items(vec![blob_item(
            "f.txt",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )])
        .unwrap();
        let n1 = Commit::from_tree_id(leaf.id, vec![], "create");
        storage
            .mono_storage()
            .save_mega_trees(vec![leaf.clone()], n1.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![n1.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![n1.id.to_string()],
            fork_base: Some(ZERO_ID.to_string()),
            n: 1,
        };
        let id =
            tp12_enqueue_claim(&storage, "/p12tb", ZERO_ID, &n1.id.to_string(), &payload).await;
        storage
            .mono_storage()
            .upsert_tombstone("/p12tb", MEGA_BRANCH_NAME, &"c".repeat(40), &"d".repeat(40))
            .await
            .unwrap();
        let exec = tp12_exec(&storage, id).await;
        match exec {
            ExecuteOutcome::Failed {
                failure, message, ..
            } => {
                assert_eq!(failure, "Conflict");
                assert!(message.contains("advertise"), "{message}");
                assert!(message.contains("fetch"), "{message}");
            }
            other => panic!("tombstone race: {other:?}"),
        }

        let (_temp2, storage2, _rt, _rc, child, path_commit, path) =
            tp12_path_fixture("p12wm", "synth").await;
        let payload = PushPayload {
            commits: vec![n1.id.to_string()],
            fork_base: Some(ZERO_ID.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(&storage2, &path, ZERO_ID, &n1.id.to_string(), &payload).await;
        let exec = tp12_exec(&storage2, id).await;
        match exec {
            ExecuteOutcome::Failed { message, .. } => {
                assert!(message.contains("materialized while waiting"), "{message}");
            }
            other => panic!("wait materialize: {other:?}"),
        }
        let _ = (child, path_commit);

        let (_temp3, storage3, _rt, _rc, _child, path_commit, path) =
            tp12_path_fixture("p12as", "tip").await;
        let payload = PushPayload {
            commits: vec!["e".repeat(40)],
            fork_base: Some(path_commit.id.to_string()),
            n: 1,
        };
        let wrong_old = "f".repeat(40);
        let id = tp12_enqueue_claim(&storage3, &path, &wrong_old, &"e".repeat(40), &payload).await;
        let exec = tp12_exec(&storage3, id).await;
        match exec {
            ExecuteOutcome::Failed { message, .. } => {
                assert!(message.contains("non-fast-forward"), "{message}");
            }
            other => panic!("nff: {other:?}"),
        }

        let payload = PushPayload {
            commits: vec![path_commit.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(
            &storage3,
            "/missing-path",
            &path_commit.id.to_string(),
            &n1.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage3, id).await;
        match exec {
            ExecuteOutcome::Failed { message, .. } => {
                assert!(
                    message.contains("missing path") || message.contains("ZERO_ID"),
                    "{message}"
                );
            }
            other => panic!("missing row: {other:?}"),
        }

        let tip = path_commit.id.to_string();
        let bogus = "9".repeat(40);
        let payload = PushPayload {
            commits: vec![bogus.clone()],
            fork_base: Some(tip.clone()),
            n: 0,
        };
        let id = tp12_enqueue_claim(&storage3, &path, &bogus, &bogus, &payload).await;
        let exec = tp12_exec(&storage3, id).await;
        match exec {
            ExecuteOutcome::Failed { message, .. } => {
                assert!(
                    message.contains("n=0")
                        || message.contains("new_id does not match")
                        || message.contains("invariant"),
                    "{message}"
                );
            }
            other => panic!("n0 mismatch: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tp12_gap14_empty_pack_and_all_known_enqueue_b3() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _rt, _rc, _child, path_commit, path) =
            tp12_path_fixture("p12g14", "path tip").await;
        let new_child = Tree::from_tree_items(vec![blob_item(
            "g.txt",
            "3333333333333333333333333333333333333333",
        )])
        .unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "g14");
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

        let cmd = crate::ceres::protocol::import_refs::RefCommand::new(
            path_commit.id.to_string(),
            new_commit.id.to_string(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let chain = crate::ceres::pack::push_chain::PushChain::from_known_tip(
            &cmd,
            new_commit.clone(),
            &storage.mono_storage(),
            crate::ceres::merge_checker::MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();
        assert_eq!(chain.ordered_commits.len(), 1);
        let payload = PushPayload::from_chain(
            &path_commit.id.to_string(),
            &new_commit.id.to_string(),
            &chain,
        );
        assert_eq!(payload.n, 1);

        let repo = tp12_monorepo(
            &storage,
            vec![cmd.clone()],
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        );
        let built = repo.build_push_chain(&cmd).await.unwrap();
        assert!(built.is_some(), "trunk empty pack must not Noop");

        let id = tp12_enqueue_claim(
            &storage,
            &path,
            &path_commit.id.to_string(),
            &new_commit.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        assert!(matches!(exec, ExecuteOutcome::Done { .. }), "{exec:?}");

        let mut pack = std::collections::HashSet::new();
        pack.insert(new_commit.id.to_string());
        let repo = tp12_monorepo(
            &storage,
            vec![cmd.clone()],
            pack,
            std::collections::HashSet::new(),
        );
        let built = repo.build_push_chain(&cmd).await.unwrap();
        assert!(
            built.is_some(),
            "all-known pack still yields a chain under trunk"
        );
    }

    #[tokio::test]
    async fn tp12_review_keeps_noop() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Review;
        let storage = test_storage_with_config(temp.path(), config).await;
        let keep = Tree::from_tree_items(vec![blob_item(
            ".gitkeep",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )])
        .unwrap();
        let root_c = Commit::from_tree_id(keep.id, vec![], "root");
        storage
            .mono_storage()
            .save_mega_trees(vec![keep.clone()], root_c.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![root_c.clone()], None)
            .await
            .unwrap();
        setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;
        let cmd = crate::ceres::protocol::import_refs::RefCommand::new(
            root_c.id.to_string(),
            root_c.id.to_string(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = tp12_monorepo(
            &storage,
            vec![cmd.clone()],
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        );
        let built = repo.build_push_chain(&cmd).await.unwrap();
        assert!(built.is_none(), "review empty pack stays Noop");
        assert!(crate::ceres::pack::RepoHandler::receive_pack_notice(&repo).is_some());
    }

    fn authored_commit(
        tree: ObjectHash,
        parents: Vec<ObjectHash>,
        name: &str,
        email: &str,
        ts: usize,
        message: &str,
    ) -> Commit {
        use git_internal::internal::object::signature::{Signature, SignatureType};
        let author = Signature {
            signature_type: SignatureType::Author,
            name: name.into(),
            email: email.into(),
            timestamp: ts,
            timezone: "+0800".into(),
        };
        let committer = Signature {
            signature_type: SignatureType::Committer,
            name: name.into(),
            email: email.into(),
            timestamp: ts,
            timezone: "+0800".into(),
        };
        Commit::new(author, committer, tree, parents, message)
    }

    fn assert_has_gpgsig(content: &str) {
        assert!(
            content.contains("gpgsig ") || content.contains("-----BEGIN PGP SIGNATURE-----"),
            "synthetic commit must carry a server GPG signature: {content}"
        );
    }

    #[tokio::test]
    async fn tp16_n1_lands_client_tip_and_root_message_matches() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
            tp12_path_fixture("p16n1", "path tip").await;
        let new_child = Tree::from_tree_items(vec![blob_item(
            "y.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let new_commit = authored_commit(
            new_child.id,
            vec![path_commit.id],
            "Alice",
            "alice@example.com",
            1_720_000_000,
            "feat: n1 client message",
        );
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
        let payload = PushPayload {
            commits: vec![new_commit.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 1,
        };
        let id = tp12_enqueue_claim(
            &storage,
            &path,
            &path_commit.id.to_string(),
            &new_commit.id.to_string(),
            &payload,
        )
        .await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("expected Done, got {exec:?}");
        };
        assert_eq!(landed_commit_id, new_commit.id.to_string());
        let landed = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            landed.content.as_deref(),
            Some("feat: n1 client message"),
            "N=1 must keep the client object"
        );
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let root_c = storage
            .mono_storage()
            .get_commit_by_hash(&root.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let root_msg = root_c.content.as_deref().unwrap_or("");
        assert!(
            root_msg.contains("feat: n1 client message"),
            "root roll-up message must match the client commit: {root_msg}"
        );
        assert!(!root_msg.contains("Mono-Squash"));
        assert_has_gpgsig(root_msg);
        assert!(
            root_c
                .author
                .as_deref()
                .unwrap_or("")
                .contains("Alice <alice@example.com>")
        );
    }

    #[tokio::test]
    async fn tp16_n3_squash_provenance_and_coauthors() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
            tp12_path_fixture("p16n3", "path tip").await;
        let t1 = Tree::from_tree_items(vec![blob_item(
            "a.txt",
            "1111111111111111111111111111111111111111",
        )])
        .unwrap();
        let t2 = Tree::from_tree_items(vec![blob_item(
            "b.txt",
            "2222222222222222222222222222222222222222",
        )])
        .unwrap();
        let t3 = Tree::from_tree_items(vec![blob_item(
            "c.txt",
            "3333333333333333333333333333333333333333",
        )])
        .unwrap();
        let c1 = authored_commit(
            t1.id,
            vec![path_commit.id],
            "Alice",
            "alice@example.com",
            1_720_000_000,
            "feat: parser",
        );
        let c2 = authored_commit(
            t2.id,
            vec![c1.id],
            "Bob",
            "bob@example.com",
            1_720_000_000 + 200_000,
            "fix: empty input",
        );
        let c3 = authored_commit(
            t3.id,
            vec![c2.id],
            "Alice",
            "alice@example.com",
            1_720_000_000 + 200_001,
            "test: edge cases",
        );
        storage
            .mono_storage()
            .save_mega_trees(vec![t1, t2, t3.clone()], c3.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![c1.clone(), c2.clone(), c3.clone()], None)
            .await
            .unwrap();
        let payload = PushPayload {
            commits: vec![c3.id.to_string(), c2.id.to_string(), c1.id.to_string()],
            fork_base: Some(path_commit.id.to_string()),
            n: 3,
        };
        let old_id = path_commit.id.to_string();
        let id = tp12_enqueue_claim(&storage, &path, &old_id, &c3.id.to_string(), &payload).await;
        let exec = tp12_exec(&storage, id).await;
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = exec
        else {
            panic!("expected Done, got {exec:?}");
        };
        assert_ne!(landed_commit_id, c3.id.to_string());
        let squash = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(squash.tree, t3.id.to_string());
        let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
        assert_eq!(parents, vec![old_id.clone()]);
        let msg = squash.content.as_deref().unwrap_or("");
        assert!(
            msg.contains("Squash 3 commits at /p16n3"),
            "signed squash keeps the subject after gpgsig: {msg}"
        );
        assert!(msg.contains("This commit was created by monoengine"));
        assert!(msg.contains("Mono-Squash-Count: 3"));
        assert!(msg.contains(&format!("Mono-Squash-Range: {old_id}..{}", c3.id)));
        assert!(!msg.contains("Mono-Commits"));
        let pos1 = msg.find(&c1.id.to_string()).expect("c1 listed");
        let pos2 = msg.find(&c2.id.to_string()).expect("c2 listed");
        let pos3 = msg.find(&c3.id.to_string()).expect("c3 listed");
        assert!(pos1 < pos2 && pos2 < pos3, "topo ascending listing");
        assert!(msg.contains("Co-authored-by: Bob <bob@example.com>"));
        assert!(msg.contains("Mono-Author-Date-Range:"));
        assert!(
            squash
                .author
                .as_deref()
                .unwrap_or("")
                .contains("Alice <alice@example.com>")
        );
        assert_has_gpgsig(msg);

        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let root_c = storage
            .mono_storage()
            .get_commit_by_hash(&root.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let root_msg = root_c.content.as_deref().unwrap_or("");
        assert!(root_msg.contains(&format!("Mono-Squash-Commit: {landed_commit_id}")));
        assert!(!root_msg.contains("feat: parser"));
        assert_has_gpgsig(root_msg);
        assert!(
            root_c
                .author
                .as_deref()
                .unwrap_or("")
                .contains("Alice <alice@example.com>")
        );
    }

    #[tokio::test]
    async fn tp16_review_merge_keeps_from_tree_id_shape() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, service, cl, _stale) =
            nested_queue_fixture("/p16m", "TP16M", "p16m").await;
        service
            .merge_cl("gate-tester", "gate-tester", cl.clone())
            .await
            .expect("merge");
        let main = storage
            .mono_storage()
            .get_main_ref("/p16m")
            .await
            .unwrap()
            .unwrap();
        let landed = storage
            .mono_storage()
            .get_commit_by_hash(&main.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        let msg = landed.content.as_deref().unwrap_or("");
        assert_eq!(msg, "cl merge generated commit");
        assert!(
            landed
                .author
                .as_deref()
                .unwrap_or("")
                .contains("mega <admin@mega.org>")
        );
        assert!(!msg.contains("Mono-Squash"));
    }

    fn tp12_monorepo(
        storage: &Storage,
        commands: Vec<crate::ceres::protocol::import_refs::RefCommand>,
        pack_commit_ids: std::collections::HashSet<String>,
        new_commit_ids: std::collections::HashSet<String>,
    ) -> crate::ceres::pack::monorepo::Monorepo {
        use std::{collections::HashMap, path::PathBuf, sync::Mutex};

        use tokio::sync::RwLock;

        crate::ceres::pack::monorepo::Monorepo {
            storage: storage.clone(),
            git_object_cache: test_service(storage).git_object_cache,
            path: PathBuf::from("/"),
            base_branch: "main".to_string(),
            pack_commit_ids: Mutex::new(pack_commit_ids),
            new_commit_ids: Mutex::new(new_commit_ids),
            no_op_notice: Mutex::new(None),
            push_chain_cache: Mutex::new(HashMap::new()),
            cl_link: std::sync::Arc::new(RwLock::new(None)),
            bellatrix: std::sync::Arc::new(crate::bellatrix::Bellatrix::new(
                storage.config().build.clone(),
            )),
            username: Some("tester".to_string()),
            command_list: Mutex::new(commands),
        }
    }
}
