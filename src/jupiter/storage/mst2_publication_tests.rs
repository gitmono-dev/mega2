use std::sync::Arc;

use sea_orm::{PaginatorTrait, TransactionTrait};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::jupiter::{
    migration::{Migrator, apply_migrations},
    storage::{
        base_storage::{BaseStorage, StorageConnector},
        push_queue_storage::{ClaimOutcome, EnqueueOutcome, EnqueueParams, PushQueueStorage},
    },
    tests::test_db_connection,
};

fn queue_row() -> push_queue::Model {
    let now = chrono::Utc::now().fixed_offset();
    push_queue::Model {
        id: 7,
        kind: PushQueueKindEnum::Push,
        operation_id: "old→new".to_owned(),
        path: "/project".to_owned(),
        old_id: "old".to_owned(),
        new_id: "new".to_owned(),
        payload: serde_json::json!({"commits": ["new"], "n": 1, "fork_base": "old"}),
        landed_commit_id: None,
        status: crate::callisto::sea_orm_active_enums::PushQueueStatusEnum::Running,
        requester: Some("alice".to_owned()),
        failure_type: None,
        error_message: None,
        heartbeat_at: now,
        superseded_by: None,
        expected_commit_hash: Some("root-before".to_owned()),
        expected_tree_hash: Some("tree-before".to_owned()),
        expected_native_sequence: None,
        expected_native_epoch: None,
        expected_native_certificate: None,
        pending_action: None,
        enqueued_at: now,
        started_at: Some(now),
        finished_at: None,
        updated_at: now,
    }
}

fn noop_queue_row() -> push_queue::Model {
    let mut row = queue_row();
    row.old_id = "tip".to_owned();
    row.new_id = "tip".to_owned();
    row.payload = serde_json::json!({"commits": [], "fork_base": null, "n": 0});
    row
}

#[test]
fn queue_digest_is_versioned_and_binds_trusted_intent_not_retry_baseline() {
    let row = queue_row();
    let original = PublicationRequest::from_trunk_queue(&row).unwrap();
    assert_eq!(original.operation_id, "mst2:trunk-queue:7");
    let mut retry = row.clone();
    retry.expected_commit_hash = Some("later-root".to_owned());
    retry.expected_tree_hash = Some("later-tree".to_owned());
    retry.started_at = None;
    retry.payload = serde_json::from_str(r#"{"n":1,"fork_base":"old","commits":["new"]}"#).unwrap();
    assert_eq!(
        PublicationRequest::from_trunk_queue(&retry)
            .unwrap()
            .request_digest,
        original.request_digest
    );
    for change in ["actor", "path", "old", "new", "payload", "identity"] {
        let mut changed = row.clone();
        match change {
            "actor" => changed.requester = Some("bob".to_owned()),
            "path" => changed.path = "/other".to_owned(),
            "old" => changed.old_id = "other-old".to_owned(),
            "new" => changed.new_id = "other-new".to_owned(),
            "payload" => changed.payload["commits"] = serde_json::json!(["new", "extra"]),
            "identity" => changed.id += 1,
            _ => unreachable!(),
        }
        assert_ne!(
            PublicationRequest::from_trunk_queue(&changed)
                .unwrap()
                .request_digest,
            original.request_digest,
            "{change}"
        );
    }
}

async fn storage() -> (tempfile::TempDir, MonoStorage) {
    let temp = tempfile::tempdir().unwrap();
    let db = test_db_connection(temp.path()).await;
    apply_migrations(&db, false).await.unwrap();
    (
        temp,
        MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        },
    )
}

async fn seed_root(mono: &MonoStorage) {
    mono.get_connection().execute_raw(Statement::from_sql_and_values(
        mono.get_connection().get_database_backend(),
        "INSERT INTO mega_refs (id, path, ref_name, ref_commit_hash, ref_tree_hash, created_at, updated_at, is_cl) \
         VALUES (1, '/', $1, $2, $3, now(), now(), false)",
        [MEGA_BRANCH_NAME.into(), "a".repeat(40).into(), "b".repeat(40).into()],
    )).await.unwrap();
}

