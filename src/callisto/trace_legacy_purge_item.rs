//! One frozen inventory item of a purge journal (MTR-23). Each item records
//! the provider-native version and generation so cloud history, not only the
//! current listing, can be proved deleted.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_legacy_purge_item")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub purge_id: String,
    #[sea_orm(column_type = "Text")]
    pub scope: String,
    #[sea_orm(column_type = "Text")]
    pub object_key: String,
    /// Provider-native object version (``None`` when the backend has no
    /// versions).
    #[sea_orm(column_type = "Text", nullable)]
    pub provider_version: Option<String>,
    /// Provider-native generation (GCS-style backends).
    #[sea_orm(column_type = "Text", nullable)]
    pub provider_generation: Option<String>,
    pub deleted_at: Option<DateTimeWithTimeZone>,
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

    #[tokio::test]
    async fn db_contract() {
        let (_temp_dir, db) = db().await;
        let purge_id = insert_journal(&db).await;
        // AC-3/AC-4: version and generation round-trip for one item.
        let item = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            purge_id: Set(purge_id.clone()),
            scope: Set("deploy/tenant".to_owned()),
            object_key: Set("agent/raw/one".to_owned()),
            provider_version: Set(Some("v3".to_owned())),
            provider_generation: Set(Some("17".to_owned())),
            deleted_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert item");
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT provider_version, provider_generation FROM trace_legacy_purge_item WHERE id = $1",
                [item.id.clone().into()],
            ))
            .await
            .expect("read item")
            .expect("item row");
        assert_eq!(
            row.try_get::<Option<String>>("", "provider_version")
                .unwrap()
                .as_deref(),
            Some("v3")
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "provider_generation")
                .unwrap()
                .as_deref(),
            Some("17")
        );

        // The journal owns its items: deleting the journal cascades.
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM trace_legacy_purge WHERE id = $1",
            [purge_id.clone().into()],
        ))
        .await
        .expect("delete journal");
        let remaining = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM trace_legacy_purge_item WHERE purge_id = $1",
                [purge_id.into()],
            ))
            .await
            .expect("count items")
            .expect("count row");
        assert_eq!(remaining.try_get::<i64>("", "n").unwrap(), 0);
        db.close().await.ok();
    }
}
