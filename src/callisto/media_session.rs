//! Media pending/sealed/finalized session row (plan-20260913 MF-08).
//!
//! Natural key: `(scope_digest, manifest_id)`. All paging indexes and tasks
//! are isolated by the same pair; nothing is loaded as a full-file Vec.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "media_session")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// SHA-256 hex of actor||0x00||canonical repo (MediaScope::digest).
    pub scope_digest: String,
    pub manifest_id: String,
    pub algorithm: String,
    pub oid: String,
    #[sea_orm(column_type = "BigInteger")]
    pub size: i64,
    #[sea_orm(column_type = "BigInteger")]
    pub chunk_count: i64,
    pub page_count: i32,
    /// `pending` | `sealed` | `finalized`
    pub state: String,
    /// Bumped on successful seal; missing-cursor binds to this generation.
    pub seal_generation: i64,
    /// Bounded created_by JSON/text (≤4096 bytes); not part of canonical id.
    pub created_by: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
    /// Pending session activity expiry (not a whole-file processing deadline).
    pub expires_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
