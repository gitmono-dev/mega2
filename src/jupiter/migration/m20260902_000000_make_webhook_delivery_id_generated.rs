use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DatabaseBackend::Postgres {
            let connection = manager.get_connection();
            connection
                .execute_unprepared("CREATE SEQUENCE IF NOT EXISTS mega_webhook_delivery_id_seq")
                .await?;
            connection
                .execute_unprepared(
                    "SELECT setval('mega_webhook_delivery_id_seq', \
                                   COALESCE((SELECT MAX(id) FROM mega_webhook_delivery), 0) + 1, \
                                   false)",
                )
                .await?;
            connection
                .execute_unprepared(
                    "ALTER TABLE mega_webhook_delivery ALTER COLUMN id \
                     SET DEFAULT nextval('mega_webhook_delivery_id_seq')",
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DatabaseBackend::Postgres {
            let connection = manager.get_connection();
            connection
                .execute_unprepared(
                    "ALTER TABLE mega_webhook_delivery ALTER COLUMN id DROP DEFAULT",
                )
                .await?;
            connection
                .execute_unprepared("DROP SEQUENCE IF EXISTS mega_webhook_delivery_id_seq")
                .await?;
        }
        Ok(())
    }
}
