//! On-the-fly MTP2 page building for native directories (spec 05).
//!
//! The slice builds pages from fixed git trees at request time; T04 replaces
//! this with persistent incremental projection. Building is canonical
//! (`mst2_codec::metapage::Page::build`), so identical directory content
//! yields identical page_ids across requests.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use base64::Engine;
use git_internal::{hash::ObjectHash, internal::object::tree::Tree};
use mst2_codec::{
    descriptor::{
        ACCESS_PROJECTION_EXACT_FULL, FS_SEMANTICS_LINUX_CODE_V1,
        MATERIALIZATION_POLICY_GIT_RAW_V1, METADATA_CODEC, SCHEMA_VERSION,
    },
    metapage::{Entry, EntryKind, Page, page_id},
};
use sea_orm::ActiveValue::Set;
use sha2::{Digest, Sha256};

pub use crate::ceres::snapshot::projection_observation::ProjectionWork;
use crate::{
    ceres::{
        api_service::ApiHandler,
        snapshot::{
            error::{SnapshotError, SnapshotErrorCode},
            projection_observation::NATIVE_PROJECTION_REVISION,
            resolver::FsKind,
            retention_dag::{MetadataDagBuilder, MetadataDagLimits, ValidatedMetadataDag},
            view::hex,
        },
    },
    jupiter::storage::{Storage, mono_storage::MST2_VERIFICATION_VERSION},
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
    /// Bounds proven while validating this subtree; rechecked at every new
    /// prefix so an identical tree cannot bypass full-path limits after a move.
    path_budget: DescendantPathBudget,
}

#[derive(Debug, Clone, Copy, Default)]
struct DescendantPathBudget {
    /// Includes the slash before each descendant name.
    suffix_bytes: usize,
    components: usize,
}

impl DescendantPathBudget {
    fn include(&mut self, name: &str, child: Self) {
        self.suffix_bytes = self.suffix_bytes.max(1 + name.len() + child.suffix_bytes);
        self.components = self.components.max(1 + child.components);
    }

