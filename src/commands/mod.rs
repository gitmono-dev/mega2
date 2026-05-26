pub mod service;

use clap::{ArgMatches, Command};

use crate::common::{
    config::Config,
    errors::{MegaError, MegaResult},
};

pub fn builtin() -> Vec<Command> {
    vec![service::cli()]
}

pub(crate) fn builtin_exec(cmd: &str) -> Option<fn(Config, &ArgMatches) -> MegaResult> {
    let f = match cmd {
        "service" => service::exec,
        _ => return None,
    };

    Some(f)
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

        assert_eq!(names, vec!["service"]);
    }
}
