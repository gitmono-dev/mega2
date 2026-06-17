use std::{ffi::OsStr, path::Path};

use orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig};
use toml::Value;
use url::Url;

use super::{
    BuckConfig, BuildConfig, Config, DbConfig, LFSConfig, LogConfig, MailConfig, OrionServerConfig,
    RedisConfig,
};
use crate::common::errors::MegaError;

const MEGA_ENV_PREFIX: &str = "MEGA_";
const RESERVED_MEGA_ENV_VARS: &[&str] = &[
    "MEGA_CONFIG",
    "MEGA_PROFILE",
    "MEGA_BASE_DIR",
    "MEGA_CACHE_DIR",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub field_path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentConfigWarning {
    pub variable: String,
    pub field_path: String,
    pub message: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), MegaError> {
        validate_log_config(&self.log)?;
        validate_database_config(&self.database)?;
        validate_lfs_config(&self.lfs)?;
        validate_build_config(&self.build)?;
        validate_redis_config(&self.redis)?;
        if let Some(mail_config) = &self.mail {
            mail_config.validate()?;
        }
        if let Some(buck_config) = &self.buck {
            validate_buck_config(buck_config)?;
        }
        validate_object_storage_config(&self.object_storage)?;
        if let Some(orion_server_config) = &self.orion_server {
            validate_orion_server_config(orion_server_config)?;
        }

        Ok(())
    }
}

impl MailConfig {
    pub fn warn_plaintext_password_deprecated(&self) {
        if self.password.is_some() {
            tracing::warn!(
                field = "mail.password",
                "mail.password is deprecated; use mail.password_ref for vault-backed SMTP credentials"
            );
        }
    }

    pub fn validate(&self) -> Result<(), MegaError> {
        self.validate_secret_fields()?;

        if self.enabled {
            if self.smtp_host.trim().is_empty() {
                return Err(MegaError::Other(
                    "mail.smtp_host is required when mail.enabled is true".to_string(),
                ));
            }
            if self.from.trim().is_empty() {
                return Err(MegaError::Other(
                    "mail.from is required when mail.enabled is true".to_string(),
                ));
            }
        }

        Ok(())
    }
}

pub(crate) fn validate_log_config(log_config: &LogConfig) -> Result<(), MegaError> {
    match log_config.level.to_ascii_lowercase().as_str() {
        "trace" | "debug" | "info" | "warn" | "error" => Ok(()),
        level => Err(MegaError::Other(format!(
            "log.level must be one of trace, debug, info, warn, error; got '{level}'"
        ))),
    }
}

pub(crate) fn validate_database_config(db_config: &DbConfig) -> Result<(), MegaError> {
    if db_config.db_type != "postgres" {
        return Err(MegaError::Other(format!(
            "database.db_type must be 'postgres', got '{}'",
            db_config.db_type
        )));
    }

    let url = Url::parse(&db_config.db_url)
        .map_err(|e| MegaError::Other(format!("database.db_url must be a valid URL: {e}")))?;
    match url.scheme() {
        "postgres" | "postgresql" => Ok(()),
        scheme => Err(MegaError::Other(format!(
            "database.db_url scheme must be 'postgres' or 'postgresql', got '{scheme}'"
        ))),
    }
}

pub(crate) fn validate_lfs_config(lfs_config: &LFSConfig) -> Result<(), MegaError> {
    require_non_empty_path("lfs.local.lfs_file_path", &lfs_config.local.lfs_file_path)?;
    require_non_empty("lfs.ssh.http_url", &lfs_config.ssh.http_url)?;
    validate_http_url("lfs.ssh.http_url", &lfs_config.ssh.http_url)
}

pub(crate) fn validate_build_config(build_config: &BuildConfig) -> Result<(), MegaError> {
    if build_config.enable_build {
        require_non_empty("build.orion_server", &build_config.orion_server)?;
        validate_http_url("build.orion_server", &build_config.orion_server)?;
    }

    Ok(())
}

pub(crate) fn validate_redis_config(redis_config: &RedisConfig) -> Result<(), MegaError> {
    require_non_empty("redis.url", &redis_config.url)?;
    let url = Url::parse(&redis_config.url)
        .map_err(|e| MegaError::Other(format!("redis.url must be a valid URL: {e}")))?;
    match url.scheme() {
        "redis" | "rediss" => Ok(()),
        scheme => Err(MegaError::Other(format!(
            "redis.url scheme must be 'redis' or 'rediss', got '{scheme}'"
        ))),
    }
}

