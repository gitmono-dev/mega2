//! Append-only, independently authenticated chunk-map read indexes.

use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Custom(
                "persisted chunk maps require primary PostgreSQL".into(),
            ));
        }
        manager
            .get_connection()
            .execute_unprepared(include_str!("m20261008_000100_chunk_maps.sql"))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // No collector or migration rollback may silently discard receipts.
        Ok(())
    }
}
