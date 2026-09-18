pub mod agent_capture_storage;
pub mod artifact_storage;
pub mod audit_storage;
pub mod base_storage;
pub mod blob_path_index;
pub mod bots_storage;
pub mod buck_storage;
pub mod cl_reviewer_storage;
pub mod cl_storage;
pub mod cla_storage;
pub mod code_review_comment_storage;
pub mod code_review_thread_storage;
pub mod commit_binding_storage;
pub mod conversation_storage;
pub mod git_db_storage;
pub mod gpg_storage;
pub mod group_storage;
pub mod init;
pub mod issue_storage;
pub mod lfs_db_storage;
pub mod mono_storage;
pub mod notification_storage;
pub mod object_storage;
pub mod oci_db_storage;
pub mod push_queue_storage;
pub mod stg_common;
pub mod user_storage;
pub mod vault_storage;
pub mod webhook_storage;

use std::sync::Arc;

use sea_orm::DatabaseConnection;
use tokio::sync::Semaphore;

use crate::{
    common::errors::MegaError,
    config::{Config, PushPolicy, reload::ConfigHandle, validate::validate_buck_config},
    contract::{policy::entitystore::SharedEntityStore, vault::integration::vault_core::VaultCore},
    jupiter::{
        service::{
            agent_capture_service::AgentCaptureService, artifact_service::ArtifactService,
            buck_service::BuckService, cl_service::CLService, cla_service::ClaService,
            code_review_service::CodeReviewService, git_service::GitService,
            import_service::ImportService, lfs_service::LfsService, mono_service::MonoService,
            oci_service::OciService, push_queue_service::PushQueueService,
            webhook_service::WebhookService,
        },
        storage::{
            agent_capture_storage::AgentCaptureStorage,
            audit_storage::AuditStorage,
            base_storage::{BaseStorage, StorageConnector},
            bots_storage::BotsStorage,
            buck_storage::BuckStorage,
            cl_reviewer_storage::ClReviewerStorage,
            cl_storage::ClStorage,
            cla_storage::ClaStorage,
            code_review_comment_storage::CodeReviewCommentStorage,
            code_review_thread_storage::CodeReviewThreadStorage,
            commit_binding_storage::CommitBindingStorage,
            conversation_storage::ConversationStorage,
            git_db_storage::GitDbStorage,
            gpg_storage::GpgStorage,
            group_storage::GroupStorage,
            init::database_connection,
            issue_storage::IssueStorage,
            lfs_db_storage::LfsDbStorage,
            mono_storage::MonoStorage,
            notification_storage::NotificationStorage,
            object_storage::MegaObjectStorageWrapper,
            oci_db_storage::OciDbStorage,
            push_queue_storage::PushQueueStorage,
            user_storage::UserStorage,
            vault_storage::VaultStorage,
            webhook_storage::WebhookStorage,
        },
    },
};

#[derive(Clone)]
pub struct AppService {
    pub mono_storage: MonoStorage,
    pub git_db_storage: GitDbStorage,
    pub gpg_storage: GpgStorage,
    pub lfs_db_storage: LfsDbStorage,
    pub cla_storage: ClaStorage,
    pub user_storage: UserStorage,
    pub group_storage: GroupStorage,
    pub vault_storage: VaultStorage,
    pub cl_storage: ClStorage,
    pub issue_storage: IssueStorage,
    pub conversation_storage: ConversationStorage,
    pub commit_binding_storage: CommitBindingStorage,
    pub reviewer_storage: ClReviewerStorage,
    pub push_queue_storage: PushQueueStorage,
    pub buck_storage: BuckStorage,
    pub code_review_comment_storage: CodeReviewCommentStorage,
    pub code_review_thread_storage: CodeReviewThreadStorage,
    pub bots_storage: BotsStorage,
    pub webhook_storage: WebhookStorage,
    pub audit_storage: AuditStorage,
    pub oci_db_storage: OciDbStorage,
}

