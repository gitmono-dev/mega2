//! On-the-fly MTP2 page building for native directories (spec 05).
//!
//! The slice builds pages from fixed git trees at request time; T04 replaces
//! this with persistent incremental projection. Building is canonical
//! (`mst2_codec::metapage::Page::build`), so identical directory content
//! yields identical page_ids across requests.

use base64::Engine;
use mst2_codec::metapage::{Entry, EntryKind, Page, page_id};
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
                let raw = fetch_raw_blob(handler, &oid).await?;
                let mut h = Sha256::new();
                h.update(&raw);
                let digest: [u8; 32] = h.finalize().into();
                let kind = match fs_kind {
                    FsKind::Regular => EntryKind::Regular,
                    FsKind::Executable => EntryKind::Executable,
                    FsKind::Symlink => EntryKind::Symlink,
                    FsKind::Directory => unreachable!("matched above"),
                };
                codec_entries.push(Entry::file(kind, name.as_bytes(), raw.len() as u64, digest));
                entries.push(DirEntry {
                    name,
                    fs_kind,
                    oid,
                    size: Some(raw.len() as u64),
                    content_digest: Some(digest),
                    directory_root: None,
                });
            }
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

/// Raw blob bytes with the `blob <len>\0` git header stripped.
async fn fetch_raw_blob<T: ApiHandler + ?Sized>(
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

pub fn hex_of(id: &[u8; 32]) -> String {
    hex(id)
}
