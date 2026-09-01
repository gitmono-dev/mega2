use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    config::{
        ObjectStorageConfig,
        reload::ConfigHandle,
        secret::{SecretRef, SecretResolver, VaultSecretResolver, is_secret_ref_value},
        validate::{validate_config_secret_ref, validate_redis_url_literal},
    },
    contract::{
        policy::entitystore::SharedEntityStore,
        vault::integration::vault_core::{VaultCore, with_audit_caller},
    },
    jupiter::{
        redis::{
            ConnectionManager, SnowflakeWorkerLease, claim_snowflake_worker,
            claim_snowflake_worker_for_id, init_connection_lazy,
        },
        storage::init::database_connection_without_id_generator,
        utils::id_generator,
    },
};

const MIN_WORKER_FENCE_CONNECTIONS: u32 = 2;

fn validate_worker_fence_database_config(
    database: &crate::config::DbConfig,
) -> Result<(), MegaError> {
    if database.max_connection < MIN_WORKER_FENCE_CONNECTIONS {
        return Err(MegaError::Other(format!(
            "database.max_connection must be at least {MIN_WORKER_FENCE_CONNECTIONS} for the process-wide worker fence"
        )));
    }
    Ok(())
}

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

    /// The process-owned Redis worker lease, when worker selection used Redis.
    /// Keeping the guard in the context makes refresh lifetime and shutdown
    /// explicit instead of leaving a detached task behind.
    pub(crate) worker_lease: Option<SnowflakeWorkerLease>,

    /// Token to signal shutdown for notification background tasks (dispatcher etc.).
    /// Created in new() ; callers (e.g. services) can clone and cancel on graceful exit.
    pub notification_shutdown: CancellationToken,

    /// Shared authorization snapshot holder (ADR-UN-02). Unique owner created in
    /// `new()`; the same `Arc` is injected into `Storage` and the HTTP state so
    /// the write path (notify) and read path (guard/push) share one instance.
    pub entity_store: Arc<SharedEntityStore>,
}

struct StartupLeaseGuard(Option<SnowflakeWorkerLease>);

impl StartupLeaseGuard {
    fn new(lease: Option<SnowflakeWorkerLease>) -> Self {
        Self(lease)
    }

    fn take(&mut self) -> Option<SnowflakeWorkerLease> {
        self.0.take()
    }

