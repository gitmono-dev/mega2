//! MST/2 snapshot HTTP surface (spec 03/04) — first slice.
//!
//! Registered only when `[mst2].enabled`; capabilities honestly declare what
//! this build serves. Slice scope: capabilities, resolve and directory on
//! native monorepo views (no bindings/imports yet, T04/T05). Reads are pinned
//! to the fixed commit captured at resolve time; no handler here touches
//! current refs after resolve.

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::Engine;
use bytes::Bytes;
use git_internal::hash::{ObjectHash, get_hash_kind};
use mst2_codec::descriptor;
use request::Mst2Bytes;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    api::MonoApiServiceState,
    ceres::snapshot::{
        descriptor::build as build_descriptor,
        error::{SnapshotError, SnapshotErrorCode},
        pages::{
            MetadataWalkOutcome, base64_of, build_directory_page, build_directory_page_with_work,
            hex_of, proof_pages, resolve_abs_metadata,
        },
        projection_observation::{NativeResolveSource, ResolvedProjection},
        runtime::{now_unix, runtime},
        view::{SnapshotView, validate_scope_relative_path},
    },
};

pub fn routers(api_state: MonoApiServiceState) -> Router<MonoApiServiceState> {
    Router::new()
        .route("/snapshots/capabilities", get(capabilities))
        .route("/snapshots/resolve", post(resolve))
        .route("/snapshots/leases/{lease_id}/renew", post(lease_renew))
        .route("/snapshots/leases/{lease_id}", delete(lease_release))
        .route("/snapshots/{snapshot_id}/descriptor", get(descriptor_get))
        .route("/snapshots/{snapshot_id}/directory", get(directory))
        .route(
            "/snapshots/{snapshot_id}/blob",
            get(raw_blob::blob).head(content::blob_head),
        )
        .route("/snapshots/{snapshot_id}/lookup", post(lookup))
        .route(
            "/snapshots/{snapshot_id}/metadata/pages",
            post(metadata_pages),
        )
        .route("/snapshots/{snapshot_id}/objects", post(content::objects))
        .route(
            "/snapshots/{snapshot_id}/chunk-map",
            get(content::chunk_map),
        )
        .route(
            "/snapshots/{snapshot_id}/chunk-map/pages",
            get(content::chunk_map_pages),
        )
        .route("/snapshots/{snapshot_id}/chunks", post(content::chunks))
        // Spec 14 §4 hard limit on JSON request bodies; oversized requests
        // are rejected before any handler runs.
        .layer(axum::extract::DefaultBodyLimit::max(JSON_REQUEST_LIMIT))
        .route_layer(axum::middleware::from_fn(reject_oversize_body))
        // Auth and error-envelope request-id apply to every route here;
        // a new handler cannot silently skip them.
        .route_layer(axum::middleware::from_fn_with_state(
            api_state,
            snapshot_auth_middleware,
        ))
}

/// Explicit fixture for the historical G authority and corruption contracts.
/// Production resolve always installs the rooted family for a new SID.
#[cfg(test)]
pub(crate) fn generic_history_routers(
    api_state: MonoApiServiceState,
) -> Router<MonoApiServiceState> {
    routers(api_state).layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            GENERIC_HISTORY_RESOLVE.scope(true, next.run(request)).await
        },
    ))
}

#[path = "snapshot_content.rs"]
mod content;

#[path = "snapshot_raw_blob.rs"]
mod raw_blob;

#[path = "snapshot_request.rs"]
mod request;

#[cfg(test)]
#[path = "snapshot_request_tests.rs"]
mod request_tests;

/// Spec 14 §4: JSON request bytes hard limit.
pub(crate) const JSON_REQUEST_LIMIT: usize = 131_072;

/// The media type is part of the MST/2 TreeFrame wire contract (spec 06
/// §1). Keep it in one place so every frame-producing endpoint has the same
/// response representation.
pub(crate) const TREEFRAME_MEDIA_TYPE: &str = "application/vnd.mega.treeframe;version=2";

/// Build a TreeFrame response with the protocol identity headers. The request
/// digest covers the exact bytes that were parsed, including JSON whitespace
/// and key ordering, so callers must pass the original body.
pub(crate) fn treeframe_response(
    snapshot_id: &str,
    request_body: &[u8],
    body: Vec<u8>,
) -> Result<Response, SnapshotError> {
    treeframe_response_body(snapshot_id, request_body, axum::body::Body::from(body))
}

fn treeframe_response_body(
    snapshot_id: &str,
    request_body: &[u8],
    body: axum::body::Body,
) -> Result<Response, SnapshotError> {
    let request_digest: [u8; 32] = Sha256::digest(request_body).into();
    Response::builder()
        .header("content-type", TREEFRAME_MEDIA_TYPE)
        .header("x-mega-snapshot-id", snapshot_id)
        .header(
            "x-mega-request-digest",
            format!("sha256:{}", hex_of(&request_digest)),
        )
        .header("cache-control", "private, no-cache, no-transform")
        .header("vary", "Authorization, Accept")
        .body(body)
        .map_err(|e| internal(format!("TreeFrame response build failed: {e}")))
}

fn guarded_treeframe_response(
    state: &MonoApiServiceState,
    context: &crate::ceres::snapshot::runtime::SnapshotContext,
    snapshot_id: &str,
    request_body: &[u8],
    frames: Vec<Vec<u8>>,
) -> Result<Response, SnapshotError> {
    guarded_treeframe_response_with_budget(state, context, snapshot_id, request_body, frames, None)
}

fn guarded_treeframe_response_with_budget(
    state: &MonoApiServiceState,
    context: &crate::ceres::snapshot::runtime::SnapshotContext,
    snapshot_id: &str,
    request_body: &[u8],
    frames: Vec<Vec<u8>>,
    memory: Option<crate::ceres::snapshot::content_budget::MemoryLease>,
) -> Result<Response, SnapshotError> {
    // Field order also keeps admission credits until frame allocations drop
    // on failures before ownership has moved into individual Bytes.
    struct FrameAllocation {
        frames: Vec<Vec<u8>>,
        memory: Option<crate::ceres::snapshot::content_budget::MemoryLease>,
    }
    let mut allocation = FrameAllocation { frames, memory };
    if let Some(lease) = &allocation.memory {
        let allocated = allocation
            .frames
            .iter()
            .try_fold(0usize, |total, frame| total.checked_add(frame.capacity()));
        if allocated.is_none_or(|allocated| allocated > lease.bytes) {
            return Err(internal("encoded frames exceed their memory reservation"));
        }
    }
    let headers = REQUEST_HEADERS.try_with(Clone::clone).map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::Unauthenticated,
            "request authentication context missing",
        )
    })?;
    let memory = allocation.memory.take().map(std::sync::Arc::new);
    let units = std::mem::take(&mut allocation.frames)
        .into_iter()
        .map(|bytes| match &memory {
            Some(lease) => {
                Bytes::from_owner(crate::ceres::snapshot::content_budget::BudgetedFrame {
                    bytes,
                    lease: lease.clone(),
                })
            }
            None => Bytes::from(bytes),
        })
        .collect::<std::collections::VecDeque<_>>();
    let stream = futures::stream::unfold(
        (units, state.clone(), context.clone(), headers),
        |(mut units, state, context, headers)| async move {
            let unit = units.pop_front()?;
            if let Err(error) = revalidate_access(&state, &context, &headers).await {
                units.clear();
                return Some((Err(error), (units, state, context, headers)));
            }
            Some((Ok(unit), (units, state, context, headers)))
        },
    );
    treeframe_response_body(
        snapshot_id,
        request_body,
        axum::body::Body::from_stream(stream),
    )
}

