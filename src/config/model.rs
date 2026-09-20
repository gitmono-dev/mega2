use std::path::PathBuf;

use git_internal::hash::HashKind;
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
    /// MST/2 snapshot surface (specs Mega_ScorpioFS_MST2 00/03/04). Every
    /// capability stays off unless explicitly enabled; resolver endpoints are
    /// registered only when `mst2.enabled` is true.
    #[serde(default)]
    pub mst2: Mst2Config,
    pub redis: RedisConfig,
    #[serde(default)]
    pub buck: Option<BuckConfig>,
    pub object_storage: ObjectStorageConfig,
    /// Background GC for `artifact_objects` rows with no `artifact_set_files` references
    /// (`docs/artifacts-protocol.md` §10.6).
    #[serde(default)]
    pub artifacts_gc: ArtifactGcConfig,
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
    /// OAuth / browser-facing HTTP settings (currently the CORS allow-list).
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// Git protocol settings (docs/refactoring/protocol.md Stage 4).
    #[serde(default)]
    pub git: GitConfig,
    /// OCI Distribution (container registry) settings, plan-20260902.
    /// `enabled = true` requires storage-only (`git.push_auth` set); the
    /// `/v2` protocol surface is only mounted under that combination.
    #[serde(default)]
    pub oci: OciConfig,
    /// Storage-only Agent Capture ingest surface, plan-20260911.
    /// `enabled = true` requires storage-only (`git.push_auth` set) and at
    /// least one `[[agent_capture.ingest_tokens]]` entry.
    #[serde(default)]
    pub agent_capture: AgentCaptureConfig,
    /// Storage-only committed-write outbound emitter, plan-20260912.
    /// `enabled = true` requires storage-only (`git.push_auth` set). Default
    /// disabled; all fields are restart-required. This card is the config
    /// surface only — no delivery until later WH cards bind a runtime.
    #[serde(default)]
    pub storage_events: StorageEventsConfig,
    /// GitHub outbound sync schema (plan-20260916 GS-03). Default disabled;
    /// this card is structure only — no semantic checks and no runtime.
    #[serde(default)]
    pub github_sync: GithubSyncConfig,
    /// Authorization enforcement switch (`[cedar]`), ADR-UN-01. Default `off`
    /// (no build, no consume of authorization data).
    #[serde(default)]
    pub cedar: CedarConfig,
}

/// `[github_sync]` configuration structure (plan-20260916 GS-03 / GS-20).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GithubSyncConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ssh_host: String,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_host_key: String,
    #[serde(default)]
    pub ssh_key_ref: String,
    #[serde(default)]
    pub bindings: Vec<GithubSyncBinding>,
    #[serde(default = "default_github_sync_advertise_timeout_seconds")]
    pub advertise_timeout_seconds: u64,
    #[serde(default = "default_github_sync_send_timeout_seconds")]
    pub send_timeout_seconds: u64,
    #[serde(default = "default_github_sync_report_timeout_seconds")]
    pub report_timeout_seconds: u64,
    #[serde(default = "default_github_sync_exit_timeout_seconds")]
    pub exit_timeout_seconds: u64,
}

pub const DEFAULT_GITHUB_SYNC_ADVERTISE_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_GITHUB_SYNC_SEND_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_GITHUB_SYNC_REPORT_TIMEOUT_SECONDS: u64 = 60;
pub const DEFAULT_GITHUB_SYNC_EXIT_TIMEOUT_SECONDS: u64 = 15;

fn default_github_sync_advertise_timeout_seconds() -> u64 {
    DEFAULT_GITHUB_SYNC_ADVERTISE_TIMEOUT_SECONDS
}

fn default_github_sync_send_timeout_seconds() -> u64 {
    DEFAULT_GITHUB_SYNC_SEND_TIMEOUT_SECONDS
}

fn default_github_sync_report_timeout_seconds() -> u64 {
    DEFAULT_GITHUB_SYNC_REPORT_TIMEOUT_SECONDS
}

fn default_github_sync_exit_timeout_seconds() -> u64 {
    DEFAULT_GITHUB_SYNC_EXIT_TIMEOUT_SECONDS
}

impl Default for GithubSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ssh_host: String::new(),
            ssh_user: String::new(),
            ssh_host_key: String::new(),
            ssh_key_ref: String::new(),
            bindings: Vec::new(),
            advertise_timeout_seconds: DEFAULT_GITHUB_SYNC_ADVERTISE_TIMEOUT_SECONDS,
            send_timeout_seconds: DEFAULT_GITHUB_SYNC_SEND_TIMEOUT_SECONDS,
            report_timeout_seconds: DEFAULT_GITHUB_SYNC_REPORT_TIMEOUT_SECONDS,
            exit_timeout_seconds: DEFAULT_GITHUB_SYNC_EXIT_TIMEOUT_SECONDS,
        }
    }
}

