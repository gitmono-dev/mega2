use std::{
    cell::RefCell,
    path::Path,
    sync::{
        Arc, Mutex,
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
            view_worker::ViewRuntime, webhook_service::WebhookService,
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
            view_storage::ViewStorage,
            webhook_storage::WebhookStorage,
        },
    },
};

const DEFAULT_TEST_DATABASE_URL: &str =
    "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";

pub async fn test_db_connection(_temp_dir: &Path) -> DatabaseConnection {
    connect_guarded_database(TestDatabaseKind::Default).await
}

/// Migrated database for trace fragment tests. Migrations run on the real helper
/// connection; the schema guard stays attached to that connection.
pub async fn migrated_trace_test_db(temp_dir: &Path) -> DatabaseConnection {
    let db = test_db_connection(temp_dir).await;
    crate::jupiter::migration::apply_migrations(&db, true)
        .await
        .expect("apply migrations");
    db
}

/// Migrated schema for GIN catalog checks. The shared `pg_trgm` extension stays
/// in the test server database; this helper does not drop it.
pub async fn trace_search_catalog_db(temp_dir: &Path) -> DatabaseConnection {
    migrated_trace_test_db(temp_dir).await
}

async fn connect_guarded_database(kind: TestDatabaseKind) -> DatabaseConnection {
    let (db_url, schema) = create_test_database(kind).await;

    let mut opt = ConnectOptions::new(db_url);
    // FIX-OX-26: a loaded `cargo test --all` can push concurrent requests
    // (e.g. a spawned held-range task plus the main body) past a two-connection
    // pool, surfacing as `500 INTERNAL: Failed to acquire connection from pool`.
    // The test Postgres allows up to 500 connections, so a larger per-test pool
    // is safe and removes that artificial starvation without affecting any
    // assertion. Bounds stay generous so only a genuinely stuck connection fails
    // the run instead of hanging it.
    opt.max_connections(8)
        .min_connections(1)
        // Generous on purpose (FIX-05, same rationale as `test_db_config`):
        // a full `cargo test --all` runs dozens of tests applying full-schema
        // migrations concurrently; acquire waits reflect machine load, not
        // broken code. A timeout still exists so a genuinely stuck connection
        // fails the run instead of hanging it.
        .acquire_timeout(std::time::Duration::from_secs(180))
        .connect_timeout(std::time::Duration::from_secs(60))
        // Same 60s bound as `setup_option`: long enough to keep a session
        // (and its pg_temp tables) for the test, short enough that an idle
        // pool slot is returned to the server before the next wave of tests.
        .idle_timeout(std::time::Duration::from_secs(60))
        .max_lifetime(std::time::Duration::from_secs(60))
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
/// Migrated schema with no qualified family. Registration tests start here.
pub async fn test_unprovisioned_db_config(_temp_dir: &Path) -> (DbConfig, TestSchemaGuard) {
    test_db_config_kind(TestDatabaseKind::MigratedOnly).await
}

/// Empty schema. Migration tests apply a prefix of the history themselves.
pub async fn test_blank_db_connection(_temp_dir: &Path) -> DatabaseConnection {
    connect_guarded_database(TestDatabaseKind::Blank).await
}

/// Empty schema, as a config. The read-only gate needs a database with no tables.
pub async fn test_blank_db_config(_temp_dir: &Path) -> (DbConfig, TestSchemaGuard) {
    test_db_config_kind(TestDatabaseKind::Blank).await
}

pub async fn test_db_config(_temp_dir: &Path) -> (DbConfig, TestSchemaGuard) {
    test_db_config_kind(TestDatabaseKind::Default).await
}

async fn test_db_config_kind(kind: TestDatabaseKind) -> (DbConfig, TestSchemaGuard) {
    let (db_url, schema) = create_test_database(kind).await;
    let config = DbConfig {
        db_type: "postgres".to_owned(),
        db_url,
        // FIX-OX-26: see `test_db_connection`. A two-connection pool is too
        // small for the concurrent requests a heavy snapshot test can issue
        // and gets starved under a loaded run.
        max_connection: 8,
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
        acquire_timeout: 180,
        connect_timeout: 30,
        sqlx_logging: false,
    };
    (config, schema)
}

/// One per-test database, dropped (`DROP DATABASE`) by whichever of
/// its holders lets go first: the [`TestSchemaGuard`], or the test thread's
/// exit for a schema something kept alive (libvault's modules hold `Arc`
/// cycles, so a vault's pool is never freed).
struct TestSchema {
    admin_url: String,
    schema: String,
    owner: String,
    removed: Mutex<bool>,
}

impl TestSchema {
    /// Drops the schema from any thread, a tokio worker included (where
    /// `block_on` panics and a task spawned there would die with the test's
    /// runtime): the work gets a thread and runtime of its own and is waited
    /// for, so a test binary never exits with a drop still in flight.
    fn drop_off_thread(&self, thread_ended: bool) {
        let mut removed = self.removed.lock().unwrap_or_else(|err| err.into_inner());
        if *removed {
            return;
        }
        if thread_ended {
            eprintln!(
                "test schema {} was still held when {} ended; dropping it now",
                self.schema, self.owner
            );
        }
        let admin_url = self.admin_url.clone();
        let schema = self.schema.clone();
        let worker = std::thread::Builder::new()
            .name(format!("drop-{schema}"))
            .spawn(move || drop_test_schema(&admin_url, &schema));
        // Keep the per-database lock until the outcome is known. A failed
        // attempt must leave the thread-exit fallback eligible to try again.
        *removed = match worker {
            Ok(worker) => worker.join().unwrap_or_else(|_| {
                eprintln!("test schema left behind: the drop thread panicked");
                false
            }),
            Err(err) => {
                eprintln!("test schema left behind: no drop thread: {err}");
                false
            }
        };
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
        self.0.drop_off_thread(false);
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
            schema.drop_off_thread(true);
        }
    }
}

fn drop_test_schema(admin_url: &str, schema: &str) -> bool {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("test schema {schema} left behind: no runtime for the drop: {err}");
            return false;
        }
    };
    let dropped = runtime.block_on(async {
        let admin = Database::connect(admin_connect_options(admin_url)).await?;
        // Stop new sessions attaching while the pool's last backends exit.
        // Terminating them from here deadlocks with a backend that is still
        // starting, so the drop itself waits only for sessions that already
        // exist.
        let _ = admin
            .execute_unprepared(&format!(
                "ALTER DATABASE {schema} WITH ALLOW_CONNECTIONS false"
            ))
            .await;
        // Only this database. `DROP ... WITH (FORCE)` waits on every backend
        // that is still starting, including one connecting to a different
        // database.
        // timeout 0 signals and returns. Waiting here blocks on a backend
        // that is still starting, and that backend blocks on this drop.
        let _ = admin
            .execute_unprepared(&format!(
                "SELECT pg_terminate_backend(pid, 0) FROM pg_stat_activity \
                 WHERE datname = '{schema}' AND pid <> pg_backend_pid()"
            ))
            .await;
        // A barrier wait inside DROP deadlocks with a backend that is still
        // starting. Cancel that wait so the other session can finish, then
        // retry once it has gone.
        let _ = admin
            .execute_unprepared("SET statement_timeout = '500ms'")
            .await;
        let drop_sql = format!("DROP DATABASE IF EXISTS {schema}");
        let mut dropped = admin.execute_unprepared(&drop_sql).await;
        for _ in 0..20 {
            if dropped.is_ok() {
                break;
            }
            let text = format!("{dropped:?}");
            if !text.contains("being accessed by other users")
                && !text.contains("statement timeout")
                && !text.contains("canceling statement")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = admin
                .execute_unprepared(&format!(
                    "SELECT pg_terminate_backend(pid, 0) FROM pg_stat_activity \
                     WHERE datname = '{schema}' AND pid <> pg_backend_pid()"
                ))
                .await;
            dropped = admin.execute_unprepared(&drop_sql).await;
        }
        dropped?;
        if let Err(err) = admin.close().await {
            eprintln!(
                "test database {schema} removed but its admin connection did not close: {err}"
            );
        }
        Ok::<(), sea_orm::DbErr>(())
    });
    match dropped {
        Ok(()) => true,
        Err(err) => {
            eprintln!("test schema {schema} left behind: {err}");
            false
        }
    }
}

const TEST_TEMPLATE_DATABASE: &str = "mega2_tpl";
const TEST_TEMPLATE_BASE: &str = "mega2_tpl_base";
const TEST_TEMPLATE_SCHEMA: &str = "mega2_test_template";

#[derive(Clone, Copy)]
enum TestDatabaseKind {
    /// Provisioned template, unless this task is building generic history.
    Default,
    /// Migrations applied, qualified family not installed.
    MigratedOnly,
    /// No tables. The caller applies the migration prefix it wants to observe.
    Blank,
}