tokio::task_local! {
    /// Per-request id for the error envelope (spec 14 §5). Sourced from the
    /// global `TraceContext` so the envelope, the `X-Request-Id` response
    /// header and log spans all carry the same id.
    static REQUEST_ID: String;
    static REQUEST_CONTEXT: Option<crate::ceres::snapshot::runtime::SnapshotContext>;
    static REQUEST_HEADERS: HeaderMap;
}

#[cfg(test)]
tokio::task_local! {
    static NATIVE_RESOLVE_BARRIERS: (std::sync::Arc<tokio::sync::Barrier>, std::sync::Arc<tokio::sync::Barrier>);
    static NATIVE_HANDOFF_BARRIERS: (std::sync::Arc<tokio::sync::Barrier>, std::sync::Arc<tokio::sync::Barrier>);
    static REJECT_NATIVE_OBSERVATION_SOURCE: bool;
    static GENERIC_HISTORY_RESOLVE: bool;
}

#[cfg(test)]
pub(crate) async fn with_rejected_native_observation_source<F: std::future::Future>(
    future: F,
) -> F::Output {
    REJECT_NATIVE_OBSERVATION_SOURCE.scope(true, future).await
}

#[cfg(test)]
pub(crate) async fn with_native_resolve_barriers<F: std::future::Future>(
    captured: std::sync::Arc<tokio::sync::Barrier>,
    release: std::sync::Arc<tokio::sync::Barrier>,
    future: F,
) -> F::Output {
    NATIVE_RESOLVE_BARRIERS
        .scope((captured, release), future)
        .await
}

#[cfg(test)]
pub(crate) async fn with_native_handoff_barriers<F: std::future::Future>(
    prepared: std::sync::Arc<tokio::sync::Barrier>,
    release: std::sync::Arc<tokio::sync::Barrier>,
    future: F,
) -> F::Output {
    NATIVE_HANDOFF_BARRIERS
        .scope((prepared, release), future)
        .await
}

/// The request id of the in-flight request, for error envelopes.
pub(crate) fn current_request_id() -> String {
    REQUEST_ID.try_with(|id| id.clone()).unwrap_or_default()
}

const LEASE_HEADER: &str = "x-mega-snapshot-lease";

/// Spec 04 §1 authentication, applied once over the whole snapshot router:
/// every endpoint except `capabilities` requires `Authorization: Bearer
/// <token>` when the deployment configured one, and snapshot-bound endpoints
/// must additionally present the lease they resolved
/// (`X-Mega-Snapshot-Lease`), validated against the session authority —
/// knowing the snapshot id alone is not a capability.
async fn snapshot_auth_middleware(
    State(state): State<MonoApiServiceState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Nested routes may be added after the server's trace layer. Reuse an
    // existing context or establish one here, including rejection responses.
    let id = req
        .extensions()
        .get::<crate::server::trace_context::TraceContext>()
        .map(|c| c.trace_id.clone())
        .unwrap_or_else(|| crate::server::trace_context::resolve_trace_id(req.headers()));
    req.extensions_mut()
        .insert(crate::server::trace_context::TraceContext {
            trace_id: id.clone(),
        });
    let mut response = REQUEST_ID
        .scope(id.to_string(), async {
            let headers = req.headers().clone();
            let path = req.uri().path().to_owned();
            let context = match authenticate_request(&state, &headers, &path).await {
                Ok(context) => context,
                Err(error) => return mst2_error_response(error),
            };
            REQUEST_HEADERS
                .scope(
                    headers.clone(),
                    REQUEST_CONTEXT.scope(context.clone(), async {
                        let response = next.run(req).await;
                        if response.status().is_success()
                            || response.status() == StatusCode::NOT_MODIFIED
                        {
                            match context {
                                Some(context) => {
                                    if let Err(error) =
                                        revalidate_access(&state, &context, &headers).await
                                    {
                                        return mst2_error_response(error);
                                    }
                                }
                                None => {
                                    if let Err(error) =
                                        authenticate_request(&state, &headers, &path).await
                                    {
                                        return mst2_error_response(error);
                                    }
                                }
                            }
                        }
                        response
                    }),
                )
                .await
        })
        .await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Reject an oversized declared length before consuming any body. Actual
/// bytes and the overall read deadline are checked by `Mst2Bytes`.
async fn reject_oversize_body(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let over = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len > JSON_REQUEST_LIMIT);
    if over {
        return mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "request body over the spec 14 limit",
        ));
    }
    next.run(req).await
}

/// Parse a JSON request body, mapping shape errors to the typed envelope
/// (spec 14 §5 INVALID_REQUEST). Size is enforced by the router layers.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_json_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    request::validate_json_keys(body)
        .and_then(|()| serde_json::from_slice(body))
        .map_err(|e| {
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::InvalidRequest,
                format!("malformed request body: {e}"),
            ))
        })
}

/// Bearer-token check shared by the auth middleware; the parsing rule is the
/// one the Git HTTP receive-pack path uses.
fn bearer_ok(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::api::oauth::bearer_token_from_authorization_value)
        .is_some_and(|cred| cred == token)
}

fn unauthenticated(message: &'static str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Unauthenticated, message)
}

