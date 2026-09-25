use std::{collections::BTreeMap, ops::Deref};

use futures::Stream;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    DbBackend, DbErr, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, Set, Statement, TransactionTrait,
    sea_query::{CaseStatement, Expr, ExprTrait, OnConflict},
};

use crate::{
    callisto::{
        git_blob, git_commit, git_repo, git_tag, git_tree, import_refs,
        import_repo_cleanups::{self, CleanupState},
        sea_orm_active_enums::RefTypeEnum,
    },
    common::{
        errors::{ImportRepoError, MegaError},
        utils::{canonicalize_mono_ref_path, escape_like, generate_id},
    },
    contract::api::common::Pagination,
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        mono_storage::MonoStorage,
    },
};

#[derive(Clone)]
pub struct GitDbStorage {
    pub base: BaseStorage,
}

impl Deref for GitDbStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl GitDbStorage {
    pub async fn create_repo_and_save_ref(
        &self,
        repo_path: &str,
        repo_name: &str,
        ref_name: &str,
        ref_id: &str,
    ) -> Result<(), MegaError> {
        // Make import repo creation idempotent:
        // - If repo_path already exists, reuse its repo_id
        // - Otherwise, create a new repo row
        let repo_id = if let Some(existing) = self.find_git_repo_exact_match(repo_path).await? {
            existing.id
        } else {
            let repo_id = generate_id();
            let repo = git_repo::Model {
                id: repo_id,
                repo_path: repo_path.to_string(),
                repo_name: repo_name.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
            };
            self.save_git_repo(repo).await?;
            repo_id
        };

        let refs = import_refs::Model {
            id: generate_id(),
            repo_id: 0,
            ref_name: ref_name.to_string(),
            ref_git_id: ref_id.to_string(),
            ref_type: RefTypeEnum::Branch,
            default_branch: true,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };
        // If ref exists, update it; otherwise insert.
        let existing_ref = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .one(self.get_connection())
            .await?;
        if existing_ref.is_some() {
            self.update_ref(repo_id, ref_name, ref_id).await?;
        } else {
            self.save_ref(repo_id, refs).await?;
        }
        Ok(())
    }

    pub async fn save_ref(
        &self,
        repo_id: i64,
        mut refs: import_refs::Model,
    ) -> Result<(), MegaError> {
        refs.repo_id = repo_id;
        let a_model = refs.into_active_model();
        import_refs::Entity::insert(a_model)
            .exec(self.get_connection())
            .await
            .map_err(|e| MegaError::Other(format!("Failed to insert import_refs: {e}")))?;
        Ok(())
    }

    pub async fn remove_ref(&self, repo_id: i64, ref_name: &str) -> Result<(), MegaError> {
        import_refs::Entity::delete_many()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn get_ref(&self, repo_id: i64) -> Result<Vec<import_refs::Model>, MegaError> {
        let result = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .order_by_asc(import_refs::Column::RefName)
            .all(self.get_connection())
            .await?;
        Ok(result)
    }

    /// The refs of `repo_id` named in `names` (at most one row per name, by
    /// the unique index); bounded by the caller's command count.
    pub async fn get_refs_by_names(
        &self,
        repo_id: i64,
        names: &[String],
    ) -> Result<Vec<import_refs::Model>, MegaError> {
        Ok(import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.is_in(names.iter().cloned()))
            .all(self.get_connection())
            .await?)
    }

    pub async fn update_ref(
        &self,
        repo_id: i64,
        ref_name: &str,
        new_id: &str,
    ) -> Result<(), MegaError> {
        let ref_data: import_refs::Model = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .one(self.get_connection())
            .await
            .unwrap()
            .unwrap();
        let mut ref_data: import_refs::ActiveModel = ref_data.into();
        ref_data.ref_git_id = Set(new_id.to_string());
        ref_data.updated_at = Set(chrono::Utc::now().naive_utc());
        ref_data.update(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn save_ref_in_txn(
        &self,
        repo_id: i64,
        mut refs: import_refs::Model,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        refs.repo_id = repo_id;
        let conn = self.build_connection_with_txn(Some(txn));
        import_refs::Entity::insert(refs.into_active_model())
            .exec(&conn)
            .await
            .map_err(|e| MegaError::Other(format!("Failed to insert import_refs: {e}")))?;
        Ok(())
    }

    pub async fn remove_ref_in_txn(
        &self,
        repo_id: i64,
        ref_name: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        import_refs::Entity::delete_many()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .exec(txn)
            .await?;
        Ok(())
    }

    /// Deletes the ref only if it still points at `expected_git_id`.
    /// Returns whether a row was removed. A concurrent push that moved the
    /// ref leaves it intact (receive-pack advertised-old-id lease).
    pub async fn remove_ref_if_unchanged<C: ConnectionTrait>(
        &self,
        repo_id: i64,
        ref_name: &str,
        expected_git_id: &str,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let result = import_refs::Entity::delete_many()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .filter(import_refs::Column::RefGitId.eq(expected_git_id))
            .exec(conn)
            .await?;
        Ok(result.rows_affected > 0)
    }

    /// Inserts the ref only if `(repo_id, ref_name)` does not exist yet
    /// (receive-pack `Create`, plan-20260923 ADR-FU-08 item 2). Returns
    /// whether a row was inserted; a conflict leaves the transaction usable.
    pub async fn create_ref_if_absent<C: ConnectionTrait>(
        &self,
        repo_id: i64,
        mut refs: import_refs::Model,
        conn: &C,
    ) -> Result<bool, MegaError> {
        refs.repo_id = repo_id;
        let inserted = import_refs::Entity::insert(refs.into_active_model())
            .on_conflict(
                OnConflict::columns([import_refs::Column::RepoId, import_refs::Column::RefName])
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(conn)
            .await?;
        Ok(inserted > 0)
    }

    /// Moves the ref to `new_id` only if it still points at
    /// `expected_git_id` (receive-pack `Update`). Returns whether a row was
    /// updated.
    pub async fn update_ref_if_unchanged<C: ConnectionTrait>(
        &self,
        repo_id: i64,
        ref_name: &str,
        expected_git_id: &str,
        new_id: &str,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let result = import_refs::Entity::update_many()
            .col_expr(import_refs::Column::RefGitId, Expr::value(new_id))
            .col_expr(
                import_refs::Column::UpdatedAt,
                Expr::value(chrono::Utc::now().naive_utc()),
            )
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .filter(import_refs::Column::RefGitId.eq(expected_git_id))
            .exec(conn)
            .await?;
        Ok(result.rows_affected > 0)
    }

    pub async fn update_ref_in_txn(
        &self,
        repo_id: i64,
        ref_name: &str,
        new_id: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        let ref_data = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .one(txn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("import_refs not found: {ref_name}")))?;
        let mut active: import_refs::ActiveModel = ref_data.into();
        active.ref_git_id = Set(new_id.to_string());
        active.updated_at = Set(chrono::Utc::now().naive_utc());
        active.update(txn).await?;
        Ok(())
    }

    /// Mark `ref_name` as the sole default branch for `repo_id` (clears peers).
    pub async fn set_default_branch_in_txn(
        &self,
        repo_id: i64,
        ref_name: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        use sea_orm::sea_query::Expr;
        import_refs::Entity::update_many()
            .col_expr(import_refs::Column::DefaultBranch, Expr::value(false))
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .exec(txn)
            .await?;
        let ref_data = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefName.eq(ref_name))
            .one(txn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("import_refs not found: {ref_name}")))?;
        let mut active: import_refs::ActiveModel = ref_data.into();
        active.default_branch = Set(true);
        active.updated_at = Set(chrono::Utc::now().naive_utc());
        active.update(txn).await?;
        Ok(())
    }

