//! Feature-gated FastCDC Media HTTP API (`libra/media/v1`).
//!
//! Nested under the LFS mount so the public URL is
//! `<repo>.git/info/lfs/libra/media/v1/...`. Every route requires
//! [`AccessTokenUser`]; none reuse the standard LFS objects “capability URL
//! without re-auth” exception.

use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header::CONTENT_TYPE},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{AccessTokenUser, model::LoginUser},
        router::lfs_router::LfsRepoContext,
    },
    ceres::lfs::media::{
        chunker, finalize,
        protocol::{
            Capabilities, FinalizeAcceptedResponse, FinalizeTaskResponse, MAX_ENVELOPE_SIZE,
            ManifestPage, MediaManifest, MissingChunksResponse, SealResponse,
        },
        scope::{MediaObjectKind, MediaScope},
        service::{MediaError, MediaService},
    },
};

const MEDIA_TAG: &str = "FastCDC Media";
const MEDIA_JSON: &str = "application/json";

pub fn media_routes() -> OpenApiRouter<MonoApiServiceState> {
    let json = OpenApiRouter::new()
        .routes(routes!(media_capabilities))
        .routes(routes!(media_prepare))
        .routes(routes!(media_put_page))
        .routes(routes!(media_seal))
        .routes(routes!(media_missing))
        .routes(routes!(media_finalize))
        .routes(routes!(media_get_task))
        .routes(routes!(media_get_manifest))
        .routes(routes!(media_get_chunk))
        .layer(DefaultBodyLimit::max(MAX_ENVELOPE_SIZE))
        .layer(middleware::from_fn(reject_oversize_media_body));
    let chunks = OpenApiRouter::new()
        .routes(routes!(media_upload_chunk))
        .layer(DefaultBodyLimit::max(chunker::MAX_SIZE))
        .layer(middleware::from_fn(reject_oversize_media_body));
    json.merge(chunks)
}

fn media_json(code: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "message": message }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = code;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(MEDIA_JSON));
    response
}

fn map_media_error(err: MediaError) -> Response {
    match err {
        MediaError::Invalid(msg) | MediaError::Json(msg) => {
            media_json(StatusCode::BAD_REQUEST, &msg)
        }
        MediaError::NotFound => media_json(StatusCode::NOT_FOUND, "not found"),
        MediaError::Conflict(msg) => media_json(StatusCode::CONFLICT, &msg),
        MediaError::TooManyRequests => {
            let mut response =
                media_json(StatusCode::TOO_MANY_REQUESTS, "media finalize queue full");
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("5"));
            response
        }
        MediaError::Storage | MediaError::Io(_) => media_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "media object store error",
        ),
    }
}

fn actor_from(user: &LoginUser) -> &str {
    if user.website_user_id.is_empty() {
        user.username.as_str()
    } else {
        user.website_user_id.as_str()
    }
}

fn media_scope(
    user: &LoginUser,
    repo: Option<Extension<LfsRepoContext>>,
) -> Result<MediaScope, MediaError> {
    let repo = repo.map(|Extension(ctx)| ctx.0).unwrap_or_default();
    MediaScope::from_server(actor_from(user), &repo)
        .map_err(|_| MediaError::Invalid("invalid media actor or repository".to_string()))
}

fn media_service(state: &MonoApiServiceState) -> MediaService {
    state
        .storage
        .lfs_service
        .media(state.storage.media_paging_storage())
}

fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn body_limit_for(method: &Method, path: &str) -> usize {
    if *method == Method::PUT && path.contains("/chunks/") {
        chunker::MAX_SIZE
    } else {
        MAX_ENVELOPE_SIZE
    }
}

async fn reject_oversize_media_body(req: Request<Body>, next: Next) -> Response {
    let max = body_limit_for(req.method(), req.uri().path());
    if content_length(req.headers()).is_some_and(|len| len > max) {
        return media_json(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    }
    next.run(req).await
}

#[utoipa::path(
    get,
    path = "/libra/media/v1/capabilities",
    responses(
        (status = 200, description = "FastCDC v1 capability document", content_type = "application/json"),
        (status = 401, description = "Missing or invalid access token")
    ),
    tag = MEDIA_TAG,
    description = "Authenticated FastCDC capability probe. Also served at `<repo>.git/info/lfs/libra/media/v1/capabilities`."
)]
pub async fn media_capabilities(
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
) -> Response {
    if let Err(err) = media_scope(&user, repo) {
        return map_media_error(err);
    }
    Json(Capabilities::v1()).into_response()
}

