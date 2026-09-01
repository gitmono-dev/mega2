//! HTTP adapter for the opt-in FastCDC Media protocol.

use std::collections::BTreeMap;

use axum::{
    Extension, Json,
    body::to_bytes,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa::openapi::{
    OpenApi,
    server::{Server, ServerVariableBuilder},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::lfs_router::{LFS_CONTENT_TYPE, LFS_STREAM_CONTENT_TYPE, LfsRepoContext};
use crate::{
    api::{MonoApiServiceState, api_doc::LFS_TAG, oauth::AccessTokenUser},
    ceres::lfs::media::{
        chunker,
        protocol::{self, ManifestResponse, MediaManifest, PrepareResponse},
        scope::MediaScope,
        service::MediaServiceError,
    },
};

const MEDIA_OPENAPI_PREFIX: &str = "/info/lfs/libra/media/v1";

fn media_openapi_server() -> Server {
    let mut server = Server::new("/{repository}");
    server.description =
        Some("Canonical repository prefix used to derive the server-side Media scope.".to_owned());
    server.variables = Some(BTreeMap::from([(
        "repository".to_owned(),
        ServerVariableBuilder::new()
            .default_value("project/demo.git")
            .description(Some("Canonical repository path without the leading slash."))
            .build(),
    )]));
    server
}

/// Builds the Media suffix routes. [`super::lfs_router::lfs_routes`] owns the
/// `/libra/media/v1` prefix so the same handlers work at both LFS mounts.
pub(crate) fn routes() -> OpenApiRouter<MonoApiServiceState> {
    let prepare_routes = OpenApiRouter::new()
        .routes(routes!(prepare))
        .route_layer(RequestBodyLimitLayer::new(protocol::MAX_MANIFEST_SIZE));
    let chunk_routes = OpenApiRouter::new()
        .routes(routes!(upload_chunk))
        .route_layer(RequestBodyLimitLayer::new(chunker::MAX_SIZE));

    OpenApiRouter::new()
        .routes(routes!(capabilities))
        .merge(prepare_routes)
        .merge(chunk_routes)
        .routes(routes!(finalize))
        .routes(routes!(manifest))
        .routes(routes!(download_chunk))
}

/// Returns the Media OpenAPI paths with the same repository-prefixed URL that
/// the HTTP rewrite middleware serves at runtime. The static `/api/v1/lfs`
/// mount deliberately excludes Media because it cannot provide this context.
pub(crate) fn openapi() -> OpenApi {
    let mut api = routes().into_openapi();
    let paths = std::mem::take(&mut api.paths.paths);
    for (path, mut item) in paths {
        item.servers = Some(vec![media_openapi_server()]);
        api.paths
            .paths
            .insert(format!("{MEDIA_OPENAPI_PREFIX}{path}"), item);
    }
    api
}

fn media_scope(
    user: &AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
) -> Result<MediaScope, MediaHttpError> {
    let Some(Extension(repository)) = repo else {
        return Err(MediaServiceError::NotFound.into());
    };

    MediaScope::from_access_token_username(&user.0.username, &repository.0)
        .map_err(|_| MediaServiceError::NotFound.into())
}

#[derive(Debug)]
enum MediaHttpError {
    Media(MediaServiceError),
    Body,
}

impl From<MediaServiceError> for MediaHttpError {
    fn from(error: MediaServiceError) -> Self {
        Self::Media(error)
    }
}

impl IntoResponse for MediaHttpError {
    fn into_response(self) -> Response {
        match self {
            Self::Media(error) => media_error_response(error),
            Self::Body => media_json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "media body exceeds its size limit",
            ),
        }
    }
}

fn media_error_response(error: MediaServiceError) -> Response {
    let (status, message) = match &error {
        MediaServiceError::Invalid => (StatusCode::BAD_REQUEST, "invalid media request"),
        MediaServiceError::NotFound => (StatusCode::NOT_FOUND, "media object not found"),
        MediaServiceError::Conflict => (
            StatusCode::CONFLICT,
            "media manifest conflicts with stored state",
        ),
        MediaServiceError::Storage | MediaServiceError::Io | MediaServiceError::Json(_) => {
            tracing::error!(error = ?error, "media storage operation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "media storage operation failed",
            )
        }
    };
    media_json_response(status, message)
}

fn media_json_response(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [("content-type", LFS_CONTENT_TYPE)],
        Json(serde_json::json!({"message": message})),
    )
        .into_response()
}

