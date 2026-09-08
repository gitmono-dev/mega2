//! TP-09: `mega_ref_tombstones` (trunk-push.md 2.5). Composite PK `(path, ref_name)`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mega_ref_tombstones")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub path: String,
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub ref_name: String,
    #[sea_orm(column_type = "Text")]
    pub last_commit_hash: String,
    #[sea_orm(column_type = "Text")]
    pub last_tree_hash: String,
    pub deleted_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
