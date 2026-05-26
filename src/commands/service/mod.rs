//! This module is responsible for handling the `service` command.

use clap::{ArgMatches, Command};

use crate::{
    common::{
        config::Config,
        errors::{MegaError, MegaResult},
    },
    context::AppContext,
};

pub mod http;
pub mod multi;
pub mod ssh;

pub fn cli() -> Command {
    let subcommands = vec![http::cli(), ssh::cli(), multi::cli()];
    Command::new("service")
        .about("Start different kinds of server: for example https or ssh")
        .subcommands(subcommands)
}

#[tokio::main]
pub(crate) async fn exec(config: Config, args: &ArgMatches) -> MegaResult {
    let context = AppContext::new(config).await;

    let (cmd, subcommand_args) = match args.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => return Ok(()),
    };

    match cmd {
        "http" => http::exec(context.clone(), subcommand_args).await,
        "ssh" => ssh::exec(context.clone(), subcommand_args).await,
        "multi" => multi::exec(context.clone(), subcommand_args).await,
        _ => Err(MegaError::Other(format!(
            "Unknown service subcommand: {cmd}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_cli_contains_mega_service_subcommands() {
        let names = cli()
            .get_subcommands()
            .map(|cmd| cmd.get_name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["http", "ssh", "multi"]);
    }
}
