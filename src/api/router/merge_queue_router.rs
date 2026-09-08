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
    config::MergeWriter,
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
    // Use MonoApiService to add to queue AND start the background processor
    match state
        .monorepo()
        .add_to_merge_queue_as(request.cl_link.clone(), requester.map(|user| user.username))
        .await
    {
        Ok(position) => {
            let display_position = state
                .storage
                .merge_queue_service
                .get_display_position_by_position(position)
                .await
                .map(Some)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "Failed to get display position after add for {}: {}",
                        request.cl_link,
                        e
                    );
                    None
                });

            let message = if state.storage.config().monorepo.merge_writer
                == crate::config::MergeWriter::Queue
            {
                "Merged".to_string()
            } else {
                "Added to queue".to_string()
            };
            let response = AddToQueueResponse {
                success: true,
                position,
                display_position,
                message,
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
    if state.storage.config().monorepo.merge_writer == MergeWriter::Queue {
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
        return Ok(Json(CommonResult::success(Some(QueueListResponse {
            items,
            total_count,
        }))));
    }
    let items = state.storage.merge_queue_service.get_queue_list().await?;
    let response = QueueListResponse::from(items);
    Ok(Json(CommonResult::success(Some(response))))
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
    if state.storage.config().monorepo.merge_writer == MergeWriter::Queue {
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
        return Ok(Json(CommonResult::success(Some(QueueStatusResponse {
            in_queue,
            item: item_opt,
        }))));
    }
    let item_model = state
        .storage
        .merge_queue_service
        .get_cl_queue_status(&cl_link)
        .await?;

    let mut item_opt: Option<QueueItem> = item_model.map(|m| m.into());

    if let Some(ref mut item) = item_opt {
        match item.status {
            QueueStatus::Waiting | QueueStatus::Testing | QueueStatus::Merging => {
                let index_result = state
                    .storage
                    .merge_queue_service
                    .get_display_position(&item.cl_link)
                    .await;

                match index_result {
                    Ok(index) => {
                        item.display_position = index;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to get display position for {}: {}",
                            item.cl_link,
                            e
                        );
                        item.display_position = None;
                    }
                }
            }
            _ => {}
        }
    }

    let response = QueueStatusResponse {
        in_queue: item_opt.is_some(),
        item: item_opt,
    };

    Ok(Json(CommonResult::success(Some(response))))
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
    // Use MonoApiService to retry AND start the background processor
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
    if state.storage.config().monorepo.merge_writer == MergeWriter::Queue {
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
        return Ok(Json(CommonResult::success(Some(QueueStatsResponse {
            stats,
        }))));
    }
    let stats = state.storage.merge_queue_service.get_queue_stats().await?;
    let response = QueueStatsResponse::from(stats);
    Ok(Json(CommonResult::success(Some(response))))
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
