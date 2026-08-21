//! UN-31: opening a vault without changing it.
//!
//! An audit command that reports on the running system's secrets must be able
//! to say it changed nothing. The normal bootstrap cannot make that promise:
//! unsealing plants default mounts when the table is absent, rewrites entries
//! left in an older format, writes the default ACL policies, mints a token
//! salt, and starts a thread that revokes expired leases and deletes their
//! records — all before the first read.
//!
//! Every test below is written as a pair wherever a pair is meaningful: the
//! writable open shows the repair really would happen against this exact
//! storage, and the readonly open shows it does not. A one-sided assertion
//! would pass just as happily against a fixture where there was nothing to
//! repair in the first place.

use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use libvault::{
    RustyVault,
    config::Config,
    core::SealConfig,
    errors::RvError,
    modules::auth::AuthModule,
    storage::{Backend, BackendEntry},
};
use serde_json::{Map, Value, json};

use crate::{
    common::errors::{VaultError, VaultResult},
    contract::vault::integration::{
        readonly_backend::{ReadonlyBackend, is_readonly_write_denied},
        vault_core::open_readonly_core,
    },
};

/// An in-memory physical backend with the `list` contract the barrier views
/// expect: immediate children only, directories marked by a trailing `/`.
///
/// `physical::mock::MockBackend` cannot be used — it accepts every write and
/// returns nothing — and the file backend would put this suite at the mercy of
/// a temp directory. The semantics here are copied from `physical::file`, which
/// is the backend `BarrierView::get_keys` was written against.
///
/// Note that `JupiterBackend` does *not* follow that contract: it returns full
/// keys, recursively (FIX-04). One visible consequence is that the expired-lease
/// checker restores nothing under Postgres, so the lease test below would be
/// vacuous against it — the record would survive a readonly open for the wrong
/// reason. Asserting readonly's guarantee where revocation demonstrably does
/// happen is the stronger claim, so these tests use the contract semantics.
#[derive(Default)]
struct MemoryBackend {
    entries: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryBackend {
    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        self.entries.lock().unwrap().clone()
    }

    fn keys(&self) -> Vec<String> {
        self.entries.lock().unwrap().keys().cloned().collect()
    }

    fn raw_get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    fn raw_put(&self, key: &str, value: Vec<u8>) {
        self.entries.lock().unwrap().insert(key.to_string(), value);
    }

    fn raw_delete(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }

