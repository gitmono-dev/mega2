use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use super::{ObjectStorageConfig, mega_base, mega_cache, secret};
use crate::common::errors::MegaError;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub base_dir: PathBuf,
    pub log: LogConfig,
    pub database: DbConfig,
    pub monorepo: MonoConfig,
    pub pack: PackConfig,
    pub lfs: LFSConfig,
    #[serde(default)]
    pub blame: BlameConfig,
    pub build: BuildConfig,
    pub redis: RedisConfig,
    #[serde(default)]
    pub buck: Option<BuckConfig>,
    pub object_storage: ObjectStorageConfig,
    #[serde(default)]
    pub orion_server: Option<OrionServerConfig>,
    #[serde(default)]
    pub sidebar: SidebarConfig,
    /// Background GC for `artifact_objects` rows with no `artifact_set_files` references
    /// (`docs/artifacts-protocol.md` §10.6).
    #[serde(default)]
    pub artifacts_gc: ArtifactGcConfig,
    /// Mail / SMTP configuration for system notifications (email outbox via email_jobs).
    /// This is the first planned consumer for SecretRef (password) after vault is ready.
    #[serde(default)]
    pub mail: Option<MailConfig>,
    /// Global notification subsystem settings (kill switch, defaults). Per-user
    /// preferences still live in the DB; this is the global layer
    /// (docs/notification.md phase 5).
    #[serde(default)]
    pub notification: Option<NotificationConfig>,
    /// Vault runtime settings (e.g. secret-access audit). Bootstrap material
    /// (unseal shares / runtime tokens) lives in the `core_key.json` key file,
    /// not here (docs/vault.md stage H).
    #[serde(default)]
    pub vault: Option<VaultConfig>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct VaultBootstrapConfig {
    pub database: DbConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogConfig {
    pub level: String,
    pub print_std: bool,
    /// Whether to enable ANSI colors for stdout logs (has no effect on file logs).
    #[serde(default = "default_with_ansi")]
    pub with_ansi: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: String::from("info"),
            print_std: true,
            with_ansi: true,
        }
    }
}

fn default_with_ansi() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DbConfig {
    pub db_type: String,
    pub db_path: PathBuf,
    pub db_url: String,
    pub max_connection: u32,
    pub min_connection: u32,
    pub acquire_timeout: u64,
    pub connect_timeout: u64,
    pub sqlx_logging: bool,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            db_type: String::from("postgres"),
            db_path: PathBuf::new(),
            db_url: String::from("postgres://localhost:5432/mega"),
            max_connection: 16,
            min_connection: 8,
            acquire_timeout: 5,
            connect_timeout: 5,
            sqlx_logging: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MonoConfig {
    pub import_dir: PathBuf,
    pub admin: Vec<String>,
    pub root_dirs: Vec<String>,
    #[serde(default)]
    pub rename: RenameConfig,
}

impl Default for MonoConfig {
    fn default() -> Self {
        Self {
            import_dir: PathBuf::from("/third-party"),
            admin: vec!["admin".to_string()],
            root_dirs: vec![
                "third-party".to_string(),
                "toolchains".to_string(),
                "project".to_string(),
                "doc".to_string(),
                "release".to_string(),
            ],
            rename: RenameConfig::default(),
        }
    }
}

/// Periodic garbage collection for repo artifact blobs (`artifact_objects`) per
/// `docs/artifacts-protocol.md` §10.6 (unreferenced `oid` + optional `last_seen_at` grace).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArtifactGcConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_artifacts_gc_interval_secs")]
    pub interval_secs: u64,
    /// Skip rows whose `last_seen_at` is newer than `now - grace_secs` (reduces races with in-flight commits).
    #[serde(default = "default_artifacts_gc_grace_secs")]
    pub grace_secs: u64,
    #[serde(default = "default_artifacts_gc_batch_limit")]
    pub batch_limit: u64,
}

fn default_artifacts_gc_interval_secs() -> u64 {
    3600
}

fn default_artifacts_gc_grace_secs() -> u64 {
    86_400
}

