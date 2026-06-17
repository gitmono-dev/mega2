//! Configuration management for the Mono and Mega application
//! This module provides functionality to load, parse, and manage configuration settings

use std::path::{Path, PathBuf};

pub use ::config as c;
use c::{ConfigError, FileFormat, Source};
pub use orbit_api::factory::ObjectStorageConfig;
use serde::de::DeserializeOwned;

use crate::common::errors::MegaError;

pub mod error;
mod expand;
pub mod loader;
mod model;
pub mod redaction;
pub mod secret;
mod source;
pub mod template;
pub mod testing;
pub mod validate;

use error::ConfigDiagnostic;
use expand::variable_placeholder_substitute;
pub use model::*;
use source::{config_from_path, config_from_path_with_profile, mega_environment_source};

/// Retrieves the base directory path for Mega
///
/// The directory is determined in the following priority order:
/// 1. Uses the `MEGA_BASE_DIR` environment variable if set
/// 2. Falls back to system default paths when environment variable is not set:
///     - On Linux: `~/.local/share/mega`
///     - On Windows: `C:\Users\{UserName}\AppData\Local\mega`
///     - On macOS: `~/Library/Application Support/mega`
///
/// # Returns
/// A PathBuf containing the base directory path
///
/// # Panics
/// Will panic if both conditions occur:
/// - Environment variable is not set
/// - System base directories cannot be determined
///
pub fn mega_base() -> PathBuf {
    // Get the base directory from the environment variable or use the default
    let base_dir = std::env::var("MEGA_BASE_DIR").unwrap_or_else(|_| {
        let base_dirs = directories::BaseDirs::new().unwrap();
        base_dirs
            .data_local_dir()
            .join("mega")
            .to_str()
            .unwrap()
            .to_string()
    });

    PathBuf::from(base_dir)
}

/// Retrieves the cache directory path for Mega
///
/// The directory is determined in the following priority order:
/// 1. Uses the `MEGA_CACHE_DIR` environment variable if set
/// 2. Falls back to system default paths when environment variable is not set:
///     - On Linux: `~/.cache/mega`
///     - On Windows: `C:\Users\{username}\AppData\Local\Cache\mega`
///     - On macOS: `~/Library/Caches/mega`
///
/// # Returns
/// A PathBuf containing the cache directory path
///
/// # Panics
/// Will panic if both conditions occur:
/// - Environment variable is not set
/// - System cache directories cannot be determined
///
pub fn mega_cache() -> PathBuf {
    // Get the cache directory from the environment variable or use the default
    let cache_dir = std::env::var("MEGA_CACHE_DIR").unwrap_or_else(|_| {
        let base_dirs = directories::BaseDirs::new().unwrap();
        base_dirs
            .cache_dir()
            .join("mega")
            .to_str()
            .unwrap()
            .to_string()
    });

    PathBuf::from(cache_dir)
}

impl Config {
    pub fn new(path: &str) -> Result<Self, MegaError> {
        Ok(Config::from_config(config_from_path(path)?)?)
    }

    pub fn new_with_profile(path: &str, profile_path: Option<&Path>) -> Result<Self, MegaError> {
        Ok(Config::from_config(config_from_path_with_profile(
            path,
            profile_path,
        )?)?)
    }

    pub fn load_vault_bootstrap(path: &str) -> Result<VaultBootstrapConfig, ConfigError> {
        deserialize_with_diagnostics(config_from_path(path)?)
    }

    pub fn load_vault_bootstrap_with_profile(
        path: &str,
        profile_path: Option<&Path>,
    ) -> Result<VaultBootstrapConfig, ConfigError> {
        deserialize_with_diagnostics(config_from_path_with_profile(path, profile_path)?)
    }

    pub fn mock() -> Self {
        Self {
            base_dir: PathBuf::new(),
            log: LogConfig::default(),
            database: DbConfig::default(),
            monorepo: MonoConfig::default(),
            pack: PackConfig::default(),
            lfs: LFSConfig::default(),
            blame: BlameConfig::default(),
            build: BuildConfig::default(),
            redis: RedisConfig::default(),
            buck: None,
            object_storage: ObjectStorageConfig {
                local: orbit_api::factory::LocalConfig {
                    root_dir: mega_base().join("objects").to_string_lossy().to_string(),
                },
                ..Default::default()
            },
            orion_server: None,
            sidebar: SidebarConfig::default(),
            artifacts_gc: ArtifactGcConfig::default(),
            mail: None,
        }
    }

    pub fn load_str(content: &str) -> Result<Self, ConfigError> {
        let builder = c::Config::builder()
            .add_source(c::File::from_str(content, FileFormat::Toml))
            .add_source(mega_environment_source());

        let config = variable_placeholder_substitute(builder)?;

        Config::from_config(config)
    }