async fn create_test_database(kind: TestDatabaseKind) -> (String, TestSchemaGuard) {
    let admin_url =
        std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
    assert_postgres_url(&admin_url);
    let kind = match kind {
        TestDatabaseKind::Default
            if crate::jupiter::storage::init::generic_history_bootstrap_active() =>
        {
            TestDatabaseKind::MigratedOnly
        }
        other => other,
    };
    let provisioned = matches!(kind, TestDatabaseKind::Default);
    if !matches!(kind, TestDatabaseKind::Blank) {
        ensure_test_template(&admin_url, provisioned).await;
    }
    let schema = format!(
        "mega2_test_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4().as_u128()
    );

    let admin = Database::connect(admin_connect_options(&admin_url))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "test PostgreSQL is not available; run `docker compose -f docker/docker-compose.test.yml up -d` first"
            )
        });
    // Cloning the migrated template is the per-test setup. Replaying migrations
    // here is what made a single fixture take minutes.
    // CREATE must reject a name left by an earlier process with the same PID.
    // Such a database belongs to the baseline, never to this test instance.
    if matches!(kind, TestDatabaseKind::Blank) {
        execute_postgres(&admin, format!("CREATE DATABASE {schema}")).await;
    } else {
        // Postgres rejects `CREATE DATABASE ... TEMPLATE` while anything, including
        // autovacuum, is connected to the template. The template itself disallows
        // new connections; this retry covers a peer clone that started first.
        let template = if provisioned {
            TEST_TEMPLATE_DATABASE
        } else {
            TEST_TEMPLATE_BASE
        };
        let create_sql = format!("CREATE DATABASE {schema} TEMPLATE {template}");
        let mut created = false;
        for attempt in 0..16 {
            match admin
                .execute_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    create_sql.clone(),
                ))
                .await
            {
                Ok(_) => {
                    created = true;
                    break;
                }
                Err(err) => {
                    let text = format!("{err:?}");
                    if text.contains("being accessed by other users") && attempt + 1 < 16 {
                        tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                        continue;
                    }
                    panic!("failed to prepare PostgreSQL test schema: {text}");
                }
            }
        }
        assert!(created, "template {template} stayed busy");
        // A clone still carries the template's schema name, database oid, and
        // migration-time UUIDs. Tests address the schema by this database's name.
        personalize_cloned_database(&admin_url, &schema).await;
    }
    let owner = std::thread::current()
        .name()
        .unwrap_or("unnamed thread")
        .to_owned();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "COMMENT ON DATABASE {schema} IS '{}'",
        owner.replace('\'', "''")
    )))
    .execute(admin.get_postgres_connection_pool())
    .await
    .expect("failed to label PostgreSQL test database");

    if matches!(kind, TestDatabaseKind::Blank) {
        let db = Database::connect(admin_connect_options(&database_url_for_schema(
            &admin_url, &schema, "public",
        )))
        .await
        .expect("connect blank test database");
        execute_postgres(&db, format!("CREATE SCHEMA {schema}")).await;
        db.close().await.expect("close blank test database");
    }
    let db_url = database_url_for_schema(&admin_url, &schema, &schema);
    let schema = Arc::new(TestSchema {
        admin_url,
        schema,
        owner,
        removed: Mutex::new(false),
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

fn database_url_for_schema(admin_url: &str, database: &str, search_path: &str) -> String {
    let mut url = Url::parse(admin_url).expect("MEGA_DATABASE__DB_URL must be a valid URL");
    url.set_path(&format!("/{database}"));
    url.query_pairs_mut()
        .append_pair(
            "options",
            &format!("-csearch_path={search_path},public -clock_timeout=120s"),
        )
        // Tags the backends of this database so a drop can find them.
        .append_pair("application_name", database);
    url.to_string()
}

async fn personalize_cloned_database(admin_url: &str, database: &str) {
    let old = TEST_TEMPLATE_SCHEMA;
    let db = Database::connect(admin_connect_options(&database_url_for_schema(
        admin_url, database, old,
    )))
    .await
    .expect("connect cloned test database");
    // Rename keeps the schema oid. Functions and rows still name the template
    // schema, and every clone shares the migration-time UUIDs, so rewrite both
    // before any test connection observes them.
    execute_postgres(&db, format!("ALTER SCHEMA {old} RENAME TO {database}")).await;
    execute_postgres(&db, format!("SET search_path TO {database}, public")).await;
    execute_postgres(&db, "SET session_replication_role = replica".to_string()).await;
    execute_postgres(
        &db,
        format!(
            "UPDATE pg_catalog.pg_proc SET prosrc = replace(prosrc, '{old}', '{database}') \
             WHERE prosrc LIKE '%{old}%' AND pronamespace IN ( \
               SELECT oid FROM pg_catalog.pg_namespace \
               WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema')"
        ),
    )
    .await;
    execute_postgres(
        &db,
        format!(
            "UPDATE pg_catalog.pg_proc SET proconfig = ARRAY( \
               SELECT replace(item, '{old}', '{database}') FROM unnest(proconfig) AS item) \
             WHERE proconfig IS NOT NULL AND EXISTS ( \
               SELECT 1 FROM unnest(proconfig) AS item WHERE item LIKE '%{old}%') \
               AND pronamespace IN ( \
               SELECT oid FROM pg_catalog.pg_namespace \
               WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema')"
        ),
    )
    .await;
    execute_postgres(
        &db,
        format!(
            "UPDATE {database}.mst2_metadata_namespace \
             SET core_schema = CASE WHEN core_schema = '{old}' THEN '{database}' ELSE core_schema END, \
                 metadata_schema = CASE WHEN metadata_schema = '{old}' THEN '{database}' ELSE metadata_schema END, \
                 database_name = current_database(), \
                 database_oid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database()), \
                 mono_lock_key2 = pg_catalog.hashtext('{database}'), \
                 server_address = pg_catalog.inet_server_addr()::text, \
                 server_port = pg_catalog.inet_server_port() \
             WHERE core_schema = '{old}' OR metadata_schema = '{old}'"
        ),
    )
    .await;
    execute_postgres(
        &db,
        format!(
            r#"DO $$
             DECLARE r record;
             BEGIN
               FOR r IN
                 SELECT c.relname AS table_name, a.attname AS column_name
                 FROM pg_catalog.pg_attribute a
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
                 WHERE n.nspname = '{database}' AND c.relkind = 'r' AND a.attnum > 0
                   AND NOT a.attisdropped AND a.attgenerated = '' AND a.attidentity = '' AND t.typname IN ('text', 'varchar', 'name')
                   AND NOT (c.relname = 'mst2_metadata_namespace' AND a.attname IN ('core_schema', 'metadata_schema'))
               LOOP
                 EXECUTE format('UPDATE %I.%I SET %I = replace(%I::text, %L, %L) WHERE %I::text LIKE %L',
                   '{database}', r.table_name, r.column_name, r.column_name, '{old}', '{database}',
                   r.column_name, '%{old}%');
               END LOOP;
             END $$;"#
        ),
    )
    .await;
    execute_postgres(
        &db,
        format!(
            r#"DO $$
             DECLARE old_storage text; new_storage text := gen_random_uuid()::text; r record;
                     old_id uuid; new_id uuid;
             BEGIN
               IF to_regclass('{database}.mst2_metadata_storage_scope') IS NOT NULL THEN
                 SELECT storage_uuid INTO old_storage FROM {database}.mst2_metadata_storage_scope WHERE singleton = 1;
                 IF old_storage IS NOT NULL THEN
                   FOR r IN
                     SELECT c.relname AS table_name, a.attname AS column_name
                     FROM pg_catalog.pg_attribute a
                     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                     JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
                     WHERE n.nspname = '{database}' AND c.relkind = 'r' AND a.attnum > 0
                       AND NOT a.attisdropped AND a.attgenerated = '' AND a.attidentity = '' AND t.typname IN ('text', 'varchar')
                   LOOP
                     EXECUTE format('UPDATE %I.%I SET %I = replace(%I, %L, %L) WHERE %I LIKE %L',
                       '{database}', r.table_name, r.column_name, r.column_name, old_storage, new_storage,
                       r.column_name, '%' || old_storage || '%');
                   END LOOP;
                 END IF;
               END IF;
               IF to_regclass('{database}.mst2_metadata_namespace') IS NOT NULL THEN
                 FOR old_id IN SELECT namespace_uuid FROM {database}.mst2_metadata_namespace WHERE singleton = 1 LOOP
                   new_id := gen_random_uuid();
                   UPDATE pg_catalog.pg_proc SET prosrc = replace(prosrc, old_id::text, new_id::text)
                     WHERE prosrc LIKE '%' || old_id::text || '%'
                       AND pronamespace IN (
                         SELECT oid FROM pg_catalog.pg_namespace
                         WHERE nspname NOT LIKE 'pg_%' AND nspname <> 'information_schema');
                   FOR r IN
                     SELECT c.relname AS table_name
                     FROM pg_catalog.pg_attribute a
                     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                     WHERE n.nspname = '{database}' AND c.relkind = 'r' AND a.attname = 'namespace_uuid'
                       AND NOT a.attisdropped
                   LOOP
                     EXECUTE format('UPDATE %I.%I SET namespace_uuid = %L::uuid WHERE namespace_uuid = %L::uuid',
                       '{database}', r.table_name, new_id, old_id);
                   END LOOP;
                   FOR r IN
                     SELECT c.relname AS table_name, a.attname AS column_name
                     FROM pg_catalog.pg_attribute a
                     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                     JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
                     WHERE n.nspname = '{database}' AND c.relkind = 'r' AND a.attnum > 0
                       AND NOT a.attisdropped AND a.attgenerated = '' AND a.attidentity = '' AND t.typname IN ('text', 'varchar')
                       AND a.attname <> 'namespace_uuid'
                   LOOP
                     EXECUTE format('UPDATE %I.%I SET %I = replace(%I, %L, %L) WHERE %I LIKE %L',
                       '{database}', r.table_name, r.column_name, r.column_name, old_id::text, new_id::text,
                       r.column_name, '%' || old_id::text || '%');
                   END LOOP;
                 END LOOP;
               END IF;
             END $$;"#
        ),
    )
    .await;
    // Rewriting function bodies changes the qualified catalog hash. Store the
    // hash of the rewritten catalog while triggers are still off.
    execute_postgres(
        &db,
        format!(
            r#"DO $$
             DECLARE core_oid oid; q_oid oid; q_ns uuid; q_storage text;
             BEGIN
               IF NOT EXISTS (
                 SELECT 1 FROM {database}.mst2_metadata_namespace WHERE graph_domain = 'qualified-v1'
               ) THEN
                 RETURN;
               END IF;
               SELECT n.core_schema_oid, n.metadata_schema_oid, n.namespace_uuid, n.metadata_storage_uuid
                 INTO core_oid, q_oid, q_ns, q_storage
                 FROM {database}.mst2_metadata_namespace n
                 WHERE n.graph_domain = 'qualified-v1';
               UPDATE {database}.mst2_metadata_namespace
                 SET catalog_fingerprint = mst2_route_family_catalog(core_oid, q_oid)
                 WHERE graph_domain = 'qualified-v1';
               UPDATE {database}.mst2_qualified_family_policy
                 SET authority_catalog = mst2_route_family_catalog(core_oid, 0::oid, q_oid),
                     expected_shape = mst2_route_family_shape(q_oid, q_ns, q_storage)
                 WHERE singleton = 1;
             END $$;"#
        ),
    )
    .await;
    execute_postgres(&db, "SET session_replication_role = origin".to_string()).await;
    refresh_unprovisioned_family_policy(&db).await;
    db.close().await.expect("close cloned test database");
}

/// The migration records the trusted Q shape, then drops the template schema.
/// Rewriting function bodies changes that hash, so a database with no real Q
/// row has to measure it again.
async fn refresh_unprovisioned_family_policy(db: &DatabaseConnection) {
    let qualified: i64 = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*) FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'"
                .to_string(),
        ))
        .await
        .expect("count qualified namespaces")
        .expect("qualified namespace count")
        .try_get_by_index(0)
        .expect("qualified namespace count value");
    if qualified != 0 {
        return;
    }
    let policy_exists: bool = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT to_regclass('mst2_qualified_family_policy') IS NOT NULL".to_string(),
        ))
        .await
        .expect("policy table probe")
        .expect("policy table probe row")
        .try_get_by_index(0)
        .expect("policy table probe value");
    if !policy_exists {
        return;
    }
    let core = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_schema() AS schema, n.oid::bigint AS oid \
             FROM pg_catalog.pg_namespace n WHERE n.nspname = current_schema()"
                .to_string(),
        ))
        .await
        .expect("current schema")
        .expect("current schema row");
    let schema: String = core.try_get("", "schema").expect("schema name");
    let oid: i64 = core.try_get("", "oid").expect("schema oid");
    let template_uuid = uuid::Uuid::new_v4().to_string();
    let template_schema = format!("mst2q_{}", template_uuid.replace('-', ""));
    let template_storage = uuid::Uuid::new_v4().to_string();
    execute_postgres(db, format!("CREATE SCHEMA \"{template_schema}\"")).await;
    let template_oid: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT oid::bigint FROM pg_catalog.pg_namespace WHERE nspname=$1",
            [template_schema.clone().into()],
        ))
        .await
        .expect("template schema oid")
        .expect("template schema oid row")
        .try_get_by_index(0)
        .expect("template schema oid value");
    db.execute_unprepared(
        &crate::jupiter::storage::qualified_metadata_family::render_family(
            &schema,
            oid,
            &template_schema,
            template_oid,
            &template_uuid,
            &template_storage,
        ),
    )
    .await
    .expect("install temporary family shape");
    let expected_shape: Vec<u8> = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            format!(
                "SELECT \"{schema}\".mst2_route_family_shape($1::bigint::oid,$2::uuid,$3) AS fingerprint"
            ),
            [
                template_oid.into(),
                template_uuid.into(),
                template_storage.into(),
            ],
        ))
        .await
        .expect("template shape")
        .expect("template shape row")
        .try_get("", "fingerprint")
        .expect("template shape value");
    execute_postgres(db, format!("DROP SCHEMA \"{template_schema}\" CASCADE")).await;
    let authority: Vec<u8> = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT \"{schema}\".mst2_route_family_catalog({oid},0::oid) AS fingerprint"),
        ))
        .await
        .expect("authority catalog")
        .expect("authority catalog row")
        .try_get("", "fingerprint")
        .expect("authority catalog value");
    execute_postgres(db, "SET session_replication_role = replica".to_string()).await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE mst2_qualified_family_policy \
         SET expected_shape=$1, authority_catalog=$2 WHERE singleton=1",
        [expected_shape.into(), authority.into()],
    ))
    .await
    .expect("refresh unprovisioned family policy");
    execute_postgres(db, "SET session_replication_role = origin".to_string()).await;
}

fn template_stamp() -> String {
    use sea_orm_migration::MigratorTrait;
    crate::jupiter::migration::Migrator::migrations()
        .last()
        .map(|migration| format!("{}+clone4", migration.name()))
        .unwrap_or_else(|| "none+clone4".to_string())
}

async fn ensure_test_template(admin_url: &str, provisioned: bool) {
    if provisioned {
        static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        let admin_url = admin_url.to_string();
        READY
            .get_or_init(|| async move {
                build_or_reuse_test_template(&admin_url, TEST_TEMPLATE_DATABASE, true).await
            })
            .await;
    } else {
        static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        let admin_url = admin_url.to_string();
        READY
            .get_or_init(|| async move {
                build_or_reuse_test_template(&admin_url, TEST_TEMPLATE_BASE, false).await
            })
            .await;
    }
}

async fn build_or_reuse_test_template(admin_url: &str, database: &str, provisioned: bool) {
    let admin = Database::connect(admin_connect_options(admin_url))
        .await
        .expect("test PostgreSQL is not available");
    execute_postgres(&admin, "SELECT pg_advisory_lock(872341001)".to_string()).await;
    let stamp = format!(
        "{}+{}",
        template_stamp(),
        if provisioned { "full" } else { "base" }
    );
    let current = admin
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT pg_catalog.shobj_description(oid, 'pg_database') AS stamp \
                 FROM pg_catalog.pg_database WHERE datname='{database}'"
            ),
        ))
        .await
        .expect("failed to read test template stamp")
        .and_then(|row| row.try_get::<String>("", "stamp").ok());
    if current.as_deref() != Some(stamp.as_str()) {
        execute_postgres(
            &admin,
            format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"),
        )
        .await;
        execute_postgres(&admin, format!("CREATE DATABASE {database}")).await;
        install_test_template(admin_url, &stamp, database, provisioned).await;
        execute_postgres(&admin, "SELECT pg_advisory_unlock(872341001)".to_string()).await;
        admin.close().await.expect("close admin");
        return;
    }
    execute_postgres(&admin, "SELECT pg_advisory_unlock(872341001)".to_string()).await;
    admin.close().await.expect("close admin");
}

async fn install_test_template(admin_url: &str, stamp: &str, database: &str, provisioned: bool) {
    let url = database_url_for_schema(admin_url, database, TEST_TEMPLATE_SCHEMA);
    let db = Database::connect(admin_connect_options(&url))
        .await
        .expect("connect test template");
    execute_postgres(
        &db,
        format!("CREATE SCHEMA IF NOT EXISTS {TEST_TEMPLATE_SCHEMA}"),
    )
    .await;
    execute_postgres(
        &db,
        format!("SET search_path TO {TEST_TEMPLATE_SCHEMA}, public"),
    )
    .await;
    apply_migrations(&db, false)
        .await
        .expect("migrate test template");
    if provisioned {
        crate::jupiter::storage::qualified_metadata_family::provision_or_verify_rooted_qualified_family(
            &db,
        )
        .await
        .expect("provision test template");
    }
    execute_postgres(
        &db,
        format!("COMMENT ON SCHEMA {TEST_TEMPLATE_SCHEMA} IS 'mega2-pristine'"),
    )
    .await;
    db.close().await.expect("close template");

    let admin = Database::connect(admin_connect_options(admin_url))
        .await
        .expect("rejoin admin");
    execute_postgres(
        &admin,
        format!("ALTER DATABASE {database} SET search_path TO {TEST_TEMPLATE_SCHEMA}, public"),
    )
    .await;
    execute_postgres(
        &admin,
        format!("ALTER DATABASE {database} SET synchronous_commit TO off"),
    )
    .await;
    execute_postgres(
        &admin,
        format!("ALTER DATABASE {database} SET lock_timeout TO '120s'"),
    )
    .await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "COMMENT ON DATABASE {database} IS '{}'",
        stamp.replace('\'', "''")
    )))
    .execute(admin.get_postgres_connection_pool())
    .await
    .expect("stamp test template");
    // template0 uses the same setting. `CREATE DATABASE ... TEMPLATE` can still
    // read it, and autovacuum will not open a session that blocks the clone.
    execute_postgres(
        &admin,
        format!("ALTER DATABASE {database} WITH ALLOW_CONNECTIONS false"),
    )
    .await;
    execute_postgres(
        &admin,
        format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = '{database}' AND pid <> pg_backend_pid()"
        ),
    )
    .await;
    admin.close().await.expect("close admin");
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
        view_storage: ViewStorage::new(base.clone()),
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
    let view_runtime = Arc::new(ViewRuntime::new());

    Storage {
        app_service: Arc::new(svc),
        native_projection_cache: Arc::default(),
        native_snapshot_sessions: Arc::default(),
        native_chunk_maps: Arc::default(),
        shadow_qualified_metadata: Arc::default(),
        rooted_qualified_metadata: Arc::default(),
        projection_observation_sink: None,
        cl_service: CLService::mock(),
        push_queue_service: PushQueueService::new(
            base.clone(),
            config.monorepo.push_policy.clone(),
        )
        .with_view_signal(view_runtime.signal())
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
        view_runtime,
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
static TRACES_NOEXT_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
struct TracesIsolatedDatabase {
    admin_url: String,
    name: String,
    dropped: AtomicBool,
    cleanup: Mutex<()>,
}

#[cfg(test)]
pub struct TracesIsolatedDatabaseGuard(Arc<TracesIsolatedDatabase>);

#[cfg(test)]
impl std::fmt::Debug for TracesIsolatedDatabaseGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TracesIsolatedDatabaseGuard")
            .field("name", &self.0.name)
            .finish()
    }
}

