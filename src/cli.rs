//! Cli module is responsible for parsing command line arguments and executing the appropriate.

use std::{env, path::PathBuf, sync::Once};

use clap::{Arg, ArgMatches, Command};

use crate::{
    commands::{CommandContext, LoadMode, builtin, builtin_exec, load_mode, unknown_subcommand},
    common::{
        config::{
            Config, LogConfig,
            loader::{ConfigInput, ConfigLoader, LoadedConfig},
            mega_cache,
        },
        errors::{MegaError, MegaResult},
    },
};

static CTRLC_HANDLER: Once = Once::new();

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
        LoadMode::None => CommandContext::default(),
        LoadMode::ConfigPath | LoadMode::RawSources | LoadMode::VaultBootstrap => {
            let loaded = load_config_path(&matches)?;
            CommandContext {
                config: None,
                config_path: Some(loaded.path),
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
    };
    Ok(ConfigLoader::new(input).load()?)
}

fn load_config(matches: &ArgMatches) -> Result<(Config, LoadedConfig), MegaError> {
    let loaded = load_config_path(matches)?;

    let config = Config::new(loaded.path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            loaded.path
        ))
    })?)?;

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

fn init_log(config: &LogConfig) {
    let log_level = match config.level.as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    };

    let file_appender = tracing_appender::rolling::hourly(mega_cache().join("logs"), "mono-logs");

    let init_result = if config.print_std {
        tracing_subscriber::fmt()
            .with_writer(std::io::stdout)
            .with_max_level(log_level)
            .with_ansi(config.with_ansi)
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_writer(file_appender)
            .with_max_level(log_level)
            .with_ansi(config.with_ansi)
            .try_init()
    };

    if init_result.is_err() {
        tracing::debug!("tracing subscriber was already initialized");
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
        parse(Some(vec!["--config", "config/config.toml"])).unwrap();
    }
}