/// Auth decision for one request path (see [`snapshot_auth_middleware`]).
/// `capabilities` stays open; lease routes need only the bearer; a
/// snapshot-bound route (`/snapshots/sha256:…/…`) also needs its lease.
async fn authenticate_request(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
    path: &str,
) -> Result<Option<crate::ceres::snapshot::runtime::SnapshotContext>, SnapshotError> {
    let config = state.storage.config();
    let token = config.mst2.auth_token.as_deref();
    let path = path.split('?').next().unwrap_or(path);
    // The router is nested under /api/v2, which strips its prefix before
    // middleware runs — accept both the stripped and full forms.
    let rest = path
        .strip_prefix("/api/v2/snapshots/")
        .or_else(|| path.strip_prefix("/snapshots/"));
    let Some(rest) = rest else {
        return Ok(None);
    };
    if rest == "capabilities" {
        return Ok(None);
    }
    if let Some(token) = token
        && !bearer_ok(headers, token)
    {
        return Err(unauthenticated("missing or invalid bearer credentials"));
    }
    let mut segments = rest.split('/');
    match segments.next() {
        // Lease management names the lease in the path, not a snapshot.
        Some("resolve") | Some("leases") | Some("capabilities") | None => Ok(None),
        Some(snapshot_id) => {
            let lease = headers.get(LEASE_HEADER).and_then(|v| v.to_str().ok());
            match lease {
                None => Err(unauthenticated("missing X-Mega-Snapshot-Lease header")),
                Some(lease) => state
                    .storage
                    .snapshot_context(snapshot_id, lease)
                    .await
                    .map(Some),
            }
        }
    }
}

fn request_context(
    state: &MonoApiServiceState,
    snapshot_id: &str,
) -> Result<crate::ceres::snapshot::runtime::SnapshotContext, SnapshotError> {
    if let Ok(Some(context)) = REQUEST_CONTEXT.try_with(Clone::clone)
        && context.built.snapshot_id == snapshot_id
    {
        return Ok(context);
    }
    if !state.storage.config().mst2.publication_enabled {
        return runtime().context(snapshot_id);
    }
    Err(SnapshotError::new(
        SnapshotErrorCode::Unauthenticated,
        "validated snapshot session missing",
    ))
}

async fn revalidate_access(
    state: &MonoApiServiceState,
    context: &crate::ceres::snapshot::runtime::SnapshotContext,
    headers: &HeaderMap,
) -> Result<(), SnapshotError> {
    let config = state.storage.config();
    if !config.mst2.enabled {
        return Err(SnapshotError::new(
            SnapshotErrorCode::SnapshotNotReady,
            "snapshot surface disabled",
        ));
    }
    if let Some(token) = config.mst2.auth_token.as_deref()
        && !bearer_ok(headers, token)
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::Unauthenticated,
            "bearer credentials changed",
        ));
    }
    let current = state
        .storage
        .snapshot_context(&context.built.snapshot_id, &context.lease_id)
        .await?;
    if current.built.descriptor != context.built.descriptor
        || current.commit_oid != context.commit_oid
        || current.root_tree_oid != context.root_tree_oid
        || current.authorization_epoch != context.authorization_epoch
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "fixed session changed during request",
        ));
    }
    Ok(())
}

async fn revalidate_request(
    state: &MonoApiServiceState,
    context: &crate::ceres::snapshot::runtime::SnapshotContext,
) -> Result<(), SnapshotError> {
    let headers = REQUEST_HEADERS.try_with(Clone::clone).map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::Unauthenticated,
            "request authentication context missing",
        )
    })?;
    revalidate_access(state, context, &headers).await
}

fn mst2_error_response(err: SnapshotError) -> Response {
    let status =
        StatusCode::from_u16(err.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({
            "error": {
                "code": err.code.as_str(),
                "message": err.message,
                "request_id": current_request_id(),
                "retryable": matches!(
                    err.code,
                    SnapshotErrorCode::SnapshotNotReady
                        | SnapshotErrorCode::MetadataNotReady
                        | SnapshotErrorCode::TemporaryUnavailable
                        | SnapshotErrorCode::Internal
                ),
            }
        })),
    )
        .into_response()
}

impl From<SnapshotError> for Response {
    fn from(err: SnapshotError) -> Self {
        mst2_error_response(err)
    }
}

impl IntoResponse for SnapshotError {
    fn into_response(self) -> Response {
        mst2_error_response(self)
    }
}

/// MegaError → snapshot error: storage/lease failures must surface as errors,
/// never as absence (spec 00 SYS-04).
fn internal<E: std::fmt::Display>(e: E) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, e.to_string())
}

async fn capabilities() -> Json<serde_json::Value> {
    // Keep this document on the canonical discovery contract consumed by the
    // v3 reader.  The full delivery path serves the complete metadata/object
    // closure, so advertising `full_hydration` as false would make a typed
    // resolve reject before it reaches this router.
    Json(json!({
        "protocol_versions": [2],
        "metadata_codecs": [1],
        "frame_encodings": ["identity"],
        "features": {
            "strict_publication": true,
            "directory": true,
            "lookup": true,
            "metadata_pages": true,
            "raw_blob": true,
            "small_objects": true,
            "chunk_reads": true,
            "full_hydration": true,
            "region_hints": false,
            "offline_export": false,
        },
        "limits": {
            "max_file_bytes": "8796093022208",
            "max_path_bytes": 4096,
            "max_path_components": 256,
            "metadata_page_bytes": 16384,
            "metadata_leaf_entries": 128,
            "max_json_request_bytes": 131072,
            "max_json_response_bytes": 1048576,
            "max_directory_entries": 256,
            "max_request_items": 128,
            "max_metadata_items": 64,
            "small_object_bytes": 262144,
            "small_batch_bytes": 8388608,
            "object_frame_raw_bytes": 1048576,
            "chunk_frame_raw_bytes": 1048652,
            "frame_wire_bytes": 2097152,
            "zstd_window_bytes": 8388608,
            "chunk_size": 1048576,
            "chunk_batch_bytes": 134217728
        }
    }))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ResolveTarget {
    kind: String,
    #[serde(default)]
    view_id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ResolveRequest {
    target: ResolveTarget,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default = "default_delivery")]
    delivery: String,
    #[serde(default = "default_lease_seconds")]
    lease_seconds: u64,
    #[serde(default = "default_codecs")]
    #[allow(dead_code)]
    supported_metadata_codecs: Vec<u16>,
}

fn default_scope() -> String {
    "/".to_string()
}
fn default_delivery() -> String {
    "full".to_string()
}
fn default_lease_seconds() -> u64 {
    600
}
fn default_codecs() -> Vec<u16> {
    vec![1]
}

fn ensure_enabled(state: &MonoApiServiceState) -> Result<(), SnapshotError> {
    if state.storage.config().mst2.enabled {
        Ok(())
    } else {
        Err(SnapshotError::new(
            SnapshotErrorCode::SnapshotNotReady,
            "mst2 surface is disabled on this deployment",
        ))
    }
}

