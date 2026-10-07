use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

pub(super) async fn restore_empty_g1_schema(db: &DatabaseConnection) {
    let count = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM mst2_metadata_current",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(
        count, 0,
        "upgrade fixture has no generation-bound incarnation to erase"
    );
    db.execute_unprepared(
        "DROP TABLE mst2_metadata_current;
         DROP FUNCTION mst2_metadata_current_guard();
         DROP INDEX idx_mst2_metadata_lifetime_node_generation;
         ALTER TABLE mst2_metadata_lifetime DROP CONSTRAINT mst2_metadata_lifetime_pkey,
           ADD PRIMARY KEY(page_id),ADD UNIQUE(node_id);
         ALTER TABLE mst2_metadata_prepare DROP CONSTRAINT mst2_metadata_prepare_state_check,
           DROP CONSTRAINT mst2_metadata_prepare_terminal_check,
           ADD CONSTRAINT mst2_metadata_prepare_state_check CHECK (state IN ('PREPARING','COMMITTED')),
           ADD CONSTRAINT mst2_metadata_prepare_check CHECK ((state='COMMITTED')=(committed_at IS NOT NULL)),
           DROP COLUMN graph_domain,DROP COLUMN aborted_at,DROP COLUMN coverage_retired_at;
         DELETE FROM seaql_migrations WHERE version='m20261007_000300_add_mst2_metadata_lifetime_history'"
    ).await.unwrap();
}
