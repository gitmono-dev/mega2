use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use orbit_api::object_storage::{ObjectKey, ObjectNamespace};
use reqwest::Method;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::CHAT_TAG, oauth::model::LoginUser},
    chat::service::channel_chat::extract_mentioned_usernames,
    common::errors::ApiError,
    contract::api::{
        chat::{
            AttachmentConfirmReq, AttachmentPresignReq, AttachmentPresignRes, AttachmentResponse,
            ChannelResponse, CreateChannelReq, CreateReactionReq, MessageResponse,
            ReactionResponse, SendMessageReq, UpdateChannelReq, UpdateMessageReq,
        },
        common::{CommonResult, Pagination},
    },
};

const CHAT_ATTACHMENT_MAX_FILE_SIZE: i64 = 100 * 1024 * 1024;
const CHAT_ATTACHMENT_MAX_FILE_NAME_LEN: usize = 255;
const CHAT_ATTACHMENT_MAX_FILE_TYPE_LEN: usize = 128;
const CHAT_ATTACHMENT_KEY_PREFIX: &str = "chat/attachments/";

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/chat",
        OpenApiRouter::new()
            .routes(routes!(list_channels))
            .routes(routes!(create_channel))
            .routes(routes!(get_channel_detail))
            .routes(routes!(update_channel))
            .routes(routes!(delete_channel))
            .routes(routes!(list_messages))
            .routes(routes!(send_message))
            .routes(routes!(edit_message))
            .routes(routes!(delete_message))
            .routes(routes!(create_reaction))
            .routes(routes!(delete_reaction))
            .routes(routes!(presign_attachment))
            .routes(routes!(confirm_attachment))
            .routes(routes!(mark_channel_read))
            .routes(routes!(mark_channel_unread)),
    )
}

fn validate_chat_attachment_metadata(
    file_name: &str,
    file_type: &str,
    file_size: i64,
    allowed_mime_types: &[String],
) -> Result<(String, String), ApiError> {
    let file_name = file_name.trim();
    if file_name.is_empty()
        || file_name.len() > CHAT_ATTACHMENT_MAX_FILE_NAME_LEN
        || file_name == "."
        || file_name == ".."
        || file_name
            .chars()
            .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "invalid attachment file_name"
        )));
    }

    let file_type = file_type.trim();
    if file_type.is_empty()
        || file_type.len() > CHAT_ATTACHMENT_MAX_FILE_TYPE_LEN
        || file_type.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "invalid attachment file_type"
        )));
    }

    let mime_parts: Vec<&str> = file_type.split('/').collect();
    if mime_parts.len() != 2 || mime_parts[0].is_empty() || mime_parts[1].is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "invalid attachment file_type"
        )));
    }

    if !(1..=CHAT_ATTACHMENT_MAX_FILE_SIZE).contains(&file_size) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "attachment file_size must be between 1 and {} bytes",
            CHAT_ATTACHMENT_MAX_FILE_SIZE
        )));
    }

    if !allowed_mime_types.is_empty() && !is_mime_type_allowed(file_type, allowed_mime_types) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "attachment file_type `{file_type}` is not in the configured allowlist"
        )));
    }

    Ok((file_name.to_owned(), file_type.to_owned()))
}

/// Returns true if `file_type` matches one of the configured allowlist patterns.
/// Exact entries match only themselves; wildcard entries (`type/*`) match any
/// subtype of that top-level type. An empty allowlist accepts everything.
fn is_mime_type_allowed(file_type: &str, allowed_mime_types: &[String]) -> bool {
    let (top, _subtype) = file_type.split_once('/').unwrap_or((file_type, ""));
    allowed_mime_types.iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern == file_type {
            return true;
        }
        if let Some((p_top, p_sub)) = pattern.split_once('/')
            && p_top == top
            && p_sub == "*"
        {
            return true;
        }
        false
    })
}

