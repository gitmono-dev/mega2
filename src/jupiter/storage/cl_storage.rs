use std::{collections::HashMap, ops::Deref};

use git_internal::internal::object::commit::Commit;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, IntoActiveModel, JoinType,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, RelationTrait, Set,
    prelude::Expr, sea_query::OnConflict,
};
use uuid::Uuid;

use crate::{
    callisto::{
        build_targets, check_result, item_assignees, label, mega_cl, mega_conversation,
        orion_tasks, path_check_configs, sea_orm_active_enums::MergeStatusEnum,
    },
    common::errors::MegaError,
    contract::api::common::Pagination,
    jupiter::{
        model::common::{ItemDetails, ListParams},
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            stg_common::{
                combine_item_list,
                query_build::{apply_sort, filter_by_assignees, filter_by_labels},
            },
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
        let cond = Condition::all();
        let cond = filter_by_labels(cond, params.labels);
        let cond = filter_by_assignees(cond, params.assignees);

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
            .join(
                JoinType::LeftJoin,
                crate::callisto::entity_ext::mega_cl::Relation::ItemLabels.def(),
            )
            .join(
                JoinType::LeftJoin,
                crate::callisto::entity_ext::mega_cl::Relation::ItemAssignees.def(),
            )
            .filter(mega_cl::Column::Status.is_in(status))
            .apply_if(params.author, |q, author| {
                q.filter(mega_cl::Column::Username.eq(author))
            })
            .filter(cond)
            .distinct();

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

        let label_query = mega_cl::Entity::find().filter(mega_cl::Column::Id.is_in(ids.clone()));
        let label_query = apply_sort(
            label_query,
            params.sort_by.as_deref(),
            params.asc,
            &sort_map,
        );
        let labels: Vec<(mega_cl::Model, Vec<label::Model>)> = label_query
            .find_with_related(label::Entity)
            .all(self.get_connection())
            .await?;

        let assignees: Vec<(mega_cl::Model, Vec<item_assignees::Model>)> = mega_cl::Entity::find()
            .filter(mega_cl::Column::Id.is_in(ids.clone()))
            .find_with_related(item_assignees::Entity)
            .all(self.get_connection())
            .await?;

        let conversations: Vec<(mega_cl::Model, Vec<mega_conversation::Model>)> =
            mega_cl::Entity::find()
                .filter(mega_cl::Column::Id.is_in(ids))
                .find_with_related(mega_conversation::Entity)
                .all(self.get_connection())
                .await?;

        let res = combine_item_list::<mega_cl::Entity>(labels, assignees, conversations);

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

    pub async fn get_cl_labels(
        &self,
        link: &str,
    ) -> Result<Option<(mega_cl::Model, Vec<label::Model>)>, MegaError> {
        let labels: Vec<(mega_cl::Model, Vec<label::Model>)> = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .find_with_related(label::Entity)
            .all(self.get_connection())
            .await?;
        Ok(labels.first().cloned())
    }

    pub async fn get_cl_assignees(
        &self,
        link: &str,
    ) -> Result<Option<(mega_cl::Model, Vec<item_assignees::Model>)>, MegaError> {
        let assignees: Vec<(mega_cl::Model, Vec<item_assignees::Model>)> = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .find_with_related(item_assignees::Entity)
            .all(self.get_connection())
            .await?;
        Ok(assignees.first().cloned())
    }

    pub async fn is_assignee(&self, link: &str, username: &str) -> Result<(), MegaError> {
        let assignee = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq(link))
            .find_with_related(item_assignees::Entity)
            .filter(item_assignees::Column::AssignneeId.eq(username))
            .all(self.get_connection())
            .await?;
        if assignee.is_empty() {
            return Err(MegaError::Other("Not an assignee".to_string()));
        }

        Ok(())
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
        let mut a_model = model.into_active_model();
        a_model.status = Set(MergeStatusEnum::Merged);
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn update_cl_to_hash(
        &self,
        model: mega_cl::Model,
        to_hash: &str,
    ) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.to_hash = Set(to_hash.to_owned());
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn update_cl_hash(
        &self,
        model: mega_cl::Model,
        from_hash: &str,
        to_hash: &str,
    ) -> Result<(), MegaError> {
        let mut a_model = model.into_active_model();
        a_model.from_hash = Set(from_hash.to_owned());
        a_model.to_hash = Set(to_hash.to_owned());
        a_model.updated_at = Set(chrono::Utc::now().naive_utc());
        a_model.update(self.get_connection()).await.unwrap();
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

    pub async fn save_cl_commits(&self, link: &str, commits: Vec<Commit>) -> Result<(), MegaError> {
        let mut save_models = vec![];
        for commit in commits {
            let model = crate::callisto::mega_cl_commits::ActiveModel {
                cl_link: Set(link.to_string()),
                commit_sha: Set(commit.id.to_string()),
                author_name: Set(commit.author.name.clone()),
                author_email: Set(commit.author.email.clone()),
                message: Set(commit.format_message()),
                created_at: Set(chrono::Utc::now().naive_utc()),
                updated_at: Set(chrono::Utc::now().naive_utc()),
            };
            save_models.push(model);
        }
        self.batch_save_model(save_models).await?;
        Ok(())
    }

    /// For each CL link, resolve the latest Orion task and aggregate its
    /// `build_targets.latest_state` with Checks-compatible worst-wins priority:
    /// Failed > Interrupted > Building > Pending/Uninitialized > Completed.
    /// Ported from mega@fae6823 `jupiter/src/storage/cl_storage.rs` (#2163).
    pub async fn latest_build_status_by_cl_links(
        &self,
        links: &[String],
    ) -> Result<HashMap<String, String>, MegaError> {
        if links.is_empty() {
            return Ok(HashMap::new());
        }

        let tasks = orion_tasks::Entity::find()
            .filter(orion_tasks::Column::Cl.is_in(links.to_vec()))
            .order_by_desc(orion_tasks::Column::CreatedAt)
            .all(self.get_connection())
            .await?;

        // First row per CL wins (already ordered by created_at DESC).
        let mut latest_task_by_cl: HashMap<String, Uuid> = HashMap::new();
        for task in tasks {
            latest_task_by_cl.entry(task.cl).or_insert(task.id);
        }

        if latest_task_by_cl.is_empty() {
            return Ok(HashMap::new());
        }

        let task_ids: Vec<Uuid> = latest_task_by_cl.values().copied().collect();
        let targets = build_targets::Entity::find()
            .filter(build_targets::Column::TaskId.is_in(task_ids))
            .all(self.get_connection())
            .await?;

        let mut states_by_task: HashMap<Uuid, Vec<String>> = HashMap::new();
        for target in targets {
            states_by_task
                .entry(target.task_id)
                .or_default()
                .push(target.latest_state);
        }

        let mut result = HashMap::new();
        for (cl, task_id) in latest_task_by_cl {
            if let Some(states) = states_by_task.get(&task_id)
                && let Some(status) = aggregate_target_states(states)
            {
                result.insert(cl, status);
            }
        }
        Ok(result)
    }
}

/// Worst-wins aggregation matching Checks UI `getTaskStatus` priority.
/// Ported from mega@fae6823 `jupiter/src/storage/cl_storage.rs` (#2163).
fn aggregate_target_states(states: &[String]) -> Option<String> {
    if states.is_empty() {
        return None;
    }
    let priority = |s: &str| -> u8 {
        match s {
            "Failed" => 0,
            "Interrupted" => 1,
            "Building" => 2,
            "Pending" | "Uninitialized" => 3,
            "Completed" => 4,
            _ => 3,
        }
    };
    states.iter().min_by_key(|s| priority(s.as_str())).cloned()
}

#[cfg(test)]
mod tests {
    use sea_orm::ConnectionTrait;
    use tempfile::TempDir;

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations, storage::base_storage::BaseStorage, tests::test_db_connection,
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

    /// SYNC-03 (mega@fae6823, #2163): latest-task-per-CL resolution and
    /// worst-wins aggregation over `build_targets.latest_state`.
    #[tokio::test]
    async fn latest_build_status_aggregates_worst_wins_per_cl() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");

        let task_old = Uuid::new_v4();
        let task_new = Uuid::new_v4();
        let task_other = Uuid::new_v4();
        for (id, cl, secs) in [
            (task_old, "CLAAAA", 1_000),
            (task_new, "CLAAAA", 2_000),
            (task_other, "CLBBBB", 3_000),
        ] {
            orion_tasks::ActiveModel {
                id: Set(id),
                changes: Set(serde_json::json!({})),
                repo_name: Set("repo".to_string()),
                cl: Set(cl.to_string()),
                created_at: Set(chrono::DateTime::from_timestamp(secs, 0)
                    .expect("valid timestamp")
                    .into()),
            }
            .insert(&conn)
            .await
            .expect("insert orion task");
        }

        // Older task is all green but must lose to the newer task's states.
        for (id, task_id, state) in [
            (Uuid::new_v4(), task_old, "Completed"),
            (Uuid::new_v4(), task_new, "Building"),
            (Uuid::new_v4(), task_new, "Failed"),
            (Uuid::new_v4(), task_other, "Completed"),
        ] {
            build_targets::ActiveModel {
                id: Set(id),
                task_id: Set(task_id),
                path: Set("/target".to_string()),
                latest_state: Set(state.to_string()),
            }
            .insert(&conn)
            .await
            .expect("insert build target");
        }

        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };

        let map = storage
            .latest_build_status_by_cl_links(&[
                "CLAAAA".to_string(),
                "CLBBBB".to_string(),
                "CLNONE".to_string(),
            ])
            .await
            .expect("aggregate query");

        // CLAAAA's latest task (created_at 2000) aggregates Building+Failed
        // worst-wins to Failed.
        assert_eq!(map.get("CLAAAA").map(String::as_str), Some("Failed"));
        assert_eq!(map.get("CLBBBB").map(String::as_str), Some("Completed"));
        assert!(!map.contains_key("CLNONE"));
    }

    /// SYNC-03: empty input short-circuits to an empty map without erroring.
    #[tokio::test]
    async fn latest_build_status_empty_input_returns_empty_map() {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, true).await.expect("migrations");

        let storage = ClStorage {
            base: BaseStorage::new(std::sync::Arc::new(conn)),
        };

        let map = storage
            .latest_build_status_by_cl_links(&[])
            .await
            .expect("empty input is not an error");
        assert!(map.is_empty());
    }
}