fn default_artifacts_gc_batch_limit() -> u64 {
    100
}

impl Default for ArtifactGcConfig {
    fn default() -> Self {
        Self {
            enable: false,
            interval_secs: default_artifacts_gc_interval_secs(),
            grace_secs: default_artifacts_gc_grace_secs(),
            batch_limit: default_artifacts_gc_batch_limit(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MailProvider {
    #[default]
    Smtp,
    Console,
}

pub const DEFAULT_MAIL_DISPATCHER_BATCH_SIZE: u64 = 50;
pub const DEFAULT_MAIL_DISPATCHER_MAX_IN_FLIGHT: usize = 8;
pub const DEFAULT_MAIL_RETRY_MAX_ATTEMPTS: i32 = 5;
pub const DEFAULT_MAIL_RETRY_BACKOFF_BASE_SECS: i64 = 30;
pub const DEFAULT_MAIL_RETRY_BACKOFF_MAX_SECS: i64 = 300;
pub const DEFAULT_MAIL_ATTACHMENT_PRUNE_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_MAIL_ATTACHMENT_RETENTION_DAYS: u32 = 30;
pub const DEFAULT_MAIL_TEMPLATE_LOCALE: &str = "en-US";

/// Mail configuration. Lives here so it participates in the main Config loading
/// / env overlay / placeholder / (future) SecretRef pipeline.
/// See docs/mail.md for the full mail module design and SecretRef migration plan.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: MailProvider,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<secret::SecretString>,
    #[serde(default)]
    pub password_ref: Option<secret::SecretRef>,
    #[serde(default)]
    pub from: String,
    #[serde(default = "default_starttls")]
    pub starttls: bool,
    #[serde(default = "default_mail_dispatcher_batch_size")]
    pub dispatcher_batch_size: u64,
    #[serde(default = "default_mail_dispatcher_max_in_flight")]
    pub dispatcher_max_in_flight: usize,
    #[serde(default = "default_mail_retry_max_attempts")]
    pub retry_max_attempts: i32,
    #[serde(default = "default_mail_retry_backoff_base_secs")]
    pub retry_backoff_base_secs: i64,
    #[serde(default = "default_mail_retry_backoff_max_secs")]
    pub retry_backoff_max_secs: i64,
    #[serde(default)]
    pub attachment_prune_enabled: bool,
    #[serde(default = "default_mail_attachment_prune_interval_secs")]
    pub attachment_prune_interval_secs: u64,
    #[serde(default = "default_mail_attachment_retention_days")]
    pub attachment_retention_days: u32,
    #[serde(default = "default_mail_attachment_prune_statuses")]
    pub attachment_prune_statuses: Vec<String>,
    #[serde(default = "default_mail_template_locale")]
    pub template_default_locale: String,
    #[serde(default)]
    pub template_dir: Option<PathBuf>,
    // Extra fields present in some sample tomls are ignored by serde (unknown fields dropped).
}

fn default_smtp_port() -> u16 {
    587
}
fn default_starttls() -> bool {
    true
}
fn default_mail_dispatcher_batch_size() -> u64 {
    DEFAULT_MAIL_DISPATCHER_BATCH_SIZE
}
fn default_mail_dispatcher_max_in_flight() -> usize {
    DEFAULT_MAIL_DISPATCHER_MAX_IN_FLIGHT
}
fn default_mail_retry_max_attempts() -> i32 {
    DEFAULT_MAIL_RETRY_MAX_ATTEMPTS
}
fn default_mail_retry_backoff_base_secs() -> i64 {
    DEFAULT_MAIL_RETRY_BACKOFF_BASE_SECS
}
fn default_mail_retry_backoff_max_secs() -> i64 {
    DEFAULT_MAIL_RETRY_BACKOFF_MAX_SECS
}
fn default_mail_attachment_prune_interval_secs() -> u64 {
    DEFAULT_MAIL_ATTACHMENT_PRUNE_INTERVAL_SECS
}
fn default_mail_attachment_retention_days() -> u32 {
    DEFAULT_MAIL_ATTACHMENT_RETENTION_DAYS
}
fn default_mail_attachment_prune_statuses() -> Vec<String> {
    vec!["sent".to_string(), "skipped".to_string()]
}
fn default_mail_template_locale() -> String {
    DEFAULT_MAIL_TEMPLATE_LOCALE.to_string()
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: MailProvider::default(),
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            username: None,
            password: None,
            password_ref: None,
            from: String::new(),
            starttls: default_starttls(),
            dispatcher_batch_size: default_mail_dispatcher_batch_size(),
            dispatcher_max_in_flight: default_mail_dispatcher_max_in_flight(),
            retry_max_attempts: default_mail_retry_max_attempts(),
            retry_backoff_base_secs: default_mail_retry_backoff_base_secs(),
            retry_backoff_max_secs: default_mail_retry_backoff_max_secs(),
            attachment_prune_enabled: false,
            attachment_prune_interval_secs: default_mail_attachment_prune_interval_secs(),
            attachment_retention_days: default_mail_attachment_retention_days(),
            attachment_prune_statuses: default_mail_attachment_prune_statuses(),
            template_default_locale: default_mail_template_locale(),
            template_dir: None,
        }
    }
}

impl MailConfig {
    pub fn validate_secret_fields(&self) -> Result<(), MegaError> {
        if self.password.is_some() && self.password_ref.is_some() {
            return Err(MegaError::Other(
                "mail.password and mail.password_ref are mutually exclusive".to_string(),
            ));
        }

        Ok(())
    }
}

pub const DEFAULT_NOTIFICATION_DELIVERY_MODE: &str = "email";
pub const NOTIFICATION_DELIVERY_MODES: &[&str] = &["email"];

/// Global notification subsystem configuration.
///
/// This is the global layer above per-user DB preferences (docs/notification.md
/// phase 5): a global kill switch plus defaults applied when a user has no
/// explicit setting. `enabled` is hot-reloadable (gates the dispatcher); the
/// defaults are read at consumption time.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NotificationConfig {
    /// Global kill switch. When false the dispatcher is gated off regardless of
    /// `mail.enabled`. Hot-reloadable.
    #[serde(default = "default_notification_enabled")]
    pub enabled: bool,
    /// Default delivery mode for users without an explicit setting.
    #[serde(default = "default_notification_delivery_mode")]
    pub default_delivery_mode: String,
    /// Default locale for rendered notifications when a user has none.
    #[serde(default = "default_mail_template_locale")]
    pub default_locale: String,
    /// Optional Slack incoming-webhook channel (docs/notification.md phase 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackConfig>,
    /// Optional generic outbound webhook channel (docs/notification.md phase 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

/// Slack incoming-webhook delivery channel (docs/notification.md phase 3).
///
/// A Slack incoming-webhook URL embeds a secret token in its path, so the URL
/// itself is the credential and is supplied as a [`secret::SecretRef`] resolved
/// from vault after startup — never stored in plaintext config.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct SlackConfig {
    #[serde(default)]
    pub enabled: bool,
    /// SecretRef to the Slack incoming-webhook URL (the URL is the credential).
    /// Required when `enabled` is true; namespace
    /// `vault://secret/config/<profile>/notification/slack/webhook_url#<field>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url_ref: Option<secret::SecretRef>,
}

