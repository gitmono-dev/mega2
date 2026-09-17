//! T06: MST/2 retention graph — nodes, edges, root coverage (spec 10 §5/§6).
//!
//! Append-only, de-duplicated reference graph used by the GC coordinator.
//! Edges are unique on `(parent_id, child_id)`; root coverage is a
//! separate many-to-many table keyed by node id, so a node is retained
//! while any active lease/pin/prepare covers it or it has a live incoming
//! edge. The collector marks nodes DELETING and only the opt-in reaper
//! removes rows; Git raw blobs are never deleted by this migration or by
//! the default (fail-closed) path.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Mst2RetentionNode::Table)
                    .if_not_exists()
                    .col(string(Mst2RetentionNode::NodeId).not_null().primary_key())
                    .col(string(Mst2RetentionNode::Kind).not_null())
                    .col(string(Mst2RetentionNode::State).not_null().default("LIVE"))
                    .col(big_integer(Mst2RetentionNode::Bytes).not_null().default(0))
                    .col(timestamp_with_time_zone(Mst2RetentionNode::CreatedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Mst2RetentionEdge::Table)
                    .if_not_exists()
                    .col(
                        big_integer(Mst2RetentionEdge::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Mst2RetentionEdge::ParentId).not_null())
                    .col(string(Mst2RetentionEdge::ChildId).not_null())
                    .col(timestamp_with_time_zone(Mst2RetentionEdge::CreatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_edge_unique")
                    .table(Mst2RetentionEdge::Table)
                    .col(Mst2RetentionEdge::ParentId)
                    .col(Mst2RetentionEdge::ChildId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_edge_child")
                    .table(Mst2RetentionEdge::Table)
                    .col(Mst2RetentionEdge::ChildId)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Mst2RetentionRoot::Table)
                    .if_not_exists()
                    .col(
                        big_integer(Mst2RetentionRoot::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Mst2RetentionRoot::NodeId).not_null())
                    // lease:<id> | pin:<id> | prepare:<id>
                    .col(string(Mst2RetentionRoot::RootKey).not_null())
                    .col(string(Mst2RetentionRoot::RootKind).not_null())
                    .col(timestamp_with_time_zone(Mst2RetentionRoot::CreatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_root_unique")
                    .table(Mst2RetentionRoot::Table)
                    .col(Mst2RetentionRoot::NodeId)
                    .col(Mst2RetentionRoot::RootKey)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_retention_root_key")
                    .table(Mst2RetentionRoot::Table)
                    .col(Mst2RetentionRoot::RootKey)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: retention state is never auto-dropped.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Mst2RetentionNode {
    Table,
    NodeId,
    Kind,
    State,
    Bytes,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Mst2RetentionEdge {
    Table,
    Id,
    ParentId,
    ChildId,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Mst2RetentionRoot {
    Table,
    Id,
    NodeId,
    RootKey,
    RootKind,
    CreatedAt,
}
