//! Cross-instance writer fence for the legacy Capture tables (MTR-23).
//! The owning migration installs a DB-level barrier function on all 12 legacy
//! tables: while one active fence exists, runtime DML is refused. The fence
//! row itself is tied to exactly one purge journal.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_legacy_writer_fence")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    /// One fence per journal; the barrier reads this relation (AC-5).
    #[sea_orm(column_type = "Text")]
    pub purge_id: String,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    pub active: bool,
    pub activated_at: DateTimeWithTimeZone,
    pub released_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ConnectionTrait, DbBackend, Set, Statement};
    use uuid::Uuid;

    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (temp_dir, db)
    }

    async fn insert_journal(db: &sea_orm::DatabaseConnection) -> String {
        let id = Uuid::new_v4().to_string();
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ($1, 'backend', 'location', 'deploy', 'digest', 'frozen')",
            [id.clone().into()],
        ))
        .await
        .expect("insert journal");
        id
    }

    fn legacy_session(client_session_id: &str) -> Statement {
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id,
              session_kind, completeness, started_at)
             VALUES ('d', 't', 'r', 'p', $1, 'external_capture', 'complete', now())",
            [client_session_id.into()],
        )
    }

    #[tokio::test]
    async fn db_contract() {
        let (_temp_dir, db) = db().await;
        let purge_id = insert_journal(&db).await;
        // AC-5: one fence per journal, tied by a unique foreign key.
        let fence = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            purge_id: Set(purge_id.clone()),
            deployment_id: Set("deploy".to_owned()),
            active: Set(true),
            activated_at: Set(chrono::Utc::now().fixed_offset()),
            released_at: Set(None),
        }
        .clone()
        .insert(&db)
        .await
        .expect("insert fence");
        let duplicate = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            active: Set(false),
            released_at: Set(Some(chrono::Utc::now().fixed_offset())),
            ..fence.clone().into()
        };
        assert!(
            duplicate.insert(&db).await.is_err(),
            "a second fence for one journal must be rejected"
        );

        // AC-6/AC-7: an active fence blocks legacy writes; releasing it
        // restores the old write behavior.
        let blocked = "fence-blocks-0001";
        let error = db
            .execute_raw(legacy_session(blocked))
            .await
            .expect_err("active fence must block the legacy write");
        assert!(
            error.to_string().contains("fenced"),
            "unexpected barrier error: {error}"
        );

        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE trace_legacy_writer_fence SET active = FALSE, released_at = now() WHERE id = $1",
            [fence.id.clone().into()],
        ))
        .await
        .expect("release fence");
        db.execute_raw(legacy_session("fence-inactive-0001"))
            .await
            .expect("inactive fence keeps the old write behavior");
        db.close().await.ok();
    }
}