impl AppService {
    fn mock() -> Arc<Self> {
        let mock = BaseStorage::mock();
        // For tests and in-memory workflows we don't need a real persistent
        // object storage. Use a filesystem-backed storage rooted in the system
        // temp directory to provide a lightweight implementation.
        Arc::new(Self {
            mono_storage: MonoStorage { base: mock.clone() },
            git_db_storage: GitDbStorage { base: mock.clone() },
            gpg_storage: GpgStorage { base: mock.clone() },
            lfs_db_storage: LfsDbStorage { base: mock.clone() },
            cla_storage: ClaStorage { base: mock.clone() },
            user_storage: UserStorage { base: mock.clone() },
            group_storage: GroupStorage { base: mock.clone() },
            vault_storage: VaultStorage { base: mock.clone() },
            cl_storage: ClStorage { base: mock.clone() },
            issue_storage: IssueStorage { base: mock.clone() },
            conversation_storage: ConversationStorage { base: mock.clone() },
            commit_binding_storage: CommitBindingStorage { base: mock.clone() },
            reviewer_storage: ClReviewerStorage { base: mock.clone() },
            push_queue_storage: PushQueueStorage::new(mock.clone()),
            buck_storage: BuckStorage { base: mock.clone() },
            code_review_comment_storage: CodeReviewCommentStorage { base: mock.clone() },
            code_review_thread_storage: CodeReviewThreadStorage { base: mock.clone() },
            bots_storage: BotsStorage { base: mock.clone() },
            webhook_storage: WebhookStorage { base: mock.clone() },
            audit_storage: AuditStorage { base: mock.clone() },
            oci_db_storage: OciDbStorage { base: mock.clone() },
        })
    }
}

#[derive(Clone)]
pub struct Storage {
    pub(crate) app_service: Arc<AppService>,
    pub cla_service: ClaService,
    pub cl_service: CLService,
    pub push_queue_service: PushQueueService,
    pub artifact_service: ArtifactService,
    pub buck_service: BuckService,
    pub mono_service: MonoService,
    pub import_service: ImportService,
    pub git_service: GitService,
    pub lfs_service: LfsService,
    pub oci_service: OciService,
    pub agent_capture_service: AgentCaptureService,
    pub config_handle: ConfigHandle,
    pub config: Arc<Config>,
    pub code_review_service: CodeReviewService,
    pub webhook_service: WebhookService,
    pub storage_event_emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    pub notification_storage: notification_storage::NotificationStorage,
    /// Shared authorization snapshot holder (ADR-UN-02). Injected by
    /// `AppContext`; the same `Arc` is shared with the HTTP state so the write
    /// path (notify) and read path (guard/push) observe the same instance.
    pub(crate) entity_store: Arc<SharedEntityStore>,
    /// Server-side commit-signing vault handle (MC-09). `None` in tests and
    /// other non-production assemblies; the synthetic-commit sites fail
    /// closed when it is absent. Injected by `AppContext::new` via
    /// [`Storage::with_vault`].
    pub(crate) vault: Option<VaultCore>,
}

impl Storage {
    pub async fn new(
        config: Arc<Config>,
        object_store: MegaObjectStorageWrapper,
    ) -> Result<Self, MegaError> {
        let connection = Arc::new(database_connection(&config.database).await?);
        Self::new_with_connection(config, connection, object_store).await
    }

