use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_snapshot_context")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub snapshot_id: String,
    pub canonical_descriptor: Vec<u8>,
    pub instance_id: String,
    pub commit_oid: String,
    pub root_tree_oid: String,
    pub metadata_root: Vec<u8>,
    pub prepare_id: String,
    pub publication_sequence: i64,
    pub writer_epoch: i64,
    pub certificate_receipt_id: Option<i64>,
    pub authorization_epoch: i64,
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
