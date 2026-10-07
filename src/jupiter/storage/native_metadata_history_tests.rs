use sea_orm::{Database, PaginatorTrait};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    callisto::{mst2_metadata_current, mst2_metadata_lifetime},
    jupiter::{
        migration::Migrator,
        tests::{TestSchemaGuard, test_db_config},
    },
};

async fn fixture() -> (DatabaseConnection, DatabaseConnection, TestSchemaGuard) {
    let temp = tempfile::tempdir().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    (first, second, schema)
}

fn prepared() -> PreparedNativeMetadataRetention {
    let entries = [mst2_codec::metapage::Entry::file(
        mst2_codec::metapage::EntryKind::Regular,
        b"file",
        3,
        [42; 32],
    )];
    let child = Page::build(&entries).unwrap();
    let root_entries = [mst2_codec::metapage::Entry::dir(b"one", page_id(&child))];
    let root = Page::build(&root_entries).unwrap();
    let mut builder = crate::ceres::snapshot::retention_dag::MetadataDagBuilder::new(
        MetadataDagLimits::default(),
    );
    builder.add_directory(&child, &entries).unwrap();
    builder.add_directory(&root, &root_entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        std::sync::Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    )
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(statement(sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

fn rejected(error: MetadataTerminalError) -> SnapshotErrorCode {
    match error {
        MetadataTerminalError::Rejected(error) => error.code,
        _ => panic!("expected definite terminal rejection"),
    }
}

#[tokio::test]
async fn history_keeps_old_composite_fk_bindings_while_current_is_an_independent_watermark() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository.begin_intent("history", &pages).await.unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let page = &pages.dag().payloads()[0];
    second.execute_raw(statement(
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size)
         VALUES($1,$2,2,'RESERVED',1,$3)",
        [page.id.to_vec().into(),node_id(&page.id).into(),(page.size as i32).into()],
    )).await.unwrap();
    assert!(
        mst2_metadata_lifetime::Entity::find_by_id((page.id.to_vec(), 1))
            .one(&second)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        mst2_metadata_lifetime::Entity::find_by_id((page.id.to_vec(), 2))
            .one(&second)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        mst2_metadata_current::Entity::find_by_id(page.id.to_vec())
            .one(&second)
            .await
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    assert!(
        second
            .execute_raw(statement(
                "UPDATE mst2_metadata_current SET generation=2 WHERE page_id=$1",
                [page.id.to_vec().into()]
            ))
            .await
            .is_err()
    );
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_prepare_page p JOIN mst2_metadata_lifetime l ON l.page_id=p.page_id AND l.generation=p.generation WHERE p.generation=1").await,2);
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_payload b JOIN mst2_metadata_lifetime l ON l.page_id=b.page_id AND l.generation=b.generation WHERE b.generation=1").await,2);
    repository.finalize(&intent).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        3
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_current").await,
        2
    );
}

#[tokio::test]
async fn history_abort_fences_a_late_installer_waiting_on_the_real_retention_barrier() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let other = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository
        .begin_intent("abort-late-install", &pages)
        .await
        .unwrap();
    repository
        .install_page(&intent, &pages.dag().payloads()[0])
        .await
        .unwrap();
    let held = repository.inner.transaction().await.unwrap();
    repository.inner.barrier(&held).await.unwrap();
    let late_intent = intent.clone();
    let late_pages = pages.dag().payloads().to_vec();
    let late = tokio::spawn(async move { other.install_pages(&late_intent, &late_pages).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let waiting=held.query_one_raw(statement(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted
             AND classid=$1::integer::oid AND objid=hashtext(current_schema())::oid
             AND database=(SELECT oid FROM pg_database WHERE datname=current_database())) AS waiting",
            [RETENTION_LOCK_KEY.into()],
        )).await.unwrap().unwrap().try_get::<bool>("","waiting").unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "late installer must wait on the captured schema barrier"
        );
        tokio::task::yield_now().await;
    }
    let terminal = repository
        .terminate_in_txn(&held, &intent, MetadataTerminalAction::Abort, None)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        late.await.unwrap(),
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::IntegrityError,
            ..
        }))
    ));
    assert_eq!(repository.abort(&intent).await.unwrap(), terminal);
    assert_eq!(
        repository
            .inspect_terminal(&second, &intent, MetadataTerminalAction::Abort)
            .await
            .unwrap(),
        MetadataTerminalObservation::Terminated(Box::new(terminal))
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_prepare WHERE state='ABORTED' AND aborted_at IS NOT NULL").await,1);
}

