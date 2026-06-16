use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::jupiter::redis::{ConnectionManager, init_connection};

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
    pub config: Arc<crate::common::config::Config>,

    pub connection: ConnectionManager,

    /// Token to signal shutdown for notification background tasks (dispatcher etc.).
    /// Created in new() ; callers (e.g. services) can clone and cancel on graceful exit.
    pub notification_shutdown: CancellationToken,
}

impl AppContext {
    /// Creates a new application context with the given configuration.
    pub async fn new(config: crate::common::config::Config) -> Self {
        let config = Arc::new(config);

        let storage = crate::jupiter::storage::Storage::new(config.clone())
            .await
            .expect("init monorepo storage err");
        let connection = init_connection(&config.redis).await;

        let storage_for_vault = storage.clone();
        let vault =
            crate::contract::vault::integration::vault_core::VaultCore::new(storage_for_vault)
                .await;

        // Late (post-Vault) construction for mail + notification dispatcher (phase 0 per docs/notification.md).
        // Must be after VaultCore (and mail) per config.md bootstrap constraints and docs/mail.md.
        // Spawns the EmailDispatcher background task (using existing outbox + claim logic).
        // The shutdown token is stored so services can coordinate graceful stop if needed.
        let notification_shutdown = CancellationToken::new();
        if let Some(mail_cfg) = &config.mail
            && mail_cfg.enabled
            && let Ok(m) = crate::mail::SmtpMailer::new(mail_cfg)
        {
            let mailer: Arc<dyn crate::mail::Mailer> = Arc::new(m);
            let notif_stg = storage.notification_storage();
            let dispatcher = crate::notification::EmailDispatcher::new(notif_stg, mailer);
            let sd = notification_shutdown.clone();
            tokio::spawn(async move {
                dispatcher.run(sd).await;
            });
        }

        storage
            .mono_service
            .init_monorepo(&config.monorepo)
            .await
            .expect("init monorepo failed");

        Self {
            storage,
            vault,
            config,
            connection,
            notification_shutdown,
        }
    }

    pub fn wrapped_context(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }
}
