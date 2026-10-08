//! Durable MST/2 GC operation log (spec 10 §6 crash replay).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_retention_gc_op")]
pub struct Model {
    /// Caller supplied idempotency key. A retry of one physical GC step must
    /// address the same row, so replay cannot decrement an edge twice.
    #[sea_orm(primary_key, auto_increment = false)]
    pub operation_id: String,
    pub node_id: String,
    /// `MARK_DELETING` or `REMOVE`.
    pub operation: String,
    /// `PENDING`, `APPLIED`, or `FAILED`; workers may replay `PENDING`.
    pub state: String,
    pub attempts: i32,
    pub created_at: DateTimeWithTimeZone,
    pub completed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
