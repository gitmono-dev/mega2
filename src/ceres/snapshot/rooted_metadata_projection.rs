//! Native source projection that stops at certified reused-directory boundaries.
//! Hints and operation-local memoization are not database installation authority.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use git_internal::{hash::ObjectHash, internal::object::tree::Tree};
use mst2_codec::{
    descriptor::{
        ACCESS_PROJECTION_EXACT_FULL, FS_SEMANTICS_LINUX_CODE_V1,
        MATERIALIZATION_POLICY_GIT_RAW_V1, METADATA_CODEC, SCHEMA_VERSION,
    },
    metapage::{Entry, EntryKind, HEADER_LEN, PAGE_MAX_BYTES, Page, page_id},
};
use sea_orm::ActiveValue::Set;
use sha2::{Digest, Sha256};

use super::{
    content_budget::{MemoryBudget, RANGE_WORK_BYTES, projection_budget},
    error::{SnapshotError, SnapshotErrorCode},
    metadata_install::MetadataInstallIdentity,
    projection_observation::NATIVE_PROJECTION_REVISION,
    resolver::{FsKind, direct_entries},
    retention_dag::{MetadataDagLimits, MetadataPageId, MetadataPagePayload},
    rooted_metadata_install::{RootedMetadataInstallPlan, RootedReuseRoot},
    view::validate_scope_relative_path,
};
use crate::{
    ceres::api_service::ApiHandler,
    common::errors::MegaError,
    jupiter::storage::{Storage, mono_storage::MST2_VERIFICATION_VERSION},
    orbit_api::object_storage::{ObjectByteStream, ObjectMeta},
};

const MAX_VERIFIED_FILE_BYTES: u64 = 8_796_093_022_208;
const SOURCE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

#[async_trait]
pub(crate) trait RootedReuseLookup: Send + Sync {
    async fn lookup_reuse(
        &self,
        tree_oid: &str,
        identity: &MetadataInstallIdentity,
    ) -> Result<Option<CertifiedReusableDirectory>, SnapshotError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertifiedReusableDirectory {
    pub(crate) page_id: MetadataPageId,
    pub(crate) proof: RootedReuseRoot,
    pub(crate) relative_path_bytes: usize,
    pub(crate) relative_components: usize,
    pub(crate) closure_nodes_upper: usize,
    pub(crate) closure_edges_upper: usize,
    pub(crate) closure_bytes_upper: u64,
    pub(crate) closure_entries_upper: usize,
}

/// API calls and materialized outputs only, not total database or codec work.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RootedProjectionWork {
    pub(crate) input_tree_entries: u64,
    pub(crate) scope_path_entries_examined: u64,
    pub(crate) tree_fetches: u64,
    pub(crate) reuse_lookups: u64,
    pub(crate) directory_memo_hits: u64,
    pub(crate) directories_scanned: u64,
    pub(crate) direct_entries_scanned: u64,
    pub(crate) verified_blob_queries: u64,
    pub(crate) verified_blob_facts_loaded: u64,
    pub(crate) verified_blob_persistence_batches: u64,
    pub(crate) blob_fetches: u64,
    pub(crate) raw_bytes_fetched: u64,
    pub(crate) raw_bytes_hashed: u64,
    pub(crate) directory_root_builds: u64,
    pub(crate) radix_route_builds: u64,
    pub(crate) codec_input_entries: u64,
    pub(crate) delta_pages: u64,
    pub(crate) delta_bytes: u64,
    pub(crate) reused_roots: u64,
    pub(crate) reused_closure_nodes_upper: u64,
    pub(crate) reused_closure_edges_upper: u64,
    pub(crate) reused_closure_bytes_upper: u64,
    pub(crate) reused_closure_entries_upper: u64,
    pub(crate) plan_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct PreparedRootedNativeMetadata {
    pub(crate) plan: RootedMetadataInstallPlan,
    pub(crate) payloads: Vec<MetadataPagePayload>,
    pub(crate) work: RootedProjectionWork,
}

pub(crate) async fn prepare_rooted_native_metadata<
    T: ApiHandler + ?Sized,
    R: RootedReuseLookup + ?Sized,
>(
    handler: &T,
    root_tree: &Tree,
    scope: &str,
    reuse: &R,
) -> Result<PreparedRootedNativeMetadata, SnapshotError> {
    validate_scope_relative_path(scope)?;
    if !handler.native_snapshot_projection() {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ScopeInvalid,
            "rooted metadata projection requires native Git source semantics",
        ));
    }
    let identity = MetadataInstallIdentity {
        source_domain: "native-git".into(),
        tagged_root_tree_oid: root_tree.id.to_tagged_string(),
        scope: scope.to_owned(),
        schema_version: SCHEMA_VERSION,
        metadata_codec: METADATA_CODEC,
        materialization_policy: MATERIALIZATION_POLICY_GIT_RAW_V1,
        fs_semantics: FS_SEMANTICS_LINUX_CODE_V1,
        access_projection: ACCESS_PROJECTION_EXACT_FULL,
        verification_revision: MST2_VERIFICATION_VERSION,
        projection_revision: NATIVE_PROJECTION_REVISION,
    };
    let storage = handler.get_context();
    let mut state = ProjectionState::new(identity);
    state.work.input_tree_entries = root_tree.tree_items.len() as u64;
    let scoped_oid = resolve_scope_oid(handler, root_tree, scope, &mut state.work).await?;
    let loaded = (scope == "/").then_some(root_tree);
    let root = project_directory(
        handler, reuse, &storage, scoped_oid, loaded, scope, &mut state,
    )
    .await?;
    state.finish(root.page_id)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PathBounds {
    bytes: usize,
    components: usize,
}

