//! UN-43: what the read-only assembly does *not* do.
//!
//! `AppContext::new` starts a notification worker, builds a Redis connection,
//! and calls `init_monorepo()` — which writes refs and objects — before any
//! command body runs. UN-30 delivered the read surface; this card is about the
//! assembly around it, and about proving each absence rather than asserting it
//! in a comment.
//!
//! Each test below is written so that it would fail if the corresponding step
//! were added back, and pairs with a control showing the step really does have
//! the effect being looked for. "No side effect" is only interesting where a
//! side effect was possible.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use crate::{config::Config, context::ReadOnlyContext, notification::service::NotificationService};

/// A config pointed at `db_config`'s database with a local object store.
async fn read_only_config(temp: &std::path::Path) -> (Config, crate::config::DbConfig) {
    let db_config = crate::jupiter::tests::test_db_config(temp).await;
    let mut config = crate::config::testing::isolated_config(temp.join("config"));
    config.database = db_config.clone();
    (config, db_config)
}

async fn migrated(db_config: &crate::config::DbConfig) -> DatabaseConnection {
    crate::jupiter::storage::init::database_connection(db_config)
        .await
        .expect("writable connection applies the migrations")
}

async fn scalar(connection: &DatabaseConnection, sql: &str) -> i64 {
    connection
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"))
        .expect("one row")
        .try_get::<i64>("", "n")
        .expect("count")
}

/// Opening the read-only context does not install a notification service.
///
/// The active service is a process-global. Asserting it is *unchanged* rather
/// than absent is deliberate: another test in this binary may legitimately have
/// installed one, and a test that only passes when it runs first is worse than
/// no test.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un43_the_read_only_assembly_installs_no_notification_service() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (config, db_config) = read_only_config(temp.path()).await;
    drop(migrated(&db_config).await);

    let before = NotificationService::active();
    let _context = ReadOnlyContext::open(config, None)
        .await
        .expect("read-only context opens");
    let after = NotificationService::active();

    // Identity, not just presence: swapping one active service for another is
    // as much a side effect as installing the first one, and comparing
    // `is_some()` would not see it.
    match (before, after) {
        (None, None) => {}
        (Some(before), Some(after)) => assert!(
            Arc::ptr_eq(&before, &after),
            "the active notification service was replaced"
        ),
        (before, after) => panic!(
            "the read-only assembly changed whether a notification service is \
             active: {} -> {}",
            before.is_some(),
            after.is_some()
        ),
    }
}

/// An unreachable Redis does not stop the read-only assembly.
///
/// This is the discriminating form: production assembly builds a Redis
/// connection and fails closed when it cannot, so a config pointing at a dead
/// address separates "Redis was not needed" from "Redis happened to work".
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un43_an_unreachable_redis_does_not_stop_the_read_only_assembly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut config, db_config) = read_only_config(temp.path()).await;
    drop(migrated(&db_config).await);
    // Port 1 has nothing on it, on purpose.
    config.redis.url = "redis://127.0.0.1:1".to_string();

    // Counting the calls, not just observing that the assembly survived: a path
    // that connected, swallowed the failure and carried on would pass a
    // survival check and still have done the thing.
    let calls_before = crate::jupiter::redis::init_connection_calls_for(&config.redis.url);
    let context = ReadOnlyContext::open(config.clone(), None)
        .await
        .expect("a read-only command must not need Redis at all");
    assert_eq!(
        crate::jupiter::redis::init_connection_calls_for(&config.redis.url),
        calls_before,
        "the read-only assembly must not initialize Redis at all"
    );
    assert!(context.vault.is_none(), "nor a vault, for a local store");

    // The control: that same URL is genuinely unusable, and calling through it
    // does move the counter — so the equality above is about the assembly not
    // calling rather than about the counter never moving.
    let redis = crate::jupiter::redis::init_connection(&config.redis).await;
    assert!(
        redis.is_err(),
        "fixture: the Redis URL must really be unreachable"
    );
    assert_eq!(
        crate::jupiter::redis::init_connection_calls_for(&config.redis.url),
        calls_before + 1,
        "fixture: the counter must move when Redis really is initialized"
    );
}

