use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::{
    common::errors::MegaResult, config::Config, context::bootstrap_monorepo_with_commit_time,
    jupiter::utils::converter::BootstrapCommitTime,
};

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
        .arg(
            Arg::new("commit-time")
                .long("commit-time")
                .value_name("UNIX_SECONDS")
                .value_parser(clap::value_parser!(BootstrapCommitTime))
                .help("Set the initial commit's author and committer time (0..=4294967295)"),
        )
}

pub(crate) async fn exec(config: Config, args: &ArgMatches) -> MegaResult {
    let object_format = config.monorepo.object_format.as_str();
    let commit_time = args.get_one::<BootstrapCommitTime>("commit-time").copied();
    bootstrap_monorepo_with_commit_time(config, commit_time).await?;
    tracing::info!(
        object_format,
        "Monorepo bootstrap completed; exiting without starting Git listeners"
    );
    Ok(())
}

#[cfg(test)]
#[path = "init_commit_time_tests.rs"]
mod commit_time_tests;

#[cfg(test)]
mod tests {
    use super::cli;

    #[test]
    fn init_cli_requires_explicit_confirmation() {
        assert!(cli().try_get_matches_from(["init"]).is_err());
        assert!(cli().try_get_matches_from(["init", "--yes"]).is_ok());
    }
}
