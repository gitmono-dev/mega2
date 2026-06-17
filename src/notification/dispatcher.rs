use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::time::{Duration, interval};
use tracing::{info, warn};

use crate::{
    common::errors::MegaError,
    config::{
        Config,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    jupiter::storage::notification_storage::NotificationStorage,
    mail::Mailer,
};

#[derive(Debug, Clone)]
pub struct EmailDispatcherControl {
    enabled: Arc<AtomicBool>,
}

impl EmailDispatcherControl {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
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

        let jobs = self.stg.fetch_pending_jobs(50).await?;
        for job in jobs {
            if job.to_email.trim().is_empty() {
                let _ = self
                    .stg
                    .mark_job_skipped(job.id, "missing recipient email")
                    .await;
                continue;
            }
            if !self.stg.try_claim_job(job.id).await? {
                continue;
            }
            let send_res = self
                .mailer
                .send_html(
                    &job.to_email,
                    &job.subject,
                    &job.body_html,
                    job.body_text.as_deref(),
                )
                .await;

            match send_res {
                Ok(_) => {
                    let _ = self.stg.mark_job_sent(job.id).await;
                }
                Err(e) => {
                    let _ = self
                        .stg
                        .mark_job_failed_with_retry(job.id, &e.to_string())
                        .await;
                }
            }
        }
        Ok(())
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{email_jobs, notification_event_types},
        config::{reload::ConfigHandle, testing::isolated_config},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
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

    #[test]
    fn email_dispatcher_subscriber_updates_control_from_mail_enabled() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
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
}
