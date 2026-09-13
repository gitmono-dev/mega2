//! Agent Capture tombstone. Natural-key lookup feeds HTTP 409.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_tombstone")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    #[sea_orm(column_type = "Text")]
    pub repo_id: String,
    #[sea_orm(column_type = "Text")]
    pub producer_id: String,
    #[sea_orm(column_type = "Text")]
    pub session_kind: String,
    #[sea_orm(column_type = "Text")]
    pub client_session_id: String,
    pub capture_id: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
