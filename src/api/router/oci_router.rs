use std::sync::LazyLock;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
            IF_RANGE, LOCATION, RANGE,
        },
    },
    response::{IntoResponse, Response},
    routing::{any, get},
};
use bytes::Bytes;
use regex::Regex;
use url::form_urlencoded;
use utoipa_axum::router::OpenApiRouter;

use crate::{
    api::MonoApiServiceState,
    ceres::oci::{
        auth::{
            authorize_repo_read, authorize_repo_write, registry_ping_allowed,
            token_from_authorization,
        },
        digest::{compute_digest, parse_digest},
        error::OciError,
        model::{
            DOCKER_MANIFEST_LIST_MEDIA_TYPE, DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE, Manifest,
            ManifestIndex, OCI_INDEX_MEDIA_TYPE, OCI_MANIFEST_MEDIA_TYPE,
        },
    },
    config::PushAuth,
    contract::git_protocol::{lookup_push_token, token_covers_repo},
    jupiter::{
        service::oci_service::DEFAULT_MAX_UPLOAD_CHUNK, utils::into_obj_stream::IntoObjectStream,
    },
};

const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const REGISTRY_API_VERSION: &str = "registry/2.0";

/// OCI repository path (`remoteName`): one or more `/`-separated path components.
/// Each component is `alphanumeric(?:(?:[._]|__|[-]+)alphanumeric)*`, matching
/// distribution `reference.pathComponent` / `remoteName` (without optional domain).
static REPOSITORY_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*)*$",
    )
    .expect("repository name regex")
});

static TAG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w][\w.-]{0,127}$").expect("tag name regex"));

/// Runtime routes for the OCI Distribution API, mounted at `/v2` by DR-11.
pub fn oci_routes() -> Router<MonoApiServiceState> {
    Router::new()
        .route("/", get(ping))
        .route("/{*tail}", any(dispatch))
}

/// OCI's OpenAPI contribution. Fine-grained operations are documented when
/// the full route family is mounted in DR-11.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new()
}

async fn ping(
    State(state): State<MonoApiServiceState>,
    request: Request,
) -> Result<Response, OciError> {
    registry_ping_allowed(&state.storage.config().git, request.headers())?;
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        "Docker-Distribution-API-Version",
        HeaderValue::from_static(REGISTRY_API_VERSION),
    );
    Ok(response)
}

async fn dispatch(
    State(state): State<MonoApiServiceState>,
    Path(tail): Path<String>,
    request: Request,
) -> Result<Response, OciError> {
    let segments: Vec<&str> = tail.split('/').collect();
    match segments.as_slice() {
        [name @ .., "manifests", reference]
            if !name.is_empty() && request.method() == Method::PUT =>
        {
            put_manifest(state, name.join("/"), reference, request).await
        }
        [name @ .., "manifests", reference]
            if !name.is_empty()
                && (*request.method() == Method::GET || *request.method() == Method::HEAD) =>
        {
            get_or_head_manifest(state, name.join("/"), reference, request).await
        }
        [name @ .., "manifests", _reference]
            if !name.is_empty() && request.method() == Method::DELETE =>
        {
            Err(OciError::Unsupported)
        }
        [name @ .., "blobs", digest]
            if !name.is_empty()
                && *digest != "uploads"
                && (*request.method() == Method::GET || *request.method() == Method::HEAD) =>
        {
            get_or_head_blob(state, name.join("/"), digest, request).await
        }
        [name @ .., "blobs", digest]
            if !name.is_empty() && *digest != "uploads" && request.method() == Method::DELETE =>
        {
            Err(OciError::Unsupported)
        }
        [name @ .., "blobs", "uploads"] | [name @ .., "blobs", "uploads", ""]
            if !name.is_empty() && request.method() == Method::POST =>
        {
            post_blob_upload(state, name.join("/"), request).await
        }
        [name @ .., "blobs", "uploads", uuid]
            if !name.is_empty() && !uuid.is_empty() && request.method() == Method::PATCH =>
        {
            patch_blob_upload(state, name.join("/"), uuid, request).await
        }
        [name @ .., "blobs", "uploads", uuid]
            if !name.is_empty() && !uuid.is_empty() && request.method() == Method::PUT =>
        {
            put_blob_upload_complete(state, name.join("/"), uuid, request).await
        }
        [name @ .., "blobs", "uploads", uuid]
            if !name.is_empty() && !uuid.is_empty() && request.method() == Method::GET =>
        {
            get_blob_upload_status(state, name.join("/"), uuid, request).await
        }
        [name @ .., "blobs", "uploads", uuid]
            if !name.is_empty() && !uuid.is_empty() && request.method() == Method::DELETE =>
        {
            delete_blob_upload(state, name.join("/"), uuid, request).await
        }
        _ => Err(OciError::NameInvalid),
    }
}

async fn get_or_head_blob(
    state: MonoApiServiceState,
    repo: String,
    digest_raw: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_read(&state.storage.config().git, request.headers(), &repo)?;

    let digest = parse_digest(digest_raw)?;
    let blob_ref = state
        .storage
        .oci_service
        .oci_storage
        .get_blob_ref(&repo, digest.as_str())
        .await
        .map_err(|_| OciError::BlobUnknown)?
        .ok_or(OciError::BlobUnknown)?;
    let size = blob_ref.size.max(0) as u64;

    if request.method() == Method::HEAD {
        let mut response = StatusCode::OK.into_response();
        insert_blob_headers(response.headers_mut(), digest.as_str(), size)?;
        return Ok(response);
    }

    match resolve_blob_range(request.headers(), size)? {
        BlobGetMode::Full => {
            let (byte_stream, _meta) = state
                .storage
                .oci_service
                .get_blob(digest.hex())
                .await
                .map_err(|_| OciError::BlobUnknown)?;
            let mut response = Body::from_stream(byte_stream).into_response();
            *response.status_mut() = StatusCode::OK;
            insert_blob_headers(response.headers_mut(), digest.as_str(), size)?;
            Ok(response)
        }
        BlobGetMode::Partial { start, end } => {
            let length = end - start + 1;
            let (byte_stream, _meta) = state
                .storage
                .oci_service
                .get_blob_range(digest.hex(), start, Some(end + 1))
                .await
                .map_err(|_| OciError::BlobUnknown)?;
            let mut response = Body::from_stream(byte_stream).into_response();
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            let headers = response.headers_mut();
            insert_blob_headers(headers, digest.as_str(), length)?;
            headers.insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                    .map_err(|_| OciError::BlobUnknown)?,
            );
            Ok(response)
        }
    }
}

