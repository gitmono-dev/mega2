use std::{
    fmt,
    io::ErrorKind,
    mem,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime},
};

use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};

use crate::{
    common::errors::MegaError,
    config::{
        ArtifactGcConfig, BuckConfig, Config, DEFAULT_NOTIFICATION_DELIVERY_MODE, LogConfig,
        NotificationConfig,
    },
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigReloadReport {
    pub applied_fields: Vec<&'static str>,
    pub restart_required_fields: Vec<&'static str>,
}

impl ConfigReloadReport {
    pub fn applied(&self) -> bool {
        !self.applied_fields.is_empty()
    }

    pub fn requires_restart(&self) -> bool {
        !self.restart_required_fields.is_empty()
    }
}

type ConfigReloadCallback =
    dyn Fn(&Config, &ConfigReloadReport) -> Result<(), MegaError> + Send + Sync + 'static;

#[derive(Clone)]
pub struct ConfigReloadSubscriber {
    name: Arc<str>,
    apply: Arc<ConfigReloadCallback>,
    rollback: Arc<ConfigReloadCallback>,
}

impl ConfigReloadSubscriber {
    pub fn new<N, A, R>(name: N, apply: A, rollback: R) -> Self
    where
        N: Into<String>,
        A: Fn(&Config, &ConfigReloadReport) -> Result<(), MegaError> + Send + Sync + 'static,
        R: Fn(&Config, &ConfigReloadReport) -> Result<(), MegaError> + Send + Sync + 'static,
    {
        Self {
            name: Arc::from(name.into()),
            apply: Arc::new(apply),
            rollback: Arc::new(rollback),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn apply(&self, config: &Config, report: &ConfigReloadReport) -> Result<(), MegaError> {
        (self.apply)(config, report)
    }

    fn rollback(&self, config: &Config, report: &ConfigReloadReport) -> Result<(), MegaError> {
        (self.rollback)(config, report)
    }
}

impl fmt::Debug for ConfigReloadSubscriber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigReloadSubscriber")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ConfigHandle {
    current: Arc<RwLock<Arc<Config>>>,
    subscribers: Arc<RwLock<Vec<ConfigReloadSubscriber>>>,
    reload_lock: Arc<Mutex<()>>,
}

impl ConfigHandle {
    pub fn new(config: Config) -> Self {
        Self::from_arc(Arc::new(config))
    }

    pub fn from_arc(config: Arc<Config>) -> Self {
        Self {
            current: Arc::new(RwLock::new(config)),
            subscribers: Arc::new(RwLock::new(Vec::new())),
            reload_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn snapshot(&self) -> Result<Arc<Config>, MegaError> {
        let current = self.current.read().map_err(|_| {
            MegaError::Other("config reload snapshot lock was poisoned".to_string())
        })?;

        Ok(Arc::clone(&current))
    }

    pub fn subscribe(&self, subscriber: ConfigReloadSubscriber) -> Result<(), MegaError> {
        self.subscribers
            .write()
            .map_err(|_| {
                MegaError::Other("config reload subscribers lock was poisoned".to_string())
            })?
            .push(subscriber);

        Ok(())
    }

    pub fn reload(&self, candidate: Config) -> Result<ConfigReloadReport, MegaError> {
        let _reload_guard = self.reload_lock.lock().map_err(|_| {
            MegaError::Other("config reload coordination lock was poisoned".to_string())
        })?;

        candidate.validate()?;

        let current = self.snapshot()?;
        let mut next = current.as_ref().clone();
        let mut report = ConfigReloadReport::default();

        apply_log_changes(&current.log, &candidate.log, &mut next.log, &mut report);
        apply_artifact_gc_changes(
            &current.artifacts_gc,
            &candidate.artifacts_gc,
            &mut next.artifacts_gc,
            &mut report,
        );
        apply_buck_changes(&current.buck, &candidate.buck, &mut next.buck, &mut report);
        apply_notification_changes(
            &current.notification,
            &candidate.notification,
            &mut next.notification,
            &mut report,
        );
        collect_database_restart_fields(&current, &candidate, &mut report);
        collect_redis_restart_fields(&current, &candidate, &mut report);
        collect_static_restart_fields(&current, &candidate, &mut report);

        if report.applied() {
            let next = Arc::new(next);
            let subscribers = self.reload_subscribers()?;
            apply_subscribers(&subscribers, &current, &next, &report)?;

            let mut current = self.current.write().map_err(|_| {
                MegaError::Other("config reload update lock was poisoned".to_string())
            })?;
            *current = next;
        }

        Ok(report)
    }

    pub fn reload_from_path(
        &self,
        path: &Path,
        profile_path: Option<&Path>,
    ) -> Result<ConfigReloadReport, MegaError> {
        let path = path.to_str().ok_or_else(|| {
            MegaError::Other(format!("Config path contains invalid UTF-8: {:?}", path))
        })?;
        let candidate = Config::new_with_profile(path, profile_path)?;

        self.reload(candidate)
    }

    fn reload_subscribers(&self) -> Result<Vec<ConfigReloadSubscriber>, MegaError> {
        let subscribers = self.subscribers.read().map_err(|_| {
            MegaError::Other("config reload subscribers lock was poisoned".to_string())
        })?;

        Ok(subscribers.clone())
    }
}

#[derive(Debug, Clone)]
pub struct ConfigReloadWatcher {
    handle: ConfigHandle,
    config_path: PathBuf,
    profile_path: Option<PathBuf>,
    poll_interval: Duration,
    watched_files: Vec<WatchedConfigFile>,
}

impl ConfigReloadWatcher {
    pub async fn new(
        handle: ConfigHandle,
        config_path: PathBuf,
        profile_path: Option<PathBuf>,
        poll_interval: Duration,
    ) -> Result<Self, MegaError> {
        if poll_interval.is_zero() {
            return Err(MegaError::Other(
                "config reload watcher poll interval must be greater than zero".to_string(),
            ));
        }

        let watched_files = load_watched_files(&config_path, profile_path.as_deref()).await?;

        Ok(Self {
            handle,
            config_path,
            profile_path,
            poll_interval,
            watched_files,
        })
    }

    pub async fn poll_once(&mut self) -> Result<Option<ConfigReloadReport>, MegaError> {
        if !self.refresh_watched_files().await? {
            return Ok(None);
        }

        self.handle
            .reload_from_path(&self.config_path, self.profile_path.as_deref())
            .map(Some)
    }

    pub async fn run_until_shutdown(
        mut self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), MegaError> {
        let mut ticker = interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(Some(report)) => {
                            tracing::info!(
                                applied_fields = ?report.applied_fields,
                                restart_required_fields = ?report.restart_required_fields,
                                "config reload watcher applied changed config"
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "config reload watcher rejected changed config"
                            );
                        }
                    }
                }
            }
        }
    }

    async fn refresh_watched_files(&mut self) -> Result<bool, MegaError> {
        let mut changed = false;

        for watched_file in &mut self.watched_files {
            let signature = file_signature(&watched_file.path).await?;
            if watched_file.signature != signature {
                watched_file.signature = signature;
                changed = true;
            }
        }

        Ok(changed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchedConfigFile {
    path: PathBuf,
    signature: Option<FileSignature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    modified: Option<SystemTime>,
    len: u64,
}

async fn load_watched_files(
    config_path: &Path,
    profile_path: Option<&Path>,
) -> Result<Vec<WatchedConfigFile>, MegaError> {
    let mut watched_files = Vec::with_capacity(if profile_path.is_some() { 2 } else { 1 });
    watched_files.push(WatchedConfigFile {
        path: config_path.to_path_buf(),
        signature: file_signature(config_path).await?,
    });

    if let Some(profile_path) = profile_path {
        watched_files.push(WatchedConfigFile {
            path: profile_path.to_path_buf(),
            signature: file_signature(profile_path).await?,
        });
    }

    Ok(watched_files)
}

async fn file_signature(path: &Path) -> Result<Option<FileSignature>, MegaError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(Some(FileSignature {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn apply_subscribers(
    subscribers: &[ConfigReloadSubscriber],
    current: &Config,
    next: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    let mut applied = Vec::new();

    for subscriber in subscribers {
        if let Err(apply_error) = subscriber.apply(next, report) {
            let rollback_error = rollback_subscribers(subscriber, &applied, current, report);
            let mut message = format!(
                "config reload subscriber '{}' failed: {apply_error}",
                subscriber.name()
            );
            if let Some(rollback_error) = rollback_error {
                message.push_str("; ");
                message.push_str(&rollback_error);
            }

            return Err(MegaError::Other(message));
        }
        applied.push(subscriber.clone());
    }

    Ok(())
}

fn rollback_subscribers(
    failed: &ConfigReloadSubscriber,
    applied: &[ConfigReloadSubscriber],
    current: &Config,
    report: &ConfigReloadReport,
) -> Option<String> {
    let mut rollback_errors = Vec::new();

    for subscriber in std::iter::once(failed).chain(applied.iter().rev()) {
        if let Err(error) = subscriber.rollback(current, report) {
            rollback_errors.push(format!(
                "rollback for config reload subscriber '{}' failed: {error}",
                subscriber.name()
            ));
        }
    }

    if rollback_errors.is_empty() {
        None
    } else {
        Some(rollback_errors.join("; "))
    }
}

fn apply_log_changes(
    current: &LogConfig,
    candidate: &LogConfig,
    next: &mut LogConfig,
    report: &mut ConfigReloadReport,
) {
    if current.level != candidate.level {
        next.level.clone_from(&candidate.level);
        report.applied_fields.push("log.level");
    }
    if current.print_std != candidate.print_std {
        next.print_std = candidate.print_std;
        report.applied_fields.push("log.print_std");
    }
    if current.with_ansi != candidate.with_ansi {
        next.with_ansi = candidate.with_ansi;
        report.applied_fields.push("log.with_ansi");
    }
}

fn apply_artifact_gc_changes(
    current: &ArtifactGcConfig,
    candidate: &ArtifactGcConfig,
    next: &mut ArtifactGcConfig,
    report: &mut ConfigReloadReport,
) {
    if !current.enable && candidate.enable {
        collect_artifact_gc_restart_fields(current, candidate, report);
        return;
    }

    if current.enable != candidate.enable {
        next.enable = candidate.enable;
        report.applied_fields.push("artifacts_gc.enable");
    }
    if current.interval_secs != candidate.interval_secs {
        next.interval_secs = candidate.interval_secs;
        report.applied_fields.push("artifacts_gc.interval_secs");
    }
    if current.grace_secs != candidate.grace_secs {
        next.grace_secs = candidate.grace_secs;
        report.applied_fields.push("artifacts_gc.grace_secs");
    }
    if current.batch_limit != candidate.batch_limit {
        next.batch_limit = candidate.batch_limit;
        report.applied_fields.push("artifacts_gc.batch_limit");
    }
}

fn apply_buck_changes(
    current: &Option<BuckConfig>,
    candidate: &Option<BuckConfig>,
    next: &mut Option<BuckConfig>,
    report: &mut ConfigReloadReport,
) {
    let current = current.clone().unwrap_or_default();
    let candidate = candidate.clone().unwrap_or_default();

    if !current.enable_session_cleanup && candidate.enable_session_cleanup {
        collect_buck_cleanup_restart_fields(&current, &candidate, report);
    } else {
        apply_buck_cleanup_changes(&current, &candidate, next, report);
    }
    collect_buck_restart_fields(&current, &candidate, report);
}

fn apply_buck_cleanup_changes(
    current: &BuckConfig,
    candidate: &BuckConfig,
    next: &mut Option<BuckConfig>,
    report: &mut ConfigReloadReport,
) {
    if current.enable_session_cleanup != candidate.enable_session_cleanup {
        next.get_or_insert_with(BuckConfig::default)
            .enable_session_cleanup = candidate.enable_session_cleanup;
        report.applied_fields.push("buck.enable_session_cleanup");
    }
    if current.cleanup_interval != candidate.cleanup_interval {
        next.get_or_insert_with(BuckConfig::default)
            .cleanup_interval = candidate.cleanup_interval;
        report.applied_fields.push("buck.cleanup_interval");
    }
    if current.completed_retention_days != candidate.completed_retention_days {
        next.get_or_insert_with(BuckConfig::default)
            .completed_retention_days = candidate.completed_retention_days;
        report.applied_fields.push("buck.completed_retention_days");
    }
}

/// Notification delivery settings are read live from the config snapshot.
/// Website mail credentials construct a bounded HTTP client at startup, so
/// changing those fields is accepted into the snapshot but requires restart.
fn apply_notification_changes(
    current: &Option<NotificationConfig>,
    candidate: &Option<NotificationConfig>,
    next: &mut Option<NotificationConfig>,
    report: &mut ConfigReloadReport,
) {
    if current == candidate {
        return;
    }

    let current_enabled = notification_enabled(current);
    let candidate_enabled = notification_enabled(candidate);
    let current_mode = notification_delivery_mode(current);
    let candidate_mode = notification_delivery_mode(candidate);
    let current_locale = notification_default_locale(current);
    let candidate_locale = notification_default_locale(candidate);

    *next = candidate.clone();

    if current_enabled != candidate_enabled {
        report.applied_fields.push("notification.enabled");
    }
    if current_mode != candidate_mode {
        report
            .applied_fields
            .push("notification.default_delivery_mode");
    }
    if current_locale != candidate_locale {
        report.applied_fields.push("notification.default_locale");
    }
    if current.as_ref().map(|config| {
        (
            &config.website_mail_base_url,
            &config.website_mail_bearer,
            &config.website_mail_bearer_ref,
        )
    }) != candidate.as_ref().map(|config| {
        (
            &config.website_mail_base_url,
            &config.website_mail_bearer,
            &config.website_mail_bearer_ref,
        )
    }) {
        report
            .restart_required_fields
            .push("notification.website_mail_base_url");
        report
            .restart_required_fields
            .push("notification.website_mail_bearer");
        report
            .restart_required_fields
            .push("notification.website_mail_bearer_ref");
    }
}

fn notification_enabled(config: &Option<NotificationConfig>) -> bool {
    config.as_ref().map(|c| c.enabled).unwrap_or(true)
}

fn notification_delivery_mode(config: &Option<NotificationConfig>) -> String {
    config
        .as_ref()
        .map(|c| c.default_delivery_mode.clone())
        .unwrap_or_else(|| DEFAULT_NOTIFICATION_DELIVERY_MODE.to_string())
}

fn notification_default_locale(config: &Option<NotificationConfig>) -> String {
    config
        .as_ref()
        .map(|c| c.default_locale.clone())
        .unwrap_or_else(|| "en-US".to_string())
}

fn collect_artifact_gc_restart_fields(
    current: &ArtifactGcConfig,
    candidate: &ArtifactGcConfig,
    report: &mut ConfigReloadReport,
) {
    if current.enable != candidate.enable {
        report.restart_required_fields.push("artifacts_gc.enable");
    }
    if current.interval_secs != candidate.interval_secs {
        report
            .restart_required_fields
            .push("artifacts_gc.interval_secs");
    }
    if current.grace_secs != candidate.grace_secs {
        report
            .restart_required_fields
            .push("artifacts_gc.grace_secs");
    }
    if current.batch_limit != candidate.batch_limit {
        report
            .restart_required_fields
            .push("artifacts_gc.batch_limit");
    }
}

fn collect_buck_cleanup_restart_fields(
    current: &BuckConfig,
    candidate: &BuckConfig,
    report: &mut ConfigReloadReport,
) {
    if current.enable_session_cleanup != candidate.enable_session_cleanup {
        report
            .restart_required_fields
            .push("buck.enable_session_cleanup");
    }
    if current.cleanup_interval != candidate.cleanup_interval {
        report.restart_required_fields.push("buck.cleanup_interval");
    }
    if current.completed_retention_days != candidate.completed_retention_days {
        report
            .restart_required_fields
            .push("buck.completed_retention_days");
    }
}

fn collect_buck_restart_fields(
    current: &BuckConfig,
    candidate: &BuckConfig,
    report: &mut ConfigReloadReport,
) {
    if current.session_timeout != candidate.session_timeout {
        report.restart_required_fields.push("buck.session_timeout");
    }
    if current.max_file_size != candidate.max_file_size {
        report.restart_required_fields.push("buck.max_file_size");
    }
    if current.max_files != candidate.max_files {
        report.restart_required_fields.push("buck.max_files");
    }
    if current.max_concurrent_uploads != candidate.max_concurrent_uploads {
        report
            .restart_required_fields
            .push("buck.max_concurrent_uploads");
    }
    if current.upload_concurrency_limit != candidate.upload_concurrency_limit {
        report
            .restart_required_fields
            .push("buck.upload_concurrency_limit");
    }
    if current.large_file_concurrency_limit != candidate.large_file_concurrency_limit {
        report
            .restart_required_fields
            .push("buck.large_file_concurrency_limit");
    }
    if current.large_file_threshold != candidate.large_file_threshold {
        report
            .restart_required_fields
            .push("buck.large_file_threshold");
    }
}

fn collect_database_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.database.db_type != candidate.database.db_type {
        report.restart_required_fields.push("database.db_type");
    }
    if current.database.db_path != candidate.database.db_path {
        report.restart_required_fields.push("database.db_path");
    }
    if current.database.db_url != candidate.database.db_url {
        report.restart_required_fields.push("database.db_url");
    }
    if current.database.max_connection != candidate.database.max_connection {
        report
            .restart_required_fields
            .push("database.max_connection");
    }
    if current.database.min_connection != candidate.database.min_connection {
        report
            .restart_required_fields
            .push("database.min_connection");
    }
    if current.database.acquire_timeout != candidate.database.acquire_timeout {
        report
            .restart_required_fields
            .push("database.acquire_timeout");
    }
    if current.database.connect_timeout != candidate.database.connect_timeout {
        report
            .restart_required_fields
            .push("database.connect_timeout");
    }
    if current.database.sqlx_logging != candidate.database.sqlx_logging {
        report.restart_required_fields.push("database.sqlx_logging");
    }
}

fn collect_redis_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.redis.url != candidate.redis.url {
        report.restart_required_fields.push("redis.url");
    }
}

fn collect_static_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.base_dir != candidate.base_dir {
        report.restart_required_fields.push("base_dir");
    }

    collect_monorepo_restart_fields(current, candidate, report);
    collect_pack_restart_fields(current, candidate, report);
    collect_lfs_restart_fields(current, candidate, report);
    collect_blame_restart_fields(current, candidate, report);
    collect_build_restart_fields(current, candidate, report);
    collect_object_storage_restart_fields(current, candidate, report);
    collect_orion_server_restart_fields(current, candidate, report);
    collect_sidebar_restart_fields(current, candidate, report);
    collect_oauth_restart_fields(current, candidate, report);
}

fn collect_monorepo_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.monorepo.import_dir != candidate.monorepo.import_dir {
        report.restart_required_fields.push("monorepo.import_dir");
    }
    if current.monorepo.admin != candidate.monorepo.admin {
        report.restart_required_fields.push("monorepo.admin");
    }
    if current.monorepo.root_dirs != candidate.monorepo.root_dirs {
        report.restart_required_fields.push("monorepo.root_dirs");
    }
    if current.monorepo.object_format != candidate.monorepo.object_format {
        report
            .restart_required_fields
            .push("monorepo.object_format");
    }
    if current.monorepo.rename.similarity_threshold
        != candidate.monorepo.rename.similarity_threshold
    {
        report
            .restart_required_fields
            .push("monorepo.rename.similarity_threshold");
    }
    if current.monorepo.rename.rename_limit != candidate.monorepo.rename.rename_limit {
        report
            .restart_required_fields
            .push("monorepo.rename.rename_limit");
    }
}

fn collect_pack_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.pack.pack_decode_mem_size != candidate.pack.pack_decode_mem_size {
        report
            .restart_required_fields
            .push("pack.pack_decode_mem_size");
    }
    if current.pack.pack_decode_disk_size != candidate.pack.pack_decode_disk_size {
        report
            .restart_required_fields
            .push("pack.pack_decode_disk_size");
    }
    if current.pack.pack_decode_cache_path != candidate.pack.pack_decode_cache_path {
        report
            .restart_required_fields
            .push("pack.pack_decode_cache_path");
    }
    if current.pack.clean_cache_after_decode != candidate.pack.clean_cache_after_decode {
        report
            .restart_required_fields
            .push("pack.clean_cache_after_decode");
    }
    if current.pack.channel_message_size != candidate.pack.channel_message_size {
        report
            .restart_required_fields
            .push("pack.channel_message_size");
    }
    if current.pack.save_entry_concurrency != candidate.pack.save_entry_concurrency {
        report
            .restart_required_fields
            .push("pack.save_entry_concurrency");
    }
}

fn collect_lfs_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.lfs.local.lfs_file_path != candidate.lfs.local.lfs_file_path {
        report
            .restart_required_fields
            .push("lfs.local.lfs_file_path");
    }
    if current.lfs.ssh.http_url != candidate.lfs.ssh.http_url {
        report.restart_required_fields.push("lfs.ssh.http_url");
    }
}

