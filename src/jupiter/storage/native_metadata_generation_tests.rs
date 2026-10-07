use std::sync::Arc;

use mst2_codec::metapage::{Entry, EntryKind};
use sea_orm::{Database, PaginatorTrait};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    callisto::mst2_metadata_lifetime,
    ceres::snapshot::retention_dag::MetadataDagBuilder,
    jupiter::{
        migration::Migrator,
        storage::mst2_retention::GcClaim,
        tests::{TestSchemaGuard, test_db_config},
    },
};

fn prepared(scope: &str) -> PreparedNativeMetadataRetention {
    let entries = [Entry::file(EntryKind::Regular, b"file", 3, [42; 32])];
    let child = Page::build(&entries).unwrap();
    let root_entries = [
        Entry::dir(b"one", page_id(&child)),
        Entry::dir(b"two", page_id(&child)),
    ];
    let root = Page::build(&root_entries).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &entries).unwrap();
    builder.add_directory(&root, &root_entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        scope,
    )
}

async fn fixture() -> (DatabaseConnection, DatabaseConnection, TestSchemaGuard) {
    let temp = tempfile::tempdir().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    (first, second, schema)
}

fn rejected(error: MetadataInstallError) -> SnapshotErrorCode {
    match error {
        MetadataInstallError::Rejected(error) => error.code,
        _ => panic!("expected definitive rejection"),
    }
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(statement(sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn mappings(db: &DatabaseConnection, intent: &GenerationPrepareIntent) -> GenerationBindings {
    let repository = PostgresMetadataGenerationRepository::new(db.clone())
        .await
        .unwrap();
    repository
        .require_fixed_plan(db, intent)
        .await
        .unwrap()
        .bindings
}

async fn wait_for_retention_waiter(txn: &DatabaseTransaction) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let waiting = txn.query_one_raw(statement(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted
             AND classid=$1::integer::oid AND objid=hashtext(current_schema())::oid
             AND database=(SELECT oid FROM pg_database WHERE datname=current_database())) AS waiting",
            [RETENTION_LOCK_KEY.into()],
        )).await.unwrap().unwrap().try_get::<bool>("", "waiting").unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "independent connection must wait on the schema retention barrier"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn generation_begin_serializes_full_reserved_bindings_before_any_payload() {
    let (first, second, _schema) = fixture().await;
    let a = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let left = a.begin_intent("reserved-a", &pages).await.unwrap();
    let held = a.inner.transaction().await.unwrap();
    a.inner.barrier(&held).await.unwrap();
    let right_pages = prepared("/");
    let blocked = tokio::spawn(async move { b.begin_intent("reserved-b", &right_pages).await });
    wait_for_retention_waiter(&held).await;
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    held.commit().await.unwrap();
    let right = blocked.await.unwrap().unwrap();
    assert_ne!(left.prepare_id(), right.prepare_id());
    assert_ne!(left.storage_seal, right.storage_seal);
    assert_eq!(left.bindings_digest, right.bindings_digest);
    assert_eq!(
        mappings(&first, &left).await,
        mappings(&second, &right).await
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE generation=1 AND state='RESERVED'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation=1"
        )
        .await,
        4
    );
    let replay = a.begin_intent("reserved-a", &pages).await.unwrap();
    assert_eq!(left, replay);
}

#[tokio::test]
async fn generation_partial_prepare_recovers_exact_complete_mapping_on_second_connection() {
    let (first, second, _schema) = fixture().await;
    let a = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = a.begin_intent("partial", &pages).await.unwrap();
    a.install_pages(&intent, &pages.dag().payloads()[..1])
        .await
        .unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
    drop(a);
    let restarted = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    assert_eq!(
        restarted
            .inspect_prepare(
                &second,
                "partial",
                intent.manifest_digest(),
                MetadataCommitPhase::Payload
            )
            .await
            .unwrap(),
        GenerationPrepareObservation::Preparing(intent.clone())
    );
    assert_eq!(
        restarted
            .observe_installed_dag(&intent)
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
    restarted
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = restarted.finalize(&intent).await.unwrap();
    assert_eq!(receipt.intent(), &intent);
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE state='LIVE' AND generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn generation_shared_live_prepare_pins_batch_before_generic_gc_can_claim() {
    let (first, second, _schema) = fixture().await;
    let a = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let old = a.begin_intent("live-old", &pages).await.unwrap();
    a.install_pages(&old, pages.dag().payloads()).await.unwrap();
    a.finalize(&old).await.unwrap();
    let shared = b.begin_intent("live-shared", &pages).await.unwrap();
    assert_eq!(
        mappings(&first, &old).await,
        mappings(&second, &shared).await
    );
    let graph = PostgresRetentionRepository::new(first.clone());
    graph
        .release_root(&RetentionRoot::Prepare(old.prepare_id().into()))
        .await
        .unwrap();
    assert_eq!(
        graph
            .mark_deleting("generic-cannot-claim-shared", &node_id(&pages.dag().root()))
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    assert_eq!(
        scalar(
            &second,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        2
    );
    b.install_pages(&shared, pages.dag().payloads())
        .await
        .unwrap();
    b.finalize(&shared).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_edge").await,
        1
    );
    assert_eq!(
        scalar(&first, "SELECT max(generation) FROM mst2_metadata_lifetime").await,
        1
    );
}

#[tokio::test]
async fn generation_observation_is_bound_to_prepare_not_only_the_same_dag() {
    let (first, second, _schema) = fixture().await;
    let a = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataGenerationRepository::new(second)
        .await
        .unwrap();
    let pages = prepared("/");
    let left = a.begin_intent("observation-left", &pages).await.unwrap();
    let right = b.begin_intent("observation-right", &pages).await.unwrap();
    a.install_pages(&left, pages.dag().payloads())
        .await
        .unwrap();
    let observation = a.observe_installed_dag(&left).await.unwrap();
    assert_eq!(
        rejected(
            b.finalize_observation(&right, &observation)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
    let receipt = a.finalize_observation(&left, &observation).await.unwrap();
    assert_eq!(receipt, a.finalize(&left).await.unwrap());
}

#[tokio::test]
async fn generation_finalization_rechecks_changed_lifetime_after_lock_free_observation() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("state-change", &pages)
        .await
        .unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let observed = repository.observe_installed_dag(&intent).await.unwrap();
    let txn = second.begin().await.unwrap();
    repository.inner.barrier(&txn).await.unwrap();
    // Fault injection only: G1 deliberately has no collector transition API.
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_lifetime DISABLE TRIGGER mst2_metadata_lifetime_guard",
    )
    .await
    .unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_lifetime SET state='DELETING' WHERE page_id=$1",
        [pages.dag().root().to_vec().into()],
    ))
    .await
    .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_lifetime ENABLE TRIGGER mst2_metadata_lifetime_guard",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        rejected(
            repository
                .finalize_observation(&intent, &observed)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='PREPARING'"
        )
        .await,
        1
    );
    assert_eq!(
        mst2_metadata_payload::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn generation_deleting_removed_and_generic_tombstone_are_never_reopened() {
    for state in ["DELETING", "REMOVED"] {
        let (first, second, _schema) = fixture().await;
        let repository = PostgresMetadataGenerationRepository::new(first.clone())
            .await
            .unwrap();
        let pages = prepared("/");
        let old = repository.begin_intent("old", &pages).await.unwrap();
        let txn = second.begin().await.unwrap();
        repository.inner.barrier(&txn).await.unwrap();
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_lifetime DISABLE TRIGGER mst2_metadata_lifetime_guard",
        )
        .await
        .unwrap();
        txn.execute_raw(statement(
            "UPDATE mst2_metadata_lifetime SET state=$1 WHERE page_id=$2",
            [state.into(), pages.dag().root().to_vec().into()],
        ))
        .await
        .unwrap();
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_lifetime ENABLE TRIGGER mst2_metadata_lifetime_guard",
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            rejected(
                repository
                    .begin_intent("fresh-disallowed", &pages)
                    .await
                    .unwrap_err()
            ),
            SnapshotErrorCode::ObjectUnavailable
        );
        assert_eq!(
            rejected(
                repository
                    .install_pages(&old, pages.dag().payloads())
                    .await
                    .unwrap_err()
            ),
            SnapshotErrorCode::ObjectUnavailable
        );
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare").await,
            1
        );
        assert_eq!(
            scalar(&first, "SELECT max(generation) FROM mst2_metadata_lifetime").await,
            1
        );
    }
    let (first, second, _schema) = fixture().await;
    let pages = prepared("/");
    let graph = PostgresRetentionRepository::new(second.clone());
    graph
        .retain_group(pages.dag().nodes(), pages.dag().edges(), &[])
        .await
        .unwrap();
    assert_eq!(
        graph
            .mark_deleting("legacy-remove", &node_id(&pages.dag().root()))
            .await
            .unwrap(),
        GcClaim::Marked
    );
    graph.complete_gc("legacy-remove").await.unwrap();
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    assert_eq!(
        rejected(
            repository
                .begin_intent("no-resurrection", &pages)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare").await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        0
    );
    assert_eq!(
        graph
            .retain_group(pages.dag().nodes(), pages.dag().edges(), &[])
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::ObjectUnavailable
    );
}

