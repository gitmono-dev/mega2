use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    config::{
        Config, ViewsConfig,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    jupiter::{
        service::{view_metrics::ViewMetrics, view_projection_service::ViewProjectionService},
        storage::{
            Storage, base_storage::StorageConnector, view_root_chain::RootChainOutcome,
            view_storage::ViewLockMode,
        },
    },
};

pub(crate) type ViewWorkerFuture = Pin<Box<dyn Future<Output = Result<(), MegaError>> + Send>>;
pub(crate) type ViewWorkerRound = Arc<dyn Fn(Storage) -> ViewWorkerFuture + Send + Sync>;
pub(crate) type PerCandidateFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    crate::jupiter::service::view_projection_service::CatchUpOutcome,
                    MegaError,
                >,
            > + Send,
    >,
>;
pub(crate) type PerCandidate = Arc<dyn Fn(i64) -> PerCandidateFuture + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct ViewSignal {
    notify: Arc<Notify>,
}

impl ViewSignal {
    pub(crate) fn notify_worker(&self) {
        self.notify.notify_one();
    }

    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }
}

pub(crate) struct ViewRuntime {
    metrics: ViewMetrics,
    signal: ViewSignal,
    service_slot: Arc<OnceLock<ViewProjectionService>>,
}

impl ViewRuntime {
    pub(crate) fn new() -> Self {
        Self {
            metrics: ViewMetrics::default(),
            signal: ViewSignal::default(),
            service_slot: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn metrics(&self) -> ViewMetrics {
        self.metrics.clone()
    }

    pub(crate) fn signal(&self) -> ViewSignal {
        self.signal.clone()
    }

    pub(crate) fn service_slot(&self) -> Arc<OnceLock<ViewProjectionService>> {
        self.service_slot.clone()
    }

    pub(crate) fn without_service_slot(&self) -> Self {
        Self {
            metrics: self.metrics(),
            signal: self.signal(),
            service_slot: Arc::new(OnceLock::new()),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ViewWorkerTaskControl {
    sender: watch::Sender<ViewsConfig>,
}

impl ViewWorkerTaskControl {
    pub(crate) fn new(config: ViewsConfig) -> Self {
        let (sender, _) = watch::channel(config);
        Self { sender }
    }

    pub(crate) fn current(&self) -> ViewsConfig {
        self.sender.borrow().clone()
    }

    fn subscribe(&self) -> watch::Receiver<ViewsConfig> {
        self.sender.subscribe()
    }

    fn set_config(&self, config: ViewsConfig) {
        self.sender.send_replace(config);
    }
}

pub(crate) fn config_reload_view_worker_subscriber(
    control: ViewWorkerTaskControl,
) -> ConfigReloadSubscriber {
    let apply_control = control.clone();
    ConfigReloadSubscriber::new(
        "view_worker_task",
        move |next, report| apply_view_worker_config(&apply_control, next, report),
        move |current, report| apply_view_worker_config(&control, current, report),
    )
}

fn apply_view_worker_config(
    control: &ViewWorkerTaskControl,
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    if report
        .applied_fields
        .iter()
        .any(|field| field.starts_with("views."))
    {
        control.set_config(config.views.clone());
    }

    Ok(())
}

pub(crate) fn spawn_view_worker_with_round(
    storage: Storage,
    token: CancellationToken,
    round: ViewWorkerRound,
) -> Result<Option<JoinHandle<()>>, MegaError> {
    let config = storage.config().views.clone();
    if !config.enabled {
        return Ok(None);
    }

    let control = ViewWorkerTaskControl::new(config);
    storage
        .config_handle()
        .subscribe(config_reload_view_worker_subscriber(control.clone()))?;
    let mut config_updates = control.subscribe();

    Ok(Some(tokio::spawn(async move {
        let mut config = control.current();
        let mut ticker = new_ticker(&config);
        let mut accepts_config_updates = true;
        let mut running_round: Option<ViewWorkerFuture> = None;

        tracing::info!(
            interval_secs = config.worker_interval_secs.max(1),
            batch_size = config.batch_size,
            "view projection worker started"
        );

        loop {
            if let Some(round_future) = running_round.take() {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        tracing::info!("view projection worker cancelled an in-progress round");
                        break;
                    }
                    result = round_future => {
                        if let Err(error) = result {
                            tracing::error!(error = %error, "view projection worker round failed");
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    tracing::info!("view projection worker received shutdown signal");
                    break;
                }
                changed = config_updates.changed(), if accepts_config_updates => {
                    match changed {
                        Ok(()) => {
                            let updated = config_updates.borrow().clone();
                            let interval_changed =
                                updated.worker_interval_secs.max(1) != config.worker_interval_secs.max(1);
                            config = updated;
                            if interval_changed {
                                ticker = new_ticker(&config);
                            }
                            tracing::info!(
                                interval_secs = config.worker_interval_secs.max(1),
                                batch_size = config.batch_size,
                                "view projection worker config updated"
                            );
                        }
                        Err(_) => {
                            accepts_config_updates = false;
                            tracing::warn!(
                                "view projection worker config update channel closed; continuing with last config"
                            );
                        }
                    }
                }
                _ = ticker.tick(), if config.enabled => {
                    running_round = Some(round(storage.clone()));
                }
            }
        }

        tracing::info!("view projection worker stopped gracefully");
    })))
}

fn new_ticker(config: &ViewsConfig) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval(Duration::from_secs(config.worker_interval_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker
}

pub(crate) async fn compensation_round(
    storage: Storage,
    per_candidate: PerCandidate,
) -> Result<(), MegaError> {
    let snapshot = storage.config();
    let batch_size = usize::try_from(snapshot.views.batch_size)
        .map_err(|_| MegaError::Other("views.batch_size does not fit usize".to_owned()))?;
    let view_storage = storage.view_storage();
    let root_outcome = view_storage
        .extend_root_chain(None, batch_size, ViewLockMode::Try)
        .await?;
    if matches!(root_outcome, RootChainOutcome::Discontinuous(_)) {
        return Ok(());
    }

    for filter_id in candidate_filter_ids(&view_storage).await? {
        if let Err(error) = per_candidate(filter_id).await {
            tracing::error!(filter_id, error = %error, "view projection catch-up failed");
        }
    }

    Ok(())
}

pub(crate) fn production_round() -> ViewWorkerRound {
    Arc::new(|storage| {
        Box::pin(async move {
            let service = storage.view_projection_service();
            let per_candidate: PerCandidate = Arc::new(move |filter_id| {
                let service = service.clone();
                Box::pin(async move { service.catch_up(filter_id).await })
            });
            compensation_round(storage, per_candidate).await
        })
    })
}

async fn candidate_filter_ids(
    storage: &crate::jupiter::storage::view_storage::ViewStorage,
) -> Result<Vec<i64>, MegaError> {
    storage
        .get_connection()
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT view_filter.id \
             FROM mega_view_filter view_filter \
             CROSS JOIN (SELECT COALESCE(MAX(seq), 0) AS max_seq FROM mega_view_root_chain) root \
             WHERE (view_filter.ready_seq IS NOT NULL OR view_filter.warming_since IS NOT NULL) \
               AND (view_filter.projected_seq < root.max_seq OR view_filter.ready_seq IS NULL) \
             ORDER BY view_filter.id ASC"
                .to_owned(),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get("", "id").map_err(MegaError::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        future::poll_fn,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    use chrono::Utc;
    use git_internal::hash::HashKind;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DbBackend, EntityTrait, Statement,
        TransactionTrait,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        callisto::{mega_view_filter, mega_view_root_chain},
        ceres::view::filter::parse_for_registration,
        config::{PushAuth, PushPolicy, reload::ConfigHandle, testing::isolated_config},
        jupiter::{
            service::view_projection_service::CatchUpOutcome,
            storage::{
                base_storage::StorageConnector,
                init::database_connection,
                object_storage::mock_object_storage,
                view_root_chain::DiscontinuityReason,
                view_storage::{ViewLock, ViewLockMode, acquire_view_lock},
                view_test_fixtures::{
                    cas_fixture_main, seed_linear_root_history, seed_single_parent_root_commit,
                    seed_unrelated_root_history, set_fixture_main,
                },
            },
            tests::{test_db_config, test_storage_with_config},
        },
    };

    fn views_config(base_dir: impl AsRef<Path>) -> Config {
        let mut config = isolated_config(base_dir);
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.cedar.enforcement = "off".to_owned();
        config.views.enabled = true;
        assert!(config.validate().is_ok());
        config
    }

    async fn storage_with_root_chain(temp: &tempfile::TempDir, commits: usize) -> Storage {
        let storage =
            test_storage_with_config(temp.path(), views_config(temp.path().join("config"))).await;
        let view_storage = storage.view_storage();
        seed_linear_root_history(view_storage.get_connection(), commits).await;
        assert_eq!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        storage
    }

    async fn insert_filter(
        storage: &Storage,
        id: i64,
        projected_seq: i64,
        ready_seq: Option<i64>,
        warming: bool,
    ) {
        let canonical = parse_for_registration(&format!(":prefix=p{id}")).unwrap();
        mega_view_filter::ActiveModel {
            id: Set(id),
            filter_id: Set(canonical.filter_id),
            canonical_spec: Set(canonical.canonical_text),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!([])),
            push_enabled: Set(false),
            projected_seq: Set(projected_seq),
            ready_seq: Set(ready_seq),
            warming_since: Set(warming.then(|| Utc::now().naive_utc())),
            last_access_at: Set(None),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(storage.view_storage().get_connection())
        .await
        .unwrap();
    }

    async fn wait_for_count(count: &AtomicUsize, expected: usize, limit: Duration) {
        tokio::time::timeout(limit, async {
            while count.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_ready(storage: &Storage, filter_id: i64, tip: i64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = mega_view_filter::Entity::find_by_id(filter_id)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                if row.projected_seq == tip
                    && row.ready_seq == Some(tip)
                    && row.warming_since.is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_projected(storage: &Storage, filter_id: i64, tip: i64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = mega_view_filter::Entity::find_by_id(filter_id)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                if row.projected_seq == tip {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn worker_hot_reload() {
        let temp = tempfile::tempdir().unwrap();
        let config = views_config(temp.path().join("subscriber"));
        let control = ViewWorkerTaskControl::new(config.views.clone());
        let handle = ConfigHandle::new(config.clone());
        handle
            .subscribe(config_reload_view_worker_subscriber(control.clone()))
            .unwrap();
        let mut candidate = config.clone();
        candidate.views.worker_interval_secs = 1;
        candidate.views.batch_size = 128;
        let report = handle.reload(candidate.clone()).unwrap();
        assert_eq!(
            report.applied_fields,
            vec!["views.worker_interval_secs", "views.batch_size"]
        );
        assert_eq!(control.current(), candidate.views);

        let temp = tempfile::tempdir().unwrap();
        let mut config = views_config(temp.path().join("worker"));
        config.views.worker_interval_secs = 60;
        let storage = test_storage_with_config(temp.path(), config).await;
        let token = CancellationToken::new();
        let count = Arc::new(AtomicUsize::new(0));
        let round_count = count.clone();
        let round: ViewWorkerRound = Arc::new(move |_| {
            let round_count = round_count.clone();
            Box::pin(async move {
                round_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&count, 1, Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let mut candidate = storage.config().as_ref().clone();
        candidate.views.worker_interval_secs = 1;
        storage.config_handle().reload(candidate).unwrap();
        wait_for_count(&count, 3, Duration::from_secs(5)).await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        let weak = Arc::downgrade(&storage.view_runtime.service_slot());
        let config_handle = storage.config_handle();
        drop(storage);
        drop(config_handle);
        assert!(weak.upgrade().is_none());

        let temp = tempfile::tempdir().unwrap();
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let worker_db = Arc::new(database_connection(&db_config).await.unwrap());
        let lock_db = Arc::new(database_connection(&db_config).await.unwrap());
        let mut config = views_config(temp.path().join("batch-size"));
        config.database = db_config;
        config.views.worker_interval_secs = 60;
        config.views.batch_size = 3;
        config.views.sync_catch_up_commits = 1;
        let storage =
            Storage::new_with_connection(Arc::new(config), worker_db, mock_object_storage())
                .await
                .unwrap();
        let view_storage = storage.view_storage();
        let mut roots = seed_linear_root_history(view_storage.get_connection(), 2).await;
        view_storage
            .extend_root_chain(None, 3, ViewLockMode::Try)
            .await
            .unwrap();
        insert_filter(&storage, 1, 0, None, true).await;
        assert_eq!(
            storage.view_projection_service().catch_up(1).await.unwrap(),
            CatchUpOutcome::Ready
        );
        let mut parent = roots.last().unwrap().clone();
        for number in 1..=4 {
            let next = seed_single_parent_root_commit(
                view_storage.get_connection(),
                HashKind::Sha1,
                &parent,
                &format!("hp14 batch-size {number}"),
            )
            .await;
            assert!(cas_fixture_main(view_storage.get_connection(), &parent, &next).await);
            roots.push(next.clone());
            parent = next;
        }
        view_storage
            .extend_root_chain(None, 3, ViewLockMode::Try)
            .await
            .unwrap();
        view_storage
            .get_connection()
            .execute_unprepared("DELETE FROM mega_view_root_chain WHERE seq = 4")
            .await
            .unwrap();

        let lock_txn = lock_db.begin().await.unwrap();
        lock_txn
            .execute_unprepared("LOCK TABLE mega_view_filter IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let started_round = started.clone();
        let completed_round = completed.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let started = started_round.clone();
            let completed = completed_round.clone();
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                let result = production(storage).await;
                completed.fetch_add(1, Ordering::SeqCst);
                result
            })
        });
        let token = CancellationToken::new();
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let row = lock_db
                    .query_one_raw(Statement::from_string(
                        DbBackend::Postgres,
                        "SELECT count(*)::bigint AS count FROM pg_locks \
                         WHERE relation = 'mega_view_filter'::regclass AND NOT granted"
                            .to_owned(),
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                let waiting: i64 = row.try_get("", "count").unwrap();
                if waiting >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        let mut candidate = storage.config().as_ref().clone();
        candidate.views.batch_size = 1;
        assert_eq!(
            storage
                .config_handle()
                .reload(candidate)
                .unwrap()
                .applied_fields,
            vec!["views.batch_size"]
        );
        lock_txn.rollback().await.unwrap();
        wait_for_count(&completed, 1, Duration::from_secs(15)).await;
        let row = mega_view_filter::Entity::find_by_id(1)
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.projected_seq, 3);
        token.cancel();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn round_selection() {
        let temp = tempfile::tempdir().unwrap();
        let storage = storage_with_root_chain(&temp, 2).await;
        insert_filter(&storage, 1, 0, Some(2), false).await;
        insert_filter(&storage, 2, 2, Some(2), false).await;
        insert_filter(&storage, 3, 2, None, true).await;
        insert_filter(&storage, 4, 0, None, true).await;
        insert_filter(&storage, 5, 0, None, false).await;

        let selected = Arc::new(Mutex::new(Vec::new()));
        let selected_for_round = selected.clone();
        let service = storage.view_projection_service();
        let per_candidate: PerCandidate = Arc::new(move |filter_id| {
            let selected = selected_for_round.clone();
            let service = service.clone();
            Box::pin(async move {
                selected.lock().unwrap().push(filter_id);
                service.catch_up(filter_id).await
            })
        });
        compensation_round(storage.clone(), per_candidate)
            .await
            .unwrap();
        assert_eq!(*selected.lock().unwrap(), vec![1, 3, 4]);

        let temp = tempfile::tempdir().unwrap();
        let storage = storage_with_root_chain(&temp, 2).await;
        insert_filter(&storage, 1, 0, Some(2), false).await;
        let view_storage = storage.view_storage();
        let unrelated =
            seed_unrelated_root_history(view_storage.get_connection(), HashKind::Sha1).await;
        set_fixture_main(
            view_storage.get_connection(),
            &unrelated.commit.id.to_string(),
            &unrelated.commit.tree_id.to_string(),
        )
        .await;
        let selected = Arc::new(Mutex::new(Vec::new()));
        let selected_for_round = selected.clone();
        let per_candidate: PerCandidate = Arc::new(move |filter_id| {
            let selected = selected_for_round.clone();
            Box::pin(async move {
                selected.lock().unwrap().push(filter_id);
                Ok(CatchUpOutcome::NotRun)
            })
        });
        compensation_round(storage.clone(), per_candidate)
            .await
            .unwrap();
        assert!(selected.lock().unwrap().is_empty());
        assert!(matches!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::Discontinuous(DiscontinuityReason::UnrelatedHistory)
        ));
    }

    #[tokio::test]
    async fn compensation_liveness() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = views_config(temp.path().join("interrupted"));
        config.views.worker_interval_secs = 60;
        config.views.batch_size = 1;
        config.views.sync_catch_up_commits = 1;
        let storage = test_storage_with_config(temp.path(), config).await;
        let view_storage = storage.view_storage();
        seed_linear_root_history(view_storage.get_connection(), 6).await;
        view_storage
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        insert_filter(&storage, 1, 0, None, true).await;
        let service = storage.view_projection_service();
        let began = Arc::new(AtomicUsize::new(0));
        let began_round = began.clone();
        let interrupted_round: ViewWorkerRound = Arc::new(move |storage| {
            let service = service.clone();
            let began = began_round.clone();
            Box::pin(async move {
                let snapshot = storage.config();
                service.catch_up_one_batch(1, &snapshot, 1).await?;
                began.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<(), MegaError>>().await
            })
        });
        let interrupted_token = CancellationToken::new();
        let interrupted =
            spawn_view_worker_with_round(storage.clone(), interrupted_token, interrupted_round)
                .unwrap()
                .unwrap();
        wait_for_count(&began, 1, Duration::from_secs(10)).await;
        assert_eq!(
            mega_view_filter::Entity::find_by_id(1)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap()
                .projected_seq,
            1
        );
        interrupted.abort();
        assert!(interrupted.await.is_err());
        let token = CancellationToken::new();
        let completed = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let completed_round = completed.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let completed = completed_round.clone();
            Box::pin(async move {
                let result = production(storage).await;
                completed.fetch_add(1, Ordering::SeqCst);
                result
            })
        });
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&completed, 1, Duration::from_secs(10)).await;
        wait_until_ready(&storage, 1, 6).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        token.cancel();
        worker.await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let mut config = views_config(temp.path().join("config"));
        config.views.worker_interval_secs = 60;
        let storage = test_storage_with_config(temp.path(), config).await;
        let view_storage = storage.view_storage();
        seed_linear_root_history(view_storage.get_connection(), 2).await;
        view_storage
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        insert_filter(&storage, 1, 2, None, true).await;
        let token = CancellationToken::new();
        let completed = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let completed_round = completed.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let completed = completed_round.clone();
            Box::pin(async move {
                let result = production(storage).await;
                completed.fetch_add(1, Ordering::SeqCst);
                result
            })
        });
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&completed, 1, Duration::from_secs(10)).await;
        wait_until_ready(&storage, 1, 2).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        token.cancel();
        worker.await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let worker_db = Arc::new(database_connection(&db_config).await.unwrap());
        let lock_db = Arc::new(database_connection(&db_config).await.unwrap());
        let mut config = views_config(temp.path().join("lock"));
        config.database = db_config;
        config.views.worker_interval_secs = 1;
        let storage =
            Storage::new_with_connection(Arc::new(config), worker_db, mock_object_storage())
                .await
                .unwrap();
        let view_storage = storage.view_storage();
        let history = seed_linear_root_history(view_storage.get_connection(), 2).await;
        view_storage
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        insert_filter(&storage, 1, 2, Some(2), false).await;
        let next = seed_single_parent_root_commit(
            view_storage.get_connection(),
            HashKind::Sha1,
            history.last().unwrap(),
            "hp14 liveness lock",
        )
        .await;
        assert!(
            cas_fixture_main(
                view_storage.get_connection(),
                history.last().unwrap(),
                &next
            )
            .await
        );
        let lock_txn = lock_db.begin().await.unwrap();
        assert!(
            acquire_view_lock(&lock_txn, ViewLock::Filter(1), ViewLockMode::Try)
                .await
                .unwrap()
        );
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let started_round = started.clone();
        let completed_round = completed.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let started = started_round.clone();
            let completed = completed_round.clone();
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                let result = production(storage).await;
                completed.fetch_add(1, Ordering::SeqCst);
                result
            })
        });
        let token = CancellationToken::new();
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&completed, 1, Duration::from_secs(10)).await;
        assert_eq!(
            mega_view_filter::Entity::find_by_id(1)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap()
                .projected_seq,
            2
        );
        let tail = view_storage
            .get_connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT seq, commit_id FROM mega_view_root_chain ORDER BY seq DESC LIMIT 1"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let tail_seq: i64 = tail.try_get("", "seq").unwrap();
        let tail_commit: String = tail.try_get("", "commit_id").unwrap();
        assert_eq!(tail_seq, 3);
        assert_eq!(tail_commit, next.commit.id.to_string());
        lock_txn.rollback().await.unwrap();
        let rounds_after_release = started.load(Ordering::SeqCst);
        wait_for_count(
            &completed,
            rounds_after_release + 1,
            Duration::from_secs(15),
        )
        .await;
        let row = mega_view_filter::Entity::find_by_id(1)
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.projected_seq, 3);
        assert_eq!(row.ready_seq, Some(2));
        assert!(row.warming_since.is_none());
        token.cancel();
        worker.await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let mut config = views_config(temp.path().join("unbounded"));
        config.views.worker_interval_secs = 60;
        config.views.max_append_walk = 1;
        let storage = test_storage_with_config(temp.path(), config).await;
        let view_storage = storage.view_storage();
        let mut history = seed_linear_root_history(view_storage.get_connection(), 1).await;
        view_storage
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        insert_filter(&storage, 1, 1, Some(1), false).await;
        let mut parent = history.last().unwrap().clone();
        for number in 1..=3 {
            let next = seed_single_parent_root_commit(
                view_storage.get_connection(),
                HashKind::Sha1,
                &parent,
                &format!("hp14 unbounded {number}"),
            )
            .await;
            assert!(cas_fixture_main(view_storage.get_connection(), &parent, &next).await);
            history.push(next.clone());
            parent = next;
        }
        let tail_before_worker = view_storage
            .get_connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT seq, commit_id FROM mega_view_root_chain ORDER BY seq DESC LIMIT 1"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let tail_seq_before_worker: i64 = tail_before_worker.try_get("", "seq").unwrap();
        let tail_commit_before_worker: String =
            tail_before_worker.try_get("", "commit_id").unwrap();
        assert_eq!(tail_seq_before_worker, 1);
        assert_eq!(
            tail_commit_before_worker,
            history.first().unwrap().commit.id.to_string()
        );
        assert_ne!(
            tail_commit_before_worker,
            history.last().unwrap().commit.id.to_string()
        );
        let token = CancellationToken::new();
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let started_round = started.clone();
        let completed_round = completed.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let started = started_round.clone();
            let completed = completed_round.clone();
            Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);
                let result = production(storage).await;
                completed.fetch_add(1, Ordering::SeqCst);
                result
            })
        });
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&completed, 1, Duration::from_secs(10)).await;
        wait_until_projected(&storage, 1, 4).await;
        let row = mega_view_filter::Entity::find_by_id(1)
            .one(view_storage.get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.ready_seq, Some(1));
        assert!(row.warming_since.is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        let tail = view_storage
            .get_connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT commit_id FROM mega_view_root_chain ORDER BY seq DESC LIMIT 1".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let tail_commit: String = tail.try_get("", "commit_id").unwrap();
        assert_eq!(tail_commit, history.last().unwrap().commit.id.to_string());
        token.cancel();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn round_errors_do_not_stop_worker() {
        let temp = tempfile::tempdir().unwrap();
        let storage = storage_with_root_chain(&temp, 1).await;
        insert_filter(&storage, 1, 0, Some(1), false).await;
        insert_filter(&storage, 2, 0, Some(1), false).await;
        let selected = Arc::new(Mutex::new(Vec::new()));
        let selected_for_round = selected.clone();
        let service = storage.view_projection_service();
        let per_candidate: PerCandidate = Arc::new(move |filter_id| {
            let selected = selected_for_round.clone();
            let service = service.clone();
            Box::pin(async move {
                selected.lock().unwrap().push(filter_id);
                if filter_id == 1 {
                    Err(MegaError::Other("injected candidate error".to_owned()))
                } else {
                    service.catch_up(filter_id).await
                }
            })
        });
        compensation_round(storage.clone(), per_candidate)
            .await
            .unwrap();
        assert_eq!(*selected.lock().unwrap(), vec![1, 2]);
        assert_eq!(
            mega_view_filter::Entity::find_by_id(1)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap()
                .projected_seq,
            0
        );
        assert_eq!(
            mega_view_filter::Entity::find_by_id(2)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap()
                .projected_seq,
            1
        );

        let token = CancellationToken::new();
        let errors = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let production = production_round();
        let error_count = errors.clone();
        let success_count = successes.clone();
        let round: ViewWorkerRound = Arc::new(move |storage| {
            let production = production.clone();
            let errors = error_count.clone();
            let successes = success_count.clone();
            Box::pin(async move {
                let result = production(storage).await;
                if result.is_ok() {
                    successes.fetch_add(1, Ordering::SeqCst);
                } else {
                    errors.fetch_add(1, Ordering::SeqCst);
                }
                result
            })
        });
        let mut candidate = storage.config().as_ref().clone();
        candidate.views.worker_interval_secs = 1;
        storage.config_handle().reload(candidate).unwrap();
        let projected_before_fault = mega_view_filter::Entity::find_by_id(1)
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap()
            .projected_seq;
        storage
            .view_storage()
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_filter RENAME COLUMN warming_since TO hp14_gone",
            )
            .await
            .unwrap();
        let worker = spawn_view_worker_with_round(storage.clone(), token.clone(), round)
            .unwrap()
            .unwrap();
        wait_for_count(&errors, 1, Duration::from_secs(17)).await;
        assert_eq!(successes.load(Ordering::SeqCst), 0);
        let row = storage
            .view_storage()
            .get_connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT projected_seq FROM mega_view_filter WHERE id = 1".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let projected_during_fault: i64 = row.try_get("", "projected_seq").unwrap();
        assert_eq!(projected_during_fault, projected_before_fault);
        storage
            .view_storage()
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_filter RENAME COLUMN hp14_gone TO warming_since",
            )
            .await
            .unwrap();
        wait_until_ready(&storage, 1, 1).await;
        token.cancel();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn view_runtime_wiring() {
        let temp = tempfile::tempdir().unwrap();
        let storage = storage_with_root_chain(&temp, 2).await;
        let clone = storage.clone();
        let slot = storage.view_runtime.service_slot();
        assert!(slot.get().is_none());
        insert_filter(&storage, 1, 0, Some(1), false).await;
        mega_view_root_chain::Entity::delete_by_id(1)
            .exec(storage.view_storage().get_connection())
            .await
            .unwrap();
        production_round()(storage.clone()).await.unwrap();
        assert!(slot.get().is_some());
        assert_eq!(
            clone
                .view_metrics()
                .counters()
                .view_batch_premise_failures_total,
            1
        );
        let service = storage.view_projection_service();
        assert_eq!(
            service.catch_up(1).await.unwrap(),
            CatchUpOutcome::BatchPremiseFailed
        );
        assert_eq!(
            storage
                .view_metrics()
                .counters()
                .view_batch_premise_failures_total,
            2
        );

        let separate = Storage::mock();
        let separate_slot = separate.view_runtime.service_slot();
        assert!(separate_slot.get().is_none());
        assert!(Arc::ptr_eq(&slot, &clone.view_runtime.service_slot()));
        assert!(!Arc::ptr_eq(&slot, &separate_slot));

        let (shared_registered_sender, shared_registered) = tokio::sync::oneshot::channel();
        let shared_signal = clone.view_signal();
        let shared_waiter = tokio::spawn(wait_after_registered(
            shared_signal,
            shared_registered_sender,
        ));
        let (separate_registered_sender, separate_registered) = tokio::sync::oneshot::channel();
        let mut separate_waiter = tokio::spawn(wait_after_registered(
            separate.view_signal(),
            separate_registered_sender,
        ));
        shared_registered.await.unwrap();
        separate_registered.await.unwrap();
        storage.view_signal().notify_worker();
        tokio::time::timeout(Duration::from_secs(1), shared_waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut separate_waiter)
                .await
                .is_err()
        );
        separate_waiter.abort();
        assert!(separate_waiter.await.is_err());

        let weak = Arc::downgrade(&slot);
        drop(service);
        drop(slot);
        drop(clone);
        drop(storage);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            separate
                .view_metrics()
                .counters()
                .view_batch_premise_failures_total,
            0
        );
    }

    async fn wait_after_registered(
        signal: ViewSignal,
        registered_sender: tokio::sync::oneshot::Sender<()>,
    ) {
        let notified = signal.notified();
        tokio::pin!(notified);
        let mut registered_sender = Some(registered_sender);
        poll_fn(|context| match notified.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending => {
                if let Some(sender) = registered_sender.take() {
                    sender.send(()).unwrap();
                }
                Poll::Pending
            }
        })
        .await;
    }
}
