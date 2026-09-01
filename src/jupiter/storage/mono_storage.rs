use std::{collections::HashMap, ops::Deref};

use futures::{StreamExt, stream::FuturesUnordered};
use git_internal::{
    hash::ObjectHash,
    internal::{
        metadata::EntryMeta,
        object::{commit::Commit, tree::Tree},
    },
};
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::Set,
    ColumnTrait, Condition, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait,
    IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
    sea_query::{CaseStatement, Expr, ExprTrait, OnConflict},
};

use crate::{
    callisto::{mega_blob, mega_cl, mega_commit, mega_refs, mega_tag, mega_tree},
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    contract::api::common::Pagination,
    jupiter::{
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            commit_binding_storage::CommitBindingStorage,
            user_storage::UserStorage,
        },
        utils::converter::IntoMegaModel,
    },
};
#[derive(Clone)]
pub struct MonoStorage {
    pub base: BaseStorage,
}

impl Deref for MonoStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

#[derive(Debug)]
pub struct RefUpdateData {
    pub path: String,
    pub ref_name: String,
    pub commit_id: String,
    pub tree_hash: String,
}

impl MonoStorage {
    pub fn user_storage(&self) -> UserStorage {
        UserStorage {
            base: self.base.clone(),
        }
    }

    pub fn commit_binding_storage(&self) -> CommitBindingStorage {
        CommitBindingStorage {
            base: self.base.clone(),
        }
    }

    pub async fn save_refs(
        &self,
        model: mega_refs::Model,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        model
            .into_active_model()
            .insert(&self.build_connection_with_txn(txn))
            .await?;
        Ok(())
    }

    /// Removes non-CL refs under the given path, but keeps the ref matching the path itself.
    pub async fn remove_none_cl_refs(&self, path: &str) -> Result<(), MegaError> {
        mega_refs::Entity::delete_many()
            .filter(mega_refs::Column::Path.starts_with(path))
            .filter(mega_refs::Column::Path.ne(path))
            .filter(mega_refs::Column::IsCl.eq(false))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn remove_ref(&self, refs: mega_refs::Model) -> Result<(), MegaError> {
        mega_refs::Entity::delete_by_id(refs.id)
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    /// Deletes a MonoRepo ref only while it still points at the advertised
    /// commit. The path keeps same-named refs in other repositories isolated.
    pub async fn remove_ref_if_unchanged<C: ConnectionTrait>(
        &self,
        path: &str,
        ref_name: &str,
        expected_commit_hash: &str,
        conn: &C,
    ) -> Result<bool, MegaError> {
        let result = mega_refs::Entity::delete_many()
            .filter(mega_refs::Column::Path.eq(path))
            .filter(mega_refs::Column::RefName.eq(ref_name))
            .filter(mega_refs::Column::RefCommitHash.eq(expected_commit_hash))
            .exec(conn)
            .await?;
        Ok(result.rows_affected > 0)
    }

    pub async fn get_refs_for_paths_and_cls(
        &self,
        paths: &[&str],
        cls: Option<&[&str]>,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        let mut query = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.is_in(paths.iter().copied()))
            .order_by_asc(mega_refs::Column::RefName);

        if let Some(cls_values) = cls {
            query = query.filter(mega_refs::Column::RefName.is_in(cls_values.iter().copied()));
        } else {
            query = query.filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME));
        }

