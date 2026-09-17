use std::path::Path as StdPath;

use anyhow::anyhow;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState, api_doc::TAG_MANAGE, router::preview_router::trunk_write_requester,
    },
    ceres::model::tag::{
        CreateTagRequest, DeleteTagResponse, TagListQuery, TagListResponse, TagResponse,
    },
    common::errors::{ApiError, map_ceres_error},
    contract::api::common::{CommonResult, Pagination},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(create_tag))
        .routes(routes!(list_tags))
        .routes(routes!(get_tag))
        .routes(routes!(delete_tag))
}

// Note: query-based path_context is intentionally removed for tag APIs; repo selection is
// resolved from router context (MonoApiServiceState) or request body for create if needed.

/// Resolve a target string (possibly "HEAD" or a commit hash) to an actual commit SHA.
/// If target_opt is Some and not "HEAD", return it directly. If it's None or "HEAD",
/// resolve to the repository's current HEAD/default branch commit.
async fn resolve_target_commit_id(
    state: &MonoApiServiceState,
    path_context: Option<&str>,
    target_opt: Option<&str>,
) -> Result<String, ApiError> {
    // if caller provided a specific non-"HEAD" target, use it directly
    if let Some(t) = target_opt
        && t != "HEAD"
        && !t.is_empty()
    {
        return Ok(t.to_string());
    }

    let import_dir = state.storage.config().monorepo.import_dir.clone();
    if let Some(path) = path_context {
        let std_path = StdPath::new(path);
        if std_path.starts_with(&import_dir) && std_path != StdPath::new(&import_dir) {
            // find repo model (longest-prefix match)
            if let Some(repo_model) = state
                .storage
                .git_db_storage()
                .find_git_repo_like_path(path)
                .await
                .map_err(|e| ApiError::from(anyhow!("Database error: {}", e)))?
            {
                let git = state.storage.git_db_storage();
                // try default branch ref
                if let Ok(Some(r)) = git.get_default_ref(repo_model.id).await {
                    return Ok(r.ref_git_id);
                }
                // fallback: any import ref for repo
                if let Ok(refs) = git.get_ref(repo_model.id).await
                    && let Some(r) = refs.into_iter().next()
                {
                    return Ok(r.ref_git_id);
                }
                return Ok("HEAD".to_string());
            }
            // If db lookup did not find a repo despite prefix, fall through to mono logic
        } else {
            // path is outside import_dir → mono
            let mono = state.storage.mono_storage();
            let resolved_path = path_context.unwrap_or("/");
            if let Ok(Some(r)) = mono.get_main_ref(resolved_path).await {
                return Ok(r.ref_commit_hash);
            }
            if let Ok(Some(root_ref)) = mono.get_main_ref("/").await {
                return Ok(root_ref.ref_commit_hash);
            }
            return Ok("HEAD".to_string());
        }
    }

    // Default fallback: try mono root ref
    let mono = state.storage.mono_storage();
    if let Ok(Some(root_ref)) = mono.get_main_ref("/").await {
        return Ok(root_ref.ref_commit_hash);
    }
    Ok("HEAD".to_string())
}

// Validate tag name against a conservative subset of Git ref rules.
fn validate_tag_name(name: &str) -> Result<(), ApiError> {
    // Basic checks that don't require iterating characters
    if name.is_empty() {
        return Err(ApiError::bad_request(anyhow!("Tag name must not be empty")));
    }

    if name.len() > 255 {
        return Err(ApiError::bad_request(anyhow!("Tag name is too long")));
    }

    if name.contains("..") || name.contains("@{") {
        return Err(ApiError::bad_request(anyhow!(
            "Tag name contains reserved sequence '..' or '@{{'"
        )));
    }

    if name.contains("//") {
        return Err(ApiError::bad_request(anyhow!(
            "Tag name must not contain '//'"
        )));
    }

    if name.ends_with(".lock") {
        return Err(ApiError::bad_request(anyhow!(
            "Tag name must not end with '.lock'"
        )));
    }

    // Single-pass character validation: forbidden chars, NUL, control chars
    let forbidden = [' ', '~', '^', ':', '?', '*', '[', '\\'];
    for c in name.chars() {
        if forbidden.contains(&c) {
            return Err(ApiError::bad_request(anyhow!(format!(
                "Tag name '{}' contains forbidden character '{}'",
                name, c
            ))));
        }
        if c == '\0' || c.is_control() {
            return Err(ApiError::bad_request(anyhow!(
                "Tag name contains invalid control characters"
            )));
        }
    }

    Ok(())
}

