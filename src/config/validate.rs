use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig};
use toml::Value;
use url::Url;

use super::{
    ArtifactGcConfig, BlameConfig, BuckConfig, BuildConfig, ChatConfig, Config, DbConfig,
    LFSConfig, LogConfig, MailConfig, MailProvider, MonoConfig, NOTIFICATION_DELIVERY_MODES,
    NotificationConfig, OAuthConfig, OrionServerConfig, PackConfig, RedisConfig, SidebarConfig,
    VAULT_AUDIT_SINKS, VaultConfig,
    secret::{SecretRef, is_secret_ref_value},
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
pub struct FileConfigWarning {
    pub source_path: PathBuf,
    pub field_path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentConfigWarning {
    pub variable: String,
    pub field_path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSourceOverride {
    pub field_path: String,
    pub source: String,
    pub overridden_source: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSourceField {
    pub field_path: String,
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigSourceDiagnostics {
    pub file_warnings: Vec<FileConfigWarning>,
    pub environment_warnings: Vec<EnvironmentConfigWarning>,
    pub source_overrides: Vec<ConfigSourceOverride>,
    pub source_fields: Vec<ConfigSourceField>,
}

impl ConfigSourceDiagnostics {
    pub fn is_empty(&self) -> bool {
        self.file_warnings.is_empty()
            && self.environment_warnings.is_empty()
            && self.source_overrides.is_empty()
            && self.source_fields.is_empty()
    }

    pub fn has_warnings(&self) -> bool {
        !self.file_warnings.is_empty() || !self.environment_warnings.is_empty()
    }

    pub fn warning_count(&self) -> usize {
        self.file_warnings.len() + self.environment_warnings.len()
    }

    pub fn emit_warnings(&self) {
        for warning in &self.file_warnings {
            tracing::warn!(
                path = %warning.source_path.display(),
                field = %warning.field_path,
                "{}",
                warning.message
            );
        }

        for warning in &self.environment_warnings {
            tracing::warn!(
                source = "env",
                variable = %warning.variable,
                field = %warning.field_path,
                "{}",
                warning.message
            );
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), MegaError> {
        validate_log_config(&self.log)?;
        validate_database_config(&self.database)?;
        validate_monorepo_config(&self.monorepo)?;
        validate_pack_config(&self.pack)?;
        validate_blame_config(&self.blame)?;
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
        validate_sidebar_config(&self.sidebar)?;
        validate_artifact_gc_config(&self.artifacts_gc)?;
        if let Some(notification_config) = &self.notification {
            validate_notification_config(notification_config)?;
        }
        if let Some(vault_config) = &self.vault {
            validate_vault_config(vault_config)?;
        }
        if let Some(oauth_config) = &self.oauth {
            validate_oauth_config(oauth_config)?;
        }
        if let Some(chat_config) = &self.chat {
            validate_chat_config(chat_config)?;
        }

        Ok(())
    }
}

/// Validate `[oauth]` settings: each CORS origin must be a browser Origin of the
/// form `scheme://host[:port]` (http/https, no path/query/fragment) that also
/// parses as an HTTP header value — i.e. exactly what the server's `CorsLayer`
/// accepts at runtime, so a configured origin can never pass validation yet be
/// silently dropped by the CORS layer.
pub(crate) fn validate_oauth_config(config: &OAuthConfig) -> Result<(), MegaError> {
    for origin in &config.allowed_cors_origins {
        validate_cors_origin(origin)?;
    }
    Ok(())
}

/// Validate `[chat]` settings: each MIME allowlist entry must be a non-empty
/// type/subtype pattern without control characters. Wildcards are allowed only
/// for the subtype (`image/*`), matching the runtime check in
/// `validate_chat_attachment_metadata`.
pub(crate) fn validate_chat_config(config: &ChatConfig) -> Result<(), MegaError> {
    for pattern in &config.attachment_allowed_mime_types {
        validate_mime_allowlist_pattern(pattern)?;
    }
    if config.open_graph_fetch_timeout_ms == 0 {
        return Err(MegaError::Other(
            "chat.open_graph_fetch_timeout_ms must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

fn validate_mime_allowlist_pattern(pattern: &str) -> Result<(), MegaError> {
    if pattern.trim().is_empty() {
        return Err(MegaError::Other(
            "chat.attachment_allowed_mime_types must not contain empty entries".to_string(),
        ));
    }
    if pattern.chars().any(|c| c.is_control()) {
        return Err(MegaError::Other(format!(
            "chat.attachment_allowed_mime_types entry `{pattern}` must not contain control characters"
        )));
    }
    let parts: Vec<&str> = pattern.split('/').collect();
    if parts.len() != 2
        || parts[0].trim().is_empty()
        || parts[1].trim().is_empty()
        || parts[0].contains('*')
        || parts[1].contains('*') && parts[1] != "*"
    {
        return Err(MegaError::Other(format!(
            "chat.attachment_allowed_mime_types entry `{pattern}` must be a MIME type (`type/subtype`) or wildcard (`type/*`)"
        )));
    }
    Ok(())
}

fn validate_cors_origin(origin: &str) -> Result<(), MegaError> {
    if origin.trim().is_empty() {
        return Err(MegaError::Other(
            "oauth.allowed_cors_origins must not contain empty entries".to_string(),
        ));
    }
    // A real Origin cannot contain whitespace (HTTP header values technically
    // permit spaces/tabs, so reject them explicitly).
    if origin.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(MegaError::Other(format!(
            "oauth.allowed_cors_origins entry `{origin}` must not contain whitespace or control characters"
        )));
    }
    // Must be usable as the `Access-Control-Allow-Origin` header value the CORS
    // layer builds at runtime (`HeaderValue::from_str`), which rejects non-ASCII.
    if http::HeaderValue::from_str(origin).is_err() {
        return Err(MegaError::Other(format!(
            "oauth.allowed_cors_origins entry `{origin}` is not a valid HTTP header value"
        )));
    }
    // Must be a `scheme://host[:port]` origin: http/https, no path/query/fragment.
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Err(MegaError::Other(format!(
            "oauth.allowed_cors_origins entry `{origin}` must be a scheme://host origin"
        )));
    };
    if scheme != "http" && scheme != "https" {
        return Err(MegaError::Other(format!(
            "oauth.allowed_cors_origins entry `{origin}` must use the http or https scheme"
        )));
    }
    if rest.is_empty() || rest.contains(['/', '?', '#']) {
        return Err(MegaError::Other(format!(
            "oauth.allowed_cors_origins entry `{origin}` must not contain a path, query, or fragment (use scheme://host[:port])"
        )));
    }
    Ok(())
}

/// Validate `[vault]` settings (docs/vault.md stage H): the audit sink must be a
/// supported value, and a `file` sink requires a non-empty `file_path`.
pub(crate) fn validate_vault_config(config: &VaultConfig) -> Result<(), MegaError> {
    let audit = &config.audit;
    if !VAULT_AUDIT_SINKS.contains(&audit.sink.as_str()) {
        return Err(MegaError::Other(format!(
            "vault.audit.sink `{}` is not supported; expected one of {:?}",
            audit.sink, VAULT_AUDIT_SINKS
        )));
    }
    if audit.sink == "file"
        && audit
            .file_path
            .as_ref()
            .map(|path| path.as_os_str().is_empty())
            .unwrap_or(true)
    {
        return Err(MegaError::Other(
            "vault.audit.file_path is required (and must be non-empty) when vault.audit.sink is \"file\""
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_notification_config(config: &NotificationConfig) -> Result<(), MegaError> {
    if !NOTIFICATION_DELIVERY_MODES.contains(&config.default_delivery_mode.as_str()) {
        return Err(MegaError::Other(format!(
            "notification.default_delivery_mode `{}` is not supported; expected one of {:?}",
            config.default_delivery_mode, NOTIFICATION_DELIVERY_MODES
        )));
    }
    if config.default_locale.trim().is_empty() {
        return Err(MegaError::Other(
            "notification.default_locale must not be empty".to_string(),
        ));
    }

    if let Some(slack) = &config.slack
        && slack.enabled
    {
        let Some(secret_ref) = &slack.webhook_url_ref else {
            return Err(MegaError::Other(
                "notification.slack.webhook_url_ref is required when notification.slack.enabled is true".to_string(),
            ));
        };
        validate_config_secret_ref(
            "notification.slack.webhook_url_ref",
            secret_ref,
            "notification/slack/webhook_url",
        )?;
    }

    if let Some(webhook) = &config.webhook
        && webhook.enabled
    {
        if webhook.url.trim().is_empty() {
            return Err(MegaError::Other(
                "notification.webhook.url must not be empty when notification.webhook.enabled is true".to_string(),
            ));
        }
        if let Some(secret_ref) = &webhook.token_ref {
            validate_config_secret_ref(
                "notification.webhook.token_ref",
                secret_ref,
                "notification/webhook/token",
            )?;
        }
    }

    Ok(())
}

/// Validate that a config-managed `SecretRef` lives under the expected
/// `config/<profile>/<suffix>` namespace (used for `mail.password` and the
/// notification channel credentials). The SecretRef value is never logged.
pub(crate) fn validate_config_secret_ref(
    field_path: &str,
    secret_ref: &SecretRef,
    suffix: &str,
) -> Result<(), MegaError> {
    if is_config_secret_under(secret_ref.secret_name(), suffix) {
        return Ok(());
    }

    Err(MegaError::Other(format!(
        "{field_path} must use a vault SecretRef under vault://secret/config/<profile>/{suffix}#<field>; value is redacted"
    )))
}

/// True when `secret_name` is exactly `config/<profile>/<suffix>` with a single
/// non-empty `<profile>` segment (no extra path components between `config/` and
/// the suffix), so a ref cannot point at an unrelated nested vault path.
fn is_config_secret_under(secret_name: &str, suffix: &str) -> bool {
    let Some(rest) = secret_name.strip_prefix("config/") else {
        return false;
    };
    let trimmed_suffix = format!("/{suffix}");
    let Some(profile) = rest.strip_suffix(&trimmed_suffix) else {
        return false;
    };

    // Exactly one profile segment: non-empty and containing no further '/'.
    !profile.is_empty() && !profile.contains('/')
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
        if let Some(secret_ref) = &self.password_ref {
            validate_mail_password_secret_ref("mail.password_ref", secret_ref)?;
        }
        if self.provider != MailProvider::Smtp
            && (self.password.is_some() || self.password_ref.is_some())
        {
            return Err(MegaError::Other(
                "mail.password and mail.password_ref are only supported when mail.provider is smtp"
                    .to_string(),
            ));
        }

        if self.enabled && self.provider == MailProvider::Smtp {
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

        if self.enabled && self.provider == MailProvider::Http {
            let url = self
                .http_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    MegaError::Other(
                        "mail.http_url is required when mail.provider is http".to_string(),
                    )
                })?;
            let parsed = reqwest::Url::parse(url)
                .map_err(|e| MegaError::Other(format!("mail.http_url is not a valid URL: {e}")))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                return Err(MegaError::Other(format!(
                    "mail.http_url scheme must be http or https, got {}",
                    parsed.scheme()
                )));
            }
            for (name, value) in &self.http_headers {
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                    MegaError::Other(format!(
                        "mail.http_headers key '{name}' is not a valid HTTP header name: {e}"
                    ))
                })?;
                reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                    MegaError::Other(format!(
                        "mail.http_headers value for '{name}' is not a valid HTTP header value: {e}"
                    ))
                })?;
            }
        }

        if self.dispatcher_batch_size == 0 {
            return Err(MegaError::Other(
                "mail.dispatcher_batch_size must be greater than 0".to_string(),
            ));
        }
        if self.dispatcher_max_in_flight == 0 {
            return Err(MegaError::Other(
                "mail.dispatcher_max_in_flight must be greater than 0".to_string(),
            ));
        }
        if self.retry_max_attempts <= 0 {
            return Err(MegaError::Other(
                "mail.retry_max_attempts must be greater than 0".to_string(),
            ));
        }
        if self.retry_backoff_base_secs <= 0 {
            return Err(MegaError::Other(
                "mail.retry_backoff_base_secs must be greater than 0".to_string(),
            ));
        }
        if self.retry_backoff_max_secs <= 0 {
            return Err(MegaError::Other(
                "mail.retry_backoff_max_secs must be greater than 0".to_string(),
            ));
        }
        if self.retry_backoff_max_secs < self.retry_backoff_base_secs {
            return Err(MegaError::Other(
                "mail.retry_backoff_max_secs must be greater than or equal to mail.retry_backoff_base_secs".to_string(),
            ));
        }
        if self.attachment_prune_interval_secs == 0 {
            return Err(MegaError::Other(
                "mail.attachment_prune_interval_secs must be greater than 0".to_string(),
            ));
        }
        if self.attachment_retention_days == 0 {
            return Err(MegaError::Other(
                "mail.attachment_retention_days must be greater than 0".to_string(),
            ));
        }
        if self.attachment_prune_statuses.is_empty() {
            return Err(MegaError::Other(
                "mail.attachment_prune_statuses must not be empty".to_string(),
            ));
        }

        let mut statuses = BTreeSet::new();
        for status in &self.attachment_prune_statuses {
            let status = status.trim();
            if !matches!(status, "sent" | "skipped") {
                return Err(MegaError::Other(
                    "mail.attachment_prune_statuses entries must be `sent` or `skipped`"
                        .to_string(),
                ));
            }
            if !statuses.insert(status) {
                return Err(MegaError::Other(
                    "mail.attachment_prune_statuses must not contain duplicates".to_string(),
                ));
            }
        }
        if self.template_default_locale.trim().is_empty() {
            return Err(MegaError::Other(
                "mail.template_default_locale must not be empty".to_string(),
            ));
        }
        if !self
            .template_default_locale
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        {
            return Err(MegaError::Other(
                "mail.template_default_locale contains unsupported characters".to_string(),
            ));
        }
        if let Some(template_dir) = &self.template_dir {
            if template_dir.as_os_str().is_empty() {
                return Err(MegaError::Other(
                    "mail.template_dir must not be empty".to_string(),
                ));
            }
            if !template_dir.is_dir() {
                return Err(MegaError::Other(format!(
                    "mail.template_dir must point to an existing directory: {}",
                    template_dir.display()
                )));
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

    if db_config.max_connection == 0 {
        return Err(MegaError::Other(
            "database.max_connection must be greater than 0".to_string(),
        ));
    }
    if db_config.min_connection > db_config.max_connection {
        return Err(MegaError::Other(format!(
            "database.min_connection must be less than or equal to database.max_connection; got {} > {}",
            db_config.min_connection, db_config.max_connection
        )));
    }
    if db_config.acquire_timeout == 0 {
        return Err(MegaError::Other(
            "database.acquire_timeout must be greater than 0".to_string(),
        ));
    }
    if db_config.connect_timeout == 0 {
        return Err(MegaError::Other(
            "database.connect_timeout must be greater than 0".to_string(),
        ));
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

pub(crate) fn validate_monorepo_config(mono_config: &MonoConfig) -> Result<(), MegaError> {
    require_non_empty_path("monorepo.import_dir", &mono_config.import_dir)?;
    if mono_config.root_dirs.is_empty() {
        return Err(MegaError::Other(
            "monorepo.root_dirs must contain at least one root directory".to_string(),
        ));
    }
    require_non_empty_list_entries("monorepo.root_dirs", &mono_config.root_dirs)?;
    require_non_empty_list_entries("monorepo.admin", &mono_config.admin)?;

    if mono_config.rename.similarity_threshold > 100 {
        return Err(MegaError::Other(format!(
            "monorepo.rename.similarity_threshold must be between 0 and 100; got {}",
            mono_config.rename.similarity_threshold
        )));
    }

    Ok(())
}

pub(crate) fn validate_pack_config(pack_config: &PackConfig) -> Result<(), MegaError> {
    validate_size_string(
        "pack.pack_decode_mem_size",
        &pack_config.pack_decode_mem_size,
    )?;
    validate_size_string(
        "pack.pack_decode_disk_size",
        &pack_config.pack_decode_disk_size,
    )?;
    require_non_empty_path(
        "pack.pack_decode_cache_path",
        &pack_config.pack_decode_cache_path,
    )?;
    if pack_config.channel_message_size == 0 {
        return Err(MegaError::Other(
            "pack.channel_message_size must be greater than 0".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_blame_config(blame_config: &BlameConfig) -> Result<(), MegaError> {
    if blame_config.max_lines_threshold == 0 {
        return Err(MegaError::Other(
            "blame.max_lines_threshold must be greater than 0".to_string(),
        ));
    }
    validate_size_string("blame.max_size_threshold", &blame_config.max_size_threshold)?;
    if blame_config.default_chunk_size == 0 {
        return Err(MegaError::Other(
            "blame.default_chunk_size must be greater than 0".to_string(),
        ));
    }
    if blame_config.max_commits_in_memory == 0 {
        return Err(MegaError::Other(
            "blame.max_commits_in_memory must be greater than 0".to_string(),
        ));
    }

    Ok(())
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
    let trimmed = redis_config.url.trim_start();
    if is_secret_ref_value(trimmed) {
        let secret_ref = SecretRef::parse(trimmed)?;
        validate_config_secret_ref("redis.url", &secret_ref, "redis/url")?;
        return Ok(());
    }
    validate_redis_url_literal("redis.url", &redis_config.url)
}

/// Validate that a resolved `redis.url` value is a literal `redis://` / `rediss://`
/// URL. This is used after a `vault://` SecretRef has been resolved so that a
/// malformed or accidentally nested SecretRef value fails with a redacted,
/// diagnostic error before reaching the Redis client.
pub(crate) fn validate_redis_url_literal(field_path: &str, url: &str) -> Result<(), MegaError> {
    let parsed = Url::parse(url)
        .map_err(|_| MegaError::Other(format!("{field_path} must be a valid URL")))?;
    match parsed.scheme() {
        "redis" | "rediss" => Ok(()),
        _ => Err(MegaError::Other(format!(
            "{field_path} scheme must be 'redis' or 'rediss'"
        ))),
    }
}

pub(crate) fn validate_mail_password_secret_ref(
    field_path: &str,
    secret_ref: &SecretRef,
) -> Result<(), MegaError> {
    validate_config_secret_ref(field_path, secret_ref, "mail/password")
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
    validate_object_storage_secret_ref(
        "object_storage.s3.access_key_id",
        &s3.access_key_id,
        "object_storage/access_key_id",
    )?;
    validate_object_storage_secret_ref(
        "object_storage.s3.secret_access_key",
        &s3.secret_access_key,
        "object_storage/secret_access_key",
    )?;

    if require_endpoint {
        require_non_empty("object_storage.s3.endpoint_url", &s3.endpoint_url)?;
        validate_http_url("object_storage.s3.endpoint_url", &s3.endpoint_url)?;
    }

    Ok(())
}

/// Validate that an object-storage credential field is either a literal value or
/// a well-formed `vault://` SecretRef under the required namespace. Literal
/// values are passed through unchanged; vault refs are validated for format and
/// namespace so that `config validate` aligns with the runtime resolution path
/// (`context::resolve_object_storage_secrets`).
fn validate_object_storage_secret_ref(
    field_path: &str,
    value: &str,
    suffix: &str,
) -> Result<(), MegaError> {
    let trimmed = value.trim_start();
    if !trimmed.starts_with("vault://") {
        return Ok(());
    }

    let secret_ref = SecretRef::parse(trimmed)?;
    validate_config_secret_ref(field_path, &secret_ref, suffix)
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

pub(crate) fn validate_artifact_gc_config(
    artifact_gc_config: &ArtifactGcConfig,
) -> Result<(), MegaError> {
    if artifact_gc_config.interval_secs == 0 {
        return Err(MegaError::Other(
            "artifacts_gc.interval_secs must be greater than 0".to_string(),
        ));
    }
    if artifact_gc_config.batch_limit == 0 {
        return Err(MegaError::Other(
            "artifacts_gc.batch_limit must be greater than 0".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_sidebar_config(sidebar_config: &SidebarConfig) -> Result<(), MegaError> {
    let mut public_ids = BTreeSet::new();
    for (index, item) in sidebar_config.default_items.iter().enumerate() {
        let public_id_field = format!("sidebar.default_items[{index}].public_id");
        require_non_empty(&public_id_field, &item.public_id)?;
        if !public_ids.insert(item.public_id.clone()) {
            return Err(MegaError::Other(format!(
                "{public_id_field} must be unique; duplicate public_id '{}'",
                item.public_id
            )));
        }

        let label_field = format!("sidebar.default_items[{index}].label");
        require_non_empty(&label_field, &item.label)?;

        let href_field = format!("sidebar.default_items[{index}].href");
        require_non_empty(&href_field, &item.href)?;
    }

    Ok(())
}

fn validate_size_string(field_path: &str, value: &str) -> Result<(), MegaError> {
    let bytes = PackConfig::get_size_from_str(value, || Ok(8 * 1024 * 1024 * 1024))
        .map_err(|e| MegaError::Other(format!("{field_path} must be a valid size: {e}")))?;
    if bytes == 0 {
        return Err(MegaError::Other(format!(
            "{field_path} must be greater than 0"
        )));
    }

    Ok(())
}

fn require_non_empty(field_path: &str, value: &str) -> Result<(), MegaError> {
    if value.trim().is_empty() {
        return Err(MegaError::Other(format!("{field_path} must not be empty")));
    }

    Ok(())
}

fn require_non_empty_list_entries(field_path: &str, values: &[String]) -> Result<(), MegaError> {
    for (index, value) in values.iter().enumerate() {
        if value.trim().is_empty() {
            return Err(MegaError::Other(format!(
                "{field_path}[{index}] must not be empty"
            )));
        }
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
    ConfigSourceDiagnostics {
        file_warnings: known_unconsumed_file_fields(path)?,
        ..Default::default()
    }
    .emit_warnings();

    Ok(())
}

pub fn known_unconsumed_file_fields(path: &Path) -> Result<Vec<FileConfigWarning>, MegaError> {
    let value = read_toml_config_file(path)?;
    Ok(known_unconsumed_file_fields_from_value(path, &value))
}

pub fn warn_unconsumed_environment_fields() {
    ConfigSourceDiagnostics {
        environment_warnings: unconsumed_environment_fields_from_keys(
            std::env::vars_os().map(|(key, _)| key),
        ),
        ..Default::default()
    }
    .emit_warnings();
}

pub fn collect_source_diagnostics(
    config_path: Option<&Path>,
    config_profile_path: Option<&Path>,
) -> Result<ConfigSourceDiagnostics, MegaError> {
    collect_source_diagnostics_from_keys(
        config_path,
        config_profile_path,
        std::env::vars_os().map(|(key, _)| key),
    )
}

pub fn collect_source_diagnostics_from_keys<I, K>(
    config_path: Option<&Path>,
    config_profile_path: Option<&Path>,
    env_keys: I,
) -> Result<ConfigSourceDiagnostics, MegaError>
where
    I: IntoIterator<Item = K>,
    K: AsRef<OsStr>,
{
    let base_value = if let Some(config_path) = config_path {
        Some((
            config_path.to_path_buf(),
            read_toml_config_file(config_path)?,
        ))
    } else {
        None
    };
    let profile_value = if let Some(config_profile_path) = config_profile_path {
        Some((
            config_profile_path.to_path_buf(),
            read_toml_config_file(config_profile_path)?,
        ))
    } else {
        None
    };

    let mut file_warnings = Vec::new();
    if let Some((path, value)) = &base_value {
        file_warnings.extend(known_unconsumed_file_fields_from_value(path, value));
    }
    if let Some((path, value)) = &profile_value {
        file_warnings.extend(known_unconsumed_file_fields_from_value(path, value));
    }

    let env_field_paths = environment_field_paths_from_keys(env_keys);

    Ok(ConfigSourceDiagnostics {
        file_warnings,
        environment_warnings: unconsumed_environment_fields_from_paths(&env_field_paths),
        source_overrides: source_overrides(&base_value, &profile_value, &env_field_paths),
        source_fields: source_fields(&base_value, &profile_value, &env_field_paths),
    })
}

fn read_toml_config_file(path: &Path) -> Result<Value, MegaError> {
    let content = std::fs::read_to_string(path)?;
    toml::from_str::<Value>(&content).map_err(|e| {
        MegaError::Other(format!(
            "failed to parse {} for config diagnostics: {e}",
            path.display()
        ))
    })
}

fn known_unconsumed_file_fields_from_value(path: &Path, value: &Value) -> Vec<FileConfigWarning> {
    known_unconsumed_fields(value)
        .into_iter()
        .map(|warning| FileConfigWarning {
            source_path: path.to_path_buf(),
            field_path: warning.field_path,
            message: warning.message,
        })
        .collect()
}

pub(crate) fn known_unconsumed_fields(value: &Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();

    // `oauth.allowed_cors_origins` is now consumed by OAuthConfig; the remaining
    // legacy keys are still ignored (no consumer yet), so warn per-key.
    if let Some(oauth) = value.get("oauth").and_then(Value::as_table) {
        for field in [
            "campsite_api_domain",
            "tinyship_api_domain",
            "api_store_backend",
        ] {
            if oauth.contains_key(field) {
                warnings.push(ConfigWarning {
                    field_path: format!("oauth.{field}"),
                    message: format!(
                        "oauth.{field} is currently ignored (no consumer yet); only oauth.allowed_cors_origins is consumed"
                    ),
                });
            }
        }
    }

    if let Some(mail) = value.get("mail").and_then(Value::as_table) {
        if mail.contains_key("password") {
            warnings.push(ConfigWarning {
                field_path: "mail.password".to_string(),
                message:
                    "mail.password is deprecated; use mail.password_ref for vault-backed SMTP credentials"
                        .to_string(),
            });
        }

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

/// Reject any field that is not in the `known_fields` whitelist, returning a
/// hard error instead of a warning. This is the strict counterpart to
/// `unknown_fields` and is applied during config loading so that typos and
/// obsolete keys fail fast instead of being silently dropped by serde.
///
/// `[oauth]` is now a recognized section (`OAuthConfig`): `allowed_cors_origins`
/// is consumed, while the legacy keys (`campsite_api_domain`,
/// `tinyship_api_domain`, `api_store_backend`) are whitelisted-but-ignored, so
/// the section is validated like any other rather than skipped.
pub fn reject_unknown_fields(value: &Value) -> Result<(), MegaError> {
    let mut errors = Vec::new();

    if let Some(table) = value.as_table() {
        collect_unknown_field_errors("", "", table, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(MegaError::Other(format!(
            "config contains unrecognized fields: {}",
            errors.join("; ")
        )))
    }
}

fn collect_unknown_field_errors(
    schema_path: &str,
    display_path: &str,
    table: &toml::Table,
    errors: &mut Vec<String>,
) {
    let Some(allowed_fields) = known_fields(schema_path) else {
        return;
    };

    for (field, value) in table {
        let schema_field_path = join_field_path(schema_path, field);
        let display_field_path = join_field_path(display_path, field);

        if !allowed_fields.contains(&field.as_str()) {
            errors.push(format!("{display_field_path} is not recognized by Config"));
            continue;
        }

        if known_fields(&schema_field_path).is_some() {
            match value {
                Value::Table(child_table) => collect_unknown_field_errors(
                    &schema_field_path,
                    &display_field_path,
                    child_table,
                    errors,
                ),
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        if let Some(item_table) = item.as_table() {
                            collect_unknown_field_errors(
                                &schema_field_path,
                                &format!("{display_field_path}[{index}]"),
                                item_table,
                                errors,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn unconsumed_environment_fields_from_keys<I, K>(
    keys: I,
) -> Vec<EnvironmentConfigWarning>
where
    I: IntoIterator<Item = K>,
    K: AsRef<OsStr>,
{
    unconsumed_environment_fields_from_paths(&environment_field_paths_from_keys(keys))
}

fn unconsumed_environment_fields_from_paths(
    env_field_paths: &[(String, String)],
) -> Vec<EnvironmentConfigWarning> {
    let mut warnings = env_field_paths
        .iter()
        .filter_map(|(variable, field_path)| environment_warning_for(variable, field_path))
        .collect::<Vec<_>>();
    warnings.sort_by(|left, right| left.variable.cmp(&right.variable));
    warnings
}

fn environment_field_paths_from_keys<I, K>(keys: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = K>,
    K: AsRef<OsStr>,
{
    keys.into_iter()
        .filter_map(|key| {
            let variable = key.as_ref().to_str()?.to_string();
            let field_path = env_key_to_field_path(&variable)?;
            Some((variable, field_path))
        })
        .collect()
}

fn source_overrides(
    base_value: &Option<(PathBuf, Value)>,
    profile_value: &Option<(PathBuf, Value)>,
    env_field_paths: &[(String, String)],
) -> Vec<ConfigSourceOverride> {
    let base_fields = source_field_paths(base_value);
    let profile_fields = source_field_paths(profile_value);
    let mut overrides = Vec::new();

    if let (Some((base_path, _)), Some((profile_path, _))) = (base_value, profile_value) {
        let source = file_source_label("profile file", profile_path);
        let overridden_source = file_source_label("base file", base_path);
        for field_path in profile_fields.intersection(&base_fields) {
            overrides.push(source_override(field_path, &source, &overridden_source));
        }
    }

    for (variable, field_path) in env_field_paths {
        if environment_field_is_ignored(field_path) || !is_known_field_path(field_path) {
            continue;
        }

        let overridden_source = if profile_fields.contains(field_path) {
            profile_value
                .as_ref()
                .map(|(path, _)| file_source_label("profile file", path))
        } else if base_fields.contains(field_path) {
            base_value
                .as_ref()
                .map(|(path, _)| file_source_label("base file", path))
        } else {
            None
        };

        if let Some(overridden_source) = overridden_source {
            let source = format!("environment variable {variable}");
            overrides.push(source_override(field_path, &source, &overridden_source));
        }
    }

    overrides.sort_by(|left, right| {
        left.field_path
            .cmp(&right.field_path)
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.overridden_source.cmp(&right.overridden_source))
    });
    overrides
}

fn source_fields(
    base_value: &Option<(PathBuf, Value)>,
    profile_value: &Option<(PathBuf, Value)>,
    env_field_paths: &[(String, String)],
) -> Vec<ConfigSourceField> {
    let mut fields = Vec::new();

    if let Some((base_path, _)) = base_value {
        let source = file_source_label("base file", base_path);
        fields.extend(
            source_field_paths(base_value)
                .into_iter()
                .map(|field_path| source_field(&field_path, &source)),
        );
    }

    if let Some((profile_path, _)) = profile_value {
        let source = file_source_label("profile file", profile_path);
        fields.extend(
            source_field_paths(profile_value)
                .into_iter()
                .map(|field_path| source_field(&field_path, &source)),
        );
    }

    fields.extend(env_field_paths.iter().filter_map(|(variable, field_path)| {
        if environment_field_is_ignored(field_path) || !is_known_field_path(field_path) {
            return None;
        }

        Some(source_field(
            field_path,
            &format!("environment variable {variable}"),
        ))
    }));

    fields.sort_by(|left, right| {
        left.field_path
            .cmp(&right.field_path)
            .then_with(|| left.source.cmp(&right.source))
    });
    fields
        .dedup_by(|left, right| left.field_path == right.field_path && left.source == right.source);
    fields
}

fn source_field_paths(source: &Option<(PathBuf, Value)>) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    if let Some((_, value)) = source {
        collect_value_field_paths("", value, &mut fields);
    }
    fields
}

fn collect_value_field_paths(prefix: &str, value: &Value, fields: &mut BTreeSet<String>) {
    match value {
        Value::Table(table) => {
            for (field, child) in table {
                collect_value_field_paths(&join_field_path(prefix, field), child, fields);
            }
        }
        Value::Array(items) => {
            if is_effective_source_field_path(prefix) {
                fields.insert(prefix.to_string());
            }
            for (index, item) in items.iter().enumerate() {
                if let Value::Table(item_table) = item {
                    for (field, child) in item_table {
                        collect_value_field_paths(
                            &format!("{prefix}[{index}].{field}"),
                            child,
                            fields,
                        );
                    }
                }
            }
        }
        _ => {
            if is_effective_source_field_path(prefix) {
                fields.insert(prefix.to_string());
            }
        }
    }
}

fn is_effective_source_field_path(field_path: &str) -> bool {
    !field_path.is_empty()
        && is_known_field_path(field_path)
        && !matches!(field_path, "mail.smtp_tls" | "mail.tls")
        && field_path != "oauth"
        // oauth.allowed_cors_origins is consumed; the legacy oauth keys are not.
        && !matches!(
            field_path,
            "oauth.campsite_api_domain" | "oauth.tinyship_api_domain" | "oauth.api_store_backend"
        )
}

fn file_source_label(kind: &str, path: &Path) -> String {
    format!("{kind} {}", path.display())
}

fn source_override(
    field_path: &str,
    source: &str,
    overridden_source: &str,
) -> ConfigSourceOverride {
    ConfigSourceOverride {
        field_path: field_path.to_string(),
        source: source.to_string(),
        overridden_source: overridden_source.to_string(),
        message: format!(
            "{source} overrides {overridden_source} for {field_path}; {}; suggested fix: if this override is unintended, {}; otherwise remove the duplicate lower-precedence setting from {overridden_source}",
            source_override_note(field_path),
            remove_source_field_action(source, field_path)
        ),
    }
}

fn source_field(field_path: &str, source: &str) -> ConfigSourceField {
    ConfigSourceField {
        field_path: field_path.to_string(),
        source: source.to_string(),
        message: format!(
            "{field_path} is set by {source}; {}. To change it, {}",
            source_value_omission_note(field_path),
            change_source_field_action(source, field_path)
        ),
    }
}

fn source_value_omission_note(field_path: &str) -> &'static str {
    if is_sensitive_source_field_path(field_path) {
        return "sensitive values are omitted; keep real credentials in deployment/environment secrets or approved SecretRef fields";
    }

    "values are omitted"
}

fn source_override_note(field_path: &str) -> String {
    let mut note = source_value_omission_note(field_path).to_string();
    if is_array_source_field_path(field_path) {
        note.push_str("; arrays replace lower-precedence values rather than append");
    }

    note
}

fn is_array_source_field_path(field_path: &str) -> bool {
    matches!(
        field_path,
        "monorepo.admin" | "monorepo.root_dirs" | "sidebar.default_items"
    ) || field_path.starts_with("monorepo.admin[")
        || field_path.starts_with("monorepo.root_dirs[")
        || field_path.starts_with("sidebar.default_items[")
}

fn is_sensitive_source_field_path(field_path: &str) -> bool {
    matches!(
        field_path,
        "database.db_url"
            | "redis.url"
            | "orion_server.db_url"
            | "object_storage.s3.access_key_id"
            | "object_storage.s3.secret_access_key"
            | "object_storage.s3.endpoint_url"
            | "mail.password"
            | "mail.password_ref"
            | "notification.slack.webhook_url_ref"
            | "notification.webhook.token_ref"
    )
}

fn remove_source_field_action(source: &str, field_path: &str) -> String {
    if let Some(variable) = source.strip_prefix("environment variable ") {
        return format!("unset {variable}");
    }

    format!("remove {field_path} from {source}")
}

fn change_source_field_action(source: &str, field_path: &str) -> String {
    if let Some(variable) = source.strip_prefix("environment variable ") {
        return format!("update {variable} or unset it to fall back to lower-precedence sources");
    }

    format!("edit {field_path} in {source} or use a higher-precedence profile/env override")
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
    let message = if matches!(
        field_path,
        "oauth.campsite_api_domain" | "oauth.tinyship_api_domain" | "oauth.api_store_backend"
    ) {
        format!(
            "{variable} maps to {field_path}, which is currently ignored (no consumer yet); only oauth.allowed_cors_origins is consumed"
        )
    } else if matches!(field_path, "mail.smtp_tls" | "mail.tls") {
        format!(
            "{variable} maps to {field_path}, which is ignored by MailConfig; use MEGA_MAIL__STARTTLS for STARTTLS behavior"
        )
    } else if field_path == "mail.password" {
        format!(
            "{variable} maps to {field_path}, which is deprecated; use MEGA_MAIL__PASSWORD_REF with a vault-backed SecretRef"
        )
    } else if !is_known_field_path(field_path) {
        format!(
            "{variable} maps to {field_path}, which is not recognized by Config and will be ignored; remove the variable or use a supported MEGA_* field path"
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

fn environment_field_is_ignored(field_path: &str) -> bool {
    // oauth.allowed_cors_origins is consumed; only the legacy oauth keys are ignored.
    matches!(
        field_path,
        "oauth.campsite_api_domain" | "oauth.tinyship_api_domain" | "oauth.api_store_backend"
    ) || matches!(field_path, "mail.smtp_tls" | "mail.tls")
}

fn is_known_field_path(field_path: &str) -> bool {
    let mut schema_path = String::new();
    let mut parts = field_path.split('.').peekable();

    while let Some(field) = parts.next() {
        // Strip array indices like `default_items[0]` down to `default_items`
        // so that per-element source diagnostics can be schema-checked.
        let field = field.split('[').next().unwrap_or(field);
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
                    "{display_field_path} is not recognized by Config and will be ignored; remove the field or add it to Config before relying on it"
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
            "notification",
            "vault",
            "oauth",
            "chat",
        ]),
        "chat" => Some(&[
            "attachment_allowed_mime_types",
            "open_graph_fetch_enabled",
            "open_graph_fetch_timeout_ms",
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
            "provider",
            "smtp_host",
            "smtp_port",
            "username",
            "password",
            "password_ref",
            "from",
            "starttls",
            "dispatcher_batch_size",
            "dispatcher_max_in_flight",
            "retry_max_attempts",
            "retry_backoff_base_secs",
            "retry_backoff_max_secs",
            "attachment_prune_enabled",
            "attachment_prune_interval_secs",
            "attachment_retention_days",
            "attachment_prune_statuses",
            "template_default_locale",
            "template_dir",
            "smtp_tls",
            "tls",
        ]),
        "notification" => Some(&[
            "enabled",
            "default_delivery_mode",
            "default_locale",
            "slack",
            "webhook",
        ]),
        "notification.slack" => Some(&["enabled", "webhook_url_ref"]),
        "notification.webhook" => Some(&["enabled", "url", "token_ref"]),
        "vault" => Some(&["audit"]),
        "vault.audit" => Some(&["enabled", "sink", "file_path", "fail_closed"]),
        // `allowed_cors_origins` is the only strongly-typed/consumed key
        // (OAuthConfig). The remaining keys are legacy compatibility fields:
        // whitelisted so the sample config loads, but ignored at deserialize time
        // until a real consumer exists.
        "oauth" => Some(&[
            "allowed_cors_origins",
            "campsite_api_domain",
            "tinyship_api_domain",
            "api_store_backend",
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use orbit_api::factory::{GcsConfig, LocalConfig, ObjectStorageBackend, S3Config};

    use super::*;
    use crate::config::{
        secret::SecretRef, template::config_init_template, testing::isolated_config,
    };

    fn valid_config() -> Config {
        isolated_config(std::env::temp_dir().join("monoengine-config-validate-tests"))
    }

    #[test]
    fn config_validate_accepts_default_isolated_test_config() {
        valid_config()
            .validate()
            .expect("isolated test config should validate");
    }

    #[test]
    fn config_validate_rejects_non_postgres_database_type() {
        let mut config = valid_config();
        config.database.db_type = "mysql".to_string();

        let err = config.validate().expect_err("database type should fail");

        assert!(err.to_string().contains("database.db_type"));
    }

    #[test]
    fn config_validate_accepts_default_notification_config() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig::default());

        config
            .validate()
            .expect("default notification config should validate");
    }

    #[test]
    fn config_validate_accepts_default_chat_config() {
        let mut config = valid_config();
        config.chat = Some(crate::config::ChatConfig::default());

        config
            .validate()
            .expect("default chat config should validate");
    }

    #[test]
    fn config_validate_accepts_chat_mime_wildcards() {
        let mut config = valid_config();
        config.chat = Some(crate::config::ChatConfig {
            attachment_allowed_mime_types: vec![
                "image/*".to_string(),
                "application/pdf".to_string(),
            ],
            ..Default::default()
        });

        config
            .validate()
            .expect("wildcard MIME allowlist should validate");
    }

    #[test]
    fn config_validate_rejects_invalid_chat_mime_patterns() {
        let mut config = valid_config();
        config.chat = Some(crate::config::ChatConfig {
            attachment_allowed_mime_types: vec!["*/*".to_string(), "bad".to_string()],
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("invalid MIME patterns should fail");

        assert!(
            err.to_string()
                .contains("chat.attachment_allowed_mime_types")
        );
    }

    #[test]
    fn config_validate_rejects_unsupported_notification_delivery_mode() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            default_delivery_mode: "carrier-pigeon".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("unsupported delivery mode should fail");

        assert!(
            err.to_string()
                .contains("notification.default_delivery_mode")
        );
    }

    #[test]
    fn config_validate_rejects_empty_notification_locale() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            default_locale: "   ".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("empty notification locale should fail");

        assert!(err.to_string().contains("notification.default_locale"));
    }

    #[test]
    fn config_validate_rejects_enabled_slack_without_webhook_url_ref() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            slack: Some(crate::config::SlackConfig {
                enabled: true,
                webhook_url_ref: None,
            }),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("enabled slack without webhook_url_ref should fail");
        assert!(
            err.to_string()
                .contains("notification.slack.webhook_url_ref")
        );
    }

    #[test]
    fn config_validate_rejects_slack_secret_ref_outside_namespace_without_leaking_ref() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            slack: Some(crate::config::SlackConfig {
                enabled: true,
                webhook_url_ref: Some(
                    crate::config::secret::SecretRef::parse(
                        "vault://secret/config/prod/mail/password#value",
                    )
                    .unwrap(),
                ),
            }),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("slack secret ref outside namespace should fail");
        let message = err.to_string();
        assert!(message.contains("notification.slack.webhook_url_ref"));
        assert!(message.contains("notification/slack/webhook_url"));
        // The SecretRef value must not leak.
        assert!(!message.contains("config/prod/mail/password"));
    }

    #[test]
    fn config_validate_accepts_enabled_slack_with_correct_namespace() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            slack: Some(crate::config::SlackConfig {
                enabled: true,
                webhook_url_ref: Some(
                    crate::config::secret::SecretRef::parse(
                        "vault://secret/config/prod/notification/slack/webhook_url#value",
                    )
                    .unwrap(),
                ),
            }),
            ..Default::default()
        });

        config
            .validate()
            .expect("slack with correct namespace should validate");
    }

    #[test]
    fn config_validate_rejects_enabled_webhook_without_url() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            webhook: Some(crate::config::WebhookConfig {
                enabled: true,
                url: "   ".to_string(),
                token_ref: None,
            }),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("enabled webhook without url should fail");
        assert!(err.to_string().contains("notification.webhook.url"));
    }

    #[test]
    fn config_validate_accepts_webhook_with_token_ref_in_namespace() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            webhook: Some(crate::config::WebhookConfig {
                enabled: true,
                url: "https://hooks.example.com/notify".to_string(),
                token_ref: Some(
                    crate::config::secret::SecretRef::parse(
                        "vault://secret/config/prod/notification/webhook/token#value",
                    )
                    .unwrap(),
                ),
            }),
            ..Default::default()
        });

        config
            .validate()
            .expect("webhook with correct namespace should validate");
    }

    #[test]
    fn config_validate_rejects_unsupported_vault_audit_sink() {
        let mut config = valid_config();
        config.vault = Some(crate::config::VaultConfig {
            audit: crate::config::VaultAuditConfig {
                sink: "syslog".to_string(),
                ..Default::default()
            },
        });

        let err = config
            .validate()
            .expect_err("unsupported vault audit sink should fail");
        assert!(err.to_string().contains("vault.audit.sink"));
    }

    #[test]
    fn config_validate_rejects_file_audit_sink_without_path() {
        let mut config = valid_config();
        config.vault = Some(crate::config::VaultConfig {
            audit: crate::config::VaultAuditConfig {
                sink: "file".to_string(),
                file_path: None,
                ..Default::default()
            },
        });

        let err = config
            .validate()
            .expect_err("file audit sink without path should fail");
        assert!(err.to_string().contains("vault.audit.file_path"));
    }

    #[test]
    fn config_validate_accepts_file_audit_sink_with_path() {
        let mut config = valid_config();
        config.vault = Some(crate::config::VaultConfig {
            audit: crate::config::VaultAuditConfig {
                sink: "file".to_string(),
                file_path: Some(std::path::PathBuf::from(
                    "/var/log/monoengine/vault-audit.jsonl",
                )),
                fail_closed: true,
                ..Default::default()
            },
        });

        config
            .validate()
            .expect("file audit sink with a path should validate");
    }

    #[test]
    fn config_validate_accepts_oauth_cors_origins() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec![
                "http://localhost:3000".to_string(),
                "https://app.example.com".to_string(),
            ],
        });

        config
            .validate()
            .expect("valid oauth cors origins should validate");
    }

    #[test]
    fn config_validate_rejects_oauth_origin_with_whitespace() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec!["http://has space.example.com".to_string()],
        });

        let err = config
            .validate()
            .expect_err("oauth origin with whitespace should fail");
        assert!(err.to_string().contains("oauth.allowed_cors_origins"));
    }

    #[test]
    fn config_validate_rejects_empty_oauth_origin() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec!["  ".to_string()],
        });

        let err = config
            .validate()
            .expect_err("empty oauth origin should fail");
        assert!(err.to_string().contains("oauth.allowed_cors_origins"));
    }

    #[test]
    fn config_validate_rejects_oauth_origin_with_path() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec!["https://app.example.com/callback".to_string()],
        });

        let err = config
            .validate()
            .expect_err("oauth origin with a path should fail");
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn config_validate_rejects_oauth_origin_with_non_http_scheme() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec!["ftp://app.example.com".to_string()],
        });

        let err = config
            .validate()
            .expect_err("oauth origin with non-http scheme should fail");
        assert!(err.to_string().contains("scheme"));
    }

    #[test]
    fn config_validate_rejects_oauth_origin_with_query_or_fragment() {
        for bad in [
            "https://app.example.com?x=1",
            "https://app.example.com#frag",
        ] {
            let mut config = valid_config();
            config.oauth = Some(crate::config::OAuthConfig {
                allowed_cors_origins: vec![bad.to_string()],
            });
            let err = config
                .validate()
                .expect_err("oauth origin with query/fragment should fail")
                .to_string();
            assert!(
                err.contains("path, query, or fragment"),
                "unexpected error for {bad}: {err}"
            );
        }
    }

    #[test]
    fn config_validate_rejects_oauth_origin_without_scheme() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            allowed_cors_origins: vec!["app.example.com".to_string()],
        });

        let err = config
            .validate()
            .expect_err("oauth origin without scheme should fail");
        assert!(err.to_string().contains("scheme://host"));
    }

    #[test]
    fn config_validate_rejects_non_postgres_database_url_scheme() {
        let mut config = valid_config();
        config.database.db_url = "mysql://mono:mono@localhost:3306/mono".to_string();

        let err = config
            .validate()
            .expect_err("database URL scheme should fail");

        assert!(err.to_string().contains("database.db_url scheme"));
    }

    #[test]
    fn config_validate_rejects_invalid_database_pool_settings() {
        let mut config = valid_config();
        config.database.max_connection = 0;
        let err = config
            .validate()
            .expect_err("zero max connections should fail");
        assert!(err.to_string().contains("database.max_connection"));

        let mut config = valid_config();
        config.database.min_connection = config.database.max_connection + 1;
        let err = config
            .validate()
            .expect_err("min connections above max should fail");
        assert!(err.to_string().contains("database.min_connection"));

        let mut config = valid_config();
        config.database.acquire_timeout = 0;
        let err = config
            .validate()
            .expect_err("zero acquire timeout should fail");
        assert!(err.to_string().contains("database.acquire_timeout"));

        let mut config = valid_config();
        config.database.connect_timeout = 0;
        let err = config
            .validate()
            .expect_err("zero connect timeout should fail");
        assert!(err.to_string().contains("database.connect_timeout"));
    }

    #[test]
    fn config_validate_rejects_invalid_monorepo_settings() {
        let mut config = valid_config();
        config.monorepo.import_dir = PathBuf::new();
        let err = config
            .validate()
            .expect_err("empty monorepo import dir should fail");
        assert!(err.to_string().contains("monorepo.import_dir"));

        let mut config = valid_config();
        config.monorepo.root_dirs.clear();
        let err = config
            .validate()
            .expect_err("empty monorepo root dirs should fail");
        assert!(err.to_string().contains("monorepo.root_dirs"));

        let mut config = valid_config();
        config.monorepo.root_dirs = vec!["".to_string()];
        let err = config
            .validate()
            .expect_err("blank monorepo root dir should fail");
        assert!(err.to_string().contains("monorepo.root_dirs[0]"));

        let mut config = valid_config();
        config.monorepo.admin = vec!["".to_string()];
        let err = config
            .validate()
            .expect_err("blank monorepo admin should fail");
        assert!(err.to_string().contains("monorepo.admin[0]"));

        let mut config = valid_config();
        config.monorepo.rename.similarity_threshold = 101;
        let err = config
            .validate()
            .expect_err("rename similarity threshold above 100 should fail");
        assert!(
            err.to_string()
                .contains("monorepo.rename.similarity_threshold")
        );
    }

    #[test]
    fn config_validate_rejects_invalid_pack_settings() {
        let mut config = valid_config();
        config.pack.pack_decode_mem_size = "definitely-not-a-size".to_string();
        let err = config
            .validate()
            .expect_err("invalid pack decode memory size should fail");
        assert!(err.to_string().contains("pack.pack_decode_mem_size"));

        let mut config = valid_config();
        config.pack.pack_decode_disk_size = "0".to_string();
        let err = config
            .validate()
            .expect_err("zero pack decode disk size should fail");
        assert!(err.to_string().contains("pack.pack_decode_disk_size"));

        let mut config = valid_config();
        config.pack.pack_decode_cache_path = PathBuf::new();
        let err = config
            .validate()
            .expect_err("empty pack decode cache path should fail");
        assert!(err.to_string().contains("pack.pack_decode_cache_path"));

        let mut config = valid_config();
        config.pack.channel_message_size = 0;
        let err = config
            .validate()
            .expect_err("zero pack channel message size should fail");
        assert!(err.to_string().contains("pack.channel_message_size"));
    }

    #[test]
    fn config_validate_rejects_invalid_blame_settings() {
        let mut config = valid_config();
        config.blame.max_lines_threshold = 0;
        let err = config
            .validate()
            .expect_err("zero blame line threshold should fail");
        assert!(err.to_string().contains("blame.max_lines_threshold"));

        let mut config = valid_config();
        config.blame.max_size_threshold = "not-a-size".to_string();
        let err = config
            .validate()
            .expect_err("invalid blame size threshold should fail");
        assert!(err.to_string().contains("blame.max_size_threshold"));

        let mut config = valid_config();
        config.blame.default_chunk_size = 0;
        let err = config
            .validate()
            .expect_err("zero blame chunk size should fail");
        assert!(err.to_string().contains("blame.default_chunk_size"));

        let mut config = valid_config();
        config.blame.max_commits_in_memory = 0;
        let err = config
            .validate()
            .expect_err("zero blame commit limit should fail");
        assert!(err.to_string().contains("blame.max_commits_in_memory"));
    }

    #[test]
    fn config_validate_rejects_unknown_log_level() {
        let mut config = valid_config();
        config.log.level = "verbose".to_string();

        let err = config.validate().expect_err("log level should fail");

        assert!(err.to_string().contains("log.level"));
    }

    #[test]
    fn config_validate_rejects_invalid_lfs_http_url() {
        let mut config = valid_config();
        config.lfs.ssh.http_url = "ftp://localhost:8000".to_string();

        let err = config.validate().expect_err("lfs url should fail");

        assert!(err.to_string().contains("lfs.ssh.http_url"));
    }

    #[test]
    fn config_validate_rejects_enabled_build_without_orion_url() {
        let mut config = valid_config();
        config.build.enable_build = true;
        config.build.orion_server = String::new();

        let err = config
            .validate()
            .expect_err("enabled build without Orion URL should fail");

        assert!(err.to_string().contains("build.orion_server"));
    }

    #[test]
    fn config_validate_rejects_invalid_redis_url_scheme() {
        let mut config = valid_config();
        config.redis.url = "http://localhost:6379".to_string();

        let err = config.validate().expect_err("redis scheme should fail");

        assert!(err.to_string().contains("redis.url scheme"));
    }

    #[test]
    fn config_validate_accepts_redis_url_secret_ref_in_namespace() {
        let mut config = valid_config();
        config.redis.url = "vault://secret/config/test/redis/url#value".to_string();

        config
            .validate()
            .expect("redis.url secret ref in namespace should validate");
    }

    #[test]
    fn config_validate_rejects_redis_url_secret_ref_outside_namespace() {
        let mut config = valid_config();
        config.redis.url = "vault://secret/config/test/mail/password#value".to_string();

        let err = config
            .validate()
            .expect_err("redis.url secret ref outside redis/url namespace should fail");

        assert!(err.to_string().contains("redis.url"));
        assert!(err.to_string().contains("redis/url"));
    }

    #[test]
    fn config_validate_rejects_invalid_orion_server_config() {
        let mut config = valid_config();
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
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: Some(crate::config::secret::SecretString::new("plain")),
            password_ref: Some(
                SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap(),
            ),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        };

        let err = mail_config
            .validate()
            .expect_err("mutual exclusion should fail");

        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn mail_validate_rejects_password_ref_outside_mail_namespace_without_leaking_ref() {
        let mail_config = MailConfig {
            enabled: false,
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: Some(
                SecretRef::parse("vault://secret/config/prod/database/password#value").unwrap(),
            ),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        };

        let err = mail_config
            .validate()
            .expect_err("wrong SecretRef namespace should fail");
        let message = err.to_string();

        assert!(message.contains("mail.password_ref"));
        assert!(message.contains("vault://secret/config/<profile>/mail/password#<field>"));
        assert!(message.contains("value is redacted"));
        assert!(!message.contains("config/prod/database/password"));
        assert!(!message.contains("#value"));
    }

    #[test]
    fn mail_validate_rejects_missing_enabled_smtp_host() {
        let mail_config = MailConfig {
            enabled: true,
            provider: MailProvider::Smtp,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        };

        let err = mail_config.validate().expect_err("smtp host should fail");

        assert!(err.to_string().contains("mail.smtp_host"));
    }

    #[test]
    fn mail_validate_accepts_enabled_console_without_smtp_fields() {
        let mail_config = MailConfig {
            enabled: true,
            provider: MailProvider::Console,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: String::new(),
            starttls: true,
            ..Default::default()
        };

        mail_config
            .validate()
            .expect("console provider should pass");
    }

    #[test]
    fn mail_validate_rejects_console_provider_credentials() {
        let mail_config = MailConfig {
            enabled: true,
            provider: MailProvider::Console,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: Some(
                SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap(),
            ),
            from: String::new(),
            starttls: true,
            ..Default::default()
        };

        let err = mail_config
            .validate()
            .expect_err("console provider must not accept smtp credentials");

        assert!(err.to_string().contains("mail.provider is smtp"));
    }

    #[test]
    fn mail_validate_rejects_zero_dispatcher_limits() {
        let mail_config = MailConfig {
            dispatcher_batch_size: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero batch size should fail");
        assert!(err.to_string().contains("mail.dispatcher_batch_size"));

        let mail_config = MailConfig {
            dispatcher_max_in_flight: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero max in-flight should fail");
        assert!(err.to_string().contains("mail.dispatcher_max_in_flight"));
    }

    #[test]
    fn mail_validate_rejects_invalid_retry_policy() {
        let mail_config = MailConfig {
            retry_max_attempts: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero retry max attempts should fail");
        assert!(err.to_string().contains("mail.retry_max_attempts"));

        let mail_config = MailConfig {
            retry_backoff_base_secs: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero retry backoff base should fail");
        assert!(err.to_string().contains("mail.retry_backoff_base_secs"));

        let mail_config = MailConfig {
            retry_backoff_max_secs: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero retry backoff max should fail");
        assert!(err.to_string().contains("mail.retry_backoff_max_secs"));

        let mail_config = MailConfig {
            retry_backoff_base_secs: 60,
            retry_backoff_max_secs: 30,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("retry backoff max below base should fail");
        assert!(err.to_string().contains("mail.retry_backoff_max_secs"));
    }

    #[test]
    fn mail_validate_rejects_invalid_attachment_prune_policy() {
        let mail_config = MailConfig {
            attachment_prune_interval_secs: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero attachment prune interval should fail");
        assert!(
            err.to_string()
                .contains("mail.attachment_prune_interval_secs")
        );

        let mail_config = MailConfig {
            attachment_retention_days: 0,
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("zero attachment retention should fail");
        assert!(err.to_string().contains("mail.attachment_retention_days"));

        let mail_config = MailConfig {
            attachment_prune_statuses: Vec::new(),
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("empty attachment prune statuses should fail");
        assert!(err.to_string().contains("mail.attachment_prune_statuses"));

        let mail_config = MailConfig {
            attachment_prune_statuses: vec!["failed".to_string()],
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("unsafe attachment prune status should fail");
        assert!(err.to_string().contains("mail.attachment_prune_statuses"));

        let mail_config = MailConfig {
            attachment_prune_statuses: vec!["sent".to_string(), "sent".to_string()],
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("duplicate attachment prune status should fail");
        assert!(err.to_string().contains("mail.attachment_prune_statuses"));
    }

    #[test]
    fn mail_validate_rejects_invalid_template_settings() {
        let mail_config = MailConfig {
            template_default_locale: String::new(),
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("empty template default locale should fail");
        assert!(err.to_string().contains("mail.template_default_locale"));

        let mail_config = MailConfig {
            template_default_locale: "en US".to_string(),
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("unsupported template default locale should fail");
        assert!(err.to_string().contains("mail.template_default_locale"));

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let missing_dir = temp_dir.path().join("missing");
        let mail_config = MailConfig {
            template_dir: Some(missing_dir),
            ..Default::default()
        };
        let err = mail_config
            .validate()
            .expect_err("missing template dir should fail");
        assert!(err.to_string().contains("mail.template_dir"));
    }

    #[test]
    fn config_validate_rejects_invalid_buck_config() {
        let mut config = valid_config();
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
        let mut config = valid_config();
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
        let mut config = valid_config();
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
    fn config_validate_accepts_object_storage_secret_refs_under_required_namespace() {
        let mut config = valid_config();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "monoengine-test".to_string(),
                access_key_id: "vault://secret/config/prod/object_storage/access_key_id#value"
                    .to_string(),
                secret_access_key:
                    "vault://secret/config/prod/object_storage/secret_access_key#value".to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        config
            .validate()
            .expect("object storage SecretRefs under required namespace should validate");
    }

    #[test]
    fn config_validate_rejects_object_storage_secret_refs_outside_required_namespace() {
        let mut config = valid_config();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "monoengine-test".to_string(),
                access_key_id: "vault://secret/config/prod/object-storage/access#value".to_string(),
                secret_access_key: "secret".to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("object storage SecretRef outside required namespace should fail");
        let message = err.to_string();

        assert!(message.contains("object_storage.s3.access_key_id"));
        assert!(message.contains("value is redacted"));
        assert!(!message.contains("config/prod/object-storage/access"));
        assert!(!message.contains("#value"));
    }

    #[test]
    fn config_validate_rejects_object_storage_secret_access_key_outside_required_namespace() {
        let mut config = valid_config();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "monoengine-test".to_string(),
                access_key_id: "AKIA-example".to_string(),
                secret_access_key: "vault://secret/config/prod/mail/password#value".to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        let err = config
            .validate()
            .expect_err("object storage secret access key outside required namespace should fail");
        let message = err.to_string();

        assert!(message.contains("object_storage.s3.secret_access_key"));
        assert!(message.contains("value is redacted"));
        assert!(!message.contains("config/prod/mail/password"));
        assert!(!message.contains("#value"));
    }

    #[test]
    fn config_validate_rejects_s3_compatible_without_endpoint() {
        let mut config = valid_config();
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
        let mut config = valid_config();
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
        let mut config = valid_config();
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
    fn config_validate_rejects_invalid_artifact_gc_settings() {
        let mut config = valid_config();
        config.artifacts_gc.interval_secs = 0;
        let err = config
            .validate()
            .expect_err("zero artifact gc interval should fail");
        assert!(err.to_string().contains("artifacts_gc.interval_secs"));

        let mut config = valid_config();
        config.artifacts_gc.batch_limit = 0;
        let err = config
            .validate()
            .expect_err("zero artifact gc batch limit should fail");
        assert!(err.to_string().contains("artifacts_gc.batch_limit"));
    }

    #[test]
    fn config_validate_rejects_invalid_sidebar_items() {
        let mut config = valid_config();
        config.sidebar.default_items[0].public_id.clear();
        let err = config
            .validate()
            .expect_err("blank sidebar public_id should fail");
        assert!(
            err.to_string()
                .contains("sidebar.default_items[0].public_id")
        );

        let mut config = valid_config();
        config.sidebar.default_items[0].label.clear();
        let err = config
            .validate()
            .expect_err("blank sidebar label should fail");
        assert!(err.to_string().contains("sidebar.default_items[0].label"));

        let mut config = valid_config();
        config.sidebar.default_items[0].href.clear();
        let err = config
            .validate()
            .expect_err("blank sidebar href should fail");
        assert!(err.to_string().contains("sidebar.default_items[0].href"));

        let mut config = valid_config();
        let duplicate_id = config.sidebar.default_items[0].public_id.clone();
        config.sidebar.default_items[1].public_id = duplicate_id;
        let err = config
            .validate()
            .expect_err("duplicate sidebar public_id should fail");
        assert!(
            err.to_string()
                .contains("sidebar.default_items[1].public_id")
        );
        assert!(err.to_string().contains("unique"));
    }

    #[test]
    fn known_unconsumed_fields_warns_for_legacy_oauth_and_mail_tls_keys() {
        let value = toml::from_str::<Value>(
            r#"
            [oauth]
            allowed_cors_origins = ["http://app.example.com"]
            campsite_api_domain = "http://api.example.com"

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

        // allowed_cors_origins is now consumed (no warning); the legacy oauth key
        // and the ignored mail TLS keys still warn.
        assert!(fields.contains(&"oauth.campsite_api_domain"));
        assert!(fields.contains(&"mail.smtp_tls"));
        assert!(fields.contains(&"mail.tls"));
        assert!(!fields.contains(&"oauth.allowed_cors_origins"));
        assert!(!fields.contains(&"oauth"));
    }

    #[test]
    fn known_unconsumed_fields_warns_for_deprecated_mail_password_without_value() {
        let content = format!(
            r#"
            [mail]
            {} = "plain-text-password"
            "#,
            "password"
        );
        let value = toml::from_str::<Value>(&content).unwrap();

        let warnings = known_unconsumed_fields(&value);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].field_path, "mail.password");
        assert!(warnings[0].message.contains("mail.password_ref"));
        assert!(!warnings[0].message.contains("plain-text-password"));
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
    fn known_unconsumed_file_fields_include_source_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &profile_path,
            r#"
            [mail]
            smtp_tls = false

            [database]
            typo = true
            "#,
        )
        .expect("write profile config");

        let warnings =
            known_unconsumed_file_fields(&profile_path).expect("profile diagnostics should parse");

        assert_eq!(warnings.len(), 2);
        assert!(
            warnings
                .iter()
                .all(|warning| warning.source_path == profile_path)
        );
        assert!(warnings.iter().any(|warning| {
            warning.field_path == "mail.smtp_tls" && warning.message.contains("mail.starttls")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.field_path == "database.typo" && warning.message.contains("not recognized")
        }));
    }

    #[test]
    fn source_diagnostics_collects_base_profile_and_env_warnings() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
            unknown_root = true
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
            [mail]
            smtp_tls = false
            "#,
        )
        .expect("write profile config");

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            [
                "MEGA_DATABASE__DB_URL",
                "MEGA_UNKNOWN__VALUE",
                "MEGA_MAIL__PASSWORD",
                "MEGA_MAIL__TLS",
            ],
        )
        .expect("diagnostics should collect");

        assert_eq!(diagnostics.file_warnings.len(), 2);
        assert_eq!(diagnostics.environment_warnings.len(), 3);
        assert_eq!(diagnostics.warning_count(), 5);
        assert!(diagnostics.has_warnings());
        assert!(!diagnostics.is_empty());
        assert!(diagnostics.file_warnings.iter().any(|warning| {
            warning.source_path == config_path && warning.field_path == "unknown_root"
        }));
        assert!(diagnostics.file_warnings.iter().any(|warning| {
            warning.source_path == profile_path && warning.field_path == "mail.smtp_tls"
        }));
        assert!(diagnostics.environment_warnings.iter().any(|warning| {
            warning.variable == "MEGA_UNKNOWN__VALUE" && warning.field_path == "unknown.value"
        }));
        assert!(diagnostics.environment_warnings.iter().any(|warning| {
            warning.variable == "MEGA_MAIL__TLS" && warning.field_path == "mail.tls"
        }));
        assert!(diagnostics.environment_warnings.iter().any(|warning| {
            warning.variable == "MEGA_MAIL__PASSWORD"
                && warning.field_path == "mail.password"
                && warning.message.contains("MEGA_MAIL__PASSWORD_REF")
        }));
    }

    #[test]
    fn source_diagnostics_collects_cross_source_overrides_without_values() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
            [log]
            level = "info"

            [database]
            db_url = "postgres://localhost:5432/base"

            [monorepo]
            root_dirs = ["base-root"]

            [mail]
            {} = "plain-text-password"
            "#,
                "password"
            ),
        )
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

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            [
                "MEGA_LOG__LEVEL",
                "MEGA_DATABASE__DB_URL",
                "MEGA_MAIL__PASSWORD",
                "MEGA_MAIL__TLS",
            ],
        )
        .expect("diagnostics should collect");
        let overrides = diagnostics
            .source_overrides
            .iter()
            .map(|source_override| source_override.message.as_str())
            .collect::<Vec<_>>();
        let override_text = overrides.join("\n");

        assert_eq!(diagnostics.source_overrides.len(), 5);
        assert!(overrides.iter().any(|message| {
            message.contains("profile file")
                && message.contains("base file")
                && message.contains("log.level")
        }));
        assert!(overrides.iter().any(|message| {
            message.contains("profile file")
                && message.contains("base file")
                && message.contains("monorepo.root_dirs")
                && message.contains("arrays replace lower-precedence values rather than append")
        }));
        assert!(overrides.iter().any(|message| {
            message.contains("MEGA_LOG__LEVEL")
                && message.contains("profile file")
                && message.contains("log.level")
        }));
        assert!(overrides.iter().any(|message| {
            message.contains("MEGA_DATABASE__DB_URL")
                && message.contains("base file")
                && message.contains("database.db_url")
        }));
        assert!(overrides.iter().any(|message| {
            message.contains("MEGA_MAIL__PASSWORD")
                && message.contains("base file")
                && message.contains("mail.password")
        }));
        assert!(override_text.contains("suggested fix"));
        assert!(override_text.contains("sensitive values are omitted"));
        assert!(override_text.contains("deployment/environment secrets"));
        assert!(override_text.contains("unset MEGA_LOG__LEVEL"));
        assert!(override_text.contains("remove log.level from profile file"));
        assert!(override_text.contains("duplicate lower-precedence setting"));
        assert!(!override_text.contains("postgres://localhost"));
        assert!(!override_text.contains("plain-text-password"));
        assert!(!override_text.contains("debug"));
        assert!(!override_text.contains("info"));
        assert!(!override_text.contains("base-root"));
        assert!(!override_text.contains("profile-root"));
        assert!(
            diagnostics
                .environment_warnings
                .iter()
                .any(|warning| warning.variable == "MEGA_MAIL__TLS")
        );
    }

    #[test]
    fn source_diagnostics_collects_field_source_graph_without_values() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
            [log]
            level = "info"

            [database]
            db_url = "postgres://localhost:5432/base"
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r##"
            [log]
            level = "debug"

            [mail]
            password_ref = "vault://secret/config/prod/mail/password#value"
            "##,
        )
        .expect("write profile config");

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            [
                "MEGA_LOG__LEVEL",
                "MEGA_MAIL__PASSWORD",
                "MEGA_UNKNOWN__VALUE",
            ],
        )
        .expect("diagnostics should collect");
        let source_fields = diagnostics
            .source_fields
            .iter()
            .map(|source_field| source_field.message.as_str())
            .collect::<Vec<_>>();
        let source_text = source_fields.join("\n");

        assert_eq!(diagnostics.source_fields.len(), 6);
        assert!(source_fields.iter().any(|message| {
            message.contains("log.level")
                && message.contains("base file")
                && message.contains(&config_path.display().to_string())
        }));
        assert!(source_fields.iter().any(|message| {
            message.contains("log.level")
                && message.contains("profile file")
                && message.contains(&profile_path.display().to_string())
        }));
        assert!(source_fields.iter().any(|message| {
            message.contains("mail.password_ref")
                && message.contains("profile file")
                && message.contains(&profile_path.display().to_string())
        }));
        assert!(source_fields.iter().any(|message| {
            message.contains("log.level") && message.contains("MEGA_LOG__LEVEL")
        }));
        assert!(source_fields
            .iter()
            .any(|message| message.contains("database.db_url") && message.contains("base file")));
        assert!(
            source_fields
                .iter()
                .any(|message| message.contains("mail.password")
                    && message.contains("MEGA_MAIL__PASSWORD"))
        );
        assert!(source_text.contains("values are omitted"));
        assert!(source_text.contains("sensitive values are omitted"));
        assert!(source_text.contains("deployment/environment secrets"));
        assert!(source_text.contains("use a higher-precedence profile/env override"));
        assert!(source_text.contains(
            "update MEGA_MAIL__PASSWORD or unset it to fall back to lower-precedence sources"
        ));
        assert!(
            diagnostics
                .environment_warnings
                .iter()
                .any(|warning| warning.variable == "MEGA_UNKNOWN__VALUE")
        );
        assert!(!source_text.contains("postgres://localhost"));
        assert!(!source_text.contains("debug"));
        assert!(!source_text.contains("info"));
        assert!(!source_text.contains("vault://secret/"));
        assert!(!source_text.contains("config/prod/mail/password"));
        assert!(!source_text.contains("#value"));
        assert!(!source_text.contains("plain-text-password"));
    }

    #[test]
    fn source_diagnostics_redacts_notification_secret_ref_values() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
            [notification.slack]
            enabled = true
            webhook_url_ref = "vault://secret/config/base/notification/slack/webhook_url#value"

            [notification.webhook]
            enabled = true
            url = "https://hooks.example.test/webhook"
            token_ref = "vault://secret/config/base/notification/webhook/token#value"
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
            [notification.slack]
            webhook_url_ref = "vault://secret/config/prod/notification/slack/webhook_url#value"
            "#,
        )
        .expect("write profile config");

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            ["MEGA_NOTIFICATION__SLACK__WEBHOOK_URL_REF"],
        )
        .expect("diagnostics should collect");
        let source_fields = diagnostics
            .source_fields
            .iter()
            .map(|source_field| source_field.message.as_str())
            .collect::<Vec<_>>();
        let source_text = source_fields.join("\n");
        let overrides = diagnostics
            .source_overrides
            .iter()
            .map(|source_override| source_override.message.as_str())
            .collect::<Vec<_>>();
        let override_text = overrides.join("\n");

        assert!(
            source_fields.iter().any(|message| {
                message.contains("notification.slack.webhook_url_ref")
                    && message.contains("base file")
            }),
            "missing slack webhook_url_ref base field: {source_text}"
        );
        assert!(
            source_fields.iter().any(|message| {
                message.contains("notification.webhook.token_ref") && message.contains("base file")
            }),
            "missing webhook token_ref base field: {source_text}"
        );
        assert!(
            override_text.contains("notification.slack.webhook_url_ref")
                && override_text.contains("profile file")
                && override_text.contains("base file"),
            "missing slack webhook_url_ref override: {override_text}"
        );
        assert!(
            source_text.contains("sensitive values are omitted"),
            "notification SecretRef fields should be marked sensitive: {source_text}"
        );
        assert!(
            !source_text.contains("vault://secret/"),
            "source diagnostics leaked notification SecretRef URI: {source_text}"
        );
        assert!(
            !source_text.contains("config/prod/notification/slack/webhook_url"),
            "source diagnostics leaked notification vault path: {source_text}"
        );
        assert!(
            !source_text.contains("config/base/notification/webhook/token"),
            "source diagnostics leaked notification webhook token path: {source_text}"
        );
        assert!(!source_text.contains("#value"));
    }

    #[test]
    fn source_diagnostics_collects_array_element_field_paths() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(
            &config_path,
            r#"
            [sidebar]
            default_items = [
                { public_id = "home", label = "Home", href = "/posts", visible = true, order_index = 0 },
            ]
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
            [sidebar]
            default_items = [
                { public_id = "home", label = "Home", href = "/prod", visible = true, order_index = 0 },
                { public_id = "chat", label = "Chat", href = "/chat", visible = true, order_index = 1 },
            ]
            "#,
        )
        .expect("write profile config");

        let diagnostics = collect_source_diagnostics_from_keys::<_, &str>(
            Some(&config_path),
            Some(&profile_path),
            [],
        )
        .expect("diagnostics should collect");

        let fields = diagnostics
            .source_fields
            .iter()
            .map(|source_field| source_field.field_path.as_str())
            .collect::<Vec<_>>();
        let overrides = diagnostics
            .source_overrides
            .iter()
            .map(|source_override| source_override.field_path.as_str())
            .collect::<Vec<_>>();

        // Per-element paths are now reported in addition to the whole-array path.
        assert!(fields.contains(&"sidebar.default_items[0].public_id"));
        assert!(fields.contains(&"sidebar.default_items[0].label"));
        assert!(fields.contains(&"sidebar.default_items[0].href"));
        assert!(fields.contains(&"sidebar.default_items[1].public_id"));
        assert!(fields.contains(&"sidebar.default_items[1].href"));
        assert!(fields.contains(&"sidebar.default_items"));

        // The first element's href is overridden by the profile.
        assert!(overrides.contains(&"sidebar.default_items[0].href"));

        // Array-replace note applies to element paths too.
        assert!(diagnostics.source_overrides.iter().any(|source_override| {
            source_override.field_path == "sidebar.default_items[0].href"
                && source_override
                    .message
                    .contains("arrays replace lower-precedence values rather than append")
        }));
    }

    /// Canonical "complete cross-source/profile matrix" lock-in: a single field
    /// present in ALL THREE sources (base file, profile file, and a `MEGA_*` env
    /// override) must be attributed in the source-field graph at every source AND
    /// produce the full override chain (profile overrides base, env overrides
    /// profile), with no values leaked (config.md stage 4 source diagnostics).
    #[test]
    fn source_diagnostics_full_cross_source_matrix_for_single_field() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        // Distinctive sentinel values so the no-leak assertion cannot false-match
        // (source diagnostics read raw TOML and never validate the value).
        std::fs::write(
            &config_path,
            r#"
            [log]
            level = "base-sentinel-value"
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
            [log]
            level = "profile-sentinel-value"
            "#,
        )
        .expect("write profile config");

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            ["MEGA_LOG__LEVEL"],
        )
        .expect("diagnostics should collect");

        // Source-field graph: log.level attributed at base, profile, AND env.
        let field_messages = diagnostics
            .source_fields
            .iter()
            .filter(|field| field.field_path == "log.level")
            .map(|field| field.message.as_str())
            .collect::<Vec<_>>();
        assert!(field_messages.iter().any(|m| m.contains("base file")));
        assert!(field_messages.iter().any(|m| m.contains("profile file")));
        assert!(field_messages.iter().any(|m| m.contains("MEGA_LOG__LEVEL")));

        // Override chain: profile overrides base, env overrides profile.
        let override_messages = diagnostics
            .source_overrides
            .iter()
            .filter(|over| over.field_path == "log.level")
            .map(|over| over.message.as_str())
            .collect::<Vec<_>>();
        assert!(override_messages.iter().any(|m| {
            m.contains("profile file") && m.contains("base file") && !m.contains("environment")
        }));
        assert!(
            override_messages
                .iter()
                .any(|m| m.contains("MEGA_LOG__LEVEL") && m.contains("profile file"))
        );

        // No raw values leak anywhere in the diagnostics.
        let all_text = format!(
            "{}\n{}",
            field_messages.join("\n"),
            override_messages.join("\n")
        );
        assert!(!all_text.contains("base-sentinel-value"));
        assert!(!all_text.contains("profile-sentinel-value"));
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
            // Consumed now -> must NOT warn.
            "MEGA_OAUTH__ALLOWED_CORS_ORIGINS",
            // Legacy, still ignored -> warns.
            "MEGA_OAUTH__CAMPSITE_API_DOMAIN",
            "MEGA_MAIL__PASSWORD",
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
                "MEGA_MAIL__PASSWORD",
                "MEGA_MAIL__SMTP_TLS",
                "MEGA_MAIL__TLS",
                "MEGA_OAUTH__CAMPSITE_API_DOMAIN",
                "MEGA_UNKNOWN__VALUE",
            ]
        );
        assert_eq!(
            fields,
            vec![
                "mail.password",
                "mail.smtp_tls",
                "mail.tls",
                "oauth.campsite_api_domain",
                "unknown.value",
            ]
        );
        // The consumed CORS origins env var produces no warning.
        assert!(!variables.contains(&"MEGA_OAUTH__ALLOWED_CORS_ORIGINS"));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("MEGA_MAIL__PASSWORD_REF"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("MEGA_MAIL__STARTTLS"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("no consumer yet"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("not recognized by Config"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.message.contains("supported MEGA_* field path"))
        );
    }

    #[test]
    fn known_config_field_path_accepts_nested_fields_and_rejects_orphans() {
        assert!(is_known_field_path("database.db_url"));
        assert!(is_known_field_path("object_storage.s3.access_key_id"));
        assert!(is_known_field_path("sidebar.default_items.label"));
        assert!(is_known_field_path("notification.enabled"));
        assert!(is_known_field_path("notification.default_delivery_mode"));
        assert!(is_known_field_path("notification.default_locale"));
        assert!(!is_known_field_path("database.db_url.extra"));
        assert!(!is_known_field_path("database.typo"));
        assert!(!is_known_field_path("unknown.value"));
        assert!(is_known_field_path("oauth.allowed_cors_origins"));
        assert!(!is_known_field_path("notification.typo"));
    }

    #[test]
    fn notification_section_is_recognized_and_validates_fields() {
        let value = toml::from_str::<Value>(
            r#"
            [notification]
            enabled = true
            default_delivery_mode = "email"
            default_locale = "en"
            typo = true
            "#,
        )
        .unwrap();

        let fields = known_unconsumed_fields(&value)
            .into_iter()
            .map(|warning| warning.field_path)
            .collect::<Vec<_>>();

        // A valid [notification] section must not be flagged as unknown ...
        assert!(!fields.iter().any(|f| f == "notification"));
        assert!(!fields.iter().any(|f| f == "notification.enabled"));
        assert!(
            !fields
                .iter()
                .any(|f| f == "notification.default_delivery_mode")
        );
        assert!(!fields.iter().any(|f| f == "notification.default_locale"));
        // ... but an unknown field inside it still is.
        assert!(fields.iter().any(|f| f == "notification.typo"));
    }

    #[test]
    fn vault_audit_section_is_recognized_and_validates_fields() {
        let value = toml::from_str::<Value>(
            r#"
            [vault.audit]
            enabled = true
            typo = true
            "#,
        )
        .unwrap();

        let fields = known_unconsumed_fields(&value)
            .into_iter()
            .map(|warning| warning.field_path)
            .collect::<Vec<_>>();

        // A valid [vault.audit] section and its `enabled` field must not be flagged ...
        assert!(!fields.iter().any(|f| f == "vault"));
        assert!(!fields.iter().any(|f| f == "vault.audit"));
        assert!(!fields.iter().any(|f| f == "vault.audit.enabled"));
        // ... but an unknown field inside it still is.
        assert!(fields.iter().any(|f| f == "vault.audit.typo"));
    }

    #[test]
    fn config_init_template_has_no_unconsumed_fields() {
        let rendered = config_init_template(Path::new("/tmp/monoengine"));
        let value = toml::from_str::<Value>(&rendered).unwrap();

        assert!(known_unconsumed_fields(&value).is_empty());
    }

    #[test]
    fn reject_unknown_fields_accepts_known_config() {
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"

            [log]
            level = "info"
            print_std = true

            [database]
            db_type = "postgres"
            db_url = "postgres://localhost:5432/mono"

            [mail]
            enabled = true
            smtp_host = "localhost"
            from = "no-reply@example.com"

            [notification]
            enabled = true
            default_delivery_mode = "email"
            "#,
        )
        .unwrap();

        assert!(reject_unknown_fields(&value).is_ok());
    }

    #[test]
    fn reject_unknown_fields_rejects_typo_fields() {
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"
            unknown_root = true

            [database]
            db_url = "postgres://localhost:5432/mono"
            typo = true

            [object_storage.s3]
            unexpected = true

            [sidebar]
            default_items = [
                { public_id = "home", label = "Home", href = "/posts", order_index = 0, icon = "x" },
            ]
            "#,
        )
        .unwrap();

        let err = reject_unknown_fields(&value).expect_err("should reject unknown fields");
        let message = err.to_string();
        assert!(message.contains("unknown_root"));
        assert!(message.contains("database.typo"));
        assert!(message.contains("object_storage.s3.unexpected"));
        assert!(message.contains("sidebar.default_items[0].icon"));
    }

    #[test]
    fn reject_unknown_fields_allows_known_oauth_keys_but_rejects_unknown_ones() {
        // `[oauth]` is now a recognized section: allowed_cors_origins (consumed)
        // plus the whitelisted legacy keys pass the strict check.
        let ok = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"

            [oauth]
            campsite_api_domain = "http://example.test"
            allowed_cors_origins = ["http://example.test"]
            "#,
        )
        .unwrap();
        assert!(reject_unknown_fields(&ok).is_ok());

        // A truly unknown key under [oauth] is a hard error now that the section
        // is validated like any other.
        let bad = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"

            [oauth]
            totally_unknown_key = "x"
            "#,
        )
        .unwrap();
        assert!(reject_unknown_fields(&bad).is_err());
    }
}