/// One monorepo-path → GitHub remote binding (plan-20260916 GS-03).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct GithubSyncBinding {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub remote: String,
}

/// OCI Distribution (container registry) settings (ADR-DR-01).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct OciConfig {
    /// Mount `/v2` when true AND `git.storage_only()`. Default `false`
    /// (fail-closed: the surface is not registered).
    #[serde(default)]
    pub enabled: bool,
}

/// Storage-only Agent Capture ingest settings (ADR-AC-03).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentCaptureConfig {
    /// Mount `/api/v1/agent-capture` when true AND `git.storage_only()`.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_agent_capture_tenant_id")]
    pub tenant_id: String,
    #[serde(default = "default_agent_capture_deployment_id")]
    pub deployment_id: String,
    #[serde(default = "default_agent_capture_max_blob_bytes")]
    pub max_blob_bytes: u64,
    #[serde(default = "default_agent_capture_max_file_blobs_per_session")]
    pub max_file_blobs_per_session: u32,
    #[serde(default = "default_agent_capture_max_events_per_batch")]
    pub max_events_per_batch: u32,
    #[serde(default = "default_agent_capture_max_event_bytes")]
    pub max_event_bytes: u64,
    #[serde(default = "default_agent_capture_lease_ttl_seconds")]
    pub lease_ttl_seconds: u64,
    #[serde(default)]
    pub ingest_tokens: Vec<AgentCaptureIngestTokenConfig>,
}

impl Default for AgentCaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tenant_id: default_agent_capture_tenant_id(),
            deployment_id: default_agent_capture_deployment_id(),
            max_blob_bytes: default_agent_capture_max_blob_bytes(),
            max_file_blobs_per_session: default_agent_capture_max_file_blobs_per_session(),
            max_events_per_batch: default_agent_capture_max_events_per_batch(),
            max_event_bytes: default_agent_capture_max_event_bytes(),
            lease_ttl_seconds: default_agent_capture_lease_ttl_seconds(),
            ingest_tokens: Vec::new(),
        }
    }
}

fn default_agent_capture_tenant_id() -> String {
    "default".to_string()
}

fn default_agent_capture_deployment_id() -> String {
    "default".to_string()
}

fn default_agent_capture_max_blob_bytes() -> u64 {
    16_777_216
}

fn default_agent_capture_max_file_blobs_per_session() -> u32 {
    20
}

fn default_agent_capture_max_events_per_batch() -> u32 {
    500
}

fn default_agent_capture_max_event_bytes() -> u64 {
    1_048_576
}

fn default_agent_capture_lease_ttl_seconds() -> u64 {
    900
}

/// Storage-only committed-write outbound emitter settings (ADR-WH-01).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StorageEventsConfig {
    /// Attempt outbound delivery when true AND `git.storage_only()`.
    #[serde(default)]
    pub enabled: bool,
    /// Opaque installation namespace for `repo.push` event ids. Required
    /// when `enabled=true`; never auto-generated.
    #[serde(default)]
    pub installation_id: Option<String>,
    #[serde(default = "default_storage_events_max_in_flight")]
    pub max_in_flight: u32,
    #[serde(default = "default_storage_events_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_storage_events_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    #[serde(default = "default_storage_events_shutdown_grace_seconds")]
    pub shutdown_grace_seconds: u64,
    #[serde(default)]
    pub targets: Vec<StorageEventsTargetConfig>,
}

impl Default for StorageEventsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            installation_id: None,
            max_in_flight: default_storage_events_max_in_flight(),
            connect_timeout_seconds: default_storage_events_connect_timeout_seconds(),
            request_timeout_seconds: default_storage_events_request_timeout_seconds(),
            shutdown_grace_seconds: default_storage_events_shutdown_grace_seconds(),
            targets: Vec::new(),
        }
    }
}

fn default_storage_events_max_in_flight() -> u32 {
    16
}

fn default_storage_events_connect_timeout_seconds() -> u64 {
    2
}

fn default_storage_events_request_timeout_seconds() -> u64 {
    5
}

fn default_storage_events_shutdown_grace_seconds() -> u64 {
    5
}