fn insert_blob_headers(
    headers: &mut HeaderMap,
    digest: &str,
    content_length: u64,
) -> Result<(), OciError> {
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).map_err(|_| OciError::BlobUnknown)?,
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest).map_err(|_| OciError::BlobUnknown)?,
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum BlobGetMode {
    Full,
    Partial { start: u64, end: u64 },
}

/// Resolve a single `bytes=` Range. Multi-range and `If-Range` are ignored
/// (full 200), matching distribution-compatible registry behavior.
fn resolve_blob_range(headers: &HeaderMap, size: u64) -> Result<BlobGetMode, OciError> {
    if headers.contains_key(IF_RANGE) {
        return Ok(BlobGetMode::Full);
    }
    let Some(value) = headers.get(RANGE) else {
        return Ok(BlobGetMode::Full);
    };
    let Ok(value) = value.to_str() else {
        return Ok(BlobGetMode::Full);
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Ok(BlobGetMode::Full);
    };
    if spec.contains(',') {
        return Ok(BlobGetMode::Full);
    }
    let Some((start_spec, end_spec)) = spec.split_once('-') else {
        return Ok(BlobGetMode::Full);
    };
    let start_spec = start_spec.trim();
    let end_spec = end_spec.trim();
    if start_spec.is_empty() {
        return Ok(BlobGetMode::Full);
    }
    let Ok(start) = start_spec.parse::<u64>() else {
        return Ok(BlobGetMode::Full);
    };
    if start >= size {
        return Err(OciError::RangeInvalid);
    }
    let end = if end_spec.is_empty() {
        size.saturating_sub(1)
    } else {
        let Ok(requested_end) = end_spec.parse::<u64>() else {
            return Ok(BlobGetMode::Full);
        };
        if requested_end < start {
            return Ok(BlobGetMode::Full);
        }
        requested_end.min(size.saturating_sub(1))
    };
    Ok(BlobGetMode::Partial { start, end })
}

async fn post_blob_upload(
    state: MonoApiServiceState,
    repo: String,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }

    let query = request.uri().query();
    let mount = query_param(query, "mount");
    let from = query_param(query, "from");
    let digest = query_param(query, "digest");

    if mount.is_some() {
        return mount_blob(state, repo, mount, from, request).await;
    }
    if let Some(digest) = digest {
        return monolithic_blob_upload(state, repo, &digest, request).await;
    }

    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;
    start_upload_session(state, &repo).await
}

async fn start_upload_session(
    state: MonoApiServiceState,
    repo: &str,
) -> Result<Response, OciError> {
    let uuid = state
        .storage
        .oci_service
        .create_upload_session(repo)
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;
    upload_session_response(StatusCode::ACCEPTED, repo, &uuid, 0)
}

async fn mount_blob(
    state: MonoApiServiceState,
    repo: String,
    mount: Option<String>,
    from: Option<String>,
    request: Request,
) -> Result<Response, OciError> {
    let Some(mount) = mount else {
        return Err(OciError::DigestInvalid);
    };
    let Some(from) = from else {
        return Err(OciError::NameInvalid);
    };
    if !valid_repository_name(&from) {
        return Err(OciError::NameInvalid);
    }

    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;
    authorize_mount_source(&state.storage.config().git, request.headers(), &from)?;

    let digest = parse_digest(&mount)?;
    let source_ref = state
        .storage
        .oci_service
        .oci_storage
        .get_blob_ref(&from, digest.as_str())
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;

    let Some(source_ref) = source_ref else {
        return start_upload_session(state, &repo).await;
    };

    state
        .storage
        .oci_service
        .oci_storage
        .put_blob_ref(&repo, digest.as_str(), source_ref.size)
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;
    completed_blob_response(&repo, digest.as_str())
}

/// Mount requires target write auth plus source pull auth (GC-DR-02). When a
/// push token is presented, it must also cover the source repository path.
fn authorize_mount_source(
    git: &crate::config::GitConfig,
    headers: &HeaderMap,
    source: &str,
) -> Result<(), OciError> {
    authorize_repo_read(git, headers, source)?;
    if !matches!(git.push_auth, Some(PushAuth::Token)) {
        return Ok(());
    }
    let Some(presented) = token_from_authorization(headers) else {
        return Ok(());
    };
    let token = lookup_push_token(&git.push_tokens, &presented).ok_or(OciError::Unauthorized)?;
    if token_covers_repo(token, &format!("/{source}")) {
        Ok(())
    } else {
        Err(OciError::Denied)
    }
}

async fn monolithic_blob_upload(
    state: MonoApiServiceState,
    repo: String,
    digest_raw: &str,
    request: Request,
) -> Result<Response, OciError> {
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;
    let expected = parse_digest(digest_raw)?;

    let uuid = state
        .storage
        .oci_service
        .create_upload_session(&repo)
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;

    let upload = state
        .storage
        .oci_service
        .get_upload_session(&uuid, &repo)
        .await
        .map_err(|_| OciError::BlobUploadUnknown)?
        .ok_or(OciError::BlobUploadUnknown)?;

    let (chunks, size) =
        append_optional_upload_chunk(&state, &repo, &uuid, upload.offset, upload.chunks, request)
            .await?;

    finalize_upload_session(&state, &repo, &uuid, chunks, size, expected.as_str()).await
}

