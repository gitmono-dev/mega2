use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
#[cfg(test)]
use std::{
    ffi::OsString,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;

use super::{
    AgentCaptureConfig, ArtifactGcConfig, BlameConfig, CedarConfig, Config, DbConfig, GitConfig,
    LFSConfig, LFSLocalConfig, LFSSshConfig, LogConfig, MonoConfig, OciConfig, PackConfig,
    PushPolicy, RedisConfig, StorageEventsConfig, ViewsConfig,
    secret::{SecretRef, SecretResolver},
};
use crate::common::errors::MegaError;
#[rustfmt::skip]
use crate::orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig};

pub const DEFAULT_TEST_DATABASE_URL: &str = "postgres://localhost:5432/mega2_test";
pub const DEFAULT_TEST_REDIS_URL: &str = "redis://127.0.0.1:6379";

const ENV_DATABASE_URL: &str = "MEGA_DATABASE__DB_URL";
const ENV_REDIS_URL: &str = "MEGA_REDIS__URL";

#[cfg(test)]
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub type EnvLockGuard = MutexGuard<'static, ()>;

#[cfg(test)]
pub fn env_lock() -> EnvLockGuard {
    // Recover from a poisoned lock instead of cascading a single test's panic
    // into PoisonError failures across every other env-serialized test. The
    // lock only serializes participating callers; it does not protect unrelated
    // environment readers. Proxy tests use child startup environments instead.
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub struct EnvVarGuard<'a> {
    _lock: &'a EnvLockGuard,
    key: &'static str,
    previous: Option<OsString>,
}

#[cfg(test)]
impl<'a> EnvVarGuard<'a> {
    pub fn set(lock: &'a EnvLockGuard, key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: callers must exclude concurrent environment readers as well as
        // writers. This lock alone only serializes participating tests.
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            _lock: lock,
            key,
            previous,
        }
    }

    /// Temporarily remove an environment variable for the duration of the guard,
    /// restoring its previous value on drop. Used by tests that assert
    /// file/profile precedence in isolation from any ambient `MEGA_*` override
    /// (e.g. the `.env.test` `MEGA_DATABASE__DB_URL` that otherwise wins via the
    /// env source layer).
    pub fn remove(lock: &'a EnvLockGuard, key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: see EnvVarGuard::set; tests hold the shared config env lock and
        // this guard restores the previous value before the lock is released.
        unsafe {
            std::env::remove_var(key);
        }
        Self {
            _lock: lock,
            key,
            previous,
        }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set; this restores the serialized test mutation.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

#[cfg(test)]
pub fn proxy_test_child_phase(test_name: &str) -> Option<String> {
    (std::env::var("MEGA2_PROXY_TEST_CHILD").as_deref() == Ok(test_name))
        .then(|| std::env::var("MEGA2_PROXY_TEST_PHASE").expect("child phase"))
}

#[cfg(test)]
pub fn run_proxy_test_child(test_name: &str, phase: &str, values: &[(&str, &str)]) {
    use std::{
        io::Read,
        process::{Child, Command, Stdio},
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::{Duration, Instant},
    };

    const PROXY_KEYS: [&str; 8] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    const OUTPUT_LIMIT: u64 = 1024 * 1024;
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let before = PROXY_KEYS.map(std::env::var_os);
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "--exact",
            test_name,
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .env("MEGA2_PROXY_TEST_CHILD", test_name)
        .env("MEGA2_PROXY_TEST_PHASE", phase)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in PROXY_KEYS {
        command.env_remove(key);
    }
    for (key, value) in values {
        command.env(key, value);
    }
    let mut child = ChildGuard(command.spawn().expect("spawn isolated proxy test"));
    let stdout = child.0.stdout.take().expect("child stdout");
    let stderr = child.0.stderr.take().expect("child stderr");
    let too_large = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(20);
    let read = |stream: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        stream
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .expect("read child output");
        if bytes.len() as u64 > OUTPUT_LIMIT {
            too_large.store(true, Ordering::Release);
        }
        bytes
    };
    let (status, stdout, stderr) = thread::scope(|scope| {
        let stdout = scope.spawn(|| read(Box::new(stdout)));
        let stderr = scope.spawn(|| read(Box::new(stderr)));
        let result = loop {
            if too_large.load(Ordering::Acquire) {
                break Err("isolated proxy test output limit exceeded");
            }
            if Instant::now() >= deadline {
                break Err("isolated proxy test exceeded 20 seconds");
            }
            if !PROXY_KEYS
                .iter()
                .zip(&before)
                .all(|(key, value)| std::env::var_os(key) == *value)
            {
                break Err("isolated proxy test changed parent proxy environment");
            }
            match child.0.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break Err("could not wait for isolated proxy test"),
            }
        };
        // Close pipes before joining readers, including every failure path.
        if result.is_err() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        (
            result,
            stdout.join().expect("stdout reader"),
            stderr.join().expect("stderr reader"),
        )
    });
    assert!(
        stdout.len() as u64 <= OUTPUT_LIMIT && stderr.len() as u64 <= OUTPUT_LIMIT,
        "child output limit"
    );
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        status.expect("isolated proxy test completion").success(),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| *line == format!("test {test_name} ... ok"))
            .count(),
        1,
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "{stdout}"
    );
    assert!(
        PROXY_KEYS
            .iter()
            .zip(&before)
            .all(|(key, value)| std::env::var_os(key) == *value),
        "parent proxy environment changed"
    );
}