/// Generic outbound webhook delivery channel (docs/notification.md phase 3).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct WebhookConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Destination URL. Operator-trusted, non-secret (unlike a Slack webhook
    /// URL); required when `enabled` is true.
    #[serde(default)]
    pub url: String,
    /// Optional bearer token SecretRef sent as `Authorization: Bearer <token>`;
    /// namespace `vault://secret/config/<profile>/notification/webhook/token#<field>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_ref: Option<secret::SecretRef>,
}

fn default_notification_enabled() -> bool {
    true
}
fn default_notification_delivery_mode() -> String {
    DEFAULT_NOTIFICATION_DELIVERY_MODE.to_string()
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: default_notification_enabled(),
            default_delivery_mode: default_notification_delivery_mode(),
            default_locale: default_mail_template_locale(),
            slack: None,
            webhook: None,
        }
    }
}

/// Vault runtime settings (docs/vault.md stage H). Currently scopes the
/// secret-access audit; bootstrap material is not configured here.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct VaultConfig {
    #[serde(default)]
    pub audit: VaultAuditConfig,
}

/// Secret-access audit settings (docs/vault.md stage H).
///
/// `enabled` defaults to on, so deployments audit by default; setting it false
/// opts out of emitting per-access records. The audit destination is the
/// `vault_audit` `tracing` target; a configurable durable/alternate sink is
/// deferred (vault.md stage H). The write-failure policy is fail-open: the
/// `tracing` sink is infallible, so a secret operation is never blocked or
/// failed by the audit step (availability-over-non-repudiation).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultAuditConfig {
    #[serde(default = "default_vault_audit_enabled")]
    pub enabled: bool,
}

