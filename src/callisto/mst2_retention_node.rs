//! MST/2 retention graph nodes (spec 10 §6).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_retention_node")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub node_id: String,
    pub kind: String,
    pub state: String,
    #[sea_orm(column_type = "BigInteger")]
    pub bytes: i64,
    /// Number of unique incoming retention edges. Root coverage is kept in
    /// `mst2_retention_root` and checked in the same transaction.
    #[sea_orm(column_type = "BigInteger")]
    pub incoming_refs: i64,
    pub created_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
