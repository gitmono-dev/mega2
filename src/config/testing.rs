use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig};

use super::{
    ArtifactGcConfig, BlameConfig, BuildConfig, Config, DbConfig, LFSConfig, LFSLocalConfig,
    LFSSshConfig, LogConfig, MailConfig, MonoConfig, PackConfig, RedisConfig, SidebarConfig,
    secret::{SecretRef, SecretResolver},
};
use crate::common::errors::MegaError;

pub const DEFAULT_TEST_DATABASE_URL: &str = "postgres://localhost:5432/monoengine_test";
pub const DEFAULT_TEST_REDIS_URL: &str = "redis://127.0.0.1:6379";

const ENV_DATABASE_URL: &str = "MEGA_DATABASE__DB_URL";
const ENV_REDIS_URL: &str = "MEGA_REDIS__URL";
const ENV_MAIL_PASSWORD_REF: &str = "MEGA_MAIL__PASSWORD_REF";

#[derive(Debug, Clone)]
pub struct TestConfigBuilder {
    base_dir: PathBuf,
    database_url: String,
    redis_url: String,
    mail_password_ref: Option<SecretRef>,
}

impl TestConfigBuilder {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            database_url: DEFAULT_TEST_DATABASE_URL.to_string(),
            redis_url: DEFAULT_TEST_REDIS_URL.to_string(),
            mail_password_ref: None,
        }
    }

    pub fn database_url(mut self, database_url: impl Into<String>) -> Self {
        self.database_url = database_url.into();
        self
    }

    pub fn redis_url(mut self, redis_url: impl Into<String>) -> Self {
        self.redis_url = redis_url.into();
        self
    }

    pub fn mail_password_ref(mut self, secret_ref: SecretRef) -> Self {
        self.mail_password_ref = Some(secret_ref);
        self
    }

    pub fn try_apply_env_overrides(self) -> Result<Self, MegaError> {
        self.try_apply_env_overrides_from(std::env::vars())
    }

    pub fn try_apply_env_overrides_from<I, K, V>(mut self, vars: I) -> Result<Self, MegaError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (key, value) in vars {
            match key.as_ref() {
                ENV_DATABASE_URL => self.database_url = value.as_ref().to_string(),
                ENV_REDIS_URL => self.redis_url = value.as_ref().to_string(),
                ENV_MAIL_PASSWORD_REF => {
                    self.mail_password_ref = Some(SecretRef::parse(value.as_ref())?);
                }
                _ => {}
            }
        }

        Ok(self)
    }

    pub fn build(self) -> Config {
        let base_dir = self.base_dir;
        let cache_dir = base_dir.join("cache");
        let object_root = base_dir.join("objects");

        Config {
            base_dir: base_dir.clone(),
            log: LogConfig::default(),
            database: DbConfig {
                db_type: "postgres".to_string(),
                db_path: PathBuf::new(),
                db_url: self.database_url,
                max_connection: 4,
                min_connection: 1,
                acquire_timeout: 5,
                connect_timeout: 5,
                sqlx_logging: false,
            },
            monorepo: MonoConfig::default(),
            pack: PackConfig {
                pack_decode_mem_size: "4G".to_string(),
                pack_decode_disk_size: "20%".to_string(),
                pack_decode_cache_path: cache_dir.join("pack_decode"),
                clean_cache_after_decode: true,
                channel_message_size: 1_000_000,
                save_entry_concurrency: 1,
            },
            lfs: LFSConfig {
                local: LFSLocalConfig {
                    lfs_file_path: base_dir.join("lfs"),
                },
                ssh: LFSSshConfig::default(),
            },
            blame: BlameConfig::default(),
            build: BuildConfig::default(),
            redis: RedisConfig {
                url: self.redis_url,
            },
            buck: None,
            object_storage: ObjectStorageConfig {
                storage_type: ObjectStorageBackend::Local,
                local: LocalConfig {
                    root_dir: object_root.to_string_lossy().to_string(),
                },
                ..Default::default()
            },
            orion_server: None,
            sidebar: SidebarConfig::default(),
            artifacts_gc: ArtifactGcConfig::default(),
            mail: self.mail_password_ref.map(|password_ref| MailConfig {
                enabled: false,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                username: Some("monoengine@example.com".to_string()),
                password: None,
                password_ref: Some(password_ref),
                from: "no-reply@example.com".to_string(),
                starttls: true,
            }),
        }
    }
}

