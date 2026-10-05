use std::sync::Arc;

use sea_orm::{DatabaseTransaction, PaginatorTrait};

use super::*;
use crate::{
    callisto::{
        mst2_native_head, mst2_publication_outbox, sea_orm_active_enums::PushQueueKindEnum,
    },
    jupiter::{
        migration::apply_migrations,
        storage::{
            base_storage::BaseStorage,
            mst2_publication_storage::{PublicationPreparation, PublicationRequest},
            push_queue_storage::{ClaimOutcome, EnqueueOutcome, EnqueueParams},
        },
        tests::test_db_connection,
    },
};

const INSTANCE: &str = "6ab219b0-4275-45ba-9d7b-7b0b633018cd";

async fn fixture() -> (tempfile::TempDir, MonoStorage, PushQueueStorage) {
    let temp = tempfile::tempdir().unwrap();
    let db = test_db_connection(temp.path()).await;
    apply_migrations(&db, false).await.unwrap();
    let base = BaseStorage::new(Arc::new(db));
    let mono = MonoStorage { base: base.clone() };
    for (id, path, commit, tree) in [(1_i64, "/", 'a', 'b'), (2, "/project", 'c', 'd')] {
        mono.get_connection().execute_raw(Statement::from_sql_and_values(
            mono.get_connection().get_database_backend(),
            "INSERT INTO mega_refs (id,path,ref_name,ref_commit_hash,ref_tree_hash,created_at,updated_at,is_cl) \
             VALUES ($1,$2,$3,$4,$5,now(),now(),false)",
            [id.into(), path.into(), MEGA_BRANCH_NAME.into(), commit.to_string().repeat(40).into(), tree.to_string().repeat(40).into()],
        )).await.unwrap();
    }
    mono.initialize_native_publication(INSTANCE).await.unwrap();
    (
        temp,
        mono,
        PushQueueStorage::new(base).with_native_publication(true),
    )
}

async fn enqueue_claim(
    queue: &PushQueueStorage,
    old: &str,
    new: &str,
    n: u64,
) -> push_queue::Model {
    let operation = format!("{old}->{new}");
    let inserted = queue
        .enqueue_atomic(EnqueueParams {
            kind: PushQueueKindEnum::Push,
            operation_id: &operation,
            path: "/project",
            old_id: old,
            new_id: new,
            requester: Some("alice"),
            payload: serde_json::json!({"n":n,"commits":[new]}),
        })
        .await
        .unwrap();
    let EnqueueOutcome::Inserted { id } = inserted else {
        panic!("fresh operation");
    };
    assert_eq!(
        queue.claim_for_execution(id).await.unwrap(),
        ClaimOutcome::Claimed
    );
    queue.get_by_id(id).await.unwrap().unwrap()
}

async fn publish_same_root(
    mono: &MonoStorage,
    txn: &DatabaseTransaction,
    row: &push_queue::Model,
) -> i64 {
    let request = PublicationRequest::from_trunk_queue(row).unwrap();
    let PublicationPreparation::Prepared(origin) =
        mono.begin_publication_in_txn(txn, request).await.unwrap()
    else {
        panic!("fresh publication");
    };
    let native = mono
        .reserve_native_publication_in_txn(txn, row, INSTANCE)
        .await
        .unwrap();
    assert!(
        mono.cas_update_root_main_ref_in_txn(
            txn,
            row.expected_commit_hash.as_deref(),
            row.expected_tree_hash.as_deref(),
            row.expected_commit_hash.as_deref().unwrap(),
            row.expected_tree_hash.as_deref().unwrap()
        )
        .await
        .unwrap()
    );
    txn.execute_raw(Statement::from_sql_and_values(txn.get_database_backend(),
        "UPDATE mega_refs SET ref_commit_hash = $1 WHERE path='/project' AND ref_name=$2 AND is_cl=false",
        [row.new_id.clone().into(), MEGA_BRANCH_NAME.into()],
    )).await.unwrap();
    let committed = mono
        .record_publication_in_txn(
            txn,
            origin,
            row.expected_commit_hash.as_deref().unwrap(),
            &row.new_id,
        )
        .await
        .unwrap();
    mono.record_native_publication_in_txn(txn, native, &committed)
        .await
        .unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        txn.get_database_backend(),
        "UPDATE push_queue SET status='Done', landed_commit_id=$1 WHERE id=$2",
        [row.new_id.clone().into(), row.id.into()],
    ))
    .await
    .unwrap();
    committed.receipt.id
}

