//! TP-07: `mega_cl.revision` — monotonic CAS for merge/rebase (GAP-12).
//!
//! Merge and rebase both write the CL row unconditionally today, so a stale
//! rebase can turn `Merged` back to `Open`. The column is a full-row version:
//! each merge/rebase update matches `WHERE revision = $expected` and
//! increments. Zero rows is `ClaimLost`.
//!
//! Forward-only (`down` is empty): the repository exposes no `migrate down`
//! entry point. Existing rows backfill to `0`.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_cl ADD COLUMN IF NOT EXISTS revision bigint NOT NULL DEFAULT 0",
            )
            .await
            .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
