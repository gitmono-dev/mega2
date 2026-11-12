//! Bind the legacy writer barrier to the triggering table's schema. Published
//! installations are repaired forward without replacing their journal or ACLs.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE OR REPLACE FUNCTION trace_legacy_write_fence()
                 RETURNS trigger
                 LANGUAGE plpgsql
                 SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 AS $$
                 DECLARE fenced boolean;
                 BEGIN
                   EXECUTE pg_catalog.format(
                     'SELECT EXISTS (SELECT 1 FROM %I.trace_legacy_writer_fence WHERE active)',
                     TG_TABLE_SCHEMA) INTO fenced;
                   IF fenced THEN
                     RAISE EXCEPTION 'legacy capture writes are fenced for purge';
                   END IF;
                   IF TG_OP = 'DELETE' THEN
                     RETURN OLD;
                   END IF;
                   RETURN NEW;
                 END;
                 $$",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Never restore the misbound barrier or erase a purge record.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn upgrade_preserves_journal_and_function_authority() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db_connection(root.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let manager = SchemaManager::new(&db);
        // Reproduce the already-published function, then apply the forward repair.
        crate::jupiter::migration::m20261015_000400_trace_legacy_purge::Migration
            .up(&manager)
            .await
            .unwrap();
        let authority = || {
            Statement::from_string(
                DbBackend::Postgres,
                "SELECT proowner::bigint AS owner, proacl::text AS acl FROM pg_proc
                 WHERE oid = 'trace_legacy_write_fence()'::regprocedure",
            )
        };
        let before = db.query_one_raw(authority()).await.unwrap().unwrap();
        let journal = uuid::Uuid::new_v4();
        let fence = uuid::Uuid::new_v4();
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest)
             VALUES ('{journal}', 'local:fixture', 'fixture', 'fixture', 'sha256:fixture');
             INSERT INTO trace_legacy_writer_fence
             (id, purge_id, deployment_id, active, released_at)
             VALUES ('{fence}', '{journal}', 'fixture', false, now())"
        ))
        .await
        .unwrap();
        Migration.up(&manager).await.unwrap();
        Migration.down(&manager).await.unwrap();
        let after = db.query_one_raw(authority()).await.unwrap().unwrap();
        assert_eq!(
            before.try_get::<i64>("", "owner").unwrap(),
            after.try_get::<i64>("", "owner").unwrap()
        );
        assert_eq!(
            before.try_get::<Option<String>>("", "acl").unwrap(),
            after.try_get::<Option<String>>("", "acl").unwrap()
        );
        db.execute_unprepared(
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind)
             VALUES ('fixture', 'tenant', 'repo', 'producer', 'before-fence', 'external_capture')",
        )
        .await
        .expect("inactive fence in non-public schema permits writes");
        db.execute_unprepared("UPDATE trace_legacy_writer_fence SET active = true")
            .await
            .unwrap();
        for sql in [
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind)
             VALUES ('fixture', 'tenant', 'repo', 'producer', 'after-fence', 'external_capture')",
            "UPDATE agent_capture_session SET producer_id = 'changed'",
            "DELETE FROM agent_capture_session",
        ] {
            let error = db.execute_unprepared(sql).await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("legacy capture writes are fenced")
            );
        }
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM trace_legacy_writer_fence f
                 JOIN trace_legacy_purge p ON p.id = f.purge_id WHERE f.active",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 1);
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn quoted_schema_ignores_shadow_fence_and_missing_real_fence_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db_connection(root.path()).await;
        let txn = db.begin().await.unwrap();
        let schema = format!("legacy\"{}", uuid::Uuid::new_v4().simple());
        let quoted = format!("\"{}\"", schema.replace('"', "\"\""));
        txn.execute_unprepared(&format!(
            "CREATE SCHEMA {quoted};
             SET LOCAL search_path TO {quoted}, pg_catalog;
             CREATE TABLE agent_capture_session (payload text);
             CREATE TABLE trace_legacy_writer_fence (active boolean);
             INSERT INTO trace_legacy_writer_fence VALUES (false)"
        ))
        .await
        .unwrap();
        let manager = SchemaManager::new(&txn);
        Migration.up(&manager).await.unwrap();
        txn.execute_unprepared(
            "CREATE TRIGGER barrier BEFORE INSERT OR UPDATE OR DELETE
             ON agent_capture_session FOR EACH ROW EXECUTE FUNCTION trace_legacy_write_fence();
             INSERT INTO agent_capture_session VALUES ('kept');
             UPDATE trace_legacy_writer_fence SET active = true;
             CREATE TEMP TABLE trace_legacy_writer_fence (active boolean);
             SET LOCAL search_path TO pg_temp, pg_catalog",
        )
        .await
        .unwrap();
        txn.execute_unprepared("SAVEPOINT refused").await.unwrap();
        let error = txn
            .execute_unprepared(&format!(
                "INSERT INTO {quoted}.agent_capture_session VALUES ('denied')"
            ))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("legacy capture writes are fenced")
        );
        txn.execute_unprepared("ROLLBACK TO SAVEPOINT refused")
            .await
            .unwrap();
        txn.execute_unprepared(&format!("DROP TABLE {quoted}.trace_legacy_writer_fence"))
            .await
            .unwrap();
        let error = txn
            .execute_unprepared(&format!("DELETE FROM {quoted}.agent_capture_session"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not exist"));
        txn.rollback().await.unwrap();
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn repair_after_retirement_does_not_recreate_legacy_tables() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db_connection(root.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let tables = db
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT format('%I.%I', schemaname, tablename) AS qualified FROM pg_tables
                 WHERE schemaname = current_schema() AND tablename LIKE 'agent_capture_%'",
            ))
            .await
            .unwrap();
        assert_eq!(tables.len(), 12);
        for table in tables {
            let qualified: String = table.try_get("", "qualified").unwrap();
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS {qualified} CASCADE"))
                .await
                .unwrap();
        }
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.unwrap();
        Migration.down(&manager).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM pg_tables
                 WHERE schemaname = current_schema() AND tablename LIKE 'agent_capture_%'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
        db.close().await.unwrap();
    }
}
