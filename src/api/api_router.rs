use anyhow::{Result, anyhow};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        api_doc::SYSTEM_COMMON,
        router::{
            admin_router, agent_capture_router, artifacts_router, bot_router, buck_router,
            cl_router, commit_router, gpg_router, group_router, merge_queue_router, preview_router,
            push_queue_router, repo_router, tag_router, user_router, webhook_router,
        },
    },
    ceres::{api_service::ApiHandler, model::git::TreeQuery},
    common::errors::{ApiError, MegaError},
    config::PushPolicy,
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(life_cycle_check))
        .route("/file/blob/{object_id}", get(get_blob_file))
        .route("/file/tree", get(get_tree_file))
        .merge(preview_router::routers())
        .merge(cl_router::routers())
        .merge(gpg_router::routers())
        .merge(user_router::routers())
        .merge(merge_queue_router::routers())
        .merge(push_queue_router::routers())
        .merge(commit_router::routers())
        .merge(tag_router::routers())
        .merge(repo_router::routers())
        .merge(buck_router::routers())
        .merge(admin_router::routers())
        .merge(artifacts_router::routers())
        .merge(group_router::routers())
        .merge(webhook_router::routers())
        .merge(bot_router::routers())
}

/// HTTP `/api/v1` surface keyed on [`PushPolicy`] (TP-18 / AW-03). Trunk is the
/// protocol subset without CL / issue / reviewer / OAuth user routers, but
/// **does** register the product writes (create-entry / delete-entry /
/// move-entry / edit/save) and, since plan-20260917 LB-04, the monorepo tag
/// routes (`tag_router`, writes gated by `git.push_auth`). Review keeps the
/// full OAuth web surface.
pub fn routers_for(policy: PushPolicy) -> OpenApiRouter<MonoApiServiceState> {
    match policy {
        PushPolicy::Trunk => storage_only_routers(),
        PushPolicy::Review => routers(),
    }
}

/// Git-adjacent surface for storage-only / trunk HTTP (no OAuth/CL/user
/// routers): read-only preview, the product writes, the tag routes and
/// (plan-20260921 AR-01) the artifacts protocol routes (writes gated by
/// `git.push_auth`).
pub fn storage_only_routers() -> OpenApiRouter<MonoApiServiceState> {
    storage_only_routers_with(false)
}

/// Same as [`storage_only_routers`], optionally merging Agent Capture under
/// `/agent-capture` (public prefix `/api/v1` is applied by the outer nest).
pub fn storage_only_routers_with(
    include_agent_capture: bool,
) -> OpenApiRouter<MonoApiServiceState> {
    let router = OpenApiRouter::new()
        .routes(routes!(life_cycle_check))
        .route("/file/blob/{object_id}", get(get_blob_file))
        .route("/file/tree", get(get_tree_file))
        .merge(preview_router::readonly_routers())
        .merge(preview_router::write_routers())
        .merge(preview_router::storage_only_write_routers())
        .merge(tag_router::routers())
        .merge(artifacts_router::routers());
    if include_agent_capture {
        router.merge(agent_capture_router::routers())
    } else {
        router
    }
}

/// Health Check
#[utoipa::path(
    get,
    path = "/status",
    responses(
        (status = 200, body = str, content_type = "text/plain")
    ),
    tag = SYSTEM_COMMON
)]
async fn life_cycle_check() -> Result<impl IntoResponse, ApiError> {
    Ok(Json("http ready"))
}

// Blob Objects Download
pub async fn get_blob_file(
    state: State<MonoApiServiceState>,
    Path(oid): Path<String>,
) -> Result<Response, ApiError> {
    let api_handler = state.monorepo();

    let result = api_handler.get_raw_blob_by_hash(&oid).await;
    let file_name = format!("inline; filename=\"{oid}\"");
    match result {
        Ok(data) => Ok(Response::builder()
            .header("Content-Type", "application/octet-stream")
            .header("Content-Disposition", file_name)
            .body(Body::from(data))
            .unwrap()),
        Err(e) => match e {
            MegaError::ObjStorageNotFound(_) => Err(ApiError::not_found(anyhow!("error={}", e))),
            _ => Err(ApiError::internal(anyhow!("error={}", e))),
        },
    }
}

// Tree Objects Download
pub async fn get_tree_file(
    state: State<MonoApiServiceState>,
    Query(query): Query<TreeQuery>,
) -> Result<Response, ApiError> {
    let data = state
        .api_handler(query.path.as_ref())
        .await?
        .get_binary_tree_by_path(std::path::Path::new(&query.path), query.oid)
        .await?;

    let file_name = format!("inline; filename=\"{}\"", "");
    Ok(Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Content-Disposition", file_name)
        .body(Body::from(data))
        .unwrap())
}