    fn validate_at(self, prefix: &str) -> Result<(), SnapshotError> {
        crate::ceres::snapshot::view::validate_scope_relative_path(prefix)?;
        let (bytes, components) = if prefix == "/" {
            (0, 0)
        } else {
            (prefix.len(), prefix[1..].split('/').count())
        };
        if bytes + self.suffix_bytes > 4096 || components + self.components > 256 {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "subtree exceeds full-path budget at this prefix",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NativeProjectionKey {
    source_domain: &'static str,
    /// Tagged OID preserves SHA-256 vs BLAKE3 even though both are 64 hex.
    tree_oid: String,
    schema_version: u16,
    metadata_codec: u16,
    materialization_policy: u16,
    fs_semantics: u16,
    access_projection: u16,
    verification_revision: i32,
    projection_revision: u16,
}

impl NativeProjectionKey {
    fn new(oid: ObjectHash) -> Self {
        Self {
            source_domain: "native-git",
            tree_oid: oid.to_tagged_string(),
            schema_version: SCHEMA_VERSION,
            metadata_codec: METADATA_CODEC,
            materialization_policy: MATERIALIZATION_POLICY_GIT_RAW_V1,
            fs_semantics: FS_SEMANTICS_LINUX_CODE_V1,
            access_projection: ACCESS_PROJECTION_EXACT_FULL,
            verification_revision: MST2_VERIFICATION_VERSION,
            projection_revision: NATIVE_PROJECTION_REVISION,
        }
    }
}

/// Storage-owned, disposable derived facts. This cache grants no authorization
/// and provides no durable retention evidence. Clearing it only costs a rebuild.
#[derive(Default)]
pub(crate) struct NativeProjectionCache {
    state: Mutex<NativeProjectionCacheState>,
}

#[derive(Default)]
struct NativeProjectionCacheState {
    pages: HashMap<NativeProjectionKey, Arc<BuiltDirectory>>,
    retained_payload_bytes: usize,
    retention_dags: HashMap<NativeRetentionKey, Arc<ValidatedMetadataDag>>,
    retained_dag_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NativeRetentionKey {
    projection: NativeProjectionKey,
    scope: String,
}

impl NativeProjectionCache {
    fn retention_dag(&self, key: &NativeRetentionKey) -> Option<Arc<ValidatedMetadataDag>> {
        self.state.lock().ok()?.retention_dags.get(key).cloned()
    }

    fn insert_retention_dag(&self, key: NativeRetentionKey, dag: Arc<ValidatedMetadataDag>) {
        // The memo has one aggregate residency bound, independent of the page
        // cache. Caller Arcs may outlive eviction; this is no request admission.
        const MAX_DAG_RESIDENT_BYTES: usize = 64 * 1024 * 1024;
        const MAX_DAG_ENTRIES: usize = 16;
        self.insert_retention_dag_with_budget(key, dag, MAX_DAG_RESIDENT_BYTES, MAX_DAG_ENTRIES);
    }

    fn insert_retention_dag_with_budget(
        &self,
        key: NativeRetentionKey,
        dag: Arc<ValidatedMetadataDag>,
        maximum_bytes: usize,
        maximum_entries: usize,
    ) {
        let Some(bytes) = dag
            .residency_bytes()
            .and_then(|bytes| {
                bytes.checked_add(
                    std::mem::size_of::<NativeRetentionKey>()
                        + std::mem::size_of::<Arc<ValidatedMetadataDag>>(),
                )
            })
            .and_then(|bytes| bytes.checked_add(key.scope.capacity()))
            .and_then(|bytes| bytes.checked_add(key.projection.tree_oid.capacity()))
        else {
            return;
        };
        if bytes > maximum_bytes || maximum_entries == 0 {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            if state.retention_dags.contains_key(&key) {
                return;
            }
            if state.retention_dags.len() >= maximum_entries
                || state.retained_dag_bytes > maximum_bytes - bytes
            {
                state.retention_dags.clear();
                state.retained_dag_bytes = 0;
            }
            state.retained_dag_bytes += bytes;
            state.retention_dags.insert(key, dag);
        }
    }

    fn get(&self, oid: ObjectHash) -> Option<Arc<BuiltDirectory>> {
        // A poisoned optimization cache is a miss, not a serving failure.
        self.state
            .lock()
            .ok()?
            .pages
            .get(&NativeProjectionKey::new(oid))
            .cloned()
    }

    fn insert(&self, oid: ObjectHash, directory: Arc<BuiltDirectory>) {
        // Bound retained payload as well as directory count: a wide directory
        // retains all entries and codec names even though its root is <=16 KiB.
        const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
        let payload_bytes = std::mem::size_of::<BuiltDirectory>()
            + directory.page_bytes.capacity()
            + directory.entries.capacity() * std::mem::size_of::<DirEntry>()
            + directory.codec_entries.capacity() * std::mem::size_of::<Entry>()
            + directory
                .entries
                .iter()
                .map(|entry| entry.name.capacity() + entry.oid.capacity())
                .sum::<usize>()
            + directory
                .codec_entries
                .iter()
                .map(|entry| entry.name.capacity())
                .sum::<usize>();
        if payload_bytes > MAX_PAYLOAD_BYTES {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            let key = NativeProjectionKey::new(oid);
            if state.pages.contains_key(&key) {
                return;
            }
            if state.pages.len() >= 8192
                || state.retained_payload_bytes + payload_bytes > MAX_PAYLOAD_BYTES
            {
                state.pages.clear();
                state.retained_payload_bytes = 0;
            }
            state.retained_payload_bytes += payload_bytes;
            state.pages.insert(key, directory);
        }
    }
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
    /// Preserve the source hash kind when preparing a cached child closure.
    directory_tree_oid: Option<ObjectHash>,
}

/// Fixed source/profile identity for a prepared metadata closure. This grants
/// no authorization, durable retention or coverage of file/chunk payloads.
#[derive(Debug)]
pub struct PreparedNativeMetadataRetention {
    key: NativeRetentionKey,
    dag: Arc<ValidatedMetadataDag>,
}

impl PreparedNativeMetadataRetention {
    #[cfg(test)]
    pub(crate) fn test_installation(dag: Arc<ValidatedMetadataDag>, scope: &str) -> Self {
        let tree_oid =
            ObjectHash::from_hex_for_kind(git_internal::hash::HashKind::Sha1, &"a".repeat(40))
                .unwrap();
        Self {
            key: NativeRetentionKey {
                projection: NativeProjectionKey::new(tree_oid),
                scope: scope.to_owned(),
            },
            dag,
        }
    }
    pub(crate) fn install_plan(
        &self,
    ) -> Result<super::metadata_install::MetadataInstallPlan, SnapshotError> {
        use super::metadata_install::{MetadataInstallIdentity, MetadataInstallPlan};
        let key = &self.key.projection;
        MetadataInstallPlan::from_validated(
            MetadataInstallIdentity {
                source_domain: key.source_domain.to_owned(),
                tagged_root_tree_oid: key.tree_oid.clone(),
                scope: self.key.scope.clone(),
                schema_version: key.schema_version,
                metadata_codec: key.metadata_codec,
                materialization_policy: key.materialization_policy,
                fs_semantics: key.fs_semantics,
                access_projection: key.access_projection,
                verification_revision: key.verification_revision,
                projection_revision: key.projection_revision,
            },
            &self.dag,
        )
    }
    pub fn fixed_root_tree_oid(&self) -> &str {
        &self.key.projection.tree_oid
    }
    pub fn scope(&self) -> &str {
        &self.key.scope
    }
    pub fn metadata_codec(&self) -> u16 {
        self.key.projection.metadata_codec
    }
    pub fn schema_version(&self) -> u16 {
        self.key.projection.schema_version
    }
    pub fn dag(&self) -> &Arc<ValidatedMetadataDag> {
        &self.dag
    }
}

/// Explicit preparation, separate from resolve/lease success. A hit returns
/// the immutable Arc before traversing any child Git tree. A miss collects
/// from existing native codec entries, rebuilding an evicted child as needed.
pub async fn prepare_native_metadata_retention<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &Tree,
    scope: &str,
    limits: MetadataDagLimits,
) -> Result<PreparedNativeMetadataRetention, SnapshotError> {
    crate::ceres::snapshot::view::validate_scope_relative_path(scope)?;
    if !handler.native_snapshot_projection() {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "retention preparation requires native Git projection",
        ));
    }
    let storage = handler.get_context();
    let limits = limits.effective();
    let key = NativeRetentionKey {
        projection: NativeProjectionKey::new(root_tree.id),
        scope: scope.to_owned(),
    };
    if let Some(dag) = storage.native_projection_cache.retention_dag(&key) {
        dag.check_limits(limits)?;
        return Ok(PreparedNativeMetadataRetention { key, dag });
    }
    let mut projection_budget = MetadataProjectionBudget::new(limits);
    let mut work = ProjectionWork::default();
    let root = if scope == "/" {
        build_subtree_inner(
            handler,
            &storage,
            root_tree,
            scope,
            true,
            &mut work,
            Some(&mut projection_budget),
        )
        .await?
    } else {
        let tree = fetch_tree_with_work(handler, root_tree, scope, &mut work).await?;
        build_subtree_inner(
            handler,
            &storage,
            &tree,
            scope,
            true,
            &mut work,
            Some(&mut projection_budget),
        )
        .await?
    };
    let root_id = root.page_id;
    let mut builder = MetadataDagBuilder::new(limits);
    let mut scheduled = HashSet::from([root_id]);
    let mut pending = vec![(root, scope.to_owned())];
    while let Some((directory, path)) = pending.pop() {
        builder.add_directory(&directory.page_bytes, &directory.codec_entries)?;
        for entry in directory
            .entries
            .iter()
            .filter(|entry| entry.fs_kind == FsKind::Directory)
        {
            let child_id = entry.directory_root.ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::IntegrityError,
                    "directory lacks its metadata root",
                )
            })?;
            if !scheduled.insert(child_id) {
                continue;
            }
            let child_oid = entry.directory_tree_oid.ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::IntegrityError,
                    "directory lacks its tagged source identity",
                )
            })?;
            let child_path = if path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{path}/{}", entry.name)
            };
            let child = if let Some(hit) =
                cached_subtree(&storage, child_oid, &child_path, true, &mut work)?
            {
                hit
            } else {
                let tree = handler
                    .get_tree_by_hash(&entry.oid)
                    .await
                    .map_err(|error| {
                        SnapshotError::new(SnapshotErrorCode::Internal, error.to_string())
                    })?;
                if tree.id != child_oid {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::IntegrityError,
                        "retention child tree identity mismatch",
                    ));
                }
                build_subtree_inner(
                    handler,
                    &storage,
                    &tree,
                    &child_path,
                    true,
                    &mut work,
                    Some(&mut projection_budget),
                )
                .await?
            };
            if child.page_id != child_id {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    "retention child page disagrees with fixed parent",
                ));
            }
            pending.push((child, child_path));
        }
    }
    let dag = Arc::new(builder.finish(root_id)?);
    storage
        .native_projection_cache
        .insert_retention_dag(key.clone(), Arc::clone(&dag));
    Ok(PreparedNativeMetadataRetention { key, dag })
}