async fn prepared(
    mono: &MonoStorage,
    txn: &DatabaseTransaction,
    request: PublicationRequest,
) -> PreparedPublication {
    match mono.begin_publication_in_txn(txn, request).await.unwrap() {
        PublicationPreparation::Prepared(prepared) => prepared,
        PublicationPreparation::AlreadyCommitted(_) => panic!("fresh operation"),
        PublicationPreparation::AlreadyCommittedNoop(_) => panic!("fresh no-op operation"),
    }
}

async fn advance(mono: &MonoStorage, txn: &DatabaseTransaction) -> bool {
    mono.cas_update_root_main_ref_in_txn(
        txn,
        Some(&"a".repeat(40)),
        Some(&"b".repeat(40)),
        &"c".repeat(40),
        &"d".repeat(40),
    )
    .await
    .unwrap()
}

async fn counts(mono: &MonoStorage) -> (u64, u64) {
    (
        mst2_publication::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        mst2_publication_outbox::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn merge_stub_admission_replays_same_receipt_and_rejects_changed_intent() {
    let (_temp, mono) = storage().await;
    seed_root(&mono).await;
    let queue = PushQueueStorage::new(mono.base.clone());
    let payload = serde_json::json!({"steps": ["first"], "mode": "stub"});
    let admitted = queue
        .enqueue_atomic(EnqueueParams {
            kind: PushQueueKindEnum::Merge,
            operation_id: "merge-stub-admission",
            path: "/project",
            old_id: "old",
            new_id: "new",
            requester: Some("alice"),
            payload: payload.clone(),
        })
        .await
        .unwrap();
    let EnqueueOutcome::Inserted { id } = admitted else {
        panic!("{admitted:?}");
    };
    // With no committed receipt, admission retains the existing adoption
    // behavior and the original persisted intent remains the execution input.
    assert_eq!(
        queue
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "merge-stub-admission",
                path: "/project",
                old_id: "old",
                new_id: "new",
                requester: Some("legacy-retry"),
                payload: serde_json::json!({"different": "no receipt yet"}),
            })
            .await
            .unwrap(),
        EnqueueOutcome::Adopted { id }
    );
    assert_eq!(
        queue.claim_for_execution(id).await.unwrap(),
        ClaimOutcome::Claimed
    );
    let row = push_queue::Entity::find_by_id(id)
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.requester.as_deref(), Some("alice"));
    assert_eq!(row.payload, payload);
    let txn = mono.get_connection().begin().await.unwrap();
    let reservation = prepared(
        &mono,
        &txn,
        PublicationRequest::from_trunk_queue(&row).unwrap(),
    )
    .await;
    assert!(advance(&mono, &txn).await);
    let landed = "c".repeat(40);
    assert!(
        PushQueueStorage::mark_done_if_running_in_txn(&txn, id, &landed)
            .await
            .unwrap()
    );
    let original = mono
        .record_publication_in_txn(&txn, reservation, &"a".repeat(40), &landed)
        .await
        .unwrap();
    assert_eq!(original.receipt.writer_kind, "trunk_merge_stub");
    txn.commit().await.unwrap();
    assert_eq!(
        queue
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "merge-stub-admission",
                path: "/project",
                old_id: "old",
                new_id: "new",
                requester: Some("alice"),
                payload: payload.clone(),
            })
            .await
            .unwrap(),
        EnqueueOutcome::Replay {
            id,
            landed_commit_id: Some(landed.clone())
        }
    );
    for change in ["actor", "payload"] {
        let (requester, retry_payload) = if change == "actor" {
            ("mallory", payload.clone())
        } else {
            (
                "alice",
                serde_json::json!({"steps": ["first", "changed"], "mode": "stub"}),
            )
        };
        let error = queue
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "merge-stub-admission",
                path: "/project",
                old_id: "old",
                new_id: "new",
                requester: Some(requester),
                payload: retry_payload,
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("MST2_PUBLICATION_CONFLICT"),
            "{change}"
        );
    }
    assert_eq!(
        mst2_publication::Entity::find()
            .all(mono.get_connection())
            .await
            .unwrap(),
        vec![original.receipt]
    );
    assert_eq!(
        mst2_publication_outbox::Entity::find()
            .all(mono.get_connection())
            .await
            .unwrap(),
        vec![original.outbox]
    );
    assert_eq!(mono.publication_sequence("/project").await.unwrap(), 1);
    assert_eq!(
        mono.get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash,
        landed
    );
}

