//! Content transport handlers (spec 04 §9, spec 06, spec 07):
//! HEAD blob, batched OBJECT frames, chunk-map descriptors/pages and CHUNK
//! frames. Identity encoding only in this slice; zstd negotiation is a later
//! WP and `frame_encodings` advertises identity alone.

use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use futures::stream::StreamExt;
use serde::Deserialize;
use serde_json::json;

use super::{
    abs_view_path, guarded_treeframe_response, internal, mst2_error_response, request::Mst2Bytes,
};
use crate::ceres::snapshot::{
    chunks::{ChunkProjection, get_or_project},
    error::{SnapshotError, SnapshotErrorCode},
    pages::{
        MetadataWalkOutcome, WalkOutcome, base64_of, fetch_raw_blob, hex_of, resolve_abs,
        resolve_abs_metadata,
    },
    resolver::FsKind,
    view::validate_scope_relative_path,
};

/// One file resolved at a fixed path with verified content.
struct ResolvedFile {
    digest: [u8; 32],
    size: u64,
    raw: Vec<u8>,
}

struct ResolvedFileMetadata {
    fs_kind: FsKind,
    oid: String,
    digest: [u8; 32],
    size: u64,
}

#[allow(clippy::result_large_err)]
async fn fixed_root_tree<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    oid: &str,
) -> Result<git_internal::internal::object::tree::Tree, Response> {
    let tree = handler.get_tree_by_hash(oid).await.map_err(internal)?;
    if tree.id.to_string() != oid {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "fetched fixed root tree identity mismatch",
        )));
    }
    Ok(tree)
}

#[allow(clippy::result_large_err)]
async fn resolve_file_metadata<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    scope: &str,
    path: &str,
    expected_digest: Option<&str>,
) -> Result<ResolvedFileMetadata, Response> {
    validate_scope_relative_path(path).map_err(mst2_error_response)?;
    let (fs_kind, oid) = match resolve_abs_metadata(handler, root_tree, &abs_view_path(scope, path))
        .await
        .map_err(mst2_error_response)?
    {
        MetadataWalkOutcome::FoundFile { fs_kind, oid } => (fs_kind, oid),
        MetadataWalkOutcome::FoundDir => {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::NotDirectory,
                format!("{path} is a directory"),
            )));
        }
        MetadataWalkOutcome::Absent => {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::PathNotFound,
                format!("{path} absent in the fixed view"),
            )));
        }
        MetadataWalkOutcome::NotDirectory { symlink } => {
            return Err(mst2_error_response(SnapshotError::new(
                if symlink {
                    SnapshotErrorCode::SymlinkTraversal
                } else {
                    SnapshotErrorCode::NotDirectory
                },
                format!("{path}: intermediate component is not a directory"),
            )));
        }
    };
    let storage = handler.get_context();
    let mut verified = storage
        .mono_storage()
        .get_verified_blobs(vec![oid.clone()])
        .await
        .map_err(|error| {
            let code = if matches!(
                error,
                crate::common::errors::MegaError::ObjStorageInconsistent(_)
            ) {
                SnapshotErrorCode::IntegrityError
            } else {
                SnapshotErrorCode::Internal
            };
            tracing::warn!(error = %error, "fixed content metadata lookup failed");
            mst2_error_response(SnapshotError::new(
                code,
                "fixed content metadata lookup failed",
            ))
        })?;
    let fact = verified.remove(&oid).ok_or_else(|| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::MetadataNotReady,
            "fixed object has no current verified size and digest fact",
        ))
    })?;
    let digest: [u8; 32] = fact.raw_sha256.as_slice().try_into().map_err(|_| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "invalid verified content digest",
        ))
    })?;
    let size = u64::try_from(fact.size).map_err(|_| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "invalid verified content size",
        ))
    })?;
    if fs_kind == FsKind::Symlink && !(1..=4095).contains(&size) {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "verified symlink size is outside the fixed filesystem profile",
        )));
    }
    if let Some(expected) = expected_digest
        && expected != format!("sha256:{}", hex_of(&digest))
    {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::DigestMismatch,
            format!("{path}: content does not match expected_digest"),
        )));
    }
    Ok(ResolvedFileMetadata {
        fs_kind,
        oid,
        digest,
        size,
    })
}

