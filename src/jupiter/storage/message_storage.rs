use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, sea_query::Expr,
};

use crate::{
    callisto::{channel, message},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct MessageStorage {
    pub base: BaseStorage,
}

impl Deref for MessageStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl MessageStorage {
    pub async fn create_message(
        &self,
        channel_id: i64,
        sender_username: Option<String>,
        content: String,
        public_id: String,
        reply_to_id: Option<i64>,
    ) -> Result<message::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = message::ActiveModel {
            id: Set(IdInstance::next_id()),
            channel_id: Set(channel_id),
            sender_username: Set(sender_username),
            content: Set(content),
            public_id: Set(public_id),
            reply_to_id: Set(reply_to_id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    /// List messages by channel_id. Caller MUST have verified the user is a member
    /// (e.g. via ChannelStorage::get_channel_by_public_id returning Some).
    pub async fn get_messages_by_channel_id(
        &self,
        channel_id: i64,
        limit: u64,
        offset: u64,
    ) -> Result<Vec<message::Model>, MegaError> {
        let models = message::Entity::find()
            .filter(message::Column::ChannelId.eq(channel_id))
            .filter(message::Column::DiscardedAt.is_null())
            .order_by_desc(message::Column::Id)
            .limit(limit)
            .offset(offset)
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn get_message_by_public_id(
        &self,
        public_id: &str,
    ) -> Result<Option<message::Model>, MegaError> {
        let model = message::Entity::find()
            .filter(message::Column::PublicId.eq(public_id))
            .filter(message::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn get_message_by_id(&self, id: i64) -> Result<Option<message::Model>, MegaError> {
        let model = message::Entity::find()
            .filter(message::Column::Id.eq(id))
            .filter(message::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    /// Update message content (actor check is service responsibility).
    pub async fn update_message_content(
        &self,
        public_id: &str,
        content: String,
    ) -> Result<message::Model, MegaError> {
        let model = message::Entity::find()
            .filter(message::Column::PublicId.eq(public_id))
            .one(self.get_connection())
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {public_id} not found")))?;

        let mut active_model: message::ActiveModel = model.into();
        active_model.content = Set(content);
        active_model.updated_at = Set(Utc::now().naive_utc());
        let updated = active_model.update(self.get_connection()).await?;
        Ok(updated)
    }

    pub async fn soft_delete_message(&self, public_id: &str) -> Result<message::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let model = message::Entity::find()
            .filter(message::Column::PublicId.eq(public_id))
            .one(self.get_connection())
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {public_id} not found")))?;

        let mut active_model: message::ActiveModel = model.into();
        active_model.discarded_at = Set(Some(now));
        let updated = active_model.update(self.get_connection()).await?;
        Ok(updated)
    }

    pub async fn get_latest_message(
        &self,
        channel_id: i64,
    ) -> Result<Option<message::Model>, MegaError> {
        let model = message::Entity::find()
            .filter(message::Column::ChannelId.eq(channel_id))
            .filter(message::Column::DiscardedAt.is_null())
            .order_by_desc(message::Column::Id)
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    /// Recompute channel.latest_message_id and last_message_at from non-discarded messages.
    /// Called after deleting the previous latest message.
    pub async fn recompute_latest_for_channel(&self, channel_id: i64) -> Result<(), MegaError> {
        let latest = self.get_latest_message(channel_id).await?;
        let now = Utc::now().naive_utc();

        if let Some(latest_msg) = latest {
            // Update channel pointer
            channel::Entity::update_many()
                .filter(channel::Column::Id.eq(channel_id))
                .col_expr(channel::Column::LatestMessageId, Expr::value(latest_msg.id))
                .col_expr(
                    channel::Column::LastMessageAt,
                    Expr::value(latest_msg.created_at),
                )
                .col_expr(channel::Column::UpdatedAt, Expr::value(now))
                .exec(self.get_connection())
                .await?;
        } else {
            // No messages left
            channel::Entity::update_many()
                .filter(channel::Column::Id.eq(channel_id))
                .col_expr(
                    channel::Column::LatestMessageId,
                    Expr::value::<Option<i64>>(None),
                )
                .col_expr(channel::Column::LastMessageAt, Expr::value(now))
                .col_expr(channel::Column::UpdatedAt, Expr::value(now))
                .exec(self.get_connection())
                .await?;
        }
        Ok(())
    }
}
