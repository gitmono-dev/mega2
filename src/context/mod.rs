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
    pub async fn new(config: crate::config::Config) -> Result<Self, MegaError> {
        let config = Arc::new(config);

        let storage = crate::jupiter::storage::Storage::new(config.clone()).await?;
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

            if mail_cfg.enabled {
                let mailer: Arc<dyn crate::mail::Mailer> =
                    if let Some(secret_ref) = &mail_cfg.password_ref {
                        let resolver =
                            VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
                        let resolved_password = resolver.resolve(secret_ref).await?;
                        Arc::new(
                            crate::mail::SmtpMailer::new_with_password(
                                mail_cfg,
                                Some(resolved_password),
                            )
                            .map_err(|e| {
                                MegaError::Other(format!("mail initialization failed: {e}"))
                            })?,
                        )
                    } else {
                        Arc::new(crate::mail::SmtpMailer::new(mail_cfg).map_err(|e| {
                            MegaError::Other(format!("mail initialization failed: {e}"))
                        })?)
                    };
                let notif_stg = storage.notification_storage();
                let dispatcher = crate::notification::EmailDispatcher::new(notif_stg, mailer);
                let sd = notification_shutdown.clone();
                tokio::spawn(async move {
                    dispatcher.run(sd).await;
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
