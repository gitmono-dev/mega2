use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

pub(super) async fn restore_pre_capability_schema(db: &DatabaseConnection) {
    let bound:i64=db.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT (SELECT count(*) FROM mst2_metadata_prepare WHERE storage_seal IS NOT NULL
           OR canonical_bindings IS NOT NULL OR bindings_digest IS NOT NULL OR primary_scope IS NOT NULL
           OR graph_domain IS NOT NULL)+(SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation IS NOT NULL)
           +(SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NOT NULL)"))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap();
    assert_eq!(
        bound, 0,
        "old-schema fixture only contains unbound legacy installations"
    );
    // Reproduce the deployment before this additive migration. Preserve every
    // original plan, member, byte, context, lease and protection root.
    db.execute_unprepared(
        "DROP TRIGGER mst2_00_install_capability_barrier ON mst2_metadata_prepare;
         DROP TRIGGER mst2_00_install_capability_barrier ON mst2_metadata_prepare_page;
         DROP TRIGGER mst2_install_capability_truncate_guard ON mst2_metadata_prepare;
         DROP TRIGGER mst2_install_capability_truncate_guard ON mst2_metadata_prepare_page;
         DROP TRIGGER mst2_install_capability_prepare_guard ON mst2_metadata_prepare;
         DROP TRIGGER mst2_install_capability_mapping_guard ON mst2_metadata_prepare_page;
         DROP TABLE mst2_metadata_install_seal;
         DROP FUNCTION mst2_install_capability_barrier();
         DROP FUNCTION mst2_install_capability_truncate_guard();
         DROP FUNCTION mst2_install_capability_prepare_guard();
         DROP FUNCTION mst2_install_capability_mapping_guard();
         DROP FUNCTION mst2_install_capability_register();
         DELETE FROM seaql_migrations WHERE version='m20261007_000500_add_mst2_install_capability'",
    )
    .await
    .unwrap();
}
