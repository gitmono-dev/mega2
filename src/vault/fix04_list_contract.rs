//! FIX-04: `JupiterBackend::list` and the contract the barrier views walk with.
//!
//! `BarrierView::get_keys` descends one level at a time: it asks for a prefix's
//! children, recurses into the names that end in `/`, and treats the rest as
//! leaves *relative to that prefix*. `physical::file::FileBackend` answers in
//! exactly those terms. `JupiterBackend` answered with whole keys, recursively,
//! so every "leaf" it reported doubled up its prefix on the following `get` and
//! resolved to nothing.
//!
//! That is not a cosmetic difference. `ExpirationManager::restore()` walks the
//! lease view this way, so under the database backend a restart recovered no
//! leases at all and expired ones were never revoked. The tests below check the
//! two levels separately: the backend against the reference implementation, and
//! the consequence end to end.

use std::{collections::HashMap, sync::Arc};

use serde_json::{Value, json};

use crate::{
    contract::vault::integration::jupiter_backend::JupiterBackend,
    jupiter::{
        migration::apply_migrations,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            vault_storage::VaultStorage,
        },
        tests::test_db_connection,
    },
    vault::{
        RustyVault,
        core::SealConfig,
        storage::{Backend, BackendEntry, physical::file::FileBackend},
    },
};

const SEAL: SealConfig = SealConfig {
    secret_shares: 1,
    secret_threshold: 1,
};

/// Keys shaped like the ones a real vault stores: nested, sharing prefixes, and
/// including a name with an underscore — a `LIKE` wildcard to Postgres but an
/// ordinary character in a vault key.
const KEYS: &[&str] = &[
    "core/mounts",
    "core/seal-config",
    "sys/policy/acl/default",
    "sys/policy/acl/root",
    "sys/id/lease1",
    "sys/id/lease2",
    "logical/abc/ssh_server_key",
    "logical/abc/ssh-server-key",
    "logical/abc/100%mine",
    "parent/tok1/child1",
    "parent/tok1/child2",
];

const PREFIXES: &[&str] = &[
    "",
    "core/",
    "sys/",
    "sys/policy/",
    "sys/policy/acl/",
    "logical/abc/",
    "nothing-here/",
    // A directory named without its trailing separator: `TokenStore` walks a
    // token's children with exactly this shape.
    "parent/tok1",
    "sys",
];

async fn seeded_jupiter(temp: &std::path::Path) -> JupiterBackend {
    let connection = Arc::new(test_db_connection(temp).await);
    apply_migrations(&connection, true).await.expect("migrate");
    let backend = JupiterBackend::new(VaultStorage {
        base: BaseStorage::new(connection),
    });
    for key in KEYS {
        backend
            .put(&BackendEntry {
                key: (*key).to_string(),
                value: b"v".to_vec(),
            })
            .await
            .expect("seed");
    }
    backend
}

async fn seeded_file(dir: &std::path::Path) -> FileBackend {
    let mut conf = HashMap::new();
    conf.insert(
        "path".to_string(),
        Value::String(dir.to_string_lossy().into_owned()),
    );
    let backend = FileBackend::new(&conf).expect("file backend");
    for key in KEYS {
        backend
            .put(&BackendEntry {
                key: (*key).to_string(),
                value: b"v".to_vec(),
            })
            .await
            .expect("seed");
    }
    backend
}

/// The database backend answers `list` the way the reference backend does.
///
/// Comparing against `FileBackend` rather than a hand-written expectation is
/// deliberate: the contract is "behave like the backend `get_keys` was written
/// against", and an expectation written by the same hand that wrote the fix
/// would only restate the fix.
///
/// The prefixes compared are segment-aligned — empty, or ending in `/` — which
/// is the only kind the barrier views produce. The two backends genuinely do
/// differ on a prefix that stops mid-name; see
/// `fix04_a_prefix_that_stops_mid_name_is_outside_the_contract`.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fix04_list_answers_in_the_same_terms_as_the_file_backend() {
    let temp = tempfile::tempdir().expect("temp dir");
    let jupiter = seeded_jupiter(temp.path()).await;
    let file_dir = tempfile::tempdir().expect("temp dir");
    let file = seeded_file(file_dir.path()).await;

    for prefix in PREFIXES {
        let mut expected = file.list(prefix).await.expect("file list");
        expected.sort();
        let actual = jupiter.list(prefix).await.expect("jupiter list");
        assert_eq!(
            actual, expected,
            "list({prefix:?}) must match the reference backend"
        );
    }
}

