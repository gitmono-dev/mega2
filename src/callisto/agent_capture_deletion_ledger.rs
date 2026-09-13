//! Deletion intent ledger. Rows record intent only; they do not delete objects.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_deletion_ledger")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    pub blob_id: Option<i64>,
    pub capture_id: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub intent: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
