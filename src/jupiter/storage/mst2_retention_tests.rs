use std::time::Duration;

use sea_orm::{
    ConnectionTrait, DatabaseConnection, EntityTrait, IsolationLevel, PaginatorTrait,
    TransactionTrait,
};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    callisto::{mst2_retention_edge, mst2_retention_root},
    ceres::snapshot::retention::RetainedKind,
    jupiter::{migration::Migrator, tests::test_db_connection},
};

fn node(id: &str) -> RetentionNode {
    RetentionNode {
        id: id.into(),
        kind: RetainedKind::Page,
        state: NodeState::Live,
        bytes: 7,
    }
}

fn edge(parent: &str, child: &str) -> RetentionEdge {
    RetentionEdge {
        parent: parent.into(),
        child: child.into(),
    }
}

async fn fixture() -> (DatabaseConnection, PostgresRetentionRepository) {
    let temp = tempfile::TempDir::new().expect("temp directory");
    let db = test_db_connection(temp.path()).await;
    Migrator::up(&db, None)
        .await
        .expect("isolated schema migrations");
    let repository = PostgresRetentionRepository::new(db.clone());
    (db, repository)
}

#[test]
fn t06b_rejects_cycles_conflicting_identities_and_unbounded_groups() {
    let a = node("a");
    let b = node("b");
    assert_eq!(
        PreparedGroup::new(&[a.clone(), b], &[edge("a", "b"), edge("b", "a")], &[])
            .err()
            .expect("cycle rejected")
            .code,
        SnapshotErrorCode::IntegrityError
    );
    let mut conflicting = a.clone();
    conflicting.bytes += 1;
    assert!(PreparedGroup::new(&[a.clone(), conflicting], &[], &[]).is_err());
    assert_eq!(
        PreparedGroup::new(&vec![a; MAX_NODES + 1], &[], &[])
            .err()
            .expect("batch rejected")
            .code,
        SnapshotErrorCode::LimitExceeded
    );
}

#[tokio::test]
async fn t06b_retains_unique_edges_and_independent_roots_once() {
    let (db, repository) = fixture().await;
    let roots = [
        RetentionRoot::Lease("one".into()),
        RetentionRoot::Pin("two".into()),
    ];
    let nodes = [node("parent"), node("child")];
    let edges = [edge("parent", "child"), edge("parent", "child")];
    for _ in 0..2 {
        repository
            .retain_group(&nodes, &edges, &roots)
            .await
            .expect("idempotent retain");
    }
    assert_eq!(
        repository
            .node("child")
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        1
    );
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        4
    );
    repository.release_root(&roots[0]).await.unwrap();
    repository.release_root(&roots[0]).await.unwrap();
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        repository
            .mark_deleting("gc-parent", "parent")
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    repository.release_root(&roots[1]).await.unwrap();
    assert_eq!(
        repository
            .mark_deleting("gc-parent", "parent")
            .await
            .unwrap(),
        GcClaim::Marked
    );
    assert_eq!(
        repository.mark_deleting("gc-child", "child").await.unwrap(),
        GcClaim::Unavailable
    );
}

