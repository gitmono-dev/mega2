//! This module is responsible for handling the `service` command.

use std::{future::Future, path::PathBuf, time::Duration};

use clap::{ArgMatches, Command};
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    cli::config_reload_log_subscriber,
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    config::reload::{ConfigHandle, ConfigReloadWatcher},
    context::AppContext,
    jupiter::service::storage_event_emitter::StorageEventEmitter,
};

pub mod http;
pub mod init;
pub mod multi;
pub mod ssh;

const CONFIG_RELOAD_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn cli() -> Command {
    let subcommands = vec![init::cli(), http::cli(), ssh::cli(), multi::cli()];
    Command::new("service")
        .about("Start different kinds of server: for example https or ssh")
        .subcommands(subcommands)
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config_path = ctx.config_path.clone();
    let config_profile_path = ctx.config_profile_path.clone();
    let config = require_config(ctx, "service")?;

    let (cmd, subcommand_args) = match args.subcommand() {
        Some((cmd, args)) => (cmd, args),
        _ => return Ok(()),
    };

    if cmd == "init" {
        return init::exec(config, subcommand_args).await;
    }

    if !matches!(cmd, "http" | "ssh" | "multi") {
        return Err(MegaError::Other(format!(
            "Unknown service subcommand: {cmd}"
        )));
    }

    // The CLI installed a recording Ctrl+C handler before config load, so a
    // signal arriving this early is already captured in the watch channel
    // instead of default-terminating the process.
    let sigint = crate::cli::service_sigint_receiver();
    let context = AppContext::new(config).await?;

    // Every exit after a successful AppContext creation — subscriber or
    // watcher setup failure, subcommand parse/startup/runtime error, Ctrl+C
    // or normal return — goes through the cleanup tail (WH-13). A failed
    // AppContext::new has no emitter to drain; that path belongs to WH-11.
    let emitter = context.storage.storage_event_emitter.clone();
    // Forward the first Ctrl+C to the shared shutdown token: the token is
    // sticky, so a signal landing before a server registers its own handler
    // is still observed by the first select that polls it.
    let signal_forwarder = tokio::spawn({
        let service_shutdown = context.service_shutdown.clone();
        async move {
            match sigint {
                // A signal recorded before this point is already visible in
                // the channel state, so nothing is lost.
                Some(mut rx) => {
                    if !*rx.borrow() {
                        let _ = rx.changed().await;
                    }
                }
                // The recording handler is always installed for service
                // subcommands; absent in-process (tests), fall back to a
                // direct lazy listener.
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            service_shutdown.cancel();
        }
    });
    let setup = async {
        context
            .config_handle
            .subscribe(config_reload_log_subscriber())?;
        spawn_config_reload_watcher(
            context.config_handle.clone(),
            config_path,
            config_profile_path,
        )
        .await
    };

    match setup.await {
        Err(error) => cleanup_tail(Err(error), emitter, None, signal_forwarder).await,
        Ok(reload_watcher) => {
            let service = context.clone();
            run_service_with_cleanup(
                emitter,
                reload_watcher,
                signal_forwarder,
                move || async move {
                    match cmd {
                        "http" => http::exec(service.clone(), subcommand_args).await,
                        "ssh" => ssh::exec(service.clone(), subcommand_args).await,
                        "multi" => multi::exec(service.clone(), subcommand_args).await,
                        _ => Err(MegaError::Other(format!(
                            "Unknown service subcommand: {cmd}"
                        ))),
                    }
                },
            )
            .await
        }
    }
}

/// Runs the service body under the unified cleanup tail (WH-13).
async fn run_service_with_cleanup<Run, Service>(
    emitter: StorageEventEmitter,
    reload_watcher: Option<ConfigReloadWatcherTask>,
    signal_forwarder: JoinHandle<()>,
    runner: Run,
) -> MegaResult
where
    Run: FnOnce() -> Service,
    Service: Future<Output = MegaResult>,
{
    let result = runner().await;
    cleanup_tail(result, emitter, reload_watcher, signal_forwarder).await
}

/// Cleanup order is fixed: the signal forwarder is retired first so it never
/// outlives the service, then the reload watcher stops (keeping the existing
/// result precedence), the storage-event emitter drains, and only then is the
/// completion receipt logged (AC6) with category fields only — no URLs,
/// secrets or bodies (ADR-WH-02). Cleanup never overwrites an existing error
/// (AC2/AC3/AC4).
async fn cleanup_tail(
    result: MegaResult,
    emitter: StorageEventEmitter,
    reload_watcher: Option<ConfigReloadWatcherTask>,
    signal_forwarder: JoinHandle<()>,
) -> MegaResult {
    signal_forwarder.abort();
    let _ = signal_forwarder.await;

    let mut result = result;
    if let Some(reload_watcher) = reload_watcher
        && let Err(stop_error) = reload_watcher.stop().await
    {
        if result.is_ok() {
            result = Err(stop_error);
        } else {
            tracing::warn!(
                error = %stop_error,
                "config reload watcher failed to stop after service error"
            );
        }
    }

    emitter.shutdown().await;
    tracing::info!(category = "lifecycle", "storage_events_shutdown_complete");
    result
}

struct ConfigReloadWatcherTask {
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl ConfigReloadWatcherTask {
    async fn stop(self) -> Result<(), MegaError> {
        let _ = self.shutdown.send(true);

        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .map_err(|_| {
                MegaError::Other("config reload watcher did not stop within timeout".to_string())
            })?
            .map_err(|error| {
                MegaError::Other(format!(
                    "config reload watcher task failed to join: {error}"
                ))
            })
    }
}

async fn spawn_config_reload_watcher(
    config_handle: ConfigHandle,
    config_path: Option<PathBuf>,
    profile_path: Option<PathBuf>,
) -> Result<Option<ConfigReloadWatcherTask>, MegaError> {
    spawn_config_reload_watcher_with_interval(
        config_handle,
        config_path,
        profile_path,
        CONFIG_RELOAD_POLL_INTERVAL,
    )
    .await
}

async fn spawn_config_reload_watcher_with_interval(
    config_handle: ConfigHandle,
    config_path: Option<PathBuf>,
    profile_path: Option<PathBuf>,
    poll_interval: Duration,
) -> Result<Option<ConfigReloadWatcherTask>, MegaError> {
    let Some(config_path) = config_path else {
        return Ok(None);
    };

    let profile_path_display = profile_path.as_ref().map(|path| path.display().to_string());
    let watcher = ConfigReloadWatcher::new(
        config_handle,
        config_path.clone(),
        profile_path,
        poll_interval,
    )
    .await?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        if let Err(error) = watcher.run_until_shutdown(shutdown_rx).await {
            tracing::warn!(
                error = %error,
                "config reload watcher stopped with error"
            );
        }
    });

    tracing::info!(
        config_path = %config_path.display(),
        profile_path = %profile_path_display.as_deref().unwrap_or("<none>"),
        "config reload watcher started"
    );

    Ok(Some(ConfigReloadWatcherTask { shutdown, handle }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, reload::ConfigHandle, template::config_init_template, testing::isolated_config,
    };

    #[test]
    fn service_cli_contains_mega_service_subcommands() {
        let names = cli()
            .get_subcommands()
            .map(|cmd| cmd.get_name().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["init", "http", "ssh", "multi"]);
    }

    #[test]
    fn service_registers_log_reload_subscriber_for_config_handle() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.log.level = "info".to_string();
        let handle = ConfigHandle::new(config);

        handle
            .subscribe(config_reload_log_subscriber())
            .expect("subscribe");
        let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
        candidate.log.level = "debug".to_string();

        let report = handle.reload(candidate).expect("reload should succeed");

        assert_eq!(report.applied_fields, vec!["log.level"]);
        assert_eq!(handle.snapshot().expect("snapshot").log.level, "debug");

        let mut restore = handle.snapshot().expect("snapshot").as_ref().clone();
        restore.log.level = "info".to_string();
        let restore_report = handle.reload(restore).expect("restore should succeed");

        assert_eq!(restore_report.applied_fields, vec!["log.level"]);
        assert_eq!(handle.snapshot().expect("snapshot").log.level, "info");
    }

    #[tokio::test]
    async fn service_reload_watcher_task_reloads_changed_profile() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        std::fs::write(&config_path, config_init_template(temp_dir.path())).expect("base config");

        let config = Config::new(config_path.to_str().expect("utf-8 config path"))
            .expect("base config should load");
        let handle = ConfigHandle::new(config);
        let task = spawn_config_reload_watcher_with_interval(
            handle.clone(),
            Some(config_path),
            Some(profile_path.clone()),
            Duration::from_millis(10),
        )
        .await
        .expect("watcher should start")
        .expect("watcher task");

        std::fs::write(&profile_path, "[log]\nlevel = \"debug\"\n").expect("profile config update");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if handle.snapshot().expect("snapshot").log.level == "debug" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("reload watcher should apply profile change");

        task.stop().await.expect("watcher should stop");
    }

    // ----- WH-13: unified cleanup tail coverage -----

    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
    };

    use bytes::Bytes;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::{
        config::{StorageEventsTargetConfig, secret::SecretString},
        jupiter::service::{
            storage_event::{CommittedEvent, EventData, EventScope, EventSource, EventType},
            storage_event_emitter::AdmissionDisposition,
            storage_event_transport::{
                EventTarget, EventTransport, TransportError, TransportSuccess,
            },
        },
    };

    /// Blocks each delivery until released; the drop probe records whether an
    /// in-flight send completed or was cancelled by the drain.
    struct BlockingTransport {
        posts: Arc<std::sync::Mutex<usize>>,
        completed: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        release: watch::Sender<bool>,
    }

    impl BlockingTransport {
        fn new() -> Arc<Self> {
            let (release, _) = watch::channel(false);
            Arc::new(Self {
                posts: Arc::new(std::sync::Mutex::new(0)),
                completed: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(AtomicUsize::new(0)),
                release,
            })
        }

        fn posted(&self) -> usize {
            *self.posts.lock().expect("posts")
        }

        fn completed(&self) -> usize {
            self.completed.load(AtomicOrdering::SeqCst)
        }

        fn cancelled(&self) -> usize {
            self.cancelled.load(AtomicOrdering::SeqCst)
        }

        fn release(&self) {
            let _ = self.release.send(true);
        }
    }

    struct SendProbe {
        completed: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        finished: bool,
    }

    impl Drop for SendProbe {
        fn drop(&mut self) {
            if self.finished {
                self.completed.fetch_add(1, AtomicOrdering::SeqCst);
            } else {
                self.cancelled.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
    }

    impl EventTransport for BlockingTransport {
        fn post(
            &self,
            _target: &EventTarget,
            _body: Bytes,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<TransportSuccess, TransportError>> + Send>>
        {
            let posts = Arc::clone(&self.posts);
            let completed = Arc::clone(&self.completed);
            let cancelled = Arc::clone(&self.cancelled);
            let mut release = self.release.subscribe();
            Box::pin(async move {
                let mut probe = SendProbe {
                    completed,
                    cancelled,
                    finished: false,
                };
                *posts.lock().expect("posts") += 1;
                if !*release.borrow() {
                    let _ = release.changed().await;
                }
                probe.finished = true;
                Ok(TransportSuccess::Accepted2xx { status: 200 })
            })
        }
    }

    fn cleanup_compiled_target(id: &str) -> (StorageEventsTargetConfig, EventTarget) {
        let config = StorageEventsTargetConfig {
            id: id.to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: format!(
                "vault://secret/config/example/storage_events/targets/{id}/hmac#value"
            ),
            events: vec!["lfs.object.uploaded".to_string()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: true,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = EventTarget::compile(&config.id, &config.url, &secret).expect("target");
        (config, compiled)
    }

    fn cleanup_emitter_config(base: PathBuf, grace_seconds: u64) -> Config {
        let mut config = isolated_config(base);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("wh13-cleanup".to_string());
        config.storage_events.max_in_flight = 4;
        config.storage_events.shutdown_grace_seconds = grace_seconds;
        config
    }

    fn cleanup_sample_event() -> CommittedEvent {
        CommittedEvent {
            event_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").expect("uuid"),
            event_type: EventType::LfsObjectUploaded,
            occurred_at: 1,
            source: EventSource::Lfs,
            scope: EventScope::LfsUnscoped,
            data: EventData::LfsObjectUploaded {
                oid: "ab".to_string(),
                size: 1,
            },
        }
    }

    async fn wait_for_cleanup_condition<F: FnMut() -> bool>(mut condition: F) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if condition() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition within 2s");
    }

    /// A watcher whose task already panicked, so `stop()` fails fast.
    fn failing_watcher() -> ConfigReloadWatcherTask {
        let (shutdown, _) = watch::channel(false);
        let handle = tokio::spawn(async {
            panic!("deliberate watcher failure for cleanup test");
        });
        ConfigReloadWatcherTask { shutdown, handle }
    }

    struct CancelFlag(Arc<AtomicBool>);

    impl Drop for CancelFlag {
        fn drop(&mut self) {
            self.0.store(true, AtomicOrdering::SeqCst);
        }
    }

    /// Stand-in for the signal forwarder the production tail retires.
    fn dummy_forwarder() -> JoinHandle<()> {
        tokio::spawn(std::future::pending())
    }

    /// Sibling-style child: signals readiness, then pends forever; the drop
    /// probe records the abort.
    fn pending_child(
        started: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            started.store(true, AtomicOrdering::SeqCst);
            let _flag = CancelFlag(cancelled);
            std::future::pending::<MegaResult>().await
        })
    }

    /// First-style child: signals readiness, then waits for the release
    /// signal before producing its result, so the test decides when the first
    /// completion happens — after both children are running.
    fn released_child(
        started: Arc<AtomicBool>,
        mut release: watch::Receiver<bool>,
        outcome: Result<(), &'static str>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            started.store(true, AtomicOrdering::SeqCst);
            if !*release.borrow() {
                let _ = release.changed().await;
            }
            outcome.map_err(|message| MegaError::Other(message.to_string()))
        })
    }

    /// Records abort-only cancellation: the flag is set when the future is
    /// dropped before it finished, never on a graceful completion.
    struct GraceProbe {
        cancelled: Arc<AtomicBool>,
        completed: bool,
    }

    impl Drop for GraceProbe {
        fn drop(&mut self) {
            if !self.completed {
                self.cancelled.store(true, AtomicOrdering::SeqCst);
            }
        }
    }

    /// http-style child: signals readiness, then shuts down gracefully `delay`
    /// after the shared token fires with the given outcome; the probe records
    /// a forced abort.
    fn graceful_child(
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        token: CancellationToken,
        delay: Duration,
        outcome: Result<(), &'static str>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            let mut probe = GraceProbe {
                cancelled,
                completed: false,
            };
            started.store(true, AtomicOrdering::SeqCst);
            token.cancelled().await;
            tokio::time::sleep(delay).await;
            finished.store(true, AtomicOrdering::SeqCst);
            probe.completed = true;
            outcome.map_err(|message| MegaError::Other(message.to_string()))
        })
    }

    /// Drop probe recording the shared sequence number of the terminal event,
    /// so the test can prove which supervisor branch ran first. The optional
    /// gate is opened only after the sequence number is stored, so a gated
    /// child waiting on it can never overtake the recorded abort.
    struct SeqProbe {
        slot: Arc<AtomicU64>,
        sequence: Arc<AtomicU64>,
        gate: Option<Arc<AtomicBool>>,
        completed: bool,
    }

    impl SeqProbe {
        fn next(sequence: &AtomicU64) -> u64 {
            sequence.fetch_add(1, AtomicOrdering::SeqCst) + 1
        }
    }

    impl Drop for SeqProbe {
        fn drop(&mut self) {
            if !self.completed {
                let seq = Self::next(&self.sequence);
                self.slot.store(seq, AtomicOrdering::SeqCst);
                if let Some(gate) = &self.gate {
                    gate.store(true, AtomicOrdering::SeqCst);
                }
            }
        }
    }

    /// Sequence-recording pending child: its abort records the sequence slot
    /// and then opens the gate the http-style child waits on.
    fn pending_child_seq(
        started: Arc<AtomicBool>,
        gate: Arc<AtomicBool>,
        aborted_seq: Arc<AtomicU64>,
        sequence: Arc<AtomicU64>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            started.store(true, AtomicOrdering::SeqCst);
            let _probe = SeqProbe {
                slot: aborted_seq,
                sequence,
                gate: Some(gate),
                completed: false,
            };
            std::future::pending::<MegaResult>().await
        })
    }

    /// Token-arm gated children complete only after the ssh-style sibling has
    /// actually been aborted (they wait on its drop-probe gate). The ssh abort
    /// happens only inside the supervisor's token arm, so the http child can
    /// never be ready before that arm runs — branch selection in these cases
    /// is structural, not timed.
    fn gated_graceful_child_seq(
        started: Arc<AtomicBool>,
        finished_seq: Arc<AtomicU64>,
        cancelled_seq: Arc<AtomicU64>,
        gate: Arc<AtomicBool>,
        sequence: Arc<AtomicU64>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            let mut probe = SeqProbe {
                slot: cancelled_seq,
                sequence: sequence.clone(),
                gate: None,
                completed: false,
            };
            started.store(true, AtomicOrdering::SeqCst);
            while !gate.load(AtomicOrdering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let seq = SeqProbe::next(&sequence);
            finished_seq.store(seq, AtomicOrdering::SeqCst);
            probe.completed = true;
            Ok(())
        })
    }

    /// Gate-waiting http-style child with an injected outcome; the probe
    /// records a forced abort.
    fn gated_graceful_child(
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        gate: Arc<AtomicBool>,
        outcome: Result<(), &'static str>,
    ) -> JoinHandle<MegaResult> {
        tokio::spawn(async move {
            let mut probe = GraceProbe {
                cancelled,
                completed: false,
            };
            started.store(true, AtomicOrdering::SeqCst);
            while !gate.load(AtomicOrdering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            finished.store(true, AtomicOrdering::SeqCst);
            probe.completed = true;
            outcome.map_err(|message| MegaError::Other(message.to_string()))
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storage_events_cleanup_paths() {
        let temp_dir = tempfile::tempdir().expect("temp dir");

        // A runner error after AppContext creation is preserved, and the
        // emitter still drains (AC2). Grace is zero, so the blocked send is
        // aborted and joined by the drain.
        let transport = BlockingTransport::new();
        let emitter = StorageEventEmitter::new_with_transport(
            &cleanup_emitter_config(temp_dir.path().join("runner-error"), 0),
            transport.clone(),
            vec![cleanup_compiled_target("ops-main")],
        );
        let probe = emitter.clone();
        let runner_emitter = emitter.clone();
        let runner_transport = transport.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_service_with_cleanup(emitter, None, dummy_forwarder(), move || async move {
                assert!(matches!(
                    runner_emitter.try_emit(cleanup_sample_event()),
                    AdmissionDisposition::Accepted { .. }
                ));
                wait_for_cleanup_condition(|| runner_transport.posted() >= 1).await;
                Err(MegaError::Other("runner boom".to_string()))
            }),
        )
        .await
        .expect("cleanup tail bounded");
        let error = result.expect_err("runner error must be preserved");
        assert!(
            error.to_string().contains("runner boom"),
            "unexpected error: {error}"
        );
        // The tail awaited the drain: the blocked send was aborted, and
        // admission is closed afterwards.
        assert_eq!(transport.cancelled(), 1);
        assert_eq!(transport.completed(), 0);
        assert_eq!(
            probe.try_emit(cleanup_sample_event()),
            AdmissionDisposition::DroppedClosed
        );

        // Normal return drains too (AC4): nonzero grace lets the in-flight
        // send complete instead of being aborted.
        let transport = BlockingTransport::new();
        let emitter = StorageEventEmitter::new_with_transport(
            &cleanup_emitter_config(temp_dir.path().join("normal-return"), 5),
            transport.clone(),
            vec![cleanup_compiled_target("ops-main")],
        );
        let runner_emitter = emitter.clone();
        let runner_transport = transport.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_service_with_cleanup(emitter, None, dummy_forwarder(), move || async move {
                assert!(matches!(
                    runner_emitter.try_emit(cleanup_sample_event()),
                    AdmissionDisposition::Accepted { .. }
                ));
                wait_for_cleanup_condition(|| runner_transport.posted() >= 1).await;
                runner_transport.release();
                Ok(())
            }),
        )
        .await
        .expect("cleanup tail bounded");
        result.expect("normal return must stay Ok");
        assert_eq!(transport.completed(), 1);
        assert_eq!(transport.cancelled(), 0);

        // A send blocked past the grace bound is aborted: the tail finishes
        // within a bounded wait instead of stalling behind the transport.
        let transport = BlockingTransport::new();
        let emitter = StorageEventEmitter::new_with_transport(
            &cleanup_emitter_config(temp_dir.path().join("blocking"), 0),
            transport.clone(),
            vec![cleanup_compiled_target("ops-main")],
        );
        let runner_emitter = emitter.clone();
        let runner_transport = transport.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_service_with_cleanup(emitter, None, dummy_forwarder(), move || async move {
                assert!(matches!(
                    runner_emitter.try_emit(cleanup_sample_event()),
                    AdmissionDisposition::Accepted { .. }
                ));
                wait_for_cleanup_condition(|| runner_transport.posted() >= 1).await;
                Ok(())
            }),
        )
        .await
        .expect("cleanup tail must finish within the grace bound");
        result.expect("blocking runner result must be preserved");
        assert_eq!(transport.cancelled(), 1);
        assert_eq!(transport.completed(), 0);

        // Watcher stop failure precedence: it replaces an Ok result but never
        // overwrites the original service error.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            cleanup_tail(
                Ok(()),
                StorageEventEmitter::disabled(),
                Some(failing_watcher()),
                dummy_forwarder(),
            ),
        )
        .await
        .expect("cleanup tail bounded");
        let error = result.expect_err("watcher stop failure replaces an Ok result");
        assert!(
            error.to_string().contains("config reload watcher"),
            "unexpected error: {error}"
        );

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            cleanup_tail(
                Err(MegaError::Other("original boom".to_string())),
                StorageEventEmitter::disabled(),
                Some(failing_watcher()),
                dummy_forwarder(),
            ),
        )
        .await
        .expect("cleanup tail bounded");
        let error = result.expect_err("original error must survive cleanup");
        assert!(
            error.to_string().contains("original boom"),
            "cleanup overwrote the original error: {error}"
        );

        // Multi first completion (AC5): both children signal readiness before
        // the first one is released, so the abort never races construction.
        // The first child result is the overall result.
        let token = CancellationToken::new();
        let first_started = Arc::new(AtomicBool::new(false));
        let sibling_started = Arc::new(AtomicBool::new(false));
        let sibling_cancelled = Arc::new(AtomicBool::new(false));
        let (first_release, first_release_rx) = watch::channel(false);
        let driver = tokio::spawn(multi::run_until_first_child(
            released_child(
                first_started.clone(),
                first_release_rx,
                Err("first child boom"),
            ),
            pending_child(sibling_started.clone(), sibling_cancelled.clone()),
            token.clone(),
        ));
        wait_for_cleanup_condition(|| {
            first_started.load(AtomicOrdering::SeqCst)
                && sibling_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        let _ = first_release.send(true);
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("first completion must not block on the sibling")
            .expect("driver task");
        let error = result.expect_err("first child error must be preserved");
        assert!(
            error.to_string().contains("first child boom"),
            "sibling abort overwrote the first child error: {error}"
        );
        assert!(sibling_cancelled.load(AtomicOrdering::SeqCst));

        let first_started = Arc::new(AtomicBool::new(false));
        let sibling_started = Arc::new(AtomicBool::new(false));
        let sibling_cancelled = Arc::new(AtomicBool::new(false));
        let (first_release, first_release_rx) = watch::channel(false);
        let driver = tokio::spawn(multi::run_until_first_child(
            released_child(first_started.clone(), first_release_rx, Ok(())),
            pending_child(sibling_started.clone(), sibling_cancelled.clone()),
            token.clone(),
        ));
        wait_for_cleanup_condition(|| {
            first_started.load(AtomicOrdering::SeqCst)
                && sibling_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        let _ = first_release.send(true);
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("first completion must not block on the sibling")
            .expect("driver task");
        result.expect("first child Ok must be preserved");
        assert!(sibling_cancelled.load(AtomicOrdering::SeqCst));

        // Multi signal path (AC1/AC5), deadlines at millisecond scale. With
        // Serve inlined into start_http, aborting a wrapper-level child equals
        // an actual server stop — the nested-detachment concern is gone.
        // (a) The http-style child never stops on its own: the ssh-style
        // pending sibling is aborted immediately, the http grace expires and
        // the http child is aborted too; overall Ok.
        let token = CancellationToken::new();
        let http_started = Arc::new(AtomicBool::new(false));
        let http_cancelled = Arc::new(AtomicBool::new(false));
        let ssh_started = Arc::new(AtomicBool::new(false));
        let ssh_cancelled = Arc::new(AtomicBool::new(false));
        let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
            pending_child(http_started.clone(), http_cancelled.clone()),
            pending_child(ssh_started.clone(), ssh_cancelled.clone()),
            token.clone(),
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(200),
        ));
        wait_for_cleanup_condition(|| {
            http_started.load(AtomicOrdering::SeqCst) && ssh_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("signal path must stay bounded")
            .expect("driver task");
        result.expect("signal path returns Ok");
        assert!(ssh_cancelled.load(AtomicOrdering::SeqCst));
        assert!(http_cancelled.load(AtomicOrdering::SeqCst));

        // (b) Token arm with graceful http success. The http child completes
        // only after the ssh sibling has actually been aborted (it waits on
        // the sibling's drop-probe gate), and the ssh abort happens only
        // inside the supervisor's token arm — so the http arm can never be
        // ready first and branch selection is structural, not timed. The
        // sequence numbers assert that order: ssh abort before http finish.
        let token = CancellationToken::new();
        let sequence = Arc::new(AtomicU64::new(0));
        let http_started = Arc::new(AtomicBool::new(false));
        let http_finished_seq = Arc::new(AtomicU64::new(0));
        let http_cancelled_seq = Arc::new(AtomicU64::new(0));
        let ssh_started = Arc::new(AtomicBool::new(false));
        let ssh_aborted_seq = Arc::new(AtomicU64::new(0));
        let ssh_gate = Arc::new(AtomicBool::new(false));
        let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
            gated_graceful_child_seq(
                http_started.clone(),
                http_finished_seq.clone(),
                http_cancelled_seq.clone(),
                ssh_gate.clone(),
                sequence.clone(),
            ),
            pending_child_seq(
                ssh_started.clone(),
                ssh_gate.clone(),
                ssh_aborted_seq.clone(),
                sequence.clone(),
            ),
            token.clone(),
            Duration::from_millis(200),
            Duration::from_secs(2),
            Duration::from_millis(200),
        ));
        wait_for_cleanup_condition(|| {
            http_started.load(AtomicOrdering::SeqCst) && ssh_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("signal path must stay bounded")
            .expect("driver task");
        result.expect("signal path returns Ok");
        let ssh_aborted = ssh_aborted_seq.load(AtomicOrdering::SeqCst);
        let http_finished = http_finished_seq.load(AtomicOrdering::SeqCst);
        assert!(ssh_aborted > 0, "ssh child must have been aborted");
        assert!(
            http_finished > 0,
            "http child must have finished gracefully"
        );
        assert_eq!(http_cancelled_seq.load(AtomicOrdering::SeqCst), 0);
        assert!(
            ssh_aborted < http_finished,
            "token arm aborts ssh before http finishes: ssh={ssh_aborted} http={http_finished}"
        );

        // ssh-first (AC5 in both directions): the ssh child result is the
        // overall result, and the http-style child completes only when the
        // token fires — which the ssh-first arm triggers — so it finishes
        // gracefully and is never aborted.
        for outcome in [Ok(()), Err("ssh child boom")] {
            let token = CancellationToken::new();
            let http_started = Arc::new(AtomicBool::new(false));
            let http_finished = Arc::new(AtomicBool::new(false));
            let http_cancelled = Arc::new(AtomicBool::new(false));
            let ssh_started = Arc::new(AtomicBool::new(false));
            let (ssh_release, ssh_release_rx) = watch::channel(false);
            let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
                graceful_child(
                    http_started.clone(),
                    http_finished.clone(),
                    http_cancelled.clone(),
                    token.clone(),
                    Duration::from_millis(200),
                    Ok(()),
                ),
                released_child(ssh_started.clone(), ssh_release_rx, outcome),
                token.clone(),
                Duration::from_millis(200),
                Duration::from_secs(2),
                Duration::from_millis(200),
            ));
            wait_for_cleanup_condition(|| {
                http_started.load(AtomicOrdering::SeqCst)
                    && ssh_started.load(AtomicOrdering::SeqCst)
            })
            .await;
            let _ = ssh_release.send(true);
            let result = tokio::time::timeout(Duration::from_secs(5), driver)
                .await
                .expect("ssh-first completion must stay bounded")
                .expect("driver task");
            match outcome {
                Ok(()) => result.expect("ssh child Ok is the overall result"),
                Err(message) => {
                    let error = result.expect_err("ssh child error is the overall result");
                    assert!(
                        error.to_string().contains(message),
                        "unexpected overall result: {error}"
                    );
                }
            }
            assert!(http_finished.load(AtomicOrdering::SeqCst));
            assert!(!http_cancelled.load(AtomicOrdering::SeqCst));
        }

        // Signal path with a genuine http error: the token arm joins the
        // graceful http child and propagates its error; the aborted ssh child
        // contributes nothing. The http child is gated on the ssh abort, so
        // the token arm is the only arm that can be ready first.
        let token = CancellationToken::new();
        let http_started = Arc::new(AtomicBool::new(false));
        let http_finished = Arc::new(AtomicBool::new(false));
        let http_cancelled = Arc::new(AtomicBool::new(false));
        let ssh_started = Arc::new(AtomicBool::new(false));
        let ssh_cancelled = Arc::new(AtomicBool::new(false));
        let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
            gated_graceful_child(
                http_started.clone(),
                http_finished.clone(),
                http_cancelled.clone(),
                ssh_cancelled.clone(),
                Err("http boom"),
            ),
            pending_child(ssh_started.clone(), ssh_cancelled.clone()),
            token.clone(),
            Duration::from_millis(200),
            Duration::from_secs(2),
            Duration::from_millis(200),
        ));
        wait_for_cleanup_condition(|| {
            http_started.load(AtomicOrdering::SeqCst) && ssh_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("signal path must stay bounded")
            .expect("driver task");
        let error = result.expect_err("genuine http error must survive the signal path");
        assert!(
            error.to_string().contains("http boom"),
            "unexpected overall result: {error}"
        );
        assert!(ssh_cancelled.load(AtomicOrdering::SeqCst));
        assert!(http_finished.load(AtomicOrdering::SeqCst));
        assert!(!http_cancelled.load(AtomicOrdering::SeqCst));

        // Genuine-error vs expected-abort distinction, pinned at the helper:
        // an already-finished child yields its real result; a pending child
        // is aborted and reports nothing. This feeds the token arm's
        // http-before-ssh precedence (a supervisor-level setup where both
        // arms are simultaneously ready is scheduler-dependent, so the
        // distinction is pinned here; the case above covers http-error
        // propagation through the token arm).
        let finished_err = tokio::spawn(async { Err(MegaError::Other("ssh boom".to_string())) });
        wait_for_cleanup_condition(|| finished_err.is_finished()).await;
        let mut finished_err = finished_err;
        let outcome = multi::abort_child_unless_finished(
            "ssh",
            &mut finished_err,
            Duration::from_millis(200),
        )
        .await;
        let outcome = outcome.expect("finished child must yield its genuine result");
        let error = outcome.expect_err("genuine error must be preserved");
        assert!(
            error.to_string().contains("ssh boom"),
            "unexpected helper result: {error}"
        );

        let pending_cancelled = Arc::new(AtomicBool::new(false));
        let pending_started = Arc::new(AtomicBool::new(false));
        let mut pending = pending_child(pending_started.clone(), pending_cancelled.clone());
        wait_for_cleanup_condition(|| pending_started.load(AtomicOrdering::SeqCst)).await;
        let outcome =
            multi::abort_child_unless_finished("ssh", &mut pending, Duration::from_millis(200))
                .await;
        assert!(outcome.is_none(), "expected abort-cancellation is no error");
        assert!(pending_cancelled.load(AtomicOrdering::SeqCst));

        // ssh already finished with a genuine error when shutdown starts: the
        // overall result is the ssh error and the http child still stops
        // gracefully through the token. The assertion is branch-agnostic on
        // purpose: the ssh-first arm preserves the result, and the token arm
        // collects the finished child's error via the is_finished path — both
        // produce "ssh boom" with a graceful http stop.
        let token = CancellationToken::new();
        let http_started = Arc::new(AtomicBool::new(false));
        let http_finished = Arc::new(AtomicBool::new(false));
        let http_cancelled = Arc::new(AtomicBool::new(false));
        let ssh_started = Arc::new(AtomicBool::new(false));
        let ssh_finished = Arc::new(AtomicBool::new(false));
        let ssh_like = tokio::spawn({
            let ssh_started = ssh_started.clone();
            let ssh_finished = ssh_finished.clone();
            async move {
                ssh_started.store(true, AtomicOrdering::SeqCst);
                ssh_finished.store(true, AtomicOrdering::SeqCst);
                Err(MegaError::Other("ssh boom".to_string()))
            }
        });
        let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
            graceful_child(
                http_started.clone(),
                http_finished.clone(),
                http_cancelled.clone(),
                token.clone(),
                Duration::from_millis(200),
                Ok(()),
            ),
            ssh_like,
            token.clone(),
            Duration::from_millis(200),
            Duration::from_secs(2),
            Duration::from_millis(200),
        ));
        wait_for_cleanup_condition(|| {
            ssh_finished.load(AtomicOrdering::SeqCst) && http_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("finished-sibling path must stay bounded")
            .expect("driver task");
        let error = result.expect_err("finished ssh child error must be the overall result");
        assert!(
            error.to_string().contains("ssh boom"),
            "unexpected overall result: {error}"
        );
        assert!(http_finished.load(AtomicOrdering::SeqCst));
        assert!(!http_cancelled.load(AtomicOrdering::SeqCst));

        // Panic on the signal path: the http child's panic is a genuine
        // failure (non-cancelled JoinError), surfaced as the overall error.
        // The panic fires only after the ssh sibling's abort opens the gate,
        // so the token arm is structurally the only branch that can run.
        let token = CancellationToken::new();
        let http_started = Arc::new(AtomicBool::new(false));
        let ssh_started = Arc::new(AtomicBool::new(false));
        let ssh_cancelled = Arc::new(AtomicBool::new(false));
        let http_like = tokio::spawn({
            let http_started = http_started.clone();
            let gate = ssh_cancelled.clone();
            async move {
                http_started.store(true, AtomicOrdering::SeqCst);
                while !gate.load(AtomicOrdering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                panic!("deliberate http child panic for multi signal test");
            }
        });
        let driver = tokio::spawn(multi::run_until_first_child_with_deadlines(
            http_like,
            pending_child(ssh_started.clone(), ssh_cancelled.clone()),
            token.clone(),
            Duration::from_millis(200),
            Duration::from_secs(2),
            Duration::from_millis(200),
        ));
        wait_for_cleanup_condition(|| {
            http_started.load(AtomicOrdering::SeqCst) && ssh_started.load(AtomicOrdering::SeqCst)
        })
        .await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("signal path must stay bounded")
            .expect("driver task");
        let error = result.expect_err("http panic must surface as the overall error");
        assert!(
            error.to_string().contains("panicked"),
            "overall error must mention the panic: {error}"
        );
        assert!(ssh_cancelled.load(AtomicOrdering::SeqCst));
    }
}