async fn put_blob_upload_complete(
    state: MonoApiServiceState,
    repo: String,
    uuid: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let expected_raw =
        query_param(request.uri().query(), "digest").ok_or(OciError::DigestInvalid)?;
    let expected = parse_digest(&expected_raw)?;

    let upload = state
        .storage
        .oci_service
        .get_upload_session(uuid, &repo)
        .await
        .map_err(|_| OciError::BlobUploadUnknown)?
        .ok_or(OciError::BlobUploadUnknown)?;

    let (chunks, size) =
        append_optional_upload_chunk(&state, &repo, uuid, upload.offset, upload.chunks, request)
            .await?;

    finalize_upload_session(&state, &repo, uuid, chunks, size, expected.as_str()).await
}

async fn delete_blob_upload(
    state: MonoApiServiceState,
    repo: String,
    uuid: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let upload = state
        .storage
        .oci_service
        .get_upload_session(uuid, &repo)
        .await
        .map_err(|_| OciError::BlobUploadUnknown)?
        .ok_or(OciError::BlobUploadUnknown)?;

    state
        .storage
        .oci_service
        .delete_chunks(uuid, upload.chunks.max(0) as u64)
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;
    state
        .storage
        .oci_service
        .oci_storage
        .delete_upload(uuid)
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Apply PATCH-equivalent rules when a trailing body is present; empty bodies
/// leave the session unchanged. Returns `(chunks, offset)`.
async fn append_optional_upload_chunk(
    state: &MonoApiServiceState,
    repo: &str,
    uuid: &str,
    offset: i64,
    chunks: i64,
    request: Request,
) -> Result<(u64, i64), OciError> {
    let content_range = parse_upload_content_range(request.headers())?;
    if let Some((start, _)) = content_range
        && start != offset as u64
    {
        return Err(OciError::RangeInvalid);
    }

    let content_length = parse_content_length(request.headers())?;
    if let (Some(cl), Some((start, end))) = (content_length, content_range)
        && cl != end.saturating_sub(start).saturating_add(1)
    {
        return Err(OciError::SizeInvalid);
    }

    let body = to_bytes(request.into_body(), DEFAULT_MAX_UPLOAD_CHUNK + 1)
        .await
        .map_err(|_| OciError::SizeInvalid)?;
    if body.len() > DEFAULT_MAX_UPLOAD_CHUNK {
        return Err(OciError::SizeInvalid);
    }
    if let Some(cl) = content_length
        && body.len() as u64 != cl
    {
        return Err(OciError::SizeInvalid);
    }

    if body.is_empty() {
        let _ = repo;
        return Ok((chunks.max(0) as u64, offset));
    }

    let chunk_len = body.len() as i64;
    let updated = state
        .storage
        .oci_service
        .append_upload_chunk(
            uuid,
            offset,
            chunks,
            chunk_len,
            Bytes::from(body.to_vec()).into_stream(),
        )
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?
        .ok_or(OciError::RangeInvalid)?;
    Ok((updated.1.max(0) as u64, updated.0))
}

async fn finalize_upload_session(
    state: &MonoApiServiceState,
    repo: &str,
    uuid: &str,
    chunks: u64,
    size: i64,
    expected_digest: &str,
) -> Result<Response, OciError> {
    match state
        .storage
        .oci_service
        .finalize_blob(uuid, chunks, expected_digest)
        .await
    {
        Ok(digest) => {
            state
                .storage
                .oci_service
                .oci_storage
                .put_blob_ref(repo, &digest, size)
                .await
                .map_err(|_| OciError::BlobUploadInvalid)?;
            state
                .storage
                .oci_service
                .delete_chunks(uuid, chunks)
                .await
                .map_err(|_| OciError::BlobUploadInvalid)?;
            state
                .storage
                .oci_service
                .oci_storage
                .delete_upload(uuid)
                .await
                .map_err(|_| OciError::BlobUploadInvalid)?;
            completed_blob_response(repo, &digest)
        }
        Err(error) if error.to_string().contains("digest mismatch") => Err(OciError::DigestInvalid),
        Err(_) => Err(OciError::BlobUploadInvalid),
    }
}

fn completed_blob_response(repo: &str, digest: &str) -> Result<Response, OciError> {
    let location = format!("/v2/{repo}/blobs/{digest}");
    let mut response = StatusCode::CREATED.into_response();
    let headers = response.headers_mut();
    headers.insert(
        LOCATION,
        HeaderValue::from_str(&location).map_err(|_| OciError::BlobUploadInvalid)?,
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest).map_err(|_| OciError::BlobUploadInvalid)?,
    );
    Ok(response)
}

async fn patch_blob_upload(
    state: MonoApiServiceState,
    repo: String,
    uuid: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let upload = state
        .storage
        .oci_service
        .get_upload_session(uuid, &repo)
        .await
        .map_err(|_| OciError::BlobUploadUnknown)?
        .ok_or(OciError::BlobUploadUnknown)?;

    let content_range = parse_upload_content_range(request.headers())?;
    if let Some((start, _)) = content_range
        && start != upload.offset as u64
    {
        return Err(OciError::RangeInvalid);
    }

    let content_length = parse_content_length(request.headers())?;
    if let (Some(cl), Some((start, end))) = (content_length, content_range)
        && cl != end.saturating_sub(start).saturating_add(1)
    {
        return Err(OciError::SizeInvalid);
    }

    let body = to_bytes(request.into_body(), DEFAULT_MAX_UPLOAD_CHUNK + 1)
        .await
        .map_err(|_| OciError::SizeInvalid)?;
    if body.len() > DEFAULT_MAX_UPLOAD_CHUNK {
        return Err(OciError::SizeInvalid);
    }
    if let Some(cl) = content_length
        && body.len() as u64 != cl
    {
        return Err(OciError::SizeInvalid);
    }

    let chunk_len = body.len() as i64;
    let updated = state
        .storage
        .oci_service
        .append_upload_chunk(
            uuid,
            upload.offset,
            upload.chunks,
            chunk_len,
            Bytes::from(body.to_vec()).into_stream(),
        )
        .await
        .map_err(|_| OciError::BlobUploadInvalid)?
        .ok_or(OciError::RangeInvalid)?;
    upload_session_response(StatusCode::ACCEPTED, &repo, uuid, updated.0)
}