/// One static outbound target. `secret_ref` is stored opaque here; WH-11
/// resolves it at startup when enabled.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StorageEventsTargetConfig {
    pub id: String,
    pub url: String,
    pub secret_ref: String,
    pub events: Vec<String>,
    #[serde(default)]
    pub git_paths: Vec<String>,
    #[serde(default)]
    pub oci_repositories: Vec<String>,
    #[serde(default)]
    pub lfs_paths: Vec<String>,
    #[serde(default)]
    pub include_unscoped_lfs: bool,
    #[serde(default)]
    pub agent_tenants: Vec<String>,
    #[serde(default)]
    pub agent_repo_paths: Vec<String>,
}

/// Independent ingest token for Agent Capture (not `[[git.push_tokens]]`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentCaptureIngestTokenConfig {
    pub name: String,
    pub token: String,
    /// Prefix authorization via `token_path_authorizes`. Omitted or empty =
    /// whole repository.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// If set, must equal `[agent_capture].tenant_id`.
    #[serde(default)]
    pub tenant_id: Option<String>,
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
            db_url: String::from("postgres://localhost:5432/mega"),
            max_connection: 16,
            min_connection: 8,
            acquire_timeout: 5,
            connect_timeout: 5,
            sqlx_logging: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MonoObjectFormat {
    #[default]
    #[serde(alias = "sha-1")]
    Sha1,
    #[serde(alias = "sha-256")]
    Sha256,
    Blake3,
}

impl MonoObjectFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    pub fn hash_kind(self) -> Result<HashKind, MegaError> {
        match self {
            Self::Sha1 => Ok(HashKind::Sha1),
            Self::Sha256 => Ok(HashKind::Sha256),
            Self::Blake3 => Ok(HashKind::Blake3),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PushPolicy {
    /// Review morphology: push does not enter MonoWriteQueue (CL pipeline).
    #[default]
    Review,
    /// Trunk morphology: push enters MonoWriteQueue (test switch in TP-03;
    /// full protocol wiring is later cards).
    Trunk,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MonoConfig {
    pub import_dir: PathBuf,
    pub admin: Vec<String>,
    pub root_dirs: Vec<String>,
    #[serde(default)]
    pub object_format: MonoObjectFormat,
    #[serde(default)]
    pub rename: RenameConfig,
    /// Runtime push morphology. Default `review` keeps existing CL semantics
    /// (hard constraint 8). Restart-required; fail-closed startup checks
    /// live in [`crate::config::validate`].
    #[serde(default)]
    pub push_policy: PushPolicy,
    /// Trunk-only first-parent chain bound (ADR-TP-17). Review morphology
    /// keeps [`crate::ceres::merge_checker::MAX_CL_CHAIN_COMMITS`].
    #[serde(default = "default_max_push_commits")]
    pub max_push_commits: usize,
}

pub const DEFAULT_MAX_PUSH_COMMITS: usize = 250;

fn default_max_push_commits() -> usize {
    DEFAULT_MAX_PUSH_COMMITS
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
            object_format: MonoObjectFormat::default(),
            rename: RenameConfig::default(),
            push_policy: PushPolicy::default(),
            max_push_commits: default_max_push_commits(),
        }
    }
}

impl PushPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Trunk => "trunk",
        }
    }
}

impl MonoConfig {
    pub fn object_hash_kind(&self) -> Result<HashKind, MegaError> {
        self.object_format.hash_kind()
    }

    pub fn ensure_normal_service_object_format(&self) -> Result<(), MegaError> {
        match self.object_format {
            MonoObjectFormat::Sha1 | MonoObjectFormat::Sha256 | MonoObjectFormat::Blake3 => {
                self.object_hash_kind().map(|_| ())
            }
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

pub const DEFAULT_NOTIFICATION_DELIVERY_MODE: &str = "in_app";

/// Global notification subsystem configuration.
///
/// This is the global layer above per-user DB preferences (docs/notification.md
/// phase 5): a global kill switch. Per-user delivery defaults live on the
/// settings row until RM-02D.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NotificationConfig {
    /// Global kill switch. When false notification delivery is gated off
    /// regardless of per-user prefs. Hot-reloadable (read live from config
    /// snapshot by the active notification service).
    #[serde(default = "default_notification_enabled")]
    pub enabled: bool,
    /// Optional generic outbound webhook channel (docs/notification.md phase 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
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

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: default_notification_enabled(),
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

/// OAuth / browser-facing HTTP settings.
///
/// Strongly-typed home for website Better Auth integration and the HTTP CORS
/// allow-list consumed by the API server's `CorsLayer`. See
/// `docs/refactoring/website-auth.md`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    /// Browser origins allowed by CORS (e.g. `https://app.example.com`). Each
    /// entry must be a valid HTTP header value; an empty list keeps the server's
    /// built-in defaults. Overridable via `MEGA_OAUTH__ALLOWED_CORS_ORIGINS`
    /// (comma-separated list).
    #[serde(default)]
    pub allowed_cors_origins: Vec<String>,
    /// Base URL of the website Better Auth API (scheme + host[:port], no path).
    /// Required for `service http`; validated fail-closed by `config validate`.
    #[serde(default)]
    pub website_api_base_url: String,
    /// Session cookie names tried in order when introspecting browser sessions.
    #[serde(default = "default_session_cookie_names")]
    pub session_cookie_names: Vec<String>,
}

fn default_session_cookie_names() -> Vec<String> {
    vec![
        "better-auth.session_token".to_string(),
        "__Secure-better-auth.session_token".to_string(),
    ]
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            allowed_cors_origins: Vec::new(),
            website_api_base_url: String::new(),
            session_cookie_names: default_session_cookie_names(),
        }
    }
}

