use std::time::Duration;

use redis::{Script, aio::ConnectionManager};
use tokio::time::sleep;
use uuid::Uuid;

use crate::jupiter::utils::id_generator::{self, MAX_WORKER_ID};

const SLOT_KEY_PREFIX: &str = "monoengine:snowflake:worker:";
const SLOT_TTL_MS: u64 = 30_000;

/// Try to exclusive-claim a snowflake worker slot in Redis (`SET NX PX`).
///
/// Returns `None` if Redis errors or every slot in `0..=MAX_WORKER_ID` is taken.
/// On success, spawns a background compare-and-PEXPIRE refresh so a live
/// process keeps the slot only while its token still owns the key.
pub async fn claim_snowflake_worker(connection: &ConnectionManager) -> Option<u32> {
    claim_snowflake_worker_with_ttl(connection, SLOT_TTL_MS).await
}

async fn claim_snowflake_worker_with_ttl(
    connection: &ConnectionManager,
    ttl_ms: u64,
) -> Option<u32> {
    let identity = id_generator::process_identity();
    let token = Uuid::new_v4().to_string();
    let mut conn = connection.clone();
    for id in 0..=MAX_WORKER_ID {
        let key = format!("{SLOT_KEY_PREFIX}{id}");
        let result: Result<Option<String>, _> = redis::cmd("SET")
            .arg(&key)
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await;
        match result {
            Ok(Some(_)) => {
                tracing::info!(
                    worker_id = id,
                    slot_key = %key,
                    identity = %identity,
                    "claimed snowflake worker slot"
                );
                spawn_slot_refresh(connection.clone(), key, token, ttl_ms);
                return Some(id);
            }
            Ok(None) => continue,
            Err(_) => {
                tracing::warn!("snowflake worker slot claim failed; falling back to hash");
                return None;
            }
        }
    }
    tracing::warn!(
        max = MAX_WORKER_ID,
        "all snowflake worker slots taken; falling back to hash"
    );
    None
}

fn spawn_slot_refresh(connection: ConnectionManager, key: String, token: String, ttl_ms: u64) {
    tokio::spawn(async move {
        let half = ttl_ms / 2;
        let mut conn = connection;
        loop {
            sleep(Duration::from_millis(half)).await;
            match refresh_owned_slot(&mut conn, &key, &token, ttl_ms).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(key = %key, "snowflake worker slot refresh lost");
                    break;
                }
                Err(_) => {
                    tracing::warn!(key = %key, "snowflake worker slot refresh failed");
                    break;
                }
            }
        }
    });
}

async fn refresh_owned_slot(
    conn: &mut ConnectionManager,
    key: &str,
    token: &str,
    ttl_ms: u64,
) -> Result<bool, redis::RedisError> {
    let script = Script::new(
        r#"
        if redis.call("GET", KEYS[1]) == ARGV[1] then
            return redis.call("PEXPIRE", KEYS[1], ARGV[2])
        else
            return 0
        end
        "#,
    );
    let ok: i32 = script
        .key(key)
        .arg(token)
        .arg(ttl_ms)
        .invoke_async(conn)
        .await?;
    Ok(ok == 1)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use redis::AsyncCommands;
    use redis_test::server::RedisServer;

    use super::*;

    fn redis_server_available() -> bool {
        Command::new("redis-server")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn init_server() -> Option<(RedisServer, ConnectionManager)> {
        if !redis_server_available() {
            eprintln!("redis-server not found; skipping snowflake worker tests");
            return None;
        }
        let server = RedisServer::new();
        let url = server.client_addr().to_owned();
        let client = redis::Client::open(url).unwrap();
        let conn = ConnectionManager::new(client).await.unwrap();
        Some((server, conn))
    }

    #[tokio::test]
    async fn first_claim_wins_exclusive_slot() {
        let Some((_server, conn)) = init_server().await else {
            return;
        };
        let a = claim_snowflake_worker_with_ttl(&conn, 5_000)
            .await
            .expect("first claim");
        let b = claim_snowflake_worker_with_ttl(&conn, 5_000)
            .await
            .expect("second claim");
        assert_ne!(a, b);
        assert!(a <= MAX_WORKER_ID);
        assert!(b <= MAX_WORKER_ID);
    }

    #[tokio::test]
    async fn refresh_fails_for_stolen_token() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };
        let key = format!("{SLOT_KEY_PREFIX}steal");
        let owner = "owner-token";
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(owner)
            .arg("PX")
            .arg(5_000)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(
            refresh_owned_slot(&mut conn, &key, owner, 5_000)
                .await
                .unwrap()
        );
        let _: () = conn.set(&key, "other-token").await.unwrap();
        assert!(
            !refresh_owned_slot(&mut conn, &key, owner, 5_000)
                .await
                .unwrap(),
            "stale owner must not renew a stolen slot"
        );
    }

    #[tokio::test]
    async fn expired_slot_can_be_reclaimed() {
        let Some((_server, mut conn)) = init_server().await else {
            return;
        };
        let key = format!("{SLOT_KEY_PREFIX}0");
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("stale")
            .arg("PX")
            .arg(50)
            .query_async(&mut conn)
            .await
            .unwrap();
        sleep(Duration::from_millis(80)).await;
        let claimed = claim_snowflake_worker_with_ttl(&conn, 5_000)
            .await
            .expect("expired slot is reclaimable");
        assert_eq!(claimed, 0);
    }
}
