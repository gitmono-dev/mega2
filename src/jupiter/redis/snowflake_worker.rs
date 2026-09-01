use std::{
    mem::ManuallyDrop,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use futures::FutureExt;
use redis::{Script, aio::ConnectionManager};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, Statement,
    TransactionTrait,
};
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::jupiter::utils::id_generator::{self, MAX_WORKER_ID, WorkerLeaseHealth};

const SLOT_KEY_PREFIX: &str = "mega:snowflake:worker:";
const SLOT_TTL_MS: u64 = 30_000;
const SLOT_REFRESH_INTERVAL_MS: u64 = SLOT_TTL_MS / 2;
const SLOT_REFRESH_TIMEOUT_MS: u64 = 5_000;
const SLOT_RELEASE_TIMEOUT_MS: u64 = 5_000;
// Includes the cross-store worker-reuse grace. A slot scan still stops as
// soon as it finds a usable slot; the bound only applies to slow Redis/PG
// operations and the one-time fencing grace.
const SLOT_SCAN_TIMEOUT_MS: u64 = 35_000;
const SLOT_CLAIM_TIMEOUT_MS: u64 = 1_000;
const SLOT_SCAN_CLEANUP_TIMEOUT_MS: u64 = 5_000;
// A newly claimed Redis/PG slot must remain fenced for the full period in
// which the previous process could still consider its local lease healthy.
// Redis is refreshed during this grace period so the claim cannot expire.
const WORKER_REUSE_GRACE_MS: u64 = 25_000;
const FENCE_ACQUIRE_TIMEOUT_MS: u64 = 5_000;
const FENCE_ROLLBACK_TIMEOUT_MS: u64 = 5_000;
const FENCE_VERIFY_TIMEOUT_MS: u64 = 5_000;
const WORKER_CLEANUP_TIMEOUT_MS: u64 = 10_000;
const WORKER_FENCE_NAMESPACE: i32 = 0x004d_4f4e;

struct WorkerFenceInner {
    transaction: Arc<Mutex<Option<ManuallyDrop<DatabaseTransaction>>>>,
}

/// A PostgreSQL transaction-scoped advisory lock that fences a worker slot
/// independently of Redis key expiry. The transaction is held for as long as
/// the process owns the worker selection, so a Redis restart cannot let a new
/// process reuse the slot while the old process is still alive.
#[derive(Clone)]
pub(crate) struct WorkerFence {
    inner: Arc<WorkerFenceInner>,
}

impl WorkerFence {
    fn new(transaction: DatabaseTransaction) -> Self {
        Self {
            inner: Arc::new(WorkerFenceInner {
                transaction: Arc::new(Mutex::new(Some(ManuallyDrop::new(transaction)))),
            }),
        }
    }

    pub(crate) async fn verify(&self) -> Result<(), crate::common::errors::MegaError> {
        let transaction = self.inner.transaction.lock().await;
        let transaction = transaction.as_ref().ok_or_else(|| {
            crate::common::errors::MegaError::IdGenerationUnavailable(
                "worker database fence is no longer active".to_string(),
            )
        })?;
        timeout(
            Duration::from_millis(FENCE_VERIFY_TIMEOUT_MS),
            transaction.query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT 1".to_string(),
            )),
        )
        .await
        .map_err(|_| {
            crate::common::errors::MegaError::Other(format!(
                "worker database fence verification timed out after {FENCE_VERIFY_TIMEOUT_MS}ms"
            ))
        })??;
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<(), crate::common::errors::MegaError> {
        let transaction = self
            .inner
            .transaction
            .lock()
            .await
            .take()
            .map(ManuallyDrop::into_inner);
        if let Some(transaction) = transaction {
            rollback_fence(transaction).await
        } else {
            Ok(())
        }
    }
}

impl Drop for WorkerFenceInner {
    fn drop(&mut self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let transaction = Arc::clone(&self.transaction);
            handle.spawn(async move {
                let transaction = transaction
                    .lock()
                    .await
                    .take()
                    .map(ManuallyDrop::into_inner);
                if let Some(transaction) = transaction
                    && let Err(error) = rollback_fence(transaction).await
                {
                    tracing::warn!(error = %error, "failed to roll back dropped worker fence");
                }
            });
        }
        // `DatabaseTransaction` has a fallible Drop implementation. When the
        // last fence is dropped outside a Tokio runtime, retaining it in its
        // ManuallyDrop slot is the only panic-free option; normal application
        // shutdown always runs through the async branch above.
    }
}