// Both arms are `Response`, so boxing only the `Err` arm would not shrink
// the returned value — the `Ok` arm carries the same 128 bytes. Unlike
// `lfs_router::enforce_lfs_access` (where the error is the rare arm and is
// boxed), there is nothing to gain here, so the lint is allowed outright.
#[allow(clippy::result_large_err)]
async fn resolve(
    state: State<MonoApiServiceState>,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let req: ResolveRequest = parse_json_body(&body)?;
    // Unknown target kinds are client errors, never a silent fallback to
    // latest (review P1.3).
    if req.target.kind != "latest" && req.target.kind != "view" {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::InvalidRequest,
            format!("unknown target kind {:?}", req.target.kind),
        )));
    }
    if req.delivery != "full" {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "only delivery=full is served by this profile",
        )));
    }
    if !req.supported_metadata_codecs.contains(&1) {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "metadata codec 1 is required",
        )));
    }
    descriptor::validate_scope(&req.scope).map_err(|e| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            e.to_string(),
        ))
    })?;

    let config = state.storage.config();
    let mut native_source = None;
    let mut selected_native_head = None;
    let (commit_oid, tree_oid, sequence, writer_epoch) = if config.mst2.publication_enabled {
        let Some(instance) = config.mst2.instance_uuid.as_deref() else {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::SnapshotNotReady,
                "native publication instance missing",
            )));
        };
        let head = state
            .storage
            .mono_storage()
            .read_native_publication_head(instance)
            .await
            .map_err(|error| {
                tracing::error!(error = %error, "native publication observation failed");
                mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::SnapshotNotReady,
                    "native publication is not ready",
                ))
            })?;
        let observation_source = ObjectHash::from_hex_for_kind(get_hash_kind(), &head.root.commit)
            .and_then(|commit| {
                ObjectHash::from_hex_for_kind(get_hash_kind(), &head.root.tree)
                    .map(|tree| (commit, tree))
            })
            .map_err(|_| {
                SnapshotError::new(
                    SnapshotErrorCode::IntegrityError,
                    "invalid fixed native source identity",
                )
            })
            .and_then(|(commit, tree)| {
                let certificate = head.token.certificate;
                #[cfg(test)]
                let certificate = if REJECT_NATIVE_OBSERVATION_SOURCE
                    .try_with(|reject| *reject)
                    .unwrap_or(false)
                {
                    None
                } else {
                    certificate
                };
                NativeResolveSource::capture(
                    &head.instance_id,
                    commit,
                    tree,
                    certificate,
                    head.token.epoch,
                    head.token.sequence,
                )
            });
        native_source = match observation_source {
            Ok(source) => Some(source),
            Err(_) => {
                if let Some(sink) = &state.storage.projection_observation_sink {
                    sink.reject_binding();
                }
                tracing::warn!("native resolve observation source rejected");
                None
            }
        };
        selected_native_head = Some(head.clone());
        (
            head.root.commit,
            head.root.tree,
            head.token.sequence.to_string(),
            head.token.epoch.to_string(),
        )
    } else {
        let main = state
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::SnapshotNotReady,
                    "monorepo main ref missing",
                ))
            })?;
        let sequence = runtime()
            .publication_sequence(&main.ref_commit_hash)
            .to_string();
        (
            main.ref_commit_hash,
            main.ref_tree_hash,
            sequence,
            "1".to_owned(),
        )
    };

    #[cfg(test)]
    if let Ok((captured, release)) = NATIVE_RESOLVE_BARRIERS.try_with(|value| value.clone()) {
        captured.wait().await;
        release.wait().await;
    }
    let view = SnapshotView::from_commit(&commit_oid, &tree_oid);
    if let Some(want) = &req.target.view_id {
        let kind_matches = req.target.kind == "view";
        if !kind_matches || want != &view.view_id {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ViewNotFound,
                format!("requested view {} is not served by this deployment", want),
            )));
        }
    }

    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&tree_oid)
        .await
        .map_err(internal)?;
    // The scope root doubles as metadata_root; building it also validates
    // that the scope exists and is a directory in this view.
    let projection_started = std::time::Instant::now();
    #[cfg(test)]
    let prepared_generic = if selected_native_head.is_some()
        && GENERIC_HISTORY_RESOLVE
            .try_with(|value| *value)
            .unwrap_or(false)
    {
        let (page, work) = build_directory_page_with_work(handler.as_ref(), &root_tree, &req.scope)
            .await
            .map_err(mst2_error_response)?;
        let prepared = crate::ceres::snapshot::pages::prepare_native_metadata_retention(
            handler.as_ref(),
            &root_tree,
            &req.scope,
            crate::ceres::snapshot::retention_dag::MetadataDagLimits::default(),
        )
        .await
        .map_err(mst2_error_response)?;
        Some((page.page_id, work, prepared))
    } else {
        None
    };
    #[cfg(not(test))]
    let prepared_generic: Option<(
        [u8; 32],
        crate::ceres::snapshot::pages::ProjectionWork,
        crate::ceres::snapshot::pages::PreparedNativeMetadataRetention,
    )> = None;
    let (metadata_root, projection_work, prepared_rooted) = if let Some((root, work, _)) =
        prepared_generic.as_ref()
    {
        (*root, Some(work.clone()), None)
    } else if selected_native_head.is_some() {
        let repository = state
            .storage
            .rooted_qualified_metadata_writer()
            .await
            .map_err(internal)?;
        let prepared =
            crate::ceres::snapshot::rooted_metadata_projection::prepare_rooted_native_metadata(
                handler.as_ref(),
                &root_tree,
                &req.scope,
                repository,
            )
            .await
            .map_err(mst2_error_response)?;
        (prepared.plan.root, None, Some(prepared))
    } else {
        let (page, work) = build_directory_page_with_work(handler.as_ref(), &root_tree, &req.scope)
            .await
            .map_err(mst2_error_response)?;
        (page.page_id, Some(work), None)
    };
    let projection_elapsed = projection_started.elapsed();

    let built = build_descriptor(&config.mst2, &view, &req.scope, metadata_root)
        .map_err(mst2_error_response)?;
    let ctx = if let Some(head) = selected_native_head.as_ref() {
        use crate::jupiter::storage::qualified_metadata_family::SnapshotMetadataFamily;
        let family = state
            .storage
            .snapshot_metadata_family(&built.snapshot_id, false)
            .await
            .map_err(mst2_error_response)?;
        if family == Some(SnapshotMetadataFamily::Generic) || prepared_generic.is_some() {
            // A permanent SID route retains its original physical family.
            let existing = state
                .storage
                .snapshot_sessions()
                .await
                .open(head, &built, None, req.lease_seconds)
                .await
                .map_err(mst2_error_response)?;
            if let Some(existing) = existing {
                existing
            } else {
                let (_, _, prepared) = prepared_generic.as_ref().ok_or_else(|| {
                    mst2_error_response(internal("existing generic route has no durable session"))
                })?;
                let sessions = state.storage.snapshot_sessions().await;
                let receipt = sessions
                    .install(&built, prepared)
                    .await
                    .map_err(mst2_error_response)?;
                #[cfg(test)]
                if let Ok((prepared, release)) = NATIVE_HANDOFF_BARRIERS.try_with(Clone::clone) {
                    prepared.wait().await;
                    release.wait().await;
                }
                sessions
                    .open(head, &built, Some(&receipt), req.lease_seconds)
                    .await
                    .map_err(mst2_error_response)?
                    .ok_or_else(|| {
                        mst2_error_response(internal(
                            "generic history fixture handoff returned no context",
                        ))
                    })?
            }
        } else {
            let repository = state
                .storage
                .rooted_qualified_metadata_writer()
                .await
                .map_err(internal)?;
            if let Some(context) = repository
                .open_session(head, &built, None, req.lease_seconds)
                .await
                .map_err(mst2_error_response)?
            {
                context
            } else {
                let prepared = prepared_rooted.as_ref().ok_or_else(|| {
                    mst2_error_response(internal("rooted resolve has no source projection"))
                })?;
                let receipt = repository
                    .install(&built, prepared)
                    .await
                    .map_err(crate::jupiter::storage::native_snapshot_session::install_error)
                    .map_err(mst2_error_response)?;
                #[cfg(test)]
                if let Ok((prepared, release)) =
                    NATIVE_HANDOFF_BARRIERS.try_with(|value| value.clone())
                {
                    prepared.wait().await;
                    release.wait().await;
                }
                repository
                    .open_session(head, &built, Some(&receipt), req.lease_seconds)
                    .await
                    .map_err(mst2_error_response)?
                    .ok_or_else(|| {
                        mst2_error_response(internal("durable session handoff returned no context"))
                    })?
            }
        }
    } else {
        runtime()
            .insert_context(built.clone(), &commit_oid, &tree_oid, req.lease_seconds)
            .map_err(mst2_error_response)?
    };

    if let Some(source) = native_source {
        let request_id = current_request_id();
        let resolved = ResolvedProjection {
            descriptor: &ctx.built.descriptor,
            snapshot_id: &ctx.built.snapshot_id,
            metadata_root: &ctx.built.metadata_root,
            context_commit: &ctx.commit_oid,
            context_root_tree: &ctx.root_tree_oid,
            fixed_root_tree: root_tree.id,
            requested_scope: &req.scope,
            request_id: &request_id,
        };
        let observation = if let Some(prepared) = prepared_rooted {
            source.observe_rooted(resolved, prepared.work, projection_elapsed)
        } else {
            source.observe(
                resolved,
                projection_work.unwrap_or_default(),
                projection_elapsed,
            )
        };
        match observation {
            Ok(observation) => {
                if let Some(sink) = &state.storage.projection_observation_sink {
                    let _ = sink.enqueue(&observation);
                }
                observation.emit();
            }
            Err(_) => {
                if let Some(sink) = &state.storage.projection_observation_sink {
                    sink.reject_binding();
                }
                tracing::warn!("native resolve observation context rejected");
            }
        }
    }

    revalidate_request(&state, &ctx)
        .await
        .map_err(mst2_error_response)?;
    let body = json!({
        "descriptor": descriptor_json(&built),
        "publication_sequence": sequence,
        "writer_epoch": writer_epoch,
        "lease_id": ctx.lease_id,
        "lease_expires_at": crate::ceres::snapshot::runtime::rfc3339(ctx.lease_expires_at_unix),
        "authorization_epoch": "1",
        "resolved_at": crate::ceres::snapshot::runtime::rfc3339(now_unix()),
        "delivery": "full",
    });
    Ok(Json(body).into_response())
}