#[tokio::test]
async fn same_request_concurrently_returns_one_original_receipt_and_outbox() {
    let (_temp, mono) = storage().await;
    seed_root(&mono).await;
    let request = PublicationRequest::for_test("same-op", "/", "a", "c", "test");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let mono = mono.clone();
        let request = request.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(tokio::spawn(async move {
            let txn = mono.get_connection().begin().await.unwrap();
            barrier.wait().await;
            let (committed, wrote) =
                match mono.begin_publication_in_txn(&txn, request).await.unwrap() {
                    PublicationPreparation::AlreadyCommitted(committed) => (committed, false),
                    PublicationPreparation::AlreadyCommittedNoop(_) => {
                        panic!("publication request")
                    }
                    PublicationPreparation::Prepared(prepared) => {
                        assert!(advance(&mono, &txn).await);
                        (
                            mono.record_publication_in_txn(&txn, prepared, "a", "c")
                                .await
                                .unwrap(),
                            true,
                        )
                    }
                };
            txn.commit().await.unwrap();
            (committed, wrote)
        }));
    }
    let mut results = Vec::new();
    for worker in workers {
        results.push(
            tokio::time::timeout(std::time::Duration::from_secs(30), worker)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(results.iter().filter(|(_, wrote)| *wrote).count(), 1);
    assert_eq!(results[0].0.receipt, results[1].0.receipt);
    assert_eq!(results[0].0.outbox, results[1].0.outbox);
    assert_eq!(mono.publication_sequence("/").await.unwrap(), 1);
    assert_eq!(counts(&mono).await, (1, 1));
}

#[tokio::test]
async fn changed_request_conflicts_before_ref_writes() {
    let (_temp, mono) = storage().await;
    seed_root(&mono).await;
    let txn = mono.get_connection().begin().await.unwrap();
    let request = PublicationRequest::from_trunk_queue(&queue_row()).unwrap();
    let reserved = prepared(&mono, &txn, request).await;
    assert!(advance(&mono, &txn).await);
    mono.record_publication_in_txn(&txn, reserved, "a", "c")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    for change in ["actor", "new", "payload"] {
        let mut changed = queue_row();
        match change {
            "actor" => changed.requester = Some("mallory".to_owned()),
            "new" => changed.new_id = "overwrite".to_owned(),
            "payload" => changed.payload["commits"] = serde_json::json!(["injected"]),
            _ => unreachable!(),
        }
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(matches!(
            mono.begin_publication_in_txn(
                &txn,
                PublicationRequest::from_trunk_queue(&changed).unwrap()
            )
            .await,
            Err(PublicationReceiptError::Conflict(_))
        ));
        txn.rollback().await.unwrap();
        assert_eq!(
            mono.get_main_ref("/")
                .await
                .unwrap()
                .unwrap()
                .ref_commit_hash,
            "c".repeat(40)
        );
        assert_eq!(mono.publication_sequence("/project").await.unwrap(), 1);
        assert_eq!(counts(&mono).await, (1, 1));
    }
}

#[tokio::test]
async fn different_operations_compete_on_actual_root_cas_without_extra_sequence() {
    let (_temp, mono) = storage().await;
    seed_root(&mono).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for i in 0..2 {
        let mono = mono.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(tokio::spawn(async move {
            let txn = mono.get_connection().begin().await.unwrap();
            barrier.wait().await;
            let request =
                PublicationRequest::for_test(&format!("competitor-{i}"), "/", "a", "c", "test");
            let reserved = prepared(&mono, &txn, request).await;
            if !advance(&mono, &txn).await {
                txn.rollback().await.unwrap();
                return false;
            }
            mono.record_publication_in_txn(&txn, reserved, "a", "c")
                .await
                .unwrap();
            txn.commit().await.unwrap();
            true
        }));
    }
    let mut winners = 0;
    for worker in workers {
        winners += usize::from(
            tokio::time::timeout(std::time::Duration::from_secs(30), worker)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(winners, 1);
    assert_eq!(counts(&mono).await, (1, 1));
    assert_eq!(mono.publication_sequence("/").await.unwrap(), 1);
}

#[tokio::test]
async fn reservation_cannot_be_finalized_from_another_transaction() {
    let (_temp, mono) = storage().await;
    // Keep the namespace/sequence unchanged after rollback: only the txn id differs.
    mono.get_connection()
        .execute_unprepared(
            "INSERT INTO mst2_namespace_seq (namespace, sequence, epoch) VALUES ('/', 0, 1)",
        )
        .await
        .unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    let reserved = prepared(
        &mono,
        &txn,
        PublicationRequest::for_test("txn-op", "/", "a", "c", "test"),
    )
    .await;
    txn.rollback().await.unwrap();
    let other = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.record_publication_in_txn(&other, reserved, "a", "c")
            .await,
        Err(PublicationReceiptError::Conflict(_))
    ));
    other.rollback().await.unwrap();
    assert_eq!(counts(&mono).await, (0, 0));
    assert_eq!(mono.publication_sequence("/").await.unwrap(), 0);
}

#[tokio::test]
async fn legacy_receipt_and_queue_alias_do_not_guess_request_digest() {
    let (_temp, mono) = storage().await;
    let row = queue_row();
    mono.get_connection().execute_raw(Statement::from_sql_and_values(
        mono.get_connection().get_database_backend(),
        "INSERT INTO mst2_publication (operation_id, namespace, sequence, old_oid, new_oid, writer_epoch, writer_kind, created_at) \
         VALUES ($1, $2, 1, 'old', 'new', 1, 'trunk_push', now())",
        [row.operation_id.clone().into(), row.path.clone().into()],
    )).await.unwrap();
    for request in [
        PublicationRequest::for_test(&row.operation_id, &row.path, "old", "new", "trunk_push"),
        PublicationRequest::from_trunk_queue(&row).unwrap(),
    ] {
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(matches!(
            mono.begin_publication_in_txn(&txn, request).await,
            Err(PublicationReceiptError::LegacyReceipt(_))
        ));
        txn.rollback().await.unwrap();
    }
    let txn = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.validate_queue_publication_replay_in_txn(&txn, &row)
            .await,
        Err(PublicationReceiptError::LegacyReceipt(_))
    ));
    txn.rollback().await.unwrap();
    let legacy = mst2_publication::Entity::find()
        .one(mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert!(legacy.request_digest.is_none());
    assert!(legacy.request_digest_version.is_none());
    assert_eq!(counts(&mono).await, (1, 0));
}