    /// Build storage over an existing DB connection. Used by `AppContext::new`,
    /// which builds the connection once and shares it with the DB-only vault
    /// bootstrap, so object-storage credentials supplied as vault `SecretRef`s
    /// can be resolved before the object store is constructed (config.md stage 6
    /// / vault.md stage G).
    pub async fn new_with_connection(
        config: Arc<Config>,
        connection: Arc<DatabaseConnection>,
        object_store: MegaObjectStorageWrapper,
    ) -> Result<Self, MegaError> {
        let config_handle = ConfigHandle::from_arc(config.clone());
        let notification_storage = NotificationStorage::new(connection.clone());
        let base = BaseStorage::new(connection.clone());

        let mono_storage = MonoStorage { base: base.clone() };
        let git_db_storage = GitDbStorage { base: base.clone() };
        let gpg_storage = GpgStorage { base: base.clone() };
        let lfs_db_storage = LfsDbStorage { base: base.clone() };
        let cla_storage = ClaStorage { base: base.clone() };
        let user_storage = UserStorage { base: base.clone() };
        let group_storage = GroupStorage { base: base.clone() };
        let cl_storage = ClStorage { base: base.clone() };
        let issue_storage = IssueStorage { base: base.clone() };
        let vault_storage = VaultStorage { base: base.clone() };
        let conversation_storage = ConversationStorage { base: base.clone() };
        // `object_store` is injected by the caller (built via `build_object_storage`),
        // which delegates to the inlined orbit factory.
        let lfs_service = LfsService {
            lfs_storage: lfs_db_storage.clone(),
            obj_storage: object_store.clone(),
            // Disabled until `bind_storage_event_emitter` enables it, but it
            // already carries the configured `installation_id` (WH-15).
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::from_config_disabled(&config),
        };

        let commit_binding_storage = CommitBindingStorage { base: base.clone() };
        let reviewer_storage = ClReviewerStorage { base: base.clone() };
        let push_queue_storage = PushQueueStorage::new(base.clone());
        let buck_storage = BuckStorage { base: base.clone() };

        let code_review_comment_storage = CodeReviewCommentStorage { base: base.clone() };
        let code_review_thread_storage = CodeReviewThreadStorage { base: base.clone() };
        let bots_storage = BotsStorage { base: base.clone() };
        let webhook_storage = WebhookStorage { base: base.clone() };
        let audit_storage = AuditStorage { base: base.clone() };
        let oci_db_storage = OciDbStorage { base: base.clone() };
        let oci_service = OciService {
            oci_storage: oci_db_storage.clone(),
            obj_storage: object_store.clone(),
        };
        let agent_capture_service = AgentCaptureService {
            storage: AgentCaptureStorage { base: base.clone() },
            obj_storage: object_store.clone(),
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::from_config_disabled(&config),
        };

        let git_service = GitService {
            obj_storage: object_store.clone(),
        };
        let mono_service = MonoService {
            mono_storage: mono_storage.clone(),
            git_service: git_service.clone(),
        };

        let import_service = ImportService {
            git_db_storage: git_db_storage.clone(),
            git_service: git_service.clone(),
        };

        let buck_config = config.buck.clone().unwrap_or_default();

        validate_buck_config(&buck_config)?;

        let upload_semaphore = Arc::new(Semaphore::new(
            buck_config.upload_concurrency_limit as usize,
        ));
        let large_file_semaphore = Arc::new(Semaphore::new(
            buck_config.large_file_concurrency_limit as usize,
        ));

        let app_service = AppService {
            mono_storage: mono_storage.clone(),
            git_db_storage,
            gpg_storage,
            lfs_db_storage,
            cla_storage,
            user_storage,
            group_storage,
            vault_storage,
            cl_storage: cl_storage.clone(),
            issue_storage,
            conversation_storage,
            commit_binding_storage,
            reviewer_storage,
            push_queue_storage: push_queue_storage.clone(),
            buck_storage,
            code_review_comment_storage,
            code_review_thread_storage,
            bots_storage,
            webhook_storage: webhook_storage.clone(),
            audit_storage,
            oci_db_storage,
        };
        let push_queue_service =
            PushQueueService::new(base.clone(), config.monorepo.push_policy.clone())
                .with_max_push_commits(config.monorepo.max_push_commits);
        let artifact_service = ArtifactService::new(base.clone(), object_store.clone());
        let buck_service = BuckService::new(
            base.clone(),
            CLService::new(base.clone()),
            upload_semaphore,
            large_file_semaphore,
            buck_config,
            git_service.clone(),
        )?;

        let webhook_service = WebhookService::new(webhook_storage.clone())?;
        // Same disabled-with-installation-id emitter as the two services
        // above; `bind_storage_event_emitter` swaps in the enabled one.
        let storage_event_emitter =
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::from_config_disabled(&config);

        Ok(Storage {
            app_service: app_service.into(),
            cla_service: ClaService::new(base.clone()),
            config_handle,
            config,
            cl_service: CLService::new(base.clone()),
            push_queue_service,
            artifact_service,
            buck_service,
            git_service,
            mono_service,
            import_service,
            lfs_service,
            oci_service,
            agent_capture_service,
            code_review_service: CodeReviewService::new(base.clone()),
            webhook_service,
            storage_event_emitter,
            notification_storage,
            entity_store: Arc::new(SharedEntityStore::default()),
            vault: None,
        })
    }