/// Build the MTP2 page for one directory of the fixed view. `rel_path` is
/// an absolute path within the fixed global view, including the descriptor
/// scope prefix ("/" = the global root); `root_tree` is that view's global
/// root tree — never a rebased scope tree or current-ref read. Every entry is represented;
/// unsupported entries reject the whole projection (spec: no silent drops).
///
/// Native results are memoized by storage assembly, tagged subtree tree OID
/// and verification/profile revision. A new global root does not invalidate
/// unchanged subtrees. The fixed root is resolved to `rel_path` once; recursive
/// descent then follows direct child OIDs rather than re-walking from the root.
pub async fn build_directory_page<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    rel_path: &str,
) -> Result<Arc<BuiltDirectory>, SnapshotError> {
    let (directory, work) = build_directory_page_with_work(handler, root_tree, rel_path).await?;
    tracing::debug!(
        ?work,
        path = rel_path,
        "native snapshot projection memoization work"
    );
    Ok(directory)
}

/// Build one directory, returning operation-local counters. Authorization and
/// the fixed-view source selection remain the caller's responsibility.
pub async fn build_directory_page_with_work<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &Tree,
    rel_path: &str,
) -> Result<(Arc<BuiltDirectory>, ProjectionWork), SnapshotError> {
    let storage = handler.get_context();
    let mut work = ProjectionWork::default();
    let native = handler.native_snapshot_projection();
    if rel_path == "/" {
        // Even cloning a wide fixed root would copy all its entries on a hit.
        let directory =
            build_subtree(handler, &storage, root_tree, rel_path, native, &mut work).await?;
        return Ok((directory, work));
    }
    let tree = fetch_tree_with_work(handler, root_tree, rel_path, &mut work).await?;
    let directory = build_subtree(handler, &storage, &tree, rel_path, native, &mut work).await?;
    Ok((directory, work))
}

fn cached_subtree(
    storage: &Storage,
    oid: ObjectHash,
    rel_path: &str,
    native: bool,
    work: &mut ProjectionWork,
) -> Result<Option<Arc<BuiltDirectory>>, SnapshotError> {
    if native && let Some(hit) = storage.native_projection_cache.get(oid) {
        hit.path_budget.validate_at(rel_path)?;
        work.reused_subtree_roots += 1;
        work.directory_root_pages_reused += 1;
        work.directory_root_page_bytes_reused += hit.page_bytes.len() as u64;
        return Ok(Some(hit));
    }
    Ok(None)
}

