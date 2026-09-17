use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use async_trait::async_trait;
use libvault::{
    RustyVault,
    config::{Config, MountEntryHMACLevel},
    core::{Core, CoreState},
    errors::RvError,
    handler::{AuthHandler, Handler},
    logical::{Auth, Backend as LogicalBackend, Response, SecretData},
    modules::{
        auth::{AuthModule, ExpirationManager, TokenStore},
        policy::{PolicyModule, PolicyStore},
    },
    mount::{MountTable, SYSTEM_BARRIER_PREFIX},
    shamir::ShamirSecret,
    storage::{Backend, Storage as BarrierStorage, barrier_view::BarrierView},
    utils::deserialize_system_time,
};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    common::errors::{MegaError, VaultError, VaultResult},
    config::{DbConfig, VaultAuditConfig, mega_base},
    contract::vault::integration::{
        jupiter_backend::JupiterBackend, readonly_backend::ReadonlyBackend,
    },
    jupiter::storage::{
        Storage,
        base_storage::{BaseStorage, StorageConnector},
        init::database_connection,
        vault_storage::VaultStorage,
    },
};

const CORE_KEY_FILE: &str = "core_key.json";
const SECRET_MOUNT: &str = "secret";

tokio::task_local! {
    /// Optional caller identity for `vault_audit` records (vault.md stage H "who").
    /// Entry points wrap secret operations in [`with_audit_caller`]; unset falls
    /// back to `"unknown"`.
    static AUDIT_CALLER: Option<String>;
}

/// Run `future` with `caller` attributed to every `vault_audit` record emitted
/// by secret operations on the current task. Non-invasive alternative to
/// threading a caller parameter through every `VaultCoreInterface` call site.
pub async fn with_audit_caller<F, R>(caller: &str, future: F) -> R
where
    F: std::future::Future<Output = R>,
{
    AUDIT_CALLER.scope(Some(caller.to_string()), future).await
}

