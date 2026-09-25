//! ImportRepo leaf cleanup (plan-20260923 ADR-FU-10), mounted on the
//! storage-only surface only (DEFER-FU-02). The path is looked up by bound
//! equality inside the cleanup entry, never by longest-prefix resolution.

use axum::{Json, extract::State, http::HeaderMap};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::REPO_TAG, api_write_auth::authorize_import_repo_removal},
    ceres::{
        model::git::{ImportRepoRemoveOutcome, ImportRepoRemoveRequest, ImportRepoRemoveResult},
        pack::{
            import_repo::{RemoveOutcome, remove_import_repo},
            path_policy::strict_import_repo_leaf_input,
        },
    },
    common::errors::ApiError,
    contract::api::common::CommonResult,
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().routes(routes!(remove_import_repo_leaf))
}

/// Remove the ImportRepo at `path`, or continue cleanup `cleanup_id`
/// (plan-20260923 ADR-FU-10). Order: body parse, strict path syntax (400),
/// push-token authorization (fixed 401 / 403), then the cleanup entry, which
/// performs every lookup.
#[utoipa::path(
    post,
    path = "/import-repo/remove",
    request_body = ImportRepoRemoveRequest,
    responses(
        (status = 200, body = CommonResult<ImportRepoRemoveResult>, content_type = "application/json"),
        (status = 400, description = "IMPORT_REPO_PATH_INVALID (checked before authorization)", body = CommonResult<String>, content_type = "application/json"),
        (status = 401, description = "missing or unknown push token; fixed body `authentication required`", body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "push_auth is not `token`, or the token does not cover `path`; fixed body `forbidden`", body = CommonResult<String>, content_type = "application/json"),
        (status = 404, description = "IMPORT_REPO_CLEANUP_NOT_FOUND", body = CommonResult<String>, content_type = "application/json"),
        (status = 409, description = "IMPORT_REPO_HAS_CHILDREN; no ledger, audit, ref, object or tree write (a refusal by the write-queue re-check leaves one Failed queue row)", body = CommonResult<String>, content_type = "application/json"),
        (status = 500, description = "retryable (write queue paused, stopped or full; detach round not completed; storage error); body `Internal server error`", body = CommonResult<String>, content_type = "application/json")
    ),
    tag = REPO_TAG
)]
async fn remove_import_repo_leaf(
    state: State<MonoApiServiceState>,
    headers: HeaderMap,
    Json(request): Json<ImportRepoRemoveRequest>,
) -> Result<Json<CommonResult<ImportRepoRemoveResult>>, ApiError> {
    let config = state.storage.config();
    let path = strict_import_repo_leaf_input(&config.monorepo, &request.path)?;
    let requester = authorize_import_repo_removal(&config.git, &headers, &path)?;
    let outcome = remove_import_repo(
        &state.storage,
        state.git_object_cache.clone(),
        &path,
        Some(requester),
        request.cleanup_id,
    )
    .await?;
    Ok(Json(CommonResult::success(Some(remove_result(
        path, outcome,
    )))))
}

