use super::{
    CreateEntryUpdate, MonoServiceLogic, TreeCommitInput, gitkeep_placeholder, prelude::*,
};
#[async_trait]
impl ApiHandler for super::MonoApiService {
    fn get_context(&self) -> Storage {
        self.storage.clone()
    }

    fn native_snapshot_projection(&self) -> bool {
        true
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
        self.guard_import_namespace(&[&file_path.to_string_lossy()], &[])?;
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
            self.resolve_trunk_land_path(&cl_root_path, &file_path.to_string_lossy(), false)
                .await?
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
            &format_commit_msg(&payload.commit_message, None),
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
            editor
                .trigger_check(self.storage.clone(), &username, &cl)
                .await?;
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
        self.guard_import_namespace(
            &[&Self::build_entry_path(&entry_info.path, &entry_info.name)],
            &[],
        )?;
        let CreateEntryUpdate {
            update_result,
            blob,
            entry_oid,
            repo_path,
            save_trees,
        } = self.prepare_create_entry_update(&entry_info).await?;
        let new_oid = entry_oid.to_string();
        let entry_path = Self::build_entry_path(&entry_info.path, &entry_info.name);
        let committed = self
            .commit_tree_update(
                TreeCommitInput {
                    update_result,
                    save_trees,
                    blobs: vec![blob],
                    repo_path,
                    policy_path: entry_path.clone(),
                    policy_is_dir: entry_info.is_directory,
                    expected_bases: None,
                    commit_msg: entry_info.commit_msg(),
                    author_username: entry_info.author_username.clone(),
                    skip_build: entry_info.skip_build,
                    mode: entry_info.mode.clone(),
                },
                requester,
            )
            .await;
        let outcome = committed?;
        if entry_info.is_directory && target_follows_landed_commit(&self.storage) {
            bind_if_still_landed(&self.storage, &entry_path).await?;
        }
        Ok(CreateEntryResult {
            commit_id: outcome.commit_id,
            new_oid,
            path: entry_path,
            cl_link: outcome.cl_link,
        })
    }

    /// Delete a directory or file: drop its item from the parent tree and
    /// commit the rewritten chain (plan-20260917 ADR-LB-02, plan-20260918
    /// ADR-FT-01). `is_directory` selects Tree vs Blob/BlobExecutable. Empty
    /// and non-empty directories share this one semantic. A parent left empty
    /// keeps a timestamped `.gitkeep` so it stays a valid (empty) directory —
    /// the same representation create-entry uses for a new empty directory.
    async fn delete_monorepo_entry(
        &self,
        entry_info: DeleteEntryInfo,
        requester: Option<String>,
    ) -> Result<DeleteEntryResult, GitError> {
        validate_entry_target(&entry_info.path, &entry_info.name)
            .map_err(|reason| GitError::CustomError(format!("[code:400] {reason}")))?;
        self.guard_import_namespace(
            &[],
            &[&Self::build_entry_path(&entry_info.path, &entry_info.name)],
        )?;
        let parent = PathBuf::from(if entry_info.path.is_empty() {
            "/"
        } else {
            entry_info.path.as_str()
        });
        let mut update_chain = self
            .load_parent_chain(
                &parent,
                |p| format!("[code:404] parent directory {} not found", p.display()),
                |p| format!("[code:400] parent path {} is not a directory", p.display()),
            )
            .await?;
        let parent_tree = update_chain
            .pop()
            .ok_or_else(|| GitError::CustomError("Empty update chain".to_string()))?;
        let mut items = parent_tree.tree_items.clone();
        // A tree may legally hold a blob and a tree of the same name
        // (create-entry's duplicate check is mode-scoped). Match the selected
        // mode; another mode with the same name is 400, a missing name is 404.
        let index = match items.iter().position(|item| {
            item.name == entry_info.name
                && if entry_info.is_directory {
                    item.mode == TreeItemMode::Tree
                } else {
                    matches!(item.mode, TreeItemMode::Blob | TreeItemMode::BlobExecutable)
                }
        }) {
            Some(index) => index,
            None if items.iter().any(|item| item.name == entry_info.name) => {
                let kind = if entry_info.is_directory {
                    "directory"
                } else {
                    "file"
                };
                return Err(GitError::CustomError(format!(
                    "[code:400] '{}' is not a {kind}",
                    entry_info.name
                )));
            }
            None => {
                return Err(GitError::CustomError(format!(
                    "[code:404] entry '{}' not found under {}",
                    entry_info.name,
                    parent.display()
                )));
            }
        };
        items.remove(index);

        let mut blobs = Vec::new();
        if items.is_empty() {
            let (keep, item) = gitkeep_placeholder();
            items.push(item);
            blobs.push(keep);
        }
        // Removing an item (or appending `.gitkeep` to a now-empty list)
        // keeps the stored order, so the new tree id is deterministic.
        let new_parent = Tree::from_tree_items(items)
            .map_err(|_| GitError::CustomError("Invalid tree".to_string()))?;
        let update_result =
            MonoServiceLogic::build_result_by_chain(parent.clone(), update_chain, new_parent.id)?;
        let entry_path = Self::build_entry_path(&entry_info.path, &entry_info.name);
        let retire_ids = if entry_info.is_directory && target_follows_landed_commit(&self.storage) {
            active_target_ids(&self.storage, &entry_path).await?
        } else {
            Vec::new()
        };
        let outcome = self
            .commit_tree_update(
                TreeCommitInput {
                    update_result,
                    save_trees: vec![new_parent],
                    blobs,
                    repo_path: parent,
                    policy_path: entry_path.clone(),
                    policy_is_dir: false,
                    expected_bases: None,
                    commit_msg: entry_info.commit_msg(),
                    author_username: entry_info.author_username.clone(),
                    skip_build: entry_info.skip_build,
                    mode: EditCLMode::TryReuse(None),
                },
                requester,
            )
            .await?;
        if !retire_ids.is_empty() {
            retire_recorded_targets(&self.storage, &retire_ids, &entry_path).await?;
        }
        Ok(DeleteEntryResult {
            commit_id: outcome.commit_id,
            path: entry_path,
            cl_link: outcome.cl_link,
        })
    }