fn validate_chat_attachment_file_path(file_path: &str) -> Result<(), ApiError> {
    if file_path.is_empty()
        || file_path.len() > 1024
        || !file_path.starts_with(CHAT_ATTACHMENT_KEY_PREFIX)
        || file_path.chars().any(|ch| ch == '\\' || ch.is_control())
        || file_path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "invalid attachment file_path"
        )));
    }
    Ok(())
}

async fn map_channel_model(
    ch: crate::callisto::channel::Model,
    state: &MonoApiServiceState,
) -> Result<ChannelResponse, ApiError> {
    let latest_message_public_id = if let Some(mid) = ch.latest_message_id {
        let m = state
            .storage
            .message_storage()
            .get_message_by_id(mid)
            .await?;
        m.map(|msg| msg.public_id)
    } else {
        None
    };

    Ok(ChannelResponse {
        public_id: ch.public_id,
        title: ch.title,
        last_message_at: ch.last_message_at.to_string(),
        latest_message_public_id,
        members_count: ch.members_count,
        image_path: ch.image_path,
        group: ch.group,
        owner_username: ch.owner_username,
        created_at: ch.created_at.to_string(),
        updated_at: ch.updated_at.to_string(),
    })
}

async fn map_message_model(
    msg: crate::callisto::message::Model,
    state: &MonoApiServiceState,
) -> Result<MessageResponse, ApiError> {
    // 1. Get reactions
    let rx_models = state
        .storage
        .reaction_storage()
        .get_reactions_by_subject("Message", msg.id)
        .await?;

    let mut reactions = Vec::new();
    for r in rx_models {
        let custom_reaction_public_id = if let Some(crid) = r.custom_reaction_id {
            let cr = state
                .storage
                .custom_reaction_storage()
                .get_custom_reaction_by_id(crid)
                .await?;
            cr.map(|c| c.public_id)
        } else {
            None
        };
        reactions.push(crate::contract::api::chat::ReactionResponse {
            public_id: r.public_id,
            content: r.content,
            username: r.username,
            custom_reaction_public_id,
        });
    }

    // 2. Get attachments
    let att_models = state
        .storage
        .attachment_storage()
        .get_attachments_by_subject("Message", msg.id)
        .await?;
    let attachments = att_models
        .into_iter()
        .map(|a| AttachmentResponse {
            public_id: a.public_id,
            file_path: a.file_path,
            file_type: a.file_type,
            name: a.name,
            size: a.size,
            position: a.position,
        })
        .collect();

    // 3. Find channel public_id
    let ch_pub_id = if let Some(ch) = state
        .storage
        .channel_storage()
        .get_channel_by_id(msg.channel_id)
        .await?
    {
        ch.public_id
    } else {
        String::new()
    };

    // 4. Find reply_to_public_id
    let reply_to_public_id = if let Some(rtid) = msg.reply_to_id {
        let rt = state
            .storage
            .message_storage()
            .get_message_by_id(rtid)
            .await?;
        rt.map(|m| m.public_id)
    } else {
        None
    };

    Ok(MessageResponse {
        public_id: msg.public_id,
        channel_public_id: ch_pub_id,
        sender_username: msg.sender_username,
        content: msg.content,
        reply_to_public_id,
        unfurled_link: msg.unfurled_link,
        created_at: msg.created_at.to_string(),
        updated_at: msg.updated_at.to_string(),
        attachments,
        reactions,
    })
}

