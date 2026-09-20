use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
};

use git_internal::internal::object::commit::Commit;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseTransaction, EntityTrait,
    IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set,
    TransactionTrait,
    prelude::Expr,
    sea_query::{LockType, OnConflict},
};

use crate::{
    callisto::{
        check_result, mega_cl, mega_cl_commits, mega_commit, mega_conversation, path_check_configs,
        sea_orm_active_enums::MergeStatusEnum,
    },
    common::errors::MegaError,
    contract::api::common::Pagination,
    jupiter::{
        model::common::{ItemDetails, ListParams},
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            stg_common::{combine_item_list, query_build::apply_sort},
        },
    },
};

#[derive(Clone)]
pub struct ClStorage {
    pub base: BaseStorage,
}

impl Deref for ClStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ClStorage {
    pub async fn get_open_cl_by_path(
        &self,
        path: &str,
        username: &str,
    ) -> Result<Option<mega_cl::Model>, MegaError> {
        let model = mega_cl::Entity::find()
            .filter(mega_cl::Column::Path.eq(path))
            .filter(mega_cl::Column::Username.eq(username))
            .filter(mega_cl::Column::Status.eq(MergeStatusEnum::Open))
            .one(self.get_connection())
            .await
            .unwrap();
        Ok(model)
    }