impl PathBounds {
    fn include(&mut self, name: &str, child: Self) -> Result<(), SnapshotError> {
        let bytes = name
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(child.bytes))
            .filter(|value| *value <= 4096)
            .ok_or_else(path_limit)?;
        let components = child
            .components
            .checked_add(1)
            .filter(|value| *value <= 256)
            .ok_or_else(path_limit)?;
        self.bytes = self.bytes.max(bytes);
        self.components = self.components.max(components);
        Ok(())
    }

    fn validate_at(self, prefix: &str) -> Result<(), SnapshotError> {
        validate_scope_relative_path(prefix)?;
        let (bytes, components) = if prefix == "/" {
            (0, 0)
        } else {
            (prefix.len(), prefix[1..].split('/').count())
        };
        bytes
            .checked_add(self.bytes)
            .filter(|value| *value <= 4096)
            .ok_or_else(path_limit)?;
        components
            .checked_add(self.components)
            .filter(|value| *value <= 256)
            .ok_or_else(path_limit)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct DirectorySummary {
    page_id: MetadataPageId,
    bounds: PathBounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlobFact {
    size: u64,
    digest: [u8; 32],
}

#[derive(Default)]
struct ClosureUpper {
    nodes: usize,
    edges: usize,
    bytes: u64,
    entries: usize,
}

impl ClosureUpper {
    fn add(&mut self, directory: &CertifiedReusableDirectory) -> Result<(), SnapshotError> {
        self.nodes = self
            .nodes
            .checked_add(directory.closure_nodes_upper)
            .ok_or_else(budget_limit)?;
        self.edges = self
            .edges
            .checked_add(directory.closure_edges_upper)
            .ok_or_else(budget_limit)?;
        self.bytes = self
            .bytes
            .checked_add(directory.closure_bytes_upper)
            .ok_or_else(budget_limit)?;
        self.entries = self
            .entries
            .checked_add(directory.closure_entries_upper)
            .ok_or_else(budget_limit)?;
        Ok(())
    }
}

struct ProjectionState {
    identity: MetadataInstallIdentity,
    required_trees: BTreeSet<String>,
    active_trees: BTreeSet<String>,
    directories: BTreeMap<String, DirectorySummary>,
    bounds: BTreeMap<MetadataPageId, PathBounds>,
    payloads: BTreeMap<MetadataPageId, MetadataPagePayload>,
    page_entries: BTreeMap<MetadataPageId, usize>,
    edges: BTreeSet<(MetadataPageId, MetadataPageId)>,
    reused: BTreeMap<MetadataPageId, CertifiedReusableDirectory>,
    source_roots: BTreeMap<String, MetadataPageId>,
    blob_facts: BTreeMap<String, BlobFact>,
    source_entries: usize,
    source_encoded_bytes: u64,
    codec_entry_visits: usize,
    payload_bytes: u64,
    delta_entries: usize,
    reuse_upper: ClosureUpper,
    work: RootedProjectionWork,
}

impl ProjectionState {
    fn new(identity: MetadataInstallIdentity) -> Self {
        Self {
            identity,
            required_trees: BTreeSet::new(),
            active_trees: BTreeSet::new(),
            directories: BTreeMap::new(),
            bounds: BTreeMap::new(),
            payloads: BTreeMap::new(),
            page_entries: BTreeMap::new(),
            edges: BTreeSet::new(),
            reused: BTreeMap::new(),
            source_roots: BTreeMap::new(),
            blob_facts: BTreeMap::new(),
            source_entries: 0,
            source_encoded_bytes: 0,
            codec_entry_visits: 0,
            payload_bytes: 0,
            delta_entries: 0,
            reuse_upper: ClosureUpper::default(),
            work: RootedProjectionWork::default(),
        }
    }

    fn require_tree(&mut self, tree_oid: &str) -> Result<(), SnapshotError> {
        if !self.required_trees.contains(tree_oid) {
            if self.required_trees.len() >= MetadataDagLimits::default().nodes {
                return Err(budget_limit());
            }
            self.required_trees.insert(tree_oid.to_owned());
        }
        Ok(())
    }

    fn admit_entries(&mut self, entries: &[(String, FsKind, String)]) -> Result<(), SnapshotError> {
        let limits = MetadataDagLimits::default();
        self.source_encoded_bytes = self
            .source_encoded_bytes
            .checked_add(HEADER_LEN as u64)
            .filter(|value| *value <= limits.payload_bytes)
            .ok_or_else(budget_limit)?;
        let mut previous: Option<&str> = None;
        for (name, kind, _) in entries {
            if name.is_empty()
                || name.len() > 255
                || name == "."
                || name == ".."
                || name.contains('/')
                || name.contains('\0')
                || previous.is_some_and(|value| value >= name.as_str())
            {
                return Err(integrity(
                    "source tree has an invalid or duplicate UTF-8 name",
                ));
            }
            previous = Some(name);
            let value_bytes = if *kind == FsKind::Directory { 32 } else { 40 };
            self.source_encoded_bytes = self
                .source_encoded_bytes
                .checked_add((3 + name.len() + value_bytes) as u64)
                .filter(|value| *value <= limits.payload_bytes)
                .ok_or_else(budget_limit)?;
        }
        Ok(())
    }

    fn record_bounds(
        &mut self,
        page: MetadataPageId,
        bounds: PathBounds,
    ) -> Result<(), SnapshotError> {
        if self
            .bounds
            .get(&page)
            .is_some_and(|previous| previous != &bounds)
        {
            return Err(integrity(
                "one canonical directory has inconsistent descendant path bounds",
            ));
        }
        self.bounds.insert(page, bounds);
        Ok(())
    }

    fn record_reuse(
        &mut self,
        tree_oid: &str,
        directory: CertifiedReusableDirectory,
        path: &str,
    ) -> Result<DirectorySummary, SnapshotError> {
        let limits = MetadataDagLimits::default();
        if directory.page_id == [0; 32]
            || directory.proof.generation <= 0
            || directory.closure_nodes_upper == 0
            || directory.closure_nodes_upper > limits.nodes
            || directory.closure_edges_upper > limits.edges
            || !(HEADER_LEN as u64..=limits.payload_bytes).contains(&directory.closure_bytes_upper)
            || directory.closure_entries_upper > limits.entries
            || directory.relative_path_bytes > 4096
            || directory.relative_components > 256
        {
            return Err(integrity(
                "reused directory hint has an invalid certificate summary",
            ));
        }
        let summary = DirectorySummary {
            page_id: directory.page_id,
            bounds: PathBounds {
                bytes: directory.relative_path_bytes,
                components: directory.relative_components,
            },
        };
        summary.bounds.validate_at(path)?;
        self.record_bounds(directory.page_id, summary.bounds)?;
        if !self.payloads.contains_key(&directory.page_id) {
            if let Some(previous) = self.reused.get(&directory.page_id) {
                if previous.proof.generation != directory.proof.generation
                    || previous.proof.certificate_digest != directory.proof.certificate_digest
                    || previous.closure_nodes_upper != directory.closure_nodes_upper
                    || previous.closure_edges_upper != directory.closure_edges_upper
                    || previous.closure_bytes_upper != directory.closure_bytes_upper
                    || previous.closure_entries_upper != directory.closure_entries_upper
                {
                    return Err(integrity(
                        "one reused boundary has inconsistent lifetime or certificate summaries",
                    ));
                }
            } else {
                self.reuse_upper.add(&directory)?;
                self.reused.insert(directory.page_id, directory);
                self.check_budget()?;
            }
        }
        self.source_roots
            .insert(tree_oid.to_owned(), summary.page_id);
        Ok(summary)
    }

    fn charge_codec(&mut self, entries: usize, route_length: usize) -> Result<(), SnapshotError> {
        let count = entries
            .checked_mul(route_length.checked_add(1).ok_or_else(budget_limit)?)
            .ok_or_else(budget_limit)?;
        self.codec_entry_visits = self
            .codec_entry_visits
            .checked_add(count)
            .filter(|value| *value <= MetadataDagLimits::default().prepare_entry_visits)
            .ok_or_else(budget_limit)?;
        self.work.codec_input_entries = self
            .work
            .codec_input_entries
            .checked_add(entries as u64)
            .ok_or_else(budget_limit)?;
        Ok(())
    }

    fn add_edge(
        &mut self,
        parent: MetadataPageId,
        child: MetadataPageId,
    ) -> Result<(), SnapshotError> {
        if self.edges.insert((parent, child)) {
            self.check_budget()?;
        }
        Ok(())
    }

    fn collect_directory(
        &mut self,
        entries: &[Entry],
        root_bytes: Vec<u8>,
    ) -> Result<MetadataPageId, SnapshotError> {
        let root = page_id(&root_bytes);
        let mut routes = VecDeque::from([(Vec::<u8>::new(), root, Some(root_bytes))]);
        while let Some((route, expected, provided)) = routes.pop_front() {
            if self.payloads.contains_key(&expected) || self.reused.contains_key(&expected) {
                continue;
            }
            let bytes = if let Some(bytes) = provided {
                bytes
            } else {
                self.charge_codec(entries.len(), route.len())?;
                self.work.radix_route_builds += 1;
                Page::pages_along_route(entries, &route)
                    .map_err(codec_error)?
                    .pop()
                    .ok_or_else(|| integrity("canonical radix route returned no page"))?
            };
            if page_id(&bytes) != expected || !(HEADER_LEN..=PAGE_MAX_BYTES).contains(&bytes.len())
            {
                return Err(integrity(
                    "canonical radix page differs from its parent binding",
                ));
            }
            let (page, _) = Page::decode(&bytes).map_err(codec_error)?;
            let page_entries = match page {
                Page::Leaf { entries } => {
                    for entry in entries.iter().filter(|entry| entry.is_dir()) {
                        self.add_edge(expected, entry.child_root)?;
                    }
                    entries.len()
                }
                Page::Branch {
                    terminal, children, ..
                } => {
                    if let Some(entry) = terminal.as_ref().filter(|entry| entry.is_dir()) {
                        self.add_edge(expected, entry.child_root)?;
                    }
                    for child in children {
                        self.add_edge(expected, child.child_page_id)?;
                        let mut child_route = route.clone();
                        child_route.push(child.label);
                        routes.push_back((child_route, child.child_page_id, None));
                    }
                    usize::from(terminal.is_some())
                }
            };
            self.payload_bytes = self
                .payload_bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(budget_limit)?;
            self.delta_entries = self
                .delta_entries
                .checked_add(page_entries)
                .ok_or_else(budget_limit)?;
            self.page_entries.insert(expected, page_entries);
            self.payloads.insert(
                expected,
                MetadataPagePayload {
                    id: expected,
                    size: bytes.len() as u64,
                    bytes,
                },
            );
            self.check_budget()?;
        }
        Ok(root)
    }

    fn check_budget(&self) -> Result<(), SnapshotError> {
        let limits = MetadataDagLimits::default();
        if self
            .payloads
            .len()
            .checked_add(self.reuse_upper.nodes)
            .is_none_or(|value| value > limits.nodes)
            || self
                .edges
                .len()
                .checked_add(self.reuse_upper.edges)
                .is_none_or(|value| value > limits.edges)
            || self
                .payload_bytes
                .checked_add(self.reuse_upper.bytes)
                .is_none_or(|value| value > limits.payload_bytes)
            || self
                .delta_entries
                .checked_add(self.reuse_upper.entries)
                .is_none_or(|value| value > limits.entries)
        {
            return Err(budget_limit());
        }
        Ok(())
    }

    fn finish(
        mut self,
        root: MetadataPageId,
    ) -> Result<PreparedRootedNativeMetadata, SnapshotError> {
        let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for &(parent, child) in &self.edges {
            children.entry(parent).or_default().push(child);
        }
        let mut reachable = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(page) = pending.pop() {
            if reachable.insert(page) && !self.reused.contains_key(&page) {
                pending.extend(children.get(&page).into_iter().flatten());
            }
        }
        self.payloads.retain(|page, _| reachable.contains(page));
        self.page_entries.retain(|page, _| reachable.contains(page));
        self.reused.retain(|page, _| reachable.contains(page));
        self.source_roots.retain(|_, page| reachable.contains(page));
        self.edges
            .retain(|(parent, child)| reachable.contains(parent) && reachable.contains(child));
        self.payload_bytes = self
            .payloads
            .values()
            .try_fold(0u64, |total, page| total.checked_add(page.size))
            .ok_or_else(budget_limit)?;
        self.delta_entries = self
            .page_entries
            .values()
            .try_fold(0usize, |total, count| total.checked_add(*count))
            .ok_or_else(budget_limit)?;
        self.reuse_upper = ClosureUpper::default();
        for directory in self.reused.values() {
            self.reuse_upper.add(directory)?;
        }
        self.check_budget()?;
        let plan = RootedMetadataInstallPlan::new(
            self.identity,
            root,
            self.payloads
                .iter()
                .map(|(page, payload)| (*page, payload.size))
                .collect(),
            self.edges,
            self.reused
                .into_iter()
                .map(|(page, directory)| (page, directory.proof))
                .collect(),
            self.source_roots,
        )?;
        self.work.delta_pages = plan.delta.len() as u64;
        self.work.delta_bytes = self.payload_bytes;
        self.work.reused_roots = plan.reused.len() as u64;
        self.work.reused_closure_nodes_upper = self.reuse_upper.nodes as u64;
        self.work.reused_closure_edges_upper = self.reuse_upper.edges as u64;
        self.work.reused_closure_bytes_upper = self.reuse_upper.bytes;
        self.work.reused_closure_entries_upper = self.reuse_upper.entries as u64;
        self.work.plan_bytes = plan.encode()?.len() as u64;
        Ok(PreparedRootedNativeMetadata {
            plan,
            payloads: self.payloads.into_values().collect(),
            work: self.work,
        })
    }
}

async fn resolve_scope_oid<T: ApiHandler + ?Sized>(
    handler: &T,
    root: &Tree,
    scope: &str,
    work: &mut RootedProjectionWork,
) -> Result<ObjectHash, SnapshotError> {
    if scope == "/" {
        return Ok(root.id);
    }
    let mut current = Cow::Borrowed(root);
    let mut components = scope[1..].split('/').peekable();
    while let Some(component) = components.next() {
        let position = current
            .tree_items
            .iter()
            .position(|item| item.name == component);
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
                "rooted metadata scope is not a directory",
            ));
        }
        let expected = item.id;
        if expected.kind() != root.id.kind() {
            return Err(integrity("scope tree crossed its source hash kind"));
        }
        if components.peek().is_none() {
            return Ok(expected);
        }
        work.tree_fetches += 1;
        let fetched = handler
            .get_tree_by_hash(&expected.to_string())
            .await
            .map_err(source_error)?;
        if fetched.id != expected {
            return Err(integrity("fetched scope ancestor identity mismatch"));
        }
        current = Cow::Owned(fetched);
    }
    Err(integrity("rooted metadata scope walk has no target"))
}