    fn contains(&self, key: &str) -> bool {
        self.entries.lock().unwrap().contains_key(key)
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RvError> {
        let entries = self.entries.lock().unwrap();
        let mut names: Vec<String> = Vec::new();
        for key in entries.keys() {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            let name = match rest.find('/') {
                Some(idx) => format!("{}/", &rest[..idx]),
                None => rest.to_string(),
            };
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    async fn get(&self, key: &str) -> Result<Option<BackendEntry>, RvError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(key)
            .map(|value| BackendEntry {
                key: key.to_string(),
                value: value.clone(),
            }))
    }

    async fn put(&self, entry: &BackendEntry) -> Result<(), RvError> {
        self.entries
            .lock()
            .unwrap()
            .insert(entry.key.clone(), entry.value.clone());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), RvError> {
        self.entries.lock().unwrap().remove(key);
        Ok(())
    }

    async fn lock(&self, _lock_name: &str) -> Result<Box<dyn Any>, RvError> {
        Ok(Box::new(true))
    }
}

const SEAL: SealConfig = SealConfig {
    secret_shares: 1,
    secret_threshold: 1,
};

fn monitored_config() -> Config {
    // A non-zero interval, so "no monitor was started" is a statement about
    // readonly mode rather than about the configuration.
    Config {
        mounts_monitor_interval: 5,
        ..Default::default()
    }
}

/// Initialize a fresh vault over `backend` and return its unseal key.
async fn init(backend: Arc<MemoryBackend>) -> Vec<u8> {
    let vault = RustyVault::new(backend, Some(&monitored_config())).expect("create vault");
    let result = vault.init(&SEAL).await.expect("init vault");
    result.secret_shares[0].clone()
}

/// Open `backend` writably and unseal it — the control arm of every pair below.
async fn open_writable(backend: Arc<MemoryBackend>, key: &[u8]) -> RustyVault {
    let vault = RustyVault::new(backend, Some(&monitored_config())).expect("create vault");
    assert!(
        vault.unseal(&[key]).await.expect("unseal"),
        "the writable control must actually unseal"
    );
    vault
}

/// Open `backend` readonly, through the write-denying wrapper the production
/// entry point uses.
async fn open_readonly(
    backend: Arc<MemoryBackend>,
    key: &[u8],
) -> Result<(RustyVault, Arc<ReadonlyBackend>), VaultError> {
    let guarded = Arc::new(ReadonlyBackend::new(backend));
    let vault = open_readonly_core(guarded.clone(), Some(&monitored_config()), &[key]).await?;
    Ok((vault, guarded))
}

/// Whether this vault's auth module has an expiration manager installed.
///
/// The vendored library carried a direct "has the expired-lease checker
/// started?" query on its `ExpirationManager`; `libvault` has no such thing, and
/// the checker thread it starts is detached with no handle to ask. VLT-04
/// replaces this with the public
/// observation the spike settled on — `AuthModule.expiration` is written by
/// `AuthModule::init` and by nothing else, and `AuthModule::init` is the only
/// caller of `start_check_expired_lease_entries`, so an empty slot *is* the
/// statement that the worker never started.
///
/// Until then it refuses by name rather than guessing.
fn expiration_installed(_vault: &RustyVault) -> VaultResult<bool> {
    Err(VaultError::ReadonlyUnavailable)
}

/// Every fail-closed path below refuses with the same named condition: the
/// stored state is missing or in an older format, and readonly mode will not
/// repair it. `detail` says which piece, and is deliberately not asserted here
/// — the contract is the refusal, not its wording.
fn assert_state_incomplete(error: &VaultError) {
    assert!(
        matches!(error, VaultError::ReadonlyStateIncomplete { .. }),
        "expected a named readonly fail-closed, got: {error}"
    );
}

fn secret_data(value: &str) -> Option<Map<String, Value>> {
    match json!({ "value": value }) {
        Value::Object(map) => Some(map),
        _ => unreachable!(),
    }
}

/// A vault opened readonly serves the secrets a writable open stored.
///
/// Refusing to write is only useful if reading still works; without this the
/// rest of the suite would be satisfied by a mode that fails at everything.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_a_readonly_open_reads_what_the_writable_one_stored() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;

    let writable = open_writable(backend.clone(), &key).await;
    let root = writable
        .core
        .load()
        .module_manager
        .get_module::<AuthModule>("auth")
        .expect("auth module")
        .token_store
        .load()
        .as_ref()
        .expect("token store")
        .root_token()
        .await
        .expect("root token")
        .id;
    writable
        .write(
            Some(root.clone()),
            "secret/un31".to_string(),
            secret_data("kept"),
        )
        .await
        .expect("store a secret");
    drop(writable);

    let (readonly, guarded) = open_readonly(backend.clone(), &key)
        .await
        .expect("readonly open");
    let response = readonly
        .read(Some(root), "secret/un31")
        .await
        .expect("read the secret")
        .expect("a response");
    assert_eq!(
        response.data.expect("data")["value"],
        Value::String("kept".into()),
        "the readonly open must serve the stored secret"
    );
    assert_eq!(
        guarded.denied_writes(),
        0,
        "reading a secret must not have needed a write"
    );
}

/// The bootstrap of a readonly open leaves storage byte-for-byte identical.
///
/// This is the whole claim in one assertion: every repair the writable path
/// performs — mount table, auth mount, default policies, token salt — would
/// show up here as a changed or added key.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_a_readonly_open_persists_nothing() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;
    drop(open_writable(backend.clone(), &key).await);

    let before = backend.snapshot();
    let (_readonly, guarded) = open_readonly(backend.clone(), &key)
        .await
        .expect("readonly open");
    let after = backend.snapshot();

    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "a readonly bootstrap must not add or remove any key"
    );
    assert!(
        before == after,
        "a readonly bootstrap must not rewrite any value either"
    );
    assert_eq!(
        guarded.denied_writes(),
        0,
        "nothing should even have attempted a write — a non-zero count means a \
         path above the backstop still tries, and only the wrapper stops it"
    );
}

