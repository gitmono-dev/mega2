use std::{net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use axum::{
    Router, ServiceExt,
    body::Body,
    extract::FromRef,
    http::{self, Request, Uri},
    middleware,
    response::Response,
    routing::any,
};
use http::{HeaderName, HeaderValue, Method};
use time::Duration;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tower::{Layer, ServiceBuilder};
use tower_http::{cors::CorsLayer, decompression::RequestDecompressionLayer, trace::TraceLayer};
use tower_sessions::{Expiry, MemoryStore, SessionManagerLayer};
use url::form_urlencoded;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    api::{
        MonoApiServiceState,
        api_doc::ApiDoc,
        api_router::{self},
        router::lfs_router,
    },
    bellatrix::Bellatrix,
    ceres::{
        api_service::{cache::GitObjectCache, state::ProtocolApiState},
        protocol::ServiceType,
    },
    common::errors::{MegaError, MegaResult, ProtocolError},
    config::{
        ArtifactGcConfig, BuckConfig, Config,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    context::AppContext,
    contract::{
        git_protocol::InfoRefsParams,
        policy::{entitystore::EntityStore, guard::cedar_guard::cedar_guard},
    },
    jupiter::service::artifact_service::ArtifactService,
    server::{CommonHttpOptions, trace_context},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuckCleanupTaskConfig {
    enabled: bool,
    cleanup_interval: u64,
    completed_retention_days: u32,
}

impl BuckCleanupTaskConfig {
    fn from_config(config: &Config) -> Self {
        Self::from_buck_config(config.buck.clone().unwrap_or_default())
    }

    fn from_buck_config(config: BuckConfig) -> Self {
        Self {
            enabled: config.enable_session_cleanup,
            cleanup_interval: config.cleanup_interval,
            completed_retention_days: config.completed_retention_days,
        }
    }

    fn interval_secs(&self) -> u64 {
        self.cleanup_interval.max(1)
    }
}

#[derive(Debug, Clone)]
struct BuckCleanupTaskControl {
    sender: watch::Sender<BuckCleanupTaskConfig>,
}

impl BuckCleanupTaskControl {
    fn new(config: BuckCleanupTaskConfig) -> Self {
        let (sender, _) = watch::channel(config);
        Self { sender }
    }

    fn current(&self) -> BuckCleanupTaskConfig {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> watch::Receiver<BuckCleanupTaskConfig> {
        self.sender.subscribe()
    }

    fn set_config(&self, config: BuckCleanupTaskConfig) {
        self.sender.send_replace(config);
    }
}

fn config_reload_buck_cleanup_subscriber(
    control: BuckCleanupTaskControl,
) -> ConfigReloadSubscriber {
    let apply_control = control.clone();
    ConfigReloadSubscriber::new(
        "buck_cleanup_task",
        move |next, report| apply_buck_cleanup_config(&apply_control, next, report),
        move |current, report| apply_buck_cleanup_config(&control, current, report),
    )
}

fn apply_buck_cleanup_config(
    control: &BuckCleanupTaskControl,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    if report
        .applied_fields
        .iter()
        .any(|field| field.starts_with("buck."))
    {
        control.set_config(BuckCleanupTaskConfig::from_config(config));
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ArtifactGcTaskControl {
    sender: watch::Sender<ArtifactGcConfig>,
}

impl ArtifactGcTaskControl {
    fn new(config: ArtifactGcConfig) -> Self {
        let (sender, _) = watch::channel(config);
        Self { sender }
    }

    fn current(&self) -> ArtifactGcConfig {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> watch::Receiver<ArtifactGcConfig> {
        self.sender.subscribe()
    }

    fn set_config(&self, config: ArtifactGcConfig) {
        self.sender.send_replace(config);
    }
}

fn config_reload_artifact_gc_subscriber(control: ArtifactGcTaskControl) -> ConfigReloadSubscriber {
    let apply_control = control.clone();
    ConfigReloadSubscriber::new(
        "artifact_gc_task",
        move |next, report| apply_artifact_gc_config(&apply_control, next, report),
        move |current, report| apply_artifact_gc_config(&control, current, report),
    )
}

fn apply_artifact_gc_config(
    control: &ArtifactGcTaskControl,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    if report
        .applied_fields
        .iter()
        .any(|field| field.starts_with("artifacts_gc."))
    {
        control.set_config(config.artifacts_gc.clone());
    }

    Ok(())
}

pub fn remove_git_suffix(full_path: &str, git_suffix: &str) -> PathBuf {
    PathBuf::from(full_path.replace(".git", "").replace(git_suffix, ""))
}

fn is_disallowed_root_repo_path(full_path: &str) -> bool {
    matches!(
        full_path.trim_start_matches('/').split('/').next(),
        Some("third-party.git")
    )
}

/// Spawns a background task to clean up expired Buck upload sessions.
///
/// Returns `None` if cleanup is disabled in configuration.
fn spawn_cleanup_task(
    ctx: AppContext,
    token: CancellationToken,
) -> Result<Option<JoinHandle<()>>, MegaError> {
    let cfg = BuckCleanupTaskConfig::from_config(&ctx.storage.config());
    if !cfg.enabled {
        return Ok(None);
    }

    let control = BuckCleanupTaskControl::new(cfg);
    ctx.config_handle
        .subscribe(config_reload_buck_cleanup_subscriber(control.clone()))?;
    let mut config_updates = control.subscribe();
    let cleanup_storage = ctx.storage.clone();

    Ok(Some(tokio::spawn(async move {
        let mut cfg = control.current();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(cfg.interval_secs()));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut accepts_config_updates = true;

        tracing::info!(
            interval_secs = cfg.interval_secs(),
            completed_retention_days = cfg.completed_retention_days,
            "Buck upload session cleanup task started"
        );

        loop {
            tokio::select! {
                changed = config_updates.changed(), if accepts_config_updates => {
                    match changed {
                        Ok(()) => {
                            let updated = config_updates.borrow().clone();
                            let interval_changed = updated.interval_secs() != cfg.interval_secs();
                            cfg = updated;
                            if interval_changed {
                                ticker = tokio::time::interval(std::time::Duration::from_secs(
                                    cfg.interval_secs(),
                                ));
                                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            }
                            tracing::info!(
                                enabled = cfg.enabled,
                                interval_secs = cfg.interval_secs(),
                                completed_retention_days = cfg.completed_retention_days,
                                "Buck upload session cleanup task config updated"
                            );
                        }
                        Err(_) => {
                            accepts_config_updates = false;
                            tracing::warn!(
                                "Buck upload session cleanup task config update channel closed; continuing with last config"
                            );
                        }
                    }
                }
                _ = ticker.tick(), if cfg.enabled => {
                    match cleanup_storage
                        .buck_storage()
                        .delete_expired_sessions(cfg.completed_retention_days)
                        .await
                    {
                        Ok(count) => {
                            if count > 0 {
                                tracing::info!(
                                    "Buck upload cleanup: deleted {} expired sessions",
                                    count
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Buck upload cleanup failed: {}. Will retry in next interval.",
                                e
                            );
                        }
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("Buck upload cleanup task received shutdown signal");
                    break;
                }
            }
        }

        tracing::info!("Buck upload cleanup task stopped gracefully");
    })))
}

/// Background GC for `artifact_objects` with no manifest references (`docs/artifacts-protocol.md` §10.6).
fn spawn_artifact_gc_task(
    ctx: AppContext,
    token: CancellationToken,
) -> Result<Option<JoinHandle<()>>, MegaError> {
    let cfg = ctx.storage.config().artifacts_gc.clone();
    if !cfg.enable {
        return Ok(None);
    }

    let control = ArtifactGcTaskControl::new(cfg);
    ctx.config_handle
        .subscribe(config_reload_artifact_gc_subscriber(control.clone()))?;
    let mut config_updates = control.subscribe();
    let service: ArtifactService = ctx.storage.artifact_service.clone();

    Ok(Some(tokio::spawn(async move {
        let mut cfg = control.current();
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(cfg.interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut accepts_config_updates = true;

        tracing::info!(
            interval_secs = cfg.interval_secs.max(1),
            grace_secs = cfg.grace_secs,
            batch_limit = cfg.batch_limit.max(1),
            "artifact_objects GC task started"
        );

        loop {
            tokio::select! {
                changed = config_updates.changed(), if accepts_config_updates => {
                    match changed {
                        Ok(()) => {
                            let updated = config_updates.borrow().clone();
                            let interval_changed =
                                updated.interval_secs.max(1) != cfg.interval_secs.max(1);
                            cfg = updated;
                            if interval_changed {
                                ticker = tokio::time::interval(std::time::Duration::from_secs(
                                    cfg.interval_secs.max(1),
                                ));
                                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            }
                            tracing::info!(
                                enabled = cfg.enable,
                                interval_secs = cfg.interval_secs.max(1),
                                grace_secs = cfg.grace_secs,
                                batch_limit = cfg.batch_limit.max(1),
                                "artifact_objects GC task config updated"
                            );
                        }
                        Err(_) => {
                            accepts_config_updates = false;
                            tracing::warn!(
                                "artifact_objects GC task config update channel closed; continuing with last config"
                            );
                        }
                    }
                }
                _ = ticker.tick(), if cfg.enable => {
                    let grace = std::time::Duration::from_secs(cfg.grace_secs);
                    let batch_limit = cfg.batch_limit.max(1);
                    match service
                        .gc_unreferenced_artifact_objects_once(grace, batch_limit)
                        .await
                    {
                        Ok(s) if s.deleted > 0 || s.candidates > 0 => {
                            tracing::info!(
                                candidates = s.candidates,
                                deleted = s.deleted,
                                skipped_still_referenced = s.skipped_still_referenced,
                                storage_delete_errors = s.storage_delete_errors,
                                db_delete_errors = s.db_delete_errors,
                                "artifact_objects GC tick"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::error!(error = %e, "artifact_objects GC tick failed"),
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("artifact_objects GC task received shutdown signal");
                    break;
                }
            }
        }

        tracing::info!("artifact_objects GC task stopped gracefully");
    })))
}

/// Returns a future that completes when the cancellation token is triggered.
async fn shutdown_signal(token: CancellationToken) {
    token.cancelled().await;
}

fn broadcast_shutdown(
    shutdown_token: &CancellationToken,
    notification_shutdown: &CancellationToken,
) {
    shutdown_token.cancel();
    notification_shutdown.cancel();
}

pub async fn start_http(ctx: AppContext, options: CommonHttpOptions) -> MegaResult {
    let CommonHttpOptions { host, port } = options.clone();
    let server_url = format!("{host}:{port}");
    let addr = SocketAddr::from_str(&server_url).map_err(|e| {
        MegaError::Other(format!("invalid HTTP listen address `{server_url}`: {e}"))
    })?;
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        MegaError::Other(format!(
            "failed to bind HTTP listener at `{server_url}`: {e}"
        ))
    })?;

    let middleware = tower::util::MapRequestLayer::new(rewrite_lfs_request_uri::<Body>);

    let shutdown_token = CancellationToken::new();
    let cleanup_handle = spawn_cleanup_task(ctx.clone(), shutdown_token.clone())?;
    let artifact_gc_handle = spawn_artifact_gc_task(ctx.clone(), shutdown_token.clone())?;
    let notification_shutdown = ctx.notification_shutdown.clone();
    let server_token = shutdown_token.clone();

    let app = app(ctx, host.clone(), port).await;
    let app_with_middleware = middleware.layer(app);

    tracing::info!(address = %addr, "HTTP server started up");

    let server_future = axum::serve(listener, app_with_middleware.into_make_service())
        .with_graceful_shutdown(shutdown_signal(server_token));

    let server_handle = tokio::spawn(async move {
        if let Err(e) = server_future.await {
            tracing::error!("HTTP server error: {}", e);
        }
    });

    tokio::pin!(server_handle);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal (Ctrl+C), starting graceful shutdown...");
        }
        result = server_handle.as_mut() => {
            if let Err(e) = result {
                tracing::error!("HTTP server unexpectedly stopped: {}", e);
            }
            tracing::info!("HTTP server stopped, initiating shutdown...");
        }
    }

    tracing::info!("Broadcasting shutdown signal to all tasks...");
    broadcast_shutdown(&shutdown_token, &notification_shutdown);

    let (cleanup_result, artifact_gc_result, server_result) = tokio::join!(
        async {
            if let Some(handle) = cleanup_handle {
                match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
                    Ok(Ok(_)) => {
                        tracing::info!("Cleanup task stopped successfully");
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Cleanup task panicked: {}", e);
                        Err(())
                    }
                    Err(_) => {
                        // Timeout indicates potential deadlock or extremely slow I/O.
                        tracing::error!(
                            "Cleanup task did not stop within 30s timeout. \
                            This may indicate a deadlock or extremely slow I/O. \
                            The task will be detached and may continue running. \
                            Operators: check DB/Redis connectivity and long-running I/O; \
                            consider increasing cleanup_interval if workloads are heavy."
                        );
                        Err(())
                    }
                }
            } else {
                Ok(())
            }
        },
        async {
            if let Some(handle) = artifact_gc_handle {
                match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
                    Ok(Ok(_)) => {
                        tracing::info!("artifact_objects GC task stopped successfully");
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        tracing::error!("artifact_objects GC task panicked: {}", e);
                        Err(())
                    }
                    Err(_) => {
                        tracing::error!(
                            "artifact_objects GC task did not stop within 30s timeout. The task will be detached."
                        );
                        Err(())
                    }
                }
            } else {
                Ok(())
            }
        },
        async {
            match server_handle.as_mut().await {
                Ok(_) => {
                    tracing::info!("HTTP server stopped gracefully");
                    Ok(())
                }
                Err(e) => {
                    tracing::error!("HTTP server join error: {}", e);
                    Err(())
                }
            }
        }
    );

    match (cleanup_result, artifact_gc_result, server_result) {
        (Ok(_), Ok(_), Ok(_)) => {
            tracing::info!("Graceful shutdown completed successfully");
        }
        _ => {
            tracing::warn!("Graceful shutdown completed with some errors");
        }
    }

    Ok(())
}

/// This is the main entry for the mono server.
/// It is responsible for creating the main router and setting up the necessary middleware.
///
/// The main router is composed of three nested routers:
/// 1. The LFS router nested in the `/`:
///   - GET or PUT `/objects/:object_id`
///   - GET or PUT `/locks`
///   - POST       `/locks/verify`
///   - POST       `/locks/:id/unlock`
///   - GET        `/objects/:object_id/chunks/:chunk_id`
///   - POST       `/objects/batch`
/// 2. The API router nested in the `/api/v1`:
///   - GET        `/api/v1/status`
///   - POST       `/api/v1/create-file`
///   - GET        `/api/v1/latest-commit`
///   - GET        `/api/v1/tree/commit-info`
///   - GET        `/api/v1/tree`
///   - GET        `/api/v1/blob`
///   - GET        `/api/v1/file/blob/:object_id`
///   - GET        `/api/v1/file/tree`
///   - GET        `/api/v1/path-can-clone`
/// 3. The OAuth router nested in the `/auth`:
///   - GET        `/auth/github`
///   - GET        `/auth/authorized`
///   - GET        `/auth/logout`
/// 4. The other routers for the git protocol:
///   - GET        end of `Regex::new(r"/info/refs$")`
///   - POST       end of `Regex::new(r"/git-upload-pack$")`
///   - POST       end of `Regex::new(r"/git-receive-pack$")`
pub async fn app(ctx: AppContext, host: String, port: u16) -> Router {
    let storage = ctx.storage;

    let git_object_cache = Arc::new(GitObjectCache {
        connection: ctx.connection.clone(),
        prefix: "git-object-rkyv:v1".to_string(),
    });

    let api_state = MonoApiServiceState {
        storage: storage.clone(),
        listen_addr: format!("http://{host}:{port}"),
        entity_store: EntityStore::new(),
        git_object_cache,
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
    };

    let origins: Vec<HeaderValue> = vec![
        HeaderValue::from_static("http://localhost"),
        HeaderValue::from_static("http://app.gitmega.com"),
        HeaderValue::from_static("http://app.gitmono.test"),
    ];

    // add RequestDecompressionLayer for handle gzip encode
    // add TraceLayer for log record
    // add CorsLayer to add cors header
    // add SessionManagerLayer for session management
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false) // Set to true in production with HTTPS
        .with_expiry(Expiry::OnInactivity(Duration::seconds(3600))); // 1 hour of inactivity

    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(lfs_router::routers().with_state(api_state.clone()))
        .nest(
            "/api/v1",
            api_router::routers()
                .with_state(api_state.clone())
                .route_layer(middleware::from_fn_with_state(
                    api_state.clone(),
                    cedar_guard,
                )),
        )
        // .nest("/auth", oauth::routers().with_state(api_state.clone()))
        // Using Regular Expressions for Path Matching in Protocol
        .route(
            "/{*path}",
            any({
                let api_state = api_state.clone();
                move |req: Request<Body>| {
                    handle_smart_protocol(req, Arc::new(ProtocolApiState::from_ref(&api_state)))
                }
            }),
        )
        .layer(
            ServiceBuilder::new().layer(session_layer).layer(
                CorsLayer::new()
                    .allow_origin(origins)
                    .allow_headers(vec![
                        http::header::AUTHORIZATION,
                        http::header::CONTENT_TYPE,
                        HeaderName::from_static("x-request-id"),
                        HeaderName::from_static("x-trace-id"),
                    ])
                    .expose_headers(vec![HeaderName::from_static("x-request-id")])
                    .allow_methods([
                        Method::GET,
                        Method::POST,
                        Method::OPTIONS,
                        Method::DELETE,
                        Method::PUT,
                    ])
                    .allow_credentials(true),
            ),
        )
        .layer(TraceLayer::new_for_http().make_span_with(trace_context::http_request_span))
        .layer(RequestDecompressionLayer::new())
        .layer(middleware::from_fn(trace_context::inject_trace_context))
        .with_state(api_state.clone())
        .split_for_parts();

    // Register /info/lfs paths for runtime compatibility (not in OpenAPI)
    // Convert OpenApiRouter to Router to avoid including /info/lfs in OpenAPI docs
    let info_lfs_router: Router = lfs_router::lfs_routes()
        .with_state(api_state.clone())
        .into();

    router
        .nest("/info/lfs", info_lfs_router)
        .merge(SwaggerUi::new("/swagger-ui").url("/api/openapi.json", api))
}

fn rewrite_lfs_request_uri<B>(mut req: Request<B>) -> Request<B> {
    let full_path = req.uri().path();

    if let Some(pos) = full_path.rfind("/info/lfs/") {
        let lfs_subpath = &full_path[pos..];

        let new_path_and_query = if let Some(query) = req.uri().query() {
            format!("{}?{}", lfs_subpath, query)
        } else {
            lfs_subpath.to_owned()
        };

        let new_uri = match Uri::builder().path_and_query(&new_path_and_query).build() {
            Ok(uri) => uri,
            Err(e) => {
                tracing::warn!(
                    "Failed to rewrite LFS URI: {}, error: {}",
                    new_path_and_query,
                    e
                );
                // Return the request unchanged, let downstream handlers deal with it
                return req;
            }
        };

        tracing::debug!("rewrite: old uri {:?}", req.uri());
        *req.uri_mut() = new_uri;
        tracing::debug!("rewrite: new uri {:?}", req.uri());
    }
    req
}

fn parse_info_refs_params(query_str: &str) -> std::result::Result<InfoRefsParams, ProtocolError> {
    let mut service_count = 0;
    for (key, _) in form_urlencoded::parse(query_str.as_bytes()) {
        if key == "service" {
            service_count += 1;
        } else {
            return Err(ProtocolError::InvalidInput(format!(
                "unsupported info/refs query parameter: {key}"
            )));
        }
    }
    if service_count > 1 {
        return Err(ProtocolError::InvalidInput(
            "duplicate service parameter".to_owned(),
        ));
    }

    let params: InfoRefsParams = serde_urlencoded::from_str(query_str).map_err(|err| {
        ProtocolError::InvalidInput(format!("invalid info/refs query parameters: {err}"))
    })?;
    let service = params
        .service
        .as_deref()
        .ok_or_else(|| ProtocolError::InvalidInput("missing service parameter".to_owned()))?;
    ServiceType::from_str(service).map_err(|err| ProtocolError::InvalidInput(err.to_string()))?;
    Ok(params)
}

async fn handle_smart_protocol(
    req: Request<Body>,
    state: Arc<ProtocolApiState>,
) -> std::result::Result<Response, ProtocolError> {
    let full_path = req.uri().path();
    if is_disallowed_root_repo_path(full_path) {
        return Err(ProtocolError::InvalidInput(
            "Repository third-party.git is not supported".to_string(),
        ));
    }
    if full_path.ends_with("/info/refs") && req.method().eq(&Method::GET) {
        let repo_path = remove_git_suffix(full_path, "/info/refs");
        let uri = req.uri();
        let query_str = uri.query().unwrap_or("");
        let params = parse_info_refs_params(query_str)?;
        crate::contract::git_protocol::http::git_info_refs(&state, params, repo_path).await
    } else if full_path.ends_with("/git-upload-pack") && req.method().eq(&Method::POST) {
        let repo_path = remove_git_suffix(full_path, "/git-upload-pack");
        crate::contract::git_protocol::http::git_upload_pack(&state, req, repo_path).await
    } else if full_path.ends_with("/git-receive-pack") && req.method().eq(&Method::POST) {
        let repo_path = remove_git_suffix(full_path, "/git-receive-pack");
        crate::contract::git_protocol::http::git_receive_pack(&state, req, repo_path).await
    } else {
        Ok(Response::builder()
            .status(404)
            .body(Body::from("Operation not supported"))
            .unwrap())
    }
}

#[cfg(test)]
mod tests {
    use http::Request;

    use super::*;
    use crate::config::{
        ArtifactGcConfig, BuckConfig, reload::ConfigHandle, testing::isolated_config,
    };

    #[test]
    fn buck_cleanup_subscriber_updates_control_from_reload() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.buck = Some(BuckConfig {
            enable_session_cleanup: true,
            cleanup_interval: 300,
            completed_retention_days: 7,
            ..Default::default()
        });
        let mut candidate = config.clone();
        candidate.buck = Some(BuckConfig {
            enable_session_cleanup: false,
            cleanup_interval: 60,
            completed_retention_days: 1,
            ..Default::default()
        });
        let control = BuckCleanupTaskControl::new(BuckCleanupTaskConfig::from_config(&config));
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_buck_cleanup_subscriber(control.clone()))
            .expect("subscribe");
        let report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(
            report.applied_fields,
            vec![
                "buck.enable_session_cleanup",
                "buck.cleanup_interval",
                "buck.completed_retention_days"
            ]
        );
        assert_eq!(
            control.current(),
            BuckCleanupTaskConfig {
                enabled: false,
                cleanup_interval: 60,
                completed_retention_days: 1,
            }
        );
    }

    #[test]
    fn artifact_gc_subscriber_updates_control_from_reload() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.artifacts_gc = ArtifactGcConfig {
            enable: true,
            interval_secs: 3600,
            grace_secs: 86_400,
            batch_limit: 100,
        };
        let mut candidate = config.clone();
        candidate.artifacts_gc = ArtifactGcConfig {
            enable: true,
            interval_secs: 120,
            grace_secs: 600,
            batch_limit: 10,
        };
        let control = ArtifactGcTaskControl::new(config.artifacts_gc.clone());
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_artifact_gc_subscriber(control.clone()))
            .expect("subscribe");
        let report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(
            report.applied_fields,
            vec![
                "artifacts_gc.interval_secs",
                "artifacts_gc.grace_secs",
                "artifacts_gc.batch_limit"
            ]
        );
        assert_eq!(
            control.current(),
            ArtifactGcConfig {
                enable: true,
                interval_secs: 120,
                grace_secs: 600,
                batch_limit: 10,
            }
        );
    }

    #[test]
    fn broadcast_shutdown_cancels_notification_tasks() {
        let shutdown_token = CancellationToken::new();
        let notification_shutdown = CancellationToken::new();

        broadcast_shutdown(&shutdown_token, &notification_shutdown);

        assert!(shutdown_token.is_cancelled());
        assert!(notification_shutdown.is_cancelled());
    }

    #[test]
    fn test_disallow_third_party_git_root_repo() {
        assert!(is_disallowed_root_repo_path("/third-party.git/info/refs"));
        assert!(is_disallowed_root_repo_path(
            "/third-party.git/git-receive-pack"
        ));
        assert!(!is_disallowed_root_repo_path(
            "/third-party/test.git/info/refs"
        ));
        assert!(!is_disallowed_root_repo_path("/project.git/info/refs"));
    }

    #[test]
    fn malformed_info_refs_query_returns_protocol_error() {
        let err =
            parse_info_refs_params("service=git-status").expect_err("bad service should fail");

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
    }

    #[test]
    fn info_refs_query_rejects_extra_and_duplicate_parameters() {
        let extra = parse_info_refs_params("service=git-upload-pack&foo=bar")
            .expect_err("extra query parameter should fail");
        let duplicate = parse_info_refs_params("service=git-upload-pack&service=git-receive-pack")
            .expect_err("duplicate service parameter should fail");

        assert!(matches!(extra, ProtocolError::InvalidInput(_)));
        assert!(matches!(duplicate, ProtocolError::InvalidInput(_)));
    }

    #[test]
    fn test_rewrite_lfs_uri_basic() {
        let req = Request::builder()
            .uri("/repo/a/b/info/lfs/objects/123")
            .body(())
            .unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(new_req.uri().path(), "/info/lfs/objects/123");
    }

    #[test]
    fn test_rewrite_keeps_query_string() {
        let req = Request::builder()
            .uri("/repo/a/info/lfs/locks?token=abc123")
            .body(())
            .unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(
            new_req.uri().path_and_query().unwrap().to_string(),
            "/info/lfs/locks?token=abc123"
        );
    }

    #[test]
    fn test_no_rewrite_when_no_lfs_prefix() {
        let req = Request::builder().uri("/not-lfs-path").body(()).unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(new_req.uri().path(), "/not-lfs-path");
    }

    #[test]
    fn test_rewrite_with_trailing_slash() {
        let req = Request::builder()
            .uri("/repo/info/lfs/locks/")
            .body(())
            .unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(new_req.uri().path(), "/info/lfs/locks/");
    }

    #[test]
    fn test_rewrite_complex_path() {
        let req = Request::builder()
            .uri("/a/b/c/info/lfs/objects/abc/def/ghi")
            .body(())
            .unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(new_req.uri().path(), "/info/lfs/objects/abc/def/ghi");
    }

    #[test]
    fn test_rewrite_when_repo_path_contains_info_lfs() {
        let req = Request::builder()
            .uri("/repos/info/lfs/info/lfs/objects/123")
            .body(())
            .unwrap();

        let new_req = rewrite_lfs_request_uri(req);

        assert_eq!(new_req.uri().path(), "/info/lfs/objects/123");
    }
}
