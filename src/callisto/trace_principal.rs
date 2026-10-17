//! Server-stable trace principal. The id is an assigned UUID, not a username.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_principal")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
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

    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let id = Uuid::new_v4().to_string();
        let stored = super::ActiveModel {
            id: Set(id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("insert principal");
        let loaded = super::Entity::find_by_id(id.clone())
            .one(&db)
            .await
            .expect("load principal")
            .expect("principal row");
        assert_eq!(loaded.id, id);
        assert_eq!(stored.id, id);
        assert_eq!(
            Uuid::parse_str(&loaded.id).expect("canonical uuid"),
            Uuid::parse_str(&id).unwrap()
        );

        let duplicate = super::ActiveModel {
            id: Set(id),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await;
        assert!(duplicate.is_err(), "principal id is immutable and unique");
    }
}
