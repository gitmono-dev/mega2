//! Content transport handlers (spec 04 §9, spec 06, spec 07):
//! HEAD blob, batched OBJECT frames, chunk-map descriptors/pages and CHUNK
//! frames. Identity encoding only in this slice; zstd negotiation is a later
//! WP and `frame_encodings` advertises identity alone.

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
};
use futures::stream::StreamExt;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    abs_view_path, guarded_treeframe_response, guarded_treeframe_response_with_budget, internal,
    mst2_error_response, request::Mst2Bytes,
};
use crate::ceres::snapshot::{
    chunks::{ChunkMapSource, VerifiedSourceChunkMap, map_build_reservation_bytes},
    content_budget::{BudgetedFrame, MemoryLease, reserve_range_work, reserve_response},
    error::{SnapshotError, SnapshotErrorCode},
    pages::{MetadataWalkOutcome, base64_of, hex_of, resolve_abs_metadata},
    resolver::FsKind,
    view::validate_scope_relative_path,
};

pub(super) struct ResolvedFileMetadata {
    fs_kind: FsKind,
    oid: String,
    pub(super) digest: [u8; 32],
    pub(super) size: u64,
    fact: crate::callisto::mst2_verified_object::Model,
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
    verified_file_metadata(handler, fs_kind, oid, path, expected_digest).await
}

