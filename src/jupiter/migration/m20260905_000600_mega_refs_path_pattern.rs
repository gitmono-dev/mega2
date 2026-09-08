//! TP-14: `mega_refs_path_pattern` partial index (trunk-push.md 2.6).
//!
//! `LIKE 'prefix%'` does not use `uniq_mref_path` on non-C Postgres collations.
//! Forward-only (`down` is empty). Postgres uses `text_pattern_ops`; other
//! backends skip the operator-class index.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Postgres {
            return Ok(());
        }
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS mega_refs_path_pattern
                 ON mega_refs (path text_pattern_ops)
                 WHERE ref_name = 'refs/heads/main' AND is_cl = false",
            )
            .await
            .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