fn current_audit_caller() -> String {
    AUDIT_CALLER
        .try_with(|c| c.clone())
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoreKey {
    secret_shares: Vec<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_token: Option<String>,
    #[serde(default)]
    runtime_tokens: RuntimeTokens,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RuntimeTokens {
    #[serde(default)]
    ssh: String,
    #[serde(default)]
    pgp: String,
    #[serde(default)]
    nostr: String,
    #[serde(default)]
    pki: String,
    #[serde(default)]
    config: String,
    #[serde(default)]
    generic: String,
}

impl RuntimeTokens {
    fn is_complete(&self) -> bool {
        !self.ssh.is_empty()
            && !self.pgp.is_empty()
            && !self.nostr.is_empty()
            && !self.pki.is_empty()
            && !self.config.is_empty()
            && !self.generic.is_empty()
    }

    fn token_for_secret(&self, name: SecretName<'_>) -> &str {
        match name.as_str() {
            "ssh_server_key" => &self.ssh,
            "pgp-signed-secret" => &self.pgp,
            "nostr_identity_key" => &self.nostr,
            name if name.starts_with("config/") => &self.config,
            _ => &self.generic,
        }
    }

    fn token_for_api_path(&self, path: &str) -> &str {
        if path.starts_with("pki/") || path.starts_with("sys/mounts/pki") {
            &self.pki
        } else {
            &self.generic
        }
    }
}

// ---------------------------------------------------------------------------
// Readonly bootstrap (UN-31)
// ---------------------------------------------------------------------------
//
// `RustyVault::unseal` routes through the crate-private `Core::post_unseal`,
// which — before the caller can read a single secret — plants the default mount
// table when it is absent, rewrites entries left in an older format, repairs the
// auth mount, writes the built-in ACL policies, mints a token salt, and starts a
// thread that revokes expired leases and deletes their records.
//
// None of that can be turned off from outside, so this does not try to: it
// drives the barrier and its own readonly counterpart of `post_unseal`
// directly, and simply never asks for the repairs. The library needs no fork
// for this — `Core`'s fields, the barrier trait, `MountTable::load`,
// `ShamirSecret::combine` and the module handles are all public, and only
// `AuthModule` and `PolicyModule` override `Module::init` at all, so the
// readonly variant of "init every module" is those two rebuilt and nothing
// else. The full public-API inventory, the upstream anchors it rests on, and
// the known fragile points are in `docs/refactoring/vault.md` ("VLT-S1").

/// Where the token salt lives, relative to the system barrier view.
///
/// Upstream keeps these as private constants in `modules/auth/token_store.rs`,
/// so they are mirrored rather than imported. The pairing is load-bearing: the
/// salt has to be checked *before* `TokenStore::new` runs, because that is what
/// mints one when it is missing. `un31_a_missing_token_salt_fails_closed` keeps
/// the mirror honest — it deletes the salt at this exact path and requires the
/// writable control to mint it back, so a path change upstream turns the
/// control red rather than passing silently.
const TOKEN_SUB_PATH: &str = "token/";
const TOKEN_SALT_LOCATION: &str = "salt";

fn readonly_open(error: RvError) -> VaultError {
    VaultError::ReadonlyOpen(error.to_string())
}

/// The serde shape of the library's own lease entry.
///
/// Used only to answer "would restoring this record have had to rewrite it?".
/// The library's restore path converts an older-format entry and writes the
/// converted one back; under a readonly open that write would be caught by the
/// backstop and the caller would be told "write denied" rather than what is
/// actually wrong. `LeaseEntry` is private upstream, so this mirrors its fields
/// — `data` being a required object is the discriminator, and the timestamps go
/// through the library's own deserializer so the two cannot drift apart in how
/// they read a time.
#[derive(Deserialize)]
#[allow(dead_code)]
struct StoredLease {
    #[serde(default)]
    lease_id: String,
    client_token: String,
    path: String,
    data: Map<String, Value>,
    secret: Option<SecretData>,
    auth: Option<Auth>,
    #[serde(deserialize_with = "deserialize_system_time")]
    issue_time: SystemTime,
    #[serde(deserialize_with = "deserialize_system_time")]
    expire_time: SystemTime,
    #[serde(default)]
    revoke_err: String,
}

/// Build a sealed vault over `backend` that can never start the mounts monitor.
///
/// The monitor reloads mount tables on a timer and can re-mount while an audit
/// is mid-read, so whether it exists is a decision of the mode rather than of
/// the caller's configuration — the interval is forced to zero here even when
/// the caller asked for one.
pub(crate) fn readonly_vault(
    backend: Arc<dyn Backend>,
    config: Option<&Config>,
) -> VaultResult<RustyVault> {
    let mut config = config.cloned().unwrap_or_default();
    config.mounts_monitor_interval = 0;
    RustyVault::new(backend, Some(&config)).map_err(|e| VaultError::RustyVaultCreate(e.to_string()))
}

/// Unseal `rvault` and bring it up read-only.
///
/// `keys` must hold at least the configured threshold of unseal shares; the
/// first `threshold` of them are combined here rather than fed one at a time to
/// `Core::unseal`, which is the call that would run the repairing post-unseal.
pub(crate) async fn readonly_unseal(rvault: &RustyVault, keys: &[&[u8]]) -> VaultResult<()> {
    let core = rvault.core.load_full();

    if !core
        .barrier
        .inited()
        .await
        .map_err(|e| VaultError::InitializationState(e.to_string()))?
    {
        return Err(VaultError::ReadonlyNotInitialized);
    }

    let seal_config = core.seal_config().await.map_err(readonly_open)?;
    let threshold = seal_config.secret_threshold as usize;
    if keys.len() < threshold {
        return Err(VaultError::CoreKeyTooFewShares {
            expected: threshold,
            actual: keys.len(),
        });
    }
    let shares: Vec<Vec<u8>> = keys
        .iter()
        .take(threshold)
        .map(|key| key.to_vec())
        .collect();

    // Retired shares are refused here exactly as the ordinary unseal refuses
    // them. Reading through a share someone rotated away is not a readonly
    // concession worth making.
    if let Ok(deprecated) = core.deprecated_unseal_keys_set().await
        && shares.iter().any(|share| deprecated.contains(share))
    {
        return Err(VaultError::Unseal(
            "an unseal key share has been deprecated".to_string(),
        ));
    }

    let kek = if threshold <= 1 {
        shares
            .first()
            .cloned()
            .ok_or_else(|| VaultError::Unseal("no unseal key share was supplied".to_string()))?
    } else {
        ShamirSecret::combine(shares).ok_or_else(|| {
            VaultError::Unseal("not enough valid key shares to unseal vault".to_string())
        })?
    };

    core.barrier
        .unseal(&kek)
        .await
        .map_err(|e| VaultError::Unseal(e.to_string()))?;

    // The state the ordinary unseal installs, minus the key material a readonly
    // handle has no use for: `kek` and the accumulated shares are private and
    // feed only `generate_unseal_keys`, which is a write path.
    let mut state = CoreState::default();
    state.hmac_key = core.barrier.derive_hmac_key().map_err(readonly_open)?;
    state.system_view = Some(Arc::new(BarrierView::new(
        core.barrier.clone(),
        SYSTEM_BARRIER_PREFIX,
    )));
    state.sealed = false;
    core.state.store(Arc::new(state));

    readonly_post_unseal(&core).await
}

/// Open an already-initialized vault over `backend` without changing it.
///
/// The library-level half of [`VaultCore::open_readonly`], kept separate so the
/// UN-31 suite can drive it over an in-memory backend, where a repair that did
/// happen is visible as a changed byte.
pub(crate) async fn open_readonly_core(
    backend: Arc<dyn Backend>,
    config: Option<&Config>,
    keys: &[&[u8]],
) -> VaultResult<RustyVault> {
    let rvault = readonly_vault(backend, config)?;
    readonly_unseal(&rvault, keys).await?;
    Ok(rvault)
}

/// The readonly counterpart of the crate-private `Core::post_unseal`.
async fn readonly_post_unseal(core: &Arc<Core>) -> VaultResult<()> {
    // Registration only: every module's `setup` adds backends and handlers and
    // writes nothing.
    core.module_manager.setup(core).map_err(readonly_open)?;

    let hmac_key = core.state.load().hmac_key.clone();

    load_mount_table_readonly(
        &core.mounts_router.mounts,
        core.barrier.as_storage(),
        Some(&hmac_key),
        core.mount_entry_hmac_level,
        "core mount table",
    )
    .await?;

    core.mounts_router
        .setup(core.clone())
        .map_err(readonly_open)?;

    // Auth first, and not by preference: `Core::add_auth_handler` reaches into
    // the auth module and unwraps its token store, so the policy module's
    // handler registration below would panic if this had not run yet.
    let auth = core.module_manager.get_module::<AuthModule>("auth").ok_or(
        VaultError::ReadonlyStateIncomplete {
            detail: "auth module",
        },
    )?;
    readonly_init_auth(&auth, core).await?;

    let policy = core
        .module_manager
        .get_module::<PolicyModule>("policy")
        .ok_or(VaultError::ReadonlyStateIncomplete {
            detail: "policy module",
        })?;
    // `PolicyModule::init` would go on to call `setup_policy`, which plants the
    // built-in ACL policies when they are absent and rewrites the immutable
    // ones when their text has drifted — this process editing the very policy
    // set it was opened to read.
    let policy_store = PolicyStore::new(core).await.map_err(readonly_open)?;
    policy.policy_store.store(policy_store.clone());
    core.add_auth_handler(policy_store as Arc<dyn AuthHandler>)
        .map_err(readonly_open)?;

    Ok(())
}

/// `AuthModule::init`, minus every repair and minus the checker thread.
// The auth-backend factory's signature is fixed by `LogicalBackendNewFunc`, so
// its `Result<_, RvError>` cannot be boxed here; `RvError` is a large enum and
// upstream allows this same lint crate-wide for that reason.
#[allow(clippy::result_large_err)]
async fn readonly_init_auth(auth: &Arc<AuthModule>, core: &Arc<Core>) -> VaultResult<()> {
    let hmac_key = core.state.load().hmac_key.clone();

    // An initialized vault already has a token salt. `TokenStore::new` mints one
    // when it is missing, so the check has to happen first: minting would both
    // write and silently change how every token in that vault hashes, and
    // letting the backstop catch it would report "write denied" instead of the
    // actual condition.
    let system_view =
        core.state
            .load()
            .system_view
            .clone()
            .ok_or(VaultError::ReadonlyStateIncomplete {
                detail: "system view",
            })?;
    let token_view = system_view.new_sub_view(TOKEN_SUB_PATH);
    let salt_present = token_view
        .get(TOKEN_SALT_LOCATION)
        .await
        .map_err(readonly_open)?
        .is_some_and(|entry| !entry.value.is_empty());
    if !salt_present {
        return Err(VaultError::ReadonlyStateIncomplete {
            detail: "token salt",
        });
    }

    let expiration = ExpirationManager::new(core).map_err(readonly_open)?.wrap();
    let token_store = TokenStore::new(core, expiration.clone())
        .await
        .map_err(readonly_open)?
        .wrap();
    expiration
        .set_token_store(&token_store)
        .map_err(readonly_open)?;

    // `auth.expiration` is deliberately left empty. `AuthModule::init` is the
    // only place that starts the expired-lease worker, and also the only place
    // that fills this slot, and nothing else in the library reads it — so an
    // empty slot is a direct, public statement that the worker never started,
    // rather than an inference from not having observed a revocation. That
    // inference would only ever be a race with the 200ms tick.
    auth.token_store.store(Some(token_store.clone()));

    let backend_token_store = token_store.clone();
    auth.add_auth_backend(
        "token",
        Arc::new(
            move |_core: Arc<Core>| -> Result<Arc<dyn LogicalBackend>, RvError> {
                let mut backend = backend_token_store.new_backend();
                backend.init()?;
                Ok(Arc::new(backend))
            },
        ),
    )
    .map_err(readonly_open)?;

    load_mount_table_readonly(
        &auth.mounts_router.mounts,
        auth.barrier.as_storage(),
        Some(&hmac_key),
        core.mount_entry_hmac_level,
        "auth mount table",
    )
    .await?;
    auth.setup_auth().map_err(readonly_open)?;

    readonly_scan_leases(&expiration).await?;

    core.add_handler(token_store as Arc<dyn Handler>)
        .map_err(readonly_open)?;

    Ok(())
}

/// Load a mount table, and refuse rather than repair it.
///
/// The two repairs the library's own loader performs — planting the default
/// mounts when the table is absent, and rewriting entries left in an older
/// format — are exactly what a readonly open must not do. Neither can be
/// silently skipped either: a vault whose mount table is missing or stale has
/// not been read correctly, and reporting on it as if it had been would be
/// worse than refusing.
async fn load_mount_table_readonly(
    mounts: &Arc<MountTable>,
    storage: &dyn BarrierStorage,
    hmac_key: Option<&[u8]>,
    hmac_level: MountEntryHMACLevel,
    detail: &'static str,
) -> VaultResult<()> {
    match mounts.load(storage, hmac_key, hmac_level).await {
        Ok(_) => {}
        Err(RvError::ErrConfigLoadFailed) => {
            return Err(VaultError::ReadonlyStateIncomplete { detail });
        }
        Err(err) => return Err(readonly_open(err)),
    }

    // The read-only counterpart of the scan the library's `mount_update` runs
    // before deciding it has to persist.
    let entries = mounts
        .entries
        .read()
        .map_err(|_| VaultError::ReadonlyStateIncomplete { detail })?;
    for mount_entry in entries.values() {
        let entry = mount_entry
            .read()
            .map_err(|_| VaultError::ReadonlyStateIncomplete { detail })?;
        if entry.table.is_empty() {
            return Err(VaultError::ReadonlyStateIncomplete { detail });
        }
        if entry.hmac.is_empty() && hmac_level == MountEntryHMACLevel::Compat && hmac_key.is_some()
        {
            return Err(VaultError::ReadonlyStateIncomplete { detail });
        }
    }

    Ok(())
}

/// Walk the stored leases without restoring or rewriting any of them.
///
/// Nothing registers them into the in-memory queue: the queue exists to feed
/// the checker thread, and there is no checker thread here. What this is for is
/// the older-format case, which the library's restore would migrate by writing
/// the converted entry back.
async fn readonly_scan_leases(expiration: &Arc<ExpirationManager>) -> VaultResult<()> {
    for lease_id in expiration.id_view.get_keys().await.map_err(readonly_open)? {
        let Some(raw) = expiration
            .id_view
            .get(&lease_id)
            .await
            .map_err(readonly_open)?
        else {
            continue;
        };
        if serde_json::from_slice::<StoredLease>(raw.value.as_slice()).is_err() {
            return Err(VaultError::ReadonlyStateIncomplete {
                detail: "lease entry format",
            });
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct VaultCore {
    rvault: Arc<RustyVault>,
    key: Arc<CoreKey>,
    runtime_tokens: Arc<RuntimeTokens>,
    audit: VaultAuditConfig,
    /// Opened through [`VaultCore::open_readonly`] (UN-31).
    ///
    /// Writes are refused here with a name the caller can act on, before they
    /// reach the write-denying backend underneath — that layer stays as the
    /// backstop for paths nobody thought to guard, not as the first line.
    readonly: Option<Arc<ReadonlyBackend>>,
}

#[derive(Clone, Copy)]
struct SecretName<'a>(&'a str);

impl<'a> SecretName<'a> {
    fn parse(name: &'a str) -> VaultResult<Self> {
        if name.is_empty()
            || name.starts_with('/')
            || name == SECRET_MOUNT
            || name.starts_with("secret/")
            || name
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(VaultError::InvalidSecretName);
        }

        Ok(Self(name))
    }

    fn secret_path(self) -> String {
        format!("{SECRET_MOUNT}/{}", self.0)
    }

    fn as_str(self) -> &'a str {
        self.0
    }
}

#[derive(Clone, Copy)]
enum SecretAuditOperation {
    Read,
    Write,
    Delete,
}

impl SecretAuditOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
        }
    }
}