async fn rollback_fence(
    transaction: DatabaseTransaction,
) -> Result<(), crate::common::errors::MegaError> {
    // Isolate SeaORM's fallible transaction Drop in a task. If the database is
    // already broken, an aborted rollback must not unwind the caller through
    // the driver's panic-on-drop path.
    let mut rollback = tokio::spawn(async move {
        std::panic::AssertUnwindSafe(transaction.rollback())
            .catch_unwind()
            .await
    });
    match timeout(
        Duration::from_millis(FENCE_ROLLBACK_TIMEOUT_MS),
        &mut rollback,
    )
    .await
    {
        Ok(Ok(Ok(Ok(())))) => Ok(()),
        Ok(Ok(Ok(Err(error)))) => Err(error.into()),
        Ok(Ok(Err(_))) => Err(crate::common::errors::MegaError::Other(
            "worker database fence rollback panicked".to_string(),
        )),
        Ok(Err(error)) => Err(crate::common::errors::MegaError::Other(format!(
            "worker database fence rollback task failed: {error}"
        ))),
        Err(_) => {
            rollback.abort();
            let _ = rollback.await;
            Err(crate::common::errors::MegaError::Other(format!(
                "worker database fence rollback timed out after {FENCE_ROLLBACK_TIMEOUT_MS}ms"
            )))
        }
    }
}

/// Try to acquire a database-side fence for a worker slot. PostgreSQL advisory
/// locks are session/transaction scoped and therefore survive Redis key loss as
/// long as the old process still owns its database transaction.
pub(crate) async fn try_acquire_worker_fence(
    connection: &DatabaseConnection,
    worker_id: u32,
) -> Result<Option<WorkerFence>, crate::common::errors::MegaError> {
    if connection.get_database_backend() != DatabaseBackend::Postgres {
        return Ok(None);
    }

    let transaction = timeout(
        Duration::from_millis(FENCE_ACQUIRE_TIMEOUT_MS),
        connection.begin(),
    )
    .await
    .map_err(|_| {
        crate::common::errors::MegaError::Other(format!(
            "worker database fence acquisition timed out after {FENCE_ACQUIRE_TIMEOUT_MS}ms"
        ))
    })??;
    let statement = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT pg_try_advisory_xact_lock($1, $2) AS locked",
        vec![WORKER_FENCE_NAMESPACE.into(), (worker_id as i32).into()],
    );
    let row = match timeout(
        Duration::from_millis(FENCE_ACQUIRE_TIMEOUT_MS),
        transaction.query_one_raw(statement),
    )
    .await
    {
        Ok(Ok(row)) => row,
        Ok(Err(error)) => {
            if let Err(rollback_error) = rollback_fence(transaction).await {
                tracing::warn!(error = %rollback_error, "failed to roll back worker fence after query error");
            }
            return Err(error.into());
        }
        Err(_) => {
            if let Err(error) = rollback_fence(transaction).await {
                tracing::warn!(error = %error, "failed to roll back timed-out worker fence query");
            }
            return Err(crate::common::errors::MegaError::Other(format!(
                "worker database fence query timed out after {FENCE_ACQUIRE_TIMEOUT_MS}ms"
            )));
        }
    };
    let Some(row) = row else {
        if let Err(error) = rollback_fence(transaction).await {
            tracing::warn!(error = %error, "failed to roll back empty worker fence query");
        }
        return Err(crate::common::errors::MegaError::Other(
            "worker fence query returned no row".to_string(),
        ));
    };
    let locked: bool = match row.try_get("", "locked") {
        Ok(locked) => locked,
        Err(error) => {
            if let Err(rollback_error) = rollback_fence(transaction).await {
                tracing::warn!(error = %rollback_error, "failed to roll back worker fence after result error");
            }
            return Err(error.into());
        }
    };
    if !locked {
        rollback_fence(transaction).await?;
        return Ok(None);
    }

    Ok(Some(WorkerFence::new(transaction)))
}

struct WorkerLeaseInner {
    worker_id: u32,
    key: String,
    token: String,
    connection: ConnectionManager,
    health: Arc<WorkerLeaseHealth>,
    fence: Option<WorkerFence>,
    cancel: CancellationToken,
    task: StdMutex<Option<JoinHandle<()>>>,
}

struct SlotRefreshConfig {
    connection: ConnectionManager,
    key: String,
    token: String,
    health: Arc<WorkerLeaseHealth>,
    fence: Option<WorkerFence>,
    cancel: CancellationToken,
    interval_ms: u64,
    ttl_ms: u64,
}

/// A process-owned Redis worker slot and its refresh task.
///
/// The guard is cloneable because `AppContext` is cloneable. The refresh task
/// is nevertheless stored exactly once behind the shared inner value, and the
/// final drop cancels it. Call [`Self::shutdown`] during graceful service
/// shutdown to await the task and release the slot immediately.
#[derive(Clone)]
pub struct SnowflakeWorkerLease {
    inner: Arc<WorkerLeaseInner>,
}

impl SnowflakeWorkerLease {
    fn new(
        worker_id: u32,
        key: String,
        token: String,
        connection: ConnectionManager,
        health: Arc<WorkerLeaseHealth>,
        fence: Option<WorkerFence>,
    ) -> Self {
        Self {
            inner: Arc::new(WorkerLeaseInner {
                worker_id,
                key,
                token,
                connection,
                health,
                fence,
                cancel: CancellationToken::new(),
                task: StdMutex::new(None),
            }),
        }
    }

