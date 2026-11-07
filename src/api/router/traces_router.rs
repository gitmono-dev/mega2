//! Authenticated traces read API. MTR-34 publishes capabilities only.

use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::MonoApiServiceState,
    api_model::traces::{TargetResolve, TargetResolveQuery, TracesCapabilities},
    common::errors::{ApiError, MegaError, TracesError},
    config::PushTokenConfig,
    contract::git_protocol::{lookup_push_token, token_covers_repo},
    jupiter::{service::trace_query_service, storage::base_storage::StorageConnector},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(capabilities))
        .routes(routes!(resolve_target))
}

/// Capabilities for an authenticated caller.
///
/// Limits come from [`crate::api_model::traces::TracesCapabilityLimits::from_protocol_limits`].
#[utoipa::path(
    get,
    path = "/traces/capabilities",
    responses(
        (status = 200, description = "Protocol capabilities", body = TracesCapabilities),
        (status = 401, description = "Missing or rejected credentials")
    ),
    tag = "Traces"
)]
pub async fn capabilities(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
) -> Result<Json<TracesCapabilities>, ApiError> {
    if !bearer_is_push_token(&headers, &state) {
        return Err(ApiError::from(MegaError::from(
            TracesError::Unauthenticated,
        )));
    }
    let server_id = state
        .storage
        .config()
        .traces
        .deployment_server_id()
        .ok_or_else(|| ApiError::from(MegaError::from(TracesError::Unauthenticated)))?
        .to_owned();
    let body = trace_query_service::read_capabilities(
        state.storage.mono_storage().get_connection(),
        &server_id,
    )
    .await;
    Ok(Json(body))
}

fn authenticated_token(
    headers: &HeaderMap,
    state: &MonoApiServiceState,
) -> Result<PushTokenConfig, ApiError> {
    let denied = || ApiError::from(MegaError::from(TracesError::Unauthenticated));
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(denied());
    };
    let Ok(value) = value.to_str() else {
        return Err(denied());
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(denied());
    };
    if token.is_empty() {
        return Err(denied());
    }
    lookup_push_token(&state.storage.config().git.push_tokens, token)
        .cloned()
        .ok_or_else(denied)
}

fn bearer_is_push_token(headers: &HeaderMap, state: &MonoApiServiceState) -> bool {
    authenticated_token(headers, state).is_ok()
}

