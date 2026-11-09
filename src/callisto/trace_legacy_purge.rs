//! Durable purge journal (MTR-23). One row freezes the authorized
//! deployment/backend inventory before any destructive step. `down` of the
//! owning migration is a no-op, so a journal row is never silently dropped.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_legacy_purge")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    /// Sanitized backend fingerprint (provider + identity digest), never a
    /// credential (AC-1).
    #[sea_orm(column_type = "Text")]
    pub backend_fingerprint: String,
    /// Historical storage location (bucket/volume + prefix), never a secret
    /// (AC-2).
    #[sea_orm(column_type = "Text")]
    pub storage_location: String,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    /// Digest of the frozen maintenance manifest.
    #[sea_orm(column_type = "Text")]
    pub manifest_digest: String,
    #[sea_orm(column_type = "Text")]
    pub state: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub cursor: Option<String>,
    pub retry_count: i32,
    pub created_at: DateTimeWithTimeZone,
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

    fn journal() -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            backend_fingerprint: Set("local:sha256:fixture".to_owned()),
            storage_location: Set("local:/tmp/mega2-legacy".to_owned()),
            deployment_id: Set("deploy-fixture".to_owned()),
            manifest_digest: Set("sha256:manifest".to_owned()),
            state: Set("frozen".to_owned()),
            cursor: Set(None),
            retry_count: Set(0),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        }
    }

    #[tokio::test]
    async fn db_contract() {
        let (_temp_dir, db) = db().await;
        // AC-1/AC-2: the journal stores the backend fingerprint and the
        // historical storage location of the frozen inventory.
        let saved = journal().insert(&db).await.expect("insert journal");
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT backend_fingerprint, storage_location, state FROM trace_legacy_purge WHERE id = $1",
                [saved.id.clone().into()],
            ))
            .await
            .expect("read journal")
            .expect("journal row");
        assert_eq!(
            row.try_get::<String>("", "backend_fingerprint").unwrap(),
            "local:sha256:fixture"
        );
        assert_eq!(
            row.try_get::<String>("", "storage_location").unwrap(),
            "local:/tmp/mega2-legacy"
        );
        assert_eq!(row.try_get::<String>("", "state").unwrap(), "frozen");

        // The UUID and state contracts are enforced by the schema.
        let bad_id = super::ActiveModel {
            id: Set("not-a-uuid".to_owned()),
            ..journal()
        };
        assert!(bad_id.insert(&db).await.is_err(), "non-uuid journal id");
        let bad_state = super::ActiveModel {
            state: Set("unknown".to_owned()),
            ..journal()
        };
        assert!(bad_state.insert(&db).await.is_err(), "unknown state");
        db.close().await.ok();
    }
}
