use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    config::{
        ObjectStorageConfig,
        reload::ConfigHandle,
        secret::{SecretRef, SecretResolver, VaultSecretResolver, is_secret_ref_value},
        validate::validate_config_secret_ref,
    },
    contract::vault::integration::vault_core::{VaultCore, with_audit_caller},
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
    ///
    /// Staged bootstrap (config.md stage 6 / vault.md stage G): the DB connection
    /// is built once and shared by a DB-only `VaultCore` bootstrap and the full
    /// `Storage`. Building vault before the object store lets object-storage
    /// credentials supplied as vault `SecretRef`s
    /// (`object_storage.s3.access_key_id` / `secret_access_key` = `vault://…`) be
    /// resolved before the object store is constructed. The concrete object store
    /// is built here through the binary-registered `ObjectStorageProvider`, so the
    /// callers no longer pre-build and inject it.
    pub async fn new(config: crate::config::Config) -> Result<Self, MegaError> {
        let config = Arc::new(config);

        // One DB connection, shared by the bootstrap vault and the full storage.
        let db_connection =
            Arc::new(crate::jupiter::storage::init::database_connection(&config.database).await?);

        // Vault first (DB-only bootstrap), so SecretRef object-storage credentials
        // can be resolved before the object store is built.
        let vault_audit = config
            .vault
            .as_ref()
            .map(|vault| vault.audit.clone())
            .unwrap_or_default();
        let vault =
            crate::contract::vault::integration::vault_core::VaultCore::from_database_connection(
                db_connection.clone(),
                crate::contract::vault::integration::vault_core::VaultCore::default_key_path(),
            )
            .await?
            .with_audit_config(vault_audit);

        // Resolve any `vault://` SecretRef object-storage credentials post-vault,
        // then build the concrete object store from the resolved config.
        let object_storage_config =
            resolve_object_storage_secrets(&config.object_storage, &vault).await?;
        let object_store =
            crate::jupiter::storage::object_storage::build_object_storage(&object_storage_config)
                .await?;

        let storage = crate::jupiter::storage::Storage::new_with_connection(
            config.clone(),
            db_connection,
            object_store,
        )
        .await?;
        let config_handle = storage.config_handle();
        let connection = init_connection(&config.redis).await?;

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

/// Resolve a credential that may be either a literal value or a `vault://`
/// SecretRef URI. Literals are returned unchanged; SecretRef URIs are resolved
/// through the vault resolver (post-vault). Leading whitespace is ignored when
/// deciding whether the value is a SecretRef, matching `config validate`.
async fn resolve_credential(
    value: &str,
    resolver: &VaultSecretResolver,
    caller: &str,
) -> Result<String, MegaError> {
    let trimmed = value.trim_start();
    if is_secret_ref_value(trimmed) {
        let secret_ref = SecretRef::parse(trimmed)?;
        with_audit_caller(caller, resolver.resolve(&secret_ref)).await
    } else {
        Ok(value.to_string())
    }
}

/// Resolve any `vault://` SecretRef object-storage credentials
/// (`object_storage.s3.access_key_id` / `secret_access_key`) against the
/// already-bootstrapped vault, returning a config with literal credentials ready
/// for `build_object_storage`. Literal credentials are passed through unchanged,
/// so deployments that keep S3 creds in env/IAM are unaffected (config.md stage 6
/// / vault.md stage G).
async fn resolve_object_storage_secrets(
    config: &ObjectStorageConfig,
    vault: &VaultCore,
) -> Result<ObjectStorageConfig, MegaError> {
    // Object-storage vault refs are only meaningful for S3/S3-compatible
    // backends; Local/GCS configs do not consume the `s3.*` fields.
    let s3_like = matches!(
        config.storage_type,
        orbit_api::factory::ObjectStorageBackend::S3
            | orbit_api::factory::ObjectStorageBackend::S3Compatible
    );
    if !s3_like {
        return Ok(config.clone());
    }

    let access_key_id_trimmed = config.s3.access_key_id.trim_start();
    let secret_access_key_trimmed = config.s3.secret_access_key.trim_start();
    let access_key_id_is_ref = is_secret_ref_value(access_key_id_trimmed);
    let secret_access_key_is_ref = is_secret_ref_value(secret_access_key_trimmed);
    if !access_key_id_is_ref && !secret_access_key_is_ref {
        return Ok(config.clone());
    }

    // Enforce the same namespace as `config validate` so a config cannot point
    // an object-storage credential at an unrelated vault path at runtime.
    if access_key_id_is_ref {
        let secret_ref = SecretRef::parse(access_key_id_trimmed)?;
        validate_config_secret_ref(
            "object_storage.s3.access_key_id",
            &secret_ref,
            "object_storage/access_key_id",
        )?;
    }
    if secret_access_key_is_ref {
        let secret_ref = SecretRef::parse(secret_access_key_trimmed)?;
        validate_config_secret_ref(
            "object_storage.s3.secret_access_key",
            &secret_ref,
            "object_storage/secret_access_key",
        )?;
    }

    let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
    let mut resolved = config.clone();
    resolved.s3.access_key_id = resolve_credential(
        &config.s3.access_key_id,
        &resolver,
        "startup:object-storage-access-key",
    )
    .await?;
    resolved.s3.secret_access_key = resolve_credential(
        &config.s3.secret_access_key,
        &resolver,
        "startup:object-storage-secret-key",
    )
    .await?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value};

    use super::*;
    use crate::{
        contract::vault::integration::vault_core::VaultCoreInterface, jupiter::tests::test_storage,
    };

    #[test]
    fn is_secret_ref_value_detects_vault_uri() {
        assert!(is_secret_ref_value(
            "vault://secret/config/prod/object_storage/access_key_id#value"
        ));
        assert!(!is_secret_ref_value("AKIAEXAMPLE"));
        assert!(!is_secret_ref_value(""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_object_storage_secrets_passes_through_literal_credentials() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut config = ObjectStorageConfig::default();
        config.s3.access_key_id = "AKIA-literal".to_string();
        config.s3.secret_access_key = "literal-secret".to_string();

        let resolved = resolve_object_storage_secrets(&config, &vault)
            .await
            .expect("literal credentials resolve");
        assert_eq!(resolved.s3.access_key_id, "AKIA-literal");
        assert_eq!(resolved.s3.secret_access_key, "literal-secret");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_object_storage_secrets_resolves_vault_secret_refs() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut access = Map::new();
        access.insert(
            "value".to_string(),
            Value::String("AKIA-from-vault".to_string()),
        );
        vault
            .write_secret("config/test/object_storage/access_key_id", Some(access))
            .await
            .expect("write access key secret");

        let mut config = ObjectStorageConfig {
            storage_type: orbit_api::factory::ObjectStorageBackend::S3,
            ..Default::default()
        };
        config.s3.access_key_id =
            "vault://secret/config/test/object_storage/access_key_id#value".to_string();
        // Mixed: the secret access key stays a literal.
        config.s3.secret_access_key = "literal-secret".to_string();

        let resolved = resolve_object_storage_secrets(&config, &vault)
            .await
            .expect("secret-ref credentials resolve");
        assert_eq!(resolved.s3.access_key_id, "AKIA-from-vault");
        assert_eq!(resolved.s3.secret_access_key, "literal-secret");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_object_storage_secrets_resolves_both_credential_refs() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        for (name, value) in [
            ("config/test/object_storage/access_key_id", "AKIA-both"),
            (
                "config/test/object_storage/secret_access_key",
                "SECRET-both",
            ),
        ] {
            let mut data = Map::new();
            data.insert("value".to_string(), Value::String(value.to_string()));
            vault.write_secret(name, Some(data)).await.expect("write");
        }

        let mut config = ObjectStorageConfig {
            storage_type: orbit_api::factory::ObjectStorageBackend::S3,
            ..Default::default()
        };
        config.s3.access_key_id =
            "vault://secret/config/test/object_storage/access_key_id#value".to_string();
        config.s3.secret_access_key =
            "vault://secret/config/test/object_storage/secret_access_key#value".to_string();

        let resolved = resolve_object_storage_secrets(&config, &vault)
            .await
            .expect("both secret refs resolve");
        assert_eq!(resolved.s3.access_key_id, "AKIA-both");
        assert_eq!(resolved.s3.secret_access_key, "SECRET-both");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_object_storage_secrets_errors_on_missing_secret_without_panicking() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut config = ObjectStorageConfig {
            storage_type: orbit_api::factory::ObjectStorageBackend::S3,
            ..Default::default()
        };
        config.s3.access_key_id =
            "vault://secret/config/test/object_storage/missing#value".to_string();
        config.s3.secret_access_key = "literal".to_string();

        // A missing secret must fail (Result), not panic, so startup surfaces a
        // diagnostic instead of crashing.
        let result = resolve_object_storage_secrets(&config, &vault).await;
        assert!(result.is_err(), "missing object-storage secret must error");
    }
}