async fn build_subtree<T: ApiHandler + ?Sized>(
    handler: &T,
    storage: &Storage,
    tree: &Tree,
    rel_path: &str,
    native: bool,
    work: &mut ProjectionWork,
) -> Result<Arc<BuiltDirectory>, SnapshotError> {
    build_subtree_inner(handler, storage, tree, rel_path, native, work, None).await
}

struct MetadataProjectionBudget {
    limits: MetadataDagLimits,
    seen: HashSet<ObjectHash>,
    required: HashSet<ObjectHash>,
    edges: HashSet<(ObjectHash, ObjectHash)>,
    entries: usize,
    encoded_entry_bytes: u64,
}

impl MetadataProjectionBudget {
    fn new(limits: MetadataDagLimits) -> Self {
        Self {
            limits,
            seen: HashSet::new(),
            required: HashSet::new(),
            edges: HashSet::new(),
            entries: 0,
            encoded_entry_bytes: 0,
        }
    }

    fn tree(&mut self, tree: &Tree) -> Result<(), SnapshotError> {
        let failed = || {
            SnapshotError::new(
                SnapshotErrorCode::LimitExceeded,
                "native metadata source projection budget exceeded",
            )
        };
        if self.seen.contains(&tree.id) {
            return Ok(());
        }
        self.require_node(tree.id)?;
        self.entries = self
            .entries
            .checked_add(tree.tree_items.len())
            .filter(|count| *count <= self.limits.entries)
            .ok_or_else(failed)?;
        self.encoded_entry_bytes = self
            .encoded_entry_bytes
            .checked_add(mst2_codec::metapage::HEADER_LEN as u64)
            .filter(|bytes| *bytes <= self.limits.payload_bytes)
            .ok_or_else(failed)?;
        for item in &tree.tree_items {
            let kind = FsKind::from_git_mode(item.mode).ok_or_else(|| {
                SnapshotError::new(
                    SnapshotErrorCode::UnsupportedEntry,
                    "unsupported entry in native retention projection",
                )
            })?;
            let value_bytes = if kind == FsKind::Directory { 32 } else { 40 };
            self.encoded_entry_bytes = self
                .encoded_entry_bytes
                .checked_add((3 + item.name.len() + value_bytes) as u64)
                .filter(|bytes| *bytes <= self.limits.payload_bytes)
                .ok_or_else(failed)?;
            if kind == FsKind::Directory {
                self.require_node(item.id)?;
                if !self.edges.contains(&(tree.id, item.id)) {
                    if self.edges.len() >= self.limits.edges {
                        return Err(failed());
                    }
                    self.edges.insert((tree.id, item.id));
                }
            }
        }
        if self.required.len() > self.limits.nodes {
            return Err(failed());
        }
        self.seen.insert(tree.id);
        Ok(())
    }

    fn require_node(&mut self, id: ObjectHash) -> Result<(), SnapshotError> {
        if !self.required.contains(&id) {
            if self.required.len() >= self.limits.nodes {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::LimitExceeded,
                    "native metadata source node budget exceeded",
                ));
            }
            self.required.insert(id);
        }
        Ok(())
    }
}

