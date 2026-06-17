use std::path::Path;

use toml::Value;
use url::Url;

use super::{BuckConfig, Config, DbConfig, MailConfig};
use crate::common::errors::MegaError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub field_path: String,
    pub message: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), MegaError> {
        validate_database_config(&self.database)?;
        if let Some(mail_config) = &self.mail {
            mail_config.validate()?;
        }
        if let Some(buck_config) = &self.buck {
            validate_buck_config(buck_config)?;
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

pub(crate) fn validate_buck_config(buck_config: &BuckConfig) -> Result<(), MegaError> {
    buck_config
        .validate()
        .map_err(|e| MegaError::Other(format!("Invalid Buck configuration: {e}")))
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
    fn config_init_template_has_no_unconsumed_fields() {
        let rendered = config_init_template(Path::new("/tmp/monoengine"));
        let value = toml::from_str::<Value>(&rendered).unwrap();

        assert!(known_unconsumed_fields(&value).is_empty());
    }
}
