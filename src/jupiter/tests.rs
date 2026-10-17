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

static TEST_SCHEMA_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub async fn test_db_connection(_temp_dir: &Path) -> DatabaseConnection {
    connect_guarded_database(TestDatabaseKind::Default).await
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
        admin.close().await
    });
    if let Err(err) = dropped {
        eprintln!("test schema {schema} left behind: {err}");
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
        TEST_SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
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
    execute_postgres(&admin, format!("DROP DATABASE IF EXISTS {schema}")).await;
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
    fn claim(&self) -> bool {
        !self.dropped.swap(true, Ordering::AcqRel)
    }

    fn drop_off_thread(&self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let worker = std::thread::Builder::new()
            .name(format!("drop-{name}"))
            .spawn(move || drop_traces_isolated_database(&admin_url, &name));
        if let Ok(worker) = worker {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
impl Drop for TracesIsolatedDatabaseGuard {
    fn drop(&mut self) {
        if self.0.claim() {
            self.0.drop_off_thread();
        }
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
            if database.claim() {
                database.drop_off_thread();
            }
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
fn drop_traces_isolated_database(admin_url: &str, name: &str) {
    if !traces_isolated_name_is_owned(name) {
        eprintln!("refusing to drop a database outside the traces_noext_ prefix");
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("traces isolated database {name} left behind: no runtime: {err}");
            return;
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
        dropped?;
        admin.close().await
    });
    if let Err(err) = dropped {
        eprintln!(
            "traces isolated database {name} left behind: {}",
            scrub_admin_error(err, admin_url)
        );
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
}
