use std::collections::{HashMap, HashSet};

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
    api::{MonoApiServiceState, api_doc::USER_TAG, oauth::model::LoginUser},
    callisto::{
        notification_event_types, user_notification_preferences, user_notification_settings,
    },
    ceres::model::{
        notification::{UpdateUserNotificationConfig, UserNotificationPreferenceItem},
        user::{AddSSHKey, ListSSHKey, ListToken},
    },
    common::errors::{ApiError, MegaError},
    contract::api::common::CommonResult,
    jupiter::storage::notification_storage::NotificationStorage,
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
            .routes(routes!(list_notification_preferences))
            .routes(routes!(update_notification_preferences))
            .routes(routes!(update_notification_preference)),
    )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserNotificationSettingsResponse {
    pub username: String,
    pub enabled: bool,
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
    tag = USER_TAG
)]
async fn list_notification_preferences(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<UserNotificationPreferencesResponse>>, ApiError> {
    let notification_storage = state.storage.notification_storage();

    Ok(Json(CommonResult::success(Some(
        load_notification_preferences_response(&notification_storage, &user.username).await?,
    ))))
}

/// Update current user's notification settings and preferences
#[utoipa::path(
    put,
    path = "/notification/preferences",
    request_body = UpdateUserNotificationConfig,
    responses(
        (status = 200, body = CommonResult<UserNotificationPreferencesResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid notification settings or preferences"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Notification event type not found"),
    ),
    tag = USER_TAG
)]
async fn update_notification_preferences(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(payload): Json<UpdateUserNotificationConfig>,
) -> Result<Json<CommonResult<UserNotificationPreferencesResponse>>, ApiError> {
    let notification_storage = state.storage.notification_storage();
    let preferences = payload
        .preferences
        .map(normalize_notification_preferences)
        .transpose()?;

    if let Some(preferences) = &preferences {
        for preference in preferences {
            ensure_preference_event_type_is_mutable(
                &notification_storage,
                &preference.event_type_code,
            )
            .await?;
        }
    }

    ensure_user_notification_settings(&notification_storage, &user).await?;

    if let Some(enabled) = payload.enabled {
        notification_storage
            .set_global_enabled(&user.username, enabled)
            .await?;
    }
    if let Some(preferences) = preferences {
        for preference in preferences {
            notification_storage
                .set_user_preference(
                    &user.username,
                    &preference.event_type_code,
                    preference.enabled,
                )
                .await?;
        }
    }

    Ok(Json(CommonResult::success(Some(
        load_notification_preferences_response(&notification_storage, &user.username).await?,
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
    tag = USER_TAG
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

    let settings = ensure_user_notification_settings(&notification_storage, &user).await?;

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

async fn load_notification_preferences_response(
    notification_storage: &NotificationStorage,
    username: &str,
) -> Result<UserNotificationPreferencesResponse, ApiError> {
    let settings = notification_storage.get_user_settings(username).await?;
    let event_types = notification_storage.list_event_types().await?;
    let preferences = notification_storage.list_user_preferences(username).await?;

    Ok(build_notification_preferences_response(
        username,
        settings,
        event_types,
        preferences,
    ))
}

async fn ensure_user_notification_settings(
    notification_storage: &NotificationStorage,
    user: &LoginUser,
) -> Result<user_notification_settings::Model, ApiError> {
    if let Some(settings) = notification_storage
        .get_user_settings(&user.username)
        .await?
    {
        return Ok(settings);
    }

    notification_storage
        .upsert_user_settings(&user.username)
        .await?;
    notification_storage
        .get_user_settings(&user.username)
        .await?
        .ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!("notification settings were not persisted"))
        })
}

async fn ensure_preference_event_type_is_mutable(
    notification_storage: &NotificationStorage,
    event_type_code: &str,
) -> Result<notification_event_types::Model, ApiError> {
    let event_type = notification_storage
        .get_event_type(event_type_code)
        .await?
        .ok_or_else(|| ApiError::not_found(anyhow::anyhow!("notification event type not found")))?;

    if event_type.system_required {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "system-required notification preferences cannot be changed"
        )));
    }

    Ok(event_type)
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
            enabled: settings.enabled,
            created_at: Some(settings.created_at.to_string()),
            updated_at: Some(settings.updated_at.to_string()),
        },
        None => UserNotificationSettingsResponse {
            username: username.to_string(),
            enabled: false,
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

fn normalize_notification_preferences(
    preferences: Vec<UserNotificationPreferenceItem>,
) -> Result<Vec<UserNotificationPreferenceItem>, ApiError> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(preferences.len());

    for preference in preferences {
        let event_type_code = validate_notification_event_type_code(&preference.event_type_code)?;
        if !seen.insert(event_type_code.clone()) {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "duplicate notification preference for event type `{event_type_code}`"
            )));
        }
        normalized.push(UserNotificationPreferenceItem {
            event_type_code,
            enabled: preference.enabled,
        });
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_router_creation() {
        let _router = routers();
    }

    #[test]
    fn login_user_dto_still_exposes_email() {
        let user = LoginUser {
            website_user_id: "1".to_string(),
            username: "alice".to_string(),
            avatar_url: String::new(),
            email: "alice@example.com".to_string(),
        };
        assert_eq!(user.email, "alice@example.com");
    }

    fn sample_settings(
        username: &str,
        enabled: bool,
        now: chrono::NaiveDateTime,
    ) -> user_notification_settings::Model {
        user_notification_settings::Model {
            username: username.to_string(),
            enabled,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn notification_preferences_response_uses_settings_defaults_and_overrides() {
        let now = chrono::Utc::now().naive_utc();
        let response = build_notification_preferences_response(
            "alice",
            Some(sample_settings("alice", true, now)),
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

        assert_eq!(response.settings.username, "alice");
        assert!(response.settings.enabled);
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
        assert_eq!(response.settings.username, "alice");
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

    #[test]
    fn normalize_notification_preferences_trims_and_rejects_duplicates() {
        let preferences =
            normalize_notification_preferences(vec![UserNotificationPreferenceItem {
                event_type_code: " cl.comment.created ".to_string(),
                enabled: true,
            }])
            .unwrap();
        assert_eq!(preferences[0].event_type_code, "cl.comment.created");
        assert!(preferences[0].enabled);

        assert!(
            normalize_notification_preferences(vec![
                UserNotificationPreferenceItem {
                    event_type_code: "cl.comment.created".to_string(),
                    enabled: true,
                },
                UserNotificationPreferenceItem {
                    event_type_code: " cl.comment.created ".to_string(),
                    enabled: false,
                },
            ])
            .is_err()
        );
    }
}
