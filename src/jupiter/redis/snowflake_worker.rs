use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use redis::{Script, aio::ConnectionManager};
use tokio::{task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::jupiter::utils::id_generator::{self, MAX_WORKER_ID, WorkerLeaseHealth};

const SLOT_KEY_PREFIX: &str = "mega:snowflake:worker:";
const SLOT_TTL_MS: u64 = 30_000;
const SLOT_REFRESH_INTERVAL_MS: u64 = SLOT_TTL_MS / 2;
const SLOT_REFRESH_TIMEOUT_MS: u64 = 5_000;
const SLOT_RELEASE_TIMEOUT_MS: u64 = 5_000;
const SLOT_SCAN_TIMEOUT_MS: u64 = 2_000;

struct WorkerLeaseInner {
    worker_id: u32,
    key: String,
    token: String,
    connection: ConnectionManager,
    health: Arc<WorkerLeaseHealth>,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
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
    ) -> Self {
        Self {
            inner: Arc::new(WorkerLeaseInner {
                worker_id,
                key,
                token,
                connection,
                health,
                cancel: CancellationToken::new(),
                task: Mutex::new(None),
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
        *task = Some(tokio::spawn(run_slot_refresh(
            inner.connection.clone(),
            inner.key.clone(),
            inner.token.clone(),
            Arc::clone(&inner.health),
            inner.cancel.clone(),
            SLOT_REFRESH_INTERVAL_MS,
            SLOT_TTL_MS,
        )));
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
        let task_error = if let Some(task) = task {
            task.await
                .err()
                .map(|error| crate::common::errors::MegaError::Other(error.to_string()))
        } else {
            None
        };

        let mut connection = self.inner.connection.clone();
        let release_result =
            release_slot_bounded(&mut connection, &self.inner.key, &self.inner.token)
                .await
                .map(|_| ());

        if let Some(error) = task_error {
            return Err(error);
        }
        release_result
    }
}

impl Drop for WorkerLeaseInner {
    fn drop(&mut self) {
        id_generator::revoke_worker_lease(&self.health);
        self.cancel.cancel();
    }
}

/// Try to exclusively claim a Snowflake worker slot in Redis (SET NX PX).
///
/// Returns `Ok(None)` if Redis is unavailable, the bounded scan times out, or
/// every slot in 0..=MAX_WORKER_ID is taken. The caller may record a stable
/// identity hash for diagnostics, but must not generate IDs from that hash in
/// a multi-writer process. On success, the caller must bind the returned guard
/// to the ID generator before starting its refresh task.
pub async fn claim_snowflake_worker(
    connection: &ConnectionManager,
) -> Result<Option<SnowflakeWorkerLease>, crate::common::errors::MegaError> {
    let identity = id_generator::process_identity();
    let claim = timeout(
        Duration::from_millis(SLOT_SCAN_TIMEOUT_MS),
        claim_worker_lease(connection, SLOT_KEY_PREFIX, &identity),
    )
    .await;

    match claim {
        Ok(lease) => Ok(lease),
        Err(_) => {
            tracing::warn!(
                timeout_ms = SLOT_SCAN_TIMEOUT_MS,
                process_identity = %id_generator::identity_digest(&identity),
                "snowflake worker slot scan timed out; ID writes require explicit exclusive worker configuration"
            );
            Ok(None)
        }
    }
}

async fn claim_worker_lease(
    connection: &ConnectionManager,
    slot_key_prefix: &str,
    identity: &str,
) -> Option<SnowflakeWorkerLease> {
    let token = Uuid::new_v4().to_string();
    let mut conn = connection.clone();

    for worker_id in 0..=MAX_WORKER_ID {
        let key = format!("{slot_key_prefix}{worker_id}");
        match claim_slot(&mut conn, &key, &token, SLOT_TTL_MS).await {
            Ok(true) => {
                let health = WorkerLeaseHealth::claimed();
                let lease =
                    SnowflakeWorkerLease::new(worker_id, key, token, connection.clone(), health);
                tracing::info!(
                    worker_id,
                    slot_key = %lease.inner.key,
                    process_identity = %id_generator::identity_digest(identity),
                    "claimed snowflake worker slot"
                );
                return Some(lease);
            }
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot claim failed; ID writes require explicit exclusive worker configuration"
                );
                return None;
            }
        }
    }

    tracing::warn!(
        max_worker_id = MAX_WORKER_ID,
        process_identity = %id_generator::identity_digest(identity),
        "all snowflake worker slots are taken; ID writes require explicit exclusive worker configuration"
    );
    None
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

async fn run_slot_refresh(
    connection: ConnectionManager,
    key: String,
    token: String,
    health: Arc<WorkerLeaseHealth>,
    cancel: CancellationToken,
    interval_ms: u64,
    ttl_ms: u64,
) {
    let mut conn = connection;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                stop_refresh(&mut conn, &key, &token, &health).await;
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
        }

        if cancel.is_cancelled() {
            continue;
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
                    stop_refresh(&mut conn, &key, &token, &health).await;
                    break;
                }
            }
            Ok(Ok(false)) => {
                tracing::warn!(
                    slot_key = %key,
                    "snowflake worker slot refresh lost ownership"
                );
                stop_refresh(&mut conn, &key, &token, &health).await;
                break;
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot refresh failed"
                );
                stop_refresh(&mut conn, &key, &token, &health).await;
                break;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = SLOT_REFRESH_TIMEOUT_MS,
                    slot_key = %key,
                    "snowflake worker slot refresh timed out"
                );
                stop_refresh(&mut conn, &key, &token, &health).await;
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
) {
    id_generator::revoke_worker_lease(health);
    match release_slot_bounded(connection, key, token).await {
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(error = %error, slot_key = %key, "failed to release snowflake worker slot");
        }
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
            let server = RedisServer::new();
            let client =
                redis::Client::open(server.client_addr().to_owned()).expect("redis test URL");
            let connection = ConnectionManager::new(client)
                .await
                .expect("redis test connection");
            return (Some(server), connection);
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
        let lease = claim_worker_lease(&connection, &prefix, "pod-a")
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
        let first = claim_worker_lease(&connection, &prefix, "pod-a")
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
        let refresh_task = tokio::spawn(run_slot_refresh(
            connection.clone(),
            key.clone(),
            old_token,
            health.clone(),
            cancel,
            10,
            200,
        ));
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
            claim_worker_lease(&connection, &prefix, "overflow-pod")
                .await
                .is_none()
        );
        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn lease_shutdown_releases_slot_and_marks_health_lost() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let lease = claim_worker_lease(&connection, &prefix, "shutdown-pod")
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
    async fn unreachable_redis_claim_is_bounded_and_returns_no_lease() {
        let client = redis::Client::open("redis://127.0.0.1:1").expect("test Redis URL");
        let connection = ConnectionManager::new_lazy_with_config(
            client,
            redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy Redis manager should be constructible");
        let started = Instant::now();

        let lease = claim_snowflake_worker(&connection)
            .await
            .expect("unreachable Redis is a bounded fallback");

        assert!(lease.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