#[tokio::test]
async fn incomplete_outbox_refuses_both_receipt_and_queue_replay() {
    for corruption in ["missing", "sequence", "namespace"] {
        let (_temp, mono) = storage().await;
        seed_root(&mono).await;
        let row = queue_row();
        let request = PublicationRequest::from_trunk_queue(&row).unwrap();
        let txn = mono.get_connection().begin().await.unwrap();
        let reserved = prepared(&mono, &txn, request.clone()).await;
        assert!(advance(&mono, &txn).await);
        mono.record_publication_in_txn(&txn, reserved, "a", "c")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let sql = match corruption {
            "missing" => "DELETE FROM mst2_publication_outbox",
            "sequence" => "UPDATE mst2_publication_outbox SET sequence = sequence + 1",
            "namespace" => "UPDATE mst2_publication_outbox SET namespace = '/other'",
            _ => unreachable!(),
        };
        mono.get_connection().execute_unprepared(sql).await.unwrap();
        let before = counts(&mono).await;
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(
            matches!(
                mono.begin_publication_in_txn(&txn, request).await,
                Err(PublicationReceiptError::Integrity(_))
            ),
            "{corruption}"
        );
        txn.rollback().await.unwrap();
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(
            matches!(
                mono.validate_queue_publication_replay_in_txn(&txn, &row)
                    .await,
                Err(PublicationReceiptError::Integrity(_))
            ),
            "{corruption}"
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            mono.get_main_ref("/")
                .await
                .unwrap()
                .unwrap()
                .ref_commit_hash,
            "c".repeat(40)
        );
        assert_eq!(mono.publication_sequence(&row.path).await.unwrap(), 1);
        assert_eq!(counts(&mono).await, before);
    }
}