fn remove_result(path: String, outcome: RemoveOutcome) -> ImportRepoRemoveResult {
    match outcome {
        RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } => ImportRepoRemoveResult {
            path,
            outcome: ImportRepoRemoveOutcome::Removed,
            repo_id: Some(repo_id),
            cleanup_id: Some(cleanup_id),
        },
        RemoveOutcome::Pending {
            repo_id,
            cleanup_id,
        } => ImportRepoRemoveResult {
            path,
            outcome: ImportRepoRemoveOutcome::Pending,
            repo_id: Some(repo_id),
            cleanup_id: Some(cleanup_id),
        },
        RemoveOutcome::Absent => ImportRepoRemoveResult {
            path,
            outcome: ImportRepoRemoveOutcome::Absent,
            repo_id: None,
            cleanup_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    //! The request-level tests run against `Storage::mock()`, whose database
    //! connection is `Disconnected`: a sea-orm entity query on it panics and a
    //! raw one fails. A refusal answered inside the test task therefore proves
    //! that nothing reached the database before authorization, and a covering
    //! token proves it reaches the cleanup entry by panicking in a spawned task.

    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, Request, StatusCode, header},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;
    use crate::{
        api::{
            api_router,
            oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        },
        ceres::api_service::cache::GitObjectCache,
        config::{PushAuth, PushPolicy, PushTokenConfig, reload::ConfigHandle},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::storage::Storage,
    };

    const UNAUTHORIZED: &[u8] =
        br#"{"req_result":false,"data":null,"err_message":"authentication required"}"#;
    const FORBIDDEN: &[u8] = br#"{"req_result":false,"data":null,"err_message":"forbidden"}"#;
    const WIDE: &str = "Bearer fu20-wide-secret";
    const NARROW: &str = "Bearer fu20-narrow-secret";
    const LEAF: &str = "Bearer fu20-leaf-secret";

    fn mock_state(push_auth: Option<PushAuth>) -> MonoApiServiceState {
        let mut storage = Storage::mock();
        let mut config = (*storage.config()).clone();
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = push_auth;
        config.git.push_tokens = vec![
            PushTokenConfig {
                name: "fu20-narrow".to_owned(),
                token: "fu20-narrow-secret".to_owned(),
                paths: Some(vec!["/project".to_owned()]),
            },
            PushTokenConfig {
                name: "fu20-leaf".to_owned(),
                token: "fu20-leaf-secret".to_owned(),
                paths: Some(vec!["/third-party/fu20-leaf".to_owned()]),
            },
            PushTokenConfig {
                name: "fu20-wide".to_owned(),
                token: "fu20-wide-secret".to_owned(),
                paths: Some(vec!["/third-party".to_owned()]),
            },
        ];
        let config = Arc::new(config);
        storage.config_handle = ConfigHandle::from_arc(config.clone());
        storage.config = config;
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "fu20-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage,
        }
    }

    fn app(state: MonoApiServiceState) -> Router {
        OpenApiRouter::new()
            .nest("/api/v1", api_router::storage_only_routers_with(false))
            .split_for_parts()
            .0
            .with_state(state)
    }

    fn request(content_type: Option<&str>, auth: Option<&str>, body: String) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/import-repo/remove");
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(auth) = auth {
            builder = builder.header(header::AUTHORIZATION, auth);
        }
        builder.body(Body::from(body)).unwrap()
    }

    fn json_request(auth: Option<&str>, body: &Value) -> Request<Body> {
        request(Some("application/json"), auth, body.to_string())
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    #[test]
    fn import_repo_remove_openapi_contract() {
        let doc = serde_json::to_value(routers().split_for_parts().1).unwrap();
        let item = &doc["paths"]["/import-repo/remove"];
        let methods: Vec<&String> = item.as_object().unwrap().keys().collect();
        assert_eq!(methods, ["post"], "{item}");
        let post = &item["post"];
        let mut statuses: Vec<&String> = post["responses"].as_object().unwrap().keys().collect();
        statuses.sort();
        assert_eq!(
            statuses,
            ["200", "400", "401", "403", "404", "409", "500"],
            "{post}"
        );
        assert_eq!(post["tags"], json!([REPO_TAG]));
        let request_ref = post["requestBody"]["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap();
        assert!(
            request_ref.ends_with("/ImportRepoRemoveRequest"),
            "{request_ref}"
        );
        let schemas = &doc["components"]["schemas"];
        let request_schema = &schemas["ImportRepoRemoveRequest"];
        assert_eq!(request_schema["additionalProperties"], json!(false));
        assert_eq!(request_schema["required"], json!(["path"]));
        assert_eq!(
            schemas["ImportRepoRemoveOutcome"]["enum"],
            json!(["removed", "pending", "absent"])
        );
        let mut keys: Vec<&String> = schemas["ImportRepoRemoveResult"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        keys.sort();
        assert_eq!(keys, ["cleanup_id", "outcome", "path", "repo_id"]);
        let mut required: Vec<&str> = schemas["ImportRepoRemoveResult"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["cleanup_id", "outcome", "path", "repo_id"]);
    }

    #[test]
    fn import_repo_remove_result_wire_shape() {
        let path = || "/third-party/a".to_owned();
        for (outcome, wire) in [
            (
                RemoveOutcome::Removed {
                    repo_id: 9_007_199_254_740_993,
                    cleanup_id: 3,
                },
                r#"{"path":"/third-party/a","outcome":"removed","repo_id":9007199254740993,"cleanup_id":3}"#,
            ),
            (
                RemoveOutcome::Pending {
                    repo_id: 7,
                    cleanup_id: 3,
                },
                r#"{"path":"/third-party/a","outcome":"pending","repo_id":7,"cleanup_id":3}"#,
            ),
            (
                RemoveOutcome::Absent,
                r#"{"path":"/third-party/a","outcome":"absent","repo_id":null,"cleanup_id":null}"#,
            ),
        ] {
            let result = remove_result(path(), outcome);
            assert_eq!(serde_json::to_string(&result).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<ImportRepoRemoveResult>(wire).unwrap(),
                result
            );
        }
    }

    #[tokio::test]
    async fn import_repo_remove_validates_before_auth() {
        let invalid = [
            "/third-party",
            "/",
            "/project/x",
            "/third-partyx/y",
            "third-party/x",
            "/third-party/x/..",
            "/third-party/../project",
            "/third-party//x",
            "/third-party/x/",
            "/third-party/./x",
            " /third-party/x",
            "/third-party/x ",
            "/third-party/x\\y",
            "/third-party/x\0y",
            "",
        ];
        let stacks = [
            (Some(PushAuth::Token), None),
            (Some(PushAuth::Token), Some(WIDE)),
            (Some(PushAuth::None), None),
        ];
        for path in invalid {
            let mut bodies = Vec::new();
            for (push_auth, auth) in stacks.iter().cloned() {
                let (status, _, body) = send(
                    app(mock_state(push_auth)),
                    json_request(auth, &json!({ "path": path })),
                )
                .await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{path:?}");
                assert!(
                    body.starts_with(
                        br#"{"req_result":false,"data":null,"err_message":"IMPORT_REPO_PATH_INVALID: "#
                    ),
                    "{path:?}: {}",
                    String::from_utf8_lossy(&body)
                );
                bodies.push(body);
            }
            assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]), "{path:?}");
            let text = String::from_utf8(bodies.remove(0)).unwrap();
            match path {
                "/third-party" => assert_eq!(
                    text,
                    r#"{"req_result":false,"data":null,"err_message":"IMPORT_REPO_PATH_INVALID: \"/third-party\": the ImportRepo directory itself is not an ImportRepo; push to a path below it"}"#
                ),
                "/third-party//x" => {
                    assert!(
                        text.contains(r#"did you mean \"/third-party/x\""#),
                        "{text}"
                    )
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn import_repo_remove_authorizes_before_any_query() {
        let paths = [
            "/third-party/fu20-absent",
            "/third-party/fu20-leaf",
            "/third-party/fu20-parent",
        ];
        let bodies = |path: &str| {
            [
                json!({ "path": path }),
                json!({ "path": path, "cleanup_id": 12 }),
            ]
        };
        let refusals = [
            (
                Some(PushAuth::Token),
                None,
                StatusCode::UNAUTHORIZED,
                UNAUTHORIZED,
            ),
            (
                Some(PushAuth::Token),
                Some("Bearer wrong"),
                StatusCode::UNAUTHORIZED,
                UNAUTHORIZED,
            ),
            (
                Some(PushAuth::Token),
                Some("Basic dTp3cm9uZw=="),
                StatusCode::UNAUTHORIZED,
                UNAUTHORIZED,
            ),
            (
                Some(PushAuth::Token),
                Some(NARROW),
                StatusCode::FORBIDDEN,
                FORBIDDEN,
            ),
            (Some(PushAuth::None), None, StatusCode::FORBIDDEN, FORBIDDEN),
            (
                Some(PushAuth::None),
                Some(WIDE),
                StatusCode::FORBIDDEN,
                FORBIDDEN,
            ),
            (None, Some(WIDE), StatusCode::FORBIDDEN, FORBIDDEN),
        ];
        for (push_auth, auth, status, expected) in refusals {
            let mut seen = Vec::new();
            for path in paths {
                for body in bodies(path) {
                    let (got, headers, bytes) = send(
                        app(mock_state(push_auth.clone())),
                        json_request(auth, &body),
                    )
                    .await;
                    assert_eq!((got, bytes.as_slice()), (status, expected), "{body}");
                    assert!(headers.get(header::WWW_AUTHENTICATE).is_none());
                    seen.push((
                        headers.get(header::CONTENT_TYPE).cloned(),
                        headers.get(header::CONTENT_LENGTH).cloned(),
                    ));
                }
            }
            assert!(seen.windows(2).all(|pair| pair[0] == pair[1]), "{auth:?}");
        }

        // A token scoped to one leaf covers only that leaf.
        for path in ["/third-party/fu20-absent", "/third-party/fu20-parent"] {
            for body in bodies(path) {
                let (status, _, bytes) = send(
                    app(mock_state(Some(PushAuth::Token))),
                    json_request(Some(LEAF), &body),
                )
                .await;
                assert_eq!(
                    (status, bytes.as_slice()),
                    (StatusCode::FORBIDDEN, FORBIDDEN)
                );
            }
        }

        // Positive control: a covering token (the wide one, or one scoped to
        // exactly this leaf) reaches the cleanup entry, whose first lookup
        // (exact path or ledger row) panics on the mock database.
        for (auth, body) in [WIDE, LEAF]
            .into_iter()
            .flat_map(|auth| bodies("/third-party/fu20-leaf").map(|body| (auth, body)))
        {
            let app = app(mock_state(Some(PushAuth::Token)));
            let joined =
                tokio::spawn(async move { send(app, json_request(Some(auth), &body)).await }).await;
            let panic = joined.expect_err("the entry ran a query").into_panic();
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            assert!(message.contains("Disconnected"), "{message}");
        }
    }

    #[tokio::test]
    async fn import_repo_remove_extractor_rejections_precede_auth() {
        let token = || app(mock_state(Some(PushAuth::Token)));
        for (request, status) in [
            (
                request(None, None, r#"{"path":"/third-party/a"}"#.to_owned()),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                request(Some("application/json"), None, "{".to_owned()),
                StatusCode::BAD_REQUEST,
            ),
            (
                json_request(None, &json!({ "cleanup_id": 1 })),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json_request(
                    None,
                    &json!({ "path": "/third-party/a", "cleanup_id": "1" }),
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json_request(
                    None,
                    &json!({ "path": "/third-party/a", "cleanup_id": 1.5 }),
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json_request(None, &json!({ "path": "/third-party/a", "cleanupId": 1 })),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            let (got, _, body) = send(token(), request).await;
            assert_eq!(got, status, "{}", String::from_utf8_lossy(&body));
            assert!(!body.starts_with(br#"{"req_result""#));
        }
        let (got, _, body) = send(
            app(mock_state(Some(PushAuth::None))),
            json_request(
                None,
                &json!({ "path": "/third-party/a", "cleanup_id": null }),
            ),
        )
        .await;
        assert_eq!((got, body.as_slice()), (StatusCode::FORBIDDEN, FORBIDDEN));
    }

    /// The live HTTP surface: Review does not mount the route, only POST is
    /// routed, and the default body limit applies.
    #[tokio::test]
    async fn import_repo_remove_live_surface() {
        let review = OpenApiRouter::new()
            .nest("/api/v1", api_router::routers())
            .split_for_parts()
            .0
            .with_state(mock_state(Some(PushAuth::Token)));
        let (status, _, _) = send(
            review,
            json_request(Some(WIDE), &json!({ "path": "/third-party/a" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let get = Request::builder()
            .method("GET")
            .uri("/api/v1/import-repo/remove")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(app(mock_state(Some(PushAuth::Token))), get).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            headers
                .get(header::ALLOW)
                .and_then(|value| value.to_str().ok()),
            Some("POST")
        );

        let oversized = json!({ "path": format!("/third-party/{}", "a".repeat(2 * 1024 * 1024)) });
        let (status, _, body) = send(
            app(mock_state(Some(PushAuth::Token))),
            json_request(None, &oversized),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
}
