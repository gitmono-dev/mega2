//! This module is responsible for handling the `service` command.

use std::{path::PathBuf, time::Duration};

use clap::{ArgMatches, Command};
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    cli::config_reload_log_subscriber,
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    config::reload::{ConfigHandle, ConfigReloadWatcher},
    context::AppContext,
};

pub mod http;
pub mod multi;
pub mod ssh;

const CONFIG_RELOAD_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn cli() -> Command {
    let subcommands = vec![http::cli(), ssh::cli(), multi::cli()];
    Command::new("service")
        .about("Start different kinds of server: for example https or ssh")
        .subcommands(subcommands)
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config_path = ctx.config_path.clone();
    let config_profile_path = ctx.config_profile_path.clone();
    let config = require_config(ctx, "service")?;

    let (cmd, subcommand_args) = match args.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => return Ok(()),
    };

    let context = AppContext::new(config).await?;
    context
        .config_handle
        .subscribe(config_reload_log_subscriber())?;
    let reload_watcher = spawn_config_reload_watcher(
        context.config_handle.clone(),
        config_path,
        config_profile_path,
    )
    .await?;

    let result = match cmd {
        "http" => http::exec(context.clone(), subcommand_args).await,
        "ssh" => ssh::exec(context.clone(), subcommand_args).await,
        "multi" => multi::exec(context.clone(), subcommand_args).await,
        _ => Err(MegaError::Other(format!(
            "Unknown service subcommand: {cmd}"
        ))),
    };

    let mut result = result;
    if let Some(reload_watcher) = reload_watcher
        && let Err(stop_error) = reload_watcher.stop().await
    {
        if result.is_ok() {
            result = Err(stop_error);
        } else {
            tracing::warn!(
                error = %stop_error,
                "config reload watcher failed to stop after service error"
            );
        }
    }

    if let Err(shutdown_error) = context.shutdown().await {
        if result.is_ok() {
            result = Err(shutdown_error);
        } else {
            tracing::warn!(
                error = %shutdown_error,
                "failed to release background resources after service error"
            );
        }
    }
    result
}

struct ConfigReloadWatcherTask {
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl ConfigReloadWatcherTask {
    async fn stop(self) -> Result<(), MegaError> {
        let _ = self.shutdown.send(true);

        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .map_err(|_| {
                MegaError::Other("config reload watcher did not stop within timeout".to_string())
            })?
            .map_err(|error| {
                MegaError::Other(format!(
                    "config reload watcher task failed to join: {error}"
                ))
            })
    }
}

async fn spawn_config_reload_watcher(
    config_handle: ConfigHandle,
    config_path: Option<PathBuf>,
    profile_path: Option<PathBuf>,
) -> Result<Option<ConfigReloadWatcherTask>, MegaError> {
    spawn_config_reload_watcher_with_interval(
        config_handle,
        config_path,
        profile_path,
        CONFIG_RELOAD_POLL_INTERVAL,
    )
    .await
}

async fn spawn_config_reload_watcher_with_interval(
    config_handle: ConfigHandle,
    config_path: Option<PathBuf>,
    profile_path: Option<PathBuf>,
    poll_interval: Duration,
) -> Result<Option<ConfigReloadWatcherTask>, MegaError> {
    let Some(config_path) = config_path else {
        return Ok(None);
    };

    let profile_path_display = profile_path.as_ref().map(|path| path.display().to_string());
    let watcher = ConfigReloadWatcher::new(
        config_handle,
        config_path.clone(),
        profile_path,
        poll_interval,
    )
    .await?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        if let Err(error) = watcher.run_until_shutdown(shutdown_rx).await {
            tracing::warn!(
                error = %error,
                "config reload watcher stopped with error"
            );
        }
    });

    tracing::info!(
        config_path = %config_path.display(),
        profile_path = %profile_path_display.as_deref().unwrap_or("<none>"),
        "config reload watcher started"
    );

    Ok(Some(ConfigReloadWatcherTask { shutdown, handle }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, reload::ConfigHandle, template::config_init_template, testing::isolated_config,
    };

    #[test]
    fn service_cli_contains_mega_service_subcommands() {
        let names = cli()
            .get_subcommands()
            .map(|cmd| cmd.get_name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["http", "ssh", "multi"]);
    }

    #[test]
    fn service_registers_log_reload_subscriber_for_config_handle() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.log.level = "info".to_string();
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_log_subscriber())
            .expect("subscribe");
        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "debug".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(report.applied_fields, vec!["log.level"]);
        assert_eq!(handle.snapshot().expect("snapshot").log.level, "debug");

        let mut restore = handle.snapshot().expect("snapshot").as_ref().clone();
        restore.log.level = "info".to_string();
        let restore_report = handle.reload(restore).expect("restore should succeed");

        assert_eq!(restore_report.applied_fields, vec!["log.level"]);
        assert_eq!(handle.snapshot().expect("snapshot").log.level, "info");
    }

    #[tokio::test]
    async fn service_reload_watcher_task_reloads_changed_profile() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path())).expect("base config");

        let config = Config::new(config_path.to_str().expect("utf-8 config path"))
            .expect("base config should load");
        let handle = ConfigHandle::new(config);
        let task = spawn_config_reload_watcher_with_interval(
            handle.clone(),
            Some(config_path),
            Some(profile_path.clone()),
            Duration::from_millis(10),
        )
        .await
        .expect("watcher should start")
        .expect("watcher task");

        std::fs::write(&profile_path, "[log]\nlevel = \"debug\"\n").expect("profile config update");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if handle.snapshot().expect("snapshot").log.level == "debug" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("reload watcher should apply profile change");

        task.stop().await.expect("watcher should stop");
    }
}