#[derive(Debug, Clone)]
pub struct TestConfigBuilder {
    base_dir: PathBuf,
    database_url: String,
    redis_url: String,
}

impl TestConfigBuilder {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            database_url: DEFAULT_TEST_DATABASE_URL.to_string(),
            redis_url: DEFAULT_TEST_REDIS_URL.to_string(),
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
                db_url: self.database_url,
                max_connection: 4,
                min_connection: 1,
                // Same reasoning as `jupiter::tests::test_db_config` (FIX-05):
                // long enough that only a real problem reaches the timeout,
                // rather than a busy machine.
                acquire_timeout: 60,
                connect_timeout: 30,
                sqlx_logging: false,
            },
            // Fixtures built on this config exercise the review morphology
            // (CL pipeline, OAuth push chain); trunk cases opt in explicitly.
            monorepo: MonoConfig {
                push_policy: PushPolicy::Review,
                ..MonoConfig::default()
            },
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
            mst2: crate::config::Mst2Config::default(),
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
            artifacts_gc: ArtifactGcConfig::default(),
            views: ViewsConfig::default(),
            notification: None,
            vault: None,
            oauth: None,
            git: GitConfig::default(),
            oci: OciConfig::default(),
            agent_capture: AgentCaptureConfig::default(),
            storage_events: StorageEventsConfig::default(),
            github_sync: crate::config::GithubSyncConfig::default(),
            cedar: CedarConfig::default(),
            traces: crate::config::TracesConfig::default(),
        }
    }
}

pub fn isolated_config(base_dir: impl AsRef<Path>) -> Config {
    TestConfigBuilder::new(base_dir.as_ref()).build()
}

#[derive(Debug, Clone, Default)]
pub struct TestSecretResolver {
    secrets: Arc<RwLock<HashMap<String, String>>>,
    denied_refs: Arc<RwLock<HashSet<String>>>,
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

    pub fn with_denied_secret(self, secret_ref: &SecretRef) -> Result<Self, MegaError> {
        self.deny_secret(secret_ref)?;
        Ok(self)
    }

    pub fn deny_secret(&self, secret_ref: &SecretRef) -> Result<(), MegaError> {
        let mut denied_refs = self
            .denied_refs
            .write()
            .map_err(|_| MegaError::Other("test secret resolver lock was poisoned".to_string()))?;
        denied_refs.insert(secret_ref.as_uri().to_string());
        Ok(())
    }
}

#[async_trait]
impl SecretResolver for TestSecretResolver {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<String, MegaError> {
        let denied_refs = self
            .denied_refs
            .read()
            .map_err(|_| MegaError::Other("test secret resolver lock was poisoned".to_string()))?;
        if denied_refs.contains(secret_ref.as_uri()) {
            return Err(MegaError::Other(format!(
                "test secret access denied for {secret_ref}"
            )));
        }
        drop(denied_refs);

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
        if let Ok(mut denied_refs) = self.denied_refs.write() {
            denied_refs.remove(secret_ref.as_uri());
        }
    }

    async fn evict_all(&self) {
        if let Ok(mut secrets) = self.secrets.write() {
            secrets.clear();
        }
        if let Ok(mut denied_refs) = self.denied_refs.write() {
            denied_refs.clear();
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

    #[tokio::test]
    async fn test_secret_resolver_denies_configured_refs_without_leaking_uri() {
        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let resolver = TestSecretResolver::new()
            .with_secret(&secret_ref, "smtp-test-value")
            .expect("secret should insert")
            .with_denied_secret(&secret_ref)
            .expect("secret should be denied");

        let err = resolver
            .resolve(&secret_ref)
            .await
            .expect_err("denied secret should fail");
        let message = err.to_string();

        assert!(message.contains("test secret access denied"));
        assert!(message.contains("vault://secret/***#***"));
        assert!(!message.contains("config/test/mail/password"));
        assert!(!message.contains("#value"));
        assert!(!message.contains("smtp-test-value"));

        resolver.evict(&secret_ref).await;
        assert!(resolver.resolve(&secret_ref).await.is_err());
    }
}