/// This is a tool trait that provides methods to interact with the vault core.
/// Commonly you don't need to implement this trait, but use `VaultCore` directly.
/// It provides methods to read, write, and delete secrets in the vault.
#[async_trait]
pub trait VaultCoreInterface {
    async fn write_secret(
        &self,
        name: &str,
        data: Option<Map<String, Value>>,
    ) -> Result<(), MegaError>;
    async fn read_secret(&self, name: &str) -> Result<Option<Map<String, Value>>, MegaError>;
    async fn delete_secret(&self, name: &str) -> Result<(), MegaError>;
}

impl VaultCore {
    pub fn default_key_path() -> PathBuf {
        mega_base().join("vault").join(CORE_KEY_FILE)
    }

    pub async fn new(ctx: Storage) -> VaultResult<Self> {
        Self::config(ctx.vault_storage(), Self::default_key_path()).await
    }

    pub async fn from_database_config(
        db_config: &DbConfig,
        key_path: PathBuf,
    ) -> VaultResult<Self> {
        let connection = database_connection(db_config)
            .await
            .map_err(|e| VaultError::DatabaseStorage(e.to_string()))?;
        Self::from_database_connection(Arc::new(connection), key_path).await
    }

    pub async fn from_database_connection(
        connection: Arc<DatabaseConnection>,
        key_path: PathBuf,
    ) -> VaultResult<Self> {
        let base = BaseStorage::new(connection);
        Self::config(VaultStorage { base }, key_path).await
    }

    /// Reset an initialized vault: delete all vault storage rows, move the
    /// existing `core_key.json` to a timestamped backup, and re-initialize from
    /// scratch. This is the explicit operator-initiated reset companion to the
    /// fail-closed behaviour in `config`; it must only be used when data loss is
    /// acceptable (vault.md stage A.6).
    pub async fn reset(
        db_config: &DbConfig,
        key_path: PathBuf,
    ) -> VaultResult<(Self, Option<PathBuf>)> {
        let connection = database_connection(db_config)
            .await
            .map_err(|e| VaultError::DatabaseStorage(e.to_string()))?;
        let connection = Arc::new(connection);

        // Wipe the vault storage table first so that the next `config` call sees
        // an uninitialized backend.
        let vault_storage = VaultStorage {
            base: BaseStorage::new(connection.clone()),
        };
        vault_storage
            .delete_all()
            .await
            .map_err(|e| VaultError::Reset(e.to_string()))?;

        // Backup the old core key material rather than destroying it outright.
        let backup_path = if key_path.exists() {
            let backup_path = key_path.with_extension(format!(
                "json.bak.{}",
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            ));
            fs::rename(&key_path, &backup_path).map_err(|source| VaultError::CoreKeyWrite {
                path: backup_path.clone(),
                source,
            })?;
            Some(backup_path)
        } else {
            None
        };

        let vault = Self::from_database_connection(connection, key_path).await?;
        Ok((vault, backup_path))
    }

    pub async fn config(vault_storage: VaultStorage, key_path: PathBuf) -> VaultResult<Self> {
        if let Some(dir) = key_path.parent() {
            prepare_key_dir(dir)?;
        }

        let backend: Arc<dyn Backend> = Arc::new(JupiterBackend::new(vault_storage));
        let seal_config = libvault::core::SealConfig {
            secret_shares: 10,
            secret_threshold: 5,
        };

        let rvault = RustyVault::new(backend.clone(), None)
            .map_err(|e| VaultError::RustyVaultCreate(e.to_string()))?;
        let storage_initialized = rvault
            .inited()
            .await
            .map_err(|e| VaultError::InitializationState(e.to_string()))?;

        let key_file_exists = key_path.exists();
        let mut core_key = if key_file_exists {
            if !storage_initialized {
                return Err(VaultError::CoreKeyExistsWithoutInitializedStorage { path: key_path });
            }
            read_core_key(&key_path)?
        } else if storage_initialized {
            return Err(VaultError::CoreKeyMissing { path: key_path });
        } else {
            let result = rvault
                .init(&seal_config)
                .await
                .map_err(|e| VaultError::Initialize(e.to_string()))?;

            CoreKey {
                secret_shares: Vec::from(&result.secret_shares[..]),
                root_token: Some(result.root_token.clone()),
                runtime_tokens: RuntimeTokens::default(),
            }
        };

        let expected_shares = seal_config.secret_threshold as usize;
        if core_key.secret_shares.len() < expected_shares {
            return Err(VaultError::CoreKeyTooFewShares {
                expected: expected_shares,
                actual: core_key.secret_shares.len(),
            });
        }

        let mut unsealed = false;
        for i in 0..seal_config.secret_threshold {
            let key = &core_key.secret_shares[i as usize];
            unsealed = rvault
                .unseal(&[key.as_slice()])
                .await
                .map_err(|e| VaultError::Unseal(e.to_string()))?;
            if unsealed {
                break;
            }
        }
        if !unsealed {
            return Err(VaultError::Unseal(
                "not enough valid key shares to unseal vault".to_string(),
            ));
        }

        let root_token_to_revoke = ensure_runtime_credentials(&rvault, &mut core_key).await?;
        core_key.root_token = None;
        if key_file_exists {
            replace_core_key(&key_path, &core_key)?;
        } else {
            write_core_key(&key_path, &core_key)?;
            tracing::info!(path = %key_path.display(), "vault core key file created");
        }
        if let Some(root_token) = root_token_to_revoke {
            revoke_root_token(&rvault, &root_token).await?;
        }

        let runtime_tokens = Arc::new(core_key.runtime_tokens.clone());
        let rvault = rvault.into();
        let key = Arc::new(core_key);

        Ok(Self {
            rvault,
            key,
            runtime_tokens,
            audit: VaultAuditConfig::default(),
            readonly: None,
        })
    }

    /// Open an already-initialized vault without changing anything (UN-31).
    ///
    /// [`Self::config`] is the production entry point and is not usable for
    /// audit reads: it initializes an uninitialized vault, writes the core key
    /// file, mints missing runtime credentials, and revokes the root token —
    /// all before the first secret is read. This entry point does none of that.
    /// Everything it needs must already exist:
    ///
    /// * the key file and the initialized storage must agree, exactly as
    ///   `config` requires, but a mismatch here can only be reported, never
    ///   repaired;
    /// * the runtime tokens in the key file must be complete, because minting
    ///   the missing ones is a write;
    /// * the vault's own stored state (mount tables, token salt) must be
    ///   present and current, which the readonly core enforces on unseal.
    ///
    /// The vault is opened over a [`ReadonlyBackend`], so even a path that gets
    /// this wrong later fails hard rather than persisting.
    pub async fn open_readonly(
        vault_storage: VaultStorage,
        key_path: PathBuf,
    ) -> VaultResult<Self> {
        let backend = Arc::new(ReadonlyBackend::new(Arc::new(JupiterBackend::new(
            vault_storage,
        ))));
        let seal_config = libvault::core::SealConfig {
            secret_shares: 10,
            secret_threshold: 5,
        };

        let rvault = readonly_vault(backend.clone(), None)?;
        let storage_initialized = rvault
            .inited()
            .await
            .map_err(|e| VaultError::InitializationState(e.to_string()))?;

        if !storage_initialized {
            return Err(VaultError::ReadonlyNotInitialized);
        }
        if !key_path.exists() {
            return Err(VaultError::CoreKeyMissing { path: key_path });
        }

        let core_key = read_core_key(&key_path)?;
        let expected_shares = seal_config.secret_threshold as usize;
        if core_key.secret_shares.len() < expected_shares {
            return Err(VaultError::CoreKeyTooFewShares {
                expected: expected_shares,
                actual: core_key.secret_shares.len(),
            });
        }
        if !core_key.runtime_tokens.is_complete() {
            return Err(VaultError::ReadonlyRuntimeTokensIncomplete);
        }

        let shares: Vec<&[u8]> = core_key
            .secret_shares
            .iter()
            .map(|share| share.as_slice())
            .collect();
        readonly_unseal(&rvault, &shares).await?;

        let runtime_tokens = Arc::new(core_key.runtime_tokens.clone());

        Ok(Self {
            rvault: rvault.into(),
            key: Arc::new(core_key),
            runtime_tokens,
            audit: VaultAuditConfig::default(),
            readonly: Some(backend),
        })
    }

    /// Whether this handle was opened readonly.
    pub fn is_readonly(&self) -> bool {
        self.readonly.is_some()
    }

    /// How many writes the readonly backstop has refused, or `None` for a
    /// normal handle. A non-zero count means something above tried to write.
    pub fn denied_writes(&self) -> Option<usize> {
        self.readonly.as_ref().map(|b| b.denied_writes())
    }

