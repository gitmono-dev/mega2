use axum::{
    Json,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::StreamExt;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::AGENT_CAPTURE_TAG},
    api_model::agent_capture::{
        AgentCaptureHttpError, Completeness, DiscoveryResponse, ErrorEnvelope, PageEnvelope,
        SessionKind, SessionPutRequest, SessionView, bearer_token, decode_repo_path_segment,
        fingerprint_session_put_body,
    },
    ceres::agent_capture::auth::{lookup_ingest_token, token_covers_capture_repo},
    common::{canonical_json, errors::MegaError},
    config::AgentCaptureIngestTokenConfig,
    jupiter::storage::agent_capture_storage::{
        AgentCaptureStorage, InsertCheckpoint, InsertEvent, InsertFileOp, SessionNaturalKey,
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectNamespace},
};

const SESSION_PUT_OPERATION: &str = "session.put";
const SESSION_PUT_MAX_BYTES: usize = 64 * 1024;

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
        .layer(DefaultBodyLimit::disable())
}

fn require_ingest_token(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
) -> Result<AgentCaptureIngestTokenConfig, AgentCaptureHttpError> {
    let presented = bearer_token(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )?;
    lookup_ingest_token(
        &state.storage.config().agent_capture.ingest_tokens,
        presented,
    )
    .cloned()
    .ok_or_else(AgentCaptureHttpError::unauthorized)
}

fn receipt_error(err: MegaError) -> AgentCaptureHttpError {
    if err.to_string().contains("fingerprint conflict") {
        AgentCaptureHttpError::conflict()
    } else {
        AgentCaptureHttpError::bad_request(err.to_string())
    }
}

fn session_put_path(path: &str) -> Result<(String, String), AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "repos") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    let repo = segments.get(idx + 1).copied().filter(|s| !s.is_empty());
    let sessions = segments.get(idx + 2).copied();
    let client = segments.get(idx + 3).copied().filter(|s| !s.is_empty());
    let extra = segments.get(idx + 4);
    match (repo, sessions, client, extra) {
        (Some(repo), Some("sessions"), Some(client), None) => {
            Ok((repo.to_owned(), client.to_owned()))
        }
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn decode_client_session_id(segment: &str) -> Result<String, AgentCaptureHttpError> {
    percent_decode_str(segment)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid client_session_id encoding"))
}

fn blob_http_error(err: MegaError) -> AgentCaptureHttpError {
    let text = err.to_string();
    if text.contains("payload too large") {
        AgentCaptureHttpError::payload_too_large("blob exceeds max_blob_bytes")
    } else if text.contains("lease conflict") {
        AgentCaptureHttpError::conflict()
    } else if text.contains("does not exist") {
        AgentCaptureHttpError::not_found()
    } else {
        AgentCaptureHttpError::bad_request(text)
    }
}

fn session_staging_path(path: &str) -> Result<i64, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
        segments.get(idx + 4),
    ) {
        (Some(id), Some(&"blobs"), Some(&"staging"), None) => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn session_events_batch_path(path: &str) -> Result<i64, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
    ) {
        (Some(id), Some(&"events:batch"), None) => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn session_file_ops_batch_path(path: &str) -> Result<i64, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
    ) {
        (Some(id), Some(&"file-ops:batch"), None) => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn session_checkpoints_post_path(path: &str) -> Result<i64, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
    ) {
        (Some(id), Some(&"checkpoints"), None) => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn repo_sessions_list_path(path: &str) -> Result<String, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "repos") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
    ) {
        (Some(repo), Some(&"sessions"), None) if !repo.is_empty() => Ok((*repo).to_owned()),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn session_resource_path(path: &str, resource: Option<&str>) -> Result<i64, AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
    ) {
        (Some(id), None, None) if resource.is_none() => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        (Some(id), Some(name), None) if resource == Some(*name) => id
            .parse()
            .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id")),
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn pagination_from_uri(uri: &axum::http::Uri) -> Result<(u32, Option<i64>), AgentCaptureHttpError> {
    let mut limit = crate::api_model::agent_capture::DEFAULT_PAGE_LIMIT;
    let mut cursor = None;
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "limit" => {
                    let parsed: u32 = value
                        .parse()
                        .map_err(|_| AgentCaptureHttpError::bad_request("invalid limit"))?;
                    limit = parsed.min(crate::api_model::agent_capture::MAX_PAGE_LIMIT);
                }
                "cursor" if !value.is_empty() => {
                    cursor = Some(
                        value
                            .parse()
                            .map_err(|_| AgentCaptureHttpError::bad_request("invalid cursor"))?,
                    );
                }
                _ => {}
            }
        }
    }
    Ok((limit, cursor))
}

fn session_kind_from_stored(kind: &str) -> Result<SessionKind, AgentCaptureHttpError> {
    match kind {
        "external_capture" => Ok(SessionKind::ExternalCapture),
        "internal_code" => Ok(SessionKind::InternalCode),
        _ => Err(AgentCaptureHttpError::bad_request("invalid session_kind")),
    }
}

fn completeness_from_stored(value: &str) -> Result<Completeness, AgentCaptureHttpError> {
    match value {
        "empty" => Ok(Completeness::Empty),
        "incomplete" => Ok(Completeness::Incomplete),
        "complete" => Ok(Completeness::Complete),
        "truncated" => Ok(Completeness::Truncated),
        _ => Err(AgentCaptureHttpError::bad_request("invalid completeness")),
    }
}

fn session_view(
    row: crate::callisto::agent_capture_session::Model,
) -> Result<SessionView, AgentCaptureHttpError> {
    Ok(SessionView {
        capture_id: row.id,
        client_session_id: row.client_session_id,
        tenant_id: row.tenant_id,
        deployment_id: row.deployment_id,
        repo_id: row.repo_id,
        producer_id: row.producer_id,
        session_kind: session_kind_from_stored(&row.session_kind)?,
        started_at: row.started_at.map(|ts| ts.with_timezone(&chrono::Utc)),
        ended_at: row.ended_at.map(|ts| ts.with_timezone(&chrono::Utc)),
        completeness: completeness_from_stored(&row.completeness)?,
        partial_reason: row.partial_reason,
        created_at: row.created_at.with_timezone(&chrono::Utc),
        updated_at: row.updated_at.with_timezone(&chrono::Utc),
    })
}

fn missing_raw() -> AgentCaptureHttpError {
    AgentCaptureHttpError {
        status: StatusCode::CONFLICT,
        envelope: ErrorEnvelope::new("missing_raw", "committed raw transcript is missing"),
    }
}

