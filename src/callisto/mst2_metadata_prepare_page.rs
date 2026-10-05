use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_metadata_prepare_page")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub prepare_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub page_id: Vec<u8>,
    pub expected_size: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