    pub fn config_handle(&self) -> ConfigHandle {
        self.config_handle.clone()
    }

    /// Shared authorization snapshot holder (ADR-UN-02). Returns the same `Arc`
    /// instance injected by `AppContext`.
    /// Replace the shared authorization snapshot holder. Test-only: production
    /// receives exactly one instance from `AppContext` (ADR-UN-02), and
    /// swapping it at runtime would break the object-identity guarantee the
    /// notify path depends on.
    #[cfg(test)]
    pub fn set_entity_store_for_test(&mut self, entity_store: Arc<SharedEntityStore>) {
        self.entity_store = entity_store;
    }

    pub fn entity_store(&self) -> Arc<SharedEntityStore> {
        self.entity_store.clone()
    }

    /// Inject the shared authorization snapshot holder (ADR-UN-02). Called once
    /// by `AppContext` during bootstrap; the same `Arc` is shared with the HTTP
    /// state.
    pub fn set_entity_store(&mut self, store: Arc<SharedEntityStore>) {
        self.entity_store = store;
    }

    /// Install the single application storage-event emitter owner (WH-11).
    /// Called once by `AppContext` during bootstrap after the target secrets
    /// have been resolved; the default constructed state is disabled.
    pub fn set_storage_event_emitter(
        &mut self,
        emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    ) {
        // LfsService / AgentCaptureService are built before the vault-bound
        // emitter exists (WH-11), so rebinding must fan out (WH-05 / WH-07).
        self.lfs_service.storage_event_emitter = emitter.clone();
        self.agent_capture_service.storage_event_emitter = emitter.clone();
        self.storage_event_emitter = emitter;
    }

    /// The server-signing vault handle (MC-09), if wired in by `AppContext`.
    pub fn vault(&self) -> Option<&VaultCore> {
        self.vault.as_ref()
    }

    /// Builder-style injection of the server-signing vault handle (MC-09).
    /// Production calls this exactly once from `AppContext::new`; tests use
    /// it to inject a test `VaultCore`.
    pub fn with_vault(mut self, vault: VaultCore) -> Self {
        self.vault = Some(vault);
        self
    }

    pub fn config(&self) -> Arc<Config> {
        self.config_handle
            .snapshot()
            .unwrap_or_else(|_| Arc::clone(&self.config))
    }

    /// TP-15 / 4.1 ②③⑥: DB-backed fail-closed checks at HTTP start.
    ///
    /// ①④⑤ live in [`Config::validate`] (no DB). ②③⑥ run here because they
    /// read `mega_cl`, `push_queue`, and `queue_control.last_policy`. Check ⑥
    /// (watermark reset) runs together with ③ only after a successful empty-queue
    /// policy switch.
    pub async fn prepare_push_policy_startup(&self) -> Result<(), MegaError> {
        let config = self.config();
        let policy = config.monorepo.push_policy.as_str();
        if config.monorepo.push_policy == PushPolicy::Trunk {
            let open = self.cl_storage().get_open_cls().await?;
            if !open.is_empty() {
                return Err(MegaError::Other(format!(
                    "push_policy=trunk refuses startup with {} open change list(s); close or merge them first",
                    open.len()
                )));
            }
        }

        let queue = self.push_queue_storage();
        if let Some((from, to)) = queue.switch_last_policy_if_queue_empty(policy).await? {
            tracing::info!(
                from = %from,
                to = %to,
                "push_policy morphology switch: blob_paths.indexed_push_id reset, last_policy updated"
            );
        }
        Ok(())
    }

    /// Get recommended concurrency limit for batch database operations.
    ///
    /// Calculates 50% of max_connection, bounded between 4 and max_connection.
    pub fn get_recommended_batch_concurrency(&self) -> usize {
        let max_conn = self.config().database.max_connection as usize;

        // Handle edge case where config might be 0 or invalid
        let safe_conn = if max_conn == 0 { 16 } else { max_conn };

        // Internal calculation using pure function
        calculate_db_concurrency_limit(safe_conn, 50, 4)
    }

