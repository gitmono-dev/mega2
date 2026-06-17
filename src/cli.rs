//! Cli module is responsible for parsing command line arguments and executing the appropriate.

use std::{
    env,
    path::PathBuf,
    sync::{Arc, Once, OnceLock},
};

use clap::{Arg, ArgMatches, Command};
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{
        Layer as FmtLayer,
        format::{DefaultFields, Format},
        writer::BoxMakeWriter,
    },
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::{
    commands::{CommandContext, LoadMode, builtin, builtin_exec, load_mode, unknown_subcommand},
    common::errors::{MegaError, MegaResult},
    config::{
        Config, LogConfig,
        loader::{ConfigInput, ConfigLoader, LoadedConfig},
        mega_cache,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
};

static CTRLC_HANDLER: Once = Once::new();
type LogReloadFn = dyn Fn(&LogConfig) -> Result<(), MegaError> + Send + Sync + 'static;
static LOG_RELOAD: OnceLock<Arc<LogReloadFn>> = OnceLock::new();

pub fn parse(args: Option<Vec<&str>>) -> MegaResult {
    let matches = match args {
        Some(args) => cli()
            .no_binary_name(true)
            .try_get_matches_from(args)
            .unwrap_or_else(|e| e.exit()),
        None => cli().try_get_matches().unwrap_or_else(|e| e.exit()),
    };

    let (cmd, subcommand_args) = match matches.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => {
            let (config, loaded) = load_config(&matches)?;
            init_log(&config.log);
            tracing::info!(
                source = ?loaded.source,
                path = %loaded.path.display(),
                "config loaded"
            );
            install_ctrlc_handler();
            return Ok(());
        }
    };

    let mode = load_mode(cmd, subcommand_args).ok_or_else(|| unknown_subcommand(cmd))?;
    let ctx = match mode {
        LoadMode::None => CommandContext {
            config: None,
            config_path: matches.get_one::<PathBuf>("config").cloned(),
            config_profile_path: None,
        },
        LoadMode::ConfigPath | LoadMode::RawSources | LoadMode::VaultBootstrap => {
            let loaded = load_config_path(&matches)?;
            let config_profile_path = loaded.profile.map(|profile| profile.path);
            CommandContext {
                config: None,
                config_path: Some(loaded.path),
                config_profile_path,
            }
        }
        LoadMode::ParsedConfig | LoadMode::FullAppContext => {
            let (config, loaded) = load_config(&matches)?;
            init_log(&config.log);
            tracing::info!(
                source = ?loaded.source,
                path = %loaded.path.display(),
                "config loaded"
            );
            CommandContext {
                config: Some(config),
                config_path: Some(loaded.path),
                config_profile_path: loaded.profile.map(|profile| profile.path),
            }
        }
    };

    install_ctrlc_handler();

    exec_subcommand(ctx, cmd, subcommand_args)
}

fn load_config_path(matches: &ArgMatches) -> Result<LoadedConfig, MegaError> {
    let cli_path = matches.get_one::<PathBuf>("config").cloned();
    let input = ConfigInput {
        cli_path,
        env_path: std::env::var_os("MEGA_CONFIG").map(PathBuf::from),
        cli_profile: matches.get_one::<String>("profile").cloned(),
        env_profile: std::env::var("MEGA_PROFILE").ok(),
    };
    Ok(ConfigLoader::new(input).load()?)
}

fn load_config(matches: &ArgMatches) -> Result<(Config, LoadedConfig), MegaError> {
    let loaded = load_config_path(matches)?;

    let config = Config::new_with_profile(
        loaded.path.to_str().ok_or_else(|| {
            MegaError::Other(format!(
                "Config path contains invalid UTF-8: {:?}",
                loaded.path
            ))
        })?,
        loaded
            .profile
            .as_ref()
            .map(|profile| profile.path.as_path()),
    )?;

    Ok((config, loaded))
}

fn install_ctrlc_handler() {
    CTRLC_HANDLER.call_once(|| {
        ctrlc::set_handler(move || {
            tracing::info!("Received Ctrl-C signal, exiting...");
            std::process::exit(0);
        })
        .unwrap();
    });
}

pub(crate) fn init_log(config: &LogConfig) {
    let (filter_layer, filter_reload_handle) =
        tracing_subscriber::reload::Layer::new(log_level_filter(config));
    let (format_layer, format_reload_handle) =
        tracing_subscriber::reload::Layer::new(log_format_layer(config));
    let init_result = tracing_subscriber::registry()
        .with(format_layer)
        .with(filter_layer)
        .try_init();

    if init_result.is_ok() {
        let reload = Arc::new(move |config: &LogConfig| {
            filter_reload_handle
                .reload(log_level_filter(config))
                .map_err(|error| {
                    MegaError::Other(format!("failed to reload log level filter: {error}"))
                })?;
            format_reload_handle
                .reload(log_format_layer(config))
                .map_err(|error| {
                    MegaError::Other(format!("failed to reload log format layer: {error}"))
                })
        });
        let _ = LOG_RELOAD.set(reload);
    } else {
        tracing::debug!("tracing subscriber was already initialized");
    }
}

