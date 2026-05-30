use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use crate::{
    callisto::channel_membership_update,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct ChannelMembershipUpdateStorage {
    pub base: BaseStorage,
}

impl Deref for ChannelMembershipUpdateStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ChannelMembershipUpdateStorage {
    pub async fn record_membership_change(
        &self,
        channel_id: i64,
        actor_username: String,
        added: Vec<String>,
        removed: Vec<String>,
    ) -> Result<channel_membership_update::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = channel_membership_update::ActiveModel {
            id: Set(IdInstance::next_id()),
            channel_id: Set(channel_id),
            actor_username: Set(actor_username),
            added_usernames: Set(json!(added)),
            removed_usernames: Set(json!(removed)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn list_updates_for_channel(
        &self,
        channel_id: i64,
    ) -> Result<Vec<channel_membership_update::Model>, MegaError> {
        let models = channel_membership_update::Entity::find()
            .filter(channel_membership_update::Column::ChannelId.eq(channel_id))
            .filter(channel_membership_update::Column::DiscardedAt.is_null())
            .all(self.get_connection())
            .await?;
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_membership_update_smoke() {
        // Smoke: struct can be constructed in test context (full integration in service tests)
        let _ = ChannelMembershipUpdateStorage {
            base: crate::jupiter::storage::base_storage::BaseStorage::mock(),
        };
    }
}
