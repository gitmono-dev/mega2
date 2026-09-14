use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
};
use percent_encoding::percent_decode_str;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::AGENT_CAPTURE_TAG},
    api_model::agent_capture::{
        AgentCaptureHttpError, DiscoveryResponse, ErrorEnvelope, SessionKind, SessionPutRequest,
        bearer_token, decode_repo_path_segment, fingerprint_session_put_body,
    },
    ceres::agent_capture::auth::{lookup_ingest_token, token_covers_capture_repo},
    common::errors::MegaError,
    config::AgentCaptureIngestTokenConfig,
    jupiter::storage::agent_capture_storage::SessionNaturalKey,
};

const SESSION_PUT_OPERATION: &str = "session.put";
const SESSION_PUT_MAX_BYTES: usize = 64 * 1024;

pub const PUBLIC_ENDPOINTS: &[(&str, &str)] = &[
    ("GET", "/api/v1/agent-capture/discovery"),
    (
        "PUT",
        "/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}",
    ),
    (
        "POST",
        "/api/v1/agent-capture/sessions/{capture_id}/blobs/staging",
    ),
    (
        "POST",
        "/api/v1/agent-capture/sessions/{capture_id}/blobs/{lease_id}/finalize",
    ),
    (
        "POST",
        "/api/v1/agent-capture/sessions/{capture_id}/events:batch",
    ),
    (
        "POST",
        "/api/v1/agent-capture/sessions/{capture_id}/file-ops:batch",
    ),
    (
        "POST",
        "/api/v1/agent-capture/sessions/{capture_id}/checkpoints",
    ),
    ("GET", "/api/v1/agent-capture/repos/{repo}/sessions"),
    ("GET", "/api/v1/agent-capture/sessions/{capture_id}"),
    (
        "GET",
        "/api/v1/agent-capture/sessions/{capture_id}/checkpoints",
    ),
    (
        "GET",
        "/api/v1/agent-capture/sessions/{capture_id}/transcript",
    ),
    (
        "GET",
        "/api/v1/agent-capture/sessions/{capture_id}/file-ops",
    ),
];

/// Internal paths start at `/agent-capture`. The outer nest is `/api/v1`.
///
/// `#[utoipa::path(path = ...)]` values are suffixes relative to that nest.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest("/agent-capture", capture_routes())
}

fn capture_routes() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(discovery))
        .routes(routes!(put_session))
        .routes(routes!(staging_blob))
        .routes(routes!(finalize_blob))
        .routes(routes!(events_batch))
        .routes(routes!(file_ops_batch))
        .routes(routes!(post_checkpoint))
        .routes(routes!(list_sessions))
        .routes(routes!(get_session))
        .routes(routes!(list_checkpoints))
        .routes(routes!(get_transcript))
        .routes(routes!(list_file_ops))
}

fn fixture() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

fn require_ingest_token(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
) -> Result<AgentCaptureIngestTokenConfig, AgentCaptureHttpError> {
    let presented = bearer_token(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )?;
    lookup_ingest_token(
        &state.storage.config().agent_capture.ingest_tokens,
        presented,
    )
    .cloned()
    .ok_or_else(AgentCaptureHttpError::unauthorized)
}

fn receipt_error(err: MegaError) -> AgentCaptureHttpError {
    if err.to_string().contains("fingerprint conflict") {
        AgentCaptureHttpError::conflict()
    } else {
        AgentCaptureHttpError::bad_request(err.to_string())
    }
}

