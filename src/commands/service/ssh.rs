use std::time::Duration;

use clap::{ArgMatches, Args, Command, FromArgMatches};

use crate::{
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    contract::policy::enforcement::Enforcement,
    server::ssh_server::{SshOptions, start_server},
};

/// Bound on the abort + join of the SSH server task after Ctrl+C (WH-13).
const SSH_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

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
    // Parse failures flow through the service cleanup tail (WH-13 AC2).
    let server_matchers = SshOptions::from_arg_matches(args)
        .map_err(|err| MegaError::Other(format!("invalid service ssh arguments: {err}")))?;
    tracing::info!("{server_matchers:#?}");
    validate_standalone_ssh_enforcement(&ctx.storage.config().cedar.enforcement)?;
    run_until_ctrl_c(ctx, server_matchers).await
}

/// `start_server` runs forever with no shutdown token and is out of scope for
/// WH-13, so shutdown is handled here: the server runs in a task, and the
/// shared service shutdown token (fed by Ctrl+C via the `service` forwarder)
/// aborts it, joined with a bounded wait before the cleanup tail runs.
async fn run_until_ctrl_c(ctx: AppContext, options: SshOptions) -> MegaResult {
    let service_shutdown = ctx.service_shutdown.clone();
    let mut server = tokio::spawn(async move { start_server(ctx, &options).await });
    tokio::select! {
        result = &mut server => {
            result.map_err(|error| MegaError::Other(format!("SSH server task failed to join: {error}")))?
        }
        _ = service_shutdown.cancelled() => {
            tracing::info!("Received shutdown signal, stopping SSH server...");
            if server.is_finished() {
                // The server finished on its own in the same instant: its
                // genuine result wins over the signal's Ok(()).
                return server
                    .await
                    .map_err(|error| MegaError::Other(format!("SSH server task failed to join: {error}")))?;
            }
            server.abort();
            match tokio::time::timeout(SSH_ABORT_JOIN_TIMEOUT, &mut server).await {
                // Completed genuinely between the check and the abort: keep
                // the real result instead of the signal's Ok(()).
                Ok(Ok(inner)) => inner,
                Ok(Err(error)) if error.is_cancelled() => Ok(()),
                Ok(Err(error)) => {
                    Err(MegaError::Other(format!("SSH server task failed: {error}")))
                }
                Err(_) => {
                    // Detached on timeout — pre-existing semantics.
                    tracing::warn!("SSH server task did not join within the abort timeout");
                    Ok(())
                }
            }
        }
    }
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
