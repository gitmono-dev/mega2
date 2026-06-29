//! Notification delivery coordinator.
//!
//! docs/notification.md phase 1: [`NotificationService`] owns the registered
//! [`NotificationChannel`]s and starts the outbox-driven [`EmailDispatcher`] for
//! the email channel. Additional channels (console / in-app / slack) are
//! registered here so future per-channel outboxes can be coordinated from one
//! place without touching the request path. Construction happens post-Vault in
//! `AppContext::new`, after the mailer (and any resolved `mail.password_ref`) is
//! ready.

use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    common::errors::MegaError,
    config::{
        Config, MailConfig,
        redaction::global_redactor,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
        secret::{SecretResolver, VaultSecretResolver},
    },
    contract::vault::integration::vault_core::VaultCore,
    jupiter::storage::notification_storage::NotificationStorage,
    mail::{Mailer, mailer_from_config},
    notification::{
        channels::{
            ConsoleChannel, EmailChannel, InAppChannel, MailerHandle, MailerSlot,
            NotificationChannel,
        },
        dispatcher::{EmailDispatcher, EmailDispatcherControl},
    },
};

/// Coordinates user-notification delivery across one or more channels.
pub struct NotificationService {
    stg: NotificationStorage,
    /// Registered channels in routing-preference order; the email channel is
    /// always first and also drives the `email_jobs` outbox dispatcher.
    channels: Vec<Arc<dyn NotificationChannel>>,
    email_channel: Arc<EmailChannel>,
    control: EmailDispatcherControl,
}

impl NotificationService {
    /// Build a service whose email channel wraps `mailer`, plus any extra
    /// channels. Dispatcher batch / concurrency / retry / prune behaviour comes
    /// from `control`.
    pub fn new(
        stg: NotificationStorage,
        mailer: Arc<dyn Mailer>,
        control: EmailDispatcherControl,
        extra_channels: Vec<Arc<dyn NotificationChannel>>,
    ) -> Self {
        let email_channel = Arc::new(EmailChannel::new(mailer));
        let mut channels = Vec::with_capacity(extra_channels.len() + 1);
        channels.push(Arc::clone(&email_channel) as Arc<dyn NotificationChannel>);
        channels.extend(extra_channels);
        Self {
            stg,
            channels,
            email_channel,
            control,
        }
    }

    /// Convenience constructor from [`MailConfig`].
    ///
    /// Registers the email channel (which drives the `email_jobs` outbox) plus a
    /// secondary [`InAppChannel`] (inbox persistence). The dispatcher fans each
    /// delivered notification out to the in-app channel best-effort after the
    /// email send succeeds (docs/notification.md phase 1/4).
    pub fn from_mail_config(
        stg: NotificationStorage,
        mailer: Arc<dyn Mailer>,
        mail: &MailConfig,
    ) -> Self {
        Self::from_mail_config_with_extra_channels(stg, mailer, mail, Vec::new())
    }

    /// Like [`from_mail_config`](Self::from_mail_config) but also registers
    /// caller-supplied secondary channels (e.g. Slack / webhook built post-vault
    /// with resolved `SecretRef` credentials, docs/notification.md phase 3). The
    /// in-app inbox channel is always registered first among the secondaries, so
    /// the final routing order is `email, in_app, <extra...>`.
    pub fn from_mail_config_with_extra_channels(
        stg: NotificationStorage,
        mailer: Arc<dyn Mailer>,
        mail: &MailConfig,
        extra_channels: Vec<Arc<dyn NotificationChannel>>,
    ) -> Self {
        let inbox: Arc<dyn NotificationChannel> = Arc::new(InAppChannel::new(stg.clone()));
        let mut channels = Vec::with_capacity(extra_channels.len() + 1);
        channels.push(inbox);
        channels.extend(extra_channels);
        Self::new(
            stg,
            mailer,
            EmailDispatcherControl::from_mail_config(mail),
            channels,
        )
    }

    /// The dispatcher control handle, used to register a config-reload
    /// subscriber so `mail.*` runtime changes take effect without restart.
    pub fn control(&self) -> EmailDispatcherControl {
        self.control.clone()
    }

    /// A clonable handle to the email channel's hot-swappable mailer, used to
    /// register the mailer-rebuild reload subscriber
    /// ([`config_reload_mailer_subscriber`]).
    pub fn email_mailer_handle(&self) -> MailerHandle {
        self.email_channel.mailer_handle()
    }

    /// Registered channels, in routing-preference order (email first).
    pub fn channels(&self) -> &[Arc<dyn NotificationChannel>] {
        &self.channels
    }

    /// Resolve a registered channel by its stable name.
    ///
    /// The `email_jobs` outbox is email-only today; additional outbox channel
    /// selectors plug in here as they are introduced.
    pub fn channel_for(&self, channel_name: &str) -> Option<Arc<dyn NotificationChannel>> {
        self.channels
            .iter()
            .find(|channel| channel.name() == channel_name)
            .map(Arc::clone)
    }