    pub async fn get_open_cls_by_path_prefix(
        &self,
        path_prefix: &str,
    ) -> Result<Vec<mega_cl::Model>, MegaError> {
        let models = mega_cl::Entity::find()
            .filter(mega_cl::Column::Path.starts_with(path_prefix))
            .filter(mega_cl::Column::Status.eq(MergeStatusEnum::Open))
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn get_open_cls(&self) -> Result<Vec<mega_cl::Model>, MegaError> {
        let models = mega_cl::Entity::find()
            .filter(mega_cl::Column::Status.eq(MergeStatusEnum::Open))
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn get_cl_list(
        &self,
        params: ListParams,
        page: Pagination,
    ) -> Result<(Vec<ItemDetails>, u64), MegaError> {
        let status = if params.status == "open" {
            vec![MergeStatusEnum::Open, MergeStatusEnum::Draft]
        } else if params.status == "closed" {
            vec![MergeStatusEnum::Closed, MergeStatusEnum::Merged]
        } else {
            vec![
                MergeStatusEnum::Open,
                MergeStatusEnum::Closed,
                MergeStatusEnum::Merged,
                MergeStatusEnum::Draft,
            ]
        };

        let base_query = mega_cl::Entity::find()
            .filter(mega_cl::Column::Status.is_in(status))
            .apply_if(params.author, |q, author| {
                q.filter(mega_cl::Column::Username.eq(author))
            });

        let mut sort_map = HashMap::new();
        sort_map.insert("created_at", mega_cl::Column::CreatedAt);
        sort_map.insert("updated_at", mega_cl::Column::UpdatedAt);

        let sort_field = params.sort_by.as_deref();
        let has_valid_sort = sort_field.and_then(|field| sort_map.get(field)).is_some();

        let mut sorted_query = apply_sort(base_query, sort_field, params.asc, &sort_map);

        if !has_valid_sort {
            sorted_query = sorted_query.order_by_desc(mega_cl::Column::Id);
        }

        let paginator = sorted_query.paginate(self.get_connection(), page.per_page);
        let total = paginator.num_items().await?;

        let cl_list = paginator.fetch_page(page.page - 1).await?;

        if cl_list.is_empty() {
            return Ok((vec![], 0));
        }

        let ids = cl_list.iter().map(|m| m.id).collect::<Vec<_>>();

        let conversations: Vec<(mega_cl::Model, Vec<mega_conversation::Model>)> =
            mega_cl::Entity::find()
                .filter(mega_cl::Column::Id.is_in(ids))
                .find_with_related(mega_conversation::Entity)
                .all(self.get_connection())
                .await?;

        let res = combine_item_list::<mega_cl::Entity>(cl_list, conversations);

        Ok((res, total))
    }

    pub async fn get_cl_suggestions_by_query(
        &self,
        query: &str,
    ) -> Result<Vec<mega_cl::Model>, MegaError> {
        let keyword = format!("%{query}%");
        let res = mega_cl::Entity::find()
            .filter(
                Condition::any()
                    .add(mega_cl::Column::Link.like(&keyword))
                    .add(mega_cl::Column::Title.like(&keyword)),
            )
            .limit(5)
            .all(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn get_cl(&self, link: &str) -> Result<Option<mega_cl::Model>, MegaError> {
        let model = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn get_cl_in_txn(
        &self,
        link: &str,
        txn: &DatabaseTransaction,
    ) -> Result<Option<mega_cl::Model>, MegaError> {
        let model = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .one(txn)
            .await?;
        Ok(model)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_cl(
        &self,
        path: &str,
        link: &str,
        title: &str,
        base_branch: &str,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> Result<String, MegaError> {
        self.new_cl_model(path, link, title, base_branch, from_hash, to_hash, username)
            .await
            .map(|res| res.link)
    }

    /// Create the CL row inside the caller's transaction (MC-04: the CL row
    /// write and the commit-listing rebuild share one transaction).
    #[allow(clippy::too_many_arguments)]
    pub async fn new_cl_model_in_txn(
        &self,
        path: &str,
        link: &str,
        title: &str,
        base_branch: &str,
        from_hash: &str,
        to_hash: &str,
        username: &str,
        txn: &DatabaseTransaction,
    ) -> Result<mega_cl::Model, MegaError> {
        let model = mega_cl::Model::new(
            path.to_owned(),
            title.to_owned(),
            link.to_owned(),
            base_branch.to_owned(),
            from_hash.to_owned(),
            to_hash.to_owned(),
            username.to_owned(),
        );
        let res = model.into_active_model().insert(txn).await?;
        Ok(res)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_cl_model(
        &self,
        path: &str,
        link: &str,
        title: &str,
        base_branch: &str,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> Result<mega_cl::Model, MegaError> {
        let model = mega_cl::Model::new(
            path.to_owned(),
            title.to_owned(),
            link.to_owned(),
            base_branch.to_owned(),
            from_hash.to_owned(),
            to_hash.to_owned(),
            username.to_owned(),
        );
        let res = model
            .into_active_model()
            .insert(self.get_connection())
            .await?;
        Ok(res)
    }

    /// Create a new CL with Draft status (for Buck upload)
    pub async fn new_cl_draft(
        &self,
        path: &str,
        link: &str,
        title: &str,
        base_branch: &str,
        from_hash: &str,
        username: &str,
    ) -> Result<String, MegaError> {
        let model = mega_cl::Model::new_draft(
            path.to_owned(),
            title.to_owned(),
            link.to_owned(),
            base_branch.to_owned(),
            from_hash.to_owned(),
            username.to_owned(),
        );
        let res = model
            .into_active_model()
            .insert(self.get_connection())
            .await?;
        Ok(res.link)
    }

    pub async fn edit_title(&self, link: &str, title: &str) -> Result<(), MegaError> {
        mega_cl::Entity::update_many()
            .col_expr(mega_cl::Column::Title, Expr::value(title))
            .col_expr(
                mega_cl::Column::UpdatedAt,
                Expr::value(chrono::Utc::now().naive_utc()),
            )
            .filter(mega_cl::Column::Link.eq(link))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn close_cl(&self, model: mega_cl::Model) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.status = Set(MergeStatusEnum::Closed);
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn reopen_cl(&self, model: mega_cl::Model) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.status = Set(MergeStatusEnum::Open);
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn update_cl_status(
        &self,
        model: mega_cl::Model,
        status: MergeStatusEnum,
    ) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.status = Set(status);
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await?;
        Ok(())
    }

    pub async fn merge_cl(&self, model: mega_cl::Model) -> Result<(), MegaError> {
        if !self.merge_cl_cas(self.get_connection(), &model).await? {
            return Err(MegaError::Other(
                "CL revision CAS missed (concurrent merge or rebase)".into(),
            ));
        }
        Ok(())
    }

    /// Merge status write inside `txn` with revision CAS. `false` = 0 rows.
    pub async fn merge_cl_in_txn(
        &self,
        model: mega_cl::Model,
        txn: &DatabaseTransaction,
    ) -> Result<bool, MegaError> {
        self.merge_cl_cas(txn, &model).await
    }

    async fn merge_cl_cas<C: ConnectionTrait>(
        &self,
        conn: &C,
        model: &mega_cl::Model,
    ) -> Result<bool, MegaError> {
        let now = chrono::Utc::now().naive_utc();
        let rows = conn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE mega_cl
                   SET status = 'merged'::merge_status_enum,
                       merge_date = $1,
                       updated_at = $2,
                       revision = revision + 1
                 WHERE link = $3
                   AND revision = $4
                   AND status = 'open'::merge_status_enum
                   AND to_hash = $5
                "#,
                [
                    sea_orm::Value::from(now),
                    sea_orm::Value::from(now),
                    sea_orm::Value::from(model.link.clone()),
                    sea_orm::Value::from(model.revision),
                    sea_orm::Value::from(model.to_hash.clone()),
                ],
            ))
            .await?;
        Ok(rows.rows_affected() == 1)
    }

    /// The to-hash advance inside the caller's transaction (MC-04: the CL row
    /// write and the commit-listing rebuild share one transaction — a listing
    /// failure must roll the advance back). Errors propagate instead of
    /// unwrapping. (The legacy pool-connection variant was removed in MC-04 R1
    /// once this became the only caller path.)
    pub async fn update_cl_to_hash_in_txn(
        &self,
        model: mega_cl::Model,
        to_hash: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        if !self
            .cas_update_cl_hashes_in_txn(&model, &model.from_hash, to_hash, txn)
            .await?
        {
            return Err(MegaError::Other(
                "CL revision CAS missed (concurrent merge or rebase)".into(),
            ));
        }
        Ok(())
    }

    /// Advance both CL hashes inside the caller's transaction (MC-04 R2:
    /// update_branch's hash move and the commit-listing rebuild/clear share
    /// one transaction). The legacy pool-connection variant was removed once
    /// this became the only caller path.
    pub async fn update_cl_hash_in_txn(
        &self,
        model: mega_cl::Model,
        from_hash: &str,
        to_hash: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        if !self
            .cas_update_cl_hashes_in_txn(&model, from_hash, to_hash, txn)
            .await?
        {
            return Err(MegaError::Other(
                "CL revision CAS missed (concurrent merge or rebase)".into(),
            ));
        }
        Ok(())
    }

    pub async fn cas_update_cl_hashes_in_txn(
        &self,
        model: &mega_cl::Model,
        from_hash: &str,
        to_hash: &str,
        txn: &DatabaseTransaction,
    ) -> Result<bool, MegaError> {
        let now = chrono::Utc::now().naive_utc();
        let rows = txn
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r#"
                UPDATE mega_cl
                   SET from_hash = $1,
                       to_hash = $2,
                       updated_at = $3,
                       revision = revision + 1
                 WHERE link = $4
                   AND revision = $5
                   AND status = 'open'::merge_status_enum
                   AND to_hash = $6
                "#,
                [
                    sea_orm::Value::from(from_hash.to_owned()),
                    sea_orm::Value::from(to_hash.to_owned()),
                    sea_orm::Value::from(now),
                    sea_orm::Value::from(model.link.clone()),
                    sea_orm::Value::from(model.revision),
                    sea_orm::Value::from(model.to_hash.clone()),
                ],
            ))
            .await?;
        Ok(rows.rows_affected() == 1)
    }

    /// Delete a CL's commit listing inside the caller's transaction (MC-04 R2:
    /// the no-change rebase branch clears it — the new range has no
    /// first-parent-valid chain, and a stale listing would fail the read-side
    /// coverage check).
    pub async fn delete_cl_commits_in_txn(
        &self,
        link: &str,
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        mega_cl_commits::Entity::delete_many()
            .filter(mega_cl_commits::Column::ClLink.eq(link))
            .exec(txn)
            .await?;
        Ok(())
    }

    pub async fn update_cl_title(
        &self,
        model: mega_cl::Model,
        title: &str,
    ) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.title = Set(title.to_owned());
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await?;
        Ok(())
    }

    pub async fn get_checks_config_by_path(
        &self,
        _: &str,
    ) -> Result<Vec<path_check_configs::Model>, MegaError> {
        let models = path_check_configs::Entity::find()
            // .filter(path_check_configs::Column::Path.eq(path))
            .filter(path_check_configs::Column::Enabled.eq(true))
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn save_check_results(
        &self,
        models: Vec<check_result::Model>,
    ) -> Result<(), MegaError> {
        let models: Vec<check_result::ActiveModel> =
            models.into_iter().map(|m| m.into_active_model()).collect();
        check_result::Entity::insert_many(models)
            .on_conflict(
                OnConflict::columns(vec![
                    check_result::Column::ClLink,
                    check_result::Column::CheckTypeCode,
                ])
                .update_columns([
                    check_result::Column::CommitId,
                    check_result::Column::Status,
                    check_result::Column::Message,
                ])
                .to_owned(),
            )
            .try_insert()
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn get_check_result(
        &self,
        cl_link: &str,
    ) -> Result<Vec<check_result::Model>, MegaError> {
        let models = check_result::Entity::find()
            .filter(check_result::Column::ClLink.eq(cl_link))
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    /// MC-04: idempotently replace a CL's commit listing inside the caller's
    /// transaction — delete all rows for the link, then batch-insert the
    /// rebuilt set (repeat/update pushes stay clean, no duplicate rows).
    ///
    /// The delete+insert pair must be atomic with the CL row write (the
    /// caller's transaction), which is why this only exists in a txn form: a
    /// plain insert uses no on-conflict clause, so any real fault (e.g. a
    /// constraint violation) aborts the whole transaction and the CL update
    /// rolls back with it.
    pub async fn save_cl_commits_in_txn(
        &self,
        link: &str,
        commits: &[Commit],
        txn: &DatabaseTransaction,
    ) -> Result<(), MegaError> {
        mega_cl_commits::Entity::delete_many()
            .filter(mega_cl_commits::Column::ClLink.eq(link))
            .exec(txn)
            .await?;
        let now = chrono::Utc::now().naive_utc();
        let save_models: Vec<mega_cl_commits::ActiveModel> = commits
            .iter()
            .map(|commit| mega_cl_commits::ActiveModel {
                cl_link: Set(link.to_string()),
                commit_sha: Set(commit.id.to_string()),
                author_name: Set(commit.author.name.clone()),
                author_email: Set(commit.author.email.clone()),
                message: Set(commit.format_message()),
                created_at: Set(now),
                updated_at: Set(now),
            })
            .collect();
        mega_cl_commits::Entity::insert_many(save_models)
            .exec(txn)
            .await?;
        Ok(())
    }

    /// MC-04 R1 P1-2: conditionally replace a CL's commit listing — only if
    /// the CL still sits at the `(from_hash, to_hash)` the caller collected
    /// `commits` for. The CL row is re-read `FOR UPDATE` inside the
    /// transaction, which serializes against a concurrent push's own CL
    /// advance: an interleaved advance makes this write abandon (returns
    /// `false`) instead of letting a stale listing overwrite the fresh one.
    pub async fn rebuild_cl_commits_if_current(
        &self,
        link: &str,
        expected_from_hash: &str,
        expected_to_hash: &str,
        commits: &[Commit],
    ) -> Result<bool, MegaError> {
        let txn = self.get_connection().begin().await?;
        let current = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("CL {link} disappeared during listing rebuild"))
            })?;
        if current.from_hash != expected_from_hash || current.to_hash != expected_to_hash {
            // The CL moved on while the chain was being collected — abandon
            // (the concurrent push's own rebuild owns the fresh listing).
            txn.rollback().await?;
            return Ok(false);
        }
        self.save_cl_commits_in_txn(link, commits, &txn).await?;
        txn.commit().await.map_err(MegaError::Db)?;
        Ok(true)
    }

    /// Whether the CL's listing currently contains `commit_sha` — the
    /// pipeline's staleness probe (a listing that does not cover the CL's
    /// current `to_hash` is missing or stale and gets rebuilt).
    pub async fn cl_commits_contain(&self, link: &str, sha: &str) -> Result<bool, MegaError> {
        let found = mega_cl_commits::Entity::find()
            .filter(mega_cl_commits::Column::ClLink.eq(link))
            .filter(mega_cl_commits::Column::CommitSha.eq(sha))
            .one(self.get_connection())
            .await?;
        Ok(found.is_some())
    }

    /// MC-04 read side: the CL's commit listing in chain order — oldest first
    /// (the base-ward member first, the CL tip last), which is the display
    /// order of the commits UI.
    ///
    /// The table is a set (no order column, by design): the chain order is
    /// rebuilt at read time from each commit's `parents_id`. The tip is the
    /// only listed sha that no other listed row names as a parent; from there
    /// each step follows the first parent until the walk leaves the listing.
    /// Commit rows are fetched in one `IN` batch — no N+1. Fail-closed, never
    /// a silently truncated chain: a listed sha without a `mega_commit` row,
    /// corrupt `parents_id`, a disconnected listing (zero or multiple tips, or
    /// a walk that cannot cover every row), or a parent cycle all error — and
    /// (Codex R1 P1-1) a non-empty listing must exactly cover the CL's
    /// `(from_hash, to_hash]`: the unique tip must equal the CL's current
    /// `to_hash`, and the first parent that leaves the listing must equal the
    /// CL's frozen `from_hash`. A listing that drops its oldest member (or is
    /// left behind by a `to_hash` advance) fails closed instead of reading as
    /// a truncated chain. An empty listing short-circuits before the CL read
    /// (legacy CLs have no rows; the router answers unknown links with 404).
    pub async fn get_cl_commits(
        &self,
        link: &str,
    ) -> Result<Vec<mega_cl_commits::Model>, MegaError> {
        let rows = mega_cl_commits::Entity::find()
            .filter(mega_cl_commits::Column::ClLink.eq(link))
            .all(self.get_connection())
            .await?;
        if rows.is_empty() {
            return Ok(rows);
        }
        // Codex R1 P1-1: the listing must provably cover the CL's current
        // `(from_hash, to_hash]` — validated after the walk below.
        let cl = self.get_cl(link).await?.ok_or_else(|| {
            MegaError::Other(format!(
                "CL {link} not found but a commit listing exists for it; fail-closed"
            ))
        })?;
        let by_sha: HashMap<String, mega_cl_commits::Model> = rows
            .iter()
            .map(|row| (row.commit_sha.clone(), row.clone()))
            .collect();
        let commit_rows = mega_commit::Entity::find()
            .filter(mega_commit::Column::CommitId.is_in(by_sha.keys().cloned().collect::<Vec<_>>()))
            .all(self.get_connection())
            .await?;
        let mut parents_of: HashMap<String, Vec<String>> = HashMap::new();
        for row in commit_rows {
            let parents: Vec<String> =
                serde_json::from_value(row.parents_id.clone()).map_err(|e| {
                    MegaError::Other(format!(
                        "corrupt parents_id for commit {}: {e}",
                        row.commit_id
                    ))
                })?;
            parents_of.insert(row.commit_id, parents);
        }
        for sha in by_sha.keys() {
            if !parents_of.contains_key(sha) {
                return Err(MegaError::Other(format!(
                    "CL {link} listing commit {sha} has no commit object in storage; \
                     the listing is incomplete (fail-closed)"
                )));
            }
        }
        // The tip is the only listed sha no other listed commit names as a
        // parent; zero or multiple tips mean the listing is not a single
        // chain.
        let mut named_as_parent: HashSet<&String> = HashSet::new();
        for parents in parents_of.values() {
            for parent in parents {
                if by_sha.contains_key(parent) {
                    named_as_parent.insert(parent);
                }
            }
        }
        let tips: Vec<&String> = by_sha
            .keys()
            .filter(|sha| !named_as_parent.contains(*sha))
            .collect();
        let [tip] = tips.as_slice() else {
            return Err(MegaError::Other(format!(
                "CL {link} listing is not a single chain ({} tips); fail-closed",
                tips.len()
            )));
        };
        let mut ordered = Vec::with_capacity(rows.len());
        let mut visited = HashSet::new();
        let mut current = (*tip).clone();
        // The first parent that leaves the listing — must be the CL's frozen
        // `from_hash` (the baseline is not a listing member).
        let exit_parent: Option<String>;
        loop {
            if !visited.insert(current.clone()) {
                return Err(MegaError::Other(format!(
                    "CL {link} listing has a parent cycle at {current}; fail-closed"
                )));
            }
            ordered.push(by_sha[&current].clone());
            match parents_of[&current].first() {
                None => {
                    exit_parent = None;
                    break;
                }
                Some(parent) if !by_sha.contains_key(parent) => {
                    exit_parent = Some(parent.clone());
                    break;
                }
                Some(parent) => current = parent.clone(),
            }
        }
        if ordered.len() != rows.len() {
            return Err(MegaError::Other(format!(
                "CL {link} listing is disconnected ({} of {} rows reachable from the tip); \
                 fail-closed",
                ordered.len(),
                rows.len()
            )));
        }
        // Codex R1 P1-1, coverage checks: ① the unique tip is the CL's current
        // `to_hash`; ② the walk left the listing exactly at the CL's frozen
        // `from_hash`. Either failing means the listing is stale or missing
        // members — fail closed rather than return a truncated chain.
        if *tip != &cl.to_hash {
            return Err(MegaError::Other(format!(
                "CL {link} listing tip {tip} does not match the CL's current to_hash {}; \
                 the listing is stale (fail-closed)",
                cl.to_hash
            )));
        }
        if exit_parent.as_deref() != Some(cl.from_hash.as_str()) {
            return Err(MegaError::Other(format!(
                "CL {link} listing does not reach the frozen from_hash {} (walk exited at \
                 {exit_parent:?}); the listing is missing members (fail-closed)",
                cl.from_hash
            )));
        }
        ordered.reverse();
        Ok(ordered)
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, TransactionTrait};
    use tempfile::TempDir;

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations,
        storage::base_storage::{BaseStorage, StorageConnector},
        tests::test_db_connection,
        utils::converter::FromMegaModel,
    };

    /// UN-10 regression: `get_cl` looks a CL up by `link` alone. After the
    /// unique index lands the lookup is an index equality probe, and the
    /// uniqueness it relies on is now enforced by the database rather than
    /// assumed.
    #[tokio::test]
    async fn get_cl_resolves_a_link_to_its_single_row() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");

        conn.execute_unprepared(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES (950001, 'UN10GETCL', 'un10 get_cl', 'open', '/', 'from', 'to', now(), \
             now(), 'un10-user', 'main')",
        )
        .await
        .expect("insert CL row");

        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };

        let found = storage
            .get_cl("UN10GETCL")
            .await
            .expect("get_cl query")
            .expect("the seeded CL is found by its link");
        assert_eq!(found.id, 950_001);
        assert_eq!(found.path, "/");

        assert!(
            storage
                .get_cl("UN10MISSING")
                .await
                .expect("get_cl query")
                .is_none(),
            "an unknown link resolves to no row"
        );
    }

    /// MC-04 fixtures: a fabricated 40-hex object id, a commit row, and a
    /// `Commit` object with a known author/message for listing assertions.
    fn mc04_sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn mc04_commit_row(commit_sha: &str, parents: &[String]) -> mega_commit::Model {
        mega_commit::Model {
            id: crate::callisto::entity_ext::generate_id(),
            commit_id: commit_sha.to_string(),
            tree: mc04_sha(900_000),
            parents_id: serde_json::json!(parents),
            author: Some("author Test User <mc04@example.invalid> 1750000000 +0000".to_string()),
            committer: Some(
                "committer Test User <mc04@example.invalid> 1750000000 +0000".to_string(),
            ),
            content: Some(format!("mc04 message for {commit_sha}")),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    fn mc04_commit(commit_sha: &str, parents: &[String]) -> Commit {
        Commit::from_mega_model(mc04_commit_row(commit_sha, parents))
    }

    /// Insert commit rows `sha(start + 1) ..= sha(start + len)` as a linear
    /// first-parent chain rooted at `sha(start)`; the root row itself is also
    /// inserted (listings stop at it as the from_hash baseline).
    async fn mc04_insert_chain(conn: &sea_orm::DatabaseConnection, start: u64, len: u64) {
        let mut rows = vec![mc04_commit_row(&mc04_sha(start), &[])];
        rows.extend(
            (1..=len).map(|i| mc04_commit_row(&mc04_sha(start + i), &[mc04_sha(start + i - 1)])),
        );
        let models: Vec<mega_commit::ActiveModel> =
            rows.into_iter().map(|m| m.into_active_model()).collect();
        mega_commit::Entity::insert_many(models)
            .exec(conn)
            .await
            .expect("insert mc04 chain");
    }

    fn mc04_listing_shas(rows: &[mega_cl_commits::Model]) -> Vec<String> {
        rows.iter().map(|r| r.commit_sha.clone()).collect()
    }

    /// Seed an open CL row at `(from, to)` (listings validate their coverage
    /// against the CL row — Codex R1 P1-1).
    async fn mc04_seed_cl(storage: &ClStorage, link: &str, from: &str, to: &str) {
        storage
            .new_cl_model("/", link, "mc04 test cl", "main", from, to, "mc04-user")
            .await
            .expect("seed CL row");
    }

    /// Write a listing in its own transaction, expecting the CL to sit at
    /// `(from, to)` (the conditional rebuild is the only production shape).
    async fn mc04_write_listing(
        storage: &ClStorage,
        link: &str,
        from: &str,
        to: &str,
        chain: &[Commit],
    ) {
        let wrote = storage
            .rebuild_cl_commits_if_current(link, from, to, chain)
            .await
            .expect("conditional rebuild");
        assert!(wrote, "the seeded CL sits at the expected range");
    }

    /// Full-chain rebuild + read-back: the listing for `(from, to]` covers
    /// exactly the chain members (the baseline excluded), in oldest-first
    /// chain order, with author/message fields from the commit metadata.
    #[tokio::test]
    async fn cl_commits_full_chain_rebuild_and_read_order() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 100, 3).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to) = (mc04_sha(100), mc04_sha(103));
        mc04_seed_cl(&storage, "CLMC04A", &from, &to).await;
        let chain: Vec<Commit> = (1..=3u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(100 + i), &[mc04_sha(100 + i - 1)]))
            .collect();

