//! Bounded canonical native metadata closure, ready for future durable storage.
//! This module retains no leases, persists no bytes and starts no collector.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use mst2_codec::{
    descriptor::METADATA_CODEC,
    metapage::{Entry, Page, page_id},
};

use crate::ceres::snapshot::{
    error::{SnapshotError, SnapshotErrorCode},
    retention::{NodeState, RetainedKind, RetentionEdge, RetentionNode},
};

pub type MetadataPageId = [u8; 32];

#[derive(Debug, Clone, Copy)]
pub struct MetadataDagLimits {
    pub nodes: usize,
    pub edges: usize,
    pub payload_bytes: u64,
    pub entries: usize,
    pub prepare_entry_visits: usize,
}

impl Default for MetadataDagLimits {
    fn default() -> Self {
        Self {
            nodes: 4096,
            edges: 16_384,
            payload_bytes: 64 * 1024 * 1024,
            entries: 131_072,
            prepare_entry_visits: 64 * 1024 * 1024,
        }
    }
}

impl MetadataDagLimits {
    /// Caller budgets may tighten, never widen, the absolute group ceilings.
    pub(crate) fn effective(self) -> Self {
        let hard = Self::default();
        Self {
            nodes: self.nodes.min(hard.nodes),
            edges: self.edges.min(hard.edges),
            payload_bytes: self.payload_bytes.min(hard.payload_bytes),
            entries: self.entries.min(hard.entries),
            prepare_entry_visits: self.prepare_entry_visits.min(hard.prepare_entry_visits),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetadataPagePayload {
    pub id: MetadataPageId,
    pub size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MetadataDagCandidate {
    pub metadata_codec: u16,
    pub root: MetadataPageId,
    pub pages: Vec<MetadataPagePayload>,
    pub edges: Vec<(MetadataPageId, MetadataPageId)>,
}

/// Immutable validated output. Supply these slices to PostgreSQL retain_group
/// only after the payloads have been durably stored.
#[derive(Debug)]
pub struct ValidatedMetadataDag {
    root: MetadataPageId,
    pages: Vec<MetadataPagePayload>,
    nodes: Vec<RetentionNode>,
    edges: Vec<RetentionEdge>,
    payload_bytes: u64,
    entry_count: usize,
}

impl ValidatedMetadataDag {
    pub fn validate(
        candidate: MetadataDagCandidate,
        limits: MetadataDagLimits,
    ) -> Result<Self, SnapshotError> {
        let limits = limits.effective();
        if candidate.metadata_codec != METADATA_CODEC {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "native metadata DAG requires metadata codec 1",
            ));
        }
        check_count(candidate.pages.len(), limits.nodes, "metadata nodes")?;
        check_count(candidate.edges.len(), limits.edges, "metadata edges")?;
        let mut payload_bytes = 0u64;
        let mut entry_count = 0usize;
        let mut pages = BTreeMap::new();
        let mut decoded = BTreeMap::new();
        let mut expected_edges = BTreeSet::new();
        let mut directory_roots = BTreeSet::from([candidate.root]);
        for payload in candidate.pages {
            payload_bytes = payload_bytes
                .checked_add(payload.bytes.len() as u64)
                .filter(|total| *total <= limits.payload_bytes)
                .ok_or_else(|| limit("metadata payload budget exceeded"))?;
            if payload.size != payload.bytes.len() as u64 || page_id(&payload.bytes) != payload.id {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::DigestMismatch,
                    "metadata page digest or advertised size mismatch",
                ));
            }
            if pages.contains_key(&payload.id) {
                return Err(integrity("duplicate metadata page identity"));
            }
            let (page, subtree_entries) = Page::decode(&payload.bytes).map_err(codec_error)?;
            let entries: &[Entry] = match &page {
                Page::Leaf { entries } => entries,
                Page::Branch {
                    terminal, children, ..
                } => {
                    for child in children {
                        expected_edges.insert((payload.id, child.child_page_id));
                    }
                    terminal.as_slice()
                }
            };
            entry_count = entry_count
                .checked_add(entries.len())
                .filter(|count| *count <= limits.entries)
                .ok_or_else(|| limit("metadata entry budget exceeded"))?;
            for entry in entries.iter().filter(|entry| entry.is_dir()) {
                expected_edges.insert((payload.id, entry.child_root));
                directory_roots.insert(entry.child_root);
            }
            check_count(expected_edges.len(), limits.edges, "metadata edges")?;
            decoded.insert(payload.id, (page, subtree_entries));
            pages.insert(payload.id, payload);
        }
        let supplied_edges: BTreeSet<_> = candidate.edges.iter().copied().collect();
        if supplied_edges.len() != candidate.edges.len() {
            return Err(integrity("duplicate metadata edge"));
        }
        validate_graph(candidate.root, &pages, &supplied_edges)?;
        if supplied_edges != expected_edges {
            return Err(integrity(
                "metadata edges disagree with canonical page references",
            ));
        }
        for (page, _) in decoded.values() {
            if let Page::Branch { children, .. } = page {
                for child in children {
                    if decoded.get(&child.child_page_id).map(|(_, count)| *count)
                        != Some(child.subtree_entries)
                    {
                        return Err(integrity("metadata branch subtree count mismatch"));
                    }
                }
            }
        }
        let mut validation_visits = 0usize;
        for root in directory_roots {
            let advertised_entries = decoded
                .get(&root)
                .map(|(_, count)| *count)
                .ok_or_else(|| unavailable("metadata child is missing"))?;
            if advertised_entries > limits.entries as u64 {
                return Err(limit("metadata directory entry budget exceeded"));
            }
            let mut entries = Vec::new();
            let mut pending = vec![root];
            while let Some(id) = pending.pop() {
                let (page, _) = decoded
                    .get(&id)
                    .ok_or_else(|| unavailable("metadata child payload is missing"))?;
                let direct_entries = match page {
                    Page::Leaf { entries } => entries.as_slice(),
                    Page::Branch {
                        terminal, children, ..
                    } => {
                        pending.extend(children.iter().map(|child| child.child_page_id));
                        terminal.as_slice()
                    }
                };
                validation_visits = validation_visits
                    .checked_add(direct_entries.len().saturating_add(1))
                    .filter(|count| *count <= limits.prepare_entry_visits)
                    .ok_or_else(|| limit("metadata canonical validation work budget exceeded"))?;
                entries.extend_from_slice(direct_entries);
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            let canonical = Page::build(&entries).map_err(codec_error)?;
            if pages.get(&root).map(|payload| payload.bytes.as_slice())
                != Some(canonical.as_slice())
            {
                return Err(integrity(
                    "metadata directory is not its canonical Build(entries)",
                ));
            }
        }
        let nodes = pages
            .values()
            .map(|payload| RetentionNode {
                id: node_id(&payload.id),
                kind: RetainedKind::Page,
                state: NodeState::Live,
                bytes: payload.size,
            })
            .collect();
        let edges = supplied_edges
            .into_iter()
            .map(|(parent, child)| RetentionEdge {
                parent: node_id(&parent),
                child: node_id(&child),
            })
            .collect();
        Ok(Self {
            root: candidate.root,
            pages: pages.into_values().collect(),
            nodes,
            edges,
            payload_bytes,
            entry_count,
        })
    }

    pub fn root(&self) -> MetadataPageId {
        self.root
    }
    pub fn payloads(&self) -> &[MetadataPagePayload] {
        &self.pages
    }
    pub fn nodes(&self) -> &[RetentionNode] {
        &self.nodes
    }
    pub fn edges(&self) -> &[RetentionEdge] {
        &self.edges
    }
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    pub fn check_limits(&self, limits: MetadataDagLimits) -> Result<(), SnapshotError> {
        let limits = limits.effective();
        check_count(self.nodes.len(), limits.nodes, "metadata nodes")?;
        check_count(self.edges.len(), limits.edges, "metadata edges")?;
        check_count(self.entry_count, limits.entries, "metadata entries")?;
        if self.payload_bytes > limits.payload_bytes {
            return Err(limit("metadata payload budget exceeded"));
        }
        Ok(())
    }

    /// Cache residency only; outstanding caller Arcs are outside that bound.
    pub(crate) fn residency_bytes(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<Self>()
            .checked_add(
                self.pages
                    .capacity()
                    .checked_mul(std::mem::size_of::<MetadataPagePayload>())?,
            )?
            .checked_add(
                self.nodes
                    .capacity()
                    .checked_mul(std::mem::size_of::<RetentionNode>())?,
            )?
            .checked_add(
                self.edges
                    .capacity()
                    .checked_mul(std::mem::size_of::<RetentionEdge>())?,
            )?;
        for payload in &self.pages {
            bytes = bytes.checked_add(payload.bytes.capacity())?;
        }
        for node in &self.nodes {
            bytes = bytes.checked_add(node.id.capacity())?;
        }
        for edge in &self.edges {
            bytes = bytes
                .checked_add(edge.parent.capacity())?
                .checked_add(edge.child.capacity())?;
        }
        Some(bytes)
    }
}

pub(crate) struct MetadataDagBuilder {
    limits: MetadataDagLimits,
    pages: BTreeMap<MetadataPageId, MetadataPagePayload>,
    edges: BTreeSet<(MetadataPageId, MetadataPageId)>,
    required_nodes: BTreeSet<MetadataPageId>,
    directories: BTreeSet<MetadataPageId>,
    payload_bytes: u64,
    entries: usize,
    entry_visits: usize,
    failed: bool,
}

impl MetadataDagBuilder {
    pub(crate) fn new(limits: MetadataDagLimits) -> Self {
        Self {
            limits: limits.effective(),
            pages: BTreeMap::new(),
            edges: BTreeSet::new(),
            required_nodes: BTreeSet::new(),
            directories: BTreeSet::new(),
            payload_bytes: 0,
            entries: 0,
            entry_visits: 0,
            failed: false,
        }
    }

