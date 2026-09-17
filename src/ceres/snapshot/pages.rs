//! On-the-fly MTP2 page building for native directories (spec 05).
//!
//! The slice builds pages from fixed git trees at request time; T04 replaces
//! this with persistent incremental projection. Building is canonical
//! (`mst2_codec::metapage::Page::build`), so identical directory content
//! yields identical page_ids across requests.

use std::collections::HashMap;

use base64::Engine;
use mst2_codec::metapage::{Entry, EntryKind, Page, page_id};
use sea_orm::ActiveValue::Set;
use sha2::{Digest, Sha256};

use crate::ceres::{
    api_service::ApiHandler,
    snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        resolver::FsKind,
        view::hex,
    },
};

/// A directory page plus everything the JSON layer needs.
pub struct BuiltDirectory {
    /// Canonical MTP2 page bytes for this directory (header + payload).
    pub page_bytes: Vec<u8>,
    /// `page_id` of the page.
    pub page_id: [u8; 32],
    /// Direct entries, byte-sorted, matching the page contents.
    pub entries: Vec<DirEntry>,
    /// The codec entries the page was built from, so callers can walk a route
    /// through the same canonical tree (`Page::pages_along_route`).
    pub codec_entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub fs_kind: FsKind,
    /// Raw git oid (tree oid for dirs, blob oid for files).
    pub oid: String,
    /// File size in bytes (files and symlinks only).
    pub size: Option<u64>,
    /// SHA-256 over raw file bytes (files and symlinks only).
    pub content_digest: Option<[u8; 32]>,
    /// Child directory page id (directories only).
    pub directory_root: Option<[u8; 32]>,
}

/// Build the MTP2 page for one directory of the fixed view. `rel_path` is
/// scope-relative ("/" = the root directory); `root_tree` is the fixed view's
/// root tree — never a current-ref read. Every entry is represented;
/// unsupported entries reject the whole projection (spec: no silent drops).
pub async fn build_directory_page<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    rel_path: &str,
) -> Result<BuiltDirectory, SnapshotError> {
    let tree = fetch_tree(handler, root_tree, rel_path).await?;
    let dirents = crate::ceres::snapshot::resolver::direct_entries(&tree)?;

    // T03 write-through verification: consult verified records first, then
    // fetch + hash the misses and persist them. A verification-record read
    // failure degrades to recomputation (safe: strictly more verification).
    let blob_oids: Vec<String> = dirents
        .iter()
        .filter(|(_, k, _)| *k != FsKind::Directory)
        .map(|(_, _, oid)| oid.clone())
        .collect();
    let verified = match handler
        .get_context()
        .mono_storage()
        .get_verified_blobs(blob_oids)
        .await
    {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(error = %e, "verified blob lookup failed; recomputing digests");
            HashMap::new()
        }
    };

    let mut new_verified: Vec<crate::callisto::mst2_verified_object::ActiveModel> = Vec::new();
    let mut entries = Vec::with_capacity(dirents.len());
    let mut codec_entries = Vec::with_capacity(dirents.len());
    for (name, fs_kind, oid) in dirents {
        match fs_kind {
            FsKind::Directory => {
                let child_rel = if rel_path == "/" {
                    format!("/{name}")
                } else {
                    format!("{rel_path}/{name}")
                };
                let child_page =
                    Box::pin(build_directory_page(handler, root_tree, &child_rel)).await?;
                codec_entries.push(Entry::dir(name.as_bytes(), child_page.page_id));
                entries.push(DirEntry {
                    name,
                    fs_kind,
                    oid,
                    size: None,
                    content_digest: None,
                    directory_root: Some(child_page.page_id),
                });
            }
            FsKind::Regular | FsKind::Executable | FsKind::Symlink => {
                let (size, digest) = if let Some(v) = verified.get(&oid) {
                    // Verified record: 64-bit size + raw digest, no content read.
                    let d: [u8; 32] = v.raw_sha256.clone().try_into().unwrap_or([0u8; 32]);
                    (v.size as u64, d)
                } else {
                    let raw = fetch_raw_blob(handler, &oid).await?;
                    let mut h = Sha256::new();
                    h.update(&raw);
                    let digest: [u8; 32] = h.finalize().into();
                    new_verified.push(crate::callisto::mst2_verified_object::ActiveModel {
                        id: sea_orm::ActiveValue::NotSet,
                        storage_domain: Set("git".to_string()),
                        git_oid: Set(oid.clone()),
                        object_kind: Set("blob".to_string()),
                        raw_sha256: Set(digest.to_vec()),
                        size: Set(raw.len() as i64),
                        verification_version: Set(1),
                        state: Set("VERIFIED".to_string()),
                        created_at: Set(chrono_now()),
                    });
                    (raw.len() as u64, digest)
                };
                let kind = match fs_kind {
                    FsKind::Regular => EntryKind::Regular,
                    FsKind::Executable => EntryKind::Executable,
                    FsKind::Symlink => EntryKind::Symlink,
                    FsKind::Directory => unreachable!("matched above"),
                };
                codec_entries.push(Entry::file(kind, name.as_bytes(), size, digest));
                entries.push(DirEntry {
                    name,
                    fs_kind,
                    oid,
                    size: Some(size),
                    content_digest: Some(digest),
                    directory_root: None,
                });
            }
        }
    }
    if !new_verified.is_empty() {
        // Records only ever describe already-fetched content; a persist
        // failure loses an optimization, never correctness.
        if let Err(e) = handler
            .get_context()
            .mono_storage()
            .insert_verified_blobs(new_verified)
            .await
        {
            tracing::warn!(error = %e, "verified blob persistence failed");
        }
    }

    let page_bytes = Page::build(&codec_entries).map_err(|e| {
        SnapshotError::new(
            SnapshotErrorCode::Internal,
            format!("MTP2 build failed for {rel_path}: {e}"),
        )
    })?;
    let pid = page_id(&page_bytes);
    Ok(BuiltDirectory {
        page_bytes,
        page_id: pid,
        entries,
        codec_entries,
    })
}

