//! Agent Capture file operations tied to a source event uid.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_file_op")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub source_event_uid: String,
    #[sea_orm(column_type = "Text")]
    pub op: String,
    #[sea_orm(column_type = "Text")]
    pub path: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
