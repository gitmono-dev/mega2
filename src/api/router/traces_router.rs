//! Authenticated traces read API. Credentials and read decisions match traces Git.

use axum::{
    Json,
    extract::{FromRef, Path, Query, State, rejection::QueryRejection},
    http::HeaderMap,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::MonoApiServiceState,
    api_model::traces::{
        SessionDetail, SessionPage, SessionQuery, StreamDirectory, StreamDirectoryQuery,
        TargetResolve, TargetResolveQuery, TracesCapabilities, admit_session_query,
        admit_stream_directory_query,
    },
    ceres::api_service::state::ProtocolApiState,
    common::errors::{ApiError, MegaError, TracesError},
    contract::git_protocol::http::{authenticate_trace_api, authorize_trace_api_read},
    jupiter::{service::trace_query_service, storage::base_storage::StorageConnector},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(capabilities))
        .routes(routes!(resolve_target))
        .routes(routes!(list_target_streams))
        .routes(routes!(list_stream_sessions))
        .routes(routes!(read_stream_session))
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
    authenticate_trace_api(&ProtocolApiState::from_ref(&state), &headers)
        .await
        .map_err(ApiError::from)?;
    let server_id = state
        .storage
        .config()
        .traces
        .deployment_server_id()
        .ok_or_else(|| ApiError::from(MegaError::from(TracesError::Unauthenticated)))?
        .to_owned();
    let index = state.storage.trace_search_index_service.state();
    Ok(Json(trace_query_service::read_capabilities(
        &server_id, &index,
    )))
}

