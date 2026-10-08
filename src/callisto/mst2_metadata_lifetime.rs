use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_metadata_lifetime")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub page_id: Vec<u8>,
    pub node_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub generation: i64,
    pub state: String,
    pub metadata_codec: i16,
    pub expected_size: i32,
    pub graph_domain: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
