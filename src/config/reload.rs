use std::sync::{Arc, RwLock};

use crate::{
    common::errors::MegaError,
    config::{Config, LogConfig},
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

#[derive(Debug, Clone)]
pub struct ConfigHandle {
    current: Arc<RwLock<Arc<Config>>>,
}

impl ConfigHandle {
    pub fn new(config: Config) -> Self {
        Self::from_arc(Arc::new(config))
    }

    pub fn from_arc(config: Arc<Config>) -> Self {
        Self {
            current: Arc::new(RwLock::new(config)),
        }
    }

    pub fn snapshot(&self) -> Result<Arc<Config>, MegaError> {
        let current = self.current.read().map_err(|_| {
            MegaError::Other("config reload snapshot lock was poisoned".to_string())
        })?;

        Ok(Arc::clone(&current))
    }

    pub fn reload(&self, candidate: Config) -> Result<ConfigReloadReport, MegaError> {
        candidate.validate()?;

        let mut current = self
            .current
            .write()
            .map_err(|_| MegaError::Other("config reload update lock was poisoned".to_string()))?;
        let mut next = current.as_ref().clone();
        let mut report = ConfigReloadReport::default();

        apply_log_changes(&current.log, &candidate.log, &mut next.log, &mut report);
        collect_database_restart_fields(&current, &candidate, &mut report);

        if report.applied() {
            *current = Arc::new(next);
        }

        Ok(report)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::isolated_config;

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
}
