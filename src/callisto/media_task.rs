//! Async Media finalize task + lease (plan-20260913 MF-08 / P-04b).
//!
//! One task per `(scope_digest, manifest_id)`. Claim/renew/complete CAS on
//! `lease_epoch`; stale workers cannot publish.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "media_task")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub task_id: String,
    pub scope_digest: String,
    pub manifest_id: String,
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub expires_at: Option<DateTimeWithTimeZone>,
    /// `pending` | `running` | `complete` | `failed`
    pub state: String,
    #[sea_orm(column_type = "BigInteger")]
    pub bytes_verified: i64,
    pub pages_verified: i32,
    pub retryable: bool,
    pub error_code: Option<String>,
    /// Crash-recovery stage label (e.g. `verify`, `fallback`, `publish`).
    pub stage: String,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
