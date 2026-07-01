use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, JoinType, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait, sea_query::Expr,
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
        image_path: Option<String>,
        owner_username: String,
        group: bool,
    ) -> Result<channel::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = channel::ActiveModel {
            id: Set(IdInstance::next_id()),
            public_id: Set(public_id),
            title: Set(title),
            image_path: Set(image_path),
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
            // Most-recently-active channels first (source MessageThread#index order:
            // last_message_at desc, id desc). id is the deterministic tie-breaker.
            .order_by_desc(channel::Column::LastMessageAt)
            .order_by_desc(channel::Column::Id)
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

#[cfg(test)]
mod tests {
    use crate::{callisto::entity_ext::generate_public_id, jupiter::tests::test_storage};

    #[tokio::test]
    async fn channel_visibility_is_membership_scoped() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let channel_storage = storage.channel_storage();
        let membership_storage = storage.channel_membership_storage();

        let channel = channel_storage
            .create_channel(
                generate_public_id(),
                Some("members only".to_string()),
                None,
                "alice".to_string(),
                true,
            )
            .await
            .expect("create channel");
        membership_storage
            .add_member(channel.id, "alice".to_string())
            .await
            .expect("add alice");

        let alice_channels = channel_storage
            .list_visible_channels("alice")
            .await
            .expect("list alice channels");
        assert_eq!(alice_channels.len(), 1);
        assert_eq!(alice_channels[0].id, channel.id);

        let bob_channels = channel_storage
            .list_visible_channels("bob")
            .await
            .expect("list bob channels");
        assert!(bob_channels.is_empty());

        let bob_lookup = channel_storage
            .get_channel_by_public_id(&channel.public_id, "bob")
            .await
            .expect("bob lookup");
        assert!(bob_lookup.is_none());
    }

    #[tokio::test]
    async fn list_visible_channels_orders_by_last_message_then_id_desc() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let channel_storage = storage.channel_storage();
        let membership_storage = storage.channel_membership_storage();

        // Four channels for alice; creation order fixes ascending ids.
        let mut ids = Vec::new();
        for title in ["a", "b", "c", "d"] {
            let ch = channel_storage
                .create_channel(
                    generate_public_id(),
                    Some(title.to_string()),
                    None,
                    "alice".to_string(),
                    true,
                )
                .await
                .expect("create channel");
            membership_storage
                .add_member(ch.id, "alice".to_string())
                .await
                .expect("add alice");
            ids.push(ch.id);
        }
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);

        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .expect("valid timestamp")
            .naive_utc();
        let t_old = base;
        let t_mid = base + chrono::Duration::seconds(10);
        let t_new = base + chrono::Duration::seconds(20);

        // a -> mid, b -> new, c -> old, d -> new (ties with b at the newest instant).
        channel_storage
            .set_latest_message(a, None, t_mid)
            .await
            .expect("set a");
        channel_storage
            .set_latest_message(b, None, t_new)
            .await
            .expect("set b");
        channel_storage
            .set_latest_message(c, None, t_old)
            .await
            .expect("set c");
        channel_storage
            .set_latest_message(d, None, t_new)
            .await
            .expect("set d");

        let listed: Vec<i64> = channel_storage
            .list_visible_channels("alice")
            .await
            .expect("list channels")
            .into_iter()
            .map(|ch| ch.id)
            .collect();

        // Expected: last_message_at DESC, then id DESC. The newest instant holds
        // {b, d}, which must come first ordered by id DESC, then mid (a), then old (c).
        let mut newest = [b, d];
        newest.sort_by(|x, y| y.cmp(x));
        assert_eq!(listed, vec![newest[0], newest[1], a, c]);
    }

    #[tokio::test]
    async fn channel_update_and_delete_require_visible_membership() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let channel_storage = storage.channel_storage();
        let membership_storage = storage.channel_membership_storage();

        let channel = channel_storage
            .create_channel(
                generate_public_id(),
                Some("editable".to_string()),
                None,
                "alice".to_string(),
                true,
            )
            .await
            .expect("create channel");
        membership_storage
            .add_member(channel.id, "alice".to_string())
            .await
            .expect("add alice");

        let bob_update = channel_storage
            .update_channel(&channel.public_id, "bob", Some("nope".to_string()), None)
            .await;
        assert!(
            bob_update.is_err(),
            "non-member update should not find the channel"
        );

        let updated = channel_storage
            .update_channel(
                &channel.public_id,
                "alice",
                Some("updated".to_string()),
                None,
            )
            .await
            .expect("member update");
        assert_eq!(updated.title.as_deref(), Some("updated"));

        channel_storage
            .soft_delete_channel(&channel.public_id, "alice")
            .await
            .expect("member soft delete");

        let deleted_lookup = channel_storage
            .get_channel_by_public_id(&channel.public_id, "alice")
            .await
            .expect("lookup deleted channel");
        assert!(deleted_lookup.is_none());
    }
}
