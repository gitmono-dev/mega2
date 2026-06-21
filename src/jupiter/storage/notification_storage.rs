use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, sea_query::Expr,
};

use crate::{
    callisto::{
        email_job_attachments, email_jobs, notification_event_types, user_inbox_notifications,
        user_notification_preferences, user_notification_settings,
    },
    config::{
        DEFAULT_MAIL_RETRY_BACKOFF_BASE_SECS, DEFAULT_MAIL_RETRY_BACKOFF_MAX_SECS,
        DEFAULT_MAIL_RETRY_MAX_ATTEMPTS,
    },
    contract::api::common::Pagination,
};

pub const MAX_EMAIL_RETRY_ATTEMPTS: i32 = DEFAULT_MAIL_RETRY_MAX_ATTEMPTS;
pub const EMAIL_RETRY_BACKOFF_BASE_SECS: i64 = DEFAULT_MAIL_RETRY_BACKOFF_BASE_SECS;
pub const EMAIL_RETRY_BACKOFF_MAX_SECS: i64 = DEFAULT_MAIL_RETRY_BACKOFF_MAX_SECS;
pub const EMAIL_JOB_SEND_TIMEOUT_SECS: i64 = 15 * 60;
pub const EMAIL_JOB_STALE_SEND_ERROR: &str = "email send attempt timed out before completion";
pub const EMAIL_JOB_STATUS_PENDING: &str = "pending";
pub const EMAIL_JOB_STATUS_SENDING: &str = "sending";
pub const EMAIL_JOB_STATUS_SENT: &str = "sent";
pub const EMAIL_JOB_STATUS_FAILED: &str = "failed";
pub const EMAIL_JOB_STATUS_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailJobFailureDisposition {
    RetryScheduled,
    DeadLettered,
    MissingJob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmailJobRetryPolicy {
    pub max_attempts: i32,
    pub backoff_base_secs: i64,
    pub backoff_max_secs: i64,
}

impl EmailJobRetryPolicy {
    pub fn new(max_attempts: i32, backoff_base_secs: i64, backoff_max_secs: i64) -> Self {
        Self {
            max_attempts,
            backoff_base_secs,
            backoff_max_secs,
        }
    }

    fn delay_secs(&self, retry_count: i32) -> i64 {
        let retry_index = retry_count.saturating_sub(1).max(0) as u32;
        let multiplier = 1_i64.checked_shl(retry_index).unwrap_or(i64::MAX);
        self.backoff_base_secs
            .saturating_mul(multiplier)
            .min(self.backoff_max_secs)
    }
}

impl Default for EmailJobRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_EMAIL_RETRY_ATTEMPTS,
            backoff_base_secs: EMAIL_RETRY_BACKOFF_BASE_SECS,
            backoff_max_secs: EMAIL_RETRY_BACKOFF_MAX_SECS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailJobListFilter {
    pub status: Option<String>,
    pub username: Option<String>,
    pub event_type_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailJobAttachment {
    pub filename: String,
    pub content_type: String,
    pub content: Vec<u8>,
}

impl EmailJobAttachment {
    pub fn new(
        filename: impl Into<String>,
        content_type: impl Into<String>,
        content: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailJobAttachmentMetadata {
    pub id: i64,
    pub email_job_id: i64,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailJobAttachmentContent {
    pub id: i64,
    pub email_job_id: i64,
    pub filename: String,
    pub content_type: String,
    pub content: Vec<u8>,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Copy)]
pub struct EmailJobEnqueue<'a> {
    pub username: &'a str,
    pub to_email: &'a str,
    pub event_type_code: &'a str,
    pub subject: &'a str,
    pub body_html: &'a str,
    pub body_text: Option<&'a str>,
    pub attachments: &'a [EmailJobAttachment],
}

fn validate_email_job_attachment(attachment: &EmailJobAttachment) -> Result<(), sea_orm::DbErr> {
    if attachment.filename.trim().is_empty() {
        return Err(sea_orm::DbErr::Custom(
            "email attachment filename must not be empty".to_string(),
        ));
    }
    if attachment.content_type.trim().is_empty() {
        return Err(sea_orm::DbErr::Custom(
            "email attachment content_type must not be empty".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmailJobStats {
    pub total: u64,
    pub pending: u64,
    pub sending: u64,
    pub sent: u64,
    pub failed: u64,
    pub skipped: u64,
}

#[derive(Debug)]
pub enum EmailJobRetryDisposition {
    Queued(Box<email_jobs::Model>),
    MissingJob,
    NotRetryable { status: String },
}

#[derive(Clone)]
pub struct NotificationStorage {
    db: Arc<DatabaseConnection>,
}

impl NotificationStorage {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    // In-app (inbox) notifications

    /// Persist an in-app notification row for a user (in-app delivery channel).
    pub async fn create_inbox_notification(
        &self,
        username: &str,
        event_type_code: &str,
        subject: &str,
        body_html: &str,
        body_text: Option<&str>,
    ) -> Result<(), sea_orm::DbErr> {
        user_inbox_notifications::ActiveModel {
            username: Set(username.to_string()),
            event_type_code: Set(event_type_code.to_string()),
            subject: Set(subject.to_string()),
            body_html: Set(body_html.to_string()),
            body_text: Set(body_text.map(str::to_string)),
            read: Set(false),
            created_at: Set(chrono::Utc::now().naive_utc()),
            ..Default::default()
        }
        .insert(self.db())
        .await?;

        Ok(())
    }

    /// List a user's in-app notifications, newest first.
    pub async fn list_inbox_notifications(
        &self,
        username: &str,
        limit: u64,
    ) -> Result<Vec<user_inbox_notifications::Model>, sea_orm::DbErr> {
        user_inbox_notifications::Entity::find()
            .filter(user_inbox_notifications::Column::Username.eq(username))
            .order_by_desc(user_inbox_notifications::Column::Id)
            .limit(limit)
            .all(self.db())
            .await
    }

    // Event types
    pub async fn list_event_types(
        &self,
    ) -> Result<Vec<notification_event_types::Model>, sea_orm::DbErr> {
        notification_event_types::Entity::find()
            .order_by_asc(notification_event_types::Column::Code)
            .all(self.db())
            .await
    }

    pub async fn get_event_type(
        &self,
        code: &str,
    ) -> Result<Option<notification_event_types::Model>, sea_orm::DbErr> {
        notification_event_types::Entity::find()
            .filter(notification_event_types::Column::Code.eq(code))
            .one(self.db())
            .await
    }

    pub async fn upsert_event_type(
        &self,
        code: &str,
        category: &str,
        description: &str,
        system_required: bool,
        default_enabled: bool,
    ) -> Result<notification_event_types::Model, sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_event_type(code).await? {
            let mut model: notification_event_types::ActiveModel = existing.into();
            model.category = Set(category.to_string());
            model.description = Set(description.to_string());
            model.system_required = Set(system_required);
            model.default_enabled = Set(default_enabled);
            model.updated_at = Set(now);
            return model.update(self.db()).await;
        }

        notification_event_types::ActiveModel {
            code: Set(code.to_string()),
            category: Set(category.to_string()),
            description: Set(description.to_string()),
            system_required: Set(system_required),
            default_enabled: Set(default_enabled),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(self.db())
        .await
    }

    //User notification settings
    pub async fn get_user_settings(
        &self,
        username: &str,
    ) -> Result<Option<user_notification_settings::Model>, sea_orm::DbErr> {
        user_notification_settings::Entity::find()
            .filter(user_notification_settings::Column::Username.eq(username))
            .one(self.db())
            .await
    }

    pub async fn upsert_user_settings(
        &self,
        username: &str,
        email: &str,
    ) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.email = Set(email.to_string());
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        } else {
            user_notification_settings::ActiveModel {
                username: Set(username.to_string()),
                email: Set(email.to_string()),
                enabled: Set(true),
                delivery_mode: Set("realtime".to_string()),
                preferred_locale: Set(None),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(self.db())
            .await?;
        }

        Ok(())
    }

    pub async fn set_preferred_locale(
        &self,
        username: &str,
        preferred_locale: Option<&str>,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.preferred_locale = Set(preferred_locale.map(str::to_string));
            model.updated_at = Set(chrono::Utc::now().naive_utc());
            model.update(self.db()).await?;
        }
        Ok(())
    }

    // Notification preferences
    pub async fn get_user_preference(
        &self,
        username: &str,
        event_type_code: &str,
    ) -> Result<Option<user_notification_preferences::Model>, sea_orm::DbErr> {
        user_notification_preferences::Entity::find()
            .filter(user_notification_preferences::Column::Username.eq(username))
            .filter(user_notification_preferences::Column::EventTypeCode.eq(event_type_code))
            .one(self.db())
            .await
    }

    pub async fn set_user_preference(
        &self,
        username: &str,
        event_type_code: &str,
        enabled: bool,
    ) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_user_preference(username, event_type_code).await? {
            let mut model: user_notification_preferences::ActiveModel = existing.into();
            model.enabled = Set(enabled);
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        } else {
            user_notification_preferences::ActiveModel {
                username: Set(username.to_string()),
                event_type_code: Set(event_type_code.to_string()),
                enabled: Set(enabled),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(self.db())
            .await?;
        }

        Ok(())
    }

    pub async fn list_user_preferences(
        &self,
        username: &str,
    ) -> Result<Vec<user_notification_preferences::Model>, sea_orm::DbErr> {
        user_notification_preferences::Entity::find()
            .filter(user_notification_preferences::Column::Username.eq(username))
            .all(self.db())
            .await
    }

    pub async fn set_global_enabled(
        &self,
        username: &str,
        enabled: bool,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.enabled = Set(enabled);
            model.updated_at = Set(chrono::Utc::now().naive_utc());
            model.update(self.db()).await?;
        }
        Ok(())
    }

    pub async fn set_delivery_mode(
        &self,
        username: &str,
        mode: &str,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.delivery_mode = Set(mode.to_string());
            model.updated_at = Set(chrono::Utc::now().naive_utc());
            model.update(self.db()).await?;
        }
        Ok(())
    }

    // Main logic of whether to send a notification for a given user and event type
    pub async fn should_send(
        &self,
        username: &str,
        event_type_code: &str,
    ) -> Result<bool, sea_orm::DbErr> {
        let event_type = match self.get_event_type(event_type_code).await? {
            Some(e) => e,
            None => return Ok(false),
        };

        let settings = match self.get_user_settings(username).await? {
            Some(s) => s,
            None => return Ok(false),
        };

        if !settings.enabled {
            return Ok(false);
        }

        if event_type.system_required {
            return Ok(true);
        }

        if let Some(pref) = self.get_user_preference(username, event_type_code).await? {
            return Ok(pref.enabled);
        }

        Ok(event_type.default_enabled)
    }

    // Email job management
    pub async fn enqueue_email_job(
        &self,
        username: &str,
        to_email: &str,
        event_type_code: &str,
        subject: &str,
        body_html: &str,
        body_text: Option<&str>,
    ) -> Result<(), sea_orm::DbErr> {
        self.enqueue_email_job_with_attachments(EmailJobEnqueue {
            username,
            to_email,
            event_type_code,
            subject,
            body_html,
            body_text,
            attachments: &[],
        })
        .await
    }

    pub async fn enqueue_email_job_with_attachments(
        &self,
        input: EmailJobEnqueue<'_>,
    ) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();
        let txn = self.db().begin().await?;

        let queued_job = email_jobs::ActiveModel {
            id: Default::default(),
            username: Set(input.username.to_string()),
            to_email: Set(input.to_email.to_string()),
            event_type_code: Set(input.event_type_code.to_string()),
            subject: Set(input.subject.to_string()),
            body_html: Set(input.body_html.to_string()),
            body_text: Set(input.body_text.map(|s| s.to_string())),
            status: Set(EMAIL_JOB_STATUS_PENDING.to_string()),
            error_message: Set(None),
            retry_count: Set(0),
            next_retry_at: Set(None),
            sent_at: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await?;

        for attachment in input.attachments {
            validate_email_job_attachment(attachment)?;
            email_job_attachments::ActiveModel {
                id: Default::default(),
                email_job_id: Set(queued_job.id),
                filename: Set(attachment.filename.clone()),
                content_type: Set(attachment.content_type.clone()),
                content: Set(attachment.content.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }

        txn.commit().await?;
        Ok(())
    }

    pub async fn get_email_job(
        &self,
        job_id: i64,
    ) -> Result<Option<email_jobs::Model>, sea_orm::DbErr> {
        email_jobs::Entity::find_by_id(job_id).one(self.db()).await
    }

    pub async fn list_email_job_attachments(
        &self,
        job_id: i64,
    ) -> Result<Vec<EmailJobAttachment>, sea_orm::DbErr> {
        email_job_attachments::Entity::find()
            .filter(email_job_attachments::Column::EmailJobId.eq(job_id))
            .order_by_asc(email_job_attachments::Column::Id)
            .all(self.db())
            .await
            .map(|attachments| {
                attachments
                    .into_iter()
                    .map(|attachment| EmailJobAttachment {
                        filename: attachment.filename,
                        content_type: attachment.content_type,
                        content: attachment.content,
                    })
                    .collect()
            })
    }

    pub async fn list_email_job_attachment_metadata(
        &self,
        job_id: i64,
    ) -> Result<Vec<EmailJobAttachmentMetadata>, sea_orm::DbErr> {
        email_job_attachments::Entity::find()
            .filter(email_job_attachments::Column::EmailJobId.eq(job_id))
            .order_by_asc(email_job_attachments::Column::Id)
            .all(self.db())
            .await
            .map(|attachments| {
                attachments
                    .into_iter()
                    .map(|attachment| EmailJobAttachmentMetadata {
                        id: attachment.id,
                        email_job_id: attachment.email_job_id,
                        filename: attachment.filename,
                        content_type: attachment.content_type,
                        size_bytes: attachment.content.len() as u64,
                        created_at: attachment.created_at,
                    })
                    .collect()
            })
    }

    pub async fn get_email_job_attachment_content(
        &self,
        job_id: i64,
        attachment_id: i64,
    ) -> Result<Option<EmailJobAttachmentContent>, sea_orm::DbErr> {
        email_job_attachments::Entity::find()
            .filter(email_job_attachments::Column::EmailJobId.eq(job_id))
            .filter(email_job_attachments::Column::Id.eq(attachment_id))
            .one(self.db())
            .await
            .map(|attachment| {
                attachment.map(|attachment| EmailJobAttachmentContent {
                    id: attachment.id,
                    email_job_id: attachment.email_job_id,
                    filename: attachment.filename,
                    content_type: attachment.content_type,
                    content: attachment.content,
                    created_at: attachment.created_at,
                })
            })
    }

    pub async fn delete_email_job_attachment(
        &self,
        job_id: i64,
        attachment_id: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        let res = email_job_attachments::Entity::delete_many()
            .filter(email_job_attachments::Column::EmailJobId.eq(job_id))
            .filter(email_job_attachments::Column::Id.eq(attachment_id))
            .exec(self.db())
            .await?;

        Ok(res.rows_affected > 0)
    }

    pub async fn prune_email_job_attachments(
        &self,
        statuses: &[String],
        older_than: chrono::NaiveDateTime,
        username: Option<&str>,
        event_type_code: Option<&str>,
    ) -> Result<u64, sea_orm::DbErr> {
        if statuses.is_empty() {
            return Ok(0);
        }

        let mut status_condition = Condition::any();
        for status in statuses {
            status_condition = status_condition.add(email_jobs::Column::Status.eq(status));
        }
        let mut job_condition = Condition::all()
            .add(status_condition)
            .add(email_jobs::Column::UpdatedAt.lt(older_than));
        if let Some(username) = username {
            job_condition = job_condition.add(email_jobs::Column::Username.eq(username));
        }
        if let Some(event_type_code) = event_type_code {
            job_condition =
                job_condition.add(email_jobs::Column::EventTypeCode.eq(event_type_code));
        }

        let job_ids = email_jobs::Entity::find()
            .select_only()
            .column(email_jobs::Column::Id)
            .filter(job_condition)
            .into_tuple::<i64>()
            .all(self.db())
            .await?;

        if job_ids.is_empty() {
            return Ok(0);
        }

        let res = email_job_attachments::Entity::delete_many()
            .filter(email_job_attachments::Column::EmailJobId.is_in(job_ids))
            .exec(self.db())
            .await?;

        Ok(res.rows_affected)
    }

    /// Fetch pending email jobs that are ready to be sent
    /// Jobs are eligible when:
    /// - status == "pending"
    /// - next_retry_at is NULL, or next_retry_at <= now
    pub async fn fetch_pending_jobs(
        &self,
        limit: u64,
    ) -> Result<Vec<email_jobs::Model>, sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        email_jobs::Entity::find()
            .filter(email_jobs::Column::Status.eq(EMAIL_JOB_STATUS_PENDING))
            .filter(
                Condition::any()
                    .add(email_jobs::Column::NextRetryAt.is_null())
                    .add(email_jobs::Column::NextRetryAt.lte(now)),
            )
            .order_by_asc(email_jobs::Column::CreatedAt)
            .limit(limit)
            .all(self.db())
            .await
    }

    /// Try to claim a job for sending by atomically changing its status from "pending" to "sending".
    /// Returns true if the claim was successful, false if the job was already claimed by another
    pub async fn try_claim_job(&self, job_id: i64) -> Result<bool, sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();
        let res = email_jobs::Entity::update_many()
            .col_expr(
                email_jobs::Column::Status,
                Expr::value(EMAIL_JOB_STATUS_SENDING),
            )
            .col_expr(email_jobs::Column::UpdatedAt, Expr::value(now))
            .filter(email_jobs::Column::Id.eq(job_id))
            .filter(email_jobs::Column::Status.eq(EMAIL_JOB_STATUS_PENDING))
            .exec(self.db())
            .await?;
        Ok(res.rows_affected == 1)
    }

    pub async fn requeue_stale_sending_jobs(
        &self,
        stale_after: chrono::Duration,
    ) -> Result<u64, sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();
        let stale_before = now - stale_after;
        let res = email_jobs::Entity::update_many()
            .col_expr(
                email_jobs::Column::Status,
                Expr::value(EMAIL_JOB_STATUS_PENDING),
            )
            .col_expr(
                email_jobs::Column::ErrorMessage,
                Expr::value(EMAIL_JOB_STALE_SEND_ERROR),
            )
            .col_expr(email_jobs::Column::UpdatedAt, Expr::value(now))
            .filter(email_jobs::Column::Status.eq(EMAIL_JOB_STATUS_SENDING))
            .filter(email_jobs::Column::UpdatedAt.lte(stale_before))
            .exec(self.db())
            .await?;
        Ok(res.rows_affected)
    }

    pub async fn mark_job_sent(&self, job_id: i64) -> Result<(), sea_orm::DbErr> {
        if let Some(job) = email_jobs::Entity::find_by_id(job_id)
            .one(self.db())
            .await?
        {
            let mut model: email_jobs::ActiveModel = job.into();
            let now = chrono::Utc::now().naive_utc();

            model.status = Set(EMAIL_JOB_STATUS_SENT.to_string());
            model.error_message = Set(None);
            model.next_retry_at = Set(None);
            model.sent_at = Set(Some(now));
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        }
        Ok(())
    }

    pub async fn mark_job_skipped(&self, job_id: i64, reason: &str) -> Result<(), sea_orm::DbErr> {
        if let Some(job) = email_jobs::Entity::find_by_id(job_id)
            .one(self.db())
            .await?
        {
            let mut model: email_jobs::ActiveModel = job.into();
            let now = chrono::Utc::now().naive_utc();

            model.status = Set(EMAIL_JOB_STATUS_SKIPPED.to_string());
            model.error_message = Set(Some(reason.to_string()));
            model.next_retry_at = Set(None);
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        }
        Ok(())
    }

    /// Mark a job as failed and schedule a retry.
    /// Default backoff: 30s, 60s, 120s, ... capped at 300s.
    /// Jobs that reach the configured retry attempt limit are moved to failed status and
    /// no longer returned by fetch_pending_jobs.
    pub async fn mark_job_failed_with_retry(
        &self,
        job_id: i64,
        error: &str,
    ) -> Result<EmailJobFailureDisposition, sea_orm::DbErr> {
        self.mark_job_failed_with_retry_policy(job_id, error, EmailJobRetryPolicy::default())
            .await
    }

    pub async fn mark_job_failed_with_retry_policy(
        &self,
        job_id: i64,
        error: &str,
        retry_policy: EmailJobRetryPolicy,
    ) -> Result<EmailJobFailureDisposition, sea_orm::DbErr> {
        if let Some(job) = email_jobs::Entity::find_by_id(job_id)
            .one(self.db())
            .await?
        {
            let mut model: email_jobs::ActiveModel = job.clone().into();
            let now = chrono::Utc::now().naive_utc();
            let retry = job.retry_count + 1;

            let disposition = if retry >= retry_policy.max_attempts {
                model.status = Set(EMAIL_JOB_STATUS_FAILED.to_string());
                model.next_retry_at = Set(None);
                tracing::warn!(
                    job_id,
                    retry_count = retry,
                    max_retry_attempts = retry_policy.max_attempts,
                    "email job moved to dead-letter status"
                );
                EmailJobFailureDisposition::DeadLettered
            } else {
                let delay_secs = retry_policy.delay_secs(retry);
                let next = now + chrono::Duration::seconds(delay_secs);
                model.status = Set(EMAIL_JOB_STATUS_PENDING.to_string());
                model.next_retry_at = Set(Some(next));
                EmailJobFailureDisposition::RetryScheduled
            };

            model.error_message = Set(Some(error.to_string()));
            model.retry_count = Set(retry);
            model.updated_at = Set(now);
            model.update(self.db()).await?;
            return Ok(disposition);
        }
        Ok(EmailJobFailureDisposition::MissingJob)
    }

    pub async fn mark_job_failed(&self, job_id: i64, error: &str) -> Result<(), sea_orm::DbErr> {
        self.mark_job_failed_with_retry(job_id, error)
            .await
            .map(|_| ())
    }

    pub async fn list_email_jobs(
        &self,
        filter: EmailJobListFilter,
        page: Pagination,
    ) -> Result<(Vec<email_jobs::Model>, u64), sea_orm::DbErr> {
        let mut condition = Condition::all();

        if let Some(status) = filter.status {
            condition = condition.add(email_jobs::Column::Status.eq(status));
        }
        if let Some(username) = filter.username {
            condition = condition.add(email_jobs::Column::Username.eq(username));
        }
        if let Some(event_type_code) = filter.event_type_code {
            condition = condition.add(email_jobs::Column::EventTypeCode.eq(event_type_code));
        }

        let paginator = email_jobs::Entity::find()
            .filter(condition)
            .order_by_desc(email_jobs::Column::CreatedAt)
            .paginate(self.db(), page.per_page);
        let total = paginator.num_items().await?;
        let items = paginator.fetch_page(page.page.saturating_sub(1)).await?;
        Ok((items, total))
    }

    pub async fn email_job_stats(&self) -> Result<EmailJobStats, sea_orm::DbErr> {
        Ok(EmailJobStats {
            total: email_jobs::Entity::find().count(self.db()).await?,
            pending: self
                .count_email_jobs_by_status(EMAIL_JOB_STATUS_PENDING)
                .await?,
            sending: self
                .count_email_jobs_by_status(EMAIL_JOB_STATUS_SENDING)
                .await?,
            sent: self
                .count_email_jobs_by_status(EMAIL_JOB_STATUS_SENT)
                .await?,
            failed: self
                .count_email_jobs_by_status(EMAIL_JOB_STATUS_FAILED)
                .await?,
            skipped: self
                .count_email_jobs_by_status(EMAIL_JOB_STATUS_SKIPPED)
                .await?,
        })
    }

    async fn count_email_jobs_by_status(&self, status: &str) -> Result<u64, sea_orm::DbErr> {
        email_jobs::Entity::find()
            .filter(email_jobs::Column::Status.eq(status))
            .count(self.db())
            .await
    }

    pub async fn prune_email_jobs(
        &self,
        statuses: &[String],
        older_than: chrono::NaiveDateTime,
    ) -> Result<u64, sea_orm::DbErr> {
        if statuses.is_empty() {
            return Ok(0);
        }

        let mut status_condition = Condition::any();
        for status in statuses {
            status_condition = status_condition.add(email_jobs::Column::Status.eq(status));
        }

        let res = email_jobs::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(status_condition)
                    .add(email_jobs::Column::UpdatedAt.lt(older_than)),
            )
            .exec(self.db())
            .await?;

        Ok(res.rows_affected)
    }

    pub async fn retry_failed_email_job(
        &self,
        job_id: i64,
    ) -> Result<EmailJobRetryDisposition, sea_orm::DbErr> {
        let Some(job) = email_jobs::Entity::find_by_id(job_id)
            .one(self.db())
            .await?
        else {
            return Ok(EmailJobRetryDisposition::MissingJob);
        };

        if job.status != EMAIL_JOB_STATUS_FAILED {
            return Ok(EmailJobRetryDisposition::NotRetryable { status: job.status });
        }

        let mut model: email_jobs::ActiveModel = job.into();
        let now = chrono::Utc::now().naive_utc();
        model.status = Set(EMAIL_JOB_STATUS_PENDING.to_string());
        model.error_message = Set(None);
        model.retry_count = Set(0);
        model.next_retry_at = Set(None);
        model.sent_at = Set(None);
        model.updated_at = Set(now);

        let queued = model.update(self.db()).await?;
        Ok(EmailJobRetryDisposition::Queued(Box::new(queued)))
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Database, Set};
    use tokio::task::JoinSet;

    use super::*;
    use crate::{
        callisto::notification_event_types,
        jupiter::{
            migration::apply_migrations,
            storage::init::database_connection,
            tests::{test_db_config, test_db_connection},
        },
    };

    #[test]
    fn retry_policy_uses_exponential_backoff_capped_at_max() {
        let policy = EmailJobRetryPolicy::new(5, 5, 18);

        assert_eq!(policy.delay_secs(0), 5);
        assert_eq!(policy.delay_secs(1), 5);
        assert_eq!(policy.delay_secs(2), 10);
        assert_eq!(policy.delay_secs(3), 18);
        assert_eq!(policy.delay_secs(30), 18);
    }

    #[tokio::test]
    async fn upsert_event_type_inserts_and_updates_existing_row() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let inserted = storage
            .upsert_event_type("test.event", "test", "First", false, true)
            .await
            .unwrap();

        assert_eq!(inserted.code, "test.event");
        assert_eq!(inserted.category, "test");
        assert_eq!(inserted.description, "First");
        assert!(!inserted.system_required);
        assert!(inserted.default_enabled);

        let updated = storage
            .upsert_event_type("test.event", "updated", "Second", true, false)
            .await
            .unwrap();

        assert_eq!(updated.code, "test.event");
        assert_eq!(updated.category, "updated");
        assert_eq!(updated.description, "Second");
        assert!(updated.system_required);
        assert!(!updated.default_enabled);
        assert_eq!(updated.created_at, inserted.created_at);
        assert!(updated.updated_at >= inserted.updated_at);

        let event_types = storage.list_event_types().await.unwrap();
        assert_eq!(event_types.len(), 1);
        assert_eq!(event_types[0].code, "test.event");
    }

    #[tokio::test]
    async fn test_should_send_logic() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));

        let now = chrono::Utc::now().naive_utc();

        // Insert event type
        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        // Insert user settings
        storage
            .upsert_user_settings("alice", "alice@test.com")
            .await
            .unwrap();

        // No override → default_enabled = true
        assert!(storage.should_send("alice", "test.event").await.unwrap());

        // Override false
        storage
            .set_user_preference("alice", "test.event", false)
            .await
            .unwrap();

        assert!(!storage.should_send("alice", "test.event").await.unwrap());
    }

    #[tokio::test]
    async fn user_settings_preferred_locale_can_be_updated_and_cleared() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        storage
            .upsert_user_settings("alice", "alice@test.com")
            .await
            .unwrap();

        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale,
            None
        );

        storage
            .set_preferred_locale("alice", Some("zh-CN"))
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale
                .as_deref(),
            Some("zh-CN")
        );

        storage.set_preferred_locale("alice", None).await.unwrap();
        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale,
            None
        );
    }

    #[tokio::test]
    async fn test_enqueue_and_mark_job() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        // Insert event type + user
        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .upsert_user_settings("alice", "alice@test.com")
            .await
            .unwrap();

        // Enqueue
        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();

        let jobs = storage.fetch_pending_jobs(10).await.unwrap();
        assert_eq!(jobs.len(), 1);

        let job_id = jobs[0].id;

        let disposition = storage
            .mark_job_failed_with_retry(job_id, "temporary smtp failure")
            .await
            .unwrap();
        assert_eq!(disposition, EmailJobFailureDisposition::RetryScheduled);
        let retried_job = crate::callisto::email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(retried_job.next_retry_at.is_some());
        assert_eq!(
            retried_job.error_message.as_deref(),
            Some("temporary smtp failure")
        );

        // Mark sent clears retry metadata from previous failures.
        storage.mark_job_sent(job_id).await.unwrap();

        let job = crate::callisto::email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(job.status, "sent");
        assert!(job.next_retry_at.is_none());
        assert!(job.error_message.is_none());
    }

    #[tokio::test]
    async fn enqueue_email_job_with_attachments_persists_attachments() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job_with_attachments(EmailJobEnqueue {
                username: "alice",
                to_email: "alice@test.com",
                event_type_code: "test.event",
                subject: "Hello",
                body_html: "<p>Hello</p>",
                body_text: Some("Hello"),
                attachments: &[
                    EmailJobAttachment::new("one.txt", "text/plain", b"one".to_vec()),
                    EmailJobAttachment::new(
                        "two.json",
                        "application/json",
                        br#"{"ok":true}"#.to_vec(),
                    ),
                ],
            })
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);

        let attachments = storage
            .list_email_job_attachments(jobs[0].id)
            .await
            .unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].filename, "one.txt");
        assert_eq!(attachments[0].content_type, "text/plain");
        assert_eq!(attachments[0].content, b"one");
        assert_eq!(attachments[1].filename, "two.json");
        assert_eq!(attachments[1].content_type, "application/json");
        assert_eq!(attachments[1].content, br#"{"ok":true}"#);

        let metadata = storage
            .list_email_job_attachment_metadata(jobs[0].id)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0].email_job_id, jobs[0].id);
        assert_eq!(metadata[0].filename, "one.txt");
        assert_eq!(metadata[0].content_type, "text/plain");
        assert_eq!(metadata[0].size_bytes, 3);
        assert!(metadata[0].id > 0);
        assert!(metadata[0].created_at >= now);
        assert_eq!(metadata[1].filename, "two.json");
        assert_eq!(metadata[1].content_type, "application/json");
        assert_eq!(metadata[1].size_bytes, br#"{"ok":true}"#.len() as u64);

        let attachment_content = storage
            .get_email_job_attachment_content(jobs[0].id, metadata[1].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attachment_content.id, metadata[1].id);
        assert_eq!(attachment_content.email_job_id, jobs[0].id);
        assert_eq!(attachment_content.filename, "two.json");
        assert_eq!(attachment_content.content_type, "application/json");
        assert_eq!(attachment_content.content, br#"{"ok":true}"#);
        assert!(
            storage
                .get_email_job_attachment_content(jobs[0].id + 1, metadata[1].id)
                .await
                .unwrap()
                .is_none()
        );

        assert!(
            storage
                .delete_email_job_attachment(jobs[0].id, metadata[0].id)
                .await
                .unwrap()
        );
        assert!(
            !storage
                .delete_email_job_attachment(jobs[0].id, metadata[0].id)
                .await
                .unwrap()
        );

        let remaining_metadata = storage
            .list_email_job_attachment_metadata(jobs[0].id)
            .await
            .unwrap();
        assert_eq!(remaining_metadata.len(), 1);
        assert_eq!(remaining_metadata[0].filename, "two.json");

        let remaining_attachments = storage
            .list_email_job_attachments(jobs[0].id)
            .await
            .unwrap();
        assert_eq!(remaining_attachments.len(), 1);
        assert_eq!(remaining_attachments[0].content, br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn concurrent_claim_only_allows_one_sender() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();
        let job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;

        let mut claims = JoinSet::new();
        for _ in 0..16 {
            let storage = storage.clone();
            claims.spawn(async move { storage.try_claim_job(job_id).await.unwrap() });
        }

        let mut successful_claims = 0;
        while let Some(result) = claims.join_next().await {
            if result.unwrap() {
                successful_claims += 1;
            }
        }

        assert_eq!(successful_claims, 1);
        let job = email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "sending");
        assert!(storage.fetch_pending_jobs(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_claim_across_independent_connections_only_allows_one_sender() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_config = test_db_config(temp_dir.path()).await;
        let db = database_connection(&db_config).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();
        let job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;

        let mut claims = JoinSet::new();
        for _ in 0..8 {
            let db = Database::connect(db_config.db_url.clone()).await.unwrap();
            let storage = NotificationStorage::new(Arc::new(db));
            claims.spawn(async move { storage.try_claim_job(job_id).await.unwrap() });
        }

        let mut successful_claims = 0;
        while let Some(result) = claims.join_next().await {
            if result.unwrap() {
                successful_claims += 1;
            }
        }

        assert_eq!(successful_claims, 1);
        let job = email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "sending");
        assert!(storage.fetch_pending_jobs(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_sending_jobs_are_requeued_but_fresh_claims_are_left_alone() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Stale",
                "<p>Stale</p>",
                Some("Stale"),
            )
            .await
            .unwrap();
        let stale_job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;
        assert!(storage.try_claim_job(stale_job_id).await.unwrap());
        let stale_job = email_jobs::Entity::find_by_id(stale_job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut stale_model: email_jobs::ActiveModel = stale_job.into();
        stale_model.updated_at = Set(chrono::Utc::now().naive_utc()
            - chrono::Duration::seconds(EMAIL_JOB_SEND_TIMEOUT_SECS + 60));
        stale_model.update(&db).await.unwrap();

        storage
            .enqueue_email_job(
                "bob",
                "bob@test.com",
                "test.event",
                "Fresh",
                "<p>Fresh</p>",
                Some("Fresh"),
            )
            .await
            .unwrap();
        let fresh_job_id = storage
            .fetch_pending_jobs(10)
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.username == "bob")
            .unwrap()
            .id;
        assert!(storage.try_claim_job(fresh_job_id).await.unwrap());

        let requeued = storage
            .requeue_stale_sending_jobs(chrono::Duration::seconds(EMAIL_JOB_SEND_TIMEOUT_SECS))
            .await
            .unwrap();
        assert_eq!(requeued, 1);

        let stale_job = email_jobs::Entity::find_by_id(stale_job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale_job.status, "pending");
        assert_eq!(
            stale_job.error_message.as_deref(),
            Some(EMAIL_JOB_STALE_SEND_ERROR)
        );

        let fresh_job = email_jobs::Entity::find_by_id(fresh_job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh_job.status, "sending");
    }

    #[tokio::test]
    async fn email_job_failure_retries_then_dead_letters() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();

        let job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;

        for retry in 1..MAX_EMAIL_RETRY_ATTEMPTS {
            let disposition = storage
                .mark_job_failed_with_retry(job_id, "smtp unavailable")
                .await
                .unwrap();
            assert_eq!(disposition, EmailJobFailureDisposition::RetryScheduled);

            let job = email_jobs::Entity::find_by_id(job_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(job.status, "pending");
            assert_eq!(job.retry_count, retry);
            assert!(job.next_retry_at.is_some());
        }

        let disposition = storage
            .mark_job_failed_with_retry(job_id, "smtp unavailable")
            .await
            .unwrap();
        assert_eq!(disposition, EmailJobFailureDisposition::DeadLettered);

        let job = email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "failed");
        assert_eq!(job.retry_count, MAX_EMAIL_RETRY_ATTEMPTS);
        assert!(job.next_retry_at.is_none());
        assert_eq!(job.error_message.as_deref(), Some("smtp unavailable"));
        assert!(storage.fetch_pending_jobs(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn email_job_failure_uses_configured_retry_policy() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();

        let job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;
        let policy = EmailJobRetryPolicy::new(2, 5, 7);
        let before_failure = chrono::Utc::now().naive_utc();

        let disposition = storage
            .mark_job_failed_with_retry_policy(job_id, "smtp unavailable", policy)
            .await
            .unwrap();
        assert_eq!(disposition, EmailJobFailureDisposition::RetryScheduled);

        let job = email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, EMAIL_JOB_STATUS_PENDING);
        assert_eq!(job.retry_count, 1);
        assert!(
            job.next_retry_at.expect("retry should be scheduled")
                >= before_failure + chrono::Duration::seconds(5)
        );

        let disposition = storage
            .mark_job_failed_with_retry_policy(job_id, "smtp unavailable", policy)
            .await
            .unwrap();
        assert_eq!(disposition, EmailJobFailureDisposition::DeadLettered);

        let job = email_jobs::Entity::find_by_id(job_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, EMAIL_JOB_STATUS_FAILED);
        assert_eq!(job.retry_count, 2);
        assert!(job.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn email_job_management_lists_stats_and_retries_failed_jobs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage
            .enqueue_email_job(
                "alice",
                "alice@test.com",
                "test.event",
                "Failed",
                "<p>Failed</p>",
                Some("Failed"),
            )
            .await
            .unwrap();
        let failed_job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;

        for _ in 0..MAX_EMAIL_RETRY_ATTEMPTS {
            storage
                .mark_job_failed_with_retry(failed_job_id, "smtp unavailable")
                .await
                .unwrap();
        }

        storage
            .enqueue_email_job(
                "bob",
                "bob@test.com",
                "test.event",
                "Sent",
                "<p>Sent</p>",
                Some("Sent"),
            )
            .await
            .unwrap();
        let sent_job_id = storage.fetch_pending_jobs(10).await.unwrap()[0].id;
        storage.mark_job_sent(sent_job_id).await.unwrap();

        let stats = storage.email_job_stats().await.unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.sent, 1);

        let (failed_jobs, failed_total) = storage
            .list_email_jobs(
                EmailJobListFilter {
                    status: Some(EMAIL_JOB_STATUS_FAILED.to_string()),
                    ..Default::default()
                },
                Pagination {
                    page: 1,
                    per_page: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(failed_total, 1);
        assert_eq!(failed_jobs.len(), 1);
        assert_eq!(failed_jobs[0].id, failed_job_id);

        let (bob_jobs, bob_total) = storage
            .list_email_jobs(
                EmailJobListFilter {
                    username: Some("bob".to_string()),
                    ..Default::default()
                },
                Pagination {
                    page: 1,
                    per_page: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(bob_total, 1);
        assert_eq!(bob_jobs[0].id, sent_job_id);

        match storage.retry_failed_email_job(sent_job_id).await.unwrap() {
            EmailJobRetryDisposition::NotRetryable { status } => {
                assert_eq!(status, EMAIL_JOB_STATUS_SENT);
            }
            other => panic!("expected non-retryable sent job, got {other:?}"),
        }

        match storage.retry_failed_email_job(-1).await.unwrap() {
            EmailJobRetryDisposition::MissingJob => {}
            other => panic!("expected missing job, got {other:?}"),
        }

        let retried = storage.retry_failed_email_job(failed_job_id).await.unwrap();
        let EmailJobRetryDisposition::Queued(job) = retried else {
            panic!("expected failed job to be requeued, got {retried:?}");
        };
        assert_eq!(job.status, EMAIL_JOB_STATUS_PENDING);
        assert_eq!(job.retry_count, 0);
        assert!(job.error_message.is_none());
        assert!(job.next_retry_at.is_none());

        let stats = storage.email_job_stats().await.unwrap();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.sent, 1);
    }

    #[tokio::test]
    async fn prune_email_jobs_removes_only_old_selected_terminal_jobs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        let old_sent_id = enqueue_test_email_job(&storage, "old-sent").await;
        storage.mark_job_sent(old_sent_id).await.unwrap();
        make_email_job_old(&db, old_sent_id).await;

        let old_skipped_id = enqueue_test_email_job(&storage, "old-skipped").await;
        storage
            .mark_job_skipped(old_skipped_id, "disabled")
            .await
            .unwrap();
        make_email_job_old(&db, old_skipped_id).await;

        let old_failed_id = enqueue_test_email_job(&storage, "old-failed").await;
        for _ in 0..MAX_EMAIL_RETRY_ATTEMPTS {
            storage
                .mark_job_failed_with_retry(old_failed_id, "smtp unavailable")
                .await
                .unwrap();
        }
        make_email_job_old(&db, old_failed_id).await;

        let old_pending_id = enqueue_test_email_job(&storage, "old-pending").await;
        make_email_job_old(&db, old_pending_id).await;

        let recent_sent_id = enqueue_test_email_job(&storage, "recent-sent").await;
        storage.mark_job_sent(recent_sent_id).await.unwrap();

        let deleted = storage
            .prune_email_jobs(
                &[
                    EMAIL_JOB_STATUS_SENT.to_string(),
                    EMAIL_JOB_STATUS_SKIPPED.to_string(),
                ],
                now - chrono::Duration::days(7),
            )
            .await
            .unwrap();

        assert_eq!(deleted, 2);
        assert!(
            email_jobs::Entity::find_by_id(old_sent_id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            email_jobs::Entity::find_by_id(old_skipped_id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            email_jobs::Entity::find_by_id(old_failed_id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            email_jobs::Entity::find_by_id(old_pending_id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            email_jobs::Entity::find_by_id(recent_sent_id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn prune_email_job_attachments_removes_only_old_selected_terminal_attachments() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        let old_sent_id = enqueue_test_email_job_with_attachment(&storage, "old-sent").await;
        storage.mark_job_sent(old_sent_id).await.unwrap();
        make_email_job_old(&db, old_sent_id).await;

        let old_skipped_id = enqueue_test_email_job_with_attachment(&storage, "old-skipped").await;
        storage
            .mark_job_skipped(old_skipped_id, "disabled")
            .await
            .unwrap();
        make_email_job_old(&db, old_skipped_id).await;

        let old_failed_id = enqueue_test_email_job_with_attachment(&storage, "old-failed").await;
        for _ in 0..MAX_EMAIL_RETRY_ATTEMPTS {
            storage
                .mark_job_failed_with_retry(old_failed_id, "smtp unavailable")
                .await
                .unwrap();
        }
        make_email_job_old(&db, old_failed_id).await;

        let old_pending_id = enqueue_test_email_job_with_attachment(&storage, "old-pending").await;
        make_email_job_old(&db, old_pending_id).await;

        let recent_sent_id = enqueue_test_email_job_with_attachment(&storage, "recent-sent").await;
        storage.mark_job_sent(recent_sent_id).await.unwrap();

        let deleted = storage
            .prune_email_job_attachments(
                &[
                    EMAIL_JOB_STATUS_SENT.to_string(),
                    EMAIL_JOB_STATUS_SKIPPED.to_string(),
                ],
                now - chrono::Duration::days(7),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(deleted, 2);
        assert!(
            storage
                .list_email_job_attachment_metadata(old_sent_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .list_email_job_attachment_metadata(old_skipped_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            storage
                .list_email_job_attachment_metadata(old_failed_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .list_email_job_attachment_metadata(old_pending_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .list_email_job_attachment_metadata(recent_sent_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            email_jobs::Entity::find_by_id(old_sent_id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            email_jobs::Entity::find_by_id(old_skipped_id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn prune_email_job_attachments_filters_by_username_and_event_type() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        for code in ["target.event", "other.event"] {
            notification_event_types::ActiveModel {
                code: Set(code.to_string()),
                category: Set("test".to_string()),
                description: Set("desc".to_string()),
                system_required: Set(false),
                default_enabled: Set(true),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let alice_target_id = enqueue_test_email_job_with_attachment_for_event(
            &storage,
            "alice",
            "target.event",
            "Alice target",
        )
        .await;
        storage.mark_job_sent(alice_target_id).await.unwrap();
        make_email_job_old(&db, alice_target_id).await;

        let alice_other_id = enqueue_test_email_job_with_attachment_for_event(
            &storage,
            "alice",
            "other.event",
            "Alice other",
        )
        .await;
        storage.mark_job_sent(alice_other_id).await.unwrap();
        make_email_job_old(&db, alice_other_id).await;

        let bob_target_id = enqueue_test_email_job_with_attachment_for_event(
            &storage,
            "bob",
            "target.event",
            "Bob target",
        )
        .await;
        storage.mark_job_sent(bob_target_id).await.unwrap();
        make_email_job_old(&db, bob_target_id).await;

        let deleted = storage
            .prune_email_job_attachments(
                &[EMAIL_JOB_STATUS_SENT.to_string()],
                now - chrono::Duration::days(7),
                Some("alice"),
                Some("target.event"),
            )
            .await
            .unwrap();

        assert_eq!(deleted, 1);
        assert!(
            storage
                .list_email_job_attachment_metadata(alice_target_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            storage
                .list_email_job_attachment_metadata(alice_other_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .list_email_job_attachment_metadata(bob_target_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    async fn enqueue_test_email_job(storage: &NotificationStorage, username: &str) -> i64 {
        storage
            .enqueue_email_job(
                username,
                &format!("{username}@test.com"),
                "test.event",
                "Hello",
                "<p>Hello</p>",
                Some("Hello"),
            )
            .await
            .unwrap();

        storage
            .fetch_pending_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.username == username)
            .unwrap()
            .id
    }

    async fn enqueue_test_email_job_with_attachment(
        storage: &NotificationStorage,
        username: &str,
    ) -> i64 {
        enqueue_test_email_job_with_attachment_for_event(storage, username, "test.event", "Hello")
            .await
    }

    async fn enqueue_test_email_job_with_attachment_for_event(
        storage: &NotificationStorage,
        username: &str,
        event_type_code: &str,
        subject: &str,
    ) -> i64 {
        let to_email = format!("{username}@test.com");
        let attachments = [EmailJobAttachment::new(
            format!("{username}.txt"),
            "text/plain",
            username.as_bytes().to_vec(),
        )];

        storage
            .enqueue_email_job_with_attachments(EmailJobEnqueue {
                username,
                to_email: &to_email,
                event_type_code,
                subject,
                body_html: "<p>Hello</p>",
                body_text: Some("Hello"),
                attachments: &attachments,
            })
            .await
            .unwrap();

        storage
            .fetch_pending_jobs(100)
            .await
            .unwrap()
            .into_iter()
            .find(|job| {
                job.username == username
                    && job.event_type_code == event_type_code
                    && job.subject == subject
            })
            .unwrap()
            .id
    }

    async fn make_email_job_old(db: &DatabaseConnection, job_id: i64) {
        let job = email_jobs::Entity::find_by_id(job_id)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let mut model: email_jobs::ActiveModel = job.into();
        model.updated_at = Set(chrono::Utc::now().naive_utc() - chrono::Duration::days(30));
        model.update(db).await.unwrap();
    }
}
