use anyhow::anyhow;
use axum::{
    Json,
    extract::{Path, State},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState, api_common::group_permission::ensure_admin,
        api_doc::GROUP_PERMISSION_TAG, oauth::model::LoginUser,
    },
    ceres::model::group::{
        AddMembersRequest, CreateGroupRequest, DeleteGroupResponse, EmptyListAdditional,
        GroupMemberResponse, GroupResponse, RemoveMemberResponse, UpdateGroupRequest,
        UserGroupsResponse,
    },
    common::errors::ApiError,
    contract::api::common::{CommonPage, CommonResult, PageParams, Pagination},
    jupiter::model::group_dto::{CreateGroupPayload, UpdateGroupPayload},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/admin",
        OpenApiRouter::new()
            .routes(routes!(create_group))
            .routes(routes!(list_groups))
            .routes(routes!(get_group))
            .routes(routes!(update_group))
            .routes(routes!(delete_group))
            .routes(routes!(add_group_members))
            .routes(routes!(remove_group_member))
            .routes(routes!(list_group_members))
            .routes(routes!(get_user_groups)),
    )
}

#[utoipa::path(
    post,
    path = "/groups",
    request_body = CreateGroupRequest,
    responses(
        (status = 200, body = CommonResult<GroupResponse>),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 409, description = "Group already exists"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn create_group(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<Json<CommonResult<GroupResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let name = req.name.trim();
    if name.is_empty() {
        tracing::warn!(
            actor = %user.username,
            "group.create rejected: empty group name"
        );
        return Err(ApiError::bad_request(anyhow!(
            "Group name must not be empty"
        )));
    }
    if name.len() > 255 {
        tracing::warn!(
            actor = %user.username,
            "group.create rejected: name too long"
        );
        return Err(ApiError::bad_request(anyhow!(
            "Group name must not exceed 255 characters"
        )));
    }

    let description = req
        .description
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty());

    let group = state
        .monorepo()
        .create_group(CreateGroupPayload {
            name: name.to_string(),
            description,
        })
        .await?;

    Ok(Json(CommonResult::success(Some(group.into()))))
}

