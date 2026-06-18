//! Admin-related API endpoints.
//!
//! Provides endpoints for admin permission checks:
//! - `GET /api/v1/admin/me` - Check if current user is admin
//! - `GET /api/v1/admin/list` - List all admins (admin-only)
//!
//! # Auth Behavior
//! - 401 Unauthorized: No valid session (handled by `LoginUser` extractor)
//! - 403 Forbidden: Logged in but not admin (for `/list` endpoint)

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path as FsPath, PathBuf},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::Response,
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
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
    callisto::{email_jobs, notification_event_types},
    ceres::model::notification::NotificationEventTypeInfo,
    common::errors::ApiError,
    config::MailConfig,
    contract::api::common::{CommonPage, CommonResult, PageParams, Pagination},
    jupiter::storage::notification_storage::{
        EMAIL_JOB_STATUS_FAILED, EMAIL_JOB_STATUS_PENDING, EMAIL_JOB_STATUS_SENDING,
        EMAIL_JOB_STATUS_SENT, EMAIL_JOB_STATUS_SKIPPED, EmailJobAttachmentMetadata,
        EmailJobListFilter, EmailJobRetryDisposition, EmailJobStats,
    },
    mail::template::{
        LocalizedMailTemplate, MailTemplate, MailTemplateKey,
        load_localized_template_sources_from_dir, localized_template_to_toml,
    },
    notification::triggers::{
        configure_notification_mail_template_registry, default_notification_mail_template_registry,
        notification_mail_template_registry_from_config,
    },
};

const MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS: i64 = 3650;
const MAIL_TEMPLATE_SOURCE_BUILT_IN: &str = "built-in";
const MAIL_TEMPLATE_SOURCE_EXTERNAL: &str = "external";
const CONTENT_DISPOSITION_VALUE_CHARS: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'{')
    .add(b'}');

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

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobAttachmentResponse {
    pub id: i64,
    pub email_job_id: i64,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub created_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobAttachmentListResponse {
    pub attachments: Vec<EmailJobAttachmentResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobAttachmentDeleteResponse {
    pub id: i64,
    pub email_job_id: i64,
    pub deleted: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailJobAttachmentPruneRequest {
    pub older_than_days: i64,
    pub statuses: Option<Vec<String>>,
    pub username: Option<String>,
    pub event_type_code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobAttachmentPruneResponse {
    pub deleted: u64,
    pub statuses: Vec<String>,
    pub older_than: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailJobPruneRequest {
    pub older_than_days: i64,
    pub statuses: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmailJobPruneResponse {
    pub deleted: u64,
    pub statuses: Vec<String>,
    pub older_than: String,
}

#[derive(Debug, Clone)]
struct EmailJobPruneInput {
    older_than_days: i64,
    older_than: chrono::NaiveDateTime,
    statuses: Vec<String>,
}

#[derive(Debug, Clone)]
struct EmailJobAttachmentPruneInput {
    older_than_days: i64,
    older_than: chrono::NaiveDateTime,
    statuses: Vec<String>,
    username: Option<String>,
    event_type_code: Option<String>,
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

#[derive(Debug, Serialize, ToSchema)]
pub struct MailTemplateListResponse {
    pub default_locale: String,
    pub template_dir: Option<String>,
    pub templates: Vec<MailTemplateAuditResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MailTemplateAuditResponse {
    pub key: String,
    pub locale: String,
    pub source: String,
    pub source_path: Option<String>,
    pub subject_template: String,
    pub html_template: String,
    pub text_template: Option<String>,
    pub overridden: bool,
    pub overrides_builtin: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MailTemplatePreviewRequest {
    pub key: String,
    pub locale: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MailTemplatePreviewResponse {
    pub subject: String,
    pub html: String,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertMailTemplateRequest {
    pub subject: String,
    pub html: String,
    pub text: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MailTemplateUpsertResponse {
    pub created: bool,
    pub registry_reloaded: bool,
    pub template: MailTemplateAuditResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationEventTypeInput {
    category: String,
    description: String,
    system_required: bool,
    default_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MailTemplatePreviewInput {
    key: String,
    locale: Option<String>,
    variables: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MailTemplateUpsertInput {
    template: LocalizedMailTemplate,
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
            .routes(routes!(list_email_job_attachments))
            .routes(routes!(download_email_job_attachment))
            .routes(routes!(delete_email_job_attachment))
            .routes(routes!(prune_email_job_attachments))
            .routes(routes!(retry_failed_email_job))
            .routes(routes!(prune_email_jobs))
            .routes(routes!(list_mail_templates))
            .routes(routes!(preview_mail_template))
            .routes(routes!(upsert_mail_template))
            .routes(routes!(list_notification_event_types))
            .routes(routes!(update_notification_event_type)),
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

/// GET /api/v1/admin/email-jobs/{id}/attachments
///
/// Lists persisted attachment metadata for a notification email outbox job.
/// Only admins can access this endpoint. Attachment content is not returned.
#[utoipa::path(
    get,
    path = "/email-jobs/{id}/attachments",
    params(
        ("id" = i64, Path, description = "Email job ID")
    ),
    responses(
        (status = 200, body = CommonResult<EmailJobAttachmentListResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
        (status = 404, description = "Email job not found"),
    ),
    tag = MAIL_TAG
)]
async fn list_email_job_attachments(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(id): Path<i64>,
) -> Result<Json<CommonResult<EmailJobAttachmentListResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let notification_storage = state.storage.notification_storage();
    if notification_storage.get_email_job(id).await?.is_none() {
        return Err(ApiError::not_found(anyhow::anyhow!("email job not found")));
    }

    let attachments = notification_storage
        .list_email_job_attachment_metadata(id)
        .await?;

    Ok(Json(CommonResult::success(Some(
        EmailJobAttachmentListResponse {
            attachments: attachments
                .into_iter()
                .map(EmailJobAttachmentResponse::from)
                .collect(),
        },
    ))))
}

/// GET /api/v1/admin/email-jobs/{job_id}/attachments/{attachment_id}/content
///
/// Downloads persisted attachment content for a notification email outbox job.
/// Only admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/email-jobs/{job_id}/attachments/{attachment_id}/content",
    params(
        ("job_id" = i64, Path, description = "Email job ID"),
        ("attachment_id" = i64, Path, description = "Email job attachment ID")
    ),
    responses(
        (status = 200, description = "Email job attachment content", content_type = "application/octet-stream"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
        (status = 404, description = "Email job or attachment not found"),
    ),
    tag = MAIL_TAG
)]
async fn download_email_job_attachment(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path((job_id, attachment_id)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    ensure_admin(&state, &user).await?;

    let notification_storage = state.storage.notification_storage();
    if notification_storage.get_email_job(job_id).await?.is_none() {
        return Err(ApiError::not_found(anyhow::anyhow!("email job not found")));
    }

    let Some(attachment) = notification_storage
        .get_email_job_attachment_content(job_id, attachment_id)
        .await?
    else {
        return Err(ApiError::not_found(anyhow::anyhow!(
            "email job attachment not found"
        )));
    };

    let content_length = attachment.content.len().to_string();
    let content_disposition = email_attachment_content_disposition(&attachment.filename);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, attachment.content_type)
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .body(Body::from(attachment.content))
        .map_err(ApiError::internal)
}

/// DELETE /api/v1/admin/email-jobs/{job_id}/attachments/{attachment_id}
///
/// Deletes a persisted attachment from a notification email outbox job.
/// Only admins can access this endpoint.
#[utoipa::path(
    delete,
    path = "/email-jobs/{job_id}/attachments/{attachment_id}",
    params(
        ("job_id" = i64, Path, description = "Email job ID"),
        ("attachment_id" = i64, Path, description = "Email job attachment ID")
    ),
    responses(
        (status = 200, body = CommonResult<EmailJobAttachmentDeleteResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
        (status = 404, description = "Email job or attachment not found"),
    ),
    tag = MAIL_TAG
)]
async fn delete_email_job_attachment(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path((job_id, attachment_id)): Path<(i64, i64)>,
) -> Result<Json<CommonResult<EmailJobAttachmentDeleteResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;

    let notification_storage = state.storage.notification_storage();
    if notification_storage.get_email_job(job_id).await?.is_none() {
        return Err(ApiError::not_found(anyhow::anyhow!("email job not found")));
    }

    if !notification_storage
        .delete_email_job_attachment(job_id, attachment_id)
        .await?
    {
        return Err(ApiError::not_found(anyhow::anyhow!(
            "email job attachment not found"
        )));
    }

    Ok(Json(CommonResult::success(Some(
        EmailJobAttachmentDeleteResponse {
            id: attachment_id,
            email_job_id: job_id,
            deleted: true,
        },
    ))))
}

/// POST /api/v1/admin/email-jobs/attachments/prune
///
/// Deletes persisted attachments for old terminal notification email outbox jobs.
/// Only admins can access this endpoint. Email job records are retained.
#[utoipa::path(
    post,
    path = "/email-jobs/attachments/prune",
    request_body = EmailJobAttachmentPruneRequest,
    responses(
        (status = 200, body = CommonResult<EmailJobAttachmentPruneResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid attachment prune request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn prune_email_job_attachments(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(payload): Json<EmailJobAttachmentPruneRequest>,
) -> Result<Json<CommonResult<EmailJobAttachmentPruneResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let input = normalize_email_job_attachment_prune_request(payload)?;

    let deleted = state
        .storage
        .notification_storage()
        .prune_email_job_attachments(
            &input.statuses,
            input.older_than,
            input.username.as_deref(),
            input.event_type_code.as_deref(),
        )
        .await?;

    Ok(Json(CommonResult::success(Some(
        EmailJobAttachmentPruneResponse {
            deleted,
            statuses: input.statuses,
            older_than: input.older_than.to_string(),
        },
    ))))
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

/// POST /api/v1/admin/email-jobs/prune
///
/// Deletes old terminal notification email outbox jobs. Only admins can access this endpoint.
#[utoipa::path(
    post,
    path = "/email-jobs/prune",
    request_body = EmailJobPruneRequest,
    responses(
        (status = 200, body = CommonResult<EmailJobPruneResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid prune request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn prune_email_jobs(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(payload): Json<EmailJobPruneRequest>,
) -> Result<Json<CommonResult<EmailJobPruneResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let input = normalize_email_job_prune_request(payload)?;

    let deleted = state
        .storage
        .notification_storage()
        .prune_email_jobs(&input.statuses, input.older_than)
        .await?;

    Ok(Json(CommonResult::success(Some(EmailJobPruneResponse {
        deleted,
        statuses: input.statuses,
        older_than: input.older_than.to_string(),
    }))))
}

/// GET /api/v1/admin/mail-templates
///
/// Lists built-in and configured external mail templates. Only admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/mail-templates",
    responses(
        (status = 200, body = CommonResult<MailTemplateListResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn list_mail_templates(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<MailTemplateListResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let mail_config = state.storage.config().mail.clone().unwrap_or_default();
    let response = build_mail_template_list_response(&mail_config)?;

    Ok(Json(CommonResult::success(Some(response))))
}

/// POST /api/v1/admin/mail-templates/preview
///
/// Renders a mail template with admin-supplied variables. Only admins can access this endpoint.
#[utoipa::path(
    post,
    path = "/mail-templates/preview",
    request_body = MailTemplatePreviewRequest,
    responses(
        (status = 200, body = CommonResult<MailTemplatePreviewResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid template preview request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn preview_mail_template(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Json(payload): Json<MailTemplatePreviewRequest>,
) -> Result<Json<CommonResult<MailTemplatePreviewResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let mail_config = state.storage.config().mail.clone().unwrap_or_default();
    let response = render_mail_template_preview(&mail_config, payload)?;

    Ok(Json(CommonResult::success(Some(response))))
}

/// PUT /api/v1/admin/mail-templates/{key}/{locale}
///
/// Creates or updates an external mail template file under `mail.template_dir`.
/// Only admins can access this endpoint.
#[utoipa::path(
    put,
    path = "/mail-templates/{key}/{locale}",
    params(
        ("key" = String, Path, description = "Mail template key"),
        ("locale" = String, Path, description = "Mail template locale")
    ),
    request_body = UpsertMailTemplateRequest,
    responses(
        (status = 200, body = CommonResult<MailTemplateUpsertResponse>, content_type = "application/json"),
        (status = 400, description = "Invalid template update request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
)]
async fn upsert_mail_template(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path((key, locale)): Path<(String, String)>,
    Json(payload): Json<UpsertMailTemplateRequest>,
) -> Result<Json<CommonResult<MailTemplateUpsertResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let mail_config = state.storage.config().mail.clone().unwrap_or_default();
    let mut response = upsert_mail_template_file(&mail_config, key, locale, payload)?;

    let registry = notification_mail_template_registry_from_config(&mail_config)
        .map_err(ApiError::internal)?;
    configure_notification_mail_template_registry(registry).map_err(ApiError::internal)?;
    response.registry_reloaded = true;

    Ok(Json(CommonResult::success(Some(response))))
}

/// GET /api/v1/admin/notification-event-types
///
/// Lists notification event types. Only admins can access this endpoint.
#[utoipa::path(
    get,
    path = "/notification-event-types",
    responses(
        (status = 200, body = CommonResult<NotificationEventTypeListResponse>, content_type = "application/json"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - not admin"),
    ),
    tag = MAIL_TAG
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
///
/// Creates or updates a notification event type. Only admins can access this endpoint.
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
    tag = MAIL_TAG
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

impl From<EmailJobAttachmentMetadata> for EmailJobAttachmentResponse {
    fn from(value: EmailJobAttachmentMetadata) -> Self {
        Self {
            id: value.id,
            email_job_id: value.email_job_id,
            filename: value.filename,
            content_type: value.content_type,
            size_bytes: value.size_bytes,
            created_at: value.created_at.to_string(),
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

fn build_mail_template_list_response(
    mail_config: &MailConfig,
) -> Result<MailTemplateListResponse, ApiError> {
    let built_in_registry = default_notification_mail_template_registry();
    let external_sources = match &mail_config.template_dir {
        Some(template_dir) => {
            load_localized_template_sources_from_dir(template_dir).map_err(ApiError::internal)?
        }
        None => Vec::new(),
    };

    let built_in_identities: HashSet<(String, String)> = built_in_registry
        .templates()
        .iter()
        .map(mail_template_identity)
        .collect();
    let external_identities: HashSet<(String, String)> = external_sources
        .iter()
        .map(|source| mail_template_identity(source.template()))
        .collect();

    let mut templates =
        Vec::with_capacity(built_in_registry.templates().len() + external_sources.len());
    templates.extend(built_in_registry.templates().iter().map(|template| {
        mail_template_audit_response(
            template,
            MAIL_TEMPLATE_SOURCE_BUILT_IN,
            None,
            external_identities.contains(&mail_template_identity(template)),
            false,
        )
    }));
    templates.extend(external_sources.iter().map(|source| {
        let template = source.template();
        mail_template_audit_response(
            template,
            MAIL_TEMPLATE_SOURCE_EXTERNAL,
            Some(source.source_path().display().to_string()),
            false,
            built_in_identities.contains(&mail_template_identity(template)),
        )
    }));
    templates.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then(left.locale.cmp(&right.locale))
            .then(left.source.cmp(&right.source))
    });

    Ok(MailTemplateListResponse {
        default_locale: mail_config.template_default_locale.clone(),
        template_dir: mail_config
            .template_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        templates,
    })
}

fn mail_template_identity(template: &LocalizedMailTemplate) -> (String, String) {
    (
        template.key().as_str().to_string(),
        template.locale().to_string(),
    )
}

fn mail_template_audit_response(
    template: &LocalizedMailTemplate,
    source: &str,
    source_path: Option<String>,
    overridden: bool,
    overrides_builtin: bool,
) -> MailTemplateAuditResponse {
    MailTemplateAuditResponse {
        key: template.key().as_str().to_string(),
        locale: template.locale().to_string(),
        source: source.to_string(),
        source_path,
        subject_template: template.template().subject_template().to_string(),
        html_template: template.template().html_template().to_string(),
        text_template: template.template().text_template().map(str::to_string),
        overridden,
        overrides_builtin,
    }
}

fn render_mail_template_preview(
    mail_config: &MailConfig,
    input: MailTemplatePreviewRequest,
) -> Result<MailTemplatePreviewResponse, ApiError> {
    let input = normalize_mail_template_preview_request(input)?;
    let registry =
        notification_mail_template_registry_from_config(mail_config).map_err(ApiError::internal)?;
    let variables: Vec<(&str, &str)> = input
        .variables
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let rendered = registry
        .render(
            &MailTemplateKey::new(input.key),
            input.locale.as_deref(),
            &variables,
        )
        .map_err(ApiError::bad_request)?;

    Ok(MailTemplatePreviewResponse {
        subject: rendered.subject,
        html: rendered.html,
        text: rendered.text,
    })
}

fn upsert_mail_template_file(
    mail_config: &MailConfig,
    key: String,
    locale: String,
    payload: UpsertMailTemplateRequest,
) -> Result<MailTemplateUpsertResponse, ApiError> {
    let input = normalize_mail_template_upsert_request(key, locale, payload)?;
    let template_dir = mail_template_dir(mail_config)?;
    let built_in_identities: HashSet<(String, String)> =
        default_notification_mail_template_registry()
            .templates()
            .iter()
            .map(mail_template_identity)
            .collect();
    let (write_path, created) = resolve_mail_template_write_path(template_dir, &input.template)?;
    write_localized_mail_template(&write_path, &input.template)?;

    Ok(MailTemplateUpsertResponse {
        created,
        registry_reloaded: false,
        template: mail_template_audit_response(
            &input.template,
            MAIL_TEMPLATE_SOURCE_EXTERNAL,
            Some(write_path.display().to_string()),
            false,
            built_in_identities.contains(&mail_template_identity(&input.template)),
        ),
    })
}

fn mail_template_dir(mail_config: &MailConfig) -> Result<&FsPath, ApiError> {
    mail_config.template_dir.as_deref().ok_or_else(|| {
        ApiError::bad_request(anyhow::anyhow!(
            "mail.template_dir must be configured before templates can be edited"
        ))
    })
}

fn resolve_mail_template_write_path(
    template_dir: &FsPath,
    template: &LocalizedMailTemplate,
) -> Result<(PathBuf, bool), ApiError> {
    if !template_dir.is_dir() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "mail.template_dir must point to an existing directory"
        )));
    }

    let identity = mail_template_identity(template);
    let existing_sources =
        load_localized_template_sources_from_dir(template_dir).map_err(ApiError::internal)?;
    if let Some(source) = existing_sources
        .iter()
        .find(|source| mail_template_identity(source.template()) == identity)
    {
        let path = source.source_path().to_path_buf();
        validate_mail_template_write_path(template_dir, &path)?;
        return Ok((path, false));
    }

    let filename = format!("{}__{}.toml", template.key().as_str(), template.locale());
    let path = template_dir.join(filename);
    if existing_sources
        .iter()
        .any(|source| source.source_path() == path)
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "derived mail template file is already used by another template"
        )));
    }
    validate_mail_template_write_path(template_dir, &path)?;
    Ok((path, true))
}

fn validate_mail_template_write_path(template_dir: &FsPath, path: &FsPath) -> Result<(), ApiError> {
    if path.parent() != Some(template_dir) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "mail template path must stay inside mail.template_dir"
        )));
    }

    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(ApiError::internal)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "mail template path must be a regular file"
            )));
        }
    }

    Ok(())
}

fn write_localized_mail_template(
    path: &FsPath,
    template: &LocalizedMailTemplate,
) -> Result<(), ApiError> {
    let contents = localized_template_to_toml(template).map_err(ApiError::internal)?;
    fs::write(path, contents).map_err(ApiError::internal)
}

fn normalize_mail_template_preview_request(
    input: MailTemplatePreviewRequest,
) -> Result<MailTemplatePreviewInput, ApiError> {
    let key = normalize_mail_template_identifier("mail template key", &input.key)?;
    let locale = trim_optional(input.locale)
        .map(|locale| normalize_mail_template_identifier("mail template locale", &locale))
        .transpose()?;
    let mut seen_variables = HashSet::new();
    let mut variables = Vec::with_capacity(input.variables.len());

    for (name, value) in input.variables {
        let name = normalize_mail_template_identifier("mail template variable", &name)?;
        if !seen_variables.insert(name.clone()) {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "mail template variables contain duplicate `{name}` entries"
            )));
        }
        variables.push((name, value));
    }

    Ok(MailTemplatePreviewInput {
        key,
        locale,
        variables,
    })
}