/// Proof pages from the scope root down to (and including) `rel_path`,
/// returned as (path, page_id, page bytes).
pub async fn proof_pages<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    rel_path: &str,
) -> Result<Vec<(String, [u8; 32], Vec<u8>)>, SnapshotError> {
    let mut chain = vec!["/".to_string()];
    if rel_path != "/" {
        let comps: Vec<&str> = rel_path[1..].split('/').collect();
        for i in 1..=comps.len() {
            chain.push(format!("/{}", comps[..i].join("/")));
        }
    }
    let mut out = Vec::with_capacity(chain.len());
    for p in chain {
        let built = Box::pin(build_directory_page(handler, root_tree, &p)).await?;
        out.push((p, built.page_id, built.page_bytes));
    }
    Ok(out)
}

pub fn base64_of(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

async fn fetch_tree<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    rel_path: &str,
) -> Result<git_internal::internal::object::tree::Tree, SnapshotError> {
    if rel_path == "/" {
        return Ok(root_tree.clone());
    }
    let comps: Vec<&str> = rel_path[1..].split('/').collect();
    let mut current = root_tree.clone();
    for (i, comp) in comps.iter().enumerate() {
        let last = i == comps.len() - 1;
        let item = current
            .tree_items
            .iter()
            .find(|x| x.name == *comp)
            .ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::PathNotFound,
                    "name absent in enumerated parent directory",
                )
            })?;
        let kind = FsKind::from_git_mode(item.mode).ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::UnsupportedEntry,
                "gitlink entries are not supported in this profile",
            )
        })?;
        if last {
            if kind != FsKind::Directory {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::NotDirectory,
                    format!("{rel_path} is not a directory"),
                ));
            }
            return handler
                .get_tree_by_hash(&item.id.to_string())
                .await
                .map_err(|e| {
                    SnapshotError::new(
                        SnapshotErrorCode::Internal,
                        format!("tree fetch failed for {rel_path}: {e}"),
                    )
                });
        }
        if kind != FsKind::Directory {
            return Err(SnapshotError::new(
                SnapshotErrorCode::NotDirectory,
                "intermediate component is not a directory",
            ));
        }
        current = handler
            .get_tree_by_hash(&item.id.to_string())
            .await
            .map_err(|e| {
                SnapshotError::new(
                    SnapshotErrorCode::Internal,
                    format!("tree fetch failed for component '{comp}': {e}"),
                )
            })?;
    }
    unreachable!("loop returns on the last component")
}

