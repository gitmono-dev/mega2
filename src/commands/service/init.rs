use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::{common::errors::MegaResult, config::Config, context::bootstrap_monorepo};

pub fn cli() -> Command {
    Command::new("init")
        .about("Initialize an empty Monorepo and exit without starting Git listeners")
        .arg(
            Arg::new("yes")
                .long("yes")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Confirm creation of the configured initial Monorepo graph"),
        )
}

pub(crate) async fn exec(config: Config, _args: &ArgMatches) -> MegaResult {
    let object_format = config.monorepo.object_format.as_str();
    bootstrap_monorepo(config).await?;
    tracing::info!(
        object_format,
        "Monorepo bootstrap completed; exiting without starting Git listeners"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cli;

    #[test]
    fn init_cli_requires_explicit_confirmation() {
        assert!(cli().try_get_matches_from(["init"]).is_err());
        assert!(cli().try_get_matches_from(["init", "--yes"]).is_ok());
    }
}
