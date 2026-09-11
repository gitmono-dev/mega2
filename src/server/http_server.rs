use std::{net::SocketAddr, str::FromStr, sync::Arc};

use axum::{
    Router, ServiceExt,
    body::Body,
    extract::{DefaultBodyLimit, FromRef},
    http::{self, Request, Uri},
    middleware,
    response::Response,
    routing::any,
};
use git_internal::internal::object::tree::Tree;
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
        oauth::{api_store::BrowserSessionStore, website_session_store::WebsiteSessionStore},
        router::{lfs_router, oci_router},
    },
    bellatrix::Bellatrix,
    ceres::{
        api_service::{cache::GitObjectCache, state::ProtocolApiState},
        protocol::ServiceType,
    },
    common::errors::{MegaError, MegaResult, ProtocolError},
    config::{
        ArtifactGcConfig, BuckConfig, Config, PushAuth, PushPolicy,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    context::AppContext,
    contract::{
        git_protocol::InfoRefsParams,
        policy::{
            enforcement::Enforcement,
            entitystore::{MEGA_CEDAR_PATH, SharedEntityStore},
            guard::cedar_guard::cedar_guard,
        },
    },
    jupiter::{
        service::{artifact_service::ArtifactService, oci_service::DEFAULT_MAX_UPLOAD_CHUNK},
        utils::converter::FromMegaModel,
    },
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

/// Read the in-repo authorization data source `/.mega_cedar.json` from the root
/// tree (ADR-UN-02).
async fn load_mega_cedar_json(
    storage: &crate::jupiter::storage::Storage,
) -> Result<String, MegaError> {
    let mono_storage = storage.mono_storage();
    let root_ref = mono_storage
        .get_main_ref("/")
        .await?
        .ok_or_else(|| MegaError::Other("Root ref not found".into()))?;
    let root_tree = Tree::from_mega_model(
        mono_storage
            .get_tree_by_hash(&root_ref.ref_tree_hash)
            .await?
            .ok_or_else(|| MegaError::Other("Root tree not found".into()))?,
    );
    let file_name = MEGA_CEDAR_PATH.trim_start_matches('/');
    let blob_item = root_tree
        .tree_items
        .iter()
        .find(|item| item.name == file_name)
        .ok_or_else(|| MegaError::Other(format!("{file_name} not found in root directory")))?;
    let blob_hash = blob_item.id.to_string();
    let content_bytes = storage.git_service.get_object_as_bytes(&blob_hash).await?;
    String::from_utf8(content_bytes)
        .map_err(|e| MegaError::Other(format!("UTF-8 decode failed: {e}")))
}

/// First-build the shared authorization snapshot before binding the HTTP
/// listener (UN-02). `off` is a no-op; a failed first build fails startup.
pub(crate) async fn ensure_authz_first_build(
    shared: &SharedEntityStore,
    storage: &crate::jupiter::storage::Storage,
    enforcement: Enforcement,
) -> Result<(), MegaError> {
    if !enforcement.builds() {
        return Ok(());
    }
    let json = load_mega_cedar_json(storage).await?;
    shared
        .ensure(&json)
        .map_err(|e| MegaError::Other(format!("authorization first-build failed: {e}")))?;
    Ok(())
}

pub async fn start_http(ctx: AppContext, options: CommonHttpOptions) -> MegaResult {
    crate::config::validate::require_oauth_for_http_service(ctx.storage.config().as_ref())?;
    warn_if_unauthenticated_push(ctx.storage.config().as_ref());

    // TP-15 / 4.1 ②③⑥: open CLs, last_policy vs non-terminal rows, watermark reset.
    ctx.storage.prepare_push_policy_startup().await?;

    // First-build the shared authorization snapshot before the listener binds
    // (UN-02). `off` is a no-op; a failed first build fails startup.
    let enforcement = Enforcement::parse(&ctx.storage.config().cedar.enforcement)
        .ok_or_else(|| MegaError::Other("invalid cedar.enforcement".to_string()))?;
    ensure_authz_first_build(&ctx.entity_store, &ctx.storage, enforcement).await?;

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

    let app = app(ctx, host.clone(), port).await?;
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

/// Built-in development CORS origins used when `oauth.allowed_cors_origins` is
/// unset or empty.
const DEFAULT_CORS_ORIGINS: &[&str] = &[
    "http://localhost",
    "http://app.gitmega.com",
    "http://app.gitmono.test",
];

/// Build the CORS allow-origin list: the configured `oauth.allowed_cors_origins`
/// when non-empty (invalid entries skipped with a warning), otherwise the
/// built-in development defaults. An empty/absent config never widens CORS (it
/// falls back to the defaults, not to allow-all). Configured entries are
/// validated up-front by `validate_oauth_config`, so the runtime skip is a
/// defensive belt-and-suspenders.
fn cors_allow_origins(oauth: Option<&crate::config::OAuthConfig>) -> Vec<HeaderValue> {
    let configured = oauth
        .map(|oauth| oauth.allowed_cors_origins.as_slice())
        .unwrap_or(&[]);
    if configured.is_empty() {
        return DEFAULT_CORS_ORIGINS
            .iter()
            .map(|origin| HeaderValue::from_static(origin))
            .collect();
    }
    configured
        .iter()
        .filter_map(|origin| match HeaderValue::from_str(origin) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(
                    origin = %origin,
                    "ignoring invalid oauth.allowed_cors_origins entry"
                );
                None
            }
        })
        .collect()
}

