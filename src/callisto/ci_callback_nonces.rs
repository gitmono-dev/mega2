//! Persistent V2 CI callback nonce replay table (`CLB-10`).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "ci_callback_nonces")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub nonce: String,
    #[sea_orm(column_type = "Text")]
    pub cl_link: String,
    #[sea_orm(column_type = "Text")]
    pub body_sha256: String,
    pub expires_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
