//! Transactional CL webhook delivery outbox with `lease_epoch` fencing (`CLB-10`).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "cl_webhook_outbox")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    #[sea_orm(column_type = "Text", unique)]
    pub event_id: String,
    #[sea_orm(column_type = "Text")]
    pub event_type: String,
    #[sea_orm(column_type = "Text")]
    pub cl_link: String,
    #[sea_orm(column_type = "Text")]
    pub cl_path: String,
    pub head_revision: i64,
    #[sea_orm(column_type = "Text")]
    pub payload_json: String,
    #[sea_orm(column_type = "Text")]
    pub state: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub lease_expires_at: Option<DateTime>,
    pub attempt_count: i32,
    pub next_retry_at: DateTime,
    #[sea_orm(column_type = "Text", nullable)]
    pub last_error: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
