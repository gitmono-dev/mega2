use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
};
use tracing::log;
use url::Url;

use crate::{
    config::{Config, DbConfig, reload::ConfigHandle},
    jupiter::{
        migration::apply_migrations,
        service::{
            artifact_service::ArtifactService, buck_service::BuckService, cl_service::CLService,
            cla_service::ClaService, code_review_service::CodeReviewService,
            git_service::GitService, import_service::ImportService, issue_service::IssueService,
            lfs_service::LfsService, merge_queue_service::MergeQueueService,
            mono_service::MonoService, webhook_service::WebhookService,
        },
        storage::{
            AppService, Storage,
            attachment_storage::AttachmentStorage,
            audit_storage::AuditStorage,
            base_storage::{BaseStorage, StorageConnector},
            bots_storage::BotsStorage,
            buck_storage::BuckStorage,
            build_trigger_storage::BuildTriggerStorage,
            channel_membership_storage::ChannelMembershipStorage,
            channel_membership_update_storage::ChannelMembershipUpdateStorage,
            channel_storage::ChannelStorage,
            cl_reviewer_storage::ClReviewerStorage,
            cl_storage::ClStorage,
            cla_storage::ClaStorage,
            code_review_comment_storage::CodeReviewCommentStorage,
            code_review_thread_storage::CodeReviewThreadStorage,
            commit_binding_storage::CommitBindingStorage,
            conversation_storage::ConversationStorage,
            custom_reaction_storage::CustomReactionStorage,
            dynamic_sidebar_storage::DynamicSidebarStorage,
            git_db_storage::GitDbStorage,
            gpg_storage::GpgStorage,
            group_storage::GroupStorage,
            issue_storage::IssueStorage,
            lfs_db_storage::LfsDbStorage,
            merge_queue_storage::MergeQueueStorage,
            message_storage::MessageStorage,
            mono_storage::MonoStorage,
            note_storage::NoteStorage,
            notification_storage::NotificationStorage,
            object_storage::mock_object_storage,
            open_graph_storage::OpenGraphStorage,
            reaction_storage::ReactionStorage,
            user_storage::UserStorage,
            vault_storage::VaultStorage,
            webhook_storage::WebhookStorage,
        },
    },
};

const DEFAULT_TEST_DATABASE_URL: &str =
    "postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it";

static TEST_SCHEMA_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub async fn test_db_connection(_temp_dir: &Path) -> DatabaseConnection {
    let db_url = create_test_database_url().await;

    let mut opt = ConnectOptions::new(db_url);
    opt.max_connections(2)
        .min_connections(1)
        .sqlx_logging(true)
        .sqlx_logging_level(log::LevelFilter::Debug);

    Database::connect(opt)
        .await
        .expect("Failed to connect to PostgreSQL test database")
}

pub async fn test_db_config(_temp_dir: &Path) -> DbConfig {
    DbConfig {
        db_type: "postgres".to_owned(),
        db_path: PathBuf::new(),
        db_url: create_test_database_url().await,
        max_connection: 2,
        min_connection: 1,
        acquire_timeout: 5,
        connect_timeout: 5,
        sqlx_logging: false,
    }
}

async fn create_test_database_url() -> String {
    let admin_url =
        std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
    assert_postgres_url(&admin_url);

    let schema = format!(
        "monoengine_test_{}_{}",
        std::process::id(),
        TEST_SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let mut admin_opt = ConnectOptions::new(admin_url.clone());
    admin_opt
        .max_connections(1)
        .min_connections(1)
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
    db.execute(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .expect("failed to prepare PostgreSQL test schema");
}

pub async fn test_storage(temp_dir: impl AsRef<Path>) -> Storage {
    let connection = test_db_connection(temp_dir.as_ref()).await;
    let connection = Arc::new(connection);
    let config = Arc::new(Config::mock());
    let base = BaseStorage::new(connection.clone());

    let svc = AppService {
        mono_storage: MonoStorage { base: base.clone() },
        git_db_storage: GitDbStorage { base: base.clone() },
        gpg_storage: GpgStorage { base: base.clone() },
        lfs_db_storage: LfsDbStorage { base: base.clone() },
        cla_storage: ClaStorage { base: base.clone() },
        user_storage: UserStorage { base: base.clone() },
        group_storage: GroupStorage { base: base.clone() },
        cl_storage: ClStorage { base: base.clone() },
        issue_storage: IssueStorage { base: base.clone() },
        vault_storage: VaultStorage { base: base.clone() },
        conversation_storage: ConversationStorage { base: base.clone() },
        note_storage: NoteStorage { base: base.clone() },
        commit_binding_storage: CommitBindingStorage { base: base.clone() },
        reviewer_storage: ClReviewerStorage { base: base.clone() },
        merge_queue_storage: MergeQueueStorage::new(base.clone()),
        buck_storage: BuckStorage { base: base.clone() },
        dynamic_sidebar_storage: DynamicSidebarStorage { base: base.clone() },
        code_review_comment_storage: CodeReviewCommentStorage { base: base.clone() },
        code_review_thread_storage: CodeReviewThreadStorage { base: base.clone() },
        build_trigger_storage: BuildTriggerStorage { base: base.clone() },
        bots_storage: BotsStorage { base: base.clone() },
        webhook_storage: WebhookStorage { base: base.clone() },
        audit_storage: AuditStorage { base: base.clone() },
        attachment_storage: AttachmentStorage { base: base.clone() },
        reaction_storage: ReactionStorage { base: base.clone() },
        custom_reaction_storage: CustomReactionStorage { base: base.clone() },
        open_graph_storage: OpenGraphStorage { base: base.clone() },
        channel_storage: ChannelStorage { base: base.clone() },
        channel_membership_storage: ChannelMembershipStorage { base: base.clone() },
        channel_membership_update_storage: ChannelMembershipUpdateStorage { base: base.clone() },
        message_storage: MessageStorage { base: base.clone() },
    };

    apply_migrations(&connection, true).await.unwrap();

    let webhook_service = WebhookService::mock(svc.webhook_storage.clone());

    Storage {
        app_service: Arc::new(svc),
        cla_service: ClaService::new(base.clone()),
        issue_service: IssueService::mock(),
        cl_service: CLService::mock(),
        merge_queue_service: MergeQueueService::mock(),
        artifact_service: ArtifactService::new(base.clone(), mock_object_storage()),
        buck_service: BuckService::mock(),
        config_handle: ConfigHandle::from_arc(config.clone()),
        config,
        git_service: GitService::mock(),
        mono_service: MonoService::mock(),
        import_service: ImportService::mock(),
        lfs_service: LfsService::mock(),
        code_review_service: CodeReviewService::mock(),
        webhook_service,
        notification_storage: NotificationStorage::new(connection.clone()),
    }
}
