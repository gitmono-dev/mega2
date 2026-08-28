//! Server-side GPG signing for synthetic commits that enter a CL chain
//! (plan-20260827 MC-09, REL-MC-02).
//!
//! Two construction sites produce commits that land inside a CL's
//! `from_hash → to_hash` range and are therefore verified per commit by the
//! merge checker (MC-02): the `update_branch` rebase chain and the buck
//! upload chain. Those commits must carry a server GPG signature.
//!
//! Key material lives in the vault under a versioned layout (private keys
//! never leave it):
//!
//! - `server-signing/keys/<key-id>` — one secret per key generation
//!   (`pub_key` / `sec_key` armored, `created_at`, `revoked` flag); the
//!   `key-id` is the key's fingerprint in the same lowercase-hex `{:?}`
//!   format `gpg_key.fingerprint` uses.
//! - `server-signing/index` — append-only list of all key ids ever created.
//! - `server-signing/active` — pointer to the key id used for signing.
//!
//! Rotation is append-only (keys are never deleted or rewritten): a new
//! generation is added and the `active` pointer moves; every historical
//! public key stays listed for the verifier side (MC-11) unless explicitly
//! revoked. First-time initialization is mutually excluded across replicas
//! with the existing RedLock facility, re-reading the pointer inside the
//! lock before generating.
//!
//! ## Layout
//!
//! - Key lifecycle: `ensure_server_signing_key` / `rotate_server_signing_key`
//!   / `list_server_signing_public_keys` on `VaultCore` (vault layout above;
//!   a revoked `active` generation is rejected fail-closed — rotate first).
//! - Commit format: `canonical_commit_payload` (the ADR-MC-09 signing-side
//!   single source of truth) and `embed_gpgsig` (git-compatible `gpgsig`
//!   header embedding).
//! - Signing: `ServerSigningContext`, the crate-internal capability handed to
//!   the two synthetic-commit sites (`update_branch` chain, buck upload
//!   chain); the secret key never leaves this module.
//! - Tests: key versioning + rotation history, cross-generation
//!   verification, revoked-active fail-closed, the RedLock initialization
//!   race, and the `git verify-commit` interop proof.

use std::sync::Arc;

use git_internal::internal::object::{
    commit::Commit,
    signature::{Signature, SignatureType},
};
use pgp::{
    composed::{
        ArmorOptions, Deserializable, DetachedSignature, KeyType, SecretKeyParamsBuilder,
        SignedPublicKey, SignedSecretKey,
    },
    crypto::hash::HashAlgorithm,
    types::{KeyDetails, KeyVersion, Password},
};
use redis::aio::ConnectionManager;
use serde_json::{Map, Value, json};

use crate::{
    common::errors::MegaError,
    contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface},
    jupiter::redis::lock::RedLock,
};

/// Reserved server identity stamped onto synthetic commits as both author
/// and committer (MC-09 AC). This is the single source of truth: MC-11
/// routes commit verification to the server keyring by comparing the
/// committer header against these constants.
pub const SERVER_SIGNING_NAME: &str = "monoengine-server";
pub const SERVER_SIGNING_EMAIL: &str = "server-signing@monoengine.internal";

const KEYS_PREFIX: &str = "server-signing/keys";
const KEY_INDEX: &str = "server-signing/index";
const ACTIVE_POINTER: &str = "server-signing/active";
const INIT_LOCK_KEY: &str = "monoengine:server-signing:init";
const INIT_LOCK_TTL_MS: u64 = 30_000;

/// The active server signing key pair, loaded from the vault.
///
/// Reachability is deliberately narrow: the secret key is module-private,
/// and the pair itself is handed out only crate-internally (the two
/// synthetic-commit sites). The verifier side (MC-11) consumes
/// [`VaultCore::list_server_signing_public_keys`] instead — public keys only.
pub struct ServerSigningKey {
    /// Fingerprint in the `gpg_key.fingerprint` storage format.
    pub(crate) key_id: String,
    pub(crate) public_key: SignedPublicKey,
    secret_key: SignedSecretKey,
}