/// Outcome of resolving one absolute view path (spec 04 §7 statuses).
pub enum WalkOutcome {
    /// Directory at this path.
    FoundDir,
    /// File/symlink at this path with raw content and its SHA-256.
    FoundFile {
        fs_kind: FsKind,
        /// Git object oid, so callers can build range-readable projections
        /// without re-walking the tree.
        oid: String,
        raw: Vec<u8>,
        size: u64,
        digest: [u8; 32],
    },
    /// The parent directory exists and was enumerated; name absent.
    Absent,
    /// An intermediate component is not a directory; `symlink` marks the
    /// symlink-traversal case of spec 04 §1.
    NotDirectory { symlink: bool },
}

/// Resolve one absolute view path against the fixed root tree. Absence is
/// proven by enumeration; gitlink entries reject the projection.
pub async fn resolve_abs<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    abs_path: &str,
) -> Result<WalkOutcome, SnapshotError> {
    if abs_path == "/" {
        return Ok(WalkOutcome::FoundDir);
    }
    let comps: Vec<&str> = abs_path[1..].split('/').collect();
    let mut current = root_tree.clone();
    for (i, comp) in comps.iter().enumerate() {
        let last = i == comps.len() - 1;
        let Some(item) = current.tree_items.iter().find(|x| x.name == *comp) else {
            return Ok(WalkOutcome::Absent);
        };
        let Some(kind) = FsKind::from_git_mode(item.mode) else {
            return Err(SnapshotError::new(
                SnapshotErrorCode::UnsupportedEntry,
                format!("gitlink entry '{comp}' is not supported in this profile"),
            ));
        };
        let oid = item.id.to_string();
        if last {
            return match kind {
                FsKind::Directory => Ok(WalkOutcome::FoundDir),
                FsKind::Regular | FsKind::Executable | FsKind::Symlink => {
                    let raw = fetch_raw_blob(handler, &oid).await?;
                    let mut h = Sha256::new();
                    h.update(&raw);
                    let digest: [u8; 32] = h.finalize().into();
                    Ok(WalkOutcome::FoundFile {
                        fs_kind: kind,
                        oid,
                        size: raw.len() as u64,
                        digest,
                        raw,
                    })
                }
            };
        }
        if kind != FsKind::Directory {
            return Ok(WalkOutcome::NotDirectory {
                symlink: kind == FsKind::Symlink,
            });
        }
        current = handler.get_tree_by_hash(&oid).await.map_err(|e| {
            SnapshotError::new(
                SnapshotErrorCode::Internal,
                format!("tree fetch failed for component '{comp}': {e}"),
            )
        })?;
    }
    unreachable!("loop returns on the last component")
}

/// Raw blob bytes with the `blob <len>\0` git header stripped.
pub async fn fetch_raw_blob<T: ApiHandler + ?Sized>(
    handler: &T,
    oid: &str,
) -> Result<Vec<u8>, SnapshotError> {
    let data = handler.get_raw_blob_by_hash(oid).await.map_err(|e| {
        // Missing/corrupt content must be an error, never an empty file
        // (spec 00 SYS-04).
        SnapshotError::new(
            SnapshotErrorCode::Internal,
            format!("blob fetch failed for {oid}: {e}"),
        )
    })?;
    Ok(strip_git_blob_header(&data))
}

fn strip_git_blob_header(data: &[u8]) -> Vec<u8> {
    if let Some(pos) = data.iter().position(|&b| b == 0)
        && data.starts_with(b"blob ")
        && let Ok(len) = std::str::from_utf8(&data[5..pos]).map(|s| s.parse::<usize>())
        && let Ok(len) = len
        && data.len() == pos + 1 + len
    {
        return data[pos + 1..].to_vec();
    }
    data.to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

fn chrono_now() -> chrono::DateTime<chrono::FixedOffset> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
        .unwrap_or_default()
        .with_timezone(&chrono::FixedOffset::east_opt(0).unwrap())
}

pub fn hex_of(id: &[u8; 32]) -> String {
    hex(id)
}
