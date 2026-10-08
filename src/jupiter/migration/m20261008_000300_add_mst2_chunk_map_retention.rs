use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Custom(
                "chunk-map retention requires primary PostgreSQL".into(),
            ));
        }
        manager
            .get_connection()
            .execute_unprepared(include_str!("m20261008_000300_chunk_map_retention.sql"))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Never discard generation fences or late-create reconciliation history.
        Ok(())
    }
}
