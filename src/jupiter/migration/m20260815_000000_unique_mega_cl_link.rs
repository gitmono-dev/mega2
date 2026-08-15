//! UN-10: unique index on `mega_cl.link`.
//!
//! The authorization guard resolves a CL `link` to its `path`, and the storage
//! layer queries `mega_cl` by `link` alone. The only indexes covering `link`
//! today are composite (`path, link`), so the lookup is neither an index
//! equality probe nor guaranteed to return a single row.
//!
//! `up` takes a write-exclusion lock, scans for duplicate links, and aborts
//! with the full conflict list rather than touching business rows; only then
//! does it create the unique index. Holding `SHARE ROW EXCLUSIVE` from before
//! the scan until the index exists closes the "scanned clean, then a duplicate
//! was inserted" race — without it the conflict list could be incomplete and
//! the index creation could fail on data the operator was never shown.
//!
//! Recovery is forward-only: the runtime runner exposes only `up`/`refresh`, so
//! a published index is repaired by a new migration. `down` exists for isolated
//! tests and the development `refresh` path and is not a production rollback.

use sea_orm::{DbBackend, Statement};
use sea_orm_migration::prelude::*;

pub const INDEX_NAME: &str = "idx_mega_cl_link_unique";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Migration(format!(
                "unique mega_cl.link index requires PostgreSQL, got {:?}",
                manager.get_database_backend()
            )));
        }

        let conn = manager.get_connection();

        // Block concurrent writers for the rest of this migration's
        // transaction: the scan below and the index creation must see the same
        // rows. `SHARE ROW EXCLUSIVE` blocks INSERT/UPDATE/DELETE while still
        // allowing reads.
        conn.execute_unprepared("LOCK TABLE mega_cl IN SHARE ROW EXCLUSIVE MODE")
            .await?;

        let conflicts = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT link, count(*) AS occurrences FROM mega_cl \
                 GROUP BY link HAVING count(*) > 1 ORDER BY link",
            ))
            .await?;

        if !conflicts.is_empty() {
            let mut listed: Vec<String> = Vec::with_capacity(conflicts.len());
            for row in &conflicts {
                let link: String = row.try_get("", "link")?;
                let occurrences: i64 = row.try_get("", "occurrences")?;
                listed.push(format!("{link} ({occurrences} rows)"));
            }
            // Abort without touching any business row: resolving duplicates is
            // a data-governance decision, not something a migration may make.
            return Err(DbErr::Migration(format!(
                "cannot create a unique index on mega_cl.link: {} duplicated link(s) found; \
                 resolve them first. Conflicting links: {}",
                listed.len(),
                listed.join(", ")
            )));
        }

        conn.execute_unprepared(&format!(
            r#"CREATE UNIQUE INDEX "{INDEX_NAME}" ON mega_cl (link)"#
        ))
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!(r#"DROP INDEX IF EXISTS "{INDEX_NAME}""#))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
    use sea_orm_migration::{SchemaManager, prelude::MigrationTrait};

    use super::*;
    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    /// Migrated database with the UN-10 index dropped again, i.e. the state a
    /// deployment is in right before this migration runs.
    async fn migrated_db_without_index() -> (tempfile::TempDir, DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, false)
            .await
            .expect("migrations should apply");
        db.execute_unprepared(&format!(r#"DROP INDEX IF EXISTS "{INDEX_NAME}""#))
            .await
            .expect("drop index for a clean starting state");
        (temp_dir, db)
    }

    async fn index_exists(db: &DatabaseConnection) -> bool {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                // Scoped to the current schema: test databases are
                // schema-isolated through `search_path` (see `jupiter::tests`)
                // and the catalog spans every schema.
                format!("SELECT to_regclass(current_schema() || '.{INDEX_NAME}')::text AS name"),
            ))
            .await
            .expect("catalog query")
            .expect("catalog row");
        row.try_get::<Option<String>>("", "name")
            .expect("name column")
            .is_some()
    }

    /// `idx_mr_path_link` is a pre-existing UNIQUE index on `(path, link)`, so
    /// duplicating a `link` requires distinct paths — which is exactly the hole
    /// this migration closes.
    fn insert_cl_sql(id: i64, path: &str, link: &str) -> String {
        format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{link}', 'un10', 'open', '{path}', 'from', 'to', now(), now(), \
             'un10-user', 'main')"
        )
    }

    async fn insert_cl(db: &impl ConnectionTrait, id: i64, path: &str, link: &str) {
        db.execute_unprepared(&insert_cl_sql(id, path, link))
            .await
            .expect("insert mega_cl row");
    }

    async fn cl_row_count(db: &DatabaseConnection) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM mega_cl".to_owned(),
            ))
            .await
            .expect("count query")
            .expect("count row");
        row.try_get("", "n").expect("n column")
    }

    /// Run `up` the way the runtime runner does: inside a transaction, so the
    /// `LOCK TABLE` taken by the migration is held until commit.
    async fn run_up_in_txn(db: &DatabaseConnection) -> Result<(), DbErr> {
        let txn = db.begin().await?;
        let manager = SchemaManager::new(&txn);
        let result = Migration.up(&manager).await;
        match result {
            Ok(()) => txn.commit().await,
            Err(e) => {
                txn.rollback().await?;
                Err(e)
            }
        }
    }

    #[tokio::test]
    async fn un10_up_creates_a_unique_index_on_link_and_down_drops_it() {
        let (_temp, db) = migrated_db_without_index().await;
        assert!(!index_exists(&db).await, "starting state has no index");

        run_up_in_txn(&db).await.expect("up should succeed");
        assert!(index_exists(&db).await, "up creates the index");

        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT indexdef FROM pg_indexes \
                     WHERE schemaname = current_schema() AND tablename = 'mega_cl' \
                     AND indexname = '{INDEX_NAME}'"
                ),
            ))
            .await
            .expect("pg_indexes query")
            .expect("index row");
        let indexdef: String = row.try_get("", "indexdef").expect("indexdef column");
        assert!(
            indexdef.contains("UNIQUE") && indexdef.contains("(link)"),
            "the index must be UNIQUE on the single `link` column: {indexdef}"
        );

        // The unique constraint is real: a second row with the same link fails.
        insert_cl(&db, 910_001, "/a", "UN10LINK1").await;
        let duplicate = db
            .execute_unprepared(&insert_cl_sql(910_002, "/b", "UN10LINK1"))
            .await;
        assert!(
            duplicate.is_err(),
            "a duplicate link must be rejected once the unique index exists"
        );

        // `down` exists for isolated tests / development refresh only — the
        // runtime runner never calls it (forward-only, ADR/ER-08).
        let manager = SchemaManager::new(&db);
        Migration.down(&manager).await.expect("down should drop it");
        assert!(!index_exists(&db).await, "down drops the index");
    }

    #[tokio::test]
    async fn un10_up_aborts_with_the_full_conflict_list_and_changes_no_row() {
        let (_temp, db) = migrated_db_without_index().await;
        // Two distinct duplicated links, plus a unique one that must not be
        // listed.
        insert_cl(&db, 920_001, "/a", "UN10DUPA").await;
        insert_cl(&db, 920_002, "/b", "UN10DUPA").await;
        insert_cl(&db, 920_003, "/a", "UN10DUPB").await;
        insert_cl(&db, 920_004, "/b", "UN10DUPB").await;
        insert_cl(&db, 920_005, "/c", "UN10DUPB").await;
        insert_cl(&db, 920_006, "/a", "UN10UNIQ").await;
        let before = cl_row_count(&db).await;

        let err = run_up_in_txn(&db)
            .await
            .expect_err("duplicated links must abort the migration");
        let message = err.to_string();
        assert!(
            message.contains("UN10DUPA") && message.contains("UN10DUPB"),
            "the error must list every conflicting link: {message}"
        );
        assert!(
            message.contains("(2 rows)") && message.contains("(3 rows)"),
            "the error must report each link's occurrence count: {message}"
        );
        assert!(
            !message.contains("UN10UNIQ"),
            "a non-duplicated link must not be listed: {message}"
        );

        assert!(!index_exists(&db).await, "an aborted up creates no index");
        assert_eq!(
            cl_row_count(&db).await,
            before,
            "an aborted up must not touch any business row"
        );
    }

    #[tokio::test]
    async fn un10_concurrent_insert_is_blocked_until_the_index_exists() {
        let (_temp, db) = migrated_db_without_index().await;
        insert_cl(&db, 930_001, "/a", "UN10RACE").await;

        // Hold the migration's transaction open: it has taken the write
        // exclusion lock and created the index, but has not committed.
        let txn = db.begin().await.expect("begin migration txn");
        let manager = SchemaManager::new(&txn);
        Migration.up(&manager).await.expect("up inside the txn");

        // A concurrent writer in another session must block rather than slip a
        // duplicate in between the scan and the index.
        let writer = db.begin().await.expect("begin writer txn");
        let blocked = tokio::time::timeout(
            Duration::from_secs(2),
            writer.execute_unprepared(&insert_cl_sql(930_002, "/b", "UN10RACE")),
        )
        .await;
        assert!(
            blocked.is_err(),
            "the concurrent insert must block while the migration holds the lock"
        );

        txn.commit().await.expect("commit migration txn");
        assert!(index_exists(&db).await, "the index exists after commit");

        // Once the lock is released the blocked writer proceeds — and is now
        // rejected by the unique index rather than silently creating a
        // duplicate. (The write itself is retried here because the timed-out
        // future above was dropped.)
        let after = tokio::time::timeout(
            Duration::from_secs(10),
            writer.execute_unprepared(&insert_cl_sql(930_003, "/c", "UN10RACE")),
        )
        .await
        .expect("the writer is no longer blocked once the migration committed");
        assert!(
            after.is_err(),
            "after the index exists a duplicate link must be rejected"
        );
        let _ = writer.rollback().await;
    }

    #[tokio::test]
    async fn un10_link_equality_query_uses_the_unique_index() {
        let (_temp, db) = migrated_db_without_index().await;
        insert_cl(&db, 940_001, "/a", "UN10PLAN").await;
        run_up_in_txn(&db).await.expect("up should succeed");

        // Force the planner to prefer any available index so the assertion is
        // about the index existing and being usable, not about table size. The
        // setting and the EXPLAIN must run in the same session, hence the
        // transaction (the pool would otherwise hand out another connection).
        let txn = db.begin().await.expect("begin plan txn");
        txn.execute_unprepared("SET LOCAL enable_seqscan = off")
            .await
            .expect("disable seqscan");
        let rows = txn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "EXPLAIN SELECT id FROM mega_cl WHERE link = 'UN10PLAN'".to_owned(),
            ))
            .await
            .expect("explain query");
        let plan = rows
            .iter()
            .map(|row| {
                row.try_get::<String>("", "QUERY PLAN")
                    .expect("QUERY PLAN column")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plan.contains(INDEX_NAME),
            "a link equality lookup must use {INDEX_NAME}: {plan}"
        );
        txn.rollback().await.expect("rollback plan txn");
    }
}