async fn get_blob_upload_status(
    state: MonoApiServiceState,
    repo: String,
    uuid: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let upload = state
        .storage
        .oci_service
        .get_upload_session(uuid, &repo)
        .await
        .map_err(|_| OciError::BlobUploadUnknown)?
        .ok_or(OciError::BlobUploadUnknown)?;
    upload_session_response(StatusCode::NO_CONTENT, &repo, uuid, upload.offset)
}

fn upload_session_response(
    status: StatusCode,
    repo: &str,
    uuid: &str,
    offset: i64,
) -> Result<Response, OciError> {
    let location = format!("/v2/{repo}/blobs/uploads/{uuid}");
    let range = upload_range_header(offset);
    let mut response = status.into_response();
    let headers = response.headers_mut();
    headers.insert(
        LOCATION,
        HeaderValue::from_str(&location).map_err(|_| OciError::BlobUploadInvalid)?,
    );
    headers.insert(
        "Docker-Upload-UUID",
        HeaderValue::from_str(uuid).map_err(|_| OciError::BlobUploadInvalid)?,
    );
    headers.insert(
        RANGE,
        HeaderValue::from_str(&range).map_err(|_| OciError::BlobUploadInvalid)?,
    );
    Ok(response)
}

fn upload_range_header(offset: i64) -> String {
    if offset <= 0 {
        "0-0".to_owned()
    } else {
        format!("0-{}", offset - 1)
    }
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key == name {
            let value = value.into_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Parse upload `Content-Range` as `start-end`, optional `bytes ` prefix and
/// optional `/*` total (distribution + HTTP forms).
fn parse_upload_content_range(headers: &HeaderMap) -> Result<Option<(u64, u64)>, OciError> {
    let Some(value) = headers.get(CONTENT_RANGE) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| OciError::RangeInvalid)?;
    let mut spec = value.trim();
    if let Some(rest) = spec.strip_prefix("bytes") {
        spec = rest.trim_start();
    }
    if let Some((range, _total)) = spec.split_once('/') {
        spec = range.trim();
    }
    let Some((start_spec, end_spec)) = spec.split_once('-') else {
        return Err(OciError::RangeInvalid);
    };
    let start = start_spec
        .trim()
        .parse::<u64>()
        .map_err(|_| OciError::RangeInvalid)?;
    let end = end_spec
        .trim()
        .parse::<u64>()
        .map_err(|_| OciError::RangeInvalid)?;
    if start > end {
        return Err(OciError::RangeInvalid);
    }
    Ok(Some((start, end)))
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, OciError> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| OciError::SizeInvalid)?;
    Ok(Some(
        value.parse::<u64>().map_err(|_| OciError::SizeInvalid)?,
    ))
}

async fn get_or_head_manifest(
    state: MonoApiServiceState,
    repo: String,
    reference: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_read(&state.storage.config().git, request.headers(), &repo)?;

    let oci_storage = &state.storage.oci_service.oci_storage;
    let digest = if reference.starts_with("sha256:") {
        parse_digest(reference)?.as_str().to_owned()
    } else {
        match oci_storage
            .get_tag(&repo, reference)
            .await
            .map_err(|_| OciError::ManifestUnknown)?
        {
            Some(tag) => tag.digest,
            None => {
                return if oci_storage
                    .repo_exists(&repo)
                    .await
                    .map_err(|_| OciError::ManifestUnknown)?
                {
                    Err(OciError::ManifestUnknown)
                } else {
                    Err(OciError::NameUnknown)
                };
            }
        }
    };

    let row = oci_storage
        .get_manifest(&repo, &digest)
        .await
        .map_err(|_| OciError::ManifestUnknown)?
        .ok_or(OciError::ManifestUnknown)?;

    let etag = format!("\"{digest}\"");
    if etag_match(request.headers(), &digest) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let headers = response.headers_mut();
        headers.insert(
            "Docker-Content-Digest",
            HeaderValue::from_str(&digest).map_err(|_| OciError::ManifestInvalid)?,
        );
        headers.insert(
            ETAG,
            HeaderValue::from_str(&etag).map_err(|_| OciError::ManifestInvalid)?,
        );
        return Ok(response);
    }

    let mut response = if request.method() == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        let hex = parse_digest(&digest)?.hex().to_owned();
        let (byte_stream, _meta) = state
            .storage
            .oci_service
            .get_manifest(&hex)
            .await
            .map_err(|_| OciError::ManifestUnknown)?;
        Body::from_stream(byte_stream).into_response()
    };

    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&row.media_type).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(&digest).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        ETAG,
        HeaderValue::from_str(&etag).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&row.size.to_string()).map_err(|_| OciError::ManifestInvalid)?,
    );
    Ok(response)
}

fn etag_match(headers: &HeaderMap, digest: &str) -> bool {
    let quoted = format!("\"{digest}\"");
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value == digest || value == quoted)
}

async fn put_manifest(
    state: MonoApiServiceState,
    repo: String,
    reference: &str,
    request: Request,
) -> Result<Response, OciError> {
    if !valid_repository_name(&repo) {
        return Err(OciError::NameInvalid);
    }
    authorize_repo_write(&state.storage.config().git, request.headers(), &repo)?;

    let body = to_bytes(request.into_body(), MAX_MANIFEST_BYTES + 1)
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(OciError::ManifestInvalid);
    }

    let media_type = manifest_media_type(&body)?;
    let digest = compute_digest(&body);
    if reference.starts_with("sha256:") {
        let expected = parse_digest(reference)?;
        if expected != digest {
            return Err(OciError::DigestInvalid);
        }
    } else if !valid_tag(reference) {
        return Err(OciError::TagInvalid);
    }
    validate_references(&state, &repo, &media_type, &body).await?;

    state
        .storage
        .oci_service
        .put_manifest(digest.hex(), Bytes::from(body.to_vec()).into_stream())
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    state
        .storage
        .oci_service
        .oci_storage
        .put_manifest(&repo, digest.as_str(), &media_type, body.len() as i64)
        .await
        .map_err(|_| OciError::ManifestInvalid)?;
    if !reference.starts_with("sha256:") {
        state
            .storage
            .oci_service
            .oci_storage
            .upsert_tag(&repo, reference, digest.as_str())
            .await
            .map_err(|_| OciError::ManifestInvalid)?;
    }

    let location = format!("/v2/{repo}/manifests/{digest}");
    let mut response = StatusCode::CREATED.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "Location",
        HeaderValue::from_str(&location).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest.as_str()).map_err(|_| OciError::ManifestInvalid)?,
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&media_type).map_err(|_| OciError::ManifestInvalid)?,
    );
    Ok(response)
}

