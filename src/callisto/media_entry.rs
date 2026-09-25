//! Per-chunk Media index row (plan-20260913 MF-08).
//!
//! Derived from sealed pages; rebuildable from immutable page objects.
//! Keys always include `scope_digest` + `manifest_id`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "media_entry")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub scope_digest: String,
    pub manifest_id: String,
    pub page_no: i32,
    /// Ordinal within the page (0-based).
    pub ordinal: i32,
    #[sea_orm(column_type = "BigInteger")]
    pub offset: i64,
    #[sea_orm(column_type = "BigInteger")]
    pub length: i64,
    pub chunk_hash: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