    pub(crate) fn contains_directory(&self, root: MetadataPageId) -> bool {
        self.directories.contains(&root)
    }

    pub(crate) fn add_directory(
        &mut self,
        root_bytes: &[u8],
        entries: &[Entry],
    ) -> Result<(), SnapshotError> {
        if self.failed {
            return Err(integrity("metadata preparation already failed"));
        }
        let result = self.add_directory_inner(root_bytes, entries);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn add_directory_inner(
        &mut self,
        root_bytes: &[u8],
        entries: &[Entry],
    ) -> Result<(), SnapshotError> {
        let root = page_id(root_bytes);
        if self.contains_directory(root) {
            return Ok(());
        }
        self.entries = self
            .entries
            .checked_add(entries.len())
            .filter(|count| *count <= self.limits.entries)
            .ok_or_else(|| limit("metadata entry budget exceeded"))?;
        self.require_node(root)?;
        if !self.pages.contains_key(&root)
            && root_bytes.len() as u64
                > self.limits.payload_bytes.saturating_sub(self.payload_bytes)
        {
            return Err(limit("metadata payload budget exceeded"));
        }
        self.charge_work(entries.len())?;
        let canonical = Page::build(entries).map_err(codec_error)?;
        if canonical != root_bytes {
            return Err(integrity("directory root disagrees with canonical entries"));
        }
        let mut routes = VecDeque::from([(Vec::<u8>::new(), root)]);
        while let Some((route, expected_id)) = routes.pop_front() {
            if self.pages.contains_key(&expected_id) {
                continue;
            }
            self.charge_work(entries.len())?;
            let pages = Page::pages_along_route(entries, &route).map_err(codec_error)?;
            let bytes = pages
                .into_iter()
                .last()
                .ok_or_else(|| integrity("canonical route returned no page"))?;
            let id = page_id(&bytes);
            if id != expected_id {
                return Err(integrity("canonical route child identity mismatch"));
            }
            self.payload_bytes = self
                .payload_bytes
                .checked_add(bytes.len() as u64)
                .filter(|total| *total <= self.limits.payload_bytes)
                .ok_or_else(|| limit("metadata payload budget exceeded"))?;
            let (page, _) = Page::decode(&bytes).map_err(codec_error)?;
            match page {
                Page::Leaf { entries } => {
                    for entry in entries.iter().filter(|entry| entry.is_dir()) {
                        self.add_edge(id, entry.child_root)?;
                    }
                }
                Page::Branch {
                    terminal, children, ..
                } => {
                    if let Some(entry) = terminal.filter(Entry::is_dir) {
                        self.add_edge(id, entry.child_root)?;
                    }
                    for child in children {
                        self.add_edge(id, child.child_page_id)?;
                        let mut child_route = route.clone();
                        child_route.push(child.label);
                        routes.push_back((child_route, child.child_page_id));
                    }
                }
            }
            self.pages.insert(
                id,
                MetadataPagePayload {
                    id,
                    size: bytes.len() as u64,
                    bytes,
                },
            );
        }
        self.directories.insert(root);
        Ok(())
    }

    fn require_node(&mut self, id: MetadataPageId) -> Result<(), SnapshotError> {
        if !self.required_nodes.contains(&id) {
            check_count(
                self.required_nodes.len().saturating_add(1),
                self.limits.nodes,
                "metadata nodes",
            )?;
            self.required_nodes.insert(id);
        }
        Ok(())
    }

    fn charge_work(&mut self, count: usize) -> Result<(), SnapshotError> {
        self.entry_visits = self
            .entry_visits
            .checked_add(count)
            .filter(|count| *count <= self.limits.prepare_entry_visits)
            .ok_or_else(|| limit("metadata preparation work budget exceeded"))?;
        Ok(())
    }

    fn add_edge(
        &mut self,
        parent: MetadataPageId,
        child: MetadataPageId,
    ) -> Result<(), SnapshotError> {
        if !self.edges.contains(&(parent, child)) {
            check_count(
                self.edges.len().saturating_add(1),
                self.limits.edges,
                "metadata edges",
            )?;
            self.require_node(parent)?;
            self.require_node(child)?;
            self.edges.insert((parent, child));
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        root: MetadataPageId,
    ) -> Result<ValidatedMetadataDag, SnapshotError> {
        if self.failed {
            return Err(integrity(
                "metadata preparation failed; no group can be exposed",
            ));
        }
        ValidatedMetadataDag::validate(
            MetadataDagCandidate {
                metadata_codec: METADATA_CODEC,
                root,
                pages: self.pages.into_values().collect(),
                edges: self.edges.into_iter().collect(),
            },
            self.limits,
        )
    }
}

fn validate_graph(
    root: MetadataPageId,
    pages: &BTreeMap<MetadataPageId, MetadataPagePayload>,
    edges: &BTreeSet<(MetadataPageId, MetadataPageId)>,
) -> Result<(), SnapshotError> {
    if !pages.contains_key(&root) {
        return Err(unavailable("metadata root payload is missing"));
    }
    let mut incoming: BTreeMap<_, usize> = pages.keys().map(|id| (*id, 0)).collect();
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for &(parent, child) in edges {
        if !pages.contains_key(&parent) || !pages.contains_key(&child) {
            return Err(unavailable("metadata child payload is missing"));
        }
        *incoming
            .get_mut(&child)
            .ok_or_else(|| unavailable("metadata child is missing"))? += 1;
        children.entry(parent).or_default().push(child);
    }
    let mut ready: VecDeque<_> = incoming
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect();
    let mut visited = 0usize;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for child in children.get(&id).into_iter().flatten() {
            let count = incoming
                .get_mut(child)
                .ok_or_else(|| unavailable("metadata child is missing"))?;
            *count -= 1;
            if *count == 0 {
                ready.push_back(*child);
            }
        }
    }
    if visited != pages.len() {
        return Err(integrity("metadata graph contains a cycle"));
    }
    let mut reachable = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(children.get(&id).into_iter().flatten().copied());
        }
    }
    if reachable.len() != pages.len() {
        return Err(integrity("metadata graph includes unreachable payloads"));
    }
    Ok(())
}

fn node_id(id: &MetadataPageId) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(76);
    value.push_str("page:sha256:");
    for byte in id {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn check_count(count: usize, maximum: usize, name: &str) -> Result<(), SnapshotError> {
    if count > maximum {
        return Err(limit(&format!("{name} budget exceeded")));
    }
    Ok(())
}
fn limit(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LimitExceeded, message)
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::ObjectUnavailable, message)
}
fn codec_error(error: mst2_codec::CodecError) -> SnapshotError {
    integrity(&format!("invalid metadata page: {error}"))
}

#[cfg(test)]
#[path = "retention_dag_tests.rs"]
mod tests;