pub(crate) fn validate_buck_config(buck_config: &BuckConfig) -> Result<(), MegaError> {
    buck_config
        .validate()
        .map_err(|e| MegaError::Other(format!("Invalid Buck configuration: {e}")))
}

pub(crate) fn validate_object_storage_config(
    object_storage: &ObjectStorageConfig,
) -> Result<(), MegaError> {
    match object_storage.storage_type {
        ObjectStorageBackend::Local => require_non_empty(
            "object_storage.local.root_dir",
            &object_storage.local.root_dir,
        ),
        ObjectStorageBackend::S3 => validate_s3_config(object_storage, false),
        ObjectStorageBackend::S3Compatible => validate_s3_config(object_storage, true),
        ObjectStorageBackend::Gcs => {
            require_non_empty("object_storage.gcs.bucket", &object_storage.gcs.bucket)
        }
    }
}

fn validate_s3_config(
    object_storage: &ObjectStorageConfig,
    require_endpoint: bool,
) -> Result<(), MegaError> {
    let s3 = &object_storage.s3;
    require_non_empty("object_storage.s3.region", &s3.region)?;
    require_non_empty("object_storage.s3.bucket", &s3.bucket)?;
    require_non_empty("object_storage.s3.access_key_id", &s3.access_key_id)?;
    require_non_empty("object_storage.s3.secret_access_key", &s3.secret_access_key)?;

    if require_endpoint {
        require_non_empty("object_storage.s3.endpoint_url", &s3.endpoint_url)?;
        validate_http_url("object_storage.s3.endpoint_url", &s3.endpoint_url)?;
    }

    Ok(())
}

pub(crate) fn validate_orion_server_config(
    orion_server_config: &OrionServerConfig,
) -> Result<(), MegaError> {
    if orion_server_config.port == 0 {
        return Err(MegaError::Other(
            "orion_server.port must be between 1 and 65535".to_string(),
        ));
    }
    require_non_empty(
        "orion_server.logger_storage_mode",
        &orion_server_config.logger_storage_mode,
    )?;
    require_non_empty(
        "orion_server.build_log_dir",
        &orion_server_config.build_log_dir,
    )?;
    require_non_empty("orion_server.db_url", &orion_server_config.db_url)?;

    let db_url = Url::parse(&orion_server_config.db_url)
        .map_err(|e| MegaError::Other(format!("orion_server.db_url must be a valid URL: {e}")))?;
    match db_url.scheme() {
        "postgres" | "postgresql" => {}
        scheme => {
            return Err(MegaError::Other(format!(
                "orion_server.db_url scheme must be 'postgres' or 'postgresql', got '{scheme}'"
            )));
        }
    }

    require_non_empty(
        "orion_server.monobase_url",
        &orion_server_config.monobase_url,
    )?;
    validate_http_url(
        "orion_server.monobase_url",
        &orion_server_config.monobase_url,
    )
}

fn require_non_empty(field_path: &str, value: &str) -> Result<(), MegaError> {
    if value.trim().is_empty() {
        return Err(MegaError::Other(format!("{field_path} must not be empty")));
    }

    Ok(())
}

fn require_non_empty_path(field_path: &str, value: &Path) -> Result<(), MegaError> {
    if value.as_os_str().is_empty() {
        return Err(MegaError::Other(format!("{field_path} must not be empty")));
    }

    Ok(())
}

fn validate_http_url(field_path: &str, value: &str) -> Result<(), MegaError> {
    let url = Url::parse(value)
        .map_err(|e| MegaError::Other(format!("{field_path} must be a valid URL: {e}")))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(MegaError::Other(format!(
            "{field_path} scheme must be 'http' or 'https', got '{scheme}'"
        ))),
    }
}

pub fn warn_known_unconsumed_file_fields(path: &Path) -> Result<(), MegaError> {
    let content = std::fs::read_to_string(path)?;
    let value = toml::from_str::<Value>(&content).map_err(|e| {
        MegaError::Other(format!(
            "failed to parse {} for config diagnostics: {e}",
            path.display()
        ))
    })?;

    for warning in known_unconsumed_fields(&value) {
        tracing::warn!(
            path = %path.display(),
            field = %warning.field_path,
            "{}",
            warning.message
        );
    }

    Ok(())
}

