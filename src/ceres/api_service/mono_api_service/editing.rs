use super::{
    CreateEntryUpdate, MonoServiceLogic, PROVISION_STALE_SNAPSHOT, RefUpdate, TreeCommitInput,
    TreeCommitOutcome, TreeUpdateResult, gitkeep_placeholder, logic::PATH_NOT_EXIST_RE, prelude::*,
};
impl super::MonoApiService {
    /// `search_tree_for_update` with the ADR-LB-03 error mapping: a missing
    /// component is the caller's 404, a component that names a blob (the walk
    /// then fails to load it as a tree: `tree not found: <hash>`) is the
    /// caller's 400; anything else propagates unchanged.
    pub(in crate::ceres::api_service::mono_api_service) async fn load_parent_chain(
        &self,
        parent: &Path,
        missing: impl Fn(&Path) -> String,
        not_a_directory: impl Fn(&Path) -> String,
    ) -> Result<Vec<Arc<Tree>>, GitError> {
        match self.search_tree_for_update(parent).await {
            Ok(chain) => Ok(chain),
            Err(err) if PATH_NOT_EXIST_RE.is_match(&err.to_string()) => {
                Err(GitError::CustomError(missing(parent)))
            }
            Err(err) if err.to_string().contains("tree not found:") => {
                Err(GitError::CustomError(not_a_directory(parent)))
            }
            Err(err) => Err(err),
        }
    }

    /// Rebuild edited parent trees and roll the new ids up every chain to the
    /// root (plan-20260917 LB-03). `ancestors` maps each edited parent path
    /// to its ancestor trees root-first (what `search_tree_for_update`
    /// returns minus the parent itself); `edits` are the new item lists of
    /// the edited parents. Paths are rebuilt deepest-first and each new id is
    /// written into the parent's item list before that parent is rebuilt, so
    /// a rename (one parent), sibling parents and ancestor/descendant parents
    /// all converge on a single new root.
    pub(in crate::ceres::api_service::mono_api_service) fn rewrite_parent_chains(
        ancestors: Vec<(PathBuf, Vec<Arc<Tree>>)>,
        edits: Vec<(PathBuf, Vec<TreeItem>)>,
    ) -> Result<TreeUpdateResult, GitError> {
        let mut known: HashMap<PathBuf, Arc<Tree>> = HashMap::new();
        for (parent, chain) in ancestors {
            let mut prefixes: Vec<PathBuf> =
                std::iter::successors(parent.parent().map(Path::to_path_buf), |p| {
                    p.parent().map(Path::to_path_buf)
                })
                .collect();
            prefixes.reverse();
            if prefixes.len() != chain.len() {
                return Err(GitError::CustomError(format!(
                    "update chain for {} has {} trees for {} ancestor levels",
                    parent.display(),
                    chain.len(),
                    prefixes.len()
                )));
            }
            for (path, tree) in prefixes.into_iter().zip(chain) {
                known.insert(path, tree);
            }
        }
        let mut pending: HashMap<PathBuf, Vec<TreeItem>> = edits.into_iter().collect();
        let mut updated_trees = Vec::new();
        let mut ref_updates = Vec::new();
        while let Some(path) = pending
            .keys()
            .max_by_key(|p| p.components().count())
            .cloned()
        {
            let Some(items) = pending.remove(&path) else {
                break;
            };
            let tree = Tree::from_tree_items(items)
                .map_err(|_| GitError::CustomError("Invalid tree".to_string()))?;
            let clean_path = MonoServiceLogic::clean_path_str(&path.to_string_lossy());
            let ref_path = if clean_path == "/" || clean_path.starts_with('/') {
                clean_path
            } else {
                format!("/{clean_path}")
            };
            ref_updates.push(RefUpdate {
                path: ref_path,
                tree_id: tree.id,
            });
            let Some(parent) = path.parent().map(Path::to_path_buf) else {
                updated_trees.push(tree);
                break;
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| GitError::CustomError("Invalid path".into()))?
                .to_owned();
            let mut parent_items = match pending.remove(&parent) {
                Some(items) => items,
                None => known
                    .get(&parent)
                    .ok_or_else(|| {
                        GitError::CustomError(format!(
                            "parent tree of {} was not loaded",
                            path.display()
                        ))
                    })?
                    .tree_items
                    .clone(),
            };
            let slot = parent_items
                .iter_mut()
                .find(|item| item.name == name)
                .ok_or_else(|| GitError::CustomError(format!("Tree item '{name}' not found")))?;
            slot.id = tree.id;
            pending.insert(parent, parent_items);
            updated_trees.push(tree);
        }
        Ok(TreeUpdateResult {
            updated_trees,
            ref_updates,
        })
    }

