use super::*;
use crate::jupiter::storage::qualified_metadata_family::reader_previous_fixture::restore_native_runtime;

async fn remove_upgrade_ledgers(core: &sea_orm::DatabaseConnection) {
    core.execute_unprepared(
        "DELETE FROM seaql_migrations WHERE version IN (
            'm20261008_000400_add_mst2_reader_retention',
            'm20261008_000500_fix_mst2_native_runtime',
            'm20261008_000600_fix_mst2_descriptor_wire')",
    )
    .await
    .unwrap();
}

async fn assert_upgrade_ledgers(core: &sea_orm::DatabaseConnection) {
    let count: i64 = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) FROM seaql_migrations WHERE version IN (
                'm20261008_000400_add_mst2_reader_retention',
                'm20261008_000500_fix_mst2_native_runtime',
                'm20261008_000600_fix_mst2_descriptor_wire')",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(count, 3);
}

async fn descriptor_matches_stored(fixture: &Fixture) -> i64 {
    q_count(
        fixture,
        "SELECT count(*) FROM {q}.mst2_qualified_session_incarnation session
         WHERE session.state='READY' AND session.canonical_descriptor=
            {q}.mst2_metadata_descriptor(session.prepare_id,session.instance_id,
                session.commit_oid,session.root_tree_oid)",
    )
    .await
}

#[tokio::test]
async fn captured_87b_descriptor_upgrade_preserves_owners_oids_and_replays_missing_ledgers() {
    let fixture = Fixture::new_with_pg_config(true).await;
    seed_live_and_terminal(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    for missing_ledgers in [false, true] {
        let rows = permanent_rows(&fixture).await;
        let owners = retained_owners(&fixture).await;
        restore_native_runtime(core, &schema, true).await;
        assert_eq!(descriptor_matches_stored(&fixture).await, 0);
        assert_eq!(permanent_rows(&fixture).await, rows);
        assert_eq!(retained_owners(&fixture).await, owners);
        if missing_ledgers {
            remove_upgrade_ledgers(core).await;
            apply_migrations(core, false).await.unwrap();
            assert_upgrade_ledgers(core).await;
        } else {
            test_upgrade_native_runtime(core).await.unwrap();
        }
        assert_eq!(descriptor_matches_stored(&fixture).await, 1);
        assert_eq!(permanent_rows(&fixture).await, rows);
        assert_eq!(retained_owners(&fixture).await, owners);
        let stamp = policy(core).await;
        if missing_ledgers {
            apply_migrations(core, false).await.unwrap();
        } else {
            test_upgrade_native_runtime(core).await.unwrap();
        }
        assert_eq!(policy(core).await, stamp);
        assert_eq!(permanent_rows(&fixture).await, rows);
        assert_eq!(retained_owners(&fixture).await, owners);
        assert_old_sid_works(&fixture).await;
    }
    let rows = permanent_rows(&fixture).await;
    let owners = retained_owners(&fixture).await;
    let stamp = policy(core).await;
    core.execute_unprepared(
        "DELETE FROM seaql_migrations WHERE version IN (
            'm20261008_000400_add_mst2_reader_retention',
            'm20261008_000500_fix_mst2_native_runtime')",
    )
    .await
    .unwrap();
    let reader = core.begin().await.unwrap();
    let registrations = reader
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT (SELECT count(*) FROM mst2_metadata_namespace) AS namespaces,
                    (SELECT count(*) FROM mst2_qualified_family_policy) AS policies",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(registrations.try_get::<i64>("", "namespaces").unwrap(), 2);
    assert_eq!(registrations.try_get::<i64>("", "policies").unwrap(), 1);
    apply_migrations(core, false).await.unwrap();
    reader.rollback().await.unwrap();
    assert_upgrade_ledgers(core).await;
    assert_eq!(descriptor_matches_stored(&fixture).await, 1);
    assert_eq!(policy(core).await, stamp);
    assert_eq!(permanent_rows(&fixture).await, rows);
    assert_eq!(retained_owners(&fixture).await, owners);
    assert_old_sid_works(&fixture).await;
}

#[tokio::test]
async fn captured_87b_qless_descriptor_upgrade_replays_missing_ledgers_before_provisioning() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let core = crate::jupiter::storage::init::database_connection(&config)
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
    restore_native_runtime(&core, &schema, false).await;
    remove_upgrade_ledgers(&core).await;
    apply_migrations(&core, false).await.unwrap();
    assert_upgrade_ledgers(&core).await;
    let stamp = policy(&core).await;
    apply_migrations(&core, false).await.unwrap();
    test_upgrade_native_runtime(&core).await.unwrap();
    assert_eq!(policy(&core).await, stamp);
    let namespace = provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
    let current_schema: String = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT metadata_schema FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_ne!(current_schema, schema);
    assert_eq!(
        provision_or_verify_rooted_qualified_family(&core)
            .await
            .unwrap(),
        namespace
    );
    let high_water: i64 = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT high_water FROM \"{}\".mst2_metadata_reader_issuance",
                current_schema.replace('"', "\"\"")
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(high_water, 0);
}

#[tokio::test]
async fn captured_87b_tampered_descriptor_rejects_without_resigning_history() {
    let fixture = Fixture::new_with_pg_config(true).await;
    seed_live_and_terminal(&fixture).await;
    let schema = q_schema(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let core = mono.get_connection();
    restore_native_runtime(core, &schema, true).await;
    let rows = permanent_rows(&fixture).await;
    let owners = retained_owners(&fixture).await;
    let before = policy(core).await;
    core.execute_unprepared(&format!(
        "CREATE OR REPLACE FUNCTION \"{}\".mst2_metadata_descriptor(
            pid text,instance text,commit_id text,tree_id text) RETURNS bytea
            LANGUAGE sql AS 'SELECT decode(''0000'',''hex'')'",
        schema.replace('"', "\"\"")
    ))
    .await
    .unwrap();
    assert!(test_upgrade_native_runtime(core).await.is_err());
    assert_eq!(policy(core).await, before);
    assert_eq!(permanent_rows(&fixture).await, rows);
    assert_eq!(retained_owners(&fixture).await, owners);
}
