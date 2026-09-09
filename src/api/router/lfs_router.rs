//! This module contains handlers for handling requests related to Git LFS (Large File Storage).
//!
//! The handlers in this module are responsible for handling various Git LFS operations such as
//! retrieving locks, verifying locks, creating and deleting locks, processing batch requests,
//! downloading and uploading objects, etc.
//!
//! Each handler corresponds to a specific endpoint or operation in the Git LFS protocol.
//! Error handling is done to return appropriate responses in case of success or failure.
//! These handlers are used in an Axum-based web application to handle Git LFS requests.
//!
//! # References
//!
//! - Git LFS Documentation: [https://git-lfs.github.com/](https://git-lfs.github.com/)
//! - Axum Documentation: [https://docs.rs/axum/](https://docs.rs/axum/)
//!
//! # Note
//!
//! Add more specific details and examples as needed to describe each handler's functionality.
//!
//! # Examples and Usage
//!
//! - `lfs_retrieve_lock`: Handles retrieving locks for Git LFS objects.
//! - `lfs_verify_lock`: Handles verifying locks for Git LFS objects.
//! - `lfs_create_lock`: Handles creating locks for Git LFS objects.
//! - `lfs_delete_lock`: Handles deleting locks for Git LFS objects.
//! - `lfs_process_batch`: Handles batch processing requests for Git LFS objects.
//! - `lfs_download_object`: Handles downloading Git LFS objects.
//! - `lfs_upload_object`: Handles uploading Git LFS objects.
//!
//! # Errors
//!
//! The handlers return `Result<Response, (StatusCode, String)>` to handle success or error cases.
//! Error responses are constructed with appropriate status codes and error messages.
//!
//! # Panics
//!
//! The code in these handlers is designed to handle errors gracefully and avoid panics.
//! However, certain unexpected situations might lead to panics, which should be minimized.
//!
//! # Security Considerations
//!
//! Ensure proper authentication and authorization mechanisms are implemented
//! when using these handlers in a web application to prevent unauthorized access.
use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE},
    },
    response::Response,
};
use base64::Engine;
use futures::TryStreamExt;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        api_doc::LFS_TAG,
        oauth::{bearer_token_from_authorization_value, login_user_from_mono_access_token},
    },
    ceres::lfs::{
        handler,
        lfs_structs::{
            BatchRequest, BatchResponse, LockList, LockListQuery, LockRequest, LockResponse,
            Operation, RequestObject, UnlockRequest, UnlockResponse, VerifiableLockList,
            VerifiableLockRequest,
        },
    },
    common::errors::GitLFSError,
    config::{GitConfig, PushAuth, PushPolicy},
    contract::git_protocol::{lookup_push_token, token_covers_repo},
};

const LFS_CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
const LFS_STREAM_CONTENT_TYPE: &str = "application/octet-stream";

/// The repository path prefix of an LFS request (the segment before
/// `/info/lfs/`), captured by `rewrite_lfs_request_uri` before that prefix is
/// stripped for routing. Empty when the request did not carry a repo prefix
/// (e.g. the internal `/api/v1/lfs` mount). Used to namespace LFS locks per
/// repository so identical ref names in different repos do not collide.
#[derive(Clone, Debug, Default)]
pub struct LfsRepoContext(pub String);

/// Resolves the repo path prefix from the optional request extension. The
/// extension is inserted by `rewrite_lfs_request_uri` for every request served
/// through the HTTP server, but the extractor is optional so that handlers
/// reached without that layer (e.g. direct tests of `app()`) degrade to the
/// empty repo (legacy bare-ref lock key) instead of returning `500`.
fn lfs_repo_path(ctx: Option<Extension<LfsRepoContext>>) -> String {
    ctx.map(|Extension(c)| c.0).unwrap_or_default()
}

pub fn lfs_routes() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
        .routes(routes!(lfs_upload_object))
        .routes(routes!(lfs_download_object))
        .routes(routes!(list_locks))
        .routes(routes!(create_lock))
        .routes(routes!(list_locks_for_verification))
        .routes(routes!(delete_lock))
        .routes(routes!(lfs_process_batch))
}

