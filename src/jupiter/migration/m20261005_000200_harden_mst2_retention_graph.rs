//! T06-B: backfill graph counters and reject invalid durable graph state.
//!
//! Applies after the additive schema migration. Existing edges must form a
//! valid DAG and counters are derived from unique edges before any collector
//! can use them. Constraints keep failed writers from silently dangling a
//! node or underflowing a counter. Completed GC receipts intentionally do not
//! reference the removed node through a foreign key.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "LOCK TABLE mst2_retention_node, mst2_retention_edge, mst2_retention_root, \
             mst2_retention_gc_op IN ACCESS EXCLUSIVE MODE",
        )
        .await?;
        let cycle = conn
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "WITH RECURSIVE walk(start_id, node_id) AS ( \
                   SELECT parent_id, child_id FROM mst2_retention_edge \
                   UNION SELECT w.start_id, e.child_id FROM walk w \
                     JOIN mst2_retention_edge e ON e.parent_id = w.node_id \
                 ) SELECT start_id FROM walk WHERE start_id = node_id LIMIT 1",
            ))
            .await?;
        if cycle.is_some() {
            return Err(DbErr::Migration(
                "MST/2 retention graph contains a cycle".into(),
            ));
        }
        conn.execute_unprepared(
            "UPDATE mst2_retention_node n SET incoming_refs = \
               (SELECT count(*) FROM mst2_retention_edge e WHERE e.child_id = n.node_id)",
        )
        .await?;
        conn.execute_unprepared(
            "ALTER TABLE mst2_retention_node \
               ADD CONSTRAINT mst2_retention_node_state_check CHECK (state IN ('LIVE', 'DELETING')), \
               ADD CONSTRAINT mst2_retention_node_kind_check \
                 CHECK (kind IN ('page', 'chunk_map', 'frame', 'verified_object')), \
               ADD CONSTRAINT mst2_retention_node_nonnegative CHECK (bytes >= 0 AND incoming_refs >= 0); \
             ALTER TABLE mst2_retention_edge \
               ADD CONSTRAINT mst2_retention_edge_parent_fk FOREIGN KEY (parent_id) \
                 REFERENCES mst2_retention_node(node_id), \
               ADD CONSTRAINT mst2_retention_edge_child_fk FOREIGN KEY (child_id) \
                 REFERENCES mst2_retention_node(node_id), \
               ADD CONSTRAINT mst2_retention_edge_no_self CHECK (parent_id <> child_id); \
             ALTER TABLE mst2_retention_root \
               ADD CONSTRAINT mst2_retention_root_node_fk FOREIGN KEY (node_id) \
                 REFERENCES mst2_retention_node(node_id), \
               ADD CONSTRAINT mst2_retention_root_kind_check CHECK (root_kind IN ('lease', 'pin', 'prepare')); \
             ALTER TABLE mst2_retention_gc_op \
               ADD CONSTRAINT mst2_retention_gc_op_kind_check CHECK (operation IN ('MARK_DELETING', 'REMOVE')), \
               ADD CONSTRAINT mst2_retention_gc_op_state_check CHECK (state IN ('PENDING', 'APPLIED', 'FAILED')), \
               ADD CONSTRAINT mst2_retention_gc_op_attempts_check CHECK (attempts >= 0), \
               ADD CONSTRAINT mst2_retention_gc_op_completion_check \
                 CHECK ((state = 'APPLIED') = (completed_at IS NOT NULL)); \
             CREATE UNIQUE INDEX idx_mst2_retention_gc_op_pending_node \
               ON mst2_retention_gc_op(node_id) WHERE state = 'PENDING'",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: preserve retained nodes and GC recovery evidence.
        Ok(())
    }
}