async fn project_directory<T: ApiHandler + ?Sized, R: RootedReuseLookup + ?Sized>(
    handler: &T,
    reuse: &R,
    storage: &Storage,
    oid: ObjectHash,
    loaded: Option<&Tree>,
    path: &str,
    state: &mut ProjectionState,
) -> Result<DirectorySummary, SnapshotError> {
    validate_scope_relative_path(path)?;
    let tagged = oid.to_tagged_string();
    if let Some(summary) = state.directories.get(&tagged).copied() {
        summary.bounds.validate_at(path)?;
        state.work.directory_memo_hits += 1;
        return Ok(summary);
    }
    state.require_tree(&tagged)?;
    if !state.active_trees.insert(tagged.clone()) {
        return Err(integrity("native source trees contain a cycle"));
    }
    state.work.reuse_lookups += 1;
    if let Some(hint) = reuse.lookup_reuse(&tagged, &state.identity).await? {
        let summary = state.record_reuse(&tagged, hint, path)?;
        state.active_trees.remove(&tagged);
        state.directories.insert(tagged, summary);
        return Ok(summary);
    }
    let fetched;
    let tree = if let Some(tree) = loaded {
        tree
    } else {
        state.work.tree_fetches += 1;
        fetched = handler
            .get_tree_by_hash(&oid.to_string())
            .await
            .map_err(source_error)?;
        &fetched
    };
    if tree.id != oid
        || tree
            .tree_items
            .iter()
            .any(|item| item.id.kind() != oid.kind())
    {
        return Err(integrity(
            "fetched source directory identity or child hash kind mismatch",
        ));
    }
    state.source_entries = state
        .source_entries
        .checked_add(tree.tree_items.len())
        .filter(|value| *value <= MetadataDagLimits::default().entries)
        .ok_or_else(budget_limit)?;
    let direct = direct_entries(tree)?;
    state.admit_entries(&direct)?;
    state.work.directories_scanned += 1;
    state.work.direct_entries_scanned += direct.len() as u64;
    let requested: Vec<_> = direct
        .iter()
        .filter(|(_, kind, oid)| *kind != FsKind::Directory && !state.blob_facts.contains_key(oid))
        .map(|(_, _, oid)| oid.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for batch in requested.chunks(64) {
        state.work.verified_blob_queries += 1;
        let facts = storage
            .mono_storage()
            .get_verified_blobs(batch.to_vec())
            .await
            .map_err(source_error)?;
        state.work.verified_blob_facts_loaded += facts.len() as u64;
        for (oid, fact) in facts {
            state.blob_facts.insert(oid, verified_fact(&fact)?);
        }
    }
    let mut pending_facts = Vec::new();
    let mut bounds = PathBounds::default();
    let mut entries = Vec::with_capacity(direct.len());
    for (name, kind, raw_oid) in direct {
        let child_path = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        validate_scope_relative_path(&child_path)?;
        match kind {
            FsKind::Directory => {
                let child_oid = ObjectHash::from_hex_for_kind(oid.kind(), &raw_oid)
                    .map_err(|error| integrity(&error.to_string()))?;
                let child = Box::pin(project_directory(
                    handler,
                    reuse,
                    storage,
                    child_oid,
                    None,
                    &child_path,
                    state,
                ))
                .await?;
                bounds.include(&name, child.bounds)?;
                entries.push(Entry::dir(name.as_bytes(), child.page_id));
            }
            FsKind::Regular | FsKind::Executable | FsKind::Symlink => {
                bounds.include(&name, PathBounds::default())?;
                let fact = if let Some(fact) = state.blob_facts.get(&raw_oid).copied() {
                    fact
                } else {
                    let fact = stream_blob_fact_with_resources(
                        || handler.get_raw_blob_stream_with_meta(&raw_oid),
                        projection_budget(),
                        &mut state.work,
                    )
                    .await?;
                    state.blob_facts.insert(raw_oid.clone(), fact);
                    pending_facts.push((raw_oid.clone(), fact));
                    if pending_facts.len() == 64 {
                        persist_facts(storage, &pending_facts, &mut state.work).await?;
                        pending_facts.clear();
                    }
                    fact
                };
                let entry_kind = match kind {
                    FsKind::Regular => EntryKind::Regular,
                    FsKind::Executable => EntryKind::Executable,
                    FsKind::Symlink => EntryKind::Symlink,
                    FsKind::Directory => {
                        return Err(integrity("file projection received a directory kind"));
                    }
                };
                entries.push(Entry::file(
                    entry_kind,
                    name.as_bytes(),
                    fact.size,
                    fact.digest,
                ));
            }
        }
    }
    if !pending_facts.is_empty() {
        persist_facts(storage, &pending_facts, &mut state.work).await?;
    }
    bounds.validate_at(path)?;
    state.charge_codec(entries.len(), 0)?;
    state.work.directory_root_builds += 1;
    let root_bytes = Page::build(&entries).map_err(codec_error)?;
    let page_id = state.collect_directory(&entries, root_bytes)?;
    state.record_bounds(page_id, bounds)?;
    state.source_roots.insert(tagged.clone(), page_id);
    let summary = DirectorySummary { page_id, bounds };
    state.active_trees.remove(&tagged);
    state.directories.insert(tagged, summary);
    Ok(summary)
}

async fn stream_blob_fact_with_resources<F, Fut>(
    open: F,
    budget: &Arc<MemoryBudget>,
    work: &mut RootedProjectionWork,
) -> Result<BlobFact, SnapshotError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(ObjectByteStream, ObjectMeta), MegaError>>,
{
    // Credit bounds consumer-visible processing, not the backend's backing
    // allocation or work surviving cancellation. No source bytes accumulate.
    let _lease = budget.reserve(RANGE_WORK_BYTES)?;
    work.blob_fetches = work.blob_fetches.checked_add(1).ok_or_else(budget_limit)?;
    let (mut input, meta) = tokio::time::timeout(SOURCE_PROGRESS_TIMEOUT, open())
        .await
        .map_err(|_| source_progress_timeout())?
        .map_err(source_error)?;
    let size = u64::try_from(meta.size)
        .ok()
        .filter(|size| *size <= MAX_VERIFIED_FILE_BYTES)
        .ok_or_else(|| integrity("raw file exceeds the native verified-object size profile"))?;
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut since_yield = 0usize;
    let mut empty_parts = 0usize;
    let mut progress_deadline = tokio::time::Instant::now() + SOURCE_PROGRESS_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= progress_deadline {
            return Err(source_progress_timeout());
        }
        let part = tokio::time::timeout_at(progress_deadline, input.next())
            .await
            .map_err(|_| source_progress_timeout())?;
        let Some(part) = part else { break };
        let bytes = part.map_err(|error| {
            tracing::warn!(error = %error, "rooted metadata raw source stream failed");
            SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "raw source stream failed",
            )
        })?;
        if bytes.len() as u64 > size - received {
            return Err(integrity(
                "raw source length disagrees with its physical size",
            ));
        }
        if bytes.len() > RANGE_WORK_BYTES {
            return Err(SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "raw source producer item exceeds the consumer processing limit",
            ));
        }
        digest.update(&bytes);
        received += bytes.len() as u64;
        since_yield += bytes.len();
        if bytes.is_empty() {
            empty_parts += 1;
        } else {
            empty_parts = 0;
            progress_deadline = tokio::time::Instant::now() + SOURCE_PROGRESS_TIMEOUT;
        }
        if since_yield >= RANGE_WORK_BYTES || empty_parts == 32 {
            tokio::task::yield_now().await;
            since_yield = 0;
            empty_parts = 0;
        }
    }
    if received != size {
        return Err(integrity(
            "raw source length disagrees with its physical size",
        ));
    }
    let fetched = work
        .raw_bytes_fetched
        .checked_add(size)
        .ok_or_else(budget_limit)?;
    let hashed = work
        .raw_bytes_hashed
        .checked_add(size)
        .ok_or_else(budget_limit)?;
    work.raw_bytes_fetched = fetched;
    work.raw_bytes_hashed = hashed;
    Ok(BlobFact {
        size,
        digest: digest.finalize().into(),
    })
}

