use std::{collections::HashMap, path::PathBuf};

use anyhow::Result;
use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::CODE_PREVIEW, api_write_auth::authorize_trunk_api_write},
    ceres::model::{
        blame::{BlameQuery, BlameRequest, BlameResult},
        change_list::DiffItemSchema,
        git::{
            BlobContentQuery, CodePreviewQuery, CreateEntryInfo, CreateEntryResult,
            DeleteEntryInfo, DeleteEntryResult, DiffPreviewPayload, EditFilePayload,
            EditFileResult, FileTreeItem, TreeCommitItem, TreeHashItem, TreeResponse,
        },
    },
    common::errors::ApiError,
    config::PushPolicy,
    contract::api::{common::CommonResult, git::commit::LatestCommitInfo},
};

async fn upsert_commit_binding(
    state: &MonoApiServiceState,
    commit_id: &str,
    author_username: Option<&str>,
) -> Result<(), ApiError> {
    let final_username = author_username.and_then(|u| {
        let t = u.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("anonymous") {
            None
        } else {
            Some(t.to_string())
        }
    });
    state
        .storage
        .commit_binding_storage()
        .upsert_binding(commit_id, final_username.clone(), final_username.is_none())
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("Failed to save commit binding: {}", e)))
}

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    readonly_routers()
        .merge(write_routers())
        .routes(routes!(preview_diff))
}

/// POST create-entry / delete-entry / edit/save used by review and trunk
/// product API writes.
pub fn write_routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(create_entry))
        .routes(routes!(delete_entry))
        .routes(routes!(save_edit))
}

/// GET-only code preview used by the storage-only HTTP surface (TP-20).
pub fn readonly_routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(get_latest_commit))
        .routes(routes!(get_tree_commit_info))
        .routes(routes!(get_file_blame))
        .routes(routes!(get_tree_content_hash))
        .routes(routes!(get_tree_dir_hash))
        .routes(routes!(path_can_be_cloned))
        .routes(routes!(get_tree_info))
        .routes(routes!(get_blob_string))
}

/// Get blob file as string
#[utoipa::path(
    get,
    params(
        BlobContentQuery
    ),
    path = "/blob",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_blob_string(
    Query(query): Query<BlobContentQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let data = state
        .api_handler(query.path.as_ref())
        .await?
        .get_blob_as_string(query.path.into(), Some(query.refs.as_str()))
        .await?;
    Ok(Json(CommonResult::success(data)))
}

