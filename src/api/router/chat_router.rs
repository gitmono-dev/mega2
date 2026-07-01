use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{
        FromRef, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use orbit_api::object_storage::{ObjectKey, ObjectNamespace};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::CHAT_TAG, oauth::model::LoginUser},
    chat::{
        domain::{ChatEvent, InMemoryChatEvents},
        service::{
            ChannelChatService, SharedChatService, channel_chat::extract_mentioned_usernames,
        },
    },
    common::errors::ApiError,
    contract::api::{
        chat::{
            AddChannelMembersReq, AttachmentConfirmReq, AttachmentPresignReq, AttachmentPresignRes,
            AttachmentResponse, ChannelMemberResponse, ChannelResponse, CreateChannelReq,
            CreateReactionReq, MessageResponse, ReactionResponse, SendMessageReq, UpdateChannelReq,
            UpdateMessageReq,
        },
        common::{CommonResult, Pagination},
    },
    jupiter::storage::Storage,
};

const CHAT_ATTACHMENT_MAX_FILE_SIZE: i64 = 100 * 1024 * 1024;
const CHAT_ATTACHMENT_MAX_FILE_NAME_LEN: usize = 255;
const CHAT_ATTACHMENT_MAX_FILE_TYPE_LEN: usize = 128;
const CHAT_ATTACHMENT_KEY_PREFIX: &str = "chat/attachments/";
const CHAT_PUSHER_CHANNEL_PREFIX: &str = "private-chat-channel-";
const PUSHER_EVENT_SUBSCRIBE: &str = "pusher:subscribe";
const PUSHER_EVENT_SUBSCRIPTION_SUCCEEDED: &str = "pusher_internal:subscription_succeeded";
const PUSHER_EVENT_CONNECTION_ESTABLISHED: &str = "pusher:connection_established";
const PUSHER_EVENT_ERROR: &str = "pusher:error";
const PUSHER_EVENT_PING: &str = "pusher:ping";
const PUSHER_EVENT_PONG: &str = "pusher:pong";

#[derive(Deserialize, IntoParams)]
struct AttachmentConfirmQuery {
    message_id: String,
}

#[derive(Deserialize)]
struct PusherClientMessage {
    event: String,
    data: Option<Value>,
}

#[derive(Serialize)]
struct PusherServerMessage<'a> {
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<&'a str>,
    data: String,
}

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/chat",
        OpenApiRouter::new()
            .route("/events", get(chat_events_ws))
            .routes(routes!(list_channels, create_channel))
            .routes(routes!(get_channel_detail, update_channel, delete_channel))
            .routes(routes!(list_messages, send_message))
            .routes(routes!(edit_message, delete_message))
            .routes(routes!(create_reaction))
            .routes(routes!(delete_reaction))
            .routes(routes!(presign_attachment))
            .routes(routes!(confirm_attachment))
            .routes(routes!(mark_channel_read, mark_channel_unread))
            .routes(routes!(list_channel_members, add_channel_members))
            .routes(routes!(remove_channel_member)),
    )
}

#[derive(Clone)]
struct ChatApiState {
    storage: Storage,
    listen_addr: String,
    events: Arc<InMemoryChatEvents>,
}

impl FromRef<MonoApiServiceState> for ChatApiState {
    fn from_ref(state: &MonoApiServiceState) -> Self {
        Self {
            storage: state.storage.clone(),
            listen_addr: state.listen_addr.clone(),
            events: state.chat_events.clone(),
        }
    }
}

impl ChatApiState {
    fn channel_chat_svc(&self) -> ChannelChatService<InMemoryChatEvents> {
        ChannelChatService::from_storage(&self.storage).with_events(self.events.clone())
    }

    fn shared_chat_svc(&self) -> SharedChatService {
        SharedChatService::from_storage(&self.storage)
    }
}

fn pusher_chat_channel(channel_public_id: &str) -> String {
    format!("{CHAT_PUSHER_CHANNEL_PREFIX}{channel_public_id}")
}

