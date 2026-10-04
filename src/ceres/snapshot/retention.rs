//! Retention graph and garbage collection (spec 10 §5–§7).
//!
//! The coordinator owns one append-only, de-duplicated reference graph
//! for MST/2-derived pages/maps/frames:
//!
//! ```text
//! root pin / active lease / prepared root ──► RetentionNode(LIVE)
//! RetentionNode ──► RetentionEdge(unique parent,child) ──► RetentionNode
//! ```
//!
//! A node is reachable while at least one root pin, active lease or
//! prepared root covers it *or* it has a non-zero live incoming-edge
//! count. Collection never deletes Git raw blobs: the [`Reaper`] only
//! reclaims MST/2-derived bytes (chunk projections, cached frames), and
//! the physical delete is a separate opt-in step so a deployment without
//! the bidirectional `GitRetentionPort` (spec 10 §7) is fail-closed.
//!
//! Persistence sits behind [`RetentionStore`]; [`mem::InMemoryRetentionStore`]
//! proves the rules deterministically. The Postgres backend is the
//! remaining integration seam (same shape as `publication`).

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// Kind of retained object (spec 10 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetainedKind {
    /// Canonical MTP2 metadata page.
    Page,
    /// Chunk map (MCM2) or chunk leaf (MCL2).
    ChunkMap,
    /// Derived, cacheable frame payload.
    Frame,
    /// Write-through verified object (raw SHA-256 fact).
    VerifiedObject,
}

impl RetainedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RetainedKind::Page => "page",
            RetainedKind::ChunkMap => "chunk_map",
            RetainedKind::Frame => "frame",
            RetainedKind::VerifiedObject => "verified_object",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Live,
    Deleting,
}

/// A retained node (spec 10 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionNode {
    pub id: String,
    pub kind: RetainedKind,
    pub state: NodeState,
    /// Logical bytes this node owns (for accounting/back-pressure).
    pub bytes: u64,
}

/// One directed retention edge. Re-adding an identical edge is a no-op,
/// so a parent referencing a child through several pages increments the
/// child's reference count exactly once (spec 10 §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RetentionEdge {
    pub parent: String,
    pub child: String,
}

/// A reachability root: an active lease, a durable pin or a prepared
/// publication root (spec 10 §5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RetentionRoot {
    Lease(String),
    Pin(String),
    Prepare(String),
}

impl RetentionRoot {
    fn key(&self) -> String {
        match self {
            RetentionRoot::Lease(id) => format!("lease:{id}"),
            RetentionRoot::Pin(id) => format!("pin:{id}"),
            RetentionRoot::Prepare(id) => format!("prepare:{id}"),
        }
    }
}

/// What a collection run decided for one unreachable node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapDecision {
    /// Marked DELETING (spec step: CAS LIVE→DELETING).
    Marked,
    /// Physical reclaim attempted via the reaper.
    Reaped,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionReport {
    /// Node ids atomically marked DELETING this run.
    pub unreachable: Vec<String>,
    /// Bytes that would be / were reclaimed.
    pub reclaimed_bytes: u64,
    /// Nodes actually physically reaped (empty when delete is disabled).
    pub reaped: Vec<String>,
}

/// Physical reclaim seam. A deployment without bidirectional GC
/// integration must use [`NoopReaper`], which reports success without
/// deleting Git-owned content (spec 10 §7).
pub trait Reaper: Send + Sync {
    fn reap(&self, node: &RetentionNode) -> Result<(), SnapshotError>;
    /// Whether this reaper performs physical deletion.
    fn physical(&self) -> bool;
}

/// Fail-closed reaper: derived bytes are detached logically but never
/// physically removed until a `GitRetentionPort` is wired in.
pub struct NoopReaper;

impl Reaper for NoopReaper {
    fn reap(&self, _node: &RetentionNode) -> Result<(), SnapshotError> {
        Ok(())
    }
    fn physical(&self) -> bool {
        false
    }
}