/// Supported vault audit sinks (docs/vault.md stage H).
pub const VAULT_AUDIT_SINKS: &[&str] = &["tracing", "file"];

/// Secret-access audit settings (docs/vault.md stage H).
///
/// `enabled` defaults to on, so deployments audit by default; setting it false
/// opts out of emitting per-access records. `sink` selects the destination:
/// `"tracing"` (default; the infallible `vault_audit` tracing target) or
/// `"file"` (a durable append-only JSONL log at `file_path`, fsync'd per record).
/// `fail_closed` makes a failed audit-record write fail the secret operation
/// (non-repudiation over availability); it only matters for fallible sinks
/// (`file`) — the `tracing` sink never fails. Audit records carry the operation,
/// logical secret name, outcome and caller only — never the secret value.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VaultAuditConfig {
    #[serde(default = "default_vault_audit_enabled")]
    pub enabled: bool,
    /// `"tracing"` (default) or `"file"`.
    #[serde(default = "default_vault_audit_sink")]
    pub sink: String,
    /// Durable append-only JSONL audit log path; required when `sink = "file"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    /// Fail the secret operation if the audit record cannot be written
    /// (default `false` = fail-open).
    #[serde(default)]
    pub fail_closed: bool,
}

fn default_vault_audit_enabled() -> bool {
    true
}

fn default_vault_audit_sink() -> String {
    "tracing".to_string()
}

impl Default for VaultAuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_vault_audit_enabled(),
            sink: default_vault_audit_sink(),
            file_path: None,
            fail_closed: false,
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

/// MST/2 snapshot feature flags (default off; spec 00 §6 rollout).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Mst2Config {
    /// Master switch for the `/api/v2/snapshots` surface.
    #[serde(default)]
    pub enabled: bool,
    /// Stable deployment identity used in ServingDescriptors (spec 03 §2).
    /// Required when `enabled`; must parse as a UUID.
    #[serde(default)]
    pub instance_uuid: Option<String>,
    /// Master switch for the T05 atomic-publication coordinator
    /// (prepare→publish/outbox). Default off; see spec 09 §9. When off,
    /// the surface stays in its provisional per-tip sequence mode.
    #[serde(default)]
    pub publication_enabled: bool,
    /// Bearer token required on every snapshot endpoint except
    /// `capabilities` (spec 04 §1). Unset keeps the unauthenticated
    /// lab-only mode: local evaluation stacks only, never a deployment.
    #[serde(default)]
    pub auth_token: Option<String>,
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

/// Git protocol settings (docs/refactoring/protocol.md Stage 4).
///
/// Controls whether anonymous (unauthenticated) clients may clone/fetch
/// repositories via upload-pack. When `anonymous_access` is `false`, every
/// upload-pack request must carry a valid Bearer or Basic token (HTTP) or a
/// successful SSH session (review UserStorage publickey; storage-only password
/// is SP-02).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GitConfig {
    /// Allow anonymous (unauthenticated) clone/fetch via upload-pack.
    /// Defaults to `true` (backwards compatible).
    #[serde(default = "default_git_anonymous_access")]
    pub anonymous_access: bool,
    /// Omitted = existing OAuth/UserStorage chain (review-only). Explicit
    /// `"token"` / `"none"` is storage-only and requires `push_policy=trunk`.
    #[serde(default)]
    pub push_auth: Option<PushAuth>,
    /// Static push tokens for `push_auth = "token"`. `paths` omitted means
    /// the whole repository. Restart-required.
    #[serde(default)]
    pub push_tokens: Vec<PushTokenConfig>,
    /// SSH receive-pack. Storage-only (`push_auth` set) requires this to be
    /// explicitly `false`. Omitted in review/OAuth form keeps today's SSH push.
    #[serde(default)]
    pub ssh_receive_pack: Option<bool>,
}

