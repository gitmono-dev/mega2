//! Channel chat service implementation per docs/chat-engine.md Slice 3.
//!
//! Responsibilities:
//! - create_channel (auto-add creator, dedup, membership update record, optional initial msg)
//! - send_message (membership check, content/attach rule, reply_to same channel, update latest, notif stub, event)
//! - update_message / delete_message with sender check + latest recompute on delete
//! - add/remove members with update record + count adjust
//! - mark read/unread (delegates to storage, unread sets flag)

use std::{collections::HashSet, sync::Arc};

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

fn is_mention_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

pub(crate) fn extract_mentioned_usernames(content: &str) -> HashSet<String> {
    let bytes = content.as_bytes();
    let mut mentions = HashSet::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] != b'@' || (cursor > 0 && is_mention_char(bytes[cursor.saturating_sub(1)]))
        {
            cursor += 1;
            continue;
        }

        let start = cursor + 1;
        let mut end = start;
        while end < bytes.len() && is_mention_char(bytes[end]) {
            end += 1;
        }
        if end > start
            && let Ok(username) = std::str::from_utf8(&bytes[start..end])
        {
            mentions.insert(username.to_owned());
        }
        cursor = end.max(cursor + 1);
    }

    mentions
}

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
    pub fn with_events<NextE: ChatEvents + 'static>(
        self,
        events: Arc<NextE>,
    ) -> ChannelChatService<NextE> {
        ChannelChatService {
            channel_storage: self.channel_storage,
            membership_storage: self.membership_storage,
            membership_update_storage: self.membership_update_storage,
            message_storage: self.message_storage,
            attachment_storage: self.attachment_storage,
            events,
        }
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
        image_path: Option<String>,
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
                image_path.clone(),
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

        let sender_username = sender.clone();
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

        let mut notification_recipients = extract_mentioned_usernames(&msg.content);
        if let Some(sender) = &sender_username {
            notification_recipients.remove(sender);
        }

        if let Some(reply_message_id) = reply_to_id
            && let Some(reply_message) = self
                .message_storage
                .get_message_by_id(reply_message_id)
                .await?
            && let Some(recipient) = reply_message.sender_username
            && sender_username.as_deref() != Some(recipient.as_str())
        {
            notification_recipients.insert(recipient);
        }

        if !notification_recipients.is_empty() {
            for membership in self.membership_storage.list_members(ch.id).await? {
                if notification_recipients.contains(&membership.username) {
                    self.message_storage
                        .create_message_notification(membership.id, msg.id)
                        .await?;
                }
            }
        }

        // Event
        self.events.message_created(&ch.public_id, &public_id).await;

        Ok(msg)
    }

    pub async fn update_message(
        &self,
        channel_public_id: &str,
        message_public_id: &str,
        actor_username: &str,
        content: String,
    ) -> Result<message::Model, MegaError> {
        let (msg, ch) = self
            .message_for_channel_actor(message_public_id, channel_public_id, actor_username)
            .await?;

        // Actor must be original sender (future: + admin)
        if msg.sender_username.as_deref() != Some(actor_username) {
            return Err(MegaError::Other("only sender can edit".into()));
        }

        let updated = self
            .message_storage
            .update_message_content(message_public_id, content)
            .await?;

        self.events
            .message_updated(&ch.public_id, message_public_id)
            .await;

        Ok(updated)
    }

    pub async fn delete_message(
        &self,
        channel_public_id: &str,
        message_public_id: &str,
        actor_username: &str,
    ) -> Result<(), MegaError> {
        let (msg, ch) = self
            .message_for_channel_actor(message_public_id, channel_public_id, actor_username)
            .await?;

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

        self.events
            .message_deleted(&ch.public_id, message_public_id)
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
        // Only adjust last_read_at when there is an actual latest message.
        let latest_at = ch.latest_message_id.map(|_| ch.last_message_at);
        self.membership_storage
            .mark_unread(ch.id, username, latest_at)
            .await
    }

    async fn message_for_channel_actor(
        &self,
        message_public_id: &str,
        channel_public_id: &str,
        actor_username: &str,
    ) -> Result<(message::Model, channel::Model), MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {message_public_id} not found")))?;
        let ch = self
            .channel_storage
            .get_channel_by_public_id(channel_public_id, actor_username)
            .await?
            .ok_or_else(|| MegaError::NotFound(format!("Message {message_public_id} not found")))?;
        if msg.channel_id != ch.id {
            return Err(MegaError::NotFound(format!(
                "Message {message_public_id} not found"
            )));
        }
        Ok((msg, ch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::domain::{ChatEvent, InMemoryChatEvents},
        jupiter::tests::test_storage,
    };

    #[tokio::test]
    async fn test_create_channel_and_send_message_and_delete_latest() {
        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;

        // Note: test_storage applies migrations
        let svc = ChannelChatService::from_storage(&storage);

        // Create with initial message and an image path
        let (ch, first_msg) = svc
            .create_channel(
                Some("Test Channel".to_string()),
                Some("avatars/test-channel.png".to_string()),
                "alice".to_string(),
                vec!["bob".to_string()],
                true,
                Some("hello world".to_string()),
                None,
            )
            .await
            .expect("create channel");

        assert_eq!(ch.owner_username, "alice");
        // image_path must be persisted at creation time, not silently dropped.
        assert_eq!(ch.image_path.as_deref(), Some("avatars/test-channel.png"));
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
        let alice_membership = svc
            .membership_storage
            .get_membership(ch.id, "alice")
            .await
            .expect("query alice membership")
            .expect("alice membership");
        let reply_notifications = svc
            .message_storage
            .get_message_notifications_by_message_id(reply.id)
            .await
            .expect("query reply notifications");
        assert_eq!(reply_notifications.len(), 1);
        assert_eq!(
            reply_notifications[0].channel_membership_id,
            alice_membership.id
        );

        let mention = svc
            .send_message(
                &ch.public_id,
                "bob".to_string(),
                "ping @alice @bob @charlie".to_string(),
                None,
                None,
            )
            .await
            .expect("send mention");
        let mention_notifications = svc
            .message_storage
            .get_message_notifications_by_message_id(mention.id)
            .await
            .expect("query mention notifications");
        assert_eq!(mention_notifications.len(), 1);
        assert_eq!(
            mention_notifications[0].channel_membership_id,
            alice_membership.id
        );
        svc.delete_message(&ch.public_id, &mention.public_id, "bob")
            .await
            .expect("delete mention");

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
        svc.remove_members(&ch.public_id, "alice", vec!["bob".to_string()])
            .await
            .expect("remove bob");

        let removed_member_delete = svc
            .delete_message(&ch.public_id, &reply.public_id, "bob")
            .await;
        assert!(matches!(removed_member_delete, Err(MegaError::NotFound(_))));

        svc.add_members(&ch.public_id, "alice", vec!["bob".to_string()])
            .await
            .expect("re-add bob");

        svc.delete_message(&ch.public_id, &reply.public_id, "bob")
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
        svc.delete_message(&ch.public_id, &first.public_id, "alice")
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

    #[tokio::test]
    async fn mark_unread_adjusts_last_read_at_before_latest_message() {
        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let svc = ChannelChatService::from_storage(&storage);

        let (ch, first_msg) = svc
            .create_channel(
                Some("Unread Test".to_string()),
                None,
                "alice".to_string(),
                vec![],
                true,
                Some("hello".to_string()),
                None,
            )
            .await
            .expect("create channel");
        let _ = first_msg.expect("initial message");

        // Mark read first to establish a known last_read_at.
        svc.mark_read(&ch.public_id, "alice")
            .await
            .expect("mark read");

        let before = svc
            .membership_storage
            .get_membership(ch.id, "alice")
            .await
            .expect("query membership")
            .expect("alice membership");

        // Mark unread — should move last_read_at before the latest message.
        svc.mark_unread(&ch.public_id, "alice")
            .await
            .expect("mark unread");

        let after = svc
            .membership_storage
            .get_membership(ch.id, "alice")
            .await
            .expect("query membership")
            .expect("alice membership");

        assert!(after.manually_marked_unread_at.is_some());
        // last_read_at must have moved backward (before the latest message).
        assert!(
            after.last_read_at < before.last_read_at,
            "last_read_at should be before the latest message after mark_unread"
        );
        assert!(
            after.last_read_at < ch.last_message_at,
            "last_read_at should be before channel's last_message_at"
        );
    }

    #[test]
    fn extracts_mentions_on_token_boundaries() {
        let mentions = extract_mentioned_usernames("hi @alice, email a@b no @bob-smith @carol.dev");

        assert!(mentions.contains("alice"));
        assert!(mentions.contains("bob-smith"));
        assert!(mentions.contains("carol.dev"));
        assert!(!mentions.contains("b"));
    }

    #[tokio::test]
    async fn in_memory_chat_events_broadcasts_service_mutations() {
        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let events = Arc::new(InMemoryChatEvents::default());
        let mut receiver = events.subscribe();
        let svc = ChannelChatService::from_storage(&storage).with_events(events);

        let (ch, first_msg) = svc
            .create_channel(
                Some("Events".to_string()),
                None,
                "alice".to_string(),
                vec!["bob".to_string()],
                true,
                Some("hello".to_string()),
                None,
            )
            .await
            .expect("create channel");
        let first_msg = first_msg.expect("initial message");

        assert_eq!(
            receiver.recv().await.expect("message created event"),
            ChatEvent::MessageCreated {
                channel_public_id: ch.public_id.clone(),
                message_public_id: first_msg.public_id.clone(),
            }
        );

        svc.update_message(
            &ch.public_id,
            &first_msg.public_id,
            "alice",
            "edited".to_string(),
        )
        .await
        .expect("update message");
        assert_eq!(
            receiver.recv().await.expect("message updated event"),
            ChatEvent::MessageUpdated {
                channel_public_id: ch.public_id.clone(),
                message_public_id: first_msg.public_id.clone(),
            }
        );

        svc.delete_message(&ch.public_id, &first_msg.public_id, "alice")
            .await
            .expect("delete message");
        assert_eq!(
            receiver.recv().await.expect("message deleted event"),
            ChatEvent::MessageDeleted {
                channel_public_id: ch.public_id.clone(),
                message_public_id: first_msg.public_id,
            }
        );

        svc.add_members(&ch.public_id, "alice", vec!["carol".to_string()])
            .await
            .expect("add member");
        assert_eq!(
            receiver.recv().await.expect("channel updated event"),
            ChatEvent::ChannelUpdated {
                channel_public_id: ch.public_id,
            }
        );
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
