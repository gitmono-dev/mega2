use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_native_publication")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub receipt_id: i64,
    pub namespace: String,
    pub instance_id: String,
    pub sequence: i64,
    pub writer_epoch: i64,
    pub old_root_commit: String,
    pub old_root_tree: String,
    pub root_commit: String,
    pub root_tree: String,
    pub origin_path: String,
    pub origin_ref: String,
    pub old_path_commit: Option<String>,
    pub old_path_tree: Option<String>,
    pub path_commit: String,
    pub path_tree: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
