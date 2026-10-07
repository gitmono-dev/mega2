//! T06-B: durable retention counters and replayable GC operations.
//!
//! The original T06 graph migration intentionally supplied only the graph
//! rows. This forward-only migration adds the counter used by the atomic
//! LIVE→DELETING CAS and an idempotent operation log for crash recovery. The
//! runtime is not switched to this schema by this migration alone.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Mst2RetentionNode::Table)
                    .add_column(
                        big_integer(Mst2RetentionNode::IncomingRefs)
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_node_gc")
                    .table(Mst2RetentionNode::Table)
                    .col(Mst2RetentionNode::State)
                    .col(Mst2RetentionNode::IncomingRefs)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Mst2RetentionGcOp::Table)
                    .if_not_exists()
                    .col(
                        string(Mst2RetentionGcOp::OperationId)
                            .not_null()
                            .primary_key(),
                    )
                    .col(string(Mst2RetentionGcOp::NodeId).not_null())
                    .col(string(Mst2RetentionGcOp::Operation).not_null())
                    .col(
                        string(Mst2RetentionGcOp::State)
                            .not_null()
                            .default("PENDING"),
                    )
                    .col(integer(Mst2RetentionGcOp::Attempts).not_null().default(0))
                    .col(
                        timestamp_with_time_zone(Mst2RetentionGcOp::CreatedAt)
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(timestamp_with_time_zone_null(
                        Mst2RetentionGcOp::CompletedAt,
                    ))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_gc_op_pending")
                    .table(Mst2RetentionGcOp::Table)
                    .col(Mst2RetentionGcOp::State)
                    .col(Mst2RetentionGcOp::CreatedAt)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_gc_op_node")
                    .table(Mst2RetentionGcOp::Table)
                    .col(Mst2RetentionGcOp::NodeId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: retention state and replay evidence are never
        // dropped automatically.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Mst2RetentionNode {
    Table,
    State,
    IncomingRefs,
}

#[derive(DeriveIden)]
enum Mst2RetentionGcOp {
    Table,
    OperationId,
    NodeId,
    Operation,
    State,
    Attempts,
    CreatedAt,
    CompletedAt,
}
