use std::{
    cell::RefCell,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
    Value, sqlx,
};
use tracing::log;
use url::Url;

use crate::{
    config::{Config, DbConfig, reload::ConfigHandle, testing::isolated_config},
    contract::policy::entitystore::SharedEntityStore,
    jupiter::{
        migration::apply_migrations,
        service::{
            agent_capture_service::AgentCaptureService, artifact_service::ArtifactService,
            buck_service::BuckService, cl_service::CLService, git_service::GitService,
            import_service::ImportService, lfs_service::LfsService, mono_service::MonoService,
            oci_service::OciService, push_queue_service::PushQueueService,
            webhook_service::WebhookService,
        },
        storage::{
            AppService, Storage,
            audit_storage::AuditStorage,
            base_storage::{BaseStorage, StorageConnector},
            bots_storage::BotsStorage,
            buck_storage::BuckStorage,
            cl_storage::ClStorage,
            commit_binding_storage::CommitBindingStorage,
            conversation_storage::ConversationStorage,
            git_db_storage::GitDbStorage,
            gpg_storage::GpgStorage,
            group_storage::GroupStorage,
            issue_storage::IssueStorage,
            lfs_db_storage::LfsDbStorage,
            mono_storage::MonoStorage,
            notification_storage::NotificationStorage,
            object_storage::mock_object_storage,
            oci_db_storage::OciDbStorage,
            push_queue_storage::PushQueueStorage,
            user_storage::UserStorage,
            vault_storage::VaultStorage,
            webhook_storage::WebhookStorage,
        },
    },
};

const DEFAULT_TEST_DATABASE_URL: &str =
    "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";

static TEST_SCHEMA_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub async fn test_db_connection(_temp_dir: &Path) -> DatabaseConnection {
    let (db_url, schema) = create_test_database_url().await;

    let mut opt = ConnectOptions::new(db_url);
    opt.max_connections(2)
        .min_connections(1)
        // Generous on purpose (FIX-05, same rationale as `test_db_config`):
        // a full `cargo test --all` runs dozens of tests applying full-schema
        // migrations concurrently; acquire waits reflect machine load, not
        // broken code. A timeout still exists so a genuinely stuck connection
        // fails the run instead of hanging it.
        .acquire_timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(60))
        .sqlx_logging(true)
        .sqlx_logging_level(log::LevelFilter::Debug);

    let mut db = Database::connect(opt)
        .await
        .expect("Failed to connect to PostgreSQL test database");
    // The schema must outlive every clone of this connection, and the metric
    // callback is the one `Arc` sea-orm copies into each clone, transaction
    // and stream. Parking the guard there drops the schema exactly when the
    // last of them goes away, panics included, without changing the helper's
    // signature for its call sites.
    db.set_metric_callback(move |_| {
        let _held_by_the_connection = &schema;
    });
    db
}

/// The returned [`TestSchemaGuard`] drops the schema; bind it for the whole
/// test (`let (db_config, _schema) = ...`), because a `DbConfig` cannot carry
/// it the way the connection from [`test_db_connection`] does.
pub async fn test_db_config(_temp_dir: &Path) -> (DbConfig, TestSchemaGuard) {
    let (db_url, schema) = create_test_database_url().await;
    let config = DbConfig {
        db_type: "postgres".to_owned(),
        db_url,
        max_connection: 2,
        min_connection: 1,
        // Generous on purpose (FIX-05). These are not a property under test —
        // no test asserts how long acquiring a connection takes — but at five
        // seconds they were being hit by the machine being busy rather than by
        // anything being wrong: a full `cargo test --all` runs dozens of
        // threads, several of which are applying migrations with
        // `refresh = true`, which drops and rebuilds an entire schema. The
        // result was a timeout reported as a vault or storage failure, in a
        // different test each run.
        //
        // A timeout still exists so a genuinely stuck connection fails the run
        // instead of hanging it; it is simply long enough that only a real
        // problem reaches it.
        acquire_timeout: 60,
        connect_timeout: 30,
        sqlx_logging: false,
    };
    (config, schema)
}

/// One per-test schema, dropped (`DROP SCHEMA ... CASCADE`) by whichever of
/// its holders lets go first: the [`TestSchemaGuard`], or the test thread's
/// exit for a schema something kept alive (libvault's modules hold `Arc`
/// cycles, so a vault's pool is never freed).
struct TestSchema {
    admin_url: String,
    schema: String,
    owner: String,
    dropped: AtomicBool,
}

impl TestSchema {
    /// Whether the caller is the one to drop the schema.
    fn claim(&self) -> bool {
        !self.dropped.swap(true, Ordering::AcqRel)
    }