/// Spec 03 §2 descriptor JSON, built from the canonical descriptor bytes.
fn descriptor_json(
    built: &crate::ceres::snapshot::descriptor::BuiltDescriptor,
) -> serde_json::Value {
    json!({
        "schema_version": 2,
        "metadata_codec": 1,
        "instance_id": built.instance_id,
        "namespace_view_id": format!("sha256:{}", hex_of(&built.descriptor.namespace_view_id)),
        "scope": built.descriptor.scope,
        "materialization_policy": 1,
        "fs_semantics": 1,
        "access_projection": 0,
        "metadata_root": built.metadata_root,
        "snapshot_id": built.snapshot_id,
    })
}

#[allow(clippy::result_large_err)]
async fn descriptor_get(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let ctx = request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    // Reading the descriptor back never touches latest (spec 04 §2).
    let body = json!({
        "snapshot_id": snapshot_id,
        "descriptor": descriptor_json(&ctx.built),
        "lease_id": ctx.lease_id,
        "lease_expires_at":
            crate::ceres::snapshot::runtime::rfc3339(ctx.lease_expires_at_unix),
    });
    Ok(Json(body).into_response())
}

#[derive(Default, Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
struct RenewRequest {
    lease_seconds: Option<u64>,
}

#[allow(clippy::result_large_err)]
async fn lease_renew(
    state: State<MonoApiServiceState>,
    AxumPath(lease_id): AxumPath<String>,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let req: RenewRequest = if body.is_empty() {
        RenewRequest::default()
    } else {
        parse_json_body(&body)?
    };
    let renewed = state
        .storage
        .snapshot_renew(&lease_id, req.lease_seconds.unwrap_or(600))
        .await
        .map_err(mst2_error_response)?;
    // Renewal never changes the version or authorization (spec 04 §4).
    let body = json!({
        "lease_id": renewed.lease_id,
        "snapshot_id": renewed.snapshot_id,
        "lease_expires_at":
            crate::ceres::snapshot::runtime::rfc3339(renewed.expires_at_unix),
    });
    Ok(Json(body).into_response())
}

#[allow(clippy::result_large_err)]
async fn lease_release(
    state: State<MonoApiServiceState>,
    AxumPath(lease_id): AxumPath<String>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    // Idempotent: releasing an unknown/already-released lease still succeeds
    // (spec 04 §2). This never deletes Git content.
    let released = state
        .storage
        .snapshot_release(&lease_id)
        .await
        .map_err(mst2_error_response)?;
    Ok(Json(json!({ "lease_id": lease_id, "released": released })).into_response())
}