pub fn isolated_config(base_dir: impl AsRef<Path>) -> Config {
    TestConfigBuilder::new(base_dir.as_ref()).build()
}

#[derive(Debug, Clone, Default)]
pub struct TestSecretResolver {
    secrets: Arc<RwLock<HashMap<String, String>>>,
}

impl TestSecretResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_secret(
        self,
        secret_ref: &SecretRef,
        value: impl Into<String>,
    ) -> Result<Self, MegaError> {
        self.insert_secret(secret_ref, value)?;
        Ok(self)
    }

    pub fn insert_secret(
        &self,
        secret_ref: &SecretRef,
        value: impl Into<String>,
    ) -> Result<(), MegaError> {
        let mut secrets = self
            .secrets
            .write()
            .map_err(|_| MegaError::Other("test secret resolver lock was poisoned".to_string()))?;
        secrets.insert(secret_ref.as_uri().to_string(), value.into());
        Ok(())
    }
}

#[async_trait]
impl SecretResolver for TestSecretResolver {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<String, MegaError> {
        let secrets = self
            .secrets
            .read()
            .map_err(|_| MegaError::Other("test secret resolver lock was poisoned".to_string()))?;

        secrets
            .get(secret_ref.as_uri())
            .cloned()
            .ok_or_else(|| MegaError::Other(format!("test secret not found: {secret_ref}")))
    }

    async fn evict(&self, secret_ref: &SecretRef) {
        if let Ok(mut secrets) = self.secrets.write() {
            secrets.remove(secret_ref.as_uri());
        }
    }

    async fn evict_all(&self) {
        if let Ok(mut secrets) = self.secrets.write() {
            secrets.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_builder_uses_isolated_paths_and_validates() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let base_dir = temp_dir.path().join("base");

        let config = TestConfigBuilder::new(&base_dir).build();

        assert_eq!(config.base_dir, base_dir);
        assert_eq!(
            config.pack.pack_decode_cache_path,
            base_dir.join("cache/pack_decode")
        );
        assert_eq!(config.lfs.local.lfs_file_path, base_dir.join("lfs"));
        assert_eq!(
            config.object_storage.local.root_dir,
            base_dir.join("objects").to_string_lossy()
        );
        config.validate().expect("test config should validate");
    }

    #[test]
    fn test_config_builder_applies_env_style_overrides() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let secret_ref = "vault://secret/config/test/mail/password#value";

        let config = TestConfigBuilder::new(temp_dir.path())
            .try_apply_env_overrides_from([
                (ENV_DATABASE_URL, "postgres://127.0.0.1:15432/test_config"),
                (ENV_REDIS_URL, "redis://127.0.0.1:16379"),
                (ENV_MAIL_PASSWORD_REF, secret_ref),
            ])
            .expect("env overrides should parse")
            .build();

        assert_eq!(
            config.database.db_url,
            "postgres://127.0.0.1:15432/test_config"
        );
        assert_eq!(config.redis.url, "redis://127.0.0.1:16379");
        assert_eq!(
            config
                .mail
                .as_ref()
                .and_then(|mail| mail.password_ref.as_ref())
                .map(SecretRef::as_uri),
            Some(secret_ref)
        );
        config
            .validate()
            .expect("overridden config should validate");
    }

    #[tokio::test]
    async fn test_secret_resolver_reads_and_evicts_values() {
        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let resolver = TestSecretResolver::new()
            .with_secret(&secret_ref, "smtp-test-value")
            .expect("secret should insert");

        assert_eq!(
            resolver.resolve(&secret_ref).await.unwrap(),
            "smtp-test-value"
        );

        resolver.evict(&secret_ref).await;
        assert!(resolver.resolve(&secret_ref).await.is_err());

        resolver
            .insert_secret(&secret_ref, "smtp-test-value")
            .expect("secret should insert again");
        resolver.evict_all().await;
        assert!(resolver.resolve(&secret_ref).await.is_err());
    }
}
