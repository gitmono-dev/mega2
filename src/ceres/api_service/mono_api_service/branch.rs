use super::{LARGE_CL_RENAME_DETECTION_THRESHOLD, prelude::*};

impl super::MonoApiService {
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
    pub(in crate::ceres::api_service::mono_api_service) async fn detect_update_conflicts(
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
            .git_receive_pack_stream(&state, commands, Bytes::from(pack_data))
            .await
            .map_err(|e| MegaError::Other(format!("{e}")))?;

        Ok(bytes)
    }

    pub(in crate::ceres::api_service::mono_api_service) async fn traverse_tree(
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
}
