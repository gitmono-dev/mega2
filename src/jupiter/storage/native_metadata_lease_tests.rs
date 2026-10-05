use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use mst2_codec::metapage::{Entry, EntryKind, Page, page_id};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    ceres::snapshot::{
        pages::PreparedNativeMetadataRetention,
        retention_dag::{MetadataDagBuilder, MetadataDagLimits},
    },
    jupiter::{
        migration::Migrator,
        storage::native_metadata_install::tests::{Fault, PgCommitFaultProxy},
        tests::{TestSchemaGuard, test_db_config},
    },
};

async fn fixture() -> (DatabaseConnection, DatabaseConnection, TestSchemaGuard) {
    let temp = tempfile::TempDir::new().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    (first, second, schema)
}

async fn actual_commit_fault(fault: Fault) {
    let temp = tempfile::TempDir::new().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let direct = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&direct, None).await.unwrap();
    let (_direct_ledger, handoff, _receipt) = handoff(&direct, "fault-prepare", 1).await;
    let access = access(&direct).await;
    let proxy = PgCommitFaultProxy::start(&config.db_url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.max_connections(1).min_connections(1);
    let connection = Database::connect(options).await.unwrap();
    let ledger = PostgresMetadataLeaseRepository::new(connection)
        .await
        .unwrap();
    let digest = operation_digest("fault-consume", &handoff, &access, 60_000).unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    assert!(
        matches!(ledger.consume("fault-consume",&handoff,&access,60_000).await.unwrap_err(),
        MetadataLeaseError::CommitUncertain { operation_id,operation_digest } if operation_id=="fault-consume" && operation_digest==digest)
    );
    proxy.wait_for_fault().await;
    let observed = ledger
        .inspect_operation(&direct, "fault-consume", digest)
        .await
        .unwrap();
    let restarted = PostgresMetadataLeaseRepository::new(direct.clone())
        .await
        .unwrap();
    match fault {
        Fault::BeforeCommit => {
            assert!(!proxy.commit_observed.load(Ordering::SeqCst));
            assert_eq!(observed, MetadataLeaseObservation::Absent);
            assert_eq!(count(&direct, "mst2_metadata_lease").await, 0);
            let state: String = direct
                .query_one_raw(statement("SELECT state FROM mst2_metadata_prepare", []))
                .await
                .unwrap()
                .unwrap()
                .try_get_by_index(0)
                .unwrap();
            assert_eq!(state, "COMMITTED");
            assert_eq!(count(&direct, "mst2_retention_root").await, 2);
            restarted
                .consume("fault-consume", &handoff, &access, 60_000)
                .await
                .unwrap();
        }
        Fault::AfterCommit => {
            assert!(proxy.commit_observed.load(Ordering::SeqCst));
            let original = match observed {
                MetadataLeaseObservation::Committed { event, state, .. } => {
                    assert_eq!(state, "ACTIVE");
                    *event
                }
                _ => panic!("actual COMMIT response loss must recover its event"),
            };
            let replay = restarted
                .consume("fault-consume", &handoff, &access, 60_000)
                .await
                .unwrap();
            assert_eq!(replay, original);
            assert_eq!(count(&direct, "mst2_metadata_prepare_consumption").await, 1);
            assert_eq!(count(&direct, "mst2_metadata_lease_operation").await, 1);
        }
        Fault::PrepareRead => panic!("query failure is not a COMMIT fault"),
    }
    assert_eq!(count(&direct, "mst2_metadata_lease").await, 1);
    assert_eq!(count(&direct, "mst2_retention_root").await, 2);
    assert_eq!(count(&direct, "mst2_retention_edge").await, 1);
}