fn parse_pusher_subscription(text: &str) -> Option<String> {
    let payload: PusherClientMessage = serde_json::from_str(text).ok()?;
    if payload.event != PUSHER_EVENT_SUBSCRIBE {
        return None;
    }

    let data = payload.data?;
    let channel = match data {
        Value::Object(map) => map.get("channel")?.as_str()?.to_owned(),
        Value::String(raw) => {
            let parsed: Value = serde_json::from_str(&raw).ok()?;
            parsed.get("channel")?.as_str()?.to_owned()
        }
        _ => return None,
    };

    channel
        .strip_prefix(CHAT_PUSHER_CHANNEL_PREFIX)
        .map(str::to_owned)
}

fn chat_event_channel_public_id(event: &ChatEvent) -> &str {
    match event {
        ChatEvent::MessageCreated {
            channel_public_id, ..
        }
        | ChatEvent::MessageUpdated {
            channel_public_id, ..
        }
        | ChatEvent::MessageDeleted {
            channel_public_id, ..
        }
        | ChatEvent::ChannelUpdated { channel_public_id } => channel_public_id,
    }
}

fn chat_event_pusher_name(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::MessageCreated { .. } => "channel-message-created",
        ChatEvent::MessageUpdated { .. } => "channel-message-updated",
        ChatEvent::MessageDeleted { .. } => "channel-message-deleted",
        ChatEvent::ChannelUpdated { .. } => "channel-updated",
    }
}

fn chat_event_payload(event: &ChatEvent) -> Value {
    match event {
        ChatEvent::MessageCreated {
            channel_public_id,
            message_public_id,
        }
        | ChatEvent::MessageUpdated {
            channel_public_id,
            message_public_id,
        }
        | ChatEvent::MessageDeleted {
            channel_public_id,
            message_public_id,
        } => json!({
            "channel_public_id": channel_public_id,
            "message_public_id": message_public_id,
        }),
        ChatEvent::ChannelUpdated { channel_public_id } => json!({
            "channel_public_id": channel_public_id,
        }),
    }
}

fn pusher_envelope(event: &str, channel: Option<&str>, data: Value) -> String {
    let envelope = PusherServerMessage {
        event,
        channel,
        data: data.to_string(),
    };
    match serde_json::to_string(&envelope) {
        Ok(serialized) => serialized,
        Err(_) => r#"{"event":"pusher:error","data":"{\"message\":\"serialization failed\"}"}"#
            .to_string(),
    }
}

fn pusher_envelope_for_chat_event(event: &ChatEvent) -> String {
    let channel_public_id = chat_event_channel_public_id(event);
    let channel = pusher_chat_channel(channel_public_id);
    pusher_envelope(
        chat_event_pusher_name(event),
        Some(&channel),
        chat_event_payload(event),
    )
}

async fn user_can_subscribe_channel(
    state: &ChatApiState,
    username: &str,
    channel_public_id: &str,
) -> bool {
    state
        .storage
        .channel_storage()
        .get_channel_by_public_id(channel_public_id, username)
        .await
        .map(|ch| ch.is_some())
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                username = %username,
                channel_public_id = %channel_public_id,
                "failed to check chat event subscription"
            );
            false
        })
}

async fn send_ws_text(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    text: String,
) -> bool {
    sender.send(Message::Text(text.into())).await.is_ok()
}