    /// Shared tail of the directory-change writes (create / delete / move):
    /// build one commit over the rewritten trees, persist the new objects, then
    /// advance the tip — `land_api_tip_push` under `push_policy=trunk`, the
    /// existing CL branch otherwise (ADR-LB-02 / ADR-LB-06). One
    /// implementation for every entry write (GC-02).
    pub(in crate::ceres::api_service::mono_api_service) async fn commit_tree_update(
        &self,
        input: TreeCommitInput,
        requester: Option<String>,
    ) -> Result<TreeCommitOutcome, GitError> {
        let TreeCommitInput {
            update_result,
            mut save_trees,
            blobs,
            repo_path,
            policy_path,
            policy_is_dir,
            expected_bases,
            commit_msg,
            author_username,
            skip_build,
            mode,
        } = input;
        let storage = self.storage.mono_storage();
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
                    "Failed to resolve build repo root for entry write, fallback to CL subtree root: {}",
                    e
                );
                repo_path_str.clone()
            }
        };

        let tip_path = if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            self.resolve_trunk_land_path(&repo_path_str, &policy_path, policy_is_dir)
                .await?
        } else {
            build_repo_path.clone()
        };

        let src_commit = edit_utils::get_repo_main_latest_commit(&self.storage, &tip_path).await?;
        if let Some(bases) = &expected_bases
            && bases.get(&tip_path) != Some(&src_commit.tree_id)
        {
            return Err(GitError::CustomError(PROVISION_STALE_SNAPSHOT.to_owned()));
        }
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
        let dst_commit = Commit::from_tree_id(
            target_tree_id,
            vec![base_commit],
            &format_commit_msg(&commit_msg, None),
        );
        let new_commit_id = dst_commit.id.to_string();

        let username = author_username.unwrap_or("Anonymous".to_string());

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
        if !blobs.is_empty() {
            self.storage
                .mono_service
                .save_blobs(&new_commit_id, blobs)
                .await?;
        }

        storage
            .batch_save_model(save_trees)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        self.storage
            .mono_service
            .mono_storage
            .save_mega_commits(vec![dst_commit], None)
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
            return Ok(TreeCommitOutcome {
                commit_id: landed,
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
            .find_or_create_cl_for_edit(&self.storage, &editor, mode, &new_commit_id, &username)
            .await?;

        if !skip_build {
            editor
                .trigger_check(self.storage.clone(), &username, &cl)
                .await?;
        }

        Ok(TreeCommitOutcome {
            commit_id: new_commit_id,
            cl_link: Some(cl.link),
        })
    }

    pub(in crate::ceres::api_service::mono_api_service) async fn prepare_create_entry_update(
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
                let (blob, tree_item) = gitkeep_placeholder();
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
                let (blob, tree_item) = gitkeep_placeholder();
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

    pub(in crate::ceres::api_service::mono_api_service) fn build_entry_path(
        path: &str,
        name: &str,
    ) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "/" {
            format!("/{name}")
        } else {
            format!("{trimmed}/{name}")
        }
    }

    pub(in crate::ceres::api_service::mono_api_service) fn ref_update_tree_id_for_path(
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
    pub(in crate::ceres::api_service::mono_api_service) async fn resolve_trunk_land_path(
        &self,
        write_path: &str,
        policy_path: &str,
        policy_is_dir: bool,
    ) -> Result<String, GitError> {
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
        self.fallback_trunk_land_path(write_path, policy_path, policy_is_dir)
            .await
    }

    /// ADR-FU-06: no non-root tip covers `write_path` (fresh stack). The
    /// first-level root of `policy_path` (the path the user wrote) is
    /// classified; errors name that path. Allowed writes land on the root,
    /// lazily materialized with the same primitive advertise uses. A write
    /// whose landing directory is `/` (new top-level entry, cross-root move,
    /// top-level delete) is `MONO_PATH_NOT_ALLOWED`: product writes never land
    /// on `/`. A root that is a strict ancestor of a nested `import_dir` is
    /// never materialized (GC-FU-04).
    pub(in crate::ceres::api_service::mono_api_service) async fn fallback_trunk_land_path(
        &self,
        write_path: &str,
        policy_path: &str,
        policy_is_dir: bool,
    ) -> Result<String, GitError> {
        let config = self.storage.config();
        let monorepo = &config.monorepo;
        let policy = |err: PathPolicyError| GitError::from(MegaError::from(err));
        let Some(target) = path_policy::operand_path(policy_path) else {
            return Err(policy(PathPolicyError::Invalid {
                path: policy_path.to_owned(),
                reason: "path must not contain '..' segments".to_owned(),
            }));
        };
        let first = target[1..].split('/').next().unwrap_or_default();
        if first.is_empty() {
            return Err(policy(path_policy::not_allowed(monorepo, &target)));
        }
        let root = format!("/{first}");
        // A top-level name that is not a configured root (including one no
        // root could have, e.g. with `\`) is outside the roots.
        path_policy::classify_creation_path(monorepo, &root)
            .map_err(|_| policy(path_policy::not_allowed(monorepo, &target)))?;
        let lands_under_root = path_policy::operand_path(write_path)
            .is_some_and(|landing| landing == root || landing.starts_with(&format!("{root}/")));
        if !lands_under_root {
            return Err(policy(path_policy::not_allowed(monorepo, "/")));
        }
        // Provisioning hint: the directory this write goes into.
        let provision_path = if policy_is_dir {
            target.clone()
        } else {
            target
                .rsplit_once('/')
                .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                .unwrap_or("/")
                .to_owned()
        };
        let uninitialized = || {
            policy(PathPolicyError::Uninitialized {
                path: provision_path.clone(),
            })
        };
        if path_policy::is_import_dir_ancestor(monorepo, &root) {
            return Err(uninitialized());
        }
        let refs = materialize::materialize_path_refs(&self.storage, &root).await?;
        if refs.iter().any(|r| r.ref_name == MEGA_BRANCH_NAME) {
            Ok(root)
        } else {
            Err(uninitialized())
        }
    }

    /// ADR-FU-06: product writes never rewrite the ImportRepo namespace —
    /// `import_dir` and below, and for removed operands also a strict ancestor
    /// of a nested `import_dir` — whether or not a repository is attached
    /// there (paths with a live ImportRepo are dispatched to
    /// `ImportApiService` before reaching this handler).
    pub(in crate::ceres::api_service::mono_api_service) fn guard_import_namespace(
        &self,
        written: &[&str],
        removed: &[&str],
    ) -> Result<(), GitError> {
        let config = self.storage.config();
        path_policy::check_write_operands(&config.monorepo, written, removed)
            .map_err(|err| GitError::from(MegaError::from(err)))
    }

    // helper to convert mega_tag model into TagInfo
    pub(in crate::ceres::api_service::mono_api_service) fn tag_model_to_info(
        &self,
        tag: mega_tag::Model,
    ) -> TagInfo {
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

    pub(in crate::ceres::api_service::mono_api_service) async fn create_annotated_tag_mono(
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
        let path_str =
            crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref()).to_string();
        let tag_model = self.build_mega_tag_model(
            tag_id_hex.clone(),
            object_id.clone(),
            name.clone(),
            path_str.clone(),
            tagger_info.clone(),
            message.clone(),
        );

        match mono_storage.insert_tag(tag_model).await {
            Ok(saved_tag) => {
                // Resolve tree hash from target commit so ref metadata is complete
                let tree_hash = self.resolve_tree_hash_for_commit(&object_id).await?;
                let refs =
                    mega_refs::Model::new(&path_str, full_ref.clone(), object_id, tree_hash, false);

                if let Err(e) = mono_storage.save_refs(refs, None).await {
                    // attempt to remove DB record
                    if let Err(del_e) = mono_storage
                        .delete_tag_by_path_and_name(&path_str, &name)
                        .await
                    {
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

    pub(in crate::ceres::api_service::mono_api_service) async fn create_lightweight_tag_mono(
        &self,
        repo_path: Option<String>,
        name: String,
        target: Option<String>,
        tagger_info: String,
        full_ref: String,
    ) -> Result<TagInfo, GitError> {
        let mono_storage = self.storage.mono_storage();

        let path_str =
            crate::ceres::model::tag::normalize_tag_selector_path(repo_path.as_deref()).to_string();
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
            .get_ref(&path_str, &full_ref)
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
    pub(in crate::ceres::api_service::mono_api_service) async fn resolve_tree_hash_for_commit(
        &self,
        commit_id: &str,
    ) -> Result<String, GitError> {
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
    pub(in crate::ceres::api_service::mono_api_service) async fn validate_target_commit_mono(
        &self,
        target: Option<&String>,
    ) -> Result<(), GitError> {
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

    pub(in crate::ceres::api_service::mono_api_service) fn build_git_internal_tag_mono(
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

    pub(in crate::ceres::api_service::mono_api_service) fn build_mega_tag_model(
        &self,
        tag_id_hex: String,
        object_id: String,
        name: String,
        path: String,
        tagger_info: String,
        message: Option<String>,
    ) -> mega_tag::Model {
        mega_tag::Model {
            id: crate::common::utils::generate_id(),
            tag_id: tag_id_hex,
            object_id,
            object_type: "commit".to_string(),
            tag_name: name,
            path,
            tagger: tagger_info,
            message: message.unwrap_or_default(),
            pack_id: String::new(),
            pack_offset: 0,
            created_at: chrono::Utc::now().naive_utc(),
        }
    }
}