#[tokio::test]
async fn generation_legacy_null_capability_remains_legacy_and_does_not_block_existing_installer() {
    let (first, second, _schema) = fixture().await;
    let legacy = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let fresh = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let old = legacy
        .begin_intent("legacy-operation", &pages)
        .await
        .unwrap();
    legacy
        .install_pages(&old, pages.dag().payloads())
        .await
        .unwrap();
    let original_receipt = legacy.finalize(&old).await.unwrap();
    assert_eq!(
        rejected(
            fresh
                .begin_intent("legacy-operation", &pages)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::ObjectUnavailable
    );
    assert_eq!(
        rejected(
            fresh
                .begin_intent("new-over-legacy-cas", &pages)
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(legacy.finalize(&old).await.unwrap(), original_receipt);
    let another = legacy
        .begin_intent("legacy-new-resolve", &pages)
        .await
        .unwrap();
    legacy
        .install_pages(&another, pages.dag().payloads())
        .await
        .unwrap();
    legacy.finalize(&another).await.unwrap();
    assert_eq!(
        scalar(
            &second,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NULL"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &second,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE storage_seal IS NULL"
        )
        .await,
        2
    );
    assert_eq!(
        mst2_metadata_lifetime::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn generation_storage_seal_scope_and_complete_child_binding_cannot_be_substituted() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("seal-tamper", &pages)
        .await
        .unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let mut wrong_bindings = intent.clone();
    wrong_bindings.bindings_digest[0] ^= 1;
    let mut wrong_seal = intent.clone();
    wrong_seal.storage_seal[0] ^= 1;
    let mut wrong_root = intent.clone();
    wrong_root.metadata_root[0] ^= 1;
    for wrong in [wrong_bindings, wrong_seal, wrong_root] {
        assert_eq!(
            rejected(
                repository
                    .install_pages(&wrong, pages.dag().payloads())
                    .await
                    .unwrap_err()
            ),
            SnapshotErrorCode::IntegrityError
        );
    }
    let mut wrong_scope = intent.clone();
    wrong_scope.primary_scope[0] ^= 1;
    assert_eq!(
        repository
            .observe_installed_dag(&wrong_scope)
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    let child = pages
        .dag()
        .payloads()
        .iter()
        .find(|page| page.id != pages.dag().root())
        .unwrap();
    let txn = second.begin().await.unwrap();
    repository.inner.barrier(&txn).await.unwrap();
    txn.execute_unprepared("ALTER TABLE mst2_metadata_prepare_page DISABLE TRIGGER mst2_metadata_generation_mapping_guard").await.unwrap();
    txn.execute_raw(statement("UPDATE mst2_metadata_prepare_page SET expected_size=expected_size+1 WHERE prepare_id=$1 AND page_id=$2", [intent.prepare_id().into(), child.id.to_vec().into()])).await.unwrap();
    txn.execute_unprepared("ALTER TABLE mst2_metadata_prepare_page ENABLE TRIGGER mst2_metadata_generation_mapping_guard").await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        repository
            .observe_installed_dag(&intent)
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
}

#[tokio::test]
async fn generation_compact_receipt_checks_actual_seal_without_loading_the_dag() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository.begin_intent("compact", &pages).await.unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = repository.finalize(&intent).await.unwrap();
    let source = receipt.legacy.identity.tagged_root_tree_oid.clone();
    let txn = second.begin().await.unwrap();
    repository
        .verify_receipt_in_txn(&txn, &receipt, &source, "/")
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    let txn = second.begin().await.unwrap();
    repository.inner.barrier(&txn).await.unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare DISABLE TRIGGER mst2_metadata_generation_seal_guard",
    )
    .await
    .unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_prepare SET storage_seal=$1 WHERE prepare_id=$2",
        [vec![0u8; 32].into(), intent.prepare_id().into()],
    ))
    .await
    .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare ENABLE TRIGGER mst2_metadata_generation_seal_guard",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let txn = second.begin().await.unwrap();
    assert_eq!(
        repository
            .verify_receipt_in_txn(&txn, &receipt, &source, "/")
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    let txn = second.begin().await.unwrap();
    repository.inner.barrier(&txn).await.unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare DISABLE TRIGGER mst2_metadata_generation_seal_guard",
    )
    .await
    .unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_prepare SET storage_seal=$1,
         canonical_bindings=set_byte(canonical_bindings,60,get_byte(canonical_bindings,60)#1) WHERE prepare_id=$2",
        [intent.storage_seal.to_vec().into(),intent.prepare_id().into()],
    )).await.unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare ENABLE TRIGGER mst2_metadata_generation_seal_guard",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let txn = second.begin().await.unwrap();
    assert_eq!(
        repository
            .verify_receipt_in_txn(&txn, &receipt, &source, "/")
            .await
            .unwrap_err()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn generation_db_guards_preserve_watermark_payload_scope_and_committed_mappings() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository.begin_intent("guards", &pages).await.unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    repository.finalize(&intent).await.unwrap();
    for sql in [
        "DELETE FROM mst2_metadata_lifetime",
        "UPDATE mst2_metadata_lifetime SET generation=2",
        "UPDATE mst2_metadata_lifetime SET state='REMOVED'",
        "DELETE FROM mst2_metadata_payload",
        "UPDATE mst2_metadata_payload SET generation=1",
        "DELETE FROM mst2_metadata_storage_scope",
        "UPDATE mst2_metadata_storage_scope SET storage_uuid=storage_uuid",
        "DELETE FROM mst2_metadata_prepare_page",
        "UPDATE mst2_metadata_prepare_page SET generation=1",
        "UPDATE mst2_metadata_prepare SET storage_seal=NULL,canonical_bindings=NULL,bindings_digest=NULL,primary_scope=NULL",
    ] {
        assert!(
            second.execute_unprepared(sql).await.is_err(),
            "guard must reject {sql}"
        );
    }
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE generation=1 AND state='LIVE'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        repository.finalize(&intent).await.unwrap().intent(),
        &intent
    );
    assert_eq!(scalar(&first, "SELECT count(*) FROM pg_indexes WHERE schemaname=current_schema() AND indexname='idx_mst2_metadata_prepare_page_lifetime'").await, 1);
}

