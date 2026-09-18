use axum::{
    Json,
    extract::{Path, State},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::CONV_TAG, oauth::model::LoginUser},
    ceres::model::conversation::ContentPayload,
    common::errors::ApiError,
    contract::api::common::CommonResult,
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/conversation",
        OpenApiRouter::new()
            .routes(routes!(delete_comment))
            .routes(routes!(edit_comment)),
    )
}

/// Delete Comment
#[utoipa::path(
    delete,
    params(
        ("comment_id", description = "A numeric ID representing a comment"),
    ),
    path = "/{comment_id}",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CONV_TAG
)]
async fn delete_comment(
    Path(comment_id): Path<i64>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state.conv_stg().remove_conversation(comment_id).await?;
    Ok(Json(CommonResult::success(None)))
}

/// Edit comment
#[utoipa::path(
    post,
    params(
        ("comment_id", description = "A numeric ID representing a comment"),
    ),
    path = "/{comment_id}",
    request_body = ContentPayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CONV_TAG
)]
async fn edit_comment(
    _: LoginUser,
    Path(comment_id): Path<i64>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<ContentPayload>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .conv_stg()
        .update_comment(comment_id, Some(payload.content))
        .await?;
    Ok(Json(CommonResult::success(None)))
}
