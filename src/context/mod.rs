use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    config::{
        ObjectStorageConfig,
        reload::ConfigHandle,
        secret::{
            SecretRef, SecretResolver, SecretString, VaultSecretResolver, is_secret_ref_value,
        },
        validate::{
            parse_secret_ref_for_field, validate_config_secret_ref, validate_redis_url_literal,
        },
    },
    contract::{
        policy::entitystore::SharedEntityStore,
        vault::integration::vault_core::{VaultCore, with_audit_caller},
    },
    jupiter::{
        redis::{ConnectionManager, init_connection},
        service::{
            git_service::GitService,
            mono_service::MonoService,
            storage_event_emitter::StorageEventEmitter,
            storage_event_transport::{EventTarget, HttpsEventTransport},
        },
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            mono_storage::MonoStorage,
            vault_storage::VaultStorage,
        },
    },
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

    /// Service-level shutdown token (WH-13). The `service` command forwards the
    /// first Ctrl+C here, so a signal arriving before a server registers its own
    /// signal handler is still observed (the token is sticky) and every exit
    /// passes through the cleanup tail.
    pub service_shutdown: CancellationToken,

    /// Shared authorization snapshot holder (ADR-UN-02). Unique owner created in
    /// `new()`; the same `Arc` is injected into `Storage` and the HTTP state so
    /// the write path (notify) and read path (guard/push) share one instance.
    pub entity_store: Arc<SharedEntityStore>,
}

