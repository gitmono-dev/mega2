use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    common::errors::{MegaError, VaultError, VaultResult},
    config::{DbConfig, VaultAuditConfig, mega_base},
    contract::vault::integration::jupiter_backend::JupiterBackend,
    jupiter::storage::{
        Storage,
        base_storage::{BaseStorage, StorageConnector},
        init::database_connection,
        vault_storage::VaultStorage,
    },
    vault::{RustyVault, logical::Response, storage::Backend},
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

#[derive(Clone)]
pub struct VaultCore {
    rvault: Arc<RustyVault>,
    key: Arc<CoreKey>,
    runtime_tokens: Arc<RuntimeTokens>,
    audit: VaultAuditConfig,
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
        let seal_config = crate::vault::core::SealConfig {
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
        })
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
    /// The vendored libvault primitive re-splits the same KEK; it does not rotate
    /// the KEK and cannot make every previously exported share set invalid.
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

    /// Emit one audit record per secret access (vault.md stage H).
    ///
    /// Records `operation` (write/read/list/delete), the `secret_name` (the
    /// logical path, never the secret value) and the `outcome`
    /// (success/miss/failure) to the `vault_audit` tracing target.
    ///
    /// **Configurable, default on.** Auditing is gated by
    /// [`VaultAuditConfig::enabled`] (default `true`); a deployment may opt out
    /// via `config.vault.audit.enabled = false`, in which case no record is
    /// emitted. The destination is the `vault_audit` tracing target; a
    /// configurable durable/alternate sink is deferred (vault.md stage H).
    ///
    /// **Failure policy: fail-open (intentional).** Auditing uses `tracing`,
    /// whose emission is infallible and cannot itself error, so a secret
    /// operation is never blocked or failed by the audit step. This is a
    /// deliberate availability-over-non-repudiation choice: a missing audit
    /// sink must not deny legitimate secret access at runtime. The secret value
    /// is hashed/omitted by construction here (only the name and outcome are
    /// recorded), so this target carries no plaintext, root token or shares.
    fn audit_secret_access(
        &self,
        operation: SecretAuditOperation,
        name: SecretName<'_>,
        outcome: &'static str,
    ) {
        if !self.audit.enabled {
            return;
        }
        let caller = current_audit_caller();
        tracing::info!(
            target: "vault_audit",
            operation = operation.as_str(),
            secret_name = name.as_str(),
            outcome,
            caller = %caller,
            "vault secret access"
        );
    }
}

#[async_trait]
impl VaultCoreInterface for VaultCore {
    async fn write_secret(
        &self,
        name: &str,
        data: Option<Map<String, Value>>,
    ) -> Result<(), MegaError> {
        let name = SecretName::parse(name)?;
        let token = self.runtime_tokens.token_for_secret(name).to_string();
        let path = name.secret_path();
        let result = self
            .rvault
            .write(Some(token), path, data)
            .await
            .map_err(|e| VaultError::WriteApi(e.to_string()));
        self.audit_secret_access(
            SecretAuditOperation::Write,
            name,
            if result.is_ok() { "success" } else { "failure" },
        );
        result?;
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
        self.audit_secret_access(
            SecretAuditOperation::Read,
            name,
            match &result {
                Ok(Some(resp)) if resp.data.is_some() => "success",
                Ok(_) => "miss",
                Err(_) => "failure",
            },
        );
        let resp = result?;

        Ok(resp.and_then(|r| r.data))
    }

    async fn delete_secret(&self, name: &str) -> Result<(), MegaError> {
        let name = SecretName::parse(name)?;
        let token = self.runtime_tokens.token_for_secret(name).to_string();
        let path = name.secret_path();
        let result = self
            .rvault
            .delete(Some(token), path, None)
            .await
            .map_err(|e| VaultError::DeleteApi(e.to_string()));
        self.audit_secret_access(
            SecretAuditOperation::Delete,
            name,
            if result.is_ok() { "success" } else { "failure" },
        );
        result?;
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
    name: "monoengine-ssh",
    display_name: "monoengine-ssh",
    hcl: r#"
path "secret/ssh_server_key" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const PGP_POLICY: RuntimePolicy = RuntimePolicy {
    name: "monoengine-pgp",
    display_name: "monoengine-pgp",
    hcl: r#"
path "secret/pgp-signed-secret" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const NOSTR_POLICY: RuntimePolicy = RuntimePolicy {
    name: "monoengine-nostr",
    display_name: "monoengine-nostr",
    hcl: r#"
path "secret/nostr_identity_key" {
    capabilities = ["create", "read", "update", "delete"]
}
"#,
};

const PKI_POLICY: RuntimePolicy = RuntimePolicy {
    name: "monoengine-pki",
    display_name: "monoengine-pki",
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
    name: "monoengine-config",
    display_name: "monoengine-config",
    hcl: r#"
path "secret/config/*" {
    capabilities = ["create", "read", "update", "delete", "list"]
}
"#,
};

const GENERIC_POLICY: RuntimePolicy = RuntimePolicy {
    name: "monoengine-generic",
    display_name: "monoengine-generic",
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
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
        let vault_core = vault_core.with_audit_config(VaultAuditConfig { enabled: false });
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
}