/// List streams of one target. The target id in the path is the only scope.
#[utoipa::path(
    get,
    path = "/traces/targets/{target_id}/streams",
    params(
        ("target_id" = String, Path, description = "Registered target id"),
        ("source" = Option<String>, Query, description = "Exact source_repo_id"),
        ("owner" = Option<String>, Query, description = "Exact principal id"),
        ("epoch" = Option<String>, Query, description = "Exact epoch id"),
        ("limit" = Option<u64>, Query, description = "Page size; default 50, max 200"),
        ("cursor" = Option<String>, Query, description = "Keyset cursor bound to these filters")
    ),
    responses(
        (status = 200, description = "Stream directory", body = StreamDirectory),
        (status = 400, description = "Invalid parameter or cursor"),
        (status = 401, description = "Missing or rejected credentials"),
        (status = 404, description = "Unknown or unreadable target")
    ),
    tag = "Traces"
)]
pub async fn list_target_streams(
    State(state): State<MonoApiServiceState>,
    Path(target_id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<StreamDirectoryQuery>, QueryRejection>,
) -> Result<Json<StreamDirectory>, ApiError> {
    let protocol = ProtocolApiState::from_ref(&state);
    authenticate_trace_api(&protocol, &headers)
        .await
        .map_err(ApiError::from)?;
    let Query(query) =
        query.map_err(|_| ApiError::from(MegaError::from(TracesError::InvalidParameter)))?;
    let request = admit_stream_directory_query(&target_id, query)
        .map_err(|error| ApiError::from(MegaError::from(error)))?;
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let target = crate::jupiter::storage::trace_target_storage::find_by_id(db, &target_id)
        .await
        .map_err(ApiError::from)?;
    let Some(target) = target.filter(|row| row.active) else {
        return Err(ApiError::from(MegaError::from(TracesError::UnknownScope)));
    };
    let may_read = match authorize_trace_api_read(&protocol, &headers, &target.project_path).await {
        Ok(_) => true,
        Err(MegaError::Traces(TracesError::UnknownScope)) => false,
        Err(error) => return Err(ApiError::from(error)),
    };
    match trace_query_service::list_target_streams(db, &target_id, &request, may_read).await {
        Ok(body) => Ok(Json(body)),
        Err(trace_query_service::StreamDirectoryError::UnknownScope) => {
            Err(ApiError::from(MegaError::from(TracesError::UnknownScope)))
        }
        Err(trace_query_service::StreamDirectoryError::InvalidParameter) => Err(ApiError::from(
            MegaError::from(TracesError::InvalidParameter),
        )),
        Err(trace_query_service::StreamDirectoryError::Database(error)) => {
            Err(ApiError::from(error))
        }
    }
}

/// Sessions of one stream. The stream scope is the only authorization scope.
#[utoipa::path(
    get,
    path = "/traces/streams/{stream_id}/sessions",
    params(
        ("stream_id" = String, Path, description = "Stream id"),
        ("agent" = Option<String>, Query, description = "Exact metadata.agent_kind"),
        ("session" = Option<String>, Query, description = "Exact session key"),
        ("code_commit" = Option<String>, Query, description = "Exact code OID"),
        ("since" = Option<String>, Query, description = "RFC3339 lower bound"),
        ("until" = Option<String>, Query, description = "RFC3339 upper bound"),
        ("limit" = Option<u64>, Query, description = "Page size; default 50, max 200"),
        ("cursor" = Option<String>, Query, description = "Keyset cursor bound to these filters")
    ),
    responses(
        (status = 200, description = "Session page", body = SessionPage),
        (status = 400, description = "Invalid parameter or cursor"),
        (status = 401, description = "Missing or rejected credentials"),
        (status = 404, description = "Unknown or unreadable stream")
    ),
    tag = "Traces"
)]
pub async fn list_stream_sessions(
    State(state): State<MonoApiServiceState>,
    Path(stream_id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<SessionQuery>, QueryRejection>,
) -> Result<Json<SessionPage>, ApiError> {
    let protocol = ProtocolApiState::from_ref(&state);
    authenticate_trace_api(&protocol, &headers)
        .await
        .map_err(ApiError::from)?;
    let Query(query) =
        query.map_err(|_| ApiError::from(MegaError::from(TracesError::InvalidParameter)))?;
    let request = admit_session_query(&stream_id, query)
        .map_err(|error| ApiError::from(MegaError::from(error)))?;
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let may_read =
        match authorize_trace_api_read_for_stream(&protocol, &headers, db, &stream_id).await {
            Ok(may_read) => may_read,
            Err(error) => return Err(ApiError::from(error)),
        };
    match trace_query_service::list_stream_sessions(db, &stream_id, &request, may_read).await {
        Ok(body) => Ok(Json(body)),
        Err(trace_query_service::SessionReadError::UnknownScope) => {
            Err(ApiError::from(MegaError::from(TracesError::UnknownScope)))
        }
        Err(trace_query_service::SessionReadError::InvalidParameter) => Err(ApiError::from(
            MegaError::from(TracesError::InvalidParameter),
        )),
        Err(trace_query_service::SessionReadError::Database(error)) => Err(ApiError::from(error)),
    }
}

/// One session of one stream. The stream scope is the only authorization scope.
#[utoipa::path(
    get,
    path = "/traces/streams/{stream_id}/sessions/{session_id}",
    params(
        ("stream_id" = String, Path, description = "Stream id"),
        ("session_id" = String, Path, description = "Session key")
    ),
    responses(
        (status = 200, description = "Session metadata", body = SessionDetail),
        (status = 401, description = "Missing or rejected credentials"),
        (status = 404, description = "Unknown or unreadable session")
    ),
    tag = "Traces"
)]
pub async fn read_stream_session(
    State(state): State<MonoApiServiceState>,
    Path((stream_id, session_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionDetail>, ApiError> {
    let protocol = ProtocolApiState::from_ref(&state);
    authenticate_trace_api(&protocol, &headers)
        .await
        .map_err(ApiError::from)?;
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let may_read =
        match authorize_trace_api_read_for_stream(&protocol, &headers, db, &stream_id).await {
            Ok(may_read) => may_read,
            Err(error) => return Err(ApiError::from(error)),
        };
    match trace_query_service::read_stream_session(db, &stream_id, &session_id, may_read).await {
        Ok(body) => Ok(Json(body)),
        Err(trace_query_service::SessionReadError::UnknownScope) => {
            Err(ApiError::from(MegaError::from(TracesError::UnknownScope)))
        }
        Err(trace_query_service::SessionReadError::InvalidParameter) => Err(ApiError::from(
            MegaError::from(TracesError::InvalidParameter),
        )),
        Err(trace_query_service::SessionReadError::Database(error)) => Err(ApiError::from(error)),
    }
}

/// Read decision for one stream, resolved through its active target path.
async fn authorize_trace_api_read_for_stream(
    protocol: &ProtocolApiState,
    headers: &HeaderMap,
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
) -> Result<bool, MegaError> {
    use sea_orm::EntityTrait;
    let stream = crate::callisto::trace_stream::Entity::find_by_id(stream_id.to_owned())
        .one(db)
        .await?
        .ok_or_else(|| MegaError::from(TracesError::UnknownScope))?;
    let target = crate::jupiter::storage::trace_target_storage::find_by_id(db, &stream.target_id)
        .await?
        .filter(|row| row.active)
        .ok_or_else(|| MegaError::from(TracesError::UnknownScope))?;
    match authorize_trace_api_read(protocol, headers, &target.project_path).await {
        Ok(_) => Ok(true),
        Err(MegaError::Traces(TracesError::UnknownScope)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Locate one active registered target. Missing and unreadable paths are the same 404.
///
/// Authentication runs first. An authenticated missing or non-canonical `path`
/// is 400 `invalid_parameter`.
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
    query: Result<Query<TargetResolveQuery>, QueryRejection>,
) -> Result<Json<TargetResolve>, ApiError> {
    let protocol = ProtocolApiState::from_ref(&state);
    authenticate_trace_api(&protocol, &headers)
        .await
        .map_err(ApiError::from)?;
    let Query(query) =
        query.map_err(|_| ApiError::from(MegaError::from(TracesError::InvalidParameter)))?;
    let path = crate::jupiter::storage::trace_target_storage::canonical_project_path(&query.path)
        .map_err(|_| ApiError::from(MegaError::from(TracesError::InvalidParameter)))?;
    let may_read = match authorize_trace_api_read(&protocol, &headers, &path).await {
        Ok(_) => true,
        Err(MegaError::Traces(TracesError::UnknownScope)) => false,
        Err(error) => return Err(ApiError::from(error)),
    };
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
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::{
        api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
        api_model::traces::TracesCapabilityLimits,
        ceres::api_service::cache::GitObjectCache,
        config::{PushAuth, PushTokenConfig, testing::isolated_config},
        jupiter::tests::{test_redis_manager, test_storage_with_config},
    };

    const SERVER_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn token(name: &str, secret: &str, paths: Option<Vec<String>>) -> PushTokenConfig {
        PushTokenConfig {
            name: name.to_owned(),
            token: secret.to_owned(),
            paths,
        }
    }

    async fn bind(db: &sea_orm::DatabaseConnection, credential_id: &str) {
        let now = chrono::Utc::now().fixed_offset();
        let principal_id = Uuid::new_v4().to_string();
        crate::callisto::trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("binding");
    }

    /// `wide` and `narrow` have active traces bindings; `unbound` is a valid
    /// push token with no binding.
    async fn traces_app() -> (axum::Router, sea_orm::DatabaseConnection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("dir");
        let mut config = isolated_config(dir.path().join("config"));
        config.traces.enabled = true;
        config.traces.server_id = Some(SERVER_ID.to_owned());
        config.git.push_auth = Some(PushAuth::Token);
        config.git.push_tokens = vec![
            token("wide", "wide-secret", None),
            token("narrow", "narrow-secret", Some(vec!["/other".to_owned()])),
            token("unbound", "unbound-secret", None),
        ];
        let storage = test_storage_with_config(dir.path(), config).await;
        let db = storage.mono_storage().get_connection().clone();
        bind(&db, "wide").await;
        bind(&db, "narrow").await;
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
        (routers().with_state(state).into(), db, dir)
    }

    async fn get(app: axum::Router, uri: &str, secret: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::builder().uri(uri);
        if let Some(secret) = secret {
            request = request.header("authorization", format!("Bearer {secret}"));
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
    async fn capabilities_base_requires_a_traces_credential_and_projects_protocol_limits() {
        let (app, _, _dir) = traces_app().await;
        let uri = "/traces/capabilities";
        let (status, body) = get(app.clone(), uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(body.contains("unauthenticated"), "{body}");
        let (status, body) = get(app.clone(), uri, Some("wrong-secret")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(app.clone(), uri, Some("unbound-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, first) = get(app.clone(), uri, Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let (status, second) = get(app, uri, Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(first["protocol"], "libra-traces/v1");
        assert_eq!(first["server_id"], SERVER_ID);
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

    #[tokio::test]
    async fn capabilities_base_accepts_mono_access_token_in_review_mode() {
        let dir = tempfile::tempdir().expect("dir");
        let mut config = isolated_config(dir.path().join("config"));
        config.traces.enabled = true;
        config.traces.server_id = Some(SERVER_ID.to_owned());
        config.git.push_auth = None;
        let storage = test_storage_with_config(dir.path(), config).await;
        let mono = storage.mono_storage();
        let db = mono.get_connection();
        crate::callisto::access_token::ActiveModel {
            id: Set(101),
            username: Set("synthetic-user".to_owned()),
            token: Set("synthetic-access-token".to_owned()),
            created_at: Set(chrono::Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .expect("access token");
        let db = storage.mono_storage().get_connection().clone();
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
        let app = routers().with_state(state).into();
        let (status, body) = get(app, "/traces/capabilities", Some("synthetic-access-token")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let bindings = crate::callisto::trace_credential_binding::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert!(bindings.is_empty(), "{bindings:?}");
    }

    #[tokio::test]
    async fn capabilities_readiness_route_reports_the_process_index_state() {
        let (app, _, _dir) = traces_app().await;
        let (status, body) = get(app, "/traces/capabilities", Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["search_ready"], false);
        assert_eq!(json["bounded_scan_supported"], false);
        assert!(json["search_unavailable_reason"].is_string(), "{json}");
    }

    #[tokio::test]
    async fn target_resolve_parameter_error_is_invalid_parameter_after_auth() {
        let (app, _, _dir) = traces_app().await;
        let (status, body) = get(app.clone(), "/traces/targets/resolve", Some("wide-secret")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("invalid_parameter"), "{body}");
        let (status, body) = get(
            app,
            "/traces/targets/resolve?path=/project/../app",
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("invalid_parameter"), "{body}");
    }

    #[tokio::test]
    async fn stream_directory_filters_and_rejects_unreadable_targets() {
        use sea_orm::Set;
        let (app, db, _dir) = traces_app().await;
        let target = crate::callisto::trace_target::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now().fixed_offset();
        async fn owner(db: &sea_orm::DatabaseConnection) -> String {
            let id = Uuid::new_v4().to_string();
            crate::callisto::trace_principal::ActiveModel {
                id: Set(id.clone()),
                created_at: Set(chrono::Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .unwrap();
            id
        }
        let owner_a = owner(&db).await;
        let owner_b = owner(&db).await;
        let epoch_a = Uuid::new_v4().to_string();
        let epoch_b = Uuid::new_v4().to_string();
        for (owner_id, source, epoch) in [
            (owner_a.as_str(), "src-a", epoch_a.as_str()),
            (owner_b.as_str(), "src-b", epoch_b.as_str()),
        ] {
            crate::callisto::trace_stream::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                principal_id: Set(owner_id.to_owned()),
                target_id: Set(target.id.clone()),
                source_repo_id: Set(source.to_owned()),
                epoch_id: Set(epoch.to_owned()),
                hash_kind: Set("sha1".to_owned()),
                tip_oid: Set(None),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .unwrap();
        }
        let path = format!("/traces/targets/{}/streams", target.id);
        let (status, body) = get(app.clone(), &path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(app.clone(), &format!("{path}?limit=0"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(app.clone(), &path, Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 2);
        assert!(
            json["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["target_id"] == target.id)
        );
        let (status, body) = get(
            app.clone(),
            &format!("{path}?source=src-b"),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert_eq!(json["items"][0]["source"], "src-b");
        let (status, body) = get(
            app.clone(),
            &format!("{path}?owner={owner_b}"),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["items"][0]["owner"], owner_b);
        let (status, body) = get(
            app.clone(),
            &format!("{path}?epoch={epoch_a}"),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["items"][0]["epoch"], epoch_a);
        let (status, body) =
            get(app.clone(), &format!("{path}?limit=1"), Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let cursor = json["next_cursor"].as_str().unwrap();
        let (status, body) = get(
            app.clone(),
            &format!("{path}?limit=1&source=other&cursor={cursor}"),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("invalid_cursor"), "{body}");
        let (status, body) = get(
            app.clone(),
            &format!("{path}?limit=201"),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("invalid_parameter"), "{body}");
        let (status, body) = get(app.clone(), &path, Some("unbound-secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (status, body) = get(app.clone(), &path, Some("narrow-secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (status, body) = get(
            app,
            &format!("/traces/targets/{}/streams", Uuid::new_v4()),
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test]
    async fn trace_api_auth_regression_unbound_and_unreadable_credentials() {
        let (app, db, _dir) = traces_app().await;
        let before = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap();
        let uri = "/traces/targets/resolve?path=/project/app";
        let (status, body) = get(app.clone(), uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(app.clone(), "/traces/targets/resolve", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = get(app.clone(), uri, Some("unbound-secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("unknown_scope"), "{body}");
        let (status, body) = get(app.clone(), uri, Some("narrow-secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.contains("unknown_scope"), "{body}");
        let (status, body) = get(
            app.clone(),
            "/traces/targets/resolve?path=/missing",
            Some("wide-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        let (status, body) = get(app.clone(), uri, Some("wide-secret")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["target_id"], before[0].id);
        assert_eq!(json["project_path"], "/project/app");
        assert_eq!(json["target_kind"], "native");
        let after = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(after, before);
        let binding = crate::callisto::trace_credential_binding::Entity::find()
            .filter(crate::callisto::trace_credential_binding::Column::CredentialId.eq("wide"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut retired: crate::callisto::trace_credential_binding::ActiveModel = binding.into();
        retired.retired_at = Set(Some(chrono::Utc::now().fixed_offset()));
        retired.update(&db).await.unwrap();
        let (status, body) = get(app, uri, Some("wide-secret")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}