pub(super) fn fs_kind_str(k: FsKind) -> &'static str {
    match k {
        FsKind::Regular => "regular",
        FsKind::Executable => "executable",
        FsKind::Symlink => "symlink",
        FsKind::Directory => "directory",
    }
}

/// Resolve a scope-relative path against the fixed tree and verify the
/// optional `expected_digest`. Absence/directory/intermediate outcomes stay
/// typed errors, never an empty body.
#[allow(clippy::result_large_err)]
async fn resolve_file<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    scope: &str,
    path: &str,
    expected_digest: Option<&str>,
) -> Result<ResolvedFile, Response> {
    let abs_path = abs_view_path(scope, path);
    match resolve_abs(handler, root_tree, &abs_path)
        .await
        .map_err(mst2_error_response)?
    {
        WalkOutcome::FoundFile {
            raw, size, digest, ..
        } => {
            if let Some(expected) = expected_digest
                && expected != format!("sha256:{}", hex_of(&digest))
            {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    format!("{path}: content does not match expected_digest"),
                )));
            }
            Ok(ResolvedFile { digest, size, raw })
        }
        WalkOutcome::FoundDir => Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::NotDirectory,
            format!("{path} is a directory"),
        ))),
        WalkOutcome::Absent => Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::PathNotFound,
            format!("{path} absent in the fixed view"),
        ))),
        WalkOutcome::NotDirectory { symlink } => Err(mst2_error_response(SnapshotError::new(
            if symlink {
                SnapshotErrorCode::SymlinkTraversal
            } else {
                SnapshotErrorCode::NotDirectory
            },
            format!("{path}: intermediate component is not a directory"),
        ))),
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct BlobQuery {
    pub(super) path: String,
    #[serde(default)]
    pub(super) expected_digest: Option<String>,
}

