//! TP-09: `mega_ref_tombstones` (trunk-push.md 2.5 / 1.10 deliverable 8).
//!
//! Forward-only (`down` is empty). Default upgrade path is fail-closed: the
//! table is created empty. Best-effort backfill is an explicit operator API.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_ref_tombstones (
                path text NOT NULL,
                ref_name text NOT NULL,
                last_commit_hash text NOT NULL,
                last_tree_hash text NOT NULL,
                deleted_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (path, ref_name)
            )",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
