//! Cli module is responsible for parsing command line arguments and executing the appropriate.

use std::{env, path::PathBuf, sync::Once};

use clap::{Arg, ArgMatches, Command};

use crate::{
    commands::{builtin, builtin_exec, unknown_subcommand},
    common::{
        config::{
            Config, LogConfig,
            loader::{ConfigInput, ConfigLoader},
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

    let cli_path = matches.get_one::<PathBuf>("config").cloned();
    let input = ConfigInput {
        cli_path,
        env_path: std::env::var_os("MEGA_CONFIG").map(PathBuf::from),
    };
    let loaded = ConfigLoader::new(input).load()?;

    let config = Config::new(loaded.path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            loaded.path
        ))
    })?)?;

    init_log(&config.log);

    tracing::info!(
        source = ?loaded.source,
        path = %loaded.path.display(),
        "config loaded"
    );

    CTRLC_HANDLER.call_once(|| {
        ctrlc::set_handler(move || {
            tracing::info!("Received Ctrl-C signal, exiting...");
            std::process::exit(0);
        })
        .unwrap();
    });

    let (cmd, subcommand_args) = match matches.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => return Ok(()),
    };

    exec_subcommand(config, cmd, subcommand_args)
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

fn exec_subcommand(config: Config, cmd: &str, args: &ArgMatches) -> MegaResult {
    if let Some(f) = builtin_exec(cmd) {
        f(config, args)
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
    fn parse_loads_config_without_subcommand() {
        parse(Some(vec!["--config", "config/config.toml"])).unwrap();
    }
}