/// A non-revoked historical public key, for the MC-11 verifier keyring.
pub struct ServerPublicKey {
    pub key_id: String,
    pub public_key: SignedPublicKey,
}

/// The reserved server identity as a git signature line (`Signature::to_data`
/// output carries the `author `/`committer ` prefix).
pub fn server_identity_signature(sig_type: SignatureType) -> Signature {
    Signature::new(
        sig_type,
        SERVER_SIGNING_NAME.to_string(),
        SERVER_SIGNING_EMAIL.to_string(),
    )
}

/// The canonical payload a commit signature covers (ADR-MC-09, signing-side
/// single source of truth): the full commit byte stream — tree, parents in
/// order, author, committer, blank line, message — normalized to end with
/// exactly one newline. The verification side (MC-02
/// `rebuild_canonical_commit_bytes` + `extract_from_commit_content`)
/// reconstructs these exact bytes from the persisted columns.
pub fn canonical_commit_payload(
    commit: &Commit,
    author: &Signature,
    committer: &Signature,
) -> Result<Vec<u8>, MegaError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"tree ");
    payload.extend_from_slice(commit.tree_id.to_string().as_bytes());
    payload.push(b'\n');
    for parent in &commit.parent_commit_ids {
        payload.extend_from_slice(b"parent ");
        payload.extend_from_slice(parent.to_string().as_bytes());
        payload.push(b'\n');
    }
    payload.extend_from_slice(
        &author
            .to_data()
            .map_err(|e| MegaError::Other(format!("failed to encode author signature: {e}")))?,
    );
    payload.push(b'\n');
    payload.extend_from_slice(
        &committer
            .to_data()
            .map_err(|e| MegaError::Other(format!("failed to encode committer signature: {e}")))?,
    );
    payload.push(b'\n');
    payload.push(b'\n');
    payload.extend_from_slice(commit.message.as_bytes());
    if !payload.ends_with(b"\n") {
        payload.push(b'\n');
    }
    Ok(payload)
}

/// Embed an armored detached signature into a commit message the way git
/// does: a `gpgsig ` header line followed by space-prefixed continuation
/// lines, then the blank header/body separator and the original message.
fn embed_gpgsig(message: &str, armored: &str) -> String {
    let mut out = String::from("gpgsig ");
    for (i, line) in armored.trim_end_matches('\n').lines().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(message);
    out
}

/// Signing capability handed to the synthetic-commit construction sites.
/// Holds cheap clones only (`VaultCore` and `ConnectionManager` are
/// Arc-backed). Crate-internal: it exposes only the signing operations —
/// never the secret key.
#[derive(Clone)]
pub struct ServerSigningContext {
    vault: VaultCore,
    redis: ConnectionManager,
}

impl ServerSigningContext {
    pub(crate) fn new(vault: VaultCore, redis: ConnectionManager) -> Self {
        Self { vault, redis }
    }

    /// Signing precheck: vault read + key parse (plus RedLock-guarded first
    /// initialization). Call sites must run this before producing any
    /// commit/ref output so a failure leaves zero persisted side effects.
    pub(crate) async fn active_key(&self) -> Result<ServerSigningKey, MegaError> {
        self.vault
            .ensure_server_signing_key(self.redis.clone())
            .await
    }

    /// Sign a synthetic commit: stamp the reserved server identity as
    /// author/committer, detach-sign the canonical payload with the active
    /// key, embed the armor as a `gpgsig` header, and rebuild the commit so
    /// its id is computed over the signed bytes (`Commit::new` hashes
    /// `to_data()` once).
    pub(crate) fn sign_commit(
        &self,
        key: &ServerSigningKey,
        commit: &Commit,
    ) -> Result<Commit, MegaError> {
        let author = server_identity_signature(SignatureType::Author);
        let committer = server_identity_signature(SignatureType::Committer);
        let payload = canonical_commit_payload(commit, &author, &committer)?;
        let armor = DetachedSignature::sign_binary_data(
            rand08::thread_rng(),
            &key.secret_key.primary_key,
            &Password::empty(),
            HashAlgorithm::Sha256,
            payload.as_slice(),
        )
        .map_err(|e| MegaError::Other(format!("failed to sign commit payload: {e}")))?
        .to_armored_string(ArmorOptions::default())
        .map_err(|e| MegaError::Other(format!("failed to armor commit signature: {e}")))?;
        let mut message = embed_gpgsig(&commit.message, &armor);
        // Git verifies the byte-exact payload (no normalization), so the
        // stored commit must already carry the canonical trailing newline.
        if !message.ends_with('\n') {
            message.push('\n');
        }
        Ok(Commit::new(
            author,
            committer,
            commit.tree_id,
            commit.parent_commit_ids.clone(),
            &message,
        ))
    }
}

