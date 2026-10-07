use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "mst2_metadata_prepare")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub prepare_id: String,
    pub operation_id: String,
    pub manifest_digest: Vec<u8>,
    pub canonical_plan: Vec<u8>,
    pub canonical_bindings: Option<Vec<u8>>,
    pub bindings_digest: Option<Vec<u8>>,
    pub primary_scope: Option<Vec<u8>>,
    pub storage_seal: Option<Vec<u8>>,
    pub source_domain: String,
    pub tagged_root_tree_oid: String,
    pub scope: String,
    pub schema_version: i16,
    pub metadata_codec: i16,
    pub materialization_policy: i16,
    pub fs_semantics: i16,
    pub access_projection: i16,
    pub verification_revision: i32,
    pub projection_revision: i16,
    pub metadata_root: Vec<u8>,
    pub node_count: i32,
    pub edge_count: i32,
    pub total_bytes: i64,
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
    pub committed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