#[cfg(test)]
impl TracesIsolatedDatabase {
    fn drop_off_thread(&self) {
        let _cleanup = self.cleanup.lock().unwrap_or_else(|err| err.into_inner());
        if self.dropped.load(Ordering::Acquire) {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let worker = std::thread::Builder::new()
            .name(format!("drop-{name}"))
            .spawn(move || drop_traces_isolated_database(&admin_url, &name));
        let removed = match worker {
            Ok(worker) => worker.join().unwrap_or_else(|_| {
                eprintln!(
                    "traces isolated database {} left behind: the drop thread panicked",
                    self.name
                );
                false
            }),
            Err(err) => {
                eprintln!(
                    "traces isolated database {} left behind: no drop thread: {}",
                    self.name,
                    scrub_admin_error(err, &self.admin_url)
                );
                false
            }
        };
        if removed {
            self.dropped.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
impl Drop for TracesIsolatedDatabaseGuard {
    fn drop(&mut self) {
        self.0.drop_off_thread();
    }
}

#[cfg(test)]
thread_local! {
    static HELD_ISOLATED_DATABASES: RefCell<HeldIsolatedDatabases> =
        const { RefCell::new(HeldIsolatedDatabases(Vec::new())) };
}

#[cfg(test)]
struct HeldIsolatedDatabases(Vec<Arc<TracesIsolatedDatabase>>);

#[cfg(test)]
impl Drop for HeldIsolatedDatabases {
    fn drop(&mut self) {
        for database in self.0.drain(..) {
            database.drop_off_thread();
        }
    }
}

#[cfg(test)]
fn traces_isolated_name_is_owned(name: &str) -> bool {
    let mut parts = name.split('_');
    parts.next() == Some("traces")
        && parts.next() == Some("noext")
        && parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && parts.next().is_none()
}

#[cfg(test)]
fn scrub_admin_error(err: impl std::fmt::Debug, admin_url: &str) -> String {
    format!("{err:?}").replace(admin_url, "[redacted]")
}

#[cfg(test)]
fn drop_traces_isolated_database(admin_url: &str, name: &str) -> bool {
    if !traces_isolated_name_is_owned(name) {
        eprintln!("refusing to drop a database outside the traces_noext_ prefix");
        return false;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("traces isolated database {name} left behind: no runtime: {err}");
            return false;
        }
    };
    let dropped = runtime.block_on(async {
        let admin = Database::connect(admin_connect_options(admin_url)).await?;
        let _ = admin
            .execute_unprepared(&format!(
                "ALTER DATABASE {name} WITH ALLOW_CONNECTIONS false"
            ))
            .await;
        let _ = admin
            .execute_unprepared(&format!(
                "SELECT pg_terminate_backend(pid, 0) FROM pg_stat_activity \
                 WHERE datname = '{name}' AND pid <> pg_backend_pid()"
            ))
            .await;
        let _ = admin
            .execute_unprepared("SET statement_timeout = '500ms'")
            .await;
        let drop_sql = format!("DROP DATABASE IF EXISTS {name}");
        let mut dropped = admin.execute_unprepared(&drop_sql).await;
        for _ in 0..20 {
            if dropped.is_ok() {
                break;
            }
            let text = format!("{dropped:?}");
            if !text.contains("being accessed by other users")
                && !text.contains("statement timeout")
                && !text.contains("canceling statement")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = admin
                .execute_unprepared(&format!(
                    "SELECT pg_terminate_backend(pid, 0) FROM pg_stat_activity \
                     WHERE datname = '{name}' AND pid <> pg_backend_pid()"
                ))
                .await;
            dropped = admin.execute_unprepared(&drop_sql).await;
        }
        let confirmed = match dropped {
            Ok(_) => match admin
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1",
                    [Value::from(name)],
                ))
                .await
            {
                Ok(None) => Ok(()),
                Ok(Some(_)) => Err(sea_orm::DbErr::Custom(format!(
                    "database {name} remains after DROP"
                ))),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        if let Err(err) = admin.close().await {
            let status = if confirmed.is_ok() {
                "removed"
            } else {
                "cleanup unconfirmed"
            };
            eprintln!(
                "traces isolated database {name} {status}; admin connection did not close: {}",
                scrub_admin_error(err, admin_url)
            );
        }
        confirmed
    });
    match dropped {
        Ok(()) => true,
        Err(err) => {
            eprintln!(
                "traces isolated database {name} left behind: {}",
                scrub_admin_error(err, admin_url)
            );
            false
        }
    }
}

/// A database cloned from `template0`, so it does not contain the shared
/// `pg_trgm` extension. The guard drops only this `traces_noext_` database.
#[cfg(test)]
pub async fn test_database_without_extensions()
-> Result<(DatabaseConnection, TracesIsolatedDatabaseGuard), String> {
    let admin_url = std::env::var("MEGA2_IT_DB_ADMIN_URL")
        .map_err(|_| "MEGA2_IT_DB_ADMIN_URL is unset".to_string())?;
    create_traces_isolated_database(&admin_url).await
}

#[cfg(test)]
async fn create_traces_isolated_database(
    admin_url: &str,
) -> Result<(DatabaseConnection, TracesIsolatedDatabaseGuard), String> {
    assert_postgres_url(admin_url);
    let name = format!(
        "traces_noext_{}_{}",
        std::process::id(),
        TRACES_NOEXT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    if !traces_isolated_name_is_owned(&name) {
        return Err("refusing to create a database outside the traces_noext_ prefix".to_string());
    }
    let admin = Database::connect(admin_connect_options(admin_url))
        .await
        .map_err(|err| {
            format!(
                "traces isolated database admin connection failed: {}",
                scrub_admin_error(err, admin_url)
            )
        })?;
    let create_sql = format!("CREATE DATABASE {name} TEMPLATE template0");
    let mut created = false;
    let mut last_error = String::new();
    for attempt in 0..16 {
        match admin
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                create_sql.clone(),
            ))
            .await
        {
            Ok(_) => {
                created = true;
                break;
            }
            Err(err) => {
                let text = scrub_admin_error(&err, admin_url);
                if text.contains("being accessed by other users") && attempt + 1 < 16 {
                    tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    continue;
                }
                last_error = text;
                break;
            }
        }
    }
    admin
        .close()
        .await
        .map_err(|err| scrub_admin_error(err, admin_url))?;
    if !created {
        return Err(last_error);
    }
    let db_url = database_url_for_schema(admin_url, &name, "public");
    let connection = Database::connect(admin_connect_options(&db_url))
        .await
        .map_err(|err| scrub_admin_error(err, admin_url))?;
    let owned = Arc::new(TracesIsolatedDatabase {
        admin_url: admin_url.to_owned(),
        name,
        dropped: AtomicBool::new(false),
        cleanup: Mutex::new(()),
    });
    let _ = HELD_ISOLATED_DATABASES.try_with(|held| held.borrow_mut().0.push(owned.clone()));
    Ok((connection, TracesIsolatedDatabaseGuard(owned)))
}

#[cfg(test)]
mod traces_isolated_database_tests {
    use super::*;

    async fn extension_count(admin_url: &str) -> i64 {
        let admin = Database::connect(admin_connect_options(admin_url))
            .await
            .expect("admin connection");
        let count = admin
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'"
                    .to_owned(),
            ))
            .await
            .expect("extension count")
            .expect("extension count row")
            .try_get_by_index::<i64>(0)
            .expect("extension count value");
        admin.close().await.expect("close admin");
        count
    }

