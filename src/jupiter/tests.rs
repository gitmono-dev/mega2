use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
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
    let db_url = create_test_database_url().await;

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

    Database::connect(opt)
        .await
        .expect("Failed to connect to PostgreSQL test database")
}

pub async fn test_db_config(_temp_dir: &Path) -> DbConfig {
    DbConfig {
        db_type: "postgres".to_owned(),
        db_url: create_test_database_url().await,
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
    }
}

async fn create_test_database_url() -> String {
    let admin_url =
        std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
    assert_postgres_url(&admin_url);

    let schema = format!(
        "mega2_test_{}_{}",
        std::process::id(),
        TEST_SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let mut admin_opt = ConnectOptions::new(admin_url.clone());
    admin_opt
        .max_connections(1)
        .min_connections(1)
        // Same FIX-05 rationale as `test_db_connection`: the probe above all
        // else must not fail merely because the shared test PostgreSQL is
        // busy serving dozens of concurrent migration runs.
        .connect_timeout(std::time::Duration::from_secs(60))
        .acquire_timeout(std::time::Duration::from_secs(120))
        .sqlx_logging(false);

    let admin = Database::connect(admin_opt).await.unwrap_or_else(|_| {
        panic!(
            "test PostgreSQL is not available; run `docker compose -f docker-compose.test.yml up -d` first"
        )
    });
    execute_postgres(&admin, format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await;
    execute_postgres(&admin, format!("CREATE SCHEMA {schema}")).await;

    database_url_with_search_path(&admin_url, &schema)
}

fn assert_postgres_url(db_url: &str) {
    let url = Url::parse(db_url).expect("MEGA_DATABASE__DB_URL must be a valid PostgreSQL URL");
    assert!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "MEGA_DATABASE__DB_URL must use postgres:// or postgresql://"
    );
}

fn database_url_with_search_path(admin_url: &str, schema: &str) -> String {
    let mut url = Url::parse(admin_url).expect("MEGA_DATABASE__DB_URL must be a valid URL");
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema},public"));
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
        push_queue_storage: PushQueueStorage::new(base.clone()),
        buck_storage: BuckStorage { base: base.clone() },
        bots_storage: BotsStorage { base: base.clone() },
        webhook_storage: WebhookStorage { base: base.clone() },
        audit_storage: AuditStorage { base: base.clone() },
        oci_db_storage: OciDbStorage { base: base.clone() },
    };

    apply_migrations(&connection, true).await.unwrap();

    let webhook_service = WebhookService::mock(svc.webhook_storage.clone());

    Storage {
        app_service: Arc::new(svc),
        cl_service: CLService::mock(),
        push_queue_service: PushQueueService::new(
            base.clone(),
            config.monorepo.push_policy.clone(),
        )
        .with_timeouts(Duration::from_secs(30), Duration::from_millis(20))
        .with_max_push_commits(config.monorepo.max_push_commits),
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

/// Redis manager for RedLock-backed server-signing key init.
pub async fn test_redis_manager() -> crate::jupiter::redis::ConnectionManager {
    let url =
        std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
    let client = ::redis::Client::open(url).expect("redis client");
    ::redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connection")
}