fn collect_blame_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.blame.max_lines_threshold != candidate.blame.max_lines_threshold {
        report
            .restart_required_fields
            .push("blame.max_lines_threshold");
    }
    if current.blame.max_size_threshold != candidate.blame.max_size_threshold {
        report
            .restart_required_fields
            .push("blame.max_size_threshold");
    }
    if current.blame.default_chunk_size != candidate.blame.default_chunk_size {
        report
            .restart_required_fields
            .push("blame.default_chunk_size");
    }
    if current.blame.max_commits_in_memory != candidate.blame.max_commits_in_memory {
        report
            .restart_required_fields
            .push("blame.max_commits_in_memory");
    }
    if current.blame.enable_caching != candidate.blame.enable_caching {
        report.restart_required_fields.push("blame.enable_caching");
    }
}

fn collect_build_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if current.build.enable_build != candidate.build.enable_build {
        report.restart_required_fields.push("build.enable_build");
    }
    if current.build.orion_server != candidate.build.orion_server {
        report.restart_required_fields.push("build.orion_server");
    }
    if current.build.orion_preheat_shallow_depth != candidate.build.orion_preheat_shallow_depth {
        report
            .restart_required_fields
            .push("build.orion_preheat_shallow_depth");
    }
}

fn collect_object_storage_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    if mem::discriminant(&current.object_storage.storage_type)
        != mem::discriminant(&candidate.object_storage.storage_type)
    {
        report
            .restart_required_fields
            .push("object_storage.storage_type");
    }
    if current.object_storage.local.root_dir != candidate.object_storage.local.root_dir {
        report
            .restart_required_fields
            .push("object_storage.local.root_dir");
    }
    if current.object_storage.s3.region != candidate.object_storage.s3.region {
        report
            .restart_required_fields
            .push("object_storage.s3.region");
    }
    if current.object_storage.s3.bucket != candidate.object_storage.s3.bucket {
        report
            .restart_required_fields
            .push("object_storage.s3.bucket");
    }
    if current.object_storage.s3.access_key_id != candidate.object_storage.s3.access_key_id {
        report
            .restart_required_fields
            .push("object_storage.s3.access_key_id");
    }
    if current.object_storage.s3.secret_access_key != candidate.object_storage.s3.secret_access_key
    {
        report
            .restart_required_fields
            .push("object_storage.s3.secret_access_key");
    }
    if current.object_storage.s3.endpoint_url != candidate.object_storage.s3.endpoint_url {
        report
            .restart_required_fields
            .push("object_storage.s3.endpoint_url");
    }
    if current.object_storage.gcs.bucket != candidate.object_storage.gcs.bucket {
        report
            .restart_required_fields
            .push("object_storage.gcs.bucket");
    }
}