async fn reject_if_tombstoned_capture(
    state: &MonoApiServiceState,
    capture_id: i64,
) -> Result<(), AgentCaptureHttpError> {
    let capture = state.storage.config().agent_capture.clone();
    let tombstoned = state
        .storage
        .agent_capture_service
        .storage
        .is_tombstoned(&capture.deployment_id, &capture.tenant_id, capture_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if tombstoned {
        Err(AgentCaptureHttpError::conflict())
    } else {
        Ok(())
    }
}

async fn collect_object_bytes(mut stream: ObjectByteStream) -> Result<Vec<u8>, MegaError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| MegaError::Other(err.to_string()))?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_event_uid(uid: &str) -> Result<(i64, i64), AgentCaptureHttpError> {
    let Some((generation, offset)) = uid.split_once(':') else {
        return Err(AgentCaptureHttpError::bad_request("invalid event_uid"));
    };
    if generation.is_empty()
        || offset.is_empty()
        || !generation.bytes().all(|b| b.is_ascii_digit())
        || !offset.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(AgentCaptureHttpError::bad_request("invalid event_uid"));
    }
    let generation = generation
        .parse::<i64>()
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid event_uid"))?;
    let offset = offset
        .parse::<i64>()
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid event_uid"))?;
    Ok((generation, offset))
}

fn file_ops_http_error(err: MegaError) -> AgentCaptureHttpError {
    match &err {
        MegaError::Other(msg) if msg.starts_with("ingest receipt fingerprint conflict") => {
            AgentCaptureHttpError::conflict()
        }
        MegaError::Other(msg)
            if msg.starts_with("agent_capture_session ") && msg.ends_with(" does not exist") =>
        {
            AgentCaptureHttpError::not_found()
        }
        _ => AgentCaptureHttpError::bad_request(err.to_string()),
    }
}

fn checkpoint_http_error(err: MegaError) -> AgentCaptureHttpError {
    match &err {
        MegaError::Other(msg) if msg.starts_with("ingest receipt fingerprint conflict") => {
            AgentCaptureHttpError::conflict()
        }
        MegaError::Other(msg)
            if msg.starts_with("agent_capture_session ") && msg.ends_with(" does not exist") =>
        {
            AgentCaptureHttpError::not_found()
        }
        _ => AgentCaptureHttpError::bad_request(err.to_string()),
    }
}

fn is_file_op_v1(op: &str) -> bool {
    matches!(op, "read" | "write" | "patch" | "delete" | "search")
}

fn is_absolute_file_op_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn validate_file_op_path(path: &str) -> Result<(), AgentCaptureHttpError> {
    if path.is_empty()
        || path.contains('\0')
        || path.contains("..")
        || is_absolute_file_op_path(path)
    {
        return Err(AgentCaptureHttpError::bad_request("invalid file_op path"));
    }
    Ok(())
}

fn events_http_error(err: MegaError) -> AgentCaptureHttpError {
    let text = err.to_string();
    if text.contains("fingerprint conflict") || text.contains("uid conflict") {
        AgentCaptureHttpError::conflict()
    } else if text.contains("does not exist") {
        AgentCaptureHttpError::not_found()
    } else {
        AgentCaptureHttpError::bad_request(text)
    }
}

fn session_finalize_path(path: &str) -> Result<(i64, String), AgentCaptureHttpError> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(idx) = segments.iter().position(|s| *s == "sessions") else {
        return Err(AgentCaptureHttpError::not_found());
    };
    match (
        segments.get(idx + 1),
        segments.get(idx + 2),
        segments.get(idx + 3),
        segments.get(idx + 4),
        segments.get(idx + 5),
    ) {
        (Some(id), Some(&"blobs"), Some(lease), Some(&"finalize"), None) => {
            let capture_id = id
                .parse()
                .map_err(|_| AgentCaptureHttpError::bad_request("invalid capture_id"))?;
            Ok((capture_id, (*lease).to_owned()))
        }
        _ => Err(AgentCaptureHttpError::not_found()),
    }
}

fn body_stream(body: axum::body::Body) -> ObjectByteStream {
    Box::pin(async_stream::stream! {
        let mut data = body.into_data_stream();
        while let Some(item) = data.next().await {
            match item {
                Ok(chunk) => yield Ok(chunk),
                Err(err) => yield Err(std::io::Error::other(err.to_string())),
            }
        }
    })
}

#[derive(Serialize, ToSchema)]
struct SessionPutResponse {
    capture_id: i64,
}