async fn handle_chat_events_socket(socket: WebSocket, user: LoginUser, state: ChatApiState) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = state.events.subscribe();
    let mut subscribed_channels = HashSet::new();

    let connected = pusher_envelope(
        PUSHER_EVENT_CONNECTION_ESTABLISHED,
        None,
        json!({
            "socket_id": crate::callisto::entity_ext::generate_public_id(),
            "activity_timeout": 120,
        }),
    );
    if !send_ws_text(&mut sender, connected).await {
        return;
    }

    loop {
        tokio::select! {
            maybe_msg = receiver.next() => {
                let Some(Ok(msg)) = maybe_msg else {
                    break;
                };
                match msg {
                    Message::Text(text) => {
                        if serde_json::from_str::<PusherClientMessage>(&text)
                            .map(|payload| payload.event == PUSHER_EVENT_PING)
                            .unwrap_or(false)
                        {
                            if !send_ws_text(
                                &mut sender,
                                pusher_envelope(PUSHER_EVENT_PONG, None, json!({})),
                            )
                            .await
                            {
                                break;
                            }
                        } else if let Some(channel_public_id) = parse_pusher_subscription(&text)
                            && user_can_subscribe_channel(&state, &user.username, &channel_public_id).await
                        {
                            subscribed_channels.insert(channel_public_id.clone());
                            let channel = pusher_chat_channel(&channel_public_id);
                            let ack = pusher_envelope(
                                PUSHER_EVENT_SUBSCRIPTION_SUCCEEDED,
                                Some(&channel),
                                json!({}),
                            );
                            if !send_ws_text(&mut sender, ack).await {
                                break;
                            }
                        } else if !send_ws_text(
                            &mut sender,
                            pusher_envelope(
                                PUSHER_EVENT_ERROR,
                                None,
                                json!({ "message": "subscription rejected" }),
                            ),
                        ).await {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Pong(_) => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let channel_public_id = chat_event_channel_public_id(&event);
                        if !subscribed_channels.contains(channel_public_id) {
                            continue;
                        }
                        if !user_can_subscribe_channel(&state, &user.username, channel_public_id).await {
                            subscribed_channels.remove(channel_public_id);
                            continue;
                        }
                        if !send_ws_text(&mut sender, pusher_envelope_for_chat_event(&event)).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "chat websocket event receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn chat_events_ws(
    user: LoginUser,
    state: State<ChatApiState>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_chat_events_socket(socket, user, state.0))
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

fn extract_urls(content: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for token in content.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '>'
            )
        });
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            urls.push(trimmed.to_string());
        }
    }
    urls
}