/// HEAD uses the verified size index; no body is produced (spec 04 §9).
#[allow(clippy::result_large_err)]
pub(super) async fn blob_head(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    if headers.contains_key("range") {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::RangeNotSupported,
            "raw blob has no Range semantics; use chunks for ranges",
        )));
    }
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = fixed_root_tree(handler.as_ref(), &ctx.root_tree_oid).await?;
    let f = resolve_file_metadata(
        handler.as_ref(),
        &root_tree,
        &ctx.built.descriptor.scope,
        &q.path,
        q.expected_digest.as_deref(),
    )
    .await?;
    axum::response::Response::builder()
        .header("content-length", f.size.to_string())
        .header("etag", format!("\"sha256:{}\"", hex_of(&f.digest)))
        .header("cache-control", "private, no-cache, no-transform")
        .header("vary", "Authorization, Accept")
        .header("x-mega-fs-kind", fs_kind_str(f.fs_kind))
        .header("x-mega-content-size", f.size.to_string())
        .body(axum::body::Body::empty())
        .map_err(|e| mst2_error_response(internal(format!("header build failed: {e}"))))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ObjectsRequest {
    items: Vec<ObjectItem>,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Clone, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ObjectItem {
    path: String,
    expected_digest: String,
}

const OBJECT_MAX_ITEMS: usize = 128;
const OBJECT_ITEM_MAX: u64 = 256 * 1024;
const OBJECT_TOTAL_MAX: usize = 8 * 1024 * 1024;

#[allow(clippy::result_large_err)]
pub(super) async fn objects(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    let req: ObjectsRequest = super::parse_json_body(&body)?;
    if req.items.is_empty() || req.items.len() > OBJECT_MAX_ITEMS {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "items must hold 1..128 entries",
        )));
    }
    let encoding = req
        .encoding
        .as_deref()
        .map(crate::ceres::snapshot::frame_stream::Encoding::parse)
        .transpose()
        .map_err(mst2_error_response)?
        .unwrap_or(crate::ceres::snapshot::frame_stream::Encoding::Identity);

    // Verify every member at its fixed path before any 200 is produced.
    // Members resolve concurrently: a batch is up to 128 files, and a
    // sequential S3 read per member dominated cold-mount time.
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = fixed_root_tree(handler.as_ref(), &ctx.root_tree_oid).await?;
    let scope = ctx.built.descriptor.scope.clone();
    // Copied references: each per-item future borrows the shared walk state
    // without moving it (the stream is an FnMut over owned items).
    let handler_ref = handler.as_ref();
    let root_ref = &root_tree;
    let scope_ref = &scope;
    let resolved: Vec<Result<_, Response>> = futures::stream::iter(req.items.clone())
        .map(move |item| async move {
            validate_scope_relative_path(&item.path).map_err(mst2_error_response)?;
            let f = resolve_file(
                handler_ref,
                root_ref,
                scope_ref,
                &item.path,
                Some(&item.expected_digest),
            )
            .await?;
            if f.size > OBJECT_ITEM_MAX {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::ScopeInvalid,
                    format!(
                        "{} is {} bytes; objects cap is 256KiB per item",
                        item.path, f.size
                    ),
                )));
            }
            Ok((item, f))
        })
        .buffered(16)
        .collect()
        .await;
    let mut unique: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut seen: Vec<[u8; 32]> = Vec::new();
    let mut logical_bytes = 0u64;
    for pair in resolved {
        let (_, f) = pair?;
        if !seen.contains(&f.digest) {
            if logical_bytes as usize + f.raw.len() > OBJECT_TOTAL_MAX {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::ScopeInvalid,
                    "unique object content exceeds the 8MiB batch cap",
                )));
            }
            logical_bytes += f.raw.len() as u64;
            seen.push(f.digest);
            unique.push((f.digest, f.raw));
        }
    }

    // Stream OBJECT frames (table + data ≤1 MiB, ≤128 objects per frame),
    // then exactly one END. Encoding is negotiated per request.
    use crate::ceres::snapshot::frame_stream::FrameStream;
    let mut stream = FrameStream::new(1, encoding);
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut frame: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut frame_raw = 0usize;
    for (cid, data) in unique {
        let next_payload = 4 + 40 * (frame.len() + 1) + frame_raw + data.len();
        if !frame.is_empty()
            && (frame.len() >= OBJECT_MAX_ITEMS
                || next_payload > mst2_codec::treeframe::OBJECT_MAX_RAW)
        {
            let bytes = stream
                .object(std::mem::take(&mut frame))
                .map_err(mst2_error_response)?;
            out.push(bytes);
            frame_raw = 0;
        }
        frame_raw += data.len();
        frame.push((cid, data));
    }
    if !frame.is_empty() {
        let bytes = stream.object(frame).map_err(mst2_error_response)?;
        out.push(bytes);
    }
    let end = stream.end(
        req.items.len() as u32,
        seen.len() as u32,
        logical_bytes,
        sha256_of(&body),
    );
    out.push(end);

    guarded_treeframe_response(&state, &ctx, &snapshot_id, &body, out).map_err(mst2_error_response)
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct ChunkMapQuery {
    pub(super) path: String,
    #[serde(default)]
    pub(super) expected_digest: Option<String>,
    #[serde(default)]
    pub(super) page: Option<String>,
    /// Canonical v3 page requests bind the page to the already verified map
    /// instead of repeating the file digest.
    #[serde(default)]
    pub(super) map_id: Option<String>,
    #[serde(default)]
    pub(super) page_index: Option<String>,
}

/// Project a fixed-path file into its range-readable representation.
#[allow(clippy::result_large_err)]
async fn project_for<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    scope: &str,
    path: &str,
    expected_digest: Option<&str>,
) -> Result<std::sync::Arc<ChunkProjection>, Response> {
    let f = resolve_file_metadata(handler, root_tree, scope, path, expected_digest).await?;
    // The first request for a digest builds the projection from the fixed
    // Git object; later requests slice the cached representation. A miss
    // rebuilds, never errors with "missing chunk".
    let digest = f.digest;
    let size = f.size;
    let projection = get_or_project(digest, || async move {
        let raw = fetch_raw_blob(handler, &f.oid).await?;
        if raw.len() as u64 != size {
            return Err(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "fixed blob length disagrees with its verified size fact",
            ));
        }
        Ok(raw)
    })
    .await
    .map_err(|error| {
        mst2_error_response(if error.code == SnapshotErrorCode::DigestMismatch {
            SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "fixed blob digest disagrees with its verified fact",
            )
        } else {
            error
        })
    })?;
    if projection.map.file_size != size || projection.map.file_content_id != digest {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "cached projection disagrees with the fixed verified fact",
        )));
    }
    Ok(projection)
}

