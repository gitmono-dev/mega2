use super::{
    ApplyOutcome, MonoServiceLogic, ProvisionPlan, TreeCommitInput, prelude::*,
    provision_retry_reason,
};
pub(in crate::ceres::api_service::mono_api_service) fn collect_page_blobs(
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

#[async_trait]

impl super::PathProvisioner for super::MonoApiService {
    /// ADR-FU-05 item 1, plan half: import namespace guard and creation
    /// classification, then walk the tree to the first missing component. An
    /// existing component that is not a directory is `MONO_PATH_CONFLICT`.
    async fn plan_provision(&self, path: &str) -> Result<ProvisionPlan, GitError> {
        let config = self.storage.config();
        let monorepo = &config.monorepo;
        let policy = |err: PathPolicyError| GitError::from(MegaError::from(err));
        path_policy::check_write_operands(monorepo, &[path], &[]).map_err(policy)?;
        path_policy::classify_creation_path(monorepo, path).map_err(policy)?;
        let root = self
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await?
            .ok_or_else(|| GitError::CustomError("monorepo root ref is missing".to_owned()))?;
        let mut tree = self.get_tree_by_hash(&root.ref_tree_hash).await?;
        let mut prefix = String::new();
        for component in path[1..].split('/') {
            prefix.push('/');
            prefix.push_str(component);
            let dir = tree
                .tree_items
                .iter()
                .find(|item| item.name == component && item.mode == TreeItemMode::Tree)
                .map(|item| item.id.to_string());
            match dir {
                Some(id) => tree = self.get_tree_by_hash(&id).await?,
                None if tree.tree_items.iter().any(|item| item.name == component) => {
                    return Err(policy(PathPolicyError::Conflict {
                        path: path.to_owned(),
                        component: prefix,
                    }));
                }
                None => {
                    return Ok(ProvisionPlan {
                        path: path.to_owned(),
                        highest_new: Some(prefix),
                    });
                }
            }
        }
        Ok(ProvisionPlan {
            path: path.to_owned(),
            highest_new: None,
        })
    }

    /// ADR-FU-05 item 1, apply half: from one snapshot of the root tree,
    /// re-derive the highest missing component and build the new levels (the
    /// leaf holds a `.gitkeep`) in one commit landed with `commit_tree_update`
    /// (MonoWriteQueue on trunk). A snapshot whose highest new component
    /// differs from the authorized plan, or whose landing tip moved before
    /// landing, is a [`ApplyOutcome::Retry`] (nothing persisted in the first
    /// case); a same-name entry of any kind at a level to create is
    /// `MONO_PATH_CONFLICT`.
    async fn apply_provision(
        &self,
        plan: &ProvisionPlan,
        requester: Option<String>,
    ) -> Result<ApplyOutcome, GitError> {
        let policy = |err: PathPolicyError| GitError::from(MegaError::from(err));
        if plan.highest_new.is_none() {
            return Ok(ApplyOutcome::Retry("plan has nothing to create".to_owned()));
        }
        let root = self
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await?
            .ok_or_else(|| GitError::CustomError("monorepo root ref is missing".to_owned()))?;
        let components: Vec<&str> = plan.path[1..].split('/').collect();
        let mut chain = vec![Arc::new(self.get_tree_by_hash(&root.ref_tree_hash).await?)];
        let mut bases = HashMap::from([("/".to_owned(), chain[0].id)]);
        let mut deepest = String::new();
        let mut missing_from = components.len();
        for (index, component) in components.iter().enumerate() {
            let parent = chain
                .last()
                .cloned()
                .ok_or_else(|| GitError::CustomError("empty provisioning chain".to_owned()))?;
            let dir = parent
                .tree_items
                .iter()
                .find(|item| item.name == *component && item.mode == TreeItemMode::Tree)
                .map(|item| item.id.to_string());
            let path = format!("{deepest}/{component}");
            match dir {
                Some(id) => {
                    let tree = self.get_tree_by_hash(&id).await?;
                    bases.insert(path.clone(), tree.id);
                    chain.push(Arc::new(tree));
                    deepest = path;
                }
                None if parent.tree_items.iter().any(|item| item.name == *component) => {
                    return Err(policy(PathPolicyError::Conflict {
                        path: plan.path.clone(),
                        component: path,
                    }));
                }
                None => {
                    missing_from = index;
                    break;
                }
            }
        }
        let highest_new = components
            .get(missing_from)
            .map(|component| format!("{deepest}/{component}"));
        if highest_new != plan.highest_new {
            return Ok(ApplyOutcome::Retry(format!(
                "highest new component changed from {:?} to {highest_new:?}",
                plan.highest_new
            )));
        }

        // New levels, bottom-up: the leaf holds a `.gitkeep` whose content is
        // unique per attempt, so two concurrent rounds never build the same
        // commit (the queue replays an identical (old_id, new_id) pair).
        let blob = Blob::from_content(&format!(
            "This file was used to maintain the git tree, provisioned {} ({})",
            plan.path,
            uuid::Uuid::new_v4()
        ));
        let keep = TreeItem {
            mode: TreeItemMode::Blob,
            id: blob.id,
            name: String::from(".gitkeep"),
        };
        let mut child = Tree::from_tree_items(vec![keep])
            .map_err(|_| GitError::CustomError("Invalid tree".to_owned()))?;
        let mut save_trees = vec![child.clone()];
        for index in (missing_from..components.len() - 1).rev() {
            child = Tree::from_tree_items(vec![TreeItem {
                mode: TreeItemMode::Tree,
                id: child.id,
                name: components[index + 1].to_owned(),
            }])
            .map_err(|_| GitError::CustomError("Invalid tree".to_owned()))?;
            save_trees.push(child.clone());
        }
        let deepest_tree = chain
            .pop()
            .ok_or_else(|| GitError::CustomError("empty provisioning chain".to_owned()))?;
        let mut items = deepest_tree.tree_items.clone();
        items.push(TreeItem {
            mode: TreeItemMode::Tree,
            id: child.id,
            name: components[missing_from].to_owned(),
        });
        sort_git_tree_items(&mut items);
        let new_deepest = Tree::from_tree_items(items)
            .map_err(|_| GitError::CustomError("Invalid tree".to_owned()))?;
        let deepest_path = if deepest.is_empty() {
            "/".to_owned()
        } else {
            deepest
        };
        let update_result = MonoServiceLogic::build_result_by_chain(
            PathBuf::from(&deepest_path),
            chain,
            new_deepest.id,
        )?;
        save_trees.push(new_deepest);

        let landed = self
            .commit_tree_update(
                TreeCommitInput {
                    update_result,
                    save_trees,
                    blobs: vec![blob],
                    repo_path: PathBuf::from(&deepest_path),
                    policy_path: plan.path.clone(),
                    policy_is_dir: true,
                    expected_bases: Some(bases),
                    commit_msg: format!("provision {}", plan.path),
                    author_username: None,
                    skip_build: true,
                    mode: EditCLMode::TryReuse(None),
                },
                requester,
            )
            .await;
        match landed {
            Ok(outcome) => Ok(ApplyOutcome::Landed(outcome.commit_id)),
            Err(err) => match provision_retry_reason(&err) {
                Some(reason) => Ok(ApplyOutcome::Retry(reason)),
                None => Err(err),
            },
        }
    }
}
