use axum::http::StatusCode;
use utoipa_axum::{router::OpenApiRouter, routes};

pub const AGENT_CAPTURE_TAG: &str = "Agent Capture";

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
pub fn routers<S>() -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    OpenApiRouter::new().nest("/agent-capture", capture_routes())
}

fn capture_routes<S>() -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
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
    responses((status = 501, description = "Fixture; handler lands in AC-16")),
    tag = AGENT_CAPTURE_TAG
)]
async fn discovery() -> StatusCode {
    fixture()
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
    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;

    fn public_app() -> (Router, Vec<String>) {
        let (router, api) = OpenApiRouter::new()
            .nest("/api/v1", routers())
            .split_for_parts();
        let paths = api.paths.paths.keys().cloned().collect();
        (router, paths)
    }

    fn sample_path(template: &str) -> String {
        template
            .replace("{repo}", "third-part%2Fmega")
            .replace("{client_session_id}", "sess-1")
            .replace("{capture_id}", "1")
            .replace("{lease_id}", "lease-1")
    }

    #[tokio::test]
    async fn all_public_paths_construct() {
        let (router, paths) = public_app();
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
            assert_eq!(
                status,
                StatusCode::NOT_IMPLEMENTED,
                "{method} {path} -> {status}"
            );
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

        let (router, paths) = public_app();
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
}
