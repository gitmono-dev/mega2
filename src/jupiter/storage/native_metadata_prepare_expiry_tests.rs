use std::{sync::atomic::Ordering, time::Duration};

use sea_orm::Database;
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    ceres::snapshot::retention::RetentionRoot,
    jupiter::{
        migration::Migrator,
        storage::native_metadata_install::{
            MetadataCommitPhase, MetadataInstallError, MetadataPrepareObservation,
            tests::{Fault, PgCommitFaultProxy, fixture, install, prepared},
        },
        tests::test_db_config,
    },
};

async fn count<C: ConnectionTrait>(db: &C, table: &str) -> i64 {
    db.query_one_raw(statement(&format!("SELECT count(*) FROM {table}"), []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn elapsed(db: &DatabaseConnection, intent: &MetadataPrepareIntent) {
    let deadline: i64 = db
        .query_one_raw(statement(
            "SELECT expires_at_ms FROM mst2_metadata_prepare_deadline WHERE prepare_id=$1",
            [intent.prepare_id().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let now = clock(db).await.unwrap();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(((deadline - now) as u64).min(100))).await;
        }
    })
    .await
    .unwrap();
}

fn rejected(error: PrepareExpiryError) -> SnapshotErrorCode {
    match error {
        PrepareExpiryError::Rejected(error) => error.code,
        other => panic!("expected definite expiry rejection: {other:?}"),
    }
}

#[tokio::test]
async fn prepare_deadline_replay_and_recovery_bind_original_duration_without_extending_it() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("duration", &pages, 3000)
        .await
        .unwrap();
    let digest = intent.manifest_digest();
    let grant = load_plan(&first, "duration", &digest)
        .await
        .unwrap()
        .unwrap()
        .deadline
        .unwrap();
    assert_eq!(
        repo.begin_intent_with_duration("duration", &pages, 3000)
            .await
            .unwrap(),
        intent
    );
    assert_eq!(
        load_plan(&first, "duration", &digest)
            .await
            .unwrap()
            .unwrap()
            .deadline
            .unwrap(),
        grant
    );
    assert!(
        matches!(repo.begin_intent_with_duration("duration", &pages, 3001).await.unwrap_err(),
        MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
    for duration in [0, DEFAULT_PREPARE_DURATION_MS + 1] {
        assert!(
            matches!(repo.begin_intent_with_duration("invalid", &pages, duration).await.unwrap_err(),
            MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::InvalidRequest)
        );
    }
    let wrong = repo
        .recovery_request_with_duration("duration", digest, MetadataCommitPhase::Intent, 3001)
        .unwrap();
    assert!(
        matches!(repo.inspect_prepare(&second, &wrong).await.unwrap_err(),
        MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
    assert_eq!(count(&first, "mst2_metadata_prepare_deadline").await, 1);
}

#[tokio::test]
async fn prepare_elapsed_receipt_is_unusable_but_inspection_keeps_all_pins() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("elapsed", &pages, 3000)
        .await
        .unwrap();
    install(&repo, &intent, &pages).await;
    repo.finalize(&intent).await.unwrap();
    elapsed(&first, &intent).await;
    assert!(
        matches!(repo.inspect_prepare(&second, &intent.recovery(MetadataCommitPhase::Finalize)).await.unwrap(),
        MetadataPrepareObservation::DeadlinePassed { intent: observed, .. } if observed==intent)
    );
    for error in [
        repo.install_page(&intent, &pages.dag().payloads()[0])
            .await
            .unwrap_err(),
        repo.finalize(&intent).await.unwrap_err(),
        repo.begin_intent_with_duration("elapsed", &pages, 3000)
            .await
            .unwrap_err(),
    ] {
        assert!(
            matches!(error, MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
        );
    }
    assert_eq!(count(&first, "mst2_retention_root").await, 2);
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 0);
}

#[tokio::test]
async fn prepare_partial_payload_expiry_preserves_orphan_bytes_and_one_terminal_event() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("partial", &pages, 3000)
        .await
        .unwrap();
    repo.install_page(&intent, &pages.dag().payloads()[0])
        .await
        .unwrap();
    elapsed(&first, &intent).await;
    let event = repo
        .expire_prepare("expire-partial", &intent)
        .await
        .unwrap();
    assert_eq!(event.previous_state, "PREPARING");
    assert!(event.expired_at_ms >= event.expires_at_ms);
    let restarted = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    assert_eq!(
        restarted
            .expire_prepare("expire-partial", &intent)
            .await
            .unwrap(),
        event
    );
    assert_eq!(
        restarted
            .inspect_prepare_expiry(&second, &event.request)
            .await
            .unwrap(),
        PrepareExpiryObservation::Expired(event)
    );
    assert_eq!(count(&first, "mst2_metadata_payload").await, 1);
    assert_eq!(count(&first, "mst2_retention_root").await, 0);
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 1);
    assert_eq!(
        rejected(
            repo.expire_prepare("other-expiry", &intent)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::Conflict
    );
    assert!(
        matches!(repo.inspect_prepare(&second, &intent.recovery(MetadataCommitPhase::Payload)).await.unwrap_err(),
        MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
    assert!(
        first
            .execute_unprepared(
                "UPDATE mst2_metadata_prepare SET state='PREPARING' WHERE state='EXPIRED'"
            )
            .await
            .is_err()
    );
    assert!(
        first
            .execute_unprepared(
                "UPDATE mst2_metadata_prepare_expiry SET expired_at_ms=expired_at_ms+1"
            )
            .await
            .is_err()
    );
    assert!(
        first
            .execute_unprepared("DELETE FROM mst2_metadata_prepare_deadline")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn prepare_committed_expiry_releases_only_own_root_and_preserves_shared_graph() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("committed", &pages, 3000)
        .await
        .unwrap();
    install(&repo, &intent, &pages).await;
    repo.finalize(&intent).await.unwrap();
    let committed_at = load_plan(&first, "committed", &intent.manifest_digest())
        .await
        .unwrap()
        .unwrap()
        .record
        .committed_at;
    let graph = PostgresRetentionRepository::new(first.clone());
    graph
        .retain_group(
            pages.dag().nodes(),
            pages.dag().edges(),
            &[
                RetentionRoot::Lease("other-lease".into()),
                RetentionRoot::Pin("workspace:one".into()),
                RetentionRoot::Pin("local:one".into()),
            ],
        )
        .await
        .unwrap();
    elapsed(&first, &intent).await;
    let event = repo
        .expire_prepare("expire-committed", &intent)
        .await
        .unwrap();
    assert_eq!(event.previous_state, "COMMITTED");
    let final_plan = load_plan(&second, "committed", &intent.manifest_digest())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_plan.record.committed_at, committed_at);
    assert_eq!(final_plan.record.state, "EXPIRED");
    assert_eq!(count(&first, "mst2_metadata_payload").await, 2);
    assert_eq!(count(&first, "mst2_retention_node").await, 2);
    assert_eq!(count(&first, "mst2_retention_edge").await, 1);
    assert_eq!(count(&first, "mst2_retention_root").await, 6);
    for root in ["lease:other-lease", "pin:workspace:one", "pin:local:one"] {
        verify_graph_root(
            &second,
            &final_plan,
            root,
            if root.starts_with("lease:") {
                "lease"
            } else {
                "pin"
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        repo.expire_prepare("expire-committed", &intent)
            .await
            .unwrap(),
        event
    );
    assert_eq!(count(&first, "mst2_retention_root").await, 6);
}

#[tokio::test]
async fn prepare_expiry_early_rejection_and_outer_rollback_keep_preparation() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("rollback", &pages, 3000)
        .await
        .unwrap();
    install(&repo, &intent, &pages).await;
    repo.finalize(&intent).await.unwrap();
    assert_eq!(
        rejected(repo.expire_prepare("early", &intent).await.unwrap_err()),
        SnapshotErrorCode::Conflict
    );
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 0);
    elapsed(&first, &intent).await;
    let request = expiry_request(repo.storage_uuid(), "rollback-expire", &intent).unwrap();
    let txn = repo.transaction().await.unwrap();
    let provisional = repo.expire_prepare_in_txn(&txn, &request).await.unwrap();
    assert_eq!(provisional.previous_state, "COMMITTED");
    assert_eq!(count(&txn, "mst2_retention_root").await, 0);
    txn.rollback().await.unwrap();
    assert_eq!(
        repo.inspect_prepare_expiry(&second, &request)
            .await
            .unwrap(),
        PrepareExpiryObservation::NotExpired {
            state: "COMMITTED".into()
        }
    );
    assert_eq!(count(&first, "mst2_retention_root").await, 2);
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 0);
}

#[tokio::test]
async fn prepare_expiry_missing_coverage_and_recovery_resurrected_pin_fail_without_repair() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("coverage", &pages, 3000)
        .await
        .unwrap();
    install(&repo, &intent, &pages).await;
    repo.finalize(&intent).await.unwrap();
    first
        .execute_raw(statement(
            "DELETE FROM mst2_retention_root WHERE root_key=$1 AND node_id=$2",
            [
                format!("prepare:{}", intent.prepare_id()).into(),
                pages.dag().nodes()[0].id.clone().into(),
            ],
        ))
        .await
        .unwrap();
    elapsed(&first, &intent).await;
    assert_eq!(
        rejected(
            repo.expire_prepare("incomplete", &intent)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(count(&first, "mst2_retention_root").await, 1);
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 0);
    let other = repo
        .begin_intent_with_duration("partial-recovery", &pages, 1)
        .await
        .unwrap();
    elapsed(&first, &other).await;
    let event = repo.expire_prepare("expired-other", &other).await.unwrap();
    PostgresRetentionRepository::new(first.clone())
        .retain_group(
            pages.dag().nodes(),
            pages.dag().edges(),
            &[RetentionRoot::Prepare(other.prepare_id().into())],
        )
        .await
        .unwrap();
    assert_eq!(
        rejected(
            repo.inspect_prepare_expiry(&second, &event.request)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(count(&first, "mst2_retention_root").await, 3);
}

#[tokio::test]
async fn prepare_expiry_clock_is_sampled_after_actual_barrier_wait() {
    let (first, second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repo
        .begin_intent_with_duration("wait-expire", &pages, 3000)
        .await
        .unwrap();
    let blocker = repo.transaction().await.unwrap();
    repo.barrier(&blocker).await.unwrap();
    let worker_repo = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    let worker_intent = intent.clone();
    let worker = tokio::spawn(async move {
        worker_repo
            .expire_prepare("waited-expiry", &worker_intent)
            .await
    });
    elapsed(&first, &intent).await;
    assert!(!worker.is_finished());
    let released_at = clock(&first).await.unwrap();
    blocker.rollback().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(event.expired_at_ms >= released_at);
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 1);
}

async fn commit_fault(fault: Fault) {
    let (direct, _second, _schema, url) = fixture().await;
    let original = PostgresMetadataInstallRepository::new(direct.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = original
        .begin_intent_with_duration("fault-expiry", &pages, 3000)
        .await
        .unwrap();
    install(&original, &intent, &pages).await;
    original.finalize(&intent).await.unwrap();
    elapsed(&direct, &intent).await;
    let proxy = PgCommitFaultProxy::start(&url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.min_connections(1).max_connections(1);
    let repo = PostgresMetadataInstallRepository::new(Database::connect(options).await.unwrap())
        .await
        .unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    let recovery = match repo
        .expire_prepare("fault-release", &intent)
        .await
        .unwrap_err()
    {
        PrepareExpiryError::CommitUncertain { recovery } => recovery,
        other => panic!("COMMIT disconnect must return a typed unknown outcome: {other:?}"),
    };
    proxy.wait_for_fault().await;
    let observed = repo
        .inspect_prepare_expiry(&direct, &recovery)
        .await
        .unwrap();
    match fault {
        Fault::BeforeCommit => {
            assert!(!proxy.commit_observed.load(Ordering::SeqCst));
            assert_eq!(
                observed,
                PrepareExpiryObservation::NotExpired {
                    state: "COMMITTED".into()
                }
            );
            assert_eq!(count(&direct, "mst2_retention_root").await, 2);
            assert_eq!(count(&direct, "mst2_metadata_prepare_expiry").await, 0);
        }
        Fault::AfterCommit => {
            assert!(proxy.commit_observed.load(Ordering::SeqCst));
            let original_event = match observed {
                PrepareExpiryObservation::Expired(receipt) => receipt,
                other => {
                    panic!("COMMIT reply loss must recover its immutable release event: {other:?}")
                }
            };
            assert_eq!(
                original
                    .expire_prepare("fault-release", &intent)
                    .await
                    .unwrap(),
                original_event
            );
        }
        Fault::PrepareRead => panic!("query failure is a separate recovery test"),
    }
    original
        .expire_prepare("fault-release", &intent)
        .await
        .unwrap();
    assert_eq!(count(&direct, "mst2_metadata_prepare_expiry").await, 1);
    assert_eq!(count(&direct, "mst2_retention_root").await, 0);
    assert_eq!(count(&direct, "mst2_metadata_payload").await, 2);
}

#[tokio::test]
async fn prepare_expiry_actual_disconnect_before_commit_keeps_refs_until_recovery() {
    commit_fault(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn prepare_expiry_actual_commit_reply_loss_recovers_one_release_event() {
    commit_fault(Fault::AfterCommit).await;
}

#[tokio::test]
async fn prepare_expiry_recovery_query_failure_and_wrong_primary_stay_unknown() {
    let (direct, second, _schema, url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(direct.clone())
        .await
        .unwrap();
    let intent = repo
        .begin_intent_with_duration("unknown-expiry", &prepared("/"), 1)
        .await
        .unwrap();
    elapsed(&direct, &intent).await;
    let event = repo
        .expire_prepare("unknown-release", &intent)
        .await
        .unwrap();
    let proxy = PgCommitFaultProxy::start(&url, Fault::PrepareRead).await;
    let fresh = Database::connect(proxy.url.clone()).await.unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    assert!(
        matches!(repo.inspect_prepare_expiry(&fresh, &event.request).await.unwrap_err(), PrepareExpiryError::CommitUncertain { recovery } if *recovery==event.request)
    );
    proxy.wait_for_fault().await;
    let (wrong, _wrong_second, _wrong_schema, _wrong_url) = fixture().await;
    assert!(matches!(
        repo.inspect_prepare_expiry(&wrong, &event.request)
            .await
            .unwrap_err(),
        PrepareExpiryError::CommitUncertain { .. }
    ));
    assert_eq!(
        repo.inspect_prepare_expiry(&second, &event.request)
            .await
            .unwrap(),
        PrepareExpiryObservation::Expired(event)
    );
    assert_eq!(count(&direct, "mst2_metadata_prepare_expiry").await, 1);
}

#[tokio::test]
async fn prepare_deadline_and_expiry_ddl_reject_unbound_grants_and_incomplete_terminal_transactions()
 {
    let (db, _second, _schema, _url) = fixture().await;
    let repo = PostgresMetadataInstallRepository::new(db.clone())
        .await
        .unwrap();
    let intent = repo
        .begin_intent_with_duration("ddl-prepare", &prepared("/"), 3000)
        .await
        .unwrap();
    let stored = load_plan(&db, intent.operation_id(), &intent.manifest_digest())
        .await
        .unwrap()
        .unwrap();
    let grant = stored.deadline.unwrap();
    assert!(
        db.execute_unprepared("UPDATE mst2_metadata_prepare SET deadline_managed=false")
            .await
            .is_err()
    );
    let txn = repo.transaction().await.unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_prepare SET state='EXPIRED' WHERE prepare_id=$1",
        [intent.prepare_id().into()],
    ))
    .await
    .unwrap();
    assert!(txn.commit().await.is_err());
    assert_eq!(count(&db, "mst2_metadata_prepare_expiry").await, 0);
    for wrong_grant in [false, true] {
        let txn = repo.transaction().await.unwrap();
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_prepare_expiry(prepare_id,operation_id,operation_digest,grant_digest,
             previous_state,pin_root,expires_at_ms,expired_at_ms) VALUES($1,'ddl-expiry',$2,$3,'PREPARING',$4,$5,$5)",
            [intent.prepare_id().into(),vec![9u8;32].into(),if wrong_grant {vec![8u8;32]} else {grant.grant_digest.to_vec()}.into(),
             format!("prepare:{}",intent.prepare_id()).into(),grant.expires_at_ms.into()],
        )).await.unwrap();
        if wrong_grant {
            txn.execute_raw(statement(
                "UPDATE mst2_metadata_prepare SET state='EXPIRED' WHERE prepare_id=$1",
                [intent.prepare_id().into()],
            ))
            .await
            .unwrap();
        }
        assert!(txn.commit().await.is_err());
        assert_eq!(count(&db, "mst2_metadata_prepare_expiry").await, 0);
    }
    let txn = repo.transaction().await.unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_prepare SELECT replacement.* FROM jsonb_populate_record(NULL::mst2_metadata_prepare,
          (SELECT to_jsonb(p)||jsonb_build_object('prepare_id',$1::text,'operation_id','missing-grant')
           FROM mst2_metadata_prepare p WHERE prepare_id=$2)) AS replacement",
        [uuid::Uuid::new_v4().to_string().into(),intent.prepare_id().into()],
    )).await.unwrap();
    assert!(txn.commit().await.is_err());
    assert_eq!(count(&db, "mst2_metadata_prepare").await, 1);
    assert_eq!(count(&db, "mst2_metadata_prepare_deadline").await, 1);
    assert_eq!(
        load_plan(&db, intent.operation_id(), &intent.manifest_digest())
            .await
            .unwrap()
            .unwrap()
            .record
            .state,
        "PREPARING"
    );
}

async fn intent_commit_fault(fault: Fault) {
    let (direct, _second, _schema, url) = fixture().await;
    let proxy = PgCommitFaultProxy::start(&url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.min_connections(1).max_connections(1);
    let repo = PostgresMetadataInstallRepository::new(Database::connect(options).await.unwrap())
        .await
        .unwrap();
    let pages = prepared("/");
    proxy.armed.store(true, Ordering::SeqCst);
    let witness = match repo
        .begin_intent_with_duration("fault-intent-duration", &pages, 30_000)
        .await
        .unwrap_err()
    {
        MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Intent,
            recovery,
            ..
        } => recovery,
        other => panic!("intent COMMIT fault must retain its exact grant witness: {other:?}"),
    };
    proxy.wait_for_fault().await;
    let observed = repo.inspect_prepare(&direct, &witness).await.unwrap();
    let restart = PostgresMetadataInstallRepository::new(direct.clone())
        .await
        .unwrap();
    match fault {
        Fault::BeforeCommit => {
            assert!(!proxy.commit_observed.load(Ordering::SeqCst));
            assert_eq!(observed, MetadataPrepareObservation::Absent);
            assert_eq!(count(&direct, "mst2_metadata_prepare_deadline").await, 0);
        }
        Fault::AfterCommit => {
            assert!(proxy.commit_observed.load(Ordering::SeqCst));
            let intent = match observed {
                MetadataPrepareObservation::Preparing(intent) => intent,
                other => panic!("expected one durable intent: {other:?}"),
            };
            assert_eq!(
                restart
                    .begin_intent_with_duration("fault-intent-duration", &pages, 30_000)
                    .await
                    .unwrap(),
                intent
            );
            let wrong = restart
                .recovery_request_with_duration(
                    intent.operation_id(),
                    intent.manifest_digest(),
                    MetadataCommitPhase::Intent,
                    30_001,
                )
                .unwrap();
            assert!(
                matches!(restart.inspect_prepare(&direct,&wrong).await.unwrap_err(),MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
            );
        }
        Fault::PrepareRead => panic!("query fault is not intent COMMIT failure"),
    }
    restart
        .begin_intent_with_duration("fault-intent-duration", &pages, 30_000)
        .await
        .unwrap();
    assert_eq!(count(&direct, "mst2_metadata_prepare").await, 1);
    assert_eq!(count(&direct, "mst2_metadata_prepare_deadline").await, 1);
}

#[tokio::test]
async fn prepare_intent_actual_commit_reply_loss_recovers_exact_duration_and_grant() {
    intent_commit_fault(Fault::AfterCommit).await;
}

#[tokio::test]
async fn prepare_intent_actual_disconnect_before_commit_recovers_absence_without_grant() {
    intent_commit_fault(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn prepare_deadline_migration_keeps_historical_unknown_duration_and_pins() {
    let temp = tempfile::TempDir::new().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let db = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&db, Some(Migrator::migrations().len() as u32 - 1))
        .await
        .unwrap();
    let pages = prepared("/");
    let plan = pages.install_plan().unwrap();
    let digest = plan.digest().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let identity = &plan.identity;
    db.execute_raw(statement(
        "INSERT INTO mst2_metadata_prepare(prepare_id,operation_id,manifest_digest,canonical_plan,source_domain,
         tagged_root_tree_oid,scope,schema_version,metadata_codec,materialization_policy,fs_semantics,access_projection,
         verification_revision,projection_revision,metadata_root,node_count,edge_count,total_bytes,state,created_at,committed_at)
         VALUES($1,'historical',$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,'COMMITTED',now()-interval '30 days',now()-interval '30 days')",
        [id.clone().into(),digest.to_vec().into(),plan.encode().unwrap().into(),identity.source_domain.clone().into(),
         identity.tagged_root_tree_oid.clone().into(),identity.scope.clone().into(),(identity.schema_version as i16).into(),
         (identity.metadata_codec as i16).into(),(identity.materialization_policy as i16).into(),(identity.fs_semantics as i16).into(),
         (identity.access_projection as i16).into(),identity.verification_revision.into(),(identity.projection_revision as i16).into(),
         plan.root.to_vec().into(),(plan.pages.len() as i32).into(),(plan.edges.len() as i32).into(),(plan.total_bytes as i64).into()],
    )).await.unwrap();
    for page in pages.dag().payloads() {
        db.execute_raw(statement("INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,expected_size) VALUES($1,$2,$3)",
            [id.clone().into(),page.id.to_vec().into(),(page.size as i32).into()])).await.unwrap();
        db.execute_raw(statement("INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
            [page.id.to_vec().into(),(page.size as i32).into(),page.bytes.clone().into()])).await.unwrap();
    }
    PostgresRetentionRepository::new(db.clone())
        .retain_group(
            pages.dag().nodes(),
            pages.dag().edges(),
            &[RetentionRoot::Prepare(id)],
        )
        .await
        .unwrap();
    Migrator::up(&db, None).await.unwrap();
    let repo = PostgresMetadataInstallRepository::new(db.clone())
        .await
        .unwrap();
    let stored = load_plan(&db, "historical", &digest)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.record.deadline_managed);
    assert!(stored.deadline.is_none());
    let intent = stored.intent().unwrap();
    assert_eq!(
        rejected(
            repo.expire_prepare("legacy-expire", &intent)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::Conflict
    );
    assert!(
        matches!(repo.begin_intent("historical",&pages).await.unwrap_err(), MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
    let fresh = Database::connect(config.db_url).await.unwrap();
    assert!(matches!(
        repo.inspect_prepare(&fresh, &intent.recovery(MetadataCommitPhase::Finalize))
            .await
            .unwrap(),
        MetadataPrepareObservation::Committed(_)
    ));
    assert_eq!(count(&db, "mst2_metadata_prepare_deadline").await, 0);
    assert_eq!(count(&db, "mst2_metadata_prepare_expiry").await, 0);
    assert_eq!(count(&db, "mst2_retention_root").await, 2);
}
