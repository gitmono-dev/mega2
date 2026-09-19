use std::sync::{Arc, Mutex, OnceLock};

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use serde_json::{Map, Value};

use crate::{
    common::errors::MegaError,
    config::{GithubSyncConfig, secret::SecretRef, validate::parse_secret_ref_for_field},
    contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface, with_audit_caller},
    jupiter::redis::{ConnectionManager, lock::RedLock},
};

const FIELD: &str = "github_sync.ssh_key_ref";
const INIT_LOCK_KEY: &str = "mega2:github_sync:ssh_key:init";
const INIT_LOCK_TTL_MS: u64 = 30_000;

/// Process-local hold for the GitHub-sync SSH private key (plan-20260916 GS-05).
#[derive(Clone)]
pub struct GithubSyncKey {
    inner: PrivateKey,
}

impl GithubSyncKey {
    fn from_openssh(openssh: &str) -> Result<Self, MegaError> {
        let inner = PrivateKey::from_openssh(openssh).map_err(|_| {
            MegaError::Other(
                "github_sync ssh key is not a valid OpenSSH Ed25519 private key".to_string(),
            )
        })?;
        if inner.algorithm() != Algorithm::Ed25519 {
            return Err(MegaError::Other(
                "github_sync ssh key is not a valid OpenSSH Ed25519 private key".to_string(),
            ));
        }
        Ok(Self { inner })
    }

    pub fn public_openssh(&self) -> Result<String, MegaError> {
        self.inner.public_key().to_openssh().map_err(|_| {
            MegaError::Other("github_sync ssh public key could not be encoded".to_string())
        })
    }
}

fn holder() -> &'static Mutex<Option<GithubSyncKey>> {
    static HOLDER: OnceLock<Mutex<Option<GithubSyncKey>>> = OnceLock::new();
    HOLDER.get_or_init(|| Mutex::new(None))
}

fn install(key: GithubSyncKey) -> GithubSyncKey {
    *holder().lock().expect("github_sync key holder") = Some(key.clone());
    key
}

pub fn held() -> Option<GithubSyncKey> {
    holder().lock().expect("github_sync key holder").clone()
}

#[cfg(test)]
fn clear_held() {
    *holder().lock().expect("github_sync key holder") = None;
}

/// Load or generate the GitHub-sync Ed25519 key and install the process hold.
///
/// First generation is mutually excluded across replicas with a RedLock; the
/// vault value is re-read inside the lock so concurrent initializers converge
/// on the single winner's key (plan-20260916 GS-16).
pub async fn ensure(
    config: &GithubSyncConfig,
    vault: &VaultCore,
    redis: ConnectionManager,
) -> Result<Option<GithubSyncKey>, MegaError> {
    with_audit_caller("startup:github-sync", ensure_inner(config, vault, redis)).await
}

fn corrupt_key_error() -> MegaError {
    MegaError::Other("github_sync ssh key is not a valid OpenSSH Ed25519 private key".to_string())
}

async fn load_existing(
    vault: &VaultCore,
    secret_ref: &SecretRef,
) -> Result<Option<GithubSyncKey>, MegaError> {
    let existing = vault.read_secret(secret_ref.secret_name()).await?;
    let Some(data) = existing.as_ref() else {
        return Ok(None);
    };
    let Some(value) = data.get(secret_ref.field()) else {
        return Ok(None);
    };
    let Some(openssh) = value.as_str() else {
        return Err(corrupt_key_error());
    };
    Ok(Some(GithubSyncKey::from_openssh(openssh)?))
}

async fn generate_and_store(
    vault: &VaultCore,
    secret_ref: &SecretRef,
) -> Result<GithubSyncKey, MegaError> {
    let generated = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(|_| {
        MegaError::Other("github_sync failed to generate Ed25519 ssh key".to_string())
    })?;
    if generated.algorithm() != Algorithm::Ed25519 {
        return Err(MegaError::Other(
            "github_sync failed to generate Ed25519 ssh key".to_string(),
        ));
    }
    let encoded = generated.to_openssh(LineEnding::LF).map_err(|_| {
        MegaError::Other("github_sync failed to encode Ed25519 ssh key as OpenSSH".to_string())
    })?;
    let mut data = Map::new();
    data.insert(
        secret_ref.field().to_string(),
        Value::String(encoded.as_str().to_string()),
    );
    vault
        .write_secret(secret_ref.secret_name(), Some(data))
        .await?;
    Ok(install(GithubSyncKey { inner: generated }))
}

