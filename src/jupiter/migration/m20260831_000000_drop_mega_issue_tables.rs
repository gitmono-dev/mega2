//! CE-23: Retire upstream Issue product tables (ADR-CE-10).
//!
//! Forward-only migration. Removes `mega_issue` rows and dependent associations in
//! shared tables (`item_labels`, `item_assignees`, `mega_conversation`,
//! `issue_cl_references`), then drops `mega_issue`, legacy `git_issue`, and unused
//! `git_pr`. CL tables and shared label/assignee/conversation tables are preserved.

use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_backend = manager.get_database_backend();
        let conn = manager.get_connection();

        for sql in [
            "DELETE FROM user_notification_preferences \
             WHERE event_type_code IN ('issue.comment.created', 'issue.closed');",
            "DELETE FROM notification_event_types \
             WHERE code IN ('issue.comment.created', 'issue.closed');",
            "DELETE FROM issue_cl_references \
             WHERE source_id IN (SELECT link FROM mega_issue) \
                OR target_id IN (SELECT link FROM mega_issue);",
            "DELETE FROM item_labels WHERE item_id IN (SELECT id FROM mega_issue);",
            "DELETE FROM item_assignees WHERE item_id IN (SELECT id FROM mega_issue);",
            "DELETE FROM mega_conversation WHERE link IN (SELECT link FROM mega_issue);",
        ] {
            conn.execute_raw(Statement::from_string(db_backend, sql))
                .await?;
        }

        for table in ["mega_issue", "git_issue", "git_pr"] {
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
        // Forward-only: dropped Issue/git mirror data cannot be reconstructed safely.
        Ok(())
    }
}
