//! Configuration management for the Mono and Mega application
//! This module provides functionality to load, parse, and manage configuration settings

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub use ::config as c;
use c::{ConfigError, FileFormat, Source};
use serde::de::DeserializeOwned;

use crate::common::errors::MegaError;
pub use crate::orbit_api::factory::ObjectStorageConfig;

pub mod error;
mod expand;
pub mod loader;
mod model;
pub mod redaction;
pub mod reload;
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
/// 3. Falls back to `.mega` in the current directory if system paths are unavailable
///
/// # Returns
/// A PathBuf containing the base directory path
///
pub fn mega_base() -> PathBuf {
    resolve_mega_dir(
        std::env::var_os("MEGA_BASE_DIR"),
        directories::BaseDirs::new().map(|base_dirs| base_dirs.data_local_dir().join("mega")),
        fallback_mega_base,
    )
}

/// Retrieves the cache directory path for Mega
///
/// The directory is determined in the following priority order:
/// 1. Uses the `MEGA_CACHE_DIR` environment variable if set
/// 2. Falls back to system default paths when environment variable is not set:
///     - On Linux: `~/.cache/mega`
///     - On Windows: `C:\Users\{username}\AppData\Local\Cache\mega`
///     - On macOS: `~/Library/Caches/mega`
/// 3. Falls back to `.mega/cache` in the current directory if system paths are unavailable
///
/// # Returns
/// A PathBuf containing the cache directory path
///
pub fn mega_cache() -> PathBuf {
    resolve_mega_dir(
        std::env::var_os("MEGA_CACHE_DIR"),
        directories::BaseDirs::new().map(|base_dirs| base_dirs.cache_dir().join("mega")),
        fallback_mega_cache,
    )
}

fn resolve_mega_dir(
    env_path: Option<OsString>,
    system_path: Option<PathBuf>,
    fallback_path: impl FnOnce() -> PathBuf,
) -> PathBuf {
    if let Some(path) = env_path {
        return PathBuf::from(path);
    }

    system_path.unwrap_or_else(fallback_path)
}

fn fallback_mega_base() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".mega")
}

