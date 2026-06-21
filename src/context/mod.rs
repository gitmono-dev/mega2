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
        let vault =
            crate::contract::vault::integration::vault_core::VaultCore::new(storage_for_vault)
                .await?;

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

            if mail_cfg.enabled {
                let resolved_password = if mail_cfg.provider == crate::config::MailProvider::Smtp
                    && let Some(secret_ref) = &mail_cfg.password_ref
                {
                    let resolver =
                        VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
                    Some(resolver.resolve(secret_ref).await?)
                } else {
                    None
                };
                let mailer: Arc<dyn crate::mail::Mailer> =
                    crate::mail::mailer_from_config(mail_cfg, resolved_password).map_err(|e| {
                        MegaError::Other(format!("mail initialization failed: {e}"))
                    })?;
                let notif_stg = storage.notification_storage();
                let service = crate::notification::NotificationService::from_mail_config(
                    notif_stg, mailer, mail_cfg,
                );
                // Honor the global notification kill switch at startup; the
                // dispatcher still spawns so a later reload can re-enable it.
                let notification_globally_enabled = config
                    .notification
                    .as_ref()
                    .map(|notification| notification.enabled)
                    .unwrap_or(true);
                if !notification_globally_enabled {
                    service.control().set_enabled(false);
                }
                config_handle.subscribe(
                    crate::notification::config_reload_email_dispatcher_subscriber(
                        service.control(),
                    ),
                )?;
                // Hot-rebuild the SMTP mailer when connection/credential fields
                // change (async rebuild re-resolves password_ref post-vault;
                // docs/mail.md phase 4).
                config_handle.subscribe(crate::notification::config_reload_mailer_subscriber(
                    service.email_mailer_handle(),
                    vault.clone(),
                ))?;
                let sd = notification_shutdown.clone();
                tokio::spawn(async move {
                    service.start(sd).await;
                });
            }
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