#[tokio::test]
async fn t06b_group_failure_rolls_back_savepoint_even_if_caller_commits() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(&[node("old")], &[], &[])
        .await
        .unwrap();
    assert_eq!(
        repository.mark_deleting("old-gc", "old").await.unwrap(),
        GcClaim::Marked
    );
    let txn = db.begin().await.unwrap();
    let error = PostgresRetentionRepository::retain_group_in_txn(
        &txn,
        &[node("new")],
        &[edge("new", "old")],
        &[RetentionRoot::Lease("partial".into())],
    )
    .await
    .expect_err("DELETING child rejects whole group");
    assert_eq!(error.code, SnapshotErrorCode::ObjectUnavailable);
    txn.commit().await.unwrap();
    assert!(repository.node("new").await.unwrap().is_none());
    assert_eq!(
        repository.node("old").await.unwrap().unwrap().state,
        "DELETING"
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn t06b_publication_rollback_removes_retention_acquisition() {
    let (db, repository) = fixture().await;
    let txn = db.begin().await.unwrap();
    PostgresRetentionRepository::retain_group_in_txn(
        &txn,
        &[node("publication")],
        &[],
        &[RetentionRoot::Prepare("writer".into())],
    )
    .await
    .unwrap();
    txn.rollback().await.unwrap();
    assert!(repository.node("publication").await.unwrap().is_none());
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn t06b_root_release_rollback_preserves_coverage() {
    let (db, repository) = fixture().await;
    let root = RetentionRoot::Lease("reader".into());
    repository
        .retain_group(&[node("page")], &[], std::slice::from_ref(&root))
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    PostgresRetentionRepository::release_root_in_txn(&txn, &root)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(
        repository.mark_deleting("page-op", "page").await.unwrap(),
        GcClaim::Unavailable
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn t06b_rejects_snapshot_isolation_without_mutating_outer_transaction() {
    let (db, repository) = fixture().await;
    let txn = db
        .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
        .await
        .unwrap();
    let error = PostgresRetentionRepository::retain_group_in_txn(
        &txn,
        &[node("page")],
        &[],
        &[RetentionRoot::Lease("reader".into())],
    )
    .await
    .expect_err("old MVCC snapshots cannot safely follow an advisory lock");
    assert_eq!(error.code, SnapshotErrorCode::Internal);
    assert!(error.message.contains("READ COMMITTED"));
    txn.commit().await.unwrap();
    assert!(repository.node("page").await.unwrap().is_none());
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn t06b_existing_parent_and_node_definition_are_immutable() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(&[node("parent"), node("child")], &[], &[])
        .await
        .unwrap();
    let mut changed = node("parent");
    changed.bytes += 1;
    assert!(repository.retain_group(&[changed], &[], &[]).await.is_err());
    assert!(
        repository
            .retain_group(&[node("parent")], &[edge("parent", "child")], &[])
            .await
            .is_err()
    );
    assert_eq!(repository.node("parent").await.unwrap().unwrap().bytes, 7);
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn t06b_gc_replay_subtracts_edges_once_and_tombstones_removed_identity() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(
            &[node("parent"), node("child")],
            &[edge("parent", "child")],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        repository
            .mark_deleting("parent-op", "parent")
            .await
            .unwrap(),
        GcClaim::Marked
    );
    let restarted = PostgresRetentionRepository::new(db.clone());
    let pending = restarted.pending_gc(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, "parent-op");
    assert_eq!(
        restarted
            .mark_deleting("parent-op", "parent")
            .await
            .unwrap(),
        GcClaim::Pending
    );
    assert_eq!(
        restarted
            .mark_deleting("another-op", "parent")
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    assert!(
        restarted
            .mark_deleting("parent-op", "different")
            .await
            .is_err()
    );

    // Simulated crash/rollback during bookkeeping: the PENDING intent and
    // child's reference survive and can be replayed after reaper replacement.
    let txn = db.begin().await.unwrap();
    let provisional = PostgresRetentionRepository::complete_gc_in_txn(&txn, "parent-op")
        .await
        .unwrap();
    assert_eq!(provisional.zero_reference_children, vec!["child"]);
    txn.rollback().await.unwrap();
    assert_eq!(
        restarted
            .node("child")
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        1
    );
    assert_eq!(restarted.pending_gc(10).await.unwrap().len(), 1);
    assert_eq!(
        restarted.node("parent").await.unwrap().unwrap().state,
        "DELETING"
    );

    let completed = restarted.complete_gc("parent-op").await.unwrap();
    assert!(!completed.replayed);
    assert_eq!(completed.zero_reference_children, vec!["child"]);
    assert!(restarted.node("parent").await.unwrap().is_none());
    assert_eq!(
        restarted
            .node("child")
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        0
    );
    assert!(restarted.complete_gc("parent-op").await.unwrap().replayed);
    assert_eq!(
        restarted
            .mark_deleting("parent-op", "parent")
            .await
            .unwrap(),
        GcClaim::Applied
    );
    assert!(restarted.pending_gc(10).await.unwrap().is_empty());
    let receipt = mst2_retention_gc_op::Entity::find_by_id("parent-op".to_owned())
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.attempts, 1);
    assert!(receipt.completed_at.is_some());

    // Old pending workers must not be able to delete same-id reconstructed
    // bytes. Reconstruction requires a future generation/fencing protocol.
    assert_eq!(
        restarted
            .retain_group(&[node("parent")], &[], &[])
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    assert!(restarted.complete_gc("parent-op").await.unwrap().replayed);
    assert!(restarted.node("parent").await.unwrap().is_none());
}

#[tokio::test]
async fn t06b_counter_audit_stops_gc_and_preserves_pending_edges() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(
            &[node("parent"), node("child")],
            &[edge("parent", "child")],
            &[],
        )
        .await
        .unwrap();
    db.execute_unprepared(
        "UPDATE mst2_retention_node SET incoming_refs = 0 WHERE node_id = 'child'",
    )
    .await
    .unwrap();
    assert_eq!(
        repository
            .mark_deleting("child-op", "child")
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        repository
            .mark_deleting("parent-op", "parent")
            .await
            .unwrap(),
        GcClaim::Marked
    );
    assert_eq!(
        repository.complete_gc("parent-op").await.unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(repository.pending_gc(10).await.unwrap().len(), 1);
    assert_eq!(
        repository.node("child").await.unwrap().unwrap().state,
        "LIVE"
    );
    db.execute_unprepared(
        "UPDATE mst2_retention_node SET incoming_refs = 1 WHERE node_id = 'child'",
    )
    .await
    .unwrap();
    repository.complete_gc("parent-op").await.unwrap();
    assert_eq!(
        repository
            .node("child")
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        0
    );
}

#[tokio::test]
async fn t06b_root_acquisition_wins_before_gc_cas() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(&[node("page")], &[], &[])
        .await
        .unwrap();
    let root_txn = db.begin().await.unwrap();
    PostgresRetentionRepository::retain_group_in_txn(
        &root_txn,
        &[node("page")],
        &[],
        &[RetentionRoot::Lease("winner".into())],
    )
    .await
    .unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let gc_repository = repository.clone();
    let mut gc = tokio::spawn(async move {
        started_tx.send(()).unwrap();
        gc_repository.mark_deleting("gc-op", "page").await
    });
    started_rx.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut gc)
            .await
            .is_err()
    );
    root_txn.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), gc)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GcClaim::Unavailable
    );
    assert_eq!(
        repository.node("page").await.unwrap().unwrap().state,
        "LIVE"
    );
    assert!(repository.pending_gc(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn t06b_gc_cas_wins_and_new_root_cannot_reference_deleting_node() {
    let (db, repository) = fixture().await;
    repository
        .retain_group(&[node("page")], &[], &[])
        .await
        .unwrap();
    let gc_txn = db.begin().await.unwrap();
    assert_eq!(
        PostgresRetentionRepository::mark_deleting_in_txn(&gc_txn, "gc-op", "page")
            .await
            .unwrap(),
        GcClaim::Marked
    );
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let acquire_repository = repository.clone();
    let mut acquire = tokio::spawn(async move {
        started_tx.send(()).unwrap();
        acquire_repository
            .retain_group(&[node("page")], &[], &[RetentionRoot::Lease("late".into())])
            .await
    });
    started_rx.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut acquire)
            .await
            .is_err()
    );
    gc_txn.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), acquire)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        mst2_retention_root::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(repository.pending_gc(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn t06b_forward_migration_backfills_old_graph_and_enforces_constraints() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = test_db_connection(temp.path()).await;
    let at = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20261005_000200_harden_mst2_retention_graph")
        .expect("hardening migration registered");
    Migrator::up(&db, Some(at.try_into().unwrap()))
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO mst2_retention_node (node_id, kind, state, bytes, created_at) \
         VALUES ('parent', 'page', 'LIVE', 7, now()), ('child', 'page', 'LIVE', 7, now()); \
         INSERT INTO mst2_retention_edge (parent_id, child_id) VALUES ('parent', 'child')",
    )
    .await
    .unwrap();
    Migrator::up(&db, None).await.unwrap();
    let repository = PostgresRetentionRepository::new(db.clone());
    assert_eq!(
        repository
            .node("child")
            .await
            .unwrap()
            .unwrap()
            .incoming_refs,
        1
    );
    assert!(
        db.execute_unprepared(
            "INSERT INTO mst2_retention_edge (parent_id, child_id) VALUES ('missing', 'child')"
        )
        .await
        .is_err()
    );
    assert!(
        db.execute_unprepared(
            "UPDATE mst2_retention_node SET incoming_refs = -1 WHERE node_id = 'child'"
        )
        .await
        .is_err()
    );
    assert!(db.execute_unprepared("INSERT INTO mst2_retention_gc_op (operation_id, node_id, operation, state) VALUES ('bad', 'child', 'REMOVE', 'APPLIED')").await.is_err());
}

#[tokio::test]
async fn t06b_forward_migration_refuses_existing_cycles() {
    let temp = tempfile::TempDir::new().unwrap();
    let db = test_db_connection(temp.path()).await;
    let at = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20261005_000200_harden_mst2_retention_graph")
        .unwrap();
    Migrator::up(&db, Some(at.try_into().unwrap()))
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO mst2_retention_node (node_id, kind, state, bytes, created_at) \
         VALUES ('a', 'page', 'LIVE', 7, now()), ('b', 'page', 'LIVE', 7, now()); \
         INSERT INTO mst2_retention_edge (parent_id, child_id) VALUES ('a', 'b'), ('b', 'a')",
    )
    .await
    .unwrap();
    assert!(Migrator::up(&db, None).await.is_err());
    assert_eq!(
        mst2_retention_edge::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        2
    );
}
