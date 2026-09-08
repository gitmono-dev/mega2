use std::{collections::HashMap, ops::Deref, str::FromStr};

use futures::{StreamExt, stream::FuturesUnordered};
use git_internal::{
    hash::ObjectHash,
    internal::{
        metadata::EntryMeta,
        object::{
            commit::Commit,
            tree::{Tree, TreeItemMode},
        },
    },
};
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::Set,
    ColumnTrait, Condition, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait,
    IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
    sea_query::{Expr, LikeExpr, OnConflict},
};

use crate::{
    callisto::{
        mega_blob, mega_cl, mega_commit, mega_ref_tombstones, mega_refs, mega_tag, mega_tree,
    },
    common::{
        errors::MegaError,
        utils::{MEGA_BRANCH_NAME, escape_like},
    },
    contract::api::common::Pagination,
    jupiter::{
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            commit_binding_storage::CommitBindingStorage,
            user_storage::UserStorage,
        },
        utils::converter::{FromMegaModel, IntoMegaModel},
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

#[derive(Debug, Clone)]
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

    /// Transactional descendant cleanup (1.10 deliverable 5).
    ///
    /// Unlike [`Self::remove_none_cl_refs`] this:
    /// - accepts `&DatabaseTransaction` (usable inside B3);
    /// - uses a component-boundary `LIKE '{path}/%'` prefix;
    /// - escapes LIKE metacharacters via [`crate::common::utils::escape_like`].
    pub async fn remove_none_cl_refs_in_txn(
        &self,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        let normalized = if path.is_empty() { "/" } else { path };
        let pattern = if normalized == "/" {
            "/%".to_owned()
        } else {
            format!("{}/%", escape_like(normalized.trim_end_matches('/')))
        };
        mega_refs::Entity::delete_many()
            .filter(mega_refs::Column::Path.like(pattern))
            .filter(mega_refs::Column::Path.ne(normalized.to_owned()))
            .filter(mega_refs::Column::IsCl.eq(false))
            .exec(txn)
            .await?;
        Ok(())
    }

    pub async fn remove_ref(&self, refs: mega_refs::Model) -> Result<(), MegaError> {
        mega_refs::Entity::delete_by_id(refs.id)
            .exec(self.get_connection())
            .await?;
        Ok(())
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

    pub async fn get_refs_for_paths_and_cls_in_txn(
        &self,
        paths: &[&str],
        cls: Option<&[&str]>,
        txn: &DatabaseTransaction,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        let mut query = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.is_in(paths.iter().copied()))
            .order_by_asc(mega_refs::Column::RefName);

        if let Some(cls_values) = cls {
            query = query.filter(mega_refs::Column::RefName.is_in(cls_values.iter().copied()));
        } else {
            query = query.filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME));
        }

        let result = query.all(txn).await?;
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

    pub async fn get_main_ref_in_txn(
        &self,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<mega_refs::Model>, MegaError> {
        let result = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq(path))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
            .one(txn)
            .await?;
        Ok(result)
    }

    /// Point lookup of a tombstone by primary key `(path, ref_name)`.
    pub async fn get_tombstone(
        &self,
        path: &str,
        ref_name: &str,
    ) -> Result<Option<mega_ref_tombstones::Model>, MegaError> {
        self.get_tombstone_on(self.get_connection(), path, ref_name)
            .await
    }

    pub async fn get_tombstone_in_txn(
        &self,
        path: &str,
        ref_name: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<mega_ref_tombstones::Model>, MegaError> {
        self.get_tombstone_on(txn, path, ref_name).await
    }

    async fn get_tombstone_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        path: &str,
        ref_name: &str,
    ) -> Result<Option<mega_ref_tombstones::Model>, MegaError> {
        Ok(
            mega_ref_tombstones::Entity::find_by_id((path.to_owned(), ref_name.to_owned()))
                .one(conn)
                .await?,
        )
    }

    /// Tombstones under `path/` with a component boundary. Uses `escape_like`.
    pub async fn list_tombstones_under_prefix(
        &self,
        path: &str,
    ) -> Result<Vec<mega_ref_tombstones::Model>, MegaError> {
        let normalized = if path.is_empty() { "/" } else { path };
        let pattern = if normalized == "/" {
            "/%".to_owned()
        } else {
            format!("{}/%", escape_like(normalized.trim_end_matches('/')))
        };
        Ok(mega_ref_tombstones::Entity::find()
            .filter(mega_ref_tombstones::Column::Path.like(LikeExpr::new(pattern).escape('\\')))
            .filter(mega_ref_tombstones::Column::Path.ne(normalized.to_owned()))
            .all(self.get_connection())
            .await?)
    }

    /// Insert or replace a tombstone. Idempotent on `(path, ref_name)`.
    pub async fn upsert_tombstone(
        &self,
        path: &str,
        ref_name: &str,
        last_commit_hash: &str,
        last_tree_hash: &str,
    ) -> Result<(), MegaError> {
        self.upsert_tombstone_on(
            self.get_connection(),
            path,
            ref_name,
            last_commit_hash,
            last_tree_hash,
        )
        .await
    }

    pub async fn upsert_tombstone_in_txn(
        &self,
        path: &str,
        ref_name: &str,
        last_commit_hash: &str,
        last_tree_hash: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        self.upsert_tombstone_on(txn, path, ref_name, last_commit_hash, last_tree_hash)
            .await
    }

    async fn upsert_tombstone_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        path: &str,
        ref_name: &str,
        last_commit_hash: &str,
        last_tree_hash: &str,
    ) -> Result<(), MegaError> {
        let now = chrono::Utc::now().fixed_offset();
        let model = mega_ref_tombstones::ActiveModel {
            path: Set(path.to_owned()),
            ref_name: Set(ref_name.to_owned()),
            last_commit_hash: Set(last_commit_hash.to_owned()),
            last_tree_hash: Set(last_tree_hash.to_owned()),
            deleted_at: Set(now),
        };
        match mega_ref_tombstones::Entity::insert(model)
            .on_conflict(
                OnConflict::columns([
                    mega_ref_tombstones::Column::Path,
                    mega_ref_tombstones::Column::RefName,
                ])
                .update_columns([
                    mega_ref_tombstones::Column::LastCommitHash,
                    mega_ref_tombstones::Column::LastTreeHash,
                    mega_ref_tombstones::Column::DeletedAt,
                ])
                .to_owned(),
            )
            .exec(conn)
            .await
        {
            Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_tombstone(&self, path: &str, ref_name: &str) -> Result<(), MegaError> {
        self.delete_tombstone_on(self.get_connection(), path, ref_name)
            .await
    }

    pub async fn delete_tombstone_in_txn(
        &self,
        path: &str,
        ref_name: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        self.delete_tombstone_on(txn, path, ref_name).await
    }

    async fn delete_tombstone_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        path: &str,
        ref_name: &str,
    ) -> Result<(), MegaError> {
        mega_ref_tombstones::Entity::delete_by_id((path.to_owned(), ref_name.to_owned()))
            .exec(conn)
            .await?;
        Ok(())
    }

    /// Best-effort upgrade backfill: insert tombstones from an operator-supplied
    /// deletion list (path, ref_name, last_commit, last_tree). Not complete.
    pub async fn backfill_tombstones_best_effort(
        &self,
        entries: &[(String, String, String, String)],
    ) -> Result<u64, MegaError> {
        let mut n = 0u64;
        for (path, ref_name, commit, tree) in entries {
            self.upsert_tombstone(path, ref_name, commit, tree).await?;
            n += 1;
        }
        Ok(n)
    }

    /// Fail-closed default upgrade path: do not invent tombstones for
    /// pre-upgrade deletions. Returns 0.
    pub fn backfill_tombstones_fail_closed(&self) -> u64 {
        0
    }

    /// Write a tombstone from the current `main` ref at `path` and delete the
    /// ref row, in one transaction (B3 SAVEPOINT repair / reaper I3).
    pub async fn tombstone_and_delete_main_ref_in_txn(
        &self,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<bool, MegaError> {
        let Some(row) = self.get_main_ref_in_txn(path, txn).await? else {
            return Ok(false);
        };
        self.upsert_tombstone_in_txn(
            path,
            MEGA_BRANCH_NAME,
            &row.ref_commit_hash,
            &row.ref_tree_hash,
            txn,
        )
        .await?;
        mega_refs::Entity::delete_by_id(row.id).exec(txn).await?;
        Ok(true)
    }

    /// Parents for a lazy-materialized commit: tombstone `last_commit_hash` if
    /// present, otherwise an empty parent list (first materialization).
    pub async fn materialize_parents(
        &self,
        path: &str,
        ref_name: &str,
    ) -> Result<Vec<ObjectHash>, MegaError> {
        let Some(row) = self.get_tombstone(path, ref_name).await? else {
            return Ok(vec![]);
        };
        let hash = ObjectHash::from_str(&row.last_commit_hash)
            .map_err(|e| MegaError::Other(format!("tombstone parent hash: {e}")))?;
        Ok(vec![hash])
    }

    /// Main refs under `path/` with a component boundary (`path LIKE '{p}/%'`).
    /// Uses `escape_like` so `%`/`_`/`\` in path cannot broaden the match.
    pub async fn list_descendant_main_refs(
        &self,
        path: &str,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        self.list_descendant_main_refs_on(self.get_connection(), path)
            .await
    }

    pub async fn list_descendant_main_refs_in_txn(
        &self,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        self.list_descendant_main_refs_on(txn, path).await
    }

    async fn list_descendant_main_refs_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        path: &str,
    ) -> Result<Vec<mega_refs::Model>, MegaError> {
        let normalized = if path.is_empty() { "/" } else { path };
        // `LIKE '/%'` matches `/` itself because `%` may be empty — always
        // exclude the query path from "descendants".
        let pattern = if normalized == "/" {
            "/%".to_owned()
        } else {
            format!("{}/%", escape_like(normalized.trim_end_matches('/')))
        };
        let result = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.like(pattern))
            .filter(mega_refs::Column::Path.ne(normalized.to_owned()))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
            .filter(mega_refs::Column::IsCl.eq(false))
            .all(conn)
            .await?;
        Ok(result)
    }

    /// Strict non-root ancestors of `path` (excludes `path` itself and `/`).
    pub fn strict_non_root_ancestor_paths(path: &str) -> Vec<String> {
        use std::path::Path;
        Path::new(path)
            .ancestors()
            .skip(1)
            .filter_map(|a| {
                let s = a.to_str()?;
                if s.is_empty() || s == "/" {
                    None
                } else {
                    Some(s.to_owned())
                }
            })
            .collect()
    }

    pub async fn attach_materialization_precheck(&self, path: &str) -> Result<(), MegaError> {
        self.attach_materialization_precheck_on(self.get_connection(), path)
            .await
    }

    pub async fn attach_materialization_precheck_in_txn(
        &self,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        self.attach_materialization_precheck_on(txn, path).await
    }

    async fn attach_materialization_precheck_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        path: &str,
    ) -> Result<(), MegaError> {
        use crate::common::utils::canonicalize_mono_ref_path;
        let normalized = canonicalize_mono_ref_path(path)?;
        if normalized != "/" {
            let target = mega_refs::Entity::find()
                .filter(mega_refs::Column::Path.eq(normalized.clone()))
                .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
                .one(conn)
                .await?;
            if target.is_some() {
                return Err(MegaError::Other(format!(
                    "attach refused: path '{normalized}' already has a materialized main ref (I3)"
                )));
            }
            for ancestor in Self::strict_non_root_ancestor_paths(&normalized) {
                let row = mega_refs::Entity::find()
                    .filter(mega_refs::Column::Path.eq(ancestor.clone()))
                    .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
                    .one(conn)
                    .await?;
                if row.is_some() {
                    return Err(MegaError::Other(format!(
                        "attach refused: ancestor '{ancestor}' already has a materialized main ref (I3)"
                    )));
                }
            }
        }
        let descendants = self.list_descendant_main_refs_on(conn, &normalized).await?;
        if let Some(d) = descendants.first() {
            return Err(MegaError::Other(format!(
                "attach refused: descendant '{}' already has a materialized main ref (I3)",
                d.path
            )));
        }
        Ok(())
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
            );
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

    /// Sequential ref updates inside an existing transaction.
    ///
    /// Unlike [`Self::batch_update_by_path_concurrent`], this variant:
    /// - accepts `&DatabaseTransaction` (usable inside B3);
    /// - updates rows one-by-one (concurrency is meaningless in one txn);
    /// - returns [`MegaError::NotFound`] when a `(path, ref_name)` row is missing
    ///   instead of silently skipping it.
    pub async fn batch_update_by_path_in_txn(
        &self,
        txn: &DatabaseTransaction,
        updates: Vec<RefUpdateData>,
    ) -> Result<(), MegaError> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut condition = Condition::any();
        for update in &updates {
            condition = condition.add(
                Condition::all()
                    .add(mega_refs::Column::Path.eq(update.path.clone()))
                    .add(mega_refs::Column::RefName.eq(update.ref_name.clone())),
            );
        }

        let existing_refs: Vec<mega_refs::Model> =
            mega_refs::Entity::find().filter(condition).all(txn).await?;

        let ref_map: HashMap<(String, String), mega_refs::Model> = existing_refs
            .into_iter()
            .map(|r| ((r.path.clone(), r.ref_name.clone()), r))
            .collect();

        for update in updates {
            let key = (update.path.clone(), update.ref_name.clone());
            let Some(ref_data) = ref_map.get(&key) else {
                return Err(MegaError::NotFound(format!(
                    "mega_refs row missing for path='{}' ref_name='{}'",
                    update.path, update.ref_name
                )));
            };
            let mut active: mega_refs::ActiveModel = ref_data.clone().into();
            active.ref_commit_hash = Set(update.commit_id);
            active.ref_tree_hash = Set(update.tree_hash);
            active.updated_at = Set(chrono::Utc::now().naive_utc());
            active.update(txn).await?;
        }

        Ok(())
    }

    /// Insert or update a single `(path, ref_name)` row inside `txn`.
    ///
    /// Creation uses `is_cl = false` (main-line / path refs). CL refs continue
    /// to use [`Self::save_or_update_cl_ref_in_txn`].
    ///
    /// Implemented as a single `INSERT … ON CONFLICT (path, ref_name) DO UPDATE`
    /// so concurrent first-creators do not race into a unique-index abort.
    /// On conflict, `id` and `is_cl` are preserved; only tip hashes and
    /// `updated_at` change.
    pub async fn upsert_ref_by_path_in_txn(
        &self,
        txn: &DatabaseTransaction,
        update: RefUpdateData,
    ) -> Result<(), MegaError> {
        let new_ref = mega_refs::Model::new(
            update.path,
            update.ref_name,
            update.commit_id,
            update.tree_hash,
            false,
        );
        mega_refs::Entity::insert(new_ref.into_active_model())
            .on_conflict(
                OnConflict::columns([mega_refs::Column::Path, mega_refs::Column::RefName])
                    .update_columns([
                        mega_refs::Column::RefCommitHash,
                        mega_refs::Column::RefTreeHash,
                        mega_refs::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(txn)
            .await?;
        Ok(())
    }

    /// Sequential upsert of many refs inside one transaction (no silent skips).
    pub async fn batch_upsert_by_path_in_txn(
        &self,
        txn: &DatabaseTransaction,
        updates: Vec<RefUpdateData>,
    ) -> Result<(), MegaError> {
        for update in updates {
            self.upsert_ref_by_path_in_txn(txn, update).await?;
        }
        Ok(())
    }

    pub async fn update_blob_filepath(
        &self,
        blob_id: &str,
        file_path: &str,
    ) -> Result<(), MegaError> {
        if let Some(model) = mega_blob::Entity::find()
            .filter(mega_blob::Column::BlobId.eq(blob_id))
            .one(self.get_connection())
            .await?
        {
            let mut active: mega_blob::ActiveModel = model.into();

            active.file_path = Set(file_path.to_string());

            active.update(self.get_connection()).await?;
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

    /// Dual-condition CAS on the root `main` ref (path=`/`).
    ///
    /// Used by MonoWriteQueue B3 as the tripwire root write (including net-zero
    /// same-value updates). Returns `Ok(true)` when exactly one row was updated,
    /// `Ok(false)` on a CAS miss (bypass / concurrent writer).
    pub async fn cas_update_root_main_ref_in_txn(
        &self,
        txn: &DatabaseTransaction,
        expected_commit_hash: Option<&str>,
        expected_tree_hash: Option<&str>,
        new_commit_hash: &str,
        new_tree_hash: &str,
    ) -> Result<bool, MegaError> {
        let now = chrono::Utc::now().naive_utc();
        // NULL-safe expected match via IS NOT DISTINCT FROM semantics:
        // missing root is represented by expected_* = None.
        let rows = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE mega_refs
                   SET ref_commit_hash = $1,
                       ref_tree_hash = $2,
                       updated_at = $3
                 WHERE path = '/'
                   AND ref_name = $4
                   AND ref_commit_hash IS NOT DISTINCT FROM $5
                   AND ref_tree_hash IS NOT DISTINCT FROM $6
                "#,
                [
                    sea_orm::Value::from(new_commit_hash.to_owned()),
                    sea_orm::Value::from(new_tree_hash.to_owned()),
                    sea_orm::Value::from(now),
                    sea_orm::Value::from(MEGA_BRANCH_NAME.to_owned()),
                    sea_orm::Value::from(expected_commit_hash.map(str::to_owned)),
                    sea_orm::Value::from(expected_tree_hash.map(str::to_owned)),
                ],
            ))
            .await?;
        Ok(rows.rows_affected() == 1)
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
        let cl = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(cl_link))
            .one(conn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {}", cl_link)))?;

        let now = chrono::Utc::now().naive_utc();
        let rows = conn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE mega_cl
                   SET from_hash = $1,
                       to_hash = $2,
                       status = 'open'::merge_status_enum,
                       title = $3,
                       updated_at = $4,
                       revision = revision + 1
                 WHERE link = $5
                   AND revision = $6
                   AND status <> 'merged'::merge_status_enum
                "#,
                [
                    sea_orm::Value::from(from_hash.to_owned()),
                    sea_orm::Value::from(to_hash.to_owned()),
                    sea_orm::Value::from(commit_message.to_owned()),
                    sea_orm::Value::from(now),
                    sea_orm::Value::from(cl_link.to_owned()),
                    sea_orm::Value::from(cl.revision),
                ],
            ))
            .await?;
        if rows.rows_affected() != 1 {
            return Err(MegaError::Other(
                "CL revision CAS missed (concurrent merge or rebase)".into(),
            ));
        }

        mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(cl_link))
            .one(conn)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found after CAS: {cl_link}")))
    }

    pub async fn save_mega_commits(
        &self,
        commits: Vec<Commit>,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        let save_models: Vec<mega_commit::ActiveModel> = commits
            .into_iter()
            .map(|c| c.into_mega_model(EntryMeta::default()))
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
        self.get_tree_by_hash_on(self.get_connection(), hash).await
    }

    pub async fn get_tree_by_hash_in_txn(
        &self,
        hash: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<mega_tree::Model>, MegaError> {
        self.get_tree_by_hash_on(txn, hash).await
    }

    async fn get_tree_by_hash_on<C: ConnectionTrait>(
        &self,
        conn: &C,
        hash: &str,
    ) -> Result<Option<mega_tree::Model>, MegaError> {
        Ok(mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.eq(hash))
            .one(conn)
            .await?)
    }

    /// Walk `path` from `root_tree_hash`. `None` if a component is missing or
    /// is not a tree. `path == "/"` returns the root tree hash.
    pub async fn resolve_path_tree_hash_in_txn(
        &self,
        root_tree_hash: &str,
        path: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<String>, MegaError> {
        if path.is_empty() || path == "/" {
            return Ok(Some(root_tree_hash.to_owned()));
        }
        let Some(model) = self.get_tree_by_hash_in_txn(root_tree_hash, txn).await? else {
            return Ok(None);
        };
        let mut tree = Tree::from_mega_model(model);
        for component in path.split('/').filter(|c| !c.is_empty()) {
            let Some(item) = tree.tree_items.iter().find(|x| x.name == component) else {
                return Ok(None);
            };
            if item.mode != TreeItemMode::Tree {
                return Ok(None);
            }
            let Some(next) = self
                .get_tree_by_hash_in_txn(&item.id.to_string(), txn)
                .await?
            else {
                return Ok(None);
            };
            tree = Tree::from_mega_model(next);
        }
        Ok(Some(tree.id.to_string()))
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

#[cfg(test)]
mod tests {
    use sea_orm::TransactionTrait;

    use super::*;
    use crate::{
        common::utils::{MEGA_BRANCH_NAME, escape_like},
        jupiter::tests::test_storage,
    };

    #[test]
    fn escape_like_covers_percent_underscore_backslash() {
        assert_eq!(escape_like("%"), r"\%");
        assert_eq!(escape_like("_"), r"\_");
        assert_eq!(escape_like(r"\"), r"\\");
        assert_eq!(escape_like(r"a%b_c\d"), r"a\%b\_c\\d");
    }

    async fn seed_root_ref(mono: &MonoStorage) -> mega_refs::Model {
        let model = mega_refs::Model::new(
            "/",
            MEGA_BRANCH_NAME.to_owned(),
            "a".repeat(40),
            "b".repeat(40),
            false,
        );
        mono.save_refs(model.clone(), None).await.unwrap();
        model
    }

    #[tokio::test]
    async fn batch_update_by_path_in_txn_updates_and_rolls_back() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let root = seed_root_ref(&mono).await;
        let original_commit = root.ref_commit_hash.clone();
        let original_tree = root.ref_tree_hash.clone();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.batch_update_by_path_in_txn(
            &txn,
            vec![RefUpdateData {
                path: "/".into(),
                ref_name: MEGA_BRANCH_NAME.into(),
                commit_id: "c".repeat(40),
                tree_hash: "d".repeat(40),
            }],
        )
        .await
        .unwrap();

        // Uncommitted: outside the txn the old values remain visible.
        let outside = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(outside.ref_commit_hash, original_commit);
        assert_eq!(outside.ref_tree_hash, original_tree);

        txn.rollback().await.unwrap();
        let after = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(after.ref_commit_hash, original_commit);
        assert_eq!(after.ref_tree_hash, original_tree);
    }

    #[tokio::test]
    async fn batch_update_by_path_in_txn_errors_on_missing_row() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        let err = mono
            .batch_update_by_path_in_txn(
                &txn,
                vec![RefUpdateData {
                    path: "/does-not-exist".into(),
                    ref_name: MEGA_BRANCH_NAME.into(),
                    commit_id: "a".repeat(40),
                    tree_hash: "b".repeat(40),
                }],
            )
            .await
            .expect_err("missing row must not be skipped");
        assert!(
            matches!(err, MegaError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        txn.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn upsert_ref_by_path_in_txn_creates_and_updates() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.upsert_ref_by_path_in_txn(
            &txn,
            RefUpdateData {
                path: "/new/path".into(),
                ref_name: MEGA_BRANCH_NAME.into(),
                commit_id: "c".repeat(40),
                tree_hash: "d".repeat(40),
            },
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let created = mono.get_main_ref("/new/path").await.unwrap().unwrap();
        assert_eq!(created.ref_commit_hash, "c".repeat(40));
        assert!(!created.is_cl);

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.upsert_ref_by_path_in_txn(
            &txn,
            RefUpdateData {
                path: "/new/path".into(),
                ref_name: MEGA_BRANCH_NAME.into(),
                commit_id: "e".repeat(40),
                tree_hash: "f".repeat(40),
            },
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let updated = mono.get_main_ref("/new/path").await.unwrap().unwrap();
        assert_eq!(updated.ref_commit_hash, "e".repeat(40));
        assert_eq!(updated.ref_tree_hash, "f".repeat(40));
    }

    #[tokio::test]
    async fn batch_update_matches_concurrent_for_existing_rows() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.upsert_ref_by_path_in_txn(
            &txn,
            RefUpdateData {
                path: "/p".into(),
                ref_name: MEGA_BRANCH_NAME.into(),
                commit_id: "1".repeat(40),
                tree_hash: "2".repeat(40),
            },
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();

        let updates = vec![RefUpdateData {
            path: "/p".into(),
            ref_name: MEGA_BRANCH_NAME.into(),
            commit_id: "3".repeat(40),
            tree_hash: "4".repeat(40),
        }];

        mono.batch_update_by_path_concurrent(updates.clone())
            .await
            .unwrap();
        let after_concurrent = mono.get_main_ref("/p").await.unwrap().unwrap();

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.upsert_ref_by_path_in_txn(
            &txn,
            RefUpdateData {
                path: "/p".into(),
                ref_name: MEGA_BRANCH_NAME.into(),
                commit_id: "1".repeat(40),
                tree_hash: "2".repeat(40),
            },
        )
        .await
        .unwrap();
        mono.batch_update_by_path_in_txn(&txn, updates)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let after_txn = mono.get_main_ref("/p").await.unwrap().unwrap();

        assert_eq!(after_concurrent.ref_commit_hash, after_txn.ref_commit_hash);
        assert_eq!(after_concurrent.ref_tree_hash, after_txn.ref_tree_hash);
    }

    #[tokio::test]
    async fn cas_update_root_main_ref_net_zero_and_miss() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let root = seed_root_ref(&mono).await;

        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        let ok = mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                Some(&root.ref_commit_hash),
                Some(&root.ref_tree_hash),
                &root.ref_commit_hash,
                &root.ref_tree_hash,
            )
            .await
            .unwrap();
        assert!(ok, "net-zero same-value CAS must write exactly once");
        let miss = mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                Some(&"9".repeat(40)),
                Some(&root.ref_tree_hash),
                &"e".repeat(40),
                &"f".repeat(40),
            )
            .await
            .unwrap();
        assert!(!miss, "CAS miss must return false without a second write");
        txn.commit().await.unwrap();

        let after = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(after.ref_commit_hash, root.ref_commit_hash);
        assert_eq!(after.ref_tree_hash, root.ref_tree_hash);
    }

    #[tokio::test]
    async fn upsert_ref_by_path_in_txn_survives_concurrent_creates() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();

        let left = mono.clone();
        let right = mono.clone();
        let (res_a, res_b) = tokio::join!(
            async {
                let conn = left.get_connection();
                let txn = conn.begin().await.unwrap();
                left.upsert_ref_by_path_in_txn(
                    &txn,
                    RefUpdateData {
                        path: "/race".into(),
                        ref_name: MEGA_BRANCH_NAME.into(),
                        commit_id: "a".repeat(40),
                        tree_hash: "b".repeat(40),
                    },
                )
                .await?;
                txn.commit().await.map_err(MegaError::from)?;
                Ok::<(), MegaError>(())
            },
            async {
                let conn = right.get_connection();
                let txn = conn.begin().await.unwrap();
                right
                    .upsert_ref_by_path_in_txn(
                        &txn,
                        RefUpdateData {
                            path: "/race".into(),
                            ref_name: MEGA_BRANCH_NAME.into(),
                            commit_id: "c".repeat(40),
                            tree_hash: "d".repeat(40),
                        },
                    )
                    .await?;
                txn.commit().await.map_err(MegaError::from)?;
                Ok::<(), MegaError>(())
            },
        );

        res_a.expect("left upsert must succeed");
        res_b.expect("right upsert must succeed");
        let row = mono.get_main_ref("/race").await.unwrap().unwrap();
        // One of the two tip pairs wins; either is fine as long as a single row remains.
        assert!(
            (row.ref_commit_hash == "a".repeat(40) && row.ref_tree_hash == "b".repeat(40))
                || (row.ref_commit_hash == "c".repeat(40) && row.ref_tree_hash == "d".repeat(40))
        );
        assert!(!row.is_cl);
    }

    #[tokio::test]
    async fn remove_none_cl_refs_in_txn_uses_component_boundary_and_escapes_like() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        for path in ["/", "/a", "/a/b", "/ab", "/a%x"] {
            mono.save_refs(
                mega_refs::Model::new(
                    path,
                    MEGA_BRANCH_NAME.to_owned(),
                    "a".repeat(40),
                    "b".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        }
        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        mono.remove_none_cl_refs_in_txn("/a", &txn).await.unwrap();
        txn.commit().await.unwrap();

        assert!(mono.get_main_ref("/a").await.unwrap().is_some());
        assert!(
            mono.get_main_ref("/a/b").await.unwrap().is_none(),
            "descendant /a/b must be removed"
        );
        assert!(
            mono.get_main_ref("/ab").await.unwrap().is_some(),
            "sibling-prefix /ab must remain"
        );
        assert!(
            mono.get_main_ref("/a%x").await.unwrap().is_some(),
            "unrelated path with LIKE metacharacter must remain"
        );
    }

    #[tokio::test]
    async fn tp09_tombstone_table_exists_after_migration() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        assert!(
            mono.get_tombstone("/never", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_none()
        );
        mono.upsert_tombstone(
            "/project/foo",
            MEGA_BRANCH_NAME,
            &"a".repeat(40),
            &"b".repeat(40),
        )
        .await
        .unwrap();
        let row = mono
            .get_tombstone("/project/foo", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .expect("tombstone");
        assert_eq!(row.last_commit_hash, "a".repeat(40));
        assert_eq!(mono.backfill_tombstones_fail_closed(), 0);
        let n = mono
            .backfill_tombstones_best_effort(&[(
                "/project/bar".into(),
                MEGA_BRANCH_NAME.into(),
                "c".repeat(40),
                "d".repeat(40),
            )])
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn tp09_tombstone_prefix_query_uses_escape_like() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        for path in [
            "/project/my_lib/child",
            "/project/myXlib/child",
            "/project/my%lib/child",
        ] {
            mono.upsert_tombstone(path, MEGA_BRANCH_NAME, &"a".repeat(40), &"b".repeat(40))
                .await
                .unwrap();
        }
        let under = mono
            .list_tombstones_under_prefix("/project/my_lib")
            .await
            .unwrap();
        let paths: Vec<_> = under.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, vec!["/project/my_lib/child"]);
        assert!(
            !paths.contains(&"/project/myXlib/child"),
            "unescaped _ must not match myXlib/child"
        );
        assert!(
            !paths.contains(&"/project/my%lib/child"),
            "unescaped % must not match my%lib/child"
        );
    }

    #[tokio::test]
    async fn tp09_tombstone_and_delete_main_ref_is_atomic() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        mono.save_refs(
            mega_refs::Model::new(
                "/stale",
                MEGA_BRANCH_NAME.to_owned(),
                "s".repeat(40),
                "t".repeat(40),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let conn = mono.get_connection();
        let txn = conn.begin().await.unwrap();
        assert!(
            mono.tombstone_and_delete_main_ref_in_txn("/stale", &txn)
                .await
                .unwrap()
        );
        txn.rollback().await.unwrap();
        assert!(mono.get_main_ref("/stale").await.unwrap().is_some());
        assert!(
            mono.get_tombstone("/stale", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_none()
        );

        let txn = conn.begin().await.unwrap();
        mono.tombstone_and_delete_main_ref_in_txn("/stale", &txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(mono.get_main_ref("/stale").await.unwrap().is_none());
        let tomb = mono
            .get_tombstone("/stale", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tomb.last_commit_hash, "s".repeat(40));
    }
}
