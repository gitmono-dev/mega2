use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::AGENT_CAPTURE_TAG},
    api_model::agent_capture::{
        AgentCaptureHttpError, DiscoveryResponse, ErrorEnvelope, bearer_token,
    },
    ceres::agent_capture::auth::lookup_ingest_token,
};

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
    let presented = bearer_token(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )?;
    lookup_ingest_token(
        &state.storage.config().agent_capture.ingest_tokens,
        presented,
    )
    .ok_or_else(AgentCaptureHttpError::unauthorized)?;
    Ok(Json(DiscoveryResponse { raw_accepted: true }))
}

#[utoipa::path(
    put,
    path = "/repos/{repo}/sessions/{client_session_id}",
    params(
        ("repo" = String, Path, description = "Single percent-encoded repo path segment"),
        ("client_session_id" = String, Path, description = "Client session id")
    ),
    responses((status = 501, description = "Fixture; handler lands in AC-21")),
    tag = AGENT_CAPTURE_TAG
)]
async fn put_session() -> StatusCode {
    fixture()
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
        config::{AgentCaptureIngestTokenConfig, reload::ConfigHandle},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::storage::Storage,
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
            let expected = if *method == "GET" && *path == "/api/v1/agent-capture/discovery" {
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
}