#[allow(clippy::result_large_err)]
pub(super) async fn verified_file_metadata<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    fs_kind: FsKind,
    oid: String,
    path: &str,
    expected_digest: Option<&str>,
) -> Result<ResolvedFileMetadata, Response> {
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
        fact,
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

#[allow(clippy::result_large_err)]
async fn read_object<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    file: &ResolvedFileMetadata,
    path: &str,
) -> Result<Vec<u8>, Response> {
    if file.size > OBJECT_ITEM_MAX {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "object exceeds the 256KiB item cap",
        )));
    }
    let expected_size = file.size as usize;
    let mut input = handler
        .get_raw_blob_stream_by_hash(&file.oid)
        .await
        .map_err(|error| {
            let code = match error {
                crate::common::errors::MegaError::ObjStorageNotFound(_) => {
                    SnapshotErrorCode::ObjectUnavailable
                }
                crate::common::errors::MegaError::ObjStorageInconsistent(_) => {
                    SnapshotErrorCode::IntegrityError
                }
                _ => SnapshotErrorCode::Internal,
            };
            tracing::warn!(error = %error, "fixed-view blob fetch failed");
            mst2_error_response(SnapshotError::new(
                code,
                "fixed-view content could not be read",
            ))
        })?;
    let mut raw = Vec::with_capacity(expected_size);
    let mut hash = Sha256::new();
    while let Some(part) = input.next().await {
        let bytes = part.map_err(|error| {
            tracing::warn!(error = %error, "fixed-view blob stream failed");
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::Internal,
                "fixed-view content could not be read",
            ))
        })?;
        // Reject oversized producer chunks before copying or hashing them.
        if bytes.len() > expected_size - raw.len() {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "fixed blob length disagrees with its verified size fact",
            )));
        }
        hash.update(&bytes);
        raw.extend_from_slice(&bytes);
    }
    if raw.len() != expected_size {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "fixed blob length disagrees with its verified size fact",
        )));
    }
    let digest: [u8; 32] = hash.finalize().into();
    if digest != file.digest {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::DigestMismatch,
            format!("{path}: content does not match expected_digest"),
        )));
    }
    Ok(raw)
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

    // Admit the whole fixed-path batch before opening any object body.
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
            let f = resolve_file_metadata(
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
    let mut sources: Vec<(ObjectItem, ResolvedFileMetadata)> = Vec::new();
    let mut content_sizes: Vec<([u8; 32], u64)> = Vec::new();
    let mut planned_bytes = 0u64;
    for pair in resolved {
        let (item, f) = pair?;
        if let Some((_, size)) = content_sizes.iter().find(|(digest, _)| *digest == f.digest) {
            if *size != f.size {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::IntegrityError,
                    "fixed content digest has conflicting verified sizes",
                )));
            }
        } else {
            if planned_bytes + f.size > OBJECT_TOTAL_MAX as u64 {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::ScopeInvalid,
                    "unique object content exceeds the 8MiB batch cap",
                )));
            }
            planned_bytes += f.size;
            content_sizes.push((f.digest, f.size));
        }
        if let Some((_, source)) = sources.iter().find(|(_, source)| source.oid == f.oid) {
            if source.digest != f.digest || source.size != f.size {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::IntegrityError,
                    "fixed object has conflicting verified facts",
                )));
            }
        } else {
            sources.push((item, f));
        }
    }
    let loaded = futures::stream::iter(sources)
        .map(|(item, file)| async move {
            let raw = read_object(handler_ref, &file, &item.path).await?;
            Ok::<_, Response>((file.digest, raw))
        })
        .buffered(16);
    tokio::pin!(loaded);
    let mut unique: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut seen: Vec<[u8; 32]> = Vec::new();
    let mut logical_bytes = 0u64;
    while let Some(pair) = loaded.next().await {
        let (digest, raw) = pair?;
        if !seen.contains(&digest) {
            logical_bytes += raw.len() as u64;
            seen.push(digest);
            unique.push((digest, raw));
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
) -> Result<std::sync::Arc<crate::jupiter::storage::native_chunk_map::PersistedChunkMap>, Response>
{
    let f = resolve_file_metadata(handler, root_tree, scope, path, expected_digest).await?;
    project_resolved(handler, &f).await
}

#[allow(clippy::result_large_err)]
async fn project_resolved<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    f: &ResolvedFileMetadata,
) -> Result<std::sync::Arc<crate::jupiter::storage::native_chunk_map::PersistedChunkMap>, Response>
{
    if f.size == 0 {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "empty files have no chunk map",
        )));
    }
    let source = ChunkMapSource::from_fact(f.fact.clone(), &f.oid).map_err(mst2_error_response)?;
    let storage = handler.get_context();
    let repository = storage.chunk_maps().await.map_err(mst2_error_response)?;
    let objects = &storage.git_service.obj_storage;
    if let Some(map) = repository
        .read(&source, objects)
        .await
        .map_err(mst2_error_response)?
    {
        return Ok(map);
    }
    let flight = crate::ceres::snapshot::chunk_map_gate::InstallFlight::acquire(
        repository
            .source_identity(&source)
            .map_err(mst2_error_response)?,
    )
    .map_err(mst2_error_response)?;
    let _gate = flight.lock().await.map_err(mst2_error_response)?;
    if let Some(map) = repository
        .read(&source, objects)
        .await
        .map_err(mst2_error_response)?
    {
        return Ok(map);
    }
    let verified =
        VerifiedSourceChunkMap::verify(handler, source.clone(), repository.memory_budget())
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
    repository
        .install(verified, objects)
        .await
        .map_err(mst2_error_response)?;
    repository
        .read(&source, objects)
        .await
        .map_err(mst2_error_response)?
        .ok_or_else(|| mst2_error_response(internal("installed chunk map source is missing")))
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
    let wire_bound = q
        .path
        .len()
        .checked_mul(6)
        .and_then(|n| n.checked_add(4096))
        .ok_or_else(|| mst2_error_response(internal("chunk map response bound overflow")))?;
    let memory = reserve_response(
        wire_bound
            .checked_mul(2)
            .ok_or_else(|| mst2_error_response(internal("chunk map response credit overflow")))?,
    )
    .map_err(mst2_error_response)?;
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
    guarded_map_json_response(&state, &ctx, &body, memory)
        .await
        .map_err(mst2_error_response)
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
    let page_index = q
        .page_index
        .as_deref()
        .map(|s| parse_decimal_count(s, "page_index"))
        .transpose()
        .map_err(mst2_error_response)?
        .unwrap_or(0);
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
    let storage = handler.get_context();
    let page = storage
        .chunk_maps()
        .await
        .map_err(mst2_error_response)?
        .selected_page(&proj, page_index)
        .await
        .map_err(mst2_error_response)?;
    // Reserve both JSON values and the encoded wire buffer before either
    // allocation. The authenticated leaf/proof retain their own lease.
    let memory = reserve_response(64 * 1024).map_err(mst2_error_response)?;
    let leaf = &page.leaf;
    let proof = &page.proof;
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
    guarded_map_json_response(&state, &ctx, &body, memory)
        .await
        .map_err(mst2_error_response)
}