#[derive(Deserialize, Debug)]
struct DirectoryQuery {
    #[serde(default = "default_scope")]
    path: String,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    ancestors: Option<String>,
}

#[path = "snapshot_rooted_metadata.rs"]
mod rooted_metadata;

fn default_limit() -> u32 {
    128
}

// Both arms are `Response`, so boxing only the `Err` arm would not shrink
// the returned value — the `Ok` arm carries the same 128 bytes. Unlike
// `lfs_router::enforce_lfs_access` (where the error is the rare arm and is
// boxed), there is nothing to gain here, so the lint is allowed outright.
#[allow(clippy::result_large_err)]
async fn directory(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<DirectoryQuery>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let ctx = request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    if !(1..=256).contains(&q.limit) {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "limit must be 1..256",
        )));
    }
    if state.storage.config().mst2.publication_enabled
        && state
            .storage
            .snapshot_metadata_family(&ctx.lease_id, true)
            .await
            .map_err(mst2_error_response)?
            == Some(
                crate::jupiter::storage::qualified_metadata_family::SnapshotMetadataFamily::Rooted,
            )
    {
        return rooted_metadata::directory_response(&state, &ctx, &snapshot_id, &q).await;
    }

    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;

    // Spec 04 §1: request paths are scope-relative; "/" is the descriptor
    // scope itself. Map onto absolute view paths before resolving.
    let scope = &ctx.built.descriptor.scope;
    let abs_path = if q.path == "/" {
        scope.clone()
    } else {
        format!("{}{}", scope.trim_end_matches('/'), q.path)
    };

    let built = build_directory_page(handler.as_ref(), &root_tree, &abs_path)
        .await
        .map_err(mst2_error_response)?;

    // Entries are byte-sorted; the cursor pins (snapshot, path, limit) and the
    // last returned name, MAC'd so clients cannot tamper (spec 04 §6).
    let mut start: usize = 0;
    let mut last_name: Option<String> = None;
    if let Some(cur) = &q.cursor {
        let (payload_b64, sig) = cur.rsplit_once('.').ok_or_else(|| {
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::CursorInvalid,
                "malformed cursor",
            ))
        })?;
        let expected = runtime().sign_cursor(payload_b64);
        if sig != expected {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::CursorInvalid,
                "cursor signature mismatch",
            )));
        }
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload_b64)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .ok_or_else(|| {
                mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::CursorInvalid,
                    "cursor payload undecodable",
                ))
            })?;
        if payload["s"].as_str() != Some(snapshot_id.as_str())
            || payload["p"].as_str() != Some(abs_path.as_str())
            || payload["l"].as_u64() != Some(q.limit as u64)
        {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::CursorInvalid,
                "cursor bound to different parameters; reopen pagination",
            )));
        }
        last_name = payload["a"].as_str().map(|s| s.to_string());
        start = built
            .entries
            .partition_point(|e| Some(&e.name) <= last_name.as_ref());
    }

    let total = built.entries.len();
    let window: Vec<&crate::ceres::snapshot::pages::DirEntry> = built
        .entries
        .iter()
        .skip(start)
        .take(q.limit as usize)
        .collect();
    let has_more = start + window.len() < total;

    let entries_json: Vec<serde_json::Value> = window
        .iter()
        .map(|e| {
            let mut o = json!({
                "name": e.name,
                "fs_kind": e.fs_kind.as_str(),
            });
            if let Some(size) = e.size {
                o["size"] = json!(size.to_string());
            }
            if let Some(d) = e.content_digest {
                o["content_digest"] = json!(format!("sha256:{}", hex_of(&d)));
            }
            if let Some(dr) = e.directory_root {
                o["directory_root"] = json!(format!("sha256:{}", hex_of(&dr)));
                o["node_class"] = json!("native_tree");
                o["lifecycle"] = json!("mutable");
            }
            o
        })
        .collect();

    let next_cursor = if has_more {
        let last = window.last().map(|e| e.name.clone()).ok_or_else(|| {
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::Internal,
                "empty window with more entries",
            ))
        })?;
        let payload = json!({
            "s": snapshot_id,
            "p": abs_path,
            "l": q.limit,
            "a": last,
        });
        let payload_b64 = base64_of(serde_json::to_vec(&payload).unwrap().as_slice());
        let sig = runtime().sign_cursor(&payload_b64);
        Some(json!(format!("{payload_b64}.{sig}")))
    } else {
        None
    };

    // range_start_exclusive must echo the cursor's last name (spec 04 §6).
    let range_start = if q.cursor.is_some() { last_name } else { None };
    let proofs = proof_pages(handler.as_ref(), &root_tree, &abs_path)
        .await
        .map_err(mst2_error_response)?;

    // Ancestor chain reuses the proof pages (same evidence, deduped).
    let ancestor_chain = if q.ancestors.as_deref() == Some("chain") {
        json!(
            proofs
                .iter()
                .filter(|(p, _, _)| p != &q.path)
                .map(|(p, pid, _)| json!({
                    "path": p,
                    "directory_root": format!("sha256:{}", hex_of(pid)),
                    "node_class": "native_tree",
                }))
                .collect::<Vec<_>>()
        )
    } else {
        json!([])
    };

    let body = json!({
        "snapshot_id": snapshot_id,
        "path": q.path,
        "metadata_root": ctx.built.metadata_root,
        "directory_root": format!("sha256:{}", hex_of(&built.page_id)),
        "node_class": "native_tree",
        "lifecycle": "mutable",
        "range_start_exclusive": range_start,
        "entries": entries_json,
        "entry_count": total.to_string(),
        "next_cursor": next_cursor,
        "proof_pages": proofs
            .iter()
            .map(|(_, pid, bytes)| json!({"digest": format!("sha256:{}", hex_of(pid)), "data_base64": base64_of(bytes)}))
            .collect::<Vec<_>>(),
        "ancestor_chain": ancestor_chain,
    });

    let mut resp = Json(body).into_response();
    // Strong ETag over this representation + no-transform (spec 04 §10).
    // The pagination parameters are part of the representation: an ETag
    // shared across limit/cursor choices would serve one window for another
    // (review P1.4).
    let etag = format!(
        "\"{}:{}:{}:{}\"",
        &snapshot_id[..16.min(snapshot_id.len())],
        hex_of(&built.page_id),
        q.limit,
        // The cursor is the server-signed token itself (base64 + MAC):
        // header-safe and already held by the client, so identity suffices.
        q.cursor.as_deref().unwrap_or(""),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        resp.headers_mut().insert("etag", v);
    }
    resp.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-cache, no-transform"),
    );
    Ok(resp)
}

