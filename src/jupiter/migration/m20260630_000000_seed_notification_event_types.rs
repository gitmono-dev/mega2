use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Seeds the seven notification event types at migration time so that they
/// exist immediately after `apply_migrations`, eliminating the write-on-read
/// upsert race that `notification::triggers::ensure_event_type_exists` would
/// otherwise trigger on first use. The runtime upsert remains as an idempotent
/// no-op fallback.
///
/// Event type codes and attributes must match the constants in
/// `src/notification/triggers.rs`.
const SEED_ROWS: &[(&str, &str, &str, bool, bool)] = &[
    (
        "cl.comment.created",
        "cl",
        "New comment on a Change List",
        false,
        true,
    ),
    ("cl.merged", "cl", "Change List was merged", false, true),
    (
        "issue.comment.created",
        "issue",
        "New comment on an Issue",
        false,
        true,
    ),
    ("issue.closed", "issue", "Issue was closed", false, true),
    (
        "item.referenced",
        "reference",
        "Your CL or Issue was referenced (mentioned)",
        false,
        true,
    ),
    (
        "chat.mention.created",
        "chat",
        "You were mentioned in a chat message",
        false,
        true,
    ),
    (
        "chat.reply.created",
        "chat",
        "Your message received a reply",
        false,
        true,
    ),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_backend = manager.get_database_backend();
        let conn = manager.get_connection();

        for (code, category, description, system_required, default_enabled) in SEED_ROWS {
            let sql = format!(
                r#"
                    INSERT INTO notification_event_types
                        (code, category, description, system_required, default_enabled, created_at, updated_at)
                    SELECT '{code}', '{category}', '{description}', {system_required}, {default_enabled},
                           CURRENT_TIMESTAMP, CURRENT_TIMESTAMP
                    WHERE NOT EXISTS (
                        SELECT 1 FROM notification_event_types WHERE code = '{code}'
                    );
                "#,
            );
            conn.execute_raw(Statement::from_string(db_backend, sql))
                .await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_backend = manager.get_database_backend();
        let codes: Vec<&str> = SEED_ROWS.iter().map(|(code, _, _, _, _)| *code).collect();
        let sql = format!(
            "DELETE FROM notification_event_types WHERE code IN ({});",
            codes
                .iter()
                .map(|c| format!("'{c}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        manager
            .get_connection()
            .execute_raw(Statement::from_string(db_backend, sql))
            .await?;

        Ok(())
    }
}
