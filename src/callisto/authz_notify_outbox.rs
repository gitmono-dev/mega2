//! TP-22: persistent authz notify outbox (trunk-push 1.2).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "authz_notify_outbox")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `push_queue.id` for B3-enqueued rebuilds; `None` for queue-external dirty.
    pub version: Option<i64>,
    /// Queue-external delete path: no monotonic version compare.
    pub dirty: bool,
    pub created_at: DateTimeWithTimeZone,
    pub replayed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