fn fallback_mega_cache() -> PathBuf {
    fallback_mega_base().join("cache")
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

    #[cfg(test)]
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
                local: crate::orbit_api::factory::LocalConfig {
                    root_dir: mega_base().join("objects").to_string_lossy().to_string(),
                },
                ..Default::default()
            },
            orion_server: None,
            artifacts_gc: ArtifactGcConfig::default(),
            notification: None,
            vault: None,
            oauth: None,
            git: GitConfig::default(),
            cedar: CedarConfig::default(),
        }
    }

    pub fn load_str(content: &str) -> Result<Self, ConfigError> {
        // Enforce the strict unknown-field whitelist when the content parses;
        // let the `config` crate produce the original, redacted parse error
        // when it does not.
        if let Ok(value) = toml::from_str::<toml::Value>(content) {
            validate::reject_unknown_fields(&value)
                .map_err(|e| ConfigError::Message(e.to_string()))?;
        }

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
        ConfigError::Type {
            origin: Some(origin),
            expected,
            key: Some(key),
            ..
        } => ConfigDiagnostic::SourceType {
            origin: origin.clone(),
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
    use std::path::Path;

    use super::*;
    use crate::config::{
        template::config_init_template,
        testing::{EnvVarGuard, env_lock},
    };

    fn check_file_permission(path: &Path) {
        let metadata = std::fs::metadata(path).expect("Failed to read metadata");
        assert!(
            !metadata.permissions().readonly(),
            "File should not be read-only"
        );
    }

    #[test]
    fn test_mega_base() {
        let lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let expected_base = temp_dir.path().join("base");
        let expected_base_value = expected_base.to_string_lossy().into_owned();
        let _base_guard = EnvVarGuard::set(&lock, "MEGA_BASE_DIR", &expected_base_value);

        let base_dir = mega_base();
        assert_eq!(base_dir, expected_base);
        std::fs::create_dir_all(&base_dir).expect("Failed to create base directory");
        assert!(base_dir.exists(), "Mega base directory should exist");
        check_file_permission(&base_dir);
    }

    #[test]
    fn test_mega_cache() {
        let lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let expected_cache = temp_dir.path().join("cache");
        let expected_cache_value = expected_cache.to_string_lossy().into_owned();
        let _cache_guard = EnvVarGuard::set(&lock, "MEGA_CACHE_DIR", &expected_cache_value);

        let cache_dir = mega_cache();
        assert_eq!(cache_dir, expected_cache);
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
        assert!(cache_dir.exists(), "Mega cache directory should exist");
        check_file_permission(&cache_dir);
    }

    #[test]
    fn mega_dir_resolution_prefers_env_path() {
        let resolved = resolve_mega_dir(
            Some(OsString::from("/tmp/from-env")),
            Some(PathBuf::from("/tmp/from-system")),
            || PathBuf::from("/tmp/from-fallback"),
        );

        assert_eq!(resolved, PathBuf::from("/tmp/from-env"));
    }

    #[test]
    fn mega_dir_resolution_falls_back_when_system_dirs_are_unavailable() {
        let resolved = resolve_mega_dir(None, None, || PathBuf::from("/tmp/from-fallback"));

        assert_eq!(resolved, PathBuf::from("/tmp/from-fallback"));
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
    fn test_buck_config_validate_cleanup_interval_zero() {
        let config = BuckConfig {
            cleanup_interval: 0,
            ..Default::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cleanup_interval"));
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
    fn test_load_str_and_sources_parse_list_env_overrides() {
        let lock = env_lock();
        let _root_dirs = EnvVarGuard::set(&lock, "MEGA_MONOREPO__ROOT_DIRS", "alpha,beta");
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
    fn test_load_str_allows_monorepo_object_format_environment_override() {
        let lock = env_lock();
        let _object_format = EnvVarGuard::set(&lock, "MEGA_MONOREPO__OBJECT_FORMAT", "sha256");
        let rendered = config_init_template(Path::new("/tmp/monoengine-test"));

        let config = Config::load_str(&rendered).expect("object format override should parse");
        assert_eq!(config.monorepo.object_format, MonoObjectFormat::Sha256);
    }

    #[test]
    fn test_bad_environment_type_reports_variable_name_without_value() {
        let lock = env_lock();
        let _print_std = EnvVarGuard::set(&lock, "MEGA_LOG__PRINT_STD", "not_bool_secret");
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
        let lock = env_lock();
        let _root_dirs = EnvVarGuard::set(&lock, "MEGA_MONOREPO__ROOT_DIRS", "env-alpha,env-beta");
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
    fn test_load_str_rejects_unknown_fields() {
        let content = r#"
            base_dir = "/tmp"
            unknown_root = true

            [database]
            db_type = "postgres"
            db_url = "postgres://localhost:5432/mono"
            typo = true
        "#;

        let err = Config::load_str(content).expect_err("unknown fields should fail");
        let message = err.to_string();
        assert!(message.contains("unknown_root"));
        assert!(message.contains("database.typo"));
    }

    #[test]
    fn test_load_str_rejects_removed_mail_section() {
        let content = format!("[{}]\nenabled = true\n", "mail");

        let err = Config::load_str(&content).expect_err("removed section should fail");

        assert!(err.to_string().contains("mail"));
    }

    #[test]
    fn test_profile_type_error_reports_profile_source_without_value() {
        let _lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path()))
            .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
                [log]
                print_std = "not_bool_secret"
            "#,
        )
        .expect("write profile config");

        let err = Config::new_with_profile(
            config_path.to_str().expect("utf-8 config path"),
            Some(&profile_path),
        )
        .expect_err("profile type conflict should fail");
        let message = err.to_string();

        assert!(message.contains(profile_path.to_str().expect("utf-8 profile path")));
        assert!(message.contains("log.print_std"));
        assert!(message.contains("expected a boolean"));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("remove the override"));
        assert!(!message.contains("not_bool_secret"));
    }

    #[test]
    fn test_profile_toml_parse_error_redacts_source_line() {
        let _lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path()))
            .expect("write base config");
        std::fs::write(
            &profile_path,
            format!(
                r#"
                [notification.webhook]
                {} = "{}{}
            "#,
                "password", "plain-text", "-password"
            ),
        )
        .expect("write invalid profile config");

        let err = Config::new_with_profile(
            config_path.to_str().expect("utf-8 config path"),
            Some(&profile_path),
        )
        .expect_err("bad profile TOML should fail");
        let message = err.to_string();

        assert!(message.contains("TOML parse error"));
        assert!(message.contains(profile_path.to_str().expect("utf-8 profile path")));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("config source line redacted"));
        let redacted_key_with_assignment = format!("{} =", "password");
        assert!(!message.contains("plain-text-password"));
        assert!(!message.contains(&redacted_key_with_assignment));
    }

    #[test]
    fn test_vault_bootstrap_loads_profile_database_override() {
        let lock = env_lock();
        // Isolate from any ambient MEGA_DATABASE__DB_URL (e.g. set by .env.test),
        // which would otherwise override the profile via the env source layer and
        // defeat the file/profile-precedence assertion below.
        let _db_url_guard = EnvVarGuard::remove(&lock, "MEGA_DATABASE__DB_URL");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
                [database]
                db_type = "postgres"
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
        let _lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                base_dir = "/tmp/monoengine-test"

                [database]
                db_type = "postgres"
                db_url = "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine"
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

#[cfg(test)]
mod tests;
