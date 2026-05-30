use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, JoinType, QueryFilter,
    QuerySelect, RelationTrait, sea_query::Expr,
};

use crate::{
    callisto::{channel, channel_membership},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct ChannelStorage {
    pub base: BaseStorage,
}

impl Deref for ChannelStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ChannelStorage {
    pub async fn create_channel(
        &self,
        public_id: String,
        title: Option<String>,
        owner_username: String,
        group: bool,
    ) -> Result<channel::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = channel::ActiveModel {
            id: Set(IdInstance::next_id()),
            public_id: Set(public_id),
            title: Set(title),
            last_message_at: Set(now),
            owner_username: Set(owner_username),
            group: Set(group),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn list_visible_channels(
        &self,
        username: &str,
    ) -> Result<Vec<channel::Model>, MegaError> {
        let models = channel::Entity::find()
            .join(
                JoinType::InnerJoin,
                channel::Relation::ChannelMembership.def(),
            )
            .filter(channel_membership::Column::Username.eq(username))
            .filter(channel::Column::DiscardedAt.is_null())
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn get_channel_by_public_id(
        &self,
        public_id: &str,
        username: &str,
    ) -> Result<Option<channel::Model>, MegaError> {
        let model = channel::Entity::find()
            .join(
                JoinType::InnerJoin,
                channel::Relation::ChannelMembership.def(),
            )
            .filter(channel::Column::PublicId.eq(public_id))
            .filter(channel_membership::Column::Username.eq(username))
            .filter(channel::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn update_channel(
        &self,
        public_id: &str,
        username: &str,
        title: Option<String>,
        image_path: Option<String>,
    ) -> Result<channel::Model, MegaError> {
        // Enforce membership for update (per row-level access)
        let model = channel::Entity::find()
            .join(
                JoinType::InnerJoin,
                channel::Relation::ChannelMembership.def(),
            )
            .filter(channel::Column::PublicId.eq(public_id))
            .filter(channel_membership::Column::Username.eq(username))
            .filter(channel::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Channel {public_id} not found or no access"))
            })?;

        let mut active_model: channel::ActiveModel = model.into();
        if let Some(t) = title {
            active_model.title = Set(Some(t));
        }
        if let Some(i) = image_path {
            active_model.image_path = Set(Some(i));
        }
        active_model.updated_at = Set(Utc::now().naive_utc());
        let updated = active_model.update(self.get_connection()).await?;
        Ok(updated)
    }

    pub async fn soft_delete_channel(
        &self,
        public_id: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();

        // Only owner can delete? For v1 allow any member (or tighten to owner later)
        let model = channel::Entity::find()
            .join(
                JoinType::InnerJoin,
                channel::Relation::ChannelMembership.def(),
            )
            .filter(channel::Column::PublicId.eq(public_id))
            .filter(channel_membership::Column::Username.eq(username))
            .filter(channel::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;

        if let Some(model) = model {
            let mut active_model: channel::ActiveModel = model.into();
            active_model.discarded_at = Set(Some(now));
            active_model.update(self.get_connection()).await?;
        }

        Ok(())
    }

    pub async fn get_channel_by_id(&self, id: i64) -> Result<Option<channel::Model>, MegaError> {
        let model = channel::Entity::find()
            .filter(channel::Column::Id.eq(id))
            .filter(channel::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn increment_members_count(
        &self,
        channel_id: i64,
        delta: i32,
    ) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();
        // Read current then update (simple, avoids expr for count)
        if let Some(ch) = self.get_channel_by_id(channel_id).await? {
            let mut am: channel::ActiveModel = ch.into();
            let current = match am.members_count {
                Set(v) => v,
                _ => 0,
            };
            am.members_count = Set((current + delta).max(0));
            am.updated_at = Set(now);
            am.update(self.get_connection()).await?;
        }
        Ok(())
    }

    pub async fn set_latest_message(
        &self,
        channel_id: i64,
        message_id: Option<i64>,
        last_message_at: chrono::NaiveDateTime,
    ) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();
        channel::Entity::update_many()
            .filter(channel::Column::Id.eq(channel_id))
            .col_expr(channel::Column::LatestMessageId, Expr::value(message_id))
            .col_expr(channel::Column::LastMessageAt, Expr::value(last_message_at))
            .col_expr(channel::Column::UpdatedAt, Expr::value(now))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }
}
