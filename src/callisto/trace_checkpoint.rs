//! Checkpoint projection. The checkpoint key is unique inside one stream, and
//! `original_tree` does not change after insert.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_checkpoint")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub checkpoint_key: String,
    #[sea_orm(column_type = "Text")]
    pub original_tree: String,
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

    fn checkpoint(stream_id: &str, key: &str, tree: &str) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            checkpoint_key: Set(key.to_owned()),
            original_tree: Set(tree.to_owned()),
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
        let other_stream = stream(&db).await;
        let stored = checkpoint(&stream_id, "ckpt-1", "tree-aaa")
            .insert(&db)
            .await
            .expect("checkpoint");
        let duplicate = checkpoint(&stream_id, "ckpt-1", "tree-bbb")
            .insert(&db)
            .await;
        assert!(duplicate.is_err(), "checkpoint key is unique in the stream");
        let other = checkpoint(&other_stream, "ckpt-1", "tree-ccc")
            .insert(&db)
            .await
            .expect("the same key is free in another stream");
        assert_ne!(stored.id, other.id);
        let changed = db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE trace_checkpoint SET original_tree = 'tree-zzz' WHERE id = $1",
                [stored.id.clone().into()],
            ))
            .await;
        assert!(changed.is_err(), "original_tree is immutable");
        let loaded = super::Entity::find_by_id(stored.id)
            .one(&db)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded.original_tree, "tree-aaa");
        assert_eq!(loaded.checkpoint_key, "ckpt-1");
    }
}