#[tokio::test]
async fn history_abort_after_observation_rejects_old_finalize_and_preserves_other_prepares() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let other = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared();
    let owner = repository
        .begin_intent("committed-owner", &pages)
        .await
        .unwrap();
    repository
        .install_pages(&owner, pages.dag().payloads())
        .await
        .unwrap();
    let owner_receipt = repository.finalize(&owner).await.unwrap();
    let abandoned = other
        .begin_intent("abandoned-shared", &pages)
        .await
        .unwrap();
    let observation = other.observe_installed_dag(&abandoned).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        4
    );
    other.abort(&abandoned).await.unwrap();
    assert!(matches!(
        other.finalize_observation(&abandoned, &observation).await,
        Err(MetadataInstallError::Rejected(_))
    ));
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        2
    );
    assert_eq!(repository.finalize(&owner).await.unwrap(), owner_receipt);
    assert_eq!(
        rejected(repository.abort(&owner).await.unwrap_err()),
        SnapshotErrorCode::Conflict
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE state='LIVE' AND generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(&second, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
}

#[tokio::test]
async fn history_terminal_outer_rollback_keeps_preparing_protection_and_durable_recovery_active() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository
        .begin_intent("abort-rollback", &pages)
        .await
        .unwrap();
    let held = repository.inner.transaction().await.unwrap();
    repository
        .terminate_in_txn(&held, &intent, MetadataTerminalAction::Abort, None)
        .await
        .unwrap();
    held.rollback().await.unwrap();
    assert_eq!(
        repository
            .inspect_terminal(&second, &intent, MetadataTerminalAction::Abort)
            .await
            .unwrap(),
        MetadataTerminalObservation::Active
    );
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_prepare WHERE state='PREPARING' AND aborted_at IS NULL").await,1);
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    repository.finalize(&intent).await.unwrap();
}

#[tokio::test]
async fn history_retirement_replays_original_receipt_and_never_recreates_lost_prepare_coverage() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository.begin_intent("retire", &pages).await.unwrap();
    repository
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = repository.finalize(&intent).await.unwrap();
    let terminal = repository.retire_prepare_coverage(&receipt).await.unwrap();
    assert_eq!(
        repository.retire_prepare_coverage(&receipt).await.unwrap(),
        terminal
    );
    assert_eq!(
        repository
            .inspect_terminal(&second, &intent, MetadataTerminalAction::RetireCoverage)
            .await
            .unwrap(),
        MetadataTerminalObservation::Terminated(Box::new(terminal))
    );
    assert!(matches!(
        repository
            .install_pages(&intent, pages.dag().payloads())
            .await,
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::ObjectUnavailable,
            ..
        }))
    ));
    assert!(repository.finalize(&intent).await.is_err());
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        2
    );
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_prepare WHERE state='COMMITTED' AND coverage_retired_at IS NOT NULL").await,1);
    assert!(
        second
            .execute_unprepared("UPDATE mst2_metadata_prepare SET coverage_retired_at=NULL")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn history_committed_missing_payload_rejects_a_late_installer_without_repairing_corruption() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository
        .begin_intent("committed-missing", &pages)
        .await
        .unwrap();
    repository
        .install_page(&intent, &pages.dag().payloads()[0])
        .await
        .unwrap();
    // Persist a corrupt COMMITTED fixture without deleting bytes or disabling
    // any guard. A late installer must refuse to fill its missing page.
    let txn = second.begin().await.unwrap();
    repository.inner.barrier(&txn).await.unwrap();
    PostgresRetentionRepository::retain_group_in_txn(
        &txn,
        pages.dag().nodes(),
        pages.dag().edges(),
        &[RetentionRoot::Prepare(intent.prepare_id().into())],
    )
    .await
    .unwrap();
    txn.execute_unprepared("UPDATE mst2_metadata_lifetime SET state='LIVE'")
        .await
        .unwrap();
    txn.execute_raw(statement("UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp() WHERE prepare_id=$1",[intent.prepare_id().into()])).await.unwrap();
    txn.commit().await.unwrap();
    assert!(matches!(
        repository
            .install_pages(&intent, pages.dag().payloads())
            .await,
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::ObjectUnavailable,
            ..
        }))
    ));
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(
        scalar(&second, "SELECT count(*) FROM mst2_retention_root").await,
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
}

