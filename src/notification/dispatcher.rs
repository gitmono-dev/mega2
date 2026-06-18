use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering},
};

use tokio::{
    task::JoinSet,
    time::{Duration, interval},
};
use tracing::{info, warn};

use crate::{
    callisto::email_jobs,
    common::errors::MegaError,
    config::{
        Config, DEFAULT_MAIL_DISPATCHER_BATCH_SIZE, DEFAULT_MAIL_DISPATCHER_MAX_IN_FLIGHT,
        MailConfig,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    jupiter::storage::notification_storage::{
        EMAIL_JOB_SEND_TIMEOUT_SECS, EmailJobFailureDisposition, EmailJobRetryPolicy,
        NotificationStorage,
    },
    mail::{MailAttachment, Mailer},
};

pub const EMAIL_DISPATCH_BATCH_SIZE: u64 = DEFAULT_MAIL_DISPATCHER_BATCH_SIZE;
pub const EMAIL_DISPATCH_MAX_IN_FLIGHT: usize = DEFAULT_MAIL_DISPATCHER_MAX_IN_FLIGHT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmailDispatcherLimits {
    pub batch_size: u64,
    pub max_in_flight: usize,
}

impl EmailDispatcherLimits {
    pub fn new(batch_size: u64, max_in_flight: usize) -> Self {
        Self {
            batch_size,
            max_in_flight,
        }
    }

    pub fn from_mail_config(config: &MailConfig) -> Self {
        Self::new(
            config.dispatcher_batch_size,
            config.dispatcher_max_in_flight,
        )
    }
}

impl Default for EmailDispatcherLimits {
    fn default() -> Self {
        Self {
            batch_size: EMAIL_DISPATCH_BATCH_SIZE,
            max_in_flight: EMAIL_DISPATCH_MAX_IN_FLIGHT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmailDispatcherControl {
    enabled: Arc<AtomicBool>,
    batch_size: Arc<AtomicU64>,
    max_in_flight: Arc<AtomicUsize>,
    retry_max_attempts: Arc<AtomicI32>,
    retry_backoff_base_secs: Arc<AtomicI64>,
    retry_backoff_max_secs: Arc<AtomicI64>,
}

impl EmailDispatcherControl {
    pub fn new(enabled: bool) -> Self {
        Self::new_with_limits(enabled, EmailDispatcherLimits::default())
    }

    pub fn from_mail_config(config: &MailConfig) -> Self {
        Self::new_with_limits_and_retry_policy(
            config.enabled,
            EmailDispatcherLimits::from_mail_config(config),
            retry_policy_from_mail_config(config),
        )
    }

    pub fn new_with_limits(enabled: bool, limits: EmailDispatcherLimits) -> Self {
        Self::new_with_limits_and_retry_policy(enabled, limits, EmailJobRetryPolicy::default())
    }

    pub fn new_with_limits_and_retry_policy(
        enabled: bool,
        limits: EmailDispatcherLimits,
        retry_policy: EmailJobRetryPolicy,
    ) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            batch_size: Arc::new(AtomicU64::new(limits.batch_size)),
            max_in_flight: Arc::new(AtomicUsize::new(limits.max_in_flight)),
            retry_max_attempts: Arc::new(AtomicI32::new(retry_policy.max_attempts)),
            retry_backoff_base_secs: Arc::new(AtomicI64::new(retry_policy.backoff_base_secs)),
            retry_backoff_max_secs: Arc::new(AtomicI64::new(retry_policy.backoff_max_secs)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn limits(&self) -> EmailDispatcherLimits {
        EmailDispatcherLimits {
            batch_size: self.batch_size.load(Ordering::Acquire),
            max_in_flight: self.max_in_flight.load(Ordering::Acquire),
        }
    }

    pub fn set_limits(&self, limits: EmailDispatcherLimits) {
        self.batch_size.store(limits.batch_size, Ordering::Release);
        self.max_in_flight
            .store(limits.max_in_flight, Ordering::Release);
    }

    pub fn retry_policy(&self) -> EmailJobRetryPolicy {
        EmailJobRetryPolicy {
            max_attempts: self.retry_max_attempts.load(Ordering::Acquire),
            backoff_base_secs: self.retry_backoff_base_secs.load(Ordering::Acquire),
            backoff_max_secs: self.retry_backoff_max_secs.load(Ordering::Acquire),
        }
    }

    pub fn set_retry_policy(&self, retry_policy: EmailJobRetryPolicy) {
        self.retry_max_attempts
            .store(retry_policy.max_attempts, Ordering::Release);
        self.retry_backoff_base_secs
            .store(retry_policy.backoff_base_secs, Ordering::Release);
        self.retry_backoff_max_secs
            .store(retry_policy.backoff_max_secs, Ordering::Release);
    }
}

pub fn config_reload_email_dispatcher_subscriber(
    control: EmailDispatcherControl,
) -> ConfigReloadSubscriber {
    let apply_control = control.clone();
    ConfigReloadSubscriber::new(
        "email_dispatcher",
        move |next, report| apply_mail_enabled(&apply_control, next, report),
        move |current, report| apply_mail_enabled(&control, current, report),
    )
}

pub struct EmailDispatcher {
    stg: NotificationStorage,
    mailer: Arc<dyn Mailer>,
    control: EmailDispatcherControl,
}

impl EmailDispatcher {
    pub fn new(stg: NotificationStorage, mailer: Arc<dyn Mailer>) -> Self {
        Self::new_with_control(stg, mailer, EmailDispatcherControl::new(true))
    }

    pub fn new_with_control(
        stg: NotificationStorage,
        mailer: Arc<dyn Mailer>,
        control: EmailDispatcherControl,
    ) -> Self {
        Self {
            stg,
            mailer,
            control,
        }
    }

    pub async fn run(self, shutdown: tokio_util::sync::CancellationToken) {
        let mut tick = interval(Duration::from_secs(2));

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("email dispatcher shutting down");
                    break;
                }
                _ = tick.tick() => {
                    if let Err(e) = self.tick_once().await {
                        warn!("email dispatcher tick error: {e}");
                    }
                }
            }
        }
    }

    async fn tick_once(&self) -> Result<(), sea_orm::DbErr> {
        if !self.control.enabled() {
            return Ok(());
        }

        let recovered = self
            .stg
            .requeue_stale_sending_jobs(chrono::Duration::seconds(EMAIL_JOB_SEND_TIMEOUT_SECS))
            .await?;
        if recovered > 0 {
            warn!(
                recovered,
                send_timeout_secs = EMAIL_JOB_SEND_TIMEOUT_SECS,
                "email dispatcher requeued stale sending jobs"
            );
        }

        let limits = self.control.limits();
        let retry_policy = self.control.retry_policy();
        let jobs = self.stg.fetch_pending_jobs(limits.batch_size).await?;
        let mut stats = EmailDispatchTickStats {
            fetched: jobs.len(),
            ..Default::default()
        };

        for chunk in jobs.chunks(limits.max_in_flight) {
            let mut tasks = JoinSet::new();

            for job in chunk.iter().cloned() {
                let stg = self.stg.clone();
                let mailer = Arc::clone(&self.mailer);
                tasks.spawn(async move { process_email_job(stg, mailer, job, retry_policy).await });
            }

            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok(outcome)) => stats.record(outcome),
                    Ok(Err(e)) => {
                        stats.task_errors += 1;
                        warn!(error = %e, "email dispatcher job processing error");
                    }
                    Err(e) => {
                        stats.task_errors += 1;
                        warn!(error = %e, "email dispatcher job task failed");
                    }
                }
            }
        }

        if stats.should_log() {
            info!(
                fetched = stats.fetched,
                sent = stats.sent,
                retry_scheduled = stats.retry_scheduled,
                dead_lettered = stats.dead_lettered,
                skipped = stats.skipped,
                claim_missed = stats.claim_missed,
                missing_after_failure = stats.missing_after_failure,
                task_errors = stats.task_errors,
                batch_size = limits.batch_size,
                max_in_flight = limits.max_in_flight,
                retry_max_attempts = retry_policy.max_attempts,
                retry_backoff_base_secs = retry_policy.backoff_base_secs,
                retry_backoff_max_secs = retry_policy.backoff_max_secs,
                "email dispatcher tick completed"
            );
        }

        Ok(())
    }
}