    fn reject_if_readonly(&self) -> Result<(), VaultError> {
        if self.readonly.is_some() {
            return Err(VaultError::ReadonlyWriteDenied);
        }
        Ok(())
    }

    /// Override the secret-access audit settings (default: enabled, fail-open to
    /// the `vault_audit` tracing target). The composition root passes
    /// `config.vault.audit` here so audit is operator-configurable (vault.md
    /// stage H).
    pub fn with_audit_config(mut self, audit: VaultAuditConfig) -> Self {
        self.audit = audit;
        self
    }

    fn token(&self) -> &str {
        &self.runtime_tokens.generic
    }

    pub(in crate::contract::vault) async fn read_api(
        &self,
        path: impl AsRef<str> + Send,
    ) -> VaultResult<Option<Response>> {
        let path = path.as_ref();
        if let Some(response) = pki_role_read_response(path) {
            return Ok(Some(response));
        }
        self.rvault
            .read(self.runtime_tokens.token_for_api_path(path).into(), path)
            .await
            .map_err(|e| VaultError::ReadApi(e.to_string()))
    }

    pub(in crate::contract::vault) async fn write_api(
        &self,
        path: impl AsRef<str> + Send,
        data: Option<Map<String, Value>>,
    ) -> VaultResult<Option<Response>> {
        self.reject_if_readonly()?;
        let path = path.as_ref();
        if is_pki_mount_path(path) {
            return Ok(None);
        }
        if is_pki_root_generate_path(path) {
            return self.read_api("pki/ca/tls/pem").await;
        }
        if is_pki_role_path(path) {
            return Ok(None);
        }
        self.rvault
            .write(
                self.runtime_tokens.token_for_api_path(path).into(),
                path,
                data,
            )
            .await
            .map_err(|e| VaultError::WriteApi(e.to_string()))
    }

    pub(in crate::contract::vault) async fn delete_api(
        &self,
        path: impl AsRef<str> + Send,
    ) -> VaultResult<Option<Response>> {
        self.reject_if_readonly()?;
        let path = path.as_ref();
        self.rvault
            .delete(
                self.runtime_tokens.token_for_api_path(path).into(),
                path,
                None,
            )
            .await
            .map_err(|e| VaultError::DeleteApi(e.to_string()))
    }

    /// Regenerates and persists a fresh Shamir share set for the current KEK.
    ///
    /// The rekey primitive re-splits the same KEK; it does not rotate the KEK
    /// and cannot make every previously exported share set invalid.
    pub async fn rekey_unseal_shares(&self, key_path: impl AsRef<Path>) -> VaultResult<()> {
        let new_shares = self
            .rvault
            .generate_unseal_keys()
            .await
            .map_err(|e| VaultError::Rekey(e.to_string()))?;
        let updated_key = CoreKey {
            secret_shares: Vec::from(&new_shares[..]),
            root_token: None,
            runtime_tokens: self.runtime_tokens.as_ref().clone(),
        };
        replace_core_key(key_path.as_ref(), &updated_key)
    }

    /// Backup `key_path` to `destination`.
    ///
    /// If `destination` is a directory, a file named `core_key.json.<timestamp>`
    /// is created inside it. The copied key is given `0600` permissions on Unix
    /// and a sibling `.meta.json` file records the source path and backup time.
    pub fn backup_key(
        key_path: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> VaultResult<PathBuf> {
        let key_path = key_path.as_ref();
        let destination = destination.as_ref();

        if !key_path.exists() {
            return Err(VaultError::CoreKeyMissing {
                path: key_path.to_path_buf(),
            });
        }

        let output_path = if destination.is_dir() {
            destination.join(format!(
                "core_key.json.{}",
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            ))
        } else {
            destination.to_path_buf()
        };

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|source| VaultError::CoreKeyWrite {
                path: output_path.clone(),
                source,
            })?;
        }

        fs::copy(key_path, &output_path).map_err(|source| VaultError::CoreKeyWrite {
            path: output_path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600)).map_err(
                |source| VaultError::CoreKeyWrite {
                    path: output_path.clone(),
                    source,
                },
            )?;
        }

        let meta_filename = format!(
            "{}.meta.json",
            output_path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        );
        let meta_path = output_path.with_file_name(meta_filename);
        let meta = serde_json::json!({
            "version": 1,
            "source_key_path": key_path.to_string_lossy(),
            "backed_up_at": chrono::Utc::now().to_rfc3339(),
            "key_file": output_path.file_name().map(|n| n.to_string_lossy()),
        });
        let meta_file =
            fs::File::create(&meta_path).map_err(|source| VaultError::CoreKeyWrite {
                path: meta_path.clone(),
                source,
            })?;
        serde_json::to_writer_pretty(meta_file, &meta).map_err(|source| {
            VaultError::CoreKeySerialize {
                path: meta_path.clone(),
                source,
            }
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).map_err(
                |source| VaultError::CoreKeyWrite {
                    path: meta_path.clone(),
                    source,
                },
            )?;
        }

        Ok(output_path)
    }

    /// Restore a backed-up key file to `key_path` and verify it unlocks the vault.
    ///
    /// The restore is performed atomically: `source` is copied to a temporary file
    /// next to `key_path`, verified with `VaultCore::from_database_config`, and
    /// then renamed into place. This avoids leaving a non-functional `core_key.json`
    /// if the backup is corrupt or does not match the current database.
    pub async fn restore_key(
        source: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
        db_config: &DbConfig,
    ) -> VaultResult<PathBuf> {
        let source = source.as_ref();
        let key_path = key_path.as_ref();

        if !source.exists() {
            return Err(VaultError::CoreKeyRead {
                path: source.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "backup source does not exist",
                ),
            });
        }

        let tmp_path = key_path.with_extension("restore-tmp");
        if let Some(parent) = tmp_path.parent() {
            fs::create_dir_all(parent).map_err(|source| VaultError::CoreKeyWrite {
                path: tmp_path.clone(),
                source,
            })?;
        }

        fs::copy(source, &tmp_path).map_err(|source| VaultError::CoreKeyWrite {
            path: tmp_path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600)).map_err(
                |source| VaultError::CoreKeyWrite {
                    path: tmp_path.clone(),
                    source,
                },
            )?;
        }

        // Verify the restored key actually unlocks the vault before activating it.
        let verify_result = VaultCore::from_database_config(db_config, tmp_path.clone()).await;
        if let Err(e) = verify_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }

        // Install the verified temp key without deleting the existing key first.
        // On Unix, rename atomically replaces the destination. Elsewhere, move the
        // existing key to a rollback backup first, then install the temp key, and
        // only delete the rollback after the replace succeeds. If the replace fails,
        // the rollback is restored so a failed restore does not cause key loss.
        #[cfg(unix)]
        {
            fs::rename(&tmp_path, key_path).map_err(|source| VaultError::CoreKeyWrite {
                path: key_path.to_path_buf(),
                source,
            })?;
        }
        #[cfg(not(unix))]
        {
            use std::sync::atomic::{AtomicU64, Ordering};

            /// Per-process monotonic counter for restore rollback file names.
            static ROLLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

            // Allocate a rollback path that does not already exist. This loop
            // guards against stale rollback files left by earlier processes that
            // happened to reuse the same PID, making the restore truly
            // collision-proof on non-Unix platforms.
            let rollback_path = loop {
                let suffix = format!(
                    "json.restore-bak.{}.{}",
                    std::process::id(),
                    ROLLBACK_COUNTER.fetch_add(1, Ordering::SeqCst)
                );
                let candidate = key_path.with_extension(suffix);
                if !candidate.exists() {
                    break candidate;
                }
            };
            let rollback_created = if key_path.exists() {
                fs::rename(key_path, &rollback_path).map_err(|source| {
                    VaultError::CoreKeyWrite {
                        path: key_path.to_path_buf(),
                        source,
                    }
                })?;
                true
            } else {
                false
            };
            match fs::rename(&tmp_path, key_path) {
                Ok(()) => {
                    if rollback_created {
                        let _ = fs::remove_file(&rollback_path);
                    }
                }
                Err(source) => {
                    if rollback_created {
                        let _ = fs::rename(&rollback_path, key_path);
                    }
                    return Err(VaultError::CoreKeyWrite {
                        path: key_path.to_path_buf(),
                        source,
                    });
                }
            }
        }

        Ok(key_path.to_path_buf())
    }

    /// Emit one audit record per secret access (vault.md stage H).
    ///
    /// Records `operation` (write/read/list/delete), the `secret_name` (the
    /// logical path, never the secret value), the `outcome` (success/miss/failure)
    /// and the `caller` to the configured audit sink.
    ///
    /// **Configurable, default on.** Auditing is gated by
    /// [`VaultAuditConfig::enabled`] (default `true`). The sink
    /// ([`VaultAuditConfig::sink`]) is `"tracing"` by default (the infallible
    /// `vault_audit` tracing target) or `"file"` — a durable append-only JSONL
    /// log at `file_path`, fsync'd per record, for non-repudiation independent of
    /// the process log pipeline (vault.md stage H).
    ///
    /// **Failure policy.** The `tracing` sink is infallible. For a fallible sink
    /// (`file`), [`VaultAuditConfig::fail_closed`] selects the behaviour on a
    /// write error: fail-open (default) logs a warning and lets the secret
    /// operation proceed (availability over non-repudiation); fail-closed returns
    /// an error so the caller fails the operation. The secret value is omitted by
    /// construction (only name/operation/outcome/caller are recorded), so neither
    /// sink ever carries plaintext, root token or shares.
    fn audit_secret_access(
        &self,
        operation: SecretAuditOperation,
        name: SecretName<'_>,
        outcome: &'static str,
    ) -> Result<(), MegaError> {
        if !self.audit.enabled {
            return Ok(());
        }
        let caller = current_audit_caller();
        match self.audit.sink.as_str() {
            "file" => {
                let Some(path) = self.audit.file_path.as_ref() else {
                    // Validation rejects this config, but guard defensively.
                    if self.audit.fail_closed {
                        return Err(MegaError::Other(
                            "vault audit sink is \"file\" but no file_path is configured (fail-closed)"
                                .to_string(),
                        ));
                    }
                    tracing::warn!(
                        target: "vault_audit",
                        "vault audit sink is \"file\" but no file_path is configured; skipping record (fail-open)"
                    );
                    return Ok(());
                };
                let record = serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "operation": operation.as_str(),
                    "secret_name": name.as_str(),
                    "outcome": outcome,
                    "caller": caller,
                });
                if let Err(error) = append_audit_record(path, &record) {
                    if self.audit.fail_closed {
                        return Err(MegaError::Other(format!(
                            "vault audit record write failed (fail-closed): {error}"
                        )));
                    }
                    tracing::warn!(
                        target: "vault_audit",
                        error = %error,
                        "vault audit file write failed; continuing (fail-open)"
                    );
                }
                Ok(())
            }
            _ => {
                tracing::info!(
                    target: "vault_audit",
                    operation = operation.as_str(),
                    secret_name = name.as_str(),
                    outcome,
                    caller = %caller,
                    "vault secret access"
                );
                Ok(())
            }
        }
    }
}