/// The specific shapes the contract is made of, stated outright.
///
/// The comparison above would still pass if both backends were wrong in the
/// same way; these assertions say what the answer has to be.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fix04_list_returns_relative_children_with_directory_markers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let jupiter = seeded_jupiter(temp.path()).await;

    assert_eq!(
        jupiter.list("").await.expect("list"),
        vec!["core/", "logical/", "parent/", "sys/"],
        "the root lists one entry per top-level directory, not every key"
    );
    assert_eq!(
        jupiter.list("sys/").await.expect("list"),
        vec!["id/", "policy/"],
        "a directory lists its immediate children, marked and deduplicated — \
         not its whole subtree"
    );
    assert_eq!(
        jupiter.list("sys/policy/acl/").await.expect("list"),
        vec!["default", "root"],
        "leaves are named relative to the prefix"
    );
    assert!(
        jupiter
            .list("nothing-here/")
            .await
            .expect("list")
            .is_empty(),
        "an absent prefix lists nothing rather than erroring"
    );
    assert_eq!(
        jupiter.list("logical/abc/").await.expect("list"),
        vec!["100%mine", "ssh-server-key", "ssh_server_key"],
        "`_` and `%` are characters in a key, not LIKE wildcards"
    );
    assert_eq!(
        jupiter.list("parent/tok1").await.expect("list"),
        vec!["child1", "child2"],
        "a directory named without its trailing separator lists its children, \
         not a bare `/` — this is the shape `TokenStore::revoke_tree_salted` \
         walks with, and getting it wrong stops the walk one level in"
    );
}

/// End to end: a lease that expired while the process was down is revoked when
/// it comes back up.
///
/// This is the behaviour the divergence silently removed — `restore()` walks the
/// lease view with `get_keys`, found nothing under this backend, and so the
/// checker thread had an empty queue no matter how many leases were overdue.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fix04_an_expired_lease_is_revoked_after_a_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let connection = Arc::new(test_db_connection(temp.path()).await);
    apply_migrations(&connection, true).await.expect("migrate");

    let storage = || VaultStorage {
        base: BaseStorage::new(connection.clone()),
    };
    let open = || async {
        RustyVault::new(Arc::new(JupiterBackend::new(storage())), None).expect("create vault")
    };

    let vault = open().await;
    let key = vault.init(&SEAL).await.expect("init").secret_shares[0].clone();

    // Seed an overdue lease through the barrier, so the stored bytes are
    // encrypted exactly as the vault itself would have written them.
    let lease_key = "sys/id/fix04lease";
    {
        let vault = open().await;
        assert!(vault.unseal(&[key.as_slice()]).await.expect("unseal"));
        let core = vault.core.load();
        core.barrier
            .as_storage()
            .put(&crate::vault::storage::StorageEntry {
                key: lease_key.to_string(),
                value: serde_json::to_vec(&json!({
                    "lease_id": "fix04lease",
                    "client_token": "fix04-token",
                    "path": "fix04/lease",
                    "data": {},
                    "secret": null,
                    "auth": null,
                    "issue_time": "1970-01-01T00:00:00Z",
                    "expire_time": "1970-01-01T00:00:01Z",
                    "revoke_err": ""
                }))
                .expect("serialize"),
            })
            .await
            .expect("seed the lease");
    }

    let backend = JupiterBackend::new(storage());
    assert!(
        backend.get(lease_key).await.expect("get").is_some(),
        "fixture: the lease is stored"
    );

    // Restart: this open must restore the lease and revoke it.
    let restarted = open().await;
    assert!(restarted.unseal(&[key.as_slice()]).await.expect("unseal"));
    for _ in 0..50 {
        if backend.get(lease_key).await.expect("get").is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        backend.get(lease_key).await.expect("get").is_none(),
        "an expired lease must be revoked and its record deleted after a \
         restart — before the list contract was fixed, restore() walked away \
         with an empty queue and this record lived forever"
    );
}

/// Where the two backends still differ, said out loud.
///
/// `FileBackend` resolves a prefix as a directory path, so a prefix that stops
/// mid-name names no directory and lists nothing; the projection here treats it
/// as what it says it is, a string prefix. Neither is more correct in the
/// abstract — the contract is only defined for segment-aligned prefixes, which
/// is all `BarrierView` ever passes (`expand_key` appends a view prefix that
/// ends in `/`, and `get_keys` recurses only into names ending in `/`). This
/// test exists so the boundary is a recorded fact rather than something a
/// future reader has to rediscover from a surprising result.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fix04_a_prefix_that_stops_mid_name_is_outside_the_contract() {
    let temp = tempfile::tempdir().expect("temp dir");
    let jupiter = seeded_jupiter(temp.path()).await;
    let file_dir = tempfile::tempdir().expect("temp dir");
    let file = seeded_file(file_dir.path()).await;

    assert_eq!(
        jupiter.list("sys/i").await.expect("list"),
        vec!["d/"],
        "the database backend reads a prefix as a prefix"
    );
    assert!(
        file.list("sys/i").await.expect("list").is_empty(),
        "the file backend reads it as a directory path, and there is no such \
         directory"
    );
}