fn default_vault_audit_enabled() -> bool {
    true
}

impl Default for VaultAuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_vault_audit_enabled(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RenameConfig {
    #[serde(default = "default_rename_similarity_threshold")]
    pub similarity_threshold: u8,
    #[serde(default = "default_rename_limit")]
    pub rename_limit: usize,
}

fn default_rename_similarity_threshold() -> u8 {
    50
}

fn default_rename_limit() -> usize {
    1000
}

impl Default for RenameConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: default_rename_similarity_threshold(),
            rename_limit: default_rename_limit(),
        }
    }
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PackConfig {
    #[serde(deserialize_with = "string_or_usize")]
    pub pack_decode_mem_size: String,
    #[serde(deserialize_with = "string_or_usize")]
    pub pack_decode_disk_size: String,
    pub pack_decode_cache_path: PathBuf,
    pub clean_cache_after_decode: bool,
    pub channel_message_size: usize,
    /// Max concurrent `save_entry` batches during receive-pack.
    /// Set to 0 to disable the limit (unbounded).
    #[serde(default = "default_save_entry_concurrency")]
    pub save_entry_concurrency: usize,
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            pack_decode_mem_size: "4G".to_string(),
            pack_decode_disk_size: "20%".to_string(),
            pack_decode_cache_path: mega_cache().join("pack_decode_cache"),
            clean_cache_after_decode: true,
            channel_message_size: 1_000_000,
            save_entry_concurrency: default_save_entry_concurrency(),
        }
    }
}

fn default_save_entry_concurrency() -> usize {
    1
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RedisConfig {
    pub url: String,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: String::from("redis://127.0.0.1:6379"),
        }
    }
}

fn string_or_usize<'deserialize, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'deserialize>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrUSize {
        String(String),
        USize(usize),
    }

    Ok(match StringOrUSize::deserialize(deserializer)? {
        StringOrUSize::String(v) => v,
        StringOrUSize::USize(v) => v.to_string(),
    })
}

