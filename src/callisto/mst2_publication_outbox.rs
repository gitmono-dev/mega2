//! MST/2 publication outbox (spec 09 §3 step G, T05).
//!
//! One row per committed publication, appended in the same DB transaction
//! as the receipt and the ref CAS. Dispatch is idempotent: a worker marks
//! `dispatched_at` and may resend, but the published state never rolls
//! back because a notification failed (spec 09 §8).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_publication_outbox")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub operation_id: String,
    pub namespace: String,
    #[sea_orm(column_type = "BigInteger")]
    pub sequence: i64,
    /// `PENDING` until the dispatcher acknowledges delivery.
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
