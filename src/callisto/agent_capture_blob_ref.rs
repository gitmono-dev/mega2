//! Typed blob ownership rows. Exactly one owner column is non-null.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_blob_ref")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub blob_id: i64,
    pub owner_session_id: Option<i64>,
    pub owner_event_id: Option<i64>,
    pub owner_checkpoint_id: Option<i64>,
    pub owner_file_op_id: Option<i64>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