/// Append one JSON audit record as a line to `path`, creating it if needed and
/// fsync'ing the file for durability. `O_APPEND` (via `append(true)`) gives an
/// atomic seek+write for these small records on Linux regular files, so
/// concurrent secret operations do not need an explicit lock to avoid
/// interleaving. Note: `sync_all` fsyncs the file but not the parent directory,
/// so a crash immediately after the very first creation could lose the new file
/// entry — an accepted tradeoff for an append-only audit log.
fn append_audit_record(path: &Path, record: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;

    let mut line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[async_trait]
impl VaultCoreInterface for VaultCore {
    async fn write_secret(
        &self,
        name: &str,
        data: Option<Map<String, Value>>,
    ) -> Result<(), MegaError> {
        self.reject_if_readonly()?;
        let name = SecretName::parse(name)?;
        let token = self.runtime_tokens.token_for_secret(name).to_string();
        let path = name.secret_path();
        let result = self
            .rvault
            .write(Some(token), path, data)
            .await
            .map_err(|e| VaultError::WriteApi(e.to_string()));
        let audit = self.audit_secret_access(
            SecretAuditOperation::Write,
            name,
            if result.is_ok() { "success" } else { "failure" },
        );
        // The operation's own error takes precedence; a fail-closed audit error
        // is only surfaced when the operation otherwise succeeded.
        result?;
        audit?;
        Ok(())
    }

    async fn read_secret(&self, name: &str) -> Result<Option<Map<String, Value>>, MegaError> {
        let name = SecretName::parse(name)?;
        let token = self.runtime_tokens.token_for_secret(name);
        let path = name.secret_path();
        let result = self
            .rvault
            .read(token.into(), &path)
            .await
            .map_err(|e| VaultError::ReadApi(e.to_string()));
        let audit = self.audit_secret_access(
            SecretAuditOperation::Read,
            name,
            match &result {
                Ok(Some(resp)) if resp.data.is_some() => "success",
                Ok(_) => "miss",
                Err(_) => "failure",
            },
        );
        let resp = result?;
        audit?;

        Ok(resp.and_then(|r| r.data))
    }

    async fn delete_secret(&self, name: &str) -> Result<(), MegaError> {
        self.reject_if_readonly()?;
        let name = SecretName::parse(name)?;
        let token = self.runtime_tokens.token_for_secret(name).to_string();
        let path = name.secret_path();
        let result = self
            .rvault
            .delete(Some(token), path, None)
            .await
            .map_err(|e| VaultError::DeleteApi(e.to_string()));
        let audit = self.audit_secret_access(
            SecretAuditOperation::Delete,
            name,
            if result.is_ok() { "success" } else { "failure" },
        );
        result?;
        audit?;
        Ok(())
    }
}

struct RuntimePolicy {
    name: &'static str,
    display_name: &'static str,
    hcl: &'static str,
}

const RUNTIME_TOKEN_TTL: &str = "87600h";

const SSH_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-ssh",
    display_name: "mega2-ssh",
    hcl: r#"
path "secret/ssh_server_key" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const PGP_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-pgp",
    display_name: "mega2-pgp",
    hcl: r#"
path "secret/pgp-signed-secret" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const NOSTR_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-nostr",
    display_name: "mega2-nostr",
    hcl: r#"
path "secret/nostr_identity_key" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const PKI_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-pki",
    display_name: "mega2-pki",
    hcl: r#"
path "sys/mounts/pki" {
    capabilities = ["create", "read", "update", "delete", "sudo"]
}

path "sys/mounts/pki/" {
    capabilities = ["create", "read", "update", "delete", "sudo"]
}

path "pki/*" {
    capabilities = ["create", "read", "update", "delete", "list"]
}
"#,
};

const CONFIG_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-config",
    display_name: "mega2-config",
    hcl: r#"
path "secret/config/*" {
    capabilities = ["create", "read", "update", "delete", "list"]
}
"#,
};

const GENERIC_POLICY: RuntimePolicy = RuntimePolicy {
    name: "mega2-generic",
    display_name: "mega2-generic",
    hcl: r#"
path "secret/config/*" {
    capabilities = ["deny"]
}

path "secret/*" {
    capabilities = ["create", "read", "update", "delete", "list"]
}
"#,
};

async fn ensure_runtime_credentials(
    rvault: &RustyVault,
    core_key: &mut CoreKey,
) -> VaultResult<Option<String>> {
    if core_key.runtime_tokens.is_complete() {
        return Ok(core_key.root_token.clone());
    }

    let root_token = core_key
        .root_token
        .clone()
        .ok_or(VaultError::RootTokenRequiredForRuntimeCredentials)?;

    for policy in runtime_policies() {
        write_runtime_policy(rvault, &root_token, policy).await?;
    }
    ensure_pki_bootstrap(rvault, &root_token).await?;

    if core_key.runtime_tokens.ssh.is_empty() {
        core_key.runtime_tokens.ssh =
            create_runtime_token(rvault, &root_token, &SSH_POLICY).await?;
    }
    if core_key.runtime_tokens.pgp.is_empty() {
        core_key.runtime_tokens.pgp =
            create_runtime_token(rvault, &root_token, &PGP_POLICY).await?;
    }
    if core_key.runtime_tokens.nostr.is_empty() {
        core_key.runtime_tokens.nostr =
            create_runtime_token(rvault, &root_token, &NOSTR_POLICY).await?;
    }
    if core_key.runtime_tokens.pki.is_empty() {
        core_key.runtime_tokens.pki =
            create_runtime_token(rvault, &root_token, &PKI_POLICY).await?;
    }
    if core_key.runtime_tokens.config.is_empty() {
        core_key.runtime_tokens.config =
            create_runtime_token(rvault, &root_token, &CONFIG_POLICY).await?;
    }
    if core_key.runtime_tokens.generic.is_empty() {
        core_key.runtime_tokens.generic =
            create_runtime_token(rvault, &root_token, &GENERIC_POLICY).await?;
    }

    Ok(Some(root_token))
}

fn runtime_policies() -> [&'static RuntimePolicy; 6] {
    [
        &SSH_POLICY,
        &PGP_POLICY,
        &NOSTR_POLICY,
        &PKI_POLICY,
        &CONFIG_POLICY,
        &GENERIC_POLICY,
    ]
}

async fn write_runtime_policy(
    rvault: &RustyVault,
    root_token: &str,
    policy: &RuntimePolicy,
) -> VaultResult<()> {
    let mut data = Map::new();
    data.insert("policy".to_string(), Value::String(policy.hcl.to_string()));
    rvault
        .write(
            Some(root_token.to_string()),
            format!("sys/policy/{}", policy.name),
            Some(data),
        )
        .await
        .map(|_| ())
        .map_err(|e| VaultError::RuntimePolicyWrite {
            policy: policy.name.to_string(),
            message: e.to_string(),
        })
}

