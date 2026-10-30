use git_internal::hash::HashKind;

use super::{RefUpdate, TreeUpdateResult, prelude::*};

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

pub(in crate::ceres::api_service::mono_api_service) static PATH_NOT_EXIST_RE: LazyLock<Regex> =
    LazyLock::new(|| {
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
        Tree::from_tree_items_with_kind(tree.id.kind(), items)
            .map_err(|_| GitError::CustomError("Invalid tree".to_string()))
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
        Self::tree_from_items_checked_with_kind(tree.id.kind(), items)
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
    pub fn tree_from_items_checked(items: Vec<TreeItem>) -> Result<Tree, GitError> {
        Self::tree_from_items_checked_with_kind(get_hash_kind(), items)
    }

    pub fn tree_from_items_checked_with_kind(
        kind: HashKind,
        mut items: Vec<TreeItem>,
    ) -> Result<Tree, GitError> {
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
        Tree::from_tree_items_with_kind(kind, items)
            .map_err(|_| GitError::CustomError("Invalid tree".to_string()))
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

        let kind = chain[0].id.kind();
        if leaf.id.kind() != kind || chain.iter().any(|tree| tree.id.kind() != kind) {
            return Err(GitError::CustomError("Invalid tree".to_string()));
        }

        let mut produced = Vec::new();
        produced.push(leaf.clone());
        let mut child_hash = leaf.id;
        let mut child_name = missing_components[missing_components.len() - 1];

        // Build brand-new intermediate trees for all but the first missing component
        // (the first missing attaches into the deepest existing tree).
        for name in missing_components.iter().rev().skip(1) {
            let intermediate = Self::tree_from_items_checked_with_kind(
                kind,
                vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: child_hash,
                    name: child_name.to_owned(),
                }],
            )?;
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
        // Unsigned synthesized commit: frame the header/body blank line once
        // (plan-20260923 ADR-FU-02).
        let commit_msg = format_commit_msg(commit_msg, None);
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
                    &commit_msg,
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