    pub fn worker_id(&self) -> u32 {
        self.inner.worker_id
    }

    pub(crate) fn health(&self) -> Arc<WorkerLeaseHealth> {
        Arc::clone(&self.inner.health)
    }

    /// Start the single refresh task after the ID generator has been bound to
    /// this lease. Calling this more than once is idempotent.
    pub fn start_refresh(&self) {
        let mut task = match self.inner.task.lock() {
            Ok(task) => task,
            Err(poisoned) => poisoned.into_inner(),
        };
        if task.is_some() {
            return;
        }

        let inner = Arc::clone(&self.inner);
        *task = Some(tokio::spawn(run_slot_refresh(SlotRefreshConfig {
            connection: inner.connection.clone(),
            key: inner.key.clone(),
            token: inner.token.clone(),
            health: Arc::clone(&inner.health),
            fence: inner.fence.clone(),
            cancel: inner.cancel.clone(),
            interval_ms: SLOT_REFRESH_INTERVAL_MS,
            ttl_ms: SLOT_TTL_MS,
        })));
    }

    /// Stop refresh, await its completion, and release this token-owned slot.
    pub async fn shutdown(&self) -> Result<(), crate::common::errors::MegaError> {
        // Revoke before cancellation/release. A refresh that is already in
        // flight must not reactivate the generator after shutdown begins.
        id_generator::revoke_worker_lease(&self.inner.health);
        self.inner.cancel.cancel();
        let task = match self.inner.task.lock() {
            Ok(mut task) => task.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let task_error = if let Some(mut task) = task {
            match timeout(Duration::from_millis(WORKER_CLEANUP_TIMEOUT_MS), &mut task).await {
                Ok(result) => result
                    .err()
                    .map(|error| crate::common::errors::MegaError::Other(error.to_string())),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Some(crate::common::errors::MegaError::Other(format!(
                        "snowflake worker refresh shutdown timed out after {WORKER_CLEANUP_TIMEOUT_MS}ms"
                    )))
                }
            }
        } else {
            None
        };

        let mut connection = self.inner.connection.clone();
        let release_result =
            release_slot_bounded(&mut connection, &self.inner.key, &self.inner.token)
                .await
                .map(|_| ());

        let mut first_error = task_error;
        if first_error.is_none() {
            first_error = release_result.err();
        }
        if let Some(fence) = &self.inner.fence
            && let Err(error) = fence.shutdown().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for WorkerLeaseInner {
    fn drop(&mut self) {
        id_generator::revoke_worker_lease(&self.health);
        self.cancel.cancel();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mut connection = self.connection.clone();
            let key = self.key.clone();
            let token = self.token.clone();
            let fence = self.fence.clone();
            handle.spawn(async move {
                if let Err(error) = release_slot_bounded(&mut connection, &key, &token).await {
                    tracing::warn!(error = %error, slot_key = %key, "failed to release dropped snowflake worker slot");
                }
                if let Some(fence) = fence
                    && let Err(error) = fence.shutdown().await
                {
                    tracing::warn!(error = %error, "failed to release dropped snowflake worker fence");
                }
            });
        }
    }
}

/// Try to exclusively claim a Snowflake worker slot in Redis (SET NX PX).
///
/// Returns `Ok(None)` if Redis is unavailable, the bounded scan times out, or
/// every slot in 0..=MAX_WORKER_ID is taken. The caller may record a stable
/// identity hash for diagnostics, but must not generate IDs from that hash in
/// a multi-writer process. On success, the caller must bind the returned guard
/// to the ID generator before starting its refresh task.
pub(crate) async fn claim_snowflake_worker(
    connection: &ConnectionManager,
    database: &DatabaseConnection,
) -> Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError> {
    claim_snowflake_worker_inner(connection, database, None).await
}

/// Claim the Redis lease for one explicitly configured worker ID. The explicit
/// value remains the source of the ID, but it is still backed by the same Redis
/// lease and PostgreSQL fence as an automatically selected worker.
pub(crate) async fn claim_snowflake_worker_for_id(
    connection: &ConnectionManager,
    database: &DatabaseConnection,
    worker_id: u32,
) -> Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError> {
    if worker_id > MAX_WORKER_ID {
        return Err(crate::common::errors::MegaError::IdGenerationUnavailable(
            format!("worker ID {worker_id} is outside 0..={MAX_WORKER_ID}"),
        ));
    }
    claim_snowflake_worker_inner(connection, database, Some(worker_id)).await
}

async fn claim_snowflake_worker_inner(
    connection: &ConnectionManager,
    database: &DatabaseConnection,
    preferred_worker_id: Option<u32>,
) -> Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError> {
    let identity = id_generator::process_identity();
    let token = Uuid::new_v4().to_string();
    let cancellation = CancellationToken::new();
    let scan_cancellation = cancellation.clone();
    let scan_connection = connection.clone();
    let scan_database = database.clone();
    let scan_identity = identity.clone();
    let scan_token = token.clone();
    let scan = tokio::spawn(async move {
        claim_worker_lease(
            &scan_connection,
            SLOT_KEY_PREFIX,
            &scan_identity,
            scan_token,
            preferred_worker_id,
            Some(&scan_database),
            &scan_cancellation,
        )
        .await
    });
    let mut scan_guard = ClaimScanGuard::new(
        scan,
        cancellation,
        connection.clone(),
        SLOT_KEY_PREFIX.to_owned(),
        token,
    );
    let claim = if let Some(scan) = scan_guard.scan.as_mut() {
        timeout(Duration::from_millis(SLOT_SCAN_TIMEOUT_MS), scan).await
    } else {
        return Ok(None);
    };

    match claim {
        Ok(Ok(Ok(lease))) => {
            scan_guard.disarm();
            Ok(lease)
        }
        Ok(Ok(Err(error))) => {
            tracing::warn!(error = %error, "snowflake worker slot scan failed");
            scan_guard.cleanup();
            Ok(None)
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "snowflake worker slot scan task failed");
            scan_guard.cleanup();
            Ok(None)
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = SLOT_SCAN_TIMEOUT_MS,
                process_identity = %id_generator::identity_digest(&identity),
                "snowflake worker slot scan timed out; ID writes require explicit exclusive worker configuration"
            );
            scan_guard.cleanup();
            Ok(None)
        }
    }
}

