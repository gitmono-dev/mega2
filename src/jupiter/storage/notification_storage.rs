use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, sea_query::Expr,
};

use crate::callisto::{
    email_jobs, notification_event_types, user_notification_preferences, user_notification_settings,
};

pub const MAX_EMAIL_RETRY_ATTEMPTS: i32 = 5;
pub const EMAIL_JOB_SEND_TIMEOUT_SECS: i64 = 15 * 60;
pub const EMAIL_JOB_STALE_SEND_ERROR: &str = "email send attempt timed out before completion";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailJobFailureDisposition {
    RetryScheduled,
    DeadLettered,
    MissingJob,
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

    // Evnt types
    pub async fn list_event_types(
        &self,
    ) -> Result<Vec<notification_event_types::Model>, sea_orm::DbErr> {
        notification_event_types::Entity::find()
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
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(self.db())
            .await?;
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
        let now = chrono::Utc::now().naive_utc();

        email_jobs::ActiveModel {
            id: Default::default(),
            username: Set(username.to_string()),
            to_email: Set(to_email.to_string()),
            event_type_code: Set(event_type_code.to_string()),
            subject: Set(subject.to_string()),
            body_html: Set(body_html.to_string()),
            body_text: Set(body_text.map(|s| s.to_string())),
            status: Set("pending".to_string()),
            error_message: Set(None),
            retry_count: Set(0),
            next_retry_at: Set(None),
            sent_at: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(self.db())
        .await?;

        Ok(())
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
            .filter(email_jobs::Column::Status.eq("pending"))
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
            .col_expr(email_jobs::Column::Status, Expr::value("sending"))
            .col_expr(email_jobs::Column::UpdatedAt, Expr::value(now))
            .filter(email_jobs::Column::Id.eq(job_id))
            .filter(email_jobs::Column::Status.eq("pending"))
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
            .col_expr(email_jobs::Column::Status, Expr::value("pending"))
            .col_expr(
                email_jobs::Column::ErrorMessage,
                Expr::value(EMAIL_JOB_STALE_SEND_ERROR),
            )
            .col_expr(email_jobs::Column::UpdatedAt, Expr::value(now))
            .filter(email_jobs::Column::Status.eq("sending"))
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

            model.status = Set("sent".to_string());
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

            model.status = Set("skipped".to_string());
            model.error_message = Set(Some(reason.to_string()));
            model.next_retry_at = Set(None);
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        }
        Ok(())
    }

    /// Mark a job as failed and schedule a retry.
    /// Backoff: 30s, 60s, ... capped at 300s.
    /// Jobs that reach MAX_EMAIL_RETRY_ATTEMPTS are moved to failed status and
    /// no longer returned by fetch_pending_jobs.
    pub async fn mark_job_failed_with_retry(
        &self,
        job_id: i64,
        error: &str,
    ) -> Result<EmailJobFailureDisposition, sea_orm::DbErr> {
        if let Some(job) = email_jobs::Entity::find_by_id(job_id)
            .one(self.db())
            .await?
        {
            let mut model: email_jobs::ActiveModel = job.clone().into();
            let now = chrono::Utc::now().naive_utc();
            let retry = job.retry_count + 1;

            let disposition = if retry >= MAX_EMAIL_RETRY_ATTEMPTS {
                model.status = Set("failed".to_string());
                model.next_retry_at = Set(None);
                tracing::warn!(
                    job_id,
                    retry_count = retry,
                    "email job moved to dead-letter status"
                );
                EmailJobFailureDisposition::DeadLettered
            } else {
                let delay_secs = (retry as i64).min(10) * 30;
                let next = now + chrono::Duration::seconds(delay_secs);
                model.status = Set("pending".to_string());
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
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Set};
    use tokio::task::JoinSet;

    use super::*;
    use crate::{
        callisto::notification_event_types,
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

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
}