    async fn database_exists(admin_url: &str, name: &str) -> bool {
        let admin = Database::connect(admin_connect_options(admin_url))
            .await
            .expect("admin connection");
        let found = admin
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1",
                [Value::from(name)],
            ))
            .await
            .expect("database lookup")
            .is_some();
        admin.close().await.expect("close admin");
        found
    }

    #[tokio::test]
    async fn traces_isolated_database_template0_has_no_extension_and_guard_drops_only_itself() {
        let admin_url = {
            let _lock = crate::config::testing::env_lock();
            std::env::var("MEGA2_IT_DB_ADMIN_URL").expect("MEGA2_IT_DB_ADMIN_URL")
        };
        let before = extension_count(&admin_url).await;
        let (db, guard) = create_traces_isolated_database(&admin_url)
            .await
            .expect("template0 database");
        let name = guard.0.name.clone();
        assert!(traces_isolated_name_is_owned(&name), "{name}");
        let extension = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT extname FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'".to_owned(),
            ))
            .await
            .expect("extension query");
        assert!(extension.is_none(), "template0 database contains pg_trgm");
        let schema = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT nspname FROM pg_catalog.pg_namespace WHERE nspname = 'mega2_extensions'"
                    .to_owned(),
            ))
            .await
            .expect("schema query");
        assert!(
            schema.is_none(),
            "template0 database contains mega2_extensions"
        );
        apply_migrations(&db, true)
            .await
            .expect("migrations on a database without the extension");
        let extension_after = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT extname FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'".to_owned(),
            ))
            .await
            .expect("extension query after migrations");
        assert!(extension_after.is_none());
        db.close().await.expect("close isolated database");
        drop(guard);
        assert!(
            !database_exists(&admin_url, &name).await,
            "{name} survived its guard"
        );
        assert_eq!(extension_count(&admin_url).await, before);
    }

    #[test]
    fn traces_isolated_database_missing_admin_url_fails() {
        let lock = crate::config::testing::env_lock();
        let _removed = crate::config::testing::EnvVarGuard::remove(&lock, "MEGA2_IT_DB_ADMIN_URL");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = runtime
            .block_on(test_database_without_extensions())
            .expect_err("missing admin URL must fail");
        assert_eq!(err, "MEGA2_IT_DB_ADMIN_URL is unset");
        assert!(!err.contains("postgres://"));
    }

    #[tokio::test]
    async fn traces_isolated_database_without_createdb_fails() {
        let admin_url = {
            let _lock = crate::config::testing::env_lock();
            std::env::var("MEGA2_IT_DB_ADMIN_URL").expect("MEGA2_IT_DB_ADMIN_URL")
        };
        let role = format!(
            "traces_nocreatedb_{}_{}",
            std::process::id(),
            TRACES_NOEXT_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let password = format!("pw{role}");
        let admin = Database::connect(admin_connect_options(&admin_url))
            .await
            .expect("admin connection");
        admin
            .execute_unprepared(&format!(
                "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOCREATEDB NOSUPERUSER NOINHERIT"
            ))
            .await
            .expect("create role without CREATEDB");
        admin.close().await.expect("close admin");

        let mut weak = Url::parse(&admin_url).expect("admin url");
        weak.set_username(&role).expect("role name");
        weak.set_password(Some(&password)).expect("password");
        let weak_url = weak.to_string();
        let result = create_traces_isolated_database(&weak_url).await;
        let cleanup = Database::connect(admin_connect_options(&admin_url))
            .await
            .expect("admin connection");
        cleanup
            .execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .expect("drop role");
        cleanup.close().await.expect("close admin");

        let err = result.expect_err("missing CREATEDB must fail");
        assert!(
            !err.contains(&password),
            "the failure included the role password"
        );
        assert!(
            !err.contains("postgres://"),
            "the failure included a database URL"
        );
        let lowered = err.to_ascii_lowercase();
        assert!(
            lowered.contains("createdb") || lowered.contains("permission"),
            "failure was not an explicit admin rejection"
        );
    }

    // FIX_MTR25_REPRO_TESTS_BEGIN

    struct Fix25OwnedCleanup {
        admin_url: String,
        name: String,
        armed: bool,
    }

    impl Fix25OwnedCleanup {
        fn new(admin_url: &str, name: &str) -> Self {
            assert!(traces_isolated_name_is_owned(name));
            Self {
                admin_url: admin_url.to_owned(),
                name: name.to_owned(),
                armed: true,
            }
        }

        fn cleanup_owned(&self) {
            let admin_url = self.admin_url.clone();
            let name = self.name.clone();
            let worker = std::thread::Builder::new()
                .name(format!("fix25-rescue-{name}"))
                .spawn(move || {
                    drop_traces_isolated_database(&admin_url, &name);
                });
            match worker {
                Ok(worker) => {
                    if worker.join().is_err() {
                        eprintln!("FIX25 owned cleanup worker panicked");
                    }
                }
                Err(_) => eprintln!("FIX25 owned cleanup worker could not start"),
            }
        }
    }

    impl Drop for Fix25OwnedCleanup {
        fn drop(&mut self) {
            if self.armed {
                self.cleanup_owned();
            }
        }
    }

    fn fix25_admin_url() -> String {
        let _lock = crate::config::testing::env_lock();
        std::env::var("MEGA2_IT_DB_ADMIN_URL").expect("MEGA2_IT_DB_ADMIN_URL")
    }

    fn fix25_bad_admin_url(admin_url: &str) -> String {
        let mut url = Url::parse(admin_url).expect("FIX25 admin URL");
        assert!(
            matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
            ),
            "FIX25 auth failure must use the qualified loopback endpoint"
        );
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        url.set_username(&format!("fix25_rejected_{}_{nonce}", std::process::id()))
            .expect("synthetic invalid account");
        url.set_password(Some(&format!("fix25-synthetic-invalid-{nonce}")))
            .expect("synthetic invalid password");
        url.to_string()
    }

    fn fix25_remove_own_creator_tls(guard: &TracesIsolatedDatabaseGuard) {
        HELD_ISOLATED_DATABASES.with(|held| {
            let mut held = held.borrow_mut();
            let index = held
                .0
                .iter()
                .position(|entry| Arc::ptr_eq(entry, &guard.0))
                .expect("the factory registered this guard on its creator thread");
            let removed = held.0.remove(index);
            assert!(Arc::ptr_eq(&removed, &guard.0));
            drop(removed);
            assert!(!held.0.iter().any(|entry| Arc::ptr_eq(entry, &guard.0)));
        });
        assert_eq!(Arc::strong_count(&guard.0), 1);
    }

    #[tokio::test]
    async fn traces_isolated_database_failed_cleanup_remains_retryable() {
        let admin_url = fix25_admin_url();
        let control_extensions_before = extension_count(&admin_url).await;
        let (db, mut guard) = create_traces_isolated_database(&admin_url)
            .await
            .expect("FIX25 real isolated database");
        let name = guard.0.name.clone();
        let mut recovery = Fix25OwnedCleanup::new(&admin_url, &name);
        db.close().await.expect("close FIX25 owned database pool");
        fix25_remove_own_creator_tls(&guard);
        Arc::get_mut(&mut guard.0)
            .expect("the real guard is the only owner")
            .admin_url = fix25_bad_admin_url(&admin_url);
        let mut owner = Arc::clone(&guard.0);
        drop(guard);
        let completed_after_failure = owner.dropped.load(Ordering::Acquire);
        let existed_after_failure = database_exists(&admin_url, &name).await;
        assert_eq!(Arc::strong_count(&owner), 1);
        Arc::get_mut(&mut owner)
            .expect("the same instance is uniquely owned after guard drop")
            .admin_url = admin_url.clone();

        // Exercise the same owner's real retry path before checking the
        // captured completion flag. No completion flag is modified here.
        owner.drop_off_thread();
        let removed_by_explicit_retry = !database_exists(&admin_url, &name).await;
        if !removed_by_explicit_retry {
            recovery.cleanup_owned();
        }
        let absent_after_cleanup = !database_exists(&admin_url, &name).await;
        if absent_after_cleanup {
            recovery.armed = false;
        }
        let control_extensions_after = extension_count(&admin_url).await;
        assert!(absent_after_cleanup, "FIX25 owned database cleanup failed");
        assert_eq!(control_extensions_after, control_extensions_before);
        assert!(
            existed_after_failure,
            "the forced connect failure did not leave the owned database"
        );
        assert!(
            removed_by_explicit_retry,
            "the same owner's explicit cleanup retry failed"
        );
        assert!(
            owner.dropped.load(Ordering::Acquire),
            "confirmed absent database must be complete"
        );
        assert!(
            !completed_after_failure,
            "FIX_MTR25_REPRO_ORACLE: failed cleanup marked complete"
        );
    }

    #[test]
    fn traces_isolated_database_creator_tls_retries_failed_cleanup() {
        let admin_url = fix25_admin_url();
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("FIX25 independent observer runtime");
        let control_extensions_before = observer.block_on(extension_count(&admin_url));
        let creator_admin_url = admin_url.clone();
        let creator = std::thread::Builder::new()
            .name("fix25-real-creator".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("FIX25 creator current-thread runtime");
                let result = runtime.block_on(async {
                    let (db, mut guard) = create_traces_isolated_database(&creator_admin_url)
                        .await
                        .expect("FIX25 creator's real isolated database");
                    let name = guard.0.name.clone();
                    let recovery = Fix25OwnedCleanup::new(&creator_admin_url, &name);
                    db.close()
                        .await
                        .expect("close creator's owned database pool");
                    fix25_remove_own_creator_tls(&guard);
                    Arc::get_mut(&mut guard.0)
                        .expect("creator owns the unique real guard")
                        .admin_url = fix25_bad_admin_url(&creator_admin_url);
                    let mut owner = Arc::clone(&guard.0);
                    drop(guard);
                    let completed_after_failure = owner.dropped.load(Ordering::Acquire);
                    let existed_after_failure = database_exists(&creator_admin_url, &name).await;
                    assert_eq!(Arc::strong_count(&owner), 1);
                    Arc::get_mut(&mut owner)
                        .expect("creator can restore its own connection parameters")
                        .admin_url = creator_admin_url.clone();
                    // Rearm only this factory-created Arc in the actual creator
                    // TLS. The returned Arc keeps its completion observable.
                    HELD_ISOLATED_DATABASES.with(|held| {
                        let mut held = held.borrow_mut();
                        assert!(!held.0.iter().any(|entry| Arc::ptr_eq(entry, &owner)));
                        held.0.push(Arc::clone(&owner));
                    });
                    assert_eq!(Arc::strong_count(&owner), 2);
                    (
                        owner,
                        recovery,
                        completed_after_failure,
                        existed_after_failure,
                    )
                });
                drop(runtime);
                result
            })
            .expect("start the real FIX25 creator OS thread");
        let (owner, mut recovery, completed_after_failure, existed_after_failure) = creator
            .join()
            .expect("join the real creator including TLS exit");
        assert_eq!(Arc::strong_count(&owner), 1);
        let removed_by_creator_tls =
            observer.block_on(async { !database_exists(&admin_url, &recovery.name).await });
        let completed_after_creator_tls = owner.dropped.load(Ordering::Acquire);
        // The OLD TLS skips the already-claimed instance. Preserve that
        // observation, then close only this owned database before the oracle.
        if !removed_by_creator_tls {
            recovery.cleanup_owned();
        }
        let absent_after_cleanup =
            observer.block_on(async { !database_exists(&admin_url, &recovery.name).await });
        if absent_after_cleanup {
            recovery.armed = false;
        }
        let control_extensions_after = observer.block_on(extension_count(&admin_url));
        assert!(
            absent_after_cleanup,
            "FIX25 creator's owned database cleanup failed"
        );
        assert_eq!(control_extensions_after, control_extensions_before);
        assert!(
            existed_after_failure,
            "the creator's forced connect failure did not leave its database"
        );
        assert!(
            !completed_after_failure && removed_by_creator_tls && completed_after_creator_tls,
            "FIX_MTR25_REPRO_ORACLE: failed cleanup marked complete"
        );
    }

    // FIX_MTR25_REPRO_TESTS_END
}

#[cfg(test)]
mod schema_guard_tests {
    use sea_orm::TransactionTrait;

    use super::*;

