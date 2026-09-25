use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use git_internal::{
    errors::GitError,
    hash::{ObjectHash, get_hash_kind},
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::{
            commit::Commit,
            tree::{Tree, TreeItem},
        },
        pack::entry::Entry,
    },
};
use sea_orm::{DatabaseTransaction, TransactionTrait};

use crate::{
    callisto::{git_tag, import_refs},
    ceres::{
        api_service::{ApiHandler, cache::GitObjectCache, history},
        model::{
            git::{
                CreateEntryInfo, CreateEntryResult, DeleteEntryInfo, DeleteEntryResult,
                EditFilePayload, EditFileResult, MoveEntryInfo, MoveEntryResult,
            },
            tag::TagInfo,
        },
        protocol::repo::Repo,
    },
    common::{
        errors::{ImportRepoError, MegaError},
        utils::format_commit_msg,
    },
    contract::api::common::Pagination,
    jupiter::{
        storage::{Storage, base_storage::StorageConnector},
        utils::converter::FromGitModel,
    },
};

#[derive(Clone)]
pub struct ImportApiService {
    pub storage: Storage,
    pub repo: Repo,
    pub git_object_cache: Arc<GitObjectCache>,
}

#[async_trait]
impl ApiHandler for ImportApiService {
    fn get_context(&self) -> Storage {
        self.storage.clone()
    }

    fn object_cache(&self) -> &GitObjectCache {
        &self.git_object_cache
    }

    async fn create_monorepo_entry(
        &self,
        _: CreateEntryInfo,
        _: Option<String>,
    ) -> Result<CreateEntryResult, GitError> {
        Err(GitError::CustomError(
            "import dir does not support create entry".to_string(),
        ))
    }

    /// ImportRepo directories keep Git semantics; the product directory API
    /// refuses them with a diagnosable 409 (plan-20260917 ADR-LB-06). The
    /// `[code:409]` prefix is what `From<E> for ApiError` maps to CONFLICT.
    async fn delete_monorepo_entry(
        &self,
        _: DeleteEntryInfo,
        _: Option<String>,
    ) -> Result<DeleteEntryResult, GitError> {
        Err(GitError::CustomError(
            "[code:409] import dir does not support delete entry".to_string(),
        ))
    }

    /// Same refusal for moves whose source lives under an ImportRepo; the
    /// monorepo handler refuses destinations under one on its own.
    async fn move_monorepo_entry(
        &self,
        _: MoveEntryInfo,
        _: Option<String>,
    ) -> Result<MoveEntryResult, GitError> {
        Err(GitError::CustomError(
            "[code:409] import dir does not support move entry".to_string(),
        ))
    }

    fn strip_relative(&self, path: &Path) -> Result<PathBuf, MegaError> {
        let path_str = path.to_string_lossy();

        // If path is truly relative (no leading slash and not absolute), return as-is
        if !path_str.starts_with('/') && !path.is_absolute() {
            // Check if it doesn't start with repo_path either
            let repo_trimmed = self.repo.repo_path.trim_start_matches('/');
            if !path_str.starts_with(repo_trimmed) {
                tracing::debug!("strip_relative -> path is already relative: {:?}", path);
                return Ok(path.to_path_buf());
            }
        }

        // Normalize both paths by removing leading slashes for consistent comparison
        let path_trimmed = path_str.trim_start_matches('/');
        let repo_trimmed = self.repo.repo_path.trim_start_matches('/');

        let path_normalized = Path::new(path_trimmed);
        let repo_normalized = Path::new(repo_trimmed);

        match path_normalized.strip_prefix(repo_normalized) {
            Ok(relative_path) => {
                tracing::debug!("strip_relative -> relative={:?}", relative_path);
                Ok(relative_path.to_path_buf())
            }
            Err(_) => Err(MegaError::Other(format!(
                "Path '{}' is not under repo '{}'",
                path.display(),
                self.repo.repo_path
            ))),
        }
    }

    async fn get_root_commit(&self) -> Result<Commit, MegaError> {
        let storage = self.storage.git_db_storage();
        let refs = storage.get_default_ref(self.repo.repo_id).await?.unwrap();
        self.get_commit_by_hash(&refs.ref_git_id).await
    }