#[allow(clippy::result_large_err)]
pub(super) async fn chunk_map(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<ChunkMapQuery>,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = fixed_root_tree(handler.as_ref(), &ctx.root_tree_oid).await?;
    let scope = ctx.built.descriptor.scope.clone();
    let proj = project_for(
        handler.as_ref(),
        &root_tree,
        &scope,
        &q.path,
        q.expected_digest.as_deref(),
    )
    .await?;
    // Canonical v3 uses a closed top-level envelope and a nested map
    // descriptor.  Keeping the map under `map` is part of profile selection;
    // the client rejects the legacy flat shape once canonical capabilities
    // have been advertised.
    let body = json!({
        "snapshot_id": snapshot_id,
        "path": q.path,
        "map": {
            "schema_version": 2,
            "file_content_id": format!("sha256:{}", hex_of(&proj.map.file_content_id)),
            "file_size": proj.map.file_size.to_string(),
            "chunk_size": mst2_codec::chunkmap::CHUNK_SIZE,
            "chunk_count": proj.map.chunk_count.to_string(),
            "page_count": proj.map.page_count.to_string(),
            "pages_root": format!("sha256:{}", hex_of(&proj.map.pages_root)),
            "map_id": format!("sha256:{}", hex_of(&proj.map_id)),
        },
    });
    Ok(Json(body).into_response())
}

