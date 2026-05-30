use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "channels")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub public_id: String,
    pub title: Option<String>,
    pub last_message_at: DateTime,
    pub latest_message_id: Option<i64>,
    pub members_count: i32,
    pub image_path: Option<String>,
    pub group: bool,
    pub notification_forced_at: Option<DateTime>,
    pub owner_username: String,
    pub discarded_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::callisto::channel_membership::Entity")]
    ChannelMembership,
}

impl Related<crate::callisto::channel_membership::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ChannelMembership.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