#[tokio::test]
async fn generation_finalize_row_lock_fences_a_late_mapping_insert_on_another_connection() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("late-mapping", &pages)
        .await
        .unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let observation = repository.observe_installed_dag(&intent).await.unwrap();
    let held = repository.inner.transaction().await.unwrap();
    repository.inner.barrier(&held).await.unwrap();
    lock_prepare(&held, intent.prepare_id()).await.unwrap();
    let contender = second.begin().await.unwrap();
    let pid = contender
        .query_one_raw(statement("SELECT pg_backend_pid() AS pid", []))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i32>("", "pid")
        .unwrap();
    let prepare_id = intent.prepare_id().to_owned();
    let root = pages.dag().root();
    let size = pages.install_plan().unwrap().pages[&root] as i32;
    let blocked = tokio::spawn(async move {
        let result = contender.execute_raw(statement(
            "INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size) VALUES($1,$2,1,$3)",
            [prepare_id.into(), root.to_vec().into(), size.into()],
        )).await;
        contender.rollback().await.unwrap();
        result
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let waiting = held
            .query_one_raw(statement(
                "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND NOT granted) AS waiting",
                [pid.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<bool>("", "waiting")
            .unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "late mapping writer must wait for the prepare row lock"
        );
        tokio::task::yield_now().await;
    }
    repository
        .finalize_in_txn(&held, &intent, &observation)
        .await
        .unwrap();
    held.commit().await.unwrap();
    let error = blocked.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("metadata generation mappings are immutable"),
        "{error}"
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='COMMITTED'"
        )
        .await,
        1
    );
    repository.finalize(&intent).await.unwrap();
}

#[tokio::test]
async fn generation_wrong_primary_never_upgrades_intent_or_reports_false_absence() {
    let (first, same_primary, _schema) = fixture().await;
    let (other, _other_connection, _other_schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let wrong = PostgresMetadataGenerationRepository::new(other.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("captured-primary", &pages)
        .await
        .unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = repository.finalize(&intent).await.unwrap();
    assert_eq!(
        rejected(
            wrong
                .install_pages(&intent, pages.dag().payloads())
                .await
                .unwrap_err()
        ),
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(
        wrong.observe_installed_dag(&intent).await.unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    assert!(matches!(
        repository
            .inspect_prepare(
                &other,
                "captured-primary",
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await,
        Err(MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Finalize,
            ..
        })
    ));
    assert_eq!(
        repository
            .inspect_prepare(
                &same_primary,
                "captured-primary",
                intent.manifest_digest(),
                MetadataCommitPhase::Finalize
            )
            .await
            .unwrap(),
        GenerationPrepareObservation::Committed(Box::new(receipt))
    );
    assert_eq!(
        scalar(&other, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        2
    );
}
