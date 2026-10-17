//! One receive attempt for a stream. The idempotency key stops a lost `ok`
//! from writing a second journal row for the same push.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_ingest_attempt")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub idempotency_key: String,
    #[sea_orm(column_type = "Text")]
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use uuid::Uuid;

    use crate::{
        callisto::{trace_principal, trace_stream, trace_target},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn stream(db: &sea_orm::DatabaseConnection) -> String {
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
        trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id),
            target_id: Set(target_id),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("stream")
        .id
    }

    fn attempt(stream_id: &str, key: &str) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            idempotency_key: Set(key.to_owned()),
            state: Set("published".to_owned()),
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
        let stream_id = stream(&db).await;
        let key = "push-retry-synthetic";
        attempt(&stream_id, key).insert(&db).await.expect("attempt");
        let duplicate = attempt(&stream_id, key).insert(&db).await;
        assert!(duplicate.is_err(), "ingest idempotency key is unique");
        let loaded = super::Entity::find().all(&db).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].idempotency_key, key);
    }
}