    async fn shutdown(&mut self) -> Result<(), MegaError> {
        if let Some(lease) = self.0.take() {
            lease.shutdown().await
        } else {
            Ok(())
        }
    }
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
    /// is built here via the inlined orbit factory, so callers no longer
    /// pre-build and inject it.
    pub async fn new(config: crate::config::Config) -> Result<Self, MegaError> {
        let config = Arc::new(config);

        id_generator::validate_layout_version()?;
        validate_worker_fence_database_config(&config.database)?;

        // One DB connection, shared by the bootstrap vault and the full storage.
        let db_connection =
            Arc::new(database_connection_without_id_generator(&config.database).await?);

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

        // Resolve Redis before the first ID-generator initialization so a
        // multi-instance deployment can claim a worker slot instead of using
        // the legacy fixed worker ID.
        let redis_config = resolve_redis_url_secret(&config.redis, &vault).await?;
        let connection = init_connection_lazy(&redis_config)?;
        let worker_lease = if let Some(worker_id) = id_generator::configured_env_worker_id() {
            let lease = claim_snowflake_worker_for_id(&connection, &db_connection, worker_id)
                .await?
                .ok_or_else(|| {
                    MegaError::IdGenerationUnavailable(format!(
                        "configured worker ID {worker_id} is unavailable in Redis or PostgreSQL"
                    ))
                })?;
            let health = lease.health();
            if let Err(error) = id_generator::initialize_worker(
                worker_id,
                id_generator::WorkerIdSource::Env,
                Some(health),
            ) {
                let _ = lease.shutdown().await;
                return Err(error);
            }
            lease.start_refresh();
            tracing::info!(
                source = ?id_generator::WorkerIdSource::Env,
                "valid MEGA_ID_GENERATOR_WORKER_ID set; claimed its Redis worker lease"
            );
            Some(lease)
        } else if let Some(lease) = claim_snowflake_worker(&connection, &db_connection).await? {
            let worker_id = lease.worker_id();
            let health = lease.health();
            if let Err(error) = id_generator::initialize_worker(
                worker_id,
                id_generator::WorkerIdSource::Redis,
                Some(health),
            ) {
                let _ = lease.shutdown().await;
                return Err(error);
            }
            lease.start_refresh();
            Some(lease)
        } else {
            let identity = id_generator::process_identity();
            let worker_id = id_generator::hash_worker_id(&identity);
            tracing::error!(
                worker_id,
                source = ?id_generator::WorkerIdSource::Hash,
                process_identity = %id_generator::identity_digest(&identity),
                "Redis worker lease unavailable; refusing writable application startup"
            );
            return Err(MegaError::IdGenerationUnavailable(
                "an exclusive worker ID could not be selected; stable identity hash is diagnostic-only"
                    .to_string(),
            ));
        };

        let mut worker_lease_guard = StartupLeaseGuard::new(worker_lease);
        let notification_shutdown = CancellationToken::new();
        let startup_notification_shutdown = notification_shutdown.clone();
        let result = async {
            // Resolve any `vault://` SecretRef object-storage credentials post-vault,
            // then build the concrete object store from the resolved config.
            let object_storage_config =
                resolve_object_storage_secrets(&config.object_storage, &vault).await?;
            let object_store = crate::jupiter::storage::object_storage::build_object_storage(
                &object_storage_config,
            )
            .await?;

            let storage = crate::jupiter::storage::Storage::new_with_connection(
                config.clone(),
                db_connection,
                object_store,
            )
            .await?;
            let config_handle = storage.config_handle();

            // Create the shared authorization snapshot holder (ADR-UN-02) and inject
            // the same `Arc` into `Storage` (write-path notify) and the HTTP state
            // (read-path guard/push). First-build `ensure` happens before the HTTP
            // listener binds (UN-02).
            let entity_store = Arc::new(SharedEntityStore::new());
            let mut storage = storage;
            storage.set_entity_store(entity_store.clone());
            // MC-09: the server-signing vault handle reaches the synthetic-commit
            // sites through storage; vault is built before storage above.
            let storage = storage.with_vault(vault.clone());

            // Build notification channels after Vault so optional Slack and webhook
            // credentials can be resolved. In-app delivery does not require `[mail]`.
            let notif_stg = storage.notification_storage();
            let mut extra_channels: Vec<
                Arc<dyn crate::notification::channels::NotificationChannel>,
            > = Vec::new();
            let mut website_mail = None;
            let (notification_enabled, default_delivery_mode) = match config.notification.as_ref() {
                Some(notification_cfg) => {
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
                    if !notification_cfg.website_mail_base_url.trim().is_empty() {
                        let bearer = match (
                            &notification_cfg.website_mail_bearer,
                            &notification_cfg.website_mail_bearer_ref,
                        ) {
                            (Some(bearer), None) => bearer.clone(),
                            (None, Some(bearer_ref)) => crate::config::secret::SecretString::new(
                                crate::contract::vault::integration::vault_core::with_audit_caller(
                                    "startup:notification-website-mail",
                                    resolver.resolve(bearer_ref),
                                )
                                .await?,
                            ),
                            _ => {
                                return Err(MegaError::Other(
                                    "website mail configuration must provide exactly one bearer source"
                                        .to_string(),
                                ));
                            }
                        };
                        website_mail = Some(Arc::new(
                            crate::notification::website_mail::WebsiteMailClient::new(
                                &notification_cfg.website_mail_base_url,
                                bearer,
                            )?,
                        ));
                    }
                    (
                        notification_cfg.enabled,
                        notification_cfg.default_delivery_mode.clone(),
                    )
                }
                None => (
                    true,
                    crate::config::DEFAULT_NOTIFICATION_DELIVERY_MODE.to_string(),
                ),
            };
            let service = Arc::new(crate::notification::NotificationService::new(
                notif_stg,
                extra_channels,
                website_mail,
                Some(config_handle.clone()),
                notification_enabled,
                default_delivery_mode,
            ));
            crate::notification::NotificationService::set_active(Some(Arc::clone(&service)));
            let sd = notification_shutdown.clone();
            tokio::spawn(async move {
                service.start(sd).await;
            });

            storage.mono_service.init_monorepo(&config.monorepo).await?;

            Ok(Self {
                storage,
                vault,
                config,
                config_handle,
                connection,
                worker_lease: worker_lease_guard.take(),
                notification_shutdown,
                entity_store,
            })
        }
        .await;

        if result.is_err() {
            startup_notification_shutdown.cancel();
            if let Err(error) = worker_lease_guard.shutdown().await {
                tracing::warn!(
                    error = %error,
                    "failed to release snowflake worker lease after startup error"
                );
            }
        }
        result
    }