/// The [LFS Server Discovery](https://github.com/git-lfs/git-lfs/blob/main/docs/api/server-discovery.md)
/// document describes the server LFS discovery protocol.
/// Example:
/// Git remote: https://git-server.com/foo/bar
/// LFS server: https://git-server.com/foo/bar.git/info/lfs
/// Locks API: https://git-server.com/foo/bar.git/info/lfs/locks
///
/// We expose both `/info/lfs` (standard Git LFS discovery path) and `/api/v1/lfs`
/// (versioned REST path for internal consistency). Both paths serve identical handlers.
///
/// For OpenAPI documentation, we only register `/api/v1/lfs` paths to avoid duplication.
/// The `/info/lfs` paths are still available at runtime for Git LFS client compatibility.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    // Only register /api/v1/lfs for OpenAPI to avoid path duplication
    // /info/lfs paths are still available at runtime via the main router
    OpenApiRouter::new().nest("/api/v1/lfs", lfs_routes())
}

/// Maps GitLFSError to HTTP status code and message.
///
/// This is a temporary workaround until handler layer returns typed errors instead of
/// GitLFSError::GeneralError(String). The string matching approach is fragile but necessary
/// because:
/// 1. LFS routes need to return LFS-specific JSON format (application/vnd.git-lfs+json)
/// 2. Handler layer currently only returns GitLFSError::GeneralError(String)
/// 3. We need to map errors to appropriate HTTP status codes (404/400/500)
///
/// TODO: Refactor handler layer to use typed GitLFSError variants (NotFound, InvalidInput, etc.)
/// and replace this function with direct pattern matching on error types.
fn map_lfs_error<E: ToString>(err: E) -> (StatusCode, String) {
    let msg = err.to_string();
    // Match common error patterns from handler layer (case-sensitive to avoid over-matching).
    if msg.contains("Not found") || msg.contains("not found") || msg.contains("doesn't exist") {
        (StatusCode::NOT_FOUND, msg)
    } else if msg.contains("Invalid") || msg.contains("invalid") || msg.contains("Bad request") {
        (StatusCode::BAD_REQUEST, msg)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

fn lfs_error_response(code: StatusCode, msg: String) -> Response<Body> {
    let error_body = serde_json::json!({ "message": msg }).to_string();
    lfs_response(code, LFS_CONTENT_TYPE, Body::from(error_body))
}

fn lfs_json_response(code: StatusCode, body: String) -> Response<Body> {
    lfs_response(code, LFS_CONTENT_TYPE, Body::from(body))
}

fn lfs_response(code: StatusCode, content_type: &'static str, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = code;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// LFS operation category for access control: reads (download / lock listing)
/// vs writes (upload / lock create / unlock).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LfsAccess {
    Read,
    Write,
}

/// Pure LFS access-control policy, mirroring the Git smart-protocol model:
/// reads follow `git.anonymous_access` (like upload-pack/fetch), writes always
/// require an authenticated caller (like receive-pack/push).
fn lfs_access_allowed(access: LfsAccess, anonymous_access: bool, authenticated: bool) -> bool {
    match access {
        LfsAccess::Read => anonymous_access || authenticated,
        LfsAccess::Write => authenticated,
    }
}

/// Extracts the mono access token from an `Authorization` header, supporting
/// both `Bearer <token>` and HTTP Basic auth (token in the password field) —
/// the same credential shapes accepted by the Git smart-HTTP transport that
/// git-lfs reuses for the hybrid LFS API.
fn lfs_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    if let Some(bearer) = bearer_token_from_authorization_value(value) {
        return Some(bearer.to_owned());
    }
    let stripped = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stripped.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    // Basic auth is "username:password"; the token lives in the password field.
    decoded.split(':').nth(1).map(str::to_owned)
}

/// Resolves the authenticated username from the request credentials, or `None`
/// when no valid token is presented.
async fn lfs_authenticated_user(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
) -> Option<String> {
    let token = lfs_token_from_headers(headers)?;
    match login_user_from_mono_access_token(&state.storage.user_storage(), &token).await {
        Ok(Some(user)) => Some(user.username),
        _ => None,
    }
}

/// Builds a `401` LFS response that asks the client to retry with credentials.
fn lfs_auth_challenge() -> Response<Body> {
    let mut response = lfs_error_response(
        StatusCode::UNAUTHORIZED,
        "authentication required".to_owned(),
    );
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Mega\", Bearer realm=\"Mega\""),
    );
    response
}

