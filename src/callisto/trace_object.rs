//! Published object membership inside one stream. Bytes stay in object
//! storage. The same OID may exist in another stream.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_object")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub stream_id: String,
    #[sea_orm(column_type = "Text")]
    pub hash_kind: String,
    #[sea_orm(column_type = "Text")]
    pub oid: String,
    pub visible: bool,
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
            hash_kind: Set("sha256".to_owned()),
            tip_oid: Set(None),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("stream")
        .id
    }

    fn object(stream_id: &str, oid: &str) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            hash_kind: Set("sha256".to_owned()),
            oid: Set(oid.to_owned()),
            visible: Set(true),
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
        let first = stream(&db).await;
        let second = stream(&db).await;
        let oid = "0123456789abcdef0123456789abcdef01234567";
        object(&first, oid).insert(&db).await.expect("object");
        let duplicate = object(&first, oid).insert(&db).await;
        assert!(duplicate.is_err(), "object oid is unique inside a stream");
        object(&second, oid)
            .insert(&db)
            .await
            .expect("same oid in another stream");
    }
}
