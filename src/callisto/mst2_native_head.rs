use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_native_head")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub namespace: String,
    pub instance_id: String,
    pub sequence: i64,
    pub writer_epoch: i64,
    pub root_commit: String,
    pub root_tree: String,
    pub state: String,
    pub certificate_receipt_id: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
