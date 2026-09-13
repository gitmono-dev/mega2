//! Agent Capture access audit. Append-only: updates and deletes are rejected.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_access_audit")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub actor: String,
    #[sea_orm(column_type = "Text")]
    pub action: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    async fn before_save<C>(self, _db: &C, insert: bool) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        if insert {
            Ok(self)
        } else {
            Err(DbErr::Custom(
                "agent_capture_access_audit is append-only".to_owned(),
            ))
        }
    }

    async fn before_delete<C>(self, _db: &C) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        Err(DbErr::Custom(
            "agent_capture_access_audit is append-only".to_owned(),
        ))
    }
}
