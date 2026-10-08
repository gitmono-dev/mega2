//! Exact-incarnation metadata graph and atomic, replayable payload collection.

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = include_str!("m20261007_000400_qualified_metadata_gc.sql")
            .replace("$HEADER_LEN$", &HEADER_LEN.to_string())
            .replace("$PAGE_MAX_BYTES$", &PAGE_MAX_BYTES.to_string());
        manager.get_connection().execute_unprepared(&sql).await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
