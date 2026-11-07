//! Authenticated traces read API. MTR-34 publishes capabilities only.

use axum::{Json, extract::State, http::HeaderMap};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::MonoApiServiceState,
    api_model::traces::TracesCapabilities,
    common::errors::{ApiError, MegaError, TracesError},
    contract::git_protocol::lookup_push_token,
    jupiter::{service::trace_query_service, storage::base_storage::StorageConnector},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().routes(routes!(capabilities))
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

fn bearer_is_push_token(headers: &HeaderMap, state: &MonoApiServiceState) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    lookup_push_token(&state.storage.config().git.push_tokens, token).is_some()
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
}