    /// Note: The `refs` parameter is intentionally ignored for import repositories,
    /// as they do not support selecting refs. The default ref is always used.
    async fn get_root_tree(&self, _: Option<&str>) -> Result<Tree, MegaError> {
        let storage = self.storage.git_db_storage();
        let refs = storage
            .get_default_ref(self.repo.repo_id)
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!(
                    "default branch of ImportRepo {:?}",
                    self.repo.repo_path
                ))
            })?;
        let root_commit = storage
            .get_commit_by_hash(self.repo.repo_id, &refs.ref_git_id)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("commit {}", refs.ref_git_id)))?;
        Ok(Tree::from_git_model(
            storage
                .get_tree_by_hash(self.repo.repo_id, &root_commit.tree)
                .await?
                .ok_or_else(|| MegaError::NotFound(format!("tree {}", root_commit.tree)))?,
        ))
    }

    async fn get_tree_by_hash(&self, hash: &str) -> Result<Tree, MegaError> {
        let model = self
            .storage
            .git_db_storage()
            .get_tree_by_hash(self.repo.repo_id, hash)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("tree {hash}")))?;
        Ok(Tree::from_git_model(model))
    }

    async fn get_commit_by_hash(&self, hash: &str) -> Result<Commit, MegaError> {
        let storage = self.storage.git_db_storage();
        let commit = storage
            .get_commit_by_hash(self.repo.repo_id, hash)
            .await?
            .unwrap();
        Ok(Commit::from_git_model(commit))
    }

    async fn get_commits_by_hashes(&self, c_hashes: Vec<String>) -> Result<Vec<Commit>, GitError> {
        let storage = self.storage.git_db_storage();
        let commits = storage
            .get_commits_by_hashes(self.repo.repo_id, &c_hashes)
            .await
            .unwrap();
        Ok(commits.into_iter().map(Commit::from_git_model).collect())
    }

    async fn item_to_commit_map(
        &self,
        path: PathBuf,
        reference: Option<&str>,
    ) -> Result<HashMap<TreeItem, Option<Commit>>, GitError> {
        history::item_to_commit_map(self, path, reference).await
    }

    async fn create_tag(
        &self,
        _repo_path: Option<String>,
        name: String,
        target: Option<String>,
        tagger_name: Option<String>,
        tagger_email: Option<String>,
        message: Option<String>,
    ) -> Result<TagInfo, GitError> {
        let is_annotated = message.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
        let tagger_info = match (tagger_name, tagger_email) {
            (Some(n), Some(e)) => format!("{} <{}>", n, e),
            (Some(n), None) => n,
            (None, Some(e)) => e,
            (None, None) => "unknown".to_string(),
        };
        let full_ref = format!("refs/tags/{}", name.clone());
        if let Err(e) = self.check_new_tag(&name, target.as_ref(), &full_ref).await {
            return Err(self.removed_or(e).await);
        }
        if is_annotated {
            return self
                .create_annotated_tag(full_ref, name, target, tagger_info, message)
                .await;
        }

        // lightweight
        self.create_lightweight_tag(full_ref, name, target, tagger_info)
            .await
    }

    async fn list_tags(
        &self,
        _repo_path: Option<String>,
        pagination: Pagination,
    ) -> Result<(Vec<TagInfo>, u64), GitError> {
        let git_storage = self.storage.git_db_storage();
        // annotated tags: fetch paged annotated tags from storage
        let (annotated_tags_page, annotated_total) = match git_storage
            .list_tags_by_repo_with_page(self.repo.repo_id, pagination.clone())
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("DB error while listing git tags: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        };

        // map annotated page into TagInfo
        let mut result: Vec<TagInfo> = annotated_tags_page
            .into_iter()
            .map(|t| TagInfo {
                name: t.tag_name,
                tag_id: t.tag_id,
                object_id: t.object_id,
                object_type: t.object_type,
                tagger: t.tagger,
                message: t.message,
                created_at: t.created_at.and_utc().to_rfc3339(),
            })
            .collect();

        // lightweight refs
        let mut lightweight_refs: Vec<TagInfo> = vec![];
        if let Ok(refs) = git_storage.get_ref(self.repo.repo_id).await {
            for r in refs {
                if r.ref_name.starts_with("refs/tags/") {
                    let tag_name = r.ref_name.trim_start_matches("refs/tags/").to_string();
                    // skip if annotated exists (anywhere)
                    // Note: we only have the annotated page in memory; to avoid duplicate names we check by tag_name against annotated page and will accept duplicates only if not present.
                    if result.iter().any(|t| t.name == tag_name) {
                        continue;
                    }
                    let created_at = r.created_at.and_utc().to_rfc3339();
                    lightweight_refs.push(TagInfo {
                        name: tag_name.clone(),
                        tag_id: r.ref_git_id.clone(),
                        object_id: r.ref_git_id.clone(),
                        object_type: "commit".to_string(),
                        tagger: "".to_string(),
                        message: "".to_string(),
                        created_at,
                    });
                }
            }
        }

        // total is annotated_total + lightweight_refs.len()
        let total = annotated_total + lightweight_refs.len() as u64;

        // fill page: annotated page items come first, then lightweight refs to make up page size
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
        _repo_path: Option<String>,
        name: String,
    ) -> Result<Option<TagInfo>, GitError> {
        let git_storage = self.storage.git_db_storage();
        // annotated first: use jupiter git_storage helper
        match git_storage
            .get_tag_by_repo_and_name(self.repo.repo_id, &name)
            .await
        {
            Ok(Some(tag)) => {
                return Ok(Some(TagInfo {
                    name: tag.tag_name,
                    tag_id: tag.tag_id,
                    object_id: tag.object_id,
                    object_type: tag.object_type,
                    tagger: tag.tagger,
                    message: tag.message,
                    created_at: tag.created_at.and_utc().to_rfc3339(),
                }));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("DB error while getting git tag: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        }
        // check import_refs for lightweight
        let full_ref = format!("refs/tags/{}", name.clone());
        if let Ok(refs) = git_storage.get_ref(self.repo.repo_id).await {
            for r in refs {
                if r.ref_name == full_ref {
                    let created_at = r.created_at.and_utc().to_rfc3339();
                    return Ok(Some(TagInfo {
                        name: name.clone(),
                        tag_id: r.ref_git_id.clone(),
                        object_id: r.ref_git_id.clone(),
                        object_type: "commit".to_string(),
                        tagger: "".to_string(),
                        message: "".to_string(),
                        created_at,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn delete_tag(&self, _repo_path: Option<String>, name: String) -> Result<(), GitError> {
        // The ref and every git_tag row of the name in one fenced transaction
        // (none for a lightweight tag, and nothing at all for an absent name).
        let txn = self.begin_write().await?;
        let written = self.delete_tag_in_txn(&txn, &name).await;
        finish_write(txn, written).await
    }

    /// Save file edit for import repo path: the object rows through the fenced
    /// `save_entry`, then the default branch in its own fenced transaction
    /// (plan-20260923 ADR-FU-09 item 5, FU-19).
    async fn save_file_edit(
        &self,
        payload: EditFilePayload,
        _: Option<String>,
    ) -> Result<EditFileResult, GitError> {
        let edit = match self.plan_file_edit(&payload).await {
            Ok(edit) => edit,
            Err(e) => return Err(self.removed_or(e).await),
        };
        self.storage
            .import_service
            .save_entry(self.repo.repo_id, &self.repo.repo_path, edit.entries)
            .await?;
        self.write_default_ref(&edit.ref_name, &edit.commit_id)
            .await?;
        Ok(EditFileResult {
            commit_id: edit.commit_id,
            new_oid: edit.blob_id,
            path: payload.path,
            cl_link: None,
        })
    }
}

/// The rows of one ImportRepo file edit, computed without writing anything.
struct FileEdit {
    entries: Vec<MetaAttached<Entry, EntryMeta>>,
    ref_name: String,
    commit_id: String,
    blob_id: String,
}

/// Commit `txn` when `written` is `Ok`, roll it back otherwise.
async fn finish_write<T>(
    txn: DatabaseTransaction,
    written: Result<T, GitError>,
) -> Result<T, GitError> {
    match written {
        Ok(value) => {
            txn.commit()
                .await
                .map_err(|e| GitError::from(MegaError::Db(e)))?;
            Ok(value)
        }
        Err(error) => {
            if let Err(e) = txn.rollback().await {
                tracing::warn!(error = %e, "rolling back an ImportRepo API write failed");
            }
            Err(error)
        }
    }
}

impl ImportApiService {
    /// The reads and object build of `save_file_edit`, with no writes.
    async fn plan_file_edit(&self, payload: &EditFilePayload) -> Result<FileEdit, GitError> {
        use git_internal::internal::object::{blob::Blob, tree::TreeItemMode};

        let path = PathBuf::from(&payload.path);
        let parent = path
            .parent()
            .ok_or_else(|| GitError::CustomError("Invalid file path".to_string()))?;
        let update_chain = self.search_tree_for_update(parent).await?;
        let parent_tree = update_chain
            .last()
            .cloned()
            .ok_or_else(|| GitError::CustomError("Parent tree not found".to_string()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| GitError::CustomError("Invalid file name".to_string()))?;
        let _current = parent_tree
            .tree_items
            .iter()
            .find(|x| x.name == name && x.mode == TreeItemMode::Blob)
            .ok_or_else(|| GitError::CustomError("[code:404] File not found".to_string()))?;

        // Create new blob and rebuild tree up to root
        let new_blob = Blob::from_content(&payload.content);
        let (updated_trees, new_root_id) =
            self.build_updated_trees(path.clone(), update_chain, new_blob.id)?;

        // The new commit's parent is the current default commit.
        let git_storage = self.storage.git_db_storage();
        let default_ref = git_storage
            .get_default_ref(self.repo.repo_id)
            .await?
            .ok_or_else(|| GitError::CustomError("Default ref not found".to_string()))?;
        let current_commit = git_storage
            .get_commit_by_hash(self.repo.repo_id, &default_ref.ref_git_id)
            .await?
            .ok_or(GitError::InvalidCommitObject)?;
        let parent_id = ObjectHash::from_hex_for_kind(get_hash_kind(), &current_commit.commit_id)
            .map_err(|_| GitError::InvalidCommitObject)?;
        let new_commit = Commit::from_tree_id(
            new_root_id,
            vec![parent_id],
            &format_commit_msg(&payload.commit_message, None),
        );

        let mut entries: Vec<MetaAttached<Entry, EntryMeta>> = Vec::new();
        for t in updated_trees.iter().cloned() {
            entries.push(MetaAttached {
                inner: Entry::from(t),
                meta: EntryMeta::new(),
            });
        }
        entries.push(MetaAttached {
            inner: Entry::from(new_blob.clone()),
            meta: EntryMeta::new(),
        });
        entries.push(MetaAttached {
            inner: Entry::from(new_commit.clone()),
            meta: EntryMeta::new(),
        });
        Ok(FileEdit {
            entries,
            ref_name: default_ref.ref_name,
            commit_id: new_commit.id.to_string(),
            blob_id: new_blob.id.to_string(),
        })
    }

    /// `IMPORT_REPO_REMOVED` when the repository this handler was dispatched
    /// to is gone (or re-imported under another id), else `error`.
    async fn removed_or(&self, error: GitError) -> GitError {
        match self
            .storage
            .git_db_storage()
            .find_git_repo_exact_match(&self.repo.repo_path)
            .await
        {
            Ok(Some(row)) if row.id == self.repo.repo_id => error,
            Ok(_) => MegaError::from(ImportRepoError::Removed {
                path: self.repo.repo_path.clone(),
            })
            .into(),
            Err(_) => error,
        }
    }

    /// A write transaction on the pool (READ COMMITTED).
    async fn begin_write(&self) -> Result<DatabaseTransaction, GitError> {
        self.storage
            .git_db_storage()
            .get_connection()
            .begin()
            .await
            .map_err(|e| GitError::from(MegaError::Db(e)))
    }

    /// The liveness lock of the repository this handler was dispatched to;
    /// a removed repository is `[code:409] IMPORT_REPO_REMOVED`.
    async fn lock_live(&self, txn: &DatabaseTransaction) -> Result<(), GitError> {
        self.storage
            .git_db_storage()
            .lock_live_import_repo(txn, self.repo.repo_id, &self.repo.repo_path)
            .await
            .map_err(GitError::from)
    }

    async fn write_default_ref_in_txn(
        &self,
        txn: &DatabaseTransaction,
        ref_name: &str,
        commit_id: &str,
    ) -> Result<(), GitError> {
        self.lock_live(txn).await?;
        self.storage
            .git_db_storage()
            .update_ref_in_txn(self.repo.repo_id, ref_name, commit_id, txn)
            .await
            .map_err(GitError::from)
    }

    async fn write_default_ref(&self, ref_name: &str, commit_id: &str) -> Result<(), GitError> {
        let txn = self.begin_write().await?;
        let written = self
            .write_default_ref_in_txn(&txn, ref_name, commit_id)
            .await;
        finish_write(txn, written).await
    }

    async fn write_annotated_tag_in_txn(
        &self,
        txn: &DatabaseTransaction,
        tag: git_tag::Model,
        tag_ref: import_refs::Model,
    ) -> Result<git_tag::Model, GitError> {
        self.lock_live(txn).await?;
        let git_storage = self.storage.git_db_storage();
        let saved = git_storage.insert_tag_in_txn(tag, txn).await.map_err(|e| {
            tracing::error!("DB insert error when creating annotated git tag: {}", e);
            GitError::CustomError("[code:500] DB insert error".to_string())
        })?;
        git_storage
            .save_ref_in_txn(self.repo.repo_id, tag_ref, txn)
            .await
            .map_err(|e| {
                tracing::error!("Failed to write import ref after DB insert: {}", e);
                GitError::CustomError("[code:500] Failed to write import ref".to_string())
            })?;
        Ok(saved)
    }

    async fn save_annotated_tag(
        &self,
        tag: git_tag::Model,
        tag_ref: import_refs::Model,
    ) -> Result<git_tag::Model, GitError> {
        let txn = self.begin_write().await?;
        let written = self.write_annotated_tag_in_txn(&txn, tag, tag_ref).await;
        finish_write(txn, written).await
    }

    async fn write_tag_ref_in_txn(
        &self,
        txn: &DatabaseTransaction,
        tag_ref: import_refs::Model,
    ) -> Result<(), GitError> {
        self.lock_live(txn).await?;
        self.storage
            .git_db_storage()
            .save_ref_in_txn(self.repo.repo_id, tag_ref, txn)
            .await
            .map_err(|e| {
                tracing::error!("Failed to write import ref for lightweight tag: {}", e);
                GitError::CustomError("[code:500] Failed to write import ref".to_string())
            })
    }

    async fn delete_tag_in_txn(
        &self,
        txn: &DatabaseTransaction,
        name: &str,
    ) -> Result<(), GitError> {
        self.lock_live(txn).await?;
        let git_storage = self.storage.git_db_storage();
        git_storage
            .remove_ref_in_txn(self.repo.repo_id, &format!("refs/tags/{name}"), txn)
            .await
            .map_err(|e| {
                tracing::error!("Failed to remove import ref when deleting tag: {}", e);
                GitError::CustomError("[code:500] Failed to remove import ref".to_string())
            })?;
        git_storage
            .delete_tag_in_txn(self.repo.repo_id, name, txn)
            .await
            .map_err(|e| {
                tracing::error!("DB delete error when deleting annotated git tag: {}", e);
                GitError::CustomError("[code:500] DB delete error".to_string())
            })
    }

    /// The `import_refs` row of a tag of this repository.
    fn tag_ref(&self, full_ref: String, object_id: String) -> import_refs::Model {
        let now = chrono::Utc::now().naive_utc();
        import_refs::Model {
            id: crate::common::utils::generate_id(),
            repo_id: self.repo.repo_id,
            ref_name: full_ref,
            ref_git_id: object_id,
            ref_type: crate::callisto::sea_orm_active_enums::RefTypeEnum::Tag,
            default_branch: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// The pre-checks of a new tag (target commit, name not taken), read on
    /// the pool before any write transaction.
    async fn check_new_tag(
        &self,
        name: &str,
        target: Option<&String>,
        full_ref: &str,
    ) -> Result<(), GitError> {
        self.validate_target_commit(target).await?;
        let git_storage = self.storage.git_db_storage();
        // Prevent duplicate tag/ref creation: check annotated table and refs first.
        match git_storage
            .get_tag_by_repo_and_name(self.repo.repo_id, name)
            .await
        {
            Ok(Some(_)) => {
                return Err(GitError::CustomError(format!(
                    "[code:400] Tag '{}' already exists",
                    name
                )));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("DB error while checking git_tag existence: {}", e);
                return Err(GitError::CustomError("[code:500] DB error".to_string()));
            }
        }
        if let Ok(refs) = git_storage.get_ref(self.repo.repo_id).await
            && refs.iter().any(|r| r.ref_name == full_ref)
        {
            return Err(GitError::CustomError(format!(
                "[code:400] Tag '{}' already exists",
                name
            )));
        }
        Ok(())
    }

    async fn create_annotated_tag(
        &self,
        full_ref: String,
        name: String,
        target: Option<String>,
        tagger_info: String,
        message: Option<String>,
    ) -> Result<TagInfo, GitError> {
        // build git_internal tag and models
        let (tag_id_hex, object_id) = self.build_git_internal_tag(
            name.clone(),
            target.clone(),
            tagger_info.clone(),
            message.clone(),
        )?;
        let new_model =
            self.build_git_tag_model(tag_id_hex, object_id.clone(), name, tagger_info, message);
        // The git_tag row and its ref in one fenced transaction.
        let saved = self
            .save_annotated_tag(new_model, self.tag_ref(full_ref, object_id))
            .await?;
        Ok(TagInfo {
            name: saved.tag_name,
            tag_id: saved.tag_id,
            object_id: saved.object_id,
            object_type: saved.object_type,
            tagger: saved.tagger,
            message: saved.message,
            created_at: saved.created_at.and_utc().to_rfc3339(),
        })
    }

    async fn create_lightweight_tag(
        &self,
        full_ref: String,
        name: String,
        target: Option<String>,
        tagger_info: String,
    ) -> Result<TagInfo, GitError> {
        let object_id = target.clone().unwrap_or_default();
        let import_ref = self.tag_ref(full_ref, object_id.clone());
        // Use ref creation time as lightweight tag created_at (capture before move)
        let created_at = import_ref.created_at.and_utc().to_rfc3339();
        let txn = self.begin_write().await?;
        let written = self.write_tag_ref_in_txn(&txn, import_ref).await;
        finish_write(txn, written).await?;
        Ok(TagInfo {
            name,
            tag_id: object_id.clone(),
            object_id,
            object_type: "commit".to_string(),
            tagger: tagger_info,
            message: String::new(),
            created_at,
        })
    }

    async fn validate_target_commit(&self, target: Option<&String>) -> Result<(), GitError> {
        if let Some(ref t) = target {
            let git_storage = self.storage.git_db_storage();
            match git_storage.get_commit_by_hash(self.repo.repo_id, t).await {
                Ok(c) => {
                    if c.is_none() {
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

    fn build_git_internal_tag(
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
        let git_internal_tag = git_internal::internal::object::tag::Tag::new(
            tag_target,
            git_internal::internal::object::types::ObjectType::Commit,
            name.clone(),
            git_internal::internal::object::signature::Signature::new(
                git_internal::internal::object::signature::SignatureType::Tagger,
                tagger_info.clone(),
                String::new(),
            ),
            message.clone().unwrap_or_default(),
        );
        Ok((
            git_internal_tag.id.to_string(),
            target.unwrap_or_else(|| "HEAD".to_string()),
        ))
    }

    fn build_git_tag_model(
        &self,
        tag_id_hex: String,
        object_id: String,
        name: String,
        tagger_info: String,
        message: Option<String>,
    ) -> git_tag::Model {
        git_tag::Model {
            id: crate::common::utils::generate_id(),
            repo_id: self.repo.repo_id,
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

    fn update_tree_hash(
        &self,
        tree: Arc<Tree>,
        name: &str,
        target_hash: ObjectHash,
    ) -> Result<Tree, GitError> {
        let index = tree
            .tree_items
            .iter()
            .position(|item| item.name == name)
            .ok_or_else(|| GitError::CustomError(format!("Tree item '{}' not found", name)))?;
        let mut items = tree.tree_items.clone();
        items[index].id = target_hash;
        Tree::from_tree_items(items).map_err(|_| GitError::CustomError("Invalid tree".to_string()))
    }

    /// Build updated trees chain and return (updated_trees, new_root_tree_id)
    fn build_updated_trees(
        &self,
        mut path: PathBuf,
        mut update_chain: Vec<Arc<Tree>>,
        mut updated_tree_hash: ObjectHash,
    ) -> Result<(Vec<Tree>, ObjectHash), GitError> {
        let mut updated_trees = Vec::new();
        while let Some(tree) = update_chain.pop() {
            let cloned_path = path.clone();
            let name = cloned_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| GitError::CustomError("Invalid path".into()))?;
            path.pop();

            let new_tree = self.update_tree_hash(tree, name, updated_tree_hash)?;
            updated_tree_hash = new_tree.id;
            updated_trees.push(new_tree);
        }
        Ok((updated_trees, updated_tree_hash))
    }
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse};
    use git_internal::internal::object::{
        blob::Blob,
        tree::{TreeItem, TreeItemMode},
    };
    use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{git_repo, sea_orm_active_enums::RefTypeEnum},
        ceres::pack::import_repo::{RemoveOutcome, detach_import_repo, remove_import_repo},
        common::errors::{ApiError, map_ceres_error},
        jupiter::storage::git_db_storage::fu18_support::{
            blocked_by_me, import_refs_count, object_counts, park_detach, single_connection,
            test_cache, wired_storage,
        },
    };

    /// A registered ImportRepo at `path` with README.md and src/lib.rs on
    /// `main`; its row and tip commit.
    async fn fu19_seed(storage: &Storage, path: &str) -> (git_repo::Model, String) {
        let model: git_repo::Model = Repo::new(PathBuf::from(path), false).unwrap().into();
        let git_db = storage.git_db_storage();
        git_db.register_import_repo(model.clone()).await.unwrap();
        let readme = Blob::from_content(&format!("fu19 {path}"));
        let lib = Blob::from_content("pub fn f() {}");
        let src = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: lib.id,
            name: "lib.rs".to_string(),
        }])
        .unwrap();
        let root = Tree::from_tree_items(vec![
            TreeItem {
                mode: TreeItemMode::Blob,
                id: readme.id,
                name: "README.md".to_string(),
            },
            TreeItem {
                mode: TreeItemMode::Tree,
                id: src.id,
                name: "src".to_string(),
            },
        ])
        .unwrap();
        let commit = Commit::from_tree_id(root.id, vec![], &format!("fu19 {path}"));
        storage
            .import_service
            .save_entry(
                model.id,
                path,
                [
                    Entry::from(readme),
                    Entry::from(lib),
                    Entry::from(src),
                    Entry::from(root),
                    Entry::from(commit.clone()),
                ]
                .into_iter()
                .map(|inner| MetaAttached {
                    inner,
                    meta: EntryMeta::new(),
                })
                .collect(),
            )
            .await
            .unwrap();
        let now = chrono::Utc::now().naive_utc();
        git_db
            .save_ref(
                model.id,
                import_refs::Model {
                    id: crate::common::utils::generate_id(),
                    repo_id: model.id,
                    ref_name: "refs/heads/main".to_string(),
                    ref_git_id: commit.id.to_string(),
                    ref_type: RefTypeEnum::Branch,
                    default_branch: true,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await
            .unwrap();
        (model, commit.id.to_string())
    }

    /// The handler the API dispatch builds for `file` (longest stored-path
    /// prefix, stored path kept).
    async fn fu19_handler(storage: &Storage, file: &str) -> ImportApiService {
        let model = storage
            .git_db_storage()
            .find_git_repo_like_path(file)
            .await
            .unwrap()
            .unwrap();
        fu19_handler_for(storage, model).await
    }

    async fn fu19_handler_for(storage: &Storage, model: git_repo::Model) -> ImportApiService {
        ImportApiService {
            storage: storage.clone(),
            repo: model.into(),
            git_object_cache: test_cache().await,
        }
    }

    fn fu19_edit(file: &str, content: &str) -> EditFilePayload {
        serde_json::from_value(serde_json::json!({
            "path": file,
            "content": content,
            "commit_message": content,
        }))
        .unwrap()
    }

    fn fu19_removed(path: &str) -> String {
        ImportRepoError::Removed {
            path: path.to_owned(),
        }
        .to_string()
    }

    fn fu19_refused(path: &str) -> String {
        format!("[code:409] {}", fu19_removed(path))
    }

    /// Status and `err_message` of an API error response.
    async fn fu19_wire(error: ApiError) -> (StatusCode, String) {
        let response = error.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (
            status,
            json["err_message"].as_str().unwrap_or_default().to_owned(),
        )
    }

    async fn fu19_detach(storage: &Storage, repo: &git_repo::Model) -> i64 {
        detach_import_repo(storage, test_cache().await, repo.id, &repo.repo_path, None)
            .await
            .unwrap()
            .expect("cleanup id")
    }

    async fn fu19_sweep(storage: &Storage, path: &str, cleanup: i64) {
        assert!(matches!(
            remove_import_repo(storage, test_cache().await, path, None, Some(cleanup))
                .await
                .unwrap(),
            RemoveOutcome::Removed { cleanup_id, .. } if cleanup_id == cleanup
        ));
    }

    async fn fu19_tag_ids<C: ConnectionTrait>(conn: &C, repo_id: i64, name: &str) -> Vec<String> {
        conn.query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT tag_id FROM git_tag WHERE repo_id = $1 AND tag_name = $2 ORDER BY tag_id",
            [repo_id.into(), name.into()],
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get("", "tag_id").unwrap())
        .collect()
    }

    async fn fu19_ref_ids<C: ConnectionTrait>(conn: &C, repo_id: i64, name: &str) -> Vec<i64> {
        conn.query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM import_refs WHERE repo_id = $1 AND ref_name = $2 ORDER BY id",
            [repo_id.into(), name.into()],
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get("", "id").unwrap())
        .collect()
    }

    async fn fu19_default_tip<C: ConnectionTrait>(conn: &C, repo_id: i64) -> String {
        conn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT ref_git_id FROM import_refs WHERE repo_id = $1 AND default_branch",
            [repo_id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "ref_git_id")
        .unwrap()
    }

    async fn fu19_exec<C: ConnectionTrait>(conn: &C, sql: &str, values: Vec<sea_orm::Value>) {
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fu19_edit_save_after_detach_rejected() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let p = "/third-party/fu19-edit";
        let file = "/third-party/fu19-edit/README.md";
        let (repo, _) = fu19_seed(&storage, p).await;
        let h = fu19_handler(&storage, file).await;

        // (a) Live: both fenced transactions run in sequence.
        let edited = h.save_file_edit(fu19_edit(file, "v2"), None).await.unwrap();
        assert_eq!(fu19_default_tip(&conn, repo.id).await, edited.commit_id);
        let counts = object_counts(&conn, repo.id).await;

        // (b) Detached, not swept: the pre-reads fail on the deleted rows and
        // the removal is reported; no panic, no row.
        let cleanup = fu19_detach(&storage, &repo).await;
        let refused = h
            .save_file_edit(fu19_edit(file, "v3"), None)
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), fu19_refused(p));
        assert_eq!(
            fu19_wire(ApiError::from(refused)).await,
            (StatusCode::CONFLICT, fu19_removed(p))
        );
        assert_eq!(object_counts(&conn, repo.id).await, counts);

        // (c) Swept.
        fu19_sweep(&storage, p, cleanup).await;
        let refused = h
            .save_file_edit(fu19_edit(file, "v4"), None)
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), fu19_refused(p));
        assert_eq!(object_counts(&conn, repo.id).await, [0, 0, 0, 0]);

        // (d) Re-imported at the same path: the stale handler is refused, a
        // handler for the new repository writes.
        let (again, _) = fu19_seed(&storage, p).await;
        assert_ne!(again.id, repo.id);
        let refused = h
            .save_file_edit(fu19_edit(file, "v5"), None)
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), fu19_refused(p));
        fu19_handler(&storage, file)
            .await
            .save_file_edit(fu19_edit(file, "v5"), None)
            .await
            .unwrap();

        // (e) Live controls: a missing tree, commit or default branch is a
        // 404, never REMOVED, never a panic.
        let q = "/third-party/fu19-edit-live";
        let (live, tip) = fu19_seed(&storage, q).await;
        let src_tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: Blob::from_content("pub fn f() {}").id,
            name: "lib.rs".to_string(),
        }])
        .unwrap()
        .id
        .to_string();
        fu19_exec(
            &conn,
            "DELETE FROM git_tree WHERE repo_id = $1 AND tree_id = $2",
            vec![live.id.into(), src_tree.clone().into()],
        )
        .await;
        let hq = fu19_handler(&storage, "/third-party/fu19-edit-live/src/lib.rs").await;
        let missing = hq
            .save_file_edit(
                fu19_edit("/third-party/fu19-edit-live/src/lib.rs", "x"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(missing.to_string(), format!("[code:404] tree {src_tree}"));
        assert_eq!(
            fu19_wire(ApiError::from(missing)).await,
            (StatusCode::NOT_FOUND, format!("tree {src_tree}"))
        );
        let root_tree = storage
            .git_db_storage()
            .get_commit_by_hash(live.id, &tip)
            .await
            .unwrap()
            .unwrap()
            .tree;
        fu19_exec(
            &conn,
            "DELETE FROM git_tree WHERE repo_id = $1 AND tree_id = $2",
            vec![live.id.into(), root_tree.clone().into()],
        )
        .await;
        let missing = hq
            .save_file_edit(
                fu19_edit("/third-party/fu19-edit-live/README.md", "x"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(missing.to_string(), format!("[code:404] tree {root_tree}"));
        assert_eq!(
            fu19_wire(ApiError::from(missing)).await,
            (StatusCode::NOT_FOUND, format!("tree {root_tree}"))
        );
        fu19_exec(
            &conn,
            "DELETE FROM git_commit WHERE repo_id = $1 AND commit_id = $2",
            vec![live.id.into(), tip.clone().into()],
        )
        .await;
        let missing = hq
            .save_file_edit(
                fu19_edit("/third-party/fu19-edit-live/README.md", "x"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(missing.to_string(), format!("[code:404] commit {tip}"));
        assert_eq!(
            fu19_wire(ApiError::from(missing)).await,
            (StatusCode::NOT_FOUND, format!("commit {tip}"))
        );
        let bare: git_repo::Model = Repo::new(PathBuf::from("/third-party/fu19-edit-bare"), false)
            .unwrap()
            .into();
        storage
            .git_db_storage()
            .register_import_repo(bare.clone())
            .await
            .unwrap();
        let missing = fu19_handler_for(&storage, bare)
            .await
            .save_file_edit(
                fu19_edit("/third-party/fu19-edit-bare/README.md", "x"),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            missing
                .to_string()
                .starts_with("[code:404] default branch of ImportRepo"),
            "{missing}"
        );
        assert_eq!(
            fu19_wire(ApiError::from(missing)).await.0,
            StatusCode::NOT_FOUND
        );

        // (f) A FU-15 alias pair: the API holds the stored path of the row it
        // resolved, so removing the canonical twin leaves the alias writable.
        let c = "/third-party/fu19-alias";
        let (canonical, _) = fu19_seed(&storage, c).await;
        let alias = git_repo::Model {
            id: crate::common::utils::generate_id(),
            repo_path: "/third-party/fu19-alias/".to_owned(),
            repo_name: "alias".to_owned(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };
        storage
            .git_db_storage()
            .save_git_repo(alias.clone())
            .await
            .unwrap();
        fu19_seed_objects(&storage, &alias).await;
        let alias_commits = object_counts(&conn, alias.id).await[0];
        let ha = fu19_handler(&storage, "/third-party/fu19-alias/README.md").await;
        assert_eq!(ha.repo.repo_id, alias.id);
        let hc = fu19_handler_for(&storage, canonical.clone()).await;
        assert!(matches!(
            remove_import_repo(&storage, test_cache().await, c, None, None)
                .await
                .unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == canonical.id
        ));
        ha.save_file_edit(
            fu19_edit("/third-party/fu19-alias/README.md", "alias"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(object_counts(&conn, alias.id).await[0], alias_commits + 1);
        assert_eq!(
            hc.save_file_edit(fu19_edit("/third-party/fu19-alias/README.md", "c"), None)
                .await
                .unwrap_err()
                .to_string(),
            fu19_refused(c)
        );
    }

    /// Objects and `main` for an already registered row.
    async fn fu19_seed_objects(storage: &Storage, model: &git_repo::Model) {
        let readme = Blob::from_content(&format!("fu19 {}", model.repo_path));
        let root = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: readme.id,
            name: "README.md".to_string(),
        }])
        .unwrap();
        let commit = Commit::from_tree_id(root.id, vec![], "fu19 alias");
        storage
            .import_service
            .save_entry(
                model.id,
                &model.repo_path,
                [
                    Entry::from(readme),
                    Entry::from(root),
                    Entry::from(commit.clone()),
                ]
                .into_iter()
                .map(|inner| MetaAttached {
                    inner,
                    meta: EntryMeta::new(),
                })
                .collect(),
            )
            .await
            .unwrap();
        let now = chrono::Utc::now().naive_utc();
        storage
            .git_db_storage()
            .save_ref(
                model.id,
                import_refs::Model {
                    id: crate::common::utils::generate_id(),
                    repo_id: model.id,
                    ref_name: "refs/heads/main".to_string(),
                    ref_git_id: commit.id.to_string(),
                    ref_type: RefTypeEnum::Branch,
                    default_branch: true,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fu19_tag_create_after_detach_rejected() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let p = "/third-party/fu19-tag-create";
        let (repo, c1) = fu19_seed(&storage, p).await;
        let h = fu19_handler(&storage, p).await;
        let tag = |name: &str, message: Option<&str>| {
            (
                name.to_owned(),
                Some(c1.clone()),
                message.map(str::to_owned),
            )
        };

        // Live: the returned TagInfo is the stored row (annotated) or the ref
        // (lightweight).
        let (name, target, message) = tag("v1", Some("one"));
        let annotated = h
            .create_tag(None, name, target, Some("fu19".into()), None, message)
            .await
            .unwrap();
        let stored = storage
            .git_db_storage()
            .get_tag_by_repo_and_name(repo.id, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                annotated.name,
                annotated.tag_id,
                annotated.object_id,
                annotated.object_type,
                annotated.tagger,
                annotated.message,
                annotated.created_at
            ),
            (
                stored.tag_name,
                stored.tag_id,
                stored.object_id,
                stored.object_type,
                stored.tagger,
                stored.message,
                stored.created_at.and_utc().to_rfc3339()
            )
        );
        let (name, target, message) = tag("l1", None);
        let light = h
            .create_tag(None, name, target, Some("fu19".into()), None, message)
            .await
            .unwrap();
        assert_eq!(
            (
                light.name.as_str(),
                light.tag_id.as_str(),
                light.object_id.as_str(),
                light.object_type.as_str(),
                light.tagger.as_str(),
                light.message.as_str()
            ),
            ("l1", c1.as_str(), c1.as_str(), "commit", "fu19", "")
        );
        let (name, target, message) = tag("v1", Some("again"));
        assert_eq!(
            h.create_tag(None, name, target, Some("fu19".into()), None, message)
                .await
                .unwrap_err()
                .to_string(),
            "[code:400] Tag 'v1' already exists"
        );
        assert_eq!(fu19_tag_ids(&conn, repo.id, "v1").await.len(), 1);
        assert_eq!(import_refs_count(&conn, repo.id).await, 3);

        // Detached, not swept: every create is refused, a taken name too.
        let cleanup = fu19_detach(&storage, &repo).await;
        let mut refused = Vec::new();
        for (name, target, message) in [
            tag("v2", Some("two")),
            tag("l2", None),
            tag("v1", Some("three")),
        ] {
            refused.push(
                h.create_tag(None, name, target, Some("fu19".into()), None, message)
                    .await
                    .unwrap_err(),
            );
        }
        for error in &refused {
            assert_eq!(error.to_string(), fu19_refused(p));
        }
        assert_eq!(
            fu19_wire(map_ceres_error(&refused[0], "Failed to create tag")).await,
            (StatusCode::CONFLICT, fu19_removed(p))
        );
        assert_eq!(object_counts(&conn, repo.id).await[3], 1, "only v1's row");
        assert!(fu19_tag_ids(&conn, repo.id, "v2").await.is_empty());
        assert_eq!(import_refs_count(&conn, repo.id).await, 0);

        // Swept: the missing target commit reads as REMOVED too.
        fu19_sweep(&storage, p, cleanup).await;
        for (name, target, message) in [tag("v3", Some("three")), tag("l3", None)] {
            assert_eq!(
                h.create_tag(None, name, target, Some("fu19".into()), None, message)
                    .await
                    .unwrap_err()
                    .to_string(),
                fu19_refused(p)
            );
        }
        assert_eq!(object_counts(&conn, repo.id).await, [0, 0, 0, 0]);
        assert_eq!(import_refs_count(&conn, repo.id).await, 0);

        // Re-imported at the same path: the stale handler is refused, a
        // handler for the new repository creates the same names.
        let (again, c2) = fu19_seed(&storage, p).await;
        assert_ne!(again.id, repo.id);
        let fresh = fu19_handler(&storage, p).await;
        for (name, message) in [("v4", Some("four")), ("l4", None)] {
            let message = message.map(str::to_owned);
            assert_eq!(
                h.create_tag(
                    None,
                    name.into(),
                    Some(c2.clone()),
                    Some("fu19".into()),
                    None,
                    message.clone()
                )
                .await
                .unwrap_err()
                .to_string(),
                fu19_refused(p)
            );
            fresh
                .create_tag(
                    None,
                    name.into(),
                    Some(c2.clone()),
                    Some("fu19".into()),
                    None,
                    message,
                )
                .await
                .unwrap();
        }
        assert_eq!(import_refs_count(&conn, again.id).await, 3);
        assert_eq!(object_counts(&conn, again.id).await[3], 1);
        assert_eq!(import_refs_count(&conn, repo.id).await, 0);

        // The other codes of the tag routes are unchanged.
        for (text, expected) in [
            (
                "[code:400] Tag 'x' already exists",
                (StatusCode::BAD_REQUEST, "Tag 'x' already exists"),
            ),
            (
                "[code:404] Tag not found",
                (StatusCode::NOT_FOUND, "Tag not found"),
            ),
            (
                "[code:500] DB error",
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
            ),
        ] {
            assert_eq!(
                fu19_wire(map_ceres_error(text, "ctx")).await,
                (expected.0, expected.1.to_owned())
            );
        }
    }

    #[tokio::test]
    async fn fu19_tag_delete_after_detach_rejected() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let p = "/third-party/fu19-tag-delete";
        let (repo, c1) = fu19_seed(&storage, p).await;
        let h = fu19_handler(&storage, p).await;
        let q = "/third-party/fu19-tag-other";
        let (other, oc1) = fu19_seed(&storage, q).await;
        let ho = fu19_handler(&storage, q).await;
        for (name, message) in [
            ("va", Some("a")),
            ("vl", None),
            ("vx", Some("x")),
            ("vy", None),
        ] {
            h.create_tag(
                None,
                name.into(),
                Some(c1.clone()),
                None,
                None,
                message.map(str::to_owned),
            )
            .await
            .unwrap();
            ho.create_tag(
                None,
                name.into(),
                Some(oc1.clone()),
                None,
                None,
                message.map(str::to_owned),
            )
            .await
            .unwrap();
        }
        // Live: the ref and the row go together, in this repository only; an
        // absent name is fine.
        for name in ["vx", "vy"] {
            h.delete_tag(None, name.into()).await.unwrap();
            assert!(fu19_tag_ids(&conn, repo.id, name).await.is_empty());
            assert!(
                fu19_ref_ids(&conn, repo.id, &format!("refs/tags/{name}"))
                    .await
                    .is_empty()
            );
            assert_eq!(
                fu19_ref_ids(&conn, other.id, &format!("refs/tags/{name}"))
                    .await
                    .len(),
                1,
                "{name}: the other repository keeps its ref"
            );
        }
        assert_eq!(
            fu19_tag_ids(&conn, other.id, "vx").await.len(),
            1,
            "the other repository keeps its git_tag row"
        );
        h.delete_tag(None, "absent".into()).await.unwrap();
        assert_eq!(fu19_tag_ids(&conn, repo.id, "va").await.len(), 1);

        // Detached: every delete is refused and changes nothing; the git_tag
        // row stays for the sweep.
        let cleanup = fu19_detach(&storage, &repo).await;
        let mut refused = Vec::new();
        for name in ["va", "vl", "never"] {
            refused.push(h.delete_tag(None, name.into()).await.unwrap_err());
        }
        for error in &refused {
            assert_eq!(error.to_string(), fu19_refused(p));
        }
        assert_eq!(
            fu19_wire(map_ceres_error(&refused[0], "Failed to delete tag")).await,
            (StatusCode::CONFLICT, fu19_removed(p))
        );
        assert_eq!(fu19_tag_ids(&conn, repo.id, "va").await.len(), 1);
        assert_eq!(import_refs_count(&conn, repo.id).await, 0);

        fu19_sweep(&storage, p, cleanup).await;
        assert_eq!(
            h.delete_tag(None, "va".into())
                .await
                .unwrap_err()
                .to_string(),
            fu19_refused(p)
        );
        assert_eq!(object_counts(&conn, repo.id).await, [0, 0, 0, 0]);

        // Re-imported at the same path: the stale handler cannot delete the
        // new repository's tag of the same name.
        let (again, c2) = fu19_seed(&storage, p).await;
        assert_ne!(again.id, repo.id);
        fu19_handler(&storage, p)
            .await
            .create_tag(
                None,
                "va".into(),
                Some(c2),
                None,
                None,
                Some("again".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            h.delete_tag(None, "va".into())
                .await
                .unwrap_err()
                .to_string(),
            fu19_refused(p)
        );
        assert_eq!(fu19_tag_ids(&conn, again.id, "va").await.len(), 1);
        assert_eq!(fu19_ref_ids(&conn, again.id, "refs/tags/va").await.len(), 1);
    }

    /// Autocommit `NOWAIT` probes of the row on `single`: the exclusive
    /// probe's error text and whether the share probe got the row.
    async fn fu19_probe(single: &sea_orm::DatabaseConnection, repo_id: i64) -> (String, bool) {
        let probe = |mode: &str| {
            single.query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("SELECT id FROM git_repo WHERE id = $1 {mode}"),
                [repo_id.into()],
            ))
        };
        let exclusive = match probe("FOR UPDATE NOWAIT").await {
            Ok(row) => format!("granted: {row:?}"),
            Err(error) => error.to_string(),
        };
        let shared = matches!(probe("FOR SHARE NOWAIT").await, Ok(Some(_)));
        (exclusive, shared)
    }

    #[tokio::test]
    async fn fu19_api_lock_serializes_with_detach() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let single = single_connection(&conn).await;

        // Part A: each API write transaction holds the share lock first and
        // writes on its own transaction; the removal waits for it and then
        // deletes its rows with the repository.
        for seam in ["tag", "light", "ref", "delete"] {
            let path = format!("/third-party/fu19-lock-{seam}");
            let (repo, c1) = fu19_seed(&storage, &path).await;
            let h = fu19_handler(&storage, &path).await;
            let plan = if seam == "ref" {
                let edit = h
                    .plan_file_edit(&fu19_edit(&format!("{path}/README.md"), "v2"))
                    .await
                    .unwrap();
                storage
                    .import_service
                    .save_entry(repo.id, &path, edit.entries)
                    .await
                    .unwrap();
                Some((edit.ref_name, edit.commit_id))
            } else {
                None
            };
            if seam == "delete" {
                h.create_tag(
                    None,
                    "va".into(),
                    Some(c1.clone()),
                    None,
                    None,
                    Some("a".into()),
                )
                .await
                .unwrap();
            }
            let held = conn.begin().await.unwrap();
            match seam {
                "tag" => {
                    let (tag_id, object) = h
                        .build_git_internal_tag(
                            "held".into(),
                            Some(c1.clone()),
                            "fu19".into(),
                            Some("held".into()),
                        )
                        .unwrap();
                    let model = h.build_git_tag_model(
                        tag_id,
                        object.clone(),
                        "held".into(),
                        "fu19".into(),
                        Some("held".into()),
                    );
                    h.write_annotated_tag_in_txn(
                        &held,
                        model,
                        h.tag_ref("refs/tags/held".into(), object),
                    )
                    .await
                    .unwrap();
                }
                "light" => h
                    .write_tag_ref_in_txn(&held, h.tag_ref("refs/tags/held".into(), c1.clone()))
                    .await
                    .unwrap(),
                "ref" => {
                    let (ref_name, commit_id) = plan.unwrap();
                    h.write_default_ref_in_txn(&held, &ref_name, &commit_id)
                        .await
                        .unwrap();
                }
                _ => h.delete_tag_in_txn(&held, "va").await.unwrap(),
            }
            // Nothing is visible outside the transaction before it commits.
            match seam {
                "tag" | "light" => {
                    assert!(fu19_tag_ids(&single, repo.id, "held").await.is_empty());
                    assert!(
                        fu19_ref_ids(&single, repo.id, "refs/tags/held")
                            .await
                            .is_empty(),
                        "{seam}: the ref is on the transaction"
                    );
                }
                "ref" => assert_eq!(
                    fu19_default_tip(&single, repo.id).await,
                    c1,
                    "ref: the move is on the transaction"
                ),
                _ => {
                    assert_eq!(fu19_tag_ids(&single, repo.id, "va").await.len(), 1);
                    assert_eq!(
                        fu19_ref_ids(&single, repo.id, "refs/tags/va").await.len(),
                        1,
                        "delete: both deletes are on the transaction"
                    );
                }
            }
            let (exclusive, shared) = fu19_probe(&single, repo.id).await;
            assert!(
                exclusive.contains("could not obtain lock"),
                "{seam}: the row is locked: {exclusive}"
            );
            assert!(shared, "{seam}: in share mode");
            let removal_storage = storage.clone();
            let removal_path = path.clone();
            let removal = tokio::spawn(async move {
                remove_import_repo(
                    &removal_storage,
                    test_cache().await,
                    &removal_path,
                    None,
                    None,
                )
                .await
            });
            assert!(
                blocked_by_me(&held, false).await,
                "{seam}: the removal waits"
            );
            held.commit().await.unwrap();
            assert!(matches!(
                removal.await.unwrap().unwrap(),
                RemoveOutcome::Removed { repo_id, .. } if repo_id == repo.id
            ));
            assert_eq!(import_refs_count(&conn, repo.id).await, 0, "{seam}");
            assert_eq!(object_counts(&conn, repo.id).await, [0, 0, 0, 0], "{seam}");
            assert!(
                storage
                    .git_db_storage()
                    .find_git_repo_exact_match(&path)
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        // Part B: the detach holds the repository row first (parked at its
        // import_refs delete); the production entry waits, then is refused.
        for (path, action) in [
            ("/third-party/fu19-lock-late", "create"),
            ("/third-party/fu19-lock-late-del", "delete"),
        ] {
            let (repo, c1) = fu19_seed(&storage, path).await;
            let h = fu19_handler(&storage, path).await;
            if action == "delete" {
                h.create_tag(
                    None,
                    "vb".into(),
                    Some(c1.clone()),
                    None,
                    None,
                    Some("b".into()),
                )
                .await
                .unwrap();
            }
            let before = object_counts(&conn, repo.id).await;
            let parked = park_detach(&single, repo.id).await;
            let detach_storage = storage.clone();
            let detach_repo = repo.clone();
            let detaching = tokio::spawn(async move {
                detach_import_repo(
                    &detach_storage,
                    test_cache().await,
                    detach_repo.id,
                    &detach_repo.repo_path,
                    None,
                )
                .await
            });
            assert!(
                blocked_by_me(&parked, false).await,
                "{action}: the detach parks"
            );
            let entry = tokio::spawn(async move {
                match action {
                    "create" => h
                        .create_tag(None, "late".into(), Some(c1), None, None, None)
                        .await
                        .map(|_| ()),
                    _ => h.delete_tag(None, "vb".into()).await,
                }
                .map_err(|e| e.to_string())
            });
            assert!(
                blocked_by_me(&parked, true).await,
                "{action}: the entry waits"
            );
            parked.commit().await.unwrap();
            assert!(detaching.await.unwrap().unwrap().is_some());
            assert_eq!(entry.await.unwrap().unwrap_err(), fu19_refused(path));
            assert_eq!(import_refs_count(&conn, repo.id).await, 0);
            assert_eq!(object_counts(&conn, repo.id).await, before);
            assert!(
                fu19_ref_ids(&conn, repo.id, "refs/tags/late")
                    .await
                    .is_empty()
            );
        }

        // Part C: the production edit/save reaches its default-ref write
        // holding the share lock (the ref row is held so the write waits).
        let path = "/third-party/fu19-lock-edit";
        let (repo, _) = fu19_seed(&storage, path).await;
        let h = fu19_handler(&storage, path).await;
        let seeded = object_counts(&conn, repo.id).await;
        let blocker = single.begin().await.unwrap();
        fu19_exec(
            &blocker,
            "SELECT id FROM import_refs WHERE repo_id = $1 AND default_branch FOR UPDATE",
            vec![repo.id.into()],
        )
        .await;
        let editing = tokio::spawn(async move {
            h.save_file_edit(
                fu19_edit("/third-party/fu19-lock-edit/README.md", "v2"),
                None,
            )
            .await
            .map(|edited| edited.commit_id)
            .map_err(|e| e.to_string())
        });
        assert!(blocked_by_me(&blocker, false).await, "the ref write waits");
        assert_eq!(
            object_counts(&conn, repo.id).await[0],
            seeded[0] + 1,
            "the object batch committed before the ref write"
        );
        let exclusive = conn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM git_repo WHERE id = $1 FOR UPDATE NOWAIT",
                [repo.id.into()],
            ))
            .await;
        assert!(
            matches!(&exclusive, Err(e) if e.to_string().contains("could not obtain lock")),
            "the waiting ref write holds the share lock: {exclusive:?}"
        );
        blocker.commit().await.unwrap();
        let commit_id = editing.await.unwrap().unwrap();
        assert_eq!(fu19_default_tip(&conn, repo.id).await, commit_id);
    }

    #[tokio::test]
    async fn fu19_edit_save_after_detach_writes_no_rows() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let single = single_connection(&conn).await;

        // W1: detached before the request.
        let p1 = "/third-party/fu19-rows-1";
        let (r1, _) = fu19_seed(&storage, p1).await;
        let h1 = fu19_handler(&storage, p1).await;
        let before = object_counts(&conn, r1.id).await;
        fu19_detach(&storage, &r1).await;
        assert_eq!(
            h1.save_file_edit(fu19_edit(&format!("{p1}/README.md"), "v2"), None)
                .await
                .unwrap_err()
                .to_string(),
            fu19_refused(p1)
        );
        assert_eq!(object_counts(&conn, r1.id).await, before);
        assert_eq!(import_refs_count(&conn, r1.id).await, 0);

        // W2: the detach holds the row; the pre-reads pass and the object
        // batch waits, then lands nothing.
        let p2 = "/third-party/fu19-rows-2";
        let (r2, _) = fu19_seed(&storage, p2).await;
        let h2 = fu19_handler(&storage, p2).await;
        let before = object_counts(&conn, r2.id).await;
        let parked = park_detach(&single, r2.id).await;
        let detach_storage = storage.clone();
        let detach_repo = r2.clone();
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                test_cache().await,
                detach_repo.id,
                &detach_repo.repo_path,
                None,
            )
            .await
        });
        assert!(blocked_by_me(&parked, false).await, "the detach parks");
        let editing = tokio::spawn(async move {
            h2.save_file_edit(
                fu19_edit("/third-party/fu19-rows-2/README.md", "late"),
                None,
            )
            .await
            .map_err(|e| e.to_string())
        });
        assert!(blocked_by_me(&parked, true).await, "the object batch waits");
        parked.commit().await.unwrap();
        assert!(detaching.await.unwrap().unwrap().is_some());
        assert_eq!(editing.await.unwrap().unwrap_err(), fu19_refused(p2));
        assert_eq!(object_counts(&conn, r2.id).await, before);
        let late_blob = Blob::from_content("late").id.to_string();
        assert_eq!(
            conn.query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM git_blob WHERE repo_id = $1 AND blob_id = $2",
                [r2.id.into(), late_blob.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "n")
            .unwrap(),
            0
        );
        assert_eq!(import_refs_count(&conn, r2.id).await, 0);

        // W3: removed between the object batch and the ref write.
        let p3 = "/third-party/fu19-rows-3";
        let (r3, _) = fu19_seed(&storage, p3).await;
        let h3 = fu19_handler(&storage, p3).await;
        let edit = h3
            .plan_file_edit(&fu19_edit(&format!("{p3}/README.md"), "v2"))
            .await
            .unwrap();
        storage
            .import_service
            .save_entry(r3.id, p3, edit.entries)
            .await
            .unwrap();
        assert!(matches!(
            remove_import_repo(&storage, test_cache().await, p3, None, None)
                .await
                .unwrap(),
            RemoveOutcome::Removed { .. }
        ));
        assert_eq!(
            h3.write_default_ref(&edit.ref_name, &edit.commit_id)
                .await
                .unwrap_err()
                .to_string(),
            fu19_refused(p3)
        );
        assert_eq!(import_refs_count(&conn, r3.id).await, 0);
        assert_eq!(object_counts(&conn, r3.id).await, [0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn fu19_tag_create_rolls_back_atomically() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let p = "/third-party/fu19-tag-atomic";
        let (repo, c1) = fu19_seed(&storage, p).await;
        let h = fu19_handler(&storage, p).await;

        // (i) The git_tag row and the ref are one transaction.
        let (tag_id, object) = h
            .build_git_internal_tag(
                "va".into(),
                Some(c1.clone()),
                "fu19".into(),
                Some("one".into()),
            )
            .unwrap();
        let held = conn.begin().await.unwrap();
        let saved = h
            .write_annotated_tag_in_txn(
                &held,
                h.build_git_tag_model(
                    tag_id.clone(),
                    object.clone(),
                    "va".into(),
                    "fu19".into(),
                    Some("one".into()),
                ),
                h.tag_ref("refs/tags/va".into(), object),
            )
            .await
            .unwrap();
        assert_eq!(saved.tag_id, tag_id);
        assert!(fu19_tag_ids(&conn, repo.id, "va").await.is_empty());
        assert!(
            fu19_ref_ids(&conn, repo.id, "refs/tags/va")
                .await
                .is_empty()
        );
        held.rollback().await.unwrap();
        assert!(fu19_tag_ids(&conn, repo.id, "va").await.is_empty());
        assert!(
            fu19_ref_ids(&conn, repo.id, "refs/tags/va")
                .await
                .is_empty()
        );

        // (ii) A loser of the name race: its ref write fails after its git_tag
        // insert; both roll back and the winner's row stays.
        h.create_tag(
            None,
            "v1".into(),
            Some(c1.clone()),
            None,
            None,
            Some("first".into()),
        )
        .await
        .unwrap();
        let winner = fu19_tag_ids(&conn, repo.id, "v1").await;
        let winner_ref = fu19_ref_ids(&conn, repo.id, "refs/tags/v1").await;
        assert_eq!(winner.len(), 1);
        let (loser_id, object) = h
            .build_git_internal_tag(
                "v1".into(),
                Some(c1),
                "unknown".into(),
                Some("second".into()),
            )
            .unwrap();
        assert_ne!(vec![loser_id.clone()], winner);
        let refused = h
            .save_annotated_tag(
                h.build_git_tag_model(
                    loser_id,
                    object.clone(),
                    "v1".into(),
                    "unknown".into(),
                    Some("second".into()),
                ),
                h.tag_ref("refs/tags/v1".into(), object),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), "[code:500] Failed to write import ref");
        assert_eq!(fu19_tag_ids(&conn, repo.id, "v1").await, winner);
        assert_eq!(
            fu19_ref_ids(&conn, repo.id, "refs/tags/v1").await,
            winner_ref
        );
    }
}