    pub fn mono_storage(&self) -> MonoStorage {
        self.app_service.mono_storage.clone()
    }

    /// Begin a database transaction on the shared app connection (monorepo + import metadata).
    pub async fn begin_db_transaction(&self) -> Result<sea_orm::DatabaseTransaction, MegaError> {
        use sea_orm::TransactionTrait;
        self.mono_storage()
            .get_connection()
            .begin()
            .await
            .map_err(MegaError::Db)
    }

    /// Best-effort classification/logging helper for object storage "not found" errors
    /// when dealing with Git blobs.
    ///
    /// Behavior:
    /// - If `err` is not `MegaError::ObjStorageNotFound`, it is returned unchanged.
    /// - If it is `ObjStorageNotFound(msg)`, we:
    ///   - Look up blob metadata via `mega_blob` table
    ///   - Emit structured logs to distinguish:
    ///       - "missing_in_db_and_s3" (likely never written / invalid request)
    ///       - "missing_in_s3_but_has_meta" (data loss or external deletion)
    ///   - Return a new `MegaError::ObjStorageNotFound` whose message is prefixed with
    ///     a classification tag:
    ///       - "[obj_missing_in_db_and_s3] ..."
    ///       - "[obj_missing_in_s3_but_has_meta] ..."
    ///       - "[obj_meta_lookup_failed] ..." (if metadata lookup itself fails)
    pub async fn classify_blob_objstorage_not_found(
        &self,
        hash: &str,
        err: MegaError,
    ) -> MegaError {
        match err {
            MegaError::ObjStorageNotFound(_msg) => {
                let mono_storage = self.mono_storage();

                match mono_storage
                    .get_mega_blobs_by_hashes(vec![hash.to_string()])
                    .await
                {
                    Ok(blobs) if blobs.is_empty() => {
                        let friendly = format!(
                            "[obj_missing_in_db_and_s3] Blob {hash} not found in both object storage and metadata (likely never written or invalid request)"
                        );
                        tracing::warn!("{}", friendly);
                        MegaError::ObjStorageNotFound(friendly)
                    }
                    Ok(mut blobs) => {
                        let Some(blob) = blobs.pop() else {
                            let friendly = format!(
                                "[obj_missing_in_db_and_s3] Blob {hash} not found in both object storage and metadata (likely never written or invalid request)"
                            );
                            tracing::warn!("{}", friendly);
                            return MegaError::ObjStorageNotFound(friendly);
                        };
                        tracing::error!(
                            "[obj_missing_in_s3_but_has_meta] Object with hash {hash} missing in S3 but metadata exists in DB for blob_id {}; possible data loss or misconfiguration",
                            blob.blob_id,
                        );
                        MegaError::ObjStorageInconsistent(format!(
                            "[obj_missing_in_s3_but_has_meta] Object {hash} missing in S3 but metadata exists; possible data loss or misconfiguration"
                        ))
                    }
                    Err(_) => MegaError::ObjStorageInconsistent(format!(
                        "[obj_meta_lookup_failed] Failed to query blob {hash} metadata while handling ObjStorageNotFound"
                    )),
                }
            }
            other => other,
        }
    }

    pub fn git_db_storage(&self) -> GitDbStorage {
        self.app_service.git_db_storage.clone()
    }

    pub fn gpg_storage(&self) -> GpgStorage {
        self.app_service.gpg_storage.clone()
    }

    pub fn lfs_db_storage(&self) -> LfsDbStorage {
        self.app_service.lfs_db_storage.clone()
    }

    pub fn user_storage(&self) -> UserStorage {
        self.app_service.user_storage.clone()
    }

    pub fn cla_storage(&self) -> ClaStorage {
        self.app_service.cla_storage.clone()
    }

    pub fn group_storage(&self) -> GroupStorage {
        self.app_service.group_storage.clone()
    }

    pub fn vault_storage(&self) -> VaultStorage {
        self.app_service.vault_storage.clone()
    }