/// The durable seam (Postgres in production), mirroring the
/// `PublicationStore` pattern.
pub trait RetentionStore {
    fn node(&self, id: &str) -> Option<RetentionNode>;
    fn root_covers(&self, node_id: &str) -> bool;
    /// Count incoming edges from parents not yet removed, including
    /// DELETING parents whose physical reclaim has not succeeded.
    fn live_incoming(&self, node_id: &str) -> usize;
    /// Idempotent: retain one LIVE node with its edges and root coverage.
    fn retain(
        &self,
        node: RetentionNode,
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError> {
        self.retain_group(std::slice::from_ref(&node), edges, roots)
    }
    /// Atomically retain the entire group, covering every supplied node
    /// with `roots`. All nodes and edge endpoints must be LIVE; acquiring
    /// a DELETING node is forbidden. Any error leaves the graph unchanged.
    fn retain_group(
        &self,
        nodes: &[RetentionNode],
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError>;
    /// Atomically check zero roots/incoming references and CAS LIVE→DELETING.
    /// A parent still protects its children until the parent is removed,
    /// including while the parent is DELETING and physical reclaim is pending.
    fn mark_deleting(&self, id: &str) -> bool;
    /// Remove a DELETING node after the reaper succeeds.
    fn remove(&self, id: &str);
    /// Release a root (lease expiry/pin removal). Idempotent.
    fn release_root(&self, root: &RetentionRoot);
    fn all_live(&self) -> Vec<RetentionNode>;
    /// Iterate edges originating at `parent` (for reachability traversal).
    fn children(&self, parent: &str) -> Vec<String>;
}

pub struct RetentionCoordinator<S: RetentionStore> {
    store: S,
}

impl<S: RetentionStore> RetentionCoordinator<S> {
    pub fn new(store: S) -> Self {
        RetentionCoordinator { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Retain `node`, linking it to `parents` and covering it by `roots`.
    /// Every parent must already be known so the graph cannot dangle;
    /// the node, its edges and the root coverage land atomically.
    pub fn retain(
        &self,
        node: RetentionNode,
        parents: &[String],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError> {
        let edges: Vec<RetentionEdge> = parents
            .iter()
            .map(|p| RetentionEdge {
                parent: p.clone(),
                child: node.id.clone(),
            })
            .collect();
        self.store.retain(node, &edges, roots)
    }

    /// Acquire root coverage for the complete group in one store operation.
    /// The root is also materialized as a LIVE anchor. If any covered node
    /// is DELETING, neither the anchor nor any partial coverage is retained.
    pub fn pin_root(
        &self,
        root: &RetentionRoot,
        covered: &[RetentionNode],
    ) -> Result<(), SnapshotError> {
        let mut nodes = Vec::with_capacity(covered.len() + 1);
        nodes.push(RetentionNode {
            id: root.key(),
            kind: RetainedKind::Frame,
            state: NodeState::Live,
            bytes: 0,
        });
        nodes.extend_from_slice(covered);
        self.store
            .retain_group(&nodes, &[], std::slice::from_ref(root))
    }

    pub fn release(&self, root: &RetentionRoot) {
        self.store.release_root(root);
    }

    /// One collection pass (spec 10 §6 steps 1–7):
    /// mark unreachable LIVE nodes DELETING, then reap. The reachability
    /// scan selects candidates; the store rechecks roots and incoming
    /// references atomically with the CAS. Acquisition either wins before
    /// that CAS or fails because the node is already DELETING.
    pub fn collect<R: Reaper>(&self, reaper: &R) -> Result<CollectionReport, SnapshotError> {
        let live = self.store.all_live();
        let reachable = self.reachable_set();

        let mut report = CollectionReport::default();
        for node in live {
            if reachable.contains(&node.id) {
                continue;
            }
            if !self.store.mark_deleting(&node.id) {
                // Another collector or a newly acquired reference won.
                continue;
            }
            report.unreachable.push(node.id.clone());
            report.reclaimed_bytes += node.bytes;
            if reaper.physical() {
                reaper.reap(&node)?;
                self.store.remove(&node.id);
                report.reaped.push(node.id);
            }
        }
        Ok(report)
    }

    /// Reachability by root-anchored graph traversal (spec mark-sweep).
    fn reachable_set(&self) -> HashSet<String> {
        let live = self.store.all_live();
        let mut roots: Vec<String> = live
            .iter()
            .filter(|n| self.store.root_covers(&n.id))
            .map(|n| n.id.clone())
            .collect();
        let mut seen = HashSet::new();
        while let Some(id) = roots.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            for child in self.store.children(&id) {
                roots.push(child);
            }
        }
        seen
    }

    /// True while `id` is reachable from any active root (used by read
    /// paths to refuse serving content a lease no longer covers).
    pub fn is_retained(&self, id: &str) -> bool {
        self.reachable_set().contains(id)
    }
}

pub mod mem {
    use super::*;

    #[derive(Default)]
    struct Inner {
        nodes: HashMap<String, RetentionNode>,
        edges: HashSet<RetentionEdge>,
        /// node id -> set of root keys covering it.
        roots: HashMap<String, HashSet<String>>,
        #[cfg(test)]
        fail_next_retain: bool,
    }

    /// Single-Mutex store; the lock is the serialization point that makes
    /// retain/mark/remove atomic, exactly as a serializable DB transaction.
    pub struct InMemoryRetentionStore {
        inner: Mutex<Inner>,
    }

    impl Default for InMemoryRetentionStore {
        fn default() -> Self {
            InMemoryRetentionStore {
                inner: Mutex::new(Inner::default()),
            }
        }
    }

    #[cfg(test)]
    impl InMemoryRetentionStore {
        /// Inject a backend failure after validation and before committing.
        pub(crate) fn fail_next_retain_for_test(&self) {
            self.inner.lock().unwrap().fail_next_retain = true;
        }
    }

    impl RetentionStore for InMemoryRetentionStore {
        fn node(&self, id: &str) -> Option<RetentionNode> {
            self.inner.lock().unwrap().nodes.get(id).cloned()
        }

        fn root_covers(&self, node_id: &str) -> bool {
            self.inner
                .lock()
                .unwrap()
                .roots
                .get(node_id)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        }

        fn live_incoming(&self, node_id: &str) -> usize {
            let g = self.inner.lock().unwrap();
            g.edges
                .iter()
                .filter(|e| e.child == node_id && g.nodes.contains_key(&e.parent))
                .count()
        }

        fn retain_group(
            &self,
            nodes: &[RetentionNode],
            edges: &[RetentionEdge],
            roots: &[RetentionRoot],
        ) -> Result<(), SnapshotError> {
            let mut g = self.inner.lock().unwrap();
            // Validate the entire transaction before mutating any graph data.
            let mut staged = HashMap::new();
            for node in nodes {
                if node.state != NodeState::Live
                    || g.nodes
                        .get(&node.id)
                        .is_some_and(|existing| existing.state != NodeState::Live)
                {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::ObjectUnavailable,
                        format!("retention acquire requires LIVE node {}", node.id),
                    ));
                }
                if staged
                    .insert(node.id.as_str(), node)
                    .is_some_and(|previous| previous != node)
                {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::Internal,
                        "retention group has conflicting node definitions",
                    ));
                }
            }
            for e in edges {
                for id in [&e.parent, &e.child] {
                    match staged.get(id.as_str()).copied().or_else(|| g.nodes.get(id)) {
                        Some(node) if node.state == NodeState::Live => {}
                        Some(_) => {
                            return Err(SnapshotError::new(
                                SnapshotErrorCode::ObjectUnavailable,
                                format!("retention edge requires LIVE endpoint {id}"),
                            ));
                        }
                        None => {
                            return Err(SnapshotError::new(
                                SnapshotErrorCode::Internal,
                                format!("retention edge references missing node {id}"),
                            ));
                        }
                    }
                }
            }
            #[cfg(test)]
            if std::mem::take(&mut g.fail_next_retain) {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::Internal,
                    "injected retention commit failure",
                ));
            }
            for node in nodes {
                g.nodes.insert(node.id.clone(), node.clone());
                let entry = g.roots.entry(node.id.clone()).or_default();
                for root in roots {
                    entry.insert(root.key());
                }
            }
            g.edges.extend(edges.iter().cloned());
            Ok(())
        }

        fn mark_deleting(&self, id: &str) -> bool {
            let mut g = self.inner.lock().unwrap();
            if g.roots.get(id).is_some_and(|roots| !roots.is_empty())
                || g.edges
                    .iter()
                    .any(|edge| edge.child == id && g.nodes.contains_key(&edge.parent))
            {
                return false;
            }
            match g.nodes.get_mut(id) {
                Some(n) if n.state == NodeState::Live => {
                    n.state = NodeState::Deleting;
                    true
                }
                _ => false,
            }
        }

        fn remove(&self, id: &str) {
            let mut g = self.inner.lock().unwrap();
            if !g
                .nodes
                .get(id)
                .is_some_and(|node| node.state == NodeState::Deleting)
            {
                return;
            }
            g.nodes.remove(id);
            g.edges.retain(|e| e.parent != id && e.child != id);
            g.roots.remove(id);
        }

        fn release_root(&self, root: &RetentionRoot) {
            let key = root.key();
            let mut g = self.inner.lock().unwrap();
            for set in g.roots.values_mut() {
                set.remove(&key);
            }
        }

        fn all_live(&self) -> Vec<RetentionNode> {
            self.inner
                .lock()
                .unwrap()
                .nodes
                .values()
                .filter(|n| n.state == NodeState::Live)
                .cloned()
                .collect()
        }

        fn children(&self, parent: &str) -> Vec<String> {
            self.inner
                .lock()
                .unwrap()
                .edges
                .iter()
                .filter(|e| e.parent == parent)
                .map(|e| e.child.clone())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            mpsc::{Receiver, SyncSender, sync_channel},
        },
        thread,
        time::Duration,
    };

    use super::{mem::InMemoryRetentionStore, *};

    fn node(id: &str, bytes: u64) -> RetentionNode {
        RetentionNode {
            id: id.to_string(),
            kind: RetainedKind::ChunkMap,
            state: NodeState::Live,
            bytes,
        }
    }

    fn coord() -> RetentionCoordinator<InMemoryRetentionStore> {
        RetentionCoordinator::new(InMemoryRetentionStore::default())
    }

    /// Pause a collection after its scan, immediately before the store CAS.
    /// Timeouts make both sides of the forced interleaving bounded.
    struct PausedMarkStore {
        inner: InMemoryRetentionStore,
        ready: SyncSender<()>,
        resume: Mutex<Receiver<()>>,
    }

    impl RetentionStore for PausedMarkStore {
        fn node(&self, id: &str) -> Option<RetentionNode> {
            self.inner.node(id)
        }

        fn root_covers(&self, node_id: &str) -> bool {
            self.inner.root_covers(node_id)
        }

        fn live_incoming(&self, node_id: &str) -> usize {
            self.inner.live_incoming(node_id)
        }

        fn retain_group(
            &self,
            nodes: &[RetentionNode],
            edges: &[RetentionEdge],
            roots: &[RetentionRoot],
        ) -> Result<(), SnapshotError> {
            self.inner.retain_group(nodes, edges, roots)
        }

        fn mark_deleting(&self, id: &str) -> bool {
            self.ready.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("test did not resume deletion");
            self.inner.mark_deleting(id)
        }

        fn remove(&self, id: &str) {
            self.inner.remove(id);
        }

        fn release_root(&self, root: &RetentionRoot) {
            self.inner.release_root(root);
        }

        fn all_live(&self) -> Vec<RetentionNode> {
            self.inner.all_live()
        }

        fn children(&self, parent: &str) -> Vec<String> {
            self.inner.children(parent)
        }
    }

    #[test]
    fn failed_pin_group_leaves_no_anchor_or_partial_coverage() {
        let c = coord();
        let original = node("existing", 10);
        c.store().retain(original.clone(), &[], &[]).unwrap();
        c.store().retain(node("deleting", 7), &[], &[]).unwrap();
        assert!(c.store().mark_deleting("deleting"));
        let root = RetentionRoot::Lease("new".into());

        // The last node fails after earlier entries would have been written
        // by the old per-node pin loop, including an update to existing.
        let err = c
            .pin_root(
                &root,
                &[node("fresh", 1), node("existing", 99), node("deleting", 7)],
            )
            .unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::ObjectUnavailable);
        assert!(c.store().node(&root.key()).is_none());
        assert!(c.store().node("fresh").is_none());
        assert_eq!(c.store().node("existing"), Some(original));
        assert!(!c.store().root_covers("existing"));
        assert!(!c.store().root_covers("deleting"));
        assert_eq!(
            c.store().node("deleting").unwrap().state,
            NodeState::Deleting
        );
    }

    #[test]
    fn supplied_deleting_node_cannot_be_created_or_pinned() {
        let c = coord();
        let root = RetentionRoot::Pin("invalid".into());
        let mut deleting = node("absent", 1);
        deleting.state = NodeState::Deleting;
        assert_eq!(
            c.pin_root(&root, &[deleting]).unwrap_err().code,
            SnapshotErrorCode::ObjectUnavailable
        );
        assert!(c.store().all_live().is_empty());
        assert!(c.store().node("absent").is_none());
        assert!(c.store().node(&root.key()).is_none());
    }

    #[test]
    fn late_invalid_edge_rolls_back_nodes_edges_and_roots() {
        let c = coord();
        c.store().retain(node("parent", 0), &[], &[]).unwrap();
        let root = RetentionRoot::Lease("new".into());
        let edges = [
            RetentionEdge {
                parent: "parent".into(),
                child: "fresh".into(),
            },
            RetentionEdge {
                parent: "missing".into(),
                child: "fresh".into(),
            },
        ];
        let err = c
            .store()
            .retain(node("fresh", 2), &edges, &[root])
            .unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Internal);
        assert!(c.store().node("fresh").is_none());
        assert!(c.store().children("parent").is_empty());
        assert!(!c.store().root_covers("fresh"));
        assert_eq!(c.store().live_incoming("fresh"), 0);
    }

    #[test]
    fn injected_commit_failure_preserves_existing_root_and_can_retry() {
        let c = coord();
        let old = RetentionRoot::Pin("old".into());
        let new = RetentionRoot::Lease("new".into());
        c.store()
            .retain(node("existing", 1), &[], std::slice::from_ref(&old))
            .unwrap();
        c.store().fail_next_retain_for_test();
        assert_eq!(
            c.pin_root(&new, &[node("existing", 1), node("fresh", 2)])
                .unwrap_err()
                .code,
            SnapshotErrorCode::Internal
        );
        assert!(c.store().node(&new.key()).is_none());
        assert!(c.store().node("fresh").is_none());
        assert!(c.store().root_covers("existing"));
        c.release(&old);
        assert!(
            !c.store().root_covers("existing"),
            "failed pin added no new root"
        );

        c.pin_root(&new, &[node("existing", 1), node("fresh", 2)])
            .unwrap();
        assert!(c.is_retained("existing"));
        assert!(c.is_retained("fresh"));
    }

    #[test]
    fn collection_rechecks_roots_and_edges_acquired_after_its_scan() {
        for via_edge in [false, true] {
            let (ready_tx, ready_rx) = sync_channel(1);
            let (resume_tx, resume_rx) = sync_channel(1);
            let c = Arc::new(RetentionCoordinator::new(PausedMarkStore {
                inner: InMemoryRetentionStore::default(),
                ready: ready_tx,
                resume: Mutex::new(resume_rx),
            }));
            c.store().retain(node("candidate", 1), &[], &[]).unwrap();
            let root = RetentionRoot::Lease("late".into());
            if via_edge {
                c.store()
                    .retain(node("parent", 0), &[], std::slice::from_ref(&root))
                    .unwrap();
            }
            let collector = {
                let c = Arc::clone(&c);
                thread::spawn(move || c.collect(&NoopReaper))
            };
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("collector did not reach its deletion CAS");
            // Both references land after candidate selection, before CAS.
            if via_edge {
                c.retain(node("candidate", 1), &["parent".into()], &[])
                    .unwrap();
            } else {
                c.pin_root(&root, &[node("candidate", 1)]).unwrap();
            }
            resume_tx.send(()).unwrap();
            let report = collector.join().unwrap().unwrap();
            assert!(report.unreachable.is_empty());
            assert_eq!(c.store().node("candidate").unwrap().state, NodeState::Live);
            assert!(c.is_retained("candidate"));
        }
    }

    #[test]
    fn deleting_parent_rejects_new_edges_and_protects_existing_children_until_removed() {
        let c = coord();
        c.store().retain(node("parent", 0), &[], &[]).unwrap();
        c.retain(node("child", 1), &["parent".into()], &[]).unwrap();
        assert!(c.store().mark_deleting("parent"));
        assert!(!c.store().mark_deleting("child"));
        assert_eq!(c.store().live_incoming("child"), 1);
        assert_eq!(
            c.retain(node("new-child", 2), &["parent".into()], &[])
                .unwrap_err()
                .code,
            SnapshotErrorCode::ObjectUnavailable
        );
        assert!(c.store().node("new-child").is_none());
        c.store().remove("parent");
        assert_eq!(c.store().live_incoming("child"), 0);
        assert!(c.store().mark_deleting("child"));
    }

    #[test]
    fn retain_cannot_add_an_edge_to_a_deleting_child() {
        let c = coord();
        c.store().retain(node("child", 1), &[], &[]).unwrap();
        assert!(c.store().mark_deleting("child"));
        let edge = RetentionEdge {
            parent: "new-parent".into(),
            child: "child".into(),
        };
        assert_eq!(
            c.store()
                .retain(node("new-parent", 0), &[edge], &[])
                .unwrap_err()
                .code,
            SnapshotErrorCode::ObjectUnavailable
        );
        assert!(c.store().node("new-parent").is_none());
        assert_eq!(c.store().node("child").unwrap().state, NodeState::Deleting);
        assert_eq!(c.store().live_incoming("child"), 0);
    }

    #[test]
    fn leaf_dies_when_root_released_but_other_root_keeps_it() {
        // root A and B both cover a shared leaf; releasing A must not
        // collect while B is live (spec: GC must not delete under active lease).
        let c = coord();
        let a = RetentionRoot::Lease("a".into());
        let b = RetentionRoot::Lease("b".into());
        c.store()
            .retain(node("leaf", 10), &[], &[a.clone(), b.clone()])
            .unwrap();
        c.release(&a);
        let r = c.collect(&NoopReaper).unwrap();
        assert!(r.unreachable.is_empty(), "root B still covers leaf");
        c.release(&b);
        let r = c.collect(&NoopReaper).unwrap();
        assert_eq!(r.unreachable, vec!["leaf".to_string()]);
        assert_eq!(r.reclaimed_bytes, 10);
        assert!(r.reaped.is_empty(), "NoopReaper never physically deletes");
    }

    #[test]
    fn reachability_follows_the_graph() {
        let c = coord();
        let root = RetentionRoot::Pin("p".into());
        c.store()
            .retain(node("root", 0), &[], std::slice::from_ref(&root))
            .unwrap();
        c.retain(node("a", 1), &["root".to_string()], &[]).unwrap();
        c.retain(node("b", 2), &["a".to_string()], &[]).unwrap();
        // An orphan with no root and no parent.
        c.store().retain(node("orphan", 9), &[], &[]).unwrap();
        assert!(c.is_retained("b"));
        assert!(!c.is_retained("orphan"));
        let r = c.collect(&NoopReaper).unwrap();
        assert_eq!(r.unreachable, vec!["orphan".to_string()]);
        assert_eq!(r.reclaimed_bytes, 9);
        // Releasing the root makes the chain unreachable, but children stay
        // LIVE while their parents await physical removal under NoopReaper.
        c.release(&root);
        let _ = c.collect(&NoopReaper).unwrap();
        assert!(!c.is_retained("b"));
    }

    #[test]
    fn duplicate_edge_counts_once() {
        let c = coord();
        c.store()
            .retain(node("parent", 0), &[], &[RetentionRoot::Lease("l".into())])
            .unwrap();
        c.retain(node("child", 5), &["parent".to_string()], &[])
            .unwrap();
        // A second logical reference through the same parent edge.
        c.retain(node("child", 5), &["parent".to_string()], &[])
            .unwrap();
        assert_eq!(c.store().live_incoming("child"), 1);
    }

    #[test]
    fn child_shared_by_two_parents_survives_one_parent_collection() {
        let c = coord();
        let r1 = RetentionRoot::Pin("r1".into());
        let r2 = RetentionRoot::Pin("r2".into());
        c.store().retain(node("p1", 0), &[], &[r1]).unwrap();
        c.store().retain(node("p2", 0), &[], &[r2]).unwrap();
        c.retain(
            node("shared", 7),
            &["p1".to_string(), "p2".to_string()],
            &[],
        )
        .unwrap();
        assert_eq!(c.store().live_incoming("shared"), 2);
        // Drop p1's coverage; its pending deletion must not affect p2's view.
        c.release(&RetentionRoot::Pin("r1".into()));
        c.collect(&NoopReaper).unwrap();
        assert!(c.store().node("shared").is_some());
    }

    #[test]
    fn physical_reaper_removes_but_noop_does_not() {
        struct Delete;
        impl Reaper for Delete {
            fn reap(&self, _n: &RetentionNode) -> Result<(), SnapshotError> {
                Ok(())
            }
            fn physical(&self) -> bool {
                true
            }
        }
        let c = coord();
        c.store().retain(node("x", 3), &[], &[]).unwrap();
        let r = c.collect(&Delete).unwrap();
        assert_eq!(r.reaped, vec!["x".to_string()]);
        assert!(c.store().node("x").is_none());
    }

    #[test]
    fn reaper_error_aborts_collection_without_losing_the_node() {
        struct Fail;
        impl Reaper for Fail {
            fn reap(&self, _n: &RetentionNode) -> Result<(), SnapshotError> {
                Err(SnapshotError::new(
                    SnapshotErrorCode::Internal,
                    "backend down",
                ))
            }
            fn physical(&self) -> bool {
                true
            }
        }
        let c = coord();
        c.store().retain(node("x", 3), &[], &[]).unwrap();
        assert!(c.collect(&Fail).is_err());
        // Node remains DELETING; physical retry/recovery is a later slice.
        assert_eq!(c.store().node("x").unwrap().state, NodeState::Deleting);
    }

    #[test]
    fn new_root_protects_a_live_node_before_it_is_collected() {
        let c = coord();
        c.store().retain(node("x", 4), &[], &[]).unwrap();
        // Before any collect, a lease covers it.
        c.store()
            .retain(node("x", 4), &[], &[RetentionRoot::Lease("l".into())])
            .unwrap();
        let r = c.collect(&NoopReaper).unwrap();
        assert!(r.unreachable.is_empty());
    }

    #[test]
    fn edge_to_unknown_parent_is_rejected() {
        let c = coord();
        let err = c
            .retain(node("x", 1), &["ghost".to_string()], &[])
            .unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::Internal);
    }

    #[test]
    fn release_is_idempotent() {
        let c = coord();
        let root = RetentionRoot::Lease("l".into());
        c.store()
            .retain(node("x", 1), &[], std::slice::from_ref(&root))
            .unwrap();
        c.release(&root);
        c.release(&root);
        assert!(!c.store().root_covers("x"));
    }

    #[test]
    fn prepare_root_protects_like_lease() {
        let c = coord();
        let prep = RetentionRoot::Prepare("op-1".into());
        c.store().retain(node("x", 1), &[], &[prep]).unwrap();
        assert!(c.collect(&NoopReaper).unwrap().unreachable.is_empty());
        c.release(&RetentionRoot::Prepare("op-1".into()));
        assert_eq!(
            c.collect(&NoopReaper).unwrap().unreachable,
            vec!["x".to_string()]
        );
    }

    #[test]
    fn repeated_collect_is_stable_and_idempotent() {
        let c = coord();
        c.store().retain(node("a", 1), &[], &[]).unwrap();
        c.store()
            .retain(node("b", 1), &[], &[RetentionRoot::Lease("l".into())])
            .unwrap();
        let first = c.collect(&NoopReaper).unwrap();
        assert_eq!(first.unreachable.len(), 1);
        // Second pass must not double-count or touch the LIVE/DELETING set.
        let second = c.collect(&NoopReaper).unwrap();
        assert!(second.unreachable.is_empty());
    }

    #[test]
    fn removing_node_removes_its_edges() {
        struct Delete;
        impl Reaper for Delete {
            fn reap(&self, _n: &RetentionNode) -> Result<(), SnapshotError> {
                Ok(())
            }
            fn physical(&self) -> bool {
                true
            }
        }
        let c = coord();
        c.store()
            .retain(node("p", 0), &[], &[RetentionRoot::Lease("l".into())])
            .unwrap();
        c.retain(node("ch", 2), &["p".to_string()], &[]).unwrap();
        c.release(&RetentionRoot::Lease("l".into()));
        c.collect(&Delete).unwrap();
        // A child selected before its parent was removed waits for the
        // next pass; iteration order must not affect the final result.
        c.collect(&Delete).unwrap();
        // Both nodes and their edge are gone; no stale incoming edge.
        assert!(c.store().node("p").is_none());
        assert!(c.store().node("ch").is_none());
        assert_eq!(c.store().live_incoming("ch"), 0);
    }
}