/// Initializes a Monorepo without bringing up normal service dependencies.
///
/// Vault is opened read-only only when object-storage credentials are SecretRefs;
/// Redis, notification workers, and Git listeners are deliberately
/// outside this one-shot path.
pub(crate) async fn bootstrap_monorepo(config: crate::config::Config) -> Result<(), MegaError> {
    config.validate()?;
    config.monorepo.object_hash_kind()?;
    let config = Arc::new(config);
    let db_connection =
        Arc::new(crate::jupiter::storage::init::database_connection(&config.database).await?);

    let object_storage_config = if object_storage_needs_vault(&config.object_storage) {
        let vault_audit = config
            .vault
            .as_ref()
            .map(|vault| vault.audit.clone())
            .unwrap_or_default();
        let vault = VaultCore::open_readonly(
            VaultStorage {
                base: BaseStorage::new(db_connection.clone()),
            },
            VaultCore::default_key_path(),
        )
        .await?
        .with_audit_config(vault_audit);
        resolve_object_storage_secrets(&config.object_storage, &vault).await?
    } else {
        config.object_storage.clone()
    };
    let object_store =
        crate::jupiter::storage::object_storage::build_object_storage(&object_storage_config)
            .await?;
    let mono_service = MonoService {
        mono_storage: MonoStorage {
            base: BaseStorage::new(db_connection),
        },
        git_service: GitService {
            obj_storage: object_store,
        },
    };

    mono_service.bootstrap_monorepo(&config.monorepo).await
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
        // TP-15 ①④⑤ (and the rest of Config::validate) must fail-close here:
        // `config validate` is a CLI path; service http/ssh/debug go through
        // AppContext without that command.
        config.validate()?;
        config.monorepo.ensure_normal_service_object_format()?;
        Self::new_with_monorepo_initialization(config).await
    }

    async fn new_with_monorepo_initialization(
        config: crate::config::Config,
    ) -> Result<Self, MegaError> {
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

        // Create the shared authorization snapshot holder (ADR-UN-02) and inject
        // the same `Arc` into `Storage` (write-path notify) and the HTTP state
        // (read-path guard/push). First-build `ensure` happens before the HTTP
        // listener binds (UN-02).
        let entity_store = Arc::new(SharedEntityStore::new());
        let mut storage = storage;
        storage.set_entity_store(entity_store.clone());
        // WH-11: with the vault ready and storage constructed, resolve the
        // static targets' HMAC secrets and bind the real HTTPS transport as
        // the single application emitter owner. Disabled configs return
        // without resolving anything (AC1); any failure is a startup error
        // (AC3) before an owner exists, so nothing needs draining here.
        bind_storage_event_emitter(&config, &mut storage, &vault).await?;
        // MC-09: the server-signing vault handle reaches the synthetic-commit
        // sites through storage; vault is built before storage above.
        let storage = storage.with_vault(vault.clone());

        // WH-11: every step after the emitter owner installation runs in one
        // tail block; if any of them fails (redis, notification channels,
        // bootstrap), the emitter is shut down before the error is returned.
        // Shutdown is idempotent — the WH-13 service tail calls it again on
        // normal exits.
        let post_owner_tail = async {
            // Resolve any `vault://` SecretRef in `redis.url` post-vault, then build
            // the shared Redis connection from the resolved config
            // (docs/refactoring/integration.md: redis.url SecretRef support).
            let redis_config = resolve_redis_url_secret(&config.redis, &vault).await?;
            let connection = init_connection(&redis_config).await?;

            // Build notification channels after Vault so optional webhook
            // credentials can be resolved. In-app delivery does not require `[mail]`.
            let notification_shutdown = CancellationToken::new();
            let notif_stg = storage.notification_storage();
            let mut extra_channels: Vec<
                Arc<dyn crate::notification::channels::NotificationChannel>,
            > = Vec::new();
            let mut website_mail = None;
            let notification_enabled = match config.notification.as_ref() {
                Some(notification_cfg) => {
                    crate::config::validate::validate_notification_config(notification_cfg)?;
                    let resolver =
                        VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
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
                    notification_cfg.enabled
                }
                None => true,
            };
            let service = Arc::new(crate::notification::NotificationService::new(
                notif_stg,
                extra_channels,
                website_mail,
                Some(config_handle.clone()),
                notification_enabled,
            ));
            crate::notification::NotificationService::set_active(Some(Arc::clone(&service)));
            let sd = notification_shutdown.clone();
            tokio::spawn(async move {
                service.start(sd).await;
            });

            storage.mono_service.init_monorepo(&config.monorepo).await?;
            storage.push_queue_service.reaper().spawn_background(
                crate::jupiter::service::push_queue_reaper::DEFAULT_REAP_INTERVAL,
                notification_shutdown.clone(),
            );
            storage
                .push_queue_service
                .blob_path_compensator()
                .spawn_background(
                    crate::jupiter::storage::blob_path_index::DEFAULT_COMPENSATE_INTERVAL,
                    notification_shutdown.clone(),
                );
            // Inspect/reconcile tombstone live refs. CL merge always lands via
            // MonoWriteQueue (MW-01), so the audit is safe under Review as well.
            storage.push_queue_service.audit().spawn_background(
                crate::jupiter::service::mono_write_audit::DEFAULT_INSPECT_INTERVAL,
                notification_shutdown.clone(),
            );

            Ok::<(ConnectionManager, CancellationToken), MegaError>((
                connection,
                notification_shutdown,
            ))
        };
        let (connection, notification_shutdown) = match post_owner_tail.await {
            Ok(parts) => parts,
            Err(error) => {
                storage.storage_event_emitter.shutdown().await;
                // The completion is recorded only after the drain, matching
                // the WH-13 service-tail receipt ordering; the line carries
                // category fields only — no URIs, secrets or bodies.
                tracing::info!(
                    category = "lifecycle",
                    "storage_events_startup_failure_drained"
                );
                return Err(error);
            }
        };

        Ok(Self {
            storage,
            vault,
            config,
            config_handle,
            connection,
            notification_shutdown,
            service_shutdown: CancellationToken::new(),
            entity_store,
        })
    }

    pub async fn shutdown_storage_events(&self) {
        self.storage.storage_event_emitter.shutdown().await;
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

/// WH-11 startup binding (plan-20260912): when `[storage_events]` is enabled,
/// resolve every static target's `secret_ref` through the vault and install the
/// real HTTPS transport as the single application emitter owner on `Storage`.
///
/// - Disabled configs return immediately and never resolve a ref (AC1).
/// - Only the `vault://secret/config/<profile>/storage_events/targets/<id>/hmac#<field>`
///   namespace is accepted (AC2), the same check `config validate` applies.
/// - Any parse, namespace, resolution or `hex:<even-hex>` compile failure is a
///   startup error (AC3); the resolved value is never written back into the
///   config snapshot (AC5), and neither the SecretRef URI nor the secret value
///   appears in errors (AC6/AC7 — resolver errors are already redacted).
async fn bind_storage_event_emitter(
    config: &crate::config::Config,
    storage: &mut crate::jupiter::storage::Storage,
    vault: &VaultCore,
) -> Result<(), MegaError> {
    if !config.storage_events.enabled {
        return Ok(());
    }
    let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(300));
    let mut compiled_targets = Vec::with_capacity(config.storage_events.targets.len());
    for (index, target) in config.storage_events.targets.iter().enumerate() {
        let secret_field = format!("storage_events.targets[{index}].secret_ref");
        let secret_ref = parse_secret_ref_for_field(&secret_field, &target.secret_ref)?;
        validate_config_secret_ref(
            &secret_field,
            &secret_ref,
            &format!("storage_events/targets/{}/hmac", target.id),
        )?;
        let value =
            with_audit_caller("startup:storage-events", resolver.resolve(&secret_ref)).await?;
        let compiled = EventTarget::compile(&target.id, &target.url, &SecretString::new(value))?;
        compiled_targets.push((target.clone(), compiled));
    }
    let transport = HttpsEventTransport::new(
        Duration::from_secs(config.storage_events.connect_timeout_seconds),
        Duration::from_secs(config.storage_events.request_timeout_seconds),
    )?;
    storage.set_storage_event_emitter(StorageEventEmitter::new_with_transport(
        config,
        Arc::new(transport),
        compiled_targets,
    ));
    Ok(())
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
/// migrates the database on connect, calls `init_monorepo()` — which writes refs
/// and objects — starts the notification worker, and opens the vault through the
/// bootstrap path that can initialize it and rotate credentials. Every one of
/// those happens before any command body runs, so a command assembled that way
/// has already changed the system it was asked to describe.
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
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::StorageEventsTargetConfig,
        contract::vault::integration::vault_core::VaultCoreInterface,
        jupiter::{
            service::{
                storage_event::{CommittedEvent, EventData, EventScope, EventSource, EventType},
                storage_event_emitter::AdmissionDisposition,
            },
            tests::test_storage,
        },
    };

    #[test]
    fn is_secret_ref_value_detects_vault_uri() {
        assert!(is_secret_ref_value(
            "vault://secret/config/prod/object_storage/access_key_id#value"
        ));
        assert!(!is_secret_ref_value("AKIAEXAMPLE"));
        assert!(!is_secret_ref_value(""));
    }

    #[tokio::test]
    async fn app_context_new_runs_config_validate_before_startup() {
        let mut config = crate::config::Config::mock();
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        match AppContext::new(config).await {
            Err(error) => assert!(
                error.to_string().contains("push_auth"),
                "trunk without push_auth must fail Config::validate: {error}"
            ),
            Ok(_) => panic!("trunk without push_auth must fail before AppContext startup"),
        }
    }

    #[tokio::test]
    async fn normal_context_accepts_sha256_object_format_preflight() {
        let mut config = crate::config::Config::mock();
        config.monorepo.object_format = crate::config::MonoObjectFormat::Sha256;
        config
            .monorepo
            .ensure_normal_service_object_format()
            .expect("sha256 normal service is enabled for Libra/git-internal peers");
    }

    #[tokio::test]
    async fn normal_context_accepts_blake3_object_format_preflight() {
        let mut config = crate::config::Config::mock();
        config.monorepo.object_format = crate::config::MonoObjectFormat::Blake3;
        config
            .monorepo
            .ensure_normal_service_object_format()
            .expect("blake3 normal service is enabled for Libra/git-internal peers");
    }

    #[tokio::test]
    async fn bootstrap_rejects_invalid_monorepo_before_database_connection() {
        let mut config = crate::config::Config::mock();
        config.database.db_url = "postgres://127.0.0.1:1/mega2".to_string();
        config.monorepo.root_dirs.clear();

        let result = tokio::time::timeout(Duration::from_secs(1), bootstrap_monorepo(config))
            .await
            .expect("invalid configuration must fail before opening the database");
        let error = result.expect_err("empty root_dirs must fail validation");
        assert!(error.to_string().contains("monorepo.root_dirs"));
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

    fn storage_events_target(id: &str, secret_ref: &str) -> StorageEventsTargetConfig {
        StorageEventsTargetConfig {
            id: id.to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: secret_ref.to_string(),
            events: vec!["repo.push".to_string()],
            git_paths: vec!["/team/a".to_string()],
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        }
    }

    /// A well-formed `repo.push` event whose path no configured target
    /// subscribes to: it projects, then fails target selection.
    fn unmatched_push_event() -> CommittedEvent {
        CommittedEvent {
            event_id: Uuid::parse_str("33333333-3333-4333-8333-333333333333").expect("uuid"),
            event_type: EventType::RepoPush,
            occurred_at: 1,
            source: EventSource::Git,
            scope: EventScope::Git {
                repo_path: "/other/repo".to_string(),
            },
            data: EventData::RepoPush {
                push_id: "p1".to_string(),
                operation_id: "op1".to_string(),
                ref_name: "refs/heads/main".to_string(),
                old_oid: "00".to_string(),
                requested_oid: "11".to_string(),
                landed_oid: "22".to_string(),
            },
        }
    }

    const WH11_SECRET_NAME: &str = "config/test/storage_events/targets/ops-main/hmac";
    const WH11_TARGET_URI: &str =
        "vault://secret/config/test/storage_events/targets/ops-main/hmac#value";
    /// Distinctive seeded sentinel: the value really flows through the vault
    /// resolver into the compiled target, so its absence from diagnostic
    /// surfaces is a meaningful check. Asserted in both the full
    /// `hex:<payload>` form and the bare payload form.
    const WH11_SENTINEL_PAYLOAD: &str =
        "abababababababababababababababababababababababababababababababab";

    fn wh11_sentinel_full() -> String {
        format!("hex:{WH11_SENTINEL_PAYLOAD}")
    }

    /// WH-11 AC1..AC7: startup secret binding against a real vault.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn storage_events_secret_startup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");

        // The storage's own config carries the enabled target, so the runtime
        // snapshot exposed by `Storage::config` is the diagnostic surface AC5
        // refers to.
        let mut bound_config =
            crate::config::testing::isolated_config(temp_dir.path().join("config"));
        bound_config.storage_events.enabled = true;
        bound_config.storage_events.installation_id = Some("it-wh11".to_string());
        bound_config.storage_events.targets =
            vec![storage_events_target("ops-main", WH11_TARGET_URI)];
        let mut storage =
            crate::jupiter::tests::test_storage_with_config(temp_dir.path(), bound_config.clone())
                .await;
        let vault = VaultCore::config(
            storage.vault_storage(),
            temp_dir.path().join("core_key.json"),
        )
        .await
        .expect("vault init");

        let event = unmatched_push_event();

        // (a) AC1: disabled + dangling secret_ref => binding is skipped, the
        // emitter stays disabled and nothing is resolved (the secret is never
        // seeded, so any resolution attempt would have failed).
        let mut disabled_config = bound_config.clone();
        disabled_config.storage_events.enabled = false;
        bind_storage_event_emitter(&disabled_config, &mut storage, &vault)
            .await
            .expect("disabled binding never resolves");
        assert_eq!(
            storage.storage_event_emitter.try_emit(event.clone()),
            AdmissionDisposition::DroppedDisabled
        );

        // (b) AC2/AC7: enabled + wrong namespace => startup error, and the
        // diagnostic never carries the SecretRef URI.
        let wrong_ns_uri = "vault://secret/config/test/storage_events/targets/other/hmac#value";
        let mut wrong_ns_config = bound_config.clone();
        wrong_ns_config.storage_events.targets =
            vec![storage_events_target("ops-main", wrong_ns_uri)];
        let error = bind_storage_event_emitter(&wrong_ns_config, &mut storage, &vault)
            .await
            .expect_err("wrong namespace must fail startup binding");
        let message = error.to_string();
        assert!(
            message.contains("storage_events.targets[0].secret_ref"),
            "{message}"
        );
        assert!(message.contains("value is redacted"), "{message}");
        assert!(!message.contains(wrong_ns_uri), "{message}");
        assert!(!message.contains("targets/other"), "{message}");
        assert!(
            matches!(
                storage.storage_event_emitter.try_emit(event.clone()),
                AdmissionDisposition::DroppedDisabled
            ),
            "a failed binding must leave the disabled emitter in place"
        );

        // (c) AC3/AC7: enabled + missing secret => startup error, redacted.
        let error = bind_storage_event_emitter(&bound_config, &mut storage, &vault)
            .await
            .expect_err("missing secret must fail startup binding");
        let message = error.to_string();
        assert!(message.contains("secret not found"), "{message}");
        assert!(message.contains("vault://secret/***#***"), "{message}");
        assert!(!message.contains(WH11_TARGET_URI), "{message}");
        assert!(!message.contains("config/test/storage_events"), "{message}");

        // (d) AC4/AC5: enabled + valid secret => the single application owner
        // is installed and admits events (a filter miss proves enabled; no
        // event is emitted, so no network happens).
        let mut data = Map::new();
        data.insert("value".to_string(), Value::String(wh11_sentinel_full()));
        vault
            .write_secret(WH11_SECRET_NAME, Some(data))
            .await
            .expect("seed target hmac secret");
        bind_storage_event_emitter(&bound_config, &mut storage, &vault)
            .await
            .expect("valid secret binds the real transport");
        assert_eq!(
            storage.storage_event_emitter.try_emit(event),
            AdmissionDisposition::DroppedFilter
        );

        // AC5: the runtime config snapshot exposed by `Storage` still shows
        // the vault:// ref string and never the seeded material, in either
        // form. The compiled `EventTarget` is not reachable from outside —
        // `StorageEventEmitter` has no Debug and its targets are private — so
        // the snapshot is the diagnostic surface covered here; the target's
        // own redacting Debug is pinned by the storage_event_transport tests.
        let snapshot_debug = format!("{:?}", storage.config());
        assert!(snapshot_debug.contains(WH11_TARGET_URI), "{snapshot_debug}");
        assert!(
            !snapshot_debug.contains(&wh11_sentinel_full()),
            "config snapshot must not contain the resolved secret value"
        );
        assert!(
            !snapshot_debug.contains(WH11_SENTINEL_PAYLOAD),
            "config snapshot must not contain the decoded hex payload"
        );
        storage.storage_event_emitter.shutdown().await;
    }

    /// WH-11: a failure after the emitter owner was installed (here: an
    /// unreachable Redis in the post-owner tail) still surfaces as an
    /// `AppContext` startup error. The drain receipt on that path is covered
    /// process-level by `integration_storage_events_runtime::secret_binding`.
    #[test]
    fn storage_events_startup_failure_after_owner_install_errors() {
        // The env lock is a std mutex guard; holding it across `.await` would
        // both trip clippy::await_holding_lock and risk blocking sibling
        // tests, so this env-serialized test runs its async body on a
        // dedicated current-thread runtime while holding the lock in sync
        // code (same pattern as the process-level targets' block_on SQL).
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let db_config = runtime.block_on(crate::jupiter::tests::test_db_config(temp_dir.path()));

        // AppContext derives the vault key path from MEGA_BASE_DIR; point it
        // at the temp dir so this test's vault stays isolated (GC-09).
        let lock = crate::config::testing::env_lock();
        let base_dir = temp_dir.path().join("base").to_string_lossy().into_owned();
        let _base_guard =
            crate::config::testing::EnvVarGuard::set(&lock, "MEGA_BASE_DIR", &base_dir);

        let vault = runtime
            .block_on(VaultCore::from_database_config(
                &db_config,
                crate::config::mega_base()
                    .join("vault")
                    .join("core_key.json"),
            ))
            .expect("vault bootstrap");
        let mut data = Map::new();
        data.insert("value".to_string(), Value::String(wh11_sentinel_full()));
        runtime
            .block_on(vault.write_secret(WH11_SECRET_NAME, Some(data)))
            .expect("seed target hmac secret");

        let mut config = crate::config::testing::isolated_config(temp_dir.path().join("config"));
        config.database = db_config;
        // The storage-only morphology that [storage_events] enabled requires.
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.cedar.enforcement = "off".to_string();
        // Fail the post-owner tail: connection to a closed port is refused.
        // The port is per-run (bind, then drop the listener) so the redis
        // init-connection call counter, keyed by URL, is never perturbed for
        // other tests (un43 pins `redis://127.0.0.1:1`).
        let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        config.redis.url = format!("redis://127.0.0.1:{dead_port}");
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("it-wh11".to_string());
        config.storage_events.targets = vec![storage_events_target("ops-main", WH11_TARGET_URI)];

        let result = runtime
            .block_on(async {
                // The timeout future must be constructed inside the runtime.
                tokio::time::timeout(Duration::from_secs(60), AppContext::new(config)).await
            })
            .expect("AppContext startup must not hang on dead redis");
        let error = match result {
            Ok(_) => panic!("unreachable redis must fail AppContext startup"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("Redis"), "{message}");
        assert!(
            !message.contains(WH11_SENTINEL_PAYLOAD),
            "startup error must not contain secret material: {message}"
        );
        assert!(
            !message.contains(WH11_TARGET_URI),
            "startup error must not contain the SecretRef URI: {message}"
        );
    }
}