    pub fn cl_storage(&self) -> ClStorage {
        self.app_service.cl_storage.clone()
    }

    pub fn issue_storage(&self) -> IssueStorage {
        self.app_service.issue_storage.clone()
    }

    pub fn conversation_storage(&self) -> ConversationStorage {
        self.app_service.conversation_storage.clone()
    }

    pub fn commit_binding_storage(&self) -> CommitBindingStorage {
        self.app_service.commit_binding_storage.clone()
    }

    pub fn reviewer_storage(&self) -> ClReviewerStorage {
        self.app_service.reviewer_storage.clone()
    }

    pub fn push_queue_storage(&self) -> PushQueueStorage {
        self.app_service.push_queue_storage.clone()
    }

    pub fn buck_storage(&self) -> BuckStorage {
        self.app_service.buck_storage.clone()
    }

    pub fn code_review_thread_storage(&self) -> CodeReviewThreadStorage {
        self.app_service.code_review_thread_storage.clone()
    }

    pub fn code_review_comment_storage(&self) -> CodeReviewCommentStorage {
        self.app_service.code_review_comment_storage.clone()
    }

    pub fn webhook_storage(&self) -> WebhookStorage {
        self.app_service.webhook_storage.clone()
    }

    pub fn notification_storage(&self) -> notification_storage::NotificationStorage {
        self.notification_storage.clone()
    }

    pub fn audit_storage(&self) -> AuditStorage {
        self.app_service.audit_storage.clone()
    }

    pub fn bots_storage(&self) -> BotsStorage {
        self.app_service.bots_storage.clone()
    }

    pub fn oci_db_storage(&self) -> OciDbStorage {
        self.app_service.oci_db_storage.clone()
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        let mut config = crate::config::testing::isolated_config(
            std::env::temp_dir().join("mega2-storage-mock"),
        );
        config.database.max_connection = 16;
        config.database.min_connection = 8;
        let config = Arc::new(config);

        let app_service = AppService::mock();
        let webhook_service = WebhookService::mock(app_service.webhook_storage.clone());

        Storage {
            app_service,
            // app_service: AppService::mock(),
            cla_service: ClaService::mock(),
            cl_service: CLService::mock(),
            push_queue_service: PushQueueService::new(
                BaseStorage::mock(),
                crate::config::PushPolicy::Review,
            ),
            artifact_service: ArtifactService::mock(),
            buck_service: BuckService::mock(),
            config_handle: ConfigHandle::from_arc(config.clone()),
            config,
            git_service: GitService::mock(),
            mono_service: MonoService::mock(),
            import_service: ImportService::mock(),
            lfs_service: LfsService::mock(),
            oci_service: OciService::mock(),
            agent_capture_service: AgentCaptureService::mock(),
            code_review_service: CodeReviewService::mock(),
            webhook_service,
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
            notification_storage: NotificationStorage::new(Arc::new(DatabaseConnection::default())),
            entity_store: Arc::new(SharedEntityStore::default()),
            vault: None,
        }
    }
}

