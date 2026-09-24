use std::{collections::HashMap, ops::Deref};

use futures::Stream;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, DbErr,
    EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QueryTrait, Set,
    Statement, TransactionTrait,
    sea_query::{CaseStatement, Expr, ExprTrait, OnConflict},
};

use crate::{
    callisto::{
        git_blob, git_commit, git_repo, git_tag, git_tree, import_refs,
        sea_orm_active_enums::RefTypeEnum,
    },
    common::{errors::MegaError, utils::generate_id},
    contract::api::common::Pagination,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
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
                .exec(self.get_connection())
                .await?;
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

    /// Finds a Git repository with a path that matches the beginning of the provided repository path using a LIKE query.
    ///
    /// # Arguments
    ///
    /// * `repo_path` - A string slice that holds the beginning of the path of the repository to search for.
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
            .filter(Expr::cust(format!("'{repo_path}' LIKE repo_path || '%'")))
            .order_by_desc(Expr::cust("LENGTH(repo_path)"));
        tracing::debug!("{}", query.build(DbBackend::Postgres).to_string());
        let result = query.one(self.get_connection()).await?;
        Ok(result)
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
}

fn last_wins_filepaths(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut map = HashMap::with_capacity(pairs.len());
    for (blob_id, file_path) in pairs {
        map.insert(blob_id, file_path);
    }
    map.into_iter().collect()
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
}