    /// Move or rename a directory or file: the item leaves the source parent
    /// tree and the same oid and mode are inserted under the destination
    /// parent, both chains rolled up to the root in one commit
    /// (plan-20260917 ADR-LB-02/03, plan-20260918 ADR-FT-01). The commit
    /// lands on the deepest directory containing both parents; a source
    /// parent left empty keeps a `.gitkeep` like delete-entry.
    async fn move_monorepo_entry(
        &self,
        entry_info: MoveEntryInfo,
        requester: Option<String>,
    ) -> Result<MoveEntryResult, GitError> {
        for (path, name) in [
            (&entry_info.from_path, &entry_info.from_name),
            (&entry_info.to_path, &entry_info.to_name),
        ] {
            validate_entry_target(path, name)
                .map_err(|reason| GitError::CustomError(format!("[code:400] {reason}")))?;
        }
        let src_parent = normalize_parent_path(&entry_info.from_path);
        let dest_parent = normalize_parent_path(&entry_info.to_path);
        let src_full = join_entry_path(&src_parent, &entry_info.from_name);
        let dest_full = join_entry_path(&dest_parent, &entry_info.to_name);
        if src_full == dest_full {
            return Err(GitError::CustomError(format!(
                "[code:400] source and destination are the same: {src_full}"
            )));
        }
        if entry_info.is_directory
            && (dest_parent == src_full || dest_parent.starts_with(&format!("{src_full}/")))
        {
            return Err(GitError::CustomError(format!(
                "[code:400] cannot move {src_full} into its own subtree {dest_parent}"
            )));
        }
        // The router dispatches on the source path only; a destination under
        // an ImportRepo must be refused here (ADR-LB-06).
        let import_dir = self.storage.config().monorepo.import_dir.clone();
        let dest_parent_path = PathBuf::from(&dest_parent);
        if dest_parent_path.starts_with(&import_dir)
            && dest_parent_path != import_dir
            && self
                .storage
                .git_db_storage()
                .find_git_repo_like_path(&dest_parent)
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?
                .is_some()
        {
            return Err(GitError::CustomError(
                "[code:409] import dir does not support move entry".to_string(),
            ));
        }
        self.guard_import_namespace(&[&dest_full], &[&src_full])?;

        let src_parent_path = PathBuf::from(&src_parent);
        let mut src_chain = self
            .load_parent_chain(
                &src_parent_path,
                |p| format!("[code:404] source parent {} not found", p.display()),
                |p| {
                    format!(
                        "[code:400] source parent path {} is not a directory",
                        p.display()
                    )
                },
            )
            .await?;
        let src_tree = src_chain
            .pop()
            .ok_or_else(|| GitError::CustomError("Empty update chain".to_string()))?;
        let mut src_items = src_tree.tree_items.clone();
        let index = match src_items.iter().position(|item| {
            item.name == entry_info.from_name
                && if entry_info.is_directory {
                    item.mode == TreeItemMode::Tree
                } else {
                    matches!(item.mode, TreeItemMode::Blob | TreeItemMode::BlobExecutable)
                }
        }) {
            Some(index) => index,
            None if src_items
                .iter()
                .any(|item| item.name == entry_info.from_name) =>
            {
                let kind = if entry_info.is_directory {
                    "directory"
                } else {
                    "file"
                };
                return Err(GitError::CustomError(format!(
                    "[code:400] '{}' is not a {kind}",
                    entry_info.from_name
                )));
            }
            None => {
                return Err(GitError::CustomError(format!(
                    "[code:404] entry '{}' not found under {}",
                    entry_info.from_name, src_parent
                )));
            }
        };
        let moved = src_items.remove(index);

        let mut ancestors = vec![(src_parent_path.clone(), src_chain)];
        let mut edits: Vec<(PathBuf, Vec<TreeItem>)> = Vec::new();
        let mut blobs = Vec::new();
        let mut dest_items = if src_parent == dest_parent {
            // Rename: one parent tree carries both edits.
            src_items
        } else {
            let mut dest_chain = self
                .load_parent_chain(
                    &dest_parent_path,
                    |p| format!("[code:404] destination parent {} not found", p.display()),
                    |p| {
                        format!(
                            "[code:400] destination parent path {} is not a directory",
                            p.display()
                        )
                    },
                )
                .await?;
            let dest_tree = dest_chain
                .pop()
                .ok_or_else(|| GitError::CustomError("Empty update chain".to_string()))?;
            ancestors.push((dest_parent_path.clone(), dest_chain));
            if src_items.is_empty() {
                let (keep, item) = gitkeep_placeholder();
                src_items.push(item);
                blobs.push(keep);
            }
            edits.push((src_parent_path.clone(), src_items));
            dest_tree.tree_items.clone()
        };
        if dest_items
            .iter()
            .any(|item| item.name == entry_info.to_name)
        {
            return Err(GitError::CustomError(format!(
                "[code:400] '{}' already exists under {}",
                entry_info.to_name, dest_parent
            )));
        }
        dest_items.push(TreeItem {
            mode: moved.mode,
            id: moved.id,
            name: entry_info.to_name.clone(),
        });
        sort_git_tree_items(&mut dest_items);
        edits.push((dest_parent_path, dest_items));

        let update_result = Self::rewrite_parent_chains(ancestors, edits)?;
        let landing = common_parent_path(&src_parent, &dest_parent);
        let moved_ids = if entry_info.is_directory && target_follows_landed_commit(&self.storage) {
            move_native_directory(&self.storage, &src_full, &dest_full).await?
        } else {
            Vec::new()
        };
        let committed = self
            .commit_tree_update(
                TreeCommitInput {
                    update_result,
                    save_trees: Vec::new(),
                    blobs,
                    repo_path: PathBuf::from(landing),
                    policy_path: dest_full.clone(),
                    policy_is_dir: false,
                    expected_bases: None,
                    commit_msg: entry_info.commit_msg(),
                    author_username: entry_info.author_username.clone(),
                    skip_build: entry_info.skip_build,
                    mode: EditCLMode::TryReuse(None),
                },
                requester,
            )
            .await;
        let outcome = match committed {
            Ok(outcome) => {
                if !moved_ids.is_empty() {
                    restore_active_ids(&self.storage, &moved_ids).await?;
                }
                outcome
            }
            Err(err) => {
                if !moved_ids.is_empty() {
                    let reverted =
                        move_recorded_targets(&self.storage, &moved_ids, &dest_full, &src_full)
                            .await;
                    if let Err(revert_err) = reverted {
                        tracing::warn!(
                            error = %revert_err,
                            "native target move could not be restored after the directory commit failed"
                        );
                    }
                    let _ = restore_active_ids(&self.storage, &moved_ids).await;
                }
                return Err(err);
            }
        };
        Ok(MoveEntryResult {
            commit_id: outcome.commit_id,
            from_path: src_full,
            to_path: dest_full,
            cl_link: outcome.cl_link,
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
        let kind = self.storage.config().monorepo.object_hash_kind()?;
        if refs.len() == kind.hex_len() && refs.chars().all(|c| c.is_ascii_hexdigit()) {
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
        crate::jupiter::utils::converter::tree_from_mega_model_with_kind(
            model,
            self.storage.config().monorepo.object_hash_kind()?,
        )
    }

    async fn get_commit_by_hash(&self, hash: &str) -> Result<Commit, MegaError> {
        let model = self
            .storage
            .mono_storage()
            .get_commit_by_hash(hash)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("commit not found: {}", hash)))?;
        crate::jupiter::utils::converter::commit_from_mega_model_with_kind(
            model,
            self.storage.config().monorepo.object_hash_kind()?,
        )
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
        let path = crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref());

        // Prevent duplicate tag/ref creation on the same (path, name)
        match mono_storage.get_tag_by_path_and_name(path, &name).await {
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

        if let Ok(Some(_)) = mono_storage.get_ref(path, &full_ref).await {
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
        let repo_path = crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref());
        let (annotated_page, annotated_total) = match mono_storage
            .get_tags_by_page(pagination.clone(), repo_path)
            .await
        {
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
        let path = crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref());
        match mono_storage.get_tag_by_path_and_name(path, &name).await {
            Ok(Some(tag)) => return Ok(Some(self.tag_model_to_info(tag))),
            Ok(None) => {}
            Err(e) => {
                tracing::error!("DB error while getting tag: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        }
        let full_ref = format!("refs/tags/{}", name.clone());
        if let Ok(Some(r)) = mono_storage.get_ref(path, &full_ref).await {
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
        let path = crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref());
        match mono_storage.get_tag_by_path_and_name(path, &name).await {
            Ok(Some(_tag)) => {
                let full_ref = format!("refs/tags/{}", name.clone());
                if let Ok(Some(r)) = mono_storage.get_ref(path, &full_ref).await {
                    mono_storage.remove_ref(r).await.map_err(|e| {
                        tracing::error!("Failed to remove ref while deleting annotated tag: {}", e);
                        GitError::CustomError("[code:500] Failed to remove ref".to_string())
                    })?;
                }
                mono_storage
                    .delete_tag_by_path_and_name(path, &name)
                    .await
                    .map_err(|e| {
                        tracing::error!("DB delete error when deleting annotated tag: {}", e);
                        GitError::CustomError("[code:500] DB delete error".to_string())
                    })?;
                Ok(())
            }
            Ok(None) => {
                let full_ref = format!("refs/tags/{}", name.clone());
                if let Ok(Some(r)) = mono_storage.get_ref(path, &full_ref).await {
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

/// Trunk lands the directory on the ref inside this call. Review only opens a
/// change list, so the global target stays with the landed tree.
fn target_follows_landed_commit(storage: &Storage) -> bool {
    storage.config().monorepo.push_policy == PushPolicy::Trunk
}

async fn bind_new_native_directory(
    storage: &Storage,
    path: &str,
) -> Result<Option<String>, GitError> {
    let mono = storage.mono_storage();
    crate::jupiter::service::trace_target_service::bind_new_native_directory(
        mono.get_connection(),
        path,
    )
    .await
    .map_err(|err| GitError::CustomError(err.to_string()))
}

async fn move_native_directory(
    storage: &Storage,
    from: &str,
    to: &str,
) -> Result<Vec<String>, GitError> {
    let mono = storage.mono_storage();
    crate::jupiter::service::trace_target_service::move_covered_native_targets(
        mono.get_connection(),
        from,
        to,
    )
    .await
    .map_err(|err| GitError::CustomError(err.to_string()))
}

async fn bind_if_still_landed(storage: &Storage, path: &str) -> Result<(), GitError> {
    crate::jupiter::service::trace_target_service::bind_if_still_landed(
        &storage.mono_storage(),
        path,
    )
    .await
    .map_err(|err| GitError::CustomError(err.to_string()))
}

async fn restore_active_ids(storage: &Storage, ids: &[String]) -> Result<(), GitError> {
    crate::jupiter::service::trace_target_service::restore_active_ids(&storage.mono_storage(), ids)
        .await
        .map_err(|err| GitError::CustomError(err.to_string()))
}

async fn move_recorded_targets(
    storage: &Storage,
    ids: &[String],
    from: &str,
    to: &str,
) -> Result<(), GitError> {
    let mono = storage.mono_storage();
    crate::jupiter::storage::trace_target_storage::move_recorded_ids(
        mono.get_connection(),
        ids,
        from,
        to,
    )
    .await
    .map(|_| ())
    .map_err(|err| GitError::CustomError(err.to_string()))
}

async fn active_target_ids(storage: &Storage, path: &str) -> Result<Vec<String>, GitError> {
    let mono = storage.mono_storage();
    let rows = crate::jupiter::storage::trace_target_storage::active_at_and_below(
        mono.get_connection(),
        path,
    )
    .await
    .map_err(|err| GitError::CustomError(err.to_string()))?;
    Ok(rows.into_iter().map(|row| row.id).collect())
}

async fn retire_recorded_targets(
    storage: &Storage,
    ids: &[String],
    path: &str,
) -> Result<(), GitError> {
    let mono = storage.mono_storage();
    crate::jupiter::storage::trace_target_storage::deactivate_ids_still_under(
        mono.get_connection(),
        ids,
        path,
    )
    .await
    .map(|_| ())
    .map_err(|err| GitError::CustomError(err.to_string()))
}

#[cfg(test)]
mod tests {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    use super::super::MonoApiService;
    use crate::{
        callisto::trace_target,
        ceres::{
            api_service::ApiHandler,
            model::git::{CreateEntryInfo, DeleteEntryInfo, EditCLMode, MoveEntryInfo},
        },
        jupiter::storage::{
            base_storage::StorageConnector,
            git_db_storage::import_fence_support::{test_cache, wired_storage},
            trace_target_storage::active_at,
        },
    };

    async fn handler() -> (tempfile::TempDir, MonoApiService) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        let mut storage =
            crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
        let git_service = crate::jupiter::service::git_service::GitService {
            obj_storage: crate::jupiter::storage::object_storage::mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = crate::jupiter::service::mono_service::MonoService {
            mono_storage: storage.mono_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .expect("init monorepo");
        let storage = crate::jupiter::tests::with_test_vault(storage, temp.path()).await;
        let api = MonoApiService {
            storage,
            git_object_cache: test_cache().await,
        };
        (temp, api)
    }

    fn directory(path: &str, name: &str) -> CreateEntryInfo {
        CreateEntryInfo {
            is_directory: true,
            name: name.to_owned(),
            path: path.to_owned(),
            content: None,
            author_email: None,
            author_username: Some("synthetic".to_owned()),
            skip_build: true,
            mode: EditCLMode::TryReuse(None),
        }
    }

    #[tokio::test]
    async fn target_path_native_api_create_move_delete_and_rebuild() {
        let (_temp, api) = handler().await;
        let mono = api.storage.mono_storage();
        let conn = mono.get_connection();

        let created = api
            .create_monorepo_entry(directory("/project", "app"), None)
            .await
            .expect("create /project/app");
        assert_eq!(created.path, "/project/app");
        let child = api
            .create_monorepo_entry(directory("/project/app", "lib"), None)
            .await
            .expect("create /project/app/lib");
        assert_eq!(child.path, "/project/app/lib");
        let apple = api
            .create_monorepo_entry(directory("/project", "apple"), None)
            .await
            .expect("create /project/apple");
        assert_eq!(apple.path, "/project/apple");

        let app = active_at(conn, "/project/app")
            .await
            .expect("load app")
            .expect("app target");
        let lib = active_at(conn, "/project/app/lib")
            .await
            .expect("load lib")
            .expect("lib target");
        let apple_target = active_at(conn, "/project/apple")
            .await
            .expect("load apple")
            .expect("apple target");
        assert_ne!(app.id, lib.id);

        let moved = api
            .move_monorepo_entry(
                MoveEntryInfo {
                    from_path: "/project".to_owned(),
                    from_name: "app".to_owned(),
                    to_path: "/project".to_owned(),
                    to_name: "moved".to_owned(),
                    is_directory: true,
                    author_username: Some("synthetic".to_owned()),
                    skip_build: true,
                },
                None,
            )
            .await
            .expect("move app");
        assert_eq!(moved.from_path, "/project/app");
        assert_eq!(moved.to_path, "/project/moved");
        let moved_app = active_at(conn, "/project/moved")
            .await
            .expect("load moved")
            .expect("moved target");
        let moved_lib = active_at(conn, "/project/moved/lib")
            .await
            .expect("load moved lib")
            .expect("moved lib");
        assert_eq!(moved_app.id, app.id);
        assert_eq!(moved_lib.id, lib.id);
        assert!(
            active_at(conn, "/project/app")
                .await
                .expect("old path")
                .is_none()
        );
        assert_eq!(
            active_at(conn, "/project/apple")
                .await
                .expect("apple stays")
                .expect("apple")
                .id,
            apple_target.id
        );

        let deleted = api
            .delete_monorepo_entry(
                DeleteEntryInfo {
                    path: "/project".to_owned(),
                    name: "moved".to_owned(),
                    is_directory: true,
                    author_username: Some("synthetic".to_owned()),
                    skip_build: true,
                },
                None,
            )
            .await
            .expect("delete moved");
        assert_eq!(deleted.path, "/project/moved");
        let retired = trace_target::Entity::find()
            .filter(trace_target::Column::Id.is_in([app.id.clone(), lib.id.clone()]))
            .all(conn)
            .await
            .expect("retired rows");
        assert_eq!(retired.len(), 2);
        assert!(retired.iter().all(|row| !row.active));
        assert!(
            active_at(conn, "/project/apple")
                .await
                .expect("apple after delete")
                .is_some()
        );

        let rebuilt = api
            .create_monorepo_entry(directory("/project", "moved"), None)
            .await
            .expect("rebuild path");
        assert_eq!(rebuilt.path, "/project/moved");
        let again = active_at(conn, "/project/moved")
            .await
            .expect("rebuilt row")
            .expect("new target");
        assert_ne!(again.id, app.id);
        assert!(again.active);
    }

    #[tokio::test]
    async fn target_path_native_api_failed_delete_keeps_target() {
        let (_temp, api) = handler().await;
        let mono = api.storage.mono_storage();
        let conn = mono.get_connection();
        let bound = crate::jupiter::service::trace_target_service::bind_first_native_write(
            conn,
            "/project/kept",
        )
        .await
        .expect("seed target");

        let err = api
            .delete_monorepo_entry(
                DeleteEntryInfo {
                    path: "/missing-parent".to_owned(),
                    name: "child".to_owned(),
                    is_directory: true,
                    author_username: None,
                    skip_build: true,
                },
                None,
            )
            .await
            .expect_err("missing parent");
        assert!(err.to_string().contains("404"), "{err}");
        let still = active_at(conn, "/project/kept")
            .await
            .expect("load kept")
            .expect("kept target");
        assert_eq!(still.id, bound.id);
        assert!(still.active);
    }

    #[tokio::test]
    async fn target_path_native_api_unbound_parent_move_keeps_child() {
        let (_temp, api) = handler().await;
        let mono = api.storage.mono_storage();
        let conn = mono.get_connection();
        let created = api
            .create_monorepo_entry(directory("/project/nest", "lib"), None)
            .await
            .expect("create child under a directory this call does not bind");
        assert_eq!(created.path, "/project/nest/lib");
        let child = active_at(conn, "/project/nest/lib")
            .await
            .expect("child target")
            .expect("bound child");
        assert!(
            active_at(conn, "/project/nest")
                .await
                .expect("parent lookup")
                .is_none()
        );

        let moved = api
            .move_monorepo_entry(
                MoveEntryInfo {
                    from_path: "/project".to_owned(),
                    from_name: "nest".to_owned(),
                    to_path: "/project".to_owned(),
                    to_name: "renamed".to_owned(),
                    is_directory: true,
                    author_username: Some("synthetic".to_owned()),
                    skip_build: true,
                },
                None,
            )
            .await
            .expect("move unbound parent");
        assert_eq!(moved.to_path, "/project/renamed");
        let followed = active_at(conn, "/project/renamed/lib")
            .await
            .expect("followed")
            .expect("child followed");
        assert_eq!(followed.id, child.id);
        assert!(
            active_at(conn, "/project/nest/lib")
                .await
                .expect("old child")
                .is_none()
        );
    }

    #[tokio::test]
    async fn target_path_native_api_review_cl_does_not_change_targets() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = wired_storage(temp.path()).await;
        let api = MonoApiService {
            storage,
            git_object_cache: test_cache().await,
        };
        let mono = api.storage.mono_storage();
        let conn = mono.get_connection();
        let kept = crate::jupiter::service::trace_target_service::bind_first_native_write(
            conn,
            "/project/kept",
        )
        .await
        .expect("seed");
        let created = api
            .create_monorepo_entry(directory("/project", "app"), None)
            .await
            .expect("review create");
        assert_eq!(created.path, "/project/app");
        assert!(
            active_at(conn, "/project/app")
                .await
                .expect("review create binds nothing")
                .is_none()
        );
        assert_eq!(
            active_at(conn, "/project/kept")
                .await
                .expect("kept")
                .expect("seed stays")
                .id,
            kept.id
        );
    }
}
