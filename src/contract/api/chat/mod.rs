use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ChannelResponse {
    pub public_id: String,
    pub title: Option<String>,
    pub last_message_at: String,
    pub latest_message_public_id: Option<String>,
    pub members_count: i32,
    pub image_path: Option<String>,
    pub group: bool,
    pub owner_username: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct MessageResponse {
    pub public_id: String,
    pub channel_public_id: String,
    pub sender_username: Option<String>,
    pub content: String,
    pub reply_to_public_id: Option<String>,
    pub unfurled_link: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub attachments: Vec<AttachmentResponse>,
    pub reactions: Vec<ReactionResponse>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct AttachmentResponse {
    pub public_id: String,
    pub file_path: String,
    pub file_type: String,
    pub name: String,
    pub size: i64,
    pub position: i32,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ReactionResponse {
    pub public_id: String,
    pub content: Option<String>,
    pub username: String,
    pub custom_reaction_public_id: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct CreateChannelReq {
    pub title: Option<String>,
    pub image_path: Option<String>,
    pub member_usernames: Vec<String>,
    pub group: bool,
    pub initial_message: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct UpdateChannelReq {
    pub title: Option<String>,
    pub image_path: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SendMessageReq {
    pub content: String,
    pub reply_to_public_id: Option<String>,
    pub attachments: Option<Vec<AttachmentConfirmReq>>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct UpdateMessageReq {
    pub content: String,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct CreateReactionReq {
    pub content: Option<String>,
    pub custom_reaction_public_id: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct AttachmentPresignReq {
    pub file_name: String,
    pub file_size: i64,
    pub file_type: String,
    pub channel_public_id: String,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct AttachmentPresignRes {
    pub upload_url: String,
    pub file_path: String,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct AttachmentConfirmReq {
    pub file_path: String,
    pub file_type: String,
    pub file_name: String,
    pub file_size: i64,
}

#[derive(Deserialize, ToSchema, Clone, Debug)]
pub struct AddChannelMembersReq {
    pub usernames: Vec<String>,
}

#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct ChannelMemberResponse {
    pub username: String,
    pub last_read_at: String,
    pub notification_level: i32,
    pub joined_at: String,
}
