use super::*;
use crate::jupiter::{
    migration::test_upgrade_reader_retention,
    storage::qualified_metadata_family::{
        RootedLookupStatus, RootedQualifiedMetadataRepository,
        provision_or_verify_rooted_qualified_family, reader_previous_fixture::restore_previous,
    },
};

#[path = "snapshot_native_runtime_upgrade_tests.rs"]
mod native_runtime;

async fn permanent_rows(fixture: &Fixture) -> Value {
    let txn = transaction(fixture).await;
    let rows=txn.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT jsonb_build_object(
           'sessions',(SELECT jsonb_agg(to_jsonb(x) ORDER BY snapshot_id,session_incarnation) FROM mst2_qualified_session_incarnation x),
           'leases',(SELECT jsonb_agg(to_jsonb(x) ORDER BY lease_id) FROM mst2_qualified_lease_binding x),
           'sources',(SELECT jsonb_agg(to_jsonb(x) ORDER BY attestation_id) FROM mst2_metadata_source_root_attestation x),
           'certificates',(SELECT jsonb_agg(to_jsonb(x) ORDER BY page_id,generation) FROM mst2_metadata_page_certificate x)) AS rows"))
        .await.unwrap().unwrap().try_get("","rows").unwrap();
    txn.commit().await.unwrap();
    rows
}

#[tokio::test]
async fn current_reader_retention_migration_verifies_fresh_family_without_rewriting_history() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let before = permanent_rows(&fixture).await;
    let high_water = q_count(
        &fixture,
        "SELECT high_water FROM {q}.mst2_metadata_reader_issuance",
    )
    .await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    test_upgrade_reader_retention(core).await.unwrap();
    assert_eq!(permanent_rows(&fixture).await, before);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        high_water
    );
    provision_or_verify_rooted_qualified_family(core)
        .await
        .unwrap();
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn captured_previous_q_upgrade_preserves_old_sid_source_proofs_and_active_legacy_reader() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let pinned = fixture
        .state
        .storage
        .snapshot_context(&fixture.snapshot, &fixture.lease)
        .await
        .unwrap();
    let before = permanent_rows(&fixture).await;
    let txn = transaction(&fixture).await;
    let active = admission(&txn, &fixture).await;
    txn.commit().await.unwrap();
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    restore_previous(core, &schema, true).await;
    assert_eq!(permanent_rows(&fixture).await, before);
    test_upgrade_reader_retention(core).await.unwrap();
    assert_eq!(permanent_rows(&fixture).await, before);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE reader_issuance=0 AND state='ACTIVE'").await,1);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE reader_issuance=0 AND anchor_kind IN ('REQUEST','READER')").await,2);
    let restarted =
        RootedQualifiedMetadataRepository::open(core, &fixture.state.storage.config().database)
            .await
            .unwrap();
    let fixed = restarted
        .context(&fixture.snapshot, &fixture.lease, &pinned.built.instance_id)
        .await
        .unwrap();
    assert_eq!(fixed.built.descriptor, pinned.built.descriptor);
    assert_eq!(fixed.commit_oid, pinned.commit_oid);
    let lookup = restarted
        .lookup_metadata(&fixed, &["/file".to_owned()])
        .await
        .unwrap();
    assert!(matches!(
        lookup.results.as_slice(),
        [RootedLookupStatus::File { .. }]
    ));
    assert_eq!(permanent_rows(&fixture).await, before);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE reader_issuance=0 AND state='ACTIVE'").await,1);
    let txn = transaction(&fixture).await;
    finish(&txn, &(active.0.clone(), 0)).await;
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    assert!(prune(&txn, 64).await >= 1);
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE reader_issuance=0"
        )
        .await,
        0
    );
    provision_or_verify_rooted_qualified_family(core)
        .await
        .unwrap();
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn captured_previous_core_without_q_upgrades_before_new_provisioning() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let core = crate::jupiter::storage::init::database_connection(&config)
        .await
        .unwrap();
    let old_schema: String = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT metadata_schema FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    restore_previous(&core, &old_schema, false).await;
    test_upgrade_reader_retention(&core).await.unwrap();
    provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
    let schema: String = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT metadata_schema FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_ne!(schema, old_schema);
    let water: i64 = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT high_water FROM \"{}\".mst2_metadata_reader_issuance",
                schema.replace('"', "\"\"")
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(water, 0);
    provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
}

#[tokio::test]
async fn captured_previous_tampered_q_rejects_upgrade_without_partial_issuance_schema() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let before = permanent_rows(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    restore_previous(core, &schema, true).await;
    core.execute_unprepared(&format!(
        "ALTER TABLE \"{}\".mst2_metadata_reader_operation ADD COLUMN unexpected integer",
        schema.replace('"', "\"\"")
    ))
    .await
    .unwrap();
    assert!(test_upgrade_reader_retention(core).await.is_err());
    assert_eq!(permanent_rows(&fixture).await, before);
    let exists: bool = core
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT to_regclass($1) IS NULL AS absent",
            [format!("{schema}.mst2_metadata_reader_issuance").into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "absent")
        .unwrap();
    assert!(exists);
    let implementation:String=core.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT encode(implementation_fingerprint,'hex') FROM mst2_qualified_family_policy WHERE singleton=1"))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap();
    assert_eq!(
        implementation,
        "6d7095dc052e60bf6de21a987b72e23fdfbfe819355f0e847d2a7f3c1cbd3f27"
    );
}