async fn build_subtree_inner<T: ApiHandler + ?Sized>(
    handler: &T,
    storage: &Storage,
    tree: &Tree,
    rel_path: &str,
    native: bool,
    work: &mut ProjectionWork,
    mut budget: Option<&mut MetadataProjectionBudget>,
) -> Result<Arc<BuiltDirectory>, SnapshotError> {
    crate::ceres::snapshot::view::validate_scope_relative_path(rel_path)?;
    if let Some(budget) = budget.as_deref_mut() {
        budget.tree(tree)?;
    }
    if let Some(hit) = cached_subtree(storage, tree.id, rel_path, native, work)? {
        return Ok(hit);
    }
    let dirents = crate::ceres::snapshot::resolver::direct_entries(tree)?;
    work.directories_rebuilt += 1;
    work.directory_entries_scanned += dirents.len() as u64;

    // T03 write-through verification: consult verified records first, then
    // fetch + hash the misses and persist them. Invalid records and lookup
    // failures are errors, never a missing verification fact.
    let blob_oids: Vec<String> = dirents
        .iter()
        .filter(|(_, k, _)| *k != FsKind::Directory)
        .map(|(_, _, oid)| oid.clone())
        .collect();
    let verified = storage
        .mono_storage()
        .get_verified_blobs(blob_oids)
        .await
        .map_err(|e| {
            let code = if matches!(
                e,
                crate::common::errors::MegaError::ObjStorageInconsistent(_)
            ) {
                SnapshotErrorCode::IntegrityError
            } else {
                SnapshotErrorCode::Internal
            };
            tracing::warn!(error = %e, "verified blob lookup failed");
            SnapshotError::new(code, "verified blob lookup failed")
        })?;

    let mut new_verified = HashMap::new();
    let mut path_budget = DescendantPathBudget::default();
    let mut entries = Vec::with_capacity(dirents.len());
    let mut codec_entries = Vec::with_capacity(dirents.len());
    for (name, fs_kind, oid) in dirents {
        let child_rel = if rel_path == "/" {
            format!("/{name}")
        } else {
            format!("{rel_path}/{name}")
        };
        crate::ceres::snapshot::view::validate_scope_relative_path(&child_rel)?;
        match fs_kind {
            FsKind::Directory => {
                let child_oid =
                    ObjectHash::from_hex_for_kind(tree.id.kind(), &oid).map_err(|e| {
                        SnapshotError::new(SnapshotErrorCode::IntegrityError, e.to_string())
                    })?;
                let child_page = if let Some(hit) =
                    cached_subtree(storage, child_oid, &child_rel, native, work)?
                {
                    hit
                } else {
                    work.tree_fetches += 1;
                    let child_tree = handler.get_tree_by_hash(&oid).await.map_err(|e| {
                        SnapshotError::new(SnapshotErrorCode::Internal, e.to_string())
                    })?;
                    if child_tree.id != child_oid {
                        return Err(SnapshotError::new(
                            SnapshotErrorCode::IntegrityError,
                            "fetched child tree identity mismatch",
                        ));
                    }
                    Box::pin(build_subtree_inner(
                        handler,
                        storage,
                        &child_tree,
                        &child_rel,
                        native,
                        work,
                        budget.as_deref_mut(),
                    ))
                    .await?
                };
                path_budget.include(&name, child_page.path_budget);
                codec_entries.push(Entry::dir(name.as_bytes(), child_page.page_id));
                entries.push(DirEntry {
                    name,
                    fs_kind,
                    oid,
                    size: None,
                    content_digest: None,
                    directory_root: Some(child_page.page_id),
                    directory_tree_oid: Some(child_oid),
                });
            }
            FsKind::Regular | FsKind::Executable | FsKind::Symlink => {
                path_budget.include(&name, DescendantPathBudget::default());
                let (size, digest) = if let Some(v) = verified.get(&oid) {
                    work.verified_blob_hits += 1;
                    // Verified record: 64-bit size + raw digest, no content read.
                    let d: [u8; 32] = v.raw_sha256.as_slice().try_into().map_err(|_| {
                        SnapshotError::new(
                            SnapshotErrorCode::IntegrityError,
                            "invalid verified content digest",
                        )
                    })?;
                    let size = u64::try_from(v.size).map_err(|_| {
                        SnapshotError::new(
                            SnapshotErrorCode::IntegrityError,
                            "invalid verified content size",
                        )
                    })?;
                    (size, d)
                } else {
                    work.verified_blob_misses += 1;
                    let raw = fetch_raw_blob(handler, &oid).await?;
                    work.raw_bytes_fetched += raw.len() as u64;
                    let mut h = Sha256::new();
                    h.update(&raw);
                    work.raw_bytes_hashed += raw.len() as u64;
                    let digest: [u8; 32] = h.finalize().into();
                    new_verified.entry(oid.clone()).or_insert(
                        crate::callisto::mst2_verified_object::ActiveModel {
                            id: sea_orm::ActiveValue::NotSet,
                            storage_domain: Set("git".to_string()),
                            git_oid: Set(oid.clone()),
                            object_kind: Set("blob".to_string()),
                            raw_sha256: Set(digest.to_vec()),
                            size: Set(raw.len() as i64),
                            verification_version: Set(MST2_VERIFICATION_VERSION),
                            state: Set("VERIFIED".to_string()),
                            created_at: Set(chrono_now()),
                        },
                    );
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
                    directory_tree_oid: None,
                });
            }
        }
    }
    if !new_verified.is_empty() {
        // Records only ever describe already-fetched content; a persist
        // failure loses an optimization, never correctness.
        if let Err(e) = storage
            .mono_storage()
            .insert_verified_blobs(new_verified.into_values().collect())
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
    work.directory_root_pages_built += 1;
    work.directory_root_page_bytes_built += page_bytes.len() as u64;
    let pid = page_id(&page_bytes);
    let built = Arc::new(BuiltDirectory {
        page_bytes,
        page_id: pid,
        entries,
        codec_entries,
        path_budget,
    });
    if native {
        storage
            .native_projection_cache
            .insert(tree.id, Arc::clone(&built));
    }
    Ok(built)
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
        out.push((p, built.page_id, built.page_bytes.clone()));
    }
    Ok(out)
}