    pub fn config(&self) -> Arc<crate::config::Config> {
        self.config_handle
            .snapshot()
            .unwrap_or_else(|_| Arc::clone(&self.config))
    }

    pub fn wrapped_context(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }

    /// Stop background work and release the Redis worker lease, if any.
    pub async fn shutdown(&self) -> Result<(), MegaError> {
        self.notification_shutdown.cancel();
        if let Some(lease) = &self.worker_lease {
            lease.shutdown().await?;
        }
        Ok(())
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
/// Whether resolving this object-storage config will need the vault.
///
/// Same two conditions `resolve_object_storage_secrets` short-circuits on, named
/// so the read-only assembly can ask the question without opening a vault it may
/// not need — and so the two cannot drift apart (UN-30).
fn object_storage_needs_vault(config: &ObjectStorageConfig) -> bool {
    let s3_like = matches!(
        config.storage_type,
        crate::orbit_api::factory::ObjectStorageBackend::S3
            | crate::orbit_api::factory::ObjectStorageBackend::S3Compatible
    );
    if !s3_like {
        return false;
    }

    is_secret_ref_value(config.s3.access_key_id.trim_start())
        || is_secret_ref_value(config.s3.secret_access_key.trim_start())
}

async fn resolve_object_storage_secrets(
    config: &ObjectStorageConfig,
    vault: &VaultCore,
) -> Result<ObjectStorageConfig, MegaError> {
    // Object-storage vault refs are only meaningful for S3/S3-compatible
    // backends; Local/GCS configs do not consume the `s3.*` fields.
    let s3_like = matches!(
        config.storage_type,
        crate::orbit_api::factory::ObjectStorageBackend::S3
            | crate::orbit_api::factory::ObjectStorageBackend::S3Compatible
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
    debug_assert!(object_storage_needs_vault(config));

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

/// Resolve a `vault://` SecretRef in `redis.url` against the already-bootstrapped
/// vault, returning a config with a literal URL ready for `init_connection`.
/// Literal URLs are passed through unchanged so deployments that keep the Redis
/// URL in env/IAM are unaffected.
async fn resolve_redis_url_secret(
    config: &crate::config::RedisConfig,
    vault: &VaultCore,
) -> Result<crate::config::RedisConfig, MegaError> {
    let trimmed = config.url.trim_start();
    if !is_secret_ref_value(trimmed) {
        return Ok(config.clone());
    }
    // Enforce the same namespace as `config validate` so a config cannot point
    // the Redis URL at an unrelated vault path at runtime.
    let secret_ref = SecretRef::parse(trimmed)?;
    validate_config_secret_ref("redis.url", &secret_ref, "redis/url")?;

    let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
    let mut resolved = config.clone();
    resolved.url = resolve_credential(&config.url, &resolver, "startup:redis-url").await?;
    validate_redis_url_literal("redis.url", &resolved.url)?;
    Ok(resolved)
}

/// What a read-only ops command is assembled from (UN-30).
///
/// [`AppContext::new`] is the production assembly and cannot be reused here: it
/// migrates the database on connect, writes the default sidebars, calls
/// `init_monorepo()` — which writes refs and objects — starts the notification
/// worker, and opens the vault through the bootstrap path that can initialize it
/// and rotate credentials. Every one of those happens before any command body
/// runs, so a command assembled that way has already changed the system it was
/// asked to describe.
///
/// This assembly reaches for the read-only counterparts instead: a connection
/// the server refuses writes on and that runs no migration (UN-30), a vault
/// opened read-only when one is needed at all (UN-31), and the minimal read
/// facade (`ReadOnlyStorage`). It carries the config's provenance (UN-34) so a
/// report can say which configuration it describes.
///
/// Proving that the whole assembly writes nothing — vault and filesystem
/// included — belongs to UN-43; what is proven here is the database, refs and
/// objects.
pub struct ReadOnlyContext {
    pub config: Arc<crate::config::Config>,
    /// Where that config came from, when the caller resolved it through the
    /// loader. `None` means nobody claimed a provenance, not that there is none.
    pub config_summary: Option<crate::commands::LoadedConfigSummary>,
    pub storage: crate::jupiter::storage::ReadOnlyStorage,
    /// Present only when the object-storage config actually needed it.
    ///
    /// Opening a vault that nothing asks for would make an audit fail on a
    /// deployment that has no vault, for no reading of anything.
    pub vault: Option<crate::contract::vault::integration::vault_core::VaultCore>,
}

impl ReadOnlyContext {
    pub async fn open(
        config: crate::config::Config,
        config_summary: Option<crate::commands::LoadedConfigSummary>,
    ) -> Result<Self, MegaError> {
        Self::open_with_vault_key_path(
            config,
            config_summary,
            crate::contract::vault::integration::vault_core::VaultCore::default_key_path(),
        )
        .await
    }

    /// [`Self::open`] with the vault key file named explicitly.
    ///
    /// Production always uses the default path; naming it is what lets the
    /// zero-side-effect proof point at a vault it created itself instead of the
    /// developer's real one.
    pub(crate) async fn open_with_vault_key_path(
        config: crate::config::Config,
        config_summary: Option<crate::commands::LoadedConfigSummary>,
        vault_key_path: std::path::PathBuf,
    ) -> Result<Self, MegaError> {
        let config = Arc::new(config);

        let db_connection = Arc::new(
            crate::jupiter::storage::init::read_only_database_connection(&config.database).await?,
        );

        // Only open the vault if resolving the object-storage credentials needs
        // it. When it is needed, a failure to open read-only is fatal: the
        // alternative would be reading through credentials this process could
        // not verify, or falling back to the bootstrap path that writes.
        let vault = if object_storage_needs_vault(&config.object_storage) {
            Some(
                crate::contract::vault::integration::vault_core::VaultCore::open_readonly(
                    crate::jupiter::storage::vault_storage::VaultStorage {
                        base: <crate::jupiter::storage::base_storage::BaseStorage as
                            crate::jupiter::storage::base_storage::StorageConnector>::new(
                            db_connection.clone(),
                        ),
                    },
                    vault_key_path,
                )
                .await?,
            )
        } else {
            None
        };

        let object_storage_config = match vault.as_ref() {
            Some(vault) => resolve_object_storage_secrets(&config.object_storage, vault).await?,
            None => config.object_storage.clone(),
        };
        let object_store =
            crate::jupiter::storage::object_storage::build_object_storage(&object_storage_config)
                .await?;

        Ok(Self {
            storage: crate::jupiter::storage::ReadOnlyStorage::new(
                config.clone(),
                db_connection,
                object_store,
            ),
            config,
            config_summary,
            vault,
        })
    }
}

#[cfg(test)]
mod un30_readonly;

#[cfg(test)]
mod un43_readonly_assembly;

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
            storage_type: crate::orbit_api::factory::ObjectStorageBackend::S3,
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
            storage_type: crate::orbit_api::factory::ObjectStorageBackend::S3,
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
            storage_type: crate::orbit_api::factory::ObjectStorageBackend::S3,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_redis_url_secret_passes_through_literal_url() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let config = crate::config::RedisConfig {
            url: "redis://127.0.0.1:6379".to_string(),
        };
        let resolved = resolve_redis_url_secret(&config, &vault)
            .await
            .expect("literal redis url resolves");
        assert_eq!(resolved.url, "redis://127.0.0.1:6379");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_redis_url_secret_resolves_vault_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut data = Map::new();
        data.insert(
            "value".to_string(),
            Value::String("redis://vault-backed:6379".to_string()),
        );
        vault
            .write_secret("config/test/redis/url", Some(data))
            .await
            .expect("write redis url secret");

        let config = crate::config::RedisConfig {
            url: "vault://secret/config/test/redis/url#value".to_string(),
        };
        let resolved = resolve_redis_url_secret(&config, &vault)
            .await
            .expect("redis url secret ref resolves");
        assert_eq!(resolved.url, "redis://vault-backed:6379");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_redis_url_secret_rejects_wrong_namespace() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut data = Map::new();
        data.insert(
            "value".to_string(),
            Value::String("redis://wrong-namespace:6379".to_string()),
        );
        vault
            .write_secret("config/test/mail/password", Some(data))
            .await
            .expect("write unrelated secret");

        let config = crate::config::RedisConfig {
            url: "vault://secret/config/test/mail/password#value".to_string(),
        };
        let result = resolve_redis_url_secret(&config, &vault).await;
        assert!(
            result.is_err(),
            "redis.url secret ref outside redis/url namespace must fail"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_redis_url_secret_rejects_malformed_resolved_url() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut data = Map::new();
        data.insert(
            "value".to_string(),
            Value::String("http://not-a-redis-url:6379".to_string()),
        );
        vault
            .write_secret("config/test/redis/url", Some(data))
            .await
            .expect("write redis url secret");

        let config = crate::config::RedisConfig {
            url: "vault://secret/config/test/redis/url#value".to_string(),
        };
        let result = resolve_redis_url_secret(&config, &vault).await;
        assert!(
            result.is_err(),
            "resolved redis.url with non-redis scheme must fail"
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("redis.url scheme"));
        assert!(!message.contains("http://not-a-redis-url:6379"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_redis_url_secret_redacted_error_does_not_leak_secret_like_scheme() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let storage = test_storage(temp_dir.path()).await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let mut data = Map::new();
        data.insert(
            "value".to_string(),
            Value::String("mysecret://sensitive-host:6379".to_string()),
        );
        vault
            .write_secret("config/test/redis/url", Some(data))
            .await
            .expect("write redis url secret");

        let config = crate::config::RedisConfig {
            url: "vault://secret/config/test/redis/url#value".to_string(),
        };
        let result = resolve_redis_url_secret(&config, &vault).await;
        assert!(
            result.is_err(),
            "resolved redis.url with secret-like scheme must fail"
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("redis.url scheme"));
        assert!(!message.contains("mysecret"));
        assert!(!message.contains("sensitive-host"));
    }

    #[test]
    fn worker_fence_requires_a_second_database_connection() {
        let config = crate::config::DbConfig {
            max_connection: 1,
            ..Default::default()
        };

        let error = validate_worker_fence_database_config(&config)
            .expect_err("a worker fence must not starve a one-connection pool");
        assert!(error.to_string().contains("at least 2"));
    }
}
