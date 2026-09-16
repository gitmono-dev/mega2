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
    routing::{get, post},
};
use base64::Engine;
use bytes::Bytes;
use mst2_codec::descriptor;
use serde::Deserialize;
use serde_json::json;

use crate::{
    api::MonoApiServiceState,
    ceres::snapshot::{
        descriptor::build as build_descriptor,
        error::{SnapshotError, SnapshotErrorCode},
        pages::{WalkOutcome, base64_of, build_directory_page, hex_of, proof_pages, resolve_abs},
        runtime::{now_unix, runtime},
        view::{SnapshotView, validate_scope_relative_path},
    },
};

pub fn routers() -> Router<MonoApiServiceState> {
    Router::new()
        .route("/snapshots/capabilities", get(capabilities))
        .route("/snapshots/resolve", post(resolve))
        .route("/snapshots/{snapshot_id}/directory", get(directory))
        .route("/snapshots/{snapshot_id}/blob", get(blob))
        .route("/snapshots/{snapshot_id}/lookup", post(lookup))
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
                "request_id": "",
                "retryable": false,
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

/// MegaError → snapshot error: storage/lease failures must surface as errors,
/// never as absence (spec 00 SYS-04).
fn internal<E: std::fmt::Display>(e: E) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, e.to_string())
}

