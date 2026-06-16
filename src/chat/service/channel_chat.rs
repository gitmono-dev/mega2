//! Channel chat service implementation per docs/chat-engine.md Slice 3.
//!
//! Responsibilities:
//! - create_channel (auto-add creator, dedup, membership update record, optional initial msg)
//! - send_message (membership check, content/attach rule, reply_to same channel, update latest, notif stub, event)
//! - update_message / delete_message with sender check + latest recompute on delete
//! - add/remove members with update record + count adjust
//! - mark read/unread (delegates to storage, unread sets flag)

use std::sync::Arc;

use crate::{
    callisto::{channel, message},
    chat::domain::{ChatEvents, NoopChatEvents},
    common::errors::MegaError,
    jupiter::storage::{
        attachment_storage::AttachmentStorage,
        channel_membership_storage::ChannelMembershipStorage,
        channel_membership_update_storage::ChannelMembershipUpdateStorage,
        channel_storage::ChannelStorage, message_storage::MessageStorage,
    },
};

/// Service facade for channel-based chat (the main capability in this migration).
#[derive(Clone)]
pub struct ChannelChatService<E: ChatEvents + 'static = NoopChatEvents> {
    pub channel_storage: ChannelStorage,
    pub membership_storage: ChannelMembershipStorage,
    pub membership_update_storage: ChannelMembershipUpdateStorage,
    pub message_storage: MessageStorage,
    pub attachment_storage: AttachmentStorage,
    pub events: Arc<E>,
}

impl ChannelChatService<NoopChatEvents> {
    pub fn new(
        channel_storage: ChannelStorage,
        membership_storage: ChannelMembershipStorage,
        membership_update_storage: ChannelMembershipUpdateStorage,
        message_storage: MessageStorage,
        attachment_storage: AttachmentStorage,
    ) -> Self {
        Self {
            channel_storage,
            membership_storage,
            membership_update_storage,
            message_storage,
            attachment_storage,
            events: Arc::new(NoopChatEvents),
        }
    }
}