    pub fn load_sources<T>(sources: Vec<Box<T>>) -> Result<Self, ConfigError>
    where
        T: Source + Send + Sync + 'static,
    {
        let mut builder = c::Config::builder();
        for source in sources {
            builder = builder.add_source(*source);
        }
        builder = builder.add_source(mega_environment_source());

        let config = variable_placeholder_substitute(builder)?;

        Config::from_config(config)
    }

    pub fn from_config(config: c::Config) -> Result<Self, ConfigError> {
        deserialize_with_diagnostics(config)
    }
}

fn deserialize_with_diagnostics<T>(config: c::Config) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
{
    config
        .try_deserialize::<T>()
        .map_err(enrich_deserialize_error)
}

fn enrich_deserialize_error(error: ConfigError) -> ConfigError {
    match &error {
        ConfigError::Type {
            origin: Some(origin),
            expected,
            key: Some(key),
            ..
        } if origin == "the environment" => ConfigDiagnostic::EnvironmentType {
            variable: environment_variable_for_key(key),
            key: key.clone(),
            expected,
        }
        .into(),
        _ => error,
    }
}

fn environment_variable_for_key(key: &str) -> String {
    format!("MEGA_{}", key.to_ascii_uppercase().replace('.', "__"))
}

#[cfg(test)]
mod test {
    use std::{ffi::OsString, path::Path, sync::Mutex};

    use serde::Deserialize;

    use super::*;
    use crate::config::template::config_init_template;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: config tests serialize mutations of MEGA_* variables with ENV_LOCK
            // and restore the previous value when the guard is dropped.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
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

    fn check_file_permission(path: &Path) {
        let metadata = std::fs::metadata(path).expect("Failed to read metadata");
        assert!(
            !metadata.permissions().readonly(),
            "File should not be read-only"
        );
    }

    #[test]
    fn test_mega_base() {
        let base_dir = mega_base();
        std::fs::create_dir_all(&base_dir).expect("Failed to create base directory");
        assert!(base_dir.exists(), "Mega base directory should exist");
        check_file_permission(&base_dir);
    }

    #[test]
    fn test_mega_cache() {
        let cache_dir = mega_cache();
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
        assert!(cache_dir.exists(), "Mega cache directory should exist");
        check_file_permission(&cache_dir);
    }

