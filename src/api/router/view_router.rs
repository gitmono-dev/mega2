use std::time::Duration;

use anyhow::anyhow;
use axum::{
    Json,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_write_auth::authorize_trunk_api_write},
    ceres::view::{
        filter::{
            FilterParseError, parse_for_registration,
            validate::{RegistrationRejection, validate_for_registration},
        },
        name::validate_view_name,
    },
    common::errors::ApiError,
    config::{MonoObjectFormat, PushAuth},
    contract::api::common::CommonResult,
    jupiter::storage::view_admission::{
        AdmitLimits, AdmitOutcome, AdmitRequest, FilterDefinition, RejectReason,
    },
};

const REGISTER_BODY_LIMIT: usize = 65_536;
const REGISTER_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const REGISTER_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const REGISTER_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const VIEW_REGISTER: &str = "View registration";

#[derive(Deserialize, ToSchema)]
struct RegisterViewRequest {
    filter_spec: String,
    name: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct RegisterViewResponse {
    filter_id: String,
    name: Option<String>,
    version: Option<i32>,
    ready: bool,
}

#[derive(Default, Deserialize, IntoParams)]
struct RegisterWaitQuery {
    #[serde(default)]
    wait: bool,
}

#[derive(Deserialize, IntoParams)]
struct ViewNameQuery {
    name: String,
    version: Option<i32>,
}

#[derive(Serialize, ToSchema)]
struct ViewStatusResponse {
    filter_id: String,
    canonical_spec: String,
    src_paths: serde_json::Value,
    push_enabled: bool,
    ready: bool,
    projected_seq: i64,
    lag_commits: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct ViewByNameResponse {
    filter_id: String,
    name: String,
    version: i32,
}

pub(crate) fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(register_view))
        .routes(routes!(get_view_by_name))
        .routes(routes!(get_view_status))
        .routes(routes!(get_view_metrics))
        .layer(DefaultBodyLimit::max(REGISTER_BODY_LIMIT))
}

fn parse_error_rule(error: &FilterParseError) -> &'static str {
    match error {
        FilterParseError::Backslash => "backslash is not allowed in a path",
        FilterParseError::SegmentEdgeWhitespace => "path segment has edge whitespace",
        FilterParseError::InvalidSegment => "path segment is invalid",
        FilterParseError::NeedsQuoting => "path requires whole-path quoting",
        FilterParseError::PartialQuote => "partial path quoting is invalid",
        FilterParseError::UnterminatedQuote => "quoted path is unterminated",
        FilterParseError::ControlChar => "control character is not allowed in a path",
        FilterParseError::EmptySelectorSegment => "selector segment must be nonempty",
        FilterParseError::ExcludeArgNotSelector => "exclude argument must be a selector",
        FilterParseError::Syntax => "filter syntax is invalid",
        FilterParseError::SpecTooLarge => "filter spec exceeds 16384 bytes",
        FilterParseError::NestingTooDeep => "filter nesting exceeds 16 levels",
    }
}

fn validation_error_rule(error: &RegistrationRejection) -> String {
    match error {
        RegistrationRejection::Trivial => "filter has no registrable source paths".to_owned(),
        RegistrationRejection::ScaleLimit {
            which,
            limit,
            actual,
        } => format!("filter scale {which:?} exceeds limit {limit} (actual {actual})"),
        RegistrationRejection::ComposeOverlap { .. } => {
            "compose source paths must not overlap".to_owned()
        }
        RegistrationRejection::ImportNamespace { .. } => {
            "source path must be outside the import namespace".to_owned()
        }
        RegistrationRejection::ReservedFirstSegment { .. } => {
            "source path must not use a reserved first segment".to_owned()
        }
    }
}

fn body_error_rule(error: &JsonRejection) -> &'static str {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return "body exceeds 65536 bytes";
    }
    match error {
        JsonRejection::MissingJsonContentType(_) => "application/json content type is required",
        JsonRejection::JsonSyntaxError(_) => "JSON syntax is invalid",
        JsonRejection::JsonDataError(_) => "JSON fields are invalid",
        JsonRejection::BytesRejection(_) => "JSON body could not be read",
        _ => "JSON body could not be read",
    }
}

fn rejection_message(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::MaxFilters => "view filter limit reached",
        RejectReason::ColdStartSlots => "view cold-start slots are full",
        RejectReason::Rate => "view registration rate exceeded",
    }
}

#[utoipa::path(
    post,
    path = "/views",
    params(RegisterWaitQuery),
    request_body(content = RegisterViewRequest, content_type = "application/json"),
    responses(
        (status = 200, body = CommonResult<RegisterViewResponse>, content_type = "application/json"),
        (status = 400, body = CommonResult<RegisterViewResponse>, content_type = "application/json"),
        (status = 401, body = CommonResult<RegisterViewResponse>, content_type = "application/json"),
        (status = 403, body = CommonResult<RegisterViewResponse>, content_type = "application/json"),
        (status = 429, body = CommonResult<RegisterViewResponse>, content_type = "application/json")
    ),
    tag = VIEW_REGISTER
)]
async fn register_view(
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
    Query(query): Query<RegisterWaitQuery>,
    request: Result<Json<RegisterViewRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let config = state.storage.config();
    let Json(request) = request.map_err(|error| {
        ApiError::bad_request(anyhow!("request body: {}", body_error_rule(&error)))
    })?;
    let canonical = parse_for_registration(&request.filter_spec).map_err(|error| {
        ApiError::bad_request(anyhow!("filter_spec: {}", parse_error_rule(&error)))
    })?;
    let check =
        validate_for_registration(&canonical.filter, &config.monorepo).map_err(|error| {
            ApiError::bad_request(anyhow!("filter_spec: {}", validation_error_rule(&error)))
        })?;
    if let Some(name) = &request.name {
        validate_view_name(name)
            .map_err(|error| ApiError::bad_request(anyhow!("name: {error}")))?;
    }
    if config.monorepo.object_format != MonoObjectFormat::Sha1 {
        return Err(ApiError::bad_request(anyhow!(
            "object_format: only sha1 is supported for view registration"
        )));
    }
    if config.git.push_auth == Some(PushAuth::None) && !config.views.allow_anonymous_register {
        return Err(ApiError::forbidden(anyhow!(
            "anonymous registration is disabled"
        )));
    }
    let mut requester = None;
    for path in &check.src_paths {
        requester = Some(authorize_trunk_api_write(&config.git, &headers, path)?);
    }
    let requester = requester
        .ok_or_else(|| ApiError::internal(anyhow!("validated filter has no source path")))?;
    let outcome = state
        .storage
        .view_storage()
        .admit(
            AdmitRequest::register(
                FilterDefinition {
                    filter_id: canonical.filter_id.clone(),
                    canonical_spec: canonical.canonical_text,
                    algo_version: 1,
                    object_format: config.monorepo.object_format.as_str().to_owned(),
                    src_paths: serde_json::json!(check.src_paths),
                    push_enabled: check.push_enabled,
                },
                request.name.clone(),
                requester,
            ),
            AdmitLimits::from(&config.views),
        )
        .await
        .map_err(ApiError::internal)?;
    match outcome {
        AdmitOutcome::Admitted { version, ready } | AdmitOutcome::Idempotent { version, ready } => {
            state.storage.view_signal().notify_worker();
            let ready = if query.wait && !ready {
                wait_for_view_ready(&state, &canonical.filter_id).await?
            } else {
                ready
            };
            Ok(Json(CommonResult::success(Some(RegisterViewResponse {
                filter_id: canonical.filter_id,
                name: request.name,
                version,
                ready,
            })))
            .into_response())
        }
        AdmitOutcome::Rejected {
            reason,
            retry_after,
        } => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(CommonResult::<RegisterViewResponse>::failed(
                    rejection_message(reason),
                )),
            )
                .into_response();
            let value = HeaderValue::from_str(&retry_after.as_secs().to_string())
                .map_err(ApiError::internal)?;
            response.headers_mut().insert("retry-after", value);
            Ok(response)
        }
    }
}