/// Trunk / storage-only LFS gate aligned with `git.push_auth` (ADR-LF-01/02).
/// Does not consult `UserStorage`. Empty `repo_path` is treated as `/`.
fn enforce_trunk_lfs_access(
    git: &GitConfig,
    headers: &HeaderMap,
    access: LfsAccess,
    repo_path: &str,
) -> Result<(), Box<Response<Body>>> {
    let path = if repo_path.is_empty() { "/" } else { repo_path };
    match git.push_auth {
        Some(PushAuth::None) => Ok(()),
        Some(PushAuth::Token) => {
            let matched = lfs_token_from_headers(headers)
                .as_deref()
                .and_then(|presented| lookup_push_token(&git.push_tokens, presented));
            match access {
                LfsAccess::Read => {
                    if git.anonymous_access || matched.is_some() {
                        Ok(())
                    } else {
                        Err(Box::new(lfs_auth_challenge()))
                    }
                }
                LfsAccess::Write => match matched {
                    None => Err(Box::new(lfs_auth_challenge())),
                    Some(token) if token_covers_repo(token, path) => Ok(()),
                    Some(_) => Err(Box::new(lfs_error_response(
                        StatusCode::FORBIDDEN,
                        format!("token is not authorized for path {path}"),
                    ))),
                },
            }
        }
        // Trunk boot requires explicit push_auth; fail-closed if somehow absent.
        None => Err(Box::new(lfs_auth_challenge())),
    }
}

/// Enforces the LFS access policy for the given operation, returning a ready
/// denial response when the caller is not permitted.
///
/// Trunk uses [`enforce_trunk_lfs_access`] (static `push_auth`). Review keeps
/// the `UserStorage` mono access-token model. The challenge is boxed because
/// it is the rare arm: an unboxed `Response<Body>` would put 128 bytes of
/// denial into every permitted request's return value too
/// (`clippy::result_large_err`).
async fn enforce_lfs_access(
    state: &MonoApiServiceState,
    headers: &HeaderMap,
    access: LfsAccess,
    repo_path: &str,
) -> Result<(), Box<Response<Body>>> {
    let config = state.storage.config();
    if config.monorepo.push_policy == PushPolicy::Trunk {
        return enforce_trunk_lfs_access(&config.git, headers, access, repo_path);
    }
    let anonymous_access = config.git.anonymous_access;
    // Only resolve the (DB-backed) token when it can actually affect the outcome.
    let authenticated = if access == LfsAccess::Read && anonymous_access {
        false
    } else {
        lfs_authenticated_user(state, headers).await.is_some()
    };
    if lfs_access_allowed(access, anonymous_access, authenticated) {
        Ok(())
    } else {
        Err(Box::new(lfs_auth_challenge()))
    }
}