fn collect_orion_server_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    match (&current.orion_server, &candidate.orion_server) {
        (None, None) => {}
        (None, Some(_)) | (Some(_), None) => report.restart_required_fields.push("orion_server"),
        (Some(current), Some(candidate)) => {
            if current.logger_storage_mode != candidate.logger_storage_mode {
                report
                    .restart_required_fields
                    .push("orion_server.logger_storage_mode");
            }
            if current.build_log_dir != candidate.build_log_dir {
                report
                    .restart_required_fields
                    .push("orion_server.build_log_dir");
            }
            if current.log_stream_buffer != candidate.log_stream_buffer {
                report
                    .restart_required_fields
                    .push("orion_server.log_stream_buffer");
            }
            if current.db_url != candidate.db_url {
                report.restart_required_fields.push("orion_server.db_url");
            }
            if current.port != candidate.port {
                report.restart_required_fields.push("orion_server.port");
            }
            if current.monobase_url != candidate.monobase_url {
                report
                    .restart_required_fields
                    .push("orion_server.monobase_url");
            }
        }
    }
}

fn collect_sidebar_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    let current_items = &current.sidebar.default_items;
    let candidate_items = &candidate.sidebar.default_items;
    if current_items.len() != candidate_items.len()
        || current_items
            .iter()
            .zip(candidate_items)
            .any(|(current, candidate)| {
                current.public_id != candidate.public_id
                    || current.label != candidate.label
                    || current.href != candidate.href
                    || current.visible != candidate.visible
                    || current.order_index != candidate.order_index
            })
    {
        report.restart_required_fields.push("sidebar.default_items");
    }
}