/// Create Tag
///
/// plan-20260917 LB-04 (ADR-LB-05 item 5): on trunk / storage-only the write
/// is gated by `git.push_auth` through [`trunk_write_requester`], with
/// `path_context` (default `/`) as the authorization path, before the name is
/// validated or storage is touched. The handler answers **200** (the utoipa
/// annotation used to claim 201).
#[utoipa::path(
    post,
    path = "/tags",
    request_body(
        content = CreateTagRequest,
        content_type = "application/json"
    ),
    responses(
        (status = 200, body = CommonResult<TagResponse>, content_type = "application/json")
    ),
    tag = TAG_MANAGE
)]
async fn create_tag(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
    Json(req): Json<CreateTagRequest>,
) -> Result<Json<CommonResult<TagResponse>>, ApiError> {
    trunk_write_requester(&state, &headers, req.path_context.as_deref().unwrap_or("/"))?;
    // We ignore query path_context for tag creation; use request target commit directly.
    validate_tag_name(&req.name)?;
    // Resolve target commit: if caller provided a target, use it; otherwise resolve using optional path_context.
    let resolved_target = if let Some(t) = req.target.as_deref() {
        if t != "HEAD" && !t.is_empty() {
            t.to_string()
        } else {
            // fallback: resolve using provided path_context if any
            resolve_target_commit_id(&state, req.path_context.as_deref(), None).await?
        }
    } else {
        resolve_target_commit_id(&state, req.path_context.as_deref(), None).await?
    };

    // dispatch to repo-specific handler via ApiHandler using path_context if provided
    let repo_path_ref = req.path_context.as_deref().unwrap_or("/");
    let api = state
        .api_handler(std::path::Path::new(repo_path_ref))
        .await
        .map_err(|e| map_ceres_error(e, "Failed to resolve api handler"))?;

    let tag_info = api
        .create_tag(
            Some(repo_path_ref.to_string()),
            req.name.clone(),
            Some(resolved_target),
            req.tagger_name.clone(),
            req.tagger_email.clone(),
            req.message.clone(),
        )
        .await
        .map_err(|e| map_ceres_error(e, "Failed to create tag"))?;

    let response = TagResponse {
        name: tag_info.name,
        tag_id: tag_info.tag_id,
        object_id: tag_info.object_id,
        object_type: tag_info.object_type,
        tagger: tag_info.tagger,
        message: tag_info.message,
        created_at: tag_info.created_at,
    };
    Ok(Json(CommonResult::success(Some(response))))
}

/// List tags (plan-20260918 ADR-FT-02): GET-only; POST is not registered (405).
#[utoipa::path(
    get,
    path = "/tags/list",
    params(TagListQuery),
    responses(
        (status = 200, body = CommonResult<TagListResponse>, content_type = "application/json")
    ),
    tag = TAG_MANAGE
)]
async fn list_tags(
    State(state): State<MonoApiServiceState>,
    Query(query): Query<TagListQuery>,
) -> Result<Json<CommonResult<TagListResponse>>, ApiError> {
    if query.per_page == 0 {
        return Err(ApiError::bad_request(anyhow!(
            "[code:400] per_page must be >= 1"
        )));
    }
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let repo_path_ref = if query.path.trim().is_empty() {
        "/"
    } else {
        query.path.trim()
    };
    let api = state
        .api_handler(std::path::Path::new(repo_path_ref))
        .await
        .map_err(|e| map_ceres_error(e, "Failed to resolve api handler"))?;
    let (tags, total) = api
        .list_tags(Some(repo_path_ref.to_string()), pagination)
        .await
        .map_err(|e| map_ceres_error(e, "Failed to list tags"))?;
    let tag_responses: Vec<TagResponse> = tags
        .into_iter()
        .map(|t| TagResponse {
            name: t.name,
            tag_id: t.tag_id,
            object_id: t.object_id,
            object_type: t.object_type,
            tagger: t.tagger,
            message: t.message,
            created_at: t.created_at,
        })
        .collect();

    let response = TagListResponse {
        total,
        items: tag_responses,
    };
    Ok(Json(CommonResult::success(Some(response))))
}

