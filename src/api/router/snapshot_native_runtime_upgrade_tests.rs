use super::*;
use crate::jupiter::{
    migration::{apply_migrations, test_upgrade_native_runtime},
    storage::qualified_metadata_family::reader_previous_fixture::restore_retention,
};

#[path = "snapshot_descriptor_upgrade_tests.rs"]
mod descriptor_wire;

async fn retained_owners(fixture: &Fixture) -> Value {
    let txn = transaction(fixture).await;
    let value = txn.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT jsonb_build_object(
           'readers',(SELECT jsonb_agg(to_jsonb(x) ORDER BY operation_id,reader_issuance) FROM mst2_metadata_reader_operation x),
           'anchors',(SELECT jsonb_agg(to_jsonb(x) ORDER BY anchor_id) FROM mst2_metadata_root_anchor x),
           'issuance',(SELECT to_jsonb(x) FROM mst2_metadata_reader_issuance x),
           'functions',(SELECT jsonb_agg(jsonb_build_array(p.proname,p.oid::bigint) ORDER BY p.proname,p.oid)
              FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace
              WHERE n.nspname=current_schema() OR (n.nspname=(SELECT c.nspname FROM mst2_metadata_family_identity i
                  JOIN pg_catalog.pg_namespace c ON c.oid=i.core_schema_oid LIMIT 1)
                AND p.proname='mst2_route_family_registration_guard'))) AS owners"))
        .await.unwrap().unwrap().try_get("", "owners").unwrap();
    txn.commit().await.unwrap();
    value
}

