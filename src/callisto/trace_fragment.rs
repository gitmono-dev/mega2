//! Fragment content and its fixed checkpoint/resource membership.
//!
//! `ascii_fold` is a plain text column. This card does not build a trigram index.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_fragment")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub content_digest: String,
    pub index_version: i64,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    #[sea_orm(column_type = "Text")]
    pub ascii_fold: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub mod membership {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
    #[sea_orm(table_name = "trace_fragment_membership")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
        pub id: String,
        #[sea_orm(column_type = "Text")]
        pub fragment_id: String,
        #[sea_orm(column_type = "Text")]
        pub stream_id: String,
        #[sea_orm(column_type = "Text")]
        pub checkpoint_id: String,
        #[sea_orm(column_type = "Text")]
        pub resource_id: String,
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, DatabaseBackend, EntityTrait, Set, Statement,
    };
    use uuid::Uuid;

    use crate::{
        callisto::{trace_checkpoint, trace_principal, trace_resource, trace_stream, trace_target},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn resource(db: &sea_orm::DatabaseConnection) -> (String, String, String) {
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
        let resource_id = trace_resource::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            checkpoint_id: Set(checkpoint_id.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(12),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("resource")
        .id;
        (stream_id, checkpoint_id, resource_id)
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let (stream_id, checkpoint_id, resource_id) = resource(&db).await;
        let stored = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            content_digest: Set("digest-1".to_owned()),
            index_version: Set(1),
            body: Set("Hello".to_owned()),
            ascii_fold: Set("hello".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("fragment");
        let duplicate = super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            content_digest: Set("digest-1".to_owned()),
            index_version: Set(1),
            body: Set("Hello".to_owned()),
            ascii_fold: Set("hello".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(
            duplicate.is_err(),
            "digest and index_version are unique in the stream"
        );
        let changed = db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE trace_fragment SET index_version = 2 WHERE id = $1",
                [stored.id.clone().into()],
            ))
            .await;
        assert!(changed.is_err(), "index_version is immutable");
        let other_resource = trace_resource::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            checkpoint_id: Set(checkpoint_id.clone()),
            ordinal: Set(1),
            byte_start: Set(12),
            byte_end: Set(24),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("second resource")
        .id;
        let membership = super::membership::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            fragment_id: Set(stored.id.clone()),
            stream_id: Set(stream_id),
            checkpoint_id: Set(checkpoint_id.clone()),
            resource_id: Set(resource_id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("membership");
        let moved = db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE trace_fragment_membership SET resource_id = $2 WHERE id = $1",
                [membership.id.clone().into(), other_resource.into()],
            ))
            .await;
        assert!(moved.is_err(), "membership provenance is immutable");
        let loaded = super::Entity::find_by_id(stored.id)
            .one(&db)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(loaded.index_version, 1);
        assert_eq!(loaded.ascii_fold, "hello");
        let stored_membership = super::membership::Entity::find_by_id(membership.id)
            .one(&db)
            .await
            .expect("load membership")
            .expect("membership row");
        assert_eq!(stored_membership.checkpoint_id, checkpoint_id);
        assert_eq!(stored_membership.resource_id, resource_id);
        let other_checkpoint = trace_checkpoint::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stored.stream_id.clone()),
            checkpoint_key: Set("ckpt-2".to_owned()),
            original_tree: Set("tree-bbb".to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("second checkpoint")
        .id;
        let foreign_resource = trace_resource::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stored.stream_id.clone()),
            checkpoint_id: Set(other_checkpoint),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(4),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("resource on the other checkpoint")
        .id;
        let crossed = super::membership::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            fragment_id: Set(loaded.id),
            stream_id: Set(stored.stream_id),
            checkpoint_id: Set(checkpoint_id),
            resource_id: Set(foreign_resource),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(
            crossed.is_err(),
            "membership resource stays on the named checkpoint"
        );
    }
}