pub fn warn_unconsumed_environment_fields() {
    let keys = std::env::vars_os().map(|(key, _)| key);

    for warning in unconsumed_environment_fields_from_keys(keys) {
        tracing::warn!(
            source = "env",
            variable = %warning.variable,
            field = %warning.field_path,
            "{}",
            warning.message
        );
    }
}

pub(crate) fn known_unconsumed_fields(value: &Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();

    if value.get("oauth").is_some() {
        warnings.push(ConfigWarning {
            field_path: "oauth".to_string(),
            message: "[oauth] is currently ignored because OAuthConfig is not implemented"
                .to_string(),
        });
    }

    if let Some(mail) = value.get("mail").and_then(Value::as_table) {
        for field in ["smtp_tls", "tls"] {
            if mail.contains_key(field) {
                warnings.push(ConfigWarning {
                    field_path: format!("mail.{field}"),
                    message: format!(
                        "mail.{field} is ignored by MailConfig; use mail.starttls for STARTTLS behavior"
                    ),
                });
            }
        }
    }

    warnings.extend(unknown_fields(value));

    warnings
}

pub(crate) fn unconsumed_environment_fields_from_keys<I, K>(
    keys: I,
) -> Vec<EnvironmentConfigWarning>
where
    I: IntoIterator<Item = K>,
    K: AsRef<OsStr>,
{
    let mut warnings = keys
        .into_iter()
        .filter_map(|key| {
            let variable = key.as_ref().to_str()?.to_string();
            let field_path = env_key_to_field_path(&variable)?;
            environment_warning_for(&variable, &field_path)
        })
        .collect::<Vec<_>>();

    warnings.sort_by(|left, right| left.variable.cmp(&right.variable));
    warnings
}

fn env_key_to_field_path(variable: &str) -> Option<String> {
    if !variable.starts_with(MEGA_ENV_PREFIX) || is_reserved_mega_env_var(variable) {
        return None;
    }

    let suffix = variable.strip_prefix(MEGA_ENV_PREFIX)?;
    if suffix.is_empty() {
        return None;
    }

    Some(suffix.to_ascii_lowercase().replace("__", "."))
}

fn is_reserved_mega_env_var(variable: &str) -> bool {
    RESERVED_MEGA_ENV_VARS.contains(&variable)
}

fn environment_warning_for(variable: &str, field_path: &str) -> Option<EnvironmentConfigWarning> {
    let message = if field_path == "oauth" || field_path.starts_with("oauth.") {
        format!(
            "{variable} maps to {field_path}, but [oauth] is currently ignored because OAuthConfig is not implemented"
        )
    } else if matches!(field_path, "mail.smtp_tls" | "mail.tls") {
        format!(
            "{variable} maps to {field_path}, which is ignored by MailConfig; use MEGA_MAIL__STARTTLS for STARTTLS behavior"
        )
    } else if !is_known_field_path(field_path) {
        format!(
            "{variable} maps to {field_path}, which is not recognized by Config and will be ignored"
        )
    } else {
        return None;
    };

    Some(EnvironmentConfigWarning {
        variable: variable.to_string(),
        field_path: field_path.to_string(),
        message,
    })
}

fn is_known_field_path(field_path: &str) -> bool {
    let mut schema_path = String::new();
    let mut parts = field_path.split('.').peekable();

    while let Some(field) = parts.next() {
        let Some(allowed_fields) = known_fields(&schema_path) else {
            return false;
        };
        if !allowed_fields.contains(&field) {
            return false;
        }

        if parts.peek().is_some() {
            schema_path = join_field_path(&schema_path, field);
        }
    }

    true
}

fn unknown_fields(value: &Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();

    if let Some(table) = value.as_table() {
        collect_unknown_fields("", "", table, &mut warnings);
    }

    warnings
}