#[tokio::test]
async fn metadata_lease_actual_disconnect_before_commit_keeps_prepare_until_recovery() {
    actual_commit_fault(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn metadata_lease_actual_commit_reply_loss_recovers_one_consumed_event_without_recount() {
    actual_commit_fault(Fault::AfterCommit).await;
}

#[tokio::test]
async fn metadata_lease_clock_is_sampled_after_actual_retention_barrier_wait() {
    let (first, second, _schema) = fixture().await;
    let (ledger, handoff, _receipt) = handoff(&first, "clock-prepare", 1).await;
    let access = access(&first).await;
    let blocked_ledger = PostgresMetadataLeaseRepository::new(second.clone())
        .await
        .unwrap();
    let blocker = ledger.install.transaction().await.unwrap();
    ledger.install.barrier(&blocker).await.unwrap();
    let worker = tokio::spawn(async move {
        blocked_ledger
            .consume("clock-consume", &handoff, &access, 1000)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {
            let waiting:bool=first.query_one_raw(statement(
                "SELECT EXISTS(SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid
                 WHERE l.locktype='advisory' AND NOT l.granted AND a.datname=current_database()
                 AND l.classid::bigint=$1 AND l.objid::bigint=(hashtext(current_schema())::bigint & 4294967295)
                 AND a.query LIKE '%pg_advisory_xact_lock%')",[super::super::mst2_retention::RETENTION_LOCK_KEY.into()]
            )).await.unwrap().unwrap().try_get_by_index(0).unwrap();
            if waiting {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    // Real lock wait exceeds the requested lease lifetime; transaction-start
    // now() would issue an already-expired lease in this exact schedule.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let released_at = clock_after_lock(&blocker).await.unwrap();
    blocker.rollback().await.unwrap();
    let receipt = worker.await.unwrap().unwrap();
    assert!(receipt.expires_at_ms() >= released_at + 1000);
    assert_eq!(count(&first, "mst2_metadata_lease").await, 1);
}

#[tokio::test]
async fn metadata_lease_corrupted_catalog_and_consumption_fail_closed_without_repair() {
    let (first, second, _schema) = fixture().await;
    let (ledger, handoff, _receipt) = handoff(&first, "corruption-prepare", 1).await;
    let access = access(&first).await;
    let receipt = ledger
        .consume("corruption-consume", &handoff, &access, 60_000)
        .await
        .unwrap();
    // Actual SQL bypass is confined to this isolated schema; production
    // immutable triggers are restored before recovery observes corruption.
    first.execute_unprepared("ALTER TABLE mst2_metadata_catalog DISABLE TRIGGER mst2_metadata_catalog_immutable;
        UPDATE mst2_metadata_catalog SET tagged_root_commit_oid='sha1:cccccccccccccccccccccccccccccccccccccccc';
        ALTER TABLE mst2_metadata_catalog ENABLE TRIGGER mst2_metadata_catalog_immutable;").await.unwrap();
    assert_eq!(
        rejection(
            ledger
                .inspect_operation(&second, receipt.operation_id(), receipt.operation_digest())
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        rejection(
            ledger
                .consume("corruption-consume", &handoff, &access, 60_000)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    first
        .execute_raw(statement(
            "ALTER TABLE mst2_metadata_catalog DISABLE TRIGGER mst2_metadata_catalog_immutable",
            [],
        ))
        .await
        .unwrap();
    first
        .execute_raw(statement(
            "UPDATE mst2_metadata_catalog SET tagged_root_commit_oid=$1",
            [handoff.binding.tagged_root_commit_oid.clone().into()],
        ))
        .await
        .unwrap();
    first.execute_unprepared("ALTER TABLE mst2_metadata_catalog ENABLE TRIGGER mst2_metadata_catalog_immutable;
        ALTER TABLE mst2_metadata_prepare_consumption DISABLE TRIGGER mst2_metadata_consumption_immutable;
        UPDATE mst2_metadata_prepare_consumption SET install_digest=decode(repeat('00',32),'hex');
        ALTER TABLE mst2_metadata_prepare_consumption ENABLE TRIGGER mst2_metadata_consumption_immutable;").await.unwrap();
    assert_eq!(
        rejection(
            ledger
                .inspect_operation(&second, receipt.operation_id(), receipt.operation_digest())
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(count(&second, "mst2_retention_root").await, 2);
    assert_eq!(count(&second, "mst2_retention_edge").await, 1);
    assert_eq!(count(&second, "mst2_metadata_lease").await, 1);
    let still_bad: Vec<u8> = second
        .query_one_raw(statement(
            "SELECT install_digest FROM mst2_metadata_prepare_consumption",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(still_bad, [0; 32]);
}

fn prepared() -> PreparedNativeMetadataRetention {
    let child_entries = [Entry::file(EntryKind::Regular, b"file", 3, [42; 32])];
    let child = Page::build(&child_entries).unwrap();
    let root_entries = [
        Entry::dir(b"one", page_id(&child)),
        Entry::dir(b"two", page_id(&child)),
    ];
    let root = Page::build(&root_entries).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &child_entries).unwrap();
    builder.add_directory(&root, &root_entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    )
}

// Synthetic fixture identities are confined to cfg(test). This is not an
// adopted native namespace identity factory or a production authorizer.
fn binding(root: [u8; 32], publication: u8) -> MetadataOnlyBinding {
    MetadataOnlyBinding {
        canonical_descriptor: ServingDescriptor {
            instance_uuid: *uuid::Uuid::parse_str("11111111-2222-4333-8444-555555555556")
                .unwrap()
                .as_bytes(),
            namespace_view_id: [22; 32],
            scope: "/".into(),
            metadata_root: root,
        }
        .encode()
        .unwrap(),
        tagged_root_commit_oid: format!("sha1:{}", "b".repeat(40)),
        publication_binding: [publication; 32],
    }
}

async fn handoff(
    db: &DatabaseConnection,
    operation: &str,
    publication: u8,
) -> (
    PostgresMetadataLeaseRepository,
    VerifiedMetadataHandoff,
    PreparedMetadataReceipt,
) {
    let install = PostgresMetadataInstallRepository::new(db.clone())
        .await
        .unwrap();
    let prepared = prepared();
    let intent = install.begin_intent(operation, &prepared).await.unwrap();
    for page in prepared.dag().payloads() {
        install.install_page(&intent, page).await.unwrap();
    }
    let receipt = install.finalize(&intent).await.unwrap();
    let ledger = PostgresMetadataLeaseRepository::new(db.clone())
        .await
        .unwrap();
    let handoff = ledger
        .verify_handoff(binding(receipt.metadata_root(), publication), &receipt)
        .await
        .unwrap();
    (ledger, handoff, receipt)
}

async fn access(db: &DatabaseConnection) -> MetadataAccess {
    let access = MetadataAccess {
        subject_id: [7; 32],
        generation: 1,
        scope: "/".into(),
    };
    db.execute_raw(statement(
        "INSERT INTO mst2_metadata_access_generation(subject_id,generation,scope,enabled) VALUES($1,1,'/',true) ON CONFLICT(subject_id) DO NOTHING",
        [access.subject_id.to_vec().into()],
    )).await.unwrap();
    access
}

async fn count<C: ConnectionTrait>(db: &C, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table}");
    db.query_one_raw(statement(&sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

fn rejection(error: MetadataLeaseError) -> SnapshotErrorCode {
    match error {
        MetadataLeaseError::Rejected(error) => error.code,
        _ => panic!("expected definite rejection"),
    }
}

#[tokio::test]
async fn metadata_lease_consumed_prepare_deadline_cannot_expire_or_break_operation_replay() {
    let (first, second, _schema) = fixture().await;
    let install = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = install
        .begin_intent_with_duration("consumed-deadline", &pages, 3000)
        .await
        .unwrap();
    for page in pages.dag().payloads() {
        install.install_page(&intent, page).await.unwrap();
    }
    let prepared_receipt = install.finalize(&intent).await.unwrap();
    let ledger = PostgresMetadataLeaseRepository::new(first.clone())
        .await
        .unwrap();
    let handoff = ledger
        .verify_handoff(
            binding(prepared_receipt.metadata_root(), 1),
            &prepared_receipt,
        )
        .await
        .unwrap();
    let access = access(&first).await;
    let lease = ledger
        .consume("consume-before-deadline", &handoff, &access, 60_000)
        .await
        .unwrap();
    let grant = load_plan(&first, intent.operation_id(), &intent.manifest_digest())
        .await
        .unwrap()
        .unwrap()
        .deadline
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while super::super::native_metadata_prepare_expiry::clock(&first)
            .await
            .unwrap()
            < grant.expires_at_ms
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(install.expire_prepare("expire-consumed", &intent).await.unwrap_err(),
        super::super::native_metadata_prepare_expiry::PrepareExpiryError::Rejected(error)
        if error.code==SnapshotErrorCode::Conflict)
    );
    assert_eq!(
        ledger
            .consume("consume-before-deadline", &handoff, &access, 60_000)
            .await
            .unwrap(),
        lease
    );
    assert!(
        matches!(ledger.inspect_operation(&second, lease.operation_id(), lease.operation_digest()).await.unwrap(),
        MetadataLeaseObservation::Committed { event, state, .. } if *event==lease && state=="ACTIVE")
    );
    assert_eq!(count(&first, "mst2_metadata_prepare_expiry").await, 0);
    assert_eq!(count(&first, "mst2_metadata_prepare_consumption").await, 1);
    assert_eq!(count(&first, "mst2_metadata_lease_operation").await, 1);
    assert_eq!(count(&first, "mst2_retention_root").await, 2);
    let stored = load_plan(&first, intent.operation_id(), &intent.manifest_digest())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.record.state, "CONSUMED");
    verify_graph_root(
        &second,
        &stored,
        &format!("lease:{}", lease.lease_id()),
        "lease",
    )
    .await
    .unwrap();
}

#[test]
fn metadata_lease_descriptor_fixture_requires_exact_plan_sid_profile_and_tagged_source() {
    let prepared = prepared();
    let plan = prepared.install_plan().unwrap();
    let valid = binding(plan.root, 1);
    checked_descriptor(&valid, &plan).unwrap();
    let mut wrong = valid.clone();
    wrong.canonical_descriptor = binding([0; 32], 1).canonical_descriptor;
    assert_eq!(
        checked_descriptor(&wrong, &plan).unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    let mut wrong = valid.clone();
    wrong.tagged_root_commit_oid = format!("sha256:{}", "a".repeat(64));
    assert_eq!(
        checked_descriptor(&wrong, &plan).unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    let mut wrong = valid;
    wrong.canonical_descriptor.push(0);
    assert!(checked_descriptor(&wrong, &plan).is_err());
}

#[tokio::test]
async fn metadata_lease_two_real_primary_connections_consume_once_and_recover_exact_binding() {
    let (first, second, _schema) = fixture().await;
    let (a, handoff, prepared_receipt) = handoff(&first, "prepare-once", 1).await;
    let b = PostgresMetadataLeaseRepository::new(second.clone())
        .await
        .unwrap();
    let access = access(&first).await;
    let (left, right) = tokio::join!(
        a.consume("consume-once", &handoff, &access, 60_000),
        b.consume("consume-once", &handoff, &access, 60_000)
    );
    let receipt = left.unwrap();
    assert_eq!(receipt, right.unwrap());
    for table in [
        "mst2_metadata_catalog",
        "mst2_metadata_lease",
        "mst2_metadata_prepare_consumption",
        "mst2_metadata_lease_operation",
    ] {
        assert_eq!(count(&second, table).await, 1, "{table}");
    }
    assert_eq!(count(&second, "mst2_retention_root").await, 2);
    assert_eq!(count(&second, "mst2_retention_edge").await, 1);
    assert_eq!(count(&second, "mst2_metadata_payload").await, 2);
    let state: String = second
        .query_one_raw(statement("SELECT state FROM mst2_metadata_prepare", []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(state, "CONSUMED");
    assert!(
        matches!(a.inspect_operation(&second,receipt.operation_id(),receipt.operation_digest()).await.unwrap(),
        MetadataLeaseObservation::Committed { event, state,version:1,.. } if *event==receipt && state=="ACTIVE")
    );
    assert_eq!(
        rejection(
            a.consume("consume-once", &handoff, &access, 60_001)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::Conflict
    );
    assert_eq!(count(&second, "mst2_metadata_lease").await, 1);
    let install = PostgresMetadataInstallRepository::new(first).await.unwrap();
    assert!(
        matches!(install.finalize(prepared_receipt.intent()).await.unwrap_err(),
        super::super::native_metadata_install::MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
    assert!(
        matches!(install.inspect_prepare(&second, &prepared_receipt.intent().recovery(super::super::native_metadata_install::MetadataCommitPhase::Finalize)).await.unwrap_err(),
        super::super::native_metadata_install::MetadataInstallError::Rejected(error) if error.code==SnapshotErrorCode::Conflict)
    );
}

#[tokio::test]
async fn metadata_lease_outer_rollback_restores_prepare_and_exposes_no_provisional_catalog_or_lease()
 {
    let (first, second, _schema) = fixture().await;
    let (ledger, handoff, _receipt) = handoff(&first, "rollback-prepare", 1).await;
    let access = access(&first).await;
    let digest = operation_digest("rollback-consume", &handoff, &access, 60_000).unwrap();
    let txn = ledger.install.transaction().await.unwrap();
    let provisional = ledger
        .consume_in_txn(&txn, "rollback-consume", digest, &handoff, &access, 60_000)
        .await
        .unwrap();
    assert_eq!(count(&txn, "mst2_metadata_lease").await, 1);
    for table in [
        "mst2_metadata_catalog",
        "mst2_metadata_lease",
        "mst2_metadata_prepare_consumption",
        "mst2_metadata_lease_operation",
    ] {
        assert_eq!(
            count(&second, table).await,
            0,
            "uncommitted {table} escaped"
        );
    }
    txn.rollback().await.unwrap();
    let state: String = second
        .query_one_raw(statement("SELECT state FROM mst2_metadata_prepare", []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(state, "COMMITTED");
    assert_eq!(count(&second, "mst2_retention_root").await, 2);
    assert_eq!(
        ledger
            .inspect_operation(&second, "rollback-consume", digest)
            .await
            .unwrap(),
        MetadataLeaseObservation::Absent
    );
    let committed = ledger
        .consume("rollback-consume", &handoff, &access, 60_000)
        .await
        .unwrap();
    assert_ne!(committed.lease_id(), provisional.lease_id());
    assert_eq!(count(&second, "mst2_retention_root").await, 2);
}

#[tokio::test]
async fn metadata_lease_current_policy_and_live_pins_fail_closed_without_repair() {
    let (first, second, _schema) = fixture().await;
    let (ledger, handoff, _receipt) = handoff(&first, "policy-prepare", 1).await;
    let mut access = access(&first).await;
    access.generation = 2;
    assert_eq!(
        rejection(
            ledger
                .consume("policy-consume", &handoff, &access, 60_000)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ScopeForbidden
    );
    assert_eq!(count(&second, "mst2_metadata_lease").await, 0);
    access.generation = 1;
    let receipt = ledger
        .consume("policy-consume", &handoff, &access, 60_000)
        .await
        .unwrap();
    first
        .execute_unprepared("UPDATE mst2_metadata_access_generation SET enabled=false,generation=2")
        .await
        .unwrap();
    assert_eq!(
        rejection(
            ledger
                .consume("policy-consume", &handoff, &access, 60_000)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ScopeForbidden
    );
    PostgresRetentionRepository::new(first.clone())
        .release_root(&RetentionRoot::Lease(receipt.lease_id().into()))
        .await
        .unwrap();
    assert_eq!(
        rejection(
            ledger
                .inspect_operation(&second, receipt.operation_id(), receipt.operation_digest())
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(count(&second, "mst2_retention_root").await, 0);
    assert_eq!(count(&second, "mst2_metadata_lease_operation").await, 1);
    let (other, _other_connection, _other_schema) = fixture().await;
    assert!(matches!(
        ledger
            .inspect_operation(&other, receipt.operation_id(), receipt.operation_digest())
            .await
            .unwrap_err(),
        MetadataLeaseError::CommitUncertain { .. }
    ));
}
