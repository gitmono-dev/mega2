//! Published trace stream. The natural key is the contract tuple
//! `(principal_id, target_id, source_repo_id, epoch_id)`. `hash_kind` is fixed
//! at insert.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_stream")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub principal_id: String,
    #[sea_orm(column_type = "Text")]
    pub target_id: String,
    #[sea_orm(column_type = "Text")]
    pub source_repo_id: String,
    #[sea_orm(column_type = "Text")]
    pub epoch_id: String,
    #[sea_orm(column_type = "Text")]
    pub hash_kind: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub tip_oid: Option<String>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, DatabaseBackend, EntityTrait, Set, Statement,
    };
    use uuid::Uuid;

    use crate::{
        callisto::{trace_principal, trace_target},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn identity(db: &sea_orm::DatabaseConnection) -> (String, String) {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("target");
        (principal_id, target_id)
    }

    fn stream(
        principal_id: &str,
        target_id: &str,
        source_repo_id: &str,
        epoch_id: &str,
        hash_kind: &str,
    ) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            target_id: Set(target_id.to_owned()),
            source_repo_id: Set(source_repo_id.to_owned()),
            epoch_id: Set(epoch_id.to_owned()),
            hash_kind: Set(hash_kind.to_owned()),
            tip_oid: Set(None),
            created_at: Set(Utc::now().fixed_offset()),
        }
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let (principal_id, target_id) = identity(&db).await;
        let source = "libra-repoid-opaque";
        let epoch = Uuid::new_v4().to_string();
        let stored = stream(&principal_id, &target_id, source, &epoch, "sha1")
            .insert(&db)
            .await
            .expect("stream");
        let duplicate = stream(&principal_id, &target_id, source, &epoch, "sha1")
            .insert(&db)
            .await;
        assert!(duplicate.is_err(), "natural tuple is unique");
        let other_epoch = stream(
            &principal_id,
            &target_id,
            source,
            &Uuid::new_v4().to_string(),
            "sha256",
        )
        .insert(&db)
        .await
        .expect("different epoch is a different stream");
        assert_ne!(stored.id, other_epoch.id);

        let changed = db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE trace_stream SET hash_kind = 'sha256' WHERE id = $1",
                [stored.id.clone().into()],
            ))
            .await;
        assert!(changed.is_err(), "hash_kind is immutable");
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE trace_stream SET tip_oid = 'aabb' WHERE id = $1",
            [stored.id.clone().into()],
        ))
        .await
        .expect("tip update");
        let loaded = super::Entity::find_by_id(stored.id)
            .one(&db)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded.hash_kind, "sha1");
        assert_eq!(loaded.tip_oid.as_deref(), Some("aabb"));
    }
}
