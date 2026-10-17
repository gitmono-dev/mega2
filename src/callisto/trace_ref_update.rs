//! Durable ref journal and index task for one stream. `ref_sequence` and the
//! idempotency key are unique. Claim state survives process restart.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_ref_update")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    pub ref_sequence: i64,
    #[sea_orm(column_type = "Text")]
    pub idempotency_key: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub old_tip: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub new_tip: String,
    #[sea_orm(column_type = "Text")]
    pub state: String,
    pub claim_until: Option<DateTimeWithTimeZone>,
    pub attempt_count: i64,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, Set};
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

    fn update(stream_id: &str, sequence: i64, key: &str) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            ref_sequence: Set(sequence),
            idempotency_key: Set(key.to_owned()),
            old_tip: Set(None),
            new_tip: Set("ccddee".to_owned()),
            state: Set("pending".to_owned()),
            claim_until: Set(None),
            attempt_count: Set(0),
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
        update(&stream_id, 1, "journal-synthetic-1")
            .insert(&db)
            .await
            .expect("journal");
        let same_sequence = update(&stream_id, 1, "journal-synthetic-2")
            .insert(&db)
            .await;
        assert!(same_sequence.is_err(), "ref_sequence is unique in a stream");
        let same_key = update(&stream_id, 2, "journal-synthetic-1")
            .insert(&db)
            .await;
        assert!(same_key.is_err(), "journal idempotency key is unique");
        update(&stream_id, 2, "journal-synthetic-2")
            .insert(&db)
            .await
            .expect("next sequence");
    }
}
