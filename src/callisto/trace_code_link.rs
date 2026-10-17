//! Code link formed from `Libra-Parent-Commit`. A missing code OID stays
//! `unresolved`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_code_link")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub checkpoint_id: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub code_oid: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub link_state: String,
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
            code_oid: Set(None),
            link_state: Set("unresolved".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("unresolved link");
        let resolved_without_oid = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            checkpoint_id: Set(checkpoint_id.clone()),
            code_oid: Set(None),
            link_state: Set("resolved".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(
            resolved_without_oid.is_err(),
            "resolved requires a code oid"
        );
        let resolved = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id),
            checkpoint_id: Set(checkpoint_id),
            code_oid: Set(Some("abc123".to_owned())),
            link_state: Set("resolved".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("resolved link");
        let loaded = super::Entity::find_by_id(stored.id)
            .one(&db)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded.link_state, "unresolved");
        assert!(loaded.code_oid.is_none());
        assert_eq!(resolved.code_oid.as_deref(), Some("abc123"));
    }
}