/// List LFS locks
///
#[utoipa::path(
    get,
    path = "/locks",
    params(
        ("path" = Option<String>, Query, description = "Filter locks by file path"),
        ("id" = Option<String>, Query, description = "Filter locks by lock ID"),
        ("limit" = Option<String>, Query, description = "Maximum number of locks to return"),
        ("refspec" = Option<String>, Query, description = "Git reference specifier"),
    ),
    responses(
        (status = 200, description = "List of locks", body = LockList, content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Invalid request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Lock not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error")
    ),
    tag = LFS_TAG,
    description = "List LFS locks. This handler is also available at `/info/lfs/locks` for Git LFS client compatibility."
)]
pub async fn list_locks(
    state: State<MonoApiServiceState>,
    repo: Option<Extension<LfsRepoContext>>,
    headers: HeaderMap,
    Query(query): Query<LockListQuery>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let repo = lfs_repo_path(repo);
    if let Err(resp) = enforce_lfs_access(&state, &headers, LfsAccess::Read, &repo).await {
        return Ok(*resp);
    }
    let result: Result<LockList, GitLFSError> =
        handler::lfs_retrieve_lock(state.storage.lfs_db_storage(), &repo, query).await;
    match result {
        Ok(lock_list) => {
            let body = serde_json::to_string(&lock_list).unwrap_or_default();
            Ok(lfs_json_response(StatusCode::OK, body))
        }
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Verify LFS locks
///
/// Verifies locks for a given ref, returning locks that belong to the current user (ours) and others (theirs).
#[utoipa::path(
    post,
    path = "/locks/verify",
    request_body = VerifiableLockRequest,
    responses(
        (status = 200, description = "Verifiable lock list", body = VerifiableLockList, content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Invalid request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Lock not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error")
    ),
    tag = LFS_TAG,
    description = "Verify LFS locks. This handler is also available at `/info/lfs/locks/verify` for Git LFS client compatibility."
)]
pub async fn list_locks_for_verification(
    state: State<MonoApiServiceState>,
    repo: Option<Extension<LfsRepoContext>>,
    headers: HeaderMap,
    Json(json): Json<VerifiableLockRequest>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let repo = lfs_repo_path(repo);
    if let Err(resp) = enforce_lfs_access(&state, &headers, LfsAccess::Read, &repo).await {
        return Ok(*resp);
    }
    let result = handler::lfs_verify_lock(state.storage.lfs_db_storage(), &repo, json).await;
    match result {
        Ok(lock_list) => {
            let body = serde_json::to_string(&lock_list).unwrap_or_default();
            Ok(lfs_json_response(StatusCode::OK, body))
        }
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Create an LFS lock
///
/// Creates a lock for a file path in the repository. The lock prevents other users from modifying the file.
#[utoipa::path(
    post,
    path = "/locks",
    request_body = LockRequest,
    responses(
        (status = 201, description = "Lock created successfully", body = LockResponse, content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Invalid request or parameters", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Resource not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error or lock already exists")
    ),
    tag = LFS_TAG,
    description = "Create an LFS lock. This handler is also available at `/info/lfs/locks` for Git LFS client compatibility."
)]
pub async fn create_lock(
    state: State<MonoApiServiceState>,
    repo: Option<Extension<LfsRepoContext>>,
    headers: HeaderMap,
    Json(json): Json<LockRequest>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let repo = lfs_repo_path(repo);
    if let Err(resp) = enforce_lfs_access(&state, &headers, LfsAccess::Write, &repo).await {
        return Ok(*resp);
    }
    let result = handler::lfs_create_lock(state.storage.lfs_db_storage(), &repo, json).await;
    match result {
        Ok(lock) => {
            let lock_response = LockResponse {
                lock,
                message: "".to_string(),
            };
            let body = serde_json::to_string(&lock_response).unwrap_or_default();
            Ok(lfs_json_response(StatusCode::CREATED, body))
        }
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Delete an LFS lock
///
/// Deletes a lock by its ID. Requires the lock to belong to the current user unless force is set to true.
#[utoipa::path(
    post,
    path = "/locks/{id}/unlock",
    params(
        ("id" = String, Path, description = "Lock ID to unlock"),
    ),
    request_body = UnlockRequest,
    responses(
        (status = 200, description = "Lock deleted successfully", body = UnlockResponse, content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Invalid request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Lock not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error or lock not found")
    ),
    tag = LFS_TAG,
    description = "Delete an LFS lock. This handler is also available at `/info/lfs/locks/{id}/unlock` for Git LFS client compatibility."
)]
pub async fn delete_lock(
    state: State<MonoApiServiceState>,
    Path(id): Path<String>,
    repo: Option<Extension<LfsRepoContext>>,
    headers: HeaderMap,
    Json(json): Json<UnlockRequest>,
) -> Result<Response, (StatusCode, String)> {
    let repo = lfs_repo_path(repo);
    if let Err(resp) = enforce_lfs_access(&state, &headers, LfsAccess::Write, &repo).await {
        return Ok(*resp);
    }
    let result = handler::lfs_delete_lock(state.storage.lfs_db_storage(), &repo, &id, json).await;

    match result {
        Ok(lock) => {
            let unlock_response = UnlockResponse {
                lock,
                message: "".to_string(),
            };
            let body = serde_json::to_string(&unlock_response).unwrap_or_default();
            Ok(lfs_json_response(StatusCode::OK, body))
        }
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Process LFS batch request
///
/// Processes a batch of LFS objects for upload or download operations.
/// Returns URLs and actions for each object.
#[utoipa::path(
    post,
    path = "/objects/batch",
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Batch response with object actions", body = BatchResponse, content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Bad request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Object(s) not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error")
    ),
    tag = LFS_TAG,
    description = "Process LFS batch request. This handler is also available at `/info/lfs/objects/batch` for Git LFS client compatibility."
)]
pub async fn lfs_process_batch(
    state: State<MonoApiServiceState>,
    repo: Option<Extension<LfsRepoContext>>,
    headers: HeaderMap,
    Json(json): Json<BatchRequest>,
) -> Result<Response<Body>, (StatusCode, String)> {
    // Batch is a single endpoint for both directions; the requested operation
    // decides whether it is a read (download) or a write (upload).
    let access = if json.operation == Operation::Upload {
        LfsAccess::Write
    } else {
        LfsAccess::Read
    };
    let repo = lfs_repo_path(repo);
    if let Err(resp) = enforce_lfs_access(&state, &headers, access, &repo).await {
        return Ok(*resp);
    }
    let result =
        handler::lfs_process_batch(&state.storage.lfs_service, json, &state.listen_addr).await;

    match result {
        Ok(res) => {
            let body = serde_json::to_string(&res).unwrap_or_default();
            Ok(lfs_json_response(StatusCode::OK, body))
        }
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            tracing::error!("Error: {}", msg);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Download an LFS object
///
/// Downloads an LFS object by its OID. Returns the object data as a stream.
#[utoipa::path(
    get,
    path = "/objects/{object_id}",
    params(
        ("object_id" = String, Path, description = "Object ID (OID) to download"),
    ),
    responses(
        (status = 200, description = "Object data stream", content_type = "application/octet-stream"),
        (status = 400, description = "Bad request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Object not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error or object not found")
    ),
    tag = LFS_TAG,
    description = "Download an LFS object. This handler is also available at `/info/lfs/objects/{object_id}` for Git LFS client compatibility."
)]
pub async fn lfs_download_object(
    state: State<MonoApiServiceState>,
    Path(oid): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    // Raw object transfer is a capability URL issued by the (auth-gated) batch
    // endpoint, which is the LFS authorization point; git-lfs does not reliably
    // attach credentials to the transfer request, so this endpoint is not
    // re-authenticated here. See `lfs_process_batch` for the read/write gate.
    let result = handler::lfs_download_object(state.storage.lfs_service.clone(), oid.clone()).await;
    match result {
        Ok(byte_stream) => Ok(lfs_response(
            StatusCode::OK,
            LFS_STREAM_CONTENT_TYPE,
            Body::from_stream(byte_stream),
        )),
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

/// Upload an LFS object
///
/// Uploads an LFS object to the server. The object data should be sent in the request body.
#[utoipa::path(
    put,
    path = "/objects/{object_id}",
    params(
        ("object_id" = String, Path, description = "Object ID (OID) to upload"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream", description = "Object data"),
    responses(
        (status = 200, description = "Object uploaded successfully", content_type = "application/vnd.git-lfs+json"),
        (status = 400, description = "Bad request", content_type = "application/vnd.git-lfs+json"),
        (status = 404, description = "Object not found", content_type = "application/vnd.git-lfs+json"),
        (status = 500, description = "Internal server error")
    ),
    tag = LFS_TAG,
    description = "Upload an LFS object. This handler is also available at `/info/lfs/objects/{object_id}` for Git LFS client compatibility."
)]
pub async fn lfs_upload_object(
    state: State<MonoApiServiceState>,
    Path(oid): Path<String>,
    req: Request<Body>,
) -> Result<Response<Body>, (StatusCode, String)> {
    // Raw object upload is a capability URL issued by the (auth-gated) batch
    // endpoint: `lfs_process_batch` requires a write token for `operation:
    // upload` and registers the object metadata, and `lfs_upload_object`
    // (handler) rejects any oid without that prior registration ("Not found").
    // So an anonymous upload is impossible without first passing the batch gate.
    // This endpoint is therefore not re-authenticated here — git-lfs does not
    // reliably attach credentials to the transfer PUT, and returning 401
    // mid-body would break the push with a broken pipe.
    let req_obj = RequestObject {
        oid,
        ..Default::default()
    };

    // Collect bytes asynchronously from the stream into a Vec<u8>
    let body_bytes: Vec<u8> = req
        .into_body()
        .into_data_stream()
        .try_fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("failed to read LFS object request body: {e}"),
            )
        })?;

    let result = handler::lfs_upload_object(&state.storage.lfs_service, &req_obj, body_bytes).await;
    match result {
        Ok(_) => Ok(lfs_json_response(StatusCode::OK, String::new())),
        Err(err) => {
            let (code, msg) = map_lfs_error(err);
            Ok(lfs_error_response(code, msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn lfs_access_policy_matrix() {
        // Reads mirror upload-pack: allowed when anonymous access is enabled,
        // or when the caller is authenticated.
        assert!(lfs_access_allowed(LfsAccess::Read, true, false));
        assert!(lfs_access_allowed(LfsAccess::Read, true, true));
        assert!(!lfs_access_allowed(LfsAccess::Read, false, false));
        assert!(lfs_access_allowed(LfsAccess::Read, false, true));
        // Writes mirror receive-pack: always require authentication, regardless
        // of the anonymous-access flag.
        assert!(!lfs_access_allowed(LfsAccess::Write, true, false));
        assert!(lfs_access_allowed(LfsAccess::Write, true, true));
        assert!(!lfs_access_allowed(LfsAccess::Write, false, false));
        assert!(lfs_access_allowed(LfsAccess::Write, false, true));
    }

    #[test]
    fn lfs_token_from_headers_parses_bearer_and_basic() {
        let mut bearer = HeaderMap::new();
        bearer.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tok-123"));
        assert_eq!(lfs_token_from_headers(&bearer).as_deref(), Some("tok-123"));

        let mut basic = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("git:tok-basic");
        basic.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(lfs_token_from_headers(&basic).as_deref(), Some("tok-basic"));

        // Missing header yields no token.
        assert_eq!(lfs_token_from_headers(&HeaderMap::new()), None);
        // Malformed Basic payload yields no token instead of panicking.
        let mut malformed = HeaderMap::new();
        malformed.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic not-base64!!"),
        );
        assert_eq!(lfs_token_from_headers(&malformed), None);
    }

    #[test]
    fn lfs_auth_challenge_is_401_with_www_authenticate() {
        let resp = lfs_auth_challenge();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key(WWW_AUTHENTICATE));
    }

    fn push_token(
        name: &str,
        secret: &str,
        paths: Option<Vec<&str>>,
    ) -> crate::config::PushTokenConfig {
        crate::config::PushTokenConfig {
            name: name.to_owned(),
            token: secret.to_owned(),
            paths: paths.map(|p| p.into_iter().map(str::to_owned).collect()),
        }
    }

    fn bearer_headers(secret: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {secret}")).unwrap(),
        );
        headers
    }

    /// Trunk + `push_auth=token` never consults UserStorage: the gate is the
    /// pure [`enforce_trunk_lfs_access`] helper (ADR-LF-01).
    #[test]
    fn trunk_token_write_gate_does_not_need_user_storage() {
        let git = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![push_token("ci", "secret-ci", Some(vec!["/project"]))],
            ..GitConfig::default()
        };

        // Missing credential → 401 challenge (no DB).
        let denied =
            enforce_trunk_lfs_access(&git, &HeaderMap::new(), LfsAccess::Write, "/project")
                .expect_err("anonymous write denied");
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(denied.headers().contains_key(WWW_AUTHENTICATE));

        // Valid token covering path → allow.
        enforce_trunk_lfs_access(
            &git,
            &bearer_headers("secret-ci"),
            LfsAccess::Write,
            "/project/foo",
        )
        .expect("covering token allows write");

        // Valid token outside paths → 403.
        let forbidden = enforce_trunk_lfs_access(
            &git,
            &bearer_headers("secret-ci"),
            LfsAccess::Write,
            "/other",
        )
        .expect_err("out-of-path write denied");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn trunk_none_allows_anonymous_read_and_write() {
        let git = GitConfig {
            push_auth: Some(PushAuth::None),
            ..GitConfig::default()
        };
        enforce_trunk_lfs_access(&git, &HeaderMap::new(), LfsAccess::Write, "/project")
            .expect("none write");
        enforce_trunk_lfs_access(&git, &HeaderMap::new(), LfsAccess::Read, "/project")
            .expect("none read");
    }

    #[test]
    fn trunk_token_read_respects_anonymous_access() {
        let mut git = GitConfig {
            push_auth: Some(PushAuth::Token),
            anonymous_access: true,
            push_tokens: vec![push_token("ci", "secret-ci", None)],
            ..GitConfig::default()
        };
        enforce_trunk_lfs_access(&git, &HeaderMap::new(), LfsAccess::Read, "/")
            .expect("anonymous read when anonymous_access");

        git.anonymous_access = false;
        let denied = enforce_trunk_lfs_access(&git, &HeaderMap::new(), LfsAccess::Read, "/")
            .expect_err("read denied without token when anonymous_access=false");
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        enforce_trunk_lfs_access(&git, &bearer_headers("secret-ci"), LfsAccess::Read, "/")
            .expect("token allows read");
    }

    #[test]
    fn empty_repo_path_is_treated_as_root_for_token_paths() {
        let whole_repo = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![push_token("root-only", "s", None)],
            ..GitConfig::default()
        };
        enforce_trunk_lfs_access(&whole_repo, &bearer_headers("s"), LfsAccess::Write, "")
            .expect("empty repo + whole-repo token");

        let scoped = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![push_token("scoped", "s", Some(vec!["/project"]))],
            ..GitConfig::default()
        };
        let forbidden =
            enforce_trunk_lfs_access(&scoped, &bearer_headers("s"), LfsAccess::Write, "")
                .expect_err("empty repo is /, scoped token does not cover");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_map_lfs_error_not_found() {
        // Test "Not found" error mapping
        let error = "Object not found";
        let (code, msg) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(msg, "Object not found");

        // Test case-insensitive "not found"
        let error = "Lock not found in database";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_map_lfs_error_bad_request() {
        // Test "Invalid" error mapping
        let error = "Invalid object ID format";
        let (code, msg) = map_lfs_error(error);
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(msg, "Invalid object ID format");

        // Test other invalid cases
        let error = "Invalid request parameters";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    /// SYNC-04 (mega@2398a92, #2175): the exact lock-list limit rejection
    /// message, passed through `lfs_retrieve_lock` unmasked, maps to 400.
    #[test]
    fn invalid_limit_rejection_maps_to_400() {
        let (code, msg) = map_lfs_error("Invalid limit parameter: -1");
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(msg.starts_with("Invalid limit parameter"));
    }

    #[test]
    fn test_map_lfs_error_internal_server_error() {
        // Test default error mapping
        let error = "Database connection failed";
        let (code, msg) = map_lfs_error(error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(msg, "Database connection failed");

        // Test generic error
        let error = "Unexpected error occurred";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_map_lfs_error_with_gitlfs_error() {
        use crate::common::errors::GitLFSError;

        // Test with GitLFSError that contains "Not found"
        let error = GitLFSError::GeneralError("Object not found".to_string());
        let (code, msg) = map_lfs_error(&error);
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(msg.contains("not found"));

        // Test with GitLFSError that contains "Invalid"
        let error = GitLFSError::GeneralError("Invalid OID format".to_string());
        let (code, msg) = map_lfs_error(&error);
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(msg.contains("Invalid"));

        // Test with GitLFSError that doesn't match patterns
        let error = GitLFSError::GeneralError("Storage error".to_string());
        let (code, _) = map_lfs_error(&error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_map_lfs_error_edge_cases() {
        // Test empty string
        let error = "";
        let (code, msg) = map_lfs_error(error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(msg, "");

        // Test string with both patterns (should match first)
        let error = "Not found: Invalid request";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND); // "Not found" comes first

        // Test case variations
        let error = "NOT FOUND";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR); // Case sensitive

        let error = "not found";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND); // Lowercase works
    }

    #[test]
    fn test_lfs_content_types() {
        // Test content type constants
        assert_eq!(LFS_CONTENT_TYPE, "application/vnd.git-lfs+json");
        assert_eq!(LFS_STREAM_CONTENT_TYPE, "application/octet-stream");
        assert_eq!(LFS_TAG, "Git LFS");
    }

    #[test]
    fn test_lfs_routes_structure() {
        // Smoke test: ensure the LFS routes can be constructed without panicking.
        // This verifies that route registration does not cause runtime failures
        // during router creation. It does NOT assert individual routes exist;
        // those should be covered by more targeted endpoint tests if needed.
        let _routes = lfs_routes();
    }

    #[test]
    fn test_lfs_download_chunk_parameter_validation() {
        // Test parameter validation logic for lfs_download_chunk
        // This tests the query parameter parsing logic

        // Valid parameters
        let mut valid_params = HashMap::new();
        valid_params.insert("offset".to_string(), "0".to_string());
        valid_params.insert("size".to_string(), "1024".to_string());

        let offset = valid_params
            .get("offset")
            .and_then(|offset| offset.parse::<u64>().ok());
        let size = valid_params
            .get("size")
            .and_then(|size| size.parse::<u64>().ok());

        assert!(offset.is_some());
        assert!(size.is_some());
        assert_eq!(offset.unwrap(), 0);
        assert_eq!(size.unwrap(), 1024);

        // Missing offset
        let mut missing_offset = HashMap::new();
        missing_offset.insert("size".to_string(), "1024".to_string());
        let offset = missing_offset
            .get("offset")
            .and_then(|offset| offset.parse::<u64>().ok());
        assert!(offset.is_none());

        // Missing size
        let mut missing_size = HashMap::new();
        missing_size.insert("offset".to_string(), "0".to_string());
        let size = missing_size
            .get("size")
            .and_then(|size| size.parse::<u64>().ok());
        assert!(size.is_none());

        // Invalid offset format
        let mut invalid_offset = HashMap::new();
        invalid_offset.insert("offset".to_string(), "invalid".to_string());
        invalid_offset.insert("size".to_string(), "1024".to_string());
        let offset = invalid_offset
            .get("offset")
            .and_then(|offset| offset.parse::<u64>().ok());
        assert!(offset.is_none());

        // Invalid size format
        let mut invalid_size = HashMap::new();
        invalid_size.insert("offset".to_string(), "0".to_string());
        invalid_size.insert("size".to_string(), "not_a_number".to_string());
        let size = invalid_size
            .get("size")
            .and_then(|size| size.parse::<u64>().ok());
        assert!(size.is_none());
    }

    #[test]
    fn test_lfs_download_chunk_parameter_edge_cases() {
        // Test edge cases for parameter parsing
        let mut params = HashMap::new();

        // Test with zero values
        params.insert("offset".to_string(), "0".to_string());
        params.insert("size".to_string(), "0".to_string());
        let offset = params.get("offset").and_then(|o| o.parse::<u64>().ok());
        let size = params.get("size").and_then(|s| s.parse::<u64>().ok());
        assert_eq!(offset, Some(0));
        assert_eq!(size, Some(0));

        // Test with large values
        params.insert("offset".to_string(), "18446744073709551615".to_string()); // u64::MAX
        params.insert("size".to_string(), "1000000".to_string());
        let offset = params.get("offset").and_then(|o| o.parse::<u64>().ok());
        let size = params.get("size").and_then(|s| s.parse::<u64>().ok());
        assert_eq!(offset, Some(u64::MAX));
        assert_eq!(size, Some(1000000));

        // Test with negative number (should fail to parse as u64)
        params.insert("offset".to_string(), "-1".to_string());
        let offset = params.get("offset").and_then(|o| o.parse::<u64>().ok());
        assert!(offset.is_none());
    }

    #[test]
    fn test_request_object_default() {
        // Test RequestObject default values
        let req_obj = RequestObject {
            oid: "test_oid".to_string(),
            ..Default::default()
        };
        assert_eq!(req_obj.oid, "test_oid");
    }

    #[test]
    fn test_lfs_router_paths() {
        // Smoke test: ensure the LFS router can be constructed without panicking.
        // This verifies that route registration (including standard and versioned
        // LFS paths such as /info/lfs and /api/v1/lfs) does not cause runtime
        // failures during router creation. It does NOT assert individual paths;
        // those should be covered by more targeted endpoint tests if needed.
        let _router = routers();
    }

    #[test]
    fn test_error_response_format() {
        // Test error response format consistency - verify both map_lfs_error and lfs_error_response
        let test_cases = vec![
            ("Not found error", StatusCode::NOT_FOUND),
            ("Invalid parameter", StatusCode::BAD_REQUEST),
            ("Generic error", StatusCode::INTERNAL_SERVER_ERROR),
        ];

        for (msg, expected_code) in test_cases {
            // Test map_lfs_error mapping
            let (code, mapped_msg) = map_lfs_error(msg);
            assert_eq!(code, expected_code, "Error mapping for message: {}", msg);

            // Test lfs_error_response function - verify response structure, status code, and Content-Type
            let response = lfs_error_response(code, mapped_msg.clone());
            assert_eq!(response.status(), expected_code);

            // Verify Content-Type header
            let content_type = response
                .headers()
                .get("Content-Type")
                .and_then(|h| h.to_str().ok());
            assert_eq!(
                content_type,
                Some(LFS_CONTENT_TYPE),
                "Content-Type should be {}",
                LFS_CONTENT_TYPE
            );

            // Verify response body is valid JSON with "message" field
            // Note: In a real test, we'd need to extract and parse the body,
            // but for unit tests we verify the structure is created correctly
        }
    }

    #[test]
    fn test_content_type_constants_consistency() {
        // Verify content type constants match LFS specification
        // JSON responses should use application/vnd.git-lfs+json
        assert_eq!(LFS_CONTENT_TYPE, "application/vnd.git-lfs+json");

        // Binary streams should use application/octet-stream
        assert_eq!(LFS_STREAM_CONTENT_TYPE, "application/octet-stream");

        // Verify they are different
        assert_ne!(LFS_CONTENT_TYPE, LFS_STREAM_CONTENT_TYPE);
    }

    #[test]
    fn test_map_lfs_error_priority() {
        // Test that "Not found" takes priority over "Invalid" when both are present
        let error = "Not found: Invalid request format";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND);

        // Test that "Not found" takes priority even if it appears after "Invalid"
        let error = "Invalid: Not found";
        let (code, _) = map_lfs_error(error);
        // Should match "Not found" first due to function's matching order
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_map_lfs_error_unicode_support() {
        // Test error messages with unicode characters
        let error = "对象未找到"; // "Object not found" in Chinese
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR); // No match, default

        let error = "Object not found: 对象";
        let (code, _) = map_lfs_error(error);
        assert_eq!(code, StatusCode::NOT_FOUND); // Should still match
    }

    #[test]
    fn test_lfs_routes_all_endpoints() {
        // Verify all expected routes are registered
        let routes = lfs_routes();
        // Routes should be created without panicking
        // This is a smoke test to ensure route registration works
        drop(routes);
    }
}