fn warn_if_unauthenticated_push(config: &Config) {
    if config.git.push_auth == Some(PushAuth::None) {
        tracing::warn!(
            "git.push_auth=none: HTTP receive-pack has no pusher identity; only use behind a trusted network boundary (loopback, Unix socket, or controlled intranet)"
        );
    }
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
pub async fn app(ctx: AppContext, host: String, port: u16) -> Result<Router, MegaError> {
    let storage = ctx.storage;
    let config = storage.config();
    let storage_only = config.git.storage_only();
    // Trunk/storage-only omit CL/reviewer/preview-write OpenAPI; LFS is mounted
    // on both morphologies (plan-20260909 LF-02). Morphology still switches the
    // `/api/v1` subset via `routers_for(PushPolicy::Trunk)`.
    let protocol_surface = storage_only || config.monorepo.push_policy == PushPolicy::Trunk;

    let git_object_cache = Arc::new(GitObjectCache {
        connection: ctx.connection.clone(),
        prefix: std::env::var("MEGA_GIT_OBJECT_CACHE_PREFIX")
            .unwrap_or_else(|_| "git-object-rkyv:v1".to_string()),
    });

    let listen_addr = std::env::var("MEGA_HTTP__PUBLIC_BASE_URL")
        .ok()
        .map(|value| value.trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("http://{host}:{port}"));

    let (session_store, origins) = if storage_only {
        (
            BrowserSessionStore::Anonymous,
            cors_allow_origins(config.oauth.as_ref()),
        )
    } else {
        let oauth = config.oauth.clone().ok_or_else(|| {
            MegaError::Other("OAuth configuration is required for the HTTP service".to_string())
        })?;
        let session_store = WebsiteSessionStore::new(
            oauth.website_api_base_url.clone(),
            oauth.session_cookie_names.clone(),
        )?;
        (
            BrowserSessionStore::Website(session_store),
            cors_allow_origins(Some(&oauth)),
        )
    };

    let api_state = MonoApiServiceState {
        storage: storage.clone(),
        session_store,
        listen_addr,
        entity_store: ctx.entity_store.clone(),
        git_object_cache,
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
    };

    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(Expiry::OnInactivity(Duration::seconds(3600)));

    let cors = CorsLayer::new()
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
        .allow_credentials(true);

    let protocol_state = api_state.clone();
    let protocol_route = || {
        let protocol_state = protocol_state.clone();
        any(move |req: Request<Body>| {
            handle_smart_protocol(req, Arc::new(ProtocolApiState::from_ref(&protocol_state)))
        })
    };

    let (router, api) = if protocol_surface {
        let include_oci = storage_only && config.oci.enabled;
        let openapi = OpenApiRouter::with_openapi(ApiDoc::openapi())
            .merge(lfs_router::routers().with_state(api_state.clone()));
        let openapi = if include_oci {
            openapi.merge(oci_router::routers().with_state(api_state.clone()))
        } else {
            openapi
        };
        openapi
            .nest(
                "/api/v1",
                api_router::routers_for(PushPolicy::Trunk).with_state(api_state.clone()),
            )
            .route("/{*path}", protocol_route())
            .layer(
                ServiceBuilder::new()
                    .layer(session_layer.clone())
                    .layer(cors.clone()),
            )
            .layer(TraceLayer::new_for_http().make_span_with(trace_context::http_request_span))
            .layer(RequestDecompressionLayer::new())
            .layer(middleware::from_fn(trace_context::inject_trace_context))
            .with_state(api_state.clone())
            .split_for_parts()
    } else {
        // Review morphology: never register OCI `/v2` (ADR-DR-01 fail-closed).
        OpenApiRouter::with_openapi(ApiDoc::openapi())
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
            .route("/{*path}", protocol_route())
            .layer(ServiceBuilder::new().layer(session_layer).layer(cors))
            .layer(TraceLayer::new_for_http().make_span_with(trace_context::http_request_span))
            .layer(RequestDecompressionLayer::new())
            .layer(middleware::from_fn(trace_context::inject_trace_context))
            .with_state(api_state.clone())
            .split_for_parts()
    };

    // Nest `/info/lfs` for both trunk/storage-only and review so Git LFS
    // discovery does not fall through to the smart-protocol catch-all.
    let info_lfs_router: Router = lfs_router::lfs_routes()
        .with_state(api_state.clone())
        .into();
    let router = router.nest("/info/lfs", info_lfs_router);

    // Static `/v2` prefix before catch-all (same pattern as `/info/lfs`).
    let router = if storage_only && config.oci.enabled {
        let oci = oci_router::oci_routes()
            .route_layer(DefaultBodyLimit::max(DEFAULT_MAX_UPLOAD_CHUNK))
            .with_state(api_state.clone());
        router.nest("/v2", oci)
    } else {
        router
    };

    Ok(router.merge(SwaggerUi::new("/swagger-ui").url("/api/openapi.json", api)))
}

pub(crate) fn storage_only_openapi_doc(include_oci: bool) -> utoipa::openapi::OpenApi {
    let router = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(lfs_router::routers())
        .nest("/api/v1", api_router::storage_only_routers());
    let router = if include_oci {
        router.merge(oci_router::routers())
    } else {
        router
    };
    router.split_for_parts().1
}

/// OpenAPI for `push_policy=trunk` HTTP (includes LFS merge). Same `/api/v1`
/// subset as storage-only; assembled via [`api_router::routers_for`] so
/// the surface is keyed on morphology, not only `push_auth`.
pub(crate) fn trunk_openapi_doc(include_oci: bool) -> utoipa::openapi::OpenApi {
    let router = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(lfs_router::routers())
        .nest("/api/v1", api_router::routers_for(PushPolicy::Trunk));
    let router = if include_oci {
        router.merge(oci_router::routers())
    } else {
        router
    };
    router.split_for_parts().1
}

fn rewrite_lfs_request_uri<B>(mut req: Request<B>) -> Request<B> {
    // Capture the repository path prefix (the segment before `/info/lfs/`)
    // before it is stripped for routing, so LFS handlers can namespace locks
    // per repository. Empty when the request carries no repo prefix.
    let (repo_prefix, rewrite_target) = {
        let full_path = req.uri().path();
        match full_path.rfind("/info/lfs/") {
            Some(pos) => {
                let lfs_subpath = &full_path[pos..];
                let target = if let Some(query) = req.uri().query() {
                    format!("{lfs_subpath}?{query}")
                } else {
                    lfs_subpath.to_owned()
                };
                (full_path[..pos].to_owned(), Some(target))
            }
            None => (String::new(), None),
        }
    };

    req.extensions_mut()
        .insert(lfs_router::LfsRepoContext(repo_prefix));

    if let Some(new_path_and_query) = rewrite_target {
        match Uri::builder().path_and_query(&new_path_and_query).build() {
            Ok(uri) => {
                tracing::debug!("rewrite: old uri {:?}", req.uri());
                *req.uri_mut() = uri;
                tracing::debug!("rewrite: new uri {:?}", req.uri());
            }
            Err(e) => {
                // Leave the URI unchanged and let downstream handlers deal with it.
                tracing::warn!(
                    "Failed to rewrite LFS URI: {}, error: {}",
                    new_path_and_query,
                    e
                );
            }
        }
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
    let parsed = crate::contract::git_protocol::path::parse_git_protocol_path(
        req.method(),
        req.uri().path(),
    )?;

    match parsed.endpoint {
        crate::contract::git_protocol::path::GitProtocolEndpoint::InfoRefs => {
            let params = parse_info_refs_params(req.uri().query().unwrap_or(""))?;
            crate::contract::git_protocol::http::git_info_refs(
                &state,
                params,
                parsed.repo_path,
                req.headers(),
            )
            .await
        }
        crate::contract::git_protocol::path::GitProtocolEndpoint::UploadPack => {
            crate::contract::git_protocol::http::git_upload_pack(&state, req, parsed.repo_path)
                .await
        }
        crate::contract::git_protocol::path::GitProtocolEndpoint::ReceivePack => {
            crate::contract::git_protocol::http::git_receive_pack(&state, req, parsed.repo_path)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use http::Request;

    use super::*;

    #[test]
    fn cors_origins_fall_back_to_defaults_when_unset() {
        let origins = cors_allow_origins(None);
        assert_eq!(origins.len(), DEFAULT_CORS_ORIGINS.len());
        assert!(origins.contains(&HeaderValue::from_static("http://localhost")));
    }

    #[test]
    fn cors_origins_fall_back_to_defaults_when_empty() {
        let oauth = crate::config::OAuthConfig {
            allowed_cors_origins: vec![],
            ..Default::default()
        };
        let origins = cors_allow_origins(Some(&oauth));
        assert_eq!(origins.len(), DEFAULT_CORS_ORIGINS.len());
    }

    #[test]
    fn cors_origins_use_configured_values() {
        let oauth = crate::config::OAuthConfig {
            allowed_cors_origins: vec![
                "https://app.example.com".to_string(),
                "http://localhost:3000".to_string(),
            ],
            ..Default::default()
        };
        let origins = cors_allow_origins(Some(&oauth));
        assert_eq!(
            origins,
            vec![
                HeaderValue::from_static("https://app.example.com"),
                HeaderValue::from_static("http://localhost:3000"),
            ]
        );
    }

    #[test]
    fn cors_origins_skip_invalid_configured_entries() {
        // A control char makes HeaderValue::from_str fail; it is skipped, not
        // turned into an allow-all.
        let oauth = crate::config::OAuthConfig {
            allowed_cors_origins: vec![
                "https://ok.example.com".to_string(),
                "bad\norigin".to_string(),
            ],
            ..Default::default()
        };
        let origins = cors_allow_origins(Some(&oauth));
        assert_eq!(
            origins,
            vec![HeaderValue::from_static("https://ok.example.com")]
        );
    }
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

    #[test]
    fn push_auth_none_warns_that_pusher_identity_is_absent() {
        use std::{
            io::Write,
            sync::{Arc, Mutex},
        };

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl MakeWriter<'_> for Buf {
            type Writer = Buf;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut none = isolated_config(temp_dir.path().join("none"));
        none.git.push_auth = Some(PushAuth::None);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Buf(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            warn_if_unauthenticated_push(&none);
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");
        assert!(
            out.contains("git.push_auth=none") && out.contains("no pusher identity"),
            "missing identity warning: {out}"
        );
    }

    #[test]
    fn storage_only_skips_oauth_http_gate() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.oauth = None;
        config.git.push_auth = Some(PushAuth::None);
        crate::config::validate::require_oauth_for_http_service(&config)
            .expect("storage-only HTTP must start without [oauth]");
    }

    #[test]
    fn omitted_push_auth_still_requires_oauth_http_gate() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.oauth = None;
        let err = crate::config::validate::require_oauth_for_http_service(&config)
            .expect_err("omitted push_auth keeps the OAuth HTTP form");
        assert!(
            err.to_string().contains("[oauth]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn storage_only_openapi_is_readonly_protocol_subset() {
        let api = storage_only_openapi_doc(false);
        let paths: Vec<String> = api.paths.paths.keys().cloned().collect();
        assert!(
            paths.iter().any(|p| p.ends_with("/status")),
            "storage-only OpenAPI must include /status: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/blob") || p.contains("/tree")),
            "storage-only OpenAPI must include blob/tree reads: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/lfs")),
            "storage-only OpenAPI must include LFS: {paths:?}"
        );
        assert!(
            paths.iter().all(|p| !p.contains("/v2")),
            "storage-only OpenAPI without include_oci must omit /v2: {paths:?}"
        );
        for needle in ["/auth", "create-entry", "/cl", "/user"] {
            assert!(
                paths.iter().all(|p| !p.contains(needle)),
                "storage-only OpenAPI must not include {needle}: {paths:?}"
            );
        }
    }

    #[test]
    fn storage_only_openapi_include_oci_dual_state() {
        let with_oci = storage_only_openapi_doc(true);
        let with_paths: Vec<String> = with_oci.paths.paths.keys().cloned().collect();
        assert!(
            with_paths.iter().any(|p| p.contains("/v2")),
            "include_oci=true must document /v2: {with_paths:?}"
        );

        let without_oci = storage_only_openapi_doc(false);
        let without_paths: Vec<String> = without_oci.paths.paths.keys().cloned().collect();
        assert!(
            without_paths.iter().all(|p| !p.contains("/v2")),
            "include_oci=false must omit /v2: {without_paths:?}"
        );
    }

    #[test]
    fn trunk_openapi_omits_cl_issue_reviewer_preview_writes_keeps_lfs() {
        let api = trunk_openapi_doc(false);
        let paths: Vec<String> = api.paths.paths.keys().cloned().collect();
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/blob") || p.contains("/tree")),
            "trunk OpenAPI must keep readonly preview: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/lfs")),
            "trunk OpenAPI must include LFS: {paths:?}"
        );
        assert!(
            paths.iter().all(|p| !p.contains("/v2")),
            "trunk OpenAPI without include_oci must omit /v2: {paths:?}"
        );
        for needle in [
            "/cl",
            "/issue",
            "reviewer",
            "create-entry",
            "/edit/save",
            "/user",
        ] {
            assert!(
                paths.iter().all(|p| !p.contains(needle)),
                "trunk OpenAPI must not include {needle}: {paths:?}"
            );
        }
    }

    #[test]
    fn trunk_openapi_include_oci_dual_state() {
        let with_oci = trunk_openapi_doc(true);
        let with_paths: Vec<String> = with_oci.paths.paths.keys().cloned().collect();
        assert!(
            with_paths.iter().any(|p| p.contains("/v2")),
            "include_oci=true must document /v2: {with_paths:?}"
        );

        let without_oci = trunk_openapi_doc(false);
        let without_paths: Vec<String> = without_oci.paths.paths.keys().cloned().collect();
        assert!(
            without_paths.iter().all(|p| !p.contains("/v2")),
            "include_oci=false must omit /v2: {without_paths:?}"
        );
    }

    #[test]
    fn review_openapi_never_includes_oci() {
        let (_, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
            .merge(lfs_router::routers())
            .nest("/api/v1", api_router::routers())
            .split_for_parts();
        let paths: Vec<String> = api.paths.paths.keys().cloned().collect();
        assert!(
            paths.iter().all(|p| !p.contains("/v2")),
            "review OpenAPI must never include OCI /v2: {paths:?}"
        );
    }

    #[test]
    fn oauth_openapi_still_includes_cl_user_and_preview_writes() {
        let (_, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
            .nest("/api/v1", api_router::routers())
            .split_for_parts();
        let paths: Vec<String> = api.paths.paths.keys().cloned().collect();
        assert!(
            paths.iter().any(|p| p.contains("/cl")),
            "OAuth OpenAPI must include /cl: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/user")),
            "OAuth OpenAPI must include /user: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("create-entry")),
            "OAuth OpenAPI must include create-entry: {paths:?}"
        );
    }

    #[tokio::test]
    async fn un02_first_build_off_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(dir.path()).await;
        let shared = SharedEntityStore::new();
        ensure_authz_first_build(&shared, &storage, Enforcement::Off)
            .await
            .expect("off must be a no-op");
        assert!(shared.snapshot().is_none(), "off must not build");
    }

    #[tokio::test]
    async fn un02_first_build_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(dir.path()).await;
        let shared = SharedEntityStore::new();
        // Empty test storage has no root repo -> first build fails, which must
        // fail server startup (UN-02 AC 6/7).
        let err = ensure_authz_first_build(&shared, &storage, Enforcement::Enforce)
            .await
            .expect_err("first build must fail without a root repo");
        assert!(
            err.to_string().contains("Root ref not found")
                || err.to_string().contains("first-build")
        );
    }

    #[tokio::test]
    async fn un02_first_build_shadow_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(dir.path()).await;
        let shared = SharedEntityStore::new();
        // `shadow` also builds the store, so a failed first build must fail
        // server startup (UN-02 AC 6).
        let err = ensure_authz_first_build(&shared, &storage, Enforcement::Shadow)
            .await
            .expect_err("shadow first build must fail without a root repo");
        assert!(
            err.to_string().contains("Root ref not found")
                || err.to_string().contains("first-build")
        );
    }
}