        mc04_write_listing(&storage, "CLMC04A", &from, &to, &chain).await;

        let rows = storage
            .get_cl_commits("CLMC04A")
            .await
            .expect("read listing");
        assert_eq!(
            mc04_listing_shas(&rows),
            vec![mc04_sha(101), mc04_sha(102), mc04_sha(103)],
            "listing must be the full chain in oldest-first chain order"
        );
        let first = &rows[0];
        assert_eq!(first.author_name, "Test User");
        assert_eq!(first.author_email, "mc04@example.invalid");
        assert_eq!(first.message, format!("mc04 message for {}", mc04_sha(101)));
        assert!(
            !rows.iter().any(|r| r.commit_sha == from),
            "the from_hash baseline is not a listing member"
        );
        assert_eq!(rows.last().expect("tip row").commit_sha, to);
    }

    /// Update rebuild: after the CL advances (frozen from, new tip), the
    /// rebuilt listing is the complete `(from, to]` chain — not the push's
    /// increment — and repeated rebuilds stay idempotent (delete + insert, no
    /// duplicate rows).
    #[tokio::test]
    async fn cl_commits_full_chain_update_rebuilds_complete_chain() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 200, 4).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to1) = (mc04_sha(200), mc04_sha(202));
        mc04_seed_cl(&storage, "CLMC04B", &from, &to1).await;

        let chain1: Vec<Commit> = (1..=2u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(200 + i), &[mc04_sha(200 + i - 1)]))
            .collect();
        mc04_write_listing(&storage, "CLMC04B", &from, &to1, &chain1).await;

        // The CL advances to sha(204) — mirror the production update path:
        // the to_hash advance and the listing rebuild in one transaction.
        let cl = storage
            .get_cl("CLMC04B")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        let to2 = mc04_sha(204);
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .update_cl_to_hash_in_txn(cl, &to2, &txn)
            .await
            .expect("advance to_hash");
        txn.commit().await.expect("commit advance");

        // The rebuilt listing must cover the whole frozen range, not just the
        // two new commits.
        let chain2: Vec<Commit> = (1..=4u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(200 + i), &[mc04_sha(200 + i - 1)]))
            .collect();
        mc04_write_listing(&storage, "CLMC04B", &from, &to2, &chain2).await;

        let rows = storage
            .get_cl_commits("CLMC04B")
            .await
            .expect("read listing");
        assert_eq!(
            mc04_listing_shas(&rows),
            vec![mc04_sha(201), mc04_sha(202), mc04_sha(203), mc04_sha(204)],
            "an update rebuild must list the complete chain from the frozen from_hash"
        );

        // Rebuilding the identical set again changes nothing (idempotent;
        // timestamps refresh, so compare the content columns).
        mc04_write_listing(&storage, "CLMC04B", &from, &to2, &chain2).await;
        let rows2 = storage.get_cl_commits("CLMC04B").await.expect("read again");
        assert_eq!(
            mc04_listing_shas(&rows),
            mc04_listing_shas(&rows2),
            "a repeated rebuild must be content-identical"
        );
        assert_eq!(rows[0].message, rows2[0].message);
    }

    /// Fail-closed: a chain member whose commit row is missing fails the
    /// write-side walk, and a listing row whose commit object is gone fails
    /// the read-side sort — never a truncated chain.
    #[tokio::test]
    async fn cl_commits_full_chain_missing_row_fails_closed() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 300, 3).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to) = (mc04_sha(300), mc04_sha(303));
        mc04_seed_cl(&storage, "CLMC04C", &from, &to).await;

        // Write side: collect_cl_chain lives in code_edit/model.rs; here the
        // read side must fail closed on a missing commit object.
        let chain: Vec<Commit> = (1..=3u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(300 + i), &[mc04_sha(300 + i - 1)]))
            .collect();
        mc04_write_listing(&storage, "CLMC04C", &from, &to, &chain).await;
        mega_commit::Entity::delete_many()
            .filter(mega_commit::Column::CommitId.eq(mc04_sha(302)))
            .exec(storage.get_connection())
            .await
            .expect("delete a chain member row");

        let err = storage
            .get_cl_commits("CLMC04C")
            .await
            .expect_err("a listing row without a commit object must fail closed");
        assert!(err.to_string().contains("fail-closed"), "{err}");
        assert!(err.to_string().contains(&mc04_sha(302)), "{err}");
    }

    /// Codex R1 P1-1 (missing oldest member): the listing dropped its
    /// base-ward member, so the walk exits at a parent that is NOT the CL's
    /// frozen `from_hash` — fail closed instead of returning the truncated
    /// chain.
    #[tokio::test]
    async fn cl_commits_full_chain_missing_oldest_member_fails_closed() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 500, 3).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to) = (mc04_sha(500), mc04_sha(503));
        mc04_seed_cl(&storage, "CLMC04D", &from, &to).await;

        // The listing covers only [502, 503] — the oldest member 501 is
        // missing. (Direct txn write: the conditional rebuild is the
        // production shape; here we stage the corrupt state directly.)
        let partial: Vec<Commit> = (2..=3u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(500 + i), &[mc04_sha(500 + i - 1)]))
            .collect();
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .save_cl_commits_in_txn("CLMC04D", &partial, &txn)
            .await
            .expect("stage partial listing");
        txn.commit().await.expect("commit staging txn");

        let err = storage
            .get_cl_commits("CLMC04D")
            .await
            .expect_err("a listing missing its oldest member must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("does not reach the frozen from_hash"), "{msg}");
        assert!(msg.contains(&from), "{msg}");
    }

    /// Codex R1 P1-1 (boundary mismatch): the listing's tip lags the CL's
    /// current `to_hash` (stale listing) — fail closed.
    #[tokio::test]
    async fn cl_commits_full_chain_stale_tip_fails_closed() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 600, 3).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to) = (mc04_sha(600), mc04_sha(603));
        mc04_seed_cl(&storage, "CLMC04E", &from, &to).await;

        // The listing was built for the previous tip 602; the CL has since
        // advanced to 603.
        let stale: Vec<Commit> = (1..=2u64)
            .rev()
            .map(|i| mc04_commit(&mc04_sha(600 + i), &[mc04_sha(600 + i - 1)]))
            .collect();
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .save_cl_commits_in_txn("CLMC04E", &stale, &txn)
            .await
            .expect("stage stale listing");
        txn.commit().await.expect("commit staging txn");

        let err = storage
            .get_cl_commits("CLMC04E")
            .await
            .expect_err("a listing whose tip lags to_hash must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match the CL's current to_hash"),
            "{msg}"
        );
        assert!(msg.contains(&mc04_sha(602)), "{msg}");
    }

    /// Codex R1 P1-2 (convergence): a stale backfill that collected the chain
    /// for the old tip must abandon when the CL has since advanced — the
    /// conditional rebuild re-reads the CL row under FOR UPDATE, so the fresh
    /// listing written by the newer push is never overwritten.
    #[tokio::test]
    async fn cl_commits_txn_fault_stale_backfill_abandons_when_cl_advanced() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 700, 2).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let (from, to_old, to_new) = (mc04_sha(700), mc04_sha(701), mc04_sha(702));
        mc04_seed_cl(&storage, "CLMC04G", &from, &to_old).await;

        // The stale backfill collected the old chain for (from → to_old).
        let stale_chain: Vec<Commit> = vec![mc04_commit(&to_old, std::slice::from_ref(&from))];

        // The interleaved newer push advances the CL to to_new and writes the
        // fresh listing in the same transaction (the production update shape).
        let cl = storage
            .get_cl("CLMC04G")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        let fresh_chain: Vec<Commit> = vec![
            mc04_commit(&to_new, std::slice::from_ref(&to_old)),
            mc04_commit(&to_old, std::slice::from_ref(&from)),
        ];
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .update_cl_to_hash_in_txn(cl, &to_new, &txn)
            .await
            .expect("advance to_hash");
        storage
            .save_cl_commits_in_txn("CLMC04G", &fresh_chain, &txn)
            .await
            .expect("fresh listing write");
        txn.commit().await.expect("commit advance txn");

        // The stale backfill lands now — it must abandon, not overwrite.
        let wrote = storage
            .rebuild_cl_commits_if_current("CLMC04G", &from, &to_old, &stale_chain)
            .await
            .expect("conditional rebuild");
        assert!(!wrote, "a stale backfill must abandon once the CL advanced");

        let rows = storage
            .get_cl_commits("CLMC04G")
            .await
            .expect("read final listing");
        assert_eq!(
            mc04_listing_shas(&rows),
            vec![to_old.clone(), to_new.clone()],
            "the fresh listing must survive the stale backfill"
        );
    }

    /// Unknown links read as an empty listing (legacy CLs have no rows;
    /// MC-05's contract answers them with an empty list, not an error).
    #[tokio::test]
    async fn cl_commits_full_chain_empty_listing_reads_empty() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let rows = storage
            .get_cl_commits("CLMC04NONE")
            .await
            .expect("unknown link is an empty listing, not an error");
        assert!(rows.is_empty());
    }

    /// Fault injection (AC: 同事务回滚): a listing batch with a duplicate
    /// primary key fails the insert; because the CL's to_hash advance shares
    /// the transaction, the advance rolls back with it — and a retry of the
    /// same orchestration then lands both.
    #[tokio::test]
    async fn cl_commits_txn_fault_rolls_back_cl_update() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        mc04_insert_chain(&conn, 400, 2).await;
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let cl = storage
            .new_cl_model(
                "/",
                "CLMC04F",
                "fault injection",
                "main",
                &mc04_sha(400),
                &mc04_sha(401),
                "mc04-user",
            )
            .await
            .expect("seed CL");

        // The fault: a rebuilt batch containing the same commit twice violates
        // the (cl_link, commit_sha) primary key on the plain insert.
        let dup = mc04_commit(&mc04_sha(402), &[mc04_sha(401)]);
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .update_cl_to_hash_in_txn(cl.clone(), &mc04_sha(402), &txn)
            .await
            .expect("CL advance in txn");
        let err = storage
            .save_cl_commits_in_txn("CLMC04F", &[dup.clone(), dup.clone()], &txn)
            .await
            .expect_err("the duplicate-PK listing batch must fail");
        drop(txn); // rolls back: neither write may survive
        let _ = err;

        let after = storage
            .get_cl("CLMC04F")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        assert_eq!(
            after.to_hash,
            mc04_sha(401),
            "the CL advance must roll back with the failed listing write"
        );
        assert!(
            !storage
                .cl_commits_contain("CLMC04F", &mc04_sha(402))
                .await
                .expect("probe"),
            "the failed listing write must leave no rows"
        );

        // Retry the same orchestration with a clean batch: both land. The
        // listing is always the full `(from, to]` chain (production rebuilds
        // never write increments), so the clean batch covers both members.
        let full_chain = [dup.clone(), mc04_commit(&mc04_sha(401), &[mc04_sha(400)])];
        let txn = storage.get_connection().begin().await.expect("begin txn");
        storage
            .update_cl_to_hash_in_txn(cl.clone(), &mc04_sha(402), &txn)
            .await
            .expect("CL advance in retry txn");
        storage
            .save_cl_commits_in_txn("CLMC04F", &full_chain, &txn)
            .await
            .expect("clean listing write in retry txn");
        txn.commit().await.expect("commit retry txn");

        let after = storage
            .get_cl("CLMC04F")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        assert_eq!(
            after.to_hash,
            mc04_sha(402),
            "the retry must advance the CL"
        );
        let rows = storage
            .get_cl_commits("CLMC04F")
            .await
            .expect("read listing after retry");
        assert_eq!(
            mc04_listing_shas(&rows),
            vec![mc04_sha(401), mc04_sha(402)],
            "the retry listing is the full (from, to] chain"
        );
    }

    #[tokio::test]
    async fn merge_cl_revision_cas_misses_on_stale_token() {
        use crate::callisto::sea_orm_active_enums::MergeStatusEnum;

        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");
        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };
        let cl = storage
            .new_cl_model(
                "/",
                "CLREV01",
                "revision cas",
                "main",
                &mc04_sha(1),
                &mc04_sha(2),
                "rev-user",
            )
            .await
            .unwrap();
        assert_eq!(cl.revision, 0);
        storage.merge_cl(cl.clone()).await.unwrap();
        let merged = storage.get_cl("CLREV01").await.unwrap().unwrap();
        assert_eq!(merged.status, MergeStatusEnum::Merged);
        assert_eq!(merged.revision, 1);

        let conn = storage.get_connection();
        let txn = conn.begin().await.unwrap();
        let stale = mega_cl::Model {
            revision: 0,
            ..merged.clone()
        };
        let missed = storage
            .cas_update_cl_hashes_in_txn(&stale, &mc04_sha(3), &mc04_sha(4), &txn)
            .await
            .unwrap();
        assert!(!missed, "stale rebase after merge must miss");
        txn.rollback().await.unwrap();
        let still = storage.get_cl("CLREV01").await.unwrap().unwrap();
        assert_eq!(still.status, MergeStatusEnum::Merged);
        assert_eq!(still.to_hash, mc04_sha(2));

        let txn = conn.begin().await.unwrap();
        let missed_status = storage
            .cas_update_cl_hashes_in_txn(&merged, &mc04_sha(3), &mc04_sha(4), &txn)
            .await
            .unwrap();
        assert!(
            !missed_status,
            "rebase after merge must miss on status=Open even with current revision"
        );
        txn.rollback().await.unwrap();
    }
}