fn manifest_media_type(body: &[u8]) -> Result<String, OciError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
    let media_type = value
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .ok_or(OciError::ManifestInvalid)?;
    if supported_media_type(media_type) {
        Ok(media_type.to_owned())
    } else {
        Err(OciError::ManifestInvalid)
    }
}

async fn validate_references(
    state: &MonoApiServiceState,
    repo: &str,
    media_type: &str,
    body: &[u8],
) -> Result<(), OciError> {
    if matches!(
        media_type,
        DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE | OCI_MANIFEST_MEDIA_TYPE
    ) {
        let manifest: Manifest =
            serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
        for descriptor in std::iter::once(&manifest.config).chain(&manifest.layers) {
            parse_digest(&descriptor.digest).map_err(|_| OciError::ManifestInvalid)?;
            if !state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists(repo, &descriptor.digest)
                .await
                .map_err(|_| OciError::ManifestInvalid)?
            {
                return Err(OciError::ManifestBlobUnknown);
            }
        }
    } else {
        let index: ManifestIndex =
            serde_json::from_slice(body).map_err(|_| OciError::ManifestInvalid)?;
        for descriptor in &index.manifests {
            parse_digest(&descriptor.digest).map_err(|_| OciError::ManifestInvalid)?;
            if state
                .storage
                .oci_service
                .oci_storage
                .get_manifest(repo, &descriptor.digest)
                .await
                .map_err(|_| OciError::ManifestInvalid)?
                .is_none()
            {
                return Err(OciError::ManifestBlobUnknown);
            }
        }
    }
    Ok(())
}

fn supported_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE
            | DOCKER_MANIFEST_LIST_MEDIA_TYPE
            | OCI_MANIFEST_MEDIA_TYPE
            | OCI_INDEX_MEDIA_TYPE
    )
}

fn valid_repository_name(name: &str) -> bool {
    if name.is_empty() || name.split('/').any(|segment| segment == "..") {
        return false;
    }
    REPOSITORY_NAME.is_match(name)
}