struct ClaimScanGuard {
    scan:
        Option<JoinHandle<Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError>>>,
    cancellation: CancellationToken,
    connection: ConnectionManager,
    slot_key_prefix: String,
    token: String,
}

impl ClaimScanGuard {
    fn new(
        scan: JoinHandle<Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError>>,
        cancellation: CancellationToken,
        connection: ConnectionManager,
        slot_key_prefix: String,
        token: String,
    ) -> Self {
        Self {
            scan: Some(scan),
            cancellation,
            connection,
            slot_key_prefix,
            token,
        }
    }

    fn disarm(&mut self) {
        self.scan.take();
    }

    fn cleanup(&mut self) {
        let Some(scan) = self.scan.take() else {
            return;
        };
        self.cancellation.cancel();
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn_abandoned_claim_cleanup(
                scan,
                self.connection.clone(),
                self.slot_key_prefix.clone(),
                self.token.clone(),
            );
        } else {
            scan.abort();
        }
    }
}

impl Drop for ClaimScanGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn spawn_abandoned_claim_cleanup(
    scan: JoinHandle<Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError>>,
    connection: ConnectionManager,
    slot_key_prefix: String,
    token: String,
) {
    tokio::spawn(async move {
        let mut scan = scan;
        match timeout(
            Duration::from_millis(SLOT_SCAN_CLEANUP_TIMEOUT_MS),
            &mut scan,
        )
        .await
        {
            Ok(Ok(Ok(Some(lease)))) => {
                if let Err(error) = lease.shutdown().await {
                    tracing::warn!(error = %error, "failed to shut down an abandoned snowflake worker lease");
                }
            }
            Ok(Ok(Ok(None))) => {}
            Ok(Ok(Err(error))) => {
                tracing::warn!(error = %error, "abandoned snowflake worker slot scan returned an error");
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "abandoned snowflake worker slot scan task failed");
            }
            Err(_) => {
                scan.abort();
                let _ = scan.await;
                tracing::warn!(
                    timeout_ms = SLOT_SCAN_CLEANUP_TIMEOUT_MS,
                    "abandoned snowflake worker slot scan was aborted before cleanup"
                );
            }
        }

        let mut connection = connection;
        if let Err(error) =
            release_token_slots_bounded(&mut connection, &slot_key_prefix, &token).await
        {
            tracing::warn!(error = %error, "failed to clean up abandoned snowflake worker claims");
        }
    });
}