/// Neither background thread is running after a readonly open.
///
/// The mounts monitor reloads and re-mounts on a timer; the expiration checker
/// revokes leases and deletes their records. Both are asserted directly rather
/// than inferred from the absence of an effect, which would only ever be a race
/// with their tick.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_a_readonly_open_starts_no_background_worker() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;

    let writable = open_writable(backend.clone(), &key).await;
    assert!(
        writable.core.load().mounts_monitor.load().is_some(),
        "the control must have a mounts monitor, or the readonly assertion \
         below is about the configuration rather than the mode"
    );
    assert!(
        expiration_installed(&writable).expect("observe the control"),
        "the control must have gone through AuthModule::init, the only caller \
         of start_check_expired_lease_entries"
    );
    drop(writable);

    let (readonly, _guarded) = open_readonly(backend.clone(), &key)
        .await
        .expect("readonly open");
    assert!(
        readonly.core.load().mounts_monitor.load().is_none(),
        "a readonly open must not create the mounts monitor, whatever the \
         configured interval says"
    );
    assert!(
        !expiration_installed(&readonly).expect("observe the readonly handle"),
        "a readonly open must not run AuthModule::init, and so must not start \
         the expired-lease checker"
    );
}

/// A missing mount table is reported, not invented.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_a_missing_mount_table_fails_closed() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;
    drop(open_writable(backend.clone(), &key).await);

    assert!(
        backend.contains("core/mounts"),
        "fixture: the mount table is stored under core/mounts"
    );
    backend.raw_delete("core/mounts");

    let error = open_readonly(backend.clone(), &key)
        .await
        .err()
        .expect("a readonly open of a vault with no mount table must fail");
    assert_state_incomplete(&error);
    assert!(
        !backend.contains("core/mounts"),
        "the failed readonly open must not have planted the default mounts"
    );

    // The control: the writable path really does repair this, so the readonly
    // refusal above is a decision rather than a shared inability.
    drop(open_writable(backend.clone(), &key).await);
    assert!(
        backend.contains("core/mounts"),
        "fixture: the writable open replants the default mount table"
    );
}

/// A mount entry left in an older format is reported, not rewritten.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_an_older_mount_entry_format_fails_closed() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;

    // Strip the `table` field from every entry, which is exactly what
    // `mount_update` exists to backfill.
    {
        let writable = open_writable(backend.clone(), &key).await;
        let core = writable.core.load();
        let storage = core.barrier.as_storage();
        let entry = storage
            .get("core/mounts")
            .await
            .expect("read the mount table")
            .expect("the mount table exists");
        let mut table: Value = serde_json::from_slice(&entry.value).expect("mount table json");
        for mount in table["entries"]
            .as_object_mut()
            .expect("entries")
            .values_mut()
        {
            mount["table"] = Value::String(String::new());
        }
        storage
            .put(&libvault::storage::StorageEntry {
                key: "core/mounts".to_string(),
                value: serde_json::to_vec(&table).expect("serialize"),
            })
            .await
            .expect("write the downgraded mount table");
    }
    let downgraded = backend.raw_get("core/mounts").expect("downgraded table");

    let error = open_readonly(backend.clone(), &key)
        .await
        .err()
        .expect("a readonly open of an older-format mount table must fail");
    assert_state_incomplete(&error);
    assert_eq!(
        backend.raw_get("core/mounts").as_ref(),
        Some(&downgraded),
        "the failed readonly open must have left the stored table untouched"
    );

    drop(open_writable(backend.clone(), &key).await);
    assert_ne!(
        backend.raw_get("core/mounts").as_ref(),
        Some(&downgraded),
        "fixture: the writable open is what rewrites an older-format table"
    );
}

