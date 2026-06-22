use std::sync::{
    Arc, RwLock,
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
        Config, DEFAULT_MAIL_ATTACHMENT_PRUNE_INTERVAL_SECS,
        DEFAULT_MAIL_ATTACHMENT_RETENTION_DAYS, DEFAULT_MAIL_DISPATCHER_BATCH_SIZE,
        DEFAULT_MAIL_DISPATCHER_MAX_IN_FLIGHT, MailConfig,
        redaction::global_redactor,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    jupiter::storage::notification_storage::{
        EMAIL_JOB_SEND_TIMEOUT_SECS, EMAIL_JOB_STATUS_SENT, EMAIL_JOB_STATUS_SKIPPED,
        EmailJobFailureDisposition, EmailJobRetryPolicy, NotificationStorage,
    },
    mail::{MailAttachment, Mailer},
    notification::{
        channels::{EmailChannel, NotificationChannel, OutboundMessage},
        redact::redact_email,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAttachmentPrunePolicy {
    pub enabled: bool,
    pub interval_secs: u64,
    pub retention_days: u32,
    pub statuses: Vec<String>,
}

impl EmailAttachmentPrunePolicy {
    pub fn new(
        enabled: bool,
        interval_secs: u64,
        retention_days: u32,
        statuses: Vec<String>,
    ) -> Self {
        Self {
            enabled,
            interval_secs,
            retention_days,
            statuses,
        }
    }

    pub fn from_mail_config(config: &MailConfig) -> Self {
        Self::new(
            config.attachment_prune_enabled,
            config.attachment_prune_interval_secs,
            config.attachment_retention_days,
            config.attachment_prune_statuses.clone(),
        )
    }
}

impl Default for EmailAttachmentPrunePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: DEFAULT_MAIL_ATTACHMENT_PRUNE_INTERVAL_SECS,
            retention_days: DEFAULT_MAIL_ATTACHMENT_RETENTION_DAYS,
            statuses: vec![
                EMAIL_JOB_STATUS_SENT.to_string(),
                EMAIL_JOB_STATUS_SKIPPED.to_string(),
            ],
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
    attachment_prune_enabled: Arc<AtomicBool>,
    attachment_prune_interval_secs: Arc<AtomicU64>,
    attachment_retention_days: Arc<AtomicU64>,
    attachment_prune_statuses: Arc<RwLock<Vec<String>>>,
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
            EmailAttachmentPrunePolicy::from_mail_config(config),
        )
    }

    pub fn new_with_limits(enabled: bool, limits: EmailDispatcherLimits) -> Self {
        Self::new_with_limits_and_retry_policy(
            enabled,
            limits,
            EmailJobRetryPolicy::default(),
            EmailAttachmentPrunePolicy::default(),
        )
    }

    pub fn new_with_limits_and_retry_policy(
        enabled: bool,
        limits: EmailDispatcherLimits,
        retry_policy: EmailJobRetryPolicy,
        attachment_prune_policy: EmailAttachmentPrunePolicy,
    ) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            batch_size: Arc::new(AtomicU64::new(limits.batch_size)),
            max_in_flight: Arc::new(AtomicUsize::new(limits.max_in_flight)),
            retry_max_attempts: Arc::new(AtomicI32::new(retry_policy.max_attempts)),
            retry_backoff_base_secs: Arc::new(AtomicI64::new(retry_policy.backoff_base_secs)),
            retry_backoff_max_secs: Arc::new(AtomicI64::new(retry_policy.backoff_max_secs)),
            attachment_prune_enabled: Arc::new(AtomicBool::new(attachment_prune_policy.enabled)),
            attachment_prune_interval_secs: Arc::new(AtomicU64::new(
                attachment_prune_policy.interval_secs,
            )),
            attachment_retention_days: Arc::new(AtomicU64::new(
                attachment_prune_policy.retention_days as u64,
            )),
            attachment_prune_statuses: Arc::new(RwLock::new(attachment_prune_policy.statuses)),
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

    pub fn attachment_prune_policy(&self) -> EmailAttachmentPrunePolicy {
        let statuses = self
            .attachment_prune_statuses
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());

        EmailAttachmentPrunePolicy {
            enabled: self.attachment_prune_enabled.load(Ordering::Acquire),
            interval_secs: self.attachment_prune_interval_secs.load(Ordering::Acquire),
            retention_days: self.attachment_retention_days.load(Ordering::Acquire) as u32,
            statuses,
        }
    }

    pub fn set_attachment_prune_policy(&self, policy: EmailAttachmentPrunePolicy) {
        self.attachment_prune_enabled
            .store(policy.enabled, Ordering::Release);
        self.attachment_prune_interval_secs
            .store(policy.interval_secs, Ordering::Release);
        self.attachment_retention_days
            .store(policy.retention_days as u64, Ordering::Release);
        match self.attachment_prune_statuses.write() {
            Ok(mut statuses) => *statuses = policy.statuses,
            Err(poisoned) => *poisoned.into_inner() = policy.statuses,
        }
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
    channel: Arc<dyn NotificationChannel>,
    /// Secondary channels (e.g. in-app inbox) the dispatcher fans each delivered
    /// notification out to, best-effort, after the primary (email) send succeeds.
    secondary_channels: Vec<Arc<dyn NotificationChannel>>,
    control: EmailDispatcherControl,
    attachment_prune_last_run_epoch_secs: AtomicI64,
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
        Self::new_with_channel(stg, Arc::new(EmailChannel::new(mailer)), control)
    }

    /// Construct the dispatcher with an explicit delivery [`NotificationChannel`]
    /// instead of wrapping a raw [`Mailer`]. Used by
    /// [`NotificationService`](crate::notification::service::NotificationService)
    /// to drive the email outbox through the channel abstraction.
    pub fn new_with_channel(
        stg: NotificationStorage,
        channel: Arc<dyn NotificationChannel>,
        control: EmailDispatcherControl,
    ) -> Self {
        Self::new_with_channels(stg, channel, Vec::new(), control)
    }

    /// Construct the dispatcher with a primary channel plus secondary channels
    /// the dispatcher fans out to (best-effort) after the primary send succeeds.
    pub fn new_with_channels(
        stg: NotificationStorage,
        channel: Arc<dyn NotificationChannel>,
        secondary_channels: Vec<Arc<dyn NotificationChannel>>,
        control: EmailDispatcherControl,
    ) -> Self {
        Self {
            stg,
            channel,
            secondary_channels,
            control,
            attachment_prune_last_run_epoch_secs: AtomicI64::new(0),
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
                        warn!("email dispatcher tick error: {}", global_redactor().redact(&e.to_string()));
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

        let attachments_pruned = self.prune_attachments_if_due().await?;
        let limits = self.control.limits();
        let retry_policy = self.control.retry_policy();
        let jobs = self.stg.fetch_pending_jobs(limits.batch_size).await?;
        let mut stats = EmailDispatchTickStats {
            fetched: jobs.len(),
            attachments_pruned,
            ..Default::default()
        };

        for chunk in jobs.chunks(limits.max_in_flight) {
            let mut tasks = JoinSet::new();

            for job in chunk.iter().cloned() {
                let stg = self.stg.clone();
                let channel = Arc::clone(&self.channel);
                let secondaries = self.secondary_channels.clone();
                tasks.spawn(async move {
                    process_email_job(stg, channel, secondaries, job, retry_policy).await
                });
            }

            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok(outcome)) => stats.record(outcome),
                    Ok(Err(e)) => {
                        stats.task_errors += 1;
                        warn!(
                            error = %global_redactor().redact(&e.to_string()),
                            "email dispatcher job processing error"
                        );
                    }
                    Err(e) => {
                        stats.task_errors += 1;
                        warn!(
                            error = %global_redactor().redact(&e.to_string()),
                            "email dispatcher job task failed"
                        );
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
                attachments_pruned = stats.attachments_pruned,
                "email dispatcher tick completed"
            );
        }

        Ok(())
    }

    async fn prune_attachments_if_due(&self) -> Result<u64, sea_orm::DbErr> {
        let policy = self.control.attachment_prune_policy();
        if !policy.enabled {
            return Ok(0);
        }

        let now = chrono::Utc::now();
        let now_epoch_secs = now.timestamp();
        let last_run = self
            .attachment_prune_last_run_epoch_secs
            .load(Ordering::Acquire);
        let interval_secs = policy.interval_secs.min(i64::MAX as u64) as i64;
        if last_run > 0 && now_epoch_secs.saturating_sub(last_run) < interval_secs {
            return Ok(0);
        }

        let older_than = now.naive_utc() - chrono::Duration::days(policy.retention_days as i64);
        let deleted = self
            .stg
            .prune_email_job_attachments(&policy.statuses, older_than, None, None)
            .await?;
        self.attachment_prune_last_run_epoch_secs
            .store(now_epoch_secs, Ordering::Release);

        Ok(deleted)
    }
}

