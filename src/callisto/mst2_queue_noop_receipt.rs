//! Committed trunk-queue no-op results, separate from visible publications.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_queue_noop_receipt")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub operation_id: String,
    #[sea_orm(column_type = "Text")]
    pub namespace: String,
    #[sea_orm(column_type = "Text")]
    pub request_digest: String,
    pub request_digest_version: i32,
    pub writer_epoch: i64,
    #[sea_orm(column_type = "Text")]
    pub writer_kind: String,
    pub observed_sequence: i64,
    #[sea_orm(column_type = "Text")]
    pub observed_root_commit: String,
    #[sea_orm(column_type = "Text")]
    pub observed_root_tree: String,
    #[sea_orm(column_type = "Text")]
    pub landed_commit_id: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
