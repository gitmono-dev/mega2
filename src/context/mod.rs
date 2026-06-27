use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    config::{
        reload::ConfigHandle,
        secret::{SecretResolver, VaultSecretResolver},
    },
    jupiter::redis::{ConnectionManager, init_connection},
};

/// This is the main application context for the Mono application.
/// It holds shared state and configuration for the application.
/// Including database connections, configuration settings, encrypted vault functions, etc.
#[derive(Clone)]
pub struct AppContext {
    /// The storage sub-context for the from jupiter abstract layer.
    pub storage: crate::jupiter::storage::Storage,

    /// The vault core for managing encrypted data.
    pub vault: crate::contract::vault::integration::vault_core::VaultCore,

    /// The configuration settings for the application.
    pub config: Arc<crate::config::Config>,

    /// Reloadable configuration handle shared with storage.
    pub config_handle: ConfigHandle,

    pub connection: ConnectionManager,

    /// Token to signal shutdown for notification background tasks (dispatcher etc.).
    /// Created in new() ; callers (e.g. services) can clone and cancel on graceful exit.
    pub notification_shutdown: CancellationToken,
}

impl AppContext {
    /// Creates a new application context with the given configuration.
    pub async fn new(
        config: crate::config::Config,
        object_store: crate::jupiter::storage::object_storage::MegaObjectStorageWrapper,
    ) -> Result<Self, MegaError> {
        let config = Arc::new(config);

        let storage = crate::jupiter::storage::Storage::new(config.clone(), object_store).await?;
        let config_handle = storage.config_handle();
        let connection = init_connection(&config.redis).await?;

        let storage_for_vault = storage.clone();
        let vault_audit = config
            .vault
            .as_ref()
            .map(|vault| vault.audit.clone())
            .unwrap_or_default();
        let vault =
            crate::contract::vault::integration::vault_core::VaultCore::new(storage_for_vault)
                .await?
                .with_audit_config(vault_audit);

        // Late (post-Vault) construction for mail + notification dispatcher (phase 0 per docs/notification.md).
        // Must be after VaultCore (and mail) per config.md bootstrap constraints and docs/mail.md.
        // Spawns the EmailDispatcher background task (using existing outbox + claim logic).
        // The shutdown token is stored so services can coordinate graceful stop if needed.
        let notification_shutdown = CancellationToken::new();
        if let Some(mail_cfg) = &config.mail {
            mail_cfg.validate()?;
            mail_cfg.warn_plaintext_password_deprecated();
            let mail_template_registry =
                crate::notification::triggers::notification_mail_template_registry_from_config(
                    mail_cfg,
                )
                .map_err(|e| {
                    MegaError::Other(format!("mail template initialization failed: {e}"))
                })?;
            crate::notification::triggers::configure_notification_mail_template_registry(
                mail_template_registry,
            )?;
            // Hot-reload the mail template registry when mail.template_* change
            // (sync, file-only rebuild; docs/mail.md phase 4).
            config_handle
                .subscribe(crate::notification::config_reload_mail_template_subscriber())?;

            // Always construct the notification service when mail is configured,
            // even if mail.enabled is false at startup. The dispatcher runs with
            // control.enabled=false and a NoopMailer, so a later reload that sets
            // mail.enabled=true can re-enable mail without a process restart
            // (docs/mail.md phase 4: runtime re-enable).
            let mailer_for_startup: Arc<dyn crate::mail::Mailer> = if mail_cfg.enabled {
                let resolved_password = if mail_cfg.provider == crate::config::MailProvider::Smtp
                    && let Some(secret_ref) = &mail_cfg.password_ref
                {
                    let resolver =
                        VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
                    Some(
                        crate::contract::vault::integration::vault_core::with_audit_caller(
                            "startup:mail-password",
                            resolver.resolve(secret_ref),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                crate::mail::mailer_from_config(mail_cfg, resolved_password)
                    .map_err(|e| MegaError::Other(format!("mail initialization failed: {e}")))?
            } else {
                Arc::new(crate::mail::NoopMailer)
            };
            let notif_stg = storage.notification_storage();
            // Build secret-bearing secondary channels (Slack / generic webhook)
            // post-vault, resolving their credentials through the vault resolver
            // (docs/notification.md phase 3: channel credentials are resolved only
            // after vault is ready).
            let mut extra_channels: Vec<
                Arc<dyn crate::notification::channels::NotificationChannel>,
            > = Vec::new();
            if let Some(notification_cfg) = config.notification.as_ref() {
                // Enforce the notification SecretRef namespace (and required
                // fields) on the real startup path before resolving any channel
                // credential, so a config cannot point a channel ref at an
                // unrelated vault path (the per-validator allowlist must not be
                // bypassable at runtime, only enforced by manual `config validate`).
                crate::config::validate::validate_notification_config(notification_cfg)?;
                let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
                if let Some(slack) = notification_cfg
                    .slack
                    .as_ref()
                    .filter(|slack| slack.enabled)
                {
                    let Some(url_ref) = &slack.webhook_url_ref else {
                        return Err(MegaError::Other(
                            "notification.slack.enabled is true but notification.slack.webhook_url_ref is missing".to_string(),
                        ));
                    };
                    let url = crate::contract::vault::integration::vault_core::with_audit_caller(
                        "startup:notification-slack",
                        resolver.resolve(url_ref),
                    )
                    .await?;
                    extra_channels.push(Arc::new(
                        crate::notification::channels::SlackChannel::new(
                            crate::config::secret::SecretString::new(url),
                        )?,
                    ));
                }
                if let Some(webhook) = notification_cfg
                    .webhook
                    .as_ref()
                    .filter(|webhook| webhook.enabled)
                {
                    let token = if let Some(token_ref) = &webhook.token_ref {
                        Some(crate::config::secret::SecretString::new(
                            crate::contract::vault::integration::vault_core::with_audit_caller(
                                "startup:notification-webhook",
                                resolver.resolve(token_ref),
                            )
                            .await?,
                        ))
                    } else {
                        None
                    };
                    extra_channels.push(Arc::new(
                        crate::notification::channels::WebhookChannel::new(
                            webhook.url.clone(),
                            token,
                        )?,
                    ));
                }
            }
            let service =
                crate::notification::NotificationService::from_mail_config_with_extra_channels(
                    notif_stg,
                    mailer_for_startup,
                    mail_cfg,
                    extra_channels,
                );
            // Honor the global notification kill switch at startup; a later
            // reload can re-enable both mail and notifications without restart.
            let notification_globally_enabled = config
                .notification
                .as_ref()
                .map(|notification| notification.enabled)
                .unwrap_or(true);
            service
                .control()
                .set_enabled(mail_cfg.enabled && notification_globally_enabled);
            config_handle.subscribe(
                crate::notification::config_reload_email_dispatcher_subscriber(service.control()),
            )?;
            // Hot-rebuild the SMTP mailer when connection/credential fields
            // change, or when mail is re-enabled at runtime (async rebuild
            // re-resolves password_ref post-vault; docs/mail.md phase 4).
            config_handle.subscribe(crate::notification::config_reload_mailer_subscriber(
                service.email_mailer_handle(),
                vault.clone(),
            ))?;
            let sd = notification_shutdown.clone();
            tokio::spawn(async move {
                service.start(sd).await;
            });
        }

        storage.mono_service.init_monorepo(&config.monorepo).await?;

        Ok(Self {
            storage,
            vault,
            config,
            config_handle,
            connection,
            notification_shutdown,
        })
    }

    pub fn config(&self) -> Arc<crate::config::Config> {
        self.config_handle
            .snapshot()
            .unwrap_or_else(|_| Arc::clone(&self.config))
    }

    pub fn wrapped_context(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }
}