/// The read-only assembly seeds nothing.
///
/// `init_monorepo()` writes a root ref, a commit, a tree and blobs; building
/// storage writes the default sidebars. On a migrated but empty database, all
/// of those counts must still be zero afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un43_the_read_only_assembly_seeds_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (config, db_config) = read_only_config(temp.path()).await;
    let observer = migrated(&db_config).await;

    for (table, sql) in [
        ("mega_refs", "SELECT count(*)::bigint AS n FROM mega_refs"),
        (
            "dynamic_sidebar",
            "SELECT count(*)::bigint AS n FROM dynamic_sidebar",
        ),
    ] {
        assert_eq!(
            scalar(&observer, sql).await,
            0,
            "fixture: {table} starts empty"
        );
    }

    let _context = ReadOnlyContext::open(config.clone(), None)
        .await
        .expect("read-only context opens");

    assert_eq!(
        scalar(&observer, "SELECT count(*)::bigint AS n FROM mega_refs").await,
        0,
        "the read-only assembly must not have called init_monorepo()"
    );
    assert_eq!(
        scalar(
            &observer,
            "SELECT count(*)::bigint AS n FROM dynamic_sidebar"
        )
        .await,
        0,
        "nor written the default sidebars"
    );

    // The control: the production pieces are what write these, so the zeros
    // above are a decision rather than a test that could not have failed.
    let storage = crate::jupiter::storage::Storage::new_with_connection(
        Arc::new(config),
        Arc::new(observer),
        crate::jupiter::storage::object_storage::mock_object_storage(),
    )
    .await
    .expect("production storage assembly");
    storage
        .mono_service
        .init_monorepo(&storage.config.monorepo)
        .await
        .expect("init_monorepo");

    let observer = migrated(&db_config).await;
    assert!(
        scalar(&observer, "SELECT count(*)::bigint AS n FROM mega_refs").await > 0,
        "fixture: init_monorepo writes a root ref"
    );
    assert!(
        scalar(
            &observer,
            "SELECT count(*)::bigint AS n FROM dynamic_sidebar"
        )
        .await
            > 0,
        "fixture: the production storage assembly writes the default sidebars"
    );
}

/// When a vault *is* needed, it is opened read-only and left untouched.
///
/// A local object store needs no vault at all, so this drives the case that does
/// need one: S3-compatible storage whose credentials are `vault://` references.
/// Both faces are checked — the stored rows and the key file on disk — because
/// the bootstrap path this must not take writes to each of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn un43_a_needed_vault_is_opened_read_only_and_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (mut config, db_config) = read_only_config(temp.path()).await;
    let observer = migrated(&db_config).await;
    let key_path = temp.path().join("core_key.json");

    // Bootstrap a real vault and store the two credentials it will be asked for.
    {
        let vault =
            crate::contract::vault::integration::vault_core::VaultCore::from_database_config(
                &db_config,
                key_path.clone(),
            )
            .await
            .expect("bootstrap the vault");
        use crate::contract::vault::integration::vault_core::VaultCoreInterface;
        for (name, value) in [
            ("config/it/object_storage/access_key_id", "AKIA-un43"),
            ("config/it/object_storage/secret_access_key", "un43-secret"),
        ] {
            let mut data = serde_json::Map::new();
            data.insert(
                "value".to_string(),
                serde_json::Value::String(value.to_string()),
            );
            vault
                .write_secret(name, Some(data))
                .await
                .expect("store the credential");
        }
    }

    config.object_storage.storage_type =
        crate::orbit_api::factory::ObjectStorageBackend::S3Compatible;
    config.object_storage.s3.region = "us-east-1".to_string();
    config.object_storage.s3.bucket = "un43-test".to_string();
    config.object_storage.s3.endpoint_url = "http://127.0.0.1:19000".to_string();
    config.object_storage.s3.access_key_id =
        "vault://secret/config/it/object_storage/access_key_id#value".to_string();
    config.object_storage.s3.secret_access_key =
        "vault://secret/config/it/object_storage/secret_access_key#value".to_string();
    assert!(
        super::object_storage_needs_vault(&config.object_storage),
        "fixture: this config must be one that needs the vault"
    );

    async fn vault_digest(connection: &DatabaseConnection) -> String {
        connection
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT coalesce(md5(string_agg(t::text, '|' ORDER BY t::text)), 'empty') \
                 AS digest FROM vault t"
                    .to_string(),
            ))
            .await
            .expect("digest the vault table")
            .expect("one row")
            .try_get::<String>("", "digest")
            .expect("digest")
    }
    let rows_before = vault_digest(&observer).await;
    let key_before = std::fs::read(&key_path).expect("read the key file");

    let context = ReadOnlyContext::open_with_vault_key_path(config, None, key_path.clone())
        .await
        .expect("a deployment whose credentials live in the vault must still open");
    let vault = context
        .vault
        .as_ref()
        .expect("a config with vault refs must have opened one");
    assert!(vault.is_readonly(), "and it must be the read-only handle");
    assert_eq!(
        vault.denied_writes(),
        Some(0),
        "nothing should have tried to write through it"
    );

    assert_eq!(
        vault_digest(&observer).await,
        rows_before,
        "the vault's stored state must be untouched"
    );
    assert_eq!(
        std::fs::read(&key_path).expect("read the key file"),
        key_before,
        "and the key file must not have been rewritten — the bootstrap path \
         rotates runtime credentials and writes it back, which is exactly what \
         a read-only open must not do"
    );
}
