use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use clap::Args;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use russh::{
    Preferred,
    keys::{Algorithm, PrivateKey},
    server::Server,
};
use tokio::sync::Mutex;

use crate::{
    ceres::api_service::{cache::GitObjectCache, state::ProtocolApiState},
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    contract::{git_protocol::ssh::SshServer, vault::integration::vault_core::VaultCoreInterface},
    server::CommonHttpOptions,
};

#[derive(Args, Clone, Debug)]
pub struct SshOptions {
    #[clap(flatten)]
    pub common: CommonHttpOptions,

    #[clap(flatten)]
    pub custom: SshCustom,
}

#[derive(Args, Clone, Debug)]
pub struct SshCustom {
    #[arg(long, default_value_t = 2222)]
    ssh_port: u16,
}

/// start an ssh server
pub async fn start_server(ctx: AppContext, command: &SshOptions) -> MegaResult {
    // we need to persist the key to prevent key expired after server restart.
    let p_key = load_key(ctx.clone()).await?;
    let ru_config = russh::server::Config {
        auth_rejection_time: std::time::Duration::from_secs(3),
        keys: vec![p_key],
        preferred: Preferred {
            // key: Cow::Borrowed(&[CERT_ECDSA_SHA2_P256]),
            ..Preferred::default()
        },
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        ..Default::default()
    };

    let ru_config = Arc::new(ru_config);

    let SshOptions {
        common: CommonHttpOptions { host, .. },
        custom: SshCustom { ssh_port },
    } = command;

    let state = ProtocolApiState {
        storage: ctx.storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ctx.connection.clone(),
            prefix: std::env::var("MEGA_GIT_OBJECT_CACHE_PREFIX")
                .unwrap_or_else(|_| "git-object-rkyv:v1".to_string()),
        }),
        entity_store: ctx.storage.entity_store(),
    };
    let mut ssh_server = SshServer {
        clients: Arc::new(Mutex::new(HashMap::new())),
        state,
        id: 0,
        channels: HashMap::new(),
        v2_channels: HashMap::new(),
        authenticated_user: None,
    };
    let server_url = format!("{host}:{ssh_port}");
    let addr = SocketAddr::from_str(&server_url)
        .map_err(|e| MegaError::Other(format!("Invalid SSH listen address {server_url}: {e}")))?;
    ssh_server
        .run_on_address(ru_config, addr)
        .await
        .map_err(|e| MegaError::Other(format!("SSH server failed: {e}")))?;
    Ok(())
}

pub async fn load_key(ctx: AppContext) -> Result<PrivateKey, MegaError> {
    let ssh_key = ctx.vault.read_secret("ssh_server_key").await?;
    if let Some(ssh_key) = ssh_key {
        let secret_key = ssh_key
            .get("secret_key")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other("Vault secret ssh_server_key is missing secret_key".to_string())
            })?;
        PrivateKey::from_openssh(secret_key).map_err(|e| {
            MegaError::Other(format!(
                "Vault secret ssh_server_key contains an invalid OpenSSH private key: {e}"
            ))
        })
    } else {
        // Generate a keypair if not present in the vault.
        //
        // `rand::rng()` returns rand 0.10's thread-local CSPRNG (ChaCha, OS-seeded),
        // which implements `CryptoRng` — the bound required by `ssh-key` 0.7's
        // `PrivateKey::random`. This matches the pattern used by russh's own tests
        // and avoids the `OsRng.unwrap_err()` dance that the new `TryRngCore`-only
        // `rand_core::OsRng` would otherwise force on us.
        let keys = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .map_err(|e| MegaError::Other(format!("Failed to generate SSH server key: {e}")))?;
        let encoded_key = keys.to_openssh(LineEnding::CR).map_err(|e| {
            MegaError::Other(format!("Failed to encode SSH server key as OpenSSH: {e}"))
        })?;
        let secret = serde_json::json!({
            "secret_key": encoded_key.as_str(),
        })
        .as_object()
        .ok_or_else(|| MegaError::Other("Failed to build SSH server key secret".to_string()))?
        .clone();

        ctx.vault
            .write_secret("ssh_server_key", Some(secret))
            .await?;
        Ok(keys)
    }
}