/// Scope-relative request path -> absolute view path (spec 04 section 1).
fn abs_view_path(scope: &str, path: &str) -> String {
    if path == "/" {
        scope.to_string()
    } else {
        format!("{}{}", scope.trim_end_matches('/'), path)
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct LookupRequest {
    paths: Vec<String>,
    #[serde(default)]
    include_ancestors: bool,
}

#[allow(clippy::result_large_err, clippy::too_many_lines)]
async fn lookup(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    _headers: HeaderMap,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let req: LookupRequest = parse_json_body(&body)?;
    if req.paths.len() > 128 {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "at most 128 paths per lookup",
        )));
    }

    let ctx = request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    if state.storage.config().mst2.publication_enabled
        && state
            .storage
            .snapshot_metadata_family(&ctx.lease_id, true)
            .await
            .map_err(mst2_error_response)?
            == Some(
                crate::jupiter::storage::qualified_metadata_family::SnapshotMetadataFamily::Rooted,
            )
    {
        return rooted_metadata::lookup_response(&state, &ctx, &snapshot_id, &req).await;
    }

    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;

    let mut results = Vec::with_capacity(req.paths.len());
    let mut deepest_dirs: Vec<String> = Vec::new();
    for path in &req.paths {
        validate_scope_relative_path(path).map_err(mst2_error_response)?;
        let abs_path = abs_view_path(&ctx.built.descriptor.scope, path);
        let mut entry = json!({"path": path});
        match resolve_abs_metadata(handler.as_ref(), &root_tree, &abs_path)
            .await
            .map_err(mst2_error_response)?
        {
            MetadataWalkOutcome::FoundDir => {
                let built = build_directory_page(handler.as_ref(), &root_tree, &abs_path)
                    .await
                    .map_err(mst2_error_response)?;
                entry["status"] = json!("found");
                let mut node = json!({"fs_kind": "directory"});
                node["directory_root"] = json!(format!("sha256:{}", hex_of(&built.page_id)));
                node["node_class"] = json!("native_tree");
                node["lifecycle"] = json!("mutable");
                if path != "/"
                    && let Some(name) = path.rsplit('/').next()
                {
                    node["name"] = json!(name);
                }
                entry["node"] = node;
                deepest_dirs.push(abs_path);
            }
            MetadataWalkOutcome::FoundFile { fs_kind, oid } => {
                let fact =
                    content::verified_file_metadata(handler.as_ref(), fs_kind, oid, path, None)
                        .await?;
                entry["status"] = json!("found");
                let mut node = json!({"fs_kind": fs_kind.as_str()});
                if let Some(name) = path.rsplit('/').next() {
                    node["name"] = json!(name);
                }
                node["size"] = json!(fact.size.to_string());
                node["content_digest"] = json!(format!("sha256:{}", hex_of(&fact.digest)));
                entry["node"] = node;
            }
            MetadataWalkOutcome::Absent => {
                entry["status"] = json!("absent");
            }
            MetadataWalkOutcome::NotDirectory { symlink } => {
                entry["status"] = if symlink {
                    json!("symlink_traversal")
                } else {
                    json!("not_directory")
                };
            }
        }
        results.push(entry);
    }

    // Shared proof pages along the deepest found directories, deduped, with
    // the 1 MiB response budget of spec 04 sections 5/7. Dropping a proof
    // silently would answer "verified" with an incomplete set; the client
    // must switch to stepwise metadata/pages instead (spec 14 §5).
    let mut proof_pages_out = Vec::new();
    let mut budget: usize = 1_048_576;
    let mut seen_pages: Vec<[u8; 32]> = Vec::new();
    let mut budget_exceeded = false;
    deepest_dirs.sort();
    deepest_dirs.dedup();
    'dirs: for dir in deepest_dirs.iter().rev() {
        let proofs = proof_pages(handler.as_ref(), &root_tree, dir)
            .await
            .map_err(mst2_error_response)?;
        for (_, pid, bytes) in proofs.into_iter().rev() {
            if seen_pages.contains(&pid) {
                continue;
            }
            if bytes.len() > budget {
                budget_exceeded = true;
                break 'dirs;
            }
            budget -= bytes.len();
            seen_pages.push(pid);
            proof_pages_out.push(json!({
                "digest": format!("sha256:{}", hex_of(&pid)),
                "data_base64": base64_of(&bytes),
            }));
        }
    }
    if budget_exceeded {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ProofBudgetExceeded,
            "lookup proof pages exceed the response budget; use metadata/pages",
        )));
    }

    let body = json!({
        "snapshot_id": snapshot_id,
        "results": results,
        "proof_pages": proof_pages_out,
    });
    Ok(Json(body).into_response())
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct MetadataPagesRequest {
    items: Vec<MetadataPageItem>,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct MetadataPageItem {
    /// Scope-relative directory path ("/" = the scope itself).
    directory_path: String,
    /// Branch-child labels from that directory's MTP2 root (spec 04 §8).
    #[serde(default)]
    route: Vec<u8>,
    /// Digest of the page the route must reach.
    #[serde(default)]
    expected_digest: Option<String>,
}

/// POST `/{snapshot_id}/metadata/pages` — batch raw MTP2 pages (spec 04 §8).
///
/// Returns the exact unique page set as a META TreeFrame stream. Routes are
/// walked inside the canonical tree built from the fixed view, so a returned
/// page is byte-identical to the one its parent commits to; a page the view
/// does not contain is a proven-absence 404, never an empty page or a
/// digest-only shortcut past the scope check.
#[allow(clippy::result_large_err, clippy::too_many_lines)]
async fn metadata_pages(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    _headers: HeaderMap,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let req: MetadataPagesRequest = parse_json_body(&body)?;
    if req.items.is_empty() || req.items.len() > 64 {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "items must hold 1..64 entries",
        )));
    }
    for item in &req.items {
        validate_scope_relative_path(&item.directory_path).map_err(mst2_error_response)?;
    }
    let ctx = request_context(&state, &snapshot_id).map_err(mst2_error_response)?;

    let unique = if state.storage.config().mst2.publication_enabled {
        use crate::jupiter::storage::native_snapshot_session::MetadataRouteRequest;
        let items: Vec<_> = req
            .items
            .iter()
            .map(|item| MetadataRouteRequest {
                directory_path: &item.directory_path,
                route: &item.route,
                expected_digest: item.expected_digest.as_deref(),
            })
            .collect();
        let batch = state
            .storage
            .snapshot_metadata_routes(&ctx, &items)
            .await
            .map_err(mst2_error_response)?;
        tracing::debug!(
            page_queries = batch.work.page_queries,
            pages_loaded = batch.work.pages_loaded,
            payload_bytes = batch.work.payload_bytes,
            walk_visits = batch.work.walk_visits,
            edge_references_checked = batch.work.edge_references_checked,
            "served persisted generic metadata routes"
        );
        batch.pages
    } else {
        let handler = state
            .api_handler(std::path::Path::new("/"))
            .await
            .map_err(internal)?;
        let root_tree = handler
            .get_tree_by_hash(&ctx.root_tree_oid)
            .await
            .map_err(internal)?;
        let scope = &ctx.built.descriptor.scope;
        let mut unique: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        let mut seen: Vec<[u8; 32]> = Vec::new();
        for item in &req.items {
            let abs_path = abs_view_path(scope, &item.directory_path);
            let built = build_directory_page(handler.as_ref(), &root_tree, &abs_path)
                .await
                .map_err(mst2_error_response)?;
            let pages =
                mst2_codec::metapage::Page::pages_along_route(&built.codec_entries, &item.route)
                    .map_err(|e| match e {
                        // A label the fixed view does not have is proven absence.
                        mst2_codec::CodecError::BadOrdering(m) => SnapshotError::new(
                            SnapshotErrorCode::PathNotFound,
                            format!("{}: route does not resolve ({m})", item.directory_path),
                        ),
                        other => SnapshotError::new(
                            SnapshotErrorCode::Internal,
                            format!("{}: route walk failed ({other})", item.directory_path),
                        ),
                    })?;
            let last = pages
                .last()
                .expect("pages_along_route returns at least the root page");
            let last_id = mst2_codec::metapage::page_id(last);
            if let Some(expected) = &item.expected_digest
                && expected != &format!("sha256:{}", hex_of(&last_id))
            {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    format!(
                        "{}: route does not reach expected_digest",
                        item.directory_path
                    ),
                )));
            }
            for page in pages {
                let id = mst2_codec::metapage::page_id(&page);
                if !seen.contains(&id) {
                    seen.push(id);
                    unique.push((id, page));
                }
            }
        }
        unique
    };
    let page_count = unique.len();
    let logical_bytes = unique.iter().map(|(_, page)| page.len() as u64).sum();

    // Frames hold at most 64 pages and at most 1 MiB of raw payload (spec 06),
    // so a wide route set becomes several META frames rather than one
    // oversized one. Encoding (identity/zstd) is negotiated per request.
    let encoding = req
        .encoding
        .as_deref()
        .map(crate::ceres::snapshot::frame_stream::Encoding::parse)
        .transpose()
        .map_err(mst2_error_response)?
        .unwrap_or(crate::ceres::snapshot::frame_stream::Encoding::Identity);
    use crate::ceres::snapshot::frame_stream::FrameStream;
    let mut stream = FrameStream::new(1, encoding);
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut frame: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut frame_raw: usize = 0;
    let mut flush = |frame: &mut Vec<([u8; 32], Vec<u8>)>,
                     raw: &mut usize,
                     out: &mut Vec<Vec<u8>>|
     -> Result<(), SnapshotError> {
        if frame.is_empty() {
            return Ok(());
        }
        let bytes = stream.meta(std::mem::take(frame))?;
        out.push(bytes);
        *raw = 0;
        Ok(())
    };
    for (id, page) in unique {
        // Per-page raw cost: 32-byte page_id + 4-byte length + bytes.
        if !frame.is_empty()
            && (frame.len() >= mst2_codec::treeframe::META_MAX_PAGES
                || frame_raw + 36 + page.len() > mst2_codec::treeframe::META_MAX_RAW)
        {
            flush(&mut frame, &mut frame_raw, &mut out).map_err(mst2_error_response)?;
        }
        frame_raw += 36 + page.len();
        frame.push((id, page));
    }
    flush(&mut frame, &mut frame_raw, &mut out).map_err(mst2_error_response)?;

    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, &body);
    let request_body_sha256: [u8; 32] = sha2::Digest::finalize(hasher).into();
    let end = stream.end(
        req.items.len() as u32,
        u32::try_from(page_count).unwrap_or(u32::MAX),
        logical_bytes,
        request_body_sha256,
    );
    out.push(end);

    guarded_treeframe_response(&state, &ctx, &snapshot_id, &body, out).map_err(mst2_error_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capabilities_advertise_the_canonical_full_delivery_contract() {
        let Json(value) = capabilities().await;
        assert_eq!(
            value,
            json!({
                "protocol_versions": [2],
                "metadata_codecs": [1],
                "frame_encodings": ["identity"],
                "features": {
                    "strict_publication": true,
                    "directory": true,
                    "lookup": true,
                    "metadata_pages": true,
                    "raw_blob": true,
                    "small_objects": true,
                    "chunk_reads": true,
                    "full_hydration": true,
                    "region_hints": false,
                    "offline_export": false,
                },
                "limits": {
                    "max_file_bytes": "8796093022208",
                    "max_path_bytes": 4096,
                    "max_path_components": 256,
                    "metadata_page_bytes": 16384,
                    "metadata_leaf_entries": 128,
                    "max_json_request_bytes": 131072,
                    "max_json_response_bytes": 1048576,
                    "max_directory_entries": 256,
                    "max_request_items": 128,
                    "max_metadata_items": 64,
                    "small_object_bytes": 262144,
                    "small_batch_bytes": 8388608,
                    "object_frame_raw_bytes": 1048576,
                    "chunk_frame_raw_bytes": 1048652,
                    "frame_wire_bytes": 2097152,
                    "zstd_window_bytes": 8388608,
                    "chunk_size": 1048576,
                    "chunk_batch_bytes": 134217728
                }
            })
        );
    }

    #[test]
    fn treeframe_response_emits_protocol_identity_headers() {
        let snapshot_id = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let request_body = br#"{"items":[], "encoding":"identity"}"#;
        let response = treeframe_response(snapshot_id, request_body, vec![1, 2, 3]).unwrap();

        assert_eq!(response.headers()["content-type"], TREEFRAME_MEDIA_TYPE);
        assert_eq!(response.headers()["x-mega-snapshot-id"], snapshot_id);
        let digest: [u8; 32] = Sha256::digest(request_body).into();
        assert_eq!(
            response.headers()["x-mega-request-digest"],
            format!("sha256:{}", hex_of(&digest))
        );
        assert_eq!(
            response.headers()["cache-control"],
            "private, no-cache, no-transform"
        );
        assert_eq!(response.headers()["vary"], "Authorization, Accept");
    }
}