impl VaultCore {
    /// Load the active server signing key, or initialize it on first use.
    ///
    /// First initialization is mutually excluded across replicas with a
    /// RedLock; the pointer is re-read inside the lock so concurrent
    /// initializers converge on the single winner's key.
    pub async fn ensure_server_signing_key(
        &self,
        redis: ConnectionManager,
    ) -> Result<ServerSigningKey, MegaError> {
        if let Some(key) = self.load_active_server_signing_key().await? {
            return Ok(key);
        }

        let lock = Arc::new(RedLock::new(redis, INIT_LOCK_KEY, INIT_LOCK_TTL_MS));
        let guard = lock.lock().await?;
        let result: Result<ServerSigningKey, MegaError> = async {
            // Re-read inside the lock: another replica may have initialized
            // while we waited.
            match self.load_active_server_signing_key().await? {
                Some(key) => Ok(key),
                None => {
                    let key = self.generate_server_signing_key()?;
                    self.persist_new_key_generation(&key, true).await?;
                    tracing::info!(key_id = %key.key_id, "initialized server signing key");
                    Ok(key)
                }
            }
        }
        .await;
        let unlock_result = guard.unlock().await;
        let key = result?;
        unlock_result?;
        Ok(key)
    }

    /// Manual rotation (operator-driven; automation is out of scope):
    /// generate a new key generation, append it to the index, and move the
    /// active pointer. Historical keys are kept — never deleted, never
    /// rewritten — so existing signed commits stay verifiable.
    pub async fn rotate_server_signing_key(
        &self,
        redis: ConnectionManager,
    ) -> Result<ServerSigningKey, MegaError> {
        let lock = Arc::new(RedLock::new(redis, INIT_LOCK_KEY, INIT_LOCK_TTL_MS));
        let guard = lock.lock().await?;
        let result: Result<ServerSigningKey, MegaError> = async {
            let key = self.generate_server_signing_key()?;
            self.persist_new_key_generation(&key, true).await?;
            tracing::info!(key_id = %key.key_id, "rotated server signing key");
            Ok(key)
        }
        .await;
        let unlock_result = guard.unlock().await;
        let key = result?;
        unlock_result?;
        Ok(key)
    }