/// A deleted built-in ACL policy is not replanted by a readonly open.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_a_deleted_default_policy_is_not_replanted() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;
    drop(open_writable(backend.clone(), &key).await);

    let policy_key = backend
        .keys()
        .into_iter()
        .find(|k| k.ends_with("/default") && k.contains("policy"))
        .expect("fixture: the writable open stored a default ACL policy");
    backend.raw_delete(&policy_key);

    let (_readonly, guarded) = open_readonly(backend.clone(), &key)
        .await
        .expect("readonly open");
    assert!(
        !backend.contains(&policy_key),
        "a readonly open must not write the built-in ACL policies back: {policy_key}"
    );
    assert_eq!(guarded.denied_writes(), 0, "nothing should have tried");

    drop(open_writable(backend.clone(), &key).await);
    assert!(
        backend.contains(&policy_key),
        "fixture: the writable open is what replants the built-in policy"
    );
}

/// An expired lease is still there after a readonly open.
///
/// The checker thread ticks every 200ms and, for a lease past its expiry,
/// revokes it and deletes the record. The wait below is generous on purpose:
/// the point is that the record survives time passing, not that it survives one
/// scheduling accident.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_an_expired_lease_survives_a_readonly_open() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;

    let lease_key = "sys/id/un31lease";
    let lease = json!({
        "lease_id": "un31lease",
        "client_token": "un31-token",
        "path": "un31/lease",
        "data": {},
        "secret": null,
        "auth": null,
        "issue_time": "1970-01-01T00:00:00Z",
        "expire_time": "1970-01-01T00:00:01Z",
        "revoke_err": ""
    });
    write_through_barrier(&backend, &key, lease_key, &lease).await;
    assert!(backend.contains(lease_key), "fixture: the lease is stored");

    let (_readonly, guarded) = open_readonly(backend.clone(), &key)
        .await
        .expect("readonly open");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        backend.contains(lease_key),
        "a readonly open must not revoke an expired lease or delete its record"
    );
    assert_eq!(
        guarded.denied_writes(),
        0,
        "no revocation should even have been attempted"
    );
    drop(_readonly);

    // The control: with the checker running, this same record is revoked.
    let writable = open_writable(backend.clone(), &key).await;
    for _ in 0..40 {
        if !backend.contains(lease_key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !backend.contains(lease_key),
        "fixture: a writable open is what revokes the expired lease — if it \
         does not, the readonly assertion above proves nothing"
    );
    drop(writable);
}

/// An older-format lease is reported, not silently migrated.
///
/// Restoring a lease stored in the previous shape converts it and writes the
/// converted entry back. Under a readonly open that write would only be stopped
/// by the backstop, and the caller would be told "write denied" instead of what
/// is actually wrong. Older format is a state the readonly mode reports, in the
/// same terms as an older-format mount table.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_an_older_format_lease_fails_closed() {
    let backend = Arc::new(MemoryBackend::default());
    let key = init(backend.clone()).await;

    // `data: null` is what distinguishes the older shape: the current
    // `LeaseEntry` requires an object there, the older one allowed none.
    let lease_key = "sys/id/un31old";
    let lease = json!({
        "lease_id": "un31old",
        "client_token": "un31-token",
        "path": "un31/lease",
        "data": null,
        "secret": null,
        "auth": null,
        "issue_time": "1970-01-01T00:00:00Z",
        "expire_time": "2999-01-01T00:00:00Z"
    });
    write_through_barrier(&backend, &key, lease_key, &lease).await;
    let stored = backend.raw_get(lease_key).expect("the lease is stored");

    let error = open_readonly(backend.clone(), &key)
        .await
        .err()
        .expect("a readonly open of an older-format lease must fail");
    assert_state_incomplete(&error);
    assert_eq!(
        backend.raw_get(lease_key).as_ref(),
        Some(&stored),
        "the failed readonly open must have left the stored lease untouched"
    );

    // The control: the writable open is what migrates it, which is exactly the
    // write the readonly refusal above avoids attempting.
    drop(open_writable(backend.clone(), &key).await);
    assert_ne!(
        backend.raw_get(lease_key).as_ref(),
        Some(&stored),
        "fixture: the writable open is what rewrites an older-format lease"
    );
}