fn map_json_bytes(
    value: &serde_json::Value,
    memory: MemoryLease,
) -> Result<bytes::Bytes, SnapshotError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(memory.bytes / 2)
        .map_err(|_| internal("chunk map JSON allocation failed"))?;
    let limit = memory.bytes / 2;
    let mut writer = BoundedMapJsonWriter {
        bytes: &mut bytes,
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| internal("chunk map JSON encoding exceeds its owned credit"))?;
    if bytes.capacity() > memory.bytes {
        return Err(internal(
            "chunk map JSON allocation exceeds its owned credit",
        ));
    }
    Ok(bytes::Bytes::from_owner(BudgetedFrame {
        bytes,
        lease: std::sync::Arc::new(memory),
    }))
}

struct BoundedMapJsonWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedMapJsonWriter<'_> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other(
                "chunk map JSON exceeds its wire bound",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn guarded_map_json_response(
    state: &crate::api::MonoApiServiceState,
    context: &crate::ceres::snapshot::runtime::SnapshotContext,
    value: &serde_json::Value,
    memory: MemoryLease,
) -> Result<Response, SnapshotError> {
    let bytes = map_json_bytes(value, memory)?;
    let headers = super::REQUEST_HEADERS.try_with(Clone::clone).map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::Unauthenticated,
            "request authentication context missing",
        )
    })?;
    super::revalidate_access(state, context, &headers).await?;
    let state = state.clone();
    let context = context.clone();
    let stream = futures::stream::once(async move {
        super::revalidate_access(&state, &context, &headers).await?;
        Ok::<_, SnapshotError>(bytes)
    });
    Response::builder()
        .header("content-type", "application/json")
        .header("cache-control", "private, no-cache, no-transform")
        .header("vary", "Authorization, Accept")
        .body(axum::body::Body::from_stream(stream))
        .map_err(|_| internal("chunk map JSON response build failed"))
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
    projection: std::sync::Arc<crate::jupiter::storage::native_chunk_map::PersistedChunkMap>,
    page: std::sync::Arc<crate::jupiter::storage::native_chunk_map::AuthenticatedChunkPage>,
    oid: String,
    index: u64,
}

struct ResolvedChunk {
    file: ResolvedFileMetadata,
    index: u64,
    map_id: String,
}

fn content_read_error(error: crate::common::errors::MegaError) -> SnapshotError {
    use crate::common::errors::MegaError;
    let code = match &error {
        MegaError::ObjStorageNotFound(_) => SnapshotErrorCode::ObjectUnavailable,
        MegaError::ObjStorageInconsistent(_) => SnapshotErrorCode::IntegrityError,
        MegaError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            SnapshotErrorCode::IntegrityError
        }
        _ => SnapshotErrorCode::Internal,
    };
    tracing::warn!(error = %error, "fixed-view content read failed");
    SnapshotError::new(code, "fixed-view content could not be read")
}