#[utoipa::path(
    post,
    path = "/groups/list",
    request_body = PageParams<EmptyListAdditional>,
    responses(
        (status = 200, body = CommonResult<CommonPage<GroupResponse>>),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn list_groups(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(json): Json<PageParams<EmptyListAdditional>>,
) -> Result<Json<CommonResult<CommonPage<GroupResponse>>>, ApiError> {
    ensure_admin(&state, &user).await?;
    validate_pagination(&json.pagination)?;

    let (items, total) = state.monorepo().list_groups(json.pagination).await?;
    let items = items.into_iter().map(Into::into).collect();

    Ok(Json(CommonResult::success(Some(CommonPage {
        total,
        items,
    }))))
}

#[utoipa::path(
    get,
    path = "/groups/{group_id}",
    params(
        ("group_id" = i64, Path, description = "Group ID")
    ),
    responses(
        (status = 200, body = CommonResult<GroupResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn get_group(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(group_id): Path<i64>,
) -> Result<Json<CommonResult<GroupResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let group = state
        .monorepo()
        .get_group_by_id(group_id)
        .await?
        .ok_or_else(|| {
            tracing::warn!(
                actor = %user.username,
                group_id,
                "group.get failed: group not found"
            );
            ApiError::not_found(anyhow!("Group not found: {}", group_id))
        })?;

    Ok(Json(CommonResult::success(Some(group.into()))))
}

#[utoipa::path(
    put,
    path = "/groups/{group_id}",
    request_body = UpdateGroupRequest,
    params(
        ("group_id" = i64, Path, description = "Group ID")
    ),
    responses(
        (status = 200, body = CommonResult<GroupResponse>),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
        (status = 409, description = "Group already exists"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn update_group(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(group_id): Path<i64>,
    Json(req): Json<UpdateGroupRequest>,
) -> Result<Json<CommonResult<GroupResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let name = req.name.trim();
    if name.is_empty() {
        tracing::warn!(
            actor = %user.username,
            group_id,
            "group.update rejected: empty group name"
        );
        return Err(ApiError::bad_request(anyhow!(
            "Group name must not be empty"
        )));
    }
    if name.len() > 255 {
        tracing::warn!(
            actor = %user.username,
            group_id,
            "group.update rejected: name too long"
        );
        return Err(ApiError::bad_request(anyhow!(
            "Group name must not exceed 255 characters"
        )));
    }

    let description = req
        .description
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty());

    let updated = state
        .monorepo()
        .update_group(
            group_id,
            UpdateGroupPayload {
                name: name.to_string(),
                description,
            },
        )
        .await?;

    Ok(Json(CommonResult::success(Some(updated.into()))))
}

#[utoipa::path(
    delete,
    path = "/groups/{group_id}",
    params(
        ("group_id" = i64, Path, description = "Group ID")
    ),
    responses(
        (status = 200, body = CommonResult<DeleteGroupResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn delete_group(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(group_id): Path<i64>,
) -> Result<Json<CommonResult<DeleteGroupResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let stats = state.monorepo().delete_group(group_id).await?;

    Ok(Json(CommonResult::success(Some(DeleteGroupResponse {
        group_id,
        deleted_members_count: stats.deleted_members_count,
        deleted_permissions_count: stats.deleted_permissions_count,
        deleted_groups_count: stats.deleted_groups_count,
    }))))
}

#[utoipa::path(
    post,
    path = "/groups/{group_id}/members",
    request_body = AddMembersRequest,
    params(
        ("group_id" = i64, Path, description = "Group ID")
    ),
    responses(
        (status = 200, body = CommonResult<Vec<GroupMemberResponse>>),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn add_group_members(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(group_id): Path<i64>,
    Json(req): Json<AddMembersRequest>,
) -> Result<Json<CommonResult<Vec<GroupMemberResponse>>>, ApiError> {
    ensure_admin(&state, &user).await?;
    if req.usernames.is_empty() {
        tracing::warn!(
            actor = %user.username,
            group_id,
            "group.members.add rejected: empty usernames"
        );
        return Err(ApiError::bad_request(anyhow!(
            "usernames must not be empty"
        )));
    }

    let members = state
        .monorepo()
        .add_group_members(group_id, req.usernames)
        .await?;
    let members = members.into_iter().map(Into::into).collect();

    Ok(Json(CommonResult::success(Some(members))))
}

#[utoipa::path(
    delete,
    path = "/groups/{group_id}/members/{username}",
    params(
        ("group_id" = i64, Path, description = "Group ID"),
        ("username" = String, Path, description = "Username")
    ),
    responses(
        (status = 200, body = CommonResult<RemoveMemberResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn remove_group_member(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path((group_id, username)): Path<(i64, String)>,
) -> Result<Json<CommonResult<RemoveMemberResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let removed = state
        .monorepo()
        .remove_group_member(group_id, &username)
        .await?;

    Ok(Json(CommonResult::success(Some(RemoveMemberResponse {
        group_id,
        username,
        removed,
    }))))
}

#[utoipa::path(
    post,
    path = "/groups/{group_id}/members/list",
    request_body = PageParams<EmptyListAdditional>,
    params(
        ("group_id" = i64, Path, description = "Group ID")
    ),
    responses(
        (status = 200, body = CommonResult<CommonPage<GroupMemberResponse>>),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
        (status = 404, description = "Group not found"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn list_group_members(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(group_id): Path<i64>,
    Json(json): Json<PageParams<EmptyListAdditional>>,
) -> Result<Json<CommonResult<CommonPage<GroupMemberResponse>>>, ApiError> {
    ensure_admin(&state, &user).await?;
    validate_pagination(&json.pagination)?;

    let (items, total) = state
        .monorepo()
        .list_group_members(group_id, json.pagination)
        .await?;
    let items = items.into_iter().map(Into::into).collect();

    Ok(Json(CommonResult::success(Some(CommonPage {
        total,
        items,
    }))))
}

#[utoipa::path(
    get,
    path = "/users/{username}/groups",
    params(
        ("username" = String, Path, description = "Username")
    ),
    responses(
        (status = 200, body = CommonResult<UserGroupsResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin only"),
    ),
    tag = GROUP_PERMISSION_TAG
)]
async fn get_user_groups(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(username): Path<String>,
) -> Result<Json<CommonResult<UserGroupsResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let groups = state.monorepo().get_user_groups(&username).await?;
    let groups = groups.into_iter().map(Into::into).collect();

    Ok(Json(CommonResult::success(Some(UserGroupsResponse {
        username,
        groups,
    }))))
}

fn validate_pagination(pagination: &Pagination) -> Result<(), ApiError> {
    if pagination.page == 0 {
        tracing::warn!("invalid pagination.page: {}", pagination.page);
        return Err(ApiError::bad_request(anyhow!(
            "pagination.page must be >= 1"
        )));
    }
    if pagination.per_page == 0 {
        tracing::warn!("invalid pagination.per_page: {}", pagination.per_page);
        return Err(ApiError::bad_request(anyhow!(
            "pagination.per_page must be >= 1"
        )));
    }
    Ok(())
}
