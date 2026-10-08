use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_snapshot_lease")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub lease_id: String,
    pub snapshot_id: String,
    pub authorization_epoch: i64,
    pub publication_sequence: i64,
    pub writer_epoch: i64,
    pub certificate_receipt_id: i64,
    pub expires_at_unix: i64,
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
