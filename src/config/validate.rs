use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use toml::Value;
use url::Url;

use super::{
    ArtifactGcConfig, BlameConfig, BuckConfig, CedarConfig, Config, DbConfig, GitConfig, LFSConfig,
    LogConfig, MonoConfig, NOTIFICATION_DELIVERY_MODES, NotificationConfig, OAuthConfig,
    PackConfig, PushAuth, PushPolicy, RedisConfig, VAULT_AUDIT_SINKS, VaultConfig,
    normalize_token_path,
    secret::{SecretRef, is_secret_ref_value},
};
use crate::common::errors::MegaError;
#[rustfmt::skip]
use crate::orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig};

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
        validate_git_config(&self.git)?;
        validate_pack_config(&self.pack)?;
        validate_blame_config(&self.blame)?;
        validate_lfs_config(&self.lfs)?;
        validate_redis_config(&self.redis)?;
        validate_cedar_config(&self.cedar)?;
        if let Some(buck_config) = &self.buck {
            validate_buck_config(buck_config)?;
        }
        validate_object_storage_config(&self.object_storage)?;
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
        reject_legacy_oauth_environment()?;
        reject_legacy_mail_environment()?;
        validate_trunk_config_surface(self)?;
        validate_agent_capture_config(self)?;
        validate_storage_events_config(self)?;

        Ok(())
    }
}

/// Validate `[oauth]` settings: website API base URL, session cookie names, and
/// each CORS origin must be a browser Origin of the form `scheme://host[:port]`
/// (http/https, no path/query/fragment) that also parses as an HTTP header value
/// — i.e. exactly what the server's `CorsLayer` accepts at runtime, so a
/// configured origin can never pass validation yet be silently dropped by the
/// CORS layer.
pub(crate) fn validate_oauth_config(config: &OAuthConfig) -> Result<(), MegaError> {
    validate_oauth_website_api_base_url(config)?;
    validate_oauth_session_cookie_names(config)?;
    for origin in &config.allowed_cors_origins {
        validate_cors_origin(origin)?;
    }
    Ok(())
}

/// Fail-closed guard for `service http`: requires `[oauth]` with a non-empty
/// `website_api_base_url`.
pub fn require_oauth_for_http_service(config: &Config) -> Result<(), MegaError> {
    if config.git.storage_only() {
        return Ok(());
    }
    let oauth = config.oauth.as_ref().ok_or_else(|| {
        MegaError::Other(
            "service http requires [oauth] with oauth.website_api_base_url (see docs/refactoring/website-auth.md)".to_string(),
        )
    })?;
    validate_oauth_website_api_base_url(oauth)
}