    pub async fn get_default_ref(
        &self,
        repo_id: i64,
    ) -> Result<Option<import_refs::Model>, MegaError> {
        let result = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::DefaultBranch.eq(true))
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    pub async fn default_branch_exist(&self, repo_id: i64) -> Result<bool, MegaError> {
        let result = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::DefaultBranch.eq(true))
            .count(self.get_connection())
            .await?;
        Ok(result > 0)
    }

    pub async fn default_branch_exist_in_txn(
        &self,
        repo_id: i64,
        txn: &DatabaseTransaction,
    ) -> Result<bool, MegaError> {
        let result = import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::DefaultBranch.eq(true))
            .count(txn)
            .await?;
        Ok(result > 0)
    }

    pub async fn list_branch_refs_in_txn(
        &self,
        repo_id: i64,
        txn: &DatabaseTransaction,
    ) -> Result<Vec<import_refs::Model>, MegaError> {
        Ok(import_refs::Entity::find()
            .filter(import_refs::Column::RepoId.eq(repo_id))
            .filter(import_refs::Column::RefType.eq(RefTypeEnum::Branch))
            .order_by_asc(import_refs::Column::Id)
            .all(txn)
            .await?)
    }

    pub async fn update_pack_id(&self, temp_pack_id: &str, pack_id: &str) -> Result<(), MegaError> {
        let conn = self.get_connection();

        //
        let txn: DatabaseTransaction = conn.begin().await?;

        //
        let tables = [
            (
                "git_blob",
                git_blob::Entity::update_many()
                    .col_expr(git_blob::Column::PackId, Expr::value(pack_id))
                    .filter(git_blob::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "git_tree",
                git_tree::Entity::update_many()
                    .col_expr(git_tree::Column::PackId, Expr::value(pack_id))
                    .filter(git_tree::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "git_tag",
                git_tag::Entity::update_many()
                    .col_expr(git_tag::Column::PackId, Expr::value(pack_id))
                    .filter(git_tag::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "git_commit",
                git_commit::Entity::update_many()
                    .col_expr(git_commit::Column::PackId, Expr::value(pack_id))
                    .filter(git_commit::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
        ];

        //
        for (name, res) in tables {
            if res.rows_affected > 0 {
                tracing::info!(" git object Updated {} rows in {}", res.rows_affected, name);
            }
        }

        //
        txn.commit().await?;
        Ok(())
    }

    pub async fn update_git_blob_filepath(
        &self,
        repo_id: i64,
        blob_id: &str,
        file_path: &str,
    ) -> Result<(), MegaError> {
        self.update_git_blob_filepaths(repo_id, vec![(blob_id.to_string(), file_path.to_string())])
            .await
    }

    /// Batch-assign `file_path` for blobs in one repo.
    ///
    /// Duplicate `blob_id`s keep the last path (same as sequential UPDATE).
    /// Missing ids are skipped. Empty input is a no-op.
    pub async fn update_git_blob_filepaths(
        &self,
        repo_id: i64,
        pairs: Vec<(String, String)>,
    ) -> Result<(), MegaError> {
        if pairs.is_empty() {
            return Ok(());
        }

        let collapsed = last_wins_filepaths(pairs);
        for chunk in collapsed.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            update_filepath_chunk(repo_id, chunk, self.get_connection()).await?;
        }
        Ok(())
    }

    /// `update_git_blob_filepaths` for a receive-pack of the live ImportRepo
    /// `(repo_id, repo_path)` (plan-20260923 ADR-FU-09 item 5): each chunk of
    /// at most 1000 blobs, in `blob_id` order, is one transaction that takes
    /// the liveness lock first, so a detach waits for at most one chunk. The
    /// first refused chunk stops the walk with `IMPORT_REPO_REMOVED`; chunks
    /// already written only touch rows the sweep deletes.
    pub async fn update_import_blob_filepaths_fenced(
        &self,
        repo_id: i64,
        repo_path: &str,
        pairs: Vec<(String, String)>,
    ) -> Result<(), MegaError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let collapsed = last_wins_filepaths(pairs);
        for chunk in collapsed.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            let txn = self.get_connection().begin().await?;
            self.lock_live_import_repo(&txn, repo_id, repo_path).await?;
            update_filepath_chunk(repo_id, chunk, &txn).await?;
            txn.commit().await?;
        }
        Ok(())
    }

    /// Finds a Git repository with an exact match on the repository path.
    ///
    /// # Arguments
    ///
    /// * `repo_path` - A string slice that holds the path of the repository to search for.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option` with the Git repository model if found, or `None` if not found.
    /// Returns a `MegaError` if an error occurs during the search.
    pub async fn find_git_repo_exact_match(
        &self,
        repo_path: &str,
    ) -> Result<Option<git_repo::Model>, MegaError> {
        let result = git_repo::Entity::find()
            .filter(git_repo::Column::RepoPath.eq(repo_path))
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    /// Finds the Git repository whose stored `repo_path` is the longest path prefix of the provided path.
    ///
    /// Matching is by whole path components: `/a/b` matches `/a/b` and `/a/b/c`, but not `/a/bc`.
    /// The stored path is compared literally (no `LIKE` wildcards), and a trailing `/` on either
    /// side is ignored.
    ///
    /// # Arguments
    ///
    /// * `repo_path` - The path to resolve; it may point at the repository root or anywhere below it.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option` with the Git repository model if found, or `None` if not found.
    /// Returns a `MegaError` if an error occurs during the search.
    pub async fn find_git_repo_like_path(
        &self,
        repo_path: &str,
    ) -> Result<Option<git_repo::Model>, MegaError> {
        let query = git_repo::Entity::find()
            .filter(Expr::cust_with_values(
                "starts_with($1 || '/', RTRIM(repo_path, '/') || '/')",
                [repo_path],
            ))
            .order_by_desc(Expr::cust("LENGTH(repo_path)"));
        tracing::debug!("{}", query.build(DbBackend::Postgres).to_string());
        let result = query.one(self.get_connection()).await?;
        Ok(result)
    }

    /// First registration of an ImportRepo row (plan-20260923 ADR-FU-09 item
    /// 2, ancestor lifecycle fence): one transaction share-locks every
    /// registered strict ancestor, in `id` order, then inserts. A parent's
    /// detach (`FOR UPDATE` on its own row) therefore either waits for the
    /// child to commit and then refuses with `IMPORT_REPO_HAS_CHILDREN`, or
    /// has committed first and the child registers as an independent import.
    pub async fn register_import_repo(&self, repo: git_repo::Model) -> Result<(), MegaError> {
        let txn = self.get_connection().begin().await?;
        self.register_import_repo_in_txn(&txn, repo).await?;
        txn.commit().await?;
        Ok(())
    }

    /// `register_import_repo` inside a caller's transaction.
    pub async fn register_import_repo_in_txn(
        &self,
        txn: &DatabaseTransaction,
        repo: git_repo::Model,
    ) -> Result<(), MegaError> {
        let ancestors = MonoStorage::strict_non_root_ancestor_paths(&repo.repo_path);
        if !ancestors.is_empty() {
            git_repo::Entity::find()
                .select_only()
                .column(git_repo::Column::Id)
                .filter(git_repo::Column::RepoPath.is_in(ancestors))
                .order_by_asc(git_repo::Column::Id)
                .lock_shared()
                .into_tuple::<i64>()
                .all(txn)
                .await?;
        }
        git_repo::Entity::insert(repo.into_active_model())
            .exec(txn)
            .await
            .map_err(|e| MegaError::Other(format!("Failed to insert git_repo: {e}")))?;
        Ok(())
    }

    pub async fn save_git_repo(&self, repo: git_repo::Model) -> Result<(), MegaError> {
        let a_model = repo.into_active_model();
        git_repo::Entity::insert(a_model)
            .exec(self.get_connection())
            .await
            .map_err(|e| MegaError::Other(format!("Failed to insert git_repo: {e}")))?;
        Ok(())
    }

    /// Rewrite `git_repo.repo_path` for an existing id (legacy alias → canonical).
    pub async fn relabel_git_repo_path(
        &self,
        repo_id: i64,
        new_path: &str,
    ) -> Result<(), MegaError> {
        let model = git_repo::Entity::find_by_id(repo_id)
            .one(self.get_connection())
            .await?
            .ok_or_else(|| MegaError::Other(format!("git_repo id {repo_id} not found")))?;
        if model.repo_path == new_path {
            return Ok(());
        }
        let mut active: git_repo::ActiveModel = model.into();
        active.repo_path = Set(new_path.to_owned());
        active.updated_at = Set(chrono::Utc::now().naive_utc());
        active
            .update(self.get_connection())
            .await
            .map_err(|e| MegaError::Other(format!("Failed to relabel git_repo path: {e}")))?;
        Ok(())
    }

    pub async fn get_commit_by_hash(
        &self,
        repo_id: i64,
        hash: &str,
    ) -> Result<Option<git_commit::Model>, MegaError> {
        Ok(git_commit::Entity::find()
            .filter(git_commit::Column::RepoId.eq(repo_id))
            .filter(git_commit::Column::CommitId.eq(hash))
            .one(self.get_connection())
            .await?)
    }

    /// One-query existence check across commit/tree/blob/tag for this repo.
    pub async fn object_exists(&self, repo_id: i64, hash: &str) -> Result<bool, MegaError> {
        let conn = self.get_connection();
        let backend = conn.get_database_backend();
        let (sql, values): (&str, Vec<sea_orm::Value>) = match backend {
            DbBackend::Postgres => (
                "SELECT 1 FROM (
                    SELECT 1 FROM git_commit WHERE repo_id = $1 AND commit_id = $2
                    UNION ALL SELECT 1 FROM git_tree WHERE repo_id = $1 AND tree_id = $2
                    UNION ALL SELECT 1 FROM git_blob WHERE repo_id = $1 AND blob_id = $2
                    UNION ALL SELECT 1 FROM git_tag WHERE repo_id = $1 AND tag_id = $2
                ) AS objects LIMIT 1",
                vec![repo_id.into(), hash.into()],
            ),
            _ => (
                "SELECT 1 FROM (
                    SELECT 1 FROM git_commit WHERE repo_id = ? AND commit_id = ?
                    UNION ALL SELECT 1 FROM git_tree WHERE repo_id = ? AND tree_id = ?
                    UNION ALL SELECT 1 FROM git_blob WHERE repo_id = ? AND blob_id = ?
                    UNION ALL SELECT 1 FROM git_tag WHERE repo_id = ? AND tag_id = ?
                ) AS objects LIMIT 1",
                vec![
                    repo_id.into(),
                    hash.into(),
                    repo_id.into(),
                    hash.into(),
                    repo_id.into(),
                    hash.into(),
                    repo_id.into(),
                    hash.into(),
                ],
            ),
        };
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(backend, sql, values))
            .await?;
        Ok(row.is_some())
    }

    pub async fn get_commits_by_hashes(
        &self,
        repo_id: i64,
        hashes: &Vec<String>,
    ) -> Result<Vec<git_commit::Model>, MegaError> {
        Ok(git_commit::Entity::find()
            .filter(git_commit::Column::RepoId.eq(repo_id))
            .filter(git_commit::Column::CommitId.is_in(hashes))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_commits_by_repo_id(
        &self,
        repo_id: i64,
    ) -> Result<impl Stream<Item = Result<git_commit::Model, DbErr>> + Send + '_, MegaError> {
        let stream = git_commit::Entity::find()
            .filter(git_commit::Column::RepoId.eq(repo_id))
            .stream(self.get_connection())
            .await
            .unwrap();
        Ok(stream)
    }

    pub async fn get_last_commit_by_repo_id(
        &self,
        repo_id: i64,
    ) -> Result<Option<git_commit::Model>, MegaError> {
        let one = git_commit::Entity::find()
            .filter(git_commit::Column::RepoId.eq(repo_id))
            .order_by_desc(git_commit::Column::CreatedAt)
            .one(self.get_connection())
            .await?;
        Ok(one)
    }

    pub async fn get_trees_by_repo_id(
        &self,
        repo_id: i64,
    ) -> Result<impl Stream<Item = Result<git_tree::Model, DbErr>> + '_ + Send, MegaError> {
        Ok(git_tree::Entity::find()
            .filter(git_tree::Column::RepoId.eq(repo_id))
            .stream(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_trees_by_hashes(
        &self,
        repo_id: i64,
        hashes: Vec<String>,
    ) -> Result<Vec<git_tree::Model>, MegaError> {
        Ok(git_tree::Entity::find()
            .filter(git_tree::Column::RepoId.eq(repo_id))
            .filter(git_tree::Column::TreeId.is_in(hashes))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_tree_by_hash(
        &self,
        repo_id: i64,
        hash: &str,
    ) -> Result<Option<git_tree::Model>, MegaError> {
        Ok(git_tree::Entity::find()
            .filter(git_tree::Column::RepoId.eq(repo_id))
            .filter(git_tree::Column::TreeId.eq(hash))
            .one(self.get_connection())
            .await?)
    }

    pub async fn get_blobs_by_repo_id(
        &self,
        repo_id: i64,
    ) -> Result<impl Stream<Item = Result<git_blob::Model, DbErr>> + '_ + Send, MegaError> {
        Ok(git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .stream(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_blobs_by_hashes(
        &self,
        repo_id: i64,
        hashes: Vec<String>,
    ) -> Result<Vec<git_blob::Model>, MegaError> {
        Ok(git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .filter(git_blob::Column::BlobId.is_in(hashes))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_tags_by_repo_id(
        &self,
        repo_id: i64,
    ) -> Result<Vec<git_tag::Model>, MegaError> {
        Ok(git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    /// Paginated annotated tags for a given import repo id.
    pub async fn list_tags_by_repo_with_page(
        &self,
        repo_id: i64,
        page: Pagination,
    ) -> Result<(Vec<git_tag::Model>, u64), MegaError> {
        let paginator = git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .order_by_asc(git_tag::Column::TagName)
            .paginate(self.get_connection(), page.per_page);
        let num_items = paginator.num_items().await?;
        Ok(paginator
            .fetch_page(page.page.saturating_sub(1))
            .await
            .map(|m| (m, num_items))?)
    }

    /// Find single tag by repo id and tag name
    pub async fn get_tag_by_repo_and_name(
        &self,
        repo_id: i64,
        name: &str,
    ) -> Result<Option<git_tag::Model>, MegaError> {
        let res = git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .filter(git_tag::Column::TagName.eq(name.to_string()))
            .one(self.get_connection())
            .await?;
        Ok(res)
    }

    /// Insert a single tag model
    pub async fn insert_tag(&self, tag: git_tag::Model) -> Result<git_tag::Model, MegaError> {
        let am: git_tag::ActiveModel = tag.clone().into();
        git_tag::Entity::insert(am)
            .exec(self.get_connection())
            .await?;
        // load saved model back by (repo_id, tag_id): tag_id is only unique
        // per repo since m20260921_000100_fix_git_tag_unique
        let model = git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(tag.repo_id))
            .filter(git_tag::Column::TagId.eq(tag.tag_id.clone()))
            .one(self.get_connection())
            .await?;
        match model {
            Some(m) => Ok(m),
            None => Err(MegaError::Other("Failed to load inserted tag".to_string())),
        }
    }

    /// Delete a tag by repo id and name
    pub async fn delete_tag(&self, repo_id: i64, name: &str) -> Result<(), MegaError> {
        git_tag::Entity::delete_many()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .filter(git_tag::Column::TagName.eq(name.to_string()))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn get_obj_count_by_repo_id(&self, repo_id: i64) -> usize {
        let c_count = git_commit::Entity::find()
            .filter(git_commit::Column::RepoId.eq(repo_id))
            .count(self.get_connection())
            .await
            .unwrap();

        let t_count = git_tree::Entity::find()
            .filter(git_tree::Column::RepoId.eq(repo_id))
            .count(self.get_connection())
            .await
            .unwrap();

        let b_count = git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .count(self.get_connection())
            .await
            .unwrap();

        let tag_count = git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .count(self.get_connection())
            .await
            .unwrap();

        (c_count + t_count + b_count + tag_count)
            .try_into()
            .unwrap()
    }

    /// Records a detach in the cleanup ledger (plan-20260923 ADR-FU-09 item
    /// 6) inside the detach transaction, so it rolls back with it.
    pub async fn insert_cleanup_in_txn<C: ConnectionTrait>(
        &self,
        cleanup_id: i64,
        path: &str,
        repo_id: i64,
        requester: &str,
        conn: &C,
    ) -> Result<import_repo_cleanups::Model, MegaError> {
        let row = import_repo_cleanups::ActiveModel {
            id: Set(cleanup_id),
            path: Set(path.to_owned()),
            repo_id: Set(repo_id),
            state: Set(CleanupState::Detached),
            requester: Set(requester.to_owned()),
            rows_deleted: Set(None),
            created_at: Set(chrono::Utc::now().fixed_offset()),
            swept_at: Set(None),
        };
        Ok(row.insert(conn).await?)
    }

    /// The oldest detached ledger rows for `path`, at most
    /// [`CLEANUP_RESUME_BATCH`], read in `(path, state, id)` index order.
    pub async fn pending_cleanups_by_path<C: ConnectionTrait>(
        &self,
        path: &str,
        conn: &C,
    ) -> Result<Vec<import_repo_cleanups::Model>, MegaError> {
        Ok(pending_cleanups_query(path).all(conn).await?)
    }

    /// Moves a cleanup from `detached` to `swept` (`WHERE id = $1 AND state =
    /// 'detached'`). Returns whether this call moved it; repeating it is a
    /// no-op that keeps the first counts.
    pub async fn mark_cleanup_swept<C: ConnectionTrait>(
        &self,
        cleanup_id: i64,
        rows_deleted: serde_json::Value,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let result = import_repo_cleanups::Entity::update_many()
            .col_expr(
                import_repo_cleanups::Column::State,
                Expr::value(CleanupState::Swept),
            )
            .col_expr(
                import_repo_cleanups::Column::RowsDeleted,
                Expr::value(rows_deleted),
            )
            .col_expr(
                import_repo_cleanups::Column::SweptAt,
                Expr::value(chrono::Utc::now().fixed_offset()),
            )
            .filter(import_repo_cleanups::Column::Id.eq(cleanup_id))
            .filter(import_repo_cleanups::Column::State.eq(CleanupState::Detached))
            .exec(conn)
            .await?;
        Ok(result.rows_affected > 0)
    }
}

/// Ledger rows one cleanup request resumes at most (ADR-FU-09 item 6).
pub const CLEANUP_RESUME_BATCH: u64 = 16;
/// Rows one sweep statement deletes at most (plan-20260923 ADR-FU-09 item 4).
pub const SWEEP_BATCH_ROWS: i64 = 1000;
/// Delete statements one cleanup request runs at most, shared by every ledger
/// row it handles (ADR-FU-09 item 6). Probes are reads and do not count.
pub const SWEEP_STATEMENT_BUDGET: u32 = 100;
/// Rows of the child check read at most (plan-20260923 ADR-FU-10 item 3).
pub const CHILD_CHECK_PAGE: usize = 64;
/// Planner settings of each sweep step (its own short transaction). With
/// sequential and bitmap scans off, the plans left for `WHERE repo_id = $1
/// LIMIT n` are index scans on a `repo_id`-leading index, which stop after
/// `n` live rows of that repository whatever the statistics say. Left on, a
/// repository estimated to be much of the table plans as a sequential scan
/// that reads the whole table once the repository is empty, and a small
/// estimate can plan as a bitmap scan that reads every entry of the
/// repository before the limit applies.
pub(crate) const SWEEP_PLANNER_SETTINGS: &str =
    "SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off";

/// Rows a sweep deleted per object table; the `rows_deleted` shape of the
/// cleanup ledger and of the `phase: "swept"` audit row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SweepCounts {
    pub git_commit: u64,
    pub git_tree: u64,
    pub git_blob: u64,
    pub git_tag: u64,
}

impl SweepCounts {
    pub fn saturating_add(self, other: SweepCounts) -> SweepCounts {
        SweepCounts {
            git_commit: self.git_commit.saturating_add(other.git_commit),
            git_tree: self.git_tree.saturating_add(other.git_tree),
            git_blob: self.git_blob.saturating_add(other.git_blob),
            git_tag: self.git_tag.saturating_add(other.git_tag),
        }
    }

    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "git_commit": self.git_commit,
            "git_tree": self.git_tree,
            "git_blob": self.git_blob,
            "git_tag": self.git_tag,
        })
    }

    /// `None`, a non-object, or any malformed value reads the whole set as
    /// zero (the ledger's writers always emit the four keys).
    pub fn from_json(value: Option<&serde_json::Value>) -> SweepCounts {
        value
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
}

/// What one call of [`GitDbStorage::sweep_import_repo_objects`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Every table probed empty under a fresh snapshot.
    pub complete: bool,
    /// Delete statements this call ran (at most the budget it was given).
    pub statements: u32,
    pub max_rows_per_statement: u64,
    pub deleted: SweepCounts,
}

/// Object tables in sweep order (ADR-FU-09 item 4). Eight literal statements:
/// no table name is ever formatted into SQL.
#[derive(Clone, Copy)]
enum SweepTable {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl SweepTable {
    const ORDER: [SweepTable; 4] = [Self::Commit, Self::Tree, Self::Blob, Self::Tag];

    /// Whether any row of `repo_id` is left; under `SWEEP_PLANNER_SETTINGS`
    /// one step of a `repo_id`-leading index.
    const fn probe_sql(self) -> &'static str {
        match self {
            Self::Commit => "SELECT 1 AS k FROM git_commit WHERE repo_id = $1 LIMIT 1",
            Self::Tree => "SELECT 1 AS k FROM git_tree WHERE repo_id = $1 LIMIT 1",
            Self::Blob => "SELECT 1 AS k FROM git_blob WHERE repo_id = $1 LIMIT 1",
            Self::Tag => "SELECT 1 AS k FROM git_tag WHERE repo_id = $1 LIMIT 1",
        }
    }

    /// One batch: under `SWEEP_PLANNER_SETTINGS` a `repo_id`-leading index
    /// picks at most `$2` primary keys and the outer scan deletes by primary
    /// key. The outer `repo_id + 0 = $1` keeps any other repository's row out
    /// by construction; no `repo_id` index can serve it, so the outer scan
    /// cannot read the whole repository to find the batch.
    const fn delete_sql(self) -> &'static str {
        match self {
            Self::Commit => {
                "DELETE FROM git_commit WHERE id = ANY (ARRAY(\
                 SELECT id FROM git_commit WHERE repo_id = $1 LIMIT $2)) AND repo_id + 0 = $1"
            }
            Self::Tree => {
                "DELETE FROM git_tree WHERE id = ANY (ARRAY(\
                 SELECT id FROM git_tree WHERE repo_id = $1 LIMIT $2)) AND repo_id + 0 = $1"
            }
            Self::Blob => {
                "DELETE FROM git_blob WHERE id = ANY (ARRAY(\
                 SELECT id FROM git_blob WHERE repo_id = $1 LIMIT $2)) AND repo_id + 0 = $1"
            }
            Self::Tag => {
                "DELETE FROM git_tag WHERE id = ANY (ARRAY(\
                 SELECT id FROM git_tag WHERE repo_id = $1 LIMIT $2)) AND repo_id + 0 = $1"
            }
        }
    }

    fn slot(self, counts: &mut SweepCounts) -> &mut u64 {
        match self {
            Self::Commit => &mut counts.git_commit,
            Self::Tree => &mut counts.git_tree,
            Self::Blob => &mut counts.git_blob,
            Self::Tag => &mut counts.git_tag,
        }
    }
}

impl GitDbStorage {
    /// Out-of-lock batched sweep of one detached ImportRepo's object rows
    /// (plan-20260923 ADR-FU-09 item 4). A table is complete only when a
    /// fresh-snapshot probe finds no row of `repo_id`; only deletes count
    /// against `statement_budget`. Each step (a probe, then at most one
    /// delete) is its own short transaction on the pool: every batch commits
    /// on its own and the planner settings end with the step.
    pub async fn sweep_import_repo_objects(
        &self,
        repo_id: i64,
        statement_budget: u32,
        conn: &DatabaseConnection,
    ) -> Result<SweepReport, MegaError> {
        let mut report = SweepReport::default();
        for table in SweepTable::ORDER {
            loop {
                // A short transaction per step scopes the planner settings;
                // the probe and the batch each still read a fresh snapshot.
                let txn = begin_sweep_step(conn).await?;
                let present = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        table.probe_sql(),
                        [repo_id.into()],
                    ))
                    .await?
                    .is_some();
                if !present {
                    txn.commit().await?;
                    break;
                }
                if report.statements >= statement_budget {
                    txn.commit().await?;
                    return Ok(report);
                }
                let affected = txn
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        table.delete_sql(),
                        [repo_id.into(), SWEEP_BATCH_ROWS.into()],
                    ))
                    .await?
                    .rows_affected();
                txn.commit().await?;
                report.statements += 1;
                *table.slot(&mut report.deleted) += affected;
                report.max_rows_per_statement =
                    std::cmp::max(report.max_rows_per_statement, affected);
            }
        }
        report.complete = true;
        Ok(report)
    }

    /// Share-lock the live ImportRepo row `(repo_id, repo_path)` until `txn`
    /// ends (plan-20260923 ADR-FU-09 item 5). Every receive-pack write of the
    /// repository takes it first in its own transaction: a detach (`FOR
    /// UPDATE`, then `DELETE`) waits for the write to commit, or has committed
    /// and the write lands no row. `repo_path` is the path the caller resolved
    /// the row by (canonical on the Git face); no such row is
    /// `IMPORT_REPO_REMOVED`.
    pub async fn lock_live_import_repo(
        &self,
        txn: &DatabaseTransaction,
        repo_id: i64,
        repo_path: &str,
    ) -> Result<(), MegaError> {
        let live = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM git_repo WHERE id = $1 AND repo_path = $2 FOR SHARE",
                [repo_id.into(), repo_path.into()],
            ))
            .await?;
        match live {
            Some(_) => Ok(()),
            None => Err(ImportRepoError::Removed {
                path: repo_path.to_owned(),
            }
            .into()),
        }
    }

    /// Insert object rows of one kind inside `txn`, in chunks, skipping rows
    /// that already exist. No statement-level retry: after a deadlock or
    /// serialization failure PostgreSQL has aborted the transaction, so the
    /// caller retries the whole transaction.
    pub async fn insert_import_objects_in_txn<E, A>(
        &self,
        models: Vec<A>,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError>
    where
        E: EntityTrait,
        A: ActiveModelTrait<Entity = E> + Send + Clone,
    {
        for chunk in models.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            E::insert_many(chunk.to_vec())
                .on_conflict(OnConflict::new().do_nothing().to_owned())
                .exec_without_returning(txn)
                .await?;
        }
        Ok(())
    }

    /// Whether other ImportRepos live below `path` (plan-20260923 ADR-FU-10
    /// item 3). A row whose canonical form is `path` itself, such as a `path/`
    /// alias the FU-15 migration left in place, is the same split identity and
    /// not a child. A full page of such aliases leaves the rest unknown, so it
    /// counts as having children.
    pub async fn import_repo_has_children<C: ConnectionTrait>(
        &self,
        repo_id: i64,
        path: &str,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let rows = conn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT repo_path FROM git_repo WHERE repo_path LIKE $1 ESCAPE '\\' AND id <> $2 LIMIT $3",
                [
                    format!("{}/%", escape_like(path)).into(),
                    repo_id.into(),
                    (CHILD_CHECK_PAGE as i64).into(),
                ],
            ))
            .await?;
        for row in &rows {
            let below: String = row.try_get("", "repo_path")?;
            if canonicalize_mono_ref_path(&below).ok().as_deref() != Some(path) {
                return Ok(true);
            }
        }
        Ok(rows.len() == CHILD_CHECK_PAGE)
    }

    /// One ledger row by its primary key (the cleanup id).
    pub async fn cleanup_by_id<C: ConnectionTrait>(
        &self,
        cleanup_id: i64,
        conn: &C,
    ) -> Result<Option<import_repo_cleanups::Model>, MegaError> {
        Ok(import_repo_cleanups::Entity::find_by_id(cleanup_id)
            .one(conn)
            .await?)
    }

    /// The ledger row an earlier detach of `repo_id` at `path` wrote, if any.
    /// A repository is detached once (the row is inserted in the transaction
    /// that deletes its `git_repo` row), so it has at most one ledger row and
    /// the first state that holds it answers. At most two queries, the
    /// `detached` one first; each reads that path's rows in one state through
    /// the `(path, state, id)` index when the planner takes it, or scans the
    /// ledger when that segment is a sizeable share of it (from about 30 %):
    /// at worst two scans of the ledger, which holds one row per detach ever
    /// made. Only `resolve_cleanup_id` reaches it, for a round that detached
    /// nothing, fresh or replayed (B3 found the repository already gone, or
    /// the row predates FU-16): a round that detached answers with its own
    /// ledger row.
    pub async fn latest_cleanup_for<C: ConnectionTrait>(
        &self,
        repo_id: i64,
        path: &str,
        conn: &C,
    ) -> Result<Option<import_repo_cleanups::Model>, MegaError> {
        for state in [CleanupState::Detached, CleanupState::Swept] {
            if let Some(row) = latest_cleanup_query(repo_id, path, state).one(conn).await? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// Cumulative rows a request deleted before it ran out of budget, kept on
    /// the still-`detached` row so the next request adds to it. `false` when
    /// the row is already swept or unknown; `mark_cleanup_swept` stays the only
    /// `detached -> swept` transition. The counts are evidence, not state:
    /// concurrent sweepers of one row, or a crash between the deletes and this
    /// write, can leave them under- or over-counted.
    pub async fn record_cleanup_progress<C: ConnectionTrait>(
        &self,
        cleanup_id: i64,
        rows_deleted: serde_json::Value,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let result = import_repo_cleanups::Entity::update_many()
            .col_expr(
                import_repo_cleanups::Column::RowsDeleted,
                Expr::value(rows_deleted),
            )
            .filter(import_repo_cleanups::Column::Id.eq(cleanup_id))
            .filter(import_repo_cleanups::Column::State.eq(CleanupState::Detached))
            .exec(conn)
            .await?;
        Ok(result.rows_affected > 0)
    }
}

/// One sweep step's transaction, with `SWEEP_PLANNER_SETTINGS` in force.
async fn begin_sweep_step(conn: &DatabaseConnection) -> Result<DatabaseTransaction, MegaError> {
    let txn = conn.begin().await?;
    txn.execute_unprepared(SWEEP_PLANNER_SETTINGS).await?;
    Ok(txn)
}

fn latest_cleanup_query(
    repo_id: i64,
    path: &str,
    state: CleanupState,
) -> sea_orm::Select<import_repo_cleanups::Entity> {
    import_repo_cleanups::Entity::find()
        .filter(import_repo_cleanups::Column::Path.eq(path))
        .filter(import_repo_cleanups::Column::State.eq(state))
        .filter(import_repo_cleanups::Column::RepoId.eq(repo_id))
        .order_by_desc(import_repo_cleanups::Column::Id)
        .limit(1)
}

fn pending_cleanups_query(path: &str) -> sea_orm::Select<import_repo_cleanups::Entity> {
    import_repo_cleanups::Entity::find()
        .filter(import_repo_cleanups::Column::Path.eq(path))
        .filter(import_repo_cleanups::Column::State.eq(CleanupState::Detached))
        .order_by_asc(import_repo_cleanups::Column::Id)
        .limit(CLEANUP_RESUME_BATCH)
}

/// The last path of each blob, in `blob_id` order (deterministic chunks; the
/// rows of one chunk are still locked in the order of the UPDATE's plan).
fn last_wins_filepaths(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut map = BTreeMap::new();
    for (blob_id, file_path) in pairs {
        map.insert(blob_id, file_path);
    }
    map.into_iter().collect()
}

async fn update_filepath_chunk<C: ConnectionTrait>(
    repo_id: i64,
    chunk: &[(String, String)],
    conn: &C,
) -> Result<(), MegaError> {
    let blob_ids: Vec<String> = chunk.iter().map(|(id, _)| id.clone()).collect();
    let mut case = CaseStatement::new();
    for (blob_id, file_path) in chunk {
        case = case.case(
            Expr::col(git_blob::Column::BlobId).eq(blob_id.clone()),
            file_path.clone(),
        );
    }
    case = case.finally(Expr::col(git_blob::Column::FilePath));
    git_blob::Entity::update_many()
        .col_expr(git_blob::Column::FilePath, case.into())
        .filter(git_blob::Column::RepoId.eq(repo_id))
        .filter(git_blob::Column::BlobId.is_in(blob_ids))
        .exec(conn)
        .await?;
    Ok(())
}

/// Test support shared by the FU-18 fence tests of several modules
/// (`src/jupiter/tests.rs` is outside that card's write set). Lock-wait polls
/// run on the lock holder's own transaction and discard the per-transaction
/// `pg_stat_activity` snapshot first; every helper stays within the test
/// pool's two connections plus one single-connection pool.
#[cfg(test)]
pub(crate) mod fu18_support {
    use std::{path::Path, sync::Arc};

    use sea_orm::{
        ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement,
        TransactionTrait,
    };

    use crate::{
        ceres::api_service::cache::GitObjectCache,
        config::RedisConfig,
        jupiter::{
            redis::init_connection,
            service::{
                git_service::GitService, import_service::ImportService, mono_service::MonoService,
            },
            storage::{Storage, object_storage::mock_object_storage},
            tests::test_storage,
        },
    };

    /// A test storage with the real Git, Monorepo and ImportRepo services and
    /// an initialized Monorepo.
    pub(crate) async fn wired_storage(temp: &Path) -> Storage {
        let mut storage = test_storage(temp).await;
        let git_service = GitService {
            obj_storage: mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = MonoService {
            mono_storage: storage.mono_storage(),
            git_service: git_service.clone(),
        };
        storage.import_service = ImportService {
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

    pub(crate) async fn test_cache() -> Arc<GitObjectCache> {
        let url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let connection = init_connection(&RedisConfig { url })
            .await
            .expect("redis connection");
        Arc::new(GitObjectCache {
            connection,
            prefix: "fu18".to_string(),
        })
    }

    /// A pool of one connection with the options of `conn`'s pool (this
    /// test's schema and backend tag included).
    pub(crate) async fn single_connection(conn: &DatabaseConnection) -> DatabaseConnection {
        let options = conn.get_postgres_connection_pool().connect_options();
        let pool = sea_orm::sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(120))
            .connect_with((*options).clone())
            .await
            .unwrap();
        sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool)
    }

    /// Whether some backend waits on a lock this holder's backend holds
    /// (`transitive`: waits on a backend that itself waits on the holder),
    /// polled for up to ten seconds.
    pub(crate) async fn blocked_by_me<C: ConnectionTrait>(holder: &C, transitive: bool) -> bool {
        let sql = if transitive {
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity w \
             JOIN pg_stat_activity d ON d.pid = ANY(pg_blocking_pids(w.pid)) \
             WHERE w.datname = current_database() AND w.wait_event_type = 'Lock' \
             AND pg_backend_pid() = ANY(pg_blocking_pids(d.pid))) AS v"
        } else {
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
             AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v"
        };
        for _ in 0..200 {
            // Inside a transaction `pg_stat_activity` is a snapshot taken at
            // its first read: discard it before every poll.
            holder
                .execute_unprepared("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            let row = holder
                .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// Hold one `import_refs` row of `repo_id` for update on `single`: a real
    /// detach of that repository then takes its own row lock and parks at
    /// its `DELETE FROM import_refs`, holding the repository row `FOR UPDATE`.
    pub(crate) async fn park_detach(
        single: &DatabaseConnection,
        repo_id: i64,
    ) -> DatabaseTransaction {
        let txn = single.begin().await.unwrap();
        let held = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT id FROM import_refs WHERE repo_id = $1 ORDER BY id LIMIT 1 FOR UPDATE",
                [repo_id.into()],
            ))
            .await
            .unwrap();
        assert!(
            held.is_some(),
            "repository {repo_id} needs an import_refs row"
        );
        txn
    }

    /// Rows of `repo_id` in `git_commit`, `git_tree`, `git_blob`, `git_tag`.
    pub(crate) async fn object_counts<C: ConnectionTrait>(conn: &C, repo_id: i64) -> [i64; 4] {
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT (SELECT count(*) FROM git_commit WHERE repo_id = $1) AS c, \
                 (SELECT count(*) FROM git_tree WHERE repo_id = $1) AS t, \
                 (SELECT count(*) FROM git_blob WHERE repo_id = $1) AS b, \
                 (SELECT count(*) FROM git_tag WHERE repo_id = $1) AS g",
                [repo_id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        ["c", "t", "b", "g"].map(|column| row.try_get::<i64>("", column).unwrap())
    }

    pub(crate) async fn import_refs_count<C: ConnectionTrait>(conn: &C, repo_id: i64) -> i64 {
        conn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM import_refs WHERE repo_id = $1",
            [repo_id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::TransactionTrait;
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations,
        storage::base_storage::{BaseStorage, StorageConnector},
        tests::test_db_connection,
    };

    fn sample_ref(repo_id: i64, ref_name: &str, git_id: &str) -> import_refs::Model {
        import_refs::Model {
            id: generate_id(),
            repo_id,
            ref_name: ref_name.to_string(),
            ref_git_id: git_id.to_string(),
            ref_type: RefTypeEnum::Branch,
            default_branch: false,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    async fn storage() -> GitDbStorage {
        let temp = TempDir::new().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        }
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_deletes_matching_old_id() {
        let git_db = storage().await;
        let repo_id = 42i64;
        let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        git_db
            .save_ref(repo_id, sample_ref(repo_id, "refs/heads/topic", old))
            .await
            .unwrap();

        let deleted = git_db
            .remove_ref_if_unchanged(repo_id, "refs/heads/topic", old, git_db.get_connection())
            .await
            .unwrap();
        assert!(deleted);
        assert!(git_db.get_ref(repo_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_does_not_delete_moved_ref() {
        let git_db = storage().await;
        let repo_id = 42i64;
        let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        git_db
            .save_ref(repo_id, sample_ref(repo_id, "refs/heads/topic", old))
            .await
            .unwrap();
        git_db
            .update_ref(repo_id, "refs/heads/topic", new)
            .await
            .unwrap();

        let deleted = git_db
            .remove_ref_if_unchanged(repo_id, "refs/heads/topic", old, git_db.get_connection())
            .await
            .unwrap();
        assert!(!deleted);
        let refs = git_db.get_ref(repo_id).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_git_id, new);
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_in_txn_stays_atomic_with_other_writes() {
        let git_db = storage().await;
        let repo_id = 7i64;
        let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        git_db
            .save_ref(repo_id, sample_ref(repo_id, "refs/heads/gone", old))
            .await
            .unwrap();

        let txn = git_db.get_connection().begin().await.unwrap();
        let deleted = git_db
            .remove_ref_if_unchanged(repo_id, "refs/heads/gone", old, &txn)
            .await
            .unwrap();
        assert!(deleted);
        git_db
            .save_ref_in_txn(
                repo_id,
                sample_ref(
                    repo_id,
                    "refs/heads/kept",
                    "cccccccccccccccccccccccccccccccccccccccc",
                ),
                &txn,
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let refs = git_db.get_ref(repo_id).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_name, "refs/heads/kept");
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_concurrent_update_keeps_new_id() {
        let git_db = storage().await;
        let repo_id = 9i64;
        let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        git_db
            .save_ref(repo_id, sample_ref(repo_id, "refs/heads/race", old))
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let updater = git_db.clone();
        let deleter = git_db.clone();
        let start_u = barrier.clone();
        let start_d = barrier;
        // `update_ref` panics if the row is already gone; spawn so a delete-first
        // race is a JoinError rather than failing the test.
        let update_join = tokio::spawn(async move {
            start_u.wait().await;
            updater.update_ref(repo_id, "refs/heads/race", new).await
        });
        let delete_join = tokio::spawn(async move {
            start_d.wait().await;
            deleter
                .remove_ref_if_unchanged(repo_id, "refs/heads/race", old, deleter.get_connection())
                .await
                .unwrap()
        });
        let _ = update_join.await;
        let deleted = delete_join.await.expect("CAS delete task");

        let refs = git_db.get_ref(repo_id).await.unwrap();
        match refs.as_slice() {
            [] => assert!(
                deleted,
                "empty table only if CAS delete won against the original id"
            ),
            [row] => {
                assert_eq!(row.ref_git_id, new);
                assert!(!deleted, "CAS delete must not remove the post-update id");
            }
            other => panic!("unexpected refs: {other:?}"),
        }
    }

    fn blob_row(repo_id: i64, blob_id: &str, file_path: &str) -> git_blob::Model {
        git_blob::Model {
            id: generate_id(),
            repo_id,
            blob_id: blob_id.to_string(),
            name: None,
            size: 0,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            file_path: file_path.to_string(),
            pack_offset: 0,
            is_delta_in_pack: false,
        }
    }

    async fn insert_blob(git_db: &GitDbStorage, model: git_blob::Model) {
        git_blob::Entity::insert(model.into_active_model())
            .exec(git_db.get_connection())
            .await
            .expect("insert git_blob");
    }

    async fn filepath_of(git_db: &GitDbStorage, repo_id: i64, blob_id: &str) -> Option<String> {
        git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .filter(git_blob::Column::BlobId.eq(blob_id))
            .one(git_db.get_connection())
            .await
            .unwrap()
            .map(|m| m.file_path)
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_empty_is_noop() {
        let git_db = storage().await;
        git_db
            .update_git_blob_filepaths(1, vec![])
            .await
            .expect("empty batch");
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_sets_paths_in_one_repo() {
        let git_db = storage().await;
        insert_blob(&git_db, blob_row(1, "aaa", "")).await;
        insert_blob(&git_db, blob_row(1, "bbb", "")).await;
        git_db
            .update_git_blob_filepaths(
                1,
                vec![
                    ("aaa".into(), "Cargo.toml".into()),
                    ("bbb".into(), "src/lib.rs".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            filepath_of(&git_db, 1, "aaa").await.as_deref(),
            Some("Cargo.toml")
        );
        assert_eq!(
            filepath_of(&git_db, 1, "bbb").await.as_deref(),
            Some("src/lib.rs")
        );
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_is_scoped_to_repo() {
        let git_db = storage().await;
        insert_blob(&git_db, blob_row(1, "shared-blob", "old-1")).await;
        insert_blob(&git_db, blob_row(2, "shared-blob", "old-2")).await;
        git_db
            .update_git_blob_filepaths(1, vec![("shared-blob".into(), "src/lib.rs".into())])
            .await
            .unwrap();
        assert_eq!(
            filepath_of(&git_db, 1, "shared-blob").await.as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            filepath_of(&git_db, 2, "shared-blob").await.as_deref(),
            Some("old-2")
        );
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_duplicate_blob_keeps_last_path() {
        let git_db = storage().await;
        insert_blob(&git_db, blob_row(1, "dup", "old")).await;
        git_db
            .update_git_blob_filepaths(
                1,
                vec![
                    ("dup".into(), "first.rs".into()),
                    ("dup".into(), "last.rs".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            filepath_of(&git_db, 1, "dup").await.as_deref(),
            Some("last.rs")
        );
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_ignores_missing_blob_id() {
        let git_db = storage().await;
        git_db
            .update_git_blob_filepaths(1, vec![("missing".into(), "nope.rs".into())])
            .await
            .unwrap();
        assert!(filepath_of(&git_db, 1, "missing").await.is_none());
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_binds_quotes_in_path() {
        let git_db = storage().await;
        insert_blob(&git_db, blob_row(1, "q", "")).await;
        git_db
            .update_git_blob_filepaths(1, vec![("q".into(), "foo's/bar.rs".into())])
            .await
            .unwrap();
        assert_eq!(
            filepath_of(&git_db, 1, "q").await.as_deref(),
            Some("foo's/bar.rs")
        );
    }

    fn tag_row(repo_id: i64, tag_id: &str, tag_name: &str) -> git_tag::Model {
        git_tag::Model {
            id: generate_id(),
            repo_id,
            tag_id: tag_id.to_string(),
            object_id: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
            object_type: "commit".to_string(),
            tag_name: tag_name.to_string(),
            tagger: "tagger t <t@t> 1789970957 +0000".to_string(),
            message: "release".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    // Regression for the repo-blind `uniq_gtag_tag_id` constraint: the same
    // annotated tag object pushed into a second import repo was silently
    // dropped by ON CONFLICT DO NOTHING, so clones of that repo could not
    // fetch the tag object.
    #[tokio::test]
    async fn same_tag_object_saved_per_repo() {
        let git_db = storage().await;
        let tag_id = "dddddddddddddddddddddddddddddddddddddddd";
        for repo_id in [1i64, 2i64] {
            git_db
                .batch_save_model::<git_tag::Entity, git_tag::ActiveModel>(vec![
                    tag_row(repo_id, tag_id, "v1.0").into_active_model(),
                ])
                .await
                .unwrap();
        }
        assert_eq!(git_db.get_tags_by_repo_id(1).await.unwrap().len(), 1);
        assert_eq!(git_db.get_tags_by_repo_id(2).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn insert_tag_loads_back_row_of_same_repo() {
        let git_db = storage().await;
        let tag_id = "ffffffffffffffffffffffffffffffffffffffff";
        git_tag::Entity::insert(tag_row(1, tag_id, "v1.0").into_active_model())
            .exec(git_db.get_connection())
            .await
            .unwrap();
        let model = git_db.insert_tag(tag_row(2, tag_id, "v1.0")).await.unwrap();
        assert_eq!(model.repo_id, 2);
    }

    fn cleanup_row(id: i64, path: &str, state: CleanupState) -> import_repo_cleanups::ActiveModel {
        import_repo_cleanups::Model {
            id,
            path: path.to_owned(),
            repo_id: id,
            state,
            requester: "anonymous".to_owned(),
            rows_deleted: None,
            created_at: chrono::Utc::now().fixed_offset(),
            swept_at: None,
        }
        .into_active_model()
    }

    #[tokio::test]
    async fn cleanup_ledger_insert_in_txn_rolls_back() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let path = "/third-party/gone";

        let txn = conn.begin().await.unwrap();
        let row = git_db
            .insert_cleanup_in_txn(101, path, 7, "ci-token", &txn)
            .await
            .unwrap();
        assert_eq!(row.state, CleanupState::Detached);
        assert_eq!(
            git_db.pending_cleanups_by_path(path, &txn).await.unwrap(),
            vec![row]
        );
        txn.rollback().await.unwrap();
        assert!(
            git_db
                .pending_cleanups_by_path(path, conn)
                .await
                .unwrap()
                .is_empty(),
            "the ledger row rolls back with its transaction"
        );

        let txn = conn.begin().await.unwrap();
        git_db
            .insert_cleanup_in_txn(101, path, 7, "ci-token", &txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let pending = git_db.pending_cleanups_by_path(path, conn).await.unwrap();
        assert_eq!(pending.len(), 1);
        let row = &pending[0];
        assert_eq!(
            (
                row.id,
                row.path.as_str(),
                row.repo_id,
                row.requester.as_str()
            ),
            (101, path, 7, "ci-token")
        );
        assert_eq!(row.state, CleanupState::Detached);
        assert_eq!(row.rows_deleted, None);
        assert_eq!(row.swept_at, None);
    }

    #[tokio::test]
    async fn cleanup_ledger_pending_by_path_uses_index() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let path = "/third-party/busy";
        // 1000 detached rows on the path, inserted newest first, plus swept
        // rows on the same path and detached rows on other paths.
        let mut rows: Vec<_> = (1..=1000i64)
            .rev()
            .map(|id| cleanup_row(id, path, CleanupState::Detached))
            .collect();
        rows.extend((1001..=1200i64).map(|id| cleanup_row(id, path, CleanupState::Swept)));
        rows.extend((1201..=1400i64).map(|id| {
            cleanup_row(
                id,
                &format!("/third-party/other-{id}"),
                CleanupState::Detached,
            )
        }));
        for chunk in rows.chunks(500) {
            import_repo_cleanups::Entity::insert_many(chunk.to_vec())
                .exec(conn)
                .await
                .unwrap();
        }
        conn.execute_unprepared("ANALYZE import_repo_cleanups")
            .await
            .unwrap();

        let pending = git_db.pending_cleanups_by_path(path, conn).await.unwrap();
        // ADR-FU-09 item 6 fixes the page at 16 rows.
        assert_eq!(
            pending.iter().map(|row| row.id).collect::<Vec<_>>(),
            (1..=16i64).collect::<Vec<_>>(),
            "the 16 oldest detached rows, oldest first"
        );

        let stmt = pending_cleanups_query(path).build(DbBackend::Postgres);
        let txn = conn.begin().await.unwrap();
        txn.execute_unprepared("SET LOCAL enable_seqscan = off")
            .await
            .unwrap();
        let plan: Vec<String> = txn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("EXPLAIN {}", stmt.sql),
                stmt.values.map(|v| v.0).unwrap_or_default(),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
            .collect();
        txn.rollback().await.unwrap();
        assert!(
            plan.iter().any(|line| line.contains("Index Scan")
                && line.contains("idx_import_repo_cleanups_path_state_id")),
            "the resume query reads the (path, state, id) index: {plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("Sort")),
            "index order needs no sort step: {plan:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_ledger_mark_swept_idempotent() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let path = "/third-party/swept";
        git_db
            .insert_cleanup_in_txn(201, path, 9, "anonymous", conn)
            .await
            .unwrap();
        let counts =
            serde_json::json!({ "git_commit": 3, "git_tree": 5, "git_blob": 8, "git_tag": 0 });

        assert!(
            git_db
                .mark_cleanup_swept(201, counts.clone(), conn)
                .await
                .unwrap()
        );
        let row = import_repo_cleanups::Entity::find_by_id(201)
            .one(conn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, CleanupState::Swept);
        assert_eq!(row.rows_deleted, Some(counts.clone()));
        let swept_at = row.swept_at.expect("swept_at is set");
        assert!(
            git_db
                .pending_cleanups_by_path(path, conn)
                .await
                .unwrap()
                .is_empty()
        );

        // A repeat (e.g. a resumed request racing the first) changes nothing.
        assert!(
            !git_db
                .mark_cleanup_swept(201, serde_json::json!({ "git_commit": 0 }), conn)
                .await
                .unwrap()
        );
        let again = import_repo_cleanups::Entity::find_by_id(201)
            .one(conn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.rows_deleted, Some(counts));
        assert_eq!(again.swept_at, Some(swept_at));
        // An unknown id is not an error either.
        assert!(
            !git_db
                .mark_cleanup_swept(999, serde_json::json!({}), conn)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn cleanup_ledger_primary_key_is_cleanup_id() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        // The key is the detach queue row id as given, not a generated id.
        let row = git_db
            .insert_cleanup_in_txn(4242, "/third-party/a", 1, "anonymous", conn)
            .await
            .unwrap();
        assert_eq!(row.id, 4242);
        assert!(
            import_repo_cleanups::Entity::find_by_id(4242)
                .one(conn)
                .await
                .unwrap()
                .is_some()
        );
        // One row per detach: the same cleanup id cannot be recorded twice,
        // even for another path.
        assert!(
            git_db
                .insert_cleanup_in_txn(4242, "/third-party/b", 2, "anonymous", conn)
                .await
                .is_err()
        );
        let key: Vec<String> = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT a.attname AS name FROM pg_index i \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
                 WHERE i.indrelid = 'import_repo_cleanups'::regclass AND i.indisprimary",
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "name").unwrap())
            .collect();
        assert_eq!(key, vec!["id".to_owned()]);
    }

    async fn save_repo(git_db: &GitDbStorage, repo_path: &str) {
        git_db
            .save_git_repo(git_repo::Model {
                id: generate_id(),
                repo_path: repo_path.to_owned(),
                repo_name: repo_path.rsplit('/').next().unwrap().to_owned(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
            })
            .await
            .unwrap();
    }

    async fn like_path(git_db: &GitDbStorage, path: &str) -> Option<String> {
        git_db
            .find_git_repo_like_path(path)
            .await
            .unwrap()
            .map(|m| m.repo_path)
    }

    #[tokio::test]
    async fn find_git_repo_like_path_binds_quoted_path() {
        let git_db = storage().await;
        save_repo(&git_db, "/third-party/x").await;

        let found = git_db
            .find_git_repo_like_path("/third-party/x'; DROP TABLE git_repo; --")
            .await
            .expect("quoted path must not break the query");
        assert!(found.is_none());
        assert_eq!(
            like_path(&git_db, "/third-party/x").await.as_deref(),
            Some("/third-party/x")
        );
    }

    #[tokio::test]
    async fn find_git_repo_like_path_matches_longest_component_prefix() {
        let git_db = storage().await;
        for path in ["/third-party/x", "/third-party/x/sub", "/third-party/a_b"] {
            save_repo(&git_db, path).await;
        }

        let cases = [
            ("/third-party/x", Some("/third-party/x")),
            ("/third-party/x/", Some("/third-party/x")),
            ("/third-party/x/src/lib.rs", Some("/third-party/x")),
            ("/third-party/x/sub", Some("/third-party/x/sub")),
            ("/third-party/x/sub/a.rs", Some("/third-party/x/sub")),
            ("/third-party/xyz/a.rs", None),
            ("/third-party/a_b/c", Some("/third-party/a_b")),
            ("/third-party/aXb/c", None),
            ("/third-party", None),
        ];
        for (path, expected) in cases {
            assert_eq!(
                like_path(&git_db, path).await.as_deref(),
                expected,
                "{path}"
            );
        }
    }

    fn commit_row(repo_id: i64, n: u32) -> git_commit::Model {
        git_commit::Model {
            id: generate_id(),
            repo_id,
            commit_id: format!("{n:040x}"),
            tree: "a".repeat(40),
            parents_id: serde_json::json!([]),
            author: None,
            committer: None,
            content: None,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    fn tree_row(repo_id: i64, n: u32) -> git_tree::Model {
        git_tree::Model {
            id: generate_id(),
            repo_id,
            tree_id: format!("{n:040x}"),
            sub_trees: Vec::new(),
            size: 0,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    async fn fu17_fill(
        git_db: &GitDbStorage,
        repo_id: i64,
        commits: u32,
        trees: u32,
        blobs: u32,
        tags: u32,
    ) {
        git_db
            .batch_save_model::<git_commit::Entity, git_commit::ActiveModel>(
                (0..commits)
                    .map(|n| commit_row(repo_id, n).into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_tree::Entity, git_tree::ActiveModel>(
                (0..trees)
                    .map(|n| tree_row(repo_id, n).into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_blob::Entity, git_blob::ActiveModel>(
                (0..blobs)
                    .map(|n| blob_row(repo_id, &format!("{n:040x}"), "").into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_tag::Entity, git_tag::ActiveModel>(
                (0..tags)
                    .map(|n| {
                        tag_row(repo_id, &format!("{n:040x}"), &format!("v{n}")).into_active_model()
                    })
                    .collect(),
            )
            .await
            .unwrap();
    }

    async fn fu17_repo_id(git_db: &GitDbStorage, path: &str) -> i64 {
        git_db
            .find_git_repo_exact_match(path)
            .await
            .unwrap()
            .expect(path)
            .id
    }

    async fn fu17_latest(git_db: &GitDbStorage, repo_id: i64, path: &str) -> Option<i64> {
        git_db
            .latest_cleanup_for(repo_id, path, git_db.get_connection())
            .await
            .unwrap()
            .map(|row| row.id)
    }

    /// Sequential scans of this schema's object tables, all backends. The
    /// autocommit `pg_stat_force_next_flush()` makes this backend flush its
    /// pending counters when it next goes idle outside a transaction, which
    /// is before that statement completes; the read that follows sees them.
    async fn fu17_seq_scans(single: &DatabaseConnection) -> i64 {
        single
            .execute_unprepared("SELECT pg_stat_force_next_flush()")
            .await
            .unwrap();
        single
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT coalesce(sum(seq_scan), 0)::bigint AS n FROM pg_stat_user_tables \
                 WHERE schemaname = current_schema() \
                 AND relname IN ('git_commit', 'git_tree', 'git_blob', 'git_tag')",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }

    /// `count` repositories of one row in each object table; the first id.
    async fn fu17_crowd(git_db: &GitDbStorage, count: i64) -> i64 {
        let base = generate_id();
        git_db
            .batch_save_model::<git_commit::Entity, git_commit::ActiveModel>(
                (0..count)
                    .map(|i| commit_row(base + i, 0).into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_tree::Entity, git_tree::ActiveModel>(
                (0..count)
                    .map(|i| tree_row(base + i, 0).into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_blob::Entity, git_blob::ActiveModel>(
                (0..count)
                    .map(|i| blob_row(base + i, &format!("{i:040x}"), "").into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_tag::Entity, git_tag::ActiveModel>(
                (0..count)
                    .map(|i| tag_row(base + i, &format!("{i:040x}"), "v0").into_active_model())
                    .collect(),
            )
            .await
            .unwrap();
        base
    }

    /// The probe and the batch of every table, under the custom and the
    /// generic plan, in a sweep step's transaction: an index scan on a
    /// `repo_id`-leading index (the batch then deleting by primary key), never
    /// a sequential or bitmap scan, never a sort.
    async fn fu17_assert_sweep_plans(git_db: &GitDbStorage, repo_id: i64) {
        for table in SweepTable::ORDER {
            let (name, indexes): (&str, &[&str]) = match table {
                SweepTable::Commit => ("git_commit", &["uniq_c_git_repo_id", "idx_ic_repo_id"]),
                SweepTable::Tree => ("git_tree", &["uniq_t_git_repo", "idx_t_repo_id"]),
                SweepTable::Blob => ("git_blob", &["uniq_b_git_repo"]),
                SweepTable::Tag => ("git_tag", &["uniq_gtag_repo_tag"]),
            };
            let steps = [
                (
                    table.probe_sql(),
                    "bigint",
                    repo_id.to_string(),
                    format!("on {name}"),
                ),
                (
                    table.delete_sql(),
                    "bigint, bigint",
                    format!("{repo_id}, {SWEEP_BATCH_ROWS}"),
                    format!("on {name} {name}_1"),
                ),
            ];
            for (sql, types, args, scanned) in steps {
                let txn = begin_sweep_step(git_db.get_connection()).await.unwrap();
                txn.execute_unprepared(&format!("PREPARE fu17_step ({types}) AS {sql}"))
                    .await
                    .unwrap();
                for mode in ["force_custom_plan", "force_generic_plan"] {
                    txn.execute_unprepared(&format!("SET LOCAL plan_cache_mode = {mode}"))
                        .await
                        .unwrap();
                    let plan: Vec<String> = txn
                        .query_all_raw(Statement::from_string(
                            DbBackend::Postgres,
                            format!("EXPLAIN EXECUTE fu17_step({args})"),
                        ))
                        .await
                        .unwrap()
                        .iter()
                        .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
                        .collect();
                    let context = format!("{name} {mode} repo {repo_id}: {plan:?}");
                    assert!(
                        plan.iter().any(|line| line.contains("Index")
                            && indexes.iter().any(
                                |index| line.contains(&format!("Scan using {index} {scanned}"))
                            )),
                        "{context}"
                    );
                    assert!(
                        plan.iter()
                            .any(|line| line.contains("Index Cond: (repo_id = ")),
                        "{context}"
                    );
                    if sql == table.probe_sql() {
                        assert!(
                            !plan.iter().any(|line| line.contains("Filter:")),
                            "{context}"
                        );
                    }
                    if sql == table.delete_sql() {
                        assert!(
                            plan.iter().any(|line| line
                                .contains(&format!("Index Scan using {name}_pkey on {name} "))),
                            "{context}"
                        );
                        assert!(
                            plan.iter()
                                .any(|line| line.contains("Index Cond: (id = ANY")),
                            "{context}"
                        );
                    }
                    assert!(
                        !plan.iter().any(|line| line.contains("Seq Scan")
                            || line.contains("Bitmap")
                            || line.contains("Sort")),
                        "{context}"
                    );
                }
                txn.execute_unprepared("DEALLOCATE fu17_step")
                    .await
                    .unwrap();
                txn.rollback().await.unwrap();
            }
        }
    }

    async fn fu17_analyze(git_db: &GitDbStorage) {
        git_db
            .get_connection()
            .execute_unprepared(
                "ANALYZE git_commit; ANALYZE git_tree; ANALYZE git_blob; ANALYZE git_tag",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fu17_sweep_bounded_and_complete() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let target = generate_id();
        let decoy = generate_id();
        let cut = generate_id();
        // An exact multiple, a small table, a table spanning three batches,
        // an empty one; a decoy repository that is most of `git_blob`, and
        // later a crowd of 300 one-row repositories that makes one repository
        // a small estimate.
        fu17_fill(&git_db, target, 1000, 3, 2500, 7).await;
        fu17_fill(&git_db, decoy, 0, 0, 50_000, 5).await;
        fu17_fill(&git_db, cut, 0, 0, 2500, 0).await;
        // Three statistics snapshots: before this test's ANALYZE (autovacuum
        // may or may not have run), analyzed with three repositories (a small
        // share and most of a table), analyzed again with the crowd.
        fu17_assert_sweep_plans(&git_db, target).await;
        fu17_analyze(&git_db).await;
        for repo_id in [target, decoy] {
            fu17_assert_sweep_plans(&git_db, repo_id).await;
        }
        let crowd = fu17_crowd(&git_db, 300).await;
        fu17_analyze(&git_db).await;
        for repo_id in [target, decoy, crowd] {
            fu17_assert_sweep_plans(&git_db, repo_id).await;
        }
        // The sweep runs its steps that way, each its own transaction as in
        // production: on a pool of one connection, whose statistics are
        // flushed on demand, sweeping three batches of the decoy (most of
        // `git_blob`) adds no sequential scan of any object table.
        let single = fu18_support::single_connection(conn).await;
        let before = fu17_seq_scans(&single).await;
        let report = git_db
            .sweep_import_repo_objects(decoy, 3, &single)
            .await
            .unwrap();
        assert_eq!((report.statements, report.deleted.git_blob), (3, 3000));
        assert_eq!(fu17_seq_scans(&single).await, before);
        // And the settings ended with each step: the pooled connection plans
        // later queries as usual.
        let settings = single
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT current_setting('enable_seqscan') AS seq, \
                 current_setting('enable_bitmapscan') AS bitmap",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settings.try_get::<String>("", "seq").unwrap(), "on");
        assert_eq!(settings.try_get::<String>("", "bitmap").unwrap(), "on");

        let report = git_db
            .sweep_import_repo_objects(target, SWEEP_STATEMENT_BUDGET, conn)
            .await
            .unwrap();
        assert!(report.complete, "{report:?}");
        assert_eq!(
            report.deleted,
            SweepCounts {
                git_commit: 1000,
                git_tree: 3,
                git_blob: 2500,
                git_tag: 7,
            }
        );
        // 1 + 1 + 3 + 1: an exact multiple costs no extra statement (the
        // probe decides, not a short batch).
        assert_eq!(report.statements, 6);
        assert_eq!(report.max_rows_per_statement, 1000);
        assert!(report.max_rows_per_statement <= SWEEP_BATCH_ROWS as u64);
        assert_eq!(git_db.get_obj_count_by_repo_id(target).await, 0);
        assert_eq!(
            git_db.get_obj_count_by_repo_id(decoy).await,
            47_005,
            "another repository's rows are never touched"
        );

        // No budget on a repository that still has rows: nothing happens.
        let none = git_db
            .sweep_import_repo_objects(cut, 0, conn)
            .await
            .unwrap();
        assert!(!none.complete);
        assert_eq!((none.statements, none.deleted), (0, SweepCounts::default()));
        // The budget cuts a table mid-way; the next call finishes it.
        let first = git_db
            .sweep_import_repo_objects(cut, 2, conn)
            .await
            .unwrap();
        assert!(!first.complete);
        assert_eq!((first.statements, first.deleted.git_blob), (2, 2000));
        let second = git_db
            .sweep_import_repo_objects(cut, SWEEP_STATEMENT_BUDGET, conn)
            .await
            .unwrap();
        assert!(second.complete);
        assert_eq!((second.statements, second.deleted.git_blob), (1, 500));

        // A swept repository costs no statement, even with no budget at all.
        let again = git_db
            .sweep_import_repo_objects(target, 0, conn)
            .await
            .unwrap();
        assert!(again.complete);
        assert_eq!(again.statements, 0);
        assert_eq!(again.deleted, SweepCounts::default());

        assert_eq!(SWEEP_BATCH_ROWS, 1000);
        assert_eq!(SWEEP_STATEMENT_BUDGET, 100);
        assert_eq!(CLEANUP_RESUME_BATCH, 16);
    }

    #[tokio::test]
    async fn fu17_child_query_escapes_and_excludes_aliases() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        for path in [
            "/third-party/a_b",
            "/third-party/a_b/",
            "/third-party/a_b/.",
            "/third-party/a_bc",
            "/third-party/a_bc/child",
            "/third-party/aXb/child",
            "/third-party/p%q",
            "/third-party/pZZq/child",
            "/third-party/a",
            "/third-party/ab",
            "/third-party/ab/x",
        ] {
            save_repo(&git_db, path).await;
        }
        let a_b = fu17_repo_id(&git_db, "/third-party/a_b").await;
        let pq = fu17_repo_id(&git_db, "/third-party/p%q").await;
        let a = fu17_repo_id(&git_db, "/third-party/a").await;
        // `_` and `%` are literal; `/a` does not match `/ab`; the alias
        // spellings of the target itself are not children.
        for (repo_id, path) in [
            (a_b, "/third-party/a_b"),
            (pq, "/third-party/p%q"),
            (a, "/third-party/a"),
        ] {
            assert!(
                !git_db
                    .import_repo_has_children(repo_id, path, conn)
                    .await
                    .unwrap(),
                "{path}"
            );
        }
        save_repo(&git_db, "/third-party/a_b/real").await;
        assert!(
            git_db
                .import_repo_has_children(a_b, "/third-party/a_b", conn)
                .await
                .unwrap()
        );
        let real = fu17_repo_id(&git_db, "/third-party/a_b/real").await;
        assert!(
            !git_db
                .import_repo_has_children(real, "/third-party/a_b/real", conn)
                .await
                .unwrap()
        );

        // A full page of aliases leaves the rest unknown: conservative.
        save_repo(&git_db, "/third-party/z").await;
        let z = fu17_repo_id(&git_db, "/third-party/z").await;
        for k in 0..CHILD_CHECK_PAGE - 1 {
            save_repo(&git_db, &format!("/third-party/z/{}", "./".repeat(k))).await;
        }
        assert!(
            !git_db
                .import_repo_has_children(z, "/third-party/z", conn)
                .await
                .unwrap()
        );
        save_repo(
            &git_db,
            &format!("/third-party/z/{}", "./".repeat(CHILD_CHECK_PAGE - 1)),
        )
        .await;
        assert!(
            git_db
                .import_repo_has_children(z, "/third-party/z", conn)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn fu17_ledger_reads_and_progress() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let (p, q) = ("/third-party/p", "/third-party/q");
        for (id, path, repo_id) in [(10, p, 3), (20, p, 1), (30, p, 2), (40, q, 1)] {
            git_db
                .insert_cleanup_in_txn(id, path, repo_id, "anonymous", conn)
                .await
                .unwrap();
        }
        assert!(
            git_db
                .mark_cleanup_swept(20, serde_json::json!({ "git_blob": 1 }), conn)
                .await
                .unwrap()
        );

        let by_id = git_db.cleanup_by_id(20, conn).await.unwrap().unwrap();
        assert_eq!(by_id.state, CleanupState::Swept);
        assert!(git_db.cleanup_by_id(99, conn).await.unwrap().is_none());
        // The row of that repository at that path, in either state.
        assert_eq!(fu17_latest(&git_db, 1, p).await, Some(20));
        assert_eq!(fu17_latest(&git_db, 2, p).await, Some(30));
        assert_eq!(fu17_latest(&git_db, 1, q).await, Some(40));
        assert_eq!(fu17_latest(&git_db, 3, p).await, Some(10));
        assert_eq!(fu17_latest(&git_db, 4, p).await, None);
        // At scale: 1000 swept and 200 detached rows of other repositories on
        // one path, the target being the oldest swept row, so both queries
        // cross the path's whole history. With these statistics the generic
        // plan is a range read of the (path, state, id) index; this pins that
        // the index serves the lookup, not a plan for every ledger shape.
        let history = "/third-party/history";
        let mut rows: Vec<_> = (1001..=2000i64)
            .map(|id| cleanup_row(id, history, CleanupState::Swept))
            .collect();
        rows.extend((2001..=2200i64).map(|id| cleanup_row(id, history, CleanupState::Detached)));
        for chunk in rows.chunks(500) {
            import_repo_cleanups::Entity::insert_many(chunk.to_vec())
                .exec(conn)
                .await
                .unwrap();
        }
        conn.execute_unprepared("ANALYZE import_repo_cleanups")
            .await
            .unwrap();
        assert_eq!(fu17_latest(&git_db, 1001, history).await, Some(1001));
        assert_eq!(fu17_latest(&git_db, 2200, history).await, Some(2200));
        assert_eq!(fu17_latest(&git_db, 5000, history).await, None);
        let stmt =
            latest_cleanup_query(1001, history, CleanupState::Swept).build(DbBackend::Postgres);
        let txn = conn.begin().await.unwrap();
        txn.execute_unprepared(&format!(
            "PREPARE fu17_latest (text, text, bigint, bigint) AS {}",
            stmt.sql
        ))
        .await
        .unwrap();
        txn.execute_unprepared("SET LOCAL plan_cache_mode = force_generic_plan")
            .await
            .unwrap();
        let plan: Vec<String> = txn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("EXPLAIN EXECUTE fu17_latest('{history}', 'swept', 1001, 1)"),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
            .collect();
        txn.rollback().await.unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_import_repo_cleanups_path_state_id")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|line| line.contains("Seq Scan on import_repo_cleanups")),
            "{plan:?}"
        );

        // Progress lands only on a row that is still detached, and does not
        // change its state.
        let progress =
            serde_json::json!({ "git_commit": 0, "git_tree": 0, "git_blob": 7, "git_tag": 0 });
        assert!(
            git_db
                .record_cleanup_progress(10, progress.clone(), conn)
                .await
                .unwrap()
        );
        let row = git_db.cleanup_by_id(10, conn).await.unwrap().unwrap();
        assert_eq!(row.state, CleanupState::Detached);
        assert_eq!(row.rows_deleted, Some(progress));
        assert!(
            git_db
                .pending_cleanups_by_path(p, conn)
                .await
                .unwrap()
                .iter()
                .any(|r| r.id == 10)
        );
        assert!(
            !git_db
                .record_cleanup_progress(20, serde_json::json!({ "git_blob": 9 }), conn)
                .await
                .unwrap()
        );
        assert_eq!(
            git_db
                .cleanup_by_id(20, conn)
                .await
                .unwrap()
                .unwrap()
                .rows_deleted,
            Some(serde_json::json!({ "git_blob": 1 }))
        );

        assert_eq!(SweepCounts::from_json(None), SweepCounts::default());
        assert_eq!(
            SweepCounts::from_json(Some(&serde_json::json!({ "git_blob": "x" }))),
            SweepCounts::default()
        );
        assert_eq!(
            SweepCounts::from_json(Some(
                &serde_json::json!({ "git_commit": 5, "git_tree": 2, "git_blob": "x" })
            )),
            SweepCounts::default(),
            "one malformed value zeroes the whole set"
        );
        assert_eq!(
            SweepCounts::from_json(Some(&serde_json::json!({ "git_blob": 3 }))),
            SweepCounts {
                git_blob: 3,
                ..SweepCounts::default()
            }
        );
        assert_eq!(
            SweepCounts {
                git_commit: 1,
                git_blob: 2,
                ..SweepCounts::default()
            }
            .to_json(),
            serde_json::json!({ "git_commit": 1, "git_tree": 0, "git_blob": 2, "git_tag": 0 })
        );
    }

    /// Two sweepers of one repository: the batch that waited on the other's
    /// row locks deletes nothing, and the probe, not that short batch, says
    /// whether the table is done.
    #[tokio::test]
    async fn fu17_sweep_survives_a_blocked_batch() {
        let git_db = storage().await;
        let conn = git_db.get_connection().clone();
        let target = generate_id();
        fu17_fill(&git_db, target, 0, 0, 2500, 0).await;
        fu17_analyze(&git_db).await;

        // The other sweeper: one batch deleted, under the same planner
        // settings, but not committed.
        let holder = begin_sweep_step(&conn).await.unwrap();
        let held = holder
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                SweepTable::Blob.delete_sql(),
                [target.into(), SWEEP_BATCH_ROWS.into()],
            ))
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(held, 1000);
        let sweeper = git_db.clone();
        let sweep = tokio::spawn(async move {
            sweeper
                .sweep_import_repo_objects(target, SWEEP_STATEMENT_BUDGET, sweeper.get_connection())
                .await
                .unwrap()
        });
        let mut waited = false;
        for _ in 0..200 {
            // Inside a transaction `pg_stat_activity` is a snapshot taken at
            // its first read: discard it before every poll.
            holder
                .execute_unprepared("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            let row = holder
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock' \
                     AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v",
                ))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(waited, "the sweep waited on the other sweeper's batch");
        holder.commit().await.unwrap();
        let report = sweep.await.unwrap();
        assert!(report.complete, "{report:?}");
        assert_eq!(report.deleted.git_blob, 1500, "{report:?}");
        assert_eq!(
            report.statements, 3,
            "the blocked batch affected 0 rows; the remaining 1500 rows took 2 batches: {report:?}"
        );
        assert_eq!(git_db.get_obj_count_by_repo_id(target).await, 0);
    }

    fn fu18_repo(path: &str) -> git_repo::Model {
        crate::ceres::protocol::repo::Repo::new(std::path::PathBuf::from(path), false)
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn fu18_lock_live_import_repo_holds_share_lock() {
        let git_db = storage().await;
        let conn = git_db.get_connection();
        let path = "/third-party/fu18-lock";
        let repo = fu18_repo(path);
        let id = repo.id;
        git_db.register_import_repo(repo).await.unwrap();

        let holder = conn.begin().await.unwrap();
        git_db
            .lock_live_import_repo(&holder, id, path)
            .await
            .expect("live row");
        for (repo_id, locked_path) in [(id + 1, path), (id, "/third-party/other")] {
            let error = git_db
                .lock_live_import_repo(&holder, repo_id, locked_path)
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "IMPORT_REPO_REMOVED: {locked_path:?} was removed; push again to import it anew"
                )
            );
        }
        // A share lock: another writer of the repository takes it too.
        let other = conn.begin().await.unwrap();
        other
            .execute_unprepared("SET LOCAL lock_timeout = '100ms'")
            .await
            .unwrap();
        git_db
            .lock_live_import_repo(&other, id, path)
            .await
            .expect("share locks are compatible");
        other.rollback().await.unwrap();
        // It outlives the statement: a writer of the row itself waits.
        let writer = conn.begin().await.unwrap();
        writer
            .execute_unprepared("SET LOCAL lock_timeout = '100ms'")
            .await
            .unwrap();
        let blocked = writer
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE git_repo SET updated_at = now() WHERE id = $1",
                [id.into()],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(blocked.contains("lock"), "{blocked}");
        writer.rollback().await.unwrap();
        holder.commit().await.unwrap();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE git_repo SET updated_at = now() WHERE id = $1",
            [id.into()],
        ))
        .await
        .expect("free after commit");
    }

    #[tokio::test]
    async fn fu18_child_registration_fenced_by_parent() {
        use crate::ceres::pack::import_repo::detach_import_repo;

        let temp = TempDir::new().unwrap();
        let storage = fu18_support::wired_storage(temp.path()).await;
        let git_db = storage.git_db_storage();
        let conn = git_db.get_connection().clone();

        // I1, the registration first: it share-locks the grandparent row,
        // the parent's detach waits, then sees the child and refuses.
        let p1 = "/third-party/fu18-p1";
        let parent1 = fu18_repo(p1);
        let parent1_id = parent1.id;
        git_db.register_import_repo(parent1).await.unwrap();
        let registering = conn.begin().await.unwrap();
        git_db
            .register_import_repo_in_txn(&registering, fu18_repo("/third-party/fu18-p1/sub/c"))
            .await
            .unwrap();
        let detach_storage = storage.clone();
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                fu18_support::test_cache().await,
                parent1_id,
                "/third-party/fu18-p1",
                None,
            )
            .await
        });
        assert!(
            fu18_support::blocked_by_me(&registering, false).await,
            "the detach waits on the registering child"
        );
        registering.commit().await.unwrap();
        let refused = detaching.await.unwrap().unwrap_err();
        assert!(
            matches!(
                refused,
                MegaError::ImportRepo(ImportRepoError::HasChildren { ref path }) if path == p1
            ),
            "{refused:?}"
        );
        assert!(
            git_db
                .find_git_repo_exact_match(p1)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            git_db
                .find_git_repo_exact_match("/third-party/fu18-p1/sub/c")
                .await
                .unwrap()
                .is_some()
        );

        // I2, the detach first: it holds the parent row for update (parked
        // at its DELETE FROM import_refs); the child registration waits on
        // it and then registers as an independent import.
        let p2 = "/third-party/fu18-p2";
        let parent2 = fu18_repo(p2);
        let parent2_id = parent2.id;
        git_db.register_import_repo(parent2).await.unwrap();
        git_db
            .save_ref(
                parent2_id,
                sample_ref(parent2_id, "refs/heads/main", &"c".repeat(40)),
            )
            .await
            .unwrap();
        let single = fu18_support::single_connection(&conn).await;
        let parked = fu18_support::park_detach(&single, parent2_id).await;
        let detach_storage = storage.clone();
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                fu18_support::test_cache().await,
                parent2_id,
                "/third-party/fu18-p2",
                None,
            )
            .await
        });
        assert!(
            fu18_support::blocked_by_me(&parked, false).await,
            "the detach parks at its import_refs delete"
        );
        let register_db = git_db.clone();
        let registering = tokio::spawn(async move {
            register_db
                .register_import_repo(fu18_repo("/third-party/fu18-p2/sub/c"))
                .await
        });
        assert!(
            fu18_support::blocked_by_me(&parked, true).await,
            "the child registration waits on the detach"
        );
        parked.commit().await.unwrap();
        assert!(detaching.await.unwrap().unwrap().is_some());
        registering.await.unwrap().expect("registers independently");
        assert!(
            git_db
                .find_git_repo_exact_match(p2)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            git_db
                .find_git_repo_exact_match("/third-party/fu18-p2/sub/c")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(fu18_support::import_refs_count(&conn, parent2_id).await, 0);
    }
}