#[allow(clippy::result_large_err)]
pub(super) async fn chunk_map_pages(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<ChunkMapQuery>,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    // Canonical v3 uses `map_id` + `page_index`; retain parsing of the old
    // names only while the legacy client is still present in this checkout.
    let page_index: u64 = match (q.page_index.as_deref(), q.page.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "page_index and page are mutually exclusive",
            )));
        }
        (Some(s), None) => parse_decimal_count(s, "page_index").map_err(mst2_error_response)?,
        (None, Some(s)) => parse_decimal_count(s, "page").map_err(mst2_error_response)?,
        (None, None) => 0,
    };
    if q.page_index.is_some() != q.map_id.is_some() {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "canonical page requests require map_id and page_index together",
        )));
    }
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = fixed_root_tree(handler.as_ref(), &ctx.root_tree_oid).await?;
    let scope = ctx.built.descriptor.scope.clone();
    let proj = project_for(
        handler.as_ref(),
        &root_tree,
        &scope,
        &q.path,
        q.expected_digest.as_deref(),
    )
    .await?;
    if let Some(expected_map) = q.map_id.as_deref() {
        let actual_map = format!("sha256:{}", hex_of(&proj.map_id));
        if expected_map != actual_map {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::DigestMismatch,
                "chunk-map page map_id does not bind to the fixed file",
            )));
        }
    }
    let (leaf, proof) = proj
        .leaf_and_proof(page_index)
        .map_err(mst2_error_response)?;
    let leaf_bytes = leaf
        .encode()
        .map_err(|e| mst2_error_response(internal(format!("chunk leaf encode failed: {e}"))))?;
    let proof_json: Vec<serde_json::Value> = proof
        .iter()
        .map(|s| {
            json!({
                "side": match s.side {
                    mst2_codec::chunkmap::ProofSide::Left => "left",
                    mst2_codec::chunkmap::ProofSide::Right => "right",
                },
                "sibling_pages": s.sibling_pages.to_string(),
                "digest": format!("sha256:{}", hex_of(&s.digest)),
            })
        })
        .collect();
    // Canonical v3 page responses are closed and carry the encoded leaf
    // directly.  The map descriptor already authenticated page_count and the
    // client verifies this page_index against that fixed descriptor.
    let body = json!({
        "map_id": format!("sha256:{}", hex_of(&proj.map_id)),
        "page_index": leaf.page_index.to_string(),
        "leaf_base64": base64_of(&leaf_bytes),
        "proof": proof_json,
    });
    Ok(Json(body).into_response())
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ChunksRequest {
    items: Vec<ChunkItem>,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ChunkItem {
    path: String,
    expected_digest: String,
    map_id: String,
    chunk_index: String,
}

const CHUNKS_MAX_ITEMS: usize = 128;
const CHUNKS_TOTAL_MAX: u64 = 128 * 1024 * 1024;

struct Planned {
    projection: std::sync::Arc<ChunkProjection>,
    index: u64,
}

#[allow(clippy::result_large_err)]
pub(super) async fn chunks(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Mst2Bytes(body): Mst2Bytes,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    let req: ChunksRequest = super::parse_json_body(&body)?;
    if req.items.is_empty() || req.items.len() > CHUNKS_MAX_ITEMS {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "items must hold 1..128 entries",
        )));
    }
    let encoding = req
        .encoding
        .as_deref()
        .map(crate::ceres::snapshot::frame_stream::Encoding::parse)
        .transpose()
        .map_err(mst2_error_response)?
        .unwrap_or(crate::ceres::snapshot::frame_stream::Encoding::Identity);

    // Verify every member first; the 200 stream starts only after all paths,
    // bindings, indices and batch caps check out (spec 04 §9).
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = fixed_root_tree(handler.as_ref(), &ctx.root_tree_oid).await?;
    let scope = ctx.built.descriptor.scope.clone();
    let mut planned: Vec<Planned> = Vec::new();
    let mut units: Vec<(String, u64)> = Vec::new();
    let mut logical_bytes = 0u64;
    for item in &req.items {
        validate_scope_relative_path(&item.path).map_err(mst2_error_response)?;
        let index =
            parse_decimal_count(&item.chunk_index, "chunk_index").map_err(mst2_error_response)?;
        let proj = project_for(
            handler.as_ref(),
            &root_tree,
            &scope,
            &item.path,
            Some(&item.expected_digest),
        )
        .await?;
        let want_map = format!("sha256:{}", hex_of(&proj.map_id));
        if want_map != item.map_id {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::DigestMismatch,
                format!("{}: map_id does not bind to this file", item.path),
            )));
        }
        if index >= proj.map.chunk_count {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                format!(
                    "{}: chunk_index {index} >= chunk_count {}",
                    item.path, proj.map.chunk_count
                ),
            )));
        }
        let unit = (item.map_id.clone(), index);
        if units.contains(&unit) {
            // Duplicate units are rejected, never deduped silently (spec 06).
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                format!("duplicate chunk unit map_id={} index={index}", item.map_id),
            )));
        }
        let len = proj
            .map
            .chunk_len(index)
            .map_err(|e| mst2_error_response(internal(e.to_string())))?;
        if logical_bytes + len > CHUNKS_TOTAL_MAX {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "chunk batch exceeds the 128MiB cap",
            )));
        }
        logical_bytes += len;
        units.push(unit);
        planned.push(Planned {
            projection: proj,
            index,
        });
    }

    use crate::ceres::snapshot::frame_stream::FrameStream;
    let mut stream = FrameStream::new(1, encoding);
    let mut out: Vec<Vec<u8>> = Vec::new();
    for p in planned.iter() {
        // Re-verified slice (digest + length) from the staged projection.
        let bytes = p
            .projection
            .chunk_bytes(p.index)
            .map_err(mst2_error_response)?;
        let frame = stream
            .chunk(
                p.projection.map_id,
                p.projection.map.file_content_id,
                p.index,
                bytes.to_vec(),
            )
            .map_err(mst2_error_response)?;
        out.push(frame);
    }
    let end = stream.end(
        req.items.len() as u32,
        planned.len() as u32,
        logical_bytes,
        sha256_of(&body),
    );
    out.push(end);

    guarded_treeframe_response(&state, &ctx, &snapshot_id, &body, out).map_err(mst2_error_response)
}

/// Strict decimal-string parse for unsigned counts (spec 04 §1: no leading
/// zeros, no signs, no floats).
fn parse_decimal_count(s: &str, field: &str) -> Result<u64, SnapshotError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            format!("{field} must be a decimal string without leading zeros"),
        ));
    }
    s.parse::<u64>().map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            format!("{field} out of range"),
        )
    })
}

fn sha256_of(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// The axum error type (`Response<Body>`) is inherently large; every
/// handler in this router returns it, so the size lint does not carry its
/// usual signal here.
#[allow(clippy::result_large_err)]
fn ensure(state: &crate::api::MonoApiServiceState) -> Result<(), Response> {
    if state.storage.config().mst2.enabled {
        Ok(())
    } else {
        Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::SnapshotNotReady,
            "mst2 surface is disabled on this deployment",
        )))
    }
}

#[cfg(test)]
#[path = "snapshot_content_tests.rs"]
mod tests;
