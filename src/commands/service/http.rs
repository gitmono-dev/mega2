use clap::{ArgMatches, Args, Command, FromArgMatches};

use crate::{
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    server::{CommonHttpOptions, http_server},
};

pub fn cli() -> Command {
    CommonHttpOptions::augment_args_for_update(Command::new("http").about("Start Mega HTTP server"))
}

pub(crate) async fn exec(ctx: AppContext, args: &ArgMatches) -> MegaResult {
    // Parse failures flow through the service cleanup tail like any other
    // post-context error (WH-13 AC2).
    let server_matchers: CommonHttpOptions = CommonHttpOptions::from_arg_matches(args)
        .map_err(|err| MegaError::Other(format!("invalid service http arguments: {err}")))?;

    tracing::info!("{server_matchers:#?}");
    http_server::start_http(ctx, server_matchers).await?;
    Ok(())
}
