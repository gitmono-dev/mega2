use std::sync::LazyLock;

use axum::{
    Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use bytes::Bytes;
use regex::Regex;
use utoipa_axum::router::OpenApiRouter;

use crate::{
    api::MonoApiServiceState,
    ceres::oci::{
        auth::{authorize_repo_write, registry_ping_allowed},
        digest::{compute_digest, parse_digest},
        error::OciError,
        model::{
            DOCKER_MANIFEST_LIST_MEDIA_TYPE, DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE, Manifest,
            ManifestIndex, OCI_INDEX_MEDIA_TYPE, OCI_MANIFEST_MEDIA_TYPE,
        },
    },
    jupiter::utils::into_obj_stream::IntoObjectStream,
};

const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const REGISTRY_API_VERSION: &str = "registry/2.0";

/// OCI repository path (`remoteName`): one or more `/`-separated path components.
/// Each component is `alphanumeric(?:(?:[._]|__|[-]+)alphanumeric)*`, matching
/// distribution `reference.pathComponent` / `remoteName` (without optional domain).
static REPOSITORY_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*)*$",
    )
    .expect("repository name regex")
});

static TAG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w][\w.-]{0,127}$").expect("tag name regex"));

/// Runtime routes for the OCI Distribution API, mounted at `/v2` by DR-11.
pub fn oci_routes() -> Router<MonoApiServiceState> {
    Router::new()
        .route("/", get(ping))
        .route("/{*tail}", any(dispatch))
}

/// OCI's OpenAPI contribution. Fine-grained operations are documented when
/// the full route family is mounted in DR-11.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
}

async fn ping(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Response, OciError> {
    registry_ping_allowed(&state.storage.config().git, request.headers())?;
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        "Docker-Distribution-API-Version",
        HeaderValue::from_static(REGISTRY_API_VERSION),
    );
    Ok(response)
}

async fn dispatch(
    State(state): State<MonoApiServiceState>,
    Path(tail): Path<String>,
    request: Request,
) -> Result<Response, OciError> {
    let segments: Vec<&str> = tail.split('/').collect();
    match segments.as_slice() {
        [name @ .., "manifests", reference]
            if !name.is_empty() && request.method() == Method::PUT =>
        {
            put_manifest(state, name.join("/"), reference, request).await
        }
        _ => Err(OciError::NameInvalid),
    }
}

async fn put_manifest(
    state: MonoApiServiceState,
    repo: String,
    reference: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let body = to_bytes(request.into_body(), MAX_MANIFEST_BYTES + 1)
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(OciError::ManifestInvalid);
    }

    let media_type = manifest_media_type(&body)?;
    let digest = compute_digest(&body);
    if reference.starts_with("sha256:") {
        let expected = parse_digest(reference)?;
        if expected != digest {
            return Err(OciError::DigestInvalid);
        }
    } else if !valid_tag(reference) {
        return Err(OciError::TagInvalid);
    }
    validate_references(&state, &repo, &media_type, &body).await?;

    state
        .storage
        .oci_service
        .put_manifest(digest.hex(), Bytes::from(body.to_vec()).into_stream())
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    state
        .storage
        .oci_service
        .oci_storage
        .put_manifest(&repo, digest.as_str(), &media_type, body.len() as i64)
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    if !reference.starts_with("sha256:") {
        state
            .storage
            .oci_service
            .oci_storage
            .upsert_tag(&repo, reference, digest.as_str())
            .await
            .map_err(|_| OciError::ManifestInvalid)?;
    }

    let location = format!("/v2/{repo}/manifests/{digest}");
    let mut response = StatusCode::CREATED.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "Location",
        HeaderValue::from_str(&location).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest.as_str()).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&media_type).map_err(|_| OciError::ManifestInvalid)?,
    );
    Ok(response)
}

fn manifest_media_type(body: &[u8]) -> Result<String, OciError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
    let media_type = value
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .ok_or(OciError::ManifestInvalid)?;
    if supported_media_type(media_type) {
        Ok(media_type.to_owned())
    } else {
        Err(OciError::ManifestInvalid)
    }
}

async fn validate_references(
    state: &MonoApiServiceState,
    repo: &str,
    media_type: &str,
    body: &[u8],
) -> Result<(), OciError> {
    if matches!(
        media_type,
        DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE | OCI_MANIFEST_MEDIA_TYPE
    ) {
        let manifest: Manifest =
            serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
        for descriptor in std::iter::once(&manifest.config).chain(&manifest.layers) {
            parse_digest(&descriptor.digest).map_err(|_| OciError::ManifestInvalid)?;
            if !state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists(repo, &descriptor.digest)
                .await
                .map_err(|_| OciError::ManifestInvalid)?
            {
                return Err(OciError::ManifestBlobUnknown);
            }
        }
    } else {
        let index: ManifestIndex =
            serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
        for descriptor in &index.manifests {
            parse_digest(&descriptor.digest).map_err(|_| OciError::ManifestInvalid)?;
            if state
                .storage
                .oci_service
                .oci_storage
                .get_manifest(repo, &descriptor.digest)
                .await
                .map_err(|_| OciError::ManifestInvalid)?
                .is_none()
            {
                return Err(OciError::ManifestBlobUnknown);
            }
        }
    }
    Ok(())
}