fn normalize_mail_template_upsert_request(
    key: String,
    locale: String,
    input: UpsertMailTemplateRequest,
) -> Result<MailTemplateUpsertInput, ApiError> {
    let key = normalize_mail_template_identifier("mail template key", &key)?;
    let locale = normalize_mail_template_identifier("mail template locale", &locale)?;
    if input.subject.trim().is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "mail template subject must not be empty"
        )));
    }
    if input.html.trim().is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "mail template html must not be empty"
        )));
    }

    let template = MailTemplate::new(input.subject, input.html, input.text.as_deref());
    template.validate_syntax().map_err(ApiError::bad_request)?;

    Ok(MailTemplateUpsertInput {
        template: LocalizedMailTemplate::new(MailTemplateKey::new(key), locale, template),
    })
}

fn normalize_mail_template_identifier(field: &str, value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "{field} must not be empty"
        )));
    }

    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        Ok(value.to_string())
    } else {
        Err(ApiError::bad_request(anyhow::anyhow!(
            "{field} contains unsupported characters"
        )))
    }
}

fn normalize_email_job_prune_request(
    input: EmailJobPruneRequest,
) -> Result<EmailJobPruneInput, ApiError> {
    if input.older_than_days < 1 || input.older_than_days > MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "older_than_days must be between 1 and {MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS}"
        )));
    }

    let statuses = normalize_prunable_email_job_statuses(input.statuses)?;
    let older_than = chrono::Utc::now().naive_utc() - chrono::Duration::days(input.older_than_days);

    Ok(EmailJobPruneInput {
        older_than_days: input.older_than_days,
        older_than,
        statuses,
    })
}