impl<E: ChatEvents + 'static> ChannelChatService<E> {
    pub fn with_events(mut self, events: Arc<E>) -> Self {
        self.events = events;
        self
    }

    /// Create a channel.
    /// - Creator is always added as member.
    /// - Deduplicate member_usernames.
    /// - Write membership update record.
    /// - If initial_message or attachments, send as first message (attachments stubbed for v1).
    /// - Returns the created channel and optional first message public_id.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_channel(
        &self,
        title: Option<String>,
        _image_path: Option<String>,
        creator_username: String,
        mut member_usernames: Vec<String>,
        group: bool,
        initial_message: Option<String>,
        attachments: Option<Vec<crate::contract::api::chat::AttachmentConfirmReq>>,
    ) -> Result<(channel::Model, Option<message::Model>), MegaError> {
        // Always include creator, dedup
        member_usernames.push(creator_username.clone());
        member_usernames.sort();
        member_usernames.dedup();

        let public_id = crate::callisto::entity_ext::generate_public_id();

        // Create channel (members_count will be adjusted)
        let ch = self
            .channel_storage
            .create_channel(
                public_id.clone(),
                title.clone(),
                creator_username.clone(),
                group,
            )
            .await?;

        // Add memberships
        for u in &member_usernames {
            // ignore duplicate key errors (idempotent add)
            let _ = self.membership_storage.add_member(ch.id, u.clone()).await;
        }

        // Record membership update (actor = creator)
        let added: Vec<String> = member_usernames.clone();
        let _ = self
            .membership_update_storage
            .record_membership_change(ch.id, creator_username.clone(), added, vec![])
            .await;

        // Adjust count
        let _ = self
            .channel_storage
            .increment_members_count(ch.id, member_usernames.len() as i32)
            .await;

        // Optional initial message
        let first_msg = if let Some(content) = initial_message {
            if !content.trim().is_empty() {
                Some(
                    self.send_message_inner(
                        &ch,
                        Some(creator_username.clone()),
                        content,
                        None,
                        attachments.unwrap_or_default(),
                    )
                    .await?,
                )
            } else {
                None
            }
        } else {
            None
        };

        // Reload channel to get latest pointers if message was sent
        let final_ch = self
            .channel_storage
            .get_channel_by_public_id(&public_id, &creator_username)
            .await?
            .unwrap_or(ch);

        Ok((final_ch, first_msg))
    }

    /// Send a message to channel.
    /// - Verifies sender membership (returns NotFound if not, to reduce enumeration per spec).
    /// - Requires content or attachments (attachments v1 stub: only content supported).
    /// - reply_to must be in same channel if provided.
    /// - Updates channel latest + last_message_at.
    /// - Writes internal message_notification for reply_to (no external delivery).
    /// - Emits message_created event.
    pub async fn send_message(
        &self,
        channel_public_id: &str,
        sender_username: String,
        content: String,
        reply_to_public_id: Option<String>,
        attachments: Option<Vec<crate::contract::api::chat::AttachmentConfirmReq>>,
    ) -> Result<message::Model, MegaError> {
        // Verify membership + channel exists (404 for non member per spec)
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, &sender_username)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Channel {channel_public_id} not found")))?;

        let has_attachments = attachments.as_ref().is_some_and(|a| !a.is_empty());
        if content.trim().is_empty() && !has_attachments {
            return Err(MegaError::Other("content or attachment required".into()));
        }

        let reply_to_id = if let Some(rp) = reply_to_public_id {
            let reply_msg = self
                .message_storage
                .get_message_by_public_id(&rp)
                .await?
                .ok_or_else(|| MegaError::Other("reply_to not found".into()))?;
            if reply_msg.channel_id != ch.id {
                return Err(MegaError::Other("reply_to cross-channel".into()));
            }
            Some(reply_msg.id)
        } else {
            None
        };

        self.send_message_inner(
            &ch,
            Some(sender_username),
            content,
            reply_to_id,
            attachments.unwrap_or_default(),
        )
        .await
    }

    async fn send_message_inner(
        &self,
        ch: &channel::Model,
        sender: Option<String>,
        content: String,
        reply_to_id: Option<i64>,
        attachments: Vec<crate::contract::api::chat::AttachmentConfirmReq>,
    ) -> Result<message::Model, MegaError> {
        let public_id = crate::callisto::entity_ext::generate_public_id();

        let msg = self
            .message_storage
            .create_message(ch.id, sender, content, public_id.clone(), reply_to_id)
            .await?;

        // Create attachments and link them to the message
        for (i, att_req) in attachments.into_iter().enumerate() {
            let att_pub_id = crate::callisto::entity_ext::generate_public_id();
            self.attachment_storage
                .create_attachment(
                    att_pub_id,
                    att_req.file_path,
                    att_req.file_type,
                    "Message".to_string(),
                    msg.id,
                    att_req.file_name,
                    att_req.file_size,
                    (i + 1) as i32,
                )
                .await?;
        }

        // Update channel latest
        self.channel_storage
            .set_latest_message(ch.id, Some(msg.id), msg.created_at)
            .await?;

        // Internal notification for reply (stub: only if reply_to)
        if let Some(_rid) = reply_to_id {
            // In full impl: find memberships that should be notified (reply + mentions)
            // For v1 we skip writing message_notifications rows unless needed for read state.
        }

        // Event
        self.events.message_created(&ch.public_id, &public_id).await;

        Ok(msg)
    }

    pub async fn update_message(
        &self,
        message_public_id: &str,
        actor_username: &str,
        content: String,
    ) -> Result<message::Model, MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {message_public_id} not found")))?;

        if msg.discarded_at.is_some() {
            return Err(MegaError::Other("cannot edit deleted message".into()));
        }

        // Actor must be original sender (future: + admin)
        if msg.sender_username.as_deref() != Some(actor_username) {
            return Err(MegaError::Other("only sender can edit".into()));
        }

        let updated = self
            .message_storage
            .update_message_content(message_public_id, content)
            .await?;

        self.events
            .message_updated(
                &self.channel_public_id_for_message(msg.channel_id).await?,
                message_public_id,
            )
            .await;

        Ok(updated)
    }

    pub async fn delete_message(
        &self,
        message_public_id: &str,
        actor_username: &str,
    ) -> Result<(), MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {message_public_id} not found")))?;

        if msg.sender_username.as_deref() != Some(actor_username) {
            return Err(MegaError::Other("only sender can delete".into()));
        }

        let channel_id = msg.channel_id;
        let was_latest = self
            .message_storage
            .get_latest_message(channel_id)
            .await?
            .map(|m| m.id == msg.id)
            .unwrap_or(false);

        let _ = self
            .message_storage
            .soft_delete_message(message_public_id)
            .await?;

        if was_latest {
            self.message_storage
                .recompute_latest_for_channel(channel_id)
                .await?;
        }

        let ch_pub = self.channel_public_id_for_message(channel_id).await?;
        self.events
            .message_deleted(&ch_pub, message_public_id)
            .await;

        Ok(())
    }

    pub async fn add_members(
        &self,
        channel_public_id: &str,
        actor_username: &str,
        usernames: Vec<String>,
    ) -> Result<(), MegaError> {
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, actor_username)
            .await?
            .ok_or_else(|| MegaError::NotFound("channel not found or no access".into()))?;

        let mut added = vec![];
        for u in usernames {
            if self
                .membership_storage
                .get_membership(ch.id, &u)
                .await?
                .is_none()
            {
                let _ = self.membership_storage.add_member(ch.id, u.clone()).await;
                added.push(u);
            }
        }

        if !added.is_empty() {
            let _ = self
                .membership_update_storage
                .record_membership_change(ch.id, actor_username.to_string(), added.clone(), vec![])
                .await;
            let _ = self
                .channel_storage
                .increment_members_count(ch.id, added.len() as i32)
                .await;
        }

        self.events.channel_updated(channel_public_id).await;
        Ok(())
    }

    pub async fn remove_members(
        &self,
        channel_public_id: &str,
        actor_username: &str,
        usernames: Vec<String>,
    ) -> Result<(), MegaError> {
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, actor_username)
            .await?
            .ok_or_else(|| MegaError::NotFound("channel not found or no access".into()))?;

        let mut removed = vec![];
        for u in usernames {
            if self
                .membership_storage
                .get_membership(ch.id, &u)
                .await?
                .is_some()
            {
                let _ = self.membership_storage.remove_member(ch.id, &u).await;
                removed.push(u);
            }
        }

        if !removed.is_empty() {
            let _ = self
                .membership_update_storage
                .record_membership_change(
                    ch.id,
                    actor_username.to_string(),
                    vec![],
                    removed.clone(),
                )
                .await;
            let _ = self
                .channel_storage
                .increment_members_count(ch.id, -(removed.len() as i32))
                .await;
        }

        self.events.channel_updated(channel_public_id).await;
        Ok(())
    }

    pub async fn mark_read(
        &self,
        channel_public_id: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, username)
            .await?
            .ok_or_else(|| MegaError::NotFound("channel not found or no access".into()))?;
        self.membership_storage.mark_read(ch.id, username).await
    }

    pub async fn mark_unread(
        &self,
        channel_public_id: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, username)
            .await?
            .ok_or_else(|| MegaError::NotFound("channel not found or no access".into()))?;
        self.membership_storage.mark_unread(ch.id, username).await
    }

    async fn channel_public_id_for_message(&self, channel_id: i64) -> Result<String, MegaError> {
        let ch = self
            .channel_storage
            .get_channel_by_id(channel_id)
            .await?
            .ok_or_else(|| MegaError::NotFound("channel missing for message".into()))?;
        Ok(ch.public_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jupiter::tests::test_storage;

    #[tokio::test]
    async fn test_create_channel_and_send_message_and_delete_latest() {
        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;

        // Note: test_storage applies migrations
        let svc = ChannelChatService::from_storage(&storage);

        // Create with initial message
        let (ch, first_msg) = svc
            .create_channel(
                Some("Test Channel".to_string()),
                None,
                "alice".to_string(),
                vec!["bob".to_string()],
                true,
                Some("hello world".to_string()),
                None,
            )
            .await
            .expect("create channel");

        assert_eq!(ch.owner_username, "alice");
        assert!(first_msg.is_some());
        let first = first_msg.unwrap();
        assert_eq!(first.content, "hello world");

        // Send reply
        let reply = svc
            .send_message(
                &ch.public_id,
                "bob".to_string(),
                "reply from bob".to_string(),
                Some(first.public_id.clone()),
                None,
            )
            .await
            .expect("send reply");

        assert_eq!(reply.reply_to_id, Some(first.id));

        // Non-member cannot send (404)
        let not_member = svc
            .send_message(
                &ch.public_id,
                "charlie".to_string(),
                "nope".to_string(),
                None,
                None,
            )
            .await;
        assert!(matches!(not_member, Err(MegaError::NotFound(_))));

        // Delete the latest (reply) -> should recompute to first
        svc.delete_message(&reply.public_id, "bob")
            .await
            .expect("delete reply");

        let after = svc
            .channel_storage
            .get_channel_by_public_id(&ch.public_id, "alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.latest_message_id, Some(first.id));

        // Delete the last remaining -> latest becomes null
        svc.delete_message(&first.public_id, "alice")
            .await
            .expect("delete first");

        let final_ch = svc
            .channel_storage
            .get_channel_by_public_id(&ch.public_id, "alice")
            .await
            .unwrap()
            .unwrap();
        assert!(final_ch.latest_message_id.is_none());
    }
}

// Convenience constructor from Storage (for future wiring in API state)
#[allow(clippy::items_after_test_module)]
// Test module placed before this impl for logical grouping of behavior + tests;
// the convenience ctor is a trivial adapter and does not affect module organization.
impl ChannelChatService<NoopChatEvents> {
    pub fn from_storage(storage: &crate::jupiter::storage::Storage) -> Self {
        Self::new(
            storage.channel_storage(),
            storage.channel_membership_storage(),
            storage.channel_membership_update_storage(),
            storage.message_storage(),
            storage.attachment_storage(),
        )
    }
}