        let result = query.all(self.get_connection()).await?;
        Ok(result)
    }

    pub async fn get_all_refs(
        &self,
        path: &str,
        filter_cl: bool,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        let mut query = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq(path))
            .order_by_asc(mega_refs::Column::RefName);

        if filter_cl {
            query = query.filter(mega_refs::Column::IsCl.eq(false));
        }
        let result = query.all(self.get_connection()).await?;

        Ok(result)
    }

    pub async fn get_main_ref(&self, path: &str) -> Result<Option<mega_refs::Model>, MegaError> {
        let result = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq(path))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    pub async fn get_ref_by_commit(
        &self,
        path: &str,
        commit: &str,
    ) -> Result<Option<mega_refs::Model>, MegaError> {
        let result = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq(path))
            .filter(mega_refs::Column::RefCommitHash.eq(commit))
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    pub async fn get_ref_by_name(
        &self,
        ref_name: &str,
    ) -> Result<Option<mega_refs::Model>, MegaError> {
        let res = mega_refs::Entity::find()
            .filter(mega_refs::Column::RefName.eq(ref_name))
            .one(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn get_ref_by_name_in_txn(
        &self,
        ref_name: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<mega_refs::Model>, MegaError> {
        Ok(mega_refs::Entity::find()
            .filter(mega_refs::Column::RefName.eq(ref_name))
            .one(txn)
            .await?)
    }

    pub async fn update_ref(
        &self,
        refs: mega_refs::Model,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        let mut ref_data: mega_refs::ActiveModel = refs.into();
        ref_data.reset(mega_refs::Column::RefCommitHash);
        ref_data.reset(mega_refs::Column::RefTreeHash);
        ref_data.reset(mega_refs::Column::UpdatedAt);
        let conn = self.build_connection_with_txn(txn);
        ref_data.update(&conn).await?;
        Ok(())
    }

    /// Create or update a CL ref (refs/cl/{cl_link}).
    ///
    /// This method creates a new CL ref if it doesn't exist, or updates an existing
    /// one with new commit and tree hashes. CL refs are marked with `is_cl_ref = true`.
    ///
    /// # Arguments
    /// * `path` - The repository path for the ref
    /// * `ref_name` - The full ref name (e.g., "refs/cl/ABC12345")
    /// * `commit_id` - The commit hash to point to
    /// * `tree_hash` - The tree hash associated with the commit
    ///
    /// # Returns
    /// Returns `Ok(())` on success, or an error if the database operation fails.
    pub async fn save_or_update_cl_ref(
        &self,
        path: &str,
        ref_name: &str,
        commit_id: &str,
        tree_hash: &str,
    ) -> Result<(), MegaError> {
        // Delegate to transaction version using default connection
        self.save_or_update_cl_ref_in_txn(
            self.get_connection(),
            path,
            ref_name,
            commit_id,
            tree_hash,
        )
        .await
    }

    /// Create or update a CL ref within a database transaction.
    ///
    /// This is the transaction-safe version of [`save_or_update_cl_ref`](Self::save_or_update_cl_ref)
    /// for use in atomic operations. It performs the same logic but accepts a connection parameter
    /// to participate in an existing transaction.
    ///
    /// # Arguments
    /// * `conn` - Database connection or transaction to use
    /// * `path` - The repository path for the ref
    /// * `ref_name` - The full ref name (e.g., "refs/cl/ABC12345")
    /// * `commit_id` - The commit hash to point to
    /// * `tree_hash` - The tree hash associated with the commit
    ///
    /// # Returns
    /// Returns `Ok(())` on success, or an error if the database operation fails.
    pub async fn save_or_update_cl_ref_in_txn<C>(
        &self,
        conn: &C,
        path: &str,
        ref_name: &str,
        commit_id: &str,
        tree_hash: &str,
    ) -> Result<(), MegaError>
    where
        C: ConnectionTrait,
    {
        let existing = mega_refs::Entity::find()
            .filter(mega_refs::Column::RefName.eq(ref_name))
            .one(conn)
            .await?;

        if let Some(existing_ref) = existing {
            // Update existing CL ref
            let mut active = existing_ref.into_active_model();
            active.ref_commit_hash = Set(commit_id.to_owned());
            active.ref_tree_hash = Set(tree_hash.to_owned());
            active.updated_at = Set(chrono::Utc::now().naive_utc());
            active.update(conn).await?;
        } else {
            // Create new CL ref
            let new_ref = mega_refs::Model::new(
                path,
                ref_name.to_owned(),
                commit_id.to_owned(),
                tree_hash.to_owned(),
                true, // is_cl_ref
            )?;
            mega_refs::Entity::insert(new_ref.into_active_model())
                .exec(conn)
                .await?;
        }
        Ok(())
    }

    pub async fn batch_update_by_path_concurrent(
        &self,
        updates: Vec<RefUpdateData>,
    ) -> Result<(), MegaError> {
        let conn = self.get_connection();
        let mut condition = Condition::any();
        for update in &updates {
            condition = condition.add(
                Condition::all()
                    .add(mega_refs::Column::Path.eq(update.path.clone()))
                    .add(mega_refs::Column::RefName.eq(update.ref_name.clone())),
            );
        }

        let existing_refs: Vec<mega_refs::Model> = mega_refs::Entity::find()
            .filter(condition)
            .all(conn)
            .await?;

        let ref_map: HashMap<(String, String), mega_refs::Model> = existing_refs
            .into_iter()
            .map(|r| ((r.path.clone(), r.ref_name.clone()), r))
            .collect();

        let mut futures = FuturesUnordered::new();

        for update in updates {
            if let Some(ref_data) = ref_map.get(&(update.path.clone(), update.ref_name.clone())) {
                let conn = conn.clone();
                let mut active: mega_refs::ActiveModel = ref_data.clone().into();

                futures.push(async move {
                    active.ref_commit_hash = Set(update.commit_id);
                    active.ref_tree_hash = Set(update.tree_hash);
                    active.updated_at = Set(chrono::Utc::now().naive_utc());
                    active.update(&conn).await?;
                    Ok::<(), MegaError>(())
                });
            }
        }

        while let Some(res) = futures.next().await {
            res?;
        }

        Ok(())
    }

    pub async fn update_blob_filepath(
        &self,
        blob_id: &str,
        file_path: &str,
    ) -> Result<(), MegaError> {
        self.update_blob_filepaths(vec![(blob_id.to_string(), file_path.to_string())])
            .await
    }

    /// Batch-assigns file paths in the monorepo blob index.
    ///
    /// Duplicate blob ids keep the last path, matching the old sequential
    /// traversal. Missing blob ids are ignored by the guarded UPDATE, and an
    /// empty batch does no database work.
    pub async fn update_blob_filepaths(
        &self,
        pairs: Vec<(String, String)>,
    ) -> Result<(), MegaError> {
        if pairs.is_empty() {
            return Ok(());
        }

        let collapsed = last_wins_mega_filepaths(pairs);
        for chunk in collapsed.chunks(<BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE) {
            let blob_ids: Vec<String> = chunk.iter().map(|(blob_id, _)| blob_id.clone()).collect();
            let mut case = CaseStatement::new();
            for (blob_id, file_path) in chunk {
                case = case.case(
                    Expr::col(mega_blob::Column::BlobId).eq(blob_id.clone()),
                    file_path.clone(),
                );
            }
            case = case.finally(Expr::col(mega_blob::Column::FilePath));

            mega_blob::Entity::update_many()
                .col_expr(mega_blob::Column::FilePath, case.into())
                .filter(mega_blob::Column::BlobId.is_in(blob_ids))
                .exec(self.get_connection())
                .await?;
        }
        Ok(())
    }

    pub async fn update_pack_id(&self, temp_pack_id: &str, pack_id: &str) -> Result<(), MegaError> {
        let conn = self.get_connection();

        let txn: DatabaseTransaction = conn.begin().await?;

        let tables = [
            (
                "mega_blob",
                mega_blob::Entity::update_many()
                    .col_expr(mega_blob::Column::PackId, Expr::value(pack_id))
                    .filter(mega_blob::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "mega_tree",
                mega_tree::Entity::update_many()
                    .col_expr(mega_tree::Column::PackId, Expr::value(pack_id))
                    .filter(mega_tree::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "mega_tag",
                mega_tag::Entity::update_many()
                    .col_expr(mega_tag::Column::PackId, Expr::value(pack_id))
                    .filter(mega_tag::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
            (
                "mega_commit",
                mega_commit::Entity::update_many()
                    .col_expr(mega_commit::Column::PackId, Expr::value(pack_id))
                    .filter(mega_commit::Column::PackId.eq(temp_pack_id))
                    .exec(&txn)
                    .await?,
            ),
        ];

        for (name, res) in tables {
            if res.rows_affected > 0 {
                tracing::info!("mega object Updated {} rows in {}", res.rows_affected, name);
            }
        }

        txn.commit().await?;
        Ok(())
    }

    /// Process commit author bindings
    pub async fn process_commit_bindings(
        &self,
        commits: &[(String, String)],
        authenticated_username: Option<&str>,
    ) -> Result<(), MegaError> {
        let commit_binding_storage = self.commit_binding_storage();

        for (commit_sha, _author_email) in commits {
            // Try to find user by authenticated username first
            let matched_username = if let Some(username) = authenticated_username {
                // Local users table removed: accept authenticated username directly
                Some(username.to_string())
            } else {
                // No authenticated username, commit will be anonymous
                tracing::info!(
                    "No authenticated username available for commit {}",
                    commit_sha
                );
                None
            };

            let is_anonymous = matched_username.is_none();

            // Save or update binding
            if let Err(e) = commit_binding_storage
                .upsert_binding(commit_sha, matched_username.clone(), is_anonymous)
                .await
            {
                tracing::error!("Failed to save commit binding for {}: {}", commit_sha, e);
                // Continue processing other commits even if one fails
            } else {
                tracing::info!(
                    "Processed binding for commit {} (anonymous: {}, username: {})",
                    commit_sha,
                    is_anonymous,
                    matched_username.unwrap_or_else(|| "anonymous".to_string())
                );
            }
        }
        Ok(())
    }

    /// Attach import-repo snapshot to monorepo root inside an existing transaction.
    ///
    /// Root `mega_refs` is updated with a compare-and-swap on `(ref_commit_hash, ref_tree_hash)`:
    /// if another writer advanced the root first, the update affects zero rows and this returns
    /// [`MegaError::StaleMonorepoRootRef`] so the caller can roll back and retry.
    pub async fn attach_to_monorepo_parent_in_txn(
        &self,
        txn: &DatabaseTransaction,
        root_ref_id: i64,
        expected_ref_commit_hash: &str,
        expected_ref_tree_hash: &str,
        commit: Commit,
        trees: Vec<Tree>,
    ) -> Result<(), MegaError> {
        let new_commit_id = commit.id.to_string();
        let new_tree_id = commit.tree_id.to_string();
        self.save_mega_trees(trees, commit.id, Some(txn)).await?;
        self.save_mega_commits(vec![commit], Some(txn)).await?;

        let now = chrono::Utc::now().naive_utc();
        let update_result = mega_refs::Entity::update_many()
            .col_expr(mega_refs::Column::RefCommitHash, Expr::value(new_commit_id))
            .col_expr(mega_refs::Column::RefTreeHash, Expr::value(new_tree_id))
            .col_expr(mega_refs::Column::UpdatedAt, Expr::value(now))
            .filter(mega_refs::Column::Id.eq(root_ref_id))
            .filter(mega_refs::Column::RefCommitHash.eq(expected_ref_commit_hash))
            .filter(mega_refs::Column::RefTreeHash.eq(expected_ref_tree_hash))
            .exec(txn)
            .await?;

        if update_result.rows_affected == 0 {
            return Err(MegaError::StaleMonorepoRootRef);
        }

        Ok(())
    }

    pub async fn mega_head_hash_with_txn(
        &self,
        mega_refs: mega_refs::Model,
        commit: Commit,
    ) -> Result<(), MegaError> {
        let txn = self.connection.begin().await?;
        self.save_refs(mega_refs, Some(&txn)).await?;
        self.save_mega_commits(vec![commit], Some(&txn)).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Save trees batch in a transaction with idempotency support.
    ///
    /// Uses `ON CONFLICT DO NOTHING` on `TreeId` to ensure idempotency.
    /// This allows safe retries: already-inserted trees are silently skipped.
    ///
    /// # Arguments
    /// * `conn` - Database connection or transaction (supports `ConnectionTrait`)
    /// * `tree_models` - Vector of tree active models to insert
    ///
    /// # Returns
    /// Returns `Ok(())` on success. If tree_models is empty, returns immediately without database operation.
    pub async fn save_trees_batch<C>(
        &self,
        conn: &C,
        tree_models: Vec<mega_tree::ActiveModel>,
    ) -> Result<(), MegaError>
    where
        C: ConnectionTrait,
    {
        if tree_models.is_empty() {
            return Ok(());
        }

        match mega_tree::Entity::insert_many(tree_models)
            .on_conflict(
                OnConflict::column(mega_tree::Column::TreeId)
                    .do_nothing()
                    .to_owned(),
            )
            .exec(conn)
            .await
        {
            Ok(_) => Ok(()),
            Err(DbErr::RecordNotInserted) => {
                // All trees already exist (idempotent operation).
                // Expected behavior when complete_upload is retried (idempotent).
                tracing::debug!("All trees already exist, skipping insert (idempotent operation)");
                Ok(())
            }
            Err(e) => {
                // Real database errors (constraint violations, connection issues, etc.)
                // should be propagated, not ignored
                tracing::error!("Database error during tree batch insert: {:?}", e);
                Err(MegaError::Db(e))
            }
        }
    }

    /// Save a commit in a transaction with idempotency support.
    ///
    /// Uses `ON CONFLICT DO NOTHING` on `CommitId` to ensure idempotency.
    /// This allows safe retries: already-inserted commits are silently skipped.
    ///
    /// # Arguments
    /// * `conn` - Database connection or transaction (supports `ConnectionTrait`)
    /// * `commit_model` - Commit active model to insert
    ///
    /// # Returns
    /// Returns `Ok(())` on success
    pub async fn save_commit_in_txn<C>(
        &self,
        conn: &C,
        commit_model: mega_commit::ActiveModel,
    ) -> Result<(), MegaError>
    where
        C: ConnectionTrait,
    {
        match mega_commit::Entity::insert(commit_model)
            .on_conflict(
                OnConflict::column(mega_commit::Column::CommitId)
                    .do_nothing()
                    .to_owned(),
            )
            .exec(conn)
            .await
        {
            Ok(_) => Ok(()),
            Err(DbErr::RecordNotInserted) => {
                // Commit already exists (idempotent operation).
                // Expected behavior when complete_upload is retried (idempotent).
                tracing::debug!("Commit already exists, skipping insert (idempotent operation)");
                Ok(())
            }
            Err(e) => {
                // Real database errors (constraint violations, connection issues, etc.)
                // should be propagated, not ignored
                tracing::error!("Database error during commit insert: {:?}", e);
                Err(MegaError::Db(e))
            }
        }
    }

    /// Get and update a CL within a transaction.
    ///
    /// # Arguments
    /// * `conn` - Database connection or transaction (supports `ConnectionTrait`)
    /// * `cl_link` - CL link (session_id for buck uploads)
    /// * `from_hash` - Base commit hash
    /// * `to_hash` - Target commit hash
    /// * `commit_message` - Commit message (used as CL title)
    ///
    /// # Returns
    /// Returns the updated CL model on success
    pub async fn get_and_update_cl_in_txn<C>(
        &self,
        conn: &C,
        cl_link: &str,
        from_hash: &str,
        to_hash: &str,
        commit_message: &str,
    ) -> Result<mega_cl::Model, MegaError>
    where
        C: ConnectionTrait,
    {
        use sea_orm::ActiveValue::Set;

        use crate::callisto::sea_orm_active_enums::MergeStatusEnum;

        let cl = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(cl_link))
            .one(conn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {}", cl_link)))?;

        let mut cl_active = cl.clone().into_active_model();
        cl_active.from_hash = Set(from_hash.to_owned());
        cl_active.to_hash = Set(to_hash.to_owned());
        cl_active.status = Set(MergeStatusEnum::Open);
        cl_active.title = Set(commit_message.to_owned());
        cl_active.updated_at = Set(chrono::Utc::now().naive_utc());

        cl_active.update(conn).await?;

        Ok(cl)
    }

    pub async fn save_mega_commits(
        &self,
        commits: Vec<Commit>,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        let save_models: Vec<mega_commit::ActiveModel> = commits
            .into_iter()
            .map(|c| c.into_mega_model(EntryMeta::default()))
            .collect::<Result<Vec<mega_commit::Model>, MegaError>>()?
            .into_iter()
            .map(|m| m.into_active_model())
            .collect();
        self.batch_save_model_with_txn(save_models, txn).await?;
        Ok(())
    }

    pub async fn save_mega_trees(
        &self,
        trees: Vec<Tree>,
        commit_id: ObjectHash,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        let save_models: Vec<mega_tree::ActiveModel> = trees
            .into_iter()
            .map(|t| t.into_mega_model(EntryMeta::default()))
            .collect::<Result<Vec<mega_tree::Model>, MegaError>>()?
            .into_iter()
            .map(|mut m| {
                m.commit_id = commit_id.to_string();
                m.into_active_model()
            })
            .collect();
        let on_conflict = OnConflict::columns(vec![mega_tree::Column::TreeId])
            .do_nothing()
            .to_owned();
        self.batch_save_model_with_conflict_and_txn(save_models, on_conflict, txn)
            .await?;
        Ok(())
    }

    pub async fn get_commit_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<mega_commit::Model>, MegaError> {
        Ok(mega_commit::Entity::find()
            .filter(mega_commit::Column::CommitId.eq(hash))
            .one(self.get_connection())
            .await?)
    }

    pub async fn get_commits_by_hashes(
        &self,
        hashes: &Vec<String>,
    ) -> Result<Vec<mega_commit::Model>, MegaError> {
        Ok(mega_commit::Entity::find()
            .filter(mega_commit::Column::CommitId.is_in(hashes))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_tree_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<mega_tree::Model>, MegaError> {
        Ok(mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.eq(hash))
            .one(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_trees_by_hashes(
        &self,
        hashes: Vec<String>,
    ) -> Result<Vec<mega_tree::Model>, MegaError> {
        Ok(mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.is_in(hashes))
            .distinct()
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_mega_blobs_by_hashes(
        &self,
        hashes: Vec<String>,
    ) -> Result<Vec<mega_blob::Model>, MegaError> {
        Ok(mega_blob::Entity::find()
            .filter(mega_blob::Column::BlobId.is_in(hashes))
            .all(self.get_connection())
            .await
            .unwrap())
    }

    pub async fn get_tag_by_name(&self, name: &str) -> Result<Option<mega_tag::Model>, MegaError> {
        let res = mega_tag::Entity::find()
            .filter(mega_tag::Column::TagName.eq(name.to_string()))
            .one(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn insert_tag(&self, tag: mega_tag::Model) -> Result<mega_tag::Model, MegaError> {
        let am: mega_tag::ActiveModel = tag.clone().into();
        mega_tag::Entity::insert(am)
            .exec(self.get_connection())
            .await?;
        let model = mega_tag::Entity::find()
            .filter(mega_tag::Column::TagId.eq(tag.tag_id.clone()))
            .one(self.get_connection())
            .await?;
        match model {
            Some(m) => Ok(m),
            None => Err(MegaError::Other("Failed to load inserted tag".to_string())),
        }
    }

    pub async fn delete_tag_by_name(&self, name: &str) -> Result<(), MegaError> {
        mega_tag::Entity::delete_many()
            .filter(mega_tag::Column::TagName.eq(name.to_string()))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    /// Paginated annotated tags stored in mega_tag table
    pub async fn get_tags_by_page(
        &self,
        page: Pagination,
    ) -> Result<(Vec<mega_tag::Model>, u64), MegaError> {
        let paginator = mega_tag::Entity::find()
            .order_by_asc(mega_tag::Column::TagName)
            .paginate(self.get_connection(), page.per_page);
        let num_items = paginator.num_items().await?;
        Ok(paginator
            .fetch_page(page.page.saturating_sub(1))
            .await
            .map(|m| (m, num_items))?)
    }
}

fn last_wins_mega_filepaths(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
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
        ActiveModelTrait, ColumnTrait, DatabaseConnection, DbBackend, EntityTrait, IntoActiveModel,
        MockDatabase, MockExecResult, QueryFilter, TransactionTrait,
    };

    use super::MonoStorage;
    use crate::{
        callisto::{mega_blob, mega_refs},
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    fn mega_blob_row(id: i64, blob_id: &str, file_path: &str) -> mega_blob::Model {
        mega_blob::Model {
            id,
            blob_id: blob_id.to_owned(),
            name: String::new(),
            size: 0,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            file_path: file_path.to_owned(),
            pack_offset: 0,
            is_delta_in_pack: false,
            commit_id: String::new(),
        }
    }

    async fn insert_mega_blob(storage: &MonoStorage, model: mega_blob::Model) {
        mega_blob::Entity::insert(model.into_active_model())
            .exec(storage.get_connection())
            .await
            .expect("insert mega blob");
    }

    async fn filepath_of(storage: &MonoStorage, blob_id: &str) -> Option<String> {
        mega_blob::Entity::find()
            .filter(mega_blob::Column::BlobId.eq(blob_id))
            .one(storage.get_connection())
            .await
            .expect("load mega blob")
            .map(|blob| blob.file_path)
    }

    fn mock_storage(exec_count: usize) -> (MonoStorage, DatabaseConnection) {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_exec_results((0..exec_count).map(|_| MockExecResult::default()))
            .into_connection();
        let storage = MonoStorage {
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
    async fn update_blob_filepaths_has_one_update_per_chunk() {
        let (storage, connection) = mock_storage(0);
        storage
            .update_blob_filepaths(Vec::new())
            .await
            .expect("empty batch");
        assert_eq!(update_statement_count(connection), 0);

        let (storage, connection) = mock_storage(1);
        storage
            .update_blob_filepaths(filepath_pairs(
                <BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE,
            ))
            .await
            .expect("single chunk");
        assert_eq!(update_statement_count(connection), 1);

        let (storage, connection) = mock_storage(2);
        storage
            .update_blob_filepaths(filepath_pairs(
                <BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE + 1,
            ))
            .await
            .expect("two chunks");
        assert_eq!(update_statement_count(connection), 2);
    }

    #[tokio::test]
    async fn update_blob_filepaths_keeps_last_duplicate_and_ignores_missing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        insert_mega_blob(&storage, mega_blob_row(1, "duplicate", "old")).await;

        storage
            .update_blob_filepaths(vec![
                ("duplicate".to_owned(), "first.rs".to_owned()),
                ("missing".to_owned(), "missing.rs".to_owned()),
                ("duplicate".to_owned(), "last.rs".to_owned()),
            ])
            .await
            .expect("duplicate and missing ids are valid input");

        assert_eq!(
            filepath_of(&storage, "duplicate").await.as_deref(),
            Some("last.rs")
        );
        assert!(filepath_of(&storage, "missing").await.is_none());
    }

    #[tokio::test]
    async fn update_blob_filepaths_chunks_large_tree() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let count = <BaseStorage as StorageConnector>::BATCH_CHUNK_SIZE + 1;
        let mut pairs = Vec::with_capacity(count);
        for index in 0..count {
            let blob_id = format!("blob-{index:04}");
            insert_mega_blob(&storage, mega_blob_row(index as i64 + 1, &blob_id, "")).await;
            pairs.push((blob_id, format!("dir/file-{index}.rs")));
        }

        storage
            .update_blob_filepaths(pairs)
            .await
            .expect("update chunked batch");

        assert_eq!(
            filepath_of(&storage, "blob-0000").await.as_deref(),
            Some("dir/file-0.rs")
        );
        let last_blob_id = format!("blob-{:04}", count - 1);
        assert_eq!(
            filepath_of(&storage, &last_blob_id).await.as_deref(),
            Some(format!("dir/file-{}.rs", count - 1).as_str())
        );
    }

    #[tokio::test]
    async fn remove_ref_if_unchanged_is_atomic_and_path_scoped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("apply migrations");
        let storage = MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let ref_name = "refs/cl/path-cas";
        for path in ["/repo-a", "/repo-b"] {
            let reference = mega_refs::Model::new(
                path,
                ref_name.to_owned(),
                expected.to_owned(),
                String::new(),
                true,
            )
            .expect("test ID generator initialized");
            reference
                .into_active_model()
                .insert(storage.get_connection())
                .await
                .expect("insert path-scoped ref");
        }

        let txn = storage
            .get_connection()
            .begin()
            .await
            .expect("begin transaction");
        assert!(
            storage
                .remove_ref_if_unchanged("/repo-a", ref_name, expected, &txn)
                .await
                .expect("delete unchanged ref")
        );
        assert!(
            !storage
                .remove_ref_if_unchanged(
                    "/repo-a",
                    ref_name,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    &txn,
                )
                .await
                .expect("check mismatched ref")
        );
        txn.rollback().await.expect("rollback transaction");

        assert_eq!(
            mega_refs::Entity::find()
                .filter(mega_refs::Column::Path.eq("/repo-a"))
                .all(storage.get_connection())
                .await
                .expect("reload repo-a refs")
                .len(),
            1
        );
        assert_eq!(
            mega_refs::Entity::find()
                .filter(mega_refs::Column::Path.eq("/repo-b"))
                .all(storage.get_connection())
                .await
                .expect("reload repo-b refs")
                .len(),
            1
        );
        assert!(
            storage
                .remove_ref_if_unchanged("/repo-a", ref_name, expected, storage.get_connection(),)
                .await
                .expect("delete repo-a ref")
        );
        assert_eq!(
            mega_refs::Entity::find()
                .filter(mega_refs::Column::Path.eq("/repo-b"))
                .all(storage.get_connection())
                .await
                .expect("verify repo-b ref")
                .len(),
            1,
            "a path-scoped CAS delete must preserve same-named refs elsewhere"
        );
    }
}