/// Create file or folder in web UI
#[utoipa::path(
    post,
    path = "/create-entry",
    request_body = CreateEntryInfo,
    responses(
        (status = 200, body = CommonResult<CreateEntryResult>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn create_entry(
    state: State<MonoApiServiceState>,
    headers: HeaderMap,
    Json(json): Json<CreateEntryInfo>,
) -> Result<Json<CommonResult<CreateEntryResult>>, ApiError> {
    let requester = trunk_write_requester(&state, &headers, &json.path)?;
    let handler = state.api_handler(json.path.as_ref()).await?;
    let result = handler
        .create_monorepo_entry(json.clone(), requester)
        .await?;

    upsert_commit_binding(&state, &result.commit_id, json.author_username.as_deref()).await?;
    Ok(Json(CommonResult::success(Some(result))))
}

/// Delete a directory (plan-20260917 LB-02): parent `path` + `name`, same
/// trunk auth and landing policy as create-entry.
#[utoipa::path(
    post,
    path = "/delete-entry",
    request_body = DeleteEntryInfo,
    responses(
        (status = 200, body = CommonResult<DeleteEntryResult>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn delete_entry(
    state: State<MonoApiServiceState>,
    headers: HeaderMap,
    Json(json): Json<DeleteEntryInfo>,
) -> Result<Json<CommonResult<DeleteEntryResult>>, ApiError> {
    let requester = trunk_write_requester(&state, &headers, &json.path)?;
    let handler = state.api_handler(json.path.as_ref()).await?;
    // Bare `?`: the service's `[code:4xx]` prefixes map through
    // `From<E> for ApiError` (`map_ceres_error` would turn 409 into 500).
    let result = handler
        .delete_monorepo_entry(json.clone(), requester)
        .await?;

    upsert_commit_binding(&state, &result.commit_id, json.author_username.as_deref()).await?;
    Ok(Json(CommonResult::success(Some(result))))
}

/// Get latest commit by path
#[utoipa::path(
    get,
    path = "/latest-commit",
    params(
        CodePreviewQuery
    ),
    responses(
        (status = 200, body = LatestCommitInfo, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_latest_commit(
    Query(query): Query<CodePreviewQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<LatestCommitInfo>, ApiError> {
    let query_path: std::path::PathBuf = query.path.into();

    // Pass refs as None if empty, ensuring consistent behavior for both tag and commit SHA
    let refs_opt = if query.refs.is_empty() {
        None
    } else {
        Some(query.refs.as_str())
    };

    tracing::debug!(
        "get_latest_commit with path: {:?}, refs: {:?}",
        query_path,
        refs_opt
    );

    // api_handler automatically determines whether to use monorepo or import handler
    let api_handler = state.api_handler(&query_path).await?;

    let res = api_handler.get_latest_commit(query_path, refs_opt).await?;

    Ok(Json(res))
}

/// Get tree brief info
#[utoipa::path(
    get,
    path = "/tree",
    params(
        CodePreviewQuery
    ),
    responses(
        (status = 200, body = CommonResult<TreeResponse>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_tree_info(
    Query(query): Query<CodePreviewQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<TreeResponse>>, ApiError> {
    let mut parts = Vec::new();

    let normalized_path = PathBuf::from(query.path.clone());
    let mut segments = normalized_path.components().peekable();
    let mut current = String::new();

    while let Some(segment) = segments.next() {
        let part = segment.as_os_str().to_string_lossy().to_string();
        if segments.peek().is_some() {
            if current != "/" && part != "/" {
                current.push('/');
            }
            current.push_str(&part);
            parts.push(current.clone());
        }
    }

    let mut file_tree = HashMap::new();

    for part in parts {
        let path = part.as_ref();
        let handler = state.api_handler(path).await?;
        let tree_items = handler
            .get_tree_info(path, Some(query.refs.as_str()))
            .await?;
        file_tree.insert(
            part,
            FileTreeItem {
                total_count: tree_items.len(),
                tree_items,
            },
        );
    }

    let tree_items = state
        .api_handler(query.path.as_ref())
        .await?
        .get_tree_info(query.path.as_ref(), Some(query.refs.as_str()))
        .await?;
    Ok(Json(CommonResult::success(Some(TreeResponse {
        file_tree,
        tree_items,
    }))))
}

/// List matching trees with commit msg by query
#[utoipa::path(
    get,
    path = "/tree/commit-info",
    params(
        CodePreviewQuery
    ),
    responses(
        (status = 200, body = CommonResult<Vec<TreeCommitItem>>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_tree_commit_info(
    Query(query): Query<CodePreviewQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<TreeCommitItem>>>, ApiError> {
    let data = state
        .api_handler(query.path.as_ref())
        .await?
        .get_tree_commit_info(query.path.into(), Some(query.refs.as_str()))
        .await?;
    Ok(Json(CommonResult::success(Some(data))))
}

/// Get tree content hash,the dir's hash as same as old,file's hash is the content hash
#[utoipa::path(
    get,
    path = "/tree/content-hash",
    params(
        CodePreviewQuery
    ),
    responses(
        (status = 200, body = CommonResult<Vec<TreeHashItem>>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_tree_content_hash(
    Query(query): Query<CodePreviewQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<TreeHashItem>>>, ApiError> {
    let data = state
        .api_handler(query.path.as_ref())
        .await?
        .get_tree_content_hash(query.path.into(), Some(query.refs.as_str()))
        .await?;
    Ok(Json(CommonResult::success(Some(data))))
}

/// return the dir's hash
#[utoipa::path(
    get,
    path = "/tree/dir-hash",
    params(
        CodePreviewQuery
    ),
    responses(
        (status = 200, body = CommonResult<Vec<TreeHashItem>>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_tree_dir_hash(
    Query(query): Query<CodePreviewQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<TreeHashItem>>>, ApiError> {
    let path = std::path::Path::new(&query.path);
    let parent_path = path
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();
    let target_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    let data = state
        .api_handler(parent_path.as_ref())
        .await?
        .get_tree_dir_hash(parent_path.into(), target_name, Some(query.refs.as_str()))
        .await?;

    Ok(Json(CommonResult::success(Some(data))))
}

/// Check if a path can be cloned
#[utoipa::path(
    get,
    path = "/tree/path-can-clone",
    params(
        BlobContentQuery
    ),
    responses(
        (status = 200, body = CommonResult<bool>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn path_can_be_cloned(
    Query(query): Query<BlobContentQuery>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<bool>>, ApiError> {
    let path: PathBuf = query.path.clone().into();
    let import_dir = state.storage.config().monorepo.import_dir.clone();
    let res = if path.starts_with(&import_dir) {
        state
            .storage
            .git_db_storage()
            .find_git_repo_exact_match(path.to_str().unwrap())
            .await
            .unwrap()
            .is_some()
    } else {
        // any path under monorepo can be cloned
        true
    };
    Ok(Json(CommonResult::success(Some(res))))
}

/// Get blame information for a file
#[utoipa::path(
    get,
    path = "/blame",
    params(
        BlameRequest
    ),
    responses(
        (status = 200, body = CommonResult<BlameResult>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn get_file_blame(
    Query(params): Query<BlameRequest>,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<BlameResult>>, ApiError> {
    let ref_name = if params.refs.is_empty() {
        None
    } else {
        Some(params.refs.as_str())
    };

    let query = BlameQuery::from(&params);
    let result = state
        .api_handler(params.path.as_ref())
        .await?
        .get_file_blame(&params.path, ref_name, query)
        .await?;
    Ok(Json(CommonResult::success(Some(result))))
}

/// Preview unified diff for a single file before saving
#[utoipa::path(
    post,
    path = "/edit/diff-preview",
    request_body = DiffPreviewPayload,
    responses(
        (status = 200, body = CommonResult<DiffItemSchema>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn preview_diff(
    state: State<MonoApiServiceState>,
    Json(payload): Json<DiffPreviewPayload>,
) -> Result<Json<CommonResult<DiffItemSchema>>, ApiError> {
    let handler = state.api_handler(payload.path.as_ref()).await?;
    let item = handler.preview_file_diff(payload).await?.map(|x| x.into());
    Ok(Json(CommonResult::success(item)))
}

/// Save edit and create a commit
#[utoipa::path(
    post,
    path = "/edit/save",
    request_body = EditFilePayload,
    responses(
        (status = 200, body = CommonResult<EditFileResult>, content_type = "application/json")
    ),
    tag = CODE_PREVIEW
)]
async fn save_edit(
    state: State<MonoApiServiceState>,
    headers: HeaderMap,
    Json(payload): Json<EditFilePayload>,
) -> Result<Json<CommonResult<EditFileResult>>, ApiError> {
    let requester = trunk_write_requester(&state, &headers, &payload.path)?;
    let handler = state.api_handler(payload.path.as_ref()).await?;
    let res = handler.save_file_edit(payload.clone(), requester).await?;

    upsert_commit_binding(&state, &res.commit_id, payload.author_username.as_deref()).await?;

    Ok(Json(CommonResult::success(Some(res))))
}

fn trunk_write_requester(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
    path: &str,
) -> Result<Option<String>, ApiError> {
    let config = state.storage.config();
    if config.monorepo.push_policy != PushPolicy::Trunk {
        return Ok(None);
    }
    authorize_trunk_api_write(&config.git, headers, path).map(Some)
}

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;
    use crate::api::api_doc::ApiDoc;

    pub(super) fn path_list(router: OpenApiRouter<MonoApiServiceState>) -> Vec<String> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .merge(router)
            .split_for_parts()
            .1
            .paths
            .paths
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn readonly_preview_keeps_reads_and_omits_cl_writes() {
        let paths = path_list(readonly_routers());
        assert!(
            paths.iter().any(|p| p.contains("/blob")),
            "readonly preview must keep /blob: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/tree")),
            "readonly preview must keep /tree: {paths:?}"
        );
        for needle in ["create-entry", "/edit/save"] {
            assert!(
                paths.iter().all(|p| !p.contains(needle)),
                "readonly preview must not include {needle}: {paths:?}"
            );
        }
    }

    #[test]
    fn write_routers_register_create_entry_and_save() {
        let paths = path_list(write_routers());
        assert!(
            paths.iter().any(|p| p.contains("create-entry")),
            "{paths:?}"
        );
        assert!(paths.iter().any(|p| p.contains("/edit/save")), "{paths:?}");
        assert!(
            paths.iter().all(|p| !p.contains("preview")),
            "write_routers must omit preview_diff: {paths:?}"
        );
    }

    #[test]
    fn review_preview_registers_create_entry_and_save() {
        let paths = path_list(routers());
        assert!(
            paths.iter().any(|p| p.contains("create-entry")),
            "{paths:?}"
        );
        assert!(paths.iter().any(|p| p.contains("/edit/save")), "{paths:?}");
    }
}

/// LB-02 AC-1: the route is registered on `write_routers` (so both Review and
/// trunk get it) and stays out of the read-only preview surface.
#[cfg(test)]
#[test]
fn delete_entry_registered_on_write_routers() {
    let paths = tests::path_list(write_routers());
    assert!(
        paths.iter().any(|p| p.contains("delete-entry")),
        "{paths:?}"
    );
    let readonly = tests::path_list(readonly_routers());
    assert!(
        readonly.iter().all(|p| !p.contains("delete-entry")),
        "{readonly:?}"
    );
}

/// LB-02 AC-3d: the handler authorizes through `trunk_write_requester`
/// before it touches storage — a missing credential is 401 and a credential
/// that does not cover the parent path is 403.
#[cfg(test)]
#[tokio::test]
async fn delete_entry_calls_trunk_write_requester() {
    use std::sync::Arc;

    use axum::{
        http::{HeaderValue, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };

    use crate::{
        api::oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        ceres::api_service::cache::GitObjectCache,
        config::{PushAuth, PushTokenConfig},
        contract::policy::entitystore::SharedEntityStore,
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::Token);
    config.git.push_tokens = vec![PushTokenConfig {
        name: "lb02-ci".to_owned(),
        token: "secret-ok".to_owned(),
        paths: Some(vec!["/project".to_owned()]),
    }];
    let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
    let state = MonoApiServiceState {
        session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "lb02-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store: Arc::new(SharedEntityStore::new()),
        storage,
    };
    let body = DeleteEntryInfo {
        path: "/project".to_owned(),
        name: "gone".to_owned(),
        author_username: None,
        skip_build: true,
    };

    let Err(err) = delete_entry(State(state.clone()), HeaderMap::new(), Json(body.clone())).await
    else {
        panic!("missing credential must be rejected");
    };
    assert_eq!(err.into_response().status(), StatusCode::UNAUTHORIZED);

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret-ok"));
    let Err(err) = delete_entry(
        State(state),
        headers,
        Json(DeleteEntryInfo {
            path: "/other".to_owned(),
            ..body
        }),
    )
    .await
    else {
        panic!("uncovered path must be rejected");
    };
    assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
}