async fn capabilities() -> Json<serde_json::Value> {
    // Honest capability set for this build (spec 04 §3): only what this slice
    // serves is true; everything else stays false until accepted.
    Json(json!({
        "protocol_versions": [2],
        "metadata_codecs": [1],
        "frame_encodings": ["identity"],
        "features": {
            "resolve": true,
            "directory": true,
            "leases": true,
            "lookup": true,
            "metadata_pages": false,
            "raw_blob": true,
            "objects": false,
            "chunk_reads": false,
            "full_hydration": false,
            "offline_export": false,
            "bindings": false,
            "immutable_release": false,
        },
        "limits": {
            "metadata_page_bytes": 16384,
            "metadata_leaf_entries": 128,
            "max_directory_page_limit": 256,
            "chunk_size": 1048576,
            "small_object_bytes": 262144
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

async fn resolve(
    state: State<MonoApiServiceState>,
    Json(req): Json<ResolveRequest>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
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

    // Fix the view on exactly one commit read; nothing below may re-read refs.
    let main = state
        .storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            mst2_error_response(SnapshotError::new(
                SnapshotErrorCode::SnapshotNotReady,
                "monorepo main ref missing; run service init",
            ))
        })?;
    let commit_oid = main.ref_commit_hash.clone();
    let tree_oid = main.ref_tree_hash.clone();
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
    let scope_page = build_directory_page(handler.as_ref(), &root_tree, &req.scope)
        .await
        .map_err(mst2_error_response)?;

    let built = build_descriptor(
        &state.storage.config().mst2,
        &view,
        &req.scope,
        scope_page.page_id,
    )
    .map_err(mst2_error_response)?;
    let ctx = runtime().insert_context(built.clone(), &commit_oid, &tree_oid, req.lease_seconds);
    let seq = runtime().publication_sequence(&commit_oid);

    let body = json!({
        "descriptor": {
            "schema_version": 2,
            "metadata_codec": 1,
            "instance_id": built.instance_id,
            "namespace_view_id": view.view_id,
            "scope": built.descriptor.scope,
            "materialization_policy": 1,
            "fs_semantics": 1,
            "access_projection": 0,
            "metadata_root": built.metadata_root,
            "snapshot_id": built.snapshot_id,
        },
        "publication_sequence": seq.to_string(),
        "writer_epoch": "1",
        "lease_id": ctx.lease_id,
        "lease_expires_at": crate::ceres::snapshot::runtime::rfc3339(ctx.lease_expires_at_unix),
        "authorization_epoch": "1",
        "resolved_at": crate::ceres::snapshot::runtime::rfc3339(now_unix()),
        "delivery": "full",
    });
    Ok(Json(body).into_response())
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

fn default_limit() -> u32 {
    128
}

async fn directory(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<DirectoryQuery>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    if !(1..=256).contains(&q.limit) {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "limit must be 1..256",
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
    let etag = format!(
        "\"{}:{}\"",
        &snapshot_id[..16.min(snapshot_id.len())],
        hex_of(&built.page_id)
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

#[derive(Deserialize, Debug)]
struct BlobQuery {
    path: String,
    #[serde(default)]
    expected_digest: Option<String>,
}

#[allow(clippy::too_many_lines)]
async fn blob(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(q): Query<BlobQuery>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
    validate_scope_relative_path(&q.path).map_err(mst2_error_response)?;
    if headers.contains_key("range") {
        // Spec 04 section 9: raw blob has no Range semantics this profile.
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::RangeNotSupported,
            "raw blob reads are whole-file; use chunks for ranges",
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
    let abs_path = abs_view_path(&ctx.built.descriptor.scope, &q.path);

    match resolve_abs(handler.as_ref(), &root_tree, &abs_path)
        .await
        .map_err(mst2_error_response)?
    {
        WalkOutcome::FoundFile {
            fs_kind,
            raw,
            digest,
            ..
        } => {
            if let Some(expected) = &q.expected_digest
                && expected != &format!("sha256:{}", hex_of(&digest))
            {
                return Err(mst2_error_response(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    "content does not match expected_digest",
                )));
            }
            let fs_kind_str = match fs_kind {
                crate::ceres::snapshot::resolver::FsKind::Regular => "regular",
                crate::ceres::snapshot::resolver::FsKind::Executable => "executable",
                crate::ceres::snapshot::resolver::FsKind::Symlink => "symlink",
                crate::ceres::snapshot::resolver::FsKind::Directory => "directory",
            };
            Response::builder()
                .header("etag", format!("\"sha256:{}\"", hex_of(&digest)))
                .header("cache-control", "private, no-cache, no-transform")
                .header("x-mega-fs-kind", fs_kind_str)
                .body(axum::body::Body::from(Bytes::from(raw)))
                .map_err(|e| {
                    mst2_error_response(SnapshotError::new(
                        SnapshotErrorCode::Internal,
                        format!("body build failed: {e}"),
                    ))
                })
        }
        WalkOutcome::FoundDir => Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::NotDirectory,
            "path is a directory",
        ))),
        WalkOutcome::Absent => Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::PathNotFound,
            "path absent in the fixed view",
        ))),
        WalkOutcome::NotDirectory { symlink } => Err(mst2_error_response(SnapshotError::new(
            if symlink {
                SnapshotErrorCode::SymlinkTraversal
            } else {
                SnapshotErrorCode::NotDirectory
            },
            "intermediate component is not a directory",
        ))),
    }
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

#[allow(clippy::too_many_lines)]
async fn lookup(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Json(req): Json<LookupRequest>,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    if req.paths.len() > 128 {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "at most 128 paths per lookup",
        )));
    }

    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let ctx = runtime()
        .context(&snapshot_id)
        .map_err(mst2_error_response)?;
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
        match resolve_abs(handler.as_ref(), &root_tree, &abs_path)
            .await
            .map_err(mst2_error_response)?
        {
            WalkOutcome::FoundDir => {
                let built = build_directory_page(handler.as_ref(), &root_tree, &abs_path)
                    .await
                    .map_err(mst2_error_response)?;
                entry["status"] = json!("found");
                let mut node = json!({"fs_kind": "directory"});
                node["directory_root"] = json!(format!("sha256:{}", hex_of(&built.page_id)));
                node["node_class"] = json!("native_tree");
                node["lifecycle"] = json!("mutable");
                if path != "/" {
                    if let Some(name) = path.rsplit('/').next() {
                        node["name"] = json!(name);
                    }
                }
                entry["node"] = node;
                deepest_dirs.push(abs_path);
            }
            WalkOutcome::FoundFile {
                fs_kind,
                size,
                digest,
                ..
            } => {
                entry["status"] = json!("found");
                let mut node = json!({"fs_kind": fs_kind.as_str()});
                if let Some(name) = path.rsplit('/').next() {
                    node["name"] = json!(name);
                }
                node["size"] = json!(size.to_string());
                node["content_digest"] = json!(format!("sha256:{}", hex_of(&digest)));
                entry["node"] = node;
            }
            WalkOutcome::Absent => {
                entry["status"] = json!("absent");
            }
            WalkOutcome::NotDirectory { symlink } => {
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
    // the 1 MiB response budget of spec 04 sections 5/7.
    let mut proof_pages_out = Vec::new();
    let mut budget: usize = 1_048_576;
    let mut seen_pages: Vec<[u8; 32]> = Vec::new();
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

    let body = json!({
        "snapshot_id": snapshot_id,
        "results": results,
        "proof_pages": proof_pages_out,
    });
    Ok(Json(body).into_response())
}