async fn map_channel_model(
    ch: crate::callisto::channel::Model,
    state: &ChatApiState,
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
    state: &ChatApiState,
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
    path = "/channels",
    responses(
        (status = 200, body = CommonResult<Vec<ChannelResponse>>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn list_channels(
    user: LoginUser,
    state: State<ChatApiState>,
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
    path = "/channels",
    request_body = CreateChannelReq,
    responses(
        (status = 200, body = CommonResult<ChannelResponse>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn create_channel(
    user: LoginUser,
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}/messages",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}/messages",
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
    state: State<ChatApiState>,
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

    // Refresh Open Graph previews for any URLs in the message content. Failures
    // are logged but do not block the send response.
    let chat_cfg = state.storage.config().chat.clone().unwrap_or_default();
    if chat_cfg.open_graph_fetch_enabled {
        for url in extract_urls(&payload.content) {
            if let Err(e) = state
                .shared_chat_svc()
                .fetch_or_refresh_open_graph_link(
                    &url,
                    true,
                    chat_cfg.open_graph_fetch_timeout_ms,
                    chat_cfg.open_graph_allow_private_networks,
                )
                .await
            {
                tracing::warn!(error = %e, url = %url, "failed to refresh open graph link");
            }
        }
    }

    let mapped = map_message_model(msg, &state).await?;
    Ok(Json(CommonResult::success(Some(mapped))))
}

/// Edit message
#[utoipa::path(
    patch,
    path = "/channels/{channel_id}/messages/{message_id}",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}/messages/{message_id}",
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
    state: State<ChatApiState>,
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
    path = "/messages/{message_id}/reactions",
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
    state: State<ChatApiState>,
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
    path = "/reactions/{reaction_id}",
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
    state: State<ChatApiState>,
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
    path = "/attachments/presign",
    request_body = AttachmentPresignReq,
    responses(
        (status = 200, body = CommonResult<AttachmentPresignRes>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn presign_attachment(
    user: LoginUser,
    state: State<ChatApiState>,
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
    path = "/attachments",
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
    Query(query): Query<AttachmentConfirmQuery>,
    state: State<ChatApiState>,
    Json(payload): Json<AttachmentConfirmReq>,
) -> Result<Json<CommonResult<AttachmentResponse>>, ApiError> {
    let message_id = query.message_id;
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

    // Verify the target message exists and the caller is a current member of its
    // channel before probing object storage. This ordering prevents an
    // authorization side-channel that would distinguish existing objects from
    // missing objects for non-members.
    let msg = state
        .channel_chat_svc()
        .message_storage
        .get_message_by_public_id(&message_id)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Message not found")))?;
    let is_member = state
        .storage
        .channel_membership_storage()
        .get_membership(msg.channel_id, &user.username)
        .await?
        .is_some();
    if !is_member {
        return Err(ApiError::not_found(anyhow::anyhow!("Message not found")));
    }

    // Bind the attachment key to the message's channel. The presign endpoint
    // stores objects under `chat/attachments/<channel_public_id>/...`; a caller
    // who is a member of some other channel must not be able to probe keys
    // outside their channel by swapping the file_path.
    let channel = state
        .storage
        .channel_storage()
        .get_channel_by_id(msg.channel_id)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Message not found")))?;
    let key_channel = payload
        .file_path
        .strip_prefix("chat/attachments/")
        .and_then(|s| s.split('/').next())
        .ok_or_else(|| ApiError::bad_request(anyhow::anyhow!("invalid attachment file_path")))?;
    if key_channel != channel.public_id {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "attachment file_path does not belong to the message channel"
        )));
    }

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
    path = "/channels/{channel_id}/reads",
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
    state: State<ChatApiState>,
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
    path = "/channels/{channel_id}/reads",
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
    state: State<ChatApiState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .channel_chat_svc()
        .mark_unread(&channel_id, &user.username)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// List members of a channel.
#[utoipa::path(
    get,
    path = "/channels/{channel_id}/members",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    responses(
        (status = 200, body = CommonResult<Vec<ChannelMemberResponse>>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn list_channel_members(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<ChatApiState>,
) -> Result<Json<CommonResult<Vec<ChannelMemberResponse>>>, ApiError> {
    let ch = state
        .storage
        .channel_storage()
        .get_channel_by_public_id(&channel_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    let members = state
        .storage
        .channel_membership_storage()
        .list_members(ch.id)
        .await?;

    let responses: Vec<ChannelMemberResponse> = members
        .into_iter()
        .map(|m| ChannelMemberResponse {
            username: m.username,
            last_read_at: m.last_read_at.to_string(),
            notification_level: m.notification_level,
            joined_at: m.created_at.to_string(),
        })
        .collect();

    Ok(Json(CommonResult::success(Some(responses))))
}

/// Add members to a channel (owner only).
#[utoipa::path(
    post,
    path = "/channels/{channel_id}/members",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
    ),
    request_body = AddChannelMembersReq,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn add_channel_members(
    user: LoginUser,
    Path(channel_id): Path<String>,
    state: State<ChatApiState>,
    Json(payload): Json<AddChannelMembersReq>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let ch = state
        .storage
        .channel_storage()
        .get_channel_by_public_id(&channel_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    if ch.owner_username != user.username {
        return Err(ApiError::forbidden(anyhow::anyhow!(
            "only the channel owner can add members"
        )));
    }

    state
        .channel_chat_svc()
        .add_members(&channel_id, &user.username, payload.usernames)
        .await?;

    Ok(Json(CommonResult::success(None)))
}

/// Remove a member from a channel (owner only).
#[utoipa::path(
    delete,
    path = "/channels/{channel_id}/members/{username}",
    params(
        ("channel_id" = String, Path, description = "Public ID of the channel"),
        ("username" = String, Path, description = "Username of the member to remove"),
    ),
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = CHAT_TAG
)]
async fn remove_channel_member(
    user: LoginUser,
    Path((channel_id, member_username)): Path<(String, String)>,
    state: State<ChatApiState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let ch = state
        .storage
        .channel_storage()
        .get_channel_by_public_id(&channel_id, &user.username)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("Channel not found")))?;

    if ch.owner_username != user.username {
        return Err(ApiError::forbidden(anyhow::anyhow!(
            "only the channel owner can remove members"
        )));
    }

    if member_username == ch.owner_username {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "channel owner cannot be removed"
        )));
    }

    state
        .channel_chat_svc()
        .remove_members(&channel_id, &user.username, vec![member_username])
        .await?;

    Ok(Json(CommonResult::success(None)))
}

#[cfg(test)]
mod tests {
    use std::{io, net::SocketAddr, sync::Arc};

    use axum::Router;
    use bytes::Bytes;
    use futures::stream;
    use orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;
    use crate::{
        api::{MonoApiServiceState, api_router, oauth::model::LoginUser},
        bellatrix::Bellatrix,
        ceres::api_service::cache::GitObjectCache,
        config::RedisConfig,
        contract::{api::common::CommonResult, policy::entitystore::EntityStore},
        jupiter::{redis::init_connection, storage::Storage, tests::test_storage},
    };

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