#[derive(Default)]
struct EmailDispatchTickStats {
    fetched: usize,
    sent: usize,
    retry_scheduled: usize,
    dead_lettered: usize,
    skipped: usize,
    claim_missed: usize,
    missing_after_failure: usize,
    task_errors: usize,
}

impl EmailDispatchTickStats {
    fn record(&mut self, outcome: EmailJobOutcome) {
        match outcome {
            EmailJobOutcome::Sent => self.sent += 1,
            EmailJobOutcome::RetryScheduled => self.retry_scheduled += 1,
            EmailJobOutcome::DeadLettered => self.dead_lettered += 1,
            EmailJobOutcome::Skipped => self.skipped += 1,
            EmailJobOutcome::ClaimMissed => self.claim_missed += 1,
            EmailJobOutcome::MissingAfterFailure => self.missing_after_failure += 1,
        }
    }

    fn should_log(&self) -> bool {
        self.fetched > 0 || self.task_errors > 0
    }
}

enum EmailJobOutcome {
    Sent,
    RetryScheduled,
    DeadLettered,
    Skipped,
    ClaimMissed,
    MissingAfterFailure,
}

async fn process_email_job(
    stg: NotificationStorage,
    mailer: Arc<dyn Mailer>,
    job: email_jobs::Model,
    retry_policy: EmailJobRetryPolicy,
) -> Result<EmailJobOutcome, sea_orm::DbErr> {
    if job.to_email.trim().is_empty() {
        stg.mark_job_skipped(job.id, "missing recipient email")
            .await?;
        return Ok(EmailJobOutcome::Skipped);
    }

    if !stg.try_claim_job(job.id).await? {
        return Ok(EmailJobOutcome::ClaimMissed);
    }

    let attachments = stg
        .list_email_job_attachments(job.id)
        .await?
        .into_iter()
        .map(|attachment| {
            MailAttachment::new(
                attachment.filename,
                attachment.content_type,
                attachment.content,
            )
        })
        .collect::<Vec<_>>();

    let send_res = mailer
        .send_html_with_attachments(
            &job.to_email,
            &job.subject,
            &job.body_html,
            job.body_text.as_deref(),
            &attachments,
        )
        .await;

    match send_res {
        Ok(_) => {
            stg.mark_job_sent(job.id).await?;
            Ok(EmailJobOutcome::Sent)
        }
        Err(e) => match stg
            .mark_job_failed_with_retry_policy(job.id, &e.to_string(), retry_policy)
            .await?
        {
            EmailJobFailureDisposition::RetryScheduled => Ok(EmailJobOutcome::RetryScheduled),
            EmailJobFailureDisposition::DeadLettered => Ok(EmailJobOutcome::DeadLettered),
            EmailJobFailureDisposition::MissingJob => Ok(EmailJobOutcome::MissingAfterFailure),
        },
    }
}