async fn ensure_inner(
    config: &GithubSyncConfig,
    vault: &VaultCore,
    redis: ConnectionManager,
) -> Result<Option<GithubSyncKey>, MegaError> {
    if !config.enabled {
        return Ok(None);
    }

    let secret_ref = parse_secret_ref_for_field(FIELD, &config.ssh_key_ref)?;
    if let Some(key) = load_existing(vault, &secret_ref).await? {
        return Ok(Some(install(key)));
    }

    let lock = Arc::new(RedLock::new(redis, INIT_LOCK_KEY, INIT_LOCK_TTL_MS));
    let guard = lock.lock().await?;
    let result = async {
        if let Some(key) = load_existing(vault, &secret_ref).await? {
            return Ok(install(key));
        }
        generate_and_store(vault, &secret_ref).await
    }
    .await;
    let unlock_result = guard.unlock().await;
    let key = result?;
    unlock_result?;
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::{
        contract::vault::integration::vault_core::VaultCore,
        jupiter::storage::{
            base_storage::{BaseStorage, StorageConnector},
            vault_storage::VaultStorage,
        },
    };

    static TEST_SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

    const KEY_REF: &str = "vault://secret/config/example/github_sync/ssh_key#value";
    const SECRET_NAME: &str = "config/example/github_sync/ssh_key";

    fn enabled_config() -> GithubSyncConfig {
        GithubSyncConfig {
            enabled: true,
            ssh_key_ref: KEY_REF.to_string(),
            ..GithubSyncConfig::default()
        }
    }

    fn disabled_config() -> GithubSyncConfig {
        GithubSyncConfig {
            enabled: false,
            ssh_key_ref: KEY_REF.to_string(),
            ..GithubSyncConfig::default()
        }
    }

    async fn test_vault() -> (tempfile::TempDir, VaultCore) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_config = crate::jupiter::tests::test_db_config(temp_dir.path()).await;
        let vault =
            VaultCore::from_database_config(&db_config, temp_dir.path().join("core_key.json"))
                .await
                .expect("vault");
        (temp_dir, vault)
    }

    async fn test_readonly_vault() -> (tempfile::TempDir, VaultCore) {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_config = crate::jupiter::tests::test_db_config(temp_dir.path()).await;
        let key_path = temp_dir.path().join("core_key.json");
        let _writable = VaultCore::from_database_config(&db_config, key_path.clone())
            .await
            .expect("writable vault");
        let connection = crate::jupiter::storage::init::database_connection(&db_config)
            .await
            .expect("readonly db");
        let storage = VaultStorage {
            base: BaseStorage::new(std::sync::Arc::new(connection)),
        };
        let vault = VaultCore::open_readonly(storage, key_path)
            .await
            .expect("readonly vault");
        (temp_dir, vault)
    }

    async fn test_redis() -> ConnectionManager {
        crate::jupiter::tests::test_redis_manager().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn ensure_initializes_once_and_reloads() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held();
        let (_temp, vault) = test_vault().await;
        let redis = test_redis().await;

        let first = ensure(&enabled_config(), &vault, redis.clone())
            .await
            .expect("generate")
            .expect("enabled hold");
        let first_pub = first.public_openssh().expect("public");
        assert!(first_pub.starts_with("ssh-ed25519 "), "{first_pub}");
        let stored = vault
            .read_secret(SECRET_NAME)
            .await
            .expect("read")
            .expect("written");
        let stored_openssh = stored
            .get("value")
            .and_then(|value| value.as_str())
            .expect("field")
            .to_string();
        assert!(
            stored_openssh.contains("BEGIN OPENSSH PRIVATE KEY"),
            "vault must store OpenSSH private key"
        );
        assert_eq!(
            held()
                .expect("holder")
                .public_openssh()
                .expect("holder public"),
            first_pub
        );

        let second = ensure(&enabled_config(), &vault, redis)
            .await
            .expect("reload")
            .expect("enabled hold");
        assert_eq!(second.public_openssh().expect("public"), first_pub);
        let stored_again = vault
            .read_secret(SECRET_NAME)
            .await
            .expect("reread")
            .expect("still present");
        assert_eq!(stored, stored_again, "load path must not overwrite vault");
        assert_eq!(
            held()
                .expect("holder after reload")
                .public_openssh()
                .expect("holder public"),
            first_pub
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn disabled_does_not_generate_or_write() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held();
        let (_temp, vault) = test_vault().await;

        let mut marker = Map::new();
        marker.insert("value".to_string(), Value::String("leave-me".to_string()));
        vault
            .write_secret(SECRET_NAME, Some(marker))
            .await
            .expect("seed");

        let redis = test_redis().await;
        let result = ensure(&disabled_config(), &vault, redis)
            .await
            .expect("disabled is a no-op");
        assert!(result.is_none());
        assert!(held().is_none());
        let stored = vault
            .read_secret(SECRET_NAME)
            .await
            .expect("read")
            .expect("untouched");
        assert_eq!(
            stored.get("value").and_then(|value| value.as_str()),
            Some("leave-me")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn delete_then_rebootstrap_yields_new_key() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held();
        let (_temp, vault) = test_vault().await;

        let redis = test_redis().await;
        let first = ensure(&enabled_config(), &vault, redis.clone())
            .await
            .expect("generate")
            .expect("hold");
        let first_pub = first.public_openssh().expect("public");

        vault.delete_secret(SECRET_NAME).await.expect("delete");
        clear_held();

        let second = ensure(&enabled_config(), &vault, redis)
            .await
            .expect("regenerate")
            .expect("hold");
        let second_pub = second.public_openssh().expect("public");
        assert_ne!(first_pub, second_pub);
        assert!(second_pub.starts_with("ssh-ed25519 "), "{second_pub}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_bootstrap_converges() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held();
        let (_temp, vault) = test_vault().await;
        let redis = test_redis().await;

        let left = {
            let config = enabled_config();
            let vault = vault.clone();
            let redis = redis.clone();
            tokio::spawn(async move { ensure(&config, &vault, redis).await })
        };
        let right = {
            let config = enabled_config();
            let vault = vault.clone();
            tokio::spawn(async move { ensure(&config, &vault, redis).await })
        };
        let left = left.await.expect("left join").expect("left ensure");
        let right = right.await.expect("right join").expect("right ensure");
        let left_pub = left.expect("left hold").public_openssh().expect("left pub");
        let right_pub = right
            .expect("right hold")
            .public_openssh()
            .expect("right pub");
        assert_eq!(left_pub, right_pub);
        assert!(left_pub.starts_with("ssh-ed25519 "), "{left_pub}");
        let stored = vault
            .read_secret(SECRET_NAME)
            .await
            .expect("read")
            .expect("written");
        let stored_key = GithubSyncKey::from_openssh(
            stored
                .get("value")
                .and_then(|value| value.as_str())
                .expect("field"),
        )
        .expect("stored openssh");
        assert_eq!(stored_key.public_openssh().expect("stored pub"), left_pub);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn fails_closed_on_vault_error_and_corrupt_value() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held();
        let redis = test_redis().await;

        let (_temp, vault) = test_vault().await;
        let mut corrupt = Map::new();
        corrupt.insert("value".to_string(), Value::String("not-a-key".to_string()));
        vault
            .write_secret(SECRET_NAME, Some(corrupt))
            .await
            .expect("seed corrupt");
        assert!(
            ensure(&enabled_config(), &vault, redis.clone())
                .await
                .is_err(),
            "corrupt vault value must fail closed"
        );
        assert!(held().is_none(), "corrupt value must not install a hold");
        let stored = vault
            .read_secret(SECRET_NAME)
            .await
            .expect("reread")
            .expect("still present");
        assert_eq!(
            stored.get("value").and_then(|value| value.as_str()),
            Some("not-a-key"),
            "corrupt value must not be overwritten"
        );

        clear_held();
        let (_temp_ro, readonly) = test_readonly_vault().await;
        assert!(
            ensure(&enabled_config(), &readonly, redis).await.is_err(),
            "unwritable vault must fail closed"
        );
        assert!(
            held().is_none(),
            "vault error must not install an empty hold"
        );
    }
}