    async fn setup_test_state(temp_dir: &std::path::Path) -> ChatApiState {
        let storage = test_storage(temp_dir).await;

        ChatApiState {
            storage: storage.clone(),
            listen_addr: "http://localhost:8000".to_string(),
            events: Arc::new(InMemoryChatEvents::default()),
        }
    }

    async fn put_attachment_object(state: &ChatApiState, key: &str, size: i64) {
        put_attachment_object_in_storage(&state.storage, key, size).await;
    }

    async fn put_attachment_object_in_storage(storage: &Storage, key: &str, size: i64) {
        let object_key = ObjectKey {
            namespace: ObjectNamespace::Attachment,
            key: key.to_string(),
        };
        let data: Vec<Result<Bytes, io::Error>> = vec![Ok(Bytes::from_static(b"attachment-bytes"))];
        storage
            .git_service
            .obj_storage
            .inner
            .put_stream(
                &object_key,
                Box::pin(stream::iter(data)),
                ObjectMeta {
                    size,
                    ..Default::default()
                },
            )
            .await
            .expect("failed to put attachment object");
    }

    async fn setup_chat_http_server(
        temp_dir: &std::path::Path,
    ) -> (String, Storage, Arc<InMemoryChatEvents>) {
        let storage = test_storage(temp_dir).await;
        let chat_events = Arc::new(InMemoryChatEvents::default());
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let redis_conn = init_connection(&RedisConfig { url: redis_url })
            .await
            .expect("test Redis should be available for HTTP black-box test");
        let git_object_cache = Arc::new(GitObjectCache {
            connection: redis_conn,
            prefix: "git-object-rkyv:v1:test".to_string(),
        });
        let api_state = MonoApiServiceState {
            storage: storage.clone(),
            listen_addr: "http://127.0.0.1:0".to_string(),
            entity_store: EntityStore::new(),
            git_object_cache,
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            chat_events: chat_events.clone(),
        };
        let api_routes: Router = api_router::routers().with_state(api_state).into();
        let app = Router::new().nest("/api/v1", api_routes);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind chat HTTP test listener");
        let addr: SocketAddr = listener.local_addr().expect("chat HTTP test addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("chat HTTP test server failed");
        });

