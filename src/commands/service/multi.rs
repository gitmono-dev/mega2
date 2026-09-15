use std::time::Duration;

use clap::{ArgMatches, Args, Command, FromArgMatches, ValueEnum};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    server::{
        CommonHttpOptions, http_server,
        ssh_server::{self, SshCustom, SshOptions},
    },
};

/// Bound on the abort + join of the ssh-style child, which has no shutdown
/// handling of its own (WH-13 AC5).
const SIBLING_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace the http child gets to stop through its own `service_shutdown` arm
/// before it is aborted (WH-13 AC5). Aborting the `start_http` wrapper would
/// only detach the real listener, so cancellation must reach it via the token.
const HTTP_GRACE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the join of the http child after a forced abort.
const HTTP_ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Clone, ValueEnum)]
pub enum StartCommand {
    Http,
    Https,
    Ssh,
}

#[derive(Args, Clone, Debug)]
pub struct StartOptions {
    service: Vec<StartCommand>,

    #[clap(flatten)]
    pub http: CommonHttpOptions,

    #[clap(flatten)]
    pub ssh: SshCustom,
}

pub fn cli() -> Command {
    StartOptions::augment_args_for_update(
        Command::new("multi").about("Start multiple server by given params"),
    )
}

pub(crate) async fn exec(ctx: AppContext, args: &ArgMatches) -> MegaResult {
    // Parse failures flow through the service cleanup tail (WH-13 AC2).
    let server_matchers = StartOptions::from_arg_matches(args)
        .map_err(|err| MegaError::Other(format!("invalid service multi arguments: {err}")))?;

    tracing::info!("{server_matchers:#?}");

    let service_type = server_matchers.service;
    if !service_type.contains(&StartCommand::Http) {
        return Err(MegaError::Other(
            "start params should provide! run like 'mega service multi http ssh'".to_owned(),
        ));
    }

    let service_shutdown = ctx.service_shutdown.clone();
    let context_clone = ctx.clone();
    let http = server_matchers.http.clone();
    let http_server =
        tokio::spawn(async move { http_server::start_http(context_clone, http).await });

    let ssh_server = if service_type.contains(&StartCommand::Ssh) {
        let ssh = SshOptions {
            common: server_matchers.http.clone(),
            custom: server_matchers.ssh,
        };
        tokio::spawn(async move { ssh_server::start_server(ctx, &ssh).await })
    } else {
        // Placeholder sibling: `service multi http` must keep running until
        // the HTTP child stops, so this task never completes on its own.
        tokio::spawn(async { std::future::pending::<MegaResult>().await })
    };

    run_until_first_child(http_server, ssh_server, service_shutdown).await
}

/// The first child to finish decides the overall result; the sibling is
/// stopped with a bounded wait, so it never blocks the service cleanup tail
/// forever (WH-13 AC5).
pub(super) async fn run_until_first_child(
    http: JoinHandle<MegaResult>,
    ssh: JoinHandle<MegaResult>,
    service_shutdown: CancellationToken,
) -> MegaResult {
    run_until_first_child_with_deadlines(
        http,
        ssh,
        service_shutdown,
        SIBLING_JOIN_TIMEOUT,
        HTTP_GRACE_TIMEOUT,
        HTTP_ABORT_JOIN_TIMEOUT,
    )
    .await
}

/// Cancellation reaches the http child through the shared token: with Serve
/// inlined into `start_http`, the token arm graceful-stops the server, and a
/// forced abort (grace expiry) drops the future and genuinely stops it. The
/// ssh child has no shutdown handling and is always aborted + bounded-joined.
/// Genuine child errors are distinguished from expected abort-cancellation:
/// the token arm reports the first genuine error (http before ssh).
pub(super) async fn run_until_first_child_with_deadlines(
    mut http: JoinHandle<MegaResult>,
    mut ssh: JoinHandle<MegaResult>,
    service_shutdown: CancellationToken,
    sibling_join: Duration,
    http_grace: Duration,
    http_abort_join: Duration,
) -> MegaResult {
    enum First {
        Http(Result<MegaResult, tokio::task::JoinError>),
        Ssh(Result<MegaResult, tokio::task::JoinError>),
        Signal,
    }

    let first = tokio::select! {
        result = &mut http => First::Http(result),
        result = &mut ssh => First::Ssh(result),
        _ = service_shutdown.cancelled() => First::Signal,
    };

    match first {
        First::Signal => {
            let ssh_result = abort_child_unless_finished("ssh", &mut ssh, sibling_join).await;
            // The http child is already graceful-stopping via the token.
            let http_result =
                graceful_join_or_abort("http", &mut http, http_grace, http_abort_join).await;
            // Genuine errors survive the signal path: http before ssh.
            match [http_result, ssh_result]
                .into_iter()
                .flatten()
                .find_map(|result| result.err())
            {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        First::Http(result) => {
            // http result is preserved; the ssh abort is expected. A genuine
            // ssh error is logged but never overwrites the http result.
            if let Some(Err(error)) =
                abort_child_unless_finished("ssh", &mut ssh, sibling_join).await
            {
                tracing::warn!(error = %error, "multi ssh child failed while http finished first");
            }
            result.map_err(|error| {
                MegaError::Other(format!("multi http server task failed to join: {error}"))
            })?
        }
        First::Ssh(result) => {
            // Cancel the token so start_http's token arm graceful-stops the
            // listener; force-abort only after the grace expires. The ssh
            // result — success or failure — is the overall result; a genuine
            // http error is logged but does not overwrite it.
            service_shutdown.cancel();
            if let Some(Err(error)) =
                graceful_join_or_abort("http", &mut http, http_grace, http_abort_join).await
            {
                tracing::warn!(error = %error, "multi http child failed while ssh finished first");
            }
            result.map_err(|error| {
                MegaError::Other(format!("multi ssh server task failed to join: {error}"))
            })?
        }
    }
}

/// Waits up to `grace` for the child to stop on its own, then aborts and
/// bounded-joins it. Returns the child's genuine result when it produced one;
/// `None` when the child was cancelled by the abort (expected).
async fn graceful_join_or_abort(
    name: &str,
    handle: &mut JoinHandle<MegaResult>,
    grace: Duration,
    abort_join: Duration,
) -> Option<MegaResult> {
    match tokio::time::timeout(grace, &mut *handle).await {
        Ok(Ok(result)) => Some(result),
        Ok(Err(error)) if error.is_cancelled() => None,
        Ok(Err(error)) => Some(Err(MegaError::Other(format!(
            "multi {name} server task failed to join: {error}"
        )))),
        Err(_) => {
            tracing::warn!("multi {name} child did not stop within the grace; aborting");
            abort_child_unless_finished(name, handle, abort_join).await
        }
    }
}

/// Aborts the child unless it already finished, then bounded-joins it.
/// Returns the genuine result (including errors and panics) when the child
/// produced one; `None` for the expected abort-cancellation.
pub(super) async fn abort_child_unless_finished(
    name: &str,
    handle: &mut JoinHandle<MegaResult>,
    join_timeout: Duration,
) -> Option<MegaResult> {
    if !handle.is_finished() {
        handle.abort();
    }
    match tokio::time::timeout(join_timeout, handle).await {
        Ok(Ok(result)) => Some(result),
        Ok(Err(error)) if error.is_cancelled() => None,
        Ok(Err(error)) => Some(Err(MegaError::Other(format!(
            "multi {name} server task failed to join: {error}"
        )))),
        Err(_) => {
            tracing::warn!("multi {name} child did not join within the timeout");
            None
        }
    }
}