async fn wait_for_view_ready(
    state: &MonoApiServiceState,
    filter_id: &str,
) -> Result<bool, ApiError> {
    let started = tokio::time::Instant::now();
    while let Some(remaining) = REGISTER_WAIT_TIMEOUT.checked_sub(started.elapsed()) {
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(REGISTER_WAIT_POLL_INTERVAL)).await;
        let row = state
            .storage
            .view_storage()
            .get_filter_by_filter_id(filter_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal(anyhow!("registered view filter disappeared")))?;
        if row.ready_seq.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[utoipa::path(
    get,
    path = "/views/{filter_id}",
    params(("filter_id" = String, Path, description = "64-character lowercase hexadecimal filter ID")),
    responses(
        (status = 200, body = CommonResult<ViewStatusResponse>, content_type = "application/json"),
        (status = 404, body = CommonResult<ViewStatusResponse>, content_type = "application/json")
    ),
    tag = VIEW_REGISTER
)]
async fn get_view_status(
    State(state): State<MonoApiServiceState>,
    Path(filter_id): Path<String>,
) -> Result<Json<CommonResult<ViewStatusResponse>>, ApiError> {
    if filter_id.len() != 64
        || !filter_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ApiError::not_found(anyhow!("view not found")));
    }
    let config = state.storage.config();
    let row = state
        .storage
        .view_storage()
        .view_status(&filter_id, config.views.max_append_walk)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(anyhow!("view not found")))?;
    Ok(Json(CommonResult::success(Some(ViewStatusResponse {
        filter_id,
        canonical_spec: row.canonical_spec,
        src_paths: row.src_paths,
        push_enabled: row.push_enabled,
        ready: row.ready,
        projected_seq: row.projected_seq,
        lag_commits: row.lag_commits,
    }))))
}

#[utoipa::path(
    get,
    path = "/views",
    params(ViewNameQuery),
    responses(
        (status = 200, body = CommonResult<ViewByNameResponse>, content_type = "application/json"),
        (status = 404, body = CommonResult<ViewByNameResponse>, content_type = "application/json")
    ),
    tag = VIEW_REGISTER
)]
async fn get_view_by_name(
    State(state): State<MonoApiServiceState>,
    Query(query): Query<ViewNameQuery>,
) -> Result<Json<CommonResult<ViewByNameResponse>>, ApiError> {
    let row = state
        .storage
        .view_storage()
        .find_view_by_name(&query.name, query.version)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(anyhow!("view not found")))?;
    Ok(Json(CommonResult::success(Some(ViewByNameResponse {
        filter_id: row.filter_id,
        name: row.name,
        version: row.version,
    }))))
}