#[utoipa::path(
    post,
    path = "/libra/media/v1/manifests",
    request_body(content = String, content_type = "application/json"),
    responses(
        (status = 200, description = "Prepare response", content_type = "application/json"),
        (status = 400, description = "Invalid manifest"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 413, description = "Body exceeds 1 MiB envelope")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_prepare(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_ENVELOPE_SIZE {
        return media_json(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    }
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => return map_media_error(MediaError::Invalid("manifest is not UTF-8".into())),
    };
    let manifest = match MediaManifest::from_json(text) {
        Ok(manifest) => manifest,
        Err(err) => return map_media_error(MediaError::Invalid(err.to_string())),
    };
    match media_service(&state).prepare(&scope, manifest).await {
        Ok(prepared) => Json(prepared).into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    put,
    path = "/libra/media/v1/manifests/{manifest_id}/pages/{page_no}",
    params(
        ("manifest_id" = String, Path),
        ("page_no" = u32, Path),
    ),
    request_body(content = String, content_type = "application/json"),
    responses(
        (status = 200, description = "Page stored"),
        (status = 400, description = "Invalid page"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 409, description = "Page conflict"),
        (status = 413, description = "Body exceeds 1 MiB envelope")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_put_page(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path((manifest_id, page_no)): Path<(String, u32)>,
    body: Bytes,
) -> Response {
    if body.len() > MAX_ENVELOPE_SIZE {
        return media_json(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    }
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => return map_media_error(MediaError::Invalid("page is not UTF-8".into())),
    };
    let page = match ManifestPage::from_json(text) {
        Ok(page) => page,
        Err(err) => return map_media_error(MediaError::Invalid(err.to_string())),
    };
    if page.page_no != page_no {
        return map_media_error(MediaError::Invalid(
            "page_no in body does not match path".into(),
        ));
    }
    match media_service(&state)
        .put_page(&scope, &manifest_id, page)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    post,
    path = "/libra/media/v1/manifests/{manifest_id}/seal",
    params(("manifest_id" = String, Path)),
    responses(
        (status = 200, description = "Sealed layout", content_type = "application/json"),
        (status = 400, description = "Invalid seal"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 409, description = "Non-canonical pages")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_seal(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path(manifest_id): Path<String>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    match media_service(&state).seal(&scope, &manifest_id).await {
        Ok(sealed) => Json::<SealResponse>(sealed).into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    get,
    path = "/libra/media/v1/manifests/{manifest_id}/missing",
    params(
        ("manifest_id" = String, Path),
        ("cursor" = Option<String>, Query, description = "Opaque missing cursor"),
    ),
    responses(
        (status = 200, description = "Missing chunk hashes", content_type = "application/json"),
        (status = 400, description = "Invalid cursor"),
        (status = 401, description = "Missing or invalid access token")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_missing(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path(manifest_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<MissingQuery>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    match media_service(&state)
        .missing_chunks_page(&scope, &manifest_id, query.cursor.as_deref())
        .await
    {
        Ok(page) => Json::<MissingChunksResponse>(page).into_response(),
        Err(err) => map_media_error(err),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct MissingQuery {
    cursor: Option<String>,
}

#[utoipa::path(
    put,
    path = "/libra/media/v1/manifests/{manifest_id}/chunks/{chunk_hash}",
    params(
        ("manifest_id" = String, Path),
        ("chunk_hash" = String, Path),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Chunk stored"),
        (status = 400, description = "Invalid chunk"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 409, description = "Corrupt existing chunk"),
        (status = 413, description = "Body exceeds max chunk size")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_upload_chunk(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path((manifest_id, chunk_hash)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if body.len() > chunker::MAX_SIZE {
        return media_json(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    }
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    match media_service(&state)
        .upload_chunk(&scope, &manifest_id, &chunk_hash, body)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    post,
    path = "/libra/media/v1/manifests/{manifest_id}/finalize",
    params(("manifest_id" = String, Path)),
    responses(
        (status = 202, description = "Finalize task accepted", content_type = "application/json"),
        (status = 400, description = "Session not sealed or invalid"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 404, description = "Pending session not found"),
        (status = 409, description = "Permanent finalize failure"),
        (status = 429, description = "Finalize queue full"),
        (status = 500, description = "Storage error")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_finalize(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path(manifest_id): Path<String>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    let media = media_service(&state);
    match finalize::enqueue_finalize(
        media,
        state.storage.lfs_service.lfs_storage.clone(),
        scope,
        manifest_id,
        state.storage.storage_event_emitter.clone(),
    )
    .await
    {
        Ok(accepted) => {
            let mut response = Json::<FinalizeAcceptedResponse>(accepted).into_response();
            *response.status_mut() = StatusCode::ACCEPTED;
            response
        }
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    get,
    path = "/libra/media/v1/tasks/{task_id}",
    params(("task_id" = String, Path)),
    responses(
        (status = 200, description = "Finalize task status", content_type = "application/json"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 404, description = "Task not found in this scope")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_get_task(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path(task_id): Path<String>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    match finalize::task_status(&media_service(&state), &scope, &task_id).await {
        Ok(status) => Json::<FinalizeTaskResponse>(status).into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    get,
    path = "/libra/media/v1/manifests/by-media/{oid}",
    params(("oid" = String, Path)),
    responses(
        (status = 200, description = "Published manifest", content_type = "application/json"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 404, description = "Not found")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_get_manifest(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path(oid): Path<String>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    match load_published(&media_service(&state), &scope, &oid).await {
        Ok(published) => Json(published).into_response(),
        Err(err) => map_media_error(err),
    }
}

#[utoipa::path(
    get,
    path = "/libra/media/v1/manifests/by-media/{oid}/chunks/{chunk_hash}",
    params(
        ("oid" = String, Path),
        ("chunk_hash" = String, Path),
    ),
    responses(
        (status = 200, description = "Chunk bytes", content_type = "application/octet-stream"),
        (status = 401, description = "Missing or invalid access token"),
        (status = 404, description = "Not found")
    ),
    tag = MEDIA_TAG
)]
pub async fn media_get_chunk(
    State(state): State<MonoApiServiceState>,
    AccessTokenUser(user): AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    Path((oid, chunk_hash)): Path<(String, String)>,
) -> Response {
    let scope = match media_scope(&user, repo) {
        Ok(scope) => scope,
        Err(err) => return map_media_error(err),
    };
    let media = media_service(&state);
    let published = match load_published(&media, &scope, &oid).await {
        Ok(published) => published,
        Err(err) => return map_media_error(err),
    };
    if !published
        .manifest
        .chunks
        .iter()
        .any(|chunk| chunk.chunk_hash == chunk_hash)
    {
        return map_media_error(MediaError::NotFound);
    }
    let key = match scope.object_key(MediaObjectKind::Chunk, &chunk_hash) {
        Ok(key) => key,
        Err(_) => return map_media_error(MediaError::NotFound),
    };
    match media.read_bytes(&key, chunker::MAX_SIZE).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            response
        }
        Err(err) => map_media_error(err),
    }
}

async fn load_published(
    media: &MediaService,
    scope: &MediaScope,
    oid: &str,
) -> Result<crate::ceres::lfs::media::protocol::ManifestResponse, MediaError> {
    crate::ceres::lfs::media::publication::load_by_media(media, scope, oid).await
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header::AUTHORIZATION},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::{
            oauth::api_store::{BrowserSessionStore, CountingSessionStore},
            router::lfs_router,
        },
        ceres::{
            api_service::cache::GitObjectCache,
            lfs::{
                digest::LfsDigest,
                media::protocol::{
                    ChunkEntry, CreatedBy, FinalizeAcceptedResponse, FinalizeTaskResponse,
                    ManifestPage, PrepareResponse,
                },
            },
        },
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            storage::{Storage, object_storage::build_object_storage},
            tests::test_storage,
        },
        orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
    };

    const MEDIA_PATHS: &[&str] = &[
        "/libra/media/v1/capabilities",
        "/libra/media/v1/manifests",
        "/libra/media/v1/manifests/{manifest_id}/pages/{page_no}",
        "/libra/media/v1/manifests/{manifest_id}/seal",
        "/libra/media/v1/manifests/{manifest_id}/missing",
        "/libra/media/v1/manifests/{manifest_id}/chunks/{chunk_hash}",
        "/libra/media/v1/manifests/{manifest_id}/finalize",
        "/libra/media/v1/tasks/{task_id}",
        "/libra/media/v1/manifests/by-media/{oid}",
        "/libra/media/v1/manifests/by-media/{oid}/chunks/{chunk_hash}",
    ];

    fn openapi_paths() -> Vec<String> {
        OpenApiRouter::new()
            .merge(media_routes())
            .split_for_parts()
            .1
            .paths
            .paths
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn openapi_lists_media_routes() {
        let paths = openapi_paths();
        for needle in MEDIA_PATHS {
            assert!(
                paths.iter().any(|p| p == needle),
                "missing {needle} in {paths:?}"
            );
        }
    }

    #[test]
    fn nested_lfs_openapi_uses_api_v1_lfs_prefix() {
        let paths: Vec<String> = lfs_router::routers()
            .split_for_parts()
            .1
            .paths
            .paths
            .keys()
            .cloned()
            .collect();
        assert!(
            paths
                .iter()
                .any(|p| p == "/api/v1/lfs/libra/media/v1/capabilities"),
            "runtime OpenAPI must describe Media under the LFS mount: {paths:?}"
        );
        for needle in MEDIA_PATHS {
            let expected = format!("/api/v1/lfs{needle}");
            assert!(
                paths.iter().any(|p| p == &expected),
                "missing {expected} in {paths:?}"
            );
        }
    }

    fn state_from(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "lfs-media-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage,
        }
    }

    struct Harness {
        _db_dir: tempfile::TempDir,
        _obj_dir: tempfile::TempDir,
        state: MonoApiServiceState,
    }

    async fn harness() -> Harness {
        let db_dir = tempfile::tempdir().unwrap();
        let mut storage = test_storage(db_dir.path()).await;
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        storage.lfs_service.obj_storage = build_object_storage(&cfg).await.unwrap();
        let state = state_from(storage);
        Harness {
            _db_dir: db_dir,
            _obj_dir: obj_dir,
            state,
        }
    }

    fn media_app(state: MonoApiServiceState) -> Router {
        media_routes().with_state(state).into()
    }

    fn lfs_mount(state: MonoApiServiceState) -> Router {
        let lfs: Router = lfs_router::lfs_routes().with_state(state).into();
        Router::new().nest("/info/lfs", lfs)
    }

    fn with_repo_path(mut req: Request<Body>, repo: &str) -> Request<Body> {
        req.extensions_mut()
            .insert(LfsRepoContext(repo.to_string()));
        req
    }

    fn with_repo(req: Request<Body>) -> Request<Body> {
        with_repo_path(req, "/acme/app.git")
    }

    fn sample_manifest(data: &[u8]) -> MediaManifest {
        let chunks = chunker::chunk_bytes(data);
        MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: LfsDigest::sha256_of(data).hex().to_owned(),
            media_size: data.len() as u64,
            chunks: chunks
                .into_iter()
                .map(|c| ChunkEntry {
                    offset: c.offset,
                    length: c.length,
                    chunk_hash: c.chunk_hash,
                    encoded_length: c.length,
                    compression: "none".to_string(),
                    checksum: None,
                })
                .collect(),
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string()],
            },
            fallback_oid: None,
        }
    }

    async fn token_for(state: &MonoApiServiceState, name: &str) -> String {
        state
            .storage
            .user_storage()
            .generate_token(name.to_string())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn missing_token_is_401_on_every_media_route() {
        let h = harness().await;
        let app = media_app(h.state.clone());
        let id = "a".repeat(64);
        let routes = [
            Request::get("/libra/media/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
            Request::post("/libra/media/v1/manifests")
                .body(Body::from("{}"))
                .unwrap(),
            Request::put(format!("/libra/media/v1/manifests/{id}/chunks/{id}"))
                .body(Body::empty())
                .unwrap(),
            Request::post(format!("/libra/media/v1/manifests/{id}/finalize"))
                .body(Body::empty())
                .unwrap(),
            Request::get(format!("/libra/media/v1/manifests/by-media/{id}"))
                .body(Body::empty())
                .unwrap(),
            Request::get(format!(
                "/libra/media/v1/manifests/by-media/{id}/chunks/{id}"
            ))
            .body(Body::empty())
            .unwrap(),
        ];
        for req in routes {
            let response = app.clone().oneshot(with_repo(req)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "expected 401 for {}",
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn capabilities_and_malformed_manifest() {
        let h = harness().await;
        let token = token_for(&h.state, "alice").await;
        let cap = media_app(h.state.clone())
            .oneshot(with_repo(
                Request::get("/libra/media/v1/capabilities")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(cap.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(cap.into_body(), usize::MAX)
            .await
            .unwrap();
        let caps: Capabilities = serde_json::from_slice(&bytes).unwrap();
        assert!(caps.chunked_lfs);
        assert_eq!(caps.chunk_algorithms, vec![chunker::ALGORITHM.to_string()]);
        assert_eq!(caps.hash_algorithms, vec!["sha256".to_string()]);
        assert_eq!(caps.max_chunk_size, chunker::MAX_SIZE as u64);
        assert_eq!(caps.max_manifest_size, MAX_ENVELOPE_SIZE as u64);
        assert!(caps.supports_batch_exists);
        assert!(caps.batch_exists);
        assert!(caps.supports_standard_lfs_fallback);
        assert!(caps.standard_lfs_fallback);
        assert!(caps.supports_manifest_id_read);
        assert_eq!(caps.manifest_paging, "v1");
        assert_eq!(caps.max_page_entries, 4096);
        assert_eq!(caps.max_page_bytes, MAX_ENVELOPE_SIZE as u64);

        let bad = media_app(h.state)
            .oneshot(with_repo(
                Request::post("/libra/media/v1/manifests")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(CONTENT_TYPE, MEDIA_JSON)
                    .body(Body::from("{not-a-manifest"))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_repository_context_is_400() {
        let h = harness().await;
        let token = token_for(&h.state, "alice").await;
        let response = media_app(h.state)
            .oneshot(
                Request::get("/libra/media/v1/capabilities")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oversize_bodies_are_413_from_content_length() {
        let h = harness().await;
        let token = token_for(&h.state, "alice").await;
        let chunk = media_app(h.state.clone())
            .oneshot(with_repo(
                Request::put(format!(
                    "/libra/media/v1/manifests/{}/chunks/{}",
                    "a".repeat(64),
                    "b".repeat(64)
                ))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("content-length", (chunker::MAX_SIZE + 1).to_string())
                .body(Body::empty())
                .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(chunk.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let manifest = media_app(h.state)
            .oneshot(with_repo(
                Request::post("/libra/media/v1/manifests")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header("content-length", (MAX_ENVELOPE_SIZE + 1).to_string())
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(manifest.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn other_actor_cannot_see_pending() {
        let h = harness().await;
        let alice = token_for(&h.state, "alice").await;
        let bob = token_for(&h.state, "bob").await;
        let manifest = sample_manifest(b"media-http-actor-scope");
        let body = serde_json::to_vec(&manifest).unwrap();
        let prepared = media_app(h.state.clone())
            .oneshot(with_repo(
                Request::post("/libra/media/v1/manifests")
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, MEDIA_JSON)
                    .body(Body::from(body))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(prepared.status(), StatusCode::OK);
        let prepared: PrepareResponse = serde_json::from_slice(
            &axum::body::to_bytes(prepared.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let other = media_app(h.state)
            .oneshot(with_repo(
                Request::post(format!(
                    "/libra/media/v1/manifests/{}/finalize",
                    prepared.manifest_id
                ))
                .header(AUTHORIZATION, format!("Bearer {bob}"))
                .body(Body::empty())
                .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn same_actor_other_repository_cannot_see_pending() {
        let h = harness().await;
        let alice = token_for(&h.state, "alice").await;
        let manifest = sample_manifest(b"media-http-repo-scope");
        let body = serde_json::to_vec(&manifest).unwrap();
        let prepared = media_app(h.state.clone())
            .oneshot(with_repo(
                Request::post("/libra/media/v1/manifests")
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, MEDIA_JSON)
                    .body(Body::from(body))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(prepared.status(), StatusCode::OK);
        let prepared: PrepareResponse = serde_json::from_slice(
            &axum::body::to_bytes(prepared.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let other_repo = media_app(h.state)
            .oneshot(with_repo_path(
                Request::post(format!(
                    "/libra/media/v1/manifests/{}/finalize",
                    prepared.manifest_id
                ))
                .header(AUTHORIZATION, format!("Bearer {alice}"))
                .body(Body::empty())
                .unwrap(),
                "/other/repo.git",
            ))
            .await
            .unwrap();
        assert_eq!(other_repo.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn repo_prefixed_lfs_mount_returns_capabilities_and_401_without_token() {
        let h = harness().await;
        let token = token_for(&h.state, "alice").await;
        let app = lfs_mount(h.state.clone());
        // Production rewrite strips `<repo>.git` and stores it in LfsRepoContext
        // (`http_server::rewrite_lfs_request_uri`); this is the post-rewrite URL.
        let path = "/info/lfs/libra/media/v1/capabilities";

        let denied = app
            .clone()
            .oneshot(with_repo(Request::get(path).body(Body::empty()).unwrap()))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let ok = app
            .oneshot(with_repo(
                Request::get(path)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let caps: Capabilities = serde_json::from_slice(
            &axum::body::to_bytes(ok.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(caps.chunk_algorithms, vec![chunker::ALGORITHM.to_string()]);
        assert_eq!(caps.hash_algorithms, vec!["sha256".to_string()]);
    }

    #[tokio::test]
    async fn status_mapping_hides_storage_keys() {
        let resp = map_media_error(MediaError::Storage);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(text, r#"{"message":"media object store error"}"#);
        assert!(!text.contains("fastcdc-v2020-32k/"));
        assert_eq!(
            map_media_error(MediaError::Invalid("bad".into())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            map_media_error(MediaError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_media_error(MediaError::Conflict("c".into())).status(),
            StatusCode::CONFLICT
        );
    }

    /// WH-06 (plan-20260912 / ADR-WH-05): the media finalize HTTP path keeps
    /// the original AccessTokenUser contract — anonymous and static-push-token
    /// requests are 401 with zero events; a valid DB access token reaches the
    /// real finalize and produces exactly one event.
    #[tokio::test]
    async fn storage_event_auth_reachability() {
        let transport = Arc::new(RecordingTransport::default());
        let h = harness_with_emitter(wh06_emitter_handle(
            transport.clone(),
            vec!["/acme/app.git".to_owned()],
        ))
        .await;
        let app = media_app(h.state.clone());
        let id = "a".repeat(64);

        // Anonymous: 401, no event.
        let anon = app
            .clone()
            .oneshot(with_repo(
                Request::post(format!("/libra/media/v1/manifests/{id}/finalize"))
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

        // A static-push-token-shaped bearer is NOT a DB access token: the
        // extractor must reject it like any unknown token.
        let static_token = app
            .clone()
            .oneshot(with_repo(
                Request::post(format!("/libra/media/v1/manifests/{id}/finalize"))
                    .header(AUTHORIZATION, "Bearer wh06-static-push-token")
                    .body(Body::empty())
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(static_token.status(), StatusCode::UNAUTHORIZED);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 0, "rejected requests deliver nothing");

        // Valid DB access token: the full prepare → chunks → finalize flow
        // over HTTP lands the finalize and delivers exactly one event whose
        // scope is the server-side canonical repository.
        let alice = token_for(&h.state, "alice").await;
        let manifest = sample_manifest(b"wh06-auth-reachability");
        let body = serde_json::to_vec(&manifest).unwrap();
        let prepared = app
            .clone()
            .oneshot(with_repo(
                Request::post("/libra/media/v1/manifests")
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, MEDIA_JSON)
                    .body(Body::from(body))
                    .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(prepared.status(), StatusCode::OK);
        let prepared: PrepareResponse = serde_json::from_slice(
            &axum::body::to_bytes(prepared.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        for chunk in &manifest.chunks {
            let data = b"wh06-auth-reachability";
            let start = chunk.offset as usize;
            let end = start + chunk.length as usize;
            let put = app
                .clone()
                .oneshot(with_repo(
                    Request::put(format!(
                        "/libra/media/v1/manifests/{}/chunks/{}",
                        prepared.manifest_id, chunk.chunk_hash
                    ))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(data[start..end].to_vec()))
                    .unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(put.status(), StatusCode::OK, "chunk upload");
        }
        // MF-03: pages + seal before async finalize.
        let pages = crate::ceres::lfs::media::protocol::split_pages(&manifest.chunks).unwrap();
        for (page_no, entries) in pages.into_iter().enumerate() {
            let page = ManifestPage {
                page_no: page_no as u32,
                entries,
            };
            let put_page = app
                .clone()
                .oneshot(with_repo(
                    Request::put(format!(
                        "/libra/media/v1/manifests/{}/pages/{}",
                        prepared.manifest_id, page_no
                    ))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, MEDIA_JSON)
                    .body(Body::from(serde_json::to_vec(&page).unwrap()))
                    .unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(put_page.status(), StatusCode::OK, "put page");
        }
        let sealed = app
            .clone()
            .oneshot(with_repo(
                Request::post(format!(
                    "/libra/media/v1/manifests/{}/seal",
                    prepared.manifest_id
                ))
                .header(AUTHORIZATION, format!("Bearer {alice}"))
                .body(Body::empty())
                .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(sealed.status(), StatusCode::OK, "seal");

        let accepted = app
            .clone()
            .oneshot(with_repo(
                Request::post(format!(
                    "/libra/media/v1/manifests/{}/finalize",
                    prepared.manifest_id
                ))
                .header(AUTHORIZATION, format!("Bearer {alice}"))
                .body(Body::empty())
                .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::ACCEPTED, "HTTP finalize 202");
        let accepted: FinalizeAcceptedResponse = serde_json::from_slice(
            &axum::body::to_bytes(accepted.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let resp = app
                    .clone()
                    .oneshot(with_repo(
                        Request::get(format!("/libra/media/v1/tasks/{}", accepted.task_id))
                            .header(AUTHORIZATION, format!("Bearer {alice}"))
                            .body(Body::empty())
                            .unwrap(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                let st: FinalizeTaskResponse = serde_json::from_slice(
                    &axum::body::to_bytes(resp.into_body(), usize::MAX)
                        .await
                        .unwrap(),
                )
                .unwrap();
                if st.state == "complete" || st.state == "failed" {
                    return st;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("task finish");
        assert_eq!(status.state, "complete", "async finalize completes");

        wh06_wait_calls(&transport, 1).await;
        let bodies = transport.bodies();
        assert_eq!(bodies.len(), 1, "exactly one finalized event");
        let envelope: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("envelope");
        assert_eq!(envelope["event_type"], "lfs.media.finalized");
        assert_eq!(envelope["scope"]["repo_path"], "/acme/app.git");
        assert_eq!(envelope["data"]["manifest_id"], prepared.manifest_id);
        assert_eq!(envelope["data"]["transfer"], "fastcdc");

        tokio::time::timeout(
            Duration::from_secs(3),
            h.state.storage.storage_event_emitter.shutdown(),
        )
        .await
        .expect("emitter shutdown within 3s");
        assert_eq!(transport.calls(), 1, "no late delivery after drain");
    }

    // --- WH-06 helpers ---

    /// Recording fake transport for the media tests (WH-09 seam).
    #[derive(Default)]
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<bytes::Bytes>>,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<bytes::Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: bytes::Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            Box::pin(async {
                Ok(
                    crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                        status: 200,
                    },
                )
            })
        }
    }

    fn wh06_emitter_handle(
        transport: Arc<RecordingTransport>,
        lfs_paths: Vec<String>,
    ) -> crate::jupiter::service::storage_event_emitter::StorageEventEmitter {
        let target_config = crate::config::StorageEventsTargetConfig {
            id: "ops-main".to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
                .to_owned(),
            events: vec!["lfs.media.finalized".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths,
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = crate::jupiter::service::storage_event_transport::EventTarget::compile(
            &target_config.id,
            &target_config.url,
            &secret,
        )
        .expect("compile target");
        let mut config =
            crate::config::testing::isolated_config(std::env::temp_dir().join("wh06-media-auth"));
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("it-wh06".to_owned());
        crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
            &config,
            transport,
            vec![(target_config, compiled)],
        )
    }

    /// harness() with the recording emitter installed as the owner.
    async fn harness_with_emitter(
        emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    ) -> Harness {
        let db_dir = tempfile::tempdir().unwrap();
        let mut storage = test_storage(db_dir.path()).await;
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        storage.lfs_service.obj_storage = build_object_storage(&cfg).await.unwrap();
        // test_storage mocks LfsService; the finalize fallback writes
        // lfs_objects, so rebind the real DB-backed storage too.
        storage.lfs_service.lfs_storage = storage.lfs_db_storage();
        storage.set_storage_event_emitter(emitter);
        let state = state_from(storage);
        Harness {
            _db_dir: db_dir,
            _obj_dir: obj_dir,
            state,
        }
    }

    async fn wh06_wait_calls(transport: &RecordingTransport, n: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }
}