async fn claim_worker_lease(
    connection: &ConnectionManager,
    slot_key_prefix: &str,
    identity: &str,
    token: String,
    preferred_worker_id: Option<u32>,
    database: Option<&DatabaseConnection>,
    cancellation: &CancellationToken,
) -> Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError> {
    let mut conn = connection.clone();
    let worker_ids = preferred_worker_id
        .map(|worker_id| vec![worker_id])
        .unwrap_or_else(|| (0..=MAX_WORKER_ID).collect());

    for worker_id in worker_ids {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let key = format!("{slot_key_prefix}{worker_id}");
        let claim = tokio::select! {
            _ = cancellation.cancelled() => return Ok(None),
            result = timeout(
                Duration::from_millis(SLOT_CLAIM_TIMEOUT_MS),
                claim_slot(&mut conn, &key, &token, SLOT_TTL_MS),
            ) => match result {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        timeout_ms = SLOT_CLAIM_TIMEOUT_MS,
                        slot_key = %key,
                        "snowflake worker slot claim timed out"
                    );
                    return Err(crate::common::errors::MegaError::Other(format!(
                        "snowflake worker slot claim timed out after {SLOT_CLAIM_TIMEOUT_MS}ms"
                    )));
                }
            },
        };
        match claim {
            Ok(true) => {
                if cancellation.is_cancelled() {
                    let _ = release_slot_bounded(&mut conn, &key, &token).await;
                    return Ok(None);
                }
                let claim_started = Instant::now();
                let fence = if let Some(database) = database {
                    match try_acquire_worker_fence(database, worker_id).await {
                        Ok(Some(fence)) => Some(fence),
                        Ok(None) => {
                            let _ = release_slot_bounded(&mut conn, &key, &token).await;
                            continue;
                        }
                        Err(error) => {
                            let _ = release_slot_bounded(&mut conn, &key, &token).await;
                            return Err(error);
                        }
                    }
                } else {
                    None
                };

                if let Some(fence) = fence {
                    match wait_for_worker_reuse_grace(
                        &mut conn,
                        &key,
                        &token,
                        claim_started,
                        cancellation,
                    )
                    .await
                    {
                        Ok(true) => {
                            // Start the local health deadline only after the
                            // cross-store reuse grace has completed.
                            let health = WorkerLeaseHealth::claimed();
                            if cancellation.is_cancelled() {
                                let _ = release_slot_bounded(&mut conn, &key, &token).await;
                                let _ = fence.shutdown().await;
                                return Ok(None);
                            }
                            let lease = SnowflakeWorkerLease::new(
                                worker_id,
                                key,
                                token.clone(),
                                connection.clone(),
                                health,
                                Some(fence),
                            );
                            tracing::info!(
                                worker_id,
                                slot_key = %lease.inner.key,
                                process_identity = %id_generator::identity_digest(identity),
                                "claimed snowflake worker slot"
                            );
                            return Ok(Some(lease));
                        }
                        Ok(false) => {
                            let _ = release_slot_bounded(&mut conn, &key, &token).await;
                            let _ = fence.shutdown().await;
                            return Ok(None);
                        }
                        Err(error) => {
                            let _ = release_slot_bounded(&mut conn, &key, &token).await;
                            let _ = fence.shutdown().await;
                            return Err(error);
                        }
                    }
                }

                let health = WorkerLeaseHealth::claimed();
                if cancellation.is_cancelled() {
                    let _ = release_slot_bounded(&mut conn, &key, &token).await;
                    return Ok(None);
                }
                let lease = SnowflakeWorkerLease::new(
                    worker_id,
                    key,
                    token.clone(),
                    connection.clone(),
                    health,
                    fence,
                );
                tracing::info!(
                    worker_id,
                    slot_key = %lease.inner.key,
                    process_identity = %id_generator::identity_digest(identity),
                    "claimed snowflake worker slot"
                );
                return Ok(Some(lease));
            }
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot claim failed; ID writes require explicit exclusive worker configuration"
                );
                return Err(error.into());
            }
        }
    }

    tracing::warn!(
        max_worker_id = MAX_WORKER_ID,
        process_identity = %id_generator::identity_digest(identity),
        "all snowflake worker slots are taken; ID writes require explicit exclusive worker configuration"
    );
    Ok(None)
}

async fn wait_for_worker_reuse_grace(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
    claim_started: Instant,
    cancellation: &CancellationToken,
) -> Result<bool, crate::common::errors::MegaError> {
    let deadline = claim_started + Duration::from_millis(WORKER_REUSE_GRACE_MS);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(true);
        }

        let remaining = deadline.saturating_duration_since(now);
        let wait = remaining.min(Duration::from_millis(SLOT_REFRESH_INTERVAL_MS));
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(false),
            _ = tokio::time::sleep(wait) => {}
        }

        if Instant::now() >= deadline {
            return Ok(true);
        }

        match timeout(
            Duration::from_millis(SLOT_REFRESH_TIMEOUT_MS),
            refresh_slot(connection, key, token, SLOT_TTL_MS),
        )
        .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return Ok(false),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => {
                return Err(crate::common::errors::MegaError::Other(format!(
                    "snowflake worker reuse grace refresh timed out after {SLOT_REFRESH_TIMEOUT_MS}ms"
                )));
            }
        }
    }
}

async fn claim_slot(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
    ttl_ms: u64,
) -> redis::RedisResult<bool> {
    let result: Option<String> = redis::cmd("SET")
        .arg(key)
        .arg(token)
        .arg("NX")
        .arg("PX")
        .arg(ttl_ms)
        .query_async(connection)
        .await?;
    Ok(result.is_some())
}