#[tokio::test]
async fn history_graph_domain_is_durable_sealed_and_cannot_be_changed_by_a_new_repository() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let mut qualified = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    qualified.graph_domain = "qualified-v1";
    let pages = prepared();
    let intent = repository
        .begin_intent("generic-domain", &pages)
        .await
        .unwrap();
    assert_eq!(intent.graph_domain, GraphDomain::Generic);
    assert!(matches!(
        qualified.begin_intent("generic-domain", &pages).await,
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::IntegrityError,
            ..
        }))
    ));
    assert!(matches!(
        qualified.begin_intent("cross-domain-page", &pages).await,
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::ObjectUnavailable,
            ..
        }))
    ));
    assert!(matches!(
        qualified
            .install_pages(&intent, pages.dag().payloads())
            .await,
        Err(MetadataInstallError::Rejected(SnapshotError {
            code: SnapshotErrorCode::IntegrityError,
            ..
        }))
    ));
    assert!(
        second
            .execute_unprepared("UPDATE mst2_metadata_prepare SET graph_domain='qualified-v1'")
            .await
            .is_err()
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE graph_domain='generic-v1'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        scalar(&second, "SELECT count(*) FROM mst2_metadata_current").await,
        2
    );
}

#[tokio::test]
async fn history_terminal_wrong_primary_is_unknown_and_current_delete_payload_update_stay_forbidden()
 {
    let (first, second, _schema) = fixture().await;
    let (other, _other_connection, _other_schema) = fixture().await;
    let repository = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repository
        .begin_intent("terminal-primary", &pages)
        .await
        .unwrap();
    repository
        .install_page(&intent, &pages.dag().payloads()[0])
        .await
        .unwrap();
    assert!(matches!(
        repository
            .inspect_terminal(&other, &intent, MetadataTerminalAction::Abort)
            .await,
        Err(MetadataTerminalError::CommitUncertain { .. })
    ));
    assert_eq!(
        repository
            .inspect_terminal(&second, &intent, MetadataTerminalAction::Abort)
            .await
            .unwrap(),
        MetadataTerminalObservation::Active
    );
    for sql in [
        "DELETE FROM mst2_metadata_current",
        "DELETE FROM mst2_metadata_lifetime",
        "DELETE FROM mst2_metadata_payload",
        "UPDATE mst2_metadata_payload SET payload=payload",
        "DELETE FROM mst2_metadata_storage_scope",
    ] {
        assert!(second.execute_unprepared(sql).await.is_err(), "{sql}");
    }
    repository.abort(&intent).await.unwrap();
    assert!(
        second
            .execute_unprepared(
                "UPDATE mst2_metadata_prepare SET state='PREPARING',aborted_at=NULL"
            )
            .await
            .is_err()
    );
    assert_eq!(
        mst2_metadata_current::Entity::find()
            .count(&first)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn history_migration_preserves_preexisting_g1_null_domain_seal_and_composite_mappings() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    let at = Migrator::migrations()
        .iter()
        .position(|migration| {
            migration.name() == "m20261007_000300_add_mst2_metadata_lifetime_history"
        })
        .unwrap();
    Migrator::up(&first, Some(at.try_into().unwrap()))
        .await
        .unwrap();
    let inner = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let plan = pages.install_plan().unwrap();
    let manifest = plan.digest().unwrap();
    let bindings = GenerationBindings(
        plan.pages
            .iter()
            .map(|(page, size)| (*page, (1, *size)))
            .collect(),
    );
    let canonical = bindings.encode().unwrap();
    let digest: [u8; 32] = Sha256::digest(&canonical).into();
    let scope = serde_json::to_vec(&(
        &inner.storage_scope.storage_uuid,
        &inner.storage_scope.database,
        inner.storage_scope.database_oid,
        &inner.storage_scope.schema,
        inner.storage_scope.schema_oid,
        &inner.storage_scope.server_address,
        inner.storage_scope.server_port,
    ))
    .unwrap();
    let prepare_id = uuid::Uuid::new_v4().to_string();
    let old_seal = seal(&prepare_id, &manifest, &plan.root, &digest, &scope, None).unwrap();
    let txn = first.begin().await.unwrap();
    inner.barrier(&txn).await.unwrap();
    for page in pages.dag().payloads() {
        txn.execute_raw(statement("INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size) VALUES($1,$2,1,'RESERVED',1,$3)",
            [page.id.to_vec().into(),node_id(&page.id).into(),(page.size as i32).into()])).await.unwrap();
        txn.execute_raw(statement("INSERT INTO mst2_metadata_payload(page_id,generation,metadata_codec,byte_size,payload) VALUES($1,1,1,$2,$3)",
            [page.id.to_vec().into(),(page.size as i32).into(),page.bytes.clone().into()])).await.unwrap();
    }
    let identity = &plan.identity;
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_prepare(prepare_id,operation_id,manifest_digest,canonical_plan,
         source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,materialization_policy,
         fs_semantics,access_projection,verification_revision,projection_revision,metadata_root,
         node_count,edge_count,total_bytes,state,canonical_bindings,bindings_digest,primary_scope,storage_seal)
         VALUES($1,'g1-before-history',$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,'PREPARING',$18,$19,$20,$21)",
        [prepare_id.clone().into(),manifest.to_vec().into(),plan.encode().unwrap().into(),identity.source_domain.clone().into(),
         identity.tagged_root_tree_oid.clone().into(),identity.scope.clone().into(),(identity.schema_version as i16).into(),
         (identity.metadata_codec as i16).into(),(identity.materialization_policy as i16).into(),(identity.fs_semantics as i16).into(),
         (identity.access_projection as i16).into(),identity.verification_revision.into(),(identity.projection_revision as i16).into(),
         plan.root.to_vec().into(),(plan.pages.len() as i32).into(),(plan.edges.len() as i32).into(),(plan.total_bytes as i64).into(),
         canonical.into(),digest.to_vec().into(),scope.into(),old_seal.to_vec().into()],
    )).await.unwrap();
    for (page, size) in &plan.pages {
        txn.execute_raw(statement("INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size) VALUES($1,$2,1,$3)",
            [prepare_id.clone().into(),page.to_vec().into(),(*size as i32).into()])).await.unwrap();
    }
    PostgresRetentionRepository::retain_group_in_txn(
        &txn,
        pages.dag().nodes(),
        pages.dag().edges(),
        &[RetentionRoot::Prepare(prepare_id.clone())],
    )
    .await
    .unwrap();
    txn.execute_unprepared("UPDATE mst2_metadata_lifetime SET state='LIVE'")
        .await
        .unwrap();
    txn.execute_raw(statement("UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp() WHERE prepare_id=$1",[prepare_id.clone().into()])).await.unwrap();
    txn.commit().await.unwrap();
    Migrator::up(&first, Some(1)).await.unwrap();
    let repository = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    let restored = repository
        .begin_intent("g1-before-history", &pages)
        .await
        .unwrap();
    assert_eq!(restored.graph_domain, GraphDomain::LegacyGeneric);
    assert_eq!(restored.storage_seal, old_seal);
    assert_eq!(restored.prepare_id(), prepare_id);
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_current WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE graph_domain IS NULL"
        )
        .await,
        1
    );
    let receipt = repository.finalize(&restored).await.unwrap();
    repository.retire_prepare_coverage(&receipt).await.unwrap();
    assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_prepare_page p JOIN mst2_metadata_lifetime l ON l.page_id=p.page_id AND l.generation=p.generation").await,2);
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
}
