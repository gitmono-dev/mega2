pub mod lock;
pub mod snowflake_worker;

pub use ::redis::{AsyncCommands, aio::ConnectionManager};
pub use snowflake_worker::claim_snowflake_worker;

use crate::{
    common::errors::MegaError,
    config::{
        RedisConfig, redaction::redact_redis_url, secret::is_secret_ref_value,
        validate::validate_redis_config,
    },
    jupiter::utils::id_generator,
};

/// How many times [`init_connection`] has been called, per URL.
///
/// UN-43 needs to assert that the read-only assembly does not initialize Redis
/// *at all*. Inferring it from "the assembly survived an unreachable address"
/// cannot tell a path that never connected from one that connected, swallowed
/// the failure and carried on — and the second is still a side effect.
///
/// Keyed by URL rather than a single counter because the test binary runs its
/// tests in parallel: a global count would be perturbed by whatever else
/// happens to initialize Redis at the same moment, and a test that only passes
/// when nothing else is running is worse than no test.
#[cfg(test)]
static INIT_CALLS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) fn init_connection_calls_for(url: &str) -> usize {
    INIT_CALLS
        .lock()
        .map(|calls| calls.get(url).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Initializes a Redis multiplexed asynchronous connection from the given configuration.
///
/// # Arguments
/// * `config` - Redis configuration including the connection URL
pub async fn init_connection(config: &RedisConfig) -> Result<ConnectionManager, MegaError> {
    #[cfg(test)]
    if let Ok(mut calls) = INIT_CALLS.lock() {
        *calls.entry(config.url.clone()).or_insert(0) += 1;
    }

    validate_redis_config(config)?;

    if is_secret_ref_value(config.url.trim_start()) {
        return Err(MegaError::Other(
            "redis.url contains an unresolved vault:// SecretRef; it must be resolved before initializing the Redis connection".to_string(),
        ));
    }

    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls crypto provider was already installed before Redis initialization");
    }

    let redis_url = redact_redis_url(&config.url);
    let client = ::redis::Client::open(config.url.as_str())
        .map_err(|e| MegaError::Other(format!("failed to open Redis URL {redis_url}: {e}")))?;
    let conn = ConnectionManager::new(client)
        .await
        .map_err(|e| MegaError::Other(format!("failed to connect to Redis at {redis_url}: {e}")))?;

    if id_generator::resolve_worker_id_from(
        std::env::var(id_generator::ENV_WORKER_ID).ok().as_deref(),
        None,
        "",
    )
    .1 != id_generator::WorkerIdSource::Env
        && let Some(id) = claim_snowflake_worker(&conn).await
    {
        let _ = id_generator::claim_worker_id(id);
    }
    id_generator::ensure_initialized();

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_connection_returns_error_for_invalid_config() {
        let config = RedisConfig {
            url: "http://localhost:6379".to_string(),
        };

        let err = init_connection(&config)
            .await
            .expect_err("invalid Redis URL scheme should fail before connecting");

        assert!(err.to_string().contains("redis.url scheme"));
    }

    #[tokio::test]
    async fn init_connection_rejects_unresolved_secret_ref_without_leaking_path() {
        let config = RedisConfig {
            url: "vault://secret/config/test/redis/url#value".to_string(),
        };

        let err = init_connection(&config)
            .await
            .expect_err("unresolved redis.url SecretRef should fail before opening client");
        let message = err.to_string();

        assert!(message.contains("unresolved vault:// SecretRef"));
        assert!(!message.contains("config/test/redis/url"));
        assert!(!message.contains("#value"));
    }
}