    /// Read-only listing of every non-revoked historical public key, for the
    /// verifier side (MC-11). Rotation is append-only, so this set only
    /// grows.
    pub async fn list_server_signing_public_keys(&self) -> Result<Vec<ServerPublicKey>, MegaError> {
        let key_ids = self.read_key_index().await?;
        let mut out = Vec::with_capacity(key_ids.len());
        for key_id in key_ids {
            let entry = self.read_secret(&format!("{KEYS_PREFIX}/{key_id}")).await?;
            let Some(entry) = entry else {
                return Err(MegaError::Other(format!(
                    "server signing key {key_id} is listed in the index but missing"
                )));
            };
            if entry.get("revoked").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let pub_armored = entry
                .get("pub_key")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    MegaError::Other(format!(
                        "server signing key {key_id} is missing its public key"
                    ))
                })?;
            let (public_key, _) = SignedPublicKey::from_string(pub_armored).map_err(|e| {
                MegaError::Other(format!(
                    "failed to parse server signing public key {key_id}: {e}"
                ))
            })?;
            public_key.verify_bindings().map_err(|e| {
                MegaError::Other(format!("invalid server signing public key {key_id}: {e}"))
            })?;
            out.push(ServerPublicKey { key_id, public_key });
        }
        Ok(out)
    }

    async fn load_active_server_signing_key(&self) -> Result<Option<ServerSigningKey>, MegaError> {
        let pointer = self.read_secret(ACTIVE_POINTER).await?;
        let Some(pointer) = pointer else {
            return Ok(None);
        };
        let key_id = pointer
            .get("key_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other(format!("Vault secret {ACTIVE_POINTER} is missing key_id"))
            })?;
        let entry = self
            .read_secret(&format!("{KEYS_PREFIX}/{key_id}"))
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "server signing active pointer references missing key {key_id}"
                ))
            })?;
        // Fail closed on a revoked active generation: the verifier side
        // skips revoked keys, so signing with one would produce commits no
        // keyring can verify. Rotate first, then revoke the old generation.
        if entry.get("revoked").and_then(Value::as_bool) == Some(true) {
            return Err(MegaError::Other(format!(
                "server signing key {key_id} is revoked; rotate to a new generation first \
                 (VaultCore::rotate_server_signing_key)"
            )));
        }
        let pub_armored = entry
            .get("pub_key")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other(format!("server signing key {key_id} is missing pub_key"))
            })?;
        let sec_armored = entry
            .get("sec_key")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                MegaError::Other(format!("server signing key {key_id} is missing sec_key"))
            })?;
        let (public_key, _) = SignedPublicKey::from_string(pub_armored).map_err(|e| {
            MegaError::Other(format!(
                "failed to parse server signing key {key_id} pub_key: {e}"
            ))
        })?;
        public_key.verify_bindings().map_err(|e| {
            MegaError::Other(format!("invalid server signing key {key_id} pub_key: {e}"))
        })?;
        let (secret_key, _) = SignedSecretKey::from_string(sec_armored).map_err(|e| {
            MegaError::Other(format!(
                "failed to parse server signing key {key_id} sec_key: {e}"
            ))
        })?;
        secret_key.verify_bindings().map_err(|e| {
            MegaError::Other(format!("invalid server signing key {key_id} sec_key: {e}"))
        })?;
        Ok(Some(ServerSigningKey {
            key_id: key_id.to_string(),
            public_key,
            secret_key,
        }))
    }

    fn generate_server_signing_key(&self) -> Result<ServerSigningKey, MegaError> {
        let mut params = SecretKeyParamsBuilder::default();
        params
            .version(KeyVersion::V4)
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(format!("{SERVER_SIGNING_NAME} <{SERVER_SIGNING_EMAIL}>"));
        let secret_key = params
            .build()
            .map_err(|e| MegaError::Other(format!("failed to build signing key params: {e}")))?
            .generate(rand08::thread_rng())
            .map_err(|e| MegaError::Other(format!("failed to generate server signing key: {e}")))?;
        let public_key = SignedPublicKey::from(secret_key.clone());
        let key_id = format!("{:?}", public_key.fingerprint());
        Ok(ServerSigningKey {
            key_id,
            public_key,
            secret_key,
        })
    }

    /// Persist a freshly generated key generation: write the keyed secret,
    /// append the id to the index (append-only), and — when `make_active` —
    /// move the active pointer.
    async fn persist_new_key_generation(
        &self,
        key: &ServerSigningKey,
        make_active: bool,
    ) -> Result<(), MegaError> {
        let pub_armored = key
            .public_key
            .to_armored_string(ArmorOptions::default())
            .map_err(|e| MegaError::Other(format!("failed to encode public key: {e}")))?;
        let sec_armored = key
            .secret_key
            .to_armored_string(ArmorOptions::default())
            .map_err(|e| MegaError::Other(format!("failed to encode secret key: {e}")))?;

        let entry = json!({
            "pub_key": pub_armored,
            "sec_key": sec_armored,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "revoked": false,
        })
        .as_object()
        .cloned()
        .ok_or_else(|| MegaError::Other("failed to build key entry secret".to_string()))?;
        self.write_secret(&format!("{KEYS_PREFIX}/{}", key.key_id), Some(entry))
            .await?;

        let mut key_ids = self.read_key_index().await?;
        if !key_ids.contains(&key.key_id) {
            key_ids.push(key.key_id.clone());
        }
        let mut index = Map::new();
        index.insert(
            "key_ids".to_string(),
            Value::Array(key_ids.into_iter().map(Value::String).collect()),
        );
        self.write_secret(KEY_INDEX, Some(index)).await?;

        if make_active {
            let mut pointer = Map::new();
            pointer.insert("key_id".to_string(), Value::String(key.key_id.clone()));
            self.write_secret(ACTIVE_POINTER, Some(pointer)).await?;
        }
        Ok(())
    }

    async fn read_key_index(&self) -> Result<Vec<String>, MegaError> {
        let Some(index) = self.read_secret(KEY_INDEX).await? else {
            return Ok(Vec::new());
        };
        let key_ids = index
            .get("key_ids")
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                MegaError::Other(format!("Vault secret {KEY_INDEX} is missing key_ids"))
            })?;
        key_ids
            .iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    MegaError::Other(format!("Vault secret {KEY_INDEX} has a non-string key id"))
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use redis::aio::ConnectionManager;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        contract::vault::integration::vault_core::VaultCore,
        jupiter::{
            migration::apply_migrations,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                vault_storage::VaultStorage,
            },
            tests::test_db_connection,
        },
    };

    fn test_redis_url() -> String {
        std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string())
    }

    async fn test_redis() -> ConnectionManager {
        let client = redis::Client::open(test_redis_url()).expect("redis client");
        ConnectionManager::new(client)
            .await
            .expect("redis connection")
    }

    async fn test_vault(temp: &TempDir) -> VaultCore {
        let conn = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(&conn, true).await.expect("migrations");
        VaultCore::config(
            VaultStorage {
                base: BaseStorage::new(conn),
            },
            temp.path().join("core_key.json"),
        )
        .await
        .expect("vault core should initialize")
    }

    // AC①/AC②: first initialization generates and persists one key
    // generation; repeated ensures reload the same active key.
    #[tokio::test]
    async fn ensure_initializes_once_and_reloads_active_key() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;

        let first = vault
            .ensure_server_signing_key(redis.clone())
            .await
            .expect("first init");
        let second = vault
            .ensure_server_signing_key(redis)
            .await
            .expect("second ensure reloads");
        assert_eq!(first.key_id, second.key_id);

        let listed = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key_id, first.key_id);
    }

    // AC②: N replicas racing first initialization converge on a single
    // winner — exactly one key generation exists afterwards and every racer
    // re-read the same active key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_initialization_has_a_single_winner() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;

        let racers = 8;
        let mut tasks = Vec::with_capacity(racers);
        for _ in 0..racers {
            let vault = vault.clone();
            let redis = redis.clone();
            tasks.push(tokio::spawn(async move {
                vault.ensure_server_signing_key(redis).await
            }));
        }
        let results = futures::future::join_all(tasks).await;

        let mut winner: Option<String> = None;
        for result in results {
            let key = result
                .expect("racer task")
                .expect("racer must succeed after re-reading the winner's key");
            match &winner {
                None => winner = Some(key.key_id),
                // Every racer — winner and losers alike — must come back with
                // the single winner's key id (the in-lock re-read convergence).
                Some(w) => assert_eq!(
                    w, &key.key_id,
                    "every replica must converge on the single winner's key"
                ),
            }
        }
        let winner = winner.expect("at least one racer");

        let listed = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys");
        assert_eq!(
            listed.len(),
            1,
            "exactly one key generation must exist after the race"
        );
        assert_eq!(listed[0].key_id, winner);
    }

    // AC①: rotation is append-only — the active pointer moves to the new
    // generation, signing uses only the active key, and the historical
    // public key set only grows.
    #[tokio::test]
    async fn rotation_moves_active_and_keeps_history() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;

        let first = vault
            .ensure_server_signing_key(redis.clone())
            .await
            .expect("init");
        let second = vault
            .rotate_server_signing_key(redis.clone())
            .await
            .expect("rotate");
        assert_ne!(first.key_id, second.key_id);

        // Signing resolves only the active key.
        let active = vault
            .ensure_server_signing_key(redis)
            .await
            .expect("reload active");
        assert_eq!(active.key_id, second.key_id);

        // History is append-only: both generations stay listed.
        let listed = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys");
        let ids: Vec<&str> = listed.iter().map(|k| k.key_id.as_str()).collect();
        assert_eq!(listed.len(), 2);
        assert!(ids.contains(&first.key_id.as_str()));
        assert!(ids.contains(&second.key_id.as_str()));
    }

    // Rollback-mode data invariant (MC-09): a commit signed by a key
    // generation stays verifiable after that generation is rotated out.
    // Historical public keys are never deleted, so the MC-02 reconstruction
    // path still verifies the commit against the pre-rotation generation's
    // public key fetched from `list_server_signing_public_keys`.
    #[tokio::test]
    async fn rotated_out_generation_still_verifies_its_signed_commits() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;
        let signing = ServerSigningContext::new(vault.clone(), redis.clone());

        // Sign with the pre-rotation active generation.
        let first = signing.active_key().await.expect("init active key");
        let tree = "52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24".parse().unwrap();
        let parent = "1111111111111111111111111111111111111111".parse().unwrap();
        let unsigned = Commit::from_tree_id(tree, vec![parent], "signed before rotation");
        let signed = signing.sign_commit(&first, &unsigned).expect("sign commit");

        use crate::jupiter::utils::converter::IntoMegaModel;
        let model = signed
            .clone()
            .into_mega_model(git_internal::internal::metadata::EntryMeta::default());

        // Rotate: a new generation becomes active; the old one is history.
        let second = vault
            .rotate_server_signing_key(redis)
            .await
            .expect("rotate");
        assert_ne!(first.key_id, second.key_id);
        let active = vault
            .load_active_server_signing_key()
            .await
            .expect("load active")
            .expect("active key exists");
        assert_eq!(
            active.key_id, second.key_id,
            "signing uses only the active generation"
        );

        // The invariant: rebuild the payload from the persisted columns the
        // way the MC-02 verifier does, and verify against the *historical*
        // public key of the rotated-out generation.
        let historical = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys")
            .into_iter()
            .find(|k| k.key_id == first.key_id)
            .expect("rotated-out generation must stay listed");

        use crate::ceres::merge_checker::gpg_signature_checker::{
            extract_from_commit_content, rebuild_canonical_commit_bytes,
        };
        let raw = rebuild_canonical_commit_bytes(&model).expect("rebuild from columns");
        let (payload, armor) = extract_from_commit_content(&raw);
        let armor = armor.expect("gpgsig header extracted");
        let (sig, _) = DetachedSignature::from_string(&armor).expect("parse armor");
        sig.verify(&historical.public_key, payload.as_bytes())
            .expect("commit signed by a rotated-out generation must still verify");
    }

    // R1 P1-2: revoking the active generation fails signing closed — the
    // verifier side skips revoked keys, so signing with one would be
    // unverifiable — and rotation to a live generation restores signing.
    #[tokio::test]
    async fn revoked_active_key_fails_closed_until_rotated() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;
        let signing = ServerSigningContext::new(vault.clone(), redis.clone());
        let first = signing.active_key().await.expect("init");

        // Revoke the active generation the way the runbook prescribes:
        // rewrite its entry with the flag only — key material is retained.
        let entry_key = format!("{KEYS_PREFIX}/{}", first.key_id);
        let mut entry = vault
            .read_secret(&entry_key)
            .await
            .expect("read entry")
            .expect("entry exists");
        entry.insert("revoked".to_string(), Value::Bool(true));
        vault
            .write_secret(&entry_key, Some(entry))
            .await
            .expect("revoke active generation");

        // (match instead of `expect_err`: `ServerSigningKey` deliberately has
        // no `Debug` impl so key material can never leak through it.)
        let err = match signing.active_key().await {
            Ok(_) => panic!("a revoked active generation must fail closed"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("revoked"), "{err}");
        assert!(
            err.to_string().contains("rotate"),
            "the error must point at rotation: {err}"
        );

        // Recovery: rotate to a fresh generation; signing works again, and
        // the revoked generation leaves the verifier keyring.
        let second = vault
            .rotate_server_signing_key(redis)
            .await
            .expect("rotate");
        assert_ne!(first.key_id, second.key_id);
        let recovered = signing
            .active_key()
            .await
            .expect("signing recovers after rotation");
        assert_eq!(recovered.key_id, second.key_id);
        let listed = vault
            .list_server_signing_public_keys()
            .await
            .expect("list public keys");
        assert_eq!(listed.len(), 1, "the revoked generation is skipped");
        assert_eq!(listed[0].key_id, second.key_id);
    }

    // AC⑤/AC⑥: a signed synthetic commit carries the reserved server
    // identity and a gpgsig header, and its stored columns verify through
    // the MC-02 reconstruction path (the byte-level closure with the
    // verifier side).
    #[tokio::test]
    async fn signed_commit_round_trips_through_verifier_reconstruction() {
        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;
        let signing = ServerSigningContext::new(vault, redis);
        let key = signing.active_key().await.expect("active key");

        let tree = "52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24".parse().unwrap();
        let parent = "1111111111111111111111111111111111111111".parse().unwrap();
        let unsigned = Commit::from_tree_id(tree, vec![parent], "server synthesized commit");
        let signed = signing.sign_commit(&key, &unsigned).expect("sign commit");

        assert!(
            signed
                .message
                .starts_with("gpgsig -----BEGIN PGP SIGNATURE-----"),
            "signed commit message must embed the gpgsig header"
        );
        assert_ne!(signed.id, unsigned.id, "embedding gpgsig changes the hash");

        // The persisted columns are produced by `into_mega_model`; rebuild
        // from those columns the way the MC-02 verifier does and verify the
        // detached signature against the server public key.
        use crate::jupiter::utils::converter::IntoMegaModel;
        let model = signed
            .clone()
            .into_mega_model(git_internal::internal::metadata::EntryMeta::default());
        assert_eq!(model.commit_id, signed.id.to_string());
        assert_eq!(
            model.author.as_deref(),
            Some(String::from_utf8_lossy(&signed.author.to_data().unwrap()).as_ref())
        );
        assert!(
            model
                .author
                .as_deref()
                .unwrap_or_default()
                .contains(SERVER_SIGNING_EMAIL),
            "author must be the reserved server identity"
        );
        assert!(
            model
                .committer
                .as_deref()
                .unwrap_or_default()
                .contains(SERVER_SIGNING_EMAIL),
            "committer must be the reserved server identity"
        );

        use crate::ceres::merge_checker::gpg_signature_checker::{
            extract_from_commit_content, rebuild_canonical_commit_bytes,
        };
        let raw = rebuild_canonical_commit_bytes(&model).expect("rebuild from columns");
        let (payload, armor) = extract_from_commit_content(&raw);
        let armor = armor.expect("gpgsig header extracted");
        let (sig, _) = DetachedSignature::from_string(&armor).expect("parse armor");
        sig.verify(&key.public_key, payload.as_bytes())
            .expect("server signature verifies over the rebuilt payload");
    }

    fn git_cli_available() -> bool {
        ["git", "gpg"].iter().all(|bin| {
            std::process::Command::new(bin)
                .arg("--version")
                .output()
                .is_ok()
        })
    }

    fn run_cli(
        program: &str,
        args: &[&str],
        stdin_bytes: Option<&[u8]>,
        gnupg_home: Option<&std::path::Path>,
    ) -> std::process::Output {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };

        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(home) = gnupg_home {
            cmd.env("GNUPGHOME", home);
        }
        if let Some(bytes) = stdin_bytes {
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn cli");
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(bytes)
                .expect("write stdin");
            child.wait_with_output().expect("wait for cli")
        } else {
            cmd.output().expect("run cli")
        }
    }

    // VER③: git CLI interop — commits produced by `sign_commit` must hash to
    // the same id git computes and must verify with the real
    // `git verify-commit` against the server public key. Soft-skips when the
    // git/gpg binaries are absent (same precedent as the RedLock tests
    // skipping without a `redis-server` binary).
    #[tokio::test]
    async fn signed_commits_verify_with_git_cli() {
        if !git_cli_available() {
            eprintln!("git/gpg binaries not found; skipping git interop test");
            return;
        }

        let temp = TempDir::new().expect("temp dir");
        let vault = test_vault(&temp).await;
        let redis = test_redis().await;
        let signing = ServerSigningContext::new(vault, redis);
        let key = signing.active_key().await.expect("active key");

        // Two synthetic-commit shapes: the update_branch chain (one parent,
        // rebase message) and the buck upload chain (root commit, upload
        // message) — both through the real `sign_commit` code path.
        let tree_a = "52a266a58f2c028ad7de4dfd3a72fdf76b0d4e24".parse().unwrap();
        let parent_a = "1111111111111111111111111111111111111111".parse().unwrap();
        let commit_a = Commit::from_tree_id(tree_a, vec![parent_a], "update-branch: rebase");
        let signed_a = signing.sign_commit(&key, &commit_a).expect("sign commit A");

        let tree_b = "341e54913a3a43069f2927cc0f703e5a9f730df1".parse().unwrap();
        let commit_b = Commit::from_tree_id(tree_b, vec![], "Upload via buck push");
        let signed_b = signing.sign_commit(&key, &commit_b).expect("sign commit B");

        // Import the server public key into an isolated GNUPGHOME.
        let gnupg_home = temp.path().join("gnupg");
        std::fs::create_dir_all(&gnupg_home).expect("create gnupg home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gnupg_home, std::fs::Permissions::from_mode(0o700))
                .expect("chmod gnupg home");
        }
        let pub_key_file = temp.path().join("server-signing.pub");
        std::fs::write(
            &pub_key_file,
            key.public_key
                .to_armored_string(ArmorOptions::default())
                .expect("armor server public key"),
        )
        .expect("write public key");
        let import = run_cli(
            "gpg",
            &[
                "--batch",
                "--import",
                pub_key_file.to_str().expect("utf-8 path"),
            ],
            None,
            Some(&gnupg_home),
        );
        assert!(
            import.status.success(),
            "gpg import failed: {}",
            String::from_utf8_lossy(&import.stderr)
        );

        // A scratch repo holding the two signed commit objects.
        let repo = temp.path().join("repo");
        let init = run_cli(
            "git",
            &["init", repo.to_str().expect("utf-8 path")],
            None,
            None,
        );
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        use git_internal::internal::object::ObjectTrait;
        for (shape, signed) in [("update_branch", &signed_a), ("buck_upload", &signed_b)] {
            let bytes = signed.to_data().expect("commit bytes");

            // git's object hash must equal the id git-internal computed over
            // the same bytes — proof the signed object is a well-formed git
            // commit.
            let repo_str = repo.to_str().expect("utf-8 path").to_string();
            let hashed = run_cli(
                "git",
                &[
                    "-C",
                    &repo_str,
                    "hash-object",
                    "-t",
                    "commit",
                    "-w",
                    "--stdin",
                ],
                Some(&bytes),
                None,
            );
            assert!(
                hashed.status.success(),
                "git hash-object failed: {}",
                String::from_utf8_lossy(&hashed.stderr)
            );
            let git_sha = String::from_utf8(hashed.stdout).expect("sha output utf-8");
            let git_sha = git_sha.trim();
            assert_eq!(
                git_sha,
                signed.id.to_string(),
                "git and git-internal must compute the same commit id"
            );

            let verify = run_cli(
                "git",
                &["-C", &repo_str, "verify-commit", git_sha],
                None,
                Some(&gnupg_home),
            );
            let verify_log = String::from_utf8_lossy(&verify.stderr).to_string();
            assert!(
                verify.status.success(),
                "git verify-commit failed for {shape}: {verify_log}"
            );
            assert!(
                verify_log.contains("Good signature from"),
                "expected a Good signature line for {shape}: {verify_log}"
            );
            // Sanitized evidence for the acceptance record (VER③): shape,
            // commit id, and the gpg verdict line only.
            if let Some(line) = verify_log.lines().find(|l| l.contains("Good signature")) {
                eprintln!("VER3 {shape} commit {git_sha}: {}", line.trim());
            }
        }
    }
}
