pub mod lock;

pub use ::redis::{AsyncCommands, aio::ConnectionManager};

use crate::{
    common::errors::MegaError,
    config::{
        RedisConfig, redaction::redact_redis_url, secret::is_secret_ref_value,
        validate::validate_redis_config,
    },
};

/// Initializes a Redis multiplexed asynchronous connection from the given configuration.
///
/// # Arguments
/// * `config` - Redis configuration including the connection URL
pub async fn init_connection(config: &RedisConfig) -> Result<ConnectionManager, MegaError> {
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
    ConnectionManager::new(client)
        .await
        .map_err(|e| MegaError::Other(format!("failed to connect to Redis at {redis_url}: {e}")))
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