fn validate_oauth_website_api_base_url(config: &OAuthConfig) -> Result<(), MegaError> {
    let value = config.website_api_base_url.trim();
    if value.is_empty() {
        return Err(MegaError::Other(
            "oauth.website_api_base_url must not be empty (required for website session introspection; see docs/refactoring/website-auth.md)".to_string(),
        ));
    }
    let parsed = Url::parse(value).map_err(|e| {
        MegaError::Other(format!(
            "oauth.website_api_base_url `{value}` is not a valid URL: {e}"
        ))
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(MegaError::Other(format!(
            "oauth.website_api_base_url `{value}` must use the http or https scheme"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MegaError::Other(format!(
            "oauth.website_api_base_url `{value}` must not contain userinfo (use scheme://host[:port])"
        )));
    }
    match parsed.host() {
        Some(url::Host::Domain("")) => {
            return Err(MegaError::Other(format!(
                "oauth.website_api_base_url `{value}` must include a non-empty host"
            )));
        }
        Some(_) => {}
        None => {
            return Err(MegaError::Other(format!(
                "oauth.website_api_base_url `{value}` must include a host (use scheme://host[:port])"
            )));
        }
    }
    let path_ok = parsed.path().is_empty() || parsed.path() == "/";
    if !path_ok || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(MegaError::Other(format!(
            "oauth.website_api_base_url `{value}` must not contain a path, query, or fragment (use scheme://host[:port])"
        )));
    }
    Ok(())
}

/// Hard-reject removed Campsite/Tinyship oauth env overrides (AU-02). Unknown
/// `MEGA_*` vars otherwise only warn unless `--deny-warnings`; these three must
/// fail ordinary `config validate` / startup validation.
fn reject_legacy_oauth_environment() -> Result<(), MegaError> {
    const LEGACY: &[&str] = &[
        "MEGA_OAUTH__CAMPSITE_API_DOMAIN",
        "MEGA_OAUTH__TINYSHIP_API_DOMAIN",
        "MEGA_OAUTH__API_STORE_BACKEND",
    ];
    for variable in LEGACY {
        if std::env::var_os(variable).is_some() {
            return Err(MegaError::Other(format!(
                "{variable} is removed; use MEGA_OAUTH__WEBSITE_API_BASE_URL (see docs/refactoring/website-auth.md)"
            )));
        }
    }
    Ok(())
}

/// Hard-reject removed SMTP `[mail]` env overrides (MN-03). Unknown `MEGA_*`
/// vars otherwise only warn unless `--deny-warnings`; any `MEGA_MAIL__*` must
/// fail ordinary `config validate` / config load / startup.
pub(crate) fn reject_legacy_mail_environment() -> Result<(), MegaError> {
    const PREFIX: &str = "MEGA_MAIL__";
    let mut found: Vec<String> = std::env::vars_os()
        .filter_map(|(key, _)| {
            let key = key.to_string_lossy();
            if key.starts_with(PREFIX) {
                Some(key.into_owned())
            } else {
                None
            }
        })
        .collect();
    found.sort();
    if let Some(variable) = found.first() {
        return Err(MegaError::Other(format!(
            "{variable} is removed; product email is delivered via website (see docs/refactoring/website-mail.md). Unset all MEGA_MAIL__* variables"
        )));
    }
    Ok(())
}

fn validate_oauth_session_cookie_names(config: &OAuthConfig) -> Result<(), MegaError> {
    if config.session_cookie_names.is_empty() {
        return Err(MegaError::Other(
            "oauth.session_cookie_names must not be empty".to_string(),
        ));
    }
    for (index, name) in config.session_cookie_names.iter().enumerate() {
        if name.trim().is_empty() {
            return Err(MegaError::Other(format!(
                "oauth.session_cookie_names[{index}] must not be empty"
            )));
        }
        if name.chars().any(|c| c.is_control()) {
            return Err(MegaError::Other(format!(
                "oauth.session_cookie_names[{index}] must not contain control characters"
            )));
        }
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
    let website_mail_enabled = !config.website_mail_base_url.trim().is_empty();
    // A present-but-blank bearer is not a configured bearer. `is_some()` alone
    // let `MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER=` (or a whitespace value) pass
    // validation, producing a service that boots healthy and then 401s on every
    // product email forever — the website side never matches `Bearer <empty>`,
    // and the failure is only a `warn!` in service.rs. Treat blank as unset so
    // it falls into the "exactly one of ... is required" branch below.
    let website_mail_bearer_count = usize::from(
        config
            .website_mail_bearer
            .as_ref()
            .is_some_and(|bearer| !bearer.expose_secret().trim().is_empty()),
    ) + usize::from(config.website_mail_bearer_ref.is_some());
    if website_mail_enabled {
        let parsed = Url::parse(&config.website_mail_base_url).map_err(|_| {
            MegaError::Other(
                "notification.website_mail_base_url must be a valid HTTP(S) URL".to_string(),
            )
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(MegaError::Other(
                "notification.website_mail_base_url must be a valid HTTP(S) URL".to_string(),
            ));
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(MegaError::Other(
                "notification.website_mail_base_url must not include a path".to_string(),
            ));
        }
        if website_mail_bearer_count != 1 {
            return Err(MegaError::Other(
                "exactly one of notification.website_mail_bearer or notification.website_mail_bearer_ref is required when notification.website_mail_base_url is set".to_string(),
            ));
        }
        if let Some(secret_ref) = &config.website_mail_bearer_ref {
            validate_config_secret_ref(
                "notification.website_mail_bearer_ref",
                secret_ref,
                "notification/website_mail/bearer",
            )?;
        }
    } else if website_mail_bearer_count != 0 {
        return Err(MegaError::Other(
            "notification.website_mail_base_url is required when a website mail bearer is configured"
                .to_string(),
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
/// `config/<profile>/<suffix>` namespace for notification channel credentials.
/// The SecretRef value is never logged.
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
    mono_config.object_hash_kind()?;
    require_non_empty_path("monorepo.import_dir", &mono_config.import_dir)?;
    if mono_config.root_dirs.is_empty() {
        return Err(MegaError::Other(
            "monorepo.root_dirs must contain at least one root directory".to_string(),
        ));
    }
    require_non_empty_list_entries("monorepo.root_dirs", &mono_config.root_dirs)?;
    require_non_empty_list_entries("monorepo.admin", &mono_config.admin)?;

    // ADR-UN-06 ⑤: the anonymous fallback principal is a reserved literal; it
    // must not be occupied by a real admin name (collision would let an
    // anonymous request inherit admin privileges).
    if mono_config
        .admin
        .iter()
        .any(|name| name == "User::\"__anonymous__\"")
    {
        return Err(MegaError::Other(
            "monorepo.admin must not contain the reserved anonymous principal User::\"__anonymous__\"".to_string(),
        ));
    }

    if mono_config.rename.similarity_threshold > 100 {
        return Err(MegaError::Other(format!(
            "monorepo.rename.similarity_threshold must be between 0 and 100; got {}",
            mono_config.rename.similarity_threshold
        )));
    }

    if mono_config.max_push_commits == 0 {
        return Err(MegaError::Other(
            "monorepo.max_push_commits must be greater than 0".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_git_config(git: &GitConfig) -> Result<(), MegaError> {
    let mut names = BTreeSet::new();
    for (idx, token) in git.push_tokens.iter().enumerate() {
        let field = format!("git.push_tokens[{idx}]");
        if token.name.trim().is_empty() {
            return Err(MegaError::Other(format!("{field}.name must not be empty")));
        }
        if !names.insert(token.name.as_str()) {
            return Err(MegaError::Other(format!(
                "{field}.name {:?} is duplicated",
                token.name
            )));
        }
        if token.token.trim().is_empty() {
            return Err(MegaError::Other(format!("{field}.token must not be empty")));
        }
        if is_secret_ref_value(&token.token) {
            let field_token = format!("{field}.token");
            let secret_ref = parse_secret_ref_for_field(&field_token, &token.token)?;
            validate_config_secret_ref(
                &field_token,
                &secret_ref,
                &format!("git/push_tokens/{}", token.name),
            )?;
        }
        if let Some(paths) = &token.paths {
            for path in paths {
                validate_token_auth_path(&format!("{field}.paths"), path)?;
            }
        }
    }
    if git.push_auth == Some(PushAuth::Token) && git.push_tokens.is_empty() {
        return Err(MegaError::Other(
            "git.push_auth=token requires at least one [[git.push_tokens]] entry".to_string(),
        ));
    }
    Ok(())
}

fn validate_token_auth_path(field_path: &str, path: &str) -> Result<(), MegaError> {
    if path.trim().is_empty() {
        return Err(MegaError::Other(format!(
            "{field_path} must not contain an empty path"
        )));
    }
    if path.contains("//") {
        return Err(MegaError::Other(format!(
            "{field_path} {path:?} must not contain empty path components"
        )));
    }
    let normalized = normalize_token_path(path);
    if !path.trim().starts_with('/') {
        return Err(MegaError::Other(format!(
            "{field_path} {path:?} must start with '/' (component-boundary prefix)"
        )));
    }
    if path.trim().ends_with('/') && normalized != "/" {
        return Err(MegaError::Other(format!(
            "{field_path} {path:?} must not have a trailing slash"
        )));
    }
    Ok(())
}

/// Fail-closed checks ①④⑤ (no DB). ②③⑥ run at HTTP start.
pub(crate) fn validate_trunk_config_surface(config: &Config) -> Result<(), MegaError> {
    if config.monorepo.push_policy == PushPolicy::Trunk && config.cedar.enforcement != "off" {
        return Err(MegaError::Other(format!(
            "push_policy=trunk requires cedar.enforcement=\"off\" (got {:?}); trunk has no Cedar authorization gate",
            config.cedar.enforcement
        )));
    }
    match &config.git.push_auth {
        Some(auth) if config.monorepo.push_policy != PushPolicy::Trunk => {
            return Err(MegaError::Other(format!(
                "git.push_auth=\"{}\" requires monorepo.push_policy=\"trunk\"",
                auth.as_str()
            )));
        }
        None if config.monorepo.push_policy == PushPolicy::Trunk => {
            return Err(MegaError::Other(
                "push_policy=trunk requires an explicit git.push_auth of \"token\" or \"none\""
                    .to_string(),
            ));
        }
        _ => {}
    }
    if config.git.storage_only() && config.git.ssh_receive_pack != Some(false) {
        return Err(MegaError::Other(
            "git.push_auth requires git.ssh_receive_pack=false (SSH receive-pack must be explicitly disabled for storage-only)".to_string(),
        ));
    }
    if config.oci.enabled && !config.git.storage_only() {
        return Err(MegaError::Other(
            "[oci] enabled=true requires git.push_auth (storage-only); the OCI \
             Distribution surface is only mounted in storage-only deployments"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_agent_capture_config(config: &Config) -> Result<(), MegaError> {
    let capture = &config.agent_capture;
    if capture.enabled && !config.git.storage_only() {
        return Err(MegaError::Other(
            "[agent_capture] enabled=true requires git.push_auth (storage-only); \
             the /api/v1/agent-capture surface is only mounted in storage-only deployments"
                .to_string(),
        ));
    }
    if capture.enabled && capture.ingest_tokens.is_empty() {
        return Err(MegaError::Other(
            "[agent_capture] enabled=true requires at least one [[agent_capture.ingest_tokens]] entry"
                .to_string(),
        ));
    }
    for token in &capture.ingest_tokens {
        if let Some(token_tenant) = &token.tenant_id
            && token_tenant != &capture.tenant_id
        {
            return Err(MegaError::Other(
                "[[agent_capture.ingest_tokens]].tenant_id must equal [agent_capture].tenant_id"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_storage_events_config(config: &Config) -> Result<(), MegaError> {
    let events = &config.storage_events;
    if events.enabled && !config.git.storage_only() {
        return Err(MegaError::Other(
            "[storage_events] enabled=true requires git.push_auth (storage-only); \
             the committed-write emitter is only available in storage-only deployments"
                .to_string(),
        ));
    }

    match events.installation_id.as_deref() {
        Some(id) if !is_storage_events_ascii_id(id, 1, 64) => {
            return Err(MegaError::Other(
                "[storage_events] installation_id must be 1..64 ASCII [A-Za-z0-9_-]".to_string(),
            ));
        }
        None if events.enabled => {
            return Err(MegaError::Other(
                "[storage_events] enabled=true requires installation_id".to_string(),
            ));
        }
        _ => {}
    }

    let mut seen_ids = std::collections::BTreeSet::new();
    for target in &events.targets {
        if !is_storage_events_ascii_id(&target.id, 1, 32) {
            return Err(MegaError::Other(
                "[[storage_events.targets]] id must be 1..32 ASCII [A-Za-z0-9_-]".to_string(),
            ));
        }
        if !seen_ids.insert(target.id.as_str()) {
            return Err(MegaError::Other(format!(
                "[[storage_events.targets]] id {:?} is duplicated",
                target.id
            )));
        }
        if target.events.is_empty() {
            return Err(MegaError::Other(
                "[[storage_events.targets]] events must be a non-empty list".to_string(),
            ));
        }
        validate_storage_events_target_url(&target.url)?;
    }

    if !(1..=5).contains(&events.connect_timeout_seconds) {
        return Err(MegaError::Other(
            "[storage_events] connect_timeout_seconds must be 1..=5".to_string(),
        ));
    }
    if !(1..=10).contains(&events.request_timeout_seconds) {
        return Err(MegaError::Other(
            "[storage_events] request_timeout_seconds must be 1..=10".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_storage_events_target_url(raw: &str) -> Result<(), MegaError> {
    let url = url::Url::parse(raw).map_err(|err| {
        MegaError::Other(format!(
            "[[storage_events.targets]] url is not a valid URL ({err})"
        ))
    })?;
    if url.scheme() != "https" {
        return Err(MegaError::Other(
            "[[storage_events.targets]] url must use https".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(MegaError::Other(
            "[[storage_events.targets]] url must not contain userinfo".to_string(),
        ));
    }
    if url.query().is_some() {
        return Err(MegaError::Other(
            "[[storage_events.targets]] url must not contain a query".to_string(),
        ));
    }
    if url.fragment().is_some() {
        return Err(MegaError::Other(
            "[[storage_events.targets]] url must not contain a fragment".to_string(),
        ));
    }
    if url.host_str().is_none() {
        return Err(MegaError::Other(
            "[[storage_events.targets]] url must include a host".to_string(),
        ));
    }
    Ok(())
}

fn is_storage_events_ascii_id(value: &str, min: usize, max: usize) -> bool {
    let len = value.len();
    (min..=max).contains(&len)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Validate `[cedar]` settings (ADR-UN-01): `enforcement` must be one of
/// `off` | `shadow` | `enforce`.
pub(crate) fn validate_cedar_config(cedar_config: &CedarConfig) -> Result<(), MegaError> {
    match cedar_config.enforcement.as_str() {
        "off" | "shadow" | "enforce" => Ok(()),
        other => Err(MegaError::Other(format!(
            "cedar.enforcement must be one of \"off\" | \"shadow\" | \"enforce\"; got {other:?}"
        ))),
    }
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

pub(crate) fn validate_redis_config(redis_config: &RedisConfig) -> Result<(), MegaError> {
    require_non_empty("redis.url", &redis_config.url)?;
    let trimmed = redis_config.url.trim_start();
    if is_secret_ref_value(trimmed) {
        let secret_ref = parse_secret_ref_for_field("redis.url", trimmed)?;
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

    let secret_ref = parse_secret_ref_for_field(field_path, trimmed)?;
    validate_config_secret_ref(field_path, &secret_ref, suffix)
}

/// Parse a `vault://` SecretRef and prefix parse failures with `field_path` so
/// `config validate` diagnostics name the offending setting (e.g. `redis.url`).
fn parse_secret_ref_for_field(field_path: &str, value: &str) -> Result<SecretRef, MegaError> {
    SecretRef::parse(value).map_err(|err| match err {
        MegaError::Other(msg) => MegaError::Other(format!("{field_path}: {msg}")),
        other => other,
    })
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

    warnings.extend(unknown_fields(value));

    warnings
}

/// Reject any field that is not in the `known_fields` whitelist, returning a
/// hard error instead of a warning. This is the strict counterpart to
/// `unknown_fields` and is applied during config loading so that typos and
/// obsolete keys fail fast instead of being silently dropped by serde.
///
/// `[oauth]` is a recognized section (`OAuthConfig`) with strongly-typed keys;
/// unknown keys under the section fail like any other section.
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
    !field_path.is_empty() && is_known_field_path(field_path)
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
        "monorepo.admin" | "monorepo.root_dirs" | "oauth.allowed_cors_origins" | "git.push_tokens"
    ) || field_path.starts_with("monorepo.admin[")
        || field_path.starts_with("monorepo.root_dirs[")
        || field_path.starts_with("oauth.allowed_cors_origins[")
        || field_path.starts_with("git.push_tokens[")
}

fn is_sensitive_source_field_path(field_path: &str) -> bool {
    matches!(
        field_path,
        "database.db_url"
            | "redis.url"
            | "object_storage.s3.access_key_id"
            | "object_storage.s3.secret_access_key"
            | "object_storage.s3.endpoint_url"
            | "notification.slack.webhook_url_ref"
            | "notification.webhook.token_ref"
            | "notification.website_mail_bearer"
            | "notification.website_mail_bearer_ref"
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
    let message = if !is_known_field_path(field_path) {
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

fn environment_field_is_ignored(_field_path: &str) -> bool {
    false
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

pub(crate) fn known_fields(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "" => Some(&[
            "base_dir",
            "log",
            "database",
            "monorepo",
            "pack",
            "lfs",
            "object_storage",
            "oauth",
            "blame",
            "redis",
            "buck",
            "artifacts_gc",
            "notification",
            "vault",
            "oauth",
            "git",
            "oci",
            "agent_capture",
            "storage_events",
            "cedar",
        ]),
        "log" => Some(&["level", "print_std", "with_ansi"]),
        "database" => Some(&[
            "db_type",
            "db_url",
            "max_connection",
            "min_connection",
            "acquire_timeout",
            "connect_timeout",
            "sqlx_logging",
        ]),
        "monorepo" => Some(&[
            "import_dir",
            "admin",
            "root_dirs",
            "object_format",
            "rename",
            "push_policy",
            "max_push_commits",
        ]),
        "monorepo.rename" => Some(&["similarity_threshold", "rename_limit"]),
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
        "notification" => Some(&[
            "enabled",
            "default_delivery_mode",
            "default_locale",
            "website_mail_base_url",
            "website_mail_bearer",
            "website_mail_bearer_ref",
            "slack",
            "webhook",
        ]),
        "notification.slack" => Some(&["enabled", "webhook_url_ref"]),
        "notification.webhook" => Some(&["enabled", "url", "token_ref"]),
        "vault" => Some(&["audit"]),
        "vault.audit" => Some(&["enabled", "sink", "file_path", "fail_closed"]),
        // Strongly-typed OAuth / website session settings (OAuthConfig).
        "oauth" => Some(&[
            "allowed_cors_origins",
            "website_api_base_url",
            "session_cookie_names",
        ]),
        "git" => Some(&[
            "anonymous_access",
            "push_auth",
            "push_tokens",
            "ssh_receive_pack",
        ]),
        "git.push_tokens" => Some(&["name", "token", "paths"]),
        "oci" => Some(&["enabled"]),
        "agent_capture" => Some(&[
            "enabled",
            "tenant_id",
            "deployment_id",
            "max_blob_bytes",
            "max_file_blobs_per_session",
            "max_events_per_batch",
            "max_event_bytes",
            "lease_ttl_seconds",
            "ingest_tokens",
        ]),
        "agent_capture.ingest_tokens" => Some(&["name", "token", "paths", "tenant_id"]),
        "storage_events" => Some(&[
            "enabled",
            "installation_id",
            "max_in_flight",
            "connect_timeout_seconds",
            "request_timeout_seconds",
            "shutdown_grace_seconds",
            "targets",
        ]),
        "storage_events.targets" => Some(&[
            "id",
            "url",
            "secret_ref",
            "events",
            "git_paths",
            "oci_repositories",
            "lfs_paths",
            "include_unscoped_lfs",
            "agent_tenants",
            "agent_repo_paths",
        ]),
        "cedar" => Some(&["enforcement"]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        PushAuth, PushPolicy, PushTokenConfig, StorageEventsTargetConfig,
        template::config_init_template, testing::isolated_config,
    };
    #[rustfmt::skip]
    use crate::orbit_api::factory::{GcsConfig, LocalConfig, ObjectStorageBackend, S3Config};

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
    fn config_validate_accepts_website_mail_it_bearer() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            website_mail_base_url: "http://website-next:7001".to_string(),
            website_mail_bearer: Some(crate::config::secret::SecretString::new("it-shared-bearer")),
            ..Default::default()
        });

        config
            .validate()
            .expect("website mail IT configuration should validate");
    }

    #[test]
    fn config_validate_rejects_website_mail_without_single_bearer_source() {
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            website_mail_base_url: "http://website-next:7001".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("website mail must require a bearer source");
        assert!(err.to_string().contains("website_mail_bearer"));
    }

    #[test]
    fn config_validate_rejects_blank_website_mail_bearer() {
        // `MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER=` (empty, or whitespace) used
        // to satisfy the `is_some()` count, so the service booted healthy and
        // then 401'd on every product email forever — a failure that only ever
        // surfaces as a `warn!` in notification::service. A blank bearer is not
        // a configured bearer.
        for blank in ["", "   ", "\t\n"] {
            let mut config = valid_config();
            config.notification = Some(crate::config::NotificationConfig {
                website_mail_base_url: "http://website-next:7001".to_string(),
                website_mail_bearer: Some(crate::config::secret::SecretString::new(blank)),
                ..Default::default()
            });

            let err = config
                .validate()
                .expect_err("a blank website mail bearer must not count as configured");
            let message = err.to_string();
            assert!(
                message.contains("website_mail_bearer"),
                "error should name the bearer field, got: {message}"
            );
            assert!(
                !message.contains(blank) || blank.is_empty(),
                "error must not echo the configured value"
            );
        }
    }

    #[test]
    fn config_validate_rejects_blank_website_mail_bearer_even_without_base_url() {
        // Mirror of the branch at the bottom of the mail block: a bearer with
        // no base_url is an error, but a *blank* bearer alone must not trip it
        // (nothing is configured, so nothing is inconsistent).
        let mut config = valid_config();
        config.notification = Some(crate::config::NotificationConfig {
            website_mail_base_url: String::new(),
            website_mail_bearer: Some(crate::config::secret::SecretString::new("   ")),
            ..Default::default()
        });

        config
            .validate()
            .expect("a blank bearer with no base_url is simply 'mail disabled'");
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
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec![
                "http://localhost:3000".to_string(),
                "https://app.example.com".to_string(),
            ],
            ..Default::default()
        });

        config
            .validate()
            .expect("valid oauth cors origins should validate");
    }

    #[test]
    fn config_validate_rejects_empty_oauth_website_api_base_url() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: String::new(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("empty oauth website_api_base_url should fail");
        assert!(err.to_string().contains("oauth.website_api_base_url"));
    }

    #[test]
    fn config_validate_rejects_invalid_oauth_website_api_base_url() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: "not-a-url".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("invalid oauth website_api_base_url should fail");
        assert!(err.to_string().contains("oauth.website_api_base_url"));
    }

    #[test]
    fn config_validate_rejects_oauth_website_api_base_url_with_path() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: "https://example.com/foo".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("oauth website_api_base_url with path should fail");
        assert!(
            err.to_string().contains("path, query, or fragment"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_validate_rejects_oauth_website_api_base_url_with_userinfo() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: "https://user@example.com".to_string(),
            ..Default::default()
        });

        let err = config
            .validate()
            .expect_err("oauth website_api_base_url with userinfo should fail");
        assert!(
            err.to_string().contains("userinfo"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_validate_rejects_legacy_oauth_environment_variables() {
        use crate::config::testing::{EnvVarGuard, env_lock};

        let lock = env_lock();
        let config = valid_config();
        let _var = EnvVarGuard::set(
            &lock,
            "MEGA_OAUTH__CAMPSITE_API_DOMAIN",
            "http://legacy.example",
        );
        let err = config
            .validate()
            .expect_err("legacy MEGA_OAUTH__CAMPSITE_API_DOMAIN should fail validate");
        assert!(
            err.to_string().contains("MEGA_OAUTH__CAMPSITE_API_DOMAIN"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_validate_rejects_legacy_mail_environment_variables() {
        use crate::config::testing::{EnvVarGuard, env_lock};

        let lock = env_lock();
        let config = valid_config();
        let _var = EnvVarGuard::set(&lock, "MEGA_MAIL__ENABLED", "true");
        let err = config
            .validate()
            .expect_err("legacy MEGA_MAIL__ENABLED should fail validate");
        assert!(
            err.to_string().contains("MEGA_MAIL__ENABLED"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_validate_accepts_default_session_cookie_names() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            ..Default::default()
        });

        config
            .validate()
            .expect("default session cookie names should validate");
        assert_eq!(
            config.oauth.as_ref().unwrap().session_cookie_names,
            vec![
                "better-auth.session_token".to_string(),
                "__Secure-better-auth.session_token".to_string(),
            ]
        );
    }

    #[test]
    fn require_oauth_for_http_service_rejects_missing_oauth_section() {
        let config = valid_config();
        let err = require_oauth_for_http_service(&config)
            .expect_err("service http should require oauth section");
        assert!(err.to_string().contains("service http requires [oauth]"));
    }

    #[test]
    fn require_oauth_for_http_service_skips_when_push_auth_is_set() {
        let mut config = valid_config();
        config.oauth = None;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        require_oauth_for_http_service(&config)
            .expect("storage-only HTTP must not require [oauth]");
    }

    #[test]
    fn require_oauth_for_http_service_rejects_empty_website_api_base_url() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig::default());

        let err = require_oauth_for_http_service(&config)
            .expect_err("service http should reject empty website_api_base_url");
        assert!(err.to_string().contains("oauth.website_api_base_url"));
    }

    #[test]
    fn config_validate_rejects_oauth_origin_with_whitespace() {
        let mut config = valid_config();
        config.oauth = Some(crate::config::OAuthConfig {
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec!["http://has space.example.com".to_string()],
            ..Default::default()
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
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec!["  ".to_string()],
            ..Default::default()
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
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec!["https://app.example.com/callback".to_string()],
            ..Default::default()
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
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec!["ftp://app.example.com".to_string()],
            ..Default::default()
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
                website_api_base_url: "http://127.0.0.1:17001".to_string(),
                allowed_cors_origins: vec![bad.to_string()],
                ..Default::default()
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
            website_api_base_url: "http://127.0.0.1:17001".to_string(),
            allowed_cors_origins: vec!["app.example.com".to_string()],
            ..Default::default()
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
    fn config_validate_accepts_blake3_bootstrap_object_format() {
        let mut config = valid_config();
        config.monorepo.object_format = crate::config::MonoObjectFormat::Blake3;
        config
            .validate()
            .expect("blake3 is valid for bootstrap; normal service is a separate check");
        config
            .monorepo
            .ensure_normal_service_object_format()
            .expect("blake3 normal service is enabled for Libra/git-internal peers");
    }

    #[test]
    fn config_validate_accepts_valid_cedar_enforcement() {
        for mode in ["off", "shadow", "enforce"] {
            let mut config = valid_config();
            config.cedar.enforcement = mode.to_string();
            config
                .validate()
                .unwrap_or_else(|e| panic!("valid enforcement {mode:?} should pass: {e}"));
        }
    }

    #[test]
    fn config_validate_rejects_invalid_cedar_enforcement() {
        let mut config = valid_config();
        config.cedar.enforcement = "on".to_string();
        let err = config
            .validate()
            .expect_err("invalid enforcement should fail");
        assert!(err.to_string().contains("cedar.enforcement"));
    }

    #[test]
    fn config_validate_rejects_admin_anonymous_reserved_word() {
        let mut config = valid_config();
        config.monorepo.admin = vec!["User::\"__anonymous__\"".to_string()];
        let err = config
            .validate()
            .expect_err("reserved anonymous principal in admin should fail");
        assert!(err.to_string().contains("__anonymous__"));
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
    fn config_validate_rejects_redis_url_secret_ref_missing_field_suffix() {
        let mut config = valid_config();
        config.redis.url = "vault://secret/config/test/redis/url".to_string();

        let err = config
            .validate()
            .expect_err("redis.url SecretRef without #field should fail");
        let message = err.to_string();
        assert!(message.contains("redis.url"));
        assert!(message.contains("secret ref must include a #field suffix"));
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
    fn config_validate_accepts_omitted_unused_object_storage_sections() {
        let mut config = valid_config();
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3Compatible,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "monoengine".to_string(),
                access_key_id: "ak".to_string(),
                secret_access_key: "sk".to_string(),
                endpoint_url: "http://127.0.0.1:9000".to_string(),
            },
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "s3compatible needs only [object_storage.s3]"
        );

        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: "/tmp/objects".to_string(),
            },
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "local needs only [object_storage.local]"
        );

        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Gcs,
            gcs: GcsConfig {
                bucket: "monoengine".to_string(),
            },
            ..Default::default()
        };
        assert!(
            config.validate().is_ok(),
            "gcs needs only [object_storage.gcs]"
        );
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
    fn reject_unknown_fields_rejects_removed_sidebar_section() {
        let value = toml::from_str::<Value>(
            r#"
            [sidebar]
            default_items = [
                { public_id = "home", label = "Home", href = "/posts", order_index = 0 },
            ]
            "#,
        )
        .unwrap();

        let err = reject_unknown_fields(&value).expect_err("removed [sidebar] must fail closed");
        assert!(err.to_string().contains("sidebar"), "{}", err);
    }

    #[test]
    fn reject_unknown_fields_rejects_removed_orion_sections() {
        let value = toml::from_str::<Value>(
            r#"
            [build]
            enable_build = true
            orion_server = "https://orion.example.test"

            [orion_server]
            port = 8004
            "#,
        )
        .unwrap();

        let err = reject_unknown_fields(&value)
            .expect_err("removed [build]/[orion_server] must fail closed");
        let message = err.to_string();
        assert!(message.contains("build"), "{message}");
        assert!(message.contains("orion_server"), "{message}");
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
            [git]
            push_auth = "token"
            [[git.push_tokens]]
            name = "ci"
            token = "base-token"
            paths = ["/project"]
            "#,
        )
        .expect("write base config");
        std::fs::write(
            &profile_path,
            r#"
            [git]
            push_auth = "token"
            [[git.push_tokens]]
            name = "ci"
            token = "profile-token"
            paths = ["/project"]
            [[git.push_tokens]]
            name = "bot"
            token = "bot-token"
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
        assert!(fields.contains(&"git.push_tokens[0].name"));
        assert!(fields.contains(&"git.push_tokens[0].token"));
        assert!(fields.contains(&"git.push_tokens[0].paths"));
        assert!(fields.contains(&"git.push_tokens[1].name"));
        assert!(fields.contains(&"git.push_tokens[1].token"));
        assert!(fields.contains(&"git.push_tokens"));

        // The first element's token is overridden by the profile.
        assert!(overrides.contains(&"git.push_tokens[0].token"));

        // Array-replace note applies to element paths too.
        assert!(diagnostics.source_overrides.iter().any(|source_override| {
            source_override.field_path == "git.push_tokens[0].token"
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
    fn known_config_field_path_accepts_nested_fields_and_rejects_orphans() {
        assert!(is_known_field_path("database.db_url"));
        assert!(!is_known_field_path("database.db_path"));
        assert!(!is_known_field_path("sidebar.default_items"));
        assert!(is_known_field_path("object_storage.s3.access_key_id"));
        assert!(is_known_field_path("git.push_tokens.name"));
        assert!(is_known_field_path("notification.enabled"));
        assert!(is_known_field_path("notification.default_delivery_mode"));
        assert!(is_known_field_path("notification.default_locale"));
        assert!(!is_known_field_path("database.db_url.extra"));
        assert!(!is_known_field_path("database.typo"));
        assert!(!is_known_field_path("unknown.value"));
        assert!(is_known_field_path("oauth.allowed_cors_origins"));
        assert!(is_known_field_path("oauth.website_api_base_url"));
        assert!(is_known_field_path("oauth.session_cookie_names"));
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
    fn reject_unknown_fields_accepts_monorepo_push_policy() {
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"
            [database]
            db_url = "postgres://localhost:5432/mono"
            [monorepo]
            import_dir = "/third-party"
            admin = ["admin"]
            root_dirs = ["project"]
            push_policy = "trunk"
            "#,
        )
        .unwrap();
        assert!(reject_unknown_fields(&value).is_ok());
    }

    #[test]
    fn reject_unknown_fields_accepts_oci_enabled() {
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"
            [database]
            db_url = "postgres://localhost:5432/mono"
            [monorepo]
            import_dir = "/third-party"
            admin = ["admin"]
            root_dirs = ["project"]
            [oci]
            enabled = true
            "#,
        )
        .unwrap();
        assert!(reject_unknown_fields(&value).is_ok());
    }

    #[test]
    fn reject_unknown_fields_rejects_unknown_agent_capture_key() {
        assert!(
            known_fields("")
                .expect("root schema")
                .contains(&"agent_capture")
        );
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"
            [database]
            db_url = "postgres://localhost:5432/mono"
            [monorepo]
            import_dir = "/third-party"
            admin = ["admin"]
            root_dirs = ["project"]
            [agent_capture]
            unexpected = true
            "#,
        )
        .unwrap();
        let err =
            reject_unknown_fields(&value).expect_err("unknown agent_capture key must fail closed");
        assert!(err.to_string().contains("unexpected"), "{err}");
    }

    #[test]
    fn reject_unknown_fields_rejects_unknown_monorepo_key() {
        let value = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"
            [database]
            db_url = "postgres://localhost:5432/mono"
            [monorepo]
            import_dir = "/third-party"
            admin = ["admin"]
            root_dirs = ["project"]
            leftover_writer = "queue"
            "#,
        )
        .unwrap();
        let err = reject_unknown_fields(&value).expect_err("unknown monorepo key");
        assert!(err.to_string().contains("leftover_writer"), "{err}");
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

            [[git.push_tokens]]
            name = "ci"
            token = "x"
            unexpected = true
            "#,
        )
        .unwrap();

        let err = reject_unknown_fields(&value).expect_err("should reject unknown fields");
        let message = err.to_string();
        assert!(message.contains("unknown_root"));
        assert!(message.contains("database.typo"));
        assert!(message.contains("object_storage.s3.unexpected"));
        assert!(message.contains("git.push_tokens[0].unexpected"));
    }

    #[test]
    fn reject_unknown_fields_rejects_removed_database_db_path() {
        let value = toml::from_str::<Value>(
            r#"
            [database]
            db_type = "postgres"
            db_path = ""
            db_url = "postgres://localhost:5432/mono"
            "#,
        )
        .unwrap();

        let err =
            reject_unknown_fields(&value).expect_err("removed database.db_path must fail closed");
        assert!(err.to_string().contains("database.db_path"), "{}", err);
    }

    #[test]
    fn reject_unknown_fields_rejects_legacy_oauth_keys() {
        let legacy_key = ["campsite", "api", "domain"].join("_");
        let legacy = toml::from_str::<Value>(&format!(
            r#"
            base_dir = "/tmp"

            [oauth]
            {legacy_key} = "http://example.test"
            website_api_base_url = "http://127.0.0.1:17001"
            "#
        ))
        .unwrap();
        let err = reject_unknown_fields(&legacy).expect_err("legacy oauth keys should fail");
        assert!(err.to_string().contains(&format!("oauth.{legacy_key}")));
    }

    #[test]
    fn reject_unknown_fields_accepts_known_oauth_keys_but_rejects_unknown_ones() {
        let ok = toml::from_str::<Value>(
            r#"
            base_dir = "/tmp"

            [oauth]
            website_api_base_url = "http://127.0.0.1:17001"
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

    fn storage_only_none() -> Config {
        let mut config = valid_config();
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.cedar.enforcement = "off".to_string();
        config
    }

    fn storage_only_token() -> Config {
        let mut config = storage_only_none();
        config.git.push_auth = Some(PushAuth::Token);
        config.git.push_tokens = vec![PushTokenConfig {
            name: "ops".to_string(),
            token: "literal-for-tests".to_string(),
            paths: None,
        }];
        config
    }

    fn sample_target() -> StorageEventsTargetConfig {
        StorageEventsTargetConfig {
            id: "ops-main".to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: "vault://secret/config/example/storage_events/targets/ops-main/hmac#value"
                .to_string(),
            events: vec!["repo.push".to_string()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        }
    }

    #[test]
    fn storage_events_validation_matrix() {
        let review = valid_config();
        assert!(!review.storage_events.enabled);
        assert!(review.storage_events.targets.is_empty());
        assert_eq!(review.storage_events.max_in_flight, 16);
        assert_eq!(review.storage_events.connect_timeout_seconds, 2);
        assert_eq!(review.storage_events.request_timeout_seconds, 5);
        assert_eq!(review.storage_events.shutdown_grace_seconds, 5);
        review
            .validate()
            .expect("unconfigured storage_events is disabled and valid");

        let mut token = storage_only_token();
        token.storage_events.enabled = true;
        token.storage_events.installation_id = Some("prod-primary-01".to_string());
        token
            .validate()
            .expect("storage-only token may enable storage_events with empty targets");

        let mut none = storage_only_none();
        none.storage_events.enabled = true;
        none.storage_events.installation_id = Some("prod-primary-01".to_string());
        none.storage_events.targets = vec![sample_target()];
        none.validate()
            .expect("storage-only none may enable storage_events");

        let mut review_enabled = valid_config();
        review_enabled.storage_events.enabled = true;
        review_enabled.storage_events.installation_id = Some("prod-primary-01".to_string());
        let err = review_enabled
            .validate()
            .expect_err("review morphology must reject enabled storage_events");
        assert!(err.to_string().contains("[storage_events]"), "{err}");
        assert!(err.to_string().contains("storage-only"), "{err}");

        let mut missing_install = storage_only_none();
        missing_install.storage_events.enabled = true;
        let err = missing_install
            .validate()
            .expect_err("enabled storage_events requires installation_id");
        assert!(err.to_string().contains("installation_id"), "{err}");

        let mut duplicate = valid_config();
        duplicate.storage_events.targets = vec![sample_target(), sample_target()];
        let err = duplicate
            .validate()
            .expect_err("duplicate target ids fail even when disabled");
        assert!(err.to_string().contains("duplicated"), "{err}");

        let mut empty_events = valid_config();
        let mut target = sample_target();
        target.events.clear();
        empty_events.storage_events.targets = vec![target];
        let err = empty_events
            .validate()
            .expect_err("empty events list is rejected when disabled");
        assert!(err.to_string().contains("events"), "{err}");
    }

    #[test]
    fn storage_events_transport_config() {
        crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest",
        )
        .expect("https without userinfo/query/fragment");

        let err = crate::config::validate::validate_storage_events_target_url(
            "http://events.example.invalid/ingest",
        )
        .expect_err("http");
        assert!(err.to_string().contains("https"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://user:pass@events.example.invalid/ingest",
        )
        .expect_err("userinfo");
        assert!(err.to_string().contains("userinfo"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest?x=1",
        )
        .expect_err("query");
        assert!(err.to_string().contains("query"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest#frag",
        )
        .expect_err("fragment");
        assert!(err.to_string().contains("fragment"), "{err}");

        let mut config = valid_config();
        config.storage_events.connect_timeout_seconds = 0;
        let err = config.validate().expect_err("connect timeout lower bound");
        assert!(err.to_string().contains("connect_timeout_seconds"), "{err}");

        let mut config = valid_config();
        config.storage_events.connect_timeout_seconds = 6;
        let err = config.validate().expect_err("connect timeout upper bound");
        assert!(err.to_string().contains("connect_timeout_seconds"), "{err}");

        let mut config = valid_config();
        config.storage_events.request_timeout_seconds = 0;
        let err = config.validate().expect_err("request timeout lower bound");
        assert!(err.to_string().contains("request_timeout_seconds"), "{err}");

        let mut config = valid_config();
        config.storage_events.request_timeout_seconds = 11;
        let err = config.validate().expect_err("request timeout upper bound");
        assert!(err.to_string().contains("request_timeout_seconds"), "{err}");

        let mut config = valid_config();
        config.storage_events.targets = vec![StorageEventsTargetConfig {
            id: "ops-main".to_string(),
            url: "http://events.example.invalid/ingest".to_string(),
            secret_ref: "vault://secret/config/example/storage_events/targets/ops-main/hmac#value"
                .to_string(),
            events: vec!["repo.push".to_string()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        }];
        let err = config
            .validate()
            .expect_err("disabled still rejects illegal URL shape");
        assert!(err.to_string().contains("https"), "{err}");
    }
}
