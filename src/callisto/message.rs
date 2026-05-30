use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "messages")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub channel_id: i64,
    pub sender_username: Option<String>,
    pub content: String,
    pub public_id: String,
    pub reply_to_id: Option<i64>,
    pub unfurled_link: Option<String>,
    pub discarded_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::callisto::channel::Entity",
        from = "crate::callisto::message::Column::ChannelId",
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
