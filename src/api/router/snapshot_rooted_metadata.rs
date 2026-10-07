//! JSON metadata endpoints use the same certified, protected fixed-root reader.

use mst2_codec::metapage::{Entry, EntryKind};

use super::*;
use crate::{
    ceres::snapshot::runtime::SnapshotContext,
    jupiter::storage::qualified_metadata_family::RootedLookupStatus,
};

fn entry_json(entry: &Entry) -> Result<serde_json::Value, SnapshotError> {
    let name = std::str::from_utf8(&entry.name).map_err(internal)?;
    let kind = match entry.kind {
        EntryKind::Regular => "regular",
        EntryKind::Executable => "executable",
        EntryKind::Symlink => "symlink",
        EntryKind::Directory => "directory",
    };
    let mut value = json!({"name":name,"fs_kind":kind});
    if entry.is_dir() {
        value["directory_root"] = json!(format!("sha256:{}", hex_of(&entry.child_root)));
        value["node_class"] = json!("native_tree");
        value["lifecycle"] = json!("mutable");
    } else {
        value["size"] = json!(entry.size.to_string());
        value["content_digest"] = json!(format!("sha256:{}", hex_of(&entry.content_id)));
    }
    Ok(value)
}

fn cursor_name(
    sid: &str,
    path: &str,
    query: &DirectoryQuery,
) -> Result<Option<String>, SnapshotError> {
    let Some(cursor) = query.cursor.as_deref() else {
        return Ok(None);
    };
    let invalid = || {
        SnapshotError::new(
            SnapshotErrorCode::CursorInvalid,
            "cursor bound to different parameters or invalid",
        )
    };
    let (payload, signature) = cursor.rsplit_once('.').ok_or_else(invalid)?;
    if runtime().sign_cursor(payload) != signature {
        return Err(invalid());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| invalid())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if value["s"].as_str() != Some(sid)
        || value["p"].as_str() != Some(path)
        || value["l"].as_u64() != Some(query.limit as u64)
    {
        return Err(invalid());
    }
    let name = value["a"].as_str().ok_or_else(invalid)?;
    if name.is_empty() || name.len() > 255 || name.contains('/') || name.contains('\0') {
        return Err(invalid());
    }
    Ok(Some(name.into()))
}

#[allow(clippy::result_large_err)]
pub(super) async fn directory_response(
    state: &MonoApiServiceState,
    ctx: &SnapshotContext,
    sid: &str,
    query: &DirectoryQuery,
) -> Result<Response, Response> {
    let absolute = abs_view_path(&ctx.built.descriptor.scope, &query.path);
    let last = cursor_name(sid, &absolute, query)?;
    let repository = state
        .storage
        .rooted_qualified_metadata_writer()
        .await
        .map_err(internal)?;
    let window = repository
        .directory_window(ctx, &query.path, last.as_deref(), query.limit as usize)
        .await
        .map_err(mst2_error_response)?;
    let entries = window
        .entries
        .iter()
        .map(entry_json)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if window.has_more {
        let name = window.entries.last().ok_or_else(|| {
            mst2_error_response(internal(
                "qualified directory returned an empty continuation",
            ))
        })?;
        let payload = json!({"s":sid,"p":absolute,"l":query.limit,"a":std::str::from_utf8(&name.name).map_err(internal)?});
        let encoded = base64_of(&serde_json::to_vec(&payload).map_err(internal)?);
        Some(format!("{encoded}.{}", runtime().sign_cursor(&encoded)))
    } else {
        None
    };
    let ancestors = if query.ancestors.as_deref() == Some("chain") {
        window
            .ancestors
            .iter()
            .filter(|(path, _)| path != &query.path)
            .map(|(path, root)| {
                json!({"path":path,
            "directory_root":format!("sha256:{}",hex_of(root)),"node_class":"native_tree"})
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    revalidate_request(state, ctx)
        .await
        .map_err(mst2_error_response)?;
    let body = json!({"snapshot_id":sid,"path":query.path,"metadata_root":ctx.built.metadata_root,
        "directory_root":format!("sha256:{}",hex_of(&window.directory_root)),"node_class":"native_tree","lifecycle":"mutable",
        "range_start_exclusive":last,"entries":entries,"entry_count":window.entry_count.to_string(),"next_cursor":next,
        "proof_pages":window.proof_pages.iter().map(|(root,bytes)|json!({"digest":format!("sha256:{}",hex_of(root)),
            "data_base64":base64_of(bytes)})).collect::<Vec<_>>(),"ancestor_chain":ancestors});
    let mut response = Json(body).into_response();
    let tag = format!(
        "\"{}:{}:{}:{}\"",
        &sid[..16.min(sid.len())],
        hex_of(&window.directory_root),
        query.limit,
        query.cursor.as_deref().unwrap_or("")
    );
    if let Ok(value) = HeaderValue::from_str(&tag) {
        response.headers_mut().insert("etag", value);
    }
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-cache, no-transform"),
    );
    Ok(response)
}

#[allow(clippy::result_large_err)]
pub(super) async fn lookup_response(
    state: &MonoApiServiceState,
    ctx: &SnapshotContext,
    sid: &str,
    request: &LookupRequest,
) -> Result<Response, Response> {
    let repository = state
        .storage
        .rooted_qualified_metadata_writer()
        .await
        .map_err(internal)?;
    let batch = repository
        .lookup_metadata(ctx, &request.paths)
        .await
        .map_err(mst2_error_response)?;
    let mut results = Vec::with_capacity(batch.results.len());
    for (path, status) in request.paths.iter().zip(batch.results) {
        let mut item = json!({"path":path});
        match status {
            RootedLookupStatus::Directory(root) => {
                let mut node = json!({"fs_kind":"directory","directory_root":format!("sha256:{}",hex_of(&root)),
                    "node_class":"native_tree","lifecycle":"mutable"});
                if path != "/" {
                    node["name"] = json!(path.rsplit('/').next());
                }
                item["status"] = json!("found");
                item["node"] = node;
            }
            RootedLookupStatus::File { entry, .. } => {
                item["status"] = json!("found");
                item["node"] = entry_json(&entry)?;
            }
            RootedLookupStatus::Absent => item["status"] = json!("absent"),
            RootedLookupStatus::NotDirectory { symlink } => {
                item["status"] = json!(if symlink {
                    "symlink_traversal"
                } else {
                    "not_directory"
                })
            }
        }
        results.push(item);
    }
    revalidate_request(state, ctx)
        .await
        .map_err(mst2_error_response)?;
    Ok(Json(json!({"snapshot_id":sid,"results":results,"proof_pages":batch.proof_pages.iter().map(|(root,bytes)|json!({
        "digest":format!("sha256:{}",hex_of(root)),"data_base64":base64_of(bytes)})).collect::<Vec<_>>()})).into_response())
}