#[derive(Serialize, ToSchema)]
struct StagingBlobResponse {
    lease_id: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct FinalizeBlobRequest {
    digest: Option<String>,
    object_key: Option<String>,
    size: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct FinalizeBlobResponse {
    digest: String,
    object_key: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct EventsBatchRequest {
    batch_id: String,
    events: Vec<EventBatchItem>,
    completeness: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct EventBatchItem {
    event_uid: String,
    event_kind: String,
    native_id: Option<String>,
    lifecycle_seq: Option<i64>,
    payload: serde_json::Value,
}

#[derive(Serialize, ToSchema)]
struct EventsBatchResponse {
    accepted: bool,
}

const FILE_OP_SCHEMA_V1: &str = "agent.file_op.v1";
const FILE_OPS_BATCH_MAX_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct FileOpsBatchRequest {
    batch_id: String,
    #[serde(default)]
    schema: Option<String>,
    ops: Vec<FileOpItem>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct FileOpItem {
    source_event_uid: String,
    op: String,
    path: String,
    digest: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct FileOpsBatchResponse {
    accepted: bool,
}

const CHECKPOINT_POST_MAX_BYTES: usize = 64 * 1024;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CheckpointPostRequest {
    checkpoint_id: String,
    transcript_digest: Option<String>,
    redacted_digest: Option<String>,
    metadata: Option<serde_json::Value>,
}

#[derive(Serialize, ToSchema)]
struct CheckpointPostResponse {
    accepted: bool,
    completeness: String,
    partial_reason: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct CheckpointListItem {
    checkpoint_id: String,
    transcript_digest: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, ToSchema)]
struct FileOpListItem {
    id: i64,
    source_event_uid: String,
    op: String,
    path: String,
    created_at: chrono::DateTime<chrono::Utc>,
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
    require_ingest_token(&state, &headers)?;
    Ok(Json(DiscoveryResponse { raw_accepted: true }))
}

#[utoipa::path(
    put,
    path = "/repos/{repo}/sessions/{client_session_id}",
    params(
        ("repo" = String, Path, description = "Single percent-encoded repo path segment"),
        ("client_session_id" = String, Path, description = "Client session id")
    ),
    request_body = SessionPutRequest,
    responses(
        (status = 200, description = "Upserted session", body = SessionPutResponse),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Token does not cover repo", body = ErrorEnvelope),
        (status = 409, description = "Immutable fingerprint conflict", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn put_session(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<SessionPutResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let (repo_segment, client_session_segment) = session_put_path(parts.uri.path())?;
    let repo_id = decode_repo_path_segment(&repo_segment)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if !token_covers_capture_repo(&token, &repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let client_session_id = decode_client_session_id(&client_session_segment)?;
    let capture = state.storage.config().agent_capture.clone();
    if state
        .storage
        .agent_capture_service
        .storage
        .is_tombstoned_client_session(
            &capture.deployment_id,
            &capture.tenant_id,
            &repo_id,
            &token.name,
            &client_session_id,
        )
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
    {
        return Err(AgentCaptureHttpError::conflict());
    }
    let bytes = axum::body::to_bytes(body, SESSION_PUT_MAX_BYTES)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("session put body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    fingerprint_session_put_body(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let put_request: SessionPutRequest = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let session_kind = match put_request.session_kind {
        SessionKind::ExternalCapture => "external_capture",
        SessionKind::InternalCode => "internal_code",
    };
    let key = SessionNaturalKey {
        deployment_id: capture.deployment_id,
        tenant_id: capture.tenant_id,
        repo_id,
        producer_id: token.name,
        session_kind: session_kind.to_owned(),
        client_session_id: client_session_id.clone(),
    };
    let fingerprint_body = serde_json::json!({
        "deployment_id": key.deployment_id,
        "tenant_id": key.tenant_id,
        "repo_id": key.repo_id,
        "producer_id": key.producer_id,
        "session_kind": key.session_kind,
        "client_session_id": key.client_session_id,
        "started_at": put_request.started_at,
        "ended_at": put_request.ended_at,
    });
    let capture_id = state
        .storage
        .agent_capture_service
        .storage
        .upsert_session(key)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    state
        .storage
        .agent_capture_service
        .storage
        .upsert_ingest_receipt(
            capture_id,
            SESSION_PUT_OPERATION,
            &client_session_id,
            &fingerprint_body,
            Some(serde_json::json!({ "capture_id": capture_id })),
        )
        .await
        .map_err(receipt_error)?;
    Ok(Json(SessionPutResponse { capture_id }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/blobs/staging",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses(
        (status = 200, description = "Staging lease", body = StagingBlobResponse),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope),
        (status = 413, description = "Blob exceeds max_blob_bytes", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn staging_blob(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<StagingBlobResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_staging_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    reject_if_tombstoned_capture(&state, capture_id).await?;
    let lease_id = state
        .storage
        .agent_capture_service
        .stage_session_blob(
            capture_id,
            &capture.deployment_id,
            &capture.tenant_id,
            capture.lease_ttl_seconds,
            capture.max_blob_bytes,
            body_stream(body),
        )
        .await
        .map_err(blob_http_error)?;
    Ok(Json(StagingBlobResponse { lease_id }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/blobs/{lease_id}/finalize",
    params(
        ("capture_id" = i64, Path, description = "Capture id"),
        ("lease_id" = String, Path, description = "Staging lease id")
    ),
    request_body = FinalizeBlobRequest,
    responses(
        (status = 200, description = "Committed blob", body = FinalizeBlobResponse),
        (status = 400, description = "Digest mismatch", body = ErrorEnvelope),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session or lease not found", body = ErrorEnvelope),
        (status = 409, description = "Foreign live lease", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn finalize_blob(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<FinalizeBlobResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let (capture_id, lease_id) = session_finalize_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    reject_if_tombstoned_capture(&state, capture_id).await?;
    state
        .storage
        .agent_capture_service
        .check_finalize_lease(
            capture_id,
            &capture.deployment_id,
            &capture.tenant_id,
            &lease_id,
        )
        .await
        .map_err(blob_http_error)?;
    let bytes = axum::body::to_bytes(body, SESSION_PUT_MAX_BYTES)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("finalize body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    let request_body: FinalizeBlobRequest = if raw.trim().is_empty() {
        FinalizeBlobRequest {
            digest: None,
            object_key: None,
            size: None,
        }
    } else {
        serde_json::from_str(raw)
            .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
    };
    let committed = state
        .storage
        .agent_capture_service
        .finalize_session_blob(
            capture_id,
            &capture.deployment_id,
            &capture.tenant_id,
            &lease_id,
            request_body.digest.as_deref(),
            request_body.object_key.as_deref(),
        )
        .await
        .map_err(blob_http_error)?;
    Ok(Json(FinalizeBlobResponse {
        digest: committed.digest,
        object_key: committed.object_key,
    }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/events:batch",
    params(("capture_id" = i64, Path, description = "Capture id")),
    request_body = EventsBatchRequest,
    responses(
        (status = 200, description = "Events accepted", body = EventsBatchResponse),
        (status = 400, description = "Invalid event_uid or body", body = ErrorEnvelope),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope),
        (status = 409, description = "Event uid or batch fingerprint conflict", body = ErrorEnvelope),
        (status = 413, description = "Batch exceeds max_events_per_batch or max_event_bytes", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn events_batch(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<EventsBatchResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_events_batch_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    reject_if_tombstoned_capture(&state, capture_id).await?;
    let max_events = usize::try_from(capture.max_events_per_batch).unwrap_or(usize::MAX);
    let max_event_bytes = usize::try_from(capture.max_event_bytes).unwrap_or(usize::MAX);
    let body_limit = max_events
        .saturating_mul(max_event_bytes)
        .saturating_add(64 * 1024);
    let bytes = axum::body::to_bytes(body, body_limit)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("events batch body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    canonical_json::fingerprint(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let request_body: EventsBatchRequest = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if request_body.batch_id.is_empty() {
        return Err(AgentCaptureHttpError::bad_request("batch_id required"));
    }
    let preview: Vec<InsertEvent> = request_body
        .events
        .iter()
        .map(|item| InsertEvent {
            capture_id,
            event_uid: item.event_uid.clone(),
            event_kind: item.event_kind.clone(),
            native_id: item.native_id.clone(),
            lifecycle_seq: item.lifecycle_seq,
            payload: item.payload.clone(),
        })
        .collect();
    let incoming_fp = AgentCaptureStorage::events_batch_fingerprint(
        &request_body.batch_id,
        &preview,
        request_body.completeness.as_deref(),
    )
    .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if let Some(existing) = state
        .storage
        .agent_capture_service
        .storage
        .event_batch_receipt_fingerprint(capture_id, &request_body.batch_id)
        .await
        .map_err(events_http_error)?
    {
        if existing == incoming_fp {
            return Ok(Json(EventsBatchResponse { accepted: true }));
        }
        return Err(AgentCaptureHttpError::conflict());
    }
    if let Some(completeness) = request_body.completeness.as_deref()
        && !matches!(completeness, "incomplete" | "complete")
    {
        return Err(AgentCaptureHttpError::bad_request("invalid completeness"));
    }
    if request_body.events.len() > max_events {
        return Err(AgentCaptureHttpError::payload_too_large(
            "events batch exceeds max_events_per_batch",
        ));
    }
    let mut events = Vec::with_capacity(request_body.events.len());
    for item in request_body.events {
        parse_event_uid(&item.event_uid)?;
        let encoded = serde_json::to_vec(&serde_json::json!({
            "event_uid": item.event_uid,
            "event_kind": item.event_kind,
            "native_id": item.native_id,
            "lifecycle_seq": item.lifecycle_seq,
            "payload": item.payload,
        }))
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
        if encoded.len() > max_event_bytes {
            return Err(AgentCaptureHttpError::payload_too_large(
                "event exceeds max_event_bytes",
            ));
        }
        events.push(InsertEvent {
            capture_id,
            event_uid: item.event_uid,
            event_kind: item.event_kind,
            native_id: item.native_id,
            lifecycle_seq: item.lifecycle_seq,
            payload: item.payload,
        });
    }
    state
        .storage
        .agent_capture_service
        .storage
        .insert_events_batch(
            capture_id,
            &request_body.batch_id,
            &events,
            request_body.completeness.as_deref(),
            "jsonl",
        )
        .await
        .map_err(events_http_error)?;
    Ok(Json(EventsBatchResponse { accepted: true }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/file-ops:batch",
    params(("capture_id" = i64, Path, description = "Capture id")),
    request_body = FileOpsBatchRequest,
    responses(
        (status = 200, description = "File-ops accepted", body = FileOpsBatchResponse),
        (status = 400, description = "Invalid file-op path, schema, or source event", body = ErrorEnvelope),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope),
        (status = 409, description = "Batch fingerprint conflict", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn file_ops_batch(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<FileOpsBatchResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_file_ops_batch_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    reject_if_tombstoned_capture(&state, capture_id).await?;
    let bytes = axum::body::to_bytes(body, FILE_OPS_BATCH_MAX_BYTES)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("file-ops batch body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    let incoming_fp = canonical_json::fingerprint(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let batch_id = value
        .get("batch_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if batch_id.is_empty() {
        return Err(AgentCaptureHttpError::bad_request("batch_id required"));
    }
    if let Some(existing) = state
        .storage
        .agent_capture_service
        .storage
        .file_op_batch_receipt_fingerprint(capture_id, batch_id)
        .await
        .map_err(file_ops_http_error)?
    {
        if existing == incoming_fp {
            return Ok(Json(FileOpsBatchResponse { accepted: true }));
        }
        return Err(AgentCaptureHttpError::conflict());
    }
    let request_body: FileOpsBatchRequest = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let schema = request_body.schema.as_deref().unwrap_or(FILE_OP_SCHEMA_V1);
    if schema != FILE_OP_SCHEMA_V1 {
        return Err(AgentCaptureHttpError::bad_request(
            "schema must be agent.file_op.v1",
        ));
    }
    let preview: Vec<InsertFileOp> = request_body
        .ops
        .iter()
        .map(|item| InsertFileOp {
            source_event_uid: item.source_event_uid.clone(),
            op: item.op.clone(),
            path: item.path.clone(),
            digest: item
                .digest
                .as_deref()
                .filter(|digest| !digest.is_empty())
                .map(str::to_owned),
        })
        .collect();
    for item in &request_body.ops {
        if item.source_event_uid.is_empty() {
            return Err(AgentCaptureHttpError::bad_request(
                "source_event_uid required",
            ));
        }
        if !is_file_op_v1(&item.op) {
            return Err(AgentCaptureHttpError::bad_request("invalid file_op op"));
        }
        validate_file_op_path(&item.path)?;
    }
    state
        .storage
        .agent_capture_service
        .storage
        .insert_file_ops_batch(
            capture_id,
            &request_body.batch_id,
            schema,
            &preview,
            capture.max_file_blobs_per_session,
            &incoming_fp,
        )
        .await
        .map_err(file_ops_http_error)?;
    Ok(Json(FileOpsBatchResponse { accepted: true }))
}

#[utoipa::path(
    post,
    path = "/sessions/{capture_id}/checkpoints",
    params(("capture_id" = i64, Path, description = "Capture id")),
    request_body = CheckpointPostRequest,
    responses(
        (status = 200, description = "Checkpoint accepted", body = CheckpointPostResponse),
        (status = 400, description = "Invalid session kind or uncommitted transcript", body = ErrorEnvelope),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope),
        (status = 409, description = "Checkpoint fingerprint conflict", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn post_checkpoint(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<CheckpointPostResponse>, AgentCaptureHttpError> {
    let (parts, body) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_checkpoints_post_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    reject_if_tombstoned_capture(&state, capture_id).await?;
    let bytes = axum::body::to_bytes(body, CHECKPOINT_POST_MAX_BYTES)
        .await
        .map_err(|_| AgentCaptureHttpError::payload_too_large("checkpoint body too large"))?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AgentCaptureHttpError::bad_request("invalid utf-8 body"))?;
    let incoming_fp = canonical_json::fingerprint(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let checkpoint_id = value
        .get("checkpoint_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if checkpoint_id.is_empty() {
        return Err(AgentCaptureHttpError::bad_request("checkpoint_id required"));
    }
    if let Some((existing, response)) = state
        .storage
        .agent_capture_service
        .storage
        .checkpoint_receipt(capture_id, checkpoint_id)
        .await
        .map_err(checkpoint_http_error)?
    {
        if existing == incoming_fp {
            let completeness = response
                .as_ref()
                .and_then(|value| value.get("completeness"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&session.completeness)
                .to_owned();
            let partial_reason = response
                .as_ref()
                .and_then(|value| value.get("partial_reason"))
                .and_then(|value| {
                    if value.is_null() {
                        None
                    } else {
                        value.as_str().map(str::to_owned)
                    }
                });
            return Ok(Json(CheckpointPostResponse {
                accepted: true,
                completeness,
                partial_reason,
            }));
        }
        return Err(AgentCaptureHttpError::conflict());
    }
    let request_body: CheckpointPostRequest = serde_json::from_str(raw)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let ingested = state
        .storage
        .agent_capture_service
        .storage
        .insert_checkpoint_ingest(
            capture_id,
            &InsertCheckpoint {
                checkpoint_id: request_body.checkpoint_id,
                transcript_digest: request_body.transcript_digest,
                redacted_digest: request_body.redacted_digest,
                metadata: request_body.metadata,
            },
            &incoming_fp,
        )
        .await
        .map_err(checkpoint_http_error)?;
    Ok(Json(CheckpointPostResponse {
        accepted: true,
        completeness: ingested.completeness,
        partial_reason: ingested.partial_reason,
    }))
}

#[utoipa::path(
    get,
    path = "/repos/{repo}/sessions",
    params(("repo" = String, Path, description = "Single percent-encoded repo path segment")),
    responses(
        (status = 200, description = "Session metadata page"),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Token does not cover repo", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_sessions(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<PageEnvelope<SessionView>>, AgentCaptureHttpError> {
    let (parts, _) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let repo_segment = repo_sessions_list_path(parts.uri.path())?;
    let repo_id = decode_repo_path_segment(&repo_segment)
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    if !token_covers_capture_repo(&token, &repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let (limit, cursor) = pagination_from_uri(&parts.uri)?;
    let capture = state.storage.config().agent_capture.clone();
    let (rows, next_cursor) = state
        .storage
        .agent_capture_service
        .storage
        .list_sessions(
            &capture.deployment_id,
            &capture.tenant_id,
            &repo_id,
            &token.name,
            limit,
            cursor,
        )
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let items = rows
        .into_iter()
        .map(session_view)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(PageEnvelope {
        items,
        next_cursor: next_cursor.map(|id| id.to_string()),
    }))
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses(
        (status = 200, description = "Session metadata", body = SessionView),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn get_session(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<SessionView>, AgentCaptureHttpError> {
    let (parts, _) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_resource_path(parts.uri.path(), None)?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if session.producer_id != token.name || !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    Ok(Json(session_view(session)?))
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/checkpoints",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses(
        (status = 200, description = "Checkpoint metadata page"),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_checkpoints(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<PageEnvelope<CheckpointListItem>>, AgentCaptureHttpError> {
    let (parts, _) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_checkpoints_post_path(parts.uri.path())?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if session.producer_id != token.name || !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let (limit, cursor) = pagination_from_uri(&parts.uri)?;
    let (rows, next_cursor) = state
        .storage
        .agent_capture_service
        .storage
        .list_checkpoints(capture_id, limit, cursor)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    Ok(Json(PageEnvelope {
        items: rows
            .into_iter()
            .map(|row| CheckpointListItem {
                checkpoint_id: row.checkpoint_id,
                transcript_digest: row.transcript_digest,
                created_at: row.created_at.with_timezone(&chrono::Utc),
            })
            .collect(),
        next_cursor: next_cursor.map(|id| id.to_string()),
    }))
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/transcript",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses(
        (status = 200, description = "Committed raw transcript bytes"),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope),
        (status = 409, description = "Committed raw transcript missing", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn get_transcript(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<axum::response::Response, AgentCaptureHttpError> {
    let (parts, _) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_resource_path(parts.uri.path(), Some("transcript"))?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if session.producer_id != token.name || !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let blob = state
        .storage
        .agent_capture_service
        .storage
        .load_transcript_blob(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(missing_raw)?;
    let key = ObjectKey {
        namespace: ObjectNamespace::Agent,
        key: blob.object_key,
    };
    let (stream, _) = state
        .storage
        .agent_capture_service
        .obj_storage
        .inner
        .get_stream(&key)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    let bytes = collect_object_bytes(stream)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    state
        .storage
        .agent_capture_service
        .storage
        .insert_access_audit(
            &capture.deployment_id,
            &capture.tenant_id,
            capture_id,
            &token.name,
            "transcript.read",
        )
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response())
}

#[utoipa::path(
    get,
    path = "/sessions/{capture_id}/file-ops",
    params(("capture_id" = i64, Path, description = "Capture id")),
    responses(
        (status = 200, description = "File-op metadata page"),
        (status = 401, description = "Missing or invalid ingest token", body = ErrorEnvelope),
        (status = 404, description = "Session not found", body = ErrorEnvelope)
    ),
    tag = AGENT_CAPTURE_TAG
)]
async fn list_file_ops(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Json<PageEnvelope<FileOpListItem>>, AgentCaptureHttpError> {
    let (parts, _) = request.into_parts();
    let token = require_ingest_token(&state, &parts.headers)?;
    let capture_id = session_resource_path(parts.uri.path(), Some("file-ops"))?;
    let capture = state.storage.config().agent_capture.clone();
    let session = state
        .storage
        .agent_capture_service
        .load_session(capture_id, &capture.deployment_id, &capture.tenant_id)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?
        .ok_or_else(AgentCaptureHttpError::not_found)?;
    if session.producer_id != token.name || !token_covers_capture_repo(&token, &session.repo_id) {
        return Err(AgentCaptureHttpError::not_found());
    }
    let (limit, cursor) = pagination_from_uri(&parts.uri)?;
    let (rows, next_cursor) = state
        .storage
        .agent_capture_service
        .storage
        .list_file_ops(capture_id, limit, cursor)
        .await
        .map_err(|err| AgentCaptureHttpError::bad_request(err.to_string()))?;
    Ok(Json(PageEnvelope {
        items: rows
            .into_iter()
            .map(|row| FileOpListItem {
                id: row.id,
                source_event_uid: row.source_event_uid,
                op: row.op,
                path: row.path,
                created_at: row.created_at.with_timezone(&chrono::Utc),
            })
            .collect(),
        next_cursor: next_cursor.map(|id| id.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::*;
    use crate::{
        api::oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        callisto::{
            agent_capture_access_audit, agent_capture_blob, agent_capture_checkpoint,
            agent_capture_event, agent_capture_file_op, agent_capture_session,
        },
        ceres::api_service::cache::GitObjectCache,
        config::{
            AgentCaptureIngestTokenConfig, PushAuth, reload::ConfigHandle, testing::isolated_config,
        },
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            service::agent_capture_service::AgentCaptureService,
            storage::{
                Storage,
                agent_capture_storage::{AgentCaptureStorage, InsertEvent},
                base_storage::StorageConnector,
                object_storage::mock_object_storage,
            },
            tests::test_storage_with_config,
        },
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
            let expected = if (*method == "GET" && *path == "/api/v1/agent-capture/discovery")
                || (*method == "PUT"
                    && *path == "/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}")
                || (*method == "POST"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/blobs/staging")
                || (*method == "POST"
                    && *path
                        == "/api/v1/agent-capture/sessions/{capture_id}/blobs/{lease_id}/finalize")
                || (*method == "POST"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/events:batch")
                || (*method == "POST"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/file-ops:batch")
                || (*method == "POST"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/checkpoints")
                || (*method == "GET" && *path == "/api/v1/agent-capture/repos/{repo}/sessions")
                || (*method == "GET" && *path == "/api/v1/agent-capture/sessions/{capture_id}")
                || (*method == "GET"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/checkpoints")
                || (*method == "GET"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/transcript")
                || (*method == "GET"
                    && *path == "/api/v1/agent-capture/sessions/{capture_id}/file-ops")
            {
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
        assert_eq!(encoded, StatusCode::UNAUTHORIZED);

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

    fn ingest_token(paths: Option<Vec<String>>) -> AgentCaptureIngestTokenConfig {
        AgentCaptureIngestTokenConfig {
            name: "hook".to_owned(),
            token: "secret-ci".to_owned(),
            paths,
            tenant_id: None,
        }
    }

    fn state_from_storage(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "ac-21-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            storage,
        }
    }

    async fn db_state(paths: Option<Vec<String>>) -> (tempfile::TempDir, MonoApiServiceState) {
        db_state_with(paths, None).await
    }

    async fn db_state_with(
        paths: Option<Vec<String>>,
        max_blob_bytes: Option<u64>,
    ) -> (tempfile::TempDir, MonoApiServiceState) {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp.path().join("config"));
        config.git.push_auth = Some(PushAuth::None);
        config.agent_capture.enabled = true;
        config.agent_capture.ingest_tokens = vec![ingest_token(paths)];
        if let Some(max_blob_bytes) = max_blob_bytes {
            config.agent_capture.max_blob_bytes = max_blob_bytes;
        }
        let mut storage = test_storage_with_config(temp.path(), config).await;
        let base = storage.app_service.mono_storage.base.clone();
        storage.agent_capture_service = AgentCaptureService {
            storage: AgentCaptureStorage { base },
            obj_storage: mock_object_storage(),
        };
        (temp, state_from_storage(storage))
    }

    async fn put_session_request(
        state: MonoApiServiceState,
        repo: &str,
        client_session_id: &str,
        authorization: Option<&str>,
        body: &str,
        idempotency_key: Option<&str>,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::PUT)
            .uri(format!(
                "/api/v1/agent-capture/repos/{repo}/sessions/{client_session_id}"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        if let Some(value) = idempotency_key {
            request = request.header("Idempotency-Key", value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    fn state_with_restricted_token() -> MonoApiServiceState {
        let mut state = dummy_state();
        let mut config = (*state.storage.config()).clone();
        config.agent_capture.ingest_tokens =
            vec![ingest_token(Some(vec!["/third-part/mega".to_owned()]))];
        let config = Arc::new(config);
        state.storage.config_handle = ConfigHandle::from_arc(config.clone());
        state.storage.config = config;
        state
    }

    #[tokio::test]
    async fn put_session_unauthorized() {
        let response = put_session_request(
            dummy_state(),
            "third-part%2Fmega",
            "sess-1",
            None,
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_session_unauthorized_before_invalid_path() {
        let response = put_session_request(
            dummy_state(),
            "%FF",
            "sess-1",
            None,
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_session_unknown_path_is_404() {
        let response = put_session_request(
            state_with_restricted_token(),
            "other%2Frepo",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_session_unknown_path_before_oversize() {
        let oversized = format!(
            r#"{{"session_kind":"external_capture","pad":"{}"}}"#,
            "x".repeat(70_000)
        );
        let response = put_session_request(
            state_with_restricted_token(),
            "other%2Frepo",
            "sess-1",
            Some("Bearer secret-ci"),
            &oversized,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_session_returns_capture_id() {
        let (_temp, state) = db_state(None).await;
        let response = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let capture_id = body["capture_id"].as_i64().expect("capture_id");
        assert!(capture_id > 0);
    }

    #[tokio::test]
    async fn put_session_conflict() {
        let (_temp, state) = db_state(None).await;
        let first = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture","started_at":"2026-01-01T00:00:00Z"}"#,
            Some("idem-a"),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture","started_at":"2026-02-01T00:00:00Z"}"#,
            Some("idem-b"),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn session_put_retry_after_completeness_change() {
        let (_temp, state) = db_state(None).await;
        let body = r#"{"session_kind":"external_capture"}"#;
        let first = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            body,
            None,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let capture_id = json_body(first).await["capture_id"].as_i64().expect("id");
        state
            .storage
            .agent_capture_service
            .storage
            .insert_event(InsertEvent {
                capture_id,
                event_uid: "0:0".to_owned(),
                event_kind: "message".to_owned(),
                native_id: None,
                lifecycle_seq: None,
                payload: serde_json::json!({"n": 1}),
            })
            .await
            .expect("bump completeness");
        let second = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-1",
            Some("Bearer secret-ci"),
            body,
            None,
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            json_body(second).await["capture_id"].as_i64().expect("id"),
            capture_id
        );
    }

    async fn staging_request(
        state: MonoApiServiceState,
        capture_id: i64,
        authorization: Option<&str>,
        body: Vec<u8>,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder().method(Method::POST).uri(format!(
            "/api/v1/agent-capture/sessions/{capture_id}/blobs/staging"
        ));
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::from(body)).expect("request"))
            .await
            .expect("response")
    }

    async fn finalize_request(
        state: MonoApiServiceState,
        capture_id: i64,
        lease_id: &str,
        authorization: Option<&str>,
        body: &str,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/v1/agent-capture/sessions/{capture_id}/blobs/{lease_id}/finalize"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    async fn open_session(
        state: MonoApiServiceState,
        client_session_id: &str,
    ) -> (MonoApiServiceState, i64) {
        let response = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            client_session_id,
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let capture_id = json_body(response).await["capture_id"]
            .as_i64()
            .expect("capture_id");
        (state, capture_id)
    }

    #[tokio::test]
    async fn staging_unauthorized() {
        let response = staging_request(dummy_state(), 1, None, b"payload".to_vec()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn staging_too_large() {
        let (_temp, state) = db_state_with(None, Some(8)).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let response =
            staging_request(state, capture_id, Some("Bearer secret-ci"), vec![b'x'; 32]).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn staging_returns_lease() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let response = staging_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let lease_id = json_body(response).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        assert!(!lease_id.is_empty());
    }

    #[tokio::test]
    async fn finalize_returns_digest() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response =
            finalize_request(state, capture_id, &lease_id, Some("Bearer secret-ci"), "{}").await;
        assert_eq!(response.status(), StatusCode::OK);
        let digest = json_body(response).await["digest"]
            .as_str()
            .expect("digest")
            .to_owned();
        assert!(!digest.is_empty());
    }

    #[tokio::test]
    async fn finalize_lease_conflict() {
        let (_temp, state) = db_state(None).await;
        let (state, owner) = open_session(state, "sess-owner").await;
        let (state, other) = open_session(state, "sess-other").await;
        let staged = staging_request(
            state.clone(),
            owner,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response =
            finalize_request(state, other, &lease_id, Some("Bearer secret-ci"), "{}").await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn finalize_lease_conflict_before_bad_json() {
        let (_temp, state) = db_state(None).await;
        let (state, owner) = open_session(state, "sess-owner").await;
        let (state, other) = open_session(state, "sess-other").await;
        let staged = staging_request(
            state.clone(),
            owner,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response = finalize_request(
            state,
            other,
            &lease_id,
            Some("Bearer secret-ci"),
            "{not-json",
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn finalize_ignores_client_object_key() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response = finalize_request(
            state,
            capture_id,
            &lease_id,
            Some("Bearer secret-ci"),
            r#"{"object_key":"client/final/key"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let object_key = body["object_key"].as_str().expect("object_key");
        assert_ne!(object_key, "client/final/key");
        assert!(object_key.contains("/raw/sha256/"), "{object_key}");
    }

    #[tokio::test]
    async fn finalize_digest_mismatch() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response = finalize_request(
            state,
            capture_id,
            &lease_id,
            Some("Bearer secret-ci"),
            r#"{"digest":"sha256:deadbeef"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn finalize_empty_digest_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-blob").await;
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let response = finalize_request(
            state.clone(),
            capture_id,
            &lease_id,
            Some("Bearer secret-ci"),
            r#"{"digest":""}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let retry =
            finalize_request(state, capture_id, &lease_id, Some("Bearer secret-ci"), "{}").await;
        assert_eq!(retry.status(), StatusCode::OK);
    }

    async fn events_request(
        state: MonoApiServiceState,
        capture_id: i64,
        authorization: Option<&str>,
        body: &str,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/v1/agent-capture/sessions/{capture_id}/events:batch"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn events_unauthorized() {
        let response = events_request(dummy_state(), 1, None, "{}").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn events_batch_ok() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{"n":1}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["accepted"], true);
    }

    #[tokio::test]
    async fn events_rejects_bad_uid() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"not-a-uid","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_unknown_kind_kept() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"weird.kind","payload":{"envelope":true}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let row = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_event::Column::EventUid.eq("0:0".to_owned()))
            .one(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("load")
            .expect("event");
        assert_eq!(row.event_kind, "unknown");
        assert_eq!(row.payload["envelope"], true);
    }

    #[tokio::test]
    async fn events_unknown_kind_fingerprint_conflict() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let first = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"custom.a","payload":{"envelope":true}}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"custom.b","payload":{"envelope":true}}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn events_invalid_completeness_new_batch_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","completeness":"nope","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let rows = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn events_conflict_different_payload() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let first = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{"n":1}}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b2","events":[{"event_uid":"0:0","event_kind":"message","payload":{"n":2}}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn events_updates_completeness() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","completeness":"complete","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .one(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("load")
            .expect("session");
        assert_eq!(session.completeness, "complete");
    }

    #[tokio::test]
    async fn events_rejects_duplicate_payload_key() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let response = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{"x":1,"x":2}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_older_identical_uid_is_idempotent() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let first = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{"n":1}},{"event_uid":"0:100","event_kind":"message","payload":{"n":2}}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b2","events":[{"event_uid":"0:0","event_kind":"message","payload":{"n":1}}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_batch_id_conflict_before_invalid_completeness() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let first = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","completeness":"complete","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","completeness":"nope","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn events_oversize_native_id_is_413() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let native = "n".repeat(2_000_000);
        let body = format!(
            r#"{{"batch_id":"b1","events":[{{"event_uid":"0:0","event_kind":"message","native_id":"{native}","payload":{{}}}}]}}"#
        );
        let response = events_request(state, capture_id, Some("Bearer secret-ci"), &body).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn with_max_events(mut state: MonoApiServiceState, max_events: u32) -> MonoApiServiceState {
        let mut config = (*state.storage.config()).clone();
        config.agent_capture.max_events_per_batch = max_events;
        let config = Arc::new(config);
        state.storage.config_handle = ConfigHandle::from_arc(config.clone());
        state.storage.config = config;
        state
    }

    #[tokio::test]
    async fn events_batch_id_conflict_before_oversize() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let first = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let state = with_max_events(state, 1);
        let second = events_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{}},{"event_uid":"0:1","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn events_oversize_batch_is_413() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-events").await;
        let state = with_max_events(state, 1);
        let response = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{}},{"event_uid":"0:1","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let rows = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    async fn file_ops_request(
        state: MonoApiServiceState,
        capture_id: i64,
        authorization: Option<&str>,
        body: &str,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/v1/agent-capture/sessions/{capture_id}/file-ops:batch"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    async fn seed_source_event(state: MonoApiServiceState, capture_id: i64) -> MonoApiServiceState {
        let response = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"src-event","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        state
    }

    async fn commit_raw_digest(
        state: MonoApiServiceState,
        capture_id: i64,
        payload: &[u8],
    ) -> (MonoApiServiceState, String) {
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            payload.to_vec(),
        )
        .await;
        assert_eq!(staged.status(), StatusCode::OK);
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let finalized = finalize_request(
            state.clone(),
            capture_id,
            &lease_id,
            Some("Bearer secret-ci"),
            "{}",
        )
        .await;
        assert_eq!(finalized.status(), StatusCode::OK);
        let digest = json_body(finalized).await["digest"]
            .as_str()
            .expect("digest")
            .to_owned();
        (state, digest)
    }

    fn with_max_file_blobs(mut state: MonoApiServiceState, max: u32) -> MonoApiServiceState {
        let mut config = (*state.storage.config()).clone();
        config.agent_capture.max_file_blobs_per_session = max;
        let config = Arc::new(config);
        state.storage.config_handle = ConfigHandle::from_arc(config.clone());
        state.storage.config = config;
        state
    }

    #[tokio::test]
    async fn file_op_unauthorized() {
        let response = file_ops_request(dummy_state(), 1, None, "{}").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn file_op_ok() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let response = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","schema":"agent.file_op.v1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["accepted"], true);
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn file_op_requires_source() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let response = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_op_rejects_dotdot() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let response = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/../secret.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_op_search_ok() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let response = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"search","path":"src"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn file_op_truncated_over_cap() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let (state, digest_a) = commit_raw_digest(state, capture_id, b"blob-a").await;
        let (state, digest_b) = commit_raw_digest(state, capture_id, b"blob-b").await;
        let state = with_max_file_blobs(state, 1);
        let body = format!(
            r#"{{"batch_id":"f1","ops":[{{"source_event_uid":"0:0","op":"write","path":"a.rs","digest":"{digest_a}"}},{{"source_event_uid":"0:0","op":"write","path":"b.rs","digest":"{digest_b}"}}]}}"#
        );
        let response =
            file_ops_request(state.clone(), capture_id, Some("Bearer secret-ci"), &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .one(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("load")
            .expect("session");
        assert_eq!(session.completeness, "truncated");
    }

    #[tokio::test]
    async fn file_op_batch_idempotency_conflict() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let first = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/lib.rs"}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn file_op_rejects_absolute() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let response = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"/etc/passwd"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_op_missing_event_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let response = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn file_op_missing_event_conflict_substring_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let response = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"fingerprint conflict","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "bad_request");
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn file_op_missing_event_receipt_prefix_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let response = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"ingest receipt fingerprint conflict","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "bad_request");
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn file_op_uncommitted_digest_is_400() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let response = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs","digest":"sha256:deadbeef"}]}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn file_op_retry_same_fingerprint_is_200() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let body = r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#;
        let first =
            file_ops_request(state.clone(), capture_id, Some("Bearer secret-ci"), body).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second =
            file_ops_request(state.clone(), capture_id, Some("Bearer secret-ci"), body).await;
        assert_eq!(second.status(), StatusCode::OK);
        let rows = agent_capture_file_op::Entity::find()
            .filter(agent_capture_file_op::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn file_op_batch_id_conflict_before_dotdot() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let first = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/../x.rs"}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn file_op_batch_id_conflict_before_missing_source() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let first = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"op":"write","path":"a.rs"}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn file_op_batch_id_conflict_before_unknown_field() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let first = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}],"extra":true}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn file_op_batch_id_conflict_before_wrong_type() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-file-ops").await;
        let state = seed_source_event(state, capture_id).await;
        let first = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = file_ops_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":1,"path":"src/main.rs"}]}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    async fn checkpoint_request(
        state: MonoApiServiceState,
        capture_id: i64,
        authorization: Option<&str>,
        body: &str,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/v1/agent-capture/sessions/{capture_id}/checkpoints"
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response")
    }

    async fn open_internal_session(
        state: MonoApiServiceState,
        client_session_id: &str,
    ) -> (MonoApiServiceState, i64) {
        let response = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            client_session_id,
            Some("Bearer secret-ci"),
            r#"{"session_kind":"internal_code"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let capture_id = json_body(response).await["capture_id"]
            .as_i64()
            .expect("capture_id");
        (state, capture_id)
    }

    #[tokio::test]
    async fn checkpoint_unauthorized() {
        let response = checkpoint_request(dummy_state(), 1, None, "{}").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn checkpoint_ok() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-checkpoint").await;
        let (state, digest) = commit_raw_digest(state, capture_id, b"raw-transcript").await;
        let response = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["accepted"], true);
        let rows = agent_capture_checkpoint::Entity::find()
            .filter(agent_capture_checkpoint::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn checkpoint_rejects_internal_code() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_internal_session(state, "sess-internal").await;
        let (state, digest) = commit_raw_digest(state, capture_id, b"raw-transcript").await;
        let response = checkpoint_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn checkpoint_requires_committed_raw() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-checkpoint").await;
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"still-staging".to_vec(),
        )
        .await;
        assert_eq!(staged.status(), StatusCode::OK);
        let lease_id = json_body(staged).await["lease_id"]
            .as_str()
            .expect("lease_id")
            .to_owned();
        let digest = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::LeaseId.eq(lease_id))
            .one(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("load")
            .expect("blob")
            .digest;
        let response = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let rows = agent_capture_checkpoint::Entity::find()
            .filter(agent_capture_checkpoint::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn checkpoint_redacted_only_incomplete() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-checkpoint").await;
        let response = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"checkpoint_id":"cp-redacted","redacted_digest":"sha256:redacted"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["completeness"], "incomplete");
        assert_eq!(body["partial_reason"], "missing_raw");
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .one(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("load")
            .expect("session");
        assert_eq!(session.completeness, "incomplete");
        assert_eq!(session.partial_reason.as_deref(), Some("missing_raw"));
    }

    #[tokio::test]
    async fn checkpoint_shares_cas_blob() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-checkpoint").await;
        let (state, digest) = commit_raw_digest(state, capture_id, b"shared-raw").await;
        let first = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-a","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-b","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        let blobs = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::Digest.eq(digest.clone()))
            .filter(agent_capture_blob::Column::Visibility.eq("raw"))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count blobs");
        assert_eq!(blobs, 1);
        let checkpoints = agent_capture_checkpoint::Entity::find()
            .filter(agent_capture_checkpoint::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count checkpoints");
        assert_eq!(checkpoints, 2);
    }

    #[tokio::test]
    async fn checkpoint_idempotency_conflict() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-checkpoint").await;
        let (state, digest) = commit_raw_digest(state, capture_id, b"raw-transcript").await;
        let first = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = checkpoint_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"checkpoint_id":"cp-1","redacted_digest":"sha256:other"}"#,
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    async fn get_request(
        state: MonoApiServiceState,
        method: Method,
        uri: &str,
        authorization: Option<&str>,
    ) -> axum::http::Response<Body> {
        let (router, _) = public_app(state);
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        router
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    async fn response_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec()
    }

    #[tokio::test]
    async fn list_sessions_metadata_only() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let response = get_request(
            state,
            Method::GET,
            "/api/v1/agent-capture/repos/third-part%2Fmega/sessions",
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert!(body.get("items").is_some());
        assert!(body.get("next_cursor").is_some());
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["capture_id"], capture_id);
        assert!(items[0].get("transcript").is_none());
    }

    #[tokio::test]
    async fn get_session_ids() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let response = get_request(
            state,
            Method::GET,
            &format!("/api/v1/agent-capture/sessions/{capture_id}"),
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["capture_id"], capture_id);
        assert!(body.get("transcript").is_none());
    }

    #[tokio::test]
    async fn get_transcript_raw_and_audit() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let payload = b"raw-transcript-bytes";
        let (state, digest) = commit_raw_digest(state, capture_id, payload).await;
        let checkpointed = checkpoint_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            &format!(r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#),
        )
        .await;
        assert_eq!(checkpointed.status(), StatusCode::OK);
        let before = agent_capture_access_audit::Entity::find()
            .filter(agent_capture_access_audit::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        let response = get_request(
            state.clone(),
            Method::GET,
            &format!("/api/v1/agent-capture/sessions/{capture_id}/transcript"),
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        assert_eq!(response_bytes(response).await, payload);
        let after = agent_capture_access_audit::Entity::find()
            .filter(agent_capture_access_audit::Column::CaptureId.eq(capture_id))
            .count(state.storage.agent_capture_service.storage.get_connection())
            .await
            .expect("count");
        assert_eq!(after, before + 1);
    }

    #[tokio::test]
    async fn get_transcript_unauthorized() {
        let response = get_request(
            dummy_state(),
            Method::GET,
            "/api/v1/agent-capture/sessions/1/transcript",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_file_ops_ok() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let response = get_request(
            state,
            Method::GET,
            &format!("/api/v1/agent-capture/sessions/{capture_id}/file-ops"),
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_file_ops_has_items_key() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let response = get_request(
            state,
            Method::GET,
            &format!("/api/v1/agent-capture/sessions/{capture_id}/file-ops"),
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert!(body.get("items").is_some());
        assert!(body["items"].as_array().expect("items").is_empty());
        assert!(body.get("next_cursor").is_some());
    }

    #[tokio::test]
    async fn missing_raw_returns_409() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-query").await;
        let response = get_request(
            state,
            Method::GET,
            &format!("/api/v1/agent-capture/sessions/{capture_id}/transcript"),
            Some("Bearer secret-ci"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "missing_raw");
    }

    async fn tombstone_capture(state: &MonoApiServiceState, capture_id: i64) {
        let capture = state.storage.config().agent_capture.clone();
        state
            .storage
            .agent_capture_service
            .storage
            .insert_tombstone(&capture.deployment_id, &capture.tenant_id, capture_id)
            .await
            .expect("tombstone");
    }

    #[tokio::test]
    async fn tombstone_rejects_reingest_put() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-tomb").await;
        tombstone_capture(&state, capture_id).await;
        let response = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-tomb",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn tombstone_rejects_staging() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-tomb").await;
        tombstone_capture(&state, capture_id).await;
        let response = staging_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn tombstone_put_unauthorized_is_401() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-tomb").await;
        tombstone_capture(&state, capture_id).await;
        let response = put_session_request(
            state,
            "third-part%2Fmega",
            "sess-tomb",
            None,
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tombstone_blocks_all_ingest_writes() {
        let (_temp, state) = db_state(None).await;
        let (state, capture_id) = open_session(state, "sess-tomb").await;
        tombstone_capture(&state, capture_id).await;
        let put = put_session_request(
            state.clone(),
            "third-part%2Fmega",
            "sess-tomb",
            Some("Bearer secret-ci"),
            r#"{"session_kind":"external_capture"}"#,
            None,
        )
        .await;
        assert_eq!(put.status(), StatusCode::CONFLICT);
        let staged = staging_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            b"payload".to_vec(),
        )
        .await;
        assert_eq!(staged.status(), StatusCode::CONFLICT);
        let finalized = finalize_request(
            state.clone(),
            capture_id,
            "lease-1",
            Some("Bearer secret-ci"),
            "{}",
        )
        .await;
        assert_eq!(finalized.status(), StatusCode::CONFLICT);
        let events = events_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"b1","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#,
        )
        .await;
        assert_eq!(events.status(), StatusCode::CONFLICT);
        let file_ops = file_ops_request(
            state.clone(),
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"batch_id":"f1","ops":[{"source_event_uid":"0:0","op":"write","path":"a.rs"}]}"#,
        )
        .await;
        assert_eq!(file_ops.status(), StatusCode::CONFLICT);
        let checkpoint = checkpoint_request(
            state,
            capture_id,
            Some("Bearer secret-ci"),
            r#"{"checkpoint_id":"cp-1","redacted_digest":"sha256:x"}"#,
        )
        .await;
        assert_eq!(checkpoint.status(), StatusCode::CONFLICT);
    }
}