fn source_progress_timeout() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::TemporaryUnavailable,
        "rooted metadata raw source made no progress",
    )
}

fn verified_fact(
    fact: &crate::callisto::mst2_verified_object::Model,
) -> Result<BlobFact, SnapshotError> {
    if fact.state != "VERIFIED"
        || fact.verification_version != MST2_VERIFICATION_VERSION
        || !(0..=MAX_VERIFIED_FILE_BYTES as i64).contains(&fact.size)
    {
        return Err(integrity(
            "native projection received an invalid current verified blob fact",
        ));
    }
    Ok(BlobFact {
        size: u64::try_from(fact.size).map_err(|_| integrity("verified blob size is invalid"))?,
        digest: fact
            .raw_sha256
            .as_slice()
            .try_into()
            .map_err(|_| integrity("verified blob digest is invalid"))?,
    })
}

async fn persist_facts(
    storage: &Storage,
    facts: &[(String, BlobFact)],
    work: &mut RootedProjectionWork,
) -> Result<(), SnapshotError> {
    let rows = facts
        .iter()
        .map(
            |(oid, fact)| crate::callisto::mst2_verified_object::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                storage_domain: Set("git".into()),
                git_oid: Set(oid.clone()),
                object_kind: Set("blob".into()),
                raw_sha256: Set(fact.digest.to_vec()),
                size: Set(fact.size as i64),
                verification_version: Set(MST2_VERIFICATION_VERSION),
                state: Set("VERIFIED".into()),
                created_at: Set(chrono::Utc::now().fixed_offset()),
            },
        )
        .collect();
    work.verified_blob_persistence_batches += 1;
    storage
        .mono_storage()
        .insert_verified_blobs(rows)
        .await
        .map_err(source_error)?;
    work.verified_blob_queries += 1;
    let stored = storage
        .mono_storage()
        .get_verified_blobs(facts.iter().map(|(oid, _)| oid.clone()).collect())
        .await
        .map_err(source_error)?;
    work.verified_blob_facts_loaded += stored.len() as u64;
    for (oid, expected) in facts {
        let actual = stored
            .get(oid)
            .ok_or_else(|| integrity("verified blob persistence lost its current fact"))?;
        if verified_fact(actual)? != *expected {
            return Err(integrity(
                "verified blob persistence conflicts with the hashed raw source",
            ));
        }
    }
    Ok(())
}