    /// Drops the schema from any thread, a tokio worker included (where
    /// `block_on` panics and a task spawned there would die with the test's
    /// runtime): the work gets a thread and runtime of its own and is waited
    /// for, so a test binary never exits with a drop still in flight.
    fn drop_off_thread(&self) {
        let admin_url = self.admin_url.clone();
        let schema = self.schema.clone();
        let worker = std::thread::Builder::new()
            .name(format!("drop-{schema}"))
            .spawn(move || drop_test_schema(&admin_url, &schema));
        match worker {
            Ok(worker) => {
                if worker.join().is_err() {
                    eprintln!("test schema left behind: the drop thread panicked");
                }
            }
            Err(err) => eprintln!("test schema left behind: no drop thread: {err}"),
        }
    }
}

/// Owns one per-test schema and drops it when dropped, on the panic path as
/// well.
pub struct TestSchemaGuard(Arc<TestSchema>);

impl TestSchemaGuard {
    pub fn schema(&self) -> &str {
        &self.0.schema
    }
}

impl Drop for TestSchemaGuard {
    fn drop(&mut self) {
        if self.0.claim() {
            self.0.drop_off_thread();
        }
    }
}

thread_local! {
    static HELD_ON_THIS_THREAD: RefCell<HeldSchemas> = const { RefCell::new(HeldSchemas(Vec::new())) };
}

/// Every schema created on this thread. libtest runs each test on a thread of
/// its own, so its destructor is the end of the test, whatever the test left
/// alive; destructors run last-registered first, so the thread-locals std and
/// tokio set up before the first schema are still there for the drop thread.
struct HeldSchemas(Vec<Arc<TestSchema>>);

impl Drop for HeldSchemas {
    fn drop(&mut self) {
        for schema in self.0.drain(..) {
            if schema.claim() {
                eprintln!(
                    "test schema {} was still held when {} ended; dropping it now",
                    schema.schema, schema.owner
                );
                schema.drop_off_thread();
            }
        }
    }
}

fn drop_test_schema(admin_url: &str, schema: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("test schema {schema} left behind: no runtime for the drop: {err}");
            return;
        }
    };
    let dropped = runtime.block_on(async {
        let admin = Database::connect(admin_connect_options(admin_url)).await?;
        // Every backend still tagged with this schema belongs to a handle that
        // has already been let go of (or to a caller that handed the guard
        // back first), so cutting it loses nothing; waiting for it to close on
        // its own would park the drop on whatever locks it still holds.
        admin
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE application_name = $1 AND pid <> pg_backend_pid()",
                [Value::from(schema)],
            ))
            .await?;
        // The timeout turns a lock held by anything else into a leaked schema
        // and a message, not a hung test run.
        admin
            .execute_unprepared(&format!(
                "SET lock_timeout = '120s'; DROP SCHEMA IF EXISTS {schema} CASCADE"
            ))
            .await?;
        admin.close().await
    });
    if let Err(err) = dropped {
        eprintln!("test schema {schema} left behind: {err}");
    }
}

async fn create_test_database_url() -> (String, TestSchemaGuard) {
    let admin_url =
        std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
    assert_postgres_url(&admin_url);

    let schema = format!(
        "mega2_test_{}_{}",
        std::process::id(),
        TEST_SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let admin = Database::connect(admin_connect_options(&admin_url))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "test PostgreSQL is not available; run `docker compose -f docker/docker-compose.test.yml up -d` first"
            )
        });
    execute_postgres(&admin, format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await;
    execute_postgres(&admin, format!("CREATE SCHEMA {schema}")).await;
    // libtest names the test thread after the test, so a schema that is ever
    // left behind can be traced back:
    // `SELECT nspname, obj_description(oid, 'pg_namespace') FROM pg_namespace`.
    // Straight through sqlx: sea-orm's `debug-print` would put the statement,
    // test name included, into the tracing output that log-hygiene tests
    // search for words such as `root_token`.
    let owner = std::thread::current()
        .name()
        .unwrap_or("unnamed thread")
        .to_owned();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "COMMENT ON SCHEMA {schema} IS '{}'",
        owner.replace('\'', "''")
    )))
    .execute(admin.get_postgres_connection_pool())
    .await
    .expect("failed to label PostgreSQL test schema");

    let db_url = database_url_for_schema(&admin_url, &schema);
    let schema = Arc::new(TestSchema {
        admin_url,
        schema,
        owner,
        dropped: AtomicBool::new(false),
    });
    let _ = HELD_ON_THIS_THREAD.try_with(|held| held.borrow_mut().0.push(schema.clone()));
    (db_url, TestSchemaGuard(schema))
}