pub fn base64_of(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

async fn fetch_tree_with_work<T: ApiHandler + ?Sized>(
    handler: &T,
    root_tree: &git_internal::internal::object::tree::Tree,
    rel_path: &str,
    work: &mut ProjectionWork,
) -> Result<git_internal::internal::object::tree::Tree, SnapshotError> {
    crate::ceres::snapshot::view::validate_scope_relative_path(rel_path)?;
    if rel_path == "/" {
        return Ok(root_tree.clone());
    }
    let comps: Vec<&str> = rel_path[1..].split('/').collect();
    let mut current = Cow::Borrowed(root_tree);
    for comp in comps {
        let position = current.tree_items.iter().position(|x| x.name == comp);
        work.scope_path_entries_examined +=
            position.map_or(current.tree_items.len(), |index| index + 1) as u64;
        let item = position
            .map(|index| &current.tree_items[index])
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
        if kind != FsKind::Directory {
            return Err(SnapshotError::new(
                SnapshotErrorCode::NotDirectory,
                format!("{rel_path} is not a directory"),
            ));
        }
        let expected_oid = item.id;
        work.tree_fetches += 1;
        let fetched = handler
            .get_tree_by_hash(&item.id.to_string())
            .await
            .map_err(|e| {
                SnapshotError::new(
                    SnapshotErrorCode::Internal,
                    format!("tree fetch failed for component '{comp}': {e}"),
                )
            })?;
        if fetched.id != expected_oid {
            return Err(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "fetched scope tree identity mismatch",
            ));
        }
        current = Cow::Owned(fetched);
    }
    Ok(current.into_owned())
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
    crate::ceres::snapshot::view::validate_scope_relative_path(abs_path)?;
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

/// GitService stores and returns raw file content, without an object header.
pub async fn fetch_raw_blob<T: ApiHandler + ?Sized>(
    handler: &T,
    oid: &str,
) -> Result<Vec<u8>, SnapshotError> {
    let data = handler.get_raw_blob_by_hash(oid).await.map_err(|e| {
        // Missing/corrupt content must be an error, never an empty file
        // (spec 00 SYS-04).
        let code = match e {
            crate::common::errors::MegaError::ObjStorageNotFound(_) => {
                SnapshotErrorCode::ObjectUnavailable
            }
            crate::common::errors::MegaError::ObjStorageInconsistent(_) => {
                SnapshotErrorCode::IntegrityError
            }
            _ => SnapshotErrorCode::Internal,
        };
        tracing::warn!(error = %e, "fixed-view blob fetch failed");
        SnapshotError::new(code, "fixed-view content could not be read")
    })?;
    Ok(data)
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

#[cfg(test)]
#[path = "native_projection_tests.rs"]
mod native_projection_tests;

#[cfg(test)]
#[path = "retention_prepare_tests.rs"]
mod retention_prepare_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use bytes::Bytes;
    use git_internal::internal::{
        metadata::EntryMeta,
        object::{
            blob::Blob,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };
    use sea_orm::{ActiveValue::Set, EntityTrait, IntoActiveModel};
    use tower::ServiceExt;

    use super::*;
    use crate::{
        api::{
            MonoApiServiceState, oauth::api_store::BrowserSessionStore, router::snapshot_router,
        },
        callisto::{mega_refs, mst2_verified_object},
        ceres::api_service::cache::GitObjectCache,
        config::testing::isolated_config,
        jupiter::{
            storage::base_storage::StorageConnector,
            tests::{test_redis_manager, test_storage_with_config},
            utils::converter::IntoMegaModel,
        },
    };

    async fn state(temp: &std::path::Path) -> MonoApiServiceState {
        let mut config = isolated_config(temp.join("config"));
        config.mst2.enabled = true;
        config.mst2.instance_uuid = Some(uuid::Uuid::new_v4().to_string());
        config.mst2.auth_token = Some("mst2-projection-test".to_string());
        let storage = test_storage_with_config(temp, config).await;
        MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage,
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_string(),
        }
    }

    async fn seed(state: &MonoApiServiceState, filename: &str, raw: &[u8]) -> String {
        seed_path(state, &format!("/{filename}"), raw).await
    }

    async fn seed_path(state: &MonoApiServiceState, path: &str, raw: &[u8]) -> String {
        let blob = Blob::from_content_bytes(raw.to_vec());
        state
            .storage
            .git_service
            .save_object_from_raw(Bytes::copy_from_slice(raw))
            .await
            .unwrap();
        let mut child_id = blob.id;
        let mut child_mode = TreeItemMode::Blob;
        let mut trees = Vec::new();
        for name in path[1..].split('/').rev() {
            let tree =
                Tree::from_tree_items(vec![TreeItem::new(child_mode, child_id, name.to_string())])
                    .unwrap();
            child_id = tree.id;
            child_mode = TreeItemMode::Tree;
            trees.push(tree.into_mega_model(EntryMeta::new()).into_active_model());
        }
        let mono = state.storage.mono_storage();
        mono.batch_save_model(trees).await.unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                "refs/heads/main".to_string(),
                blob.id.to_string(),
                child_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        blob.id.to_string()
    }

    async fn seed_legacy(state: &MonoApiServiceState, oid: &str, raw: &[u8]) {
        mst2_verified_object::Entity::insert(mst2_verified_object::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            storage_domain: Set("git".to_string()),
            git_oid: Set(oid.to_string()),
            object_kind: Set("blob".to_string()),
            raw_sha256: Set(Sha256::digest(raw).to_vec()),
            size: Set(raw.len() as i64),
            verification_version: Set(1),
            state: Set("VERIFIED".to_string()),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        })
        .exec(state.storage.mono_storage().get_connection())
        .await
        .unwrap();
    }

    async fn snapshot_id(state: &MonoApiServiceState, scope: &str) -> String {
        let main = state
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let handler = crate::ceres::api_service::mono_api_service::MonoApiService::from(state);
        let tree = handler.get_tree_by_hash(&main.ref_tree_hash).await.unwrap();
        let built = build_directory_page(&handler, &tree, scope).await.unwrap();
        crate::ceres::snapshot::descriptor::build(
            &state.storage.config().mst2,
            &crate::ceres::snapshot::view::SnapshotView::from_commit(
                &main.ref_commit_hash,
                &main.ref_tree_hash,
            ),
            scope,
            built.page_id,
        )
        .unwrap()
        .snapshot_id
    }

    fn request(method: &str, uri: &str, lease: Option<&str>, body: Body) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer mst2-projection-test")
            .header("content-type", "application/json");
        if let Some(lease) = lease {
            request = request.header("x-mega-snapshot-lease", lease);
        }
        request.body(body).unwrap()
    }

    #[tokio::test]
    async fn mst2_raw_header_like_bytes_survive_projection_and_http_reads() {
        assert_header_like_blob_http(false).await;
    }

    #[tokio::test]
    async fn mst2_legacy_header_like_fact_is_reverified_before_http_projection() {
        assert_header_like_blob_http(true).await;
    }

    async fn assert_header_like_blob_http(legacy: bool) {
        let temp = tempfile::TempDir::new().unwrap();
        let state = state(temp.path()).await;
        let raw = b"blob 3\0abc";
        let filename = format!("binary-{}", uuid::Uuid::new_v4());
        let oid = seed(&state, &filename, raw).await;
        if legacy {
            seed_legacy(&state, &oid, b"abc").await;
        }
        let app = snapshot_router::routers(state.clone()).with_state(state.clone());
        let resolve = app
            .clone()
            .oneshot(request(
                "POST",
                "/snapshots/resolve",
                None,
                Body::from(r#"{"target":{"kind":"latest"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resolve.status(), 200);
        let resolved: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resolve.into_body(), 1_048_576)
                .await
                .unwrap(),
        )
        .unwrap();
        let snapshot = snapshot_id(&state, "/").await;
        let lease = resolved["lease_id"].as_str().unwrap();
        let digest = sha256_hex(raw);
        let rows = state
            .storage
            .mono_storage()
            .get_verified_blobs(vec![oid.clone()])
            .await
            .unwrap();
        assert_eq!(rows[&oid].size, raw.len() as i64);
        assert_eq!(rows[&oid].raw_sha256, Sha256::digest(raw).to_vec());
        assert_eq!(rows[&oid].verification_version, MST2_VERIFICATION_VERSION);

        let directory = app
            .clone()
            .oneshot(request(
                "GET",
                &format!("/snapshots/{snapshot}/directory"),
                Some(lease),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(directory.status(), 200);
        let directory: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(directory.into_body(), 1_048_576)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(directory["entries"][0]["size"], raw.len().to_string());
        assert_eq!(
            directory["entries"][0]["content_digest"],
            format!("sha256:{digest}")
        );
        let uri =
            format!("/snapshots/{snapshot}/blob?path=/{filename}&expected_digest=sha256:{digest}");
        let get = app
            .clone()
            .oneshot(request("GET", &uri, Some(lease), Body::empty()))
            .await
            .unwrap();
        assert_eq!(get.status(), 200);
        assert_eq!(
            axum::body::to_bytes(get.into_body(), 128)
                .await
                .unwrap()
                .as_ref(),
            raw
        );
        let head = app
            .clone()
            .oneshot(request("HEAD", &uri, Some(lease), Body::empty()))
            .await
            .unwrap();
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["content-length"], raw.len().to_string());
        assert!(
            axum::body::to_bytes(head.into_body(), 128)
                .await
                .unwrap()
                .is_empty()
        );
        let again = app
            .oneshot(request(
                "POST",
                "/snapshots/resolve",
                None,
                Body::from(r#"{"target":{"kind":"latest"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(again.status(), 200);
        let again: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(again.into_body(), 1_048_576)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(again["descriptor"], resolved["descriptor"]);
        crate::ceres::snapshot::runtime::runtime()
            .release_lease(again["lease_id"].as_str().unwrap());
        crate::ceres::snapshot::runtime::runtime().release_lease(lease);
    }

    #[tokio::test]
    async fn mst2_invalid_verified_record_is_not_recomputed_into_a_successful_snapshot() {
        let temp = tempfile::TempDir::new().unwrap();
        let state = state(temp.path()).await;
        let filename = format!("corrupt-{}", uuid::Uuid::new_v4());
        let raw = b"valid stored content";
        let oid = seed(&state, &filename, raw).await;
        mst2_verified_object::Entity::insert(mst2_verified_object::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            storage_domain: Set("git".to_string()),
            git_oid: Set(oid),
            object_kind: Set("blob".to_string()),
            raw_sha256: Set(vec![7; 31]),
            size: Set(raw.len() as i64),
            verification_version: Set(MST2_VERIFICATION_VERSION),
            state: Set("VERIFIED".to_string()),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        })
        .exec(state.storage.mono_storage().get_connection())
        .await
        .unwrap();
        let response = snapshot_router::routers(state.clone())
            .with_state(state)
            .oneshot(request(
                "POST",
                "/snapshots/resolve",
                None,
                Body::from(r#"{"target":{"kind":"latest"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1_048_576)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "INTEGRITY_ERROR");
    }

    #[tokio::test]
    async fn mst2_legacy_fact_cannot_upgrade_when_raw_storage_is_unavailable() {
        use sea_orm::{ColumnTrait, QueryFilter};

        use crate::orbit_api::object_storage::{ObjectKey, ObjectNamespace};

        let temp = tempfile::TempDir::new().unwrap();
        let state = state(temp.path()).await;
        let filename = format!("missing-{}", uuid::Uuid::new_v4());
        let oid = seed(&state, &filename, b"blob 3\0abc").await;
        seed_legacy(&state, &oid, b"abc").await;
        state
            .storage
            .git_service
            .obj_storage
            .inner
            .delete(&ObjectKey {
                namespace: ObjectNamespace::Git,
                key: oid.clone(),
            })
            .await
            .unwrap();
        let response = snapshot_router::routers(state.clone())
            .with_state(state.clone())
            .oneshot(request(
                "POST",
                "/snapshots/resolve",
                None,
                Body::from(r#"{"target":{"kind":"latest"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1_048_576)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "OBJECT_UNAVAILABLE");
        let saved = mst2_verified_object::Entity::find()
            .filter(mst2_verified_object::Column::GitOid.eq(oid))
            .one(state.storage.mono_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.verification_version, 1);
        assert_eq!(saved.size, 3);
        assert_eq!(saved.raw_sha256, Sha256::digest(b"abc").to_vec());
    }

    #[tokio::test]
    async fn mst2_shared_blob_paths_upgrade_one_verification_record() {
        let temp = tempfile::TempDir::new().unwrap();
        let state = state(temp.path()).await;
        let raw = b"blob 3\0abc";
        let oid = state
            .storage
            .git_service
            .save_object_from_raw(Bytes::copy_from_slice(raw))
            .await
            .unwrap();
        seed_legacy(&state, &oid, b"abc").await;
        let blob = Blob::from_content_bytes(raw.to_vec());
        let tree = Tree::from_tree_items(vec![
            TreeItem::new(
                TreeItemMode::Blob,
                blob.id,
                format!("a-{}", uuid::Uuid::new_v4()),
            ),
            TreeItem::new(
                TreeItemMode::Blob,
                blob.id,
                format!("b-{}", uuid::Uuid::new_v4()),
            ),
        ])
        .unwrap();
        let handler = crate::ceres::api_service::mono_api_service::MonoApiService::from(&state);
        let built = build_subtree(
            &handler,
            &state.storage,
            &tree,
            "/",
            false,
            &mut ProjectionWork::default(),
        )
        .await
        .unwrap();
        assert_eq!(built.entries.len(), 2);
        assert!(built.entries.iter().all(|entry| entry.size == Some(10)
            && entry.content_digest == Some(Sha256::digest(raw).into())));
        let rows = state
            .storage
            .mono_storage()
            .get_verified_blobs(vec![oid.clone()])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&oid].size, 10);
        assert_eq!(rows[&oid].verification_version, MST2_VERIFICATION_VERSION);
    }

    #[tokio::test]
    async fn mst2_projection_checks_full_leaf_path_before_resolve_and_http_reads() {
        for (component_boundary, valid) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let temp = tempfile::TempDir::new().unwrap();
            let state = state(temp.path()).await;
            let marker = uuid::Uuid::new_v4().to_string();
            let mut components = if component_boundary {
                let mut parts = vec!["d".to_string(); if valid { 255 } else { 256 }];
                parts[0] = marker;
                parts.push("file".to_string());
                parts
            } else {
                let mut parts = vec!["d".repeat(255); 15];
                parts[0].replace_range(..marker.len(), &marker);
                parts.push("d".repeat(127));
                parts.push("f".repeat(if valid { 127 } else { 128 }));
                parts
            };
            let filename = components.pop().unwrap();
            let scope = format!("/{}", components.join("/"));
            let full_path = format!("{scope}/{filename}");
            if component_boundary {
                assert_eq!(
                    full_path[1..].split('/').count(),
                    if valid { 256 } else { 257 }
                );
            } else {
                assert_eq!(full_path.len(), if valid { 4096 } else { 4097 });
            }
            let raw = b"boundary file bytes";
            seed_path(&state, &full_path, raw).await;
            let app = snapshot_router::routers(state.clone()).with_state(state.clone());
            let resolve = app
                .clone()
                .oneshot(request(
                    "POST",
                    "/snapshots/resolve",
                    None,
                    Body::from(
                        serde_json::json!({"target":{"kind":"latest"}, "scope":scope}).to_string(),
                    ),
                ))
                .await
                .unwrap();
            let status = resolve.status();
            let resolved: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(resolve.into_body(), 1_048_576)
                    .await
                    .unwrap(),
            )
            .unwrap();
            if !valid {
                assert_eq!(status, 400);
                assert_eq!(resolved["error"]["code"], "SCOPE_INVALID");
                assert!(resolved.get("lease_id").is_none());
                continue;
            }
            assert_eq!(status, 200);
            let snapshot = snapshot_id(&state, &scope).await;
            let lease = resolved["lease_id"].as_str().unwrap();
            let uri = format!(
                "/snapshots/{snapshot}/blob?path=/{filename}&expected_digest=sha256:{}",
                sha256_hex(raw)
            );
            let get = app
                .oneshot(request("GET", &uri, Some(lease), Body::empty()))
                .await
                .unwrap();
            assert_eq!(get.status(), 200);
            assert_eq!(
                axum::body::to_bytes(get.into_body(), 128)
                    .await
                    .unwrap()
                    .as_ref(),
                raw
            );
            crate::ceres::snapshot::runtime::runtime().release_lease(lease);
        }
    }
}