    #[test]
    fn test_get_size_from_str() {
        use crate::config::PackConfig;

        assert_eq!(
            PackConfig::get_size_from_str("1MB", || Ok(1000 * 1000)).unwrap(),
            1000 * 1000
        );
        assert_eq!(
            PackConfig::get_size_from_str("2MiB", || Ok(2 * 1024 * 1024)).unwrap(),
            2 * 1024 * 1024
        );
        assert_eq!(
            PackConfig::get_size_from_str("20M", || Ok(0)).unwrap(),
            20 * 1024 * 1024
        );
        assert_eq!(
            PackConfig::get_size_from_str("3GB", || Ok(3 * 1000 * 1000 * 1000)).unwrap(),
            3 * 1000 * 1000 * 1000
        );
        assert_eq!(
            PackConfig::get_size_from_str("4GiB", || Ok(4 * 1024 * 1024 * 1024)).unwrap(),
            4 * 1024 * 1024 * 1024
        );
        assert_eq!(
            PackConfig::get_size_from_str("4G", || Ok(4 * 1024 * 1024 * 1024)).unwrap(),
            4 * 1024 * 1024 * 1024
        );
        assert_eq!(PackConfig::get_size_from_str("1%", || Ok(100)).unwrap(), 1);
        assert_eq!(
            PackConfig::get_size_from_str("50%", || Ok(100)).unwrap(),
            50
        );
        assert_eq!(
            PackConfig::get_size_from_str("0.01", || Ok(100)).unwrap(),
            1
        );
        assert_eq!(
            PackConfig::get_size_from_str("0.5", || Ok(100)).unwrap(),
            50
        );
        assert_eq!(
            PackConfig::get_size_from_str("1", || Ok(100)).unwrap(),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_buck_config_validate_success() {
        let config = BuckConfig::default();
        assert!(config.validate().is_ok(), "Default config should be valid");
    }

    #[test]
    fn test_buck_config_validate_upload_limit_zero() {
        let config = BuckConfig {
            upload_concurrency_limit: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("upload_concurrency_limit"));
    }

    #[test]
    fn test_buck_config_validate_large_file_limit_zero() {
        let config = BuckConfig {
            large_file_concurrency_limit: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("large_file_concurrency_limit"));
    }

    #[test]
    fn test_buck_config_validate_max_files_zero() {
        let config = BuckConfig {
            max_files: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max_files"));
    }

    #[test]
    fn test_buck_config_validate_session_timeout_zero() {
        let config = BuckConfig {
            session_timeout: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("session_timeout"));
    }

    #[test]
    fn test_buck_config_validate_valid_values() {
        let config = BuckConfig {
            upload_concurrency_limit: 100,
            large_file_concurrency_limit: 20,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_mail_config_deserial_basic() {
        #[derive(Deserialize)]
        struct Wrapper {
            mail: MailConfig,
        }

        let toml = r#"
            [mail]
            enabled = true
            smtp_host = "smtp.example.com"
            smtp_port = 587
            from = "no-reply@example.com"
            starttls = true
        "#;

        let parsed: Wrapper = toml::from_str(toml).expect("MailConfig should deserialize");
        assert!(parsed.mail.enabled);
        assert_eq!(parsed.mail.smtp_host, "smtp.example.com");
        assert_eq!(parsed.mail.smtp_port, 587);
        assert_eq!(parsed.mail.from, "no-reply@example.com");
        assert!(parsed.mail.starttls);
        assert!(parsed.mail.username.is_none());
        assert!(parsed.mail.password.is_none());
        assert!(parsed.mail.password_ref.is_none());
    }

    #[test]
    fn test_mail_config_deserial_password_ref() {
        #[derive(Deserialize)]
        struct Wrapper {
            mail: MailConfig,
        }

        let toml = r##"
            [mail]
            enabled = true
            smtp_host = "smtp.example.com"
            from = "no-reply@example.com"
            password_ref = "vault://secret/config/prod/mail/password#value"
        "##;

        let parsed: Wrapper = toml::from_str(toml).expect("MailConfig should deserialize");
        assert_eq!(
            parsed.mail.password_ref.unwrap().as_uri(),
            "vault://secret/config/prod/mail/password#value"
        );
    }

    #[test]
    fn test_load_str_and_sources_parse_list_env_overrides() {
        let _lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _root_dirs = EnvVarGuard::set("MEGA_MONOREPO__ROOT_DIRS", "alpha,beta");
        let rendered = config_init_template(Path::new("/tmp/monoengine-test"));
        let expected = vec!["alpha".to_string(), "beta".to_string()];

        let config = Config::load_str(&rendered).expect("load_str should parse list env override");
        assert_eq!(config.monorepo.root_dirs, expected);

        let config = Config::load_sources(vec![Box::new(c::File::from_str(
            &rendered,
            FileFormat::Toml,
        ))])
        .expect("load_sources should parse list env override");
        assert_eq!(config.monorepo.root_dirs, expected);
    }

    #[test]
    fn test_bad_environment_type_reports_variable_name_without_value() {
        let _lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _print_std = EnvVarGuard::set("MEGA_LOG__PRINT_STD", "not_bool_secret");
        let rendered = config_init_template(Path::new("/tmp/monoengine-test"));

        let err = Config::load_str(&rendered).expect_err("bad bool env override should fail");
        let message = err.to_string();

        assert!(message.contains("MEGA_LOG__PRINT_STD"));
        assert!(message.contains("log.print_std"));
        assert!(message.contains("expected a boolean"));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("remove the override"));
        assert!(!message.contains("not_bool_secret"));
    }

    #[test]
    fn test_new_with_profile_merges_profile_before_env() {
        let _lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _root_dirs = EnvVarGuard::set("MEGA_MONOREPO__ROOT_DIRS", "env-alpha,env-beta");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path()))
            .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
                [log]
                level = "debug"

                [monorepo]
                root_dirs = ["profile-root"]
            "#,
        )
        .expect("write profile config");

        let config = Config::new_with_profile(
            config_path.to_str().expect("utf-8 config path"),
            Some(&profile_path),
        )
        .expect("profile config should load");

        assert_eq!(config.log.level, "debug");
        assert_eq!(
            config.monorepo.root_dirs,
            vec!["env-alpha".to_string(), "env-beta".to_string()]
        );
    }

    #[test]
    fn test_vault_bootstrap_loads_profile_database_override() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
                [database]
                db_type = "postgres"
                db_path = ""
                db_url = "postgres://localhost:5432/base"
                max_connection = 4
                min_connection = 1
                acquire_timeout = 5
                connect_timeout = 5
                sqlx_logging = false
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
                [database]
                db_url = "postgres://localhost:5432/profile"
            "#,
        )
        .expect("write profile config");

        let loaded = Config::load_vault_bootstrap_with_profile(
            config_path.to_str().expect("utf-8 config path"),
            Some(&profile_path),
        )
        .expect("vault bootstrap profile should parse");

        assert_eq!(loaded.database.db_url, "postgres://localhost:5432/profile");
        assert_eq!(loaded.database.max_connection, 4);
    }

    #[test]
    fn test_vault_bootstrap_config_only_requires_database() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                base_dir = "/tmp/monoengine-test"

                [database]
                db_type = "postgres"
                db_path = ""
                db_url = "postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it"
                max_connection = 4
                min_connection = 1
                acquire_timeout = 5
                connect_timeout = 5
                sqlx_logging = false
            "#,
        )
        .expect("write config");

        let loaded = Config::load_vault_bootstrap(config_path.to_str().expect("utf-8 config path"))
            .expect("vault bootstrap config should parse without redis or object storage");

        assert_eq!(loaded.database.db_type, "postgres");
        assert_eq!(loaded.database.max_connection, 4);
    }
}