fn session_put_path(path: &str) -> Result<(String, String), AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "repos") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    let repo = segments.get(idx + 1).copied().filter(|s| !s.is_empty());
    let sessions = segments.get(idx + 2).copied();
    let client = segments.get(idx + 3).copied().filter(|s| !s.is_empty());
    let extra = segments.get(idx + 4);
    match (repo, sessions, client, extra) {
        (Some(repo), Some("sessions"), Some(client), None) => {
            Ok((repo.to_owned(), client.to_owned()))
        }
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn decode_client_session_id(segment: &str) -> Result<String, AgentCaptureHttpError> {
    percent_decode_str(segment)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid client_session_id encoding"))
}

#[derive(Serialize, ToSchema)]
struct SessionPutResponse {
    capture_id: i64,
}

#[utoipa::path(
    get,
    path = "/discovery",
    responses(
        (status = 200, description = "Capture protocol discovery", body = DiscoveryResponse),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn discovery(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
) -> Result<Json<DiscoveryResponse>, AgentCaptureHttpError> {
    require_ingest_token(&state, &headers)?;
    Ok(Json(DiscoveryResponse { raw_accepted: true }))
}

#[utoipa::path(
    put,
    path = "/repos/{repo}/sessions/{client_session_id}",
    params(
        ("repo" = String, Path, description = "Single percent-encoded repo path segment"),
        ("client_session_id" = String, Path, description = "Client session id")
    ),
    request_body = SessionPutRequest,
    responses(
        (status = 200, description = "Upserted session", body = SessionPutResponse),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Token does not cover repo", body = ErrorEnvelope),
        (status = 409, description = "Immutable fingerprint conflict", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn put_session(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<SessionPutResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let (repo_segment, client_session_segment) = session_put_path(parts.uri.path())?;
    let repo_id = decode_repo_path_segment(&repo_segment)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if !token_covers_capture_repo(&token, &repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let client_session_id = decode_client_session_id(&client_session_segment)?;
    let bytes = axum::body::to_bytes(body, SESSION_PUT_MAX_BYTES)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("session put body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    fingerprint_session_put_body(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let put_request: SessionPutRequest = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let capture = state.storage.config().agent_capture.clone();
    let session_kind = match put_request.session_kind {
        SessionKind::ExternalCapture => "external_capture",
        SessionKind::InternalCode => "internal_code",
    };
    let key = SessionNaturalKey {
        deployment_id: capture.deployment_id,
        tenant_id: capture.tenant_id,
        repo_id,
        producer_id: token.name,
        session_kind: session_kind.to_owned(),
        client_session_id: client_session_id.clone(),
    };
    let fingerprint_body = serde_json::json!({
        "deployment_id": key.deployment_id,
        "tenant_id": key.tenant_id,
        "repo_id": key.repo_id,
        "producer_id": key.producer_id,
        "session_kind": key.session_kind,
        "client_session_id": key.client_session_id,
        "started_at": put_request.started_at,
        "ended_at": put_request.ended_at,
    });
    let capture_id = state
        .storage
        .agent_capture_service
        .storage
        .upsert_session(key)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    state
        .storage
        .agent_capture_service
        .storage
        .upsert_ingest_receipt(
            capture_id,
            SESSION_PUT_OPERATION,
            &client_session_id,
            &fingerprint_body,
            Some(serde_json::json!({ "capture_id": capture_id })),
        )
        .await
        .map_err(receipt_error)?;
    Ok(Json(SessionPutResponse { capture_id }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/blobs/staging",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-08")),
    tag = AGENT_CAPTURE_TAG
)]
async fn staging_blob() -> StatusCode {
    fixture()
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/blobs/{lease_id}/finalize",
    params(
        ("capture_id" = i64, Path, description = "Capture id"),
        ("lease_id" = String, Path, description = "Staging lease id")
    ),
    responses((status = 501, description = "Fixture; handler lands in AC-08")),
    tag = AGENT_CAPTURE_TAG
)]
async fn finalize_blob() -> StatusCode {
    fixture()
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/events:batch",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-09")),
    tag = AGENT_CAPTURE_TAG
)]
async fn events_batch() -> StatusCode {
    fixture()
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/file-ops:batch",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-10")),
    tag = AGENT_CAPTURE_TAG
)]
async fn file_ops_batch() -> StatusCode {
    fixture()
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/checkpoints",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-11")),
    tag = AGENT_CAPTURE_TAG
)]
async fn post_checkpoint() -> StatusCode {
    fixture()
}