    /// Create a [`ConsoleChannel`] as an extra channel for dev/CI dry-run
    /// delivery. The channel logs a redacted summary without actually sending.
    pub fn console_channel() -> Arc<dyn NotificationChannel> {
        Arc::new(ConsoleChannel::new())
    }

    /// Start the background delivery loop(s). Consumes `self`; runs until
    /// `shutdown` is cancelled. The email channel is the primary (retried)
    /// channel; the remaining registered channels are secondary fan-out targets.
    pub async fn start(self, shutdown: CancellationToken) {
        let secondaries: Vec<Arc<dyn NotificationChannel>> =
            self.channels.iter().skip(1).cloned().collect();
        let primary: Arc<dyn NotificationChannel> = self.email_channel;
        let dispatcher =
            EmailDispatcher::new_with_channels(self.stg, primary, secondaries, self.control);
        dispatcher.run(shutdown).await;
    }
}

/// Mail fields whose change requires rebuilding the SMTP mailer at runtime.
const MAILER_REBUILD_FIELDS: &[&str] = &[
    "mail.enabled",
    "mail.provider",
    "mail.smtp_host",
    "mail.smtp_port",
    "mail.username",
    "mail.password",
    "mail.password_ref",
    "mail.from",
    "mail.starttls",
    "mail.http_url",
    "mail.http_headers",
    "mail.http_timeout_secs",
];

/// Config-reload subscriber that hot-rebuilds the SMTP mailer when connection /
/// credential fields change (docs/mail.md phase 4 dynamic mailer rebuild).
///
/// Rebuilding must re-resolve `mail.password_ref` through the (async) vault
/// resolver, but the reload pipeline is synchronous. The subscriber therefore
/// spawns the rebuild onto the current Tokio runtime (the config watcher runs in
/// an async task) and returns immediately; the new mailer is swapped into the
/// shared [`MailerHandle`] when it is ready. On rebuild failure the previous
/// mailer is kept (fail-safe). If no runtime is available, the change is logged
/// as restart-required.
pub fn config_reload_mailer_subscriber(
    mailer_handle: MailerHandle,
    vault: VaultCore,
) -> ConfigReloadSubscriber {
    ConfigReloadSubscriber::new(
        "mail_mailer_rebuild",
        move |config, report| apply_mailer_rebuild(&mailer_handle, &vault, config, report),
        // The async rebuild keeps the previous mailer on failure, so there is no
        // synchronous rollback work to do.
        |_config, _report| Ok(()),
    )
}

fn apply_mailer_rebuild(
    mailer_handle: &MailerHandle,
    vault: &VaultCore,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    let reconfigured = report
        .applied_fields
        .iter()
        .any(|field| MAILER_REBUILD_FIELDS.contains(field));
    if !reconfigured {
        return Ok(());
    }
    let Some(mail) = config.mail.clone() else {
        return Ok(());
    };
    if !mail.enabled {
        // A disabled mail section uses the NoopMailer installed at startup. Do
        // not rebuild (and possibly fail) while mail is off.
        return Ok(());
    }

    let handle = Arc::clone(mailer_handle);
    let vault = vault.clone();
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(rebuild_and_swap_mailer(handle, vault, mail));
        }
        Err(_) => {
            warn!(
                "mail connection/credentials reconfigured but no async runtime is available to rebuild the mailer; a restart is required"
            );
        }
    }

    Ok(())
}

/// Rebuild the SMTP mailer and hot-swap it into `handle`, keeping the previously
/// installed mailer on failure (fail-safe). Extracted from the task spawned by
/// [`apply_mailer_rebuild`] so the success/failure outcomes are directly
/// awaitable in tests instead of only being observable through a detached task.
async fn rebuild_and_swap_mailer(handle: MailerHandle, vault: VaultCore, mail: MailConfig) {
    match rebuild_mailer(&vault, &mail).await {
        Ok(mailer) => {
            handle.store(Arc::new(MailerSlot(mailer)));
            info!("mail mailer hot-rebuilt after config reload");
        }
        Err(e) => {
            warn!(
                error = %global_redactor().redact(&e.to_string()),
                "mail mailer rebuild failed; keeping the previous mailer"
            );
        }
    }
}