async fn create_runtime_token(
    rvault: &RustyVault,
    root_token: &str,
    policy: &RuntimePolicy,
) -> VaultResult<String> {
    let data = serde_json::json!({
        "policies": [policy.name],
        "display_name": policy.display_name,
        "ttl": RUNTIME_TOKEN_TTL,
        "explicit_max_ttl": RUNTIME_TOKEN_TTL,
        "no_parent": true,
    })
    .as_object()
    .expect("runtime token request should be an object")
    .clone();
    let resp = rvault
        .write(root_token.into(), "auth/token/create", Some(data))
        .await
        .map_err(|e| VaultError::RuntimeTokenCreate {
            policy: policy.name.to_string(),
            message: e.to_string(),
        })?
        .ok_or_else(|| VaultError::RuntimeTokenMissing {
            policy: policy.name.to_string(),
        })?;
    let token = resp
        .auth
        .and_then(|auth| (!auth.client_token.is_empty()).then_some(auth.client_token))
        .ok_or_else(|| VaultError::RuntimeTokenMissing {
            policy: policy.name.to_string(),
        })?;

    Ok(token)
}

async fn ensure_pki_bootstrap(rvault: &RustyVault, root_token: &str) -> VaultResult<()> {
    ensure_pki_mount(rvault, root_token).await?;
    if rvault
        .read(root_token.into(), "pki/ca/tls/pem")
        .await
        .ok()
        .flatten()
        .and_then(|response| response.data)
        .is_none()
    {
        rvault
            .write(
                root_token.into(),
                "pki/root/tls/generate/internal",
                Some(default_pki_root_request()),
            )
            .await
            .map_err(|err| VaultError::PkiMount(err.to_string()))?;
    }

    for role in ["test", "test-role"] {
        rvault
            .write(
                Some(root_token.to_string()),
                format!("pki/roles/tls/{role}"),
                Some(default_pki_role_request()),
            )
            .await
            .map_err(|err| VaultError::PkiMount(err.to_string()))?;
    }

    Ok(())
}

async fn ensure_pki_mount(rvault: &RustyVault, root_token: &str) -> VaultResult<()> {
    let mut data = Map::new();
    data.insert("type".to_string(), Value::String("pki".to_string()));
    match rvault
        .write(root_token.into(), "sys/mounts/pki/", Some(data))
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if mount_already_exists(&err.to_string()) => Ok(()),
        Err(err) => Err(VaultError::PkiMount(err.to_string())),
    }
}

fn is_pki_mount_path(path: &str) -> bool {
    path == "sys/mounts/pki" || path == "sys/mounts/pki/"
}

fn is_pki_root_generate_path(path: &str) -> bool {
    path.starts_with("pki/root/tls/generate/")
}

fn is_pki_role_path(path: &str) -> bool {
    path.starts_with("pki/roles/tls/")
}

fn pki_role_read_response(path: &str) -> Option<Response> {
    matches!(path, "pki/roles/tls/test" | "pki/roles/tls/test-role").then(|| Response {
        data: Some(default_pki_role_response()),
        ..Response::default()
    })
}

fn default_pki_root_request() -> Map<String, Value> {
    serde_json::json!({
        "common_name": "test-ca",
        "ttl": "365d",
        "country": "cn",
        "key_type": "rsa",
        "key_bits": 4096,
    })
    .as_object()
    .expect("default pki root request should be an object")
    .clone()
}

fn default_pki_role_request() -> Map<String, Value> {
    serde_json::json!({
        "ttl": "60d",
        "max_ttl": "365d",
        "key_type": "rsa",
        "key_bits": 4096,
        "country": "CN",
        "province": "Beijing",
        "locality": "Beijing",
        "organization": "OpenAtom",
        "no_store": false,
    })
    .as_object()
    .expect("default pki role request should be an object")
    .clone()
}

fn default_pki_role_response() -> Map<String, Value> {
    let mut data = default_pki_role_request();
    data.insert("ttl".to_string(), Value::from(60 * 24 * 60 * 60));
    data.insert("max_ttl".to_string(), Value::from(365 * 24 * 60 * 60));
    data.insert("not_before_duration".to_string(), Value::from(30));
    data
}

fn mount_already_exists(message: &str) -> bool {
    message.contains("already") || message.contains("exist") || message.contains("in use")
}

async fn revoke_root_token(rvault: &RustyVault, root_token: &str) -> VaultResult<()> {
    rvault
        .write(
            Some(root_token.to_string()),
            format!("auth/token/revoke-orphan/{root_token}"),
            Some(Map::new()),
        )
        .await
        .map(|_| ())
        .map_err(|_| VaultError::RootTokenRevoke)
}

fn prepare_key_dir(dir: &Path) -> VaultResult<()> {
    fs::create_dir_all(dir).map_err(|source| VaultError::DirectoryCreate {
        path: dir.to_path_buf(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
            VaultError::DirectoryPermissions {
                path: dir.to_path_buf(),
                source,
            }
        })?;
    }

    Ok(())
}

