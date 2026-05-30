use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "channel_memberships")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub channel_id: i64,
    pub username: String,
    pub last_read_at: DateTime,
    pub manually_marked_unread_at: Option<DateTime>,
    pub notification_level: i32,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::callisto::channel::Entity",
        from = "crate::callisto::channel_membership::Column::ChannelId",
        to = "crate::callisto::channel::Column::Id"
    )]
    Channel,
}

impl Related<crate::callisto::channel::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Channel.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