fn normalize_email_job_attachment_prune_request(
    input: EmailJobAttachmentPruneRequest,
) -> Result<EmailJobAttachmentPruneInput, ApiError> {
    if input.older_than_days < 1 || input.older_than_days > MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "older_than_days must be between 1 and {MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS}"
        )));
    }

    let statuses = normalize_prunable_email_job_statuses(input.statuses)?;
    let older_than = chrono::Utc::now().naive_utc() - chrono::Duration::days(input.older_than_days);

    Ok(EmailJobAttachmentPruneInput {
        older_than_days: input.older_than_days,
        older_than,
        statuses,
        username: trim_optional(input.username),
        event_type_code: trim_optional(input.event_type_code),
    })
}

fn normalize_prunable_email_job_statuses(
    statuses: Option<Vec<String>>,
) -> Result<Vec<String>, ApiError> {
    let statuses = match statuses {
        Some(statuses) if !statuses.is_empty() => statuses,
        _ => vec![
            EMAIL_JOB_STATUS_SENT.to_string(),
            EMAIL_JOB_STATUS_SKIPPED.to_string(),
        ],
    };
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(statuses.len());

    for status in statuses {
        let status = status.trim().to_ascii_lowercase();
        if !matches!(
            status.as_str(),
            EMAIL_JOB_STATUS_SENT | EMAIL_JOB_STATUS_SKIPPED
        ) {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "email job prune status must be `{EMAIL_JOB_STATUS_SENT}` or `{EMAIL_JOB_STATUS_SKIPPED}`"
            )));
        }
        if seen.insert(status.clone()) {
            normalized.push(status);
        }
    }

    if normalized.is_empty() {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "email job prune statuses must not be empty"
        )));
    }

    Ok(normalized)
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

