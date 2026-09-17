//! FT-05: `mega_tag.path` so later cards can isolate tags by `(path, tag_name)`.
//!
//! Forward-only (`down` is a no-op). Existing rows are backfilled from
//! `mega_refs.path` where `ref_name = 'refs/tags/' || tag_name`; otherwise
//! `/`. A tag that matches more than one distinct ref path, or a
//! `(path, tag_name)` collision after backfill, fails the upgrade.

use sea_orm::{DbBackend, Statement};
use sea_orm_migration::prelude::*;

pub const INDEX_NAME: &str = "idx_mtag_path_tag_name";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(MegaTag::Table)
                    .add_column(ColumnDef::new(MegaTag::Path).text().not_null().default("/"))
                    .to_owned(),
            )
            .await?;

        let conn = manager.get_connection();
        let ambiguous = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT t.tag_name AS tag_name, count(DISTINCT r.path) AS paths \
                 FROM mega_tag t \
                 JOIN mega_refs r ON r.ref_name = ('refs/tags/' || t.tag_name) \
                 GROUP BY t.id, t.tag_name \
                 HAVING count(DISTINCT r.path) > 1 \
                 ORDER BY t.tag_name",
            ))
            .await?;
        if !ambiguous.is_empty() {
            let mut listed: Vec<String> = Vec::with_capacity(ambiguous.len());
            for row in &ambiguous {
                let tag_name: String = row.try_get("", "tag_name")?;
                let paths: i64 = row.try_get("", "paths")?;
                listed.push(format!("{tag_name} ({paths} paths)"));
            }
            return Err(DbErr::Migration(format!(
                "cannot backfill mega_tag.path: {} tag_name(s) match more than one \
                 mega_refs.path; resolve them first. Conflicting tags: {}",
                listed.len(),
                listed.join(", ")
            )));
        }

        conn.execute_unprepared(
            "UPDATE mega_tag AS t \
             SET path = s.path \
             FROM ( \
                 SELECT t2.id AS id, min(r.path) AS path \
                 FROM mega_tag t2 \
                 JOIN mega_refs r ON r.ref_name = ('refs/tags/' || t2.tag_name) \
                 GROUP BY t2.id \
                 HAVING count(DISTINCT r.path) = 1 \
             ) AS s \
             WHERE t.id = s.id",
        )
        .await?;

        let conflicts = conn
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT path, tag_name, count(*) AS occurrences \
                 FROM mega_tag \
                 GROUP BY path, tag_name \
                 HAVING count(*) > 1 \
                 ORDER BY path, tag_name",
            ))
            .await?;
        if !conflicts.is_empty() {
            let mut listed: Vec<String> = Vec::with_capacity(conflicts.len());
            for row in &conflicts {
                let path: String = row.try_get("", "path")?;
                let tag_name: String = row.try_get("", "tag_name")?;
                let occurrences: i64 = row.try_get("", "occurrences")?;
                listed.push(format!("{path}/{tag_name} ({occurrences} rows)"));
            }
            return Err(DbErr::Migration(format!(
                "cannot create unique (path, tag_name) on mega_tag: {} duplicated \
                 pair(s) found; resolve them first. Conflicts: {}",
                listed.len(),
                listed.join(", ")
            )));
        }

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(INDEX_NAME)
                    .unique()
                    .table(MegaTag::Table)
                    .col(MegaTag::Path)
                    .col(MegaTag::TagName)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum MegaTag {
    Table,
    Path,
    TagName,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

    use super::*;
    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    async fn path_null_count(db: &DatabaseConnection) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM mega_tag WHERE path IS NULL".to_owned(),
            ))
            .await
            .expect("null count")
            .expect("null count row");
        row.try_get("", "n").expect("n")
    }

    async fn tag_path(db: &DatabaseConnection, tag_name: &str) -> String {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT path FROM mega_tag WHERE tag_name = $1",
                [tag_name.into()],
            ))
            .await
            .expect("path query")
            .expect("tag row");
        row.try_get("", "path").expect("path")
    }

    #[tokio::test]
    async fn mega_tag_path_migration_backfill() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("new database must apply including mega_tag.path");
        assert_eq!(
            path_null_count(&db).await,
            0,
            "fresh up leaves no NULL path"
        );

        db.execute_unprepared(&format!("DROP INDEX IF EXISTS {INDEX_NAME}"))
            .await
            .expect("drop unique index");
        db.execute_unprepared("ALTER TABLE mega_tag DROP COLUMN path")
            .await
            .expect("drop path to restore pre-FT-05 shape");

        db.execute_unprepared(
            "INSERT INTO mega_tag \
             (id, tag_id, object_id, object_type, tag_name, tagger, message, \
              created_at, pack_id, pack_offset) \
             VALUES \
             (910001, 'tag-matched', 'obj-1', 'commit', 'ft05-matched', 't', '', \
              now(), '', 0), \
             (910002, 'tag-orphan', 'obj-2', 'commit', 'ft05-orphan', 't', '', \
              now(), '', 0)",
        )
        .await
        .expect("seed mega_tag");
        db.execute_unprepared(
            "INSERT INTO mega_refs \
             (id, path, ref_name, ref_commit_hash, ref_tree_hash, \
              created_at, updated_at, is_cl) \
             VALUES \
             (910001, '/project', 'refs/tags/ft05-matched', 'c', 'tree', \
              now(), now(), false)",
        )
        .await
        .expect("seed mega_refs");

        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("backfill up must succeed");

        assert_eq!(
            path_null_count(&db).await,
            0,
            "backfill leaves no NULL path"
        );
        assert_eq!(tag_path(&db, "ft05-matched").await, "/project");
        assert_eq!(tag_path(&db, "ft05-orphan").await, "/");

        let idx = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass(current_schema() || '.{INDEX_NAME}')::text AS name"),
            ))
            .await
            .expect("catalog")
            .expect("catalog row");
        assert!(
            idx.try_get::<Option<String>>("", "name")
                .expect("name")
                .is_some(),
            "{INDEX_NAME} must exist"
        );

        let kept = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass(current_schema() || '.idx_mtag_tag_id')::text AS name"
                    .to_owned(),
            ))
            .await
            .expect("tag_id catalog")
            .expect("tag_id row");
        assert!(
            kept.try_get::<Option<String>>("", "name")
                .expect("name")
                .is_some(),
            "idx_mtag_tag_id must remain"
        );
    }
}
