//! Credential binding for a trace principal.
//!
//! `namespace` keeps static push tokens and Mono accounts independent.
//! The row stores a credential id, not secret material. Retiring a row is
//! the explicit step that allows the same id to be bound again.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_credential_binding")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub principal_id: String,
    #[sea_orm(column_type = "Text")]
    pub namespace: String,
    #[sea_orm(column_type = "Text")]
    pub credential_id: String,
    pub created_at: DateTimeWithTimeZone,
    pub retired_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use uuid::Uuid;

    use super::Column;
    use crate::{
        callisto::trace_principal,
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn principal(db: &sea_orm::DatabaseConnection) -> String {
        let id = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("insert principal");
        id
    }

    fn binding(principal_id: &str, namespace: &str, credential_id: &str) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            namespace: Set(namespace.to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let token_owner = principal(&db).await;
        let account_owner = principal(&db).await;
        let credential_id = "cred-synthetic-alpha";

        binding(&token_owner, "static_push_token", credential_id)
            .insert(&db)
            .await
            .expect("token namespace binding");
        binding(&account_owner, "mono_account", credential_id)
            .insert(&db)
            .await
            .expect("account namespace binding is independent");

        let rows = super::Entity::find()
            .filter(Column::CredentialId.eq(credential_id))
            .all(&db)
            .await
            .expect("load bindings");
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.namespace == "static_push_token" && row.principal_id == token_owner)
        );
        assert!(
            rows.iter()
                .any(|row| row.namespace == "mono_account" && row.principal_id == account_owner)
        );

        let duplicate = binding(&token_owner, "static_push_token", credential_id)
            .insert(&db)
            .await;
        assert!(
            duplicate.is_err(),
            "one active binding per namespace and credential"
        );

        let mut token_row: super::ActiveModel = rows
            .into_iter()
            .find(|row| row.namespace == "static_push_token")
            .expect("token row")
            .into();
        token_row.retired_at = Set(Some(Utc::now().fixed_offset()));
        token_row.update(&db).await.expect("retire binding");

        let rebound = binding(&account_owner, "static_push_token", credential_id)
            .insert(&db)
            .await
            .expect("explicit rebind after retire");
        assert_ne!(rebound.principal_id, token_owner);
    }
}