async fn run_slot_refresh(config: SlotRefreshConfig) {
    let SlotRefreshConfig {
        connection,
        key,
        token,
        health,
        fence,
        cancel,
        interval_ms,
        ttl_ms,
    } = config;
    let mut conn = connection;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                stop_refresh(&mut conn, &key, &token, &health, fence.as_ref()).await;
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
        }

        if cancel.is_cancelled() {
            continue;
        }

        if let Some(fence) = &fence
            && let Err(error) = fence.verify().await
        {
            tracing::warn!(error = %error, slot_key = %key, "snowflake worker database fence lost");
            stop_refresh(&mut conn, &key, &token, &health, Some(fence)).await;
            break;
        }

        match timeout(
            Duration::from_millis(SLOT_REFRESH_TIMEOUT_MS),
            refresh_slot(&mut conn, &key, &token, ttl_ms),
        )
        .await
        {
            Ok(Ok(true)) => {
                if !id_generator::refresh_worker_lease(&health) {
                    tracing::warn!(
                        slot_key = %key,
                        "snowflake worker refresh completed after local lease revocation"
                    );
                    stop_refresh(&mut conn, &key, &token, &health, fence.as_ref()).await;
                    break;
                }
            }
            Ok(Ok(false)) => {
                tracing::warn!(
                    slot_key = %key,
                    "snowflake worker slot refresh lost ownership"
                );
                stop_refresh(&mut conn, &key, &token, &health, fence.as_ref()).await;
                break;
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot refresh failed"
                );
                stop_refresh(&mut conn, &key, &token, &health, fence.as_ref()).await;
                break;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = SLOT_REFRESH_TIMEOUT_MS,
                    slot_key = %key,
                    "snowflake worker slot refresh timed out"
                );
                stop_refresh(&mut conn, &key, &token, &health, fence.as_ref()).await;
                break;
            }
        }
    }
}

async fn stop_refresh(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
    health: &std::sync::Arc<WorkerLeaseHealth>,
    fence: Option<&WorkerFence>,
) {
    id_generator::revoke_worker_lease(health);
    match release_slot_bounded(connection, key, token).await {
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(error = %error, slot_key = %key, "failed to release snowflake worker slot");
        }
    }
    if let Some(fence) = fence
        && let Err(error) = fence.shutdown().await
    {
        tracing::warn!(error = %error, slot_key = %key, "failed to release snowflake worker fence");
    }
}

async fn refresh_slot(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
    ttl_ms: u64,
) -> redis::RedisResult<bool> {
    let script = Script::new(
        r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("PEXPIRE", KEYS[1], ARGV[2])
            else
                return 0
            end
        "#,
    );
    let refreshed: i32 = script
        .key(key)
        .arg(token)
        .arg(ttl_ms)
        .invoke_async(connection)
        .await?;
    Ok(refreshed == 1)
}

async fn release_slot(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
) -> redis::RedisResult<bool> {
    let script = Script::new(
        r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("DEL", KEYS[1])
            else
                return 0
            end
        "#,
    );
    let released: i32 = script.key(key).arg(token).invoke_async(connection).await?;
    Ok(released == 1)
}

async fn release_slot_bounded(
    connection: &mut ConnectionManager,
    key: &str,
    token: &str,
) -> Result<bool, crate::common::errors::MegaError> {
    timeout(
        Duration::from_millis(SLOT_RELEASE_TIMEOUT_MS),
        release_slot(connection, key, token),
    )
    .await
    .map_err(|_| {
        crate::common::errors::MegaError::Other(format!(
            "snowflake worker slot release timed out after {SLOT_RELEASE_TIMEOUT_MS}ms"
        ))
    })?
    .map_err(crate::common::errors::MegaError::from)
}

async fn release_token_slots_bounded(
    connection: &mut ConnectionManager,
    slot_key_prefix: &str,
    token: &str,
) -> Result<i32, crate::common::errors::MegaError> {
    timeout(
        Duration::from_millis(SLOT_RELEASE_TIMEOUT_MS),
        release_token_slots(connection, slot_key_prefix, token),
    )
    .await
    .map_err(|_| {
        crate::common::errors::MegaError::Other(format!(
            "abandoned snowflake worker cleanup timed out after {SLOT_RELEASE_TIMEOUT_MS}ms"
        ))
    })?
    .map_err(crate::common::errors::MegaError::from)
}

