use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::MERGE_QUEUE_TAG, oauth::OptionalSessionUser},
    callisto::sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
    ceres::model::merge_queue::{
        AddToQueueRequest, AddToQueueResponse, QueueItem, QueueListResponse, QueueStats,
        QueueStatsResponse, QueueStatus, QueueStatusResponse,
    },
    common::errors::ApiError,
    contract::api::common::CommonResult,
};

fn retired_gone(feature: &str) -> ApiError {
    ApiError::with_status(
        StatusCode::GONE,
        anyhow::anyhow!(
            "{feature} is retired on MonoWriteQueue. Queue rows are not deleted (台账). Use POST /push-queue/cancel/{{id}} for a single Queued item."
        ),
    )
}

/// Creates the merge queue router with all endpoints
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/merge-queue",
        OpenApiRouter::new()
            .routes(routes!(add_to_queue))
            .routes(routes!(remove_from_queue))
            .routes(routes!(get_queue_list))
            .routes(routes!(get_cl_queue_status))
            .routes(routes!(retry_queue_item))
            .routes(routes!(get_queue_stats))
            .routes(routes!(cancel_all_pending)),
    )
}

/// Adds a CL to the merge queue
#[utoipa::path(
    post,
    path = "/add",
    request_body = AddToQueueRequest,
    responses(
        (status = 200, body = CommonResult<AddToQueueResponse>, content_type = "application/json")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn add_to_queue(
    state: State<MonoApiServiceState>,
    // UN-20: capture the requesting subject, if any. `OptionalSessionUser`
    // never rejects, so an anonymous enqueue stays possible and is recorded as
    // NULL — this handler adds no new rejection surface.
    OptionalSessionUser(requester): OptionalSessionUser,
    Json(request): Json<AddToQueueRequest>,
) -> Result<Json<CommonResult<AddToQueueResponse>>, ApiError> {
    match state
        .monorepo()
        .add_to_merge_queue_as(request.cl_link.clone(), requester.map(|user| user.username))
        .await
    {
        Ok(position) => {
            let display_position = match state
                .storage
                .push_queue_service
                .storage()
                .list_merge_for_legacy_ui()
                .await
            {
                Ok(listed) => listed
                    .iter()
                    .position(|row| row.id == position)
                    .map(|idx| idx + 1),
                Err(e) => {
                    tracing::warn!(
                        "Failed to get display position after add for {}: {}",
                        request.cl_link,
                        e
                    );
                    None
                }
            };

            let response = AddToQueueResponse {
                success: true,
                position,
                display_position,
                message: "Merged".to_string(),
            };
            Ok(Json(CommonResult::success(Some(response))))
        }
        Err(e) => Ok(Json(CommonResult::failed(&e.to_string()))),
    }
}

/// Removes a CL from the merge queue
#[utoipa::path(
    delete,
    path = "/remove/{cl_link}",
    params(
        ("cl_link" = String, Path, description = "CL link to remove")
    ),
    responses(
        (status = 410, description = "Retired: rows are not deleted from MonoWriteQueue")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn remove_from_queue(
    _state: State<MonoApiServiceState>,
    Path(_cl_link): Path<String>,
) -> Result<Json<CommonResult<Value>>, ApiError> {
    Err(retired_gone("DELETE /merge-queue/remove"))
}

/// Gets the current merge queue list
#[utoipa::path(
    get,
    path = "/list",
    responses(
        (status = 200, body = CommonResult<QueueListResponse>, content_type = "application/json")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn get_queue_list(
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueListResponse>>, ApiError> {
    let rows = state
        .storage
        .push_queue_service
        .storage()
        .list_merge_for_legacy_ui()
        .await?;
    let mut items: Vec<QueueItem> = rows.iter().map(QueueItem::from_push_queue).collect();
    for (idx, item) in items.iter_mut().enumerate() {
        item.display_position = Some(idx + 1);
    }
    let total_count = items.len();
    Ok(Json(CommonResult::success(Some(QueueListResponse {
        items,
        total_count,
    }))))
}

/// Gets the status of a specific CL in the queue
#[utoipa::path(
    get,
    path = "/status/{cl_link}",
    params(
        ("cl_link" = String, Path, description = "CL link to check status")
    ),
    responses(
        (status = 200, body = CommonResult<QueueStatusResponse>, content_type = "application/json")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn get_cl_queue_status(
    state: State<MonoApiServiceState>,
    Path(cl_link): Path<String>,
) -> Result<Json<CommonResult<QueueStatusResponse>>, ApiError> {
    let rows = state
        .storage
        .push_queue_service
        .storage()
        .list_by_kind_and_operation(PushQueueKindEnum::Merge, &cl_link)
        .await?;
    let listed = state
        .storage
        .push_queue_service
        .storage()
        .list_merge_for_legacy_ui()
        .await?;
    let item_opt = rows
        .first()
        .map(QueueItem::from_push_queue)
        .map(|mut item| {
            if let Some(idx) = listed.iter().position(|r| r.id == item.position) {
                item.display_position = Some(idx + 1);
            }
            item
        });
    let in_queue = item_opt
        .as_ref()
        .is_some_and(|i| matches!(i.status, QueueStatus::Waiting | QueueStatus::Merging));
    Ok(Json(CommonResult::success(Some(QueueStatusResponse {
        in_queue,
        item: item_opt,
    }))))
}

/// Retries a failed queue item
#[utoipa::path(
    post,
    path = "/retry/{cl_link}",
    params(
        ("cl_link" = String, Path, description = "The cl_link to retry")
    ),
    responses(
        (status = 200, description = "Successfully retried item", body = CommonResult<Value>),
        (status = 404, description = "Item not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn retry_queue_item(
    state: State<MonoApiServiceState>,
    // UN-20: same optional capture as the add path — a retry records who asked
    // for it, and an anonymous retry is still allowed (recorded as NULL).
    OptionalSessionUser(requester): OptionalSessionUser,
    Path(cl_link): Path<String>,
) -> Result<Json<CommonResult<Value>>, ApiError> {
    match state
        .monorepo()
        .retry_merge_queue_item_as(&cl_link, requester.map(|user| user.username))
        .await
    {
        Ok(success) => {
            let response = if success {
                json!({
                    "success": true,
                    "message": "Item retried"
                })
            } else {
                json!({
                    "success": false,
                    "message": "Item not found or cannot be retried"
                })
            };
            Ok(Json(CommonResult::success(Some(response))))
        }
        Err(e) => Ok(Json(CommonResult::failed(&e.to_string()))),
    }
}

/// Gets queue statistics
#[utoipa::path(
    get,
    path = "/stats",
    responses(
        (status = 200, body = CommonResult<QueueStatsResponse>, content_type = "application/json")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn get_queue_stats(
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueStatsResponse>>, ApiError> {
    let pq = &state.storage.push_queue_service;
    let waiting = pq
        .storage()
        .count_by_kind_and_status(PushQueueKindEnum::Merge, PushQueueStatusEnum::Queued)
        .await? as usize;
    let merging = pq
        .storage()
        .count_by_kind_and_status(PushQueueKindEnum::Merge, PushQueueStatusEnum::Running)
        .await? as usize;
    let merged = pq
        .storage()
        .count_by_kind_and_status(PushQueueKindEnum::Merge, PushQueueStatusEnum::Done)
        .await? as usize;
    let failed = pq
        .storage()
        .count_by_kind_and_status(PushQueueKindEnum::Merge, PushQueueStatusEnum::Failed)
        .await? as usize
        + pq.storage()
            .count_by_kind_and_status(PushQueueKindEnum::Merge, PushQueueStatusEnum::Cancelled)
            .await? as usize;
    let stats = QueueStats {
        total_items: waiting + merging + merged + failed,
        waiting_count: waiting,
        testing_count: 0,
        merging_count: merging,
        failed_count: failed,
        merged_count: merged,
    };
    Ok(Json(CommonResult::success(Some(QueueStatsResponse {
        stats,
    }))))
}

/// Cancels all pending queue items
#[utoipa::path(
    post,
    path = "/cancel-all",
    responses(
        (status = 410, description = "Retired: batch cancel is not supported")
    ),
    tag = MERGE_QUEUE_TAG
)]
async fn cancel_all_pending(
    _state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Value>>, ApiError> {
    Err(retired_gone("POST /merge-queue/cancel-all"))
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use axum::{
        body::Body,
        http::{Request, StatusCode, header::CONTENT_TYPE},
    };
    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };
    use sea_orm::{ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement};
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use crate::{
        api::{
            MonoApiServiceState,
            oauth::{api_store::BrowserSessionStore, model::LoginUser},
        },
        bellatrix::Bellatrix,
        callisto::{push_queue, sea_orm_active_enums::PushQueueKindEnum},
        ceres::api_service::cache::GitObjectCache,
        common::utils::MEGA_BRANCH_NAME,
        contract::policy::entitystore::SharedEntityStore,
        jupiter::storage::{Storage, base_storage::StorageConnector},
    };

    fn blob_item(name: &str, hex: &str) -> TreeItem {
        TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(hex).unwrap(),
            name.to_string(),
        )
    }

    async fn seed_mergeable_cl(storage: &Storage, link: &str) {
        let mono = storage.mono_storage();
        let old_tree = Tree::from_tree_items(vec![blob_item(
            ".gitkeep",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )])
        .expect("old tree");
        let new_tree = Tree::from_tree_items(vec![blob_item(
            "queued.txt",
            "cccccccccccccccccccccccccccccccccccccccc",
        )])
        .expect("new tree");
        let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
        let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
        mono.save_mega_trees(
            vec![old_tree.clone(), new_tree.clone()],
            old_commit.id,
            None,
        )
        .await
        .unwrap();
        mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            crate::callisto::mega_refs::Model {
                id: 1,
                path: "/".to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: old_commit.id.to_string(),
                ref_tree_hash: old_tree.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
        mono.save_or_update_cl_ref(
            "/",
            &format!("refs/cl/{link}"),
            &new_commit.id.to_string(),
            &new_tree.id.to_string(),
        )
        .await
        .unwrap();
        storage
            .cl_storage()
            .new_cl_model(
                "/",
                link,
                "queue add",
                "main",
                &old_commit.id.to_string(),
                &new_commit.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
    }

    fn api_state(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Fixed(
                crate::api::oauth::api_store::FixedUserSessionStore {
                    user: LoginUser {
                        username: "queue-requester".to_string(),
                        ..Default::default()
                    },
                },
            ),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "mw01-test".to_string(),
            }),
            listen_addr: "http://127.0.0.1:0".to_string(),
            entity_store: Arc::new(SharedEntityStore::new()),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            storage,
        }
    }

    #[tokio::test]
    async fn queue_add_uses_push_queue_not_merge_queue_table() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        seed_mergeable_cl(&storage, "MW01ADD").await;

        let (router, _api) = OpenApiRouter::new()
            .merge(super::routers())
            .split_for_parts();
        let response = router
            .with_state(api_state(storage.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/merge-queue/add")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"cl_link":"MW01ADD"}"#))
                    .unwrap(),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);

        let pq_rows = push_queue::Entity::find()
            .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Merge))
            .filter(push_queue::Column::OperationId.eq("MW01ADD"))
            .all(storage.cl_storage().get_connection())
            .await
            .expect("read push_queue");
        assert!(
            !pq_rows.is_empty(),
            "default config POST /merge-queue/add must write push_queue"
        );

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('merge_queue')::text AS table_name;".to_owned(),
        );
        let row = storage
            .cl_storage()
            .get_connection()
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL catalog")
            .expect("PostgreSQL catalog query should return one row");
        let table_name: Option<String> = row
            .try_get("", "table_name")
            .expect("PostgreSQL catalog query should expose table_name");
        assert!(
            table_name.is_none(),
            "POST /merge-queue/add must not write merge_queue; the table is dropped"
        );
    }
}