// Private helper function for concurrency calculation
// This is a pure function for easy testing and potential reuse
fn calculate_db_concurrency_limit(
    max_connections: usize,
    percentage: usize,
    min_limit: usize,
) -> usize {
    let calculated = (max_connections * percentage) / 100;
    calculated.max(min_limit).min(max_connections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_db_concurrency_limit() {
        // Normal case: 50% of 16 = 8
        assert_eq!(calculate_db_concurrency_limit(16, 50, 4), 8);

        // Min bound: 50% of 5 = 2.5 -> clamped to min 4
        assert_eq!(calculate_db_concurrency_limit(5, 50, 4), 4);

        // Max bound: 50% of 100 = 50 (within bounds)
        assert_eq!(calculate_db_concurrency_limit(100, 50, 4), 50);

        // Edge case: small connection pool
        assert_eq!(calculate_db_concurrency_limit(4, 50, 4), 4);

        // Edge case: very small connection pool (cannot exceed max_connections)
        assert_eq!(calculate_db_concurrency_limit(2, 50, 4), 2);
    }

    #[test]
    fn test_get_recommended_batch_concurrency() {
        // Create a mock Storage for testing
        let storage = Storage::mock();

        // The mock config should have default max_connection = 16
        let concurrency = storage.get_recommended_batch_concurrency();

        // Should be 50% of 16 = 8
        assert_eq!(concurrency, 8);
    }

    #[test]
    fn mock_storage_includes_agent_capture_service() {
        let storage = Storage::mock();
        assert_eq!(
            crate::orbit_api::object_storage::ObjectNamespace::Agent.to_string(),
            "agent"
        );
        let _ = &storage.agent_capture_service;
    }

    #[test]
    fn storage_config_reads_current_config_handle_snapshot() {
        let storage = Storage::mock();
        let handle = storage.config_handle();
        let mut candidate = storage.config().as_ref().clone();
        candidate.log.level = "debug".to_string();

        handle.reload(candidate).expect("reload should apply");

        assert_eq!(storage.config().log.level, "debug");
    }
}

/// The read surface an audit command gets (UN-30).
///
/// [`Storage::new_with_connection`] is not usable for reading: it assembles
/// every service in the system, several of which exist to write. That is not
/// something an audit should carry.
///
/// So this is built from the pieces a read actually needs and nothing else. It
/// is deliberately small: what an audit reads is the authorization source in the
/// monorepo root, which takes the mono storage to resolve the ref and tree and
/// the object store to fetch the blob. Adding more later should mean adding a
/// read someone needs, not restoring the full assembly.
#[derive(Clone)]
pub struct ReadOnlyStorage {
    base: BaseStorage,
    mono_storage: MonoStorage,
    git_service: GitService,
    config: Arc<Config>,
}

impl ReadOnlyStorage {
    /// Assemble over an existing read-only connection.
    ///
    /// The connection is the caller's: `ReadOnlyContext` builds it through
    /// [`init::read_only_database_connection`], so no migration runs and the
    /// server rejects writes. Nothing here writes either, which is the point —
    /// the database-level guarantee is the backstop, not the reason.
    pub fn new(
        config: Arc<Config>,
        connection: Arc<DatabaseConnection>,
        object_store: MegaObjectStorageWrapper,
    ) -> Self {
        let base = BaseStorage::new(connection);
        Self {
            mono_storage: MonoStorage { base: base.clone() },
            git_service: GitService {
                obj_storage: object_store,
            },
            base,
            config,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub fn mono_storage(&self) -> &MonoStorage {
        &self.mono_storage
    }

    pub fn git_service(&self) -> &GitService {
        &self.git_service
    }

    pub fn base(&self) -> &BaseStorage {
        &self.base
    }

    /// Read the in-repo authorization source from the monorepo root.
    ///
    /// This is the read the audit exists to perform, and it is the same walk the
    /// server does at startup — root ref, root tree, the named blob — so the
    /// audit describes the file the server would actually load rather than a
    /// separately-derived idea of it.
    pub async fn read_authz_source(&self) -> Result<String, MegaError> {
        use git_internal::internal::object::tree::Tree;

        use crate::{
            callisto::mega_tree, contract::policy::entitystore::MEGA_CEDAR_PATH,
            jupiter::utils::converter::FromMegaModel,
        };

        let root_ref = self
            .mono_storage
            .get_main_ref("/")
            .await?
            .ok_or_else(|| MegaError::Other("Root ref not found".into()))?;
        let root_tree: mega_tree::Model = self
            .mono_storage
            .get_tree_by_hash(&root_ref.ref_tree_hash)
            .await?
            .ok_or_else(|| MegaError::Other("Root tree not found".into()))?;
        let root_tree = Tree::from_mega_model(root_tree);

        let file_name = MEGA_CEDAR_PATH.trim_start_matches('/');
        let blob = root_tree
            .tree_items
            .iter()
            .find(|item| item.name == file_name)
            .ok_or_else(|| MegaError::Other(format!("{file_name} not found in root directory")))?;

        let bytes = self
            .git_service
            .get_object_as_bytes(&blob.id.to_string())
            .await?;
        String::from_utf8(bytes).map_err(|e| MegaError::Other(format!("UTF-8 decode failed: {e}")))
    }
}
