//! Agent Capture checkpoints (external_capture snapshots).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_checkpoint")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub capture_id: i64,
    #[sea_orm(column_type = "Text")]
    pub checkpoint_id: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub transcript_digest: Option<String>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub metadata: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
