pub mod authz_audit;
mod authz_audit_protect;
mod authz_audit_run;
pub mod config;
pub mod debug;
pub mod service;
#[cfg(test)]
mod un34_readonly_config;

use std::path::PathBuf;

use clap::{ArgMatches, Command};
use serde::Serialize;

use crate::{
    common::errors::{MegaError, MegaResult},
    config::{Config, loader::ConfigSource},
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

/// Where the config this process is running on came from (UN-34).
///
/// A read-only report is only worth as much as the reader's ability to tell
/// *which* configuration it describes. Carrying the provenance alongside the
/// parsed config is what lets a report say so instead of leaving the reader to
/// reconstruct it from the command line.
///
/// `paths` is for operator diagnostics only. It never enters a report or any
/// acceptance evidence (ER-11) — [`Self::sanitized`] is the shape that does, and
/// it has no path field to leak one.
#[derive(Debug, Clone)]
pub struct LoadedConfigSummary {
    pub source: ConfigSource,
    pub profile_name: Option<String>,
    pub paths: LoadedConfigPaths,
}

#[derive(Debug, Clone)]
pub struct LoadedConfigPaths {
    pub config: PathBuf,
    pub profile: Option<PathBuf>,
}

/// The provenance summary as it appears in a report.
///
/// The JSON representation is frozen (UN-34): `{"source": "cli"|"env"|"cwd"|
/// "global"|"default_generated", "profile": <string|null>}`. One representation,
/// no paths, no options — a consumer that has to guess which of several shapes
/// it received cannot compare two reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SanitizedSourceSummary {
    pub source: &'static str,
    pub profile: Option<String>,
}

impl LoadedConfigSummary {
    /// The path-free projection that reports embed.
    ///
    /// Dropping the paths is the point: a config path names a filesystem
    /// layout, and a profile path can name a deployment. Neither belongs in an
    /// artifact that travels.
    pub fn sanitized(&self) -> SanitizedSourceSummary {
        SanitizedSourceSummary {
            source: self.source.as_str(),
            profile: self.profile_name.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct CommandContext {
    pub config: Option<Config>,
    pub config_path: Option<PathBuf>,
    pub config_profile_path: Option<PathBuf>,
    /// Provenance of the config above, when one was loaded at all.
    pub config_summary: Option<LoadedConfigSummary>,
}

pub(crate) type CommandExec = fn(CommandContext, &ArgMatches) -> MegaResult;

pub fn builtin() -> Vec<Command> {
    vec![
        service::cli(),
        config::cli(),
        debug::cli(),
        authz_audit::cli(),
    ]
}

pub(crate) fn builtin_exec(cmd: &str) -> Option<CommandExec> {
    let f = match cmd {
        "service" => service::exec,
        "config" => config::exec,
        "debug" => debug::exec,
        "authz-audit" => authz_audit::exec,
        _ => return None,
    };

    Some(f)
}

pub(crate) fn load_mode(cmd: &str, args: &ArgMatches) -> Option<LoadMode> {
    match cmd {
        "service" | "debug" => Some(LoadMode::FullAppContext),
        "config" => Some(config::load_mode(args)),
        "authz-audit" => Some(authz_audit::load_mode(args)),
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

        assert_eq!(names, vec!["service", "config", "debug", "authz-audit"]);
    }
}
