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
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;

use crate::ceres::snapshot::{
    chunks::{ChunkProjection, get_or_project},
    error::{SnapshotError, SnapshotErrorCode},
    pages::{WalkOutcome, base64_of, hex_of, resolve_abs},
    resolver::FsKind,
    runtime::runtime,
    view::validate_scope_relative_path,
};

use super::{abs_view_path, internal, mst2_error_response};

/// One file resolved at a fixed path with verified content.
struct ResolvedFile {
    fs_kind: FsKind,
    digest: [u8; 32],
    size: u64,
    raw: Vec<u8>,
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
            fs_kind,
            raw,
            size,
            digest,
            ..
        } => {
            if let Some(expected) = expected_digest
                && expected != format!("sha256:{}", hex_of(&digest))
            {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    format!("{path}: content does not match expected_digest"),
                )));
            }
            Ok(ResolvedFile {
                fs_kind,
                digest,
                size,
                raw,
            })
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
pub(super) async fn blob_head(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
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
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;
    let f = resolve_file(
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
        .header("x-mega-fs-kind", fs_kind_str(f.fs_kind))
        .header("x-mega-content-size", f.size.to_string())
        .body(axum::body::Body::empty())
        .map_err(|e| mst2_error_response(internal(format!("header build failed: {e}"))))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ObjectsRequest {
    items: Vec<ObjectItem>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ObjectItem {
    path: String,
    expected_digest: String,
}

const OBJECT_MAX_ITEMS: usize = 128;
const OBJECT_ITEM_MAX: u64 = 256 * 1024;
const OBJECT_TOTAL_MAX: usize = 8 * 1024 * 1024;

pub(super) async fn objects(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    let req: ObjectsRequest = serde_json::from_slice(&body).map_err(|e| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            format!("malformed request body: {e}"),
        ))
    })?;
    if req.items.is_empty() || req.items.len() > OBJECT_MAX_ITEMS {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "items must hold 1..128 entries",
        )));
    }

    // Verify every member at its fixed path before any 200 is produced.
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;
    let scope = ctx.built.descriptor.scope.clone();
    let mut unique: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut seen: Vec<[u8; 32]> = Vec::new();
    let mut logical_bytes = 0u64;
    for item in &req.items {
        validate_scope_relative_path(&item.path).map_err(mst2_error_response)?;
        let f = resolve_file(
            handler.as_ref(),
            &root_tree,
            &scope,
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
    // then exactly one END.
    const STREAM_ID: u32 = 1;
    let mut out: Vec<u8> = Vec::new();
    let mut sequence = 0u64;
    let mut frame: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut frame_raw = 0usize;
    for (cid, data) in unique {
        let next_payload = 4 + 40 * (frame.len() + 1) + frame_raw + data.len();
        if !frame.is_empty()
            && (frame.len() >= OBJECT_MAX_ITEMS
                || next_payload > mst2_codec::treeframe::OBJECT_MAX_RAW)
        {
            let payload = mst2_codec::treeframe::ObjectPayload {
                objects: std::mem::take(&mut frame),
            }
            .encode(STREAM_ID, sequence)
            .map_err(|e| mst2_error_response(internal(format!("OBJECT frame: {e}"))))?;
            out.extend_from_slice(&payload);
            sequence += 1;
            frame_raw = 0;
        }
        frame_raw += data.len();
        frame.push((cid, data));
    }
    if !frame.is_empty() {
        let payload = mst2_codec::treeframe::ObjectPayload { objects: frame }
            .encode(STREAM_ID, sequence)
            .map_err(|e| mst2_error_response(internal(format!("OBJECT frame: {e}"))))?;
        out.extend_from_slice(&payload);
        sequence += 1;
    }

    let end = mst2_codec::treeframe::EndPayload {
        request_item_count: req.items.len() as u32,
        unique_unit_count: seen.len() as u32,
        logical_bytes,
        request_body_sha256: sha256_of(&body),
    }
    .encode(STREAM_ID, sequence);
    out.extend_from_slice(&end);

    axum::response::Response::builder()
        .header("content-type", "application/octet-stream")
        .header("cache-control", "private, no-cache, no-transform")
        .body(axum::body::Body::from(Bytes::from(out)))
        .map_err(|e| mst2_error_response(internal(format!("body build failed: {e}"))))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct ChunkMapQuery {
    pub(super) path: String,
    #[serde(default)]
    pub(super) expected_digest: Option<String>,
    #[serde(default)]
    pub(super) page: Option<String>,
}

/// Project a fixed-path file into its range-readable representation.
async fn project_for<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    scope: &str,
    path: &str,
    expected_digest: Option<&str>,
) -> Result<std::sync::Arc<ChunkProjection>, Response> {
    let f = resolve_file(handler, root_tree, scope, path, expected_digest).await?;
    // The first request for a digest builds the projection from the fixed
    // Git object; later requests slice the cached representation. A miss
    // rebuilds, never errors with "missing chunk".
    let digest = f.digest;
    get_or_project(digest, || async move {
        let raw = f.raw;
        if raw.len() as u64 != f.size {
            return Err(SnapshotError::new(
                SnapshotErrorCode::Internal,
                "resolved blob size disagrees with its length",
            ));
        }
        Ok(raw)
    })
    .await
    .map_err(mst2_error_response)
}

pub(super) async fn chunk_map(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<ChunkMapQuery>,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;
    let scope = ctx.built.descriptor.scope.clone();
    let proj = project_for(
        handler.as_ref(),
        &root_tree,
        &scope,
        &q.path,
        q.expected_digest.as_deref(),
    )
    .await?;
    let body = json!({
        "snapshot_id": snapshot_id,
        "path": q.path,
        "schema_version": 2,
        "file_content_id": format!("sha256:{}", hex_of(&proj.map.file_content_id)),
        "file_size": proj.map.file_size.to_string(),
        "chunk_size": mst2_codec::chunkmap::CHUNK_SIZE,
        "chunk_count": proj.map.chunk_count.to_string(),
        "page_count": proj.map.page_count.to_string(),
        "pages_root": format!("sha256:{}", hex_of(&proj.map.pages_root)),
        "map_id": format!("sha256:{}", hex_of(&proj.map_id)),
    });
    Ok(Json(body).into_response())
}

pub(super) async fn chunk_map_pages(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<ChunkMapQuery>,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    let page_index: u64 = match q.page.as_deref() {
        Some(s) => parse_decimal_count(s, "page").map_err(mst2_error_response)?,
        None => 0,
    };
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;
    let scope = ctx.built.descriptor.scope.clone();
    let proj = project_for(
        handler.as_ref(),
        &root_tree,
        &scope,
        &q.path,
        q.expected_digest.as_deref(),
    )
    .await?;
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
    let body = json!({
        "snapshot_id": snapshot_id,
        "path": q.path,
        "map_id": format!("sha256:{}", hex_of(&proj.map_id)),
        "page_count": proj.map.page_count.to_string(),
        "leaf": {
            "page_index": leaf.page_index.to_string(),
            "count": leaf.chunk_sha256.len().to_string(),
            "data_base64": base64_of(&leaf_bytes),
        },
        "proof": proof_json,
    });
    Ok(Json(body).into_response())
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ChunksRequest {
    items: Vec<ChunkItem>,
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

pub(super) async fn chunks(
    state: State<crate::api::MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    body: Bytes,
) -> Result<Response, Response> {
    ensure(&state)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    let req: ChunksRequest = serde_json::from_slice(&body).map_err(|e| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            format!("malformed request body: {e}"),
        ))
    })?;
    if req.items.is_empty() || req.items.len() > CHUNKS_MAX_ITEMS {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "items must hold 1..128 entries",
        )));
    }

    // Verify every member first; the 200 stream starts only after all paths,
    // bindings, indices and batch caps check out (spec 04 §9).
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let root_tree = handler
        .get_tree_by_hash(&ctx.root_tree_oid)
        .await
        .map_err(internal)?;
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

    const STREAM_ID: u32 = 1;
    let mut out: Vec<u8> = Vec::new();
    for (sequence, p) in planned.iter().enumerate() {
        // Re-verified slice (digest + length) from the staged projection.
        let bytes = p
            .projection
            .chunk_bytes(p.index)
            .map_err(mst2_error_response)?;
        let frame = mst2_codec::treeframe::ChunkPayload {
            map_id: p.projection.map_id,
            file_content_id: p.projection.map.file_content_id,
            chunk_index: p.index,
            chunk_bytes: bytes.to_vec(),
        }
        .encode(STREAM_ID, sequence as u64)
        .map_err(|e| mst2_error_response(internal(format!("CHUNK frame: {e}"))))?;
        out.extend_from_slice(&frame);
    }
    let end = mst2_codec::treeframe::EndPayload {
        request_item_count: req.items.len() as u32,
        unique_unit_count: planned.len() as u32,
        logical_bytes,
        request_body_sha256: sha256_of(&body),
    }
    .encode(STREAM_ID, planned.len() as u64);
    out.extend_from_slice(&end);

    axum::response::Response::builder()
        .header("content-type", "application/octet-stream")
        .header("cache-control", "private, no-cache, no-transform")
        .body(axum::body::Body::from(Bytes::from(out)))
        .map_err(|e| mst2_error_response(internal(format!("body build failed: {e}"))))
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