#[tokio::test]
async fn same_root_changes_advance_global_head_and_historical_replay_ignores_latest() {
    let (_temp, mono, queue) = fixture().await;
    let root_before = mono.get_main_ref("/").await.unwrap().unwrap();
    let first = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    assert_eq!(first.expected_native_sequence, Some(0));
    assert_eq!(first.expected_native_epoch, Some(1));
    assert_eq!(first.expected_native_certificate, None);
    let txn = mono.get_connection().begin().await.unwrap();
    let first_receipt = publish_same_root(&mono, &txn, &first).await;
    txn.commit().await.unwrap();
    let first_head = mono.read_native_publication_head(INSTANCE).await.unwrap();
    assert_eq!(first_head.token.sequence, 1);
    assert_eq!(first_head.root.commit, root_before.ref_commit_hash);
    assert_eq!(first_head.root.tree, root_before.ref_tree_hash);
    assert_eq!(first_head.token.certificate, Some(first_receipt));
    let second = enqueue_claim(&queue, &first.new_id, &"f".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    publish_same_root(&mono, &txn, &second).await;
    txn.commit().await.unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    let replay = mono
        .begin_publication_in_txn(&txn, PublicationRequest::from_trunk_queue(&first).unwrap())
        .await
        .unwrap();
    assert!(
        matches!(replay, PublicationPreparation::AlreadyCommitted(ref committed) if committed.receipt.id == first_receipt)
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        mono.read_native_publication_head(INSTANCE)
            .await
            .unwrap()
            .token
            .sequence,
        2
    );
    assert_eq!(
        mst2_native_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn stale_same_root_claim_token_rejects_before_mutation_and_reset_clears_the_group() {
    let (_temp, mono, queue) = fixture().await;
    let row = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    // Independent transaction advances the head without changing either root OID.
    mono.get_connection()
        .execute_unprepared("UPDATE mst2_native_head SET sequence=1 WHERE namespace='/'")
        .await
        .unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.reserve_native_publication_in_txn(&txn, &row, INSTANCE)
            .await,
        Err(PublicationReceiptError::Conflict(_))
    ));
    txn.rollback().await.unwrap();
    assert_eq!(
        mono.get_main_ref("/project")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash,
        "c".repeat(40)
    );
    assert!(queue.reset_running_to_queued(row.id).await.unwrap());
    let reset = queue.get_by_id(row.id).await.unwrap().unwrap();
    assert_eq!(
        (
            reset.expected_commit_hash,
            reset.expected_tree_hash,
            reset.expected_native_sequence,
            reset.expected_native_epoch,
            reset.expected_native_certificate
        ),
        (None, None, None, None, None)
    );
}

#[tokio::test]
async fn an_uncommitted_certificate_never_exposes_a_partial_head_to_another_connection() {
    let (_temp, mono, queue) = fixture().await;
    let first = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    publish_same_root(&mono, &txn, &first).await;
    txn.commit().await.unwrap();
    let row = enqueue_claim(&queue, &first.new_id, &"f".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    publish_same_root(&mono, &txn, &row).await;
    // The pool has two actual connections: one is held by txn, this read is on the other.
    let before = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        mono.read_native_publication_head(INSTANCE),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(before.token.sequence, 1);
    txn.commit().await.unwrap();
    let after = mono.read_native_publication_head(INSTANCE).await.unwrap();
    assert_eq!(after.token.sequence, 2);
    assert_ne!(before.token.certificate, after.token.certificate);
}

#[tokio::test]
async fn marked_history_fails_without_certificate_but_literal_v1_history_replays() {
    let (_temp, mono, queue) = fixture().await;
    let row = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    let receipt_id = publish_same_root(&mono, &txn, &row).await;
    txn.commit().await.unwrap();
    mono.get_connection()
        .execute_unprepared("DELETE FROM mst2_native_head; DELETE FROM mst2_native_publication;")
        .await
        .unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.begin_publication_in_txn(&txn, PublicationRequest::from_trunk_queue(&row).unwrap())
            .await,
        Err(PublicationReceiptError::Integrity(_))
    ));
    txn.rollback().await.unwrap();
    mono.get_connection()
        .execute_raw(Statement::from_sql_and_values(
            mono.get_connection().get_database_backend(),
            "UPDATE mst2_publication SET native_certificate_version=NULL WHERE id=$1",
            [receipt_id.into()],
        ))
        .await
        .unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.begin_publication_in_txn(&txn, PublicationRequest::from_trunk_queue(&row).unwrap())
            .await
            .unwrap(),
        PublicationPreparation::AlreadyCommitted(_)
    ));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn initialization_uses_only_a_counter_floor_and_cannot_reinitialize_or_serve_it() {
    let (_temp, mono, queue) = fixture().await;
    assert!(mono.read_native_publication_head(INSTANCE).await.is_err());
    assert!(mono.initialize_native_publication(INSTANCE).await.is_err());
    mono.get_connection().execute_unprepared("DELETE FROM mst2_native_head; INSERT INTO mst2_namespace_seq(namespace,sequence,epoch) VALUES('/older',19,1)").await.unwrap();
    mono.initialize_native_publication(INSTANCE).await.unwrap();
    let head = mst2_native_head::Entity::find()
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.sequence, 19);
    assert_eq!(head.state, "INITIALIZING");
    assert_eq!(head.certificate_receipt_id, None);
    let row = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    publish_same_root(&mono, &txn, &row).await;
    txn.commit().await.unwrap();
    assert_eq!(
        mono.read_native_publication_head(INSTANCE)
            .await
            .unwrap()
            .token
            .sequence,
        20
    );
}