fn collect_oauth_restart_fields(
    current: &Config,
    candidate: &Config,
    report: &mut ConfigReloadReport,
) {
    match (&current.oauth, &candidate.oauth) {
        (None, None) => {}
        (None, Some(_)) | (Some(_), None) => report.restart_required_fields.push("oauth"),
        (Some(current), Some(candidate)) => {
            if current.allowed_cors_origins != candidate.allowed_cors_origins {
                report
                    .restart_required_fields
                    .push("oauth.allowed_cors_origins");
            }
            if current.website_api_base_url != candidate.website_api_base_url {
                report
                    .restart_required_fields
                    .push("oauth.website_api_base_url");
            }
            if current.session_cookie_names != candidate.session_cookie_names {
                report
                    .restart_required_fields
                    .push("oauth.session_cookie_names");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ArtifactGcConfig, BuckConfig,
        template::config_init_template,
        testing::{EnvVarGuard, env_lock, isolated_config},
    };

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn reload_applies_log_fields_and_preserves_restart_required_database_fields() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.log.level = "info".to_string();
        current.log.print_std = true;
        current.database.db_url = "postgres://localhost:5432/current".to_string();
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "debug".to_string();
        candidate.log.print_std = false;
        candidate.database.db_url = "postgres://localhost:5432/restart-required".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");

        assert_eq!(report.applied_fields, vec!["log.level", "log.print_std"]);
        assert_eq!(report.restart_required_fields, vec!["database.db_url"]);
        assert!(report.applied());
        assert!(report.requires_restart());
        assert_eq!(snapshot.log.level, "debug");
        assert!(!snapshot.log.print_std);
        assert_eq!(
            snapshot.database.db_url,
            "postgres://localhost:5432/current"
        );
    }

    #[test]
    fn reload_reports_restart_required_database_change_without_publishing_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.database.db_url = "postgres://localhost:5432/current".to_string();
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.database.db_url = "postgres://localhost:5432/restart-required".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");

        assert!(report.applied_fields.is_empty());
        assert_eq!(report.restart_required_fields, vec!["database.db_url"]);
        assert!(!report.applied());
        assert!(report.requires_restart());
        assert_eq!(
            snapshot.database.db_url,
            "postgres://localhost:5432/current"
        );
    }

    #[test]
    fn reload_reports_static_consumer_fields_as_restart_required_without_publishing_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.blame.enable_caching = true;
        let current_object_root = current.object_storage.local.root_dir.clone();
        let current_lfs_path = current.lfs.local.lfs_file_path.clone();
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.monorepo.root_dirs = vec!["changed-root".to_string()];
        candidate.monorepo.object_format = crate::config::MonoObjectFormat::Sha256;
        candidate.pack.channel_message_size = 2_000_000;
        candidate.lfs.local.lfs_file_path = temp_dir.path().join("candidate-lfs");
        candidate.blame.enable_caching = false;
        candidate.build.orion_preheat_shallow_depth = 8;
        candidate.object_storage.local.root_dir = temp_dir
            .path()
            .join("candidate-objects")
            .to_string_lossy()
            .to_string();
        candidate.object_storage.s3.secret_access_key = "candidate-secret-access-key".to_string();
        candidate.object_storage.gcs.bucket = "candidate-gcs-bucket".to_string();
        candidate.orion_server = Some(Default::default());
        candidate.sidebar.default_items[0].label = "Changed".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");
        let report_debug = format!("{report:?}");

        assert!(report.applied_fields.is_empty());
        assert_eq!(
            report.restart_required_fields,
            vec![
                "monorepo.root_dirs",
                "monorepo.object_format",
                "pack.channel_message_size",
                "lfs.local.lfs_file_path",
                "blame.enable_caching",
                "build.orion_preheat_shallow_depth",
                "object_storage.local.root_dir",
                "object_storage.s3.secret_access_key",
                "object_storage.gcs.bucket",
                "orion_server",
                "sidebar.default_items",
            ]
        );
        assert!(!report.applied());
        assert!(report.requires_restart());
        assert_eq!(snapshot.object_storage.local.root_dir, current_object_root);
        assert_eq!(snapshot.lfs.local.lfs_file_path, current_lfs_path);
        assert_eq!(
            snapshot.monorepo.object_format,
            crate::config::MonoObjectFormat::Sha1
        );
        assert!(snapshot.orion_server.is_none());
        assert_ne!(snapshot.sidebar.default_items[0].label, "Changed");
        assert!(!report_debug.contains("candidate-secret-access-key"));
        assert!(!report_debug.contains("candidate-gcs-bucket"));
        assert!(!report_debug.contains("candidate-objects"));
    }

    #[test]
    fn reload_applies_artifact_gc_runtime_fields() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.artifacts_gc = ArtifactGcConfig {
            enable: true,
            interval_secs: 3600,
            grace_secs: 86_400,
            batch_limit: 100,
        };
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.artifacts_gc = ArtifactGcConfig {
            enable: false,
            interval_secs: 120,
            grace_secs: 600,
            batch_limit: 10,
        };

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");

        assert_eq!(
            report.applied_fields,
            vec![
                "artifacts_gc.enable",
                "artifacts_gc.interval_secs",
                "artifacts_gc.grace_secs",
                "artifacts_gc.batch_limit"
            ]
        );
        assert!(report.restart_required_fields.is_empty());
        assert_eq!(
            snapshot.artifacts_gc,
            ArtifactGcConfig {
                enable: false,
                interval_secs: 120,
                grace_secs: 600,
                batch_limit: 10,
            }
        );
    }

    #[test]
    fn reload_reports_artifact_gc_enable_requires_restart_without_publishing_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.artifacts_gc = ArtifactGcConfig {
            enable: false,
            interval_secs: 3600,
            grace_secs: 86_400,
            batch_limit: 100,
        };
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.artifacts_gc = ArtifactGcConfig {
            enable: true,
            interval_secs: 120,
            grace_secs: 600,
            batch_limit: 10,
        };

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");

        assert!(report.applied_fields.is_empty());
        assert_eq!(
            report.restart_required_fields,
            vec![
                "artifacts_gc.enable",
                "artifacts_gc.interval_secs",
                "artifacts_gc.grace_secs",
                "artifacts_gc.batch_limit"
            ]
        );
        assert_eq!(
            snapshot.artifacts_gc,
            ArtifactGcConfig {
                enable: false,
                interval_secs: 3600,
                grace_secs: 86_400,
                batch_limit: 100,
            }
        );
    }

    #[test]
    fn reload_applies_buck_cleanup_runtime_fields() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.buck = Some(BuckConfig {
            enable_session_cleanup: true,
            cleanup_interval: 300,
            completed_retention_days: 7,
            ..Default::default()
        });
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.buck = Some(BuckConfig {
            enable_session_cleanup: false,
            cleanup_interval: 60,
            completed_retention_days: 1,
            ..Default::default()
        });

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");
        let buck = snapshot.buck.as_ref().expect("buck config");

        assert_eq!(
            report.applied_fields,
            vec![
                "buck.enable_session_cleanup",
                "buck.cleanup_interval",
                "buck.completed_retention_days"
            ]
        );
        assert!(report.restart_required_fields.is_empty());
        assert!(!buck.enable_session_cleanup);
        assert_eq!(buck.cleanup_interval, 60);
        assert_eq!(buck.completed_retention_days, 1);
    }

    #[test]
    fn reload_reports_buck_cleanup_enable_requires_restart_without_publishing_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.buck = Some(BuckConfig {
            enable_session_cleanup: false,
            cleanup_interval: 300,
            completed_retention_days: 7,
            ..Default::default()
        });
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.buck = Some(BuckConfig {
            enable_session_cleanup: true,
            cleanup_interval: 60,
            completed_retention_days: 1,
            ..Default::default()
        });

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");
        let buck = snapshot.buck.as_ref().expect("buck config");

        assert!(report.applied_fields.is_empty());
        assert_eq!(
            report.restart_required_fields,
            vec![
                "buck.enable_session_cleanup",
                "buck.cleanup_interval",
                "buck.completed_retention_days"
            ]
        );
        assert!(!buck.enable_session_cleanup);
        assert_eq!(buck.cleanup_interval, 300);
        assert_eq!(buck.completed_retention_days, 7);
    }

    #[test]
    fn reload_reports_buck_upload_fields_as_restart_required() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.buck = Some(BuckConfig::default());
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.buck = Some(BuckConfig {
            max_files: 10,
            upload_concurrency_limit: 25,
            ..Default::default()
        });

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");
        let buck = snapshot.buck.as_ref().expect("buck config");

        assert!(report.applied_fields.is_empty());
        assert_eq!(
            report.restart_required_fields,
            vec!["buck.max_files", "buck.upload_concurrency_limit"]
        );
        assert_eq!(buck.max_files, BuckConfig::default().max_files);
        assert_eq!(
            buck.upload_concurrency_limit,
            BuckConfig::default().upload_concurrency_limit
        );
    }

    #[test]
    fn reload_rejects_invalid_candidate_and_keeps_current_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.log.level = "info".to_string();
        let handle = ConfigHandle::new(current);

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "verbose".to_string();

        let err = handle
            .reload(candidate)
            .expect_err("invalid candidate should fail");
        let snapshot = handle.snapshot().expect("snapshot after failed reload");

        assert!(err.to_string().contains("log.level"));
        assert_eq!(snapshot.log.level, "info");
    }

    #[test]
    fn reload_applies_subscribers_before_publishing_snapshot() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.log.level = "info".to_string();
        let handle = ConfigHandle::new(current);
        let handle_for_subscriber = handle.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let apply_events = events.clone();
        let rollback_events = events.clone();
        handle
            .subscribe(ConfigReloadSubscriber::new(
                "log",
                move |next, report| {
                    let visible = handle_for_subscriber
                        .snapshot()
                        .expect("subscriber can read old snapshot");
                    apply_events
                        .lock()
                        .expect("events")
                        .push(format!("apply:{}:{}", next.log.level, visible.log.level));
                    assert_eq!(report.applied_fields, vec!["log.level"]);
                    Ok(())
                },
                move |current, _| {
                    rollback_events
                        .lock()
                        .expect("events")
                        .push(format!("rollback:{}", current.log.level));
                    Ok(())
                },
            ))
            .expect("subscribe");

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "debug".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");
        let events = events.lock().expect("events").clone();

        assert_eq!(report.applied_fields, vec!["log.level"]);
        assert_eq!(snapshot.log.level, "debug");
        assert_eq!(events, vec!["apply:debug:info"]);
    }

    #[test]
    fn reload_rolls_back_subscribers_and_keeps_snapshot_when_apply_fails() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut current = isolated_config(temp_dir.path().join("current"));
        current.log.level = "info".to_string();
        let handle = ConfigHandle::new(current);
        let events = Arc::new(Mutex::new(Vec::new()));

        let first_apply_events = events.clone();
        let first_rollback_events = events.clone();
        handle
            .subscribe(ConfigReloadSubscriber::new(
                "first",
                move |next, _| {
                    first_apply_events
                        .lock()
                        .expect("events")
                        .push(format!("first apply {}", next.log.level));
                    Ok(())
                },
                move |current, _| {
                    first_rollback_events
                        .lock()
                        .expect("events")
                        .push(format!("first rollback {}", current.log.level));
                    Ok(())
                },
            ))
            .expect("subscribe first");

        let second_apply_events = events.clone();
        let second_rollback_events = events.clone();
        handle
            .subscribe(ConfigReloadSubscriber::new(
                "second",
                move |next, _| {
                    second_apply_events
                        .lock()
                        .expect("events")
                        .push(format!("second apply {}", next.log.level));
                    Err(MegaError::Other("simulated subscriber failure".to_string()))
                },
                move |current, _| {
                    second_rollback_events
                        .lock()
                        .expect("events")
                        .push(format!("second rollback {}", current.log.level));
                    Ok(())
                },
            ))
            .expect("subscribe second");

        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "debug".to_string();

        let err = handle
            .reload(candidate)
            .expect_err("subscriber failure should fail reload");
        let snapshot = handle.snapshot().expect("snapshot after failed reload");
        let events = events.lock().expect("events").clone();

        assert!(err.to_string().contains("second"));
        assert!(err.to_string().contains("simulated subscriber failure"));
        assert_eq!(snapshot.log.level, "info");
        assert_eq!(
            events,
            vec![
                "first apply debug",
                "second apply debug",
                "second rollback info",
                "first rollback info"
            ]
        );
    }

    #[test]
    fn reload_from_path_uses_profile_candidate_and_keeps_restart_required_fields() {
        let lock = env_lock();
        // Isolate from any ambient MEGA_DATABASE__DB_URL (e.g. set by .env.test):
        // it would override both the base and the profile db_url via the env
        // source layer, so no db_url change would be detected and the
        // restart-required assertion below would see an empty list.
        let _db_url_guard = EnvVarGuard::remove(&lock, "MEGA_DATABASE__DB_URL");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path())).expect("base config");

        let initial = Config::new(config_path.to_str().expect("utf-8 config path"))
            .expect("base config should load");
        let original_db_url = initial.database.db_url.clone();
        let handle = ConfigHandle::new(initial);

        std::fs::write(
            &profile_path,
            r#"
            [log]
            level = "debug"

            [database]
            db_url = "postgres://localhost:5432/restart-required"
            "#,
        )
        .expect("profile config");

        let report = handle
            .reload_from_path(&config_path, Some(&profile_path))
            .expect("reload from profile should succeed");
        let snapshot = handle.snapshot().expect("snapshot after reload");

        assert_eq!(report.applied_fields, vec!["log.level"]);
        assert_eq!(report.restart_required_fields, vec!["database.db_url"]);
        assert_eq!(snapshot.log.level, "debug");
        assert_eq!(snapshot.database.db_url, original_db_url);
    }

    #[test]
    fn reload_watcher_reloads_when_profile_file_changes() {
        let _lock = env_lock();
        test_runtime().block_on(async {
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let config_path = temp_dir.path().join("config.toml");
            let profile_path = temp_dir.path().join("config.prod.toml");
            std::fs::write(&config_path, config_init_template(temp_dir.path()))
                .expect("base config");

            let initial = Config::new(config_path.to_str().expect("utf-8 config path"))
                .expect("base config should load");
            let handle = ConfigHandle::new(initial);
            let mut watcher = ConfigReloadWatcher::new(
                handle.clone(),
                config_path,
                Some(profile_path.clone()),
                Duration::from_secs(60),
            )
            .await
            .expect("watcher");

            assert!(watcher.poll_once().await.expect("unchanged poll").is_none());

            std::fs::write(&profile_path, "[log]\nlevel = \"debug\"\n")
                .expect("profile config update");

            let report = watcher
                .poll_once()
                .await
                .expect("changed poll")
                .expect("reload report");
            let snapshot = handle.snapshot().expect("snapshot after reload");

            assert_eq!(report.applied_fields, vec!["log.level"]);
            assert_eq!(snapshot.log.level, "debug");
        });
    }

    #[test]
    fn reload_watcher_keeps_snapshot_after_invalid_profile_change() {
        let _lock = env_lock();
        test_runtime().block_on(async {
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let config_path = temp_dir.path().join("config.toml");
            let profile_path = temp_dir.path().join("config.prod.toml");
            std::fs::write(&config_path, config_init_template(temp_dir.path()))
                .expect("base config");

            let initial = Config::new(config_path.to_str().expect("utf-8 config path"))
                .expect("base config should load");
            let original_level = initial.log.level.clone();
            let handle = ConfigHandle::new(initial);
            let mut watcher = ConfigReloadWatcher::new(
                handle.clone(),
                config_path,
                Some(profile_path.clone()),
                Duration::from_secs(60),
            )
            .await
            .expect("watcher");

            std::fs::write(&profile_path, "[log]\nlevel = \"verbose\"\n")
                .expect("invalid profile config");
            let error = watcher
                .poll_once()
                .await
                .expect_err("invalid config should fail reload");
            let snapshot = handle.snapshot().expect("snapshot after failed reload");

            assert!(error.to_string().contains("log.level"));
            assert_eq!(snapshot.log.level, original_level);

            std::fs::write(&profile_path, "[log]\nlevel = \"debug\"\n")
                .expect("valid profile config");
            let report = watcher
                .poll_once()
                .await
                .expect("fixed config should reload")
                .expect("reload report");
            let snapshot = handle.snapshot().expect("snapshot after fixed reload");

            assert_eq!(report.applied_fields, vec!["log.level"]);
            assert_eq!(snapshot.log.level, "debug");
        });
    }

    #[test]
    fn reload_watcher_run_until_shutdown_stops_cleanly() {
        let _lock = env_lock();
        test_runtime().block_on(async {
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let config_path = temp_dir.path().join("config.toml");
            std::fs::write(&config_path, config_init_template(temp_dir.path()))
                .expect("base config");

            let initial = Config::new(config_path.to_str().expect("utf-8 config path"))
                .expect("base config should load");
            let handle = ConfigHandle::new(initial);
            let watcher =
                ConfigReloadWatcher::new(handle, config_path, None, Duration::from_millis(10))
                    .await
                    .expect("watcher");
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let run = tokio::spawn(watcher.run_until_shutdown(shutdown_rx));

            shutdown_tx.send(true).expect("send shutdown");

            run.await
                .expect("watcher task should join")
                .expect("watcher should stop");
        });
    }
}
