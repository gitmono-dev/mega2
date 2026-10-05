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
