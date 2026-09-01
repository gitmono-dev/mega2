use std::time::Duration;

use redis::{Script, aio::ConnectionManager};
use tokio::time::sleep;
use uuid::Uuid;

use crate::jupiter::utils::id_generator::{self, MAX_WORKER_ID};

const SLOT_KEY_PREFIX: &str = "mega:snowflake:worker:";
const SLOT_TTL_MS: u64 = 30_000;
const SLOT_REFRESH_INTERVAL_MS: u64 = SLOT_TTL_MS / 2;

#[derive(Debug, Clone)]
struct WorkerLease {
    worker_id: u32,
    key: String,
    token: String,
}

/// Try to exclusively claim a Snowflake worker slot in Redis (SET NX PX).
///
/// Returns None if Redis errors or every slot in 0..=MAX_WORKER_ID is taken.
/// On success, a background task refreshes the lease while the process owns
/// it. The refresh compares the unique token before extending the TTL, so a
/// stale process cannot renew a slot after it changes hands.
pub async fn claim_snowflake_worker(connection: &ConnectionManager) -> Option<u32> {
    let identity = id_generator::process_identity();
    claim_worker_lease(connection, SLOT_KEY_PREFIX, &identity, true)
        .await
        .map(|lease| lease.worker_id)
}

async fn claim_worker_lease(
    connection: &ConnectionManager,
    slot_key_prefix: &str,
    identity: &str,
    spawn_refresh: bool,
) -> Option<WorkerLease> {
    let token = Uuid::new_v4().to_string();
    let mut conn = connection.clone();

    for worker_id in 0..=MAX_WORKER_ID {
        let key = format!("{slot_key_prefix}{worker_id}");
        match claim_slot(&mut conn, &key, &token, SLOT_TTL_MS).await {
            Ok(true) => {
                let lease = WorkerLease {
                    worker_id,
                    key,
                    token,
                };
                tracing::info!(
                    worker_id,
                    slot_key = %lease.key,
                    process_identity = %id_generator::identity_digest(identity),
                    "claimed snowflake worker slot"
                );
                if spawn_refresh {
                    spawn_slot_refresh(connection.clone(), lease.clone());
                }
                return Some(lease);
            }
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot claim failed; falling back to hash"
                );
                return None;
            }
        }
    }

    tracing::warn!(
        max_worker_id = MAX_WORKER_ID,
        process_identity = %id_generator::identity_digest(identity),
        "all snowflake worker slots are taken; falling back to hash"
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

fn spawn_slot_refresh(connection: ConnectionManager, lease: WorkerLease) {
    tokio::spawn(run_slot_refresh(
        connection,
        lease.key,
        lease.token,
        SLOT_REFRESH_INTERVAL_MS,
        SLOT_TTL_MS,
    ));
}

async fn run_slot_refresh(
    connection: ConnectionManager,
    key: String,
    token: String,
    interval_ms: u64,
    ttl_ms: u64,
) {
    let mut conn = connection;
    loop {
        sleep(Duration::from_millis(interval_ms)).await;
        match refresh_slot(&mut conn, &key, &token, ttl_ms).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    slot_key = %key,
                    "snowflake worker slot refresh lost ownership"
                );
                break;
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    slot_key = %key,
                    "snowflake worker slot refresh failed"
                );
                break;
            }
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

#[cfg(test)]
mod tests {
    use std::{process::Command, time::Duration};

    use redis::{AsyncCommands, aio::ConnectionManager};
    use redis_test::server::RedisServer;
    use tokio::time::{sleep, timeout};
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
        let lease = claim_worker_lease(&connection, &prefix, "pod-a", false)
            .await
            .expect("first worker slot should be available");

        assert_eq!(lease.worker_id, 0);
        let ttl: i64 = redis::cmd("PTTL")
            .arg(&lease.key)
            .query_async(&mut connection)
            .await
            .expect("read worker slot TTL");
        assert!(ttl > 0 && ttl <= SLOT_TTL_MS as i64);

        assert!(
            refresh_slot(&mut connection, &lease.key, &lease.token, SLOT_TTL_MS)
                .await
                .expect("refresh the owned slot")
        );
        let owner: String = connection
            .get(&lease.key)
            .await
            .expect("read worker slot owner");
        assert_eq!(owner, lease.token);

        cleanup_slots(&mut connection, &prefix).await;
    }

    #[tokio::test]
    async fn duplicate_claim_and_stale_refresh_cannot_extend_new_owner() {
        let (_server, mut connection) = test_connection().await;
        let prefix = test_prefix();
        let first = claim_worker_lease(&connection, &prefix, "pod-a", false)
            .await
            .expect("first worker slot should be available");
        let second_token = Uuid::new_v4().to_string();

        assert!(
            !claim_slot(&mut connection, &first.key, &second_token, SLOT_TTL_MS)
                .await
                .expect("duplicate claim should be a successful Redis operation")
        );

        let _: String = redis::cmd("SET")
            .arg(&first.key)
            .arg(&second_token)
            .arg("PX")
            .arg(200)
            .query_async(&mut connection)
            .await
            .expect("simulate a new owner");
        assert!(
            !refresh_slot(&mut connection, &first.key, &first.token, SLOT_TTL_MS)
                .await
                .expect("stale refresh should be rejected")
        );
        let owner: String = connection.get(&first.key).await.expect("read new owner");
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
        let refresh_task = tokio::spawn(run_slot_refresh(
            connection.clone(),
            key.clone(),
            old_token,
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
            claim_worker_lease(&connection, &prefix, "overflow-pod", false)
                .await
                .is_none()
        );
        cleanup_slots(&mut connection, &prefix).await;
    }
}