fn valid_tag(tag: &str) -> bool {
    TAG_NAME.is_match(tag)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{
                ACCEPT_RANGES, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
                IF_NONE_MATCH, RANGE, WWW_AUTHENTICATE,
            },
        },
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use tower::ServiceExt;

    use super::oci_routes;
    use crate::{
        api::{
            MonoApiServiceState,
            oauth::api_store::{BrowserSessionStore, CountingSessionStore},
        },
        bellatrix::Bellatrix,
        ceres::{api_service::cache::GitObjectCache, oci::digest::compute_digest},
        config::{GitConfig, PushAuth, PushTokenConfig, testing::isolated_config},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            service::oci_service::OciService,
            storage::{Storage, object_storage::mock_object_storage},
            tests::{test_db_config, test_storage_with_config},
            utils::into_obj_stream::IntoObjectStream,
        },
    };

    async fn state(anonymous_access: bool) -> MonoApiServiceState {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("config"));
        config.database = test_db_config(temp_dir.path()).await;
        config.git = GitConfig {
            anonymous_access,
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![PushTokenConfig {
                name: "test".to_owned(),
                token: "secret".to_owned(),
                paths: Some(vec!["/team".to_owned()]),
            }],
            ssh_receive_pack: Some(false),
        };
        let mut storage = test_storage_with_config(temp_dir.path(), config).await;
        storage.oci_service = OciService {
            oci_storage: storage.oci_db_storage(),
            obj_storage: mock_object_storage(),
        };
        storage
            .oci_service
            .oci_storage
            .put_blob_ref(
                "team/image",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                0,
            )
            .await
            .expect("seed manifest config blob");
        state_with_storage(storage)
    }

    fn state_with_storage(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "oci-router-test".to_owned(),
            }),
            listen_addr: "http://127.0.0.1:0".to_owned(),
            entity_store: Arc::new(SharedEntityStore::new()),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            storage,
        }
    }

    fn manifest() -> String {
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":0},"layers":[]}"#.to_owned()
    }

    fn authenticated(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(
            AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("any:secret")),
        )
    }

    async fn seed_blob(state: &MonoApiServiceState, bytes: &[u8]) -> String {
        let digest = compute_digest(bytes);
        state
            .storage
            .oci_service
            .put_blob(digest.hex(), Bytes::copy_from_slice(bytes).into_stream())
            .await
            .expect("put blob bytes");
        state
            .storage
            .oci_service
            .oci_storage
            .put_blob_ref("team/image", digest.as_str(), bytes.len() as i64)
            .await
            .expect("put blob ref");
        digest.as_str().to_owned()
    }

    #[tokio::test]
    async fn ping_anonymous_open_ok() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Docker-Distribution-API-Version")
                .expect("version"),
            "registry/2.0"
        );
    }

    #[tokio::test]
    async fn ping_anonymous_closed_401() {
        let response = oci_routes()
            .with_state(state(false).await)
            .oneshot(Request::get("/").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(WWW_AUTHENTICATE));
    }

    #[tokio::test]
    async fn ping_invalid_credentials_401() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                Request::get("/")
                    .header(AUTHORIZATION, "Bearer invalid")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_manifest_by_tag_201() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .header(CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.headers().contains_key("Location"));
        assert!(response.headers().contains_key("Docker-Content-Digest"));
    }

    #[tokio::test]
    async fn put_manifest_sets_tag_row() {
        let state = state(true).await;
        let storage = state.storage.clone();
        let response = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let digest = response
            .headers()
            .get("Docker-Content-Digest")
            .expect("digest")
            .to_str()
            .expect("valid digest");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            storage
                .oci_service
                .oci_storage
                .get_tag("team/image", "v1")
                .await
                .expect("tag")
                .expect("tag exists")
                .digest,
            digest
        );
    }

    #[tokio::test]
    async fn put_manifest_by_digest_mismatch() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put(format!(
                    "/team/image/manifests/sha256:{}",
                    "0".repeat(64)
                )))
                .body(Body::from(manifest()))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("DIGEST_INVALID")
        );
    }

    #[tokio::test]
    async fn put_manifest_invalid_name() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::put("/team/../image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("NAME_INVALID")
        );
    }

    #[tokio::test]
    async fn get_manifest_by_tag_200() {
        let state = state(true).await;
        let put = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .header(CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("put");
        assert_eq!(put.status(), StatusCode::CREATED);

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get("/team/image/manifests/v1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .expect("content-type")
                .to_str()
                .expect("valid"),
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert!(response.headers().contains_key("Docker-Content-Digest"));
    }

    #[tokio::test]
    async fn get_manifest_by_digest_200() {
        let state = state(true).await;
        let put = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("put");
        let digest = put
            .headers()
            .get("Docker-Content-Digest")
            .expect("digest")
            .to_str()
            .expect("valid digest")
            .to_owned();

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/manifests/{digest}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_manifest_if_none_match_304() {
        let state = state(true).await;
        let put = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("put");
        let digest = put
            .headers()
            .get("Docker-Content-Digest")
            .expect("digest")
            .to_str()
            .expect("valid digest")
            .to_owned();

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get("/team/image/manifests/v1")
                    .header(IF_NONE_MATCH, &digest)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(response.headers().contains_key("Docker-Content-Digest"));
        assert!(response.headers().contains_key(ETAG));
    }

    #[tokio::test]
    async fn get_manifest_unknown_repo() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                Request::get("/missing/repo/manifests/v1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("NAME_UNKNOWN")
        );
    }

    #[tokio::test]
    async fn get_manifest_unknown_tag() {
        // `state()` seeds an `oci_blob_ref` for team/image (no tags), so the
        // repo is known and a missing tag must be MANIFEST_UNKNOWN, not NAME_UNKNOWN.
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                Request::get("/team/image/manifests/missing")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("MANIFEST_UNKNOWN")
        );
    }

    #[tokio::test]
    async fn head_manifest_headers_no_body() {
        let state = state(true).await;
        let put = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::put("/team/image/manifests/v1"))
                    .body(Body::from(manifest()))
                    .expect("request"),
            )
            .await
            .expect("put");
        assert_eq!(put.status(), StatusCode::CREATED);

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::head("/team/image/manifests/v1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("head");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(CONTENT_TYPE));
        assert!(response.headers().contains_key("Docker-Content-Digest"));
        assert!(response.headers().contains_key(ETAG));
        assert!(response.headers().contains_key(CONTENT_LENGTH));
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.len(), 0);
    }

    #[tokio::test]
    async fn manifest_read_auth_anonymous_off() {
        let response = oci_routes()
            .with_state(state(false).await)
            .oneshot(
                Request::get("/team/image/manifests/v1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn head_blob_sets_three_headers() {
        let state = state(true).await;
        let digest = seed_blob(&state, b"blob-bytes").await;

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::head(format!("/team/image/blobs/{digest}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("head");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LENGTH)
                .expect("content-length")
                .to_str()
                .expect("valid"),
            "10"
        );
        assert_eq!(
            response
                .headers()
                .get("Docker-Content-Digest")
                .expect("digest")
                .to_str()
                .expect("valid"),
            digest
        );
        assert_eq!(
            response
                .headers()
                .get(ACCEPT_RANGES)
                .expect("accept-ranges")
                .to_str()
                .expect("valid"),
            "bytes"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.len(), 0);
    }

    #[tokio::test]
    async fn get_blob_returns_source_bytes() {
        let state = state(true).await;
        let payload = b"source-blob-payload";
        let digest = seed_blob(&state, payload).await;

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{digest}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Docker-Content-Digest")
                .expect("digest")
                .to_str()
                .expect("valid"),
            digest
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), payload);
    }

    #[tokio::test]
    async fn get_blob_range_206_content_range() {
        let state = state(true).await;
        let payload = b"0123456789";
        let digest = seed_blob(&state, payload).await;

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{digest}"))
                    .header(RANGE, "bytes=2-5")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_RANGE)
                .expect("content-range")
                .to_str()
                .expect("valid"),
            "bytes 2-5/10"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), b"2345");
    }

    #[tokio::test]
    async fn get_blob_range_start_ge_size_416() {
        let state = state(true).await;
        let digest = seed_blob(&state, b"abcd").await;

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{digest}"))
                    .header(RANGE, "bytes=4-7")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("RANGE_INVALID")
        );
    }

    #[tokio::test]
    async fn get_blob_unknown_404() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                Request::get(format!("/team/image/blobs/{digest}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("BLOB_UNKNOWN")
        );
    }

    #[tokio::test]
    async fn blob_read_auth_anonymous_off() {
        let state = state(false).await;
        let digest = seed_blob(&state, b"auth-check").await;

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{digest}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("get");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    async fn init_upload(state: MonoApiServiceState) -> (MonoApiServiceState, String) {
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::post("/team/image/blobs/uploads/"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("init");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let uuid = response
            .headers()
            .get("Docker-Upload-UUID")
            .expect("uuid")
            .to_str()
            .expect("valid uuid")
            .to_owned();
        (state, uuid)
    }

    #[tokio::test]
    async fn init_returns_session_headers() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::post("/team/image/blobs/uploads/"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("init");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let uuid = response
            .headers()
            .get("Docker-Upload-UUID")
            .expect("uuid")
            .to_str()
            .expect("valid uuid");
        assert_eq!(
            response
                .headers()
                .get("Location")
                .expect("location")
                .to_str()
                .expect("valid"),
            format!("/v2/team/image/blobs/uploads/{uuid}")
        );
        assert_eq!(
            response
                .headers()
                .get(RANGE)
                .expect("range")
                .to_str()
                .expect("valid"),
            "0-0"
        );
    }

    #[tokio::test]
    async fn patch_updates_offset() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"chunk-one";
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .header(CONTENT_LENGTH, payload.len().to_string())
                    .body(Body::from(payload.as_slice()))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response
                .headers()
                .get(RANGE)
                .expect("range")
                .to_str()
                .expect("valid"),
            format!("0-{}", payload.len() - 1)
        );
        let upload = state
            .storage
            .oci_service
            .oci_storage
            .get_upload(&uuid)
            .await
            .expect("get upload")
            .expect("upload exists");
        assert_eq!(upload.offset, payload.len() as i64);
    }

    #[tokio::test]
    async fn patch_updates_chunks() {
        let (state, uuid) = init_upload(state(true).await).await;
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::from(&b"abc"[..]))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let upload = state
            .storage
            .oci_service
            .oci_storage
            .get_upload(&uuid)
            .await
            .expect("get upload")
            .expect("upload exists");
        assert_eq!(upload.chunks, 1);
    }

    #[tokio::test]
    async fn patch_range_mismatch_keeps_session() {
        let (state, uuid) = init_upload(state(true).await).await;
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .header(CONTENT_RANGE, "bytes 5-8/*")
                    .header(CONTENT_LENGTH, "4")
                    .body(Body::from(&b"abcd"[..]))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("RANGE_INVALID")
        );
        let upload = state
            .storage
            .oci_service
            .oci_storage
            .get_upload(&uuid)
            .await
            .expect("get upload")
            .expect("session kept");
        assert_eq!((upload.offset, upload.chunks), (0, 0));
    }

    #[tokio::test]
    async fn patch_size_mismatch_keeps_session() {
        let (state, uuid) = init_upload(state(true).await).await;
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .header(CONTENT_RANGE, "bytes 0-3/*")
                    .header(CONTENT_LENGTH, "9")
                    .body(Body::from(&b"abcd"[..]))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("SIZE_INVALID")
        );
        let upload = state
            .storage
            .oci_service
            .oci_storage
            .get_upload(&uuid)
            .await
            .expect("get upload")
            .expect("session kept");
        assert_eq!((upload.offset, upload.chunks), (0, 0));
    }

    #[tokio::test]
    async fn status_204_with_range_header() {
        let (state, uuid) = init_upload(state(true).await).await;
        let patch = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::from(&b"hello"[..]))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(patch.status(), StatusCode::ACCEPTED);

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::get(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("status");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(RANGE)
                .expect("range")
                .to_str()
                .expect("valid"),
            "0-4"
        );
        assert_eq!(
            response
                .headers()
                .get("Docker-Upload-UUID")
                .expect("uuid")
                .to_str()
                .expect("valid"),
            uuid
        );
    }

    #[tokio::test]
    async fn session_survives_restart() {
        let (state, uuid) = init_upload(state(true).await).await;
        let patch = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::from(&b"persist"[..]))
                    .expect("request"),
            )
            .await
            .expect("patch");
        assert_eq!(patch.status(), StatusCode::ACCEPTED);

        // Drop in-memory API state and rebuild from the same Storage (DB + CAS).
        let restarted = state_with_storage(state.storage.clone());
        let response = oci_routes()
            .with_state(restarted)
            .oneshot(
                authenticated(Request::get(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("status after restart");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(RANGE)
                .expect("range")
                .to_str()
                .expect("valid"),
            "0-6"
        );
    }

    #[tokio::test]
    async fn unknown_upload_uuid_404() {
        let response = oci_routes()
            .with_state(state(true).await)
            .oneshot(
                authenticated(Request::get(
                    "/team/image/blobs/uploads/00000000-0000-4000-8000-000000000000",
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("status");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("BLOB_UPLOAD_UNKNOWN")
        );
    }

    async fn patch_payload(
        state: &MonoApiServiceState,
        uuid: &str,
        payload: &[u8],
    ) -> axum::http::Response<Body> {
        oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::patch(format!("/team/image/blobs/uploads/{uuid}")))
                    .header(CONTENT_LENGTH, payload.len().to_string())
                    .body(Body::from(payload.to_vec()))
                    .expect("request"),
            )
            .await
            .expect("patch")
    }

    async fn put_complete(
        state: &MonoApiServiceState,
        uuid: &str,
        digest: &str,
        body: Body,
    ) -> axum::http::Response<Body> {
        oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::put(format!(
                    "/team/image/blobs/uploads/{uuid}?digest={digest}"
                )))
                .body(body)
                .expect("request"),
            )
            .await
            .expect("complete")
    }

    #[tokio::test]
    async fn complete_creates_blob_ref() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"complete-blob-ref";
        let digest = compute_digest(payload);
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );

        let response = put_complete(&state, &uuid, digest.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let blob_ref = state
            .storage
            .oci_service
            .oci_storage
            .get_blob_ref("team/image", digest.as_str())
            .await
            .expect("get blob ref")
            .expect("blob ref exists");
        assert_eq!(blob_ref.size, payload.len() as i64);

        let get = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{}", digest.as_str()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("blob get");
        assert_eq!(get.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn complete_removes_chunks() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"remove-chunks";
        let digest = compute_digest(payload);
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );
        assert!(
            state
                .storage
                .oci_service
                .get_chunk_stream(&uuid, 0)
                .await
                .is_ok()
        );

        let response = put_complete(&state, &uuid, digest.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            state
                .storage
                .oci_service
                .get_chunk_stream(&uuid, 0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn complete_returns_dcd_header() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"dcd-header";
        let digest = compute_digest(payload);
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );

        let response = put_complete(&state, &uuid, digest.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("Docker-Content-Digest")
                .expect("dcd")
                .to_str()
                .expect("utf8"),
            digest.as_str()
        );
        assert_eq!(
            response
                .headers()
                .get("Location")
                .expect("location")
                .to_str()
                .expect("utf8"),
            format!("/v2/team/image/blobs/{}", digest.as_str())
        );
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .get_upload(&uuid)
                .await
                .expect("get upload")
                .is_none()
        );
    }

    #[tokio::test]
    async fn digest_mismatch_keeps_session() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"actual-bytes";
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );
        let wrong = compute_digest(b"other-bytes");

        let response = put_complete(&state, &uuid, wrong.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("DIGEST_INVALID")
        );
        let upload = state
            .storage
            .oci_service
            .oci_storage
            .get_upload(&uuid)
            .await
            .expect("get upload")
            .expect("session kept");
        assert_eq!(upload.chunks, 1);
        assert!(
            state
                .storage
                .oci_service
                .get_chunk_stream(&uuid, 0)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn retry_after_mismatch_succeeds() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"retry-bytes";
        let digest = compute_digest(payload);
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );
        let wrong = compute_digest(b"nope");
        assert_eq!(
            put_complete(&state, &uuid, wrong.as_str(), Body::empty())
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        let response = put_complete(&state, &uuid, digest.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists("team/image", digest.as_str())
                .await
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn monolithic_upload_201() {
        let state = state(true).await;
        let payload = b"monolithic-body";
        let digest = compute_digest(payload);
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::post(format!(
                    "/team/image/blobs/uploads/?digest={}",
                    digest.as_str()
                )))
                .header(CONTENT_LENGTH, payload.len().to_string())
                .body(Body::from(payload.as_slice()))
                .expect("request"),
            )
            .await
            .expect("monolithic");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("Docker-Content-Digest")
                .expect("dcd")
                .to_str()
                .expect("utf8"),
            digest.as_str()
        );
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists("team/image", digest.as_str())
                .await
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn pass_b_crash_retry_safe() {
        let (state, uuid) = init_upload(state(true).await).await;
        let payload = b"pass-b-retry";
        let digest = compute_digest(payload);
        assert_eq!(
            patch_payload(&state, &uuid, payload).await.status(),
            StatusCode::ACCEPTED
        );

        // Simulate pass-B crash: CAS key exists without a blob_ref row.
        state
            .storage
            .oci_service
            .put_blob(
                digest.hex(),
                Bytes::from_static(b"partial-or-stale").into_stream(),
            )
            .await
            .expect("seed stale cas");
        assert!(
            !state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists("team/image", digest.as_str())
                .await
                .expect("exists check")
        );

        let response = put_complete(&state, &uuid, digest.as_str(), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists("team/image", digest.as_str())
                .await
                .expect("exists")
        );

        let get = oci_routes()
            .with_state(state)
            .oneshot(
                Request::get(format!("/team/image/blobs/{}", digest.as_str()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("blob get");
        assert_eq!(get.status(), StatusCode::OK);
        let body = to_bytes(get.into_body(), usize::MAX).await.expect("body");
        assert_eq!(&body[..], payload);
    }

    #[tokio::test]
    async fn mount_returns_201() {
        let state = state(true).await;
        let digest = seed_blob(&state, b"mount-source").await;
        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::post(format!(
                    "/team/app/blobs/uploads/?mount={digest}&from=team/image"
                )))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("mount");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("Location")
                .expect("location")
                .to_str()
                .expect("utf8"),
            format!("/v2/team/app/blobs/{digest}")
        );
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .blob_ref_exists("team/app", &digest)
                .await
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn mount_source_denied() {
        let state = state(true).await;
        let digest = seed_blob(&state, b"denied-source").await;
        // Seed a source the token cannot pull from under path-scoped mount auth.
        state
            .storage
            .oci_service
            .oci_storage
            .put_blob_ref("other/image", &digest, 13)
            .await
            .expect("seed other ref");

        let response = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::post(format!(
                    "/team/image/blobs/uploads/?mount={digest}&from=other/image"
                )))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("mount");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(std::str::from_utf8(&body).expect("utf8").contains("DENIED"));
    }

    #[tokio::test]
    async fn mount_fallback_session() {
        let state = state(true).await;
        let digest = format!("sha256:{}", "c".repeat(64));
        let response = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::post(format!(
                    "/team/image/blobs/uploads/?mount={digest}&from=team/image"
                )))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("mount");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(response.headers().contains_key("Docker-Upload-UUID"));
        assert!(
            response
                .headers()
                .get("Location")
                .expect("location")
                .to_str()
                .expect("utf8")
                .contains("/blobs/uploads/")
        );
    }

    #[tokio::test]
    async fn delete_upload_204() {
        let (state, uuid) = init_upload(state(true).await).await;
        assert_eq!(
            patch_payload(&state, &uuid, b"to-delete").await.status(),
            StatusCode::ACCEPTED
        );

        let response = oci_routes()
            .with_state(state.clone())
            .oneshot(
                authenticated(Request::delete(format!("/team/image/blobs/uploads/{uuid}")))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("delete");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            state
                .storage
                .oci_service
                .oci_storage
                .get_upload(&uuid)
                .await
                .expect("get upload")
                .is_none()
        );
        assert!(
            state
                .storage
                .oci_service
                .get_chunk_stream(&uuid, 0)
                .await
                .is_err()
        );

        let missing = oci_routes()
            .with_state(state)
            .oneshot(
                authenticated(Request::delete(
                    "/team/image/blobs/uploads/00000000-0000-4000-8000-000000000000",
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("delete missing");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(missing.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("utf8")
                .contains("BLOB_UPLOAD_UNKNOWN")
        );
    }

    #[test]
    fn repository_name_matches_distribution_remote_name() {
        assert!(super::valid_repository_name("team/image"));
        assert!(super::valid_repository_name("team/foo.bar"));
        assert!(super::valid_repository_name("team/foo_bar"));
        assert!(super::valid_repository_name("team/foo__bar"));
        assert!(super::valid_repository_name("team/foo--bar"));
        assert!(!super::valid_repository_name("team/image."));
        assert!(!super::valid_repository_name("team/image__"));
        assert!(!super::valid_repository_name("team/../image"));
        assert!(!super::valid_repository_name(""));
    }
}
