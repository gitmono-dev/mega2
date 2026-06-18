//! Admin-related API endpoints.
//!
//! Provides endpoints for admin permission checks:
//! - `GET /api/v1/admin/me` - Check if current user is admin
//! - `GET /api/v1/admin/list` - List all admins (admin-only)
//!
//! # Auth Behavior
//! - 401 Unauthorized: No valid session (handled by `LoginUser` extractor)
//! - 403 Forbidden: Logged in but not admin (for `/list` endpoint)

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        api_common::group_permission::ensure_admin,
        api_doc::{MAIL_TAG, USER_TAG},
        oauth::model::LoginUser,
    },
    callisto::email_jobs,
    common::errors::ApiError,
    contract::api::common::{CommonPage, CommonResult, PageParams, Pagination},
    jupiter::storage::notification_storage::{
        EMAIL_JOB_STATUS_FAILED, EMAIL_JOB_STATUS_PENDING, EMAIL_JOB_STATUS_SENDING,
        EMAIL_JOB_STATUS_SENT, EMAIL_JOB_STATUS_SKIPPED, EmailJobListFilter,
        EmailJobRetryDisposition, EmailJobStats,
    },
};

#[derive(Serialize, ToSchema)]
pub struct IsAdminResponse {
    pub is_admin: bool,
}

#[derive(Serialize, ToSchema)]
pub struct AdminListResponse {
    pub admins: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailJobListRequest {
    pub status: Option<String>,
    pub username: Option<String>,
    pub event_type_code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobResponse {
    pub id: i64,
    pub username: String,
    pub to_email: String,
    pub event_type_code: String,
    pub subject: String,
    pub status: String,
    pub error_message: Option<String>,
    pub retry_count: i32,
    pub next_retry_at: Option<String>,
    pub sent_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobStatsResponse {
    pub total: u64,
    pub pending: u64,
    pub sending: u64,
    pub sent: u64,
    pub failed: u64,
    pub skipped: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobRetryResponse {
    pub job: EmailJobResponse,
}

/// Build the admin router.
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/admin",
        OpenApiRouter::new()
            .routes(routes!(is_admin_me))
            .routes(routes!(admin_list))
            .routes(routes!(list_email_jobs))
            .routes(routes!(email_job_stats))
            .routes(routes!(retry_failed_email_job)),
    )
}

/// GET /api/v1/admin/me
///
/// Returns whether the current user is an admin.
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
///
/// Returns a list of all admin usernames.
/// Only admins can access this endpoint.
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

/// POST /api/v1/admin/email-jobs/list
///
/// Lists notification email outbox jobs. Only admins can access this endpoint.
#[utoipa::path(
    post,
    path = "/email-jobs/list",
    request_body = PageParams<EmailJobListRequest>,
    responses(
        (status = 200, body = CommonResult<CommonPage<EmailJobResponse>>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn list_email_jobs(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(json): Json<PageParams<EmailJobListRequest>>,
) -> Result<Json<CommonResult<CommonPage<EmailJobResponse>>>, ApiError> {
    ensure_admin(&state, &user).await?;
    validate_email_job_pagination(&json.pagination)?;

    let filter = EmailJobListFilter {
        status: normalize_email_job_status(json.additional.status)?,
        username: trim_optional(json.additional.username),
        event_type_code: trim_optional(json.additional.event_type_code),
    };
    let (items, total) = state
        .storage
        .notification_storage()
        .list_email_jobs(filter, json.pagination)
        .await?;

    Ok(Json(CommonResult::success(Some(CommonPage {
        total,
        items: items.into_iter().map(EmailJobResponse::from).collect(),
    }))))
}

/// GET /api/v1/admin/email-jobs/stats
///
/// Returns notification email outbox counts by status. Only admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/email-jobs/stats",
    responses(
        (status = 200, body = CommonResult<EmailJobStatsResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn email_job_stats(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<EmailJobStatsResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let stats = state
        .storage
        .notification_storage()
        .email_job_stats()
        .await?;
    Ok(Json(CommonResult::success(Some(stats.into()))))
}

/// POST /api/v1/admin/email-jobs/{id}/retry
///
/// Requeues a failed notification email outbox job. Only admins can access this endpoint.
#[utoipa::path(
    post,
    path = "/email-jobs/{id}/retry",
    params(
        ("id" = i64, Path, description = "Email job ID")
    ),
    responses(
        (status = 200, body = CommonResult<EmailJobRetryResponse>, content_type = "application/json"),
        (status = 400, description = "Email job is not in failed status"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
        (status = 404, description = "Email job not found"),
    ),
    tag = MAIL_TAG
)]
async fn retry_failed_email_job(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(id): Path<i64>,
) -> Result<Json<CommonResult<EmailJobRetryResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    match state
        .storage
        .notification_storage()
        .retry_failed_email_job(id)
        .await?
    {
        EmailJobRetryDisposition::Queued(job) => {
            Ok(Json(CommonResult::success(Some(EmailJobRetryResponse {
                job: EmailJobResponse::from(*job),
            }))))
        }
        EmailJobRetryDisposition::MissingJob => {
            Err(ApiError::not_found(anyhow::anyhow!("email job not found")))
        }
        EmailJobRetryDisposition::NotRetryable { status } => Err(ApiError::bad_request(
            anyhow::anyhow!("email job is not retryable from status `{status}`"),
        )),
    }
}

impl From<email_jobs::Model> for EmailJobResponse {
    fn from(value: email_jobs::Model) -> Self {
        Self {
            id: value.id,
            username: value.username,
            to_email: value.to_email,
            event_type_code: value.event_type_code,
            subject: value.subject,
            status: value.status,
            error_message: value.error_message,
            retry_count: value.retry_count,
            next_retry_at: value.next_retry_at.map(|dt| dt.to_string()),
            sent_at: value.sent_at.map(|dt| dt.to_string()),
            created_at: value.created_at.to_string(),
            updated_at: value.updated_at.to_string(),
        }
    }
}

impl From<EmailJobStats> for EmailJobStatsResponse {
    fn from(value: EmailJobStats) -> Self {
        Self {
            total: value.total,
            pending: value.pending,
            sending: value.sending,
            sent: value.sent,
            failed: value.failed,
            skipped: value.skipped,
        }
    }
}

fn validate_email_job_pagination(pagination: &Pagination) -> Result<(), ApiError> {
    if pagination.page == 0 {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "page must be greater than 0"
        )));
    }
    if pagination.per_page == 0 || pagination.per_page > 100 {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "per_page must be between 1 and 100"
        )));
    }
    Ok(())
}

fn normalize_email_job_status(status: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(status) = trim_optional(status) else {
        return Ok(None);
    };
    let status = status.to_ascii_lowercase();
    if matches!(
        status.as_str(),
        EMAIL_JOB_STATUS_PENDING
            | EMAIL_JOB_STATUS_SENDING
            | EMAIL_JOB_STATUS_SENT
            | EMAIL_JOB_STATUS_FAILED
            | EMAIL_JOB_STATUS_SKIPPED
    ) {
        Ok(Some(status))
    } else {
        Err(ApiError::bad_request(anyhow::anyhow!(
            "invalid email job status `{status}`"
        )))
    }
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}