#[utoipa::path(
    get,
    path = "/repos/{repo}/sessions",
    params(("repo" = String, Path, description = "Single percent-encoded repo path segment")),
    responses((status = 501, description = "Fixture; handler lands in AC-12")),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_sessions() -> StatusCode {
    fixture()
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-12")),
    tag = AGENT_CAPTURE_TAG
)]
async fn get_session() -> StatusCode {
    fixture()
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/checkpoints",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-12")),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_checkpoints() -> StatusCode {
    fixture()
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/transcript",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-12")),
    tag = AGENT_CAPTURE_TAG
)]
async fn get_transcript() -> StatusCode {
    fixture()
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/file-ops",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses((status = 501, description = "Fixture; handler lands in AC-12")),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_file_ops() -> StatusCode {
    fixture()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;
    use crate::{
        api::oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        ceres::api_service::cache::GitObjectCache,
        config::{
            AgentCaptureIngestTokenConfig, PushAuth, reload::ConfigHandle, testing::isolated_config,
        },
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            service::agent_capture_service::AgentCaptureService,
            storage::{
                Storage,
                agent_capture_storage::{AgentCaptureStorage, InsertEvent},
                object_storage::mock_object_storage,
            },
            tests::test_storage_with_config,
        },
    };

    fn dummy_state() -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "ac-16-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage: Storage::mock(),
        }
    }

    fn state_with_ingest_token() -> MonoApiServiceState {
        let mut state = dummy_state();
        let mut config = (*state.storage.config()).clone();
        config.agent_capture.ingest_tokens = vec![AgentCaptureIngestTokenConfig {
            name: "hook".to_owned(),
            token: "secret-ci".to_owned(),
            paths: None,
            tenant_id: None,
        }];
        let config = Arc::new(config);
        state.storage.config_handle = ConfigHandle::from_arc(config.clone());
        state.storage.config = config;
        state
    }

    fn public_app(state: MonoApiServiceState) -> (Router, Vec<String>) {
        let (router, api) = OpenApiRouter::new()
            .nest("/api/v1", routers())
            .split_for_parts();
        let paths = api.paths.paths.keys().cloned().collect();
        (router.with_state(state), paths)
    }

    fn sample_path(template: &str) -> String {
        template
            .replace("{repo}", "third-part%2Fmega")
            .replace("{client_session_id}", "sess-1")
            .replace("{capture_id}", "1")
            .replace("{lease_id}", "lease-1")
    }

    async fn discovery(
        state: MonoApiServiceState,
        authorization: Option<&str>,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::get("/api/v1/agent-capture/discovery");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    async fn json_body(response: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn all_public_paths_construct() {
        let (router, paths) = public_app(dummy_state());
        for (method, path) in PUBLIC_ENDPOINTS {
            assert!(
                paths.iter().any(|p| p == path),
                "OpenAPI missing {path}; have {paths:?}"
            );
            assert!(
                !path.contains("/api/v1/api/v1"),
                "doubled /api/v1 prefix: {path}"
            );
            let status = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.parse::<Method>().expect("method"))
                        .uri(sample_path(path))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response")
                .status();
            let expected = if (*method == "GET" && *path == "/api/v1/agent-capture/discovery")
                || (*method == "PUT"
                    && *path == "/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}")
            {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::NOT_IMPLEMENTED
            };
            assert_eq!(status, expected, "{method} {path} -> {status}");
        }
    }

    #[tokio::test]
    async fn repo_path_is_single_segment() {
        let src = include_str!("agent_capture_router.rs");
        let catch_all_repo = format!("{{*{}}}", "repo");
        let catch_all_path = format!("{{*{}}}", "path");
        assert!(src.contains("{repo}"));
        assert!(!src.contains(&catch_all_repo));
        assert!(!src.contains(&catch_all_path));

        let (router, paths) = public_app(dummy_state());
        assert!(
            paths.iter().any(|p| p.contains("/repos/{repo}/sessions")),
            "expected single-segment {{repo}}: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .all(|p| !p.contains(&catch_all_repo) && !p.contains("{*")),
            "catch-all repo param leaked into OpenAPI: {paths:?}"
        );

        let encoded = router
            .clone()
            .oneshot(
                Request::get("/api/v1/agent-capture/repos/third-part%2Fmega/sessions")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status();
        assert_eq!(encoded, StatusCode::NOT_IMPLEMENTED);

        let multi = router
            .oneshot(
                Request::get("/api/v1/agent-capture/repos/third-part/mega/sessions")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status();
        assert_eq!(multi, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn discovery_unauthorized() {
        let response = discovery(dummy_state(), None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(!body.to_string().contains("secret-ci"));
    }

    #[tokio::test]
    async fn discovery_wrong_token() {
        let response = discovery(state_with_ingest_token(), Some("Bearer wrong-token")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "unauthorized");
        assert!(!body.to_string().contains("secret-ci"));
        assert!(!body.to_string().contains("wrong-token"));
    }

    #[tokio::test]
    async fn discovery_raw_accepted() {
        let response = discovery(state_with_ingest_token(), Some("Bearer secret-ci")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["raw_accepted"], true);
    }

    fn ingest_token(paths: Option<Vec<String>>) -> AgentCaptureIngestTokenConfig {
        AgentCaptureIngestTokenConfig {
            name: "hook".to_owned(),
            token: "secret-ci".to_owned(),
            paths,
            tenant_id: None,
        }
    }

    fn state_from_storage(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "ac-21-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage,
        }
    }

    async fn db_state(paths: Option<Vec<String>>) -> (tempfile::TempDir, MonoApiServiceState) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp.path().join("config"));
        config.git.push_auth = Some(PushAuth::None);
        config.agent_capture.enabled = true;
        config.agent_capture.ingest_tokens = vec![ingest_token(paths)];
        let mut storage = test_storage_with_config(temp.path(), config).await;
        let base = storage.app_service.mono_storage.base.clone();
        storage.agent_capture_service = AgentCaptureService {
            storage: AgentCaptureStorage { base },
            obj_storage: mock_object_storage(),
        };
        (temp, state_from_storage(storage))
    }

    async fn put_session_request(
        state: MonoApiServiceState,
        repo: &str,
        client_session_id: &str,
        authorization: Option<&str>,
        body: &str,
        idempotency_key: Option<&str>,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::PUT)
            .uri(format!(
                "/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        if let Some(value) = idempotency_key {
            request = request.header("Idempotency-Key", value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    fn state_with_restricted_token() -> MonoApiServiceState {
        let mut state = dummy_state();
        let mut config = (*state.storage.config()).clone();
        config.agent_capture.ingest_tokens =
            vec![ingest_token(Some(vec!["/third-part/mega".to_owned()]))];
        let config = Arc::new(config);
        state.storage.config_handle = ConfigHandle::from_arc(config.clone());
        state.storage.config = config;
        state
    }

    #[tokio::test]
    async fn put_session_unauthorized() {
        let response = put_session_request(
            dummy_state(),
            "third-part%2Fmega",
            "sess-1",
            None,
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_session_unauthorized_before_invalid_path() {
        let response = put_session_request(
            dummy_state(),
            "%FF",
            "sess-1",
            None,
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_session_unknown_path_is_404() {
        let response = put_session_request(
            state_with_restricted_token(),
            "other%2Frepo",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_session_unknown_path_before_oversize() {
        let oversized = format!(
            r#"{{"session_kind":"external_capture","pad":"{}"}}"#,
            "x".repeat(70_000)
        );
        let response = put_session_request(
            state_with_restricted_token(),
            "other%2Frepo",
            "sess-1",
            Some("Bearer secret-ci"),
            &oversized,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_session_returns_capture_id() {
        let (_temp, state) = db_state(None).await;
        let response = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let capture_id = body["capture_id"].as_i64().expect("capture_id");
        assert!(capture_id > 0);
    }

    #[tokio::test]
    async fn put_session_conflict() {
        let (_temp, state) = db_state(None).await;
        let first = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture","started_at":"2026-01-01T00:00:00Z"}"#,
            Some("idem-a"),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture","started_at":"2026-02-01T00:00:00Z"}"#,
            Some("idem-b"),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn session_put_retry_after_completeness_change() {
        let (_temp, state) = db_state(None).await;
        let body = r#"{"session_kind":"external_capture"}"#;
        let first = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            body,
            None,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let capture_id = json_body(first).await["capture_id"].as_i64().expect("id");
        state
            .storage
            .agent_capture_service
            .storage
            .insert_event(InsertEvent {
                capture_id,
                event_uid: "0:0".to_owned(),
                event_kind: "message".to_owned(),
                native_id: None,
                lifecycle_seq: None,
                payload: serde_json::json!({"n": 1}),
            })
            .await
            .expect("bump completeness");
        let second = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            body,
            None,
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            json_body(second).await["capture_id"].as_i64().expect("id"),
            capture_id
        );
    }
}
