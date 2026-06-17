use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    common::{
        config::{DbConfig, mega_base},
        errors::MegaError,
    },
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
}

pub type VaultResult<T> = Result<T, VaultError>;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("failed to create vault directory at {path}: {source}")]
    DirectoryCreate {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to restrict vault directory permissions at {path}: {source}")]
    DirectoryPermissions {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "vault core key file is missing at {path}; restore key material or run an explicit reset"
    )]
    CoreKeyMissing { path: PathBuf },
    #[error("vault core key file exists at {path}, but vault storage is not initialized")]
    CoreKeyExistsWithoutInitializedStorage { path: PathBuf },
    #[error("failed to read vault core key file at {path}: {source}")]
    CoreKeyRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse vault core key file at {path}: {source}")]
    CoreKeyDeserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to create vault core key file at {path}: {source}")]
    CoreKeyWrite {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize vault core key file at {path}: {source}")]
    CoreKeySerialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("vault core key file contains {actual} shares, but {expected} are required")]
    CoreKeyTooFewShares { expected: usize, actual: usize },
    #[error("failed to create RustyVault instance: {0}")]
    RustyVaultCreate(String),
    #[error("failed to inspect vault initialization state: {0}")]
    InitializationState(String),
    #[error("failed to initialize vault core: {0}")]
    Initialize(String),
    #[error("failed to unseal vault core: {0}")]
    Unseal(String),
    #[error("failed to rekey vault unseal shares: {0}")]
    Rekey(String),
    #[error("vault root token is required to create missing runtime credentials")]
    RootTokenRequiredForRuntimeCredentials,
    #[error("failed to write vault runtime policy {policy}: {message}")]
    RuntimePolicyWrite { policy: String, message: String },
    #[error("failed to create vault runtime token for policy {policy}: {message}")]
    RuntimeTokenCreate { policy: String, message: String },
    #[error("vault runtime token response for policy {policy} did not include a client token")]
    RuntimeTokenMissing { policy: String },
    #[error("failed to ensure vault pki mount: {0}")]
    PkiMount(String),
    #[error("failed to revoke initialized vault root token")]
    RootTokenRevoke,
    #[error("invalid vault secret name")]
    InvalidSecretName,
    #[error("failed to read from vault API: {0}")]
    ReadApi(String),
    #[error("failed to write to vault API: {0}")]
    WriteApi(String),
    #[error("failed to delete from vault API: {0}")]
    DeleteApi(String),
}

impl From<VaultError> for MegaError {
    fn from(err: VaultError) -> Self {
        MegaError::Other(err.to_string())
    }
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
        let connection = database_connection(db_config).await;
        Self::from_database_connection(Arc::new(connection), key_path).await
    }

    pub async fn from_database_connection(
        connection: Arc<DatabaseConnection>,
        key_path: PathBuf,
    ) -> VaultResult<Self> {
        let base = BaseStorage::new(connection);
        Self::config(VaultStorage { base }, key_path).await
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
        })
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

    fn audit_secret_access(
        &self,
        operation: SecretAuditOperation,
        name: SecretName<'_>,
        outcome: &'static str,
    ) {
        tracing::info!(
            target: "vault_audit",
            operation = operation.as_str(),
            secret_name = name.as_str(),
            outcome,
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
    use crate::{
        common::config::DbConfig,
        jupiter::{
            migration::apply_migrations,
            tests::{test_db_connection, test_storage},
        },
    };

    async fn test_vault_storage(temp_dir: &Path) -> VaultStorage {
        let connection = Arc::new(test_db_connection(temp_dir).await);
        apply_migrations(&connection, true).await.unwrap();
        VaultStorage {
            base: BaseStorage::new(connection),
        }
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
    async fn test_vault_core_from_database_config_does_not_need_full_storage() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temporary directory");
        let db_path = temp_dir.path().join("bootstrap.db");
        let db_url = format!("sqlite://{}", db_path.to_string_lossy());
        let db_config = DbConfig {
            db_type: "sqlite".to_string(),
            db_path,
            db_url,
            max_connection: 5,
            min_connection: 1,
            acquire_timeout: 5,
            connect_timeout: 5,
            sqlx_logging: false,
        };
        let key_path = temp_dir.path().join(CORE_KEY_FILE);

        let vault_core = VaultCore::from_database_config(&db_config, key_path)
            .await
            .expect("vault core should initialize from DB-only bootstrap");

        assert!(vault_core.rvault.core.load().inited().await.unwrap());
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
