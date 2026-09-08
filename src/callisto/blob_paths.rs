//! TP-13: blob appearance pairs (`blob_id`, `path`) with a row-level
//! `indexed_push_id` watermark (ADR-TP-11). Composite PK `(blob_id, path)`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "blob_paths")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub blob_id: String,
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub path: String,
    pub indexed_push_id: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