pub(crate) fn config_reload_log_subscriber() -> ConfigReloadSubscriber {
    ConfigReloadSubscriber::new(
        "log",
        |next, report| reload_log_if_needed(&next.log, report),
        |current, report| reload_log_if_needed(&current.log, report),
    )
}

fn reload_log_if_needed(config: &LogConfig, report: &ConfigReloadReport) -> Result<(), MegaError> {
    if !report
        .applied_fields
        .iter()
        .any(|field| matches!(*field, "log.level" | "log.print_std" | "log.with_ansi"))
    {
        return Ok(());
    }

    reload_log(config)
}

fn reload_log(config: &LogConfig) -> Result<(), MegaError> {
    if let Some(reload) = LOG_RELOAD.get() {
        reload(config)
    } else {
        tracing::debug!(
            "log reload requested before reloadable tracing subscriber was initialized"
        );
        Ok(())
    }
}

fn log_format_layer(
    config: &LogConfig,
) -> FmtLayer<tracing_subscriber::Registry, DefaultFields, Format, BoxMakeWriter> {
    tracing_subscriber::fmt::layer()
        .with_writer(log_writer(config))
        .with_ansi(config.with_ansi)
}

fn log_writer(config: &LogConfig) -> BoxMakeWriter {
    if config.print_std {
        BoxMakeWriter::new(std::io::stdout)
    } else {
        BoxMakeWriter::new(tracing_appender::rolling::hourly(
            mega_cache().join("logs"),
            "mono-logs",
        ))
    }
}

fn log_level_filter(config: &LogConfig) -> LevelFilter {
    match config.level.as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

fn cli() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .subcommands(builtin())
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Sets a config file work directory"),
        )
        .arg(
            Arg::new("profile")
                .long("profile")
                .value_name("NAME")
                .help("Loads config.<profile>.toml next to the selected config file"),
        )
}

fn exec_subcommand(ctx: CommandContext, cmd: &str, args: &ArgMatches) -> MegaResult {
    if let Some(f) = builtin_exec(cmd) {
        f(ctx, args)
    } else {
        Err(unknown_subcommand(cmd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        template::config_init_template,
        testing::{EnvVarGuard, env_lock},
    };

    #[test]
    fn cli_accepts_config_path() {
        let matches = cli()
            .no_binary_name(true)
            .try_get_matches_from(["--config", "config/config.toml"])
            .unwrap();

        assert_eq!(
            matches.get_one::<PathBuf>("config"),
            Some(&PathBuf::from("config/config.toml"))
        );
    }

    #[test]
    fn cli_accepts_profile() {
        let matches = cli()
            .no_binary_name(true)
            .try_get_matches_from([
                "--config",
                "config/config.toml",
                "--profile",
                "prod",
                "config",
                "validate",
            ])
            .unwrap();

        assert_eq!(
            matches.get_one::<String>("profile").map(String::as_str),
            Some("prod")
        );
    }

    #[test]
    fn cli_accepts_service_http_options() {
        let matches = cli()
            .no_binary_name(true)
            .try_get_matches_from([
                "--config",
                "config/config.toml",
                "service",
                "http",
                "--host",
                "0.0.0.0",
                "-p",
                "9000",
            ])
            .unwrap();
        let Some(("service", service_args)) = matches.subcommand() else {
            panic!("service subcommand should parse");
        };
        let Some(("http", http_args)) = service_args.subcommand() else {
            panic!("http subcommand should parse");
        };

        assert_eq!(http_args.get_one::<String>("host").unwrap(), "0.0.0.0");
        assert_eq!(http_args.get_one::<u16>("port"), Some(&9000));
    }

    #[test]
    fn cli_accepts_config_secret_ref() {
        let matches = cli()
            .no_binary_name(true)
            .try_get_matches_from([
                "config",
                "secret",
                "ref",
                "mail.password",
                "--vault-path",
                "config/prod/mail/password",
            ])
            .unwrap();
        let Some(("config", config_args)) = matches.subcommand() else {
            panic!("config subcommand should parse");
        };
        let Some(("secret", secret_args)) = config_args.subcommand() else {
            panic!("secret subcommand should parse");
        };
        let Some(("ref", ref_args)) = secret_args.subcommand() else {
            panic!("secret ref subcommand should parse");
        };

        assert_eq!(ref_args.get_one::<String>("name").unwrap(), "mail.password");
    }

    #[test]
    fn parse_loads_config_without_subcommand() {
        let _lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path())).expect("write config");
        let config_path = config_path.to_string_lossy().to_string();

        parse(Some(vec!["--config", &config_path])).unwrap();
    }

    #[test]
    fn config_validate_reports_bad_env_type_without_cli_preload() {
        let lock = env_lock();
        let _print_std = EnvVarGuard::set(&lock, "MEGA_LOG__PRINT_STD", "not_bool_secret");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path())).expect("write config");
        let config_path = config_path.to_string_lossy().to_string();

        let err = parse(Some(vec!["--config", &config_path, "config", "validate"]))
            .expect_err("bad env override should fail in config validate");
        let message = err.to_string();

        assert!(message.contains("MEGA_LOG__PRINT_STD"));
        assert!(message.contains("log.print_std"));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("remove the override"));
        assert!(!message.contains("not_bool_secret"));
    }
}
