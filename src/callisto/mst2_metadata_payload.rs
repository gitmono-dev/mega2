use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_metadata_payload")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub page_id: Vec<u8>,
    pub metadata_codec: i16,
    pub byte_size: i32,
    pub payload: Vec<u8>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
