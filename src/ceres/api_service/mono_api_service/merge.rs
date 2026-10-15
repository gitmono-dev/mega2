use super::{
    ApplyChangeContext, MonoServiceLogic, PreparedClUpdate, PushApplyArgs, RefUpdate,
    TreeUpdateResult, prelude::*,
};
impl super::MonoApiService {
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
    pub(in crate::ceres::api_service::mono_api_service) async fn ensure_merge_entry_prechecks(
        &self,
        cl: &mega_cl::Model,
    ) -> Result<(), GitError> {
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

    pub(in crate::ceres::api_service::mono_api_service) async fn queue_merge_defers_from_hash_conflict_to_b3(
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
    pub(in crate::ceres::api_service::mono_api_service) async fn merge_cl_via_queue(
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

    pub(in crate::ceres::api_service::mono_api_service) async fn follow_merge_queue(
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
    pub(in crate::ceres::api_service::mono_api_service) async fn build_signed_cl_update(
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

    pub(in crate::ceres::api_service::mono_api_service) fn apply_missing_path_update(
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

    pub(in crate::ceres::api_service::mono_api_service) fn update_parent_tree(
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

    pub(in crate::ceres::api_service::mono_api_service) fn record_tree(
        path: PathBuf,
        tree: &Tree,
        tree_cache: &mut HashMap<PathBuf, Tree>,
        new_trees: &mut HashMap<ObjectHash, Tree>,
    ) {
        tree_cache.insert(path, tree.clone());
        new_trees.insert(tree.id, tree.clone());
    }

    pub(in crate::ceres::api_service::mono_api_service) fn propagate_up(
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
    pub(in crate::ceres::api_service::mono_api_service) async fn apply_update_result_in_open_txn(
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

    pub(in crate::ceres::api_service::mono_api_service) async fn notify_authz_after_main_write(
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
    pub(in crate::ceres::api_service::mono_api_service) async fn prepare_signed_cl_update(
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
}