/// Write a raw value through an unsealed barrier, so the stored bytes are
/// encrypted exactly as the vault itself would have written them.
async fn write_through_barrier(
    backend: &Arc<MemoryBackend>,
    key: &[u8],
    storage_key: &str,
    value: &Value,
) {
    let writable = open_writable(backend.clone(), key).await;
    let core = writable.core.load();
    core.barrier
        .as_storage()
        .put(&libvault::storage::StorageEntry {
            key: storage_key.to_string(),
            value: serde_json::to_vec(value).expect("serialize"),
        })
        .await
        .expect("seed through the barrier");
}

/// The backstop itself: reads pass, writes fail hard and change nothing.
///
/// A denial has to be an error rather than a silent no-op — a swallowed write
/// leaves the caller believing its state was persisted.
#[tokio::test]
#[ignore = "VLT-04"]
async fn un31_the_readonly_backend_denies_put_and_delete() {
    let inner = Arc::new(MemoryBackend::default());
    inner.raw_put("kept", b"value".to_vec());
    let guarded = ReadonlyBackend::new(inner.clone());

    let entry = guarded.get("kept").await.expect("get").expect("present");
    assert_eq!(entry.value, b"value".to_vec(), "reads pass through");
    assert_eq!(
        guarded.list("").await.expect("list"),
        vec!["kept".to_string()],
        "lists pass through"
    );

    let denied_put = guarded
        .put(&BackendEntry {
            key: "kept".to_string(),
            value: b"overwritten".to_vec(),
        })
        .await
        .expect_err("put must fail");
    assert!(
        is_readonly_write_denied(&denied_put),
        "the refusal must name the reason: {denied_put}"
    );
    let denied_delete = guarded.delete("kept").await.expect_err("delete must fail");
    assert!(
        is_readonly_write_denied(&denied_delete),
        "the refusal must name the reason: {denied_delete}"
    );

    assert_eq!(
        inner.raw_get("kept"),
        Some(b"value".to_vec()),
        "the refused writes must not have reached the wrapped backend"
    );
    assert_eq!(
        guarded.denied_writes(),
        2,
        "both refusals are counted, so an operator can see that something tried"
    );
}

/// The production entry point, over the real database-backed storage.
///
/// The tests above exercise the vendored core through an in-memory backend;
/// these two cover the seam an audit command actually calls, where the key file
/// and the initialized storage have to agree and no runtime credential may be
/// minted.
mod vault_core {
    use std::sync::Arc;

    use serde_json::{Map, Value};

