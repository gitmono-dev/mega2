use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    routing::get,
};
use russh::keys::{HashAlg, parse_public_key_base64};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        api_doc::{MAIL_TAG, USER_TAG},
        oauth::model::LoginUser,
    },
    callisto::{
        notification_event_types, user_notification_preferences, user_notification_settings,
    },
    ceres::model::user::{
        AddSSHKey, ClaContentRes, ClaSignStatusRes, ListSSHKey, ListToken, UpdateClaContentPayload,
    },
    common::errors::{ApiError, MegaError},
    contract::api::common::CommonResult,
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/user",
        OpenApiRouter::new()
            .route("/", get(user))
            .routes(routes!(list_key))
            .routes(routes!(add_key))
            .routes(routes!(remove_key))
            .routes(routes!(generate_token))
            .routes(routes!(list_token))
            .routes(routes!(remove_token))
            .routes(routes!(get_cla_sign_status))
            .routes(routes!(change_sign_status))
            .routes(routes!(get_cla_content))
            .routes(routes!(update_cla_content))
            .routes(routes!(list_notification_preferences))
            .routes(routes!(update_notification_preference)),
    )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserNotificationSettingsResponse {
    pub username: String,
    pub email: Option<String>,
    pub enabled: bool,
    pub delivery_mode: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserNotificationPreferenceResponse {
    pub event_type_code: String,
    pub category: String,
    pub description: String,
    pub system_required: bool,
    pub default_enabled: bool,
    pub explicit_enabled: Option<bool>,
    pub enabled: bool,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserNotificationPreferencesResponse {
    pub settings: UserNotificationSettingsResponse,
    pub preferences: Vec<UserNotificationPreferenceResponse>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNotificationPreferenceRequest {
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateNotificationPreferenceResponse {
    pub preference: UserNotificationPreferenceResponse,
}

async fn user(
    user: LoginUser,
    _: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<LoginUser>>, ApiError> {
    Ok(Json(CommonResult::success(Some(user))))
}

/// Add SSH Key
#[utoipa::path(
    post,
    path = "/ssh",
    request_body = AddSSHKey,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn add_key(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(json): Json<AddSSHKey>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let ssh_parts: Vec<&str> = json.ssh_key.split_whitespace().collect();
    let key = parse_public_key_base64(
        ssh_parts
            .get(1)
            .ok_or_else(|| MegaError::Other("Invalid key format".to_string()))?,
    )?;
    let title = if json.title.is_empty() {
        ssh_parts
            .get(2)
            .ok_or_else(|| MegaError::Other("Invalid key format".to_string()))?
            .to_string()
    } else {
        json.title
    };
    state
        .user_stg()
        .save_ssh_key(
            user.username,
            &title,
            &json.ssh_key,
            &key.fingerprint(HashAlg::Sha256).to_string(),
        )
        .await?;
    Ok(Json(CommonResult::success(None)))
}

/// Delete SSH Key
#[utoipa::path(
    delete,
        params(
        ("key_id", description = "A numeric ID representing a SSH"),
    ),
    path = "/ssh/{key_id}",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn remove_key(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Path(key_id): Path<i64>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state
        .user_stg()
        .delete_ssh_key(user.username, key_id)
        .await?;
    Ok(Json(CommonResult::success(None)))
}

/// Get User's SSH key list
#[utoipa::path(
    get,
    path = "/ssh/list",
    responses(
        (status = 200, body = CommonResult<Vec<ListSSHKey>>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn list_key(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<ListSSHKey>>>, ApiError> {
    let res = state.user_stg().list_user_ssh(user.username).await?;
    Ok(Json(CommonResult::success(Some(
        res.into_iter().map(|x| x.into()).collect(),
    ))))
}

/// Generate Token For http push
#[utoipa::path(
    post,
    path = "/token/generate",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn generate_token(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.user_stg().generate_token(user.username).await?;
    Ok(Json(CommonResult::success(Some(res))))
}

/// Delete User's http push token
#[utoipa::path(
    delete,
        params(
        ("key_id", description = "A numeric ID representing a User Token"),
    ),
    path = "/token/{key_id}",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn remove_token(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Path(key_id): Path<i64>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state.user_stg().delete_token(user.username, key_id).await?;
    Ok(Json(CommonResult::success(None)))
}

/// Get User's push token list
#[utoipa::path(
    get,
    path = "/token/list",
    responses(
        (status = 200, body = CommonResult<Vec<ListToken>>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn list_token(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<ListToken>>>, ApiError> {
    let data = state.user_stg().list_token(user.username).await?;
    let res = data.into_iter().map(|x| x.into()).collect();
    Ok(Json(CommonResult::success(Some(res))))
}

/// Get current user's notification preferences
#[utoipa::path(
    get,
    path = "/notification/preferences",
    responses(
        (status = 200, body = CommonResult<UserNotificationPreferencesResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
    ),
    tag = MAIL_TAG
)]
async fn list_notification_preferences(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<UserNotificationPreferencesResponse>>, ApiError> {
    let notification_storage = state.storage.notification_storage();
    let settings = notification_storage
        .get_user_settings(&user.username)
        .await?;
    let event_types = notification_storage.list_event_types().await?;
    let preferences = notification_storage
        .list_user_preferences(&user.username)
        .await?;

    Ok(Json(CommonResult::success(Some(
        build_notification_preferences_response(&user.username, settings, event_types, preferences),
    ))))
}

/// Update current user's notification preference for one event type
#[utoipa::path(
    put,
    path = "/notification/preferences/{event_type_code}",
    params(
        ("event_type_code" = String, Path, description = "Notification event type code")
    ),
    request_body = UpdateNotificationPreferenceRequest,
    responses(
        (status = 200, body = CommonResult<UpdateNotificationPreferenceResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid event type or notification settings are missing"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Notification event type not found"),
    ),
    tag = MAIL_TAG
)]
async fn update_notification_preference(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(event_type_code): Path<String>,
    Json(payload): Json<UpdateNotificationPreferenceRequest>,
) -> Result<Json<CommonResult<UpdateNotificationPreferenceResponse>>, ApiError> {
    let event_type_code = validate_notification_event_type_code(&event_type_code)?;
    let notification_storage = state.storage.notification_storage();
    let event_type = notification_storage
        .get_event_type(&event_type_code)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("notification event type not found")))?;

    if event_type.system_required {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "system-required notification preferences cannot be changed"
        )));
    }

    let settings = notification_storage
        .get_user_settings(&user.username)
        .await?
        .ok_or_else(|| {
            ApiError::bad_request(anyhow::anyhow!(
                "notification settings are not configured for current user"
            ))
        })?;

    notification_storage
        .set_user_preference(&user.username, &event_type_code, payload.enabled)
        .await?;
    let preference = notification_storage
        .get_user_preference(&user.username, &event_type_code)
        .await?
        .ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!("notification preference was not persisted"))
        })?;

    Ok(Json(CommonResult::success(Some(
        UpdateNotificationPreferenceResponse {
            preference: build_notification_preference_response(
                event_type,
                Some(preference),
                Some(&settings),
            ),
        },
    ))))
}

fn build_notification_preferences_response(
    username: &str,
    settings: Option<user_notification_settings::Model>,
    mut event_types: Vec<notification_event_types::Model>,
    preferences: Vec<user_notification_preferences::Model>,
) -> UserNotificationPreferencesResponse {
    event_types.sort_by(|left, right| left.code.cmp(&right.code));
    let mut preferences_by_event: HashMap<String, user_notification_preferences::Model> =
        preferences
            .into_iter()
            .map(|preference| (preference.event_type_code.clone(), preference))
            .collect();

    UserNotificationPreferencesResponse {
        settings: build_notification_settings_response(username, settings.as_ref()),
        preferences: event_types
            .into_iter()
            .map(|event_type| {
                let preference = preferences_by_event.remove(&event_type.code);
                build_notification_preference_response(event_type, preference, settings.as_ref())
            })
            .collect(),
    }
}

fn build_notification_settings_response(
    username: &str,
    settings: Option<&user_notification_settings::Model>,
) -> UserNotificationSettingsResponse {
    match settings {
        Some(settings) => UserNotificationSettingsResponse {
            username: settings.username.clone(),
            email: Some(settings.email.clone()),
            enabled: settings.enabled,
            delivery_mode: Some(settings.delivery_mode.clone()),
            created_at: Some(settings.created_at.to_string()),
            updated_at: Some(settings.updated_at.to_string()),
        },
        None => UserNotificationSettingsResponse {
            username: username.to_string(),
            email: None,
            enabled: false,
            delivery_mode: None,
            created_at: None,
            updated_at: None,
        },
    }
}

fn build_notification_preference_response(
    event_type: notification_event_types::Model,
    preference: Option<user_notification_preferences::Model>,
    settings: Option<&user_notification_settings::Model>,
) -> UserNotificationPreferenceResponse {
    let explicit_enabled = preference.as_ref().map(|preference| preference.enabled);
    let updated_at = preference
        .as_ref()
        .map(|preference| preference.updated_at.to_string());
    let enabled = match settings {
        Some(settings) if settings.enabled => {
            if event_type.system_required {
                true
            } else {
                explicit_enabled.unwrap_or(event_type.default_enabled)
            }
        }
        _ => false,
    };

    UserNotificationPreferenceResponse {
        event_type_code: event_type.code,
        category: event_type.category,
        description: event_type.description,
        system_required: event_type.system_required,
        default_enabled: event_type.default_enabled,
        explicit_enabled,
        enabled,
        updated_at,
    }
}

fn validate_notification_event_type_code(event_type_code: &str) -> Result<String, ApiError> {
    let event_type_code = event_type_code.trim();
    if event_type_code.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "event_type_code must not be empty"
        )));
    }
    Ok(event_type_code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_router_creation() {
        let _router = routers();
    }

    #[test]
    fn notification_preferences_response_uses_settings_defaults_and_overrides() {
        let now = chrono::Utc::now().naive_utc();
        let response = build_notification_preferences_response(
            "alice",
            Some(user_notification_settings::Model {
                username: "alice".to_string(),
                email: "alice@example.com".to_string(),
                enabled: true,
                delivery_mode: "realtime".to_string(),
                created_at: now,
                updated_at: now,
            }),
            vec![
                notification_event_types::Model {
                    code: "z.event".to_string(),
                    category: "test".to_string(),
                    description: "default off".to_string(),
                    system_required: false,
                    default_enabled: false,
                    created_at: now,
                    updated_at: now,
                },
                notification_event_types::Model {
                    code: "a.event".to_string(),
                    category: "test".to_string(),
                    description: "default on".to_string(),
                    system_required: false,
                    default_enabled: true,
                    created_at: now,
                    updated_at: now,
                },
            ],
            vec![user_notification_preferences::Model {
                username: "alice".to_string(),
                event_type_code: "z.event".to_string(),
                enabled: true,
                created_at: now,
                updated_at: now,
            }],
        );

        assert_eq!(
            response.settings.email.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(response.preferences.len(), 2);
        assert_eq!(response.preferences[0].event_type_code, "a.event");
        assert_eq!(response.preferences[0].explicit_enabled, None);
        assert!(response.preferences[0].enabled);
        assert_eq!(response.preferences[1].event_type_code, "z.event");
        assert_eq!(response.preferences[1].explicit_enabled, Some(true));
        assert!(response.preferences[1].enabled);
    }

    #[test]
    fn notification_preferences_response_is_disabled_without_settings() {
        let now = chrono::Utc::now().naive_utc();
        let response = build_notification_preferences_response(
            "alice",
            None,
            vec![notification_event_types::Model {
                code: "test.event".to_string(),
                category: "test".to_string(),
                description: "default on".to_string(),
                system_required: false,
                default_enabled: true,
                created_at: now,
                updated_at: now,
            }],
            Vec::new(),
        );

        assert!(!response.settings.enabled);
        assert_eq!(response.settings.email, None);
        assert!(!response.preferences[0].enabled);
    }

    #[test]
    fn validate_notification_event_type_code_rejects_blank_code() {
        assert!(validate_notification_event_type_code("  ").is_err());
        assert_eq!(
            validate_notification_event_type_code(" cl.comment.created ").unwrap(),
            "cl.comment.created"
        );
    }
}
/// Get current user's CLA sign status
#[utoipa::path(
    get,
    path = "/cla/status",
    responses(
        (status = 200, body = CommonResult<ClaSignStatusRes>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn get_cla_sign_status(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<ClaSignStatusRes>>, ApiError> {
    let (cla_signed, cla_signed_at) = state
        .monorepo()
        .get_or_init_cla_sign_status(&user.username)
        .await?;

    let res = ClaSignStatusRes {
        username: user.username,
        cla_signed,
        cla_signed_at: cla_signed_at.map(|dt| dt.and_utc().timestamp()),
    };
    Ok(Json(CommonResult::success(Some(res))))
}

/// Change CLA sign status for current user
#[utoipa::path(
    post,
    path = "/cla/change-sign-status",
    responses(
        (status = 200, body = CommonResult<ClaSignStatusRes>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn change_sign_status(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<ClaSignStatusRes>>, ApiError> {
    let (cla_signed, cla_signed_at) = state
        .monorepo()
        .change_cla_sign_status(&user.username)
        .await?;

    let res = ClaSignStatusRes {
        username: user.username,
        cla_signed,
        cla_signed_at: cla_signed_at.map(|dt| dt.and_utc().timestamp()),
    };
    Ok(Json(CommonResult::success(Some(res))))
}

/// Get latest CLA text content
#[utoipa::path(
    get,
    path = "/cla/content",
    responses(
        (status = 200, body = CommonResult<ClaContentRes>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn get_cla_content(
    _user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<ClaContentRes>>, ApiError> {
    let content = state.monorepo().get_cla_content().await?;
    Ok(Json(CommonResult::success(Some(ClaContentRes { content }))))
}

/// Update latest CLA text content
#[utoipa::path(
    post,
    path = "/cla/content",
    request_body = UpdateClaContentPayload,
    responses(
        (status = 200, body = CommonResult<ClaContentRes>, content_type = "application/json")
    ),
    tag = USER_TAG
)]
async fn update_cla_content(
    _user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(payload): Json<UpdateClaContentPayload>,
) -> Result<Json<CommonResult<ClaContentRes>>, ApiError> {
    state
        .monorepo()
        .update_cla_content(&payload.content)
        .await?;
    Ok(Json(CommonResult::success(Some(ClaContentRes {
        content: payload.content,
    }))))
}