fn collect_unknown_fields(
    schema_path: &str,
    display_path: &str,
    table: &toml::Table,
    warnings: &mut Vec<ConfigWarning>,
) {
    let Some(allowed_fields) = known_fields(schema_path) else {
        return;
    };

    for (field, value) in table {
        let schema_field_path = join_field_path(schema_path, field);
        let display_field_path = join_field_path(display_path, field);

        if !allowed_fields.contains(&field.as_str()) {
            warnings.push(ConfigWarning {
                field_path: display_field_path.clone(),
                message: format!(
                    "{display_field_path} is not recognized by Config and will be ignored"
                ),
            });
            continue;
        }

        if known_fields(&schema_field_path).is_some() {
            match value {
                Value::Table(child_table) => collect_unknown_fields(
                    &schema_field_path,
                    &display_field_path,
                    child_table,
                    warnings,
                ),
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        if let Some(item_table) = item.as_table() {
                            collect_unknown_fields(
                                &schema_field_path,
                                &format!("{display_field_path}[{index}]"),
                                item_table,
                                warnings,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn join_field_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_string()
    } else {
        format!("{prefix}.{field}")
    }
}

fn known_fields(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "" => Some(&[
            "base_dir",
            "log",
            "database",
            "monorepo",
            "build",
            "pack",
            "lfs",
            "object_storage",
            "oauth",
            "blame",
            "redis",
            "buck",
            "artifacts_gc",
            "orion_server",
            "sidebar",
            "mail",
        ]),
        "log" => Some(&["level", "print_std", "with_ansi"]),
        "database" => Some(&[
            "db_type",
            "db_path",
            "db_url",
            "max_connection",
            "min_connection",
            "acquire_timeout",
            "connect_timeout",
            "sqlx_logging",
        ]),
        "monorepo" => Some(&["import_dir", "admin", "root_dirs", "rename"]),
        "monorepo.rename" => Some(&["similarity_threshold", "rename_limit"]),
        "build" => Some(&[
            "enable_build",
            "orion_server",
            "orion_preheat_shallow_depth",
        ]),
        "pack" => Some(&[
            "pack_decode_mem_size",
            "pack_decode_disk_size",
            "pack_decode_cache_path",
            "clean_cache_after_decode",
            "channel_message_size",
            "save_entry_concurrency",
        ]),
        "lfs" => Some(&["ssh", "local"]),
        "lfs.ssh" => Some(&["http_url"]),
        "lfs.local" => Some(&["lfs_file_path"]),
        "object_storage" => Some(&["storage_type", "s3", "gcs", "local"]),
        "object_storage.s3" => Some(&[
            "region",
            "bucket",
            "access_key_id",
            "secret_access_key",
            "endpoint_url",
        ]),
        "object_storage.gcs" => Some(&["bucket"]),
        "object_storage.local" => Some(&["root_dir"]),
        "blame" => Some(&[
            "max_lines_threshold",
            "max_size_threshold",
            "default_chunk_size",
            "max_commits_in_memory",
            "enable_caching",
        ]),
        "redis" => Some(&["url"]),
        "buck" => Some(&[
            "session_timeout",
            "max_file_size",
            "max_files",
            "max_concurrent_uploads",
            "upload_concurrency_limit",
            "large_file_concurrency_limit",
            "large_file_threshold",
            "enable_session_cleanup",
            "cleanup_interval",
            "completed_retention_days",
        ]),
        "artifacts_gc" => Some(&["enable", "interval_secs", "grace_secs", "batch_limit"]),
        "orion_server" => Some(&[
            "logger_storage_mode",
            "build_log_dir",
            "log_stream_buffer",
            "db_url",
            "port",
            "monobase_url",
        ]),
        "sidebar" => Some(&["default_items"]),
        "sidebar.default_items" => Some(&["public_id", "label", "href", "visible", "order_index"]),
        "mail" => Some(&[
            "enabled",
            "smtp_host",
            "smtp_port",
            "username",
            "password",
            "password_ref",
            "from",
            "starttls",
            "smtp_tls",
            "tls",
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use orbit_api::factory::{GcsConfig, LocalConfig, S3Config};

    use super::*;
    use crate::config::{secret::SecretRef, template::config_init_template};

    #[test]
    fn config_validate_accepts_default_mock_config() {
        Config::mock()
            .validate()
            .expect("mock config should validate");
    }

    #[test]
    fn config_validate_rejects_non_postgres_database_type() {
        let mut config = Config::mock();
        config.database.db_type = "mysql".to_string();

        let err = config.validate().expect_err("database type should fail");

        assert!(err.to_string().contains("database.db_type"));
    }

    #[test]
    fn config_validate_rejects_non_postgres_database_url_scheme() {
        let mut config = Config::mock();
        config.database.db_url = "mysql://mono:mono@localhost:3306/mono".to_string();

        let err = config
            .validate()
            .expect_err("database URL scheme should fail");

        assert!(err.to_string().contains("database.db_url scheme"));
    }

    #[test]
    fn config_validate_rejects_unknown_log_level() {
        let mut config = Config::mock();
        config.log.level = "verbose".to_string();

        let err = config.validate().expect_err("log level should fail");

        assert!(err.to_string().contains("log.level"));
    }

    #[test]
    fn config_validate_rejects_invalid_lfs_http_url() {
        let mut config = Config::mock();
        config.lfs.ssh.http_url = "ftp://localhost:8000".to_string();

        let err = config.validate().expect_err("lfs url should fail");

        assert!(err.to_string().contains("lfs.ssh.http_url"));
    }

    #[test]
    fn config_validate_rejects_enabled_build_without_orion_url() {
        let mut config = Config::mock();
        config.build.enable_build = true;
        config.build.orion_server = String::new();

        let err = config
            .validate()
            .expect_err("enabled build without Orion URL should fail");

        assert!(err.to_string().contains("build.orion_server"));
    }

    #[test]
    fn config_validate_rejects_invalid_redis_url_scheme() {
        let mut config = Config::mock();
        config.redis.url = "http://localhost:6379".to_string();

        let err = config.validate().expect_err("redis scheme should fail");

        assert!(err.to_string().contains("redis.url scheme"));
    }

    #[test]
    fn config_validate_rejects_invalid_orion_server_config() {
        let mut config = Config::mock();
        let orion_server = OrionServerConfig {
            port: 0,
            ..Default::default()
        };
        config.orion_server = Some(orion_server);

        let err = config
            .validate()
            .expect_err("orion server port should fail");

        assert!(err.to_string().contains("orion_server.port"));
    }

    #[test]
    fn mail_validate_rejects_password_and_password_ref_together() {
        let mail_config = MailConfig {
            enabled: false,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: Some(crate::config::secret::SecretString::new("plain")),
            password_ref: Some(
                SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap(),
            ),
            from: "no-reply@example.com".to_string(),
            starttls: true,
        };

        let err = mail_config
            .validate()
            .expect_err("mutual exclusion should fail");

        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn mail_validate_rejects_missing_enabled_smtp_host() {
        let mail_config = MailConfig {
            enabled: true,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
        };

        let err = mail_config.validate().expect_err("smtp host should fail");

        assert!(err.to_string().contains("mail.smtp_host"));
    }

    #[test]
    fn config_validate_rejects_invalid_buck_config() {
        let mut config = Config::mock();
        config.buck = Some(BuckConfig {
            max_files: 0,
            ..Default::default()
        });

        let err = config.validate().expect_err("buck config should fail");

        assert!(err.to_string().contains("Invalid Buck configuration"));
        assert!(err.to_string().contains("max_files"));
    }

    #[test]
    fn config_validate_rejects_empty_local_object_storage_root() {
        let mut config = Config::mock();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("local object storage root should fail");

        assert!(err.to_string().contains("object_storage.local.root_dir"));
    }

    #[test]
    fn config_validate_rejects_incomplete_s3_object_storage() {
        let mut config = Config::mock();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: String::new(),
                access_key_id: "key".to_string(),
                secret_access_key: "secret".to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("missing s3 bucket should fail");

        assert!(err.to_string().contains("object_storage.s3.bucket"));
    }

    #[test]
    fn config_validate_rejects_s3_compatible_without_endpoint() {
        let mut config = Config::mock();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3Compatible,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "mono".to_string(),
                access_key_id: "key".to_string(),
                secret_access_key: "secret".to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("missing s3-compatible endpoint should fail");

        assert!(err.to_string().contains("object_storage.s3.endpoint_url"));
    }

    #[test]
    fn config_validate_accepts_complete_s3_compatible_object_storage() {
        let mut config = Config::mock();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3Compatible,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "mono".to_string(),
                access_key_id: "key".to_string(),
                secret_access_key: "secret".to_string(),
                endpoint_url: "http://localhost:9000".to_string(),
            },
            ..Default::default()
        };

        config
            .validate()
            .expect("complete s3-compatible config should validate");
    }

    #[test]
    fn config_validate_rejects_incomplete_gcs_object_storage() {
        let mut config = Config::mock();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Gcs,
            gcs: GcsConfig {
                bucket: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("missing gcs bucket should fail");

        assert!(err.to_string().contains("object_storage.gcs.bucket"));
    }

    #[test]
    fn known_unconsumed_fields_warns_for_oauth_and_legacy_mail_tls_keys() {
        let value = toml::from_str::<Value>(
            r#"
            [oauth]
            enabled = true

            [mail]
            smtp_tls = false
            tls = false
            starttls = false
            "#,
        )
        .unwrap();

        let warnings = known_unconsumed_fields(&value);
        let fields = warnings
            .iter()
            .map(|warning| warning.field_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(fields, vec!["oauth", "mail.smtp_tls", "mail.tls"]);
    }

    #[test]
    fn known_unconsumed_fields_warns_for_unknown_fields() {
        let value = toml::from_str::<Value>(
            r#"
            unknown_root = true

            [database]
            typo = true

            [object_storage.s3]
            unexpected = true

            [mail]
            smtp_tls = false
            extra = true

            [sidebar]
            default_items = [
                { public_id = "home", label = "Home", href = "/posts", visible = true, order_index = 0, icon = "home" },
            ]
            "#,
        )
        .unwrap();

        let warnings = known_unconsumed_fields(&value);
        let fields = warnings
            .iter()
            .map(|warning| warning.field_path.as_str())
            .collect::<Vec<_>>();

        assert!(fields.contains(&"mail.smtp_tls"));
        assert!(fields.contains(&"unknown_root"));
        assert!(fields.contains(&"database.typo"));
        assert!(fields.contains(&"object_storage.s3.unexpected"));
        assert!(fields.contains(&"mail.extra"));
        assert!(fields.contains(&"sidebar.default_items[0].icon"));
    }

    #[test]
    fn unconsumed_environment_fields_warns_for_unknown_ignored_and_legacy_keys() {
        let warnings = unconsumed_environment_fields_from_keys([
            "MEGA_DATABASE__DB_URL",
            "MEGA_MONOREPO__ROOT_DIRS",
            "MEGA_LOG__PRINT_STD",
            "MEGA_CONFIG",
            "MEGA_PROFILE",
            "MEGA_BASE_DIR",
            "MEGA_CACHE_DIR",
            "OTHER_VAR",
            "MEGA_UNKNOWN__VALUE",
            "MEGA_OAUTH__ALLOWED_CORS_ORIGINS",
            "MEGA_MAIL__TLS",
            "MEGA_MAIL__SMTP_TLS",
        ]);
        let variables = warnings
            .iter()
            .map(|warning| warning.variable.as_str())
            .collect::<Vec<_>>();
        let fields = warnings
            .iter()
            .map(|warning| warning.field_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            variables,
            vec![
                "MEGA_MAIL__SMTP_TLS",
                "MEGA_MAIL__TLS",
                "MEGA_OAUTH__ALLOWED_CORS_ORIGINS",
                "MEGA_UNKNOWN__VALUE",
            ]
        );
        assert_eq!(
            fields,
            vec![
                "mail.smtp_tls",
                "mail.tls",
                "oauth.allowed_cors_origins",
                "unknown.value",
            ]
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("MEGA_MAIL__STARTTLS"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("OAuthConfig"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("not recognized by Config"))
        );
    }

    #[test]
    fn known_config_field_path_accepts_nested_fields_and_rejects_orphans() {
        assert!(is_known_field_path("database.db_url"));
        assert!(is_known_field_path("object_storage.s3.access_key_id"));
        assert!(is_known_field_path("sidebar.default_items.label"));
        assert!(!is_known_field_path("database.db_url.extra"));
        assert!(!is_known_field_path("database.typo"));
        assert!(!is_known_field_path("unknown.value"));
        assert!(!is_known_field_path("oauth.allowed_cors_origins"));
    }

    #[test]
    fn config_init_template_has_no_unconsumed_fields() {
        let rendered = config_init_template(Path::new("/tmp/monoengine"));
        let value = toml::from_str::<Value>(&rendered).unwrap();

        assert!(known_unconsumed_fields(&value).is_empty());
    }
}
