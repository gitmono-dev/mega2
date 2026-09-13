//! Agent Capture event rows keyed by `(capture_id, event_uid)`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_event")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub event_uid: String,
    #[sea_orm(column_type = "Text")]
    pub event_kind: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub native_id: Option<String>,
    pub lifecycle_seq: Option<i64>,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
    #[sea_orm(column_type = "Text")]
    pub payload_fingerprint: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
