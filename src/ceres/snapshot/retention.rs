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

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

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
    /// Node ids that became unreachable this run.
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
    fn live_incoming(&self, node_id: &str) -> usize;
    /// Idempotent: create the node if absent, upsert edges and roots in
    /// one atomic step (spec §6 "no empty window").
    fn retain(
        &self,
        node: RetentionNode,
        edges: &[RetentionEdge],
        roots: &[RetentionRoot],
    ) -> Result<(), SnapshotError>;
    /// Atomically CAS a node LIVE→DELETING; false if it is no longer LIVE.
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
        for p in parents {
            if self.store.node(p).is_none() {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::Internal,
                    format!("retention edge to unknown parent {p}"),
                ));
            }
        }
        let edges: Vec<RetentionEdge> = parents
            .iter()
            .map(|p| RetentionEdge {
                parent: p.clone(),
                child: node.id.clone(),
            })
            .collect();
        self.store.retain(node, &edges, roots)
    }

    /// Add coverage from a root to a node, atomically re-lifting it out of
    /// a DELETING state (spec §10). The root is also materialized as a live
    /// anchor node so reachability can start from it. The store's `retain`
    /// is the upsert, so both the fresh and the already-known cases take
    /// the same path.
    pub fn pin_root(
        &self,
        root: &RetentionRoot,
        covered: &[RetentionNode],
    ) -> Result<(), SnapshotError> {
        let roots = std::slice::from_ref(root);
        self.store.retain(
            RetentionNode {
                id: root.key(),
                kind: RetainedKind::Frame,
                state: NodeState::Live,
                bytes: 0,
            },
            &[],
            roots,
        )?;
        for n in covered {
            self.store.retain(
                RetentionNode {
                    state: NodeState::Live,
                    ..n.clone()
                },
                &[],
                roots,
            )?;
        }
        Ok(())
    }

    pub fn release(&self, root: &RetentionRoot) {
        self.store.release_root(root);
    }

    /// One collection pass (spec 10 §6 steps 1–7):
    /// mark unreachable LIVE nodes DELETING, then reap. Nodes marked
    /// DELETING are skipped entirely on later passes if a new root/edge
    /// raced in only after the CAS — here reachability is recomputed under
    /// the store lock, so a retain concurrent with a pass either observes
    /// the node (keeping it LIVE) or lands before the next pass.
    pub fn collect<R: Reaper>(&self, reaper: &R) -> Result<CollectionReport, SnapshotError> {
        let live = self.store.all_live();
        let reachable = self.reachable_set();

        let mut report = CollectionReport::default();
        for node in live {
            if reachable.contains(&node.id) {
                continue;
            }
            if !self.store.mark_deleting(&node.id) {
                // Lost the CAS: a new reference made it LIVE again.
                continue;
            }
            report.unreachable.push(node.id.clone());
            report.reclaimed_bytes += node.bytes;
            // Re-verify after the CAS: the store serializes retain against
            // mark, so a zero incoming count here is stable for this pass.
            if self.store.root_covers(&node.id) || self.store.live_incoming(&node.id) > 0 {
                // A concurrent retain resurrected it; the CAS semantics of
                // the store keep it LIVE, so do not reap.
                continue;
            }
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

        fn retain(
            &self,
            node: RetentionNode,
            edges: &[RetentionEdge],
            roots: &[RetentionRoot],
        ) -> Result<(), SnapshotError> {
            let mut g = self.inner.lock().unwrap();
            // Atomic re-lift: never downgrade a LIVE node to DELETING.
            g.nodes
                .entry(node.id.clone())
                .and_modify(|existing| {
                    existing.bytes = node.bytes;
                    existing.kind = node.kind;
                    existing.state = NodeState::Live;
                })
                .or_insert_with(|| node.clone());
            // Defensive: do not insert an edge to a missing child node.
            for e in edges {
                if !g.nodes.contains_key(&e.child) || !g.nodes.contains_key(&e.parent) {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::Internal,
                        "retention edge references missing node",
                    ));
                }
                g.edges.insert(e.clone());
            }
            let entry = g.roots.entry(node.id.clone()).or_default();
            for r in roots {
                entry.insert(r.key());
            }
            Ok(())
        }

        fn mark_deleting(&self, id: &str) -> bool {
            let mut g = self.inner.lock().unwrap();
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
    use super::mem::InMemoryRetentionStore;
    use super::*;

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
        // Releasing the root cascades unreachability to a and b only after
        // the anchor itself is collected (the anchor node is root-covered).
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
        // Remove p1's coverage and p1 itself; shared still has p2.
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
        // Node remains (DELETING) so a retry can replay; it is not lost.
        assert!(c.store().node("x").is_some());
    }

    #[test]
    fn new_root_revives_a_node_before_it_is_collected() {
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
        // Both nodes and their edge are gone; no stale incoming edge.
        assert_eq!(c.store().live_incoming("ch"), 0);
    }
}