async fn read_body(request: Request, limit: usize) -> Result<bytes::Bytes, MediaHttpError> {
    to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| MediaHttpError::Body)
}

#[utoipa::path(
    get,
    path = "/capabilities",
    responses(
        (status = 200, description = "FastCDC Media capabilities", content_type = "application/json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Repository context unavailable", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn capabilities(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
) -> Result<Json<serde_json::Value>, MediaHttpError> {
    media_scope(&user, repo)?;
    Ok(Json(protocol::capabilities()))
}

#[utoipa::path(
    post,
    path = "/manifests",
    request_body = MediaManifest,
    responses(
        (status = 200, description = "Prepared manifest and ordered missing chunk hashes", body = PrepareResponse, content_type = "application/json"),
        (status = 400, description = "Malformed or invalid Media manifest", content_type = "application/vnd.git-lfs+json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Repository context unavailable", content_type = "application/vnd.git-lfs+json"),
        (status = 413, description = "Manifest exceeds 10 MiB"),
        (status = 500, description = "Storage operation failed", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn prepare(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<PrepareResponse>, MediaHttpError> {
    let scope = media_scope(&user, repo)?;
    let data = read_body(request, protocol::MAX_MANIFEST_SIZE).await?;
    let manifest = protocol::parse_manifest(&data).map_err(|_| MediaServiceError::Invalid)?;
    let response = state
        .storage
        .lfs_service
        .prepare_media(&scope, manifest)
        .await?;
    Ok(Json(response))
}

#[utoipa::path(
    put,
    path = "/manifests/{manifest_id}/chunks/{hash}",
    params(
        ("manifest_id" = String, Path, description = "Canonical Media manifest ID"),
        ("hash" = String, Path, description = "Declared SHA-256 chunk hash")
    ),
    responses(
        (status = 204, description = "Chunk verified and stored"),
        (status = 400, description = "Chunk content or request is invalid", content_type = "application/vnd.git-lfs+json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Manifest, chunk declaration, or repository was not found", content_type = "application/vnd.git-lfs+json"),
        (status = 409, description = "Stored pending manifest conflicts", content_type = "application/vnd.git-lfs+json"),
        (status = 413, description = "Chunk exceeds 8 MiB"),
        (status = 500, description = "Storage operation failed", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn upload_chunk(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    State(state): State<MonoApiServiceState>,
    Path((manifest_id, hash)): Path<(String, String)>,
    request: Request,
) -> Result<StatusCode, MediaHttpError> {
    let scope = media_scope(&user, repo)?;
    let data = read_body(request, chunker::MAX_SIZE).await?;
    state
        .storage
        .lfs_service
        .upload_media_chunk(&scope, &manifest_id, &hash, data)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/manifests/{manifest_id}/finalize",
    params(("manifest_id" = String, Path, description = "Canonical Media manifest ID")),
    responses(
        (status = 204, description = "Verified Media object published to standard LFS fallback"),
        (status = 400, description = "Uploaded Media object is invalid", content_type = "application/vnd.git-lfs+json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Manifest or repository was not found", content_type = "application/vnd.git-lfs+json"),
        (status = 409, description = "Manifest or fallback state conflicts", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Storage operation failed", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn finalize(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    State(state): State<MonoApiServiceState>,
    Path(manifest_id): Path<String>,
) -> Result<StatusCode, MediaHttpError> {
    let scope = media_scope(&user, repo)?;
    state
        .storage
        .lfs_service
        .finalize_media(&scope, &manifest_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/manifests/by-media/{media_oid}",
    params(("media_oid" = String, Path, description = "Finalized Media SHA-256 object ID")),
    responses(
        (status = 200, description = "Finalized Media manifest", body = ManifestResponse, content_type = "application/json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Manifest or repository was not found", content_type = "application/vnd.git-lfs+json"),
        (status = 409, description = "Stored manifest conflicts with its Media object", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Storage operation failed", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn manifest(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    State(state): State<MonoApiServiceState>,
    Path(media_oid): Path<String>,
) -> Result<Json<ManifestResponse>, MediaHttpError> {
    let scope = media_scope(&user, repo)?;
    let response = state
        .storage
        .lfs_service
        .finalized_media_manifest(&scope, &media_oid)
        .await?;
    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/manifests/by-media/{media_oid}/chunks/{hash}",
    params(
        ("media_oid" = String, Path, description = "Finalized Media SHA-256 object ID"),
        ("hash" = String, Path, description = "Declared SHA-256 chunk hash")
    ),
    responses(
        (status = 200, description = "Verified Media chunk bytes", content_type = "application/octet-stream"),
        (status = 400, description = "Stored chunk content is invalid", content_type = "application/vnd.git-lfs+json"),
        (status = 401, description = "Mono access token required"),
        (status = 404, description = "Manifest, chunk declaration, or repository was not found", content_type = "application/vnd.git-lfs+json"),
        (status = 409, description = "Stored manifest conflicts with its Media object", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Storage operation failed", content_type = "application/vnd.git-lfs+json")
    ),
    tag = LFS_TAG
)]
async fn download_chunk(
    user: AccessTokenUser,
    repo: Option<Extension<LfsRepoContext>>,
    State(state): State<MonoApiServiceState>,
    Path((media_oid, hash)): Path<(String, String)>,
) -> Result<Response, MediaHttpError> {
    let scope = media_scope(&user, repo)?;
    let bytes = state
        .storage
        .lfs_service
        .read_finalized_media_chunk(&scope, &media_oid, &hash)
        .await?;
    Ok(([("content-type", LFS_STREAM_CONTENT_TYPE)], bytes).into_response())
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc};

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE},
        },
    };
    use tower::{Service, ServiceBuilder, ServiceExt};

    use super::*;
    use crate::{
        api::{
            MonoApiServiceState,
            oauth::{
                api_store::{BrowserSessionStore, FixedUserSessionStore},
                model::LoginUser,
            },
        },
        bellatrix::Bellatrix,
        ceres::{
            api_service::cache::GitObjectCache,
            lfs::media::protocol::{ChunkEntry, CreatedBy},
        },
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{service::lfs_service::LfsService, storage::Storage, tests::test_storage},
        orbit::factory::{
            LocalConfig, ObjectStorageBackend, ObjectStorageConfig, ObjectStorageFactory,
        },
    };

    fn api_state(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Fixed(FixedUserSessionStore {
                user: LoginUser::default(),
            }),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("create lazy redis connection"),
                prefix: "lfs-media-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            storage,
        }
    }

    async fn fixture() -> (tempfile::TempDir, MonoApiServiceState, String, String) {
        let temp_dir = tempfile::tempdir().expect("create temp directory");
        let mut storage = test_storage(temp_dir.path()).await;
        let object_storage = ObjectStorageFactory::build(&ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: temp_dir
                    .path()
                    .join("objects")
                    .to_string_lossy()
                    .into_owned(),
            },
            ..Default::default()
        })
        .await
        .expect("build local object storage");
        storage.lfs_service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: object_storage,
        };

        let alice = storage
            .user_storage()
            .generate_token("alice".to_owned())
            .await
            .expect("create alice token");
        let bob = storage
            .user_storage()
            .generate_token("bob".to_owned())
            .await
            .expect("create bob token");
        (temp_dir, api_state(storage), alice, bob)
    }

    const REPOSITORY: &str = "/project/demo.git";

    fn app(
        state: MonoApiServiceState,
    ) -> impl Service<Request<Body>, Response = Response, Error = Infallible> + Clone {
        let info_lfs_router: Router = super::super::lfs_router::lfs_routes()
            .with_state(state)
            .into();
        ServiceBuilder::new()
            .layer(tower::util::MapRequestLayer::new(
                crate::server::http_server::rewrite_lfs_request_uri::<Body>,
            ))
            .service(Router::new().nest("/info/lfs", info_lfs_router))
    }

    fn media_uri_for(repository: &str, suffix: &str) -> String {
        format!("{repository}/info/lfs/libra/media/v1{suffix}")
    }

    fn media_uri(suffix: &str) -> String {
        media_uri_for(REPOSITORY, suffix)
    }

    fn manifest_for(media: &[u8]) -> MediaManifest {
        let chunks = chunker::chunk_bytes(media)
            .into_iter()
            .map(|chunk| ChunkEntry {
                offset: chunk.offset,
                length: chunk.length,
                chunk_hash: chunk.chunk_hash,
                encoded_length: chunk.length,
                compression: "none".to_owned(),
                checksum: None,
            })
            .collect();
        MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_owned(),
            hash_algorithm: "sha256".to_owned(),
            media_oid: protocol::sha256_hex(media),
            media_size: media.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "monoengine-test".to_owned(),
                version: "1".to_owned(),
                capabilities: vec![chunker::ALGORITHM.to_owned()],
            },
            fallback_oid: None,
        }
    }

    fn bearer_request(method: &str, uri: String, token: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(body)
            .expect("build request")
    }

    #[tokio::test]
    async fn media_router_requires_token_and_preserves_actor_and_repository_scope() {
        let (_temp_dir, state, alice, bob) = fixture().await;
        let media_app = app(state);
        let capability_path = media_uri("/capabilities");

        for token in [None, Some("unknown-token")] {
            let mut request = Request::builder().uri(&capability_path);
            if let Some(token) = token {
                request = request.header(AUTHORIZATION, format!("Bearer {token}"));
            }
            let response = media_app
                .clone()
                .oneshot(request.body(Body::empty()).expect("build request"))
                .await
                .expect("router responds");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let response = media_app
            .clone()
            .oneshot(bearer_request(
                "GET",
                capability_path.clone(),
                &alice,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let capabilities: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("read capabilities"),
        )
        .expect("decode capabilities");
        assert_eq!(
            capabilities["chunk_algorithms"],
            serde_json::json!(["fastcdc-v1"])
        );
        assert_eq!(
            capabilities["hash_algorithms"],
            serde_json::json!(["sha256"])
        );
        assert_eq!(capabilities["max_chunk_size"], chunker::MAX_SIZE);
        assert_eq!(
            capabilities["max_manifest_size"],
            protocol::MAX_MANIFEST_SIZE
        );
        assert_eq!(capabilities["supports_batch_exists"], true);
        assert_eq!(capabilities["supports_standard_lfs_fallback"], true);

        let media = (0..(256 * 1024))
            .map(|index| ((index * 31 + 17) % 251) as u8)
            .collect::<Vec<_>>();
        let manifest = manifest_for(&media);
        let response = media_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(media_uri("/manifests"))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&manifest).expect("encode manifest"),
                    ))
                    .expect("build prepare request"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let prepared: PrepareResponse = serde_json::from_slice(
            &to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("read prepare response"),
        )
        .expect("decode prepare response");

        for hash in &prepared.missing_chunks {
            let chunk = manifest
                .chunks
                .iter()
                .find(|chunk| &chunk.chunk_hash == hash)
                .expect("prepared hash is declared");
            let start = chunk.offset as usize;
            let end = (chunk.offset + chunk.length) as usize;
            let response = media_app
                .clone()
                .oneshot(bearer_request(
                    "PUT",
                    media_uri(&format!(
                        "/manifests/{}/chunks/{hash}",
                        prepared.manifest_id
                    )),
                    &alice,
                    Body::from(media[start..end].to_vec()),
                ))
                .await
                .expect("router responds");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        let response = media_app
            .clone()
            .oneshot(bearer_request(
                "POST",
                media_uri(&format!("/manifests/{}/finalize", prepared.manifest_id)),
                &alice,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let manifest_path = media_uri(&format!("/manifests/by-media/{}", manifest.media_oid));
        let response = media_app
            .clone()
            .oneshot(bearer_request(
                "GET",
                manifest_path.clone(),
                &alice,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let finalized: ManifestResponse = serde_json::from_slice(
            &to_bytes(response.into_body(), protocol::MAX_MANIFEST_SIZE)
                .await
                .expect("read finalized manifest"),
        )
        .expect("decode finalized manifest");
        let mut expected_manifest = manifest.clone();
        expected_manifest.fallback_oid = Some(expected_manifest.media_oid.clone());
        assert_eq!(finalized.manifest, expected_manifest);
        assert_eq!(finalized.manifest_id, prepared.manifest_id);

        let first_chunk = manifest.chunks.first().expect("media has a chunk");
        let response = media_app
            .clone()
            .oneshot(bearer_request(
                "GET",
                format!("{manifest_path}/chunks/{}", first_chunk.chunk_hash),
                &alice,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(LFS_STREAM_CONTENT_TYPE)
        );
        let start = first_chunk.offset as usize;
        let end = (first_chunk.offset + first_chunk.length) as usize;
        assert_eq!(
            to_bytes(response.into_body(), chunker::MAX_SIZE)
                .await
                .expect("read chunk"),
            media[start..end]
        );

        let response = media_app
            .clone()
            .oneshot(bearer_request(
                "GET",
                manifest_path.clone(),
                &bob,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = media_app
            .oneshot(bearer_request(
                "GET",
                media_uri_for(
                    "/project/other.git",
                    &format!("/manifests/by-media/{}", manifest.media_oid),
                ),
                &alice,
                Body::empty(),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn media_router_rejects_malformed_and_oversized_bodies() {
        let (_temp_dir, state, alice, _) = fixture().await;
        let media_app = app(state);

        let response = media_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(media_uri("/manifests"))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .expect("build malformed request"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(LFS_CONTENT_TYPE)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &to_bytes(response.into_body(), 1024)
                    .await
                    .expect("read error response"),
            )
            .expect("decode error response"),
            serde_json::json!({"message": "invalid media request"})
        );

        let response = media_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(media_uri("/manifests"))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(
                        CONTENT_LENGTH,
                        (protocol::MAX_MANIFEST_SIZE + 1).to_string(),
                    )
                    .body(Body::empty())
                    .expect("build oversized manifest request"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let response = media_app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(media_uri(&format!(
                        "/manifests/{}/chunks/{}",
                        "a".repeat(64),
                        "b".repeat(64)
                    )))
                    .header(AUTHORIZATION, format!("Bearer {alice}"))
                    .header(CONTENT_LENGTH, (chunker::MAX_SIZE + 1).to_string())
                    .body(Body::empty())
                    .expect("build oversized chunk request"),
            )
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn media_http_errors_map_statuses_and_redact_server_failures() {
        for (error, status, message) in [
            (
                MediaServiceError::Invalid,
                StatusCode::BAD_REQUEST,
                "invalid media request",
            ),
            (
                MediaServiceError::NotFound,
                StatusCode::NOT_FOUND,
                "media object not found",
            ),
            (
                MediaServiceError::Conflict,
                StatusCode::CONFLICT,
                "media manifest conflicts with stored state",
            ),
            (
                MediaServiceError::Storage,
                StatusCode::INTERNAL_SERVER_ERROR,
                "media storage operation failed",
            ),
        ] {
            let response = MediaHttpError::from(error).into_response();
            assert_eq!(response.status(), status);
            assert_eq!(
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some(LFS_CONTENT_TYPE)
            );
            let body = to_bytes(response.into_body(), 1024)
                .await
                .expect("read error response");
            let value: serde_json::Value =
                serde_json::from_slice(&body).expect("decode error response");
            assert_eq!(value, serde_json::json!({"message": message}));
            assert!(!String::from_utf8_lossy(&body).contains("media-v1/"));
        }
    }

    #[test]
    fn media_openapi_describes_the_callable_repository_scoped_routes() {
        let api = openapi();
        for path in [
            "/info/lfs/libra/media/v1/capabilities",
            "/info/lfs/libra/media/v1/manifests",
            "/info/lfs/libra/media/v1/manifests/{manifest_id}/chunks/{hash}",
            "/info/lfs/libra/media/v1/manifests/{manifest_id}/finalize",
            "/info/lfs/libra/media/v1/manifests/by-media/{media_oid}",
            "/info/lfs/libra/media/v1/manifests/by-media/{media_oid}/chunks/{hash}",
        ] {
            let item = api
                .paths
                .paths
                .get(path)
                .unwrap_or_else(|| panic!("missing OpenAPI path {path}"));
            let server = item
                .servers
                .as_ref()
                .and_then(|servers| servers.first())
                .expect("Media path declares its repository server");
            assert_eq!(server.url, "/{repository}");
            assert_eq!(
                server
                    .variables
                    .as_ref()
                    .and_then(|variables| variables.get("repository"))
                    .map(|variable| variable.default_value.as_str()),
                Some("project/demo.git")
            );
        }
        assert!(
            !api.paths
                .paths
                .contains_key("/api/v1/lfs/libra/media/v1/capabilities"),
            "a repository-free Media alias must not be documented"
        );
    }

    #[test]
    fn media_openapi_documents_every_handler_error_status() {
        let document = serde_json::to_value(openapi()).expect("serialize OpenAPI document");
        for (path, method, statuses) in [
            (
                "/info/lfs/libra/media/v1/manifests",
                "post",
                &["400", "401", "404", "413", "500"][..],
            ),
            (
                "/info/lfs/libra/media/v1/manifests/{manifest_id}/chunks/{hash}",
                "put",
                &["400", "401", "404", "409", "413", "500"][..],
            ),
            (
                "/info/lfs/libra/media/v1/manifests/by-media/{media_oid}/chunks/{hash}",
                "get",
                &["400", "401", "404", "409", "500"][..],
            ),
        ] {
            let responses = &document["paths"][path][method]["responses"];
            for status in statuses {
                assert!(
                    responses.get(*status).is_some(),
                    "{method} {path} is missing documented HTTP {status}"
                );
            }
        }
    }
}
