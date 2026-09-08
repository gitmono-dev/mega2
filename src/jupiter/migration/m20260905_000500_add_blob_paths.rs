//! TP-13: `blob_paths` appearance table (ADR-TP-11).
//!
//! Forward-only (`down` is empty). `(blob_id, path)` is unique; `indexed_push_id`
//! is nullable so review-mode rows and watermark resets (TP-15) stay representable.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS blob_paths (
                blob_id text NOT NULL,
                path text NOT NULL,
                indexed_push_id bigint,
                PRIMARY KEY (blob_id, path)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS blob_paths_path_idx ON blob_paths (path)",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