fn source_error(error: MegaError) -> SnapshotError {
    let code = match error {
        MegaError::ObjStorageNotFound(_) => SnapshotErrorCode::ObjectUnavailable,
        MegaError::ObjStorageInconsistent(_) => SnapshotErrorCode::IntegrityError,
        _ => SnapshotErrorCode::Internal,
    };
    tracing::warn!(error = %error, "rooted native metadata source operation failed");
    SnapshotError::new(code, "rooted native metadata source operation failed")
}

fn codec_error(error: mst2_codec::CodecError) -> SnapshotError {
    integrity(&error.to_string())
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn budget_limit() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::LimitExceeded,
        "rooted metadata projection exceeds its fixed budget",
    )
}
fn path_limit() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::ScopeInvalid,
        "rooted metadata subtree exceeds its full-path budget",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
    };

    use bytes::Bytes;
    use git_internal::hash::HashKind;
    use tokio::sync::Notify;
    use uuid::Uuid;

    use super::*;

    struct FactInput {
        parts: VecDeque<std::io::Result<Bytes>>,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        hold_eof: Option<Arc<Notify>>,
    }

    impl futures::Stream for FactInput {
        type Item = std::io::Result<Bytes>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let input = self.get_mut();
            input.polls.fetch_add(1, Ordering::SeqCst);
            if let Some(part) = input.parts.pop_front() {
                Poll::Ready(Some(part))
            } else if let Some(entered) = &input.hold_eof {
                entered.notify_one();
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }
    }

    impl Drop for FactInput {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fact_input(
        parts: Vec<std::io::Result<Bytes>>,
        hold_eof: Option<Arc<Notify>>,
    ) -> (ObjectByteStream, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        (
            Box::pin(FactInput {
                parts: parts.into(),
                polls: polls.clone(),
                drops: drops.clone(),
                hold_eof,
            }),
            polls,
            drops,
        )
    }

    #[tokio::test]
    async fn streamed_cold_fact_keeps_large_memory_source_and_empty_raw_costs_exact() {
        use crate::orbit_api::object_storage::{ObjectKey, ObjectNamespace};

        for size in [0, RANGE_WORK_BYTES + 113] {
            let backend = crate::jupiter::storage::object_storage::mock_object_storage();
            let key = ObjectKey {
                namespace: ObjectNamespace::Git,
                key: "ab".repeat(20),
            };
            let mut raw = vec![0xff; size];
            if !raw.is_empty() {
                raw[..11].copy_from_slice(b"blob 3\0abc\0");
            }
            let expected: [u8; 32] = Sha256::digest(&raw).into();
            backend
                .inner
                .put_stream(
                    &key,
                    Box::pin(futures::stream::iter([Ok(Bytes::from(raw))])),
                    ObjectMeta::default(),
                )
                .await
                .unwrap();
            let budget = MemoryBudget::new(RANGE_WORK_BYTES);
            let opens = AtomicUsize::new(0);
            let mut work = RootedProjectionWork::default();
            let fact = stream_blob_fact_with_resources(
                || async {
                    opens.fetch_add(1, Ordering::SeqCst);
                    backend
                        .inner
                        .get_stream(&key)
                        .await
                        .map_err(MegaError::from)
                },
                &budget,
                &mut work,
            )
            .await
            .unwrap();
            assert_eq!(
                fact,
                BlobFact {
                    size: size as u64,
                    digest: expected
                }
            );
            assert_eq!(opens.load(Ordering::SeqCst), 1);
            assert_eq!(work.blob_fetches, 1);
            assert_eq!(work.raw_bytes_fetched, size as u64);
            assert_eq!(work.raw_bytes_hashed, size as u64);
            assert_eq!(work.verified_blob_persistence_batches, 0);
            assert_eq!(budget.used(), 0);
        }
    }

    #[tokio::test]
    async fn streamed_cold_fact_rejects_physical_mismatch_and_late_error_before_fact_return() {
        let raw = Bytes::from_static(b"raw");
        for (size, parts, code, polls_expected) in [
            (
                -1,
                vec![Ok(raw.clone())],
                SnapshotErrorCode::IntegrityError,
                0,
            ),
            (
                MAX_VERIFIED_FILE_BYTES as i64 + 1,
                vec![Ok(raw.clone())],
                SnapshotErrorCode::IntegrityError,
                0,
            ),
            (
                MAX_VERIFIED_FILE_BYTES as i64,
                vec![],
                SnapshotErrorCode::IntegrityError,
                1,
            ),
            (
                RANGE_WORK_BYTES as i64 + 1,
                vec![Ok(Bytes::from(vec![7; RANGE_WORK_BYTES + 1]))],
                SnapshotErrorCode::TemporaryUnavailable,
                1,
            ),
            (
                2,
                vec![Ok(raw.clone())],
                SnapshotErrorCode::IntegrityError,
                1,
            ),
            (
                4,
                vec![Ok(raw.clone())],
                SnapshotErrorCode::IntegrityError,
                2,
            ),
            (
                3,
                vec![Ok(raw.clone()), Ok(Bytes::from_static(b"x"))],
                SnapshotErrorCode::IntegrityError,
                2,
            ),
            (
                3,
                vec![Ok(raw), Err(std::io::Error::other("late source error"))],
                SnapshotErrorCode::ObjectUnavailable,
                2,
            ),
            (
                0,
                vec![Err(std::io::Error::other("empty source error"))],
                SnapshotErrorCode::ObjectUnavailable,
                1,
            ),
        ] {
            let (input, polls, drops) = fact_input(parts, None);
            let budget = MemoryBudget::new(RANGE_WORK_BYTES);
            let mut work = RootedProjectionWork::default();
            let error = stream_blob_fact_with_resources(
                || async move {
                    Ok((
                        input,
                        ObjectMeta {
                            size,
                            ..Default::default()
                        },
                    ))
                },
                &budget,
                &mut work,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(polls.load(Ordering::SeqCst), polls_expected);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(work.blob_fetches, 1);
            assert_eq!(work.raw_bytes_fetched, 0);
            assert_eq!(work.raw_bytes_hashed, 0);
            assert_eq!(budget.used(), 0);
        }
    }

    #[tokio::test]
    async fn streamed_cold_fact_cancellation_at_exact_size_drops_source_and_refunds_credit() {
        let entered = Arc::new(Notify::new());
        let (input, polls, drops) =
            fact_input(vec![Ok(Bytes::from_static(b"raw"))], Some(entered.clone()));
        let budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let task_budget = budget.clone();
        let task = tokio::spawn(async move {
            let mut work = RootedProjectionWork::default();
            stream_blob_fact_with_resources(
                || async move {
                    Ok((
                        input,
                        ObjectMeta {
                            size: 3,
                            ..Default::default()
                        },
                    ))
                },
                &task_budget,
                &mut work,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(budget.used(), RANGE_WORK_BYTES);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn streamed_cold_fact_reserves_fixed_credit_before_open_and_preserves_open_errors() {
        let budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let occupied = budget.reserve(RANGE_WORK_BYTES).unwrap();
        let mut work = RootedProjectionWork::default();
        let opens = AtomicUsize::new(0);
        let error = stream_blob_fact_with_resources(
            || async {
                opens.fetch_add(1, Ordering::SeqCst);
                Err(MegaError::Other("unexpected open".into()))
            },
            &budget,
            &mut work,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, SnapshotErrorCode::TemporaryUnavailable);
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(work, RootedProjectionWork::default());
        drop(occupied);
        for (error, code) in [
            (
                MegaError::ObjStorageNotFound("missing".into()),
                SnapshotErrorCode::ObjectUnavailable,
            ),
            (
                MegaError::ObjStorageInconsistent("inconsistent".into()),
                SnapshotErrorCode::IntegrityError,
            ),
            (
                MegaError::Other("transport".into()),
                SnapshotErrorCode::Internal,
            ),
        ] {
            let mut work = RootedProjectionWork::default();
            let got = stream_blob_fact_with_resources(|| async { Err(error) }, &budget, &mut work)
                .await
                .unwrap_err();
            assert_eq!(got.code, code);
            assert_eq!(work.blob_fetches, 1);
            assert_eq!(work.raw_bytes_fetched, 0);
            assert_eq!(work.raw_bytes_hashed, 0);
            assert_eq!(budget.used(), 0);
        }
    }

    fn state() -> ProjectionState {
        ProjectionState::new(MetadataInstallIdentity {
            source_domain: "native-git".into(),
            tagged_root_tree_oid: ObjectHash::from_hex_for_kind(HashKind::Sha1, &"a".repeat(40))
                .unwrap()
                .to_tagged_string(),
            scope: "/".into(),
            schema_version: SCHEMA_VERSION,
            metadata_codec: METADATA_CODEC,
            materialization_policy: MATERIALIZATION_POLICY_GIT_RAW_V1,
            fs_semantics: FS_SEMANTICS_LINUX_CODE_V1,
            access_projection: ACCESS_PROJECTION_EXACT_FULL,
            verification_revision: MST2_VERIFICATION_VERSION,
            projection_revision: NATIVE_PROJECTION_REVISION,
        })
    }

    fn reusable(page: MetadataPageId) -> CertifiedReusableDirectory {
        CertifiedReusableDirectory {
            page_id: page,
            proof: RootedReuseRoot {
                generation: 1,
                attestation_id: Uuid::from_u128(1),
                attestation_digest: [1; 32],
                certificate_digest: [2; 32],
            },
            relative_path_bytes: 0,
            relative_components: 0,
            closure_nodes_upper: 1,
            closure_edges_upper: 0,
            closure_bytes_upper: HEADER_LEN as u64,
            closure_entries_upper: 0,
        }
    }

    #[test]
    fn reused_path_bounds_recheck_moved_aliases_and_checked_overflow() {
        let bounds = PathBounds {
            bytes: 4093,
            components: 255,
        };
        assert!(bounds.validate_at("/").is_ok());
        assert!(bounds.validate_at("/a").is_ok());
        assert_eq!(
            bounds.validate_at("/a/b").unwrap_err().code,
            SnapshotErrorCode::ScopeInvalid
        );
        assert!(
            PathBounds {
                bytes: usize::MAX,
                components: 0
            }
            .validate_at("/a")
            .is_err()
        );
        let mut parent = PathBounds::default();
        parent
            .include(
                "a",
                PathBounds {
                    bytes: 3,
                    components: 1,
                },
            )
            .unwrap();
        parent.include("longer", PathBounds::default()).unwrap();
        assert_eq!(
            parent,
            PathBounds {
                bytes: 7,
                components: 2
            }
        );
        assert!(
            parent
                .include(
                    "a",
                    PathBounds {
                        bytes: usize::MAX,
                        components: 0
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn canonical_radix_payloads_and_shared_reuse_edges_form_a_delta_only_plan() {
        let empty = Page::build(&[]).unwrap();
        let reused_page = page_id(&empty);
        let mut state = state();
        state
            .record_reuse(
                &format!("sha1:{}", "b".repeat(40)),
                reusable(reused_page),
                "/left",
            )
            .unwrap();
        let mut entries: Vec<_> = (0..260)
            .map(|index| {
                Entry::file(
                    EntryKind::Regular,
                    format!("file-{index:03}").as_bytes(),
                    index,
                    [3; 32],
                )
            })
            .collect();
        entries.push(Entry::dir(b"left", reused_page));
        entries.push(Entry::dir(b"right", reused_page));
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        let bytes = Page::build(&entries).unwrap();
        let root = state.collect_directory(&entries, bytes).unwrap();
        state
            .source_roots
            .insert(state.identity.tagged_root_tree_oid.clone(), root);
        let prepared = state.finish(root).unwrap();
        assert!(prepared.payloads.len() > 1);
        assert_eq!(prepared.plan.reused.len(), 1);
        assert!(!prepared.plan.delta.contains_key(&reused_page));
        assert_eq!(prepared.work.tree_fetches, 0);
        assert_eq!(prepared.work.blob_fetches, 0);
        assert_eq!(
            prepared.plan.child_first_delta().unwrap().last(),
            Some(&root)
        );
        let mut decoded_edges = BTreeSet::new();
        for payload in &prepared.payloads {
            assert_eq!(page_id(&payload.bytes), payload.id);
            match Page::decode(&payload.bytes).unwrap().0 {
                Page::Leaf { entries } => {
                    for entry in entries.iter().filter(|entry| entry.is_dir()) {
                        decoded_edges.insert((payload.id, entry.child_root));
                    }
                }
                Page::Branch {
                    terminal, children, ..
                } => {
                    if let Some(entry) = terminal.filter(Entry::is_dir) {
                        decoded_edges.insert((payload.id, entry.child_root));
                    }
                    for child in children {
                        decoded_edges.insert((payload.id, child.child_page_id));
                    }
                }
            }
        }
        assert_eq!(decoded_edges, prepared.plan.edges);
        assert_eq!(
            RootedMetadataInstallPlan::decode(
                &prepared.plan.encode().unwrap(),
                &prepared.plan.digest().unwrap()
            )
            .unwrap(),
            prepared.plan
        );
    }

    #[test]
    fn reuse_budget_is_deduplicated_conservative_and_never_expands_descendants() {
        let empty = Page::build(&[]).unwrap();
        let page = page_id(&empty);
        let hint = reusable(page);
        let mut state = state();
        state
            .record_reuse(&format!("sha1:{}", "b".repeat(40)), hint.clone(), "/one")
            .unwrap();
        let mut alias = hint.clone();
        alias.proof.attestation_id = Uuid::from_u128(2);
        alias.proof.attestation_digest = [9; 32];
        state
            .record_reuse(&format!("sha1:{}", "c".repeat(40)), alias, "/two")
            .unwrap();
        assert_eq!(state.reuse_upper.nodes, 1);
        assert_eq!(state.reuse_upper.bytes, HEADER_LEN as u64);
        assert_eq!(state.reused.get(&page).unwrap().proof, hint.proof);
        assert!(state.payloads.is_empty());
        assert!(state.edges.is_empty());
        let mut different_lifetime = hint.clone();
        different_lifetime.proof.generation = 2;
        assert!(
            state
                .record_reuse(
                    &format!("sha1:{}", "f".repeat(40)),
                    different_lifetime,
                    "/lifetime"
                )
                .is_err()
        );
        let mut bad = hint;
        bad.relative_path_bytes = 1;
        assert!(
            state
                .record_reuse(&format!("sha1:{}", "d".repeat(40)), bad, "/three")
                .is_err()
        );
        let mut huge = reusable([4; 32]);
        huge.closure_nodes_upper = MetadataDagLimits::default().nodes;
        assert_eq!(
            state
                .record_reuse(&format!("sha1:{}", "e".repeat(40)), huge, "/four")
                .unwrap_err()
                .code,
            SnapshotErrorCode::LimitExceeded
        );
    }
}
