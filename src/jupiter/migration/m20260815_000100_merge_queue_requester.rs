//! UN-18: nullable `requester` column on `merge_queue`.
//!
//! A queued merge is executed later by a background worker, so the subject that
//! requested it has to be persisted with the queue entry — otherwise the merge
//! runs with no authorization principal of its own. This migration only adds
//! the column; writing and reading it is UN-20, and the execution decision
//! (including what a NULL means for legacy rows) is UN-17.
//!
//! Purely additive: nullable, no default, no backfill, no existing row touched.
//!
//! Recovery is forward-only, and the safe repair depends on whether the column
//! has a consumer yet:
//!
//! * **pre-consumer** (UN-20 unpublished, the column is always NULL): a new
//!   migration may drop it.
//! * **post-consumer** (requesters are being written): dropping the column
//!   unconditionally destroys the authorization subject and its audit trail.
//!   Keep the column and disable the consumer, or move to a replacement column
//!   with a data copy — never an unconditional drop.

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub const COLUMN_NAME: &str = "requester";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum MergeQueue {
    Table,
    Requester,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Migration(format!(
                "merge_queue.requester column requires PostgreSQL, got {:?}",
                manager.get_database_backend()
            )));
        }

        manager
            .alter_table(
                Table::alter()
                    .table(MergeQueue::Table)
                    .add_column(ColumnDef::new(MergeQueue::Requester).string().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(MergeQueue::Table)
                    .drop_column(MergeQueue::Requester)
                    .to_owned(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
    use sea_orm_migration::{SchemaManager, prelude::MigrationTrait};

    use super::*;
    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    /// `(is_nullable, column_default)` of `merge_queue.requester`, or `None`
    /// when the column does not exist.
    async fn requester_column(db: &DatabaseConnection) -> Option<(String, Option<String>)> {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    // Test databases are schema-isolated through `search_path`
                    // (see `jupiter::tests`), and the catalog spans every
                    // schema — so this must be scoped, or a concurrent test's
                    // table answers instead.
                    "SELECT is_nullable, column_default FROM information_schema.columns \
                     WHERE table_schema = current_schema() AND table_name = 'merge_queue' \
                     AND column_name = '{COLUMN_NAME}'"
                ),
            ))
            .await
            .expect("catalog query")?;
        Some((
            row.try_get("", "is_nullable").expect("is_nullable column"),
            row.try_get("", "column_default").expect("column_default"),
        ))
    }

    async fn insert_queue_row(db: &DatabaseConnection, id: i64, cl_link: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO merge_queue \
             (id, cl_link, status, position, retry_count, created_at, updated_at) \
             VALUES ({id}, '{cl_link}', 'waiting', 1, 0, now(), now())"
        ))
        .await
        .expect("insert merge_queue row");
    }

    #[tokio::test]
    async fn un18_up_adds_a_nullable_requester_column_without_a_default() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, false)
            .await
            .expect("migrations should apply");

        let (is_nullable, default) = requester_column(&db)
            .await
            .expect("the column exists after the migration");
        assert_eq!(is_nullable, "YES", "the column must be nullable");
        assert!(
            default.is_none(),
            "the column must have no default (no implicit backfill): {default:?}"
        );
    }

    #[tokio::test]
    async fn un18_existing_rows_keep_a_null_requester_and_down_drops_the_column() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, false)
            .await
            .expect("migrations should apply");

        // A row written before any consumer exists reads back as NULL — the
        // legacy semantics UN-17 builds on.
        insert_queue_row(&db, 960_001, "UN18QUEUE").await;
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT {COLUMN_NAME} FROM merge_queue WHERE id = 960001"),
            ))
            .await
            .expect("select query")
            .expect("the inserted row");
        let requester: Option<String> = row.try_get("", COLUMN_NAME).expect("requester column");
        assert!(
            requester.is_none(),
            "an existing row must keep a NULL requester: {requester:?}"
        );

        // `down` is isolated-test evidence only: the runtime runner exposes no
        // rollback entry point (forward-only), and dropping this column once it
        // has a consumer would destroy the authorization subject.
        let manager = SchemaManager::new(&db);
        Migration
            .down(&manager)
            .await
            .expect("down drops the column");
        assert!(
            requester_column(&db).await.is_none(),
            "down removes the column in an isolated database"
        );

        // Re-applying is the forward path back to the published state.
        Migration.up(&manager).await.expect("up re-adds the column");
        assert!(
            requester_column(&db).await.is_some(),
            "up re-adds the column"
        );
    }
}
