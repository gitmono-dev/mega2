//! Ingest idempotency receipts. Fingerprints come from the AC-22 helper.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_ingest_receipt")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    #[sea_orm(column_type = "Text")]
    pub producer_id: String,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub operation: String,
    #[sea_orm(column_type = "Text")]
    pub idempotency_key: String,
    #[sea_orm(column_type = "Text")]
    pub fingerprint: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub response: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