/// Get Tag by name
#[utoipa::path(
    get,
    path = "/tags/{name}",
    responses(
        (status = 200, body = CommonResult<TagResponse>, content_type = "application/json"),
        (status = 404, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = TAG_MANAGE
)]
async fn get_tag(
    State(state): State<MonoApiServiceState>,
    Path(name): Path<String>,
) -> Result<Json<CommonResult<TagResponse>>, ApiError> {
    let repo_path = "/".to_string();
    let api = state
        .api_handler(std::path::Path::new(&repo_path))
        .await
        .map_err(|e| map_ceres_error(e, "Failed to resolve api handler"))?;

    match api
        .get_tag(Some(repo_path.clone()), name.clone())
        .await
        .map_err(|e| map_ceres_error(e, "Failed to get tag"))?
    {
        Some(t) => {
            let response = TagResponse {
                name: t.name,
                tag_id: t.tag_id,
                object_id: t.object_id,
                object_type: t.object_type,
                tagger: t.tagger,
                message: t.message,
                created_at: t.created_at,
            };
            Ok(Json(CommonResult::success(Some(response))))
        }
        None => Err(ApiError::not_found(anyhow!(format!(
            "Tag '{}' not found",
            name
        )))),
    }
}

/// Delete Tag
///
/// plan-20260917 LB-04 (ADR-LB-05 item 5): delete has no body, so on trunk /
/// storage-only the authorization path is fixed to `/` — a token whose
/// `paths` does not cover `/` gets 403 on every tag delete. Checked before
/// any storage access.
#[utoipa::path(
    delete,
    path = "/tags/{name}",
    responses(
        (status = 200, body = CommonResult<DeleteTagResponse>, content_type = "application/json"),
        (status = 404, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = TAG_MANAGE
)]
async fn delete_tag(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<CommonResult<DeleteTagResponse>>, ApiError> {
    trunk_write_requester(&state, &headers, "/")?;
    let repo_path = "/".to_string(); // use root for delete operations by default
    let api = state
        .api_handler(std::path::Path::new(&repo_path))
        .await
        .map_err(|e| map_ceres_error(e, "Failed to resolve api handler"))?;
    api.delete_tag(Some(repo_path.clone()), name.clone())
        .await
        .map_err(|e| map_ceres_error(e, "Failed to delete tag"))?;

    let response = DeleteTagResponse {
        deleted_tag: name.clone(),
        message: format!("Tag '{}' successfully deleted", name),
    };
    Ok(Json(CommonResult::success(Some(response))))
}

#[cfg(test)]
fn openapi_of(router: OpenApiRouter<MonoApiServiceState>) -> utoipa::openapi::OpenApi {
    router.split_for_parts().1
}

/// LB-04 AC-1/AC-2: the four tag routes are mounted on the storage-only /
/// trunk surface (Review already had them).
#[cfg(test)]
#[test]
fn tag_routes_registered_on_storage_only_routers() {
    let api = openapi_of(crate::api::api_router::storage_only_routers_with(false));
    let paths: Vec<String> = api.paths.paths.keys().cloned().collect();
    for needle in ["/tags", "/tags/list", "/tags/{name}"] {
        assert!(
            paths.iter().any(|p| p.ends_with(needle)),
            "{needle} missing from storage-only routers: {paths:?}"
        );
    }
    let item = api.paths.paths.get("/tags/{name}").expect("/tags/{name}");
    assert!(
        item.get.is_some() && item.delete.is_some(),
        "get + delete on /tags/{{name}}"
    );
    let list = api.paths.paths.get("/tags/list").expect("/tags/list");
    assert!(list.get.is_some(), "list is GET");
    assert!(list.post.is_none(), "list is not POST");
}

/// LB-04 AC-6: the create-tag OpenAPI response is the handler's real status,
/// 200 — the former 201 annotation must not survive alongside it.
#[cfg(test)]
#[test]
fn tag_create_openapi_status_is_200() {
    let api = openapi_of(routers());
    let post = api
        .paths
        .paths
        .get("/tags")
        .and_then(|item| item.post.as_ref())
        .expect("POST /tags");
    let codes: Vec<&String> = post.responses.responses.keys().collect();
    assert!(codes.iter().any(|c| c.as_str() == "200"), "{codes:?}");
    assert!(codes.iter().all(|c| c.as_str() != "201"), "{codes:?}");
}

/// LB-04 AC-3: both tag writes authorize through `trunk_write_requester`
/// before any storage access — a missing credential is 401; a token scoped
/// to `/project` is 403 for delete (authorization path `/`) and for a create
/// whose `path_context` is omitted or `/`, while `path_context = "/project"`
/// passes the gate.
#[cfg(test)]
#[tokio::test]
async fn tag_writes_call_trunk_write_requester() {
    use std::sync::Arc;

    use axum::{
        http::{HeaderValue, StatusCode, header::AUTHORIZATION},
        response::IntoResponse,
    };

    use crate::{
        api::oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        ceres::api_service::cache::GitObjectCache,
        config::{PushAuth, PushPolicy, PushTokenConfig},
        contract::policy::entitystore::SharedEntityStore,
    };

    fn body(path_context: Option<&str>) -> Json<CreateTagRequest> {
        Json(CreateTagRequest {
            name: "lb04-v1".to_owned(),
            target: None,
            path_context: path_context.map(str::to_owned),
            tagger_name: None,
            tagger_email: None,
            message: None,
        })
    }

    let temp = tempfile::tempdir().expect("temp dir");
    let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::Token);
    config.git.push_tokens = vec![PushTokenConfig {
        name: "lb04-ci".to_owned(),
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
            prefix: "lb04-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store: Arc::new(SharedEntityStore::new()),
        storage,
    };

    let Err(err) = create_tag(State(state.clone()), HeaderMap::new(), body(None)).await else {
        panic!("create without credential must be rejected");
    };
    assert_eq!(err.into_response().status(), StatusCode::UNAUTHORIZED);
    let Err(err) = delete_tag(
        State(state.clone()),
        HeaderMap::new(),
        Path("lb04-v1".to_owned()),
    )
    .await
    else {
        panic!("delete without credential must be rejected");
    };
    assert_eq!(err.into_response().status(), StatusCode::UNAUTHORIZED);

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret-ok"));
    for path_context in [None, Some("/")] {
        let Err(err) = create_tag(State(state.clone()), headers.clone(), body(path_context)).await
        else {
            panic!(
                "create with a /project-scoped token and path_context {path_context:?} must be 403"
            );
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path_context:?}");
    }
    let Err(err) = delete_tag(
        State(state.clone()),
        headers.clone(),
        Path("lb04-v1".to_owned()),
    )
    .await
    else {
        panic!("delete with a /project-scoped token must be 403");
    };
    assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);

    // `path_context = "/project"` is covered: the gate passes and the handler
    // proceeds to storage (no `/project` tip in this bare store, hence not 401/403).
    match create_tag(State(state), headers, body(Some("/project"))).await {
        Ok(_) => {}
        Err(err) => {
            let status = err.into_response().status();
            assert!(
                status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                "covered path_context must pass the auth gate, got {status}"
            );
        }
    }
}
