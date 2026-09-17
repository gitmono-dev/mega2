//! MST/2 publication receipt (spec 09 §7, T05).
//!
//! One row per committed namespace publication, written in the same DB
//! transaction as the ref CAS that made it visible (trunk push today).
//! The natural key is `operation_id`: a retried push lands here once and
//! the same receipt row is reused — replay never advances the sequence
//! again. The per-namespace sequence lives in `mst2_namespace_seq` and is
//! bumped with `UPDATE ... RETURNING` inside the caller's transaction, so
//! the sequence, the ref update and the outbox row commit atomically.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_publication")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// Deterministic writer-side operation id (e.g. push old→new).
    pub operation_id: String,
    /// Namespace path this publication advances (e.g. "/").
    pub namespace: String,
    /// Monotonic per-namespace sequence assigned at commit.
    #[sea_orm(column_type = "BigInteger")]
    pub sequence: i64,
    pub old_oid: String,
    pub new_oid: String,
    #[sea_orm(column_type = "BigInteger")]
    pub writer_epoch: i64,
    /// `trunk_push` | `web_edit` | `import` | … (spec 09 §5 writer matrix).
    pub writer_kind: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