#[utoipa::path(
    get,
    path = "/views/metrics",
    responses((status = 200, body = CommonResult<crate::jupiter::service::view_metrics::ViewMetricsSnapshot>, content_type = "application/json")),
    tag = VIEW_REGISTER
)]
async fn get_view_metrics(
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<crate::jupiter::service::view_metrics::ViewMetricsSnapshot>>, ApiError>
{
    let snapshot = state
        .storage
        .view_projection_service()
        .metrics_snapshot()
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(CommonResult::success(Some(snapshot))))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, convert::Infallible, str::FromStr, sync::Arc};

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use futures::stream;
    use git_internal::hash::{HashKind, ObjectHash};
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;
    use crate::{
        api::oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        ceres::api_service::cache::GitObjectCache,
        config::{Config, PushPolicy, PushTokenConfig, testing::isolated_config},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            storage::{
                Storage,
                base_storage::StorageConnector,
                view_storage::ViewLockMode,
                view_test_fixtures::{
                    cas_fixture_main, seed_linear_root_history,
                    seed_missing_first_parent_root_commit, seed_multi_parent_root_commit,
                    seed_single_parent_root_commit, seed_unrelated_root_history, set_fixture_main,
                },
            },
            tests::test_storage_with_config,
        },
    };

    const ROOT: &str = "Bearer hp15-root-secret";
    const PROJECT: &str = "Bearer hp15-project-secret";
    const SEC: &str = "Bearer hp15-sec-secret";
    const AB: &str = "Bearer hp15-ab-secret";
    const ROOT33: &str = "Bearer hp33-root-secret";
    const PROJECT33: &str = "Bearer hp33-project-secret";

    struct Harness {
        _temp: tempfile::TempDir,
        storage: Storage,
        app: Router,
    }

    async fn harness(configure: impl FnOnce(&mut Config)) -> Harness {
        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(PushAuth::Token);
        config.git.ssh_receive_pack = Some(false);
        config.views.enabled = true;
        config.views.max_concurrent_cold_starts = 8;
        config.git.push_tokens = [
            ("hp15-root", "/", "hp15-root-secret"),
            ("hp15-project", "/project", "hp15-project-secret"),
            ("hp15-sec", "/secret", "hp15-sec-secret"),
            ("hp15-ab", "/project/a/b", "hp15-ab-secret"),
        ]
        .into_iter()
        .map(|(name, path, token)| PushTokenConfig {
            name: name.to_owned(),
            token: token.to_owned(),
            paths: Some(vec![path.to_owned()]),
        })
        .collect();
        configure(&mut config);
        let storage = test_storage_with_config(temp.path(), config).await;
        let state = MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").unwrap(),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .unwrap(),
                prefix: "hp15-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage: storage.clone(),
        };
        let app = OpenApiRouter::new()
            .nest("/api/v1", routers())
            .split_for_parts()
            .0
            .with_state(state);
        Harness {
            _temp: temp,
            storage,
            app,
        }
    }

    async fn harness33(configure: impl FnOnce(&mut Config)) -> Harness {
        harness(|config| {
            config.git.push_tokens = [
                ("hp33-root", "/", "hp33-root-secret"),
                ("hp33-project", "/project", "hp33-project-secret"),
            ]
            .into_iter()
            .map(|(name, path, token)| PushTokenConfig {
                name: name.to_owned(),
                token: token.to_owned(),
                paths: Some(vec![path.to_owned()]),
            })
            .collect();
            configure(config);
        })
        .await
    }

    async fn get(harness: &Harness, uri: &str) -> (StatusCode, Value) {
        let response = harness
            .app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn register_wait(
        harness: &Harness,
        filter_spec: &str,
        auth: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/views?wait=true")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let response = harness
            .app
            .clone()
            .oneshot(
                request
                    .body(Body::from(json!({"filter_spec": filter_spec}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, serde_json::from_slice(&bytes).unwrap())
    }

    async fn send(
        harness: &Harness,
        body: Body,
        content_type: Option<&str>,
        auth: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request = Request::builder().method("POST").uri("/api/v1/views");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let response = harness
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, serde_json::from_slice(&bytes).unwrap())
    }

    async fn register(
        harness: &Harness,
        filter_spec: &str,
        name: Option<&str>,
        auth: Option<&str>,
    ) -> (StatusCode, HeaderMap, Value) {
        send(
            harness,
            Body::from(json!({"filter_spec": filter_spec, "name": name}).to_string()),
            Some("application/json"),
            auth,
        )
        .await
    }

    async fn snapshot(storage: &Storage) -> Vec<Vec<Value>> {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let mut result = Vec::new();
        for table in ["mega_view_filter", "mega_view", "mega_view_register_log"] {
            let rows = db
                .query_all_raw(Statement::from_string(
                    DbBackend::Postgres,
                    format!("SELECT to_jsonb(t) AS row FROM {table} t ORDER BY id"),
                ))
                .await
                .unwrap();
            result.push(
                rows.into_iter()
                    .map(|row| row.try_get("", "row").unwrap())
                    .collect(),
            );
        }
        result
    }

    fn assert_response(status: StatusCode, body: &Value, expected: StatusCode) {
        assert_eq!(status, expected, "{body}");
        assert_eq!(body["req_result"], expected == StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn register_success_response() {
        let h = harness(|_| {}).await;
        let (status, _, first) = register(&h, ":/project/a", None, Some(ROOT)).await;
        assert_response(status, &first, StatusCode::OK);
        let canonical = parse_for_registration(":/project/a").unwrap();
        assert_eq!(first["data"]["filter_id"], canonical.filter_id);
        assert!(first["data"]["name"].is_null());
        assert!(first["data"]["version"].is_null());
        assert_eq!(first["data"]["ready"], false);
        let before = snapshot(&h.storage).await;
        let (status, _, idem) = register(&h, ":/project/a/", None, Some(ROOT)).await;
        assert_response(status, &idem, StatusCode::OK);
        assert_eq!(idem["data"]["filter_id"], first["data"]["filter_id"]);
        assert_eq!(snapshot(&h.storage).await, before);

        let (status, _, named) =
            register(&h, ":/project/b", Some("agent/task-1"), Some(ROOT)).await;
        assert_response(status, &named, StatusCode::OK);
        assert_eq!(named["data"]["name"], "agent/task-1");
        assert_eq!(named["data"]["version"], 1);
        assert_eq!(named["data"]["ready"], false);
        let named_snapshot = snapshot(&h.storage).await;
        let (status, _, named_idem) =
            register(&h, ":/project/b", Some("agent/task-1"), Some(ROOT)).await;
        assert_response(status, &named_idem, StatusCode::OK);
        assert_eq!(named_idem["data"]["version"], 1);
        assert_eq!(snapshot(&h.storage).await, named_snapshot);
        let (status, _, next_version) =
            register(&h, ":/project/c", Some("agent/task-1"), Some(ROOT)).await;
        assert_response(status, &next_version, StatusCode::OK);
        assert_eq!(next_version["data"]["version"], 2);

        h.storage
            .view_storage()
            .get_connection()
            .execute_unprepared(&format!(
                "UPDATE mega_view_filter SET projected_seq = 1, ready_seq = 1, warming_since = NULL WHERE filter_id = '{}'",
                canonical.filter_id
            ))
            .await
            .unwrap();
        let (status, _, ready) = register(&h, ":/project/a", None, Some(ROOT)).await;
        assert_response(status, &ready, StatusCode::OK);
        assert_eq!(ready["data"]["ready"], true);
    }

    #[tokio::test]
    async fn register_validation_400() {
        let h = harness(|_| {}).await;
        let mut rules = HashMap::<String, String>::new();
        let simple = json!({"filter_spec": ":/project/a"}).to_string();
        let body_cases = [
            (
                simple.as_str(),
                Some("text/plain"),
                "application/json content type is required",
            ),
            ("{", Some("application/json"), "JSON syntax is invalid"),
            ("{}", Some("application/json"), "JSON fields are invalid"),
            (
                r#"{"filter_spec":1}"#,
                Some("application/json"),
                "JSON fields are invalid",
            ),
        ];
        for (body, content_type, rule) in body_cases {
            for auth in [None, Some(ROOT)] {
                let (status, _, result) =
                    send(&h, Body::from(body.to_owned()), content_type, auth).await;
                assert_response(status, &result, StatusCode::BAD_REQUEST);
                assert_eq!(result["err_message"], format!("request body: {rule}"));
                rules.insert(format!("body:{rule}"), rule.to_owned());
            }
        }
        let nested = format!("{}:/project/a{}", ":[".repeat(17), "]".repeat(17));
        let deep = format!("{}:/project/a", ":[".repeat(8000));
        let members = format!(":[{}]", vec![":/a"; 65].join(","));
        let selectors = format!(
            ":exclude[{}]",
            (0..257)
                .map(|index| format!("::s{index}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        for spec in [
            ":/project//a",
            ":nop",
            ":empty",
            ":prefix=x:[:/w:prefix=b,:/z:prefix=a]",
            ":[:/a:prefix=x,:/a/b:prefix=y]",
            ":/third-party/x",
            ":/.view/x",
            &"x".repeat(16_385),
            &nested,
            &deep,
            &members,
            &selectors,
        ] {
            let kind = match parse_for_registration(spec) {
                Err(error) => format!("parse:{error:?}"),
                Ok(canonical) => format!(
                    "validation:{:?}",
                    validate_for_registration(&canonical.filter, &h.storage.config().monorepo)
                        .unwrap_err()
                ),
            };
            for auth in [None, Some(ROOT)] {
                let (status, _, result) = register(&h, spec, None, auth).await;
                assert_response(status, &result, StatusCode::BAD_REQUEST);
                let message = result["err_message"].as_str().unwrap();
                let rule = message.strip_prefix("filter_spec: ").unwrap();
                if let Some(previous) = rules.insert(kind.clone(), rule.to_owned()) {
                    assert_eq!(rule, previous, "{spec}");
                }
                if spec == members {
                    assert!(rule.contains("64") && rule.contains("65"), "{rule}");
                }
                if spec == selectors {
                    assert!(rule.contains("256") && rule.contains("257"), "{rule}");
                }
            }
        }
        for name in [
            "", "a//b", "/a", "a/", "a/./b", "a/../b", "a b", "a@1", "a/b.git", "视图",
        ] {
            let kind = format!("name:{:?}", validate_view_name(name).unwrap_err());
            for auth in [None, Some(ROOT)] {
                let (status, _, result) = register(&h, ":/project/a", Some(name), auth).await;
                assert_response(status, &result, StatusCode::BAD_REQUEST);
                let rule = result["err_message"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("name: ")
                    .unwrap();
                if let Some(previous) = rules.insert(kind.clone(), rule.to_owned()) {
                    assert_eq!(rule, previous, "{name}");
                }
            }
        }
        let (status, _, valid_middle) =
            register(&h, ":/project/a", Some("a.git/b"), Some(ROOT)).await;
        assert_response(status, &valid_middle, StatusCode::OK);
        let sha256 =
            harness(|config| config.monorepo.object_format = MonoObjectFormat::Sha256).await;
        for auth in [None, Some(ROOT)] {
            let (status, _, result) = register(&sha256, ":/project/a", None, auth).await;
            assert_response(status, &result, StatusCode::BAD_REQUEST);
            assert!(
                result["err_message"]
                    .as_str()
                    .unwrap()
                    .starts_with("object_format:")
            );
            rules.insert(
                "object_format".to_owned(),
                result["err_message"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("object_format: ")
                    .unwrap()
                    .to_owned(),
            );
        }
        let mut seen = HashMap::<&str, &str>::new();
        for (kind, rule) in &rules {
            if let Some(other_kind) = seen.insert(rule, kind) {
                panic!("{kind} and {other_kind} share rule {rule}");
            }
        }
    }

    #[tokio::test]
    async fn register_body_limit_400() {
        let h = harness(|_| {}).await;
        let make_body = |size: usize| {
            let prefix = r#"{"filter_spec":""#;
            let suffix = r#""}"#;
            format!(
                "{prefix}{}{suffix}",
                "x".repeat(size - prefix.len() - suffix.len())
            )
        };
        let oversized = make_body(65_537);
        assert_eq!(oversized.len(), 65_537);
        let (status, _, response) = send(
            &h,
            Body::from(oversized.clone()),
            Some("application/json"),
            Some(ROOT),
        )
        .await;
        assert_response(status, &response, StatusCode::BAD_REQUEST);
        assert!(
            response["err_message"]
                .as_str()
                .unwrap()
                .starts_with("request body:")
        );
        assert!(response["err_message"].as_str().unwrap().contains("65536"));

        let middle = oversized.len() / 2;
        let chunks = [
            Ok::<_, Infallible>(bytes::Bytes::copy_from_slice(
                &oversized.as_bytes()[..middle],
            )),
            Ok(bytes::Bytes::copy_from_slice(
                &oversized.as_bytes()[middle..],
            )),
        ];
        let (status, _, streamed) = send(
            &h,
            Body::from_stream(stream::iter(chunks)),
            Some("application/json"),
            Some(ROOT),
        )
        .await;
        assert_response(status, &streamed, StatusCode::BAD_REQUEST);
        assert_eq!(streamed["err_message"], response["err_message"]);
        let (status, _, anonymous) =
            send(&h, Body::from(oversized), Some("application/json"), None).await;
        assert_response(status, &anonymous, StatusCode::BAD_REQUEST);
        assert_eq!(anonymous["err_message"], response["err_message"]);

        let boundary = make_body(65_536);
        assert_eq!(boundary.len(), 65_536);
        let (status, _, response) = send(
            &h,
            Body::from(boundary),
            Some("application/json"),
            Some(ROOT),
        )
        .await;
        assert_response(status, &response, StatusCode::BAD_REQUEST);
        assert!(
            response["err_message"]
                .as_str()
                .unwrap()
                .starts_with("filter_spec:")
        );
    }

    #[tokio::test]
    async fn register_authorization() {
        let h = harness(|_| {}).await;
        for auth in [
            None,
            Some("Bearer hp15-wrong-secret"),
            Some("Bearer hp15-root"),
        ] {
            let (status, _, response) = register(&h, ":/project/x", None, auth).await;
            assert_response(status, &response, StatusCode::UNAUTHORIZED);
            assert_eq!(response["err_message"], "authentication required");
        }
        for auth in [Some(SEC), Some(PROJECT)] {
            let (status, _, response) = register(&h, ":exclude[::secret]", None, auth).await;
            assert_response(status, &response, StatusCode::FORBIDDEN);
            assert_eq!(
                response["err_message"],
                "token is not authorized for path /"
            );
        }
        let (status, _, response) = register(&h, ":exclude[::secret]", None, Some(ROOT)).await;
        assert_response(status, &response, StatusCode::OK);

        let composed = ":/project/a:[:/b:prefix=x,:/c:prefix=y]";
        let (status, _, response) = register(&h, composed, None, Some(AB)).await;
        assert_response(status, &response, StatusCode::FORBIDDEN);
        assert_eq!(
            response["err_message"],
            "token is not authorized for path /project/a/c"
        );
        let (status, _, response) = register(&h, composed, None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::OK);

        let denied = harness(|config| config.git.push_auth = Some(PushAuth::None)).await;
        let (status, _, response) = register(&denied, ":/project/a", None, None).await;
        assert_response(status, &response, StatusCode::FORBIDDEN);
        assert_eq!(
            response["err_message"],
            "anonymous registration is disabled"
        );
        let allowed = harness(|config| {
            config.git.push_auth = Some(PushAuth::None);
            config.views.allow_anonymous_register = true;
        })
        .await;
        let (status, _, response) = register(&allowed, ":/project/a", None, None).await;
        assert_response(status, &response, StatusCode::OK);
    }

    #[tokio::test]
    async fn rejected_registrations_write_nothing() {
        let h = harness(|config| config.views.register_rate_per_token = 1).await;
        let invalid_cases = [
            (
                Body::from(json!({"filter_spec": ":nop"}).to_string()),
                Some("application/json"),
                None,
                StatusCode::BAD_REQUEST,
            ),
            (
                Body::from("{"),
                Some("application/json"),
                Some(PROJECT),
                StatusCode::BAD_REQUEST,
            ),
            (
                Body::from(json!({"filter_spec": ":/project/x", "name": "a@1"}).to_string()),
                Some("application/json"),
                Some(PROJECT),
                StatusCode::BAD_REQUEST,
            ),
            (
                Body::from("x".repeat(65_537)),
                Some("application/json"),
                Some(PROJECT),
                StatusCode::BAD_REQUEST,
            ),
            (
                Body::from(json!({"filter_spec": ":/project/x"}).to_string()),
                Some("application/json"),
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (
                Body::from(json!({"filter_spec": ":/project/x"}).to_string()),
                Some("application/json"),
                Some(SEC),
                StatusCode::FORBIDDEN,
            ),
        ];
        for (index, (body, content_type, auth, expected)) in invalid_cases.into_iter().enumerate() {
            let before = snapshot(&h.storage).await;
            let (status, _, response) = send(&h, body, content_type, auth).await;
            assert_response(status, &response, expected);
            if index == 3 {
                assert_eq!(
                    response["err_message"],
                    "request body: body exceeds 65536 bytes"
                );
            }
            assert_eq!(snapshot(&h.storage).await, before);
        }
        let (status, _, response) = register(&h, ":/project/x", None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::OK);
        let before = snapshot(&h.storage).await;
        let (status, _, response) = register(&h, ":/project/y", None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(snapshot(&h.storage).await, before);

        let anonymous = harness(|config| config.git.push_auth = Some(PushAuth::None)).await;
        let before = snapshot(&anonymous.storage).await;
        assert!(before.iter().all(Vec::is_empty));
        let (status, _, response) = register(&anonymous, ":/project/a", None, None).await;
        assert_response(status, &response, StatusCode::FORBIDDEN);
        assert_eq!(snapshot(&anonymous.storage).await, before);

        let failed_admit = harness(|config| config.views.register_rate_per_token = 1).await;
        failed_admit
            .storage
            .view_storage()
            .get_connection()
            .execute_unprepared("ALTER TABLE mega_view RENAME COLUMN created_by TO hp_gone")
            .await
            .unwrap();
        let before = snapshot(&failed_admit.storage).await;
        let (status, _, response) = register(
            &failed_admit,
            ":/project/a",
            Some("agent/task-1"),
            Some(ROOT),
        )
        .await;
        assert_response(status, &response, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(snapshot(&failed_admit.storage).await, before);
    }

    #[tokio::test]
    async fn admission_rejection_429() {
        let cases = [(1, 8, 10, false), (8, 1, 10, false), (8, 8, 1, true)];
        for (max_filters, cold_slots, rate, rate_case) in cases {
            let h = harness(|config| {
                config.views.max_filters = max_filters;
                config.views.max_concurrent_cold_starts = cold_slots;
                config.views.register_rate_per_token = rate;
            })
            .await;
            let (status, _, first) = register(&h, ":/project/a", None, Some(ROOT)).await;
            assert_response(status, &first, StatusCode::OK);
            let (status, headers, second) = register(&h, ":/project/b", None, Some(ROOT)).await;
            assert_response(status, &second, StatusCode::TOO_MANY_REQUESTS);
            let retry: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
            if rate_case {
                assert!((3590..=3600).contains(&retry), "{retry}");
                assert_eq!(second["err_message"], "view registration rate exceeded");
            } else {
                assert_eq!(retry, 30);
            }
        }
    }

    #[tokio::test]
    async fn register_mode_counts_rate_on_recycled_filter() {
        let h = harness(|config| config.views.register_rate_per_token = 2).await;
        let canonical = parse_for_registration(":/project/a").unwrap();
        let check =
            validate_for_registration(&canonical.filter, &h.storage.config().monorepo).unwrap();
        let (status, _, response) = register(&h, ":/project/a", None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::OK);
        let rows = snapshot(&h.storage).await;
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[2].len(), 1);
        assert_eq!(rows[0][0]["canonical_spec"], canonical.canonical_text);
        assert_eq!(rows[0][0]["algo_version"], 1);
        assert_eq!(rows[0][0]["object_format"], "sha1");
        assert_eq!(rows[0][0]["src_paths"], json!(check.src_paths));
        assert_eq!(rows[0][0]["push_enabled"], check.push_enabled);
        assert_eq!(rows[2][0]["requester"], "hp15-project");
        let (_, _, idem) = register(&h, ":/project/a", None, Some(PROJECT)).await;
        assert_eq!(idem["req_result"], true);
        assert_eq!(snapshot(&h.storage).await[2].len(), 1);

        let db = h.storage.view_storage().get_connection().clone();
        let recycle = || async {
            db.execute_unprepared(&format!(
                "UPDATE mega_view_filter SET projected_seq = 0, ready_seq = NULL, warming_since = NULL WHERE filter_id = '{}'",
                canonical.filter_id
            ))
            .await
            .unwrap();
        };
        recycle().await;
        let (status, _, response) = register(&h, ":/project/a", None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::OK);
        let rows = snapshot(&h.storage).await;
        assert!(!rows[0][0]["warming_since"].is_null());
        assert_eq!(
            rows[0]
                .iter()
                .filter(|row| !row["warming_since"].is_null())
                .count(),
            1
        );
        assert_eq!(rows[2].len(), 2);
        recycle().await;
        let (status, _, response) = register(&h, ":/project/a", None, Some(PROJECT)).await;
        assert_response(status, &response, StatusCode::TOO_MANY_REQUESTS);
        let rows = snapshot(&h.storage).await;
        assert!(rows[0][0]["warming_since"].is_null());
        assert_eq!(rows[2].len(), 2);
        let (status, _, response) = register(&h, ":/project/a", None, Some(ROOT)).await;
        assert_response(status, &response, StatusCode::OK);
        let rows = snapshot(&h.storage).await;
        assert_eq!(rows[2].len(), 3);
        assert_eq!(
            rows[2]
                .iter()
                .filter(|row| row["requester"] == "hp15-root")
                .count(),
            1
        );
        let (status, _, response) =
            register(&h, ":/project/n", Some("agent/task-1"), Some(ROOT)).await;
        assert_response(status, &response, StatusCode::OK);
        let rows = snapshot(&h.storage).await;
        assert_eq!(rows[1][0]["created_by"], "hp15-root");
        assert_eq!(rows[2].len(), 4);
        assert_eq!(
            rows[2]
                .iter()
                .filter(|row| row["requester"] == "hp15-project")
                .count(),
            2
        );
        assert_eq!(
            rows[2]
                .iter()
                .filter(|row| row["requester"] == "hp15-root")
                .count(),
            2
        );
        for row in &rows[2] {
            assert!(matches!(
                row["requester"].as_str(),
                Some("hp15-project" | "hp15-root")
            ));
        }
        let anonymous = harness(|config| {
            config.git.push_auth = Some(PushAuth::None);
            config.views.allow_anonymous_register = true;
        })
        .await;
        let (status, _, response) = register(&anonymous, ":/project/a", None, None).await;
        assert_response(status, &response, StatusCode::OK);
        assert_eq!(
            snapshot(&anonymous.storage).await[2][0]["requester"],
            "anonymous"
        );
    }

    #[tokio::test]
    async fn get_by_filter_id_fields() {
        let h = harness33(|_| {}).await;
        for spec in [
            ":/a",
            ":exclude[::secret]",
            ":/a:[:/b:prefix=x,:/c:prefix=y]",
        ] {
            let (status, _, registered) = register(&h, spec, None, Some(ROOT33)).await;
            assert_response(status, &registered, StatusCode::OK);
            let filter_id = registered["data"]["filter_id"].as_str().unwrap();
            let (status, response) = get(&h, &format!("/api/v1/views/{filter_id}")).await;
            assert_response(status, &response, StatusCode::OK);
            let data = response["data"].as_object().unwrap();
            let mut keys = data.keys().map(String::as_str).collect::<Vec<_>>();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "canonical_spec",
                    "filter_id",
                    "lag_commits",
                    "projected_seq",
                    "push_enabled",
                    "ready",
                    "src_paths",
                ]
            );
            let canonical = parse_for_registration(spec).unwrap();
            let check =
                validate_for_registration(&canonical.filter, &h.storage.config().monorepo).unwrap();
            assert_eq!(data["filter_id"], filter_id);
            assert_eq!(data["canonical_spec"], canonical.canonical_text);
            assert_eq!(data["src_paths"], json!(check.src_paths));
            assert_eq!(data["push_enabled"], check.push_enabled);
            assert_eq!(data["ready"], false);
            assert_eq!(data["projected_seq"], 0);
            assert!(data["lag_commits"].is_null() || data["lag_commits"].is_i64());
            if spec == ":exclude[::secret]" {
                assert_eq!(data["src_paths"], json!(["/"]));
                assert_eq!(data["push_enabled"], false);
            }
            h.storage
                .view_storage()
                .get_connection()
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE mega_view_filter SET projected_seq = 1, ready_seq = 1 WHERE filter_id = $1",
                    [sea_orm::Value::from(filter_id.to_owned())],
                ))
                .await
                .unwrap();
            let (status, response) = get(&h, &format!("/api/v1/views/{filter_id}")).await;
            assert_response(status, &response, StatusCode::OK);
            assert_eq!(response["data"]["ready"], true);
            assert_eq!(response["data"]["projected_seq"], 1);
        }
        for filter_id in ["0".repeat(64), "not-a-filter-id".to_owned()] {
            let (status, response) = get(&h, &format!("/api/v1/views/{filter_id}")).await;
            assert_response(status, &response, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn get_by_name_version() {
        let h = harness33(|_| {}).await;
        let mut ids = Vec::new();
        for spec in [":/project/a", ":/project/b"] {
            let (status, _, response) =
                register(&h, spec, Some("agent/task-1"), Some(ROOT33)).await;
            assert_response(status, &response, StatusCode::OK);
            ids.push(response["data"]["filter_id"].as_str().unwrap().to_owned());
        }
        for (uri, id, version) in [
            ("/api/v1/views?name=agent%2Ftask-1", &ids[1], 2),
            ("/api/v1/views?name=agent%2Ftask-1&version=1", &ids[0], 1),
        ] {
            let (status, response) = get(&h, uri).await;
            assert_response(status, &response, StatusCode::OK);
            assert_eq!(
                response["data"],
                json!({"filter_id": id, "name": "agent/task-1", "version": version})
            );
        }
        for uri in [
            "/api/v1/views?name=no-such-view",
            "/api/v1/views?name=agent%2Ftask-1&version=3",
        ] {
            let (status, response) = get(&h, uri).await;
            assert_response(status, &response, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn metrics_snapshot_unauthenticated() {
        let h = harness33(|_| {}).await;
        let (status, _, response) = register(&h, ":/project/a", None, Some(ROOT33)).await;
        assert_response(status, &response, StatusCode::OK);
        let (status, response) = get(&h, "/api/v1/views/metrics").await;
        assert_response(status, &response, StatusCode::OK);
        let expected = h
            .storage
            .view_projection_service()
            .metrics_snapshot()
            .await
            .unwrap();
        assert_eq!(response["data"], serde_json::to_value(expected).unwrap());
        assert_eq!(response["data"]["view_cold_start_slots_in_use"], 1);
        h.storage.view_metrics().increment_batch_premise_failures();
        let (status, response) = get(&h, "/api/v1/views/metrics").await;
        assert_response(status, &response, StatusCode::OK);
        assert_eq!(response["data"]["view_batch_premise_failures_total"], 1);
    }

    async fn drain_signal(storage: &Storage) {
        let _ =
            tokio::time::timeout(Duration::from_millis(10), storage.view_signal().notified()).await;
    }

    async fn assert_admitted_signal(
        h: &Harness,
        spec: &str,
        name: Option<&str>,
        wait: bool,
        expected_view: Option<(&str, i32)>,
        expect_warming: bool,
    ) -> Value {
        drain_signal(&h.storage).await;
        let filter_id = parse_for_registration(spec).unwrap().filter_id;
        let signal = h.storage.view_signal();
        let committed = async {
            if expect_warming || expected_view.is_some() {
                Some(
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            let row = h
                                .storage
                                .view_storage()
                                .get_filter_by_filter_id(&filter_id)
                                .await
                                .unwrap();
                            let filter_committed = row
                                .as_ref()
                                .is_some_and(|row| !expect_warming || row.warming_since.is_some());
                            let view_committed = if let Some((name, version)) = expected_view {
                                h.storage
                                    .view_storage()
                                    .find_view_by_name(name, Some(version))
                                    .await
                                    .unwrap()
                                    .is_some_and(|view| view.filter_id == filter_id)
                            } else {
                                true
                            };
                            if filter_committed && view_committed {
                                break tokio::time::Instant::now();
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .unwrap(),
                )
            } else {
                None
            }
        };
        let observed = async {
            let deadline = if expect_warming || expected_view.is_some() {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(1)
            };
            tokio::time::timeout(deadline, signal.notified())
                .await
                .unwrap();
            let woke_at = tokio::time::Instant::now();
            let row = h
                .storage
                .view_storage()
                .get_filter_by_filter_id(&filter_id)
                .await
                .unwrap()
                .unwrap();
            if expect_warming {
                assert!(row.warming_since.is_some());
            }
            if let Some((name, version)) = expected_view {
                let view = h
                    .storage
                    .view_storage()
                    .find_view_by_name(name, Some(version))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(view.filter_pk, row.id);
                assert_eq!(view.filter_id, filter_id);
                assert_eq!(view.version, version);
            }
            woke_at
        };
        let (response, woke_at, committed_at) = if wait {
            tokio::join!(register_wait(h, spec, Some(ROOT33)), observed, committed)
        } else {
            tokio::join!(register(h, spec, name, Some(ROOT33)), observed, committed)
        };
        if let Some(committed_at) = committed_at {
            assert!(woke_at <= committed_at + Duration::from_secs(1));
        }
        assert_response(response.0, &response.2, StatusCode::OK);
        response.2
    }

    #[tokio::test]
    async fn wait_true_blocks_until_ready_or_timeout() {
        let h = harness33(|_| {}).await;
        drain_signal(&h.storage).await;
        let signal = h.storage.view_signal();
        let storage = h.storage.clone();
        let filter_id = parse_for_registration(":/project/ready").unwrap().filter_id;
        let updater = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), signal.notified())
                .await
                .unwrap();
            let result = storage
                .view_storage()
                .get_connection()
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE mega_view_filter SET projected_seq = 1, ready_seq = 1, warming_since = NULL WHERE filter_id = $1",
                    [sea_orm::Value::from(filter_id)],
                ))
                .await
                .unwrap();
            assert_eq!(result.rows_affected(), 1);
            tokio::time::Instant::now()
        });
        let (status, _, ready) = register_wait(&h, ":/project/ready", Some(ROOT33)).await;
        assert_response(status, &ready, StatusCode::OK);
        let updated_at = updater.await.unwrap();
        assert_eq!(ready["data"]["ready"], true);
        assert_eq!(
            ready["data"]["filter_id"],
            parse_for_registration(":/project/ready").unwrap().filter_id
        );
        assert!(updated_at.elapsed() <= Duration::from_secs(1));

        let started = tokio::time::Instant::now();
        let (status, _, timed_out) = register_wait(&h, ":/project/timeout", Some(ROOT33)).await;
        let elapsed = started.elapsed();
        assert_response(status, &timed_out, StatusCode::OK);
        assert_eq!(timed_out["data"]["ready"], false);
        assert!(elapsed >= REGISTER_WAIT_TIMEOUT);
        assert!(elapsed <= REGISTER_WAIT_TIMEOUT + Duration::from_secs(2));
        let (status, _, without_wait) = register(&h, ":/project/timeout", None, Some(ROOT33)).await;
        assert_response(status, &without_wait, StatusCode::OK);
        assert_eq!(timed_out["data"], without_wait["data"]);

        let (status, _, without_wait) = register(&h, ":/project/ready", None, Some(ROOT33)).await;
        assert_response(status, &without_wait, StatusCode::OK);
        assert_eq!(ready["data"], without_wait["data"]);

        let (status, _, immediate) = register(&h, ":/project/ready", None, Some(ROOT33)).await;
        assert_response(status, &immediate, StatusCode::OK);
        let started = tokio::time::Instant::now();
        let (status, _, already_ready) = register_wait(&h, ":/project/ready", Some(ROOT33)).await;
        assert_response(status, &already_ready, StatusCode::OK);
        assert_eq!(already_ready["data"], immediate["data"]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn signal_after_admission_commit() {
        let h = harness33(|_| {}).await;
        let first = assert_admitted_signal(&h, ":/project/a", None, false, None, true).await;
        let filter_id = first["data"]["filter_id"].as_str().unwrap();
        assert_admitted_signal(&h, ":/project/a", None, false, None, false).await;
        assert_admitted_signal(
            &h,
            ":/project/a",
            Some("agent/task-1"),
            false,
            Some(("agent/task-1", 1)),
            false,
        )
        .await;
        assert_admitted_signal(
            &h,
            ":/project/b",
            Some("agent/task-1"),
            false,
            Some(("agent/task-1", 2)),
            true,
        )
        .await;

        h.storage
            .view_storage()
            .get_connection()
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE mega_view_filter SET projected_seq = 0, ready_seq = NULL, warming_since = NULL WHERE filter_id = $1",
                [sea_orm::Value::from(filter_id.to_owned())],
            ))
            .await
            .unwrap();
        assert_admitted_signal(&h, ":/project/a", None, false, None, true).await;
        let waiting = harness33(|_| {}).await;
        assert_admitted_signal(&waiting, ":/project/wait", None, true, None, true).await;

        let signal = h.storage.view_signal();

        let invalid = [
            (":[", Some(ROOT33), StatusCode::BAD_REQUEST),
            (":/project/x", None, StatusCode::UNAUTHORIZED),
            (":/a", Some(PROJECT33), StatusCode::FORBIDDEN),
        ];
        for (spec, auth, expected) in invalid {
            drain_signal(&h.storage).await;
            let (status, _, response) = register(&h, spec, None, auth).await;
            assert_response(status, &response, expected);
            assert!(
                tokio::time::timeout(Duration::from_millis(300), signal.notified())
                    .await
                    .is_err()
            );
        }

        let rejected = harness33(|config| config.views.max_concurrent_cold_starts = 1).await;
        let (status, _, response) = register(&rejected, ":/project/a", None, Some(ROOT33)).await;
        assert_response(status, &response, StatusCode::OK);
        drain_signal(&rejected.storage).await;
        let (status, _, response) = register(&rejected, ":/project/b", None, Some(ROOT33)).await;
        assert_response(status, &response, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(300),
                rejected.storage.view_signal().notified()
            )
            .await
            .is_err()
        );

        let failed = harness33(|_| {}).await;
        failed
            .storage
            .view_storage()
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_register_log RENAME COLUMN requester TO hp_gone",
            )
            .await
            .unwrap();
        drain_signal(&failed.storage).await;
        let (status, _, response) = register(&failed, ":/project/a", None, Some(ROOT33)).await;
        assert_response(status, &response, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(300),
                failed.storage.view_signal().notified()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn lag_commits_definition() {
        async fn setup(
            chain: bool,
            length: usize,
            projected_seq: i64,
        ) -> (
            Harness,
            Vec<crate::jupiter::storage::view_test_fixtures::RootCommitFixture>,
            String,
        ) {
            let h = harness33(|config| config.views.max_append_walk = 3).await;
            let (status, _, response) = register(&h, ":/project/a", None, Some(ROOT33)).await;
            assert_response(status, &response, StatusCode::OK);
            let filter_id = response["data"]["filter_id"].as_str().unwrap().to_owned();
            h.storage
                .view_storage()
                .get_connection()
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "UPDATE mega_view_filter SET projected_seq = $1 WHERE filter_id = $2",
                    [
                        sea_orm::Value::from(projected_seq),
                        sea_orm::Value::from(filter_id.clone()),
                    ],
                ))
                .await
                .unwrap();
            let db = h.storage.view_storage().get_connection().clone();
            let history = seed_linear_root_history(&db, length).await;
            if chain {
                h.storage
                    .view_storage()
                    .extend_root_chain(None, 5, ViewLockMode::Try)
                    .await
                    .unwrap();
            }
            (h, history, filter_id)
        }

        async fn assert_lag(h: &Harness, filter_id: &str, expected: Option<i64>) {
            let (status, response) = get(h, &format!("/api/v1/views/{filter_id}")).await;
            assert_response(status, &response, StatusCode::OK);
            assert_eq!(response["data"]["lag_commits"], json!(expected));
        }

        let (h, history, id) = setup(true, 5, 2).await;
        assert_lag(&h, &id, Some(3)).await;
        let db = h.storage.view_storage().get_connection().clone();
        db.execute_unprepared("UPDATE mega_view_filter SET projected_seq = 5")
            .await
            .unwrap();
        assert_lag(&h, &id, Some(0)).await;
        db.execute_unprepared("UPDATE mega_view_filter SET projected_seq = 2")
            .await
            .unwrap();
        let next = seed_single_parent_root_commit(&db, HashKind::Sha1, &history[4], "next").await;
        let tip = seed_single_parent_root_commit(&db, HashKind::Sha1, &next, "tip").await;
        assert!(cas_fixture_main(&db, &history[4], &tip).await);
        assert_lag(&h, &id, Some(5)).await;

        let (h, _, id) = setup(false, 3, 0).await;
        assert_lag(&h, &id, Some(3)).await;
        let (h, _, id) = setup(false, 4, 0).await;
        assert_lag(&h, &id, None).await;

        let (h, _, id) = setup(true, 5, 2).await;
        h.storage
            .view_storage()
            .get_connection()
            .execute_unprepared(
                "DELETE FROM mega_refs WHERE path = '/' AND ref_name = 'refs/heads/main'",
            )
            .await
            .unwrap();
        assert_lag(&h, &id, None).await;

        let (h, history, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        let mut tip = history[4].clone();
        for number in 0..4 {
            tip = seed_single_parent_root_commit(
                &db,
                HashKind::Sha1,
                &tip,
                &format!("extra-{number}"),
            )
            .await;
        }
        assert!(cas_fixture_main(&db, &history[4], &tip).await);
        assert_lag(&h, &id, None).await;

        let (h, history, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        assert!(cas_fixture_main(&db, &history[4], &history[2]).await);
        assert_lag(&h, &id, None).await;

        let (h, history, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        let fork = seed_single_parent_root_commit(&db, HashKind::Sha1, &history[2], "fork").await;
        set_fixture_main(
            &db,
            &fork.commit.id.to_string(),
            &fork.commit.tree_id.to_string(),
        )
        .await;
        assert_lag(&h, &id, None).await;

        let (h, _, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        let unrelated = seed_unrelated_root_history(&db, HashKind::Sha1).await;
        let unrelated_tip =
            seed_single_parent_root_commit(&db, HashKind::Sha1, &unrelated, "unrelated tip").await;
        set_fixture_main(
            &db,
            &unrelated_tip.commit.id.to_string(),
            &unrelated_tip.commit.tree_id.to_string(),
        )
        .await;
        assert_lag(&h, &id, None).await;

        let (h, history, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        let unrelated = seed_unrelated_root_history(&db, HashKind::Sha1).await;
        let merge =
            seed_multi_parent_root_commit(&db, HashKind::Sha1, &history[4], &unrelated).await;
        set_fixture_main(
            &db,
            &merge.commit.id.to_string(),
            &merge.commit.tree_id.to_string(),
        )
        .await;
        assert_lag(&h, &id, None).await;

        let (h, _, id) = setup(true, 5, 2).await;
        let db = h.storage.view_storage().get_connection().clone();
        let missing = seed_missing_first_parent_root_commit(
            &db,
            HashKind::Sha1,
            ObjectHash::from_str(&"f".repeat(40)).unwrap(),
        )
        .await;
        set_fixture_main(
            &db,
            &missing.commit.id.to_string(),
            &missing.commit.tree_id.to_string(),
        )
        .await;
        assert_lag(&h, &id, None).await;
    }
}