fn email_attachment_content_disposition(filename: &str) -> String {
    let fallback = sanitize_content_disposition_filename(filename);
    let encoded = utf8_percent_encode(filename, CONTENT_DISPOSITION_VALUE_CHARS);
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn sanitize_content_disposition_filename(filename: &str) -> String {
    let sanitized: String = filename
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_graphic() && !matches!(ch, '"' | '\\' | ';') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_router_creation() {
        let _router = routers();
    }

    #[test]
    fn build_mail_template_list_response_marks_external_overrides() {
        let dir = tempfile::tempdir().expect("temp dir");
        let template_path = dir.path().join("cl-comment.toml");
        std::fs::write(
            &template_path,
            r#"
key = "cl.comment.created"
locale = "en-US"
subject = "Override {{actor_username}}"
html = "<p>{{comment_text}}</p>"
text = "{{actor_username}}: {{comment_text}}"
"#,
        )
        .expect("write template");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };

        let response = build_mail_template_list_response(&mail_config).unwrap();

        assert_eq!(response.default_locale, "en-US");
        assert_eq!(
            response.template_dir.as_deref(),
            Some(dir.path().display().to_string().as_str())
        );
        let built_in = response
            .templates
            .iter()
            .find(|template| {
                template.key == "cl.comment.created"
                    && template.locale == "en-US"
                    && template.source == MAIL_TEMPLATE_SOURCE_BUILT_IN
            })
            .expect("built-in template");
        assert!(built_in.overridden);
        assert!(!built_in.overrides_builtin);
        assert_eq!(built_in.source_path, None);

        let external = response
            .templates
            .iter()
            .find(|template| {
                template.key == "cl.comment.created"
                    && template.locale == "en-US"
                    && template.source == MAIL_TEMPLATE_SOURCE_EXTERNAL
            })
            .expect("external template");
        assert!(!external.overridden);
        assert!(external.overrides_builtin);
        assert_eq!(external.subject_template, "Override {{actor_username}}");
        assert_eq!(
            external.source_path.as_deref(),
            Some(template_path.display().to_string().as_str())
        );
    }

    #[test]
    fn render_mail_template_preview_renders_external_template() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("custom.toml"),
            r#"
key = "custom.event"
locale = "en-US"
subject = "Hello {{name}}"
html = "<p>{{name}}</p>"
text = "Hello {{name}}"
"#,
        )
        .expect("write template");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };
        let response = render_mail_template_preview(
            &mail_config,
            MailTemplatePreviewRequest {
                key: " custom.event ".to_string(),
                locale: Some(" en-US ".to_string()),
                variables: BTreeMap::from([(" name ".to_string(), "<Alice>".to_string())]),
            },
        )
        .unwrap();

        assert_eq!(response.subject, "Hello <Alice>");
        assert_eq!(response.html, "<p>&lt;Alice&gt;</p>");
        assert_eq!(response.text.as_deref(), Some("Hello <Alice>"));
    }

    #[test]
    fn upsert_mail_template_file_creates_external_override() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };

        let response = upsert_mail_template_file(
            &mail_config,
            "cl.comment.created".to_string(),
            "en-US".to_string(),
            UpsertMailTemplateRequest {
                subject: "Override {{actor_username}}".to_string(),
                html: "<p>{{comment_text}}</p>".to_string(),
                text: Some("{{actor_username}}: {{comment_text}}".to_string()),
            },
        )
        .unwrap();

        assert!(response.created);
        assert!(!response.registry_reloaded);
        assert!(response.template.overrides_builtin);
        let source_path = response.template.source_path.as_deref().unwrap();
        assert!(std::path::Path::new(source_path).exists());

        let registry = notification_mail_template_registry_from_config(&mail_config).unwrap();
        let rendered = registry
            .render(
                &MailTemplateKey::new("cl.comment.created"),
                Some("en-US"),
                &[
                    ("actor_username", "alice"),
                    ("cl_link", "CL1"),
                    ("comment_text", "<hello>"),
                ],
            )
            .unwrap();

        assert_eq!(rendered.subject, "Override alice");
        assert_eq!(rendered.html, "<p>&lt;hello&gt;</p>");
        assert_eq!(rendered.text.as_deref(), Some("alice: <hello>"));
    }

    #[test]
    fn upsert_mail_template_file_reuses_existing_external_source() {
        let dir = tempfile::tempdir().expect("temp dir");
        let template_path = dir.path().join("custom-name.toml");
        std::fs::write(
            &template_path,
            r#"
key = "custom.event"
locale = "en-US"
subject = "Old {{name}}"
html = "<p>Old {{name}}</p>"
"#,
        )
        .expect("write template");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };

        let response = upsert_mail_template_file(
            &mail_config,
            "custom.event".to_string(),
            "en-US".to_string(),
            UpsertMailTemplateRequest {
                subject: "New {{name}}".to_string(),
                html: "<p>New {{name}}</p>".to_string(),
                text: None,
            },
        )
        .unwrap();

        assert!(!response.created);
        assert_eq!(
            response.template.source_path.as_deref(),
            Some(template_path.display().to_string().as_str())
        );
        assert!(!dir.path().join("custom.event__en-US.toml").exists());
        let updated = std::fs::read_to_string(&template_path).unwrap();
        assert!(updated.contains("subject = \"New {{name}}\""));
    }

    #[test]
    fn upsert_mail_template_file_rejects_unconfigured_dir_and_bad_syntax() {
        assert!(
            upsert_mail_template_file(
                &MailConfig::default(),
                "custom.event".to_string(),
                "en-US".to_string(),
                UpsertMailTemplateRequest {
                    subject: "Subject".to_string(),
                    html: "<p>Body</p>".to_string(),
                    text: None,
                },
            )
            .is_err()
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };
        assert!(
            upsert_mail_template_file(
                &mail_config,
                "custom.event".to_string(),
                "en-US".to_string(),
                UpsertMailTemplateRequest {
                    subject: "Subject {{name".to_string(),
                    html: "<p>{{name}}</p>".to_string(),
                    text: None,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn upsert_mail_template_file_rejects_derived_path_collision() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("custom.event__en-US.toml"),
            r#"
key = "other.event"
locale = "en-US"
subject = "Other {{name}}"
html = "<p>Other {{name}}</p>"
"#,
        )
        .expect("write template");
        let mail_config = MailConfig {
            template_dir: Some(dir.path().to_path_buf()),
            ..MailConfig::default()
        };

        assert!(
            upsert_mail_template_file(
                &mail_config,
                "custom.event".to_string(),
                "en-US".to_string(),
                UpsertMailTemplateRequest {
                    subject: "New {{name}}".to_string(),
                    html: "<p>New {{name}}</p>".to_string(),
                    text: None,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn normalize_mail_template_preview_request_rejects_unsafe_inputs() {
        assert!(
            normalize_mail_template_preview_request(MailTemplatePreviewRequest {
                key: " ".to_string(),
                locale: None,
                variables: BTreeMap::new(),
            })
            .is_err()
        );
        assert!(
            normalize_mail_template_preview_request(MailTemplatePreviewRequest {
                key: "cl.comment.created".to_string(),
                locale: Some("en/US".to_string()),
                variables: BTreeMap::new(),
            })
            .is_err()
        );
        assert!(
            normalize_mail_template_preview_request(MailTemplatePreviewRequest {
                key: "cl.comment.created".to_string(),
                locale: None,
                variables: BTreeMap::from([
                    ("name".to_string(), "alice".to_string()),
                    (" name ".to_string(), "bob".to_string()),
                ]),
            })
            .is_err()
        );
    }

    #[test]
    fn normalize_email_job_prune_request_defaults_to_sent_and_skipped() {
        let input = normalize_email_job_prune_request(EmailJobPruneRequest {
            older_than_days: 30,
            statuses: None,
        })
        .unwrap();

        assert_eq!(input.older_than_days, 30);
        assert_eq!(
            input.statuses,
            vec![
                EMAIL_JOB_STATUS_SENT.to_string(),
                EMAIL_JOB_STATUS_SKIPPED.to_string()
            ]
        );
    }

    #[test]
    fn normalize_email_job_prune_request_trims_and_deduplicates_statuses() {
        let input = normalize_email_job_prune_request(EmailJobPruneRequest {
            older_than_days: 7,
            statuses: Some(vec![
                " Sent ".to_string(),
                "sent".to_string(),
                " skipped ".to_string(),
            ]),
        })
        .unwrap();

        assert_eq!(
            input.statuses,
            vec![
                EMAIL_JOB_STATUS_SENT.to_string(),
                EMAIL_JOB_STATUS_SKIPPED.to_string()
            ]
        );
    }

    #[test]
    fn normalize_email_job_prune_request_rejects_unsafe_inputs() {
        assert!(
            normalize_email_job_prune_request(EmailJobPruneRequest {
                older_than_days: 0,
                statuses: None,
            })
            .is_err()
        );
        assert!(
            normalize_email_job_prune_request(EmailJobPruneRequest {
                older_than_days: MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS + 1,
                statuses: None,
            })
            .is_err()
        );
        assert!(
            normalize_email_job_prune_request(EmailJobPruneRequest {
                older_than_days: 7,
                statuses: Some(vec![EMAIL_JOB_STATUS_FAILED.to_string()]),
            })
            .is_err()
        );
    }

    #[test]
    fn normalize_email_job_attachment_prune_request_defaults_to_sent_and_skipped() {
        let input = normalize_email_job_attachment_prune_request(EmailJobAttachmentPruneRequest {
            older_than_days: 30,
            statuses: None,
            username: None,
            event_type_code: None,
        })
        .unwrap();

        assert_eq!(input.older_than_days, 30);
        assert_eq!(input.username, None);
        assert_eq!(input.event_type_code, None);
        assert_eq!(
            input.statuses,
            vec![
                EMAIL_JOB_STATUS_SENT.to_string(),
                EMAIL_JOB_STATUS_SKIPPED.to_string()
            ]
        );
    }

    #[test]
    fn normalize_email_job_attachment_prune_request_trims_filters() {
        let input = normalize_email_job_attachment_prune_request(EmailJobAttachmentPruneRequest {
            older_than_days: 7,
            statuses: Some(vec![EMAIL_JOB_STATUS_SENT.to_string()]),
            username: Some(" alice ".to_string()),
            event_type_code: Some(" cl.comment.created ".to_string()),
        })
        .unwrap();

        assert_eq!(input.username.as_deref(), Some("alice"));
        assert_eq!(input.event_type_code.as_deref(), Some("cl.comment.created"));
    }

    #[test]
    fn normalize_email_job_attachment_prune_request_rejects_unsafe_inputs() {
        assert!(
            normalize_email_job_attachment_prune_request(EmailJobAttachmentPruneRequest {
                older_than_days: 0,
                statuses: None,
                username: None,
                event_type_code: None,
            })
            .is_err()
        );
        assert!(
            normalize_email_job_attachment_prune_request(EmailJobAttachmentPruneRequest {
                older_than_days: MAX_EMAIL_JOB_PRUNE_RETENTION_DAYS + 1,
                statuses: None,
                username: None,
                event_type_code: None,
            })
            .is_err()
        );
        assert!(
            normalize_email_job_attachment_prune_request(EmailJobAttachmentPruneRequest {
                older_than_days: 7,
                statuses: Some(vec![EMAIL_JOB_STATUS_FAILED.to_string()]),
                username: None,
                event_type_code: None,
            })
            .is_err()
        );
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
        assert_eq!(response.description, "New comment on a Change List");
        assert!(!response.system_required);
        assert!(response.default_enabled);
    }

    #[test]
    fn email_job_attachment_response_maps_metadata() {
        let created_at = chrono::Utc::now().naive_utc();
        let response = EmailJobAttachmentResponse::from(EmailJobAttachmentMetadata {
            id: 42,
            email_job_id: 7,
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size_bytes: 1024,
            created_at,
        });

        assert_eq!(response.id, 42);
        assert_eq!(response.email_job_id, 7);
        assert_eq!(response.filename, "report.pdf");
        assert_eq!(response.content_type, "application/pdf");
        assert_eq!(response.size_bytes, 1024);
        assert_eq!(response.created_at, created_at.to_string());
    }

    #[test]
    fn email_attachment_content_disposition_sanitizes_and_encodes_filename() {
        let disposition = email_attachment_content_disposition(" 报告\";\r\n.txt ");

        assert!(disposition.starts_with("attachment; filename="));
        assert!(disposition.contains("filename=\"______.txt\""));
        assert!(disposition.contains("filename*=UTF-8''%20%E6%8A%A5%E5%91%8A"));
        assert!(!disposition.contains('\r'));
        assert!(!disposition.contains('\n'));
    }

    #[test]
    fn validate_notification_event_type_code_trims_and_rejects_blank_code() {
        assert_eq!(
            validate_notification_event_type_code(" cl.comment.created ").unwrap(),
            "cl.comment.created"
        );
        assert!(validate_notification_event_type_code("  ").is_err());
    }

    #[test]
    fn normalize_notification_event_type_input_trims_and_rejects_blank_fields() {
        let normalized =
            normalize_notification_event_type_input(UpdateNotificationEventTypeRequest {
                category: " cl ".to_string(),
                description: " New comment ".to_string(),
                system_required: false,
                default_enabled: true,
            })
            .unwrap();

        assert_eq!(
            normalized,
            NotificationEventTypeInput {
                category: "cl".to_string(),
                description: "New comment".to_string(),
                system_required: false,
                default_enabled: true,
            }
        );

        assert!(
            normalize_notification_event_type_input(UpdateNotificationEventTypeRequest {
                category: " ".to_string(),
                description: "New comment".to_string(),
                system_required: false,
                default_enabled: true,
            })
            .is_err()
        );
        assert!(
            normalize_notification_event_type_input(UpdateNotificationEventTypeRequest {
                category: "cl".to_string(),
                description: " ".to_string(),
                system_required: false,
                default_enabled: true,
            })
            .is_err()
        );
    }
}