#[derive(Default)]
struct EmailDispatchTickStats {
    fetched: usize,
    attachments_pruned: u64,
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
        self.fetched > 0 || self.attachments_pruned > 0 || self.task_errors > 0
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

/// Process a single outbox job through the resolved delivery channel.
///
/// The span carries the job id, event type, channel name and a **redacted**
/// recipient so delivery is traceable without leaking PII (docs/notification.md
/// phase 0). The full recipient address is never logged.
#[tracing::instrument(
    skip_all,
    fields(
        job_id = job.id,
        event_type = %job.event_type_code,
        channel = channel.name(),
        recipient = %redact_email(&job.to_email),
    )
)]
async fn process_email_job(
    stg: NotificationStorage,
    channel: Arc<dyn NotificationChannel>,
    secondary_channels: Vec<Arc<dyn NotificationChannel>>,
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

    let message = OutboundMessage {
        username: &job.username,
        event_type_code: &job.event_type_code,
        to: &job.to_email,
        subject: &job.subject,
        body_html: &job.body_html,
        body_text: job.body_text.as_deref(),
        attachments: &attachments,
    };
    let send_res = channel.deliver(&message).await;

    match send_res {
        Ok(_) => {
            stg.mark_job_sent(job.id).await?;
            // Fan out to secondary channels (e.g. in-app inbox). Best-effort:
            // a secondary failure is logged but does not change the job status
            // (the email — the primary, retried channel — already succeeded).
            for secondary in &secondary_channels {
                if let Err(e) = secondary.deliver(&message).await {
                    warn!(
                        channel = secondary.name(),
                        error = %global_redactor().redact(&e.to_string()),
                        "secondary notification channel delivery failed"
                    );
                }
            }
            Ok(EmailJobOutcome::Sent)
        }
        Err(e) => match stg
            .mark_job_failed_with_retry_policy(job.id, &e.to_string(), retry_policy)
            .await?
        {
            EmailJobFailureDisposition::RetryScheduled => Ok(EmailJobOutcome::RetryScheduled),
            EmailJobFailureDisposition::DeadLettered => {
                // Alert hook for ops: a notification exhausted its retries and was
                // dead-lettered (docs/notification.md phase 4). The span already
                // carries job_id / event_type / channel / redacted recipient; the
                // raw error (which may contain PII) is persisted to the outbox row,
                // not emitted here.
                warn!(
                    target: "notification_alert",
                    retry_count = job.retry_count,
                    "email notification dead-lettered after exhausting retries"
                );
                Ok(EmailJobOutcome::DeadLettered)
            }
            EmailJobFailureDisposition::MissingJob => Ok(EmailJobOutcome::MissingAfterFailure),
        },
    }
}