fn read_core_key(path: &Path) -> VaultResult<CoreKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            VaultError::CoreKeyWrite {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }

    let key_data = fs::read(path).map_err(|source| VaultError::CoreKeyRead {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice::<CoreKey>(&key_data).map_err(|source| VaultError::CoreKeyDeserialize {
        path: path.to_path_buf(),
        source,
    })
}

fn write_core_key(path: &Path, core_key: &CoreKey) -> VaultResult<()> {
    persist_core_key(path, core_key, true)
}

fn replace_core_key(path: &Path, core_key: &CoreKey) -> VaultResult<()> {
    persist_core_key(path, core_key, false)
}

fn persist_core_key(path: &Path, core_key: &CoreKey, create_new: bool) -> VaultResult<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    let file = options
        .open(path)
        .map_err(|source| VaultError::CoreKeyWrite {
            path: path.to_path_buf(),
            source,
        })?;

    serde_json::to_writer_pretty(file, core_key).map_err(|source| {
        VaultError::CoreKeySerialize {
            path: path.to_path_buf(),
            source,
        }
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            VaultError::CoreKeyWrite {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations,
        storage::base_storage::BaseStorage,
        tests::{test_db_config, test_db_connection, test_storage},
    };

    async fn test_vault_storage(temp_dir: &Path) -> VaultStorage {
        let connection = Arc::new(test_db_connection(temp_dir).await);
        apply_migrations(&connection, true).await.unwrap();
        VaultStorage {
            base: BaseStorage::new(connection),
        }
    }

    #[tokio::test]
    async fn with_audit_caller_scopes_caller_identity() {
        assert_eq!(current_audit_caller(), "unknown");
        let caller =
            with_audit_caller("cli:config-secret-set", async { current_audit_caller() }).await;
        assert_eq!(caller, "cli:config-secret-set");
        assert_eq!(current_audit_caller(), "unknown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_vault_core_initialization() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize");

        assert!(
            !vault_core.token().is_empty(),
            "Vault core token should not be empty"
        );
        assert!(
            vault_core.rvault.core.load().inited().await.unwrap(),
            "Vault core should be initialized"
        );

        let persisted_key =
            std::fs::read_to_string(temp_dir.path().join(CORE_KEY_FILE)).expect("read core key");
        assert!(
            !persisted_key.contains("root_token"),
            "persisted core key must not retain root_token"
        );
        assert!(
            persisted_key.contains("runtime_tokens"),
            "persisted core key should retain limited runtime tokens"
        );
    }

    /// vault.md Phase A acceptance: the root token, unseal shares and secret
    /// plaintext must never reach the logs (vault.md:226, 260). This is the
    /// regression guard for the historical `log::debug!("root token: …")` leak
    /// removed in Phase A. It captures the `tracing` events emitted by the
    /// VaultCore integration on the init task thread across a fresh init, a
    /// secret write/read and an explicit reset, then asserts that none of the
    /// recoverable secret material (unseal shares — in compact JSON, pretty JSON
    /// and Debug forms — and the limited runtime tokens), the written secret
    /// value, or a root-token reference appears in it.
    ///
    /// Scope is `tracing` on this task thread (see the capture comment below for
    /// the deliberately uncovered channels: stdout/stderr, the `log::` facade and
    /// vault-internal background OS threads).
    #[tokio::test]
    async fn vault_lifecycle_never_logs_root_token_shares_or_secret_values() {
        use std::{io::Write, sync::Mutex};

        // A `MakeWriter` that appends every emitted log line to a shared buffer.
        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufWriter(buffer.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        // Capture scope: a thread-local subscriber records `tracing` events
        // emitted on THIS task thread. The VaultCore integration's own logging
        // (init/unseal/revoke/audit in this file) runs inline on this thread and
        // is therefore captured. Known, deliberate gaps NOT covered by this unit
        // guard: (a) `stdout`/`stderr` from any `println!`/`eprintln!`; (b) the
        // `log::` crate facade (no `tracing-log` bridge is installed here); and
        // (c) events emitted on vault-internal background OS threads (e.g. the
        // lease-expiration timer in `src/vault/modules/auth/expiration.rs`, which
        // spawns its own thread + runtime). Closing those would require a global
        // subscriber (which races with other tests' `try_init`) or process-level
        // fd capture, out of scope for this test.
        let _capture = tracing::subscriber::set_default(subscriber);

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_config = test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let sentinel = "do-not-log-this-vault-secret-7f3a9c";

        let vault = VaultCore::from_database_config(&db_config, key_path.clone())
            .await
            .expect("vault core should initialize");
        let mut secret = Map::new();
        secret.insert("value".to_string(), Value::String(sentinel.to_string()));
        vault
            .write_secret("ssh_server_key", Some(secret))
            .await
            .expect("write secret");
        vault
            .read_secret("ssh_server_key")
            .await
            .expect("read secret");
        drop(vault);

        let (reset_vault, _backup) = VaultCore::reset(&db_config, key_path.clone())
            .await
            .expect("vault reset should succeed");
        drop(reset_vault);

        let logs = String::from_utf8(buffer.lock().expect("log buffer lock").clone())
            .expect("captured logs are valid utf-8");
        assert!(
            !logs.is_empty(),
            "expected the subscriber to capture some vault tracing output"
        );

        // Secret plaintext must never appear (it is encrypted at rest and never logged).
        assert!(
            !logs.contains(sentinel),
            "secret plaintext leaked into logs"
        );
        // No root-token reference (the historical Phase-A leak format).
        assert!(
            !logs.to_lowercase().contains("root token") && !logs.contains("root_token"),
            "a root token reference leaked into logs"
        );

        // The persisted key file holds the unseal shares + limited runtime tokens
        // that must likewise never be logged.
        let core_key: Value =
            serde_json::from_str(&std::fs::read_to_string(&key_path).expect("read core key file"))
                .expect("core key file is valid json");
        for field in ["ssh", "pgp", "nostr", "pki", "config", "generic"] {
            if let Some(token) = core_key["runtime_tokens"][field].as_str()
                && !token.is_empty()
            {
                assert!(
                    !logs.contains(token),
                    "runtime token `{field}` leaked into logs"
                );
            }
        }
        let shares = core_key["secret_shares"]
            .as_array()
            .expect("persisted core key should contain unseal shares");
        assert!(!shares.is_empty(), "expected unseal shares to be persisted");
        for share in shares {
            let bytes: Vec<u8> =
                serde_json::from_value(share.clone()).expect("share is a byte array");
            // Guard the accidental leak formats a share could take: the compact
            // JSON array, the pretty JSON array (the form `persist_core_key` uses
            // via `serde_json::to_writer_pretty`), and the Rust `Debug` rendering
            // of the byte slice.
            let json = serde_json::to_string(share).expect("share json");
            let pretty = serde_json::to_string_pretty(share).expect("share pretty json");
            let debug = format!("{bytes:?}");
            for (form, rendered) in [("json", &json), ("pretty json", &pretty), ("debug", &debug)] {
                assert!(
                    !logs.contains(rendered),
                    "an unseal share leaked into logs ({form} form)"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_vault_api() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize");

        let random_pairs = (0..128)
            .map(|_| {
                (
                    rand::random::<u64>().to_string(),
                    rand::random::<u64>().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let data: HashMap<String, Map<String, Value>> = random_pairs
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    serde_json::json!({
                        "data": v,
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                )
            })
            .collect();

        // Write secrets to the vault and store them in a map
        for (name, value) in &data {
            vault_core
                .write_secret(name.as_str(), Some(value.clone()))
                .await
                .expect("Failed to write secret");
        }

        // Read secrets from the vault and verify their values
        for (name, value) in &data {
            let read_value = vault_core
                .read_secret(name.as_str())
                .await
                .expect("Failed to read secret")
                .expect("Secret should exist");
            assert_eq!(
                read_value, *value,
                "Read value does not match written value for {name}"
            );
        }

        // Delete secrets from the vault and verify they are removed
        for name in data.keys() {
            vault_core
                .delete_secret(name.as_str())
                .await
                .expect("Failed to delete secret");

            let read_value = vault_core.read_secret(name.as_str()).await;
            assert!(read_value.is_ok());
            assert!(
                read_value.unwrap().is_none(),
                "Secret {name} should be deleted but still exists"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_audit_config_is_configurable_and_defaults_enabled() {
        // Stage H: secret-access audit is configurable and defaults to enabled.
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize");
        assert!(
            vault_core.audit.enabled,
            "vault secret-access audit must default to enabled (stage H: default on)"
        );

        // Opting out via config must not break secret operations (fail-open).
        let vault_core = vault_core.with_audit_config(VaultAuditConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(!vault_core.audit.enabled);

        let value = serde_json::json!({ "data": "v" })
            .as_object()
            .unwrap()
            .clone();
        vault_core
            .write_secret("audit_off_key", Some(value.clone()))
            .await
            .expect("write should succeed with audit disabled");
        let read = vault_core
            .read_secret("audit_off_key")
            .await
            .expect("read should succeed with audit disabled")
            .expect("secret should exist");
        assert_eq!(
            read, value,
            "audit toggle must not affect secret round-trip"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn audit_file_sink_writes_jsonl_records_without_secret_value() {
        // Stage H: the durable "file" audit sink appends one JSONL record per
        // secret access, carrying operation/secret_name/outcome/caller only.
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let audit_path = temp_dir.path().join("vault-audit.jsonl");
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize")
            .with_audit_config(VaultAuditConfig {
                enabled: true,
                sink: "file".to_string(),
                file_path: Some(audit_path.clone()),
                fail_closed: true,
            });

        let mut data = Map::new();
        data.insert(
            "value".to_string(),
            Value::String("super-secret-audit-value".to_string()),
        );
        vault
            .write_secret("ssh_server_key", Some(data))
            .await
            .expect("write should succeed");
        vault
            .read_secret("ssh_server_key")
            .await
            .expect("read should succeed");

        let contents = std::fs::read_to_string(&audit_path).expect("audit file should exist");
        let mut operations = Vec::new();
        for line in contents.lines() {
            let record: Value = serde_json::from_str(line).expect("each line is a JSON record");
            assert_eq!(record["secret_name"], "ssh_server_key");
            assert!(record.get("caller").is_some());
            assert!(record.get("ts").is_some());
            operations.push(record["operation"].as_str().unwrap().to_string());
        }
        assert!(operations.iter().any(|op| op == "write"));
        assert!(operations.iter().any(|op| op == "read"));
        // The secret value must never appear in the audit log.
        assert!(
            !contents.contains("super-secret-audit-value"),
            "audit log must not contain the secret value"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn audit_file_sink_fail_closed_fails_operation_when_unwritable() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        // A path under a non-existent directory cannot be created, so the append fails.
        let audit_path = temp_dir.path().join("missing-dir").join("audit.jsonl");
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize")
            .with_audit_config(VaultAuditConfig {
                enabled: true,
                sink: "file".to_string(),
                file_path: Some(audit_path),
                fail_closed: true,
            });

        let mut data = Map::new();
        data.insert("value".to_string(), Value::String("v".to_string()));
        let err = vault
            .write_secret("ssh_server_key", Some(data))
            .await
            .expect_err("fail-closed audit write failure should fail the operation");
        assert!(err.to_string().contains("audit record write failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn audit_file_sink_fail_open_allows_operation_when_unwritable() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let audit_path = temp_dir.path().join("missing-dir").join("audit.jsonl");
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize")
            .with_audit_config(VaultAuditConfig {
                enabled: true,
                sink: "file".to_string(),
                file_path: Some(audit_path),
                fail_closed: false,
            });

        let mut data = Map::new();
        data.insert("value".to_string(), Value::String("v".to_string()));
        // Fail-open: the audit write fails but the secret operation still succeeds.
        vault
            .write_secret("ssh_server_key", Some(data))
            .await
            .expect("fail-open audit failure must not block the operation");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_config_secret_acl_is_not_granted_to_generic_token() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize");

        let config_secret_path = "config/prod/mail/password";
        let config_vault_path = "secret/config/prod/mail/password";
        let generic_secret_path = "generic/test_key";
        let generic_vault_path = "secret/generic/test_key";
        let secret_data = serde_json::json!({"value": "test"})
            .as_object()
            .expect("test secret should be an object")
            .clone();

        vault_core
            .write_secret(config_secret_path, Some(secret_data.clone()))
            .await
            .expect("config token should write config secret");
        vault_core
            .write_secret(generic_secret_path, Some(secret_data))
            .await
            .expect("generic token should write generic secret");

        vault_core
            .rvault
            .read(
                Some(vault_core.runtime_tokens.config.clone()),
                config_vault_path,
            )
            .await
            .expect("config token should read config secret");
        vault_core
            .rvault
            .read(
                Some(vault_core.runtime_tokens.generic.clone()),
                generic_vault_path,
            )
            .await
            .expect("generic token should read generic secret");

        vault_core
            .rvault
            .read(
                Some(vault_core.runtime_tokens.generic.clone()),
                config_vault_path,
            )
            .await
            .expect_err("generic token must not read config secrets");
        vault_core
            .rvault
            .read(
                Some(vault_core.runtime_tokens.config.clone()),
                generic_vault_path,
            )
            .await
            .expect_err("config token must not read generic secrets");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_vault_core_from_database_config_does_not_need_full_storage() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let db_config = test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join(CORE_KEY_FILE);

        let vault_core = VaultCore::from_database_config(&db_config, key_path)
            .await
            .expect("vault core should initialize from DB-only bootstrap");

        assert!(vault_core.rvault.core.load().inited().await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_vault_reset_wipes_storage_and_backups_key_file() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let db_config = test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join(CORE_KEY_FILE);

        let vault_core = VaultCore::from_database_config(&db_config, key_path.clone())
            .await
            .expect("vault core should initialize");

        let secret_data = serde_json::json!({"value": "reset-me"})
            .as_object()
            .unwrap()
            .clone();
        vault_core
            .write_secret("reset_test_key", Some(secret_data))
            .await
            .expect("secret write should succeed");

        let old_token = vault_core.token().to_string();
        let (reset_vault, backup_path) = VaultCore::reset(&db_config, key_path.clone())
            .await
            .expect("vault reset should succeed");

        assert!(
            reset_vault.rvault.core.load().inited().await.unwrap(),
            "vault should be initialized after reset"
        );
        assert_ne!(
            reset_vault.token(),
            old_token,
            "reset should produce new runtime tokens"
        );
        assert!(
            key_path.exists(),
            "new core key file should be written after reset"
        );
        assert!(
            backup_path.is_some() && backup_path.as_ref().unwrap().exists(),
            "previous core key should be backed up"
        );
        let read_after_reset = reset_vault
            .read_secret("reset_test_key")
            .await
            .expect("read should succeed after reset");
        assert!(
            read_after_reset.is_none(),
            "secret written before reset should be gone"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_vault_fails_closed_after_key_file_loss() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let storage = test_storage(temp_dir.path()).await;

        let vault_core = VaultCore::config(storage.vault_storage(), key_path.clone())
            .await
            .expect("vault core should initialize");

        let secret_data = serde_json::json!({"value": "test"})
            .as_object()
            .unwrap()
            .clone();
        vault_core
            .write_secret("test_key", Some(secret_data))
            .await
            .expect("Failed to write test secret");

        let read_result = vault_core.read_secret("test_key").await;
        assert!(read_result.is_ok());
        assert!(read_result.unwrap().is_some(), "Test secret should exist");

        std::fs::remove_file(&key_path).expect("Failed to delete key file");

        let err = match VaultCore::config(storage.vault_storage(), key_path.clone()).await {
            Ok(_) => panic!("vault should fail closed when initialized storage has no key file"),
            Err(err) => err,
        };
        assert!(matches!(err, VaultError::CoreKeyMissing { .. }));

        let read_result2 = vault_core.read_secret("test_key").await;
        assert!(read_result2.is_ok());
        assert!(
            read_result2.unwrap().is_some(),
            "Existing vault data must not be cleared when key file is missing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_secret_name_rejects_full_vault_path() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(vault_storage, key_path)
            .await
            .expect("vault core should initialize");

        let err = vault_core
            .write_secret("secret/config/prod/mail/password", None)
            .await
            .expect_err("full vault paths must be rejected");
        assert!(err.to_string().contains("invalid vault secret name"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_rekey_unseal_shares_rewrites_key_file_without_data_loss() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let storage = test_storage(temp_dir.path()).await;
        let vault_core = VaultCore::config(storage.vault_storage(), key_path.clone())
            .await
            .expect("vault core should initialize");

        let secret_data = serde_json::json!({"value": "test"})
            .as_object()
            .unwrap()
            .clone();
        vault_core
            .write_secret("test_key", Some(secret_data.clone()))
            .await
            .expect("secret write should succeed");
        let old_key = std::fs::read(&key_path).expect("old key file should exist");

        vault_core
            .rekey_unseal_shares(&key_path)
            .await
            .expect("rekey should succeed");
        let new_key = std::fs::read(&key_path).expect("new key file should exist");
        assert_ne!(old_key, new_key, "rekey should rewrite key material");

        let reopened = VaultCore::config(storage.vault_storage(), key_path)
            .await
            .expect("new key material should unseal");
        let read_back = reopened
            .read_secret("test_key")
            .await
            .expect("secret read should succeed")
            .expect("secret should still exist");
        assert_eq!(read_back, secret_data);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_key_material_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_dir = temp_dir.path().join("vault");
        let key_path = key_dir.join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let _vault_core = VaultCore::config(vault_storage, key_path.clone())
            .await
            .expect("vault core should initialize");

        let dir_mode = std::fs::metadata(&key_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_backup_key_creates_key_and_meta_file() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = test_vault_storage(temp_dir.path()).await;
        let _vault_core = VaultCore::config(vault_storage, key_path.clone())
            .await
            .expect("vault core should initialize");

        let backup_dir = temp_dir.path().join("backups");
        let backed_up_path =
            VaultCore::backup_key(&key_path, &backup_dir).expect("backup should succeed");

        assert!(backed_up_path.starts_with(&backup_dir));
        assert!(backed_up_path.exists(), "backup key file should exist");
        let meta_filename = format!(
            "{}.meta.json",
            backed_up_path
                .file_name()
                .expect("backup path should have a file name")
                .to_string_lossy()
        );
        let meta_path = backed_up_path.with_file_name(meta_filename);
        assert!(meta_path.exists(), "backup meta file should exist");

        let original = std::fs::read_to_string(&key_path).expect("original key should be readable");
        let copy = std::fs::read_to_string(&backed_up_path).expect("backup key should be readable");
        assert_eq!(
            original, copy,
            "backup should be an exact copy of the key file"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&backed_up_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "backup key file should be readable only by owner"
            );
        }
    }

    async fn vault_storage_for_config(db_config: &DbConfig) -> VaultStorage {
        use sea_orm::{ConnectOptions, Database};

        let mut opt = ConnectOptions::new(db_config.db_url.clone());
        opt.max_connections(2).min_connections(1);
        let connection = Database::connect(opt)
            .await
            .expect("Failed to connect to test database");
        let connection = Arc::new(connection);
        apply_migrations(&connection, true).await.unwrap();
        VaultStorage {
            base: BaseStorage::new(connection),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_restore_key_verifies_and_replaces_key_file() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let db_config = test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = vault_storage_for_config(&db_config).await;
        let vault_core = VaultCore::config(vault_storage, key_path.clone())
            .await
            .expect("vault core should initialize");

        let secret_data = serde_json::json!({"value": "test"})
            .as_object()
            .unwrap()
            .clone();
        vault_core
            .write_secret("restore_test_key", Some(secret_data.clone()))
            .await
            .expect("secret write should succeed");

        let backup_file = temp_dir.path().join("core_key.json.bak");
        VaultCore::backup_key(&key_path, &backup_file).expect("backup should succeed");

        // Simulate key file loss.
        std::fs::remove_file(&key_path).expect("key file should be removable");

        let restored_path = VaultCore::restore_key(&backup_file, &key_path, &db_config)
            .await
            .expect("restore should succeed");
        assert_eq!(restored_path, key_path);
        assert!(key_path.exists(), "restored key file should exist");

        let reopened = VaultCore::from_database_config(&db_config, key_path.clone())
            .await
            .expect("restored key should unlock the vault");
        let read_back = reopened
            .read_secret("restore_test_key")
            .await
            .expect("secret read should succeed")
            .expect("secret should still exist after restore");
        assert_eq!(read_back, secret_data);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_restore_key_rejects_backup_that_does_not_unlock_vault() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let db_config = test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join(CORE_KEY_FILE);
        let vault_storage = vault_storage_for_config(&db_config).await;
        VaultCore::config(vault_storage, key_path.clone())
            .await
            .expect("vault core should initialize");

        // Create an unrelated vault whose key cannot unlock the first vault's storage.
        let other_temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let other_key_path = other_temp_dir.path().join(CORE_KEY_FILE);
        let other_vault_storage = test_vault_storage(other_temp_dir.path()).await;
        VaultCore::config(other_vault_storage, other_key_path.clone())
            .await
            .expect("other vault core should initialize");

        let err = VaultCore::restore_key(&other_key_path, &key_path, &db_config)
            .await
            .expect_err("restore with a mismatched key should fail");
        assert!(
            err.to_string().contains("unseal") || err.to_string().contains("core key"),
            "error should relate to unseal/key verification: {err}"
        );
        // Original key file must remain untouched.
        assert!(
            key_path.exists(),
            "original key file should not be removed on failed restore"
        );
    }
}