fn apply_mail_enabled(
    control: &EmailDispatcherControl,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    if report.applied_fields.contains(&"mail.enabled") {
        let enabled = config.mail.as_ref().is_some_and(|mail| mail.enabled);
        control.set_enabled(enabled);
    }
    if report.applied_fields.iter().any(|field| {
        matches!(
            *field,
            "mail.dispatcher_batch_size" | "mail.dispatcher_max_in_flight"
        )
    }) && let Some(mail) = &config.mail
    {
        control.set_limits(EmailDispatcherLimits::from_mail_config(mail));
    }
    if report.applied_fields.iter().any(|field| {
        matches!(
            *field,
            "mail.retry_max_attempts"
                | "mail.retry_backoff_base_secs"
                | "mail.retry_backoff_max_secs"
        )
    }) && let Some(mail) = &config.mail
    {
        control.set_retry_policy(retry_policy_from_mail_config(mail));
    }

    Ok(())
}

fn retry_policy_from_mail_config(config: &MailConfig) -> EmailJobRetryPolicy {
    EmailJobRetryPolicy::new(
        config.retry_max_attempts,
        config.retry_backoff_base_secs,
        config.retry_backoff_max_secs,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, atomic::AtomicUsize};

    use async_trait::async_trait;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{email_jobs, notification_event_types},
        config::{reload::ConfigHandle, testing::isolated_config},
        jupiter::{
            migration::apply_migrations,
            storage::notification_storage::{EmailJobAttachment, EmailJobEnqueue},
            tests::test_db_connection,
        },
        mail::NoopMailer,
    };

    struct FailingMailer;

    #[async_trait]
    impl Mailer for FailingMailer {
        async fn send_html(
            &self,
            _to: &str,
            _subject: &str,
            _html: &str,
            _text: Option<&str>,
        ) -> Result<(), MegaError> {
            Err(MegaError::Other("smtp unavailable".to_string()))
        }
    }

    struct TrackingMailer {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Mailer for TrackingMailer {
        async fn send_html(
            &self,
            _to: &str,
            _subject: &str,
            _html: &str,
            _text: Option<&str>,
        ) -> Result<(), MegaError> {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct CapturingAttachmentMailer {
        attachments: Arc<Mutex<Vec<MailAttachment>>>,
    }

    #[async_trait]
    impl Mailer for CapturingAttachmentMailer {
        async fn send_html(
            &self,
            _to: &str,
            _subject: &str,
            _html: &str,
            _text: Option<&str>,
        ) -> Result<(), MegaError> {
            Ok(())
        }

        async fn send_html_with_attachments(
            &self,
            _to: &str,
            _subject: &str,
            _html: &str,
            _text: Option<&str>,
            attachments: &[MailAttachment],
        ) -> Result<(), MegaError> {
            *self.attachments.lock().unwrap() = attachments.to_vec();
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_dispatcher_sends_pending_jobs() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();

        // ensure event type exists
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        // enqueue a job
        stg.enqueue_email_job(
            "alice",
            "alice@example.com",
            "cl.comment.created",
            "Subject",
            "<p>Body</p>",
            Some("Body"),
        )
        .await
        .unwrap();

        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(NoopMailer));
        dispatcher.tick_once().await.unwrap();

        let jobs = stg.fetch_pending_jobs(10).await.unwrap();
        assert!(jobs.is_empty(), "pending queue should be empty after send");

        let sent = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].status, "sent");
    }

    #[tokio::test]
    async fn dispatcher_sends_persisted_job_attachments() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        stg.enqueue_email_job_with_attachments(EmailJobEnqueue {
            username: "alice",
            to_email: "alice@example.com",
            event_type_code: "cl.comment.created",
            subject: "Subject",
            body_html: "<p>Body</p>",
            body_text: Some("Body"),
            attachments: &[EmailJobAttachment::new(
                "report.txt",
                "text/plain",
                b"hello".to_vec(),
            )],
        })
        .await
        .unwrap();

        let mailer = CapturingAttachmentMailer::default();
        let captured = Arc::clone(&mailer.attachments);
        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(mailer));
        dispatcher.tick_once().await.unwrap();

        let sent = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].status, "sent");

        let attachments = captured.lock().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "report.txt");
        assert_eq!(attachments[0].content_type, "text/plain");
        assert_eq!(attachments[0].content, b"hello");
    }

    #[tokio::test]
    async fn dispatcher_processes_jobs_with_bounded_parallelism() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        for idx in 0..(EMAIL_DISPATCH_MAX_IN_FLIGHT + 2) {
            stg.enqueue_email_job(
                "alice",
                &format!("alice+{idx}@example.com"),
                "cl.comment.created",
                "Subject",
                "<p>Body</p>",
                Some("Body"),
            )
            .await
            .unwrap();
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let mailer = TrackingMailer {
            in_flight,
            max_in_flight: Arc::clone(&max_in_flight),
        };
        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(mailer));
        dispatcher.tick_once().await.unwrap();

        let observed = max_in_flight.load(Ordering::SeqCst);
        assert!(
            observed > 1,
            "dispatcher should process more than one email concurrently"
        );
        assert!(
            observed <= EMAIL_DISPATCH_MAX_IN_FLIGHT,
            "dispatcher exceeded max in-flight send limit"
        );

        let sent = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(sent.len(), EMAIL_DISPATCH_MAX_IN_FLIGHT + 2);
        assert!(sent.iter().all(|job| job.status == "sent"));
    }

    #[tokio::test]
    async fn dispatcher_respects_batch_size_for_backpressure() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        for idx in 0..(EMAIL_DISPATCH_BATCH_SIZE + 3) {
            stg.enqueue_email_job(
                "alice",
                &format!("alice+{idx}@example.com"),
                "cl.comment.created",
                "Subject",
                "<p>Body</p>",
                Some("Body"),
            )
            .await
            .unwrap();
        }

        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(NoopMailer));
        dispatcher.tick_once().await.unwrap();

        let all_jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        let sent = all_jobs.iter().filter(|job| job.status == "sent").count();
        let pending = all_jobs
            .iter()
            .filter(|job| job.status == "pending")
            .count();

        assert_eq!(sent, EMAIL_DISPATCH_BATCH_SIZE as usize);
        assert_eq!(pending, 3);
    }

    #[tokio::test]
    async fn dispatcher_respects_configured_batch_size_for_backpressure() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        for idx in 0..5 {
            stg.enqueue_email_job(
                "alice",
                &format!("alice+{idx}@example.com"),
                "cl.comment.created",
                "Subject",
                "<p>Body</p>",
                Some("Body"),
            )
            .await
            .unwrap();
        }

        let control =
            EmailDispatcherControl::new_with_limits(true, EmailDispatcherLimits::new(2, 1));
        let dispatcher =
            EmailDispatcher::new_with_control(stg.clone(), Arc::new(NoopMailer), control);
        dispatcher.tick_once().await.unwrap();

        let all_jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        let sent = all_jobs.iter().filter(|job| job.status == "sent").count();
        let pending = all_jobs
            .iter()
            .filter(|job| job.status == "pending")
            .count();

        assert_eq!(sent, 2);
        assert_eq!(pending, 3);
    }

    #[tokio::test]
    async fn dispatcher_skips_pending_jobs_when_disabled_by_control() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        stg.enqueue_email_job(
            "alice",
            "alice@example.com",
            "cl.comment.created",
            "Subject",
            "<p>Body</p>",
            Some("Body"),
        )
        .await
        .unwrap();

        let control = EmailDispatcherControl::new(false);
        let dispatcher =
            EmailDispatcher::new_with_control(stg.clone(), Arc::new(NoopMailer), control);
        dispatcher.tick_once().await.unwrap();

        let jobs = stg.fetch_pending_jobs(10).await.unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "disabled dispatcher should leave job pending"
        );
        assert_eq!(jobs[0].status, "pending");
    }

    #[tokio::test]
    async fn dispatcher_requeues_failed_send_with_retry_backoff() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        stg.enqueue_email_job(
            "alice",
            "alice@example.com",
            "cl.comment.created",
            "Subject",
            "<p>Body</p>",
            Some("Body"),
        )
        .await
        .unwrap();

        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(FailingMailer));
        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "pending");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_some());
        assert_eq!(
            jobs[0].error_message.as_deref(),
            Some("Other error: smtp unavailable")
        );
    }

    #[tokio::test]
    async fn dispatcher_uses_configured_retry_policy_for_dead_letter() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("cl.comment.created".into()),
            category: Set("cl".into()),
            description: Set("New comment".into()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        stg.enqueue_email_job(
            "alice",
            "alice@example.com",
            "cl.comment.created",
            "Subject",
            "<p>Body</p>",
            Some("Body"),
        )
        .await
        .unwrap();

        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(1, 1, 1),
        );
        let dispatcher =
            EmailDispatcher::new_with_control(stg.clone(), Arc::new(FailingMailer), control);
        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "failed");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_none());
    }

    #[test]
    fn email_dispatcher_subscriber_updates_control_from_mail_enabled() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            enabled: true,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });
        let mut candidate = config.clone();
        candidate.mail.as_mut().expect("mail config").enabled = false;
        let report = ConfigReloadReport {
            applied_fields: vec!["mail.enabled"],
            restart_required_fields: Vec::new(),
        };
        let control = EmailDispatcherControl::new(true);
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_email_dispatcher_subscriber(control.clone()))
            .expect("subscribe");
        let applied_report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(applied_report, report);
        assert!(!control.enabled());
    }

    #[test]
    fn email_dispatcher_subscriber_updates_control_limits_from_mail_config() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            enabled: true,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
            dispatcher_batch_size: 50,
            dispatcher_max_in_flight: 8,
            ..Default::default()
        });
        let control =
            EmailDispatcherControl::from_mail_config(config.mail.as_ref().expect("mail config"));
        let mut candidate = config.clone();
        let mail = candidate.mail.as_mut().expect("mail config");
        mail.dispatcher_batch_size = 11;
        mail.dispatcher_max_in_flight = 4;
        let report = ConfigReloadReport {
            applied_fields: vec![
                "mail.dispatcher_batch_size",
                "mail.dispatcher_max_in_flight",
            ],
            restart_required_fields: Vec::new(),
        };
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_email_dispatcher_subscriber(control.clone()))
            .expect("subscribe");
        let applied_report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(applied_report, report);
        assert_eq!(control.limits(), EmailDispatcherLimits::new(11, 4));
    }

    #[test]
    fn email_dispatcher_subscriber_updates_retry_policy_from_mail_config() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            enabled: true,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });
        let control =
            EmailDispatcherControl::from_mail_config(config.mail.as_ref().expect("mail config"));
        let mut candidate = config.clone();
        let mail = candidate.mail.as_mut().expect("mail config");
        mail.retry_max_attempts = 7;
        mail.retry_backoff_base_secs = 15;
        mail.retry_backoff_max_secs = 120;
        let report = ConfigReloadReport {
            applied_fields: vec![
                "mail.retry_max_attempts",
                "mail.retry_backoff_base_secs",
                "mail.retry_backoff_max_secs",
            ],
            restart_required_fields: Vec::new(),
        };
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_email_dispatcher_subscriber(control.clone()))
            .expect("subscribe");
        let applied_report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(applied_report, report);
        assert_eq!(control.retry_policy(), EmailJobRetryPolicy::new(7, 15, 120));
    }
}
