pub mod chat_migrate;
pub mod config;
pub mod service;

use std::path::PathBuf;

use clap::{ArgMatches, Command};

use crate::common::{
    config::Config,
    errors::{MegaError, MegaResult},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadMode {
    None,
    ConfigPath,
    RawSources,
    ParsedConfig,
    VaultBootstrap,
    FullAppContext,
}

#[derive(Debug, Default)]
pub(crate) struct CommandContext {
    pub config: Option<Config>,
    pub config_path: Option<PathBuf>,
}

pub(crate) type CommandExec = fn(CommandContext, &ArgMatches) -> MegaResult;

pub fn builtin() -> Vec<Command> {
    vec![service::cli(), chat_migrate::cli(), config::cli()]
}

pub(crate) fn builtin_exec(cmd: &str) -> Option<CommandExec> {
    let f = match cmd {
        "service" => service::exec,
        "chat-migrate" => chat_migrate::exec,
        "config" => config::exec,
        _ => return None,
    };

    Some(f)
}

pub(crate) fn load_mode(cmd: &str, args: &ArgMatches) -> Option<LoadMode> {
    match cmd {
        "service" | "chat-migrate" => Some(LoadMode::FullAppContext),
        "config" => Some(config::load_mode(args)),
        _ => None,
    }
}

pub(crate) fn require_config(ctx: CommandContext, cmd: &str) -> Result<Config, MegaError> {
    ctx.config
        .ok_or_else(|| MegaError::Other(format!("{cmd} requires a parsed config")))
}

pub(crate) fn require_config_path(ctx: &CommandContext, cmd: &str) -> Result<PathBuf, MegaError> {
    ctx.config_path
        .clone()
        .ok_or_else(|| MegaError::Other(format!("{cmd} requires a config path")))
}

pub(crate) fn unknown_subcommand(cmd: &str) -> MegaError {
    MegaError::Other(format!("Unknown subcommand: {cmd}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_contains_service_command() {
        let names = builtin()
            .into_iter()
            .map(|cmd| cmd.get_name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["service", "chat-migrate", "config"]);
    }
}
