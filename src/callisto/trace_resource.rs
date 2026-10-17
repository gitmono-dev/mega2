//! Resource projection. Order and the logical byte range are stored columns.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_resource")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub checkpoint_id: String,
    pub ordinal: i64,
    pub byte_start: i64,
    pub byte_end: i64,
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
        callisto::{trace_checkpoint, trace_principal, trace_stream, trace_target},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn checkpoint(db: &sea_orm::DatabaseConnection) -> (String, String) {
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
        let stream_id = trace_stream::ActiveModel {
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
        .id;
        let checkpoint_id = trace_checkpoint::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            checkpoint_key: Set("ckpt-1".to_owned()),
            original_tree: Set("tree-aaa".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("checkpoint")
        .id;
        (stream_id, checkpoint_id)
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let (stream_id, checkpoint_id) = checkpoint(&db).await;
        let stored = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            checkpoint_id: Set(checkpoint_id.clone()),
            ordinal: Set(2),
            byte_start: Set(16),
            byte_end: Set(48),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("resource");
        let duplicate = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id),
            checkpoint_id: Set(checkpoint_id),
            ordinal: Set(2),
            byte_start: Set(0),
            byte_end: Set(8),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(duplicate.is_err(), "ordinal is unique in the checkpoint");
        let inverted = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stored.stream_id.clone()),
            checkpoint_id: Set(stored.checkpoint_id.clone()),
            ordinal: Set(3),
            byte_start: Set(20),
            byte_end: Set(10),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(inverted.is_err(), "byte range is ordered");
        let loaded = super::Entity::find_by_id(stored.id)
            .one(&db)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded.ordinal, 2);
        assert_eq!(loaded.byte_start, 16);
        assert_eq!(loaded.byte_end, 48);
        let (_other_stream, other_checkpoint) = checkpoint(&db).await;
        let crossed = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stored.stream_id),
            checkpoint_id: Set(other_checkpoint),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(1),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(
            crossed.is_err(),
            "a resource stays in its checkpoint stream"
        );
    }
}