async fn policy(core: &sea_orm::DatabaseConnection) -> Value {
    core.query_one_raw(Statement::from_string(
        DbBackend::Postgres,
        "SELECT to_jsonb(p) FROM mst2_qualified_family_policy p WHERE singleton=1",
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

async fn seed_live_and_terminal(fixture: &Fixture) {
    let txn = transaction(fixture).await;
    admission(&txn, fixture).await;
    txn.commit().await.unwrap();
    let txn = transaction(fixture).await;
    let terminal = admission(&txn, fixture).await;
    finish(&txn, &terminal).await;
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        1
    );
    assert_eq!(
        q_count(
            fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='FINISHED'"
        )
        .await,
        1
    );
    assert!(
        q_count(
            fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await
            >= 2
    );
}

async fn assert_old_sid_works(fixture: &Fixture) {
    let pinned = fixture
        .state
        .storage
        .snapshot_context(&fixture.snapshot, &fixture.lease)
        .await
        .unwrap();
    let repository = RootedQualifiedMetadataRepository::open(
        fixture.state.storage.mono_storage().get_connection(),
        &fixture.state.storage.config().database,
    )
    .await
    .unwrap();
    let context = repository
        .context(&fixture.snapshot, &fixture.lease, &pinned.built.instance_id)
        .await
        .unwrap();
    assert_eq!(context.built.descriptor, pinned.built.descriptor);
    assert_eq!(context.commit_oid, pinned.commit_oid);
    let lookup = repository
        .lookup_metadata(&context, &["/file".to_owned()])
        .await
        .unwrap();
    assert!(matches!(
        lookup.results.as_slice(),
        [RootedLookupStatus::File { .. }]
    ));
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn captured_cc90_upgrade_preserves_nonzero_reader_issuance_all_owners_oids_and_old_sid() {
    let fixture = Fixture::new_with_pg_config(true).await;
    seed_live_and_terminal(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    let rows = permanent_rows(&fixture).await;
    let owners = retained_owners(&fixture).await;
    restore_retention(core, &schema, true, false).await;
    assert_eq!(retained_owners(&fixture).await, owners);
    test_upgrade_native_runtime(core).await.unwrap();
    assert_eq!(permanent_rows(&fixture).await, rows);
    assert_eq!(retained_owners(&fixture).await, owners);
    let stamp = policy(core).await;
    test_upgrade_native_runtime(core).await.unwrap();
    assert_eq!(policy(core).await, stamp);
    assert_eq!(retained_owners(&fixture).await, owners);
    assert_eq!(permanent_rows(&fixture).await, rows);
    assert_old_sid_works(&fixture).await;
    assert_eq!(permanent_rows(&fixture).await, rows);
    provision_or_verify_rooted_qualified_family(core)
        .await
        .unwrap();
}

#[tokio::test]
async fn captured_cc90_interrupted_before_reader_migration_resumes_without_reader_ddl() {
    let fixture = Fixture::new_with_pg_config(true).await;
    seed_live_and_terminal(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    restore_retention(core, &schema, true, false).await;
    let rows = permanent_rows(&fixture).await;
    let owners = retained_owners(&fixture).await;
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM seaql_migrations WHERE version IN ($1,$2,$3)",
        [
            "m20261008_000400_add_mst2_reader_retention".into(),
            "m20261008_000500_fix_mst2_native_runtime".into(),
            "m20261008_000600_fix_mst2_descriptor_wire".into(),
        ],
    ))
    .await
    .unwrap();
    apply_migrations(core, false).await.unwrap();
    assert_eq!(permanent_rows(&fixture).await, rows);
    assert_eq!(retained_owners(&fixture).await, owners);
    let count: i64 = core.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT count(*) FROM seaql_migrations WHERE version IN
            ('m20261008_000400_add_mst2_reader_retention','m20261008_000500_fix_mst2_native_runtime',
             'm20261008_000600_fix_mst2_descriptor_wire')"))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap();
    assert_eq!(count, 3);
    assert_old_sid_works(&fixture).await;
}

async fn qless_upgrade(legacy: bool) {
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
    restore_retention(&core, &old_schema, false, legacy).await;
    test_upgrade_native_runtime(&core).await.unwrap();
    let stamp = policy(&core).await;
    test_upgrade_native_runtime(&core).await.unwrap();
    assert_eq!(policy(&core).await, stamp);
    provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
    provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
    let row = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT metadata_schema FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'",
        ))
        .await
        .unwrap()
        .unwrap();
    let schema: String = row.try_get_by_index(0).unwrap();
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
}

#[tokio::test]
async fn captured_cc90_qless_canonical_policy_upgrades_before_new_provisioning() {
    qless_upgrade(false).await;
}

#[tokio::test]
async fn captured_cc90_qless_exact_legacy_deparse_policy_upgrades_before_new_provisioning() {
    qless_upgrade(true).await;
}

#[tokio::test]
async fn captured_cc90_tampered_decoder_rejects_upgrade_without_rewriting_owners_or_policy() {
    let fixture = Fixture::new_with_pg_config(true).await;
    seed_live_and_terminal(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    restore_retention(core, &schema, true, false).await;
    let before = policy(core).await;
    let owners = retained_owners(&fixture).await;
    let rows = permanent_rows(&fixture).await;
    core.execute_unprepared(&format!(
        "CREATE OR REPLACE FUNCTION \"{}\".mst2_metadata_decode_rooted_plan(b bytea) RETURNS jsonb
           LANGUAGE plpgsql IMMUTABLE STRICT AS $$ BEGIN RAISE EXCEPTION 'tampered decoder'; END $$",
        schema.replace('"', "\"\""))).await.unwrap();
    assert!(test_upgrade_native_runtime(core).await.is_err());
    assert_eq!(policy(core).await, before);
    assert_eq!(retained_owners(&fixture).await, owners);
    assert_eq!(permanent_rows(&fixture).await, rows);
}

#[tokio::test]
async fn captured_cc90_qless_arbitrary_policy_shape_is_never_resigned() {
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (config, _schema) = test_db_config(temp.path()).await;
        let core = crate::jupiter::storage::init::database_connection(&config)
            .await
            .unwrap();
        let schema: String = core.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT metadata_schema FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'"))
            .await.unwrap().unwrap().try_get_by_index(0).unwrap();
        restore_retention(&core, &schema, false, legacy).await;
        core.execute_unprepared("ALTER TABLE mst2_qualified_family_policy DISABLE TRIGGER mst2_route_family_policy_immutable;
            UPDATE mst2_qualified_family_policy SET expected_shape=decode(repeat('a5',32),'hex') WHERE singleton=1;
            ALTER TABLE mst2_qualified_family_policy ENABLE TRIGGER mst2_route_family_policy_immutable").await.unwrap();
        let before = policy(&core).await;
        let schemas: Value = core.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT coalesce(jsonb_agg(n.nspname ORDER BY n.nspname),'[]'::jsonb) FROM pg_catalog.pg_namespace n
                WHERE EXISTS(SELECT 1 FROM pg_catalog.pg_proc p WHERE p.pronamespace=n.oid
                  AND p.proname='mst2_metadata_dml_barrier'
                  AND strpos(p.prosrc,chr(34)||current_schema()||chr(34))>0)"))
            .await.unwrap().unwrap().try_get_by_index(0).unwrap();
        assert!(test_upgrade_native_runtime(&core).await.is_err());
        assert_eq!(policy(&core).await, before);
        let after: Value = core.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT coalesce(jsonb_agg(n.nspname ORDER BY n.nspname),'[]'::jsonb) FROM pg_catalog.pg_namespace n
                WHERE EXISTS(SELECT 1 FROM pg_catalog.pg_proc p WHERE p.pronamespace=n.oid
                  AND p.proname='mst2_metadata_dml_barrier'
                  AND strpos(p.prosrc,chr(34)||current_schema()||chr(34))>0)"))
            .await.unwrap().unwrap().try_get_by_index(0).unwrap();
        assert_eq!(
            after, schemas,
            "rejected migration must roll back every temporary template"
        );
    }
}