async fn release_token_slots(
    connection: &mut ConnectionManager,
    slot_key_prefix: &str,
    token: &str,
) -> redis::RedisResult<i32> {
    let script = Script::new(
        r#"
            local released = 0
            for _, key in ipairs(KEYS) do
                if redis.call("GET", key) == ARGV[1] then
                    released = released + redis.call("DEL", key)
                end
            end
            return released
        "#,
    );
    let mut invocation = script.prepare_invoke();
    invocation.arg(token);
    for worker_id in 0..=MAX_WORKER_ID {
        invocation.key(format!("{slot_key_prefix}{worker_id}"));
    }
    invocation.invoke_async(connection).await
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    use redis::{AsyncCommands, aio::ConnectionManager};
    use redis_test::server::RedisServer;
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::*;

    const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

    fn redis_server_available() -> bool {
        Command::new("redis-server")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn test_connection() -> (Option<RedisServer>, ConnectionManager) {
        if redis_server_available() {
            let local_server = std::panic::catch_unwind(RedisServer::new).ok();
            if let Some(server) = local_server
                && let Ok(client) = redis::Client::open(server.client_addr().to_owned())
                && let Ok(connection) = ConnectionManager::new(client).await
            {
                return (Some(server), connection);
            }
        }

        let url =
            std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string());
        let client = redis::Client::open(url).expect("configured Redis URL");
        let connection = ConnectionManager::new(client)
            .await
            .expect("configured Redis connection");
        (None, connection)
    }

    fn test_prefix() -> String {
        format!("mega:test:snowflake:{}:", Uuid::new_v4())
    }

    async fn claim_test_worker(
        connection: &ConnectionManager,
        prefix: &str,
        identity: &str,
    ) -> Option<SnowflakeWorkerLease> {
        let cancellation = CancellationToken::new();
        claim_worker_lease(
            connection,
            prefix,
            identity,
            Uuid::new_v4().to_string(),
            None,
            None,
            &cancellation,
        )
        .await
        .expect("worker slot scan should complete")
    }

    async fn cleanup_slots(connection: &mut ConnectionManager, prefix: &str) {
        let mut pipeline = redis::pipe();
        for worker_id in 0..=MAX_WORKER_ID {
            pipeline.cmd("DEL").arg(format!("{prefix}{worker_id}"));
        }
        let _: Vec<i32> = pipeline
            .query_async(connection)
            .await
            .expect("clean up snowflake test slots");
    }

    #[tokio::test]
    async fn claims_with_set_nx_px_and_refreshes_by_token() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let lease = claim_test_worker(&connection, &prefix, "pod-a")
            .await
            .expect("first worker slot should be available");

        assert_eq!(lease.worker_id(), 0);
        let ttl: i64 = redis::cmd("PTTL")
            .arg(&lease.inner.key)
            .query_async(&mut connection)
            .await
            .expect("read worker slot TTL");
        assert!(ttl > 0 && ttl <= SLOT_TTL_MS as i64);

        assert!(
            refresh_slot(
                &mut connection,
                &lease.inner.key,
                &lease.inner.token,
                SLOT_TTL_MS
            )
            .await
            .expect("refresh the owned slot")
        );
        let owner: String = connection
            .get(&lease.inner.key)
            .await
            .expect("read worker slot owner");
        assert_eq!(owner, lease.inner.token);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn duplicate_claim_and_stale_refresh_cannot_extend_new_owner() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let first = claim_test_worker(&connection, &prefix, "pod-a")
            .await
            .expect("first worker slot should be available");
        let second_token = Uuid::new_v4().to_string();

        assert!(
            !claim_slot(
                &mut connection,
                &first.inner.key,
                &second_token,
                SLOT_TTL_MS
            )
            .await
            .expect("duplicate claim should be a successful Redis operation")
        );

        let _: String = redis::cmd("SET")
            .arg(&first.inner.key)
            .arg(&second_token)
            .arg("PX")
            .arg(200)
            .query_async(&mut connection)
            .await
            .expect("simulate a new owner");
        assert!(
            !refresh_slot(
                &mut connection,
                &first.inner.key,
                &first.inner.token,
                SLOT_TTL_MS
            )
            .await
            .expect("stale refresh should be rejected")
        );
        let owner: String = connection
            .get(&first.inner.key)
            .await
            .expect("read new owner");
        assert_eq!(owner, second_token);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn expired_and_lost_refresh_tasks_stop_without_double_renewal() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let key = format!("{prefix}0");
        let old_token = Uuid::new_v4().to_string();
        let new_token = Uuid::new_v4().to_string();

        let _: String = redis::cmd("SET")
            .arg(&key)
            .arg(&old_token)
            .arg("PX")
            .arg(40)
            .query_async(&mut connection)
            .await
            .expect("seed short-lived lease");
        sleep(Duration::from_millis(70)).await;
        assert!(
            !refresh_slot(&mut connection, &key, &old_token, 200)
                .await
                .expect("expired refresh should be rejected")
        );

        let _: String = redis::cmd("SET")
            .arg(&key)
            .arg(&old_token)
            .arg("PX")
            .arg(200)
            .query_async(&mut connection)
            .await
            .expect("seed lease for refresh task");
        let health = WorkerLeaseHealth::claimed();
        health.activate();
        let cancel = CancellationToken::new();
        let refresh_task = tokio::spawn(run_slot_refresh(SlotRefreshConfig {
            connection: connection.clone(),
            key: key.clone(),
            token: old_token,
            health: health.clone(),
            fence: None,
            cancel,
            interval_ms: 10,
            ttl_ms: 200,
        }));
        sleep(Duration::from_millis(25)).await;
        let _: String = redis::cmd("SET")
            .arg(&key)
            .arg(&new_token)
            .arg("PX")
            .arg(200)
            .query_async(&mut connection)
            .await
            .expect("replace lease owner");
        timeout(Duration::from_secs(1), refresh_task)
            .await
            .expect("stale refresh task should stop")
            .expect("stale refresh task should not panic");
        assert!(!health.is_healthy());
        let owner: String = connection.get(&key).await.expect("read replacement owner");
        assert_eq!(owner, new_token);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn full_slot_scan_falls_back_without_claiming_a_duplicate() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();

        for worker_id in 0..=MAX_WORKER_ID {
            let key = format!("{prefix}{worker_id}");
            let token = Uuid::new_v4().to_string();
            assert!(
                claim_slot(&mut connection, &key, &token, SLOT_TTL_MS)
                    .await
                    .expect("seed worker slot")
            );
        }

        assert!(
            claim_test_worker(&connection, &prefix, "overflow-pod")
                .await
                .is_none()
        );
        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn lease_shutdown_releases_slot_and_marks_health_lost() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let lease = claim_test_worker(&connection, &prefix, "shutdown-pod")
            .await
            .expect("worker slot should be available");
        let health = lease.health();
        health.activate();

        lease
            .shutdown()
            .await
            .expect("lease shutdown should release slot");
        assert!(!health.is_healthy());
        let exists: bool = connection
            .exists(&lease.inner.key)
            .await
            .expect("check released worker slot");
        assert!(!exists);
    }

    #[tokio::test]
    async fn abandoned_claim_cleanup_is_token_scoped() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let token = Uuid::new_v4().to_string();
        let other_token = Uuid::new_v4().to_string();

        let _: String = redis::cmd("SET")
            .arg(format!("{prefix}0"))
            .arg(&token)
            .arg("PX")
            .arg(SLOT_TTL_MS)
            .query_async(&mut connection)
            .await
            .expect("seed abandoned claim");
        let _: String = redis::cmd("SET")
            .arg(format!("{prefix}1"))
            .arg(&other_token)
            .arg("PX")
            .arg(SLOT_TTL_MS)
            .query_async(&mut connection)
            .await
            .expect("seed another owner");

        let released = release_token_slots_bounded(&mut connection, &prefix, &token)
            .await
            .expect("token-scoped cleanup");
        assert_eq!(released, 1);
        let abandoned_exists: bool = connection
            .exists(format!("{prefix}0"))
            .await
            .expect("check abandoned claim");
        assert!(!abandoned_exists);
        let other_exists: bool = connection
            .exists(format!("{prefix}1"))
            .await
            .expect("check another owner");
        assert!(other_exists);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn abandoned_scan_cleanup_releases_its_token_after_scan_completion() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let token = Uuid::new_v4().to_string();
        let other_token = Uuid::new_v4().to_string();

        let _: String = redis::cmd("SET")
            .arg(format!("{prefix}0"))
            .arg(&token)
            .arg("PX")
            .arg(SLOT_TTL_MS)
            .query_async(&mut connection)
            .await
            .expect("seed abandoned scan claim");
        let _: String = redis::cmd("SET")
            .arg(format!("{prefix}1"))
            .arg(&other_token)
            .arg("PX")
            .arg(SLOT_TTL_MS)
            .query_async(&mut connection)
            .await
            .expect("seed another scan owner");

        let scan = tokio::spawn(async {
            Ok::<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError>(None)
        });
        spawn_abandoned_claim_cleanup(scan, connection.clone(), prefix.clone(), token);

        timeout(Duration::from_secs(1), async {
            loop {
                let exists: bool = connection
                    .exists(format!("{prefix}0"))
                    .await
                    .expect("check abandoned scan claim");
                if !exists {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("abandoned scan cleanup should finish");
        let other_exists: bool = connection
            .exists(format!("{prefix}1"))
            .await
            .expect("check another scan owner");
        assert!(other_exists);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn postgres_worker_fence_prevents_reuse_until_shutdown() {
        let first_temp = tempfile::TempDir::new().expect("first fence temp dir");
        let second_temp = tempfile::TempDir::new().expect("second fence temp dir");
        let first = crate::jupiter::tests::test_db_connection(first_temp.path()).await;
        let second = crate::jupiter::tests::test_db_connection(second_temp.path()).await;
        let worker_id = (Uuid::new_v4().as_u128() as u32) & MAX_WORKER_ID;

        let first_fence = try_acquire_worker_fence(&first, worker_id)
            .await
            .expect("first fence query")
            .expect("first process should acquire the fence");
        first_fence
            .verify()
            .await
            .expect("first fence should be alive");
        assert!(
            try_acquire_worker_fence(&second, worker_id)
                .await
                .expect("second fence query")
                .is_none(),
            "a second process must not reuse a worker while the first fence is held"
        );

        first_fence
            .shutdown()
            .await
            .expect("first process should release the fence");
        let second_fence = try_acquire_worker_fence(&second, worker_id)
            .await
            .expect("second fence retry")
            .expect("worker should be reusable after the first fence shuts down");
        second_fence
            .shutdown()
            .await
            .expect("second process should release the fence");
    }

    #[tokio::test]
    async fn unreachable_redis_claim_is_bounded_and_returns_no_lease() {
        let client = redis::Client::open("redis://127.0.0.1:1").expect("test Redis URL");
        let connection = ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy Redis manager should be constructible");
        let started = Instant::now();

        let lease = claim_snowflake_worker(&connection, &DatabaseConnection::default())
            .await
            .expect("unreachable Redis is a bounded fallback");

        assert!(lease.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