#[tokio::test]
async fn concurrent_noop_operations_keep_the_publication_sequence_and_outbox() {
    let (_temp, mono) = storage().await;
    seed_root(&mono).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let mono = mono.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(tokio::spawn(async move {
            let txn = mono.get_connection().begin().await.unwrap();
            barrier.wait().await;
            let (receipt, wrote) = match mono
                .begin_publication_in_txn(
                    &txn,
                    PublicationRequest::from_trunk_queue(&noop_queue_row()).unwrap(),
                )
                .await
                .unwrap()
            {
                PublicationPreparation::Prepared(reserved) => {
                    assert!(
                        mono.cas_update_root_main_ref_in_txn(
                            &txn,
                            Some(&"a".repeat(40)),
                            Some(&"b".repeat(40)),
                            &"a".repeat(40),
                            &"b".repeat(40)
                        )
                        .await
                        .unwrap()
                    );
                    (
                        mono.record_noop_operation_in_txn(
                            &txn,
                            reserved,
                            &"a".repeat(40),
                            &"b".repeat(40),
                            "tip",
                        )
                        .await
                        .unwrap(),
                        true,
                    )
                }
                PublicationPreparation::AlreadyCommittedNoop(receipt) => (receipt, false),
                PublicationPreparation::AlreadyCommitted(_) => panic!("no-op request"),
            };
            txn.commit().await.unwrap();
            (receipt, wrote)
        }));
    }
    let mut results = Vec::new();
    for worker in workers {
        results.push(
            tokio::time::timeout(std::time::Duration::from_secs(30), worker)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(results.iter().filter(|(_, wrote)| *wrote).count(), 1);
    assert_eq!(results[0].0, results[1].0);
    assert_eq!(results[0].0.observed_sequence, 0);
    assert_eq!(mono.publication_sequence("/project").await.unwrap(), 0);
    assert_eq!(counts(&mono).await, (0, 0));
    assert_eq!(
        mst2_queue_noop_receipt::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        1
    );
    for change in ["actor", "payload"] {
        let mut row = noop_queue_row();
        if change == "actor" {
            row.requester = Some("other".to_owned());
        } else {
            row.payload["fork_base"] = serde_json::json!("other");
        }
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(matches!(
            mono.begin_publication_in_txn(
                &txn,
                PublicationRequest::from_trunk_queue(&row).unwrap()
            )
            .await,
            Err(PublicationReceiptError::Conflict(_))
        ));
        txn.rollback().await.unwrap();
        let txn = mono.get_connection().begin().await.unwrap();
        assert!(matches!(
            mono.validate_queue_publication_replay_in_txn(&txn, &row)
                .await,
            Err(PublicationReceiptError::Conflict(_))
        ));
        txn.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn noop_receipt_replay_rejects_unsupported_version_and_publication_corruption() {
    for corruption in ["version", "publication", "outbox"] {
        let (_temp, mono) = storage().await;
        let row = noop_queue_row();
        let request = PublicationRequest::from_trunk_queue(&row).unwrap();
        let txn = mono.get_connection().begin().await.unwrap();
        let reserved = prepared(&mono, &txn, request.clone()).await;
        mono.record_noop_operation_in_txn(&txn, reserved, "root", "tree", "tip")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let sql = match corruption {
            "version" => "UPDATE mst2_queue_noop_receipt SET request_digest_version = 2",
            "publication" => {
                "INSERT INTO mst2_publication (operation_id, namespace, sequence, old_oid, new_oid, writer_epoch, writer_kind, created_at) \
                              VALUES ('mst2:trunk-queue:7', '/project', 1, 'a', 'b', 1, 'trunk_push', now())"
            }
            "outbox" => {
                "INSERT INTO mst2_publication_outbox (operation_id, namespace, sequence, state, created_at) \
                         VALUES ('mst2:trunk-queue:7', '/project', 1, 'PENDING', now())"
            }
            _ => unreachable!(),
        };
        mono.get_connection().execute_unprepared(sql).await.unwrap();
        let publications_before = mst2_publication::Entity::find()
            .all(mono.get_connection())
            .await
            .unwrap();
        let outbox_before = mst2_publication_outbox::Entity::find()
            .all(mono.get_connection())
            .await
            .unwrap();
        for admission in [false, true] {
            let txn = mono.get_connection().begin().await.unwrap();
            let result = if admission {
                mono.validate_queue_publication_replay_in_txn(&txn, &row)
                    .await
            } else {
                mono.begin_publication_in_txn(&txn, request.clone())
                    .await
                    .map(|_| ())
            };
            if corruption == "version" {
                assert!(matches!(
                    result,
                    Err(PublicationReceiptError::LegacyReceipt(_))
                ));
            } else {
                assert!(matches!(result, Err(PublicationReceiptError::Integrity(_))));
            }
            txn.rollback().await.unwrap();
            let namespace = mono
                .get_connection()
                .query_one_raw(Statement::from_sql_and_values(
                    mono.get_connection().get_database_backend(),
                    "SELECT sequence FROM mst2_namespace_seq WHERE namespace = $1",
                    [row.path.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(namespace.try_get::<i64>("", "sequence").unwrap(), 0);
            assert_eq!(
                mst2_publication::Entity::find()
                    .all(mono.get_connection())
                    .await
                    .unwrap(),
                publications_before
            );
            assert_eq!(
                mst2_publication_outbox::Entity::find()
                    .all(mono.get_connection())
                    .await
                    .unwrap(),
                outbox_before
            );
        }
        assert_eq!(
            mst2_queue_noop_receipt::Entity::find()
                .count(mono.get_connection())
                .await
                .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn noop_reservation_cannot_move_to_another_transaction() {
    let (_temp, mono) = storage().await;
    mono.get_connection()
        .execute_unprepared(
            "INSERT INTO mst2_namespace_seq (namespace, sequence, epoch) VALUES ('/project', 0, 1)",
        )
        .await
        .unwrap();
    let txn = mono.get_connection().begin().await.unwrap();
    let reserved = prepared(
        &mono,
        &txn,
        PublicationRequest::from_trunk_queue(&noop_queue_row()).unwrap(),
    )
    .await;
    txn.rollback().await.unwrap();
    let other = mono.get_connection().begin().await.unwrap();
    assert!(matches!(
        mono.record_noop_operation_in_txn(&other, reserved, "root", "tree", "tip")
            .await,
        Err(PublicationReceiptError::Conflict(_))
    ));
    other.rollback().await.unwrap();
    assert_eq!(mono.publication_sequence("/project").await.unwrap(), 0);
    assert_eq!(counts(&mono).await, (0, 0));
    assert_eq!(
        mst2_queue_noop_receipt::Entity::find()
            .count(mono.get_connection())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn migration_preserves_legacy_receipts_and_validates_digest_columns() {
    let temp = tempfile::tempdir().unwrap();
    let db = test_db_connection(temp.path()).await;
    let old_count = u32::try_from(Migrator::migrations().len() - 2).unwrap();
    Migrator::up(&db, Some(old_count)).await.unwrap();
    db.execute_unprepared(
        "INSERT INTO mst2_publication (operation_id, namespace, sequence, old_oid, new_oid, writer_epoch, writer_kind, created_at) \
         VALUES ('legacy', '/', 1, 'a', 'b', 1, 'test', now())"
    ).await.unwrap();
    apply_migrations(&db, false).await.unwrap();
    let latest = Migrator::migrations().pop().unwrap();
    latest
        .up(&sea_orm_migration::SchemaManager::new(&db))
        .await
        .unwrap();
    let legacy = mst2_publication::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(legacy.request_digest.is_none());
    assert!(legacy.request_digest_version.is_none());
    for (digest, version) in [
        (Some(format!("sha256:{}", "a".repeat(64))), Some(1)),
        (Some(format!("sha256:{}", "A".repeat(64))), Some(1)),
        (Some("broken".to_owned()), Some(1)),
        (None, Some(1)),
        (Some(format!("sha256:{}", "a".repeat(64))), None),
        (Some(format!("sha256:{}", "a".repeat(64))), Some(0)),
    ] {
        let valid = digest.as_deref() == Some(format!("sha256:{}", "a".repeat(64)).as_str())
            && version == Some(1);
        let txn = db.begin().await.unwrap();
        let result = txn.execute_raw(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "UPDATE mst2_publication SET request_digest = $1, request_digest_version = $2 WHERE operation_id = 'legacy'",
            [digest.into(), version.into()],
        )).await;
        assert_eq!(result.is_ok(), valid);
        txn.rollback().await.unwrap();
    }
    let legacy = mst2_publication::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(legacy.request_digest.is_none());
}

#[cfg(unix)]
pub(super) fn crash_checkpoint(phase: &str) {
    if std::env::var("MEGA_MST2_RECEIPT_CRASH_PHASE")
        .ok()
        .as_deref()
        == Some(phase)
    {
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        panic!("SIGKILL did not terminate the worker");
    }
}

#[cfg(unix)]
#[test]
fn receipt_crash_worker() {
    let Ok(url) = std::env::var("MEGA_MST2_RECEIPT_WORKER_DB") else {
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let db = sea_orm::Database::connect(url).await.unwrap();
            let mono = MonoStorage {
                base: BaseStorage::new(Arc::new(db)),
            };
            let txn = mono.get_connection().begin().await.unwrap();
            let noop = std::env::var("MEGA_MST2_RECEIPT_WORKER_NOOP").is_ok();
            let request = if noop {
                PublicationRequest::from_trunk_queue(&noop_queue_row()).unwrap()
            } else {
                PublicationRequest::for_test("process-op", "/", "a", "c", "test")
            };
            let reserved = prepared(&mono, &txn, request).await;
            if noop {
                assert!(
                    mono.cas_update_root_main_ref_in_txn(
                        &txn,
                        Some(&"a".repeat(40)),
                        Some(&"b".repeat(40)),
                        &"a".repeat(40),
                        &"b".repeat(40)
                    )
                    .await
                    .unwrap()
                );
            } else {
                assert!(advance(&mono, &txn).await);
            }
            crash_checkpoint("ref-written");
            if noop {
                mono.record_noop_operation_in_txn(
                    &txn,
                    reserved,
                    &"a".repeat(40),
                    &"b".repeat(40),
                    "tip",
                )
                .await
                .unwrap();
            } else {
                mono.record_publication_in_txn(&txn, reserved, "a", "c")
                    .await
                    .unwrap();
            }
            txn.commit().await.unwrap();
            crash_checkpoint("commit-complete");
        });
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_recovery_and_commit_without_response_use_original_receipt() {
    use std::os::unix::process::ExitStatusExt;

    for phase in [
        "ref-written",
        "receipt-written",
        "outbox-written",
        "commit-complete",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (config, _schema) = crate::jupiter::tests::test_db_config(temp.path()).await;
        let db = crate::jupiter::storage::init::database_connection(&config)
            .await
            .unwrap();
        let mono = MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        seed_root(&mono).await;
        let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jupiter::storage::mst2_publication_storage::tests::receipt_crash_worker",
                "--nocapture",
            ])
            .env("MEGA_MST2_RECEIPT_WORKER_DB", &config.db_url)
            .env("MEGA_MST2_RECEIPT_CRASH_PHASE", phase)
            .env_remove("MEGA_MST2_RECEIPT_WORKER_NOOP")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = worker.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                worker.kill().unwrap();
                worker.wait().unwrap();
                panic!("receipt worker timed out at {phase}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{phase}: {status}");
        let committed = phase == "commit-complete";
        let root = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(
            root.ref_commit_hash,
            if committed { "c" } else { "a" }.repeat(40)
        );
        assert_eq!(counts(&mono).await, if committed { (1, 1) } else { (0, 0) });
        assert_eq!(
            mono.publication_sequence("/").await.unwrap(),
            i64::from(committed)
        );
        let retry_db = sea_orm::Database::connect(config.db_url.as_str())
            .await
            .unwrap();
        let retry_mono = MonoStorage {
            base: BaseStorage::new(Arc::new(retry_db)),
        };
        let txn = retry_mono.get_connection().begin().await.unwrap();
        let request = PublicationRequest::for_test("process-op", "/", "a", "c", "test");
        match retry_mono
            .begin_publication_in_txn(&txn, request)
            .await
            .unwrap()
        {
            PublicationPreparation::AlreadyCommitted(original) => {
                assert!(committed);
                assert_eq!(original.receipt.sequence, 1);
                assert_eq!(original.receipt.new_oid, "c");
                assert_eq!(original.outbox.sequence, 1);
                txn.commit().await.unwrap();
            }
            PublicationPreparation::Prepared(_) => {
                assert!(!committed);
                txn.rollback().await.unwrap();
            }
            PublicationPreparation::AlreadyCommittedNoop(_) => panic!("publication request"),
        }
        assert_eq!(counts(&mono).await, if committed { (1, 1) } else { (0, 0) });
    }
}

#[cfg(unix)]
#[tokio::test]
async fn noop_sigkill_and_lost_commit_response_never_create_a_publication() {
    use std::os::unix::process::ExitStatusExt;

    for phase in ["ref-written", "noop-receipt-written", "commit-complete"] {
        let temp = tempfile::tempdir().unwrap();
        let (config, _schema) = crate::jupiter::tests::test_db_config(temp.path()).await;
        let db = crate::jupiter::storage::init::database_connection(&config)
            .await
            .unwrap();
        let mono = MonoStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        seed_root(&mono).await;
        let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jupiter::storage::mst2_publication_storage::tests::receipt_crash_worker",
                "--nocapture",
            ])
            .env("MEGA_MST2_RECEIPT_WORKER_DB", &config.db_url)
            .env("MEGA_MST2_RECEIPT_CRASH_PHASE", phase)
            .env("MEGA_MST2_RECEIPT_WORKER_NOOP", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = worker.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                worker.kill().unwrap();
                worker.wait().unwrap();
                panic!("no-op receipt worker timed out at {phase}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{phase}: {status}");
        let committed = phase == "commit-complete";
        assert_eq!(
            mono.get_main_ref("/")
                .await
                .unwrap()
                .unwrap()
                .ref_commit_hash,
            "a".repeat(40)
        );
        assert_eq!(counts(&mono).await, (0, 0));
        assert_eq!(mono.publication_sequence("/project").await.unwrap(), 0);
        assert_eq!(
            mst2_queue_noop_receipt::Entity::find()
                .count(mono.get_connection())
                .await
                .unwrap(),
            u64::from(committed)
        );
        let original = mst2_queue_noop_receipt::Entity::find()
            .one(mono.get_connection())
            .await
            .unwrap();
        if committed {
            let txn = mono.get_connection().begin().await.unwrap();
            assert!(advance(&mono, &txn).await);
            txn.commit().await.unwrap();
        }
        // Reconnect after the worker died; committed no-op identity survives a later root.
        let retry_db = sea_orm::Database::connect(config.db_url.as_str())
            .await
            .unwrap();
        let retry_mono = MonoStorage {
            base: BaseStorage::new(Arc::new(retry_db)),
        };
        let txn = retry_mono.get_connection().begin().await.unwrap();
        match retry_mono
            .begin_publication_in_txn(
                &txn,
                PublicationRequest::from_trunk_queue(&noop_queue_row()).unwrap(),
            )
            .await
            .unwrap()
        {
            PublicationPreparation::AlreadyCommittedNoop(receipt) => {
                assert!(committed);
                assert_eq!(Some(receipt.clone()), original);
                assert_eq!(receipt.observed_sequence, 0);
                assert_eq!(receipt.observed_root_commit, "a".repeat(40));
                assert_eq!(receipt.landed_commit_id, "tip");
                txn.commit().await.unwrap();
            }
            PublicationPreparation::Prepared(_) => {
                assert!(!committed);
                txn.rollback().await.unwrap();
            }
            PublicationPreparation::AlreadyCommitted(_) => panic!("no-op must not publish"),
        }
        assert_eq!(counts(&mono).await, (0, 0));
        assert_eq!(mono.publication_sequence("/project").await.unwrap(), 0);
        assert_eq!(
            mono.get_main_ref("/")
                .await
                .unwrap()
                .unwrap()
                .ref_commit_hash,
            if committed { "c" } else { "a" }.repeat(40)
        );
    }
}