    async fn current_database(db: &DatabaseConnection) -> String {
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database()".to_owned(),
        ))
        .await
        .expect("current_database query")
        .expect("current_database row")
        .try_get_by_index::<String>(0)
        .expect("current_database value")
    }

    async fn database_exists(name: &str) -> bool {
        let admin_url = std::env::var("MEGA_DATABASE__DB_URL")
            .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
        let admin = Database::connect(admin_connect_options(&admin_url))
            .await
            .expect("admin connection");
        admin
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT 1 FROM pg_database WHERE datname = $1",
                [Value::from(name)],
            ))
            .await
            .expect("pg_database query")
            .is_some()
    }

    #[tokio::test]
    async fn the_schema_goes_with_the_last_handle_to_the_connection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let schema = current_database(&db).await;
        assert!(schema.starts_with("mega2_test_"), "{schema}");
        assert!(database_exists(&schema).await);

        let clone = db.clone();
        drop(db);
        assert!(
            database_exists(&schema).await,
            "a live clone must keep the database"
        );

        drop(clone);
        assert!(
            !database_exists(&schema).await,
            "database {schema} outlived its connection"
        );
    }

    #[tokio::test]
    async fn a_panicking_test_still_drops_its_schema() {
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel();
        let failed = tokio::spawn(async move {
            let temp = tempfile::tempdir().expect("temp dir");
            let db = test_db_connection(temp.path()).await;
            let _ = schema_tx.send(current_database(&db).await);
            panic!("simulated test failure while holding the connection");
        })
        .await;
        assert!(failed.is_err(), "the task must have panicked");

        let schema = schema_rx.await.expect("schema name sent before the panic");
        assert!(
            !database_exists(&schema).await,
            "database {schema} survived the panic"
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
                    .send(current_database(&db).await)
                    .expect("database name sent");
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
            .block_on(database_exists(&schema));
        assert!(
            !exists,
            "database {schema} outlived the thread that never released it"
        );
    }

    #[tokio::test]
    async fn the_config_guard_cuts_a_lingering_transaction_instead_of_waiting_on_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (db_config, guard) = test_db_config(temp.path()).await;
        let db = crate::jupiter::storage::init::database_connection(&db_config)
            .await
            .expect("connection through the config");
        let schema = current_database(&db).await;
        assert_eq!(schema, guard.schema());

        // An open transaction with a lock the drop needs: without the
        // terminate step the drop would wait on it for as long as it lives.
        let txn = db.begin().await.expect("transaction");
        txn.execute_unprepared("CREATE TABLE lock_holder (id int)")
            .await
            .expect("create table in transaction");

        drop(guard);
        assert!(
            !database_exists(&schema).await,
            "database {schema} outlived its guard"
        );
        drop(txn);
    }

    const DB_CLEANUP_REPRO_CASE: &str = "ceres::snapshot::pages::tests::mst2_projection_checks_full_leaf_path_before_resolve_and_http_reads";

    fn db_cleanup_repro_emit(prefix: &str, receipt: &serde_json::Value) -> std::io::Result<()> {
        use std::io::Write;

        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{prefix} {receipt}")?;
        stdout.flush()
    }

    fn db_cleanup_repro_process_start(pid: u32) -> String {
        let mut probe = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("FIX_MTR14_REPRO_SETUP: read process start");
        let started = std::time::Instant::now();
        while probe
            .try_wait()
            .expect("FIX_MTR14_REPRO_SETUP: process identity probe")
            .is_none()
        {
            if started.elapsed() >= Duration::from_secs(2) {
                let _ = probe.kill();
                let _ = probe.wait();
                panic!("FIX_MTR14_REPRO_SETUP: process identity probe timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = probe
            .wait_with_output()
            .expect("FIX_MTR14_REPRO_SETUP: process identity output");
        assert!(output.status.success(), "FIX_MTR14_REPRO_SETUP: ps failed");
        let start = String::from_utf8(output.stdout)
            .expect("FIX_MTR14_REPRO_SETUP: process start is not UTF-8")
            .trim()
            .to_owned();
        assert!(!start.is_empty(), "FIX_MTR14_REPRO_SETUP: no process start");
        start
    }

    type DbCleanupReproCatalog = Vec<(String, i64, String)>;

    fn db_cleanup_repro_catalog(prefixes: &[String]) -> (String, DbCleanupReproCatalog) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("FIX_MTR14_REPRO_SETUP: admin runtime");
        runtime.block_on(async {
            let admin_url = std::env::var("MEGA_DATABASE__DB_URL")
                .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
            let admin = Database::connect(admin_connect_options(&admin_url))
                .await
                .expect("FIX_MTR14_REPRO_SETUP: admin connection");
            let owner: String = admin
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT current_user::text AS owner".to_owned(),
                ))
                .await
                .expect("FIX_MTR14_REPRO_SETUP: admin owner query")
                .expect("FIX_MTR14_REPRO_SETUP: admin owner row")
                .try_get("", "owner")
                .expect("FIX_MTR14_REPRO_SETUP: admin owner value");
            let predicates = (1..=prefixes.len())
                .map(|index| {
                    format!("left(datname::text, char_length(${index}::text)) = ${index}::text")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let rows = admin
                .query_all_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    format!(
                        "SELECT datname::text AS name, oid::bigint AS oid, \
                         pg_get_userbyid(datdba)::text AS owner FROM pg_database \
                         WHERE {predicates} ORDER BY datname"
                    ),
                    prefixes.iter().cloned().map(Value::from),
                ))
                .await
                .expect("FIX_MTR14_REPRO_SETUP: catalog query");
            admin
                .close()
                .await
                .expect("FIX_MTR14_REPRO_SETUP: close admin before continuing");
            let databases = rows
                .into_iter()
                .map(|row| {
                    (
                        row.try_get("", "name").expect("repro database name"),
                        row.try_get("", "oid").expect("repro database OID"),
                        row.try_get("", "owner").expect("repro database owner"),
                    )
                })
                .collect();
            (owner, databases)
        })
    }

    struct DbCleanupReproChild {
        child: std::process::Child,
        start: String,
        started: std::time::Instant,
        raw_log_dir: std::path::PathBuf,
        status: Option<std::process::ExitStatus>,
        one_pass: Option<bool>,
        timed_out: bool,
        stopped_on_unwind: bool,
    }

    impl DbCleanupReproChild {
        fn receipt(&self, phase: &str) -> std::io::Result<()> {
            let receipt = serde_json::json!({
                "phase": phase,
                "pid": self.child.id(),
                "start": self.start,
                "raw_log_dir": self.raw_log_dir,
                "stdout": self.raw_log_dir.join("stdout.log"),
                "stderr": self.raw_log_dir.join("stderr.log"),
                "libtest_log": self.raw_log_dir.join("libtest.log"),
                "exit_code": self.status.as_ref().and_then(std::process::ExitStatus::code),
                "success": self.status.as_ref().is_some_and(std::process::ExitStatus::success),
                "one_pass": self.one_pass,
                "timed_out": self.timed_out,
                "stopped_on_unwind": self.stopped_on_unwind,
                "elapsed_seconds": self.started.elapsed().as_secs_f64(),
            });
            let filename = if phase == "spawn" {
                "identity.json"
            } else {
                "terminal.json"
            };
            std::fs::write(self.raw_log_dir.join(filename), receipt.to_string())?;
            db_cleanup_repro_emit("FIX_MTR14_REPRO_CHILD", &receipt)
        }

        fn exactly_one_pass(&self) -> bool {
            let Ok(logfile) = std::fs::read_to_string(self.raw_log_dir.join("libtest.log")) else {
                return false;
            };
            let Ok(stdout) = std::fs::read_to_string(self.raw_log_dir.join("stdout.log")) else {
                return false;
            };
            let outcomes = logfile
                .lines()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            let summaries = stdout
                .lines()
                .filter(|line| line.starts_with("test result:"))
                .collect::<Vec<_>>();
            let expected = format!("ok {DB_CLEANUP_REPRO_CASE}");
            outcomes == [expected.as_str()]
                && summaries.len() == 1
                && summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")
        }
    }

    impl Drop for DbCleanupReproChild {
        fn drop(&mut self) {
            if self.status.is_none() {
                self.status = self.child.try_wait().ok().flatten();
                if self.status.is_none() {
                    self.stopped_on_unwind = true;
                    let _ = self.child.kill();
                    self.status = self.child.wait().ok();
                }
                self.one_pass = Some(false);
                let _ = self.receipt("terminal");
            }
        }
    }

    #[test]
    fn db_cleanup_repro() {
        let parent_pid = std::process::id();
        let parent_start = db_cleanup_repro_process_start(parent_pid);
        let artifacts = tempfile::Builder::new()
            .prefix(&format!("mega2-fix14-repro-{parent_pid}-"))
            .tempdir()
            .expect("FIX_MTR14_REPRO_SETUP: private log directory")
            .keep();
        let (owner, baseline) = db_cleanup_repro_catalog(&["mega2_test_".to_owned()]);
        std::fs::write(
            artifacts.join("baseline.json"),
            serde_json::json!({
                "owner": owner, "databases": baseline,
            })
            .to_string(),
        )
        .expect("FIX_MTR14_REPRO_SETUP: record baseline");
        db_cleanup_repro_emit(
            "FIX_MTR14_REPRO_ARTIFACT",
            &serde_json::json!({
                "parent_pid": parent_pid, "parent_start": parent_start,
                "raw_log_dir": artifacts, "baseline_admin_closed": true,
                "child_case": DB_CLEANUP_REPRO_CASE, "child_count": 16,
                "child_timeout_seconds": 120, "blocked_pids": [],
                "unix_pid_collision_gate": false,
            }),
        )
        .expect("FIX_MTR14_REPRO_SETUP: emit artifact identity");

        let executable =
            std::env::current_exe().expect("FIX_MTR14_REPRO_SETUP: current executable");
        let mut children = Vec::with_capacity(16);
        for index in 0..16 {
            let raw_log_dir = artifacts.join(format!("child-{index:02}"));
            std::fs::create_dir(&raw_log_dir).expect("FIX_MTR14_REPRO_SETUP: child log directory");
            let stdout =
                std::fs::File::create(raw_log_dir.join("stdout.log")).expect("repro stdout");
            let stderr =
                std::fs::File::create(raw_log_dir.join("stderr.log")).expect("repro stderr");
            let started = std::time::Instant::now();
            let mut command = std::process::Command::new(&executable);
            command
                .args(["--exact", DB_CLEANUP_REPRO_CASE, "--nocapture", "--logfile"])
                .arg(raw_log_dir.join("libtest.log"))
                .stdin(std::process::Stdio::null())
                .stdout(stdout)
                .stderr(stderr);
            let child = command
                .spawn()
                .expect("FIX_MTR14_REPRO_SETUP: spawn exact child");
            children.push(DbCleanupReproChild {
                child,
                start: String::new(),
                started,
                raw_log_dir,
                status: None,
                one_pass: None,
                timed_out: false,
                stopped_on_unwind: false,
            });
            let child = children.last_mut().expect("repro child registered");
            child.start = db_cleanup_repro_process_start(child.child.id());
            child
                .receipt("spawn")
                .expect("FIX_MTR14_REPRO_SETUP: record child identity");
        }
        while children.iter().any(|child| child.status.is_none()) {
            for child in children.iter_mut().filter(|child| child.status.is_none()) {
                child.status = child
                    .child
                    .try_wait()
                    .expect("FIX_MTR14_REPRO_SETUP: wait child");
                if child.status.is_none() && child.started.elapsed() >= Duration::from_secs(120) {
                    child.timed_out = true;
                    child
                        .child
                        .kill()
                        .expect("FIX_MTR14_REPRO_SETUP: kill owned timed-out child");
                    child.status = Some(
                        child
                            .child
                            .wait()
                            .expect("FIX_MTR14_REPRO_SETUP: reap owned child"),
                    );
                }
                if child.status.is_some() {
                    child.one_pass = Some(
                        !child.timed_out
                            && child
                                .status
                                .as_ref()
                                .is_some_and(std::process::ExitStatus::success)
                            && child.exactly_one_pass(),
                    );
                    child
                        .receipt("terminal")
                        .expect("FIX_MTR14_REPRO_SETUP: record child terminal");
                }
            }
            if children.iter().any(|child| child.status.is_none()) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(
            children.iter().all(|child| child.one_pass == Some(true)),
            "FIX_MTR14_REPRO_CHILD_FAILURE: all 16 children must exit normally with exactly one pass"
        );
        let prefixes = children
            .iter()
            .map(|child| format!("mega2_test_{}_", child.child.id()))
            .collect::<Vec<_>>();
        let (after_owner, after) = db_cleanup_repro_catalog(&prefixes);
        assert_eq!(
            after_owner, owner,
            "FIX_MTR14_REPRO_SETUP: admin owner changed"
        );
        let residue = after
            .into_iter()
            .filter(|row| !baseline.iter().any(|old| old.0 == row.0))
            .collect::<Vec<_>>();
        assert!(
            residue.iter().all(|row| row.2 == owner
                && prefixes.iter().any(|prefix| row
                    .0
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| !suffix.is_empty()
                        && suffix.bytes().all(|byte| byte.is_ascii_digit())))),
            "FIX_MTR14_REPRO_SETUP: unexpected database ownership or name"
        );
        std::fs::write(
            artifacts.join("residue.json"),
            serde_json::to_vec(&residue).expect("serialize repro residue"),
        )
        .expect("FIX_MTR14_REPRO_SETUP: record residue");
        if !residue.is_empty() {
            panic!("FIX_MTR14_REPRO_ORACLE: owned database remains after test completion");
        }
    }

    #[cfg(unix)]
    mod db_cleanup_parallel_support {
        use std::{
            fs::{self, File, OpenOptions},
            io::{Read, Write},
            os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            path::{Path, PathBuf},
            process::{Command, Stdio},
            time::{Duration, Instant},
        };

        use serde_json::{Value as Json, json};

        use super::*;

        const ENV: &str = "MEGA2_FIX_MTR14_BARRIER_DIR";
        const CHILD_A: &str = "jupiter::tests::schema_guard_tests::db_cleanup_parallel_child_a";
        const CHILD_B: &str = "jupiter::tests::schema_guard_tests::db_cleanup_parallel_child_b";
        const LIMIT: Duration = Duration::from_secs(120);

        #[derive(Clone)]
        struct Barrier {
            directory: PathBuf,
            nonce: String,
            parent_pid: u32,
            parent_start: String,
        }

        fn text<'a>(value: &'a Json, field: &str) -> &'a str {
            value[field]
                .as_str()
                .expect("FIX_MTR14_PARALLEL_SETUP: missing text field")
        }

        fn number(value: &Json, field: &str) -> u32 {
            value[field]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .expect("FIX_MTR14_PARALLEL_SETUP: invalid PID")
        }

        fn checked_metadata(metadata: &fs::Metadata, directory: bool) {
            let uid = unsafe { libc::geteuid() };
            assert_eq!(metadata.uid(), uid, "FIX_MTR14_PARALLEL_SETUP: wrong UID");
            assert_eq!(
                metadata.permissions().mode() & 0o7777,
                if directory { 0o700 } else { 0o600 },
                "FIX_MTR14_PARALLEL_SETUP: wrong mode"
            );
            assert!(
                if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                },
                "FIX_MTR14_PARALLEL_SETUP: wrong file kind"
            );
        }

        fn read_file(path: &Path) -> Option<Json> {
            let mut file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("FIX_MTR14_PARALLEL_SETUP: barrier open: {error}"),
            };
            checked_metadata(&file.metadata().expect("barrier metadata"), false);
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(16385)
                .read_to_end(&mut bytes)
                .expect("barrier read");
            assert!(
                bytes.len() <= 16384,
                "FIX_MTR14_PARALLEL_SETUP: oversized barrier file"
            );
            Some(
                serde_json::from_slice(&bytes)
                    .expect("FIX_MTR14_PARALLEL_SETUP: malformed barrier JSON"),
            )
        }

        fn publish(barrier: &Barrier, name: &str, value: Json) {
            assert!(
                !name.contains('/'),
                "FIX_MTR14_PARALLEL_SETUP: invalid leaf"
            );
            let destination = barrier.directory.join(name);
            assert!(
                !destination.exists(),
                "FIX_MTR14_PARALLEL_SETUP: duplicate marker"
            );
            let staging = barrier
                .directory
                .join(format!(".{name}.{}", std::process::id()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&staging)
                .expect("FIX_MTR14_PARALLEL_SETUP: create marker");
            file.write_all(value.to_string().as_bytes())
                .expect("barrier write");
            file.flush().expect("barrier flush");
            drop(file);
            fs::rename(staging, destination).expect("FIX_MTR14_PARALLEL_SETUP: publish marker");
        }

        fn validate(barrier: &Barrier, value: &Json) {
            assert_eq!(
                text(value, "nonce"),
                barrier.nonce,
                "FIX_MTR14_PARALLEL_SETUP: wrong nonce"
            );
            assert_eq!(
                number(value, "parent_pid"),
                barrier.parent_pid,
                "wrong parent PID"
            );
            assert_eq!(
                text(value, "parent_start"),
                barrier.parent_start,
                "wrong parent start"
            );
        }

        fn marker(barrier: &Barrier, details: Json) -> Json {
            json!({"nonce": barrier.nonce, "parent_pid": barrier.parent_pid,
            "parent_start": barrier.parent_start, "details": details})
        }

        fn inherited_barrier() -> Option<Barrier> {
            let directory = PathBuf::from(std::env::var_os(ENV)?);
            assert!(
                directory.is_absolute(),
                "FIX_MTR14_PARALLEL_SETUP: relative barrier"
            );
            assert_eq!(
                directory.canonicalize().expect("canonical barrier"),
                directory,
                "FIX_MTR14_PARALLEL_SETUP: noncanonical barrier"
            );
            for component in directory.ancestors() {
                assert!(
                    !fs::symlink_metadata(component)
                        .expect("barrier path metadata")
                        .file_type()
                        .is_symlink(),
                    "FIX_MTR14_PARALLEL_SETUP: symlink in path"
                );
            }
            checked_metadata(
                &fs::symlink_metadata(&directory).expect("barrier directory"),
                true,
            );
            let manifest = read_file(&directory.join("owner.json"))
                .expect("FIX_MTR14_PARALLEL_SETUP: missing owner manifest");
            assert_eq!(
                text(&manifest, "protocol"),
                "mega2-fix14-parallel-v1",
                "unknown barrier protocol"
            );
            let nonce = text(&manifest, "nonce").to_owned();
            let parsed =
                uuid::Uuid::parse_str(&nonce).expect("FIX_MTR14_PARALLEL_SETUP: invalid nonce");
            assert_eq!(
                parsed.get_version_num(),
                4,
                "FIX_MTR14_PARALLEL_SETUP: nonce is not random v4"
            );
            let parent_pid = number(&manifest, "parent_pid");
            assert_eq!(
                unsafe { libc::getppid() } as u32,
                parent_pid,
                "FIX_MTR14_PARALLEL_SETUP: manifest is not the actual parent"
            );
            let parent_start = text(&manifest, "parent_start").to_owned();
            assert_eq!(
                db_cleanup_repro_process_start(parent_pid),
                parent_start,
                "FIX_MTR14_PARALLEL_SETUP: parent identity changed"
            );
            let expected = format!("mega2-fix14-parallel-{parent_pid}-{nonce}-");
            assert!(
                directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&expected) && name.len() > expected.len()),
                "FIX_MTR14_PARALLEL_SETUP: path does not bind nonce and parent"
            );
            Some(Barrier {
                directory,
                nonce,
                parent_pid,
                parent_start,
            })
        }

        async fn wait_marker(barrier: &Barrier, name: &str, deadline: Instant) -> Json {
            loop {
                if let Some(value) = read_file(&barrier.directory.join(name)) {
                    validate(barrier, &value);
                    return value;
                }
                assert!(
                    Instant::now() < deadline,
                    "FIX_MTR14_PARALLEL_SETUP: marker timeout: {name}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        async fn backend(db: &DatabaseConnection) -> i64 {
            let row = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT pg_backend_pid()::bigint AS pid, 1::bigint AS alive".to_owned(),
                ))
                .await
                .expect("FIX_MTR14_PARALLEL_ASSERT: live connection query")
                .expect("backend row");
            assert_eq!(row.try_get::<i64>("", "alive").expect("alive value"), 1);
            row.try_get("", "pid").expect("backend PID")
        }

        async fn absent(name: &str) {
            let admin_url = std::env::var("MEGA_DATABASE__DB_URL")
                .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
            let admin = Database::connect(admin_connect_options(&admin_url))
                .await
                .expect("post admin");
            let exists = admin
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT 1 FROM pg_database WHERE datname = $1",
                    [Value::from(name)],
                ))
                .await
                .expect("exact owned database query")
                .is_some();
            admin.close().await.expect("close post admin");
            assert!(
                !exists,
                "FIX_MTR14_PARALLEL_ASSERT: owned database {name} remains"
            );
        }

        async fn workload(role: &str, barrier: Option<&Barrier>, deadline: Instant) {
            let pid = std::process::id();
            let start = db_cleanup_repro_process_start(pid);
            if let Some(barrier) = barrier {
                let registration =
                    wait_marker(barrier, &format!("register-{role}.json"), deadline).await;
                assert_eq!(
                    number(&registration["details"], "pid"),
                    pid,
                    "unregistered child PID"
                );
                assert_eq!(
                    text(&registration["details"], "case"),
                    case(role),
                    "unregistered child case"
                );
                assert_eq!(
                    text(&registration["details"], "start"),
                    start,
                    "unregistered child start"
                );
            }
            let directory = tempfile::tempdir().expect("child fixture directory");
            let (own_config, own_guard) = test_db_config(directory.path()).await;
            let (control_config, control_guard) = test_db_config(directory.path()).await;
            let own_name = own_guard.schema().to_owned();
            let control_name = control_guard.schema().to_owned();
            let prefix = format!("mega2_test_{pid}_");
            assert!(own_name.starts_with(&prefix) && control_name.starts_with(&prefix));
            assert_ne!(own_name, control_name);
            // Existing one-connection admin options keep backend-PID comparison meaningful.
            let own = Database::connect(admin_connect_options(&own_config.db_url))
                .await
                .expect("own connection");
            let control = Database::connect(admin_connect_options(&control_config.db_url))
                .await
                .expect("control connection");
            let own_backend = backend(&own).await;
            let control_backend = backend(&control).await;
            if let Some(barrier) = barrier {
                publish(
                    barrier,
                    &format!("ready-{role}.json"),
                    marker(
                        barrier,
                        json!({"pid": pid, "start": start, "own": own_name,
                    "control": control_name, "own_backend": own_backend,
                    "control_backend": control_backend}),
                    ),
                );
                if role == "a" {
                    wait_marker(barrier, "release-a.json", deadline).await;
                } else {
                    let exited = wait_marker(barrier, "a-exited.json", deadline).await;
                    let registered = read_file(&barrier.directory.join("register-a.json"))
                        .expect("registered A");
                    validate(barrier, &registered);
                    assert_eq!(exited["details"]["pid"], registered["details"]["pid"]);
                    assert_eq!(exited["details"]["start"], registered["details"]["start"]);
                    assert_eq!(exited["details"]["one_pass"], true);
                    // A's process and both of its fixtures have finished; B's exact backends survive.
                    assert_eq!(
                        backend(&own).await,
                        own_backend,
                        "B own backend was terminated"
                    );
                    assert_eq!(
                        backend(&control).await,
                        control_backend,
                        "B control backend was terminated"
                    );
                    publish(
                        barrier,
                        "b-survived.json",
                        marker(
                            barrier,
                            json!({"pid": pid, "start": start, "own_backend": own_backend,
                        "control_backend": control_backend, "queried_after_a_exit": true}),
                        ),
                    );
                }
            }
            own.close().await.expect("close own connection");
            drop(own_guard);
            absent(&own_name).await;
            assert_eq!(
                backend(&control).await,
                control_backend,
                "FIX_MTR14_PARALLEL_ASSERT: own cleanup terminated the independent control backend"
            );
            control.close().await.expect("close control connection");
            drop(control_guard);
            absent(&control_name).await;
            let details = json!({"role": role, "pid": pid, "start": start,
            "own": own_name, "control": control_name, "control_backend": control_backend,
            "own_absent": true, "control_survived_own_cleanup": true, "control_absent": true});
            if let Some(barrier) = barrier {
                publish(
                    barrier,
                    &format!("finished-{role}.json"),
                    marker(barrier, details),
                );
            } else {
                db_cleanup_repro_emit("FIX_MTR14_PARALLEL_SINGLE", &details)
                    .expect("single-mode receipt");
            }
        }

        pub(super) fn child(role: &str) {
            let started = Instant::now();
            let barrier = inherited_barrier(); // Invalid external values panic before any fixture SQL.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("child runtime");
            runtime.block_on(workload(role, barrier.as_ref(), started + LIMIT));
        }

        fn case(role: &str) -> &'static str {
            if role == "a" { CHILD_A } else { CHILD_B }
        }

        fn one_pass(child: &DbCleanupReproChild, expected: &str) -> bool {
            let Ok(logfile) = fs::read_to_string(child.raw_log_dir.join("libtest.log")) else {
                return false;
            };
            let Ok(stdout) = fs::read_to_string(child.raw_log_dir.join("stdout.log")) else {
                return false;
            };
            let expected = format!("ok {expected}");
            let outcomes = logfile
                .lines()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            let summaries = stdout
                .lines()
                .filter(|line| line.starts_with("test result:"))
                .collect::<Vec<_>>();
            outcomes == [expected.as_str()]
                && summaries.len() == 1
                && summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")
        }

        fn spawn(role: &str, barrier: &Barrier) -> DbCleanupReproChild {
            let raw_log_dir = barrier.directory.join(format!("child-{role}"));
            fs::create_dir(&raw_log_dir).expect("child log directory");
            fs::set_permissions(&raw_log_dir, fs::Permissions::from_mode(0o700))
                .expect("child log mode");
            let mut command = Command::new(std::env::current_exe().expect("current exe"));
            command
                .args(["--exact", case(role), "--nocapture", "--logfile"])
                .arg(raw_log_dir.join("libtest.log"))
                .env(ENV, &barrier.directory)
                .stdin(Stdio::null())
                .stdout(File::create(raw_log_dir.join("stdout.log")).expect("child stdout"))
                .stderr(File::create(raw_log_dir.join("stderr.log")).expect("child stderr"));
            let started = Instant::now();
            let mut child = DbCleanupReproChild {
                child: command
                    .spawn()
                    .expect("FIX_MTR14_PARALLEL_SETUP: spawn child"),
                start: String::new(),
                started,
                raw_log_dir,
                status: None,
                one_pass: None,
                timed_out: false,
                stopped_on_unwind: false,
            };
            child.start = db_cleanup_repro_process_start(child.child.id());
            child.receipt("spawn").expect("child identity receipt");
            publish(
                barrier,
                &format!("register-{role}.json"),
                marker(
                    barrier,
                    json!({"pid": child.child.id(), "start": child.start, "case": case(role)}),
                ),
            );
            child
        }

        fn poll(child: &mut DbCleanupReproChild, expected: &str) {
            if child.status.is_some() {
                return;
            }
            child.status = child.child.try_wait().expect("wait owned child");
            if child.status.is_none() && child.started.elapsed() >= LIMIT {
                child.timed_out = true;
                let _ = child.child.kill();
                child.status = Some(child.child.wait().expect("reap own timed-out child"));
            }
            if child.status.is_some() {
                child.one_pass = Some(
                    !child.timed_out
                        && child
                            .status
                            .as_ref()
                            .is_some_and(std::process::ExitStatus::success)
                        && one_pass(child, expected),
                );
                child.receipt("terminal").expect("child terminal receipt");
            }
        }

        pub(super) fn parent() {
            assert!(
                std::env::var_os(ENV).is_none(),
                "FIX_MTR14_PARALLEL_SETUP: unexpected parent barrier env"
            );
            let parent_pid = std::process::id();
            let parent_start = db_cleanup_repro_process_start(parent_pid);
            let nonce = uuid::Uuid::new_v4().to_string();
            let directory = tempfile::Builder::new()
                .prefix(&format!("mega2-fix14-parallel-{parent_pid}-{nonce}-"))
                .tempdir()
                .expect("private barrier directory")
                .keep();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .expect("private barrier mode");
            let directory = directory.canonicalize().expect("canonical private barrier");
            checked_metadata(
                &fs::symlink_metadata(&directory).expect("parent barrier metadata"),
                true,
            );
            let barrier = Barrier {
                directory,
                nonce,
                parent_pid,
                parent_start,
            };
            let (_, baseline) = db_cleanup_repro_catalog(&["mega2_test_".to_owned()]);
            publish(
                &barrier,
                "owner.json",
                json!({"protocol": "mega2-fix14-parallel-v1",
            "nonce": barrier.nonce, "parent_pid": barrier.parent_pid, "parent_start": barrier.parent_start}),
            );
            db_cleanup_repro_emit(
                "FIX_MTR14_PARALLEL_ARTIFACT",
                &json!({"raw_log_dir": barrier.directory,
            "parent_pid": parent_pid, "parent_start": barrier.parent_start, "nonce": barrier.nonce,
            "baseline_admin_closed": true, "child_count": 2, "child_timeout_seconds": 120,
            "blocked_pids": []}),
            )
            .expect("parallel artifact receipt");
            let mut a = spawn("a", &barrier);
            let mut b = spawn("b", &barrier);
            loop {
                poll(&mut a, CHILD_A);
                poll(&mut b, CHILD_B);
                assert!(
                    a.status.is_none() && b.status.is_none(),
                    "FIX_MTR14_PARALLEL_SETUP: child ended before overlap"
                );
                if let (Some(ready_a), Some(ready_b)) = (
                    read_file(&barrier.directory.join("ready-a.json")),
                    read_file(&barrier.directory.join("ready-b.json")),
                ) {
                    validate(&barrier, &ready_a);
                    validate(&barrier, &ready_b);
                    assert_eq!(number(&ready_a["details"], "pid"), a.child.id());
                    assert_eq!(text(&ready_a["details"], "start"), a.start);
                    assert_eq!(number(&ready_b["details"], "pid"), b.child.id());
                    assert_eq!(text(&ready_b["details"], "start"), b.start);
                    publish(
                        &barrier,
                        "release-a.json",
                        marker(&barrier, json!({"both_children_live": true})),
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            while a.status.is_none() {
                poll(&mut a, CHILD_A);
                poll(&mut b, CHILD_B);
                assert!(
                    b.status.is_none(),
                    "FIX_MTR14_PARALLEL_ASSERT: B ended before A"
                );
                if a.status.is_none() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert_eq!(
                a.one_pass,
                Some(true),
                "FIX_MTR14_PARALLEL_CHILD_FAILURE: A must pass once"
            );
            let finished_a =
                read_file(&barrier.directory.join("finished-a.json")).expect("finished A receipt");
            validate(&barrier, &finished_a);
            assert_eq!(finished_a["details"]["own_absent"], true);
            assert_eq!(finished_a["details"]["control_absent"], true);
            publish(
                &barrier,
                "a-exited.json",
                marker(
                    &barrier,
                    json!({"pid": a.child.id(), "start": a.start, "one_pass": true}),
                ),
            );
            while b.status.is_none() {
                poll(&mut b, CHILD_B);
                if b.status.is_none() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert_eq!(
                b.one_pass,
                Some(true),
                "FIX_MTR14_PARALLEL_CHILD_FAILURE: B must pass once"
            );
            let survived =
                read_file(&barrier.directory.join("b-survived.json")).expect("B survival receipt");
            validate(&barrier, &survived);
            assert_eq!(survived["details"]["queried_after_a_exit"], true);
            let finished_b =
                read_file(&barrier.directory.join("finished-b.json")).expect("finished B receipt");
            validate(&barrier, &finished_b);
            assert_eq!(finished_b["details"]["own_absent"], true);
            assert_eq!(finished_b["details"]["control_absent"], true);
            let prefixes = [
                format!("mega2_test_{}_", a.child.id()),
                format!("mega2_test_{}_", b.child.id()),
            ];
            let (_, after) = db_cleanup_repro_catalog(&prefixes);
            assert!(
                after
                    .iter()
                    .all(|row| baseline.iter().any(|before| before == row)),
                "FIX_MTR14_PARALLEL_ASSERT: child-owned residue after both children passed"
            );
        }
    }

    #[test]
    fn db_cleanup_parallel_parent() {
        #[cfg(unix)]
        db_cleanup_parallel_support::parent();
        #[cfg(not(unix))]
        panic!("FIX_MTR14_PARALLEL_SETUP: Unix identity and no-symlink checks required");
    }

    #[test]
    fn db_cleanup_parallel_child_a() {
        #[cfg(unix)]
        db_cleanup_parallel_support::child("a");
        #[cfg(not(unix))]
        panic!("FIX_MTR14_PARALLEL_SETUP: Unix identity and no-symlink checks required");
    }

    #[test]
    fn db_cleanup_parallel_child_b() {
        #[cfg(unix)]
        db_cleanup_parallel_support::child("b");
        #[cfg(not(unix))]
        panic!("FIX_MTR14_PARALLEL_SETUP: Unix identity and no-symlink checks required");
    }

    #[cfg(unix)]
    mod db_namespace_pid_reuse_support {
        use std::{
            fs::{self, File, OpenOptions},
            io::{Read, Write},
            os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            path::{Path, PathBuf},
            process::{Child, Command, ExitStatus, Stdio},
            time::{Duration, Instant},
        };

        use futures::FutureExt;
        use serde_json::{Value as Json, json};

        use super::*;

        const ENV: &str = "MEGA2_FIX_MTR26_BARRIER_DIR";
        const CASE: &str = "jupiter::tests::schema_guard_tests::db_namespace_pid_reuse_child";
        const LIMIT: Duration = Duration::from_secs(120);

        struct Barrier {
            directory: PathBuf,
            nonce: String,
            parent_pid: u32,
            parent_start: String,
        }

        #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
        struct Identity {
            name: String,
            oid: i64,
            owner_oid: i64,
            owner: String,
            allow_connections: bool,
            is_template: bool,
            connection_limit: i32,
            comment: Option<String>,
        }

        fn runtime() -> Result<tokio::runtime::Runtime, String> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "FIX_MTR26_SETUP: admin runtime".to_owned())
        }

        async fn admin() -> Result<DatabaseConnection, String> {
            let url = std::env::var("MEGA_DATABASE__DB_URL")
                .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
            let mut options = admin_connect_options(&url);
            options
                .connect_timeout(Duration::from_secs(10))
                .acquire_timeout(Duration::from_secs(10));
            let db = tokio::time::timeout(Duration::from_secs(10), Database::connect(options))
                .await
                .map_err(|_| "FIX_MTR26_SETUP: admin connect timeout".to_owned())?
                .map_err(|_| "FIX_MTR26_SETUP: admin connection".to_owned())?;
            db.execute_unprepared("SET statement_timeout = '10s'")
                .await
                .map_err(|_| "FIX_MTR26_SETUP: bounded admin statement".to_owned())?;
            Ok(db)
        }

        async fn catalog(
            db: &DatabaseConnection,
            name: &str,
            exact: bool,
        ) -> Result<Vec<Identity>, String> {
            let predicate = if exact {
                "datname::text = $1"
            } else {
                "left(datname::text, char_length($1::text)) = $1::text"
            };
            let rows = db
                .query_all_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    format!(
                        "SELECT datname::text AS name, oid::bigint AS oid, \
                         datdba::bigint AS owner_oid, pg_get_userbyid(datdba)::text AS owner, \
                         datallowconn AS allow_connections, datistemplate AS is_template, \
                         datconnlimit AS connection_limit, \
                         shobj_description(oid, 'pg_database')::text AS comment \
                         FROM pg_catalog.pg_database WHERE {predicate} ORDER BY datname"
                    ),
                    [Value::from(name)],
                ))
                .await
                .map_err(|_| "FIX_MTR26_SETUP: catalog query".to_owned())?;
            rows.into_iter()
                .map(|row| {
                    Ok(Identity {
                        name: row.try_get("", "name").map_err(|_| "catalog name")?,
                        oid: row.try_get("", "oid").map_err(|_| "catalog OID")?,
                        owner_oid: row
                            .try_get("", "owner_oid")
                            .map_err(|_| "catalog owner OID")?,
                        owner: row.try_get("", "owner").map_err(|_| "catalog owner")?,
                        allow_connections: row
                            .try_get("", "allow_connections")
                            .map_err(|_| "catalog allow connections")?,
                        is_template: row
                            .try_get("", "is_template")
                            .map_err(|_| "catalog template")?,
                        connection_limit: row
                            .try_get("", "connection_limit")
                            .map_err(|_| "catalog connection limit")?,
                        comment: row.try_get("", "comment").map_err(|_| "catalog comment")?,
                    })
                })
                .collect()
        }

        fn catalog_now(name: &str, exact: bool) -> Result<Vec<Identity>, String> {
            runtime()?.block_on(async {
                let db = admin().await?;
                let result = catalog(&db, name, exact).await;
                let closed = db
                    .close()
                    .await
                    .map_err(|_| "FIX_MTR26_SETUP: close catalog admin".to_owned());
                let rows = result?;
                closed?;
                Ok(rows)
            })
        }

        fn text<'a>(value: &'a Json, field: &str) -> &'a str {
            value[field].as_str().expect("FIX_MTR26_SETUP: text field")
        }

        fn pid(value: &Json, field: &str) -> u32 {
            value[field]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .expect("FIX_MTR26_SETUP: PID field")
        }

        fn private_metadata(metadata: &fs::Metadata, directory: bool) {
            assert_eq!(
                metadata.uid(),
                unsafe { libc::geteuid() },
                "FIX_MTR26_SETUP: UID"
            );
            assert_eq!(
                metadata.permissions().mode() & 0o7777,
                if directory { 0o700 } else { 0o600 },
                "FIX_MTR26_SETUP: private mode"
            );
            assert!(
                if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                },
                "FIX_MTR26_SETUP: file kind"
            );
        }

        fn read(path: &Path) -> Option<Json> {
            let mut file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(error) => panic!("FIX_MTR26_SETUP: marker open: {error}"),
            };
            private_metadata(&file.metadata().expect("marker metadata"), false);
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(16385)
                .read_to_end(&mut bytes)
                .expect("marker read");
            assert!(bytes.len() <= 16384, "FIX_MTR26_SETUP: oversized marker");
            Some(serde_json::from_slice(&bytes).expect("FIX_MTR26_SETUP: marker JSON"))
        }

        fn marker(barrier: &Barrier, details: Json) -> Json {
            json!({"nonce": barrier.nonce, "parent_pid": barrier.parent_pid,
                "parent_start": barrier.parent_start, "details": details})
        }

        fn publish(barrier: &Barrier, name: &str, value: Json) {
            assert!(!name.contains('/'), "FIX_MTR26_SETUP: marker leaf");
            let destination = barrier.directory.join(name);
            assert!(!destination.exists(), "FIX_MTR26_SETUP: duplicate marker");
            let staging = barrier
                .directory
                .join(format!(".{name}.{}", std::process::id()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&staging)
                .expect("FIX_MTR26_SETUP: marker create");
            file.write_all(value.to_string().as_bytes())
                .expect("marker write");
            file.flush().expect("marker flush");
            drop(file);
            fs::rename(staging, destination).expect("marker publish");
        }

        fn validate(barrier: &Barrier, value: &Json) {
            assert_eq!(
                text(value, "nonce"),
                barrier.nonce,
                "FIX_MTR26_SETUP: nonce"
            );
            assert_eq!(pid(value, "parent_pid"), barrier.parent_pid, "parent PID");
            assert_eq!(
                text(value, "parent_start"),
                barrier.parent_start,
                "parent start"
            );
        }

        fn inherited() -> Option<Barrier> {
            let directory = PathBuf::from(std::env::var_os(ENV)?);
            assert!(
                directory.is_absolute(),
                "FIX_MTR26_SETUP: relative directory"
            );
            assert_eq!(
                directory.canonicalize().expect("canonical directory"),
                directory
            );
            for component in directory.ancestors() {
                assert!(
                    !fs::symlink_metadata(component)
                        .expect("path metadata")
                        .file_type()
                        .is_symlink()
                );
            }
            private_metadata(
                &fs::symlink_metadata(&directory).expect("directory metadata"),
                true,
            );
            let owner = read(&directory.join("owner.json")).expect("FIX_MTR26_SETUP: owner marker");
            assert_eq!(text(&owner, "protocol"), "mega2-fix26-namespace-v1");
            let nonce = text(&owner, "nonce").to_owned();
            let parsed = uuid::Uuid::parse_str(&nonce).expect("FIX_MTR26_SETUP: nonce UUID");
            assert_eq!(
                parsed.get_version_num(),
                4,
                "FIX_MTR26_SETUP: random v4 nonce"
            );
            assert_eq!(parsed.to_string(), nonce, "canonical nonce");
            let parent_pid = pid(&owner, "parent_pid");
            assert_eq!(
                unsafe { libc::getppid() } as u32,
                parent_pid,
                "actual parent PID"
            );
            let parent_start = text(&owner, "parent_start").to_owned();
            assert_eq!(
                db_cleanup_repro_process_start(parent_pid),
                parent_start,
                "parent identity"
            );
            let prefix = format!("mega2-fix26-namespace-{parent_pid}-{nonce}-");
            assert!(
                directory
                    .file_name()
                    .and_then(|leaf| leaf.to_str())
                    .is_some_and(|leaf| leaf.starts_with(&prefix) && leaf.len() > prefix.len())
            );
            Some(Barrier {
                directory,
                nonce,
                parent_pid,
                parent_start,
            })
        }

        async fn wait(barrier: &Barrier, name: &str, deadline: Instant) -> Json {
            loop {
                if let Some(value) = read(&barrier.directory.join(name)) {
                    validate(barrier, &value);
                    return value;
                }
                assert!(
                    Instant::now() < deadline,
                    "FIX_MTR26_SETUP: marker timeout {name}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        fn decimal_name(name: &str, child_pid: u32) -> bool {
            name.len() <= 63
                && name
                    .strip_prefix(&format!("mega2_test_{child_pid}_"))
                    .is_some_and(|tail| {
                        !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit())
                    })
        }

        async fn fixture(barrier: Option<&Barrier>, deadline: Instant) -> Json {
            let child_pid = std::process::id();
            let child_start = db_cleanup_repro_process_start(child_pid);
            if let Some(barrier) = barrier {
                let registration = wait(barrier, "register.json", deadline).await;
                assert_eq!(pid(&registration["details"], "pid"), child_pid);
                assert_eq!(text(&registration["details"], "start"), child_start);
                assert_eq!(text(&registration["details"], "case"), CASE);
                publish(
                    barrier,
                    "ready.json",
                    marker(
                        barrier,
                        json!({"pid": child_pid, "start": child_start, "case": CASE}),
                    ),
                );
                let go = wait(barrier, "go.json", deadline).await;
                assert_eq!(
                    text(&go["details"], "legacy"),
                    format!("mega2_test_{child_pid}_0")
                );
                assert_eq!(
                    db_cleanup_repro_process_start(barrier.parent_pid),
                    barrier.parent_start
                );
            }
            let temp = tempfile::tempdir().expect("fixture temp directory");
            let (config, guard) = test_db_config(temp.path()).await;
            let name = guard.schema().to_owned();
            assert!(
                decimal_name(&name, child_pid),
                "FIX_MTR26_ASSERT: actual factory name"
            );
            if let Some(barrier) = barrier {
                publish(
                    barrier,
                    "allocated.json",
                    marker(barrier, json!({"name": name})),
                );
                assert_ne!(
                    name,
                    format!("mega2_test_{child_pid}_0"),
                    "factory reused legacy name"
                );
            }
            let db = crate::jupiter::storage::init::database_connection(&config)
                .await
                .expect("FIX_MTR26_ASSERT: actual config connection");
            assert_eq!(current_database(&db).await, name, "actual factory database");
            let row = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT current_schema()::text AS schema, 1::bigint AS alive".to_owned(),
                ))
                .await
                .expect("FIX_MTR26_ASSERT: actual fixture query")
                .expect("fixture query row");
            assert_eq!(row.try_get::<String>("", "schema").expect("schema"), name);
            assert_eq!(row.try_get::<i64>("", "alive").expect("alive"), 1);
            db.close().await.expect("close actual factory connection");
            drop(guard);
            assert!(
                !database_exists(&name).await,
                "FIX_MTR26_ASSERT: actual guard residue"
            );
            json!({"pid": child_pid, "start": child_start, "case": CASE, "name": name,
                "query_passed": true, "guard_dropped": true, "own_absent": true})
        }

        pub(super) fn child() {
            let started = Instant::now();
            let barrier = inherited();
            runtime().expect("child runtime").block_on(async {
                let result = std::panic::AssertUnwindSafe(tokio::time::timeout(
                    LIMIT,
                    fixture(barrier.as_ref(), started + LIMIT),
                ))
                .catch_unwind()
                .await;
                match result {
                    Ok(Ok(details)) => {
                        if let Some(barrier) = &barrier {
                            publish(barrier, "finished.json", marker(barrier, details.clone()));
                        }
                        db_cleanup_repro_emit("FIX_MTR26_CHILD_FINISHED", &details)
                            .expect("child receipt");
                    }
                    Ok(Err(_)) => panic!("FIX_MTR26_SETUP: child fixture timeout"),
                    Err(payload) => {
                        if let Some(barrier) = &barrier {
                            let message = payload
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| {
                                    payload
                                        .downcast_ref::<&str>()
                                        .map(|message| (*message).to_owned())
                                })
                                .unwrap_or_else(|| "non-string panic".to_owned());
                            let message = message.chars().take(8192).collect::<String>();
                            publish(
                                barrier,
                                "fixture-panic.json",
                                marker(barrier, json!({"message": message})),
                            );
                        }
                        std::panic::resume_unwind(payload);
                    }
                }
            });
        }

        struct Run {
            barrier: Barrier,
            child: Child,
            child_start: String,
            started: Instant,
            status: Option<ExitStatus>,
            timed_out: bool,
            sentinel: Option<(Identity, bool)>,
        }

        impl Run {
            fn stop(&mut self) -> Result<(), String> {
                if self.status.is_none() {
                    self.status = self
                        .child
                        .try_wait()
                        .map_err(|_| "FIX_MTR26_SETUP: child poll")?;
                }
                if self.status.is_none() {
                    let observed = std::panic::catch_unwind(|| {
                        db_cleanup_repro_process_start(self.child.id())
                    })
                    .ok();
                    if observed.as_deref() != Some(self.child_start.as_str()) {
                        return Err(
                            "FIX_MTR26_SETUP: refuse signal after child identity mismatch"
                                .to_owned(),
                        );
                    }
                    self.child
                        .kill()
                        .map_err(|_| "FIX_MTR26_SETUP: stop own child")?;
                    self.status = Some(
                        self.child
                            .wait()
                            .map_err(|_| "FIX_MTR26_SETUP: reap own child")?,
                    );
                }
                Ok(())
            }

            fn poll(&mut self) {
                self.status = self.child.try_wait().expect("FIX_MTR26_SETUP: child wait");
                if self.status.is_none() && self.started.elapsed() >= LIMIT {
                    self.timed_out = true;
                    self.stop().expect("FIX_MTR26_SETUP: timeout stop");
                }
            }

            fn sentinel(&mut self, baseline: &[Identity]) -> Result<(), String> {
                let name = format!("mega2_test_{}_0", self.child.id());
                let current = catalog_now(&name, true)?;
                if let Some(before) = baseline.iter().find(|row| row.name == name) {
                    if current != [before.clone()] {
                        return Err(
                            "FIX_MTR26_SETUP: baseline sentinel identity changed".to_owned()
                        );
                    }
                    self.sentinel = Some((before.clone(), false));
                    return Ok(());
                }
                if !current.is_empty() {
                    return Err(
                        "FIX_MTR26_SETUP: unowned exact name appeared after baseline".to_owned(),
                    );
                }
                let comment = format!(
                    "FIX_MTR26:{}:{}:{}",
                    self.barrier.nonce,
                    self.barrier.parent_pid,
                    self.child.id()
                );
                runtime()?.block_on(async {
                    let db = admin().await?;
                    let owner: String = db
                        .query_one_raw(Statement::from_string(
                            DatabaseBackend::Postgres,
                            "SELECT current_user::text AS owner".to_owned(),
                        ))
                        .await
                        .map_err(|_| "sentinel owner query")?
                        .ok_or("sentinel owner row")?
                        .try_get("", "owner")
                        .map_err(|_| "sentinel owner value")?;
                    db.execute_unprepared(&format!(
                        "CREATE DATABASE {name} WITH ALLOW_CONNECTIONS false"
                    ))
                    .await
                    .map_err(|_| "FIX_MTR26_SETUP: unique sentinel CREATE failed")?;
                    let initial = catalog(&db, &name, true).await?;
                    let [initial] = initial.as_slice() else {
                        return Err("sentinel identity row".to_owned());
                    };
                    if initial.owner != owner
                        || initial.allow_connections
                        || initial.comment.is_some()
                    {
                        return Err(
                            "FIX_MTR26_SETUP: newly created sentinel identity mismatch".to_owned()
                        );
                    }
                    self.sentinel = Some((initial.clone(), true));
                    db.execute_unprepared(&format!("COMMENT ON DATABASE {name} IS '{comment}'"))
                        .await
                        .map_err(|_| "FIX_MTR26_SETUP: sentinel comment")?;
                    let labeled = catalog(&db, &name, true).await?;
                    let [labeled] = labeled.as_slice() else {
                        return Err("labeled sentinel row".to_owned());
                    };
                    let mut expected = initial.clone();
                    expected.comment = Some(comment);
                    if *labeled != expected {
                        return Err("sentinel label identity mismatch".to_owned());
                    }
                    self.sentinel = Some((labeled.clone(), true));
                    db.close()
                        .await
                        .map_err(|_| "FIX_MTR26_SETUP: close seed admin")?;
                    Ok(())
                })
            }

            fn cleanup(&mut self) -> Result<Json, String> {
                let Some((expected, owned)) = self.sentinel.clone() else {
                    return Ok(json!({"sentinel": null}));
                };
                let actual = catalog_now(&expected.name, true)?;
                if actual != [expected.clone()] {
                    return Err(
                        "FIX_MTR26_RESOURCE: sentinel identity changed; no DDL permitted"
                            .to_owned(),
                    );
                }
                if owned {
                    runtime()?.block_on(async {
                        let db = admin().await?;
                        if catalog(&db, &expected.name, true).await? != [expected.clone()] {
                            db.close()
                                .await
                                .map_err(|_| "close changed sentinel admin")?;
                            return Err(
                                "FIX_MTR26_RESOURCE: pre-DROP identity changed; no DDL permitted"
                                    .to_owned(),
                            );
                        }
                        db.execute_unprepared(&format!("DROP DATABASE {}", expected.name))
                            .await
                            .map_err(|_| "FIX_MTR26_RESOURCE: owned sentinel DROP")?;
                        let absent = catalog(&db, &expected.name, true).await?.is_empty();
                        db.close()
                            .await
                            .map_err(|_| "FIX_MTR26_RESOURCE: close cleanup admin")?;
                        if !absent {
                            return Err("FIX_MTR26_RESOURCE: owned sentinel remains".to_owned());
                        }
                        Ok(())
                    })?;
                }
                self.sentinel = None;
                Ok(
                    json!({"identity": expected, "owned": owned, "legacy_unchanged": true,
                    "owned_seed_removed": owned, "baseline_seed_preserved": !owned}),
                )
            }
        }

        impl Drop for Run {
            fn drop(&mut self) {
                let stopped = self.stop();
                let cleanup = if stopped.is_ok() {
                    self.cleanup()
                } else {
                    Err("child not reaped; no cleanup".to_owned())
                };
                if stopped.is_err() || cleanup.is_err() {
                    let _ = db_cleanup_repro_emit(
                        "FIX_MTR26_UNWIND_STOP",
                        &json!({"pid": self.child.id(),
                        "start": self.child_start, "stop": stopped, "cleanup": cleanup}),
                    );
                }
            }
        }

        fn log_file(path: &Path) -> File {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .expect("private child log")
        }

        fn one_test(directory: &Path, success: bool) -> bool {
            let Ok(stdout) = fs::read_to_string(directory.join("stdout.log")) else {
                return false;
            };
            let lines = stdout.lines().collect::<Vec<_>>();
            let running = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| line.starts_with("running ").then_some((index, *line)))
                .collect::<Vec<_>>();
            let cases = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    (line.starts_with("test ") && !line.starts_with("test result:"))
                        .then_some((index, *line))
                })
                .collect::<Vec<_>>();
            let summaries = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    line.starts_with("test result:").then_some((index, *line))
                })
                .collect::<Vec<_>>();
            if running.len() != 1
                || running[0].1 != "running 1 test"
                || cases.len() != 1
                || summaries.len() != 1
                || running[0].0 >= cases[0].0
                || cases[0].0 >= summaries[0].0
                || lines[summaries[0].0 + 1..]
                    .iter()
                    .any(|line| !line.is_empty())
            {
                return false;
            }
            let Some(tail) = cases[0].1.strip_prefix(&format!("test {CASE} ... ")) else {
                return false;
            };
            let prefix = if success {
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
            } else {
                "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; "
            };
            let Some(summary) = summaries[0].1.strip_prefix(prefix) else {
                return false;
            };
            let Some((filtered, elapsed)) = summary.split_once(" filtered out; finished in ")
            else {
                return false;
            };
            let Some(elapsed) = elapsed.strip_suffix('s') else {
                return false;
            };
            let Some((seconds, fraction)) = elapsed.split_once('.') else {
                return false;
            };
            let digits =
                |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
            if !digits(filtered) || !digits(seconds) || fraction.len() != 2 || !digits(fraction) {
                return false;
            }
            let terminals = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    matches!(*line, "ok" | "FAILED" | "ignored").then_some((index, *line))
                })
                .collect::<Vec<_>>();
            let expected = if success { "ok" } else { "FAILED" };
            let terminal_index = if tail == expected {
                if !terminals.is_empty() {
                    return false;
                }
                cases[0].0
            } else {
                let inline_marker =
                    tail.strip_prefix("FIX_MTR26_CHILD_FINISHED ")
                        .is_some_and(|raw| {
                            serde_json::from_str::<Json>(raw).is_ok_and(|value| value.is_object())
                        });
                if (!tail.is_empty() && !inline_marker)
                    || terminals.len() != 1
                    || terminals[0].1 != expected
                    || terminals[0].0 <= cases[0].0
                    || terminals[0].0 >= summaries[0].0
                {
                    return false;
                }
                terminals[0].0
            };
            let failures = lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| (*line == "failures:").then_some(index))
                .collect::<Vec<_>>();
            let captured = lines
                .iter()
                .copied()
                .filter(|line| line.starts_with("---- "))
                .collect::<Vec<_>>();
            if success {
                return failures.is_empty() && captured.is_empty();
            }
            let Some(last_failure) = failures.last().copied() else {
                return false;
            };
            let expected_capture = format!("---- {CASE} stdout ----");
            if !(1..=2).contains(&failures.len())
                || failures
                    .iter()
                    .any(|index| *index <= terminal_index || *index >= summaries[0].0)
                || captured.len() > 1
                || captured
                    .iter()
                    .any(|line| *line != expected_capture.as_str())
            {
                return false;
            }
            let failed_cases = lines[last_failure + 1..summaries[0].0]
                .iter()
                .copied()
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            let expected_failed = format!("    {CASE}");
            failed_cases == [expected_failed.as_str()]
        }

        pub(super) fn parent() {
            assert!(
                std::env::var_os(ENV).is_none(),
                "FIX_MTR26_SETUP: parent inherited barrier"
            );
            let parent_pid = std::process::id();
            let parent_start = db_cleanup_repro_process_start(parent_pid);
            let nonce = uuid::Uuid::new_v4().to_string();
            let directory = tempfile::Builder::new()
                .prefix(&format!("mega2-fix26-namespace-{parent_pid}-{nonce}-"))
                .tempdir()
                .expect("private directory")
                .keep();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .expect("private directory mode");
            let directory = directory
                .canonicalize()
                .expect("canonical private directory");
            private_metadata(
                &fs::symlink_metadata(&directory).expect("private directory metadata"),
                true,
            );
            let barrier = Barrier {
                directory,
                nonce,
                parent_pid,
                parent_start,
            };
            let baseline = catalog_now("mega2_test_", false)
                .expect("FIX_MTR26_SETUP: initial baseline closed");
            publish(
                &barrier,
                "owner.json",
                json!({"protocol": "mega2-fix26-namespace-v1",
                "nonce": barrier.nonce, "parent_pid": barrier.parent_pid, "parent_start": barrier.parent_start}),
            );
            let started = Instant::now();
            let child = Command::new(std::env::current_exe().expect("current test binary"))
                .args(["--exact", CASE, "--nocapture", "--test-threads=1"])
                .env(ENV, &barrier.directory)
                .stdin(Stdio::null())
                .stdout(log_file(&barrier.directory.join("stdout.log")))
                .stderr(log_file(&barrier.directory.join("stderr.log")))
                .spawn()
                .expect("FIX_MTR26_SETUP: one fresh exact child");
            let mut run = Run {
                barrier,
                child,
                child_start: String::new(),
                started,
                status: None,
                timed_out: false,
                sentinel: None,
            };
            run.child_start = db_cleanup_repro_process_start(run.child.id());
            db_cleanup_repro_emit("FIX_MTR26_ARTIFACT", &json!({"directory": run.barrier.directory,
                "nonce": run.barrier.nonce, "parent_pid": parent_pid, "parent_start": run.barrier.parent_start,
                "pid": run.child.id(), "start": run.child_start, "case": CASE, "child_count": 1,
                "timeout_seconds": 120, "baseline_admin_closed": true})).expect("artifact receipt");
            publish(
                &run.barrier,
                "register.json",
                marker(
                    &run.barrier,
                    json!({"pid": run.child.id(), "start": run.child_start, "case": CASE}),
                ),
            );
            let ready = loop {
                run.poll();
                if let Some(ready) = read(&run.barrier.directory.join("ready.json")) {
                    break ready;
                }
                assert!(
                    run.status.is_none(),
                    "FIX_MTR26_SETUP: child ended before READY"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            validate(&run.barrier, &ready);
            assert_eq!(pid(&ready["details"], "pid"), run.child.id());
            assert_eq!(text(&ready["details"], "start"), run.child_start);
            assert_eq!(text(&ready["details"], "case"), CASE);
            assert_eq!(
                db_cleanup_repro_process_start(run.child.id()),
                run.child_start
            );
            run.sentinel(&baseline)
                .expect("FIX_MTR26_SETUP: exact sentinel ready and admin closed");
            let legacy = format!("mega2_test_{}_0", run.child.id());
            db_cleanup_repro_emit("FIX_MTR26_SENTINEL", &json!({"identity": run.sentinel.as_ref().map(|seed| &seed.0),
                "owned": run.sentinel.as_ref().is_some_and(|seed| seed.1), "seed_admin_closed": true})).expect("seed receipt");
            publish(
                &run.barrier,
                "go.json",
                marker(&run.barrier, json!({"legacy": legacy})),
            );
            while run.status.is_none() {
                run.poll();
                std::thread::sleep(Duration::from_millis(10));
            }
            let finished = read(&run.barrier.directory.join("finished.json"));
            let panic = read(&run.barrier.directory.join("fixture-panic.json"));
            for receipt in finished.iter().chain(panic.iter()) {
                validate(&run.barrier, receipt);
            }
            let exit = run.status.as_ref().and_then(ExitStatus::code);
            let one_pass =
                exit == Some(0) && one_test(&run.barrier.directory, true) && !run.timed_out;
            let old_collision = exit == Some(101)
                && one_test(&run.barrier.directory, false)
                && !run.timed_out
                && panic.as_ref().is_some_and(|receipt| {
                    let message = text(&receipt["details"], "message");
                    message.starts_with("failed to prepare PostgreSQL test schema: ")
                        && message.contains("code: \"42P04\"")
                        && message.contains(&format!(
                            "message: \"database \\\"{legacy}\\\" already exists\""
                        ))
                });
            let cleanup = run
                .cleanup()
                .expect("FIX_MTR26_RESOURCE: exact legacy identity and cleanup before oracle");
            let after = catalog_now(&format!("mega2_test_{}_", run.child.id()), false)
                .expect("FIX_MTR26_RESOURCE: closed after-catalog");
            let no_new = after
                .iter()
                .all(|row| baseline.iter().any(|before| before == row));
            db_cleanup_repro_emit(
                "FIX_MTR26_TERMINAL",
                &json!({"pid": run.child.id(), "start": run.child_start,
                "exit_code": exit, "one_pass": one_pass, "old_42P04_exact_sentinel": old_collision,
                "timed_out": run.timed_out, "cleanup": cleanup, "no_new_child_databases": no_new,
                "resources_checked_before_oracle": true}),
            )
            .expect("terminal receipt");
            assert!(no_new, "FIX_MTR26_RESOURCE: actual child fixture residue");
            if old_collision {
                panic!(
                    "FIX_MTR26_OLD_COLLISION: real fixture rejected its exact PID_0 sentinel with 42P04"
                );
            }
            assert!(
                one_pass && panic.is_none(),
                "FIX_MTR26_ORACLE: expected exactly one real fixture pass"
            );
            let finished = finished.expect("FIX_MTR26_ORACLE: actual fixture receipt");
            let details = &finished["details"];
            assert_eq!(pid(details, "pid"), run.child.id());
            assert_eq!(text(details, "start"), run.child_start);
            assert_eq!(text(details, "case"), CASE);
            let name = text(details, "name");
            assert!(decimal_name(name, run.child.id()) && name != legacy);
            assert_eq!(details["query_passed"], true);
            assert_eq!(details["guard_dropped"], true);
            assert_eq!(details["own_absent"], true);
        }
    }

    #[test]
    fn db_namespace_pid_reuse_parent() {
        #[cfg(unix)]
        db_namespace_pid_reuse_support::parent();
        #[cfg(not(unix))]
        panic!("FIX_MTR26_SETUP: Unix PID identity and private directory checks required");
    }

    #[test]
    fn db_namespace_pid_reuse_child() {
        #[cfg(unix)]
        db_namespace_pid_reuse_support::child();
        #[cfg(not(unix))]
        panic!("FIX_MTR26_SETUP: Unix PID identity and private directory checks required");
    }
}
