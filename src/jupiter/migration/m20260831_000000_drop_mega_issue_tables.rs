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
            "DELETE FROM reactions \
             WHERE subject_id IN (SELECT id FROM mega_conversation \
                                  WHERE link IN (SELECT link FROM mega_issue));",
            "DELETE FROM item_labels WHERE item_type = 'issue';",
            "DELETE FROM item_assignees WHERE item_type = 'issue';",
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

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::tests::test_db_connection;

    /// Shared item_id between Issue and CL rows must not delete CL labels/assignees.
    #[tokio::test]
    async fn drop_mega_issue_preserves_cl_shared_table_rows_on_id_collision() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        let shared_item_id = 9_900_001_i64;
        let now = "2026-08-31T00:00:00";
        for sql in [
            "CREATE TABLE IF NOT EXISTS mega_issue (\
                id BIGINT PRIMARY KEY, link TEXT NOT NULL, title TEXT NOT NULL, \
                status TEXT NOT NULL, created_at TIMESTAMP NOT NULL, \
                updated_at TIMESTAMP NOT NULL, closed_at TIMESTAMP NULL, author TEXT NOT NULL);",
            "CREATE TABLE IF NOT EXISTS item_labels (\
                created_at TIMESTAMP NOT NULL, updated_at TIMESTAMP NOT NULL, \
                item_id BIGINT NOT NULL, label_id BIGINT NOT NULL, item_type TEXT NOT NULL, \
                PRIMARY KEY (item_id, label_id));",
            "CREATE TABLE IF NOT EXISTS item_assignees (\
                created_at TIMESTAMP NOT NULL, updated_at TIMESTAMP NOT NULL, \
                item_id BIGINT NOT NULL, assignnee_id TEXT NOT NULL, item_type TEXT NOT NULL, \
                PRIMARY KEY (item_id, assignnee_id));",
            &format!(
                "INSERT INTO mega_issue (id, link, title, status, created_at, updated_at, closed_at, author) \
                 VALUES ({shared_item_id}, 'CE23ISSU', 'issue row', 'open', '{now}', '{now}', NULL, 'alice');"
            ),
            &format!(
                "INSERT INTO item_labels (created_at, updated_at, item_id, label_id, item_type) \
                 VALUES ('{now}', '{now}', {shared_item_id}, 9900001, 'issue');"
            ),
            &format!(
                "INSERT INTO item_labels (created_at, updated_at, item_id, label_id, item_type) \
                 VALUES ('{now}', '{now}', {shared_item_id}, 9900002, 'cl');"
            ),
            &format!(
                "INSERT INTO item_assignees (created_at, updated_at, item_id, assignnee_id, item_type) \
                 VALUES ('{now}', '{now}', {shared_item_id}, 'alice', 'issue');"
            ),
            &format!(
                "INSERT INTO item_assignees (created_at, updated_at, item_id, assignnee_id, item_type) \
                 VALUES ('{now}', '{now}', {shared_item_id}, 'bob', 'cl');"
            ),
        ] {
            db.execute_raw(Statement::from_string(DbBackend::Postgres, sql.to_owned()))
                .await
                .expect("seed CE-23 collision fixture");
        }

        for sql in [
            "DELETE FROM item_labels WHERE item_type = 'issue';",
            "DELETE FROM item_assignees WHERE item_type = 'issue';",
        ] {
            db.execute_raw(Statement::from_string(DbBackend::Postgres, sql.to_owned()))
                .await
                .expect("CE-23 shared-table cleanup");
        }

        let issue_labels: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*)::bigint AS count FROM item_labels WHERE item_type = 'issue';"
                    .to_owned(),
            ))
            .await
            .expect("count issue labels")
            .expect("issue label count row")
            .try_get("", "count")
            .expect("count column");
        assert_eq!(issue_labels, 0, "issue item_labels must be removed");

        let cl_labels: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT count(*)::bigint AS count FROM item_labels \
                     WHERE item_type = 'cl' AND item_id = {shared_item_id};"
                ),
            ))
            .await
            .expect("count cl labels")
            .expect("cl label count row")
            .try_get("", "count")
            .expect("count column");
        assert_eq!(cl_labels, 1, "CL item_labels must survive id collision");

        let cl_assignees: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT count(*)::bigint AS count FROM item_assignees \
                     WHERE item_type = 'cl' AND item_id = {shared_item_id};"
                ),
            ))
            .await
            .expect("count cl assignees")
            .expect("cl assignee count row")
            .try_get("", "count")
            .expect("count column");
        assert_eq!(
            cl_assignees, 1,
            "CL item_assignees must survive id collision"
        );
    }
}
