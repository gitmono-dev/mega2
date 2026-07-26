use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Migration(format!(
                "reaction null-safe unique index requires PostgreSQL, got {:?}",
                manager.get_database_backend()
            )));
        }

        let conn = manager.get_connection();
        conn.execute_unprepared(r#"DROP INDEX IF EXISTS "idx-reactions-unique-active";"#)
            .await?;
        conn.execute_unprepared(
            r#"CREATE UNIQUE INDEX "idx-reactions-unique-active"
               ON reactions (subject_type, subject_id, username, content, custom_reaction_id)
               NULLS NOT DISTINCT
               WHERE discarded_at IS NULL;"#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(r#"DROP INDEX IF EXISTS "idx-reactions-unique-active";"#)
            .await?;
        conn.execute_unprepared(
            r#"CREATE UNIQUE INDEX IF NOT EXISTS "idx-reactions-unique-active"
               ON reactions (subject_type, subject_id, username, content, custom_reaction_id)
               WHERE discarded_at IS NULL;"#,
        )
        .await?;

        Ok(())
    }
}
