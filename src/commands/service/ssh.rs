use clap::{ArgMatches, Args, Command, FromArgMatches};

use crate::{
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    contract::policy::enforcement::Enforcement,
    server::ssh_server::{SshOptions, start_server},
};

pub fn cli() -> Command {
    SshOptions::augment_args_for_update(Command::new("ssh").about("Start Git SSH server"))
}

/// Standalone `service ssh` only supports `off` enforcement (ADR-UN-02 topology
/// constraint): authorization state must be shared with the HTTP server via
/// `service multi`. Refuse before first-build.
pub fn validate_standalone_ssh_enforcement(enforcement: &str) -> Result<(), MegaError> {
    let enforcement = Enforcement::parse(enforcement)
        .ok_or_else(|| MegaError::Other("invalid cedar.enforcement".to_string()))?;
    if enforcement != Enforcement::Off {
        return Err(MegaError::Other(
            "standalone `service ssh` does not support authorization enforcement; use `service multi` (shared instance with HTTP) or set `cedar.enforcement = off`".to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn exec(ctx: AppContext, args: &ArgMatches) -> MegaResult {
    let server_matchers = SshOptions::from_arg_matches(args)
        .map_err(|err| err.exit())
        .unwrap();
    tracing::info!("{server_matchers:#?}");
    validate_standalone_ssh_enforcement(&ctx.storage.config().cedar.enforcement)?;
    start_server(ctx, &server_matchers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un03_standalone_ssh_off_allowed() {
        assert!(validate_standalone_ssh_enforcement("off").is_ok());
    }

    #[test]
    fn un03_standalone_ssh_shadow_refused_with_guidance() {
        let err = validate_standalone_ssh_enforcement("shadow")
            .expect_err("shadow must be refused for standalone ssh");
        let msg = err.to_string();
        assert!(msg.contains("service multi"), "missing guidance: {msg}");
        assert!(msg.contains("off"), "missing off guidance: {msg}");
    }

    #[test]
    fn un03_standalone_ssh_enforce_refused_with_guidance() {
        let err = validate_standalone_ssh_enforcement("enforce")
            .expect_err("enforce must be refused for standalone ssh");
        let msg = err.to_string();
        assert!(msg.contains("service multi"), "missing guidance: {msg}");
        assert!(msg.contains("off"), "missing off guidance: {msg}");
    }
}
