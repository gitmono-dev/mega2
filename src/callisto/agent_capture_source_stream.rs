//! Agent Capture source-stream watermarks keyed by generation.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_source_stream")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub stream_kind: String,
    pub generation: i64,
    pub byte_offset: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