        (format!("http://{addr}/api/v1"), storage, chat_events)
    }

    async fn expect_common_data(response: reqwest::Response) -> Value {
        let status = response.status();
        let body = response.text().await.expect("read response body");
        assert!(status.is_success(), "status={status}, body={body}");
        let parsed: CommonResult<Value> =
            serde_json::from_str(&body).expect("response should be CommonResult JSON");
        assert!(parsed.req_result, "body={body}");
        parsed.data.expect("success response should include data")
    }

    async fn expect_common_ok(response: reqwest::Response) {
        let status = response.status();
        let body = response.text().await.expect("read response body");
        assert!(status.is_success(), "status={status}, body={body}");
        let parsed: CommonResult<Value> =
            serde_json::from_str(&body).expect("response should be CommonResult JSON");
        assert!(parsed.req_result, "body={body}");
    }

    async fn expect_client_error(response: reqwest::Response) {
        let status = response.status();
        let body = response.text().await.expect("read response body");
        assert!(status.is_client_error(), "status={status}, body={body}");
    }

    #[test]
    fn parse_pusher_subscription_accepts_object_and_string_data() {
        assert_eq!(
            parse_pusher_subscription(
                r#"{"event":"pusher:subscribe","data":{"channel":"private-chat-channel-chan_123"}}"#,
            )
            .as_deref(),
            Some("chan_123")
        );
        assert_eq!(
            parse_pusher_subscription(
                r#"{"event":"pusher:subscribe","data":"{\"channel\":\"private-chat-channel-chan_456\"}"}"#,
            )
            .as_deref(),
            Some("chan_456")
        );
        assert_eq!(
            parse_pusher_subscription(
                r#"{"event":"pusher:subscribe","data":{"channel":"presence-other"}}"#,
            ),
            None
        );
    }

    #[test]
    fn pusher_envelope_for_chat_event_uses_chat_channel_and_payload() {
        let envelope = pusher_envelope_for_chat_event(&ChatEvent::MessageCreated {
            channel_public_id: "chan_123".to_string(),
            message_public_id: "msg_123".to_string(),
        });
        let parsed: Value = serde_json::from_str(&envelope).expect("pusher envelope JSON");

        assert_eq!(parsed["event"], "channel-message-created");
        assert_eq!(parsed["channel"], "private-chat-channel-chan_123");
        let data: Value =
            serde_json::from_str(parsed["data"].as_str().unwrap()).expect("pusher data JSON");
        assert_eq!(data["channel_public_id"], "chan_123");
        assert_eq!(data["message_public_id"], "msg_123");
    }

    #[tokio::test]
    async fn chat_http_black_box_lifecycle_matrix() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let (base_url, storage, chat_events) = setup_chat_http_server(temp_dir.path()).await;
        let mut event_rx = chat_events.subscribe();
        let client = reqwest::Client::new();

        let channel = expect_common_data(
            client
                .post(format!("{base_url}/chat/channels"))
                .json(&json!({
                    "title": "HTTP General",
                    "image_path": null,
                    "member_usernames": ["bob"],
                    "group": true,
                    "initial_message": "Welcome over HTTP"
                }))
                .send()
                .await
                .expect("create channel request"),
        )
        .await;
        assert_eq!(channel["title"], "HTTP General");
        assert_eq!(channel["owner_username"], "admin");
        assert!(channel.get("id").is_none(), "channel must not expose DB id");
        let channel_id = channel["public_id"].as_str().unwrap().to_string();
        let initial_message_event =
            tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
                .await
                .expect("initial message event should be published")
                .expect("initial message event channel should be open");
        assert!(matches!(
            initial_message_event,
            ChatEvent::MessageCreated {
                channel_public_id,
                ..
            } if channel_public_id == channel_id
        ));

        let channels = expect_common_data(
            client
                .get(format!("{base_url}/chat/channels"))
                .send()
                .await
                .expect("list channels request"),
        )
        .await;
        assert_eq!(channels.as_array().unwrap().len(), 1);

        let detail = expect_common_data(
            client
                .get(format!("{base_url}/chat/channels/{channel_id}"))
                .send()
                .await
                .expect("channel detail request"),
        )
        .await;
        assert_eq!(detail["public_id"], channel_id);

        let updated = expect_common_data(
            client
                .patch(format!("{base_url}/chat/channels/{channel_id}"))
                .json(&json!({
                    "title": "HTTP General Renamed",
                    "image_path": "chat/channel.png"
                }))
                .send()
                .await
                .expect("update channel request"),
        )
        .await;
        assert_eq!(updated["title"], "HTTP General Renamed");

        let message = expect_common_data(
            client
                .post(format!("{base_url}/chat/channels/{channel_id}/messages"))
                .json(&json!({
                    "content": "hello from HTTP",
                    "reply_to_public_id": null,
                    "attachments": null
                }))
                .send()
                .await
                .expect("send message request"),
        )
        .await;
        assert_eq!(message["sender_username"], "admin");
        assert!(message.get("id").is_none(), "message must not expose DB id");
        let message_id = message["public_id"].as_str().unwrap().to_string();

        let messages = expect_common_data(
            client
                .get(format!(
                    "{base_url}/chat/channels/{channel_id}/messages?page=1&per_page=10"
                ))
                .send()
                .await
                .expect("list messages request"),
        )
        .await;
        assert_eq!(messages.as_array().unwrap().len(), 2);

        let edited = expect_common_data(
            client
                .patch(format!(
                    "{base_url}/chat/channels/{channel_id}/messages/{message_id}"
                ))
                .json(&json!({ "content": "hello from HTTP, edited" }))
                .send()
                .await
                .expect("edit message request"),
        )
        .await;
        assert_eq!(edited["content"], "hello from HTTP, edited");

        let reaction = expect_common_data(
            client
                .post(format!("{base_url}/chat/messages/{message_id}/reactions"))
                .json(&json!({
                    "content": "clap",
                    "custom_reaction_public_id": null
                }))
                .send()
                .await
                .expect("create reaction request"),
        )
        .await;
        assert_eq!(reaction["username"], "admin");
        let reaction_id = reaction["public_id"].as_str().unwrap().to_string();

        expect_common_ok(
            client
                .delete(format!("{base_url}/chat/reactions/{reaction_id}"))
                .send()
                .await
                .expect("delete reaction request"),
        )
        .await;

        expect_common_ok(
            client
                .post(format!("{base_url}/chat/channels/{channel_id}/members"))
                .json(&json!({ "usernames": ["carol"] }))
                .send()
                .await
                .expect("add members request"),
        )
        .await;
        let members = expect_common_data(
            client
                .get(format!("{base_url}/chat/channels/{channel_id}/members"))
                .send()
                .await
                .expect("list members request"),
        )
        .await;
        assert!(
            members
                .as_array()
                .unwrap()
                .iter()
                .any(|member| member["username"] == "carol")
        );
        expect_common_ok(
            client
                .delete(format!(
                    "{base_url}/chat/channels/{channel_id}/members/carol"
                ))
                .send()
                .await
                .expect("remove member request"),
        )
        .await;

        expect_common_ok(
            client
                .post(format!("{base_url}/chat/channels/{channel_id}/reads"))
                .send()
                .await
                .expect("mark read request"),
        )
        .await;
        expect_common_ok(
            client
                .delete(format!("{base_url}/chat/channels/{channel_id}/reads"))
                .send()
                .await
                .expect("mark unread request"),
        )
        .await;

        let presign = expect_common_data(
            client
                .post(format!("{base_url}/chat/attachments/presign"))
                .json(&json!({
                    "file_name": "http.txt",
                    "file_size": 16,
                    "file_type": "text/plain",
                    "channel_public_id": channel_id
                }))
                .send()
                .await
                .expect("presign attachment request"),
        )
        .await;
        let file_path = presign["file_path"].as_str().unwrap().to_string();
        expect_client_error(
            client
                .post(format!(
                    "{base_url}/chat/attachments?message_id={message_id}"
                ))
                .json(&json!({
                    "file_path": file_path,
                    "file_type": "text/plain",
                    "file_name": "http.txt",
                    "file_size": 16
                }))
                .send()
                .await
                .expect("confirm missing attachment request"),
        )
        .await;

        put_attachment_object_in_storage(&storage, &file_path, 16).await;
        let attachment = expect_common_data(
            client
                .post(format!(
                    "{base_url}/chat/attachments?message_id={message_id}"
                ))
                .json(&json!({
                    "file_path": file_path,
                    "file_type": "text/plain",
                    "file_name": "http.txt",
                    "file_size": 16
                }))
                .send()
                .await
                .expect("confirm attachment request"),
        )
        .await;
        assert_eq!(attachment["name"], "http.txt");

        expect_common_ok(
            client
                .delete(format!(
                    "{base_url}/chat/channels/{channel_id}/messages/{message_id}"
                ))
                .send()
                .await
                .expect("delete message request"),
        )
        .await;
        expect_common_ok(
            client
                .delete(format!("{base_url}/chat/channels/{channel_id}"))
                .send()
                .await
                .expect("delete channel request"),
        )
        .await;
    }

    #[tokio::test]
    async fn test_chat_router_handlers_lifecycle() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let state = setup_test_state(temp_dir.path()).await;

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

        // 1b. Channel member management endpoints
        let members_res = list_channel_members(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to list members")
        .0;
        let members = members_res.data.unwrap();
        assert_eq!(members.len(), 2);
        assert!(members.iter().any(|m| m.username == "alice"));
        assert!(members.iter().any(|m| m.username == "bob"));

        // Non-member cannot list members.
        let charlie = LoginUser {
            campsite_user_id: "user-charlie".to_string(),
            username: "charlie".to_string(),
            avatar_url: "".to_string(),
            email: "charlie@example.com".to_string(),
        };
        let dave = LoginUser {
            campsite_user_id: "user-dave".to_string(),
            username: "dave".to_string(),
            avatar_url: "".to_string(),
            email: "dave@example.com".to_string(),
        };
        let non_member_list = list_channel_members(
            charlie.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await;
        assert!(non_member_list.is_err());

        // Non-owner cannot add members.
        let bob_add = add_channel_members(
            bob.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
            Json(AddChannelMembersReq {
                usernames: vec!["charlie".to_string()],
            }),
        )
        .await;
        assert!(bob_add.is_err());

        // Owner adds dave.
        let add_res = add_channel_members(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
            Json(AddChannelMembersReq {
                usernames: vec!["dave".to_string()],
            }),
        )
        .await
        .expect("failed to add dave")
        .0;
        assert!(add_res.req_result);

        let members_after_add = list_channel_members(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to list members after add")
        .0
        .data
        .unwrap();
        assert!(members_after_add.iter().any(|m| m.username == "dave"));

        // Owner cannot remove themselves.
        let self_remove = remove_channel_member(
            alice.clone(),
            Path((ch.public_id.clone(), "alice".to_string())),
            State(state.clone()),
        )
        .await;
        assert!(self_remove.is_err());

        // Owner removes dave; bob is preserved for the later message flow.
        let remove_res = remove_channel_member(
            alice.clone(),
            Path((ch.public_id.clone(), "dave".to_string())),
            State(state.clone()),
        )
        .await
        .expect("failed to remove dave")
        .0;
        assert!(remove_res.req_result);
        let members_after_remove = list_channel_members(
            alice.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await
        .expect("failed to list members after remove")
        .0
        .data
        .unwrap();
        assert!(!members_after_remove.iter().any(|m| m.username == "dave"));

        // Removed member can no longer list members.
        let removed_list = list_channel_members(
            dave.clone(),
            Path(ch.public_id.clone()),
            State(state.clone()),
        )
        .await;
        assert!(removed_list.is_err());

        // 2. List visible channels for alice
        let list_res = list_channels(alice.clone(), State(state.clone()))
            .await
            .expect("failed to list channels")
            .0;
        let alice_channels = list_res.data.unwrap();
        assert_eq!(alice_channels.len(), 2);
        assert!(
            alice_channels
                .iter()
                .any(|channel| channel.public_id == ch.public_id)
        );
        assert!(
            alice_channels
                .iter()
                .any(|channel| channel.public_id == other_ch.public_id)
        );

        // List for non-member (e.g. charlie) should be empty
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
            Query(AttachmentConfirmQuery {
                message_id: sent_msg.public_id.clone(),
            }),
            State(state.clone()),
            Json(confirm_req.clone()),
        )
        .await;
        assert!(missing_object_confirm.is_err());

        // Seed the correct object so the following negative tests fail for
        // authorization/channel-binding reasons, not because the object is
        // missing.
        put_attachment_object(&state, &presign_res.file_path, 16).await;

        // A non-member must not be able to probe attachment state.
        let non_member_confirm = confirm_attachment(
            charlie.clone(),
            Query(AttachmentConfirmQuery {
                message_id: sent_msg.public_id.clone(),
            }),
            State(state.clone()),
            Json(confirm_req.clone()),
        )
        .await;
        assert!(non_member_confirm.is_err());

        // A member must not confirm an attachment whose object key belongs to a
        // different channel, even if they are a member of both.
        let wrong_channel_path =
            presign_res
                .file_path
                .replacen(&ch.public_id, &other_ch.public_id, 1);
        put_attachment_object(&state, &wrong_channel_path, 16).await;
        let wrong_channel_confirm = confirm_attachment(
            alice.clone(),
            Query(AttachmentConfirmQuery {
                message_id: sent_msg.public_id.clone(),
            }),
            State(state.clone()),
            Json(AttachmentConfirmReq {
                file_path: wrong_channel_path,
                ..confirm_req.clone()
            }),
        )
        .await;
        assert!(wrong_channel_confirm.is_err());

        // 10. Confirm/register attachment
        let confirmed_att = confirm_attachment(
            alice.clone(),
            Query(AttachmentConfirmQuery {
                message_id: sent_msg.public_id.clone(),
            }),
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

    #[test]
    fn extract_urls_finds_http_and_https_tokens() {
        let content = "Check out https://example.com/page, and http://test.org?q=1. Also \
            https://else.where/path.";
        let urls = extract_urls(content);
        assert_eq!(urls.len(), 3);
        assert!(urls.contains(&"https://example.com/page".to_string()));
        assert!(urls.contains(&"http://test.org?q=1".to_string()));
        assert!(urls.contains(&"https://else.where/path".to_string()));
    }
}