/// Locate one active registered target. Missing and unreadable paths are the same 404.
#[utoipa::path(
    get,
    path = "/traces/targets/resolve",
    params(("path" = String, Query, description = "Canonical project path")),
    responses(
        (status = 200, description = "Registered target", body = TargetResolve),
        (status = 401, description = "Missing or rejected credentials"),
        (status = 404, description = "Unknown or unreadable target")
    ),
    tag = "Traces"
)]
pub async fn resolve_target(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
    Query(query): Query<TargetResolveQuery>,
) -> Result<Json<TargetResolve>, ApiError> {
    let token = authenticated_token(&headers, &state)?;
    let path = crate::jupiter::storage::trace_target_storage::canonical_project_path(&query.path)
        .map_err(|_| ApiError::from(MegaError::from(TracesError::InvalidParameter)))?;
    let may_read = token_covers_repo(&token, &path);
    match trace_query_service::resolve_registered_target(
        state.storage.mono_storage().get_connection(),
        &path,
        may_read,
    )
    .await
    {
        Ok(body) => Ok(Json(body)),
        Err(trace_query_service::TargetResolveError::UnknownScope) => {
            Err(ApiError::from(MegaError::from(TracesError::UnknownScope)))
        }
        Err(trace_query_service::TargetResolveError::InvalidPath) => Err(ApiError::from(
            MegaError::from(TracesError::InvalidParameter),
        )),
        Err(trace_query_service::TargetResolveError::Database(error)) => Err(ApiError::from(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
        api_model::traces::TracesCapabilityLimits,
        ceres::api_service::cache::GitObjectCache,
        config::{PushTokenConfig, testing::isolated_config},
        jupiter::tests::{test_redis_manager, test_storage_with_config},
    };

    async fn capabilities_app(token: &str) -> axum::Router {
        let dir = tempfile::tempdir().expect("dir");
        let mut config = isolated_config(dir.path().join("config"));
        config.traces.enabled = true;
        config.traces.server_id = Some("11111111-1111-4111-8111-111111111111".to_owned());
        config.git.push_tokens.push(PushTokenConfig {
            name: "reader".to_owned(),
            token: token.to_owned(),
            paths: None,
        });
        let storage = test_storage_with_config(dir.path(), config).await;
        let state = MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage,
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_owned(),
        };
        // Keep the temp dir alive for the storage files by leaking it for the test process.
        std::mem::forget(dir);
        routers().with_state(state).into()
    }

    async fn call(app: axum::Router, token: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::builder().uri("/traces/capabilities");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    #[tokio::test]
    async fn capabilities_base_requires_a_push_token_and_projects_protocol_limits() {
        let app = capabilities_app("secret-token").await;
        let (status, body) = call(app, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(body.contains("unauthenticated"), "{body}");

        let app = capabilities_app("secret-token").await;
        let (status, body) = call(app, Some("wrong-token")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

        let app = capabilities_app("secret-token").await;
        let (status, first) = call(app.clone(), Some("secret-token")).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let (status, second) = call(app, Some("secret-token")).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(first["protocol"], "libra-traces/v1");
        assert_eq!(first["server_id"], "11111111-1111-4111-8111-111111111111");
        assert_eq!(first["server_id"], second["server_id"]);
        assert_eq!(first["git_transports"], serde_json::json!(["http", "ssh"]));
        assert_eq!(
            first["object_formats"],
            serde_json::json!(["sha1", "sha256"])
        );
        assert_eq!(
            first["limits"],
            serde_json::to_value(TracesCapabilityLimits::from_protocol_limits()).unwrap()
        );
    }

    use sea_orm::EntityTrait;

    async fn resolve_app() -> (axum::Router, sea_orm::DatabaseConnection) {
        let dir = tempfile::tempdir().expect("dir");
        let mut config = isolated_config(dir.path().join("config"));
        config.traces.enabled = true;
        config.traces.server_id = Some("11111111-1111-4111-8111-111111111111".to_owned());
        config.git.push_tokens.push(PushTokenConfig {
            name: "wide".to_owned(),
            token: "secret-token".to_owned(),
            paths: None,
        });
        config.git.push_tokens.push(PushTokenConfig {
            name: "narrow".to_owned(),
            token: "narrow-token".to_owned(),
            paths: Some(vec!["/other".to_owned()]),
        });
        let storage = test_storage_with_config(dir.path(), config).await;
        let db = storage.mono_storage().get_connection().clone();
        crate::jupiter::storage::trace_target_storage::insert_active(
            &db,
            "native",
            "/project/app",
            None,
        )
        .await
        .expect("target");
        let state = MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage,
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_owned(),
        };
        std::mem::forget(dir);
        (routers().with_state(state).into(), db)
    }

    async fn call_resolve(
        app: axum::Router,
        token: Option<&str>,
        path: &str,
    ) -> (StatusCode, String) {
        let uri = format!("/traces/targets/resolve?path={path}");
        let mut request = Request::builder().uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    #[tokio::test]
    async fn target_resolve_route_locates_only_a_readable_registered_target() {
        let (app, db) = resolve_app().await;
        let before = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len();
        let (status, body) = call_resolve(app.clone(), None, "/project/app").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = call_resolve(app.clone(), Some("narrow-token"), "/project/app").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("unknown_scope"), "{body}");
        let (status, body) = call_resolve(app.clone(), Some("secret-token"), "/missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (status, body) = call_resolve(app, Some("secret-token"), "/project/app").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(json["target_id"].as_str().unwrap().len() > 10, "{body}");
        assert_eq!(json["project_path"], "/project/app");
        assert_eq!(json["target_kind"], "native");
        let after = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len();
        assert_eq!(before, 1);
        assert_eq!(after, before);
    }
}