fn supported_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE
            | DOCKER_MANIFEST_LIST_MEDIA_TYPE
            | OCI_MANIFEST_MEDIA_TYPE
            | OCI_INDEX_MEDIA_TYPE
    )
}

fn valid_repository_name(name: &str) -> bool {
    if name.is_empty() || name.split('/').any(|segment| segment == "..") {
        return false;
    }
    REPOSITORY_NAME.is_match(name)
}

fn valid_tag(tag: &str) -> bool {
    TAG_NAME.is_match(tag)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE},
        },
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tower::ServiceExt;

    use super::oci_routes;
    use crate::{
        api::{
            MonoApiServiceState,
            oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        },
        bellatrix::Bellatrix,
        ceres::api_service::cache::GitObjectCache,
        config::{GitConfig, PushAuth, PushTokenConfig, testing::isolated_config},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            service::oci_service::OciService,
            storage::{Storage, object_storage::mock_object_storage},
            tests::{test_db_config, test_storage_with_config},
        },
    };

    async fn state(anonymous_access: bool) -> MonoApiServiceState {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("config"));
        config.database = test_db_config(temp_dir.path()).await;
        config.git = GitConfig {
            anonymous_access,
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![PushTokenConfig {
                name: "test".to_owned(),
                token: "secret".to_owned(),
                paths: Some(vec!["/team".to_owned()]),
            }],
            ssh_receive_pack: Some(false),
        };
        let mut storage = test_storage_with_config(temp_dir.path(), config).await;
        storage.oci_service = OciService {
            oci_storage: storage.oci_db_storage(),
            obj_storage: mock_object_storage(),
        };
        storage
            .oci_service
            .oci_storage
            .put_blob_ref(
                "team/image",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                0,
            )
            .await
            .expect("seed manifest config blob");
        state_with_storage(storage)
    }

    fn state_with_storage(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "oci-router-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            storage,
        }
    }

    fn manifest() -> String {
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":0},"layers":[]}"#.to_owned()
    }

    fn authenticated(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(
            AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("any:secret")),
        )
    }

    #[tokio::test]
    async fn ping_anonymous_open_ok() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Docker-Distribution-API-Version")
                .expect("version"),
            "registry/2.0"
        );
    }

    #[tokio::test]
    async fn ping_anonymous_closed_401() {
        let response = oci_routes()
            .with_state(state(false).await)
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(WWW_AUTHENTICATE));
    }

    #[tokio::test]
    async fn ping_invalid_credentials_401() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                Request::get("/")
                    .header(AUTHORIZATION, "Bearer invalid")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_manifest_by_tag_201() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .header(CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.headers().contains_key("Location"));
        assert!(response.headers().contains_key("Docker-Content-Digest"));
    }

    #[tokio::test]
    async fn put_manifest_sets_tag_row() {
        let state = state(true).await;
        let storage = state.storage.clone();
        let response = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let digest = response
            .headers()
            .get("Docker-Content-Digest")
            .expect("digest")
            .to_str()
            .expect("valid digest");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            storage
                .oci_service
                .oci_storage
                .get_tag("team/image", "v1")
                .await
                .expect("tag")
                .expect("tag exists")
                .digest,
            digest
        );
    }

    #[tokio::test]
    async fn put_manifest_by_digest_mismatch() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put(format!(
                    "/team/image/manifests/sha256:{}",
                    "0".repeat(64)
                )))
                .body(Body::from(manifest()))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("DIGEST_INVALID")
        );
    }

    #[tokio::test]
    async fn put_manifest_invalid_name() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put("/team/../image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("NAME_INVALID")
        );
    }

    #[test]
    fn repository_name_matches_distribution_remote_name() {
        assert!(super::valid_repository_name("team/image"));
        assert!(super::valid_repository_name("team/foo.bar"));
        assert!(super::valid_repository_name("team/foo_bar"));
        assert!(super::valid_repository_name("team/foo__bar"));
        assert!(super::valid_repository_name("team/foo--bar"));
        assert!(!super::valid_repository_name("team/image."));
        assert!(!super::valid_repository_name("team/image__"));
        assert!(!super::valid_repository_name("team/../image"));
        assert!(!super::valid_repository_name(""));
    }
}