    use crate::{
        common::errors::VaultError,
        contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface},
        jupiter::{
            migration::apply_migrations,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                vault_storage::VaultStorage,
            },
            tests::test_db_connection,
        },
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore = "VLT-04"]
    async fn un31_open_readonly_reads_secrets_and_refuses_writes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let connection = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(&connection, true).await.expect("migrate");
        let key_path = temp.path().join("core_key.json");

        let writable = VaultCore::from_database_connection(connection.clone(), key_path.clone())
            .await
            .expect("bootstrap the vault");
        let mut data = Map::new();
        data.insert("value".to_string(), Value::String("stored".to_string()));
        writable
            .write_secret("un31/secret", Some(data))
            .await
            .expect("store a secret");
        drop(writable);

        let storage = VaultStorage {
            base: BaseStorage::new(connection.clone()),
        };
        let readonly = VaultCore::open_readonly(storage, key_path)
            .await
            .expect("readonly open");

        assert!(readonly.is_readonly());
        assert_eq!(
            readonly
                .read_secret("un31/secret")
                .await
                .expect("read")
                .expect("present")["value"],
            Value::String("stored".to_string()),
            "a readonly handle must still read"
        );

        let error = readonly
            .write_secret("un31/secret", Some(Map::new()))
            .await
            .expect_err("a readonly handle must refuse to write");
        assert!(
            error.to_string().contains("opened readonly"),
            "the refusal must name the reason: {error}"
        );
        let error = readonly
            .delete_secret("un31/secret")
            .await
            .expect_err("a readonly handle must refuse to delete");
        assert!(
            error.to_string().contains("opened readonly"),
            "the refusal must name the reason: {error}"
        );

        assert_eq!(
            readonly.denied_writes(),
            Some(0),
            "the refusals must have been caught above the backstop, so the \
             backstop itself never had to fire"
        );
        assert_eq!(
            readonly
                .read_secret("un31/secret")
                .await
                .expect("read")
                .expect("present")["value"],
            Value::String("stored".to_string()),
            "the refused write must not have changed the secret"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore = "VLT-04"]
    async fn un31_open_readonly_reports_an_uninitialized_vault_instead_of_initializing_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let connection = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(&connection, true).await.expect("migrate");
        let key_path = temp.path().join("core_key.json");

        let storage = VaultStorage {
            base: BaseStorage::new(connection.clone()),
        };
        let Err(error) = VaultCore::open_readonly(storage, key_path.clone()).await else {
            panic!("an uninitialized vault must not be opened readonly");
        };
        assert!(
            matches!(error, VaultError::ReadonlyNotInitialized),
            "unexpected error: {error}"
        );
        assert!(
            !key_path.exists(),
            "the failed readonly open must not have written a core key file"
        );

        // The control: the production bootstrap is what initializes, and it is
        // exactly that behaviour the readonly entry point exists to avoid.
        VaultCore::from_database_connection(connection, key_path.clone())
            .await
            .expect("the writable bootstrap initializes");
        assert!(
            key_path.exists(),
            "fixture: the writable path writes the key"
        );
    }

    /// Missing runtime credentials are reported, not minted.
    ///
    /// The writable bootstrap creates whatever runtime token the key file is
    /// short of — writing a policy, issuing a token, and rewriting the key file
    /// on disk. A readonly open cannot do any of that, and quietly proceeding
    /// with an incomplete token set would mean reading secrets under a
    /// credential that does not cover them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore = "VLT-04"]
    async fn un31_open_readonly_reports_incomplete_runtime_credentials() {
        let temp = tempfile::tempdir().expect("temp dir");
        let connection = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(&connection, true).await.expect("migrate");
        let key_path = temp.path().join("core_key.json");

        drop(
            VaultCore::from_database_connection(connection.clone(), key_path.clone())
                .await
                .expect("bootstrap the vault"),
        );

        // Blank one runtime token, exactly as an older key file predating that
        // token would look.
        let mut key: Value =
            serde_json::from_slice(&std::fs::read(&key_path).expect("read key file"))
                .expect("key file json");
        key["runtime_tokens"]["pki"] = Value::String(String::new());
        let downgraded = serde_json::to_vec_pretty(&key).expect("serialize");
        std::fs::write(&key_path, &downgraded).expect("write key file");

        let storage = VaultStorage {
            base: BaseStorage::new(connection.clone()),
        };
        let Err(error) = VaultCore::open_readonly(storage, key_path.clone()).await else {
            panic!("an incomplete runtime token set must not be opened readonly");
        };
        assert!(
            matches!(error, VaultError::ReadonlyRuntimeTokensIncomplete),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(&key_path).expect("read key file"),
            downgraded,
            "the failed readonly open must not have rewritten the key file"
        );

        // The control: the writable bootstrap takes the minting branch for the
        // same key file — it does not refuse at the door, it tries, and here it
        // gets as far as needing the root token that the first bootstrap
        // deliberately revoked. Refusing to open is a decision of the readonly
        // mode, not something both paths do.
        let Err(writable_error) =
            VaultCore::from_database_connection(connection, key_path.clone()).await
        else {
            panic!("fixture: the writable bootstrap cannot mint without a root token");
        };
        assert!(
            matches!(
                writable_error,
                VaultError::RootTokenRequiredForRuntimeCredentials
            ),
            "fixture: the writable path is the one that tries to mint: {writable_error}"
        );
    }
}