#[tokio::test]
async fn reservation_owner_and_unchanged_selected_ref_cannot_issue_a_certificate() {
    let (_temp, mono, queue) = fixture().await;
    let row = enqueue_claim(&queue, &"c".repeat(40), &"e".repeat(40), 1).await;
    let txn = mono.get_connection().begin().await.unwrap();
    let native = mono
        .reserve_native_publication_in_txn(&txn, &row, INSTANCE)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    let other = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.finish_native_noop_in_txn(&other, native).await,
        Err(PublicationReceiptError::Conflict(_))
    ));
    other.rollback().await.unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    let native = mono
        .reserve_native_publication_in_txn(&txn, &row, INSTANCE)
        .await
        .unwrap();
    let PublicationPreparation::Prepared(origin) = mono
        .begin_publication_in_txn(&txn, PublicationRequest::from_trunk_queue(&row).unwrap())
        .await
        .unwrap()
    else {
        panic!("fresh publication");
    };
    let committed = mono
        .record_publication_in_txn(
            &txn,
            origin,
            row.expected_commit_hash.as_deref().unwrap(),
            &row.old_id,
        )
        .await
        .unwrap();
    assert!(
        mono.record_native_publication_in_txn(&txn, native, &committed)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        mst2_native_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
}

#[cfg(unix)]
pub(super) fn crash_checkpoint(phase: &str) {
    if std::env::var("MEGA_MST2_NATIVE_CRASH_PHASE")
        .ok()
        .as_deref()
        == Some(phase)
    {
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        panic!("SIGKILL did not terminate the native worker");
    }
}