/// Storage-only git HTTP push authentication (TP-15 config surface; TP-19
/// implements the authenticator).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PushAuth {
    Token,
    None,
}

impl PushAuth {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::None => "none",
        }
    }
}

impl GitConfig {
    /// Explicit `push_auth` selects the storage-only HTTP/SSH form.
    pub fn storage_only(&self) -> bool {
        self.push_auth.is_some()
    }

    /// SSH receive-pack is a review/OAuth channel. Storage-only never exposes
    /// it; `ssh_receive_pack = false` also disables it under review.
    pub fn ssh_receive_pack_enabled(&self) -> bool {
        !self.storage_only() && self.ssh_receive_pack != Some(false)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PushTokenConfig {
    pub name: String,
    pub token: String,
    /// Prefix authorization. Omitted or empty = whole repository.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

fn default_git_anonymous_access() -> bool {
    true
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            anonymous_access: default_git_anonymous_access(),
            push_auth: None,
            push_tokens: Vec::new(),
            ssh_receive_pack: None,
        }
    }
}

/// Component-boundary path authorization used by `[[git.push_tokens]].paths`.
/// `/foo` does not authorize `/foobar`.
pub fn token_path_authorizes(authorized: &str, pushed: &str) -> bool {
    let authorized = normalize_token_path(authorized);
    let pushed = normalize_token_path(pushed);
    if authorized == "/" {
        return true;
    }
    pushed == authorized || pushed.starts_with(&format!("{authorized}/"))
}

pub fn normalize_token_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    let mut p = trimmed.replace('\\', "/");
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// `[cedar]` authorization enforcement settings (ADR-UN-01).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CedarConfig {
    /// Three-state enforcement switch: `off` | `shadow` | `enforce`.
    /// `off` = do not build or consume authorization data (default).
    #[serde(default = "default_cedar_enforcement")]
    pub enforcement: String,
}

impl Default for CedarConfig {
    fn default() -> Self {
        Self {
            enforcement: default_cedar_enforcement(),
        }
    }
}

fn default_cedar_enforcement() -> String {
    "off".to_string()
}

#[cfg(test)]
mod tests {
    use git_internal::hash::HashKind;

    use super::{MonoConfig, MonoObjectFormat};

    #[test]
    fn monorepo_object_format_defaults_and_accepts_sha_aliases() {
        let default_config: MonoConfig = toml::from_str(
            r#"
import_dir = "/third-party"
admin = ["admin"]
root_dirs = ["project"]
"#,
        )
        .expect("default object format should deserialize");
        assert_eq!(default_config.object_format, MonoObjectFormat::Sha1);

        for (value, expected) in [
            ("sha1", MonoObjectFormat::Sha1),
            ("sha-1", MonoObjectFormat::Sha1),
            ("sha256", MonoObjectFormat::Sha256),
            ("sha-256", MonoObjectFormat::Sha256),
            ("blake3", MonoObjectFormat::Blake3),
        ] {
            let config: MonoConfig = toml::from_str(&format!(
                r#"
import_dir = "/third-party"
admin = ["admin"]
root_dirs = ["project"]
object_format = "{value}"
"#
            ))
            .unwrap_or_else(|error| panic!("{value:?} should deserialize: {error}"));
            assert_eq!(config.object_format, expected);
        }

        let err = toml::from_str::<MonoConfig>(
            r#"
import_dir = "/third-party"
admin = ["admin"]
root_dirs = ["project"]
object_format = "black3"
"#,
        )
        .expect_err("the misspelled BLAKE3 value must not deserialize");
        assert!(err.to_string().contains("black3"));
    }

    #[test]
    fn b3_03_blake3_hash_kind_ok() {
        let config = MonoConfig {
            object_format: MonoObjectFormat::Blake3,
            ..Default::default()
        };
        assert_eq!(
            config.object_hash_kind().expect("blake3 maps to HashKind"),
            HashKind::Blake3
        );
    }

    #[test]
    fn b3_04_blake3_normal_service_ok() {
        for format in [MonoObjectFormat::Sha256, MonoObjectFormat::Blake3] {
            let config = MonoConfig {
                object_format: format,
                ..Default::default()
            };
            config
                .ensure_normal_service_object_format()
                .unwrap_or_else(|err| {
                    panic!("{format:?} must be servable for Libra/git-internal peers: {err}")
                });
        }
    }
}
