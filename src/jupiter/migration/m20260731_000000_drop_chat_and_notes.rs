use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_backend = manager.get_database_backend();
        let conn = manager.get_connection();

        conn.execute_raw(Statement::from_string(
            db_backend,
            "DELETE FROM user_notification_preferences \
             WHERE event_type_code IN ('chat.mention.created', 'chat.reply.created');",
        ))
        .await?;
        conn.execute_raw(Statement::from_string(
            db_backend,
            "DELETE FROM notification_event_types \
             WHERE code IN ('chat.mention.created', 'chat.reply.created');",
        ))
        .await?;

        for table in [
            "message_notifications",
            "messages",
            "channel_membership_updates",
            "channel_memberships",
            "channels",
            "attachments",
            "open_graph_links",
            "non_member_note_views",
            "note_views",
            "notes",
        ] {
            manager
                .drop_table(
                    Table::drop()
                        .table(Alias::new(table))
                        .if_exists()
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: dropped product data cannot be reconstructed safely.
        Ok(())
    }
}
