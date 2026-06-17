use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(r#"ALTER TYPE merge_status_enum ADD VALUE IF NOT EXISTS 'draft';"#)
            .await?;

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        // Note: PostgreSQL does not support removing enum values directly
        // This migration cannot be fully reversed without recreating the enum
        Ok(())
    }
}