async fn maintenance_fixture() -> (tempfile::TempDir, MonoStorage, PushQueueStorage, NativeRoot) {
    use git_internal::{
        hash::HashKind,
        internal::object::{
            blob::Blob,
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };

    let (temp, mono, queue) = fixture().await;
    mono.get_connection()
        .execute_unprepared("DELETE FROM mst2_native_head")
        .await
        .unwrap();
    queue
        .set_control_flags(Some(true), None, None)
        .await
        .unwrap();
    let blob =
        Blob::from_content_bytes_with_kind(HashKind::Sha1, b"maintenance root".to_vec()).unwrap();
    let tree = Tree::from_tree_items_with_kind(
        HashKind::Sha1,
        vec![TreeItem::new(
            TreeItemMode::Blob,
            blob.id,
            ".gitkeep".to_owned(),
        )],
    )
    .unwrap();
    let commit =
        Commit::from_tree_id_with_kind(HashKind::Sha1, tree.id, vec![], "maintenance root")
            .unwrap();
    mono.save_mega_trees(vec![tree.clone()], commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![commit.clone()], None)
        .await
        .unwrap();
    let root = NativeRoot {
        commit: commit.id.to_string(),
        tree: tree.id.to_string(),
    };
    mono.get_connection().execute_raw(Statement::from_sql_and_values(
        mono.get_connection().get_database_backend(),
        "UPDATE mega_refs SET ref_commit_hash=$1,ref_tree_hash=$2 WHERE path='/' AND ref_name=$3 AND is_cl=false",
        [root.commit.clone().into(), root.tree.clone().into(), MEGA_BRANCH_NAME.into()],
    )).await.unwrap();
    (temp, mono, queue, root)
}

async fn assert_no_maintenance_head(mono: &MonoStorage) {
    assert_eq!(
        mst2_native_head::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_native_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn maintenance_initializes_only_a_floor_and_preserves_the_paused_gate() {
    let (_temp, mono, queue, root) = maintenance_fixture().await;
    mono.get_connection()
        .execute_unprepared(
            "INSERT INTO mst2_namespace_seq(namespace,sequence,epoch) VALUES('/older',23,1)",
        )
        .await
        .unwrap();
    mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
        .await
        .unwrap();
    let before = mst2_native_head::Entity::find()
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.instance_id, INSTANCE);
    assert_eq!(before.sequence, 23);
    assert_eq!(before.writer_epoch, 1);
    assert_eq!(
        (&before.root_commit, &before.root_tree),
        (&root.commit, &root.tree)
    );
    assert_eq!(before.state, "INITIALIZING");
    assert_eq!(before.certificate_receipt_id, None);
    assert!(queue.get_control().await.unwrap().paused);
    assert!(mono.read_native_publication_head(INSTANCE).await.is_err());
    assert!(
        mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
            .await
            .is_err()
    );
    assert!(
        mono.initialize_native_publication_for_maintenance(
            "11111111-2222-4333-8444-555555555555",
            &root
        )
        .await
        .is_err()
    );
    assert_eq!(
        mst2_native_head::Entity::find()
            .one(mono.get_connection())
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        selected_ref(mono.get_connection(), "/").await.unwrap(),
        Some(root)
    );
    assert_eq!(
        mst2_native_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn maintenance_rejects_unpaused_missing_or_hard_stopped_control_without_writes() {
    for state in ["unpaused", "missing", "hard-stopped"] {
        let (_temp, mono, queue, root) = maintenance_fixture().await;
        match state {
            "unpaused" => queue
                .set_control_flags(Some(false), None, None)
                .await
                .unwrap(),
            "hard-stopped" => queue
                .set_control_flags(None, Some(true), None)
                .await
                .unwrap(),
            "missing" => {
                mono.get_connection()
                    .execute_unprepared("DELETE FROM queue_control")
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
                .await
                .is_err(),
            "{state}"
        );
        assert_no_maintenance_head(&mono).await;
        let control = crate::callisto::queue_control::Entity::find()
            .one(mono.get_connection())
            .await
            .unwrap();
        match state {
            "missing" => assert!(control.is_none()),
            "hard-stopped" => assert!(control.unwrap().hard_stopped),
            _ => assert!(!control.unwrap().paused),
        }
    }
}

#[tokio::test]
async fn maintenance_refuses_queued_and_running_work_until_drained() {
    for status in ["Queued", "Running"] {
        let (_temp, mono, queue, root) = maintenance_fixture().await;
        queue
            .set_control_flags(Some(false), None, None)
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = queue
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "maintenance-pending",
                path: "/project",
                old_id: &"c".repeat(40),
                new_id: &"e".repeat(40),
                requester: None,
                payload: serde_json::json!({"n":1,"commits":["e".repeat(40)]}),
            })
            .await
            .unwrap()
        else {
            panic!("fresh pending operation");
        };
        mono.get_connection()
            .execute_raw(Statement::from_sql_and_values(
                mono.get_connection().get_database_backend(),
                "UPDATE push_queue SET status=$1::push_queue_status_enum WHERE id=$2",
                [status.into(), id.into()],
            ))
            .await
            .unwrap();
        queue
            .set_control_flags(Some(true), None, None)
            .await
            .unwrap();
        assert!(
            mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
                .await
                .is_err(),
            "{status}"
        );
        assert_no_maintenance_head(&mono).await;
        assert!(queue.get_control().await.unwrap().paused);
    }
}

#[tokio::test]
async fn maintenance_refuses_busy_writer_or_admission_locks_and_can_retry() {
    for lock in ["writer", "admission"] {
        let (_temp, mono, _queue, root) = maintenance_fixture().await;
        let other = mono.get_connection().begin().await.unwrap();
        if lock == "writer" {
            PushQueueStorage::acquire_mono_write_lock(&other)
                .await
                .unwrap();
        } else {
            other
                .execute_unprepared("SELECT id FROM queue_control WHERE id=1 FOR UPDATE")
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mono.initialize_native_publication_for_maintenance(INSTANCE, &root),
        )
        .await;
        assert!(
            result
                .expect("maintenance lock refusal must be bounded")
                .is_err(),
            "{lock}"
        );
        assert_no_maintenance_head(&mono).await;
        other.rollback().await.unwrap();
        mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn maintenance_rejects_wrong_or_ambiguous_root_and_invalid_counters() {
    for damage in [
        "commit",
        "tree",
        "missing",
        "ambiguous",
        "negative",
        "exhausted",
    ] {
        let (_temp, mono, _queue, mut root) = maintenance_fixture().await;
        match damage {
            "commit" => root.commit = "e".repeat(40),
            "tree" => root.tree = "e".repeat(40),
            "missing" => {
                mono.get_connection()
                    .execute_unprepared("DELETE FROM mega_refs WHERE path='/'")
                    .await
                    .unwrap();
            }
            "ambiguous" => {
                // Simulate a corrupted/old schema, independently of the normal
                // uniqueness protection on selected refs.
                mono.get_connection()
                    .execute_unprepared("DROP INDEX uniq_mref_path")
                    .await
                    .unwrap();
                mono.get_connection().execute_unprepared("INSERT INTO mega_refs(id,path,ref_name,ref_commit_hash,ref_tree_hash,created_at,updated_at,is_cl) SELECT 3,path,ref_name,ref_commit_hash,ref_tree_hash,created_at,updated_at,is_cl FROM mega_refs WHERE path='/'").await.unwrap();
            }
            "negative" | "exhausted" => {
                let floor = if damage == "negative" {
                    -1_i64
                } else {
                    i64::MAX
                };
                mono.get_connection().execute_raw(Statement::from_sql_and_values(
                    mono.get_connection().get_database_backend(),
                    "INSERT INTO mst2_namespace_seq(namespace,sequence,epoch) VALUES('/older',$1,1)", [floor.into()],
                )).await.unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
                .await
                .is_err(),
            "{damage}"
        );
        assert_no_maintenance_head(&mono).await;
    }
}

#[tokio::test]
async fn maintenance_rejects_nil_instance_and_non_sha1_expected_roots() {
    let (_temp, mono, _queue, root) = maintenance_fixture().await;
    for instance in ["invalid", "00000000-0000-0000-0000-000000000000"] {
        assert!(
            mono.initialize_native_publication_for_maintenance(instance, &root)
                .await
                .is_err()
        );
    }
    for commit in ["a".repeat(64), "A".repeat(40)] {
        let wrong = NativeRoot {
            commit,
            tree: root.tree.clone(),
        };
        assert!(
            mono.initialize_native_publication_for_maintenance(INSTANCE, &wrong)
                .await
                .is_err()
        );
    }
    assert_no_maintenance_head(&mono).await;
}

#[tokio::test]
async fn maintenance_refuses_missing_commit_tree_or_a_mismatched_commit_tree() {
    for damage in ["commit", "tree", "wrong-tree"] {
        let (_temp, mono, _queue, root) = maintenance_fixture().await;
        let sql = match damage {
            "commit" => "DELETE FROM mega_commit WHERE commit_id=$1",
            "tree" => "DELETE FROM mega_tree WHERE tree_id=$1",
            "wrong-tree" => "UPDATE mega_commit SET tree=repeat('e',40) WHERE commit_id=$1",
            _ => unreachable!(),
        };
        let id = if damage == "tree" {
            &root.tree
        } else {
            &root.commit
        };
        mono.get_connection()
            .execute_raw(Statement::from_sql_and_values(
                mono.get_connection().get_database_backend(),
                sql,
                [id.clone().into()],
            ))
            .await
            .unwrap();
        assert!(
            mono.initialize_native_publication_for_maintenance(INSTANCE, &root)
                .await
                .is_err(),
            "{damage}"
        );
        assert_no_maintenance_head(&mono).await;
        assert_eq!(
            selected_ref(mono.get_connection(), "/").await.unwrap(),
            Some(root)
        );
    }
}