fn apply_mail_enabled(
    control: &EmailDispatcherControl,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    if report.applied_fields.contains(&"mail.enabled")
        || report.applied_fields.contains(&"notification.enabled")
    {
        // The dispatcher is enabled only when mail is enabled AND the global
        // notification kill switch is on (docs/notification.md phase 5).
        let mail_enabled = config.mail.as_ref().is_some_and(|mail| mail.enabled);
        let notification_enabled = config
            .notification
            .as_ref()
            .map(|notification| notification.enabled)
            .unwrap_or(true);
        control.set_enabled(mail_enabled && notification_enabled);
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
    if report.applied_fields.iter().any(|field| {
        matches!(
            *field,
            "mail.attachment_prune_enabled"
                | "mail.attachment_prune_interval_secs"
                | "mail.attachment_retention_days"
                | "mail.attachment_prune_statuses"
        )
    }) && let Some(mail) = &config.mail
    {
        control.set_attachment_prune_policy(EmailAttachmentPrunePolicy::from_mail_config(mail));
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
    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        time::Instant,
    };
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{email_jobs, notification_event_types},
        config::{MailProvider, reload::ConfigHandle, testing::isolated_config},
        jupiter::{
            migration::apply_migrations,
            storage::notification_storage::{EmailJobAttachment, EmailJobEnqueue},
            tests::test_db_connection,
        },
        mail::{NoopMailer, SmtpMailer},
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
    async fn integration_mail_dispatcher_mailpit_sends_outbox_job() {
        let mailpit_api_url = mailpit_api_url();
        let mailpit_client = reqwest::Client::new();
        if !mailpit_available(&mailpit_client, &mailpit_api_url).await {
            eprintln!(
                "skipping integration_mail_dispatcher_mailpit_sends_outbox_job: test Mailpit unavailable at {mailpit_api_url}; start it with `docker compose -f docker-compose.test.yml up -d mailpit`"
            );
            return;
        }

        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("Mailpit dispatcher integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>Mailpit body</p>",
            Some("Mailpit body"),
        )
        .await
        .unwrap();

        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: 11025,
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer = SmtpMailer::new_with_password(&mail, None).unwrap();
        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(mailer));

        dispatcher.tick_once().await.unwrap();

        assert!(
            wait_for_mailpit_subject(&mailpit_client, &mailpit_api_url, &subject).await,
            "Mailpit did not receive message with subject `{subject}`"
        );

        let sent = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].status, EMAIL_JOB_STATUS_SENT);
        assert!(sent[0].sent_at.is_some());
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_failure_retries_outbox_job() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP failure integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>SMTP failure body</p>",
            Some("SMTP failure body"),
        )
        .await
        .unwrap();

        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: 1,
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer = SmtpMailer::new_with_password(&mail, None).unwrap();
        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(3, 1, 1),
            EmailAttachmentPrunePolicy::default(),
        );
        let dispatcher = EmailDispatcher::new_with_control(stg.clone(), Arc::new(mailer), control);

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "pending");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_some());
        assert!(jobs[0].sent_at.is_none());
        assert!(jobs[0].error_message.is_some());
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_protocol_rejection_retries_without_credential_leak() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP protocol rejection integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>SMTP protocol rejection body</p>",
            Some("SMTP protocol rejection body"),
        )
        .await
        .unwrap();

        let smtp_port = spawn_smtp_protocol_rejection_server().await;
        let credential_sentinel = "leak-check-credential".to_string();
        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port,
            username: Some("smtp-user".to_string()),
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer =
            SmtpMailer::new_with_password(&mail, Some(credential_sentinel.clone())).unwrap();
        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(3, 1, 1),
            EmailAttachmentPrunePolicy::default(),
        );
        let dispatcher = EmailDispatcher::new_with_control(stg.clone(), Arc::new(mailer), control);

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "pending");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_some());
        assert!(jobs[0].sent_at.is_none());
        let error_message = jobs[0].error_message.as_deref().unwrap_or_default();
        assert!(error_message.contains("smtp send error"));
        assert!(!error_message.contains(&credential_sentinel));
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_auth_rejection_retries_without_credential_leak() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP auth rejection integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>SMTP auth rejection body</p>",
            Some("SMTP auth rejection body"),
        )
        .await
        .unwrap();

        let smtp_port = spawn_smtp_auth_rejection_server().await;
        let credential_sentinel = "auth-leak-check-credential".to_string();
        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port,
            username: Some("smtp-user".to_string()),
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer =
            SmtpMailer::new_with_password(&mail, Some(credential_sentinel.clone())).unwrap();
        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(3, 1, 1),
            EmailAttachmentPrunePolicy::default(),
        );
        let dispatcher = EmailDispatcher::new_with_control(stg.clone(), Arc::new(mailer), control);

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "pending");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_some());
        assert!(jobs[0].sent_at.is_none());
        let error_message = jobs[0].error_message.as_deref().unwrap_or_default();
        assert!(error_message.contains("smtp send error"));
        assert!(!error_message.contains(&credential_sentinel));
        assert!(!error_message.contains("smtp-user"));
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_relay_denied_retries_without_credential_leak() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP permission rejection integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>SMTP permission rejection body</p>",
            Some("SMTP permission rejection body"),
        )
        .await
        .unwrap();

        let smtp_port = spawn_smtp_relay_denied_server().await;
        let credential_sentinel = "permission-leak-check-credential".to_string();
        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port,
            username: Some("smtp-user".to_string()),
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer =
            SmtpMailer::new_with_password(&mail, Some(credential_sentinel.clone())).unwrap();
        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(3, 1, 1),
            EmailAttachmentPrunePolicy::default(),
        );
        let dispatcher = EmailDispatcher::new_with_control(stg.clone(), Arc::new(mailer), control);

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "pending");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_some());
        assert!(jobs[0].sent_at.is_none());
        let error_message = jobs[0].error_message.as_deref().unwrap_or_default();
        assert!(error_message.contains("smtp send error"));
        assert!(error_message.contains("Relay access denied"));
        assert!(!error_message.contains(&credential_sentinel));
        assert!(!error_message.contains("smtp-user"));
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_skips_missing_recipient_without_retry() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP missing recipient integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "",
            "cl.comment.created",
            &subject,
            "<p>Missing recipient body</p>",
            Some("Missing recipient body"),
        )
        .await
        .unwrap();

        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: 1,
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer = SmtpMailer::new_with_password(&mail, None).unwrap();
        let dispatcher = EmailDispatcher::new(stg.clone(), Arc::new(mailer));

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, EMAIL_JOB_STATUS_SKIPPED);
        assert_eq!(jobs[0].retry_count, 0);
        assert!(jobs[0].next_retry_at.is_none());
        assert!(jobs[0].sent_at.is_none());
        assert_eq!(
            jobs[0].error_message.as_deref(),
            Some("missing recipient email")
        );
    }

    #[tokio::test]
    async fn integration_mail_dispatcher_smtp_failure_dead_letters_outbox_job() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let subject = format!("SMTP dead-letter integration {}", Uuid::new_v4());
        stg.enqueue_email_job(
            "alice",
            "alice@example.test",
            "cl.comment.created",
            &subject,
            "<p>SMTP dead-letter body</p>",
            Some("SMTP dead-letter body"),
        )
        .await
        .unwrap();

        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: 1,
            from: "no-reply@example.test".to_string(),
            starttls: false,
            ..Default::default()
        };
        let mailer = SmtpMailer::new_with_password(&mail, None).unwrap();
        let control = EmailDispatcherControl::new_with_limits_and_retry_policy(
            true,
            EmailDispatcherLimits::default(),
            EmailJobRetryPolicy::new(1, 1, 1),
            EmailAttachmentPrunePolicy::default(),
        );
        let dispatcher = EmailDispatcher::new_with_control(stg.clone(), Arc::new(mailer), control);

        dispatcher.tick_once().await.unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "failed");
        assert_eq!(jobs[0].retry_count, 1);
        assert!(jobs[0].next_retry_at.is_none());
        assert!(jobs[0].sent_at.is_none());
        assert!(jobs[0].error_message.is_some());
    }

    #[tokio::test]
    async fn dispatcher_prunes_old_terminal_attachments_by_policy() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        let old_sent_id =
            enqueue_test_email_job_with_attachment(&stg, "old-sent", "Old sent").await;
        stg.mark_job_sent(old_sent_id).await.unwrap();
        make_email_job_old(&db, old_sent_id).await;

        let recent_sent_id =
            enqueue_test_email_job_with_attachment(&stg, "recent-sent", "Recent sent").await;
        stg.mark_job_sent(recent_sent_id).await.unwrap();

        let old_skipped_id =
            enqueue_test_email_job_with_attachment(&stg, "old-skipped", "Old skipped").await;
        stg.mark_job_skipped(old_skipped_id, "no recipient")
            .await
            .unwrap();
        make_email_job_old(&db, old_skipped_id).await;

        let mut mail = crate::config::MailConfig {
            enabled: true,
            ..Default::default()
        };
        mail.attachment_prune_enabled = true;
        mail.attachment_prune_interval_secs = 1;
        mail.attachment_retention_days = 7;
        mail.attachment_prune_statuses = vec![EMAIL_JOB_STATUS_SENT.to_string()];
        let control = EmailDispatcherControl::from_mail_config(&mail);
        let dispatcher =
            EmailDispatcher::new_with_control(stg.clone(), Arc::new(NoopMailer), control);

        dispatcher.tick_once().await.unwrap();

        assert!(
            stg.list_email_job_attachment_metadata(old_sent_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            stg.list_email_job_attachment_metadata(recent_sent_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            stg.list_email_job_attachment_metadata(old_skipped_id)
                .await
                .unwrap()
                .len(),
            1
        );
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
    async fn dispatcher_drains_high_water_queue_across_bounded_ticks() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db.clone()));
        insert_test_event_type(&db).await;

        const TOTAL_JOBS: usize = 17;
        const BATCH_SIZE: u64 = 4;

        for idx in 0..TOTAL_JOBS {
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

        let control = EmailDispatcherControl::new_with_limits(
            true,
            EmailDispatcherLimits::new(BATCH_SIZE, 2),
        );
        let dispatcher =
            EmailDispatcher::new_with_control(stg.clone(), Arc::new(NoopMailer), control);

        for tick in 1..=5 {
            dispatcher.tick_once().await.unwrap();

            let all_jobs = email_jobs::Entity::find().all(&db).await.unwrap();
            let sent = all_jobs.iter().filter(|job| job.status == "sent").count();
            let pending = all_jobs
                .iter()
                .filter(|job| job.status == "pending")
                .count();
            let expected_sent = (tick * BATCH_SIZE as usize).min(TOTAL_JOBS);

            assert_eq!(sent, expected_sent);
            assert_eq!(pending, TOTAL_JOBS - expected_sent);
        }
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
            EmailAttachmentPrunePolicy::default(),
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
    fn email_dispatcher_subscriber_gates_on_global_notification_kill_switch() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            enabled: true,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });
        config.notification = Some(crate::config::NotificationConfig {
            enabled: true,
            ..Default::default()
        });
        let mut candidate = config.clone();
        candidate
            .notification
            .as_mut()
            .expect("notification config")
            .enabled = false;
        let report = ConfigReloadReport {
            applied_fields: vec!["notification.enabled"],
            restart_required_fields: Vec::new(),
        };
        let control = EmailDispatcherControl::new(true);
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_email_dispatcher_subscriber(control.clone()))
            .expect("subscribe");
        let applied_report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(applied_report, report);
        assert!(
            !control.enabled(),
            "global notification kill switch should gate the dispatcher off"
        );
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

    #[test]
    fn email_dispatcher_subscriber_updates_attachment_prune_policy_from_mail_config() {
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
        mail.attachment_prune_enabled = true;
        mail.attachment_prune_interval_secs = 600;
        mail.attachment_retention_days = 14;
        mail.attachment_prune_statuses = vec![EMAIL_JOB_STATUS_SENT.to_string()];
        let report = ConfigReloadReport {
            applied_fields: vec![
                "mail.attachment_prune_enabled",
                "mail.attachment_prune_interval_secs",
                "mail.attachment_retention_days",
                "mail.attachment_prune_statuses",
            ],
            restart_required_fields: Vec::new(),
        };
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_email_dispatcher_subscriber(control.clone()))
            .expect("subscribe");
        let applied_report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(applied_report, report);
        assert_eq!(
            control.attachment_prune_policy(),
            EmailAttachmentPrunePolicy::new(true, 600, 14, vec![EMAIL_JOB_STATUS_SENT.to_string()])
        );
    }

    async fn insert_test_event_type(db: &sea_orm::DatabaseConnection) {
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
        .insert(db)
        .await
        .unwrap();
    }

    async fn enqueue_test_email_job_with_attachment(
        stg: &NotificationStorage,
        username: &str,
        subject: &str,
    ) -> i64 {
        stg.enqueue_email_job_with_attachments(EmailJobEnqueue {
            username,
            to_email: &format!("{username}@example.com"),
            event_type_code: "cl.comment.created",
            subject,
            body_html: "<p>Body</p>",
            body_text: Some("Body"),
            attachments: &[EmailJobAttachment::new(
                format!("{username}.txt"),
                "text/plain",
                b"hello".to_vec(),
            )],
        })
        .await
        .unwrap();

        email_jobs::Entity::find()
            .all(stg.db())
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.username == username && job.subject == subject)
            .unwrap()
            .id
    }

    async fn make_email_job_old(db: &sea_orm::DatabaseConnection, job_id: i64) {
        let mut job: email_jobs::ActiveModel = email_jobs::Entity::find_by_id(job_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .into();
        job.updated_at = Set(chrono::Utc::now().naive_utc() - chrono::Duration::days(10));
        job.update(db).await.unwrap();
    }

    fn mailpit_api_url() -> String {
        std::env::var("MAILPIT_API_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:18025".to_string())
            .trim_end_matches('/')
            .to_string()
    }

    // Probe the test Mailpit HTTP API. Returns false (rather than panicking) when
    // Mailpit is unreachable so the single Mailpit-dependent integration test can
    // skip gracefully instead of failing the whole test binary when the docker
    // compose stack is not running (docs/refactoring/mail.md phase 5 gating).
    async fn mailpit_available(client: &reqwest::Client, api_url: &str) -> bool {
        match client
            .get(format!("{api_url}/api/v1/messages"))
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    async fn spawn_smtp_protocol_rejection_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(b"554 test smtp protocol rejection\r\n")
                    .await;
            }
        });
        port
    }

    async fn spawn_smtp_auth_rejection_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let _ = writer.write_all(b"220 test smtp auth server\r\n").await;

                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes_read = reader.read_line(&mut line).await.unwrap_or_default();
                    if bytes_read == 0 {
                        break;
                    }

                    let command = line.to_ascii_uppercase();
                    if command.starts_with("EHLO") || command.starts_with("HELO") {
                        let _ = writer
                            .write_all(b"250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250 OK\r\n")
                            .await;
                    } else if command.starts_with("AUTH") {
                        let _ = writer
                            .write_all(b"535 5.7.8 Authentication credentials invalid\r\n")
                            .await;
                    } else if command.starts_with("QUIT") {
                        let _ = writer.write_all(b"221 Bye\r\n").await;
                        break;
                    } else {
                        let _ = writer.write_all(b"250 OK\r\n").await;
                    }
                }
            }
        });
        port
    }

    async fn spawn_smtp_relay_denied_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let _ = writer
                    .write_all(b"220 test smtp permission server\r\n")
                    .await;

                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes_read = reader.read_line(&mut line).await.unwrap_or_default();
                    if bytes_read == 0 {
                        break;
                    }

                    let command = line.to_ascii_uppercase();
                    if command.starts_with("EHLO") || command.starts_with("HELO") {
                        let _ = writer
                            .write_all(b"250-localhost\r\n250-AUTH PLAIN\r\n250 OK\r\n")
                            .await;
                    } else if command.starts_with("AUTH") {
                        let _ = writer
                            .write_all(b"235 2.7.0 Authentication successful\r\n")
                            .await;
                    } else if command.starts_with("MAIL FROM") {
                        let _ = writer.write_all(b"250 2.1.0 Sender OK\r\n").await;
                    } else if command.starts_with("RCPT TO") {
                        let _ = writer.write_all(b"550 5.7.1 Relay access denied\r\n").await;
                    } else if command.starts_with("QUIT") {
                        let _ = writer.write_all(b"221 Bye\r\n").await;
                        break;
                    } else {
                        let _ = writer.write_all(b"250 OK\r\n").await;
                    }
                }
            }
        });
        port
    }

    async fn wait_for_mailpit_subject(
        client: &reqwest::Client,
        api_url: &str,
        subject: &str,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);

        loop {
            if let Ok(response) = client
                .get(format!("{api_url}/api/v1/messages"))
                .send()
                .await
                && response.status().is_success()
                && let Ok(payload) = response.json::<Value>().await
                && mailpit_has_subject(&payload, subject)
            {
                return true;
            }

            if Instant::now() >= deadline {
                return false;
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn mailpit_has_subject(payload: &Value, subject: &str) -> bool {
        payload
            .get("messages")
            .or_else(|| payload.get("Messages"))
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("Subject")
                        .or_else(|| message.get("subject"))
                        .and_then(Value::as_str)
                        == Some(subject)
                })
            })
    }
}
