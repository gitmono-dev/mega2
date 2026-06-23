use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr,
};

use crate::{
    callisto::channel_membership,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct ChannelMembershipStorage {
    pub base: BaseStorage,
}

impl Deref for ChannelMembershipStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ChannelMembershipStorage {
    pub async fn add_member(
        &self,
        channel_id: i64,
        username: String,
    ) -> Result<channel_membership::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = channel_membership::ActiveModel {
            id: Set(IdInstance::next_id()),
            channel_id: Set(channel_id),
            username: Set(username),
            last_read_at: Set(now),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn list_members(
        &self,
        channel_id: i64,
    ) -> Result<Vec<channel_membership::Model>, MegaError> {
        let models = channel_membership::Entity::find()
            .filter(channel_membership::Column::ChannelId.eq(channel_id))
            .all(self.get_connection())
            .await?;
        Ok(models)
    }

    pub async fn mark_read(&self, channel_id: i64, username: &str) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();
        channel_membership::Entity::update_many()
            .filter(channel_membership::Column::ChannelId.eq(channel_id))
            .filter(channel_membership::Column::Username.eq(username))
            .col_expr(channel_membership::Column::LastReadAt, Expr::value(now))
            .col_expr(channel_membership::Column::UpdatedAt, Expr::value(now))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn mark_unread(
        &self,
        channel_id: i64,
        username: &str,
        latest_message_at: Option<chrono::NaiveDateTime>,
    ) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();
        // Per spec: set manually_marked_unread_at AND move last_read_at before the
        // latest message so the channel appears unread (Rails-compatible behavior).
        let mut update = channel_membership::Entity::update_many()
            .filter(channel_membership::Column::ChannelId.eq(channel_id))
            .filter(channel_membership::Column::Username.eq(username))
            .col_expr(
                channel_membership::Column::ManuallyMarkedUnreadAt,
                Expr::value(now),
            )
            .col_expr(channel_membership::Column::UpdatedAt, Expr::value(now));

        if let Some(latest_at) = latest_message_at {
            let unread_before = latest_at - chrono::Duration::seconds(1);
            update = update.col_expr(
                channel_membership::Column::LastReadAt,
                Expr::value(unread_before),
            );
        }

        update.exec(self.get_connection()).await?;
        Ok(())
    }

    pub async fn remove_member(&self, channel_id: i64, username: &str) -> Result<(), MegaError> {
        channel_membership::Entity::delete_many()
            .filter(channel_membership::Column::ChannelId.eq(channel_id))
            .filter(channel_membership::Column::Username.eq(username))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn get_membership(
        &self,
        channel_id: i64,
        username: &str,
    ) -> Result<Option<channel_membership::Model>, MegaError> {
        let model = channel_membership::Entity::find()
            .filter(channel_membership::Column::ChannelId.eq(channel_id))
            .filter(channel_membership::Column::Username.eq(username))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }
}