fn admin_connect_options(admin_url: &str) -> ConnectOptions {
    let mut admin_opt = ConnectOptions::new(admin_url);
    admin_opt
        .max_connections(1)
        .min_connections(1)
        // Same FIX-05 rationale as `test_db_connection`: the probe above all
        // else must not fail merely because the shared test PostgreSQL is
        // busy serving dozens of concurrent migration runs.
        .connect_timeout(std::time::Duration::from_secs(60))
        .acquire_timeout(std::time::Duration::from_secs(120))
        .sqlx_logging(false);
    admin_opt
}

fn assert_postgres_url(db_url: &str) {
    let url = Url::parse(db_url).expect("MEGA_DATABASE__DB_URL must be a valid PostgreSQL URL");
    assert!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "MEGA_DATABASE__DB_URL must use postgres:// or postgresql://"
    );
}

fn database_url_for_schema(admin_url: &str, schema: &str) -> String {
    let mut url = Url::parse(admin_url).expect("MEGA_DATABASE__DB_URL must be a valid URL");
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema},public"))
        // Tags the backends of this schema so `TestSchemaGuard` can find them.
        .append_pair("application_name", schema);
    url.to_string()
}

async fn execute_postgres(db: &DatabaseConnection, sql: String) {
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .expect("failed to prepare PostgreSQL test schema");
}

pub async fn test_storage(temp_dir: impl AsRef<Path>) -> Storage {
    test_storage_with_config(
        temp_dir.as_ref(),
        isolated_config(temp_dir.as_ref().join("config")),
    )
    .await
}

/// Like [`test_storage`], named for queue-merge fixtures.
pub async fn test_storage_queue_merge(temp_dir: impl AsRef<Path>) -> Storage {
    test_storage_with_config(
        temp_dir.as_ref(),
        isolated_config(temp_dir.as_ref().join("config")),
    )
    .await
}

pub async fn test_storage_with_config(temp_dir: impl AsRef<Path>, config: Config) -> Storage {
    let connection = test_db_connection(temp_dir.as_ref()).await;
    let connection = Arc::new(connection);
    let config = Arc::new(config);
    let base = BaseStorage::new(connection.clone());

    let svc = AppService {
        mono_storage: MonoStorage { base: base.clone() },
        git_db_storage: GitDbStorage { base: base.clone() },
        gpg_storage: GpgStorage { base: base.clone() },
        lfs_db_storage: LfsDbStorage { base: base.clone() },
        user_storage: UserStorage { base: base.clone() },
        group_storage: GroupStorage { base: base.clone() },
        cl_storage: ClStorage { base: base.clone() },
        issue_storage: IssueStorage { base: base.clone() },
        vault_storage: VaultStorage { base: base.clone() },
        conversation_storage: ConversationStorage { base: base.clone() },
        commit_binding_storage: CommitBindingStorage { base: base.clone() },
        push_queue_storage: PushQueueStorage::new(base.clone())
            .with_native_publication(config.mst2.publication_enabled),
        buck_storage: BuckStorage { base: base.clone() },
        bots_storage: BotsStorage { base: base.clone() },
        webhook_storage: WebhookStorage { base: base.clone() },
        audit_storage: AuditStorage { base: base.clone() },
        oci_db_storage: OciDbStorage { base: base.clone() },
        media_paging_storage:
            crate::jupiter::storage::media_paging_storage::MediaPagingStorage::new(base.clone()),
    };

    apply_migrations(&connection, true).await.unwrap();

    let webhook_service = WebhookService::mock(svc.webhook_storage.clone());

    Storage {
        app_service: Arc::new(svc),
        native_projection_cache: Arc::default(),
        projection_observation_sink: None,
        cl_service: CLService::mock(),
        push_queue_service: PushQueueService::new(
            base.clone(),
            config.monorepo.push_policy.clone(),
        )
        .with_timeouts(Duration::from_secs(30), Duration::from_millis(20))
        .with_max_push_commits(config.monorepo.max_push_commits)
        .with_native_publication(config.mst2.publication_enabled),
        artifact_service: ArtifactService::new(base.clone(), mock_object_storage()),
        buck_service: BuckService::mock(),
        config_handle: ConfigHandle::from_arc(config.clone()),
        config,
        git_service: GitService::mock(),
        mono_service: MonoService::mock(),
        import_service: ImportService::mock(),
        lfs_service: LfsService::mock(),
        oci_service: OciService::mock(),
        agent_capture_service: AgentCaptureService::mock(),
        webhook_service,
        storage_event_emitter:
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
        notification_storage: NotificationStorage::new(connection.clone()),
        entity_store: Arc::new(SharedEntityStore::default()),
        vault: None,
    }
}

