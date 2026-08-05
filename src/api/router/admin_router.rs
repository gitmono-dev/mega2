//! Admin-related API endpoints.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState, api_common::group_permission::ensure_admin, api_doc::USER_TAG,
        oauth::model::LoginUser,
    },
    callisto::notification_event_types,
    ceres::model::notification::NotificationEventTypeInfo,
    common::errors::ApiError,
    contract::api::common::CommonResult,
};

#[derive(Serialize, ToSchema)]
pub struct IsAdminResponse {
    pub is_admin: bool,
}

#[derive(Serialize, ToSchema)]
pub struct AdminListResponse {
    pub admins: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationEventTypeListResponse {
    pub event_types: Vec<NotificationEventTypeInfo>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNotificationEventTypeRequest {
    pub category: String,
    pub description: String,
    pub system_required: bool,
    pub default_enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationEventTypeResponse {
    pub event_type: NotificationEventTypeInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationEventTypeInput {
    category: String,
    description: String,
    system_required: bool,
    default_enabled: bool,
}

/// Build the admin router.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/admin",
        OpenApiRouter::new()
            .routes(routes!(is_admin_me))
            .routes(routes!(admin_list))
            .routes(routes!(list_notification_event_types))
            .routes(routes!(update_notification_event_type)),
    )
}

/// GET /api/v1/admin/me
#[utoipa::path(
    get,
    path = "/me",
    responses(
        (status = 200, body = CommonResult<IsAdminResponse>),
        (status = 401, description = "Unauthorized"),
    ),
    tag = USER_TAG
)]
async fn is_admin_me(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<IsAdminResponse>>, ApiError> {
    let is_admin = state.monorepo().check_is_admin(&user.username).await?;

    Ok(Json(CommonResult::success(Some(IsAdminResponse {
        is_admin,
    }))))
}

/// GET /api/v1/admin/list
#[utoipa::path(
    get,
    path = "/list",
    responses(
        (status = 200, body = CommonResult<AdminListResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = USER_TAG
)]
async fn admin_list(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<AdminListResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let admins = state.monorepo().get_all_admins().await?;

    Ok(Json(CommonResult::success(Some(AdminListResponse {
        admins,
    }))))
}

/// GET /api/v1/admin/notification-event-types
#[utoipa::path(
    get,
    path = "/notification-event-types",
    responses(
        (status = 200, body = CommonResult<NotificationEventTypeListResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = USER_TAG
)]
async fn list_notification_event_types(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<NotificationEventTypeListResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let event_types = state
        .storage
        .notification_storage()
        .list_event_types()
        .await?;

    Ok(Json(CommonResult::success(Some(
        NotificationEventTypeListResponse {
            event_types: event_types
                .into_iter()
                .map(NotificationEventTypeInfo::from)
                .collect(),
        },
    ))))
}

/// PUT /api/v1/admin/notification-event-types/{code}
#[utoipa::path(
    put,
    path = "/notification-event-types/{code}",
    params(
        ("code" = String, Path, description = "Notification event type code")
    ),
    request_body = UpdateNotificationEventTypeRequest,
    responses(
        (status = 200, body = CommonResult<NotificationEventTypeResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid notification event type payload"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = USER_TAG
)]
async fn update_notification_event_type(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(code): Path<String>,
    Json(payload): Json<UpdateNotificationEventTypeRequest>,
) -> Result<Json<CommonResult<NotificationEventTypeResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let code = validate_notification_event_type_code(&code)?;
    let input = normalize_notification_event_type_input(payload)?;
    let event_type = state
        .storage
        .notification_storage()
        .upsert_event_type(
            &code,
            &input.category,
            &input.description,
            input.system_required,
            input.default_enabled,
        )
        .await?;

    Ok(Json(CommonResult::success(Some(
        NotificationEventTypeResponse {
            event_type: NotificationEventTypeInfo::from(event_type),
        },
    ))))
}

impl From<notification_event_types::Model> for NotificationEventTypeInfo {
    fn from(value: notification_event_types::Model) -> Self {
        Self {
            code: value.code,
            category: value.category,
            description: value.description,
            system_required: value.system_required,
            default_enabled: value.default_enabled,
        }
    }
}

fn validate_notification_event_type_code(code: &str) -> Result<String, ApiError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "notification event type code must not be empty"
        )));
    }
    Ok(code.to_string())
}

fn normalize_notification_event_type_input(
    input: UpdateNotificationEventTypeRequest,
) -> Result<NotificationEventTypeInput, ApiError> {
    let category = input.category.trim();
    if category.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "notification event type category must not be empty"
        )));
    }

    let description = input.description.trim();
    if description.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "notification event type description must not be empty"
        )));
    }

    Ok(NotificationEventTypeInput {
        category: category.to_string(),
        description: description.to_string(),
        system_required: input.system_required,
        default_enabled: input.default_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_router_creation() {
        let _router = routers();
    }

    #[test]
    fn notification_event_type_info_maps_from_model() {
        let now = chrono::Utc::now().naive_utc();
        let response = NotificationEventTypeInfo::from(notification_event_types::Model {
            code: "cl.comment.created".to_string(),
            category: "cl".to_string(),
            description: "New comment on a Change List".to_string(),
            system_required: false,
            default_enabled: true,
            created_at: now,
            updated_at: now,
        });

        assert_eq!(response.code, "cl.comment.created");
        assert_eq!(response.category, "cl");
    }

    #[test]
    fn notification_event_type_input_rejects_blank_fields() {
        assert!(validate_notification_event_type_code(" ").is_err());
        assert!(
            normalize_notification_event_type_input(UpdateNotificationEventTypeRequest {
                category: " ".to_string(),
                description: "description".to_string(),
                system_required: false,
                default_enabled: true,
            })
            .is_err()
        );
    }
}