/// Visible channels
#[utoipa::path(
    get,
    path = "/chat/channels",
    responses(
        (status = 200, body = CommonResult<Vec<ChannelResponse>>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn list_channels(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<ChannelResponse>>>, ApiError> {
    let channels = state
        .channel_chat_svc()
        .channel_storage
        .list_visible_channels(&user.username)
        .await?;

    let mut list = Vec::new();
    for ch in channels {
        list.push(map_channel_model(ch, &state).await?);
    }

    Ok(Json(CommonResult::success(Some(list))))
}

/// Create channel
#[utoipa::path(
    post,
    path = "/chat/channels",
    request_body = CreateChannelReq,
    responses(
        (status = 200, body = CommonResult<ChannelResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn create_channel(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(payload): Json<CreateChannelReq>,
) -> Result<Json<CommonResult<ChannelResponse>>, ApiError> {
    let (ch, _) = state
        .channel_chat_svc()
        .create_channel(
            payload.title,
            payload.image_path,
            user.username,
            payload.member_usernames,
            payload.group,
            payload.initial_message,
            None,
        )
        .await?;

    let mapped = map_channel_model(ch, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Get channel details
#[utoipa::path(
    get,
    path = "/chat/channels/{channel_id}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    responses(
        (status = 200, body = CommonResult<ChannelResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn get_channel_detail(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<ChannelResponse>>, ApiError> {
    let ch = state
        .channel_chat_svc()
        .channel_storage
        .get_channel_by_public_id(&channel_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    let mapped = map_channel_model(ch, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Update channel
#[utoipa::path(
    patch,
    path = "/chat/channels/{channel_id}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    request_body = UpdateChannelReq,
    responses(
        (status = 200, body = CommonResult<ChannelResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn update_channel(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<UpdateChannelReq>,
) -> Result<Json<CommonResult<ChannelResponse>>, ApiError> {
    let ch = state
        .channel_chat_svc()
        .channel_storage
        .update_channel(
            &channel_id,
            &user.username,
            payload.title,
            payload.image_path,
        )
        .await?;

    let mapped = map_channel_model(ch, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Soft delete channel
#[utoipa::path(
    delete,
    path = "/chat/channels/{channel_id}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn delete_channel(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .channel_chat_svc()
        .channel_storage
        .soft_delete_channel(&channel_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// Message list
#[utoipa::path(
    get,
    path = "/chat/channels/{channel_id}/messages",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
        Pagination,
    ),
    responses(
        (status = 200, body = CommonResult<Vec<MessageResponse>>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn list_messages(
    user: LoginUser,
    Path(channel_id): Path<String>,
    Query(pagination): Query<Pagination>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<MessageResponse>>>, ApiError> {
    // 1. Verify membership
    let ch = state
        .channel_chat_svc()
        .channel_storage
        .get_channel_by_public_id(&channel_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    // 2. Fetch messages
    let limit = pagination.per_page;
    let offset = (pagination.page.max(1) - 1) * limit;

    let messages = state
        .channel_chat_svc()
        .message_storage
        .get_messages_by_channel_id(ch.id, limit, offset)
        .await?;

    let mut list = Vec::new();
    for msg in messages {
        list.push(map_message_model(msg, &state).await?);
    }

    Ok(Json(CommonResult::success(Some(list))))
}

/// Send message
#[utoipa::path(
    post,
    path = "/chat/channels/{channel_id}/messages",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    request_body = SendMessageReq,
    responses(
        (status = 200, body = CommonResult<MessageResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn send_message(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<SendMessageReq>,
) -> Result<Json<CommonResult<MessageResponse>>, ApiError> {
    let msg = state
        .channel_chat_svc()
        .send_message(
            &channel_id,
            user.username.clone(),
            payload.content.clone(),
            payload.reply_to_public_id.clone(),
            payload.attachments,
        )
        .await?;

    let member_names: std::collections::HashSet<String> = state
        .storage
        .channel_membership_storage()
        .list_members(msg.channel_id)
        .await
        .map(|members| members.into_iter().map(|m| m.username).collect())
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                channel_id = %channel_id,
                "failed to list channel members; skipping chat notifications"
            );
            std::collections::HashSet::new()
        });

    let mentioned: Vec<String> = extract_mentioned_usernames(&payload.content)
        .into_iter()
        .filter(|name| member_names.contains(name))
        .collect();
    let mention_enqueued = if mentioned.is_empty() {
        std::collections::HashSet::new()
    } else {
        crate::notification::triggers::on_chat_mention_created(
            &state.storage.notification_storage(),
            &user.username,
            &channel_id,
            &payload.content,
            &mentioned,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to enqueue chat mention notifications");
            std::collections::HashSet::new()
        })
    };

    if let Some(reply_to_public_id) = &payload.reply_to_public_id {
        match state
            .storage
            .message_storage()
            .get_message_by_public_id(reply_to_public_id)
            .await
        {
            Ok(Some(parent)) => {
                if let Some(parent_author) = parent.sender_username
                    && parent_author != user.username
                    && member_names.contains(&parent_author)
                    && !mention_enqueued.contains(&parent_author)
                    && let Err(e) = crate::notification::triggers::on_chat_reply_created(
                        &state.storage.notification_storage(),
                        &user.username,
                        &channel_id,
                        &payload.content,
                        &parent_author,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "failed to enqueue chat reply notification");
                }
            }
            Ok(None) => {
                tracing::warn!(
                    reply_to = %reply_to_public_id,
                    "reply-to message not found; skipping reply notification"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    reply_to = %reply_to_public_id,
                    "failed to look up reply-to message; skipping reply notification"
                );
            }
        }
    }

    let mapped = map_message_model(msg, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Edit message
#[utoipa::path(
    patch,
    path = "/chat/channels/{channel_id}/messages/{message_id}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
        ("message_id" = String, Path, description = "Public ID of the message"),
    ),
    request_body = UpdateMessageReq,
    responses(
        (status = 200, body = CommonResult<MessageResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn edit_message(
    user: LoginUser,
    Path((channel_id, message_id)): Path<(String, String)>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<UpdateMessageReq>,
) -> Result<Json<CommonResult<MessageResponse>>, ApiError> {
    let msg = state
        .channel_chat_svc()
        .update_message(&channel_id, &message_id, &user.username, payload.content)
        .await?;

    let mapped = map_message_model(msg, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Soft delete message
#[utoipa::path(
    delete,
    path = "/chat/channels/{channel_id}/messages/{message_id}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
        ("message_id" = String, Path, description = "Public ID of the message"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn delete_message(
    user: LoginUser,
    Path((channel_id, message_id)): Path<(String, String)>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .channel_chat_svc()
        .delete_message(&channel_id, &message_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// Create reaction
#[utoipa::path(
    post,
    path = "/chat/messages/{message_id}/reactions",
    params(
        ("message_id" = String, Path, description = "Public ID of the message"),
    ),
    request_body = CreateReactionReq,
    responses(
        (status = 200, body = CommonResult<ReactionResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn create_reaction(
    user: LoginUser,
    Path(message_id): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<CreateReactionReq>,
) -> Result<Json<CommonResult<ReactionResponse>>, ApiError> {
    let rx = state
        .shared_chat_svc()
        .create_reaction(
            &message_id,
            user.username,
            payload.content,
            payload.custom_reaction_public_id,
        )
        .await?;

    // Map custom_reaction_public_id
    let custom_reaction_public_id = if let Some(crid) = rx.custom_reaction_id {
        let cr = state
            .storage
            .custom_reaction_storage()
            .get_custom_reaction_by_id(crid)
            .await?;
        cr.map(|c| c.public_id)
    } else {
        None
    };

    let mapped = crate::contract::api::chat::ReactionResponse {
        public_id: rx.public_id,
        content: rx.content,
        username: rx.username,
        custom_reaction_public_id,
    };

    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Delete reaction
#[utoipa::path(
    delete,
    path = "/chat/reactions/{reaction_id}",
    params(
        ("reaction_id" = String, Path, description = "Public ID of the reaction"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn delete_reaction(
    user: LoginUser,
    Path(reaction_id): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .shared_chat_svc()
        .delete_reaction(&reaction_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// Presign upload URL
#[utoipa::path(
    post,
    path = "/chat/attachments/presign",
    request_body = AttachmentPresignReq,
    responses(
        (status = 200, body = CommonResult<AttachmentPresignRes>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn presign_attachment(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(payload): Json<AttachmentPresignReq>,
) -> Result<Json<CommonResult<AttachmentPresignRes>>, ApiError> {
    let allowed_mime_types = state
        .storage
        .config()
        .chat
        .as_ref()
        .map(|c| c.attachment_allowed_mime_types.clone())
        .unwrap_or_default();
    let (file_name, _) = validate_chat_attachment_metadata(
        &payload.file_name,
        &payload.file_type,
        payload.file_size,
        &allowed_mime_types,
    )?;

    // 1. Verify membership
    let _ = state
        .channel_chat_svc()
        .channel_storage
        .get_channel_by_public_id(&payload.channel_public_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    // 2. Generate a unique key / path
    let uuid_str = uuid::Uuid::new_v4().to_string();
    let file_key = format!(
        "chat/attachments/{}/{}_{}",
        payload.channel_public_id, uuid_str, file_name
    );

    let key = ObjectKey {
        namespace: ObjectNamespace::Attachment,
        key: file_key.clone(),
    };

    // 3. Generate presigned URL
    let obj_storage = &state.storage.git_service.obj_storage;
    let upload_url = obj_storage
        .inner
        .signed_url(&key, Method::PUT, Duration::from_secs(3600))
        .await?
        .unwrap_or_else(|| format!("/api/v1/chat/attachments/upload/{}", file_key));

    Ok(Json(CommonResult::success(Some(AttachmentPresignRes {
        upload_url,
        file_path: file_key,
    }))))
}

/// Confirm attachment upload on an existing message
#[utoipa::path(
    post,
    path = "/chat/attachments",
    params(
        ("message_id" = String, Query, description = "Public ID of the message"),
    ),
    request_body = AttachmentConfirmReq,
    responses(
        (status = 200, body = CommonResult<AttachmentResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn confirm_attachment(
    user: LoginUser,
    Query(message_id): Query<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<AttachmentConfirmReq>,
) -> Result<Json<CommonResult<AttachmentResponse>>, ApiError> {
    let allowed_mime_types = state
        .storage
        .config()
        .chat
        .as_ref()
        .map(|c| c.attachment_allowed_mime_types.clone())
        .unwrap_or_default();
    let (file_name, file_type) = validate_chat_attachment_metadata(
        &payload.file_name,
        &payload.file_type,
        payload.file_size,
        &allowed_mime_types,
    )?;
    validate_chat_attachment_file_path(&payload.file_path)?;

    // Verify the uploaded object actually exists before registering it.
    let object_key = ObjectKey {
        namespace: ObjectNamespace::Attachment,
        key: payload.file_path.clone(),
    };
    let object_exists = state
        .storage
        .git_service
        .obj_storage
        .inner
        .exists(&object_key)
        .await?;
    if !object_exists {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "attachment object not found"
        )));
    }

    // Check if there are other attachments on the message to determine position
    let msg = state
        .channel_chat_svc()
        .message_storage
        .get_message_by_public_id(&message_id)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Message not found")))?;

    let existing = state
        .storage
        .attachment_storage()
        .get_attachments_by_subject("Message", msg.id)
        .await?;
    let position = (existing.len() + 1) as i32;

    let att = state
        .shared_chat_svc()
        .create_attachment(
            &message_id,
            &user.username,
            payload.file_path,
            file_type,
            file_name,
            payload.file_size,
            position,
        )
        .await?;

    let mapped = AttachmentResponse {
        public_id: att.public_id,
        file_path: att.file_path,
        file_type: att.file_type,
        name: att.name,
        size: att.size,
        position: att.position,
    };

    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Mark channel read
#[utoipa::path(
    post,
    path = "/chat/channels/{channel_id}/reads",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn mark_channel_read(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .channel_chat_svc()
        .mark_read(&channel_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// Mark channel unread
#[utoipa::path(
    delete,
    path = "/chat/channels/{channel_id}/reads",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn mark_channel_unread(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .channel_chat_svc()
        .mark_unread(&channel_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Arc};

    use bytes::Bytes;
    use futures::stream;
    use orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace};

    use super::*;
    use crate::{api::oauth::model::LoginUser, jupiter::tests::test_storage};

    #[test]
    fn chat_attachment_metadata_rejects_unsafe_inputs() {
        assert!(validate_chat_attachment_metadata("../x.txt", "text/plain", 1, &[]).is_err());
        assert!(validate_chat_attachment_metadata("x.txt", "text/plain", 0, &[]).is_err());
        assert!(validate_chat_attachment_metadata("x.txt", "not-a-mime", 1, &[]).is_err());
        assert!(validate_chat_attachment_metadata("x.txt", "text/plain", 1, &[]).is_ok());
        // Malformed MIME strings must be rejected even when an allowlist is present.
        assert!(
            validate_chat_attachment_metadata("x.png", "image/", 1, &["image/*".to_string()])
                .is_err()
        );
        assert!(
            validate_chat_attachment_metadata(
                "x.png",
                "image/png/extra",
                1,
                &["image/*".to_string()]
            )
            .is_err()
        );
    }

    #[test]
    fn chat_attachment_metadata_enforces_mime_allowlist() {
        let allowlist = vec!["image/*".to_string(), "application/pdf".to_string()];
        assert!(validate_chat_attachment_metadata("x.png", "image/png", 1, &allowlist).is_ok());
        assert!(validate_chat_attachment_metadata("x.jpg", "image/jpeg", 1, &allowlist).is_ok());
        assert!(
            validate_chat_attachment_metadata("x.pdf", "application/pdf", 1, &allowlist).is_ok()
        );
        assert!(validate_chat_attachment_metadata("x.txt", "text/plain", 1, &allowlist).is_err());
        assert!(
            validate_chat_attachment_metadata("x.exe", "application/x-msdownload", 1, &allowlist)
                .is_err()
        );
    }

    #[test]
    fn chat_attachment_file_path_requires_chat_attachment_key() {
        assert!(
            validate_chat_attachment_file_path("chat/attachments/channel/uuid_report.txt").is_ok()
        );
        assert!(validate_chat_attachment_file_path("../report.txt").is_err());
        assert!(
            validate_chat_attachment_file_path("chat/attachments/channel/../report.txt").is_err()
        );
        assert!(validate_chat_attachment_file_path("other/attachments/report.txt").is_err());
    }

    async fn setup_test_state(temp_dir: &std::path::Path) -> Option<MonoApiServiceState> {
        let storage = test_storage(temp_dir).await;
        let redis_url = &storage.config().redis.url;
        let client = ::redis::Client::open(redis_url.as_str()).ok()?;
        let redis_conn = crate::jupiter::redis::ConnectionManager::new(client)
            .await
            .ok()?;

        let git_object_cache = Arc::new(crate::ceres::api_service::cache::GitObjectCache {
            connection: redis_conn,
            prefix: "git-object-rkyv:v1".to_string(),
        });
        Some(MonoApiServiceState {
            storage: storage.clone(),
            listen_addr: "http://localhost:8000".to_string(),
            entity_store: crate::contract::policy::entitystore::EntityStore::new(),
            git_object_cache,
            bellatrix: Arc::new(crate::bellatrix::Bellatrix::new(
                storage.config().build.clone(),
            )),
        })
    }

    #[tokio::test]
    async fn test_chat_router_handlers_lifecycle() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let Some(state) = setup_test_state(temp_dir.path()).await else {
            println!("Skipping chat_router integration tests because Redis is not available.");
            return;
        };

        let alice = LoginUser {
            campsite_user_id: "user-alice".to_string(),
            username: "alice".to_string(),
            avatar_url: "".to_string(),
            email: "alice@example.com".to_string(),
        };

        let bob = LoginUser {
            campsite_user_id: "user-bob".to_string(),
            username: "bob".to_string(),
            avatar_url: "".to_string(),
            email: "bob@example.com".to_string(),
        };

        // 1. Create channel
        let req = CreateChannelReq {
            title: Some("General Channel".to_string()),
            image_path: None,
            member_usernames: vec!["bob".to_string()],
            group: true,
            initial_message: Some("Welcome to General!".to_string()),
        };
        let res = create_channel(alice.clone(), State(state.clone()), Json(req))
            .await
            .expect("failed to create channel")
            .0;
        assert!(res.req_result);
        let ch = res.data.unwrap();
        assert_eq!(ch.title.as_deref(), Some("General Channel"));
        assert_eq!(ch.owner_username, "alice");

        let other_res = create_channel(
            alice.clone(),
            State(state.clone()),
            Json(CreateChannelReq {
                title: Some("Other Channel".to_string()),
                image_path: None,
                member_usernames: vec!["bob".to_string()],
                group: true,
                initial_message: None,
            }),
        )
        .await
        .expect("failed to create second channel")
        .0;
        let other_ch = other_res.data.unwrap();

        // 2. List visible channels for alice
        let list_res = list_channels(alice.clone(), State(state.clone()))
            .await
            .expect("failed to list channels")
            .0;
        assert_eq!(list_res.data.unwrap().len(), 1);

        // List for non-member (e.g. charlie) should be empty
        let charlie = LoginUser {
            campsite_user_id: "user-charlie".to_string(),
            username: "charlie".to_string(),
            avatar_url: "".to_string(),
            email: "charlie@example.com".to_string(),
        };
        let list_res_charlie = list_channels(charlie.clone(), State(state.clone()))
            .await
            .expect("failed to list channels for charlie")
            .0;
        assert_eq!(list_res_charlie.data.unwrap().len(), 0);

        // 3. Get channel detail
        let ch_detail = get_channel_detail(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to get channel detail")
        .0;
        assert_eq!(ch_detail.data.unwrap().public_id, ch.public_id);

        // 4. Update channel
        let update_req = UpdateChannelReq {
            title: Some("New General Title".to_string()),
            image_path: Some("path/to/image.png".to_string()),
        };
        let updated = update_channel(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
            Json(update_req),
        )
        .await
        .expect("failed to update channel")
        .0;
        let updated_ch = updated.data.unwrap();
        assert_eq!(updated_ch.title.as_deref(), Some("New General Title"));
        assert_eq!(updated_ch.image_path.as_deref(), Some("path/to/image.png"));

        // 5. Send message
        let send_req = SendMessageReq {
            content: "Hello from Bob".to_string(),
            reply_to_public_id: None,
            attachments: None,
        };
        let sent_msg = send_message(
            bob.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
            Json(send_req),
        )
        .await
        .expect("failed to send message")
        .0
        .data
        .unwrap();
        assert_eq!(sent_msg.content, "Hello from Bob");
        assert_eq!(sent_msg.sender_username.as_deref(), Some("bob"));

        // 6. List messages
        let messages = list_messages(
            alice.clone(),
            Path(ch.public_id.clone()),
            Query(Pagination {
                page: 1,
                per_page: 10,
            }),
            State(state.clone()),
        )
        .await
        .expect("failed to list messages")
        .0
        .data
        .unwrap();
        // Should have initial message + Hello from Bob
        assert_eq!(messages.len(), 2);

        // 7. Edit message
        let edit_req = UpdateMessageReq {
            content: "Hello from Bob (Edited)".to_string(),
        };
        let wrong_channel_edit = edit_message(
            bob.clone(),
            Path((other_ch.public_id.clone(), sent_msg.public_id.clone())),
            State(state.clone()),
            Json(UpdateMessageReq {
                content: "wrong channel edit".to_string(),
            }),
        )
        .await;
        assert!(wrong_channel_edit.is_err());

        let edited_msg = edit_message(
            bob.clone(),
            Path((ch.public_id.clone(), sent_msg.public_id.clone())),
            State(state.clone()),
            Json(edit_req),
        )
        .await
        .expect("failed to edit message")
        .0
        .data
        .unwrap();
        assert_eq!(edited_msg.content, "Hello from Bob (Edited)");

        // Non-sender (alice) cannot edit
        let edit_fail = edit_message(
            alice.clone(),
            Path((ch.public_id.clone(), sent_msg.public_id.clone())),
            State(state.clone()),
            Json(UpdateMessageReq {
                content: "alice hack".to_string(),
            }),
        )
        .await;
        assert!(edit_fail.is_err());

        // 8. Create and delete reaction
        let react_req = CreateReactionReq {
            content: Some("🎉".to_string()),
            custom_reaction_public_id: None,
        };
        let rx = create_reaction(
            alice.clone(),
            Path(sent_msg.public_id.clone()),
            State(state.clone()),
            Json(react_req),
        )
        .await
        .expect("failed to create reaction")
        .0
        .data
        .unwrap();
        assert_eq!(rx.content.as_deref(), Some("🎉"));
        assert_eq!(rx.username, "alice");

        // Delete reaction
        let del_rx = delete_reaction(alice.clone(), Path(rx.public_id), State(state.clone()))
            .await
            .expect("failed to delete reaction")
            .0;
        assert!(del_rx.req_result);

        // 9. Presign attachment
        let presign_req = AttachmentPresignReq {
            file_name: "test.pdf".to_string(),
            file_size: 1000,
            file_type: "application/pdf".to_string(),
            channel_public_id: ch.public_id.clone(),
        };
        let presign_res =
            presign_attachment(alice.clone(), State(state.clone()), Json(presign_req))
                .await
                .expect("failed to presign attachment")
                .0
                .data
                .unwrap();
        assert!(!presign_res.upload_url.is_empty());
        assert!(presign_res.file_path.contains("test.pdf"));

        // Confirm without uploading the object should fail.
        let confirm_req = AttachmentConfirmReq {
            file_path: presign_res.file_path.clone(),
            file_type: "application/pdf".to_string(),
            file_name: "test.pdf".to_string(),
            file_size: 1000,
        };
        let missing_object_confirm = confirm_attachment(
            alice.clone(),
            Query(sent_msg.public_id.clone()),
            State(state.clone()),
            Json(confirm_req.clone()),
        )
        .await;
        assert!(missing_object_confirm.is_err());

        // Upload the object so the confirmation can verify ownership/existence.
        let object_key = ObjectKey {
            namespace: ObjectNamespace::Attachment,
            key: presign_res.file_path.clone(),
        };
        let data: Vec<Result<Bytes, io::Error>> = vec![Ok(Bytes::from_static(b"attachment-bytes"))];
        state
            .storage
            .git_service
            .obj_storage
            .inner
            .put_stream(
                &object_key,
                Box::pin(stream::iter(data)),
                ObjectMeta {
                    size: 16,
                    ..Default::default()
                },
            )
            .await
            .expect("failed to put attachment object");

        // 10. Confirm/register attachment
        let confirmed_att = confirm_attachment(
            alice.clone(),
            Query(sent_msg.public_id.clone()),
            State(state.clone()),
            Json(confirm_req),
        )
        .await
        .expect("failed to confirm attachment")
        .0
        .data
        .unwrap();
        assert_eq!(confirmed_att.name, "test.pdf");

        // 11. Mark read and unread
        let read_res = mark_channel_read(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to mark read")
        .0;
        assert!(read_res.req_result);

        let unread_res = mark_channel_unread(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to mark unread")
        .0;
        assert!(unread_res.req_result);

        // 12. Soft delete message
        let wrong_channel_delete = delete_message(
            bob.clone(),
            Path((other_ch.public_id, sent_msg.public_id.clone())),
            State(state.clone()),
        )
        .await;
        assert!(wrong_channel_delete.is_err());

        let del_msg_res = delete_message(
            bob.clone(),
            Path((ch.public_id.clone(), sent_msg.public_id.clone())),
            State(state.clone()),
        )
        .await
        .expect("failed to delete message")
        .0;
        assert!(del_msg_res.req_result);

        // 13. Soft delete channel
        let del_ch_res = delete_channel(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to delete channel")
        .0;
        assert!(del_ch_res.req_result);
    }
}
