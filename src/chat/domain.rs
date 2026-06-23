use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatEntityKind {
    Channel,
    Message,
    Attachment,
    Reaction,
    CustomReaction,
    OpenGraphLink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatEntityRef {
    pub kind: ChatEntityKind,
    pub public_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatCapability {
    SharedFoundations,
    ChannelChat,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChatMigrationSlice {
    pub capability: ChatCapability,
    pub source_components: &'static [&'static str],
    pub primary_tables: &'static [&'static str],
    pub rust_target: &'static str,
}

pub const MIGRATION_SLICES: &[ChatMigrationSlice] = &[
    ChatMigrationSlice {
        capability: ChatCapability::SharedFoundations,
        source_components: &[
            "api/app/models/attachment.rb",
            "api/app/models/reaction.rb",
            "api/app/models/custom_reaction.rb",
            "api/app/models/open_graph_link.rb",
        ],
        primary_tables: &[
            "attachments",
            "reactions",
            "custom_reactions",
            "open_graph_links",
        ],
        rust_target: "src/callisto/{attachment,reaction,custom_reaction,open_graph_link}.rs + src/jupiter/storage/{attachment,reaction,custom_reaction,open_graph}_storage.rs + src/chat/service/shared.rs",
    },
    ChatMigrationSlice {
        capability: ChatCapability::ChannelChat,
        source_components: &[
            "api/app/models/message_thread.rb",
            "api/app/models/message.rs",
            "api/app/models/message_thread_membership.rb",
            "api/app/models/message_thread_membership_update.rb",
            "api/app/models/message_notification.rb",
            "api/app/controllers/api/v1/message_threads_controller.rb",
        ],
        primary_tables: &[
            "channels",
            "channel_memberships",
            "channel_membership_updates",
            "messages",
            "message_notifications",
        ],
        rust_target: "src/callisto/{channel,channel_membership,channel_membership_update,message,message_notification}.rs + src/jupiter/storage/{channel,channel_membership,message}_storage.rs + src/chat/service/channel_chat.rs",
    },
];

use async_trait::async_trait;

/// Internal event broadcaster for chat mutations (Slice 3).
/// WebSocket/Pusher compatibility is a later independent slice and must not block CRUD.
#[async_trait]
pub trait ChatEvents: Send + Sync {
    async fn message_created(&self, channel_public_id: &str, message_public_id: &str);
    async fn message_updated(&self, channel_public_id: &str, message_public_id: &str);
    async fn message_deleted(&self, channel_public_id: &str, message_public_id: &str);
    async fn channel_updated(&self, channel_public_id: &str);
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatEvent {
    MessageCreated {
        channel_public_id: String,
        message_public_id: String,
    },
    MessageUpdated {
        channel_public_id: String,
        message_public_id: String,
    },
    MessageDeleted {
        channel_public_id: String,
        message_public_id: String,
    },
    ChannelUpdated {
        channel_public_id: String,
    },
}

/// No-op default implementation.
#[derive(Clone, Default)]
pub struct NoopChatEvents;

#[async_trait]
impl ChatEvents for NoopChatEvents {
    async fn message_created(&self, _channel_public_id: &str, _message_public_id: &str) {}
    async fn message_updated(&self, _channel_public_id: &str, _message_public_id: &str) {}
    async fn message_deleted(&self, _channel_public_id: &str, _message_public_id: &str) {}
    async fn channel_updated(&self, _channel_public_id: &str) {}
}

#[derive(Clone)]
pub struct InMemoryChatEvents {
    sender: broadcast::Sender<ChatEvent>,
}

impl Default for InMemoryChatEvents {
    fn default() -> Self {
        Self::new(128)
    }
}

impl InMemoryChatEvents {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.sender.subscribe()
    }

    fn publish(&self, event: ChatEvent) {
        let _ = self.sender.send(event);
    }
}

#[async_trait]
impl ChatEvents for InMemoryChatEvents {
    async fn message_created(&self, channel_public_id: &str, message_public_id: &str) {
        self.publish(ChatEvent::MessageCreated {
            channel_public_id: channel_public_id.to_owned(),
            message_public_id: message_public_id.to_owned(),
        });
    }

    async fn message_updated(&self, channel_public_id: &str, message_public_id: &str) {
        self.publish(ChatEvent::MessageUpdated {
            channel_public_id: channel_public_id.to_owned(),
            message_public_id: message_public_id.to_owned(),
        });
    }

    async fn message_deleted(&self, channel_public_id: &str, message_public_id: &str) {
        self.publish(ChatEvent::MessageDeleted {
            channel_public_id: channel_public_id.to_owned(),
            message_public_id: message_public_id.to_owned(),
        });
    }

    async fn channel_updated(&self, channel_public_id: &str) {
        self.publish(ChatEvent::ChannelUpdated {
            channel_public_id: channel_public_id.to_owned(),
        });
    }
}
