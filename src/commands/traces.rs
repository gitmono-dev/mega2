//! `mega2 traces` maintenance commands. Later traces maintenance commands
//! register another sibling under this parent. This command never generates a
//! default config.

#[path = "traces_target.rs"]
mod traces_target;

use clap::{ArgMatches, Command};

use crate::{
    commands::{CommandContext, unknown_subcommand},
    common::errors::MegaResult,
};

pub fn cli() -> Command {
    Command::new("traces")
        .about("Server-side traces maintenance")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(traces_target::cli())
}

#[tokio::main(flavor = "current_thread")]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("rebind-target", sub)) => traces_target::rebind(ctx, sub).await,
        Some((cmd, _)) => Err(unknown_subcommand(cmd)),
        None => Err(unknown_subcommand("traces")),
    }
}
