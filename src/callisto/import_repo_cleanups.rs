//! ImportRepo cleanup ledger (plan-20260923 ADR-FU-09 item 6): one row per
//! detach, keyed by the detach `push_queue` row id (`cleanup_id`), and the
//! durable resume cursor for the out-of-lock sweep.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum CleanupState {
    #[sea_orm(string_value = "detached")]
    Detached,
    #[sea_orm(string_value = "swept")]
    Swept,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "import_repo_cleanups")]
pub struct Model {
    /// The `cleanup_id`: the detach `push_queue` row id, not a generated id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub path: String,
    pub repo_id: i64,
    pub state: CleanupState,
    #[sea_orm(column_type = "Text")]
    pub requester: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub rows_deleted: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
    pub swept_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
