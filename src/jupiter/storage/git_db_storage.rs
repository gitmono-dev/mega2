use std::{collections::HashMap, ops::Deref};

use futures::Stream;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, DbErr,
    EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QueryTrait, Set,
    TransactionTrait,
    sea_query::{CaseStatement, Expr, ExprTrait},
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
            let repo_id = generate_id()?;
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
            id: generate_id()?,
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

    /// Deletes a ref only if it still points at the client-advertised object.
    ///
    /// The receive-pack old id is a lease, so a concurrent update must leave
    /// the moved ref intact instead of turning a stale delete into data loss.
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

    /// Updates one blob using the pre-FC-10 repository-wide lookup contract.
    ///
    /// New traversal code must use [`Self::update_git_blob_filepaths`] so the
    /// repository scope is explicit and all paths are written in batches.
    #[deprecated(note = "use update_git_blob_filepaths with an explicit repo_id")]
    pub async fn update_git_blob_filepath(
        &self,
        blob_id: &String,
        file_path: &str,
    ) -> Result<(), MegaError> {
        if let Some(model) = git_blob::Entity::find()
            .filter(git_blob::Column::BlobId.eq(blob_id))
            .one(self.get_connection())
            .await?
        {
            let mut active: git_blob::ActiveModel = model.into();
            active.file_path = Set(file_path.to_owned());
            active.update(self.get_connection()).await?;
        }
        Ok(())
    }

    /// Batch-assigns file paths for blobs belonging to one import repository.
    ///
    /// Duplicate blob ids keep the last path, matching the old sequential
    /// traversal. Missing blob ids are ignored by the guarded UPDATE, and an
    /// empty batch does no database work.
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
            let blob_ids: Vec<String> = chunk.iter().map(|(blob_id, _)| blob_id.clone()).collect();
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
            .await?)
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

    /// Find a stored annotated tag object by its object id.
    pub async fn get_tag_by_hash(
        &self,
        repo_id: i64,
        tag_id: &str,
    ) -> Result<Option<git_tag::Model>, MegaError> {
        let result = git_tag::Entity::find()
            .filter(git_tag::Column::RepoId.eq(repo_id))
            .filter(git_tag::Column::TagId.eq(tag_id))
            .one(self.get_connection())
            .await?;
        Ok(result)
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
        // load saved model back by tag_id
        let model = git_tag::Entity::find()
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
    let mut positions: HashMap<String, usize> = HashMap::with_capacity(pairs.len());
    let mut collapsed: Vec<(String, String)> = Vec::with_capacity(pairs.len());

    for (blob_id, file_path) in pairs {
        if let Some(index) = positions.get(&blob_id).copied() {
            collapsed[index].1 = file_path;
        } else {
            positions.insert(blob_id.clone(), collapsed.len());
            collapsed.push((blob_id, file_path));
        }
    }

    collapsed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::{
        ColumnTrait, DatabaseConnection, DbBackend, EntityTrait, IntoActiveModel, MockDatabase,
        MockExecResult, QueryFilter, TransactionTrait, sea_query::Expr,
    };
    use tokio::sync::Barrier;

    use super::GitDbStorage;
    use crate::{
        callisto::{git_blob, git_tag, import_refs, sea_orm_active_enums::RefTypeEnum},
        common::utils::generate_id,
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    #[tokio::test]
    async fn get_tag_by_hash_is_repo_scoped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let repo_id = 17;
        let tag = git_tag::Model {
            id: generate_id().expect("test ID generator initialized"),
            repo_id,
            tag_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            object_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            object_type: "commit".to_owned(),
            tag_name: "v1.0.0".to_owned(),
            tagger: "Monoengine Test <test@example.invalid>".to_owned(),
            message: "tag lookup regression".to_owned(),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        };
        storage.insert_tag(tag.clone()).await.expect("insert tag");

        assert_eq!(
            storage
                .get_tag_by_hash(repo_id, &tag.tag_id)
                .await
                .expect("look up tag")
                .map(|stored| stored.tag_name),
            Some(tag.tag_name)
        );
        assert!(
            storage
                .get_tag_by_hash(repo_id + 1, &tag.tag_id)
                .await
                .expect("look up other repo")
                .is_none()
        );
    }

    fn branch_ref(repo_id: i64, ref_name: &str, ref_git_id: &str) -> import_refs::Model {
        import_refs::Model {
            id: generate_id().expect("test ID generator initialized"),
            repo_id,
            ref_name: ref_name.to_owned(),
            ref_git_id: ref_git_id.to_owned(),
            ref_type: RefTypeEnum::Branch,
            default_branch: false,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }

    fn blob_row(id: i64, repo_id: i64, blob_id: &str, file_path: &str) -> git_blob::Model {
        git_blob::Model {
            id,
            repo_id,
            blob_id: blob_id.to_owned(),
            name: None,
            size: 0,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            file_path: file_path.to_owned(),
            pack_offset: 0,
            is_delta_in_pack: false,
        }
    }

    async fn insert_blob(storage: &GitDbStorage, model: git_blob::Model) {
        git_blob::Entity::insert(model.into_active_model())
            .exec(storage.get_connection())
            .await
            .expect("insert git blob");
    }

    async fn filepath_of(storage: &GitDbStorage, repo_id: i64, blob_id: &str) -> Option<String> {
        git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .filter(git_blob::Column::BlobId.eq(blob_id))
            .one(storage.get_connection())
            .await
            .expect("load git blob")
            .map(|blob| blob.file_path)
    }

    fn mock_storage(exec_count: usize) -> (GitDbStorage, DatabaseConnection) {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results((0..exec_count).map(|_| MockExecResult::default()))
            .into_connection();
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(connection.clone())),
        };
        (storage, connection)
    }

    fn update_statement_count(connection: DatabaseConnection) -> usize {
        connection
            .into_transaction_log()
            .iter()
            .flat_map(|transaction| transaction.statements())
            .filter(|statement| statement.sql.trim_start().starts_with("UPDATE"))
            .count()
    }

    fn filepath_pairs(count: usize) -> Vec<(String, String)> {
        (0..count)
            .map(|index| (format!("blob-{index:04}"), format!("dir/file-{index}.rs")))
            .collect()
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_has_one_update_per_chunk() {
        let (storage, connection) = mock_storage(0);
        storage
            .update_git_blob_filepaths(1, Vec::new())
            .await
            .expect("empty batch");
        assert_eq!(update_statement_count(connection), 0);

        let (storage, connection) = mock_storage(1);
        storage
            .update_git_blob_filepaths(
                1,
                filepath_pairs(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE),
            )
            .await
            .expect("single chunk");
        assert_eq!(update_statement_count(connection), 1);

        let (storage, connection) = mock_storage(2);
        storage
            .update_git_blob_filepaths(
                1,
                filepath_pairs(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE + 1),
            )
            .await
            .expect("two chunks");
        assert_eq!(update_statement_count(connection), 2);
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_is_empty_safe_and_repo_scoped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        insert_blob(&storage, blob_row(1, 1, "shared", "old-a")).await;
        insert_blob(&storage, blob_row(2, 2, "shared", "old-b")).await;

        storage
            .update_git_blob_filepaths(1, Vec::new())
            .await
            .expect("empty batch");
        storage
            .update_git_blob_filepaths(1, vec![("shared".to_owned(), "new-a".to_owned())])
            .await
            .expect("update repo-scoped batch");

        assert_eq!(
            filepath_of(&storage, 1, "shared").await.as_deref(),
            Some("new-a")
        );
        assert_eq!(
            filepath_of(&storage, 2, "shared").await.as_deref(),
            Some("old-b")
        );
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_keeps_last_duplicate_and_ignores_missing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        insert_blob(&storage, blob_row(1, 1, "duplicate", "old")).await;

        storage
            .update_git_blob_filepaths(
                1,
                vec![
                    ("duplicate".to_owned(), "first.rs".to_owned()),
                    ("missing".to_owned(), "missing.rs".to_owned()),
                    ("duplicate".to_owned(), "last.rs".to_owned()),
                ],
            )
            .await
            .expect("duplicate and missing ids are valid input");

        assert_eq!(
            filepath_of(&storage, 1, "duplicate").await.as_deref(),
            Some("last.rs")
        );
        assert!(filepath_of(&storage, 1, "missing").await.is_none());
    }

    #[tokio::test]
    async fn update_git_blob_filepaths_chunks_large_tree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let repo_id = 3;
        let count = <BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE + 1;
        let mut pairs = Vec::with_capacity(count);
        for index in 0..count {
            let blob_id = format!("blob-{index:04}");
            insert_blob(&storage, blob_row(index as i64 + 1, repo_id, &blob_id, "")).await;
            pairs.push((blob_id, format!("dir/file-{index}.rs")));
        }

        storage
            .update_git_blob_filepaths(repo_id, pairs)
            .await
            .expect("update chunked batch");

        assert_eq!(
            filepath_of(&storage, repo_id, "blob-0000").await.as_deref(),
            Some("dir/file-0.rs")
        );
        assert_eq!(
            filepath_of(
                &storage,
                repo_id,
                &format!("blob-{count_minus_one:04}", count_minus_one = count - 1),
            )
            .await
            .as_deref(),
            Some(format!("dir/file-{}.rs", count - 1).as_str())
        );
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_is_atomic_and_repo_scoped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = GitDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let repo_id = 29;
        let original = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let moved = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        storage
            .save_ref(repo_id, branch_ref(repo_id, "refs/heads/cas", original))
            .await
            .expect("save ref");
        storage
            .save_ref(repo_id, branch_ref(repo_id, "refs/heads/atomic", original))
            .await
            .expect("save atomic ref");
        storage
            .save_ref(
                repo_id + 1,
                branch_ref(repo_id + 1, "refs/heads/cas", original),
            )
            .await
            .expect("save other-repo ref");

        storage
            .update_ref(repo_id, "refs/heads/cas", moved)
            .await
            .expect("move ref before stale delete");

        let txn = storage
            .get_connection()
            .begin()
            .await
            .expect("begin transaction");
        assert!(
            storage
                .remove_ref_if_unchanged(repo_id, "refs/heads/atomic", original, &txn)
                .await
                .expect("delete unchanged ref")
        );
        assert!(
            !storage
                .remove_ref_if_unchanged(repo_id, "refs/heads/cas", original, &txn)
                .await
                .expect("check stale delete")
        );
        txn.rollback()
            .await
            .expect("rollback conflicted transaction");

        let refs = storage.get_ref(repo_id).await.expect("load refs");
        assert_eq!(refs.len(), 2, "a stale delete must roll back sibling work");
        assert_eq!(
            refs.iter()
                .find(|reference| reference.ref_name == "refs/heads/cas")
                .map(|reference| reference.ref_git_id.as_str()),
            Some(moved)
        );
        assert_eq!(
            storage
                .get_ref(repo_id + 1)
                .await
                .expect("load other-repo refs")
                .len(),
            1,
            "the repository scope must be part of the conditional delete"
        );

        assert!(
            storage
                .remove_ref_if_unchanged(
                    repo_id,
                    "refs/heads/cas",
                    moved,
                    storage.get_connection(),
                )
                .await
            .expect("delete current ref")
        );
        assert_eq!(
            storage.get_ref(repo_id).await.expect("reload refs").len(),
            1
        );

        let race_repo_id = repo_id + 2;
        storage
            .save_ref(
                race_repo_id,
                branch_ref(race_repo_id, "refs/heads/race", original),
            )
            .await
            .expect("save race ref");
        let update_connection = storage.get_connection().clone();
        let delete_connection = storage.get_connection().clone();
        let delete_storage = storage.clone();
        let barrier = Arc::new(Barrier::new(2));
        let update_barrier = barrier.clone();
        let delete_barrier = barrier.clone();
        let update_original = original.to_owned();
        let update_moved = moved.to_owned();
        let update_task = async move {
            update_barrier.wait().await;
            import_refs::Entity::update_many()
                .col_expr(import_refs::Column::RefGitId, Expr::value(update_moved))
                .filter(import_refs::Column::RepoId.eq(race_repo_id))
                .filter(import_refs::Column::RefName.eq("refs/heads/race"))
                .filter(import_refs::Column::RefGitId.eq(update_original))
                .exec(&update_connection)
                .await
                .expect("race update")
                .rows_affected
        };
        let delete_task = async move {
            delete_barrier.wait().await;
            delete_storage
                .remove_ref_if_unchanged(
                    race_repo_id,
                    "refs/heads/race",
                    original,
                    &delete_connection,
                )
                .await
                .expect("race delete")
        };
        let (updated, deleted) = tokio::join!(update_task, delete_task);
        assert!(
            !(updated == 1 && deleted),
            "a successful later update and stale delete must not both commit"
        );
        let race_refs = storage.get_ref(race_repo_id).await.expect("load race refs");
        match (updated, deleted) {
            (1, false) => assert_eq!(race_refs[0].ref_git_id, moved),
            (0, true) => assert!(race_refs.is_empty()),
            outcome => panic!("unexpected CAS race outcome: {outcome:?}"),
        }
    }
}
