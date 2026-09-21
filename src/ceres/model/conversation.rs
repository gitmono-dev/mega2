use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::callisto::{mega_conversation, sea_orm_active_enums::ConvTypeEnum};

#[derive(Serialize, ToSchema)]
pub struct ConversationItem {
    pub id: i64,
    pub username: String,
    pub conv_type: ConvType,
    pub comment: Option<String>,
    pub resolved: Option<bool>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ConversationItem {
    pub fn from_model(conversation: mega_conversation::Model) -> Self {
        Self {
            id: conversation.id,
            username: conversation.username,
            conv_type: conversation.conv_type.into(),
            comment: conversation.comment,
            resolved: conversation.resolved,
            created_at: conversation.created_at.and_utc().timestamp(),
            updated_at: conversation.updated_at.and_utc().timestamp(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct ContentPayload {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum ConvType {
    Comment,
    Deploy,
    Commit,
    ForcePush,
    Edit,
    Review,
    Approve,
    MergeQueue,
    Merged,
    Closed,
    Reopen,
    Label,
    Assignee,
    Mention,
    Draft,
}

impl From<ConvTypeEnum> for ConvType {
    fn from(value: ConvTypeEnum) -> Self {
        match value {
            ConvTypeEnum::Comment => ConvType::Comment,
            ConvTypeEnum::Deploy => ConvType::Deploy,
            ConvTypeEnum::Commit => ConvType::Commit,
            ConvTypeEnum::ForcePush => ConvType::ForcePush,
            ConvTypeEnum::Edit => ConvType::Edit,
            ConvTypeEnum::Review => ConvType::Review,
            ConvTypeEnum::Approve => ConvType::Approve,
            ConvTypeEnum::MergeQueue => ConvType::MergeQueue,
            ConvTypeEnum::Merged => ConvType::Merged,
            ConvTypeEnum::Closed => ConvType::Closed,
            ConvTypeEnum::Reopen => ConvType::Reopen,
            ConvTypeEnum::Label => ConvType::Label,
            ConvTypeEnum::Assignee => ConvType::Assignee,
            ConvTypeEnum::Mention => ConvType::Mention,
            ConvTypeEnum::Draft => ConvType::Draft,
        }
    }
}
