//! Corrective migration for the publication receipt uniqueness (P3.1).
//!
//! `m20260917_000200_add_mst2_publication` was applied by early builds with
//! a unique index on `operation_id` alone; the namespace-scoped uniqueness
//! (`namespace, operation_id`) was then written into that same migration
//! body, so databases that had already applied it never gained the new
//! index — and the receipt insert's `ON CONFLICT (namespace, operation_id)`
//! fails with "no unique or exclusion constraint matching". This migration
//! brings existing databases to the intended shape; on fresh ones the
//! drop is a no-op.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name("idx_mst2_publication_operation_unique")
                    .table(Mst2Publication::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_publication_ns_operation_unique")
                    .table(Mst2Publication::Table)
                    .col(Mst2Publication::Namespace)
                    .col(Mst2Publication::OperationId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // No down: the (namespace, operation_id) index is the intended
        // invariant; restoring the namespace-blind one would reintroduce P3.1.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Mst2Publication {
    Table,
    Namespace,
    OperationId,
}