impl PackConfig {
    /// Converts a size string to bytes
    /// Supports formats:
    /// - Bytes with units: "1MB", "2MiB", "3GB", "4GiB"
    /// - Percentage of total memory: "1%", "50%"
    /// - Decimal ratio of total memory: "0.01", "0.5"
    /// - For compatibility: Any integer greater than or equal to 1, for example "1" will be interpreted as 1Gib.
    ///
    /// # Examples
    /// ```
    /// use crate::config::PackConfig;
    ///
    /// assert_eq!(PackConfig::get_size_from_str("1MB", || Ok(1 * 1000 * 1000)).unwrap(), 1 * 1000 * 1000);
    /// assert_eq!(PackConfig::get_size_from_str("2MiB", || Ok(2 * 1024 * 1024)).unwrap(), 2 * 1024 * 1024);
    /// assert_eq!(PackConfig::get_size_from_str("3GB", || Ok(3 * 1000 * 1000 * 1000)).unwrap(), 3 * 1000 * 1000 * 1000);
    /// assert_eq!(PackConfig::get_size_from_str("4GiB", || Ok(4 * 1024 * 1024 * 1024)).unwrap(), 4 * 1024 * 1024 * 1024);
    /// assert_eq!(PackConfig::get_size_from_str("4G", || Ok(4 * 1024 * 1024 * 1024)).unwrap(), 4 * 1024 * 1024 * 1024);
    /// assert_eq!(PackConfig::get_size_from_str("1%", || Ok(100)).unwrap(), 1);
    /// assert_eq!(PackConfig::get_size_from_str("50%", || Ok(100)).unwrap(), 50);
    /// assert_eq!(PackConfig::get_size_from_str("0.01", || Ok(100)).unwrap(), 1);
    /// assert_eq!(PackConfig::get_size_from_str("0.5", || Ok(100)).unwrap(), 50);
    /// assert_eq!(PackConfig::get_size_from_str("1", || Ok(100)).unwrap(), 1 * 1024 * 1024 * 1024);
    /// ```
    /// # Notes
    /// - fn_get_total_capacity is a function that returns the total memory capacity in bytes.
    ///   If the function fails, it returns a String error message.
    pub fn get_size_from_str(
        size_str: &str,
        fn_get_total_capacity: fn() -> Result<usize, String>,
    ) -> Result<usize, String> {
        let size_str = size_str.trim();

        // Try to parse as percentage or decimal ratio
        if size_str.ends_with('%') {
            let percentage: f64 = size_str
                .trim_end_matches('%')
                .parse()
                .map_err(|_| format!("Invalid percentage: {size_str}"))?;
            let total_mem = fn_get_total_capacity()?;

            return Ok((total_mem as f64 * percentage / 100.0) as usize);
        }

        let ratio_result = size_str.parse::<f64>();
        if let Ok(ratio) = ratio_result
            && ratio > 0.0
            && ratio < 1.0
        {
            let total_mem = fn_get_total_capacity()?;

            return Ok((total_mem as f64 * ratio) as usize);
        }

        // Parse size with units
        let mut chars = size_str.chars().peekable();
        let mut number = String::new();

        // Parse the numeric part
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                chars.next();
            } else {
                break;
            }
        }

        let value: f64 = number
            .parse()
            .map_err(|_| format!("Invalid size: {size_str}"))?;
        let unit = chars.collect::<String>().to_uppercase();

        // For compatibility,
        // old configuration files use integer and use GiB as the default unit.
        if unit.is_empty() {
            return Ok((value * 1024.0 * 1024.0 * 1024.0) as usize);
        }

        let bytes = match unit.as_str() {
            "B" => value,
            "KB" => value * 1_000.0,
            "MB" => value * 1_000.0 * 1_000.0,
            "GB" => value * 1_000.0 * 1_000.0 * 1_000.0,
            "TB" => value * 1_000.0 * 1_000.0 * 1_000.0 * 1_000.0,
            "KIB" | "K" => value * 1_024.0,
            "MIB" | "M" => value * 1_024.0 * 1_024.0,
            "GIB" | "G" => value * 1_024.0 * 1_024.0 * 1_024.0,
            "TIB" | "T" => value * 1_099_511_627_776.0,
            _ => Err(format!("Invalid unit: {unit}"))?,
        };

        Ok(bytes as usize)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LFSConfig {
    pub local: LFSLocalConfig,
    pub ssh: LFSSshConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LFSLocalConfig {
    pub lfs_file_path: PathBuf,
}

impl Default for LFSLocalConfig {
    fn default() -> Self {
        Self {
            lfs_file_path: mega_base().join("lfs"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LFSSshConfig {
    pub http_url: String,
}

impl Default for LFSSshConfig {
    fn default() -> Self {
        Self {
            http_url: "http://localhost:8000".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlameConfig {
    /// Maximum number of lines before considering a file as large
    pub max_lines_threshold: usize,
    /// Maximum file size in bytes before considering a file as large
    #[serde(deserialize_with = "string_or_usize")]
    pub max_size_threshold: String,
    /// Default chunk size for streaming operations when processing large files
    pub default_chunk_size: usize,
    /// Maximum number of commits to process in memory at once during blame traversal
    pub max_commits_in_memory: usize,
    /// Enable caching of intermediate blame results for better performance
    pub enable_caching: bool,
}

impl Default for BlameConfig {
    fn default() -> Self {
        Self {
            max_lines_threshold: 5000,
            max_size_threshold: "1MB".to_string(),
            default_chunk_size: 100,
            max_commits_in_memory: 50,
            enable_caching: true,
        }
    }
}

impl BlameConfig {
    /// Converts the max_size_threshold string to bytes using the same logic as PackConfig
    pub fn get_max_size_bytes(&self) -> Result<usize, String> {
        PackConfig::get_size_from_str(&self.max_size_threshold, || {
            // Default to 8GB total memory for calculation if needed
            Ok(8 * 1024 * 1024 * 1024)
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BuildConfig {
    pub enable_build: bool,
    pub orion_server: String,
    #[serde(default)]
    pub orion_preheat_shallow_depth: usize,
}

/// Orion Server configuration (flat structure)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OrionServerConfig {
    // Log storage configuration
    #[serde(default = "default_logger_storage_mode")]
    pub logger_storage_mode: String,

    #[serde(default = "default_build_log_dir")]
    pub build_log_dir: String,

    #[serde(default = "default_log_stream_buffer")]
    pub log_stream_buffer: usize,

    // Database configuration
    #[serde(default = "default_db_url")]
    pub db_url: String,

    #[serde(default = "default_port")]
    pub port: u16,

    /// Mono server base URL for file/blob API (e.g. file blob endpoint). Replaces MONOBASE_URL env.
    #[serde(default = "default_monobase_url")]
    pub monobase_url: String,
}

fn default_monobase_url() -> String {
    "http://localhost:8000".to_string()
}

fn default_logger_storage_mode() -> String {
    "local".to_string()
}

fn default_build_log_dir() -> String {
    "/tmp/logs".to_string()
}

fn default_log_stream_buffer() -> usize {
    4096
}

fn default_db_url() -> String {
    "postgres://localhost/orion".to_string()
}

fn default_port() -> u16 {
    8004
}

impl Default for OrionServerConfig {
    fn default() -> Self {
        Self {
            logger_storage_mode: default_logger_storage_mode(),
            build_log_dir: default_build_log_dir(),
            log_stream_buffer: default_log_stream_buffer(),
            db_url: default_db_url(),
            port: default_port(),
            monobase_url: default_monobase_url(),
        }
    }
}

/// Buck upload API configuration
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BuckConfig {
    /// Session timeout in seconds (default: 3600 = 1 hour)
    #[serde(default = "default_session_timeout")]
    pub session_timeout: u64,

    /// Maximum file size (default: "100MB")
    #[serde(default = "default_max_file_size")]
    pub max_file_size: String,

    /// Maximum number of files per session (default: 1000)
    #[serde(default = "default_max_files")]
    pub max_files: u32,

    /// Maximum concurrent uploads (default: 5) - returned to client as suggestion.
    /// Recommended range: 1-100. Values above 1000 are rejected to avoid overload.
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: u32,

    /// Global upload concurrency limit (default: 50)
    /// Controls the total number of concurrent upload requests
    #[serde(default = "default_upload_concurrency_limit")]
    pub upload_concurrency_limit: u32,

    /// Large file concurrency limit (default: 10)
    /// Controls the number of concurrent large file uploads to prevent memory exhaustion
    #[serde(default = "default_large_file_concurrency_limit")]
    pub large_file_concurrency_limit: u32,

    /// Large file threshold (default: "1MB")
    /// Files larger than this are considered "large" and subject to additional concurrency limits
    #[serde(default = "default_large_file_threshold")]
    pub large_file_threshold: String,

    /// Enable session cleanup task (default: true)
    #[serde(default = "default_enable_session_cleanup")]
    pub enable_session_cleanup: bool,

    /// Cleanup task interval in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval: u64,

    /// Retention days for completed sessions (default: 7 days)
    /// Completed sessions older than this will be deleted along with their file records
    #[serde(default = "default_completed_retention_days")]
    pub completed_retention_days: u32,
}

fn default_session_timeout() -> u64 {
    3600
}
fn default_max_file_size() -> String {
    "100MB".to_string()
}
fn default_max_files() -> u32 {
    1000
}
fn default_max_concurrent_uploads() -> u32 {
    5
}
fn default_upload_concurrency_limit() -> u32 {
    50
}
fn default_large_file_concurrency_limit() -> u32 {
    10
}
fn default_large_file_threshold() -> String {
    "1MB".to_string()
}
fn default_enable_session_cleanup() -> bool {
    true
}
fn default_cleanup_interval() -> u64 {
    300
}
fn default_completed_retention_days() -> u32 {
    7
}

impl BuckConfig {
    /// Parse max_file_size string to bytes
    /// Returns the size in bytes, or an error message if parsing fails
    pub fn get_max_file_size_bytes(&self) -> Result<u64, String> {
        PackConfig::get_size_from_str(&self.max_file_size, || Ok(0)).map(|v| v as u64)
    }

    /// Parse large_file_threshold string to bytes
    /// Returns the size in bytes, or an error message if parsing fails
    pub fn get_large_file_threshold_bytes(&self) -> Result<u64, String> {
        PackConfig::get_size_from_str(&self.large_file_threshold, || Ok(0)).map(|v| v as u64)
    }

    /// Validate configuration values
    ///
    /// # Returns
    /// * `Ok(())` - All values are valid
    /// * `Err(String)` - Error message describing the validation failure
    pub fn validate(&self) -> Result<(), String> {
        if self.max_concurrent_uploads < 1 {
            return Err(format!(
                "max_concurrent_uploads must be >= 1, got {}",
                self.max_concurrent_uploads
            ));
        }

        if self.max_concurrent_uploads > 1000 {
            return Err(format!(
                "max_concurrent_uploads must be <= 1000, got {}",
                self.max_concurrent_uploads
            ));
        }

        if self.upload_concurrency_limit < 1 {
            return Err(format!(
                "upload_concurrency_limit must be >= 1, got {}",
                self.upload_concurrency_limit
            ));
        }

        if self.large_file_concurrency_limit < 1 {
            return Err(format!(
                "large_file_concurrency_limit must be >= 1, got {}",
                self.large_file_concurrency_limit
            ));
        }

        if self.max_files == 0 {
            return Err(format!("max_files must be > 0, got {}", self.max_files));
        }

        if self.session_timeout == 0 {
            return Err(format!(
                "session_timeout must be > 0, got {}",
                self.session_timeout
            ));
        }

        if self.cleanup_interval == 0 {
            return Err(format!(
                "cleanup_interval must be > 0, got {}",
                self.cleanup_interval
            ));
        }

        Ok(())
    }
}

impl Default for BuckConfig {
    fn default() -> Self {
        Self {
            session_timeout: default_session_timeout(),
            max_file_size: default_max_file_size(),
            max_files: default_max_files(),
            max_concurrent_uploads: default_max_concurrent_uploads(),
            upload_concurrency_limit: default_upload_concurrency_limit(),
            large_file_concurrency_limit: default_large_file_concurrency_limit(),
            large_file_threshold: default_large_file_threshold(),
            enable_session_cleanup: default_enable_session_cleanup(),
            cleanup_interval: default_cleanup_interval(),
            completed_retention_days: default_completed_retention_days(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct SidebarConfig {
    pub default_items: Vec<SidebarItem>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SidebarItem {
    pub public_id: String,
    pub label: String,
    pub href: String,
    #[serde(default = "default_visible")]
    pub visible: bool,
    pub order_index: i32,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            default_items: vec![
                SidebarItem {
                    public_id: "home".to_string(),
                    label: "Home".to_string(),
                    href: "/posts".to_string(),
                    visible: true,
                    order_index: 0,
                },
                SidebarItem {
                    public_id: "inbox".to_string(),
                    label: "Inbox".to_string(),
                    href: "/inbox".to_string(),
                    visible: true,
                    order_index: 1,
                },
                SidebarItem {
                    public_id: "docs".to_string(),
                    label: "Docs".to_string(),
                    href: "/notes".to_string(),
                    visible: true,
                    order_index: 2,
                },
            ],
        }
    }
}

fn default_visible() -> bool {
    true
}