async fn rebuild_mailer(
    vault: &VaultCore,
    mail: &MailConfig,
) -> Result<Arc<dyn Mailer>, MegaError> {
    let resolved_password = if mail.provider == crate::config::MailProvider::Smtp
        && let Some(secret_ref) = &mail.password_ref
    {
        let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
        Some(
            crate::contract::vault::integration::vault_core::with_audit_caller(
                "reload:mail-password",
                resolver.resolve(secret_ref),
            )
            .await?,
        )
    } else {
        None
    };
    mailer_from_config(mail, resolved_password)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use tempfile::TempDir;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        callisto::{email_jobs, notification_event_types},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
        mail::NoopMailer,
        notification::channels::ConsoleChannel,
    };

    #[tokio::test]
    async fn service_registers_email_and_extra_channels() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db));
        let inbox: Arc<dyn NotificationChannel> = Arc::new(InAppChannel::new(stg.clone()));
        let console: Arc<dyn NotificationChannel> = Arc::new(ConsoleChannel::new());
        let service = NotificationService::new(
            stg,
            Arc::new(NoopMailer),
            EmailDispatcherControl::new(true),
            vec![inbox, console],
        );

        assert_eq!(service.channels().len(), 3);
        assert_eq!(service.channels()[0].name(), "email");
        assert!(service.channel_for("email").is_some());
        assert!(service.channel_for("in_app").is_some());
        assert!(service.channel_for("console").is_some());
        assert!(service.channel_for("slack").is_none());
    }

    #[tokio::test]
    async fn from_mail_config_with_extra_channels_registers_slack_and_webhook() {
        use crate::notification::channels::{SlackChannel, WebhookChannel};

        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db));
        let slack: Arc<dyn NotificationChannel> = Arc::new(
            SlackChannel::new(crate::config::secret::SecretString::new(
                "http://127.0.0.1:1/services/secret",
            ))
            .unwrap(),
        );
        let webhook: Arc<dyn NotificationChannel> =
            Arc::new(WebhookChannel::new("http://127.0.0.1:1/hook".to_string(), None).unwrap());
        let service = NotificationService::from_mail_config_with_extra_channels(
            stg,
            Arc::new(NoopMailer),
            &crate::config::MailConfig::default(),
            vec![slack, webhook],
        );

        // Order: email, in_app, then the extras.
        assert_eq!(service.channels().len(), 4);
        assert_eq!(service.channels()[0].name(), "email");
        assert_eq!(service.channels()[1].name(), "in_app");
        assert!(service.channel_for("slack").is_some());
        assert!(service.channel_for("webhook").is_some());
    }

    #[tokio::test]
    async fn from_mail_config_registers_in_app_secondary_channel() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db));
        let service = NotificationService::from_mail_config(
            stg,
            Arc::new(NoopMailer),
            &crate::config::MailConfig::default(),
        );

        assert_eq!(service.channels().len(), 2);
        assert!(service.channel_for("email").is_some());
        assert!(service.channel_for("in_app").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn mailer_rebuild_subscriber_swaps_noop_when_mail_re_enabled() {
        use arc_swap::ArcSwap;

        use crate::{
            config::{MailProvider, testing::isolated_config},
            jupiter::tests::test_storage,
            mail::NoopMailer,
        };

        let temp_dir = TempDir::new().unwrap();
        let storage = test_storage(temp_dir.path()).await;
        let key_path = temp_dir.path().join("core_key.json");
        let vault = VaultCore::config(storage.vault_storage(), key_path)
            .await
            .expect("vault should initialize");
        let handle: MailerHandle =
            Arc::new(ArcSwap::from_pointee(MailerSlot(Arc::new(NoopMailer))));
        let original = handle.load().0.clone();

        let mut config = isolated_config(temp_dir.path().join("cfg"));
        config.mail = Some(MailConfig {
            enabled: true,
            provider: MailProvider::Console,
            ..Default::default()
        });
        let mut report = ConfigReloadReport::default();
        report.applied_fields.push("mail.enabled");

        apply_mailer_rebuild(&handle, &vault, &config, &report).expect("apply should succeed");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let swapped = loop {
            let current = handle.load().0.clone();
            if !Arc::ptr_eq(&current, &original) {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            swapped,
            "NoopMailer should be replaced after mail is re-enabled"
        );
    }

    /// docs/mail.md phase 5 ("仍需解析失败"): a hot mailer rebuild whose
    /// `mail.password_ref` cannot be resolved from the vault must fail *safely* —
    /// the resolution error is returned (never panicking), it carries only the
    /// redacted SecretRef (no path/field/value leak), and `apply_mailer_rebuild`
    /// keeps the previously installed mailer instead of swapping in a half-built
    /// one. This is the failure counterpart to
    /// `mailer_rebuild_subscriber_swaps_noop_when_mail_re_enabled`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn mailer_rebuild_fails_safely_and_redacts_when_password_ref_unresolvable() {
        use arc_swap::ArcSwap;

        use crate::{
            config::{MailProvider, secret::SecretRef},
            jupiter::tests::test_storage,
            mail::NoopMailer,
        };

        let temp_dir = TempDir::new().unwrap();
        let storage = test_storage(temp_dir.path()).await;
        let key_path = temp_dir.path().join("core_key.json");
        let vault = VaultCore::config(storage.vault_storage(), key_path)
            .await
            .expect("vault should initialize");

        // SMTP mail whose password_ref points at a vault path that holds no
        // secret, so re-resolution during the rebuild fails.
        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let mail = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("apikey".to_string()),
            password: None,
            password_ref: Some(secret_ref),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        };

        // The rebuild surfaces an Err (not a panic), and neither the raw nor the
        // redacted error leaks the SecretRef path/field.
        let err = match rebuild_mailer(&vault, &mail).await {
            Ok(_) => panic!("an unresolvable password_ref must fail the rebuild"),
            Err(e) => e,
        };
        let raw = err.to_string();
        let redacted = global_redactor().redact(&raw);
        for needle in ["config/test/mail/password", "#value"] {
            assert!(!raw.contains(needle), "raw rebuild error leaked `{needle}`");
            assert!(
                !redacted.contains(needle),
                "redacted rebuild error leaked `{needle}`"
            );
        }

        // End-to-end fail-safe: the swap body that apply_mailer_rebuild spawns
        // must keep the previously installed mailer when the rebuild fails.
        // Awaiting it directly drives the failure branch to a deterministic
        // completion (no detached task to race against), proving the branch ran
        // and that it never swapped in a half-built mailer.
        let handle: MailerHandle =
            Arc::new(ArcSwap::from_pointee(MailerSlot(Arc::new(NoopMailer))));
        let original = handle.load().0.clone();

        rebuild_and_swap_mailer(handle.clone(), vault.clone(), mail).await;

        assert!(
            Arc::ptr_eq(&handle.load().0, &original),
            "a failed rebuild must keep the previous mailer"
        );
    }

    #[tokio::test]
    async fn service_start_delivers_email_outbox_then_stops_on_shutdown() {
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

        let service = NotificationService::from_mail_config(
            stg.clone(),
            Arc::new(NoopMailer),
            &crate::config::MailConfig {
                enabled: true,
                ..Default::default()
            },
        );
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { service.start(shutdown).await }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let delivered = loop {
            let pending = stg.fetch_pending_jobs(10).await.unwrap();
            if pending.is_empty() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        shutdown.cancel();
        handle.await.unwrap();

        assert!(delivered, "service should drain the email outbox");
        let sent = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].status, "sent");

        // The in-app secondary channel persisted an inbox row for the recipient.
        let inbox = stg.list_inbox_notifications("alice", 10).await.unwrap();
        assert_eq!(inbox.len(), 1, "in-app channel should persist an inbox row");
        assert_eq!(inbox[0].event_type_code, "cl.comment.created");
    }

    /// Multichannel fan-out (docs/integration.md): a registered WebhookChannel
    /// receives each notification the dispatcher delivers, after the email
    /// (primary) send succeeds — the end-to-end proof that the phase-3 secret
    /// channels integrate with the live dispatcher, not just in isolation.
    #[tokio::test]
    async fn service_start_fans_out_delivery_to_webhook_channel() {
        use std::sync::Mutex as StdMutex;

        use axum::{Router, routing::post};

        use crate::notification::channels::WebhookChannel;

        let received: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |axum::extract::State(state): axum::extract::State<
                        Arc<StdMutex<Vec<serde_json::Value>>>,
                    >,
                     body: axum::body::Bytes| async move {
                        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
                            state.lock().unwrap().push(value);
                        }
                        "ok"
                    },
                ),
            )
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}/hook");

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
            "bob",
            "bob@example.com",
            "cl.comment.created",
            "Webhook Subject",
            "<p>Body</p>",
            Some("Body"),
        )
        .await
        .unwrap();

        let webhook: Arc<dyn NotificationChannel> =
            Arc::new(WebhookChannel::new(url, None).expect("build webhook channel"));
        let service = NotificationService::from_mail_config_with_extra_channels(
            stg.clone(),
            Arc::new(NoopMailer),
            &crate::config::MailConfig {
                enabled: true,
                ..Default::default()
            },
            vec![webhook],
        );
        // Registration order: email, in_app, webhook.
        assert!(service.channel_for("webhook").is_some());

        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { service.start(shutdown).await }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let delivered = loop {
            if !received.lock().unwrap().is_empty() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        shutdown.cancel();
        handle.await.unwrap();
        server.abort();

        assert!(
            delivered,
            "webhook channel should receive the fan-out delivery"
        );
        let payloads = received.lock().unwrap().clone();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["username"], "bob");
        assert_eq!(payloads[0]["event_type"], "cl.comment.created");
        assert_eq!(payloads[0]["subject"], "Webhook Subject");
        assert_eq!(payloads[0]["body_html"], "<p>Body</p>");
        assert_eq!(payloads[0]["body_text"], "Body");
    }
}
