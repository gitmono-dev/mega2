use clap::{ArgMatches, Args, Command, FromArgMatches};

use crate::{
    common::errors::MegaResult,
    context::AppContext,
    server::{CommonHttpOptions, http_server},
};

pub fn cli() -> Command {
    CommonHttpOptions::augment_args_for_update(Command::new("http").about("Start Mega HTTP server"))
}

pub(crate) async fn exec(ctx: AppContext, args: &ArgMatches) -> MegaResult {
    let server_matchers: CommonHttpOptions = CommonHttpOptions::from_arg_matches(args)
        .map_err(|err| err.exit())
        .unwrap();

    tracing::info!("{server_matchers:#?}");
    http_server::start_http(ctx, server_matchers).await;
    Ok(())
}
