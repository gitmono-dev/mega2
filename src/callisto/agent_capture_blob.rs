//! Agent Capture CAS blob rows (staging and committed).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_blob")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    #[sea_orm(column_type = "Text")]
    pub digest: String,
    #[sea_orm(column_type = "Text")]
    pub visibility: String,
    #[sea_orm(column_type = "Text")]
    pub object_key: String,
    pub size_bytes: i64,
    #[sea_orm(column_type = "Text")]
    pub lease_state: String,
    pub lease_generation: i64,
    #[sea_orm(column_type = "Text", nullable)]
    pub lease_id: Option<String>,
    pub lease_expires_at: Option<DateTimeWithTimeZone>,
    pub capture_id: Option<i64>,
    #[sea_orm(column_type = "Text", nullable)]
    pub upload_intent: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub cleanup_intent: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