#[allow(clippy::result_large_err)]
async fn read_chunk_range<T: crate::ceres::api_service::ApiHandler + ?Sized>(
    handler: &T,
    planned: &Planned,
) -> Result<Vec<u8>, Response> {
    let projection = &planned.projection;
    let len = projection.map.chunk_len(planned.index).map_err(|error| {
        mst2_error_response(internal(format!("invalid admitted chunk: {error}")))
    })?;
    let start = planned
        .index
        .checked_mul(mst2_codec::chunkmap::CHUNK_SIZE as u64)
        .ok_or_else(|| mst2_error_response(internal("chunk offset overflow")))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| mst2_error_response(internal("chunk end overflow")))?;
    let (mut input, meta) = handler
        .get_raw_blob_range_stream_exact(&planned.oid, start, end)
        .await
        .map_err(|error| mst2_error_response(content_read_error(error)))?
        .ok_or_else(|| {
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::RangeNotSupported,
                "fixed source does not support exact raw ranges",
            ))
        })?;
    if u64::try_from(meta.size).ok() != Some(projection.map.file_size) {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::IntegrityError,
            "range source size disagrees with the fixed verified fact",
        )));
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(len as usize).map_err(|_| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "range allocation could not be admitted",
        ))
    })?;
    while let Some(part) = input.next().await {
        let bytes = part.map_err(|error| {
            tracing::warn!(error = %error, "fixed-view range stream failed");
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "fixed-view range stream failed",
            ))
        })?;
        if bytes.len() > len as usize - raw.len() {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "range response exceeds its exact requested length",
            )));
        }
        raw.extend_from_slice(&bytes);
    }
    planned
        .page
        .verify_chunk(&projection.map, planned.index, &raw)
        .map_err(mst2_error_response)?;
    Ok(raw)
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
    let mut resolved: Vec<ResolvedChunk> = Vec::new();
    let mut units: Vec<(String, u64)> = Vec::new();
    let mut distinct: std::collections::HashMap<[u8; 32], u64> = std::collections::HashMap::new();
    let mut logical_bytes = 0u64;
    for item in &req.items {
        validate_scope_relative_path(&item.path).map_err(mst2_error_response)?;
        let index =
            parse_decimal_count(&item.chunk_index, "chunk_index").map_err(mst2_error_response)?;
        let file = resolve_file_metadata(
            handler.as_ref(),
            &root_tree,
            &scope,
            &item.path,
            Some(&item.expected_digest),
        )
        .await?;
        let chunk_count = file.size.div_ceil(mst2_codec::chunkmap::CHUNK_SIZE as u64);
        if index >= chunk_count {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                format!(
                    "{}: chunk_index {index} >= chunk_count {}",
                    item.path, chunk_count
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
        let start = index * mst2_codec::chunkmap::CHUNK_SIZE as u64;
        let len = (file.size - start).min(mst2_codec::chunkmap::CHUNK_SIZE as u64);
        if logical_bytes + len > CHUNKS_TOTAL_MAX {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "chunk batch exceeds the 128MiB cap",
            )));
        }
        logical_bytes += len;
        units.push(unit);
        match distinct.entry(file.digest) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                if *entry.get() != file.size {
                    return Err(mst2_error_response(SnapshotError::new(
                        SnapshotErrorCode::IntegrityError,
                        "fixed facts disagree for the same content digest",
                    )));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                // Check the profile before any body I/O. Cold construction
                // owns its credits and is dropped after durable installation;
                // warm requests retain only descriptors and selected pages.
                map_build_reservation_bytes(file.size).map_err(mst2_error_response)?;
                entry.insert(file.size);
            }
        }
        resolved.push(ResolvedChunk {
            file,
            index,
            map_id: item.map_id.clone(),
        });
    }
    let response_bytes = usize::try_from(logical_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(req.items.len() * 1024 + 1024))
        .ok_or_else(|| mst2_error_response(internal("chunk response memory overflow")))?;
    let response_memory = reserve_response(response_bytes).map_err(mst2_error_response)?;
    let mut maps = std::collections::HashMap::new();
    let mut pages = std::collections::HashMap::new();
    for item in resolved {
        let source = ChunkMapSource::from_fact(item.file.fact.clone(), &item.file.oid)
            .map_err(mst2_error_response)?;
        let source_key = source.canonical_bytes().map_err(mst2_error_response)?;
        let proj = if let Some(map) = maps.get(&source_key) {
            std::sync::Arc::clone(map)
        } else {
            let map = project_resolved(handler.as_ref(), &item.file).await?;
            maps.insert(source_key, std::sync::Arc::clone(&map));
            map
        };
        let want_map = format!("sha256:{}", hex_of(&proj.map_id));
        if want_map != item.map_id {
            return Err(mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::DigestMismatch,
                "map_id does not bind to the fixed file",
            )));
        }
        let page_key = (
            proj.source_id(),
            item.index / mst2_codec::chunkmap::CHUNKS_PER_PAGE as u64,
        );
        let page = if let Some(page) = pages.get(&page_key) {
            std::sync::Arc::clone(page)
        } else {
            let storage = handler.get_context();
            let page = storage
                .chunk_maps()
                .await
                .map_err(mst2_error_response)?
                .selected_page(&proj, page_key.1)
                .await
                .map_err(mst2_error_response)?;
            pages.insert(page_key, std::sync::Arc::clone(&page));
            page
        };
        planned.push(Planned {
            projection: proj,
            page,
            oid: item.file.oid,
            index: item.index,
        });
    }

    use crate::ceres::snapshot::frame_stream::FrameStream;
    let mut stream = FrameStream::new(1, encoding);
    let mut out: Vec<Vec<u8>> = Vec::new();
    for p in planned.iter() {
        let _work_memory = reserve_range_work().map_err(mst2_error_response)?;
        // The source is the current request's fixed OID, never a cached
        // handler/backend/credential from a different scope.
        let bytes = read_chunk_range(handler.as_ref(), p).await?;
        let frame = stream
            .chunk(
                p.projection.map_id,
                p.projection.map.file_content_id,
                p.index,
                bytes,
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

    guarded_treeframe_response_with_budget(
        &state,
        &ctx,
        &snapshot_id,
        &body,
        out,
        Some(response_memory),
    )
    .map_err(mst2_error_response)
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
