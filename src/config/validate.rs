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

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secret::SecretRef;

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
}