/// Inject a vault handle so trunk synthetic commits can be server-signed (TP-16).
pub async fn with_test_vault(storage: Storage, dir: impl AsRef<Path>) -> Storage {
    use crate::contract::vault::integration::vault_core::VaultCore;

    let vault = VaultCore::config(
        storage.vault_storage(),
        dir.as_ref().join("tp16_vault_core_key.json"),
    )
    .await
    .expect("test vault core");
    storage.with_vault(vault)
}

/// Start a standalone Redis instance on an OS-assigned test port.
pub fn test_redis_server() -> redis_test::server::RedisServer {
    use redis_test::server::RedisServer;

    // RedisServer::new reserves room for a cluster bus port and rejects
    // ports >= 55535. These standalone servers need no cluster bus port.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("free Redis test port");
    let port = listener.local_addr().expect("Redis test address").port();
    drop(listener);
    RedisServer::new_with_addr_and_modules(RedisServer::get_addr(port), &[], false)
}

/// Redis manager for RedLock-backed server-signing key init.
pub async fn test_redis_manager() -> crate::jupiter::redis::ConnectionManager {
    let url =
        std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
    let client = ::redis::Client::open(url).expect("redis client");
    ::redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connection")
}

#[cfg(test)]
mod schema_guard_tests {
    use sea_orm::TransactionTrait;

    use super::*;

    async fn current_schema(db: &DatabaseConnection) -> String {
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_schema()".to_owned(),
        ))
        .await
        .expect("current_schema query")
        .expect("current_schema row")
        .try_get_by_index::<String>(0)
        .expect("current_schema value")
    }

    async fn schema_exists(schema: &str) -> bool {
        let admin_url = std::env::var("MEGA_DATABASE__DB_URL")
            .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
        let admin = Database::connect(admin_connect_options(&admin_url))
            .await
            .expect("admin connection");
        admin
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT 1 FROM pg_namespace WHERE nspname = $1",
                [Value::from(schema)],
            ))
            .await
            .expect("pg_namespace query")
            .is_some()
    }

    #[tokio::test]
    async fn the_schema_goes_with_the_last_handle_to_the_connection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let schema = current_schema(&db).await;
        assert!(schema.starts_with("mega2_test_"), "{schema}");
        assert!(schema_exists(&schema).await);

        let clone = db.clone();
        drop(db);
        assert!(
            schema_exists(&schema).await,
            "a live clone must keep the schema"
        );

        drop(clone);
        assert!(
            !schema_exists(&schema).await,
            "schema {schema} outlived its connection"
        );
    }

    #[tokio::test]
    async fn a_panicking_test_still_drops_its_schema() {
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel();
        let failed = tokio::spawn(async move {
            let temp = tempfile::tempdir().expect("temp dir");
            let db = test_db_connection(temp.path()).await;
            let _ = schema_tx.send(current_schema(&db).await);
            panic!("simulated test failure while holding the connection");
        })
        .await;
        assert!(failed.is_err(), "the task must have panicked");

        let schema = schema_rx.await.expect("schema name sent before the panic");
        assert!(
            !schema_exists(&schema).await,
            "schema {schema} survived the panic"
        );
    }

    #[test]
    fn a_schema_whose_connection_is_never_released_goes_with_the_test_thread() {
        let (schema_tx, schema_rx) = std::sync::mpsc::channel();
        let leaker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let temp = tempfile::tempdir().expect("temp dir");
                let db = test_db_connection(temp.path()).await;
                schema_tx
                    .send(current_schema(&db).await)
                    .expect("schema name sent");
                // What an `Arc` cycle does to a vault's pool, done on purpose.
                std::mem::forget(db);
            });
        });
        leaker.join().expect("leaker thread");

        let schema = schema_rx.recv().expect("schema name");
        let exists = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(schema_exists(&schema));
        assert!(
            !exists,
            "schema {schema} outlived the thread that never released it"
        );
    }

    #[tokio::test]
    async fn the_config_guard_cuts_a_lingering_transaction_instead_of_waiting_on_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (db_config, guard) = test_db_config(temp.path()).await;
        let db = crate::jupiter::storage::init::database_connection(&db_config)
            .await
            .expect("connection through the config");
        let schema = current_schema(&db).await;
        assert_eq!(schema, guard.schema());

        // An open transaction with a lock the drop needs: without the
        // terminate step the drop would wait on it for as long as it lives.
        let txn = db.begin().await.expect("transaction");
        txn.execute_unprepared("CREATE TABLE lock_holder (id int)")
            .await
            .expect("create table in transaction");

        drop(guard);
        assert!(
            !schema_exists(&schema).await,
            "schema {schema} outlived its guard"
        );
        drop(txn);
    }
}
