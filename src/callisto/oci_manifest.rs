//! OCI manifest metadata keyed by repository and digest.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "oci_manifest")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub repo_name: String,
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub digest: String,
    #[sea_orm(column_type = "Text")]
    pub media_type: String,
    pub size: i64,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
