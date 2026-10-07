#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use git_internal::{hash::HashKind, internal::object::tree::Tree};
use sea_orm::{ConnectionTrait, TransactionTrait};
use tracing::error;

#[cfg(test)]
use crate::ceres::view::filter::print;
use crate::{
    ceres::view::{
        filter::{Filter, recheck_definition},
        project::{
            PreviousProjection, ProjectError, ProjectFailure, ProjectionInput, ProjectionRequest,
            Segment, project_commit, project_with_output,
        },
        tree::{
            FILTER_TREE_MEMO_CAPACITY_BYTES, FilterMemo, FilterOutput, FilterTreeError, filter_tree,
        },
        tree_source::{MissingObject, MissingObjectReason, TreeSource, empty_tree_id},
    },
    common::errors::MegaError,
    config::Config,
    jupiter::{
        service::view_metrics::{ViewMetrics, ViewMetricsSnapshot},
        storage::{
            Storage,
            base_storage::StorageConnector,
            view_projection_storage::ProjectionFilter,
            view_storage::{ViewLock, ViewLockMode, acquire_view_lock},
            view_tree_source::ViewTreeSource,
        },
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CatchUpOutcome {
    Advanced,
    Ready,
    MainNotCovered,
    NotRun,
    BatchPremiseFailed,
    Stopped(ViewProjectionStop),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ViewProjectionStop {
    pub(crate) filter_id: String,
    pub(crate) seq: i64,
    pub(crate) commit_id: String,
    pub(crate) reason: ViewProjectionStopReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ViewProjectionStopReason {
    PremiseCheckFailed,
    RowAbsent { tree_id: String },
    Unparsable { tree_id: String },
    CommitRowMissing,
}

#[derive(Clone)]
pub(crate) struct ViewProjectionService {
    storage: Storage,
    metrics: ViewMetrics,
    memo: Arc<Mutex<FilterMemo>>,
    #[cfg(test)]
    test_hooks: Arc<ViewProjectionTestHooks>,
}

impl ViewProjectionService {
    pub(crate) fn new(storage: Storage, metrics: ViewMetrics) -> Self {
        Self {
            storage,
            metrics,
            memo: Arc::new(Mutex::new(FilterMemo::with_capacity(
                FILTER_TREE_MEMO_CAPACITY_BYTES,
            ))),
            #[cfg(test)]
            test_hooks: Arc::new(ViewProjectionTestHooks::default()),
        }
    }

    pub(crate) async fn catch_up(&self, filter_pk: i64) -> Result<CatchUpOutcome, MegaError> {
        loop {
            let snapshot = self.storage.config();
            let batch_size = usize::try_from(snapshot.views.batch_size)
                .map_err(|_| MegaError::Other("views.batch_size does not fit usize".to_owned()))?;
            let outcome = self
                .catch_up_one_batch(filter_pk, &snapshot, batch_size)
                .await?;
            if outcome != CatchUpOutcome::Advanced {
                return Ok(outcome);
            }
            #[cfg(test)]
            {
                self.test_hooks.record_advanced_batch(batch_size);
                self.test_hooks.run_between_batches();
            }
        }
    }

    pub(crate) async fn catch_up_one_batch(
        &self,
        filter_pk: i64,
        snapshot: &Config,
        batch_size: usize,
    ) -> Result<CatchUpOutcome, MegaError> {
        let kind = snapshot.monorepo.object_hash_kind()?;
        let view_storage = self.storage.view_storage();
        let mono = self.storage.mono_storage();
        let txn = match view_storage.get_connection().begin().await {
            Ok(txn) => txn,
            Err(error) => {
                let error = MegaError::from(error);
                log_batch_error(filter_pk, None, &error);
                return Err(error);
            }
        };
        let error_context = Mutex::new(None);

        let filter_locked =
            match acquire_view_lock(&txn, ViewLock::Filter(filter_pk), ViewLockMode::Try).await {
                Ok(locked) => locked,
                Err(error) => {
                    let _ = txn.rollback().await;
                    log_batch_error(filter_pk, None, &error);
                    return Err(error);
                }
            };
        if !filter_locked {
            txn.rollback().await?;
            return Ok(CatchUpOutcome::NotRun);
        }
        let gc_locked =
            match acquire_view_lock(&txn, ViewLock::ObjectGcShared, ViewLockMode::Try).await {
                Ok(locked) => locked,
                Err(error) => {
                    let _ = txn.rollback().await;
                    log_batch_error(filter_pk, None, &error);
                    return Err(error);
                }
            };
        if !gc_locked {
            txn.rollback().await?;
            return Ok(CatchUpOutcome::NotRun);
        }

        let result = self
            .catch_up_in_transaction(CatchUpBatch {
                view_storage: &view_storage,
                mono: &mono,
                txn: &txn,
                filter_pk,
                kind,
                batch_size,
                error_context: &error_context,
            })
            .await;
        match result {
            Ok(outcome) => match txn.commit().await {
                Ok(()) => {
                    if let CatchUpOutcome::Stopped(stop) = &outcome {
                        self.record_projection_stop(stop);
                    }
                    Ok(outcome)
                }
                Err(error) => {
                    let error = MegaError::from(error);
                    let context = error_context
                        .lock()
                        .ok()
                        .and_then(|context| context.clone());
                    log_batch_error(filter_pk, context.as_ref(), &error);
                    Err(error)
                }
            },
            Err(error) => {
                let context = error_context
                    .lock()
                    .ok()
                    .and_then(|context| context.clone());
                let _ = txn.rollback().await;
                log_batch_error(filter_pk, context.as_ref(), &error);
                Err(error)
            }
        }
    }

    async fn catch_up_in_transaction(
        &self,
        batch: CatchUpBatch<'_>,
    ) -> Result<CatchUpOutcome, MegaError> {
        let CatchUpBatch {
            view_storage,
            mono,
            txn,
            filter_pk,
            kind,
            batch_size,
            error_context,
        } = batch;
        let filter = view_storage
            .projection_filter(txn, filter_pk)
            .await?
            .ok_or_else(|| MegaError::Other(format!("view filter {filter_pk} does not exist")))?;
        *match error_context.lock() {
            Ok(context) => context,
            Err(poisoned) => poisoned.into_inner(),
        } = Some(filter.clone());
        if filter.ready_seq.is_none() && filter.warming_since.is_none() {
            return Ok(CatchUpOutcome::NotRun);
        }
        let canonical = recheck_definition(&filter.canonical_spec, &filter.filter_id)
            .map_err(|error| MegaError::Other(error.to_string()))?;
        let tip = view_storage.projection_tip(txn).await?;
        if filter.projected_seq >= tip {
            #[cfg(test)]
            self.test_hooks
                .terminal_checks
                .fetch_add(1, Ordering::Relaxed);
            return Ok(
                if view_storage
                    .mark_ready_if_covered(txn, filter_pk, tip)
                    .await?
                {
                    CatchUpOutcome::Ready
                } else {
                    CatchUpOutcome::MainNotCovered
                },
            );
        }

        let Some((first, last)) = batch_bounds(filter.projected_seq, tip, batch_size) else {
            return self.batch_premise_failed(&filter, tip, batch_size);
        };
        let rows = view_storage.projection_rows(txn, first, last).await?;
        if !contiguous_rows(&rows, first, last) {
            return self.batch_premise_failed(&filter, tip, batch_size);
        }

        let commit_ids = rows
            .iter()
            .map(|row| row.commit_id.clone())
            .collect::<Vec<_>>();
        let commits = mono
            .get_commits_by_hashes_fallible(txn, &commit_ids)
            .await?;
        let commits = commits
            .into_iter()
            .map(|commit| (commit.commit_id.clone(), commit))
            .collect::<HashMap<_, _>>();
        let empty_tree = empty_tree_id(kind)
            .map_err(|error| MegaError::Other(format!("failed to calculate empty tree: {error}")))?
            .to_string();
        let mut previous = view_storage
            .previous_projection(txn, filter_pk, filter.projected_seq, &empty_tree)
            .await?;
        let mut source = ViewTreeSource::new(mono, txn, kind)?;
        self.prefetch_trees(
            &mut source,
            &rows
                .iter()
                .map(|row| row.tree_id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
        let outputs = self
            .prefetch_filter_outputs(
                kind,
                &mut source,
                &canonical.filter,
                &rows,
                filter.projected_seq,
            )
            .await?;
        #[cfg(test)]
        self.test_hooks.clear_memo_if_requested(&self.memo);

        let mut segments = Vec::new();
        let mut written_outputs = BTreeMap::new();
        let mut stop = None;
        for (index, row) in rows.iter().enumerate() {
            if row.seq <= filter.projected_seq {
                continue;
            }
            let parent_tree = if index == 0 {
                empty_tree.as_str()
            } else {
                rows[index - 1].tree_id.as_str()
            };
            let input = ProjectionInput {
                seq: row.seq,
                commit_id: &row.commit_id,
                tree_id: &row.tree_id,
                row: commits.get(&row.commit_id),
            };
            let segment = self
                .project_with_prefetch(
                    &mut source,
                    ProjectionWork {
                        kind,
                        filter: &canonical.filter,
                        input,
                        parent_tree,
                        previous: &previous,
                        prefetched_output: outputs.get(&row.seq),
                    },
                )
                .await?;
            let segment = match segment {
                Ok(segment) => segment,
                Err(ProjectFailure::Internal) => {
                    return Err(project_error(ProjectFailure::Internal));
                }
                Err(ProjectFailure::Data(error)) => match stop_dispatch(&error) {
                    StopDispatch::Stop(reason) => {
                        stop = Some(ViewProjectionStop {
                            filter_id: filter.filter_id.clone(),
                            seq: row.seq,
                            commit_id: row.commit_id.clone(),
                            reason,
                        });
                        break;
                    }
                    StopDispatch::Internal => {
                        return Err(project_error(ProjectFailure::Data(error)));
                    }
                },
            };
            if let Some(segment) = segment {
                if segment.view_commit.is_some()
                    && let Some(output) = outputs.get(&row.seq)
                {
                    written_outputs.insert(row.seq, output.clone());
                }
                previous = PreviousProjection {
                    view_commit: segment
                        .view_commit
                        .as_ref()
                        .map(|commit| commit.id.to_string()),
                    view_tree: segment.view_tree.clone(),
                };
                segments.push(segment);
            }
        }

        let projected_seq = stop.as_ref().map_or(last, |stop| stop.seq - 1);
        view_storage
            .write_projection_batch(txn, filter_pk, &segments, &written_outputs, projected_seq)
            .await?;
        let outcome = if let Some(stop) = stop {
            CatchUpOutcome::Stopped(stop)
        } else if last == tip {
            #[cfg(test)]
            self.test_hooks
                .terminal_checks
                .fetch_add(1, Ordering::Relaxed);
            if view_storage
                .mark_ready_if_covered(txn, filter_pk, tip)
                .await?
            {
                CatchUpOutcome::Ready
            } else {
                CatchUpOutcome::MainNotCovered
            }
        } else {
            CatchUpOutcome::Advanced
        };
        #[cfg(test)]
        self.test_hooks.record_advanced_projected_seq(projected_seq);
        Ok(outcome)
    }

    async fn prefetch_filter_outputs<C: ConnectionTrait>(
        &self,
        kind: HashKind,
        source: &mut ViewTreeSource<'_, C>,
        filter: &Filter,
        rows: &[crate::callisto::mega_view_root_chain::Model],
        projected_seq: i64,
    ) -> Result<BTreeMap<i64, FilterOutput>, MegaError> {
        let mut outputs = BTreeMap::new();
        let mut prefetched = BTreeSet::new();
        loop {
            let mut needed = BTreeSet::new();
            for row in rows {
                if row.seq <= projected_seq || outputs.contains_key(&row.seq) {
                    continue;
                }
                let tracking = self.tracking_tree_source(source);
                let result = self.filter_tree_with_memo(kind, &tracking, filter, &row.tree_id);
                needed.extend(tracking.take_unprefetched());
                match result {
                    Ok(output) => {
                        outputs.insert(row.seq, output);
                    }
                    Err(FilterTreeError::Missing(_)) | Err(FilterTreeError::Invariant) => {}
                }
            }
            if needed.is_empty() {
                return Ok(outputs);
            }
            if needed.iter().any(|tree_id| prefetched.contains(tree_id)) {
                return Err(MegaError::Other(
                    "view tree source returned unprefetched during trial prefetch".to_owned(),
                ));
            }
            self.prefetch_trees(source, &needed.iter().cloned().collect::<Vec<_>>())
                .await?;
            prefetched.extend(needed);
        }
    }

    async fn project_with_prefetch<C: ConnectionTrait>(
        &self,
        source: &mut ViewTreeSource<'_, C>,
        work: ProjectionWork<'_, '_>,
    ) -> Result<Result<Option<Segment>, ProjectFailure>, MegaError> {
        let mut prefetched = BTreeSet::new();
        loop {
            let tracking = self.tracking_tree_source(source);
            let result = self.with_memo(|memo| match work.prefetched_output {
                Some(output) => project_with_output(
                    work.kind,
                    &tracking,
                    memo,
                    work.filter,
                    ProjectionRequest {
                        input: work.input.clone(),
                        parent_tree: work.parent_tree,
                        prev: work.previous,
                    },
                    output,
                ),
                None => project_commit(
                    work.kind,
                    &tracking,
                    memo,
                    work.filter,
                    work.input.clone(),
                    work.parent_tree,
                    work.previous,
                ),
            });
            let needed = tracking.take_unprefetched();
            if !needed.is_empty() {
                if needed.iter().any(|tree_id| prefetched.contains(tree_id)) {
                    return Err(MegaError::Other(
                        "view tree source returned unprefetched after prefetch".to_owned(),
                    ));
                }
                #[cfg(test)]
                self.test_hooks.refetches.fetch_add(1, Ordering::Relaxed);
                self.prefetch_trees(source, &needed.iter().cloned().collect::<Vec<_>>())
                    .await?;
                prefetched.extend(needed);
                continue;
            }
            return Ok(result);
        }
    }

    async fn prefetch_trees<C: ConnectionTrait>(
        &self,
        source: &mut ViewTreeSource<'_, C>,
        ids: &[String],
    ) -> Result<(), MegaError> {
        #[cfg(test)]
        self.test_hooks.record_prefetch(ids);
        source.prefetch(ids).await
    }

    fn filter_tree_with_memo<S: TreeSource + ?Sized>(
        &self,
        kind: HashKind,
        source: &S,
        filter: &Filter,
        tree_id: &str,
    ) -> Result<FilterOutput, FilterTreeError> {
        self.with_memo(|memo| {
            #[cfg(test)]
            if memo.contains(&print(filter), tree_id) {
                self.test_hooks.memo_hits.fetch_add(1, Ordering::Relaxed);
            }
            filter_tree(kind, source, memo, filter, tree_id)
        })
    }

    #[cfg(test)]
    fn tracking_tree_source<'a, C: ConnectionTrait>(
        &'a self,
        source: &'a ViewTreeSource<'a, C>,
    ) -> TrackingTreeSource<'a, C> {
        TrackingTreeSource::new(source, self.test_hooks.clone())
    }

    #[cfg(not(test))]
    fn tracking_tree_source<'a, C: ConnectionTrait>(
        &'a self,
        source: &'a ViewTreeSource<'a, C>,
    ) -> TrackingTreeSource<'a, C> {
        TrackingTreeSource::new(source)
    }

    fn batch_premise_failed(
        &self,
        filter: &ProjectionFilter,
        tip: i64,
        batch_size: usize,
    ) -> Result<CatchUpOutcome, MegaError> {
        self.metrics.increment_batch_premise_failures();
        error!(
            metric = "view_batch_premise_failures_total",
            filter_id = %filter.filter_id,
            s0 = filter.projected_seq,
            tip,
            batch_size,
            "view projection batch premise failed"
        );
        Ok(CatchUpOutcome::BatchPremiseFailed)
    }

    pub(crate) async fn metrics_snapshot(&self) -> Result<ViewMetricsSnapshot, MegaError> {
        Ok(ViewMetricsSnapshot {
            counters: self.metrics.counters(),
            view_cold_start_slots_in_use: self
                .storage
                .view_storage()
                .warming_filter_count()
                .await?,
        })
    }

    fn record_projection_stop(&self, stop: &ViewProjectionStop) {
        self.metrics.increment_projection_stops();
        match &stop.reason {
            ViewProjectionStopReason::PremiseCheckFailed => error!(
                metric = %"view_projection_stops_total",
                filter_id = %stop.filter_id,
                s = stop.seq,
                commit_id = %stop.commit_id,
                reason = %stop.reason.as_str(),
                "view projection stopped"
            ),
            ViewProjectionStopReason::RowAbsent { tree_id }
            | ViewProjectionStopReason::Unparsable { tree_id } => error!(
                metric = %"view_projection_stops_total",
                filter_id = %stop.filter_id,
                s = stop.seq,
                commit_id = %stop.commit_id,
                reason = %stop.reason.as_str(),
                tree_id = %tree_id,
                "view projection stopped"
            ),
            ViewProjectionStopReason::CommitRowMissing => error!(
                metric = %"view_projection_stops_total",
                filter_id = %stop.filter_id,
                s = stop.seq,
                commit_id = %stop.commit_id,
                reason = %stop.reason.as_str(),
                "view projection stopped"
            ),
        }
    }

    fn with_memo<T>(&self, f: impl FnOnce(&mut FilterMemo) -> T) -> T {
        let mut memo = match self.memo.lock() {
            Ok(memo) => memo,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut memo)
    }
}

#[cfg(test)]
#[derive(Default)]
struct ViewProjectionTestHooks {
    memo_hits: AtomicU64,
    refetches: AtomicU64,
    prefetches: AtomicU64,
    prefetched_ids: Mutex<BTreeSet<String>>,
    advanced_batch_sizes: Mutex<Vec<usize>>,
    advanced_projected_seqs: Mutex<Vec<i64>>,
    terminal_checks: AtomicU64,
    clear_memo_before_project: AtomicBool,
    omit_unprefetched_record_for: Mutex<Option<String>>,
    always_unprefetched_for: Mutex<Option<String>>,
    between_batches: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[cfg(test)]
impl ViewProjectionTestHooks {
    fn record_advanced_batch(&self, batch_size: usize) {
        let mut batches = match self.advanced_batch_sizes.lock() {
            Ok(batches) => batches,
            Err(poisoned) => poisoned.into_inner(),
        };
        batches.push(batch_size);
    }

    fn record_advanced_projected_seq(&self, projected_seq: i64) {
        let mut projected_seqs = match self.advanced_projected_seqs.lock() {
            Ok(projected_seqs) => projected_seqs,
            Err(poisoned) => poisoned.into_inner(),
        };
        projected_seqs.push(projected_seq);
    }

    fn record_prefetch(&self, ids: &[String]) {
        self.prefetches.fetch_add(1, Ordering::Relaxed);
        let mut prefetched = match self.prefetched_ids.lock() {
            Ok(prefetched) => prefetched,
            Err(poisoned) => poisoned.into_inner(),
        };
        prefetched.extend(ids.iter().cloned());
    }

    fn clear_memo_if_requested(&self, memo: &Mutex<FilterMemo>) {
        if !self.clear_memo_before_project.load(Ordering::Relaxed) {
            return;
        }
        let mut memo = match memo.lock() {
            Ok(memo) => memo,
            Err(poisoned) => poisoned.into_inner(),
        };
        *memo = FilterMemo::with_capacity(FILTER_TREE_MEMO_CAPACITY_BYTES);
    }

    fn run_between_batches(&self) {
        let hook = match self.between_batches.lock() {
            Ok(hook) => hook.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

struct CatchUpBatch<'a> {
    view_storage: &'a crate::jupiter::storage::view_storage::ViewStorage,
    mono: &'a crate::jupiter::storage::mono_storage::MonoStorage,
    txn: &'a sea_orm::DatabaseTransaction,
    filter_pk: i64,
    kind: HashKind,
    batch_size: usize,
    error_context: &'a Mutex<Option<ProjectionFilter>>,
}

struct ProjectionWork<'input, 'state> {
    kind: HashKind,
    filter: &'state Filter,
    input: ProjectionInput<'input>,
    parent_tree: &'state str,
    previous: &'state PreviousProjection,
    prefetched_output: Option<&'state FilterOutput>,
}

struct TrackingTreeSource<'a, C: ConnectionTrait> {
    source: &'a ViewTreeSource<'a, C>,
    unprefetched: RefCell<BTreeSet<String>>,
    #[cfg(test)]
    hooks: Arc<ViewProjectionTestHooks>,
}

impl<'a, C: ConnectionTrait> TrackingTreeSource<'a, C> {
    #[cfg(test)]
    fn new(source: &'a ViewTreeSource<'a, C>, hooks: Arc<ViewProjectionTestHooks>) -> Self {
        Self {
            source,
            unprefetched: RefCell::new(BTreeSet::new()),
            hooks,
        }
    }

    #[cfg(not(test))]
    fn new(source: &'a ViewTreeSource<'a, C>) -> Self {
        Self {
            source,
            unprefetched: RefCell::new(BTreeSet::new()),
        }
    }

    fn take_unprefetched(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.unprefetched.borrow_mut())
    }
}

impl<C: ConnectionTrait> TreeSource for TrackingTreeSource<'_, C> {
    fn read_tree(&self, tree_id: &str) -> Result<Tree, MissingObject> {
        #[cfg(test)]
        if self
            .hooks
            .always_unprefetched_for
            .lock()
            .ok()
            .and_then(|id| id.clone())
            .as_deref()
            == Some(tree_id)
        {
            self.unprefetched.borrow_mut().insert(tree_id.to_owned());
            return Err(MissingObject {
                tree_id: tree_id.to_owned(),
                reason: MissingObjectReason::Unprefetched,
            });
        }
        let result = self.source.read_tree(tree_id);
        #[cfg(test)]
        let omit_record = self
            .hooks
            .omit_unprefetched_record_for
            .lock()
            .ok()
            .and_then(|id| id.clone())
            .as_deref()
            == Some(tree_id);
        #[cfg(not(test))]
        let omit_record = false;
        if !omit_record
            && matches!(
                result.as_ref().err().map(|missing| &missing.reason),
                Some(MissingObjectReason::Unprefetched)
            )
        {
            self.unprefetched.borrow_mut().insert(tree_id.to_owned());
        }
        result
    }
}

fn log_batch_error(filter_pk: i64, context: Option<&ProjectionFilter>, error: &MegaError) {
    if let Some(context) = context {
        error!(
            filter_pk,
            filter_id = %context.filter_id,
            s0 = context.projected_seq,
            error = %error,
            "view projection batch failed"
        );
    } else {
        error!(filter_pk, error = %error, "view projection batch failed");
    }
}

fn batch_bounds(s0: i64, tip: i64, batch_size: usize) -> Option<(i64, i64)> {
    let batch_size = i64::try_from(batch_size).ok()?;
    if batch_size <= 0 {
        return None;
    }
    let first = s0.max(1);
    let last = s0.checked_add(batch_size)?.min(tip);
    (first <= last).then_some((first, last))
}

fn contiguous_rows(
    rows: &[crate::callisto::mega_view_root_chain::Model],
    first: i64,
    last: i64,
) -> bool {
    rows.len() == usize::try_from(last - first + 1).unwrap_or_default()
        && rows
            .iter()
            .enumerate()
            .all(|(offset, row)| row.seq == first + offset as i64)
}

fn project_error(error: ProjectFailure) -> MegaError {
    MegaError::Other(match error {
        ProjectFailure::Data(ProjectError::MissingObject(missing)) => {
            format!("view projection data missing: {missing:?}")
        }
        other => other.to_string(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StopDispatch {
    Stop(ViewProjectionStopReason),
    Internal,
}

fn stop_dispatch(error: &ProjectError) -> StopDispatch {
    match error {
        ProjectError::Premise(_) => {
            StopDispatch::Stop(ViewProjectionStopReason::PremiseCheckFailed)
        }
        ProjectError::MissingObject(MissingObject {
            tree_id,
            reason: MissingObjectReason::Absent,
        }) => StopDispatch::Stop(ViewProjectionStopReason::RowAbsent {
            tree_id: tree_id.clone(),
        }),
        ProjectError::MissingObject(MissingObject {
            tree_id,
            reason: MissingObjectReason::Malformed,
        }) => StopDispatch::Stop(ViewProjectionStopReason::Unparsable {
            tree_id: tree_id.clone(),
        }),
        ProjectError::MissingObject(MissingObject {
            reason: MissingObjectReason::Unprefetched,
            ..
        }) => StopDispatch::Internal,
        ProjectError::MissingCommit { .. } => {
            StopDispatch::Stop(ViewProjectionStopReason::CommitRowMissing)
        }
    }
}

impl ViewProjectionStopReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::PremiseCheckFailed => "premise_check_failed",
            Self::RowAbsent { .. } => "row_absent",
            Self::Unparsable { .. } => "unparsable",
            Self::CommitRowMissing => "commit_row_missing",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap, HashSet},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use chrono::Utc;
    use git_internal::{
        hash::{HashKind, ObjectHash},
        internal::{
            metadata::EntryMeta,
            object::{
                ObjectTrait,
                commit::Commit,
                signature::Signature,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
        },
    };
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait,
        IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, Statement, TransactionTrait,
    };

    use super::{
        CatchUpOutcome, StopDispatch, ViewProjectionService, ViewProjectionStop,
        ViewProjectionStopReason, stop_dispatch,
    };
    use crate::{
        callisto::{
            mega_commit, mega_refs, mega_tree, mega_view_commit_map, mega_view_filter,
            mega_view_object, mega_view_object_ref,
        },
        ceres::view::{
            commit::RewriteError,
            filter::{RecheckFailure, parse_for_registration, recheck_definition},
            project::{PreviousProjection, ProjectError, ProjectionInput, project_commit},
            tree::{FILTER_TREE_MEMO_CAPACITY_BYTES, FilterMemo, filter_tree},
            tree_source::{InMemoryTreeSource, MissingObject, MissingObjectReason, empty_tree_id},
        },
        common::utils::{MEGA_BRANCH_NAME, generate_id},
        jupiter::{
            service::view_metrics::ViewMetrics,
            storage::{
                base_storage::StorageConnector,
                init::database_connection,
                object_storage::mock_object_storage,
                view_storage::{ViewLock, ViewLockMode, acquire_view_lock},
                view_test_fixtures::{RootCommitFixture, RootTreeFixture, root_tree_from_paths},
                view_tree_source::ViewTreeSource,
            },
            tests::{TestSchemaGuard, test_db_config, test_storage_with_config},
            utils::converter::IntoMegaModel,
        },
    };

    async fn seed_fixed_linear_history(
        storage: &crate::jupiter::storage::Storage,
        trees: Vec<RootTreeFixture>,
    ) -> Vec<RootCommitFixture> {
        let kind = HashKind::Sha1;
        let mono = storage.mono_storage();
        let mut parent = None;
        let mut fixtures = Vec::with_capacity(trees.len());
        for (index, tree) in trees.into_iter().enumerate() {
            let author = Signature::from_data(
                b"author HP-11 fixture <hp11@example.test> 1700000000 +0000".to_vec(),
            )
            .unwrap();
            let committer = Signature::from_data(
                b"committer HP-11 fixture <hp11@example.test> 1700000001 +0000".to_vec(),
            )
            .unwrap();
            let parents = parent.into_iter().collect::<Vec<ObjectHash>>();
            let commit = Commit::new_with_kind(
                kind,
                author,
                committer,
                tree.root.id,
                parents,
                &format!("HP-11 fixed root commit {}", index + 1),
            )
            .unwrap();
            mono.save_mega_trees(tree.trees.clone(), commit.id, None)
                .await
                .unwrap();
            mono.save_mega_commits(vec![commit.clone()], None)
                .await
                .unwrap();
            parent = Some(commit.id);
            fixtures.push(RootCommitFixture {
                commit,
                trees: tree.trees,
                blobs: tree.blobs,
            });
        }
        let tip = fixtures.last().unwrap();
        mega_refs::ActiveModel {
            id: Set(generate_id()),
            path: Set("/".to_owned()),
            ref_name: Set(MEGA_BRANCH_NAME.to_owned()),
            ref_commit_hash: Set(tip.commit.id.to_string()),
            ref_tree_hash: Set(tip.commit.tree_id.to_string()),
            created_at: Set(Utc::now().naive_utc()),
            updated_at: Set(Utc::now().naive_utc()),
            is_cl: Set(false),
        }
        .insert(storage.view_storage().get_connection())
        .await
        .unwrap();
        fixtures
    }

    const HP12_SENTINEL_AUTHOR: &str =
        "author hp12-sentinel-author <author@hp12.invalid> 01700000000 +0000";
    const HP12_SENTINEL_COMMITTER: &str =
        "committer hp12-sentinel-author <author@hp12.invalid> 1700000001 +0000";
    const HP12_SENTINEL_MESSAGE: &str = "hp12-sentinel-message";

    async fn seed_hp12_linear_history(
        storage: &crate::jupiter::storage::Storage,
        trees: Vec<RootTreeFixture>,
        raw_premise_at_seven: bool,
    ) -> Vec<RootCommitFixture> {
        let kind = HashKind::Sha1;
        let mono = storage.mono_storage();
        let mut parent = None;
        let mut fixtures = Vec::with_capacity(trees.len());
        for (index, tree) in trees.into_iter().enumerate() {
            let parents = parent.into_iter().collect::<Vec<ObjectHash>>();
            let raw_premise = raw_premise_at_seven && index == 6;
            let commit = if raw_premise {
                let parent = parents.first().unwrap();
                let bytes = format!(
                    "tree {}\nparent {parent}\n{HP12_SENTINEL_AUTHOR}\n{HP12_SENTINEL_COMMITTER}\n\n{HP12_SENTINEL_MESSAGE} {}",
                    tree.root.id,
                    index + 1,
                )
                .into_bytes();
                let id = ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, &bytes)
                    .unwrap();
                Commit::from_bytes(&bytes, id).unwrap()
            } else {
                let author = Signature::from_data(
                    b"author hp12-sentinel-author <author@hp12.invalid> 1700000000 +0000".to_vec(),
                )
                .unwrap();
                let committer = Signature::from_data(
                    b"committer hp12-sentinel-author <author@hp12.invalid> 1700000001 +0000"
                        .to_vec(),
                )
                .unwrap();
                Commit::new_with_kind(
                    kind,
                    author,
                    committer,
                    tree.root.id,
                    parents,
                    &format!("{HP12_SENTINEL_MESSAGE} {}", index + 1),
                )
                .unwrap()
            };
            mono.save_mega_trees(tree.trees.clone(), commit.id, None)
                .await
                .unwrap();
            if raw_premise {
                let model: mega_commit::Model =
                    commit.clone().into_mega_model(EntryMeta::default());
                model
                    .into_active_model()
                    .insert(storage.view_storage().get_connection())
                    .await
                    .unwrap();
            } else {
                mono.save_mega_commits(vec![commit.clone()], None)
                    .await
                    .unwrap();
            }
            parent = Some(commit.id);
            fixtures.push(RootCommitFixture {
                commit,
                trees: tree.trees,
                blobs: tree.blobs,
            });
        }
        let tip = fixtures.last().unwrap();
        mega_refs::ActiveModel {
            id: Set(generate_id()),
            path: Set("/".to_owned()),
            ref_name: Set(MEGA_BRANCH_NAME.to_owned()),
            ref_commit_hash: Set(tip.commit.id.to_string()),
            ref_tree_hash: Set(tip.commit.tree_id.to_string()),
            created_at: Set(Utc::now().naive_utc()),
            updated_at: Set(Utc::now().naive_utc()),
            is_cl: Set(false),
        }
        .insert(storage.view_storage().get_connection())
        .await
        .unwrap();
        fixtures
    }

    async fn append_fixed_root(
        storage: &crate::jupiter::storage::Storage,
        tree: RootTreeFixture,
        parent_commit_id: &str,
    ) -> RootCommitFixture {
        let kind = HashKind::Sha1;
        let author = Signature::from_data(
            b"author HP-11 fixture <hp11@example.test> 1700000000 +0000".to_vec(),
        )
        .unwrap();
        let committer = Signature::from_data(
            b"committer HP-11 fixture <hp11@example.test> 1700000001 +0000".to_vec(),
        )
        .unwrap();
        let commit = Commit::new_with_kind(
            kind,
            author,
            committer,
            tree.root.id,
            vec![ObjectHash::from_hex_for_kind(kind, parent_commit_id).unwrap()],
            "HP-11 fixed appended root commit",
        )
        .unwrap();
        let mono = storage.mono_storage();
        mono.save_mega_trees(tree.trees.clone(), commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .unwrap();
        let view_storage = storage.view_storage();
        let updated = mega_refs::Entity::update_many()
            .col_expr(
                mega_refs::Column::RefCommitHash,
                sea_orm::sea_query::Expr::value(commit.id.to_string()),
            )
            .col_expr(
                mega_refs::Column::RefTreeHash,
                sea_orm::sea_query::Expr::value(commit.tree_id.to_string()),
            )
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .filter(mega_refs::Column::RefCommitHash.eq(parent_commit_id))
            .exec(view_storage.get_connection())
            .await
            .unwrap();
        assert_eq!(
            updated.rows_affected, 1,
            "fixture main@/ CAS must advance once"
        );
        RootCommitFixture {
            commit,
            trees: tree.trees,
            blobs: tree.blobs,
        }
    }

    fn failed_batch_roots() -> Vec<Vec<(String, Vec<u8>)>> {
        (1..=6)
            .map(|number| {
                vec![
                    ("README".to_owned(), format!("root-{number}").into_bytes()),
                    ("a/f".to_owned(), format!("a-{number}").into_bytes()),
                    ("b/f".to_owned(), format!("b-{number}").into_bytes()),
                ]
            })
            .collect()
    }

    async fn failed_batch_fixture(
        batch_size: u64,
    ) -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        ViewMetrics,
        i64,
    ) {
        service_with_history(":prefix=p", failed_batch_roots(), batch_size).await
    }

    async fn service_with_history(
        spec: &str,
        roots: Vec<Vec<(String, Vec<u8>)>>,
        batch_size: u64,
    ) -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        ViewMetrics,
        i64,
    ) {
        let (temp, storage) = storage_with_history(roots, batch_size).await;
        let filter_pk = insert_warming_filter(&storage, 1, spec).await;
        let metrics = ViewMetrics::default();
        (temp, storage, metrics, filter_pk)
    }

    fn hp12_roots() -> Vec<Vec<(String, Vec<u8>)>> {
        (1..=12)
            .map(|number| {
                let c_number = if number == 7 { 6 } else { number };
                vec![
                    ("README".to_owned(), format!("readme-{number}").into_bytes()),
                    ("a/b/f".to_owned(), format!("a-b-{number}").into_bytes()),
                    ("c/f".to_owned(), format!("c-{c_number}").into_bytes()),
                ]
            })
            .collect()
    }

    async fn hp12_storage_with_history(
        batch_size: u64,
    ) -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        Vec<RootCommitFixture>,
        String,
    ) {
        hp12_storage_with_raw_premise(batch_size, false).await
    }

    async fn hp12_storage_for_defect(
        batch_size: u64,
        defect: Hp12Defect,
    ) -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        Vec<RootCommitFixture>,
        String,
    ) {
        hp12_storage_with_raw_premise(batch_size, matches!(defect, Hp12Defect::Premise)).await
    }

    async fn hp12_storage_with_raw_premise(
        batch_size: u64,
        raw_premise_at_seven: bool,
    ) -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        Vec<RootCommitFixture>,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.views.batch_size = batch_size;
        config.views.sync_catch_up_commits = if batch_size < 64 { batch_size } else { 64 };
        let storage = test_storage_with_config(&temp, config).await;
        let trees = hp12_roots()
            .iter()
            .map(|paths| root_tree_from_paths(HashKind::Sha1, paths))
            .collect();
        let fixtures = seed_hp12_linear_history(&storage, trees, raw_premise_at_seven).await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        let seventh_root = fixtures[6]
            .trees
            .iter()
            .find(|tree| tree.id == fixtures[6].commit.tree_id)
            .unwrap();
        let a_tree_id = seventh_root
            .tree_items
            .iter()
            .find(|item| item.name == "a")
            .unwrap()
            .id
            .to_string();
        (temp, storage, fixtures, a_tree_id)
    }

    async fn hp12_counted_storage_for_defect(
        batch_size: u64,
        defect: Hp12Defect,
    ) -> (
        tempfile::TempDir,
        TestSchemaGuard,
        crate::jupiter::storage::Storage,
        Arc<AtomicUsize>,
        Vec<RootCommitFixture>,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let (db_config, schema) = test_db_config(temp.path()).await;
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.database = db_config.clone();
        config.views.batch_size = batch_size;
        config.views.sync_catch_up_commits = if batch_size < 64 { batch_size } else { 64 };
        let mega_commit_selects = Arc::new(AtomicUsize::new(0));
        let callback_count = mega_commit_selects.clone();
        let mut db = database_connection(&db_config).await.unwrap();
        db.set_metric_callback(move |info| {
            let statement = info.statement.to_string();
            if statement.starts_with("SELECT") && statement.contains("mega_commit") {
                callback_count.fetch_add(1, Ordering::Relaxed);
            }
        });
        let storage = crate::jupiter::storage::Storage::new_with_connection(
            Arc::new(config),
            Arc::new(db),
            mock_object_storage(),
        )
        .await
        .unwrap();
        let trees = hp12_roots()
            .iter()
            .map(|paths| root_tree_from_paths(HashKind::Sha1, paths))
            .collect();
        let fixtures =
            seed_hp12_linear_history(&storage, trees, matches!(defect, Hp12Defect::Premise)).await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        let seventh_root = fixtures[6]
            .trees
            .iter()
            .find(|tree| tree.id == fixtures[6].commit.tree_id)
            .unwrap();
        let a_tree_id = seventh_root
            .tree_items
            .iter()
            .find(|item| item.name == "a")
            .unwrap()
            .id
            .to_string();
        (
            temp,
            schema,
            storage,
            mega_commit_selects,
            fixtures,
            a_tree_id,
        )
    }

    async fn hp12_prefix_storage() -> (tempfile::TempDir, crate::jupiter::storage::Storage) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.views.batch_size = 1000;
        config.views.sync_catch_up_commits = 64;
        let storage = test_storage_with_config(&temp, config).await;
        let trees = hp12_roots()[..6]
            .iter()
            .map(|paths| root_tree_from_paths(HashKind::Sha1, paths))
            .collect();
        seed_hp12_linear_history(&storage, trees, false).await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        (temp, storage)
    }

    async fn hp12_prefix_spine_tree(
        storage: &crate::jupiter::storage::Storage,
        fixture: &RootCommitFixture,
    ) -> String {
        let mono = storage.mono_storage();
        let view_storage = storage.view_storage();
        let root_id = fixture.commit.tree_id.to_string();
        let mut source =
            ViewTreeSource::new(&mono, view_storage.get_connection(), HashKind::Sha1).unwrap();
        source
            .prefetch(std::slice::from_ref(&root_id))
            .await
            .unwrap();
        let canonical = parse_for_registration(":/a:prefix=x").unwrap();
        filter_tree(
            HashKind::Sha1,
            &source,
            &mut FilterMemo::with_capacity(FILTER_TREE_MEMO_CAPACITY_BYTES),
            &canonical.filter,
            &root_id,
        )
        .unwrap()
        .tree_id
    }

    #[derive(Clone, Copy, Debug)]
    enum Hp12Defect {
        Premise,
        RowAbsent,
        Unparsable,
        CommitRowMissing,
    }

    enum Hp12Repair {
        Author {
            commit_id: String,
            author: Option<String>,
        },
        Tree(mega_tree::Model),
        Commit(mega_commit::Model),
    }

    impl Hp12Defect {
        fn reason(self, tree_id: &str) -> ViewProjectionStopReason {
            match self {
                Self::Premise => ViewProjectionStopReason::PremiseCheckFailed,
                Self::RowAbsent => ViewProjectionStopReason::RowAbsent {
                    tree_id: tree_id.to_owned(),
                },
                Self::Unparsable => ViewProjectionStopReason::Unparsable {
                    tree_id: tree_id.to_owned(),
                },
                Self::CommitRowMissing => ViewProjectionStopReason::CommitRowMissing,
            }
        }

        fn alert_reason(self) -> &'static str {
            match self {
                Self::Premise => "premise_check_failed",
                Self::RowAbsent => "row_absent",
                Self::Unparsable => "unparsable",
                Self::CommitRowMissing => "commit_row_missing",
            }
        }
    }

    async fn introduce_hp12_defect(
        storage: &crate::jupiter::storage::Storage,
        fixtures: &[RootCommitFixture],
        a_tree_id: &str,
        defect: Hp12Defect,
    ) -> Hp12Repair {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let commit_id = fixtures[6].commit.id.to_string();
        match defect {
            Hp12Defect::Premise => {
                let row = mega_commit::Entity::find()
                    .filter(mega_commit::Column::CommitId.eq(commit_id.clone()))
                    .one(db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(row.author.as_deref(), Some(HP12_SENTINEL_AUTHOR));
                Hp12Repair::Author {
                    commit_id,
                    author: Some(HP12_SENTINEL_AUTHOR.to_owned()),
                }
            }
            Hp12Defect::RowAbsent | Hp12Defect::Unparsable => {
                let tree = mega_tree::Entity::find()
                    .filter(mega_tree::Column::TreeId.eq(a_tree_id))
                    .one(db)
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(defect, Hp12Defect::RowAbsent) {
                    mega_tree::Entity::delete_by_id(tree.id)
                        .exec(db)
                        .await
                        .unwrap();
                } else {
                    let malformed = tree.sub_trees[..tree.sub_trees.len() - 1].to_vec();
                    assert!(
                        crate::ceres::view::tree_source::parse_tree_bytes(
                            HashKind::Sha1,
                            a_tree_id,
                            &malformed,
                        )
                        .is_err()
                    );
                    mega_tree::Entity::update_many()
                        .col_expr(
                            mega_tree::Column::SubTrees,
                            sea_orm::sea_query::Expr::value(malformed),
                        )
                        .filter(mega_tree::Column::Id.eq(tree.id))
                        .exec(db)
                        .await
                        .unwrap();
                }
                Hp12Repair::Tree(tree)
            }
            Hp12Defect::CommitRowMissing => {
                let commit = mega_commit::Entity::find()
                    .filter(mega_commit::Column::CommitId.eq(commit_id))
                    .one(db)
                    .await
                    .unwrap()
                    .unwrap();
                mega_commit::Entity::delete_by_id(commit.id)
                    .exec(db)
                    .await
                    .unwrap();
                Hp12Repair::Commit(commit)
            }
        }
    }

    async fn repair_hp12_defect(storage: &crate::jupiter::storage::Storage, repair: Hp12Repair) {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        match repair {
            Hp12Repair::Author { commit_id, author } => {
                mega_commit::Entity::update_many()
                    .col_expr(
                        mega_commit::Column::Author,
                        sea_orm::sea_query::Expr::value(author),
                    )
                    .filter(mega_commit::Column::CommitId.eq(commit_id))
                    .exec(db)
                    .await
                    .unwrap();
            }
            Hp12Repair::Tree(tree) => {
                if mega_tree::Entity::find_by_id(tree.id)
                    .one(db)
                    .await
                    .unwrap()
                    .is_some()
                {
                    mega_tree::Entity::update_many()
                        .col_expr(
                            mega_tree::Column::SubTrees,
                            sea_orm::sea_query::Expr::value(tree.sub_trees),
                        )
                        .filter(mega_tree::Column::Id.eq(tree.id))
                        .exec(db)
                        .await
                        .unwrap();
                } else {
                    tree.into_active_model().insert(db).await.unwrap();
                }
            }
            Hp12Repair::Commit(commit) => {
                commit.into_active_model().insert(db).await.unwrap();
            }
        }
    }

    fn assert_hp12_stop(
        outcome: CatchUpOutcome,
        defect: Hp12Defect,
        a_tree_id: &str,
        commit_id: &str,
    ) -> ViewProjectionStop {
        match outcome {
            CatchUpOutcome::Stopped(stop) => {
                assert_eq!(stop.seq, 7);
                assert_eq!(stop.commit_id, commit_id);
                assert_eq!(stop.reason, defect.reason(a_tree_id));
                stop
            }
            other => panic!("expected projection stop, got {other:?}"),
        }
    }

    async fn hp12_catch_up(service: &ViewProjectionService, filter_pk: i64) -> CatchUpOutcome {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            service.catch_up(filter_pk),
        )
        .await
        .expect("HP-12 catch-up timeout")
        .unwrap()
    }

    async fn hp12_catch_up_one_batch(
        service: &ViewProjectionService,
        filter_pk: i64,
        snapshot: &crate::config::Config,
        batch_size: usize,
    ) -> CatchUpOutcome {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            service.catch_up_one_batch(filter_pk, snapshot, batch_size),
        )
        .await
        .expect("HP-12 single-batch catch-up timeout")
        .unwrap()
    }

    async fn storage_with_history(
        roots: Vec<Vec<(String, Vec<u8>)>>,
        batch_size: u64,
    ) -> (tempfile::TempDir, crate::jupiter::storage::Storage) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.views.batch_size = batch_size;
        config.views.sync_catch_up_commits = batch_size.min(1);
        let storage = test_storage_with_config(&temp, config).await;
        let trees = roots
            .into_iter()
            .map(|paths| root_tree_from_paths(HashKind::Sha1, &paths))
            .collect();
        seed_fixed_linear_history(&storage, trees).await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        (temp, storage)
    }

    async fn counted_storage_with_history(
        roots: Vec<Vec<(String, Vec<u8>)>>,
        batch_size: u64,
    ) -> (
        tempfile::TempDir,
        TestSchemaGuard,
        crate::jupiter::storage::Storage,
        Arc<AtomicUsize>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let (db_config, schema) = test_db_config(temp.path()).await;
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.database = db_config.clone();
        config.views.batch_size = batch_size;
        config.views.sync_catch_up_commits = batch_size.min(1);
        let counter = Arc::new(AtomicUsize::new(0));
        let callback_counter = counter.clone();
        let mut db = database_connection(&db_config).await.unwrap();
        db.set_metric_callback(move |_| {
            callback_counter.fetch_add(1, Ordering::Relaxed);
        });
        let storage = crate::jupiter::storage::Storage::new_with_connection(
            Arc::new(config),
            Arc::new(db),
            mock_object_storage(),
        )
        .await
        .unwrap();
        let trees = roots
            .into_iter()
            .map(|paths| root_tree_from_paths(HashKind::Sha1, &paths))
            .collect();
        seed_fixed_linear_history(&storage, trees).await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        (temp, schema, storage, counter)
    }

    async fn service_with_unprefetched_history() -> (
        tempfile::TempDir,
        crate::jupiter::storage::Storage,
        ViewMetrics,
        i64,
        String,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let config = crate::config::testing::isolated_config(temp.path().join("config"));
        let storage = test_storage_with_config(&temp, config).await;
        let kind = HashKind::Sha1;
        let empty = crate::ceres::view::tree_source::empty_tree_id(kind).unwrap();
        let b = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(TreeItemMode::Tree, empty, "c".to_owned())],
        )
        .unwrap();
        let a_first = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(TreeItemMode::Tree, b.id, "b".to_owned())],
        )
        .unwrap();
        let root_first = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(
                TreeItemMode::Tree,
                a_first.id,
                "a".to_owned(),
            )],
        )
        .unwrap();
        let second = root_tree_from_paths(kind, &[("a/f".to_owned(), b"two".to_vec())]);
        let a_first_id = a_first.id.to_string();
        seed_fixed_linear_history(
            &storage,
            vec![
                RootTreeFixture {
                    root: root_first.clone(),
                    trees: vec![b, a_first, root_first],
                    blobs: Vec::new(),
                },
                second,
            ],
        )
        .await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        let filter_pk = insert_warming_filter(&storage, 1, ":/a").await;
        (temp, storage, ViewMetrics::default(), filter_pk, a_first_id)
    }

    async fn insert_warming_filter(
        storage: &crate::jupiter::storage::Storage,
        filter_pk: i64,
        spec: &str,
    ) -> i64 {
        let canonical = parse_for_registration(spec).unwrap();
        mega_view_filter::ActiveModel {
            id: Set(filter_pk),
            filter_id: Set(canonical.filter_id),
            canonical_spec: Set(canonical.canonical_text),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!([])),
            push_enabled: Set(false),
            projected_seq: Set(0),
            ready_seq: Set(None),
            warming_since: Set(Some(Utc::now().naive_utc())),
            last_access_at: Set(None),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(storage.view_storage().get_connection())
        .await
        .unwrap();
        filter_pk
    }

    #[derive(Debug, Eq, PartialEq)]
    struct ProjectionSnapshot {
        maps: Vec<(i64, i64, Option<String>, String)>,
        objects: Vec<(String, i16, Vec<u8>, Option<chrono::NaiveDateTime>)>,
        refs: Vec<(i64, String)>,
    }

    async fn projection_snapshot(storage: &crate::jupiter::storage::Storage) -> ProjectionSnapshot {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let maps = mega_view_commit_map::Entity::find()
            .order_by_asc(mega_view_commit_map::Column::FilterPk)
            .order_by_asc(mega_view_commit_map::Column::SeqFrom)
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.filter_pk, row.seq_from, row.view_commit, row.view_tree))
            .collect();
        let objects = mega_view_object::Entity::find()
            .order_by_asc(mega_view_object::Column::ObjectId)
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.object_id, row.kind, row.data, row.gc_marked_at))
            .collect();
        let refs = mega_view_object_ref::Entity::find()
            .order_by_asc(mega_view_object_ref::Column::FilterPk)
            .order_by_asc(mega_view_object_ref::Column::ObjectId)
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.filter_pk, row.object_id))
            .collect();
        ProjectionSnapshot {
            maps,
            objects,
            refs,
        }
    }

    async fn projection_counts(
        storage: &crate::jupiter::storage::Storage,
        filter_pk: i64,
    ) -> (u64, u64, u64) {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let maps = mega_view_commit_map::Entity::find()
            .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
            .count(db)
            .await
            .unwrap();
        let objects = mega_view_object::Entity::find().count(db).await.unwrap();
        let refs = mega_view_object_ref::Entity::find()
            .filter(mega_view_object_ref::Column::FilterPk.eq(filter_pk))
            .count(db)
            .await
            .unwrap();
        (maps, objects, refs)
    }

    async fn reset_projection_state(storage: &crate::jupiter::storage::Storage) {
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        for sql in [
            "DELETE FROM mega_view_object_ref",
            "DELETE FROM mega_view_commit_map",
            "DELETE FROM mega_view_object",
            "UPDATE mega_view_filter SET projected_seq = 0, ready_seq = NULL, warming_since = now()",
        ] {
            db.execute_unprepared(sql).await.unwrap();
        }
    }

    async fn assert_snapshot_warming_count(
        service: &ViewProjectionService,
        storage: &crate::jupiter::storage::Storage,
        expected: u64,
    ) -> crate::jupiter::service::view_metrics::ViewMetricsSnapshot {
        let snapshot = service.metrics_snapshot().await.unwrap();
        let view_storage = storage.view_storage();
        let statement = Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) AS count FROM mega_view_filter WHERE warming_since IS NOT NULL",
        );
        let row = view_storage
            .get_connection()
            .query_one_raw(statement)
            .await
            .unwrap()
            .unwrap();
        let count: i64 = row.try_get("", "count").unwrap();
        assert_eq!(snapshot.view_cold_start_slots_in_use, expected);
        assert_eq!(snapshot.view_cold_start_slots_in_use, count as u64);
        snapshot
    }

    async fn assert_hp12_prefix_state(
        storage: &crate::jupiter::storage::Storage,
        filter_pks: &[i64],
        reference: &ProjectionSnapshot,
    ) {
        assert_eq!(projection_snapshot(storage).await, *reference);
        for filter_pk in filter_pks {
            assert!(
                persisted_commit_map(storage, *filter_pk)
                    .await
                    .iter()
                    .all(|(seq, _, _)| *seq < 7)
            );
            let filter = mega_view_filter::Entity::find_by_id(*filter_pk)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(filter.projected_seq, 6);
            assert!(filter.ready_seq.is_none());
        }
    }

    async fn reference_commit_map(
        storage: &crate::jupiter::storage::Storage,
        fixtures: &[RootCommitFixture],
        spec: &str,
    ) -> Vec<(i64, Option<String>, String)> {
        let kind = HashKind::Sha1;
        let canonical = parse_for_registration(spec).unwrap();
        let mut tree_bytes = HashMap::new();
        for fixture in fixtures {
            for tree in &fixture.trees {
                tree_bytes.insert(tree.id.to_string(), tree.to_data().unwrap());
            }
        }
        let source = InMemoryTreeSource::new(kind, tree_bytes, HashSet::new());
        let db = storage.view_storage();
        let db = db.get_connection();
        let mut memo = FilterMemo::with_capacity(FILTER_TREE_MEMO_CAPACITY_BYTES);
        let empty_tree = empty_tree_id(kind).unwrap().to_string();
        let mut previous = PreviousProjection {
            view_commit: None,
            view_tree: empty_tree.clone(),
        };
        let mut expected = Vec::new();
        for (index, fixture) in fixtures.iter().enumerate() {
            let commit_id = fixture.commit.id.to_string();
            let tree_id = fixture.commit.tree_id.to_string();
            let row = mega_commit::Entity::find()
                .filter(mega_commit::Column::CommitId.eq(commit_id.clone()))
                .one(db)
                .await
                .unwrap()
                .unwrap();
            let parent_tree = if index == 0 {
                empty_tree.clone()
            } else {
                fixtures[index - 1].commit.tree_id.to_string()
            };
            let segment = project_commit(
                kind,
                &source,
                &mut memo,
                &canonical.filter,
                ProjectionInput {
                    seq: index as i64 + 1,
                    commit_id: &commit_id,
                    tree_id: &tree_id,
                    row: Some(&row),
                },
                &parent_tree,
                &previous,
            )
            .unwrap();
            if let Some(segment) = segment {
                previous = PreviousProjection {
                    view_commit: segment
                        .view_commit
                        .as_ref()
                        .map(|commit| commit.id.to_string()),
                    view_tree: segment.view_tree.clone(),
                };
                expected.push((
                    segment.seq,
                    segment.view_commit.map(|commit| commit.id.to_string()),
                    segment.view_tree,
                ));
            }
        }
        expected
    }

    async fn persisted_commit_map(
        storage: &crate::jupiter::storage::Storage,
        filter_pk: i64,
    ) -> Vec<(i64, Option<String>, String)> {
        mega_view_commit_map::Entity::find()
            .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
            .order_by_asc(mega_view_commit_map::Column::SeqFrom)
            .all(storage.view_storage().get_connection())
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.seq_from, row.view_commit, row.view_tree))
            .collect()
    }

    fn persisted_view_object_closure(
        objects: &HashMap<String, &mega_view_object::Model>,
        view_commit: &str,
    ) -> HashSet<String> {
        let mut closure = HashSet::from([view_commit.to_owned()]);
        let commit = objects.get(view_commit).unwrap();
        let commit = Commit::from_bytes(
            &commit.data,
            ObjectHash::from_hex_for_kind(HashKind::Sha1, view_commit).unwrap(),
        )
        .unwrap();
        let mut pending = vec![commit.tree_id.to_string()];
        while let Some(tree_id) = pending.pop() {
            let Some(tree) = objects.get(&tree_id) else {
                continue;
            };
            if !closure.insert(tree_id.clone()) {
                continue;
            }
            let tree = Tree::from_bytes(
                &tree.data,
                ObjectHash::from_hex_for_kind(HashKind::Sha1, &tree_id).unwrap(),
            )
            .unwrap();
            pending.extend(
                tree.tree_items
                    .into_iter()
                    .filter(|item| item.mode == TreeItemMode::Tree)
                    .map(|item| item.id.to_string()),
            );
        }
        closure
    }

    async fn assert_internal_error_no_effect(
        storage: &crate::jupiter::storage::Storage,
        service: &ViewProjectionService,
        metrics: &ViewMetrics,
        filter_pk: i64,
        logs: &CapturedLog,
    ) {
        let before_counts = projection_counts(storage, filter_pk).await;
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let before_filter = mega_view_filter::Entity::find_by_id(filter_pk)
            .one(db)
            .await
            .unwrap();
        let before_state = before_filter
            .as_ref()
            .map(|row| (row.projected_seq, row.ready_seq, row.warming_since));
        let canonical_spec = before_filter.as_ref().map(|row| row.canonical_spec.clone());
        let log_offset = captured_log_offset(logs);
        assert!(service.catch_up(filter_pk).await.is_err());
        assert_one_internal_event(
            logs,
            log_offset,
            before_filter.is_some(),
            canonical_spec.as_deref(),
        );
        assert_eq!(projection_counts(storage, filter_pk).await, before_counts);
        let after_state = mega_view_filter::Entity::find_by_id(filter_pk)
            .one(db)
            .await
            .unwrap()
            .map(|row| (row.projected_seq, row.ready_seq, row.warming_since));
        assert_eq!(after_state, before_state);
        assert_eq!(
            metrics
                .view_batch_premise_failures_total
                .load(Ordering::Relaxed),
            0
        );
    }

    fn determinism_roots() -> Vec<Vec<(String, Vec<u8>)>> {
        (1..=12)
            .map(|number| {
                vec![
                    ("README".to_owned(), format!("readme-{number}").into_bytes()),
                    ("a/file".to_owned(), format!("a-{number}").into_bytes()),
                    ("b/file".to_owned(), format!("b-{number}").into_bytes()),
                    ("c/file".to_owned(), format!("c-{number}").into_bytes()),
                ]
            })
            .collect()
    }

    async fn seed_determinism_filters(storage: &crate::jupiter::storage::Storage) -> Vec<i64> {
        let mut ids = Vec::new();
        for (filter_pk, spec) in [
            (1, ":/a"),
            (2, ":prefix=p"),
            (3, ":exclude[::a]"),
            (4, ":[:/b:prefix=x,:/c:prefix=y]"),
        ] {
            ids.push(insert_warming_filter(storage, filter_pk, spec).await);
        }
        ids
    }

    async fn catch_up_all(
        service: &ViewProjectionService,
        filter_pks: &[i64],
    ) -> ProjectionSnapshot {
        for filter_pk in filter_pks {
            assert_eq!(
                service.catch_up(*filter_pk).await.unwrap(),
                CatchUpOutcome::Ready
            );
        }
        projection_snapshot(&service.storage).await
    }

    type CapturedLog = Arc<Mutex<Vec<u8>>>;

    fn capture_tracing<F: FnOnce(&CapturedLog)>(f: F) -> String {
        capture_tracing_with_max_level(tracing::Level::ERROR, f)
    }

    fn capture_tracing_with_reader<F: FnOnce(&CapturedLog)>(f: F) -> String {
        capture_tracing_with_max_level(tracing::Level::DEBUG, f)
    }

    fn capture_tracing_with_max_level<F: FnOnce(&CapturedLog)>(
        max_level: tracing::Level,
        f: F,
    ) -> String {
        use std::io::Write;

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;

            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let _pin_registry = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_max_level(max_level)
                .finish(),
        );
        let buffer: CapturedLog = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buffer.clone()))
            .with_ansi(false)
            .with_max_level(max_level)
            .finish();
        tracing::subscriber::with_default(subscriber, || f(&buffer));
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    fn captured_log_offset(logs: &CapturedLog) -> usize {
        logs.lock().unwrap().len()
    }

    fn captured_log_since(logs: &CapturedLog, offset: usize) -> String {
        String::from_utf8(logs.lock().unwrap()[offset..].to_vec()).unwrap()
    }

    fn event_field_keys(event: &str) -> BTreeSet<&str> {
        event
            .split_whitespace()
            .filter_map(|token| token.split_once('=').map(|(key, _)| key))
            .collect()
    }

    fn assert_one_premise_event(logs: &CapturedLog, offset: usize) {
        let captured = captured_log_since(logs, offset);
        let events = captured
            .lines()
            .filter(|line| line.contains("view projection batch premise failed"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{captured}");
        let event = events[0];
        assert!(event.contains("metric") && event.contains("view_batch_premise_failures_total"));
        assert!(event.contains("filter_id="));
        assert!(event.contains("s0="));
        assert!(event.contains("tip="));
        assert!(event.contains("batch_size="));
        assert_eq!(
            event_field_keys(event),
            BTreeSet::from(["batch_size", "filter_id", "metric", "s0", "tip"]),
            "{event}"
        );
        assert!(!event.contains("hp11@example.test"));
        assert!(!event.contains("HP-11 fixed root commit"));
    }

    fn assert_one_internal_event(
        logs: &CapturedLog,
        offset: usize,
        has_filter_context: bool,
        canonical_spec: Option<&str>,
    ) {
        let captured = captured_log_since(logs, offset);
        let events = captured
            .lines()
            .filter(|line| line.contains("view projection batch failed"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{captured}");
        let event = events[0];
        assert!(event.contains("filter_pk="));
        assert!(event.contains("error="));
        assert_eq!(
            event_field_keys(event),
            if has_filter_context {
                BTreeSet::from(["error", "filter_id", "filter_pk", "s0"])
            } else {
                BTreeSet::from(["error", "filter_pk"])
            },
            "{event}"
        );
        if let Some(canonical_spec) = canonical_spec {
            assert!(!event.contains(canonical_spec), "{event}");
        }
        assert!(!event.contains("hp11@example.test"));
        assert!(!event.contains("HP-11 fixed root commit"));
    }

    #[tokio::test]
    async fn determinism() {
        let (_reference_temp, reference_storage) =
            storage_with_history(determinism_roots(), 1000).await;
        let reference_filters = seed_determinism_filters(&reference_storage).await;
        let reference_service =
            ViewProjectionService::new(reference_storage.clone(), ViewMetrics::default());
        let reference = catch_up_all(&reference_service, &reference_filters).await;

        let (_second_temp, second_storage) = storage_with_history(determinism_roots(), 1000).await;
        let second_filters = seed_determinism_filters(&second_storage).await;
        let second_service =
            ViewProjectionService::new(second_storage.clone(), ViewMetrics::default());
        assert_eq!(
            catch_up_all(&second_service, &second_filters).await,
            reference
        );

        let (_third_temp, third_storage) = storage_with_history(determinism_roots(), 1000).await;
        let third_filters = seed_determinism_filters(&third_storage).await;
        let third_service =
            ViewProjectionService::new(third_storage.clone(), ViewMetrics::default());
        assert_eq!(
            catch_up_all(&third_service, &third_filters).await,
            reference
        );

        let reference_view_storage = reference_storage.view_storage();
        let reference_db = reference_view_storage.get_connection();
        for sql in [
            "DELETE FROM mega_view_object_ref",
            "DELETE FROM mega_view_commit_map",
            "DELETE FROM mega_view_object",
            "UPDATE mega_view_filter SET projected_seq = 0, ready_seq = NULL, warming_since = now()",
        ] {
            reference_db.execute_unprepared(sql).await.unwrap();
        }
        assert_eq!(
            catch_up_all(&reference_service, &reference_filters).await,
            reference
        );

        let (_one_temp, one_storage) = storage_with_history(determinism_roots(), 1).await;
        let one_filters = seed_determinism_filters(&one_storage).await;
        let one_service = ViewProjectionService::new(one_storage.clone(), ViewMetrics::default());
        assert_eq!(catch_up_all(&one_service, &one_filters).await, reference);

        let (_concurrent_temp, concurrent_storage) =
            storage_with_history(determinism_roots(), 1).await;
        let concurrent_filters = seed_determinism_filters(&concurrent_storage).await;
        let concurrent_service =
            ViewProjectionService::new(concurrent_storage.clone(), ViewMetrics::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let concurrent_first = concurrent_service.clone();
        let concurrent_second = concurrent_service.clone();
        let concurrent_filter = concurrent_filters[0];
        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();
        let (first, second) = tokio::join!(
            async move {
                first_barrier.wait().await;
                loop {
                    match concurrent_first.catch_up(concurrent_filter).await.unwrap() {
                        CatchUpOutcome::Ready => break CatchUpOutcome::Ready,
                        CatchUpOutcome::NotRun => tokio::task::yield_now().await,
                        outcome => panic!("unexpected concurrent catch-up result: {outcome:?}"),
                    }
                }
            },
            async move {
                second_barrier.wait().await;
                loop {
                    match concurrent_second.catch_up(concurrent_filter).await.unwrap() {
                        CatchUpOutcome::Ready => break CatchUpOutcome::Ready,
                        CatchUpOutcome::NotRun => tokio::task::yield_now().await,
                        outcome => panic!("unexpected concurrent catch-up result: {outcome:?}"),
                    }
                }
            },
        );
        assert_eq!(first, CatchUpOutcome::Ready);
        assert_eq!(second, CatchUpOutcome::Ready);
        assert_eq!(
            catch_up_all(&concurrent_service, &concurrent_filters[1..]).await,
            reference
        );

        let (_reload_temp, reload_storage) = storage_with_history(determinism_roots(), 3).await;
        let reload_filters = seed_determinism_filters(&reload_storage).await;
        let reload_service =
            ViewProjectionService::new(reload_storage.clone(), ViewMetrics::default());
        let handle = reload_storage.config_handle();
        let reload_once = Arc::new(AtomicBool::new(false));
        let applied_fields = Arc::new(Mutex::new(None));
        let hook_once = reload_once.clone();
        let hook_fields = applied_fields.clone();
        *reload_service.test_hooks.between_batches.lock().unwrap() = Some(Arc::new(move || {
            if !hook_once.swap(true, Ordering::Relaxed) {
                let mut candidate = (*handle.snapshot().unwrap()).clone();
                candidate.views.batch_size = 2;
                let report = handle.reload(candidate).unwrap();
                *hook_fields.lock().unwrap() = Some(report.applied_fields);
            }
        }));
        assert_eq!(
            reload_service.catch_up(reload_filters[0]).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            *applied_fields.lock().unwrap(),
            Some(vec!["views.batch_size"])
        );
        assert_eq!(
            reload_service
                .test_hooks
                .advanced_batch_sizes
                .lock()
                .unwrap()[..2],
            [3, 2]
        );
        assert_eq!(
            reload_service
                .test_hooks
                .advanced_projected_seqs
                .lock()
                .unwrap()[..2],
            [3, 5]
        );
        *reload_service.test_hooks.between_batches.lock().unwrap() = None;
        assert_eq!(
            catch_up_all(&reload_service, &reload_filters[1..]).await,
            reference
        );
    }

    #[tokio::test]
    async fn commit_map_matches_design() {
        let reference_temp = tempfile::tempdir().unwrap();
        let reference_config =
            crate::config::testing::isolated_config(reference_temp.path().join("config"));
        let reference_storage = test_storage_with_config(&reference_temp, reference_config).await;
        let mut reference_fixtures = seed_fixed_linear_history(
            &reference_storage,
            vec![
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), b"one".to_vec()),
                        ("a/file".to_owned(), b"same".to_vec()),
                    ],
                ),
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), b"two".to_vec()),
                        ("a/file".to_owned(), b"same".to_vec()),
                    ],
                ),
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), b"three".to_vec()),
                        ("a/file".to_owned(), b"changed".to_vec()),
                    ],
                ),
            ],
        )
        .await;
        let reference_view_storage = reference_storage.view_storage();
        assert_eq!(
            reference_view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        let reference_filter = insert_warming_filter(&reference_storage, 1, ":/a").await;
        let reference_expected =
            reference_commit_map(&reference_storage, &reference_fixtures, ":/a").await;
        let reference_service =
            ViewProjectionService::new(reference_storage.clone(), ViewMetrics::default());
        assert_eq!(
            reference_service.catch_up(reference_filter).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            persisted_commit_map(&reference_storage, reference_filter).await,
            reference_expected
        );
        let main = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(reference_view_storage.get_connection())
            .await
            .unwrap()
            .unwrap();
        reference_fixtures.push(
            append_fixed_root(
                &reference_storage,
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), b"four".to_vec()),
                        ("a/file".to_owned(), b"changed".to_vec()),
                    ],
                ),
                &main.ref_commit_hash,
            )
            .await,
        );
        assert_eq!(
            reference_view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        let reference_expected =
            reference_commit_map(&reference_storage, &reference_fixtures, ":/a").await;
        assert_eq!(
            reference_service.catch_up(reference_filter).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            persisted_commit_map(&reference_storage, reference_filter).await,
            reference_expected
        );

        let (_temp, storage, metrics, filter_pk) = service_with_history(
            ":/a",
            vec![
                vec![
                    ("README".to_owned(), b"one".to_vec()),
                    ("a/file".to_owned(), b"same".to_vec()),
                ],
                vec![
                    ("README".to_owned(), b"two".to_vec()),
                    ("a/file".to_owned(), b"same".to_vec()),
                ],
                vec![
                    ("README".to_owned(), b"three".to_vec()),
                    ("a/file".to_owned(), b"changed".to_vec()),
                ],
            ],
            1000,
        )
        .await;
        let service = ViewProjectionService::new(storage.clone(), metrics);
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        let rows = mega_view_commit_map::Entity::find()
            .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
            .order_by_asc(mega_view_commit_map::Column::SeqFrom)
            .all(storage.view_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq_from, 1);
        assert!(rows[0].view_commit.is_some());
        assert_eq!(rows[1].seq_from, 3);
        assert!(rows[1].view_commit.is_some());
        let view_storage = storage.view_storage();
        let transaction = view_storage.get_connection().begin().await.unwrap();
        let previous = view_storage
            .previous_projection(
                &transaction,
                filter_pk,
                2,
                &crate::ceres::view::tree_source::empty_tree_id(HashKind::Sha1)
                    .unwrap()
                    .to_string(),
            )
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
        assert_eq!(previous.view_commit, rows[0].view_commit);
        assert_eq!(previous.view_tree, rows[0].view_tree);
        let parent = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(view_storage.get_connection())
            .await
            .unwrap()
            .unwrap();
        append_fixed_root(
            &storage,
            root_tree_from_paths(
                HashKind::Sha1,
                &[
                    ("README".to_owned(), b"four".to_vec()),
                    ("a/file".to_owned(), b"changed".to_vec()),
                ],
            ),
            &parent.ref_commit_hash,
        )
        .await;
        assert_eq!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            mega_view_commit_map::Entity::find()
                .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
                .count(view_storage.get_connection())
                .await
                .unwrap(),
            2
        );
        let parent = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(view_storage.get_connection())
            .await
            .unwrap()
            .unwrap();
        append_fixed_root(
            &storage,
            root_tree_from_paths(
                HashKind::Sha1,
                &[
                    ("README".to_owned(), b"five".to_vec()),
                    ("a/file".to_owned(), b"new".to_vec()),
                ],
            ),
            &parent.ref_commit_hash,
        )
        .await;
        assert_eq!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            mega_view_commit_map::Entity::find()
                .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
                .count(view_storage.get_connection())
                .await
                .unwrap(),
            3
        );

        let absent_temp = tempfile::tempdir().unwrap();
        let absent_config =
            crate::config::testing::isolated_config(absent_temp.path().join("config"));
        let storage = test_storage_with_config(&absent_temp, absent_config).await;
        let absent_fixtures = seed_fixed_linear_history(
            &storage,
            vec![
                root_tree_from_paths(HashKind::Sha1, &[("README".to_owned(), b"one".to_vec())]),
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("README".to_owned(), b"two".to_vec()),
                        ("absent/file".to_owned(), b"now-present".to_vec()),
                    ],
                ),
            ],
        )
        .await;
        let view_storage = storage.view_storage();
        assert_eq!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        let filter_pk = insert_warming_filter(&storage, 1, ":/absent").await;
        let empty_tree = empty_tree_id(HashKind::Sha1).unwrap().to_string();
        let transaction = view_storage.get_connection().begin().await.unwrap();
        let previous = view_storage
            .previous_projection(&transaction, filter_pk, 0, &empty_tree)
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
        assert_eq!(
            previous,
            PreviousProjection {
                view_commit: None,
                view_tree: empty_tree.clone(),
            }
        );
        let expected = reference_commit_map(&storage, &absent_fixtures, ":/absent").await;
        assert_eq!(expected[0], (1, None, empty_tree));
        let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(persisted_commit_map(&storage, filter_pk).await, expected);

        let temp = tempfile::tempdir().unwrap();
        let config = crate::config::testing::isolated_config(temp.path().join("config"));
        let storage = test_storage_with_config(&temp, config).await;
        let kind = HashKind::Sha1;
        let empty = crate::ceres::view::tree_source::empty_tree_id(kind).unwrap();
        let b = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(TreeItemMode::Tree, empty, "c".to_owned())],
        )
        .unwrap();
        let a_first = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(TreeItemMode::Tree, b.id, "b".to_owned())],
        )
        .unwrap();
        let root_first = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(
                TreeItemMode::Tree,
                a_first.id,
                "a".to_owned(),
            )],
        )
        .unwrap();
        let second = root_tree_from_paths(kind, &[("a/f".to_owned(), b"two".to_vec())]);
        let unprefetched_fixtures = seed_fixed_linear_history(
            &storage,
            vec![
                RootTreeFixture {
                    root: root_first.clone(),
                    trees: vec![b, a_first.clone(), root_first],
                    blobs: Vec::new(),
                },
                second,
            ],
        )
        .await;
        storage
            .view_storage()
            .extend_root_chain(None, 1000, ViewLockMode::Try)
            .await
            .unwrap();
        let filter_pk = insert_warming_filter(&storage, 1, ":/a").await;
        let expected = reference_commit_map(&storage, &unprefetched_fixtures, ":/a").await;
        let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(persisted_commit_map(&storage, filter_pk).await, expected);
        assert!(service.test_hooks.refetches.load(Ordering::Relaxed) >= 1);
        let first = mega_view_commit_map::Entity::find_by_id((filter_pk, 1))
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.view_tree, a_first.id.to_string());
        let view_commit = first.view_commit.unwrap();
        let object = mega_view_object::Entity::find_by_id(view_commit.clone())
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        let commit = Commit::from_bytes(
            &object.data,
            ObjectHash::from_hex_for_kind(kind, &view_commit).unwrap(),
        )
        .unwrap();
        assert!(commit.parent_commit_ids.is_empty());
    }

    #[tokio::test]
    async fn object_refs_match_design() {
        let (_temp, storage) = storage_with_history(
            vec![
                vec![("a/file".to_owned(), b"before-b-c".to_vec())],
                vec![
                    ("a/file".to_owned(), b"one".to_vec()),
                    ("b/file".to_owned(), b"one".to_vec()),
                    ("c/file".to_owned(), b"one".to_vec()),
                ],
                vec![("a/file".to_owned(), b"two".to_vec())],
            ],
            1,
        )
        .await;
        let filter_pks = seed_determinism_filters(&storage).await;
        let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
        assert_eq!(
            service
                .catch_up_one_batch(filter_pks[2], &storage.config(), 1)
                .await
                .unwrap(),
            CatchUpOutcome::Advanced
        );
        let initial_empty = mega_view_commit_map::Entity::find_by_id((filter_pks[2], 1))
            .one(storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert!(initial_empty.view_commit.is_none());
        assert_eq!(
            initial_empty.view_tree,
            empty_tree_id(HashKind::Sha1).unwrap().to_string()
        );
        assert_eq!(
            mega_view_object::Entity::find()
                .count(storage.view_storage().get_connection())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            mega_view_object_ref::Entity::find()
                .filter(mega_view_object_ref::Column::FilterPk.eq(filter_pks[2]))
                .count(storage.view_storage().get_connection())
                .await
                .unwrap(),
            0
        );
        catch_up_all(&service, &filter_pks).await;
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        let objects = mega_view_object::Entity::find().all(db).await.unwrap();
        let object_ids = objects
            .iter()
            .map(|object| object.object_id.as_str())
            .collect::<HashSet<_>>();
        let objects_by_id = objects
            .iter()
            .map(|object| (object.object_id.clone(), object))
            .collect::<HashMap<_, _>>();
        assert!(!objects.is_empty());
        for filter_pk in &filter_pks {
            let refs = mega_view_object_ref::Entity::find()
                .filter(mega_view_object_ref::Column::FilterPk.eq(*filter_pk))
                .all(db)
                .await
                .unwrap();
            assert!(
                refs.iter()
                    .all(|reference| object_ids.contains(reference.object_id.as_str()))
            );
            let ref_ids = refs
                .iter()
                .map(|reference| reference.object_id.as_str())
                .collect::<HashSet<_>>();
            let maps = mega_view_commit_map::Entity::find()
                .filter(mega_view_commit_map::Column::FilterPk.eq(*filter_pk))
                .all(db)
                .await
                .unwrap();
            for map in &maps {
                if let Some(commit) = &map.view_commit {
                    for object_id in persisted_view_object_closure(&objects_by_id, commit) {
                        assert!(
                            ref_ids.contains(object_id.as_str()),
                            "filter {filter_pk} is missing a ref for {object_id}"
                        );
                    }
                }
            }
            if *filter_pk == 1 {
                for map in maps.iter().filter(|map| map.view_commit.is_some()) {
                    assert!(!object_ids.contains(map.view_tree.as_str()));
                    assert!(!ref_ids.contains(map.view_tree.as_str()));
                }
            } else {
                for map in maps.iter().filter(|map| map.view_commit.is_some()) {
                    assert!(
                        ref_ids.contains(map.view_tree.as_str()),
                        "filter {filter_pk} is missing its view tree ref at seq {}: {}",
                        map.seq_from,
                        map.view_tree
                    );
                }
            }
        }

        let exclude_ref_ids = mega_view_object_ref::Entity::find()
            .filter(mega_view_object_ref::Column::FilterPk.eq(3_i64))
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|reference| reference.object_id)
            .collect::<HashSet<_>>();
        let other_ref_ids = mega_view_object_ref::Entity::find()
            .filter(mega_view_object_ref::Column::FilterPk.ne(3_i64))
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|reference| reference.object_id)
            .collect::<HashSet<_>>();
        let deleted_exclude_objects = objects
            .iter()
            .filter(|object| {
                exclude_ref_ids.contains(&object.object_id)
                    && !other_ref_ids.contains(&object.object_id)
            })
            .map(|object| (object.object_id.clone(), object.kind, object.data.clone()))
            .collect::<Vec<_>>();
        assert!(!deleted_exclude_objects.is_empty());
        mega_view_object::Entity::delete_many()
            .filter(
                mega_view_object::Column::ObjectId
                    .is_in(deleted_exclude_objects.iter().map(|(id, _, _)| id.clone())),
            )
            .exec(db)
            .await
            .unwrap();

        let empty_tree = crate::ceres::view::tree_source::empty_tree_id(HashKind::Sha1)
            .unwrap()
            .to_string();
        for filter_pk in [3, 4] {
            assert!(
                mega_view_object_ref::Entity::find_by_id((filter_pk, empty_tree.clone()))
                    .one(db)
                    .await
                    .unwrap()
                    .is_some()
            );
        }

        mega_view_object::Entity::update_many()
            .col_expr(
                mega_view_object::Column::GcMarkedAt,
                sea_orm::sea_query::Expr::value(Some(Utc::now().naive_utc())),
            )
            .filter(mega_view_object::Column::ObjectId.eq(empty_tree.clone()))
            .exec(db)
            .await
            .unwrap();
        let memo_hits_before = service.test_hooks.memo_hits.load(Ordering::Relaxed);
        mega_view_object_ref::Entity::delete_many()
            .filter(mega_view_object_ref::Column::FilterPk.eq(3_i64))
            .exec(db)
            .await
            .unwrap();
        mega_view_commit_map::Entity::delete_many()
            .filter(mega_view_commit_map::Column::FilterPk.eq(3))
            .exec(db)
            .await
            .unwrap();
        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::ProjectedSeq,
                sea_orm::sea_query::Expr::value(0_i64),
            )
            .col_expr(
                mega_view_filter::Column::ReadySeq,
                sea_orm::sea_query::Expr::value(sea_orm::Value::BigInt(None)),
            )
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(Some(Utc::now().naive_utc())),
            )
            .filter(mega_view_filter::Column::Id.eq(3))
            .exec(db)
            .await
            .unwrap();
        assert_eq!(service.catch_up(3).await.unwrap(), CatchUpOutcome::Ready);
        assert!(service.test_hooks.memo_hits.load(Ordering::Relaxed) > memo_hits_before);
        assert!(
            mega_view_object::Entity::find_by_id(empty_tree.clone())
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .gc_marked_at
                .is_none()
        );
        for (object_id, kind, data) in deleted_exclude_objects {
            let object = mega_view_object::Entity::find_by_id(object_id.clone())
                .one(db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(object.kind, kind);
            assert_eq!(object.data, data);
            assert!(
                mega_view_object_ref::Entity::find_by_id((3, object_id))
                    .one(db)
                    .await
                    .unwrap()
                    .is_some()
            );
        }

        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::ProjectedSeq,
                sea_orm::sea_query::Expr::value(0_i64),
            )
            .col_expr(
                mega_view_filter::Column::ReadySeq,
                sea_orm::sea_query::Expr::value(sea_orm::Value::BigInt(None)),
            )
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(Some(Utc::now().naive_utc())),
            )
            .filter(mega_view_filter::Column::Id.eq(2))
            .exec(db)
            .await
            .unwrap();
        assert_eq!(service.catch_up(2).await.unwrap(), CatchUpOutcome::Ready);
    }

    #[tokio::test]
    async fn ready_same_txn() {
        let roots = (1..=5)
            .map(|number| vec![("a/file".to_owned(), format!("{number}").into_bytes())])
            .collect();
        let (_temp, storage, metrics, filter_pk) = service_with_history(":/a", roots, 1).await;
        let service = ViewProjectionService::new(storage.clone(), metrics);
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        for expected_seq in 1..=5 {
            assert_eq!(
                service
                    .catch_up_one_batch(filter_pk, &storage.config(), 1)
                    .await
                    .unwrap(),
                if expected_seq == 5 {
                    CatchUpOutcome::Ready
                } else {
                    CatchUpOutcome::Advanced
                }
            );
            let row = mega_view_filter::Entity::find_by_id(filter_pk)
                .one(db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.projected_seq, expected_seq);
            if expected_seq < 5 {
                assert!(row.ready_seq.is_none());
                assert!(row.warming_since.is_some());
            } else {
                assert_eq!(row.ready_seq, Some(5));
                assert!(row.warming_since.is_none());
            }
        }
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        let tail_main = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let displaced_hash = "f".repeat(40);
        let updated = mega_refs::Entity::update_many()
            .col_expr(
                mega_refs::Column::RefCommitHash,
                sea_orm::sea_query::Expr::value(displaced_hash.clone()),
            )
            .filter(mega_refs::Column::Id.eq(tail_main.id))
            .filter(mega_refs::Column::RefCommitHash.eq(tail_main.ref_commit_hash.clone()))
            .exec(db)
            .await
            .unwrap();
        assert_eq!(
            updated.rows_affected, 1,
            "fixture main@/ CAS must displace once"
        );
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::MainNotCovered
        );
        let updated = mega_refs::Entity::update_many()
            .col_expr(
                mega_refs::Column::RefCommitHash,
                sea_orm::sea_query::Expr::value(tail_main.ref_commit_hash),
            )
            .filter(mega_refs::Column::Id.eq(tail_main.id))
            .filter(mega_refs::Column::RefCommitHash.eq(displaced_hash))
            .exec(db)
            .await
            .unwrap();
        assert_eq!(
            updated.rows_affected, 1,
            "fixture main@/ CAS must restore once"
        );
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );

        let second_filter = insert_warming_filter(&storage, 2, ":prefix=p").await;
        let warming_before_main_move = mega_view_filter::Entity::find_by_id(second_filter)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .warming_since;
        let tail_main = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let detached = append_fixed_root(
            &storage,
            root_tree_from_paths(
                HashKind::Sha1,
                &[("a/file".to_owned(), b"detached".to_vec())],
            ),
            &tail_main.ref_commit_hash,
        )
        .await;
        assert_eq!(
            service.catch_up(second_filter).await.unwrap(),
            CatchUpOutcome::MainNotCovered
        );
        let pending = mega_view_filter::Entity::find_by_id(second_filter)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.projected_seq, 5);
        assert!(pending.ready_seq.is_none());
        assert_eq!(pending.warming_since, warming_before_main_move);
        let updated = mega_refs::Entity::update_many()
            .col_expr(
                mega_refs::Column::RefCommitHash,
                sea_orm::sea_query::Expr::value(tail_main.ref_commit_hash.clone()),
            )
            .col_expr(
                mega_refs::Column::RefTreeHash,
                sea_orm::sea_query::Expr::value(tail_main.ref_tree_hash.clone()),
            )
            .filter(mega_refs::Column::Id.eq(tail_main.id))
            .filter(mega_refs::Column::RefCommitHash.eq(detached.commit.id.to_string()))
            .filter(mega_refs::Column::RefTreeHash.eq(detached.commit.tree_id.to_string()))
            .exec(db)
            .await
            .unwrap();
        assert_eq!(
            updated.rows_affected, 1,
            "fixture main@/ CAS must return to tail once"
        );
        assert_eq!(
            service.catch_up(second_filter).await.unwrap(),
            CatchUpOutcome::Ready
        );
        let recovered_at_tail = mega_view_filter::Entity::find_by_id(second_filter)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered_at_tail.ready_seq, Some(5));
        assert!(recovered_at_tail.warming_since.is_none());

        let third_filter = insert_warming_filter(&storage, 3, ":prefix=q").await;
        let warming_before_extend = mega_view_filter::Entity::find_by_id(third_filter)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .warming_since;
        let main = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
            .one(db)
            .await
            .unwrap()
            .unwrap();
        append_fixed_root(
            &storage,
            root_tree_from_paths(HashKind::Sha1, &[("a/file".to_owned(), b"six".to_vec())]),
            &main.ref_commit_hash,
        )
        .await;
        assert_eq!(
            service.catch_up(third_filter).await.unwrap(),
            CatchUpOutcome::MainNotCovered
        );
        let third_pending = mega_view_filter::Entity::find_by_id(third_filter)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third_pending.projected_seq, 5);
        assert!(third_pending.ready_seq.is_none());
        assert_eq!(third_pending.warming_since, warming_before_extend);
        assert_eq!(
            view_storage
                .extend_root_chain(None, 1000, ViewLockMode::Try)
                .await
                .unwrap(),
            crate::jupiter::storage::view_root_chain::RootChainOutcome::CaughtUp
        );
        assert_eq!(
            service.catch_up(third_filter).await.unwrap(),
            CatchUpOutcome::Ready
        );
        assert_eq!(
            service.catch_up(filter_pk).await.unwrap(),
            CatchUpOutcome::Ready
        );
        let extended = mega_view_filter::Entity::find_by_id(filter_pk)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(extended.projected_seq, 6);
        assert_eq!(extended.ready_seq, Some(5));
    }

    #[tokio::test]
    async fn noop_returns() {
        let (_temp, storage, metrics, filter_pk) =
            service_with_history(":/a", vec![vec![("a/file".to_owned(), b"one".to_vec())]], 1)
                .await;
        let view_storage = storage.view_storage();
        let db = view_storage.get_connection();
        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(sea_orm::Value::ChronoDateTime(None)),
            )
            .filter(mega_view_filter::Column::Id.eq(filter_pk))
            .exec(db)
            .await
            .unwrap();
        let service = ViewProjectionService::new(storage.clone(), metrics);
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                service.catch_up(filter_pk),
            )
            .await
            .unwrap()
            .unwrap(),
            CatchUpOutcome::NotRun
        );
        assert_eq!(
            mega_view_commit_map::Entity::find()
                .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
                .count(db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(mega_view_object::Entity::find().count(db).await.unwrap(), 0);
        assert_eq!(
            mega_view_object_ref::Entity::find()
                .filter(mega_view_object_ref::Column::FilterPk.eq(filter_pk))
                .count(db)
                .await
                .unwrap(),
            0
        );
        let row = mega_view_filter::Entity::find_by_id(filter_pk)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.projected_seq, 0);
        assert_eq!(projection_counts(&storage, filter_pk).await, (0, 0, 0));
        assert!(row.ready_seq.is_none());
        assert!(row.warming_since.is_none());

        for lock in [ViewLock::Filter(filter_pk), ViewLock::ObjectGcExclusive] {
            let (_temp, storage, metrics, filter_pk) =
                service_with_history(":/a", vec![vec![("a/file".to_owned(), b"one".to_vec())]], 1)
                    .await;
            let holder = storage
                .view_storage()
                .get_connection()
                .begin()
                .await
                .unwrap();
            assert!(
                acquire_view_lock(&holder, lock, ViewLockMode::Try)
                    .await
                    .unwrap()
            );
            let service = ViewProjectionService::new(storage.clone(), metrics);
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    service.catch_up(filter_pk),
                )
                .await
                .unwrap()
                .unwrap(),
                CatchUpOutcome::NotRun
            );
            assert_eq!(
                mega_view_commit_map::Entity::find()
                    .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
                    .count(storage.view_storage().get_connection())
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                mega_view_object::Entity::find()
                    .count(storage.view_storage().get_connection())
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                mega_view_object_ref::Entity::find()
                    .filter(mega_view_object_ref::Column::FilterPk.eq(filter_pk))
                    .count(storage.view_storage().get_connection())
                    .await
                    .unwrap(),
                0
            );
            let row = mega_view_filter::Entity::find_by_id(filter_pk)
                .one(storage.view_storage().get_connection())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.projected_seq, 0);
            assert!(row.ready_seq.is_none());
            assert!(row.warming_since.is_some());
            holder.rollback().await.unwrap();
            assert_eq!(
                service.catch_up(filter_pk).await.unwrap(),
                CatchUpOutcome::Ready
            );
        }
    }

    #[test]
    fn failed_batch_no_effect() {
        let captured = capture_tracing(|logs| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(0).await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let log_offset = captured_log_offset(logs);
                    assert_eq!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            service.catch_up(filter_pk),
                        )
                        .await
                        .unwrap()
                        .unwrap(),
                        CatchUpOutcome::BatchPremiseFailed
                    );
                    assert_eq!(
                        metrics
                            .view_batch_premise_failures_total
                            .load(Ordering::Relaxed),
                        1
                    );
                    let row = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(row.projected_seq, 0);
                    assert!(row.ready_seq.is_none());
                    assert!(row.warming_since.is_some());
                    assert_eq!(projection_counts(&storage, filter_pk).await, (0, 0, 0));
                    assert_one_premise_event(logs, log_offset);

                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(0).await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_eq!(
                        service
                            .catch_up_one_batch(filter_pk, &storage.config(), 2)
                            .await
                            .unwrap(),
                        CatchUpOutcome::Advanced
                    );
                    let counts_before = projection_counts(&storage, filter_pk).await;
                    let log_offset = captured_log_offset(logs);
                    assert_eq!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            service.catch_up(filter_pk),
                        )
                        .await
                        .unwrap()
                        .unwrap(),
                        CatchUpOutcome::BatchPremiseFailed
                    );
                    assert_eq!(
                        metrics
                            .view_batch_premise_failures_total
                            .load(Ordering::Relaxed),
                        1
                    );
                    assert_eq!(
                        mega_view_filter::Entity::find_by_id(filter_pk)
                            .one(storage.view_storage().get_connection())
                            .await
                            .unwrap()
                            .unwrap()
                            .projected_seq,
                        2
                    );
                    let row = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(row.ready_seq.is_none());
                    assert!(row.warming_since.is_some());
                    assert_eq!(projection_counts(&storage, filter_pk).await, counts_before);
                    assert_one_premise_event(logs, log_offset);

                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(1000).await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_eq!(
                        service
                            .catch_up_one_batch(filter_pk, &storage.config(), 2)
                            .await
                            .unwrap(),
                        CatchUpOutcome::Advanced
                    );
                    storage
                        .view_storage()
                        .get_connection()
                        .execute_unprepared("DELETE FROM mega_view_root_chain WHERE seq = 3")
                        .await
                        .unwrap();
                    let counts_before = projection_counts(&storage, filter_pk).await;
                    let log_offset = captured_log_offset(logs);
                    assert_eq!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            service.catch_up(filter_pk),
                        )
                        .await
                        .unwrap()
                        .unwrap(),
                        CatchUpOutcome::BatchPremiseFailed
                    );
                    assert_eq!(
                        metrics
                            .view_batch_premise_failures_total
                            .load(Ordering::Relaxed),
                        1
                    );
                    assert_eq!(projection_counts(&storage, filter_pk).await, counts_before);
                    assert_eq!(
                        mega_view_filter::Entity::find_by_id(filter_pk)
                            .one(storage.view_storage().get_connection())
                            .await
                            .unwrap()
                            .unwrap()
                            .projected_seq,
                        2
                    );
                    let row = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(row.ready_seq.is_none());
                    assert!(row.warming_since.is_some());
                    assert_one_premise_event(logs, log_offset);

                    let (_temp, storage, metrics, _) = failed_batch_fixture(1000).await;
                    assert!(
                        mega_view_filter::Entity::find_by_id(404_i64)
                            .one(storage.view_storage().get_connection())
                            .await
                            .unwrap()
                            .is_none()
                    );
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_internal_error_no_effect(&storage, &service, &metrics, 404, logs).await;

                    let (_temp, storage, metrics, filter_pk, target_tree) =
                        service_with_unprefetched_history().await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    *service
                        .test_hooks
                        .omit_unprefetched_record_for
                        .lock()
                        .unwrap() = Some(target_tree.clone());
                    assert_internal_error_no_effect(&storage, &service, &metrics, filter_pk, logs)
                        .await;

                    let (_temp, storage, metrics, filter_pk, target_tree) =
                        service_with_unprefetched_history().await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    *service.test_hooks.always_unprefetched_for.lock().unwrap() = Some(target_tree);
                    assert_internal_error_no_effect(&storage, &service, &metrics, filter_pk, logs)
                        .await;
                    assert!(service.test_hooks.refetches.load(Ordering::Relaxed) >= 1);

                    let (_temp, storage, metrics, _) = failed_batch_fixture(1000).await;
                    let conflicting_filter =
                        insert_warming_filter(&storage, 2, ":[:/a:prefix=x,:/b:prefix=x]").await;
                    let conflicting = mega_view_filter::Entity::find_by_id(conflicting_filter)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(
                        recheck_definition(&conflicting.canonical_spec, &conflicting.filter_id)
                            .is_ok()
                    );
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_internal_error_no_effect(
                        &storage,
                        &service,
                        &metrics,
                        conflicting_filter,
                        logs,
                    )
                    .await;

                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(1000).await;
                    mega_view_filter::Entity::update_many()
                        .col_expr(
                            mega_view_filter::Column::CanonicalSpec,
                            sea_orm::sea_query::Expr::value(":prefix=p:nop"),
                        )
                        .filter(mega_view_filter::Column::Id.eq(filter_pk))
                        .exec(storage.view_storage().get_connection())
                        .await
                        .unwrap();
                    let corrupt = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(matches!(
                        recheck_definition(&corrupt.canonical_spec, &corrupt.filter_id),
                        Err(error) if error.failed == RecheckFailure::RoundTrip
                    ));
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_internal_error_no_effect(&storage, &service, &metrics, filter_pk, logs)
                        .await;

                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(1000).await;
                    mega_view_filter::Entity::update_many()
                        .col_expr(
                            mega_view_filter::Column::FilterId,
                            sea_orm::sea_query::Expr::value("0".repeat(64)),
                        )
                        .filter(mega_view_filter::Column::Id.eq(filter_pk))
                        .exec(storage.view_storage().get_connection())
                        .await
                        .unwrap();
                    let corrupt = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(matches!(
                        recheck_definition(&corrupt.canonical_spec, &corrupt.filter_id),
                        Err(error) if error.failed == RecheckFailure::FilterIdMismatch
                    ));
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    assert_internal_error_no_effect(&storage, &service, &metrics, filter_pk, logs)
                        .await;

                    let (_temp, storage, metrics, filter_pk) = failed_batch_fixture(1000).await;
                    let view_storage = storage.view_storage();
                    let db = view_storage.get_connection();
                    db.execute_unprepared(
                        "ALTER TABLE mega_view_object_ref RENAME COLUMN object_id TO hp11_gone",
                    )
                    .await
                    .unwrap();
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let log_offset = captured_log_offset(logs);
                    assert!(service.catch_up(filter_pk).await.is_err());
                    assert_one_internal_event(logs, log_offset, true, Some(":prefix=p"));
                    assert_eq!(
                        metrics
                            .view_batch_premise_failures_total
                            .load(Ordering::Relaxed),
                        0
                    );
                    assert_eq!(
                        mega_view_commit_map::Entity::find()
                            .filter(mega_view_commit_map::Column::FilterPk.eq(filter_pk))
                            .count(db)
                            .await
                            .unwrap(),
                        0
                    );
                    assert_eq!(
                        mega_view_filter::Entity::find_by_id(filter_pk)
                            .one(db)
                            .await
                            .unwrap()
                            .unwrap()
                            .projected_seq,
                        0
                    );
                    let row = mega_view_filter::Entity::find_by_id(filter_pk)
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(row.ready_seq.is_none());
                    assert!(row.warming_since.is_some());
                    db.execute_unprepared(
                        "ALTER TABLE mega_view_object_ref RENAME COLUMN hp11_gone TO object_id",
                    )
                    .await
                    .unwrap();
                    assert_eq!(projection_counts(&storage, filter_pk).await, (0, 0, 0));
                    assert_eq!(
                        service.catch_up(filter_pk).await.unwrap(),
                        CatchUpOutcome::Ready
                    );
                    let (_reference_temp, reference_storage, reference_metrics, reference_filter) =
                        failed_batch_fixture(1000).await;
                    let reference_service =
                        ViewProjectionService::new(reference_storage.clone(), reference_metrics);
                    assert_eq!(
                        reference_service.catch_up(reference_filter).await.unwrap(),
                        CatchUpOutcome::Ready
                    );
                    assert_eq!(
                        projection_snapshot(&storage).await,
                        projection_snapshot(&reference_storage).await
                    );
                });
        });
        let premise_events = captured
            .lines()
            .filter(|line| line.contains("view projection batch premise failed"))
            .collect::<Vec<_>>();
        assert_eq!(premise_events.len(), 3, "{captured}");
        for event in premise_events {
            assert!(
                event.contains("metric") && event.contains("view_batch_premise_failures_total")
            );
            assert!(event.contains("filter_id="));
            assert!(event.contains("s0="));
            assert!(event.contains("tip="));
            assert!(event.contains("batch_size="));
            assert!(!event.contains("filter_pk="));
        }
        let internal_events = captured
            .lines()
            .filter(|line| line.contains("view projection batch failed"))
            .collect::<Vec<_>>();
        assert_eq!(internal_events.len(), 7, "{captured}");
        assert_eq!(
            internal_events
                .iter()
                .filter(|event| event.contains("filter_id=") && event.contains("s0="))
                .count(),
            6,
            "{captured}"
        );
        for event in internal_events {
            assert!(event.contains("filter_pk="));
            assert!(event.contains("error="));
            assert!(!event.contains("metric="));
        }
        assert!(
            !captured.contains("HP-11 fixed root commit"),
            "captured unexpected commit content: {captured}"
        );
        assert!(!captured.contains("hp11@example.test"));
        assert!(!captured.contains(":prefix=p"));
        assert!(!captured.contains(":[:/a:prefix=x,:/b:prefix=x]"));
    }

    #[tokio::test]
    async fn final_batch_returns_ready_without_a_terminal_transaction() {
        async fn count(one_batch: bool) -> usize {
            let (_temp, _schema, storage, counter) = counted_storage_with_history(
                vec![vec![("a/file".to_owned(), b"one".to_vec())]],
                1000,
            )
            .await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/a").await;
            let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
            counter.store(0, Ordering::Relaxed);
            let outcome = if one_batch {
                service
                    .catch_up_one_batch(filter_pk, &storage.config(), 1000)
                    .await
            } else {
                service.catch_up(filter_pk).await
            };
            assert_eq!(outcome.unwrap(), CatchUpOutcome::Ready);
            assert_eq!(
                service.test_hooks.terminal_checks.load(Ordering::Relaxed),
                1
            );
            counter.load(Ordering::Relaxed)
        }

        assert_eq!(count(true).await, count(false).await);
    }

    #[tokio::test]
    async fn statement_count_independent_of_batch() {
        fn roots(batch_size: u64) -> Vec<Vec<(String, Vec<u8>)>> {
            (1..=batch_size)
                .map(|number| {
                    vec![
                        ("README".to_owned(), format!("root-{number}").into_bytes()),
                        ("a/b/file".to_owned(), format!("view-{number}").into_bytes()),
                    ]
                })
                .collect()
        }

        async fn run(batch_size: u64) -> usize {
            let (_temp, _schema, storage, counter) =
                counted_storage_with_history(roots(batch_size), batch_size).await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
            let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
            counter.store(0, Ordering::Relaxed);
            assert_eq!(
                service.catch_up(filter_pk).await.unwrap(),
                CatchUpOutcome::Ready
            );
            assert_eq!(service.test_hooks.refetches.load(Ordering::Relaxed), 0);
            assert!(service.test_hooks.prefetches.load(Ordering::Relaxed) > 0);
            assert_eq!(
                service
                    .test_hooks
                    .advanced_projected_seqs
                    .lock()
                    .unwrap()
                    .as_slice(),
                &[batch_size as i64]
            );
            assert_eq!(
                service.test_hooks.terminal_checks.load(Ordering::Relaxed),
                1
            );
            counter.load(Ordering::Relaxed)
        }

        async fn run_after_clearing_memo(batch_size: u64) -> usize {
            let fixtures = roots(batch_size)
                .iter()
                .map(|paths| root_tree_from_paths(HashKind::Sha1, paths))
                .collect::<Vec<_>>();
            let mut tree_bytes = HashMap::new();
            let mut root_ids = Vec::new();
            let mut nested_ids = Vec::new();
            for fixture in &fixtures {
                root_ids.push(fixture.root.id.to_string());
                for tree in &fixture.trees {
                    let tree_id = tree.id.to_string();
                    if tree_id != fixture.root.id.to_string() {
                        nested_ids.push(tree_id.clone());
                    }
                    tree_bytes.insert(tree_id, tree.to_data().unwrap());
                }
            }
            let source = InMemoryTreeSource::new(HashKind::Sha1, tree_bytes, HashSet::new());
            let (_temp, _schema, storage, counter) =
                counted_storage_with_history(roots(batch_size), batch_size).await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
            let canonical = parse_for_registration(":/a/b").unwrap();
            let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
            service.with_memo(|memo| {
                for root_id in &root_ids {
                    filter_tree(HashKind::Sha1, &source, memo, &canonical.filter, root_id).unwrap();
                }
            });
            service
                .test_hooks
                .clear_memo_before_project
                .store(true, Ordering::Relaxed);
            counter.store(0, Ordering::Relaxed);
            assert_eq!(
                service.catch_up(filter_pk).await.unwrap(),
                CatchUpOutcome::Ready
            );
            assert_eq!(service.test_hooks.refetches.load(Ordering::Relaxed), 0);
            let prefetched = service.test_hooks.prefetched_ids.lock().unwrap();
            assert!(root_ids.iter().all(|id| prefetched.contains(id)));
            assert!(nested_ids.iter().all(|id| !prefetched.contains(id)));
            drop(prefetched);
            assert_eq!(
                service
                    .test_hooks
                    .advanced_projected_seqs
                    .lock()
                    .unwrap()
                    .as_slice(),
                &[batch_size as i64]
            );
            assert_eq!(
                service.test_hooks.terminal_checks.load(Ordering::Relaxed),
                1
            );
            counter.load(Ordering::Relaxed)
        }

        let ten = run(10).await;
        let five_hundred = run(500).await;
        assert!(ten > 0);
        assert_eq!(ten, five_hundred);
        let cleared_ten = run_after_clearing_memo(10).await;
        let cleared_five_hundred = run_after_clearing_memo(500).await;
        assert_eq!(cleared_ten, cleared_five_hundred);
    }

    #[tokio::test]
    async fn stop_state_equals_prefix_reference() {
        for batch_size in [1, 1000] {
            for defect in [
                Hp12Defect::Premise,
                Hp12Defect::RowAbsent,
                Hp12Defect::Unparsable,
                Hp12Defect::CommitRowMissing,
            ] {
                let specs = if matches!(defect, Hp12Defect::Premise | Hp12Defect::CommitRowMissing)
                {
                    vec![":/a/b", ":/a:prefix=x"]
                } else {
                    vec![":/a/b"]
                };
                let (_reference_temp, reference_storage) = hp12_prefix_storage().await;
                let mut reference_filters = Vec::new();
                for (index, spec) in specs.iter().enumerate() {
                    let filter_pk =
                        insert_warming_filter(&reference_storage, index as i64 + 1, spec).await;
                    let service = ViewProjectionService::new(
                        reference_storage.clone(),
                        ViewMetrics::default(),
                    );
                    assert_eq!(
                        hp12_catch_up(&service, filter_pk).await,
                        CatchUpOutcome::Ready
                    );
                    reference_filters.push(filter_pk);
                }
                let reference = projection_snapshot(&reference_storage).await;

                let (_temp, _schema, storage, mega_commit_selects, fixtures, a_tree_id) =
                    hp12_counted_storage_for_defect(batch_size, defect).await;
                let mut filter_pks = Vec::new();
                for (index, spec) in specs.iter().enumerate() {
                    filter_pks.push(insert_warming_filter(&storage, index as i64 + 1, spec).await);
                }
                let commit_id = fixtures[6].commit.id.to_string();
                let _repair = introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
                let metrics = ViewMetrics::default();
                let service = ViewProjectionService::new(storage.clone(), metrics.clone());

                for filter_pk in &filter_pks {
                    assert_hp12_stop(
                        hp12_catch_up(&service, *filter_pk).await,
                        defect,
                        &a_tree_id,
                        &commit_id,
                    );
                }
                assert_hp12_prefix_state(&storage, &filter_pks, &reference).await;
                if specs.len() == 2 {
                    let spine_tree = hp12_prefix_spine_tree(&storage, &fixtures[6]).await;
                    let reference_view_storage = reference_storage.view_storage();
                    assert!(
                        mega_view_object::Entity::find()
                            .filter(mega_view_object::Column::ObjectId.eq(&spine_tree))
                            .one(reference_view_storage.get_connection())
                            .await
                            .unwrap()
                            .is_none()
                    );
                    let view_storage = storage.view_storage();
                    assert!(
                        mega_view_object::Entity::find()
                            .filter(mega_view_object::Column::ObjectId.eq(&spine_tree))
                            .one(view_storage.get_connection())
                            .await
                            .unwrap()
                            .is_none()
                    );
                }
                for filter_pk in &filter_pks {
                    assert!(
                        persisted_commit_map(&storage, *filter_pk)
                            .await
                            .iter()
                            .all(|(seq, _, _)| *seq < 7)
                    );
                    let filter = mega_view_filter::Entity::find_by_id(*filter_pk)
                        .one(storage.view_storage().get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(filter.projected_seq, 6);
                    assert!(filter.ready_seq.is_none());

                    mega_commit_selects.store(0, Ordering::Relaxed);
                    assert_hp12_stop(
                        hp12_catch_up(&service, *filter_pk).await,
                        defect,
                        &a_tree_id,
                        &commit_id,
                    );
                    assert_eq!(mega_commit_selects.load(Ordering::Relaxed), 1);
                    assert_hp12_stop(
                        hp12_catch_up_one_batch(&service, *filter_pk, &storage.config(), 1).await,
                        defect,
                        &a_tree_id,
                        &commit_id,
                    );
                }
                assert_hp12_prefix_state(&storage, &filter_pks, &reference).await;

                reset_projection_state(&storage).await;
                for filter_pk in &filter_pks {
                    assert_hp12_stop(
                        hp12_catch_up(&service, *filter_pk).await,
                        defect,
                        &a_tree_id,
                        &commit_id,
                    );
                }
                assert_hp12_prefix_state(&storage, &filter_pks, &reference).await;

                reset_projection_state(&storage).await;
                for filter_pk in &filter_pks {
                    assert_hp12_stop(
                        hp12_catch_up_one_batch(&service, *filter_pk, &storage.config(), 1000)
                            .await,
                        defect,
                        &a_tree_id,
                        &commit_id,
                    );
                }
                assert_hp12_prefix_state(&storage, &filter_pks, &reference).await;
                assert_eq!(filter_pks.len(), reference_filters.len());
            }
        }
    }

    #[test]
    fn stop_alert_and_counter() {
        let captured = capture_tracing_with_reader(|logs| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let metrics = ViewMetrics::default();
                    for defect in [
                        Hp12Defect::Premise,
                        Hp12Defect::RowAbsent,
                        Hp12Defect::Unparsable,
                        Hp12Defect::CommitRowMissing,
                    ] {
                        let (_temp, storage, fixtures, a_tree_id) =
                            hp12_storage_for_defect(1000, defect).await;
                        let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
                        let filter_id = mega_view_filter::Entity::find_by_id(filter_pk)
                            .one(storage.view_storage().get_connection())
                            .await
                            .unwrap()
                            .unwrap()
                            .filter_id;
                        let commit_id = fixtures[6].commit.id.to_string();
                        let _repair =
                            introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
                        let service = ViewProjectionService::new(storage.clone(), metrics.clone());

                        for _ in 0..2 {
                            let offset = captured_log_offset(logs);
                            assert_hp12_stop(
                                hp12_catch_up(&service, filter_pk).await,
                                defect,
                                &a_tree_id,
                                &commit_id,
                            );
                            let new_logs = captured_log_since(logs, offset);
                            let events = new_logs
                                .lines()
                                .filter(|line| line.contains("view projection stopped"))
                                .collect::<Vec<_>>();
                            assert_eq!(events.len(), 1, "{new_logs}");
                            let event = events[0];
                            assert!(event.contains("ERROR"), "{event}");
                            assert!(!new_logs.contains("hp12-sentinel-author"), "{new_logs}");
                            assert!(!new_logs.contains("hp12-sentinel-message"), "{new_logs}");
                            let fields = event
                                .split("mega2_core::jupiter::service::view_projection_service: ")
                                .nth(1)
                                .expect("stop event target");
                            let mut expected =
                                BTreeSet::from(["commit_id", "filter_id", "metric", "reason", "s"]);
                            if matches!(defect, Hp12Defect::RowAbsent | Hp12Defect::Unparsable) {
                                expected.insert("tree_id");
                                assert!(fields.contains(&format!("tree_id={a_tree_id}")));
                            }
                            assert_eq!(event_field_keys(fields), expected, "{event}");
                            assert!(
                                fields.contains("metric=view_projection_stops_total"),
                                "event={event}; fields={fields}"
                            );
                            assert!(fields.contains(&format!("filter_id={filter_id}")));
                            assert!(fields.contains("s=7"));
                            assert!(fields.contains(&format!("commit_id={commit_id}")));
                            assert!(fields.contains(&format!("reason={}", defect.alert_reason())));
                        }
                    }
                    assert_eq!(metrics.counters().view_projection_stops_total, 8);

                    let (_temp, storage, fixtures, a_tree_id) =
                        hp12_storage_with_history(1000).await;
                    let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
                    let commit_id = fixtures[6].commit.id.to_string();
                    let _repair = introduce_hp12_defect(
                        &storage,
                        &fixtures,
                        &a_tree_id,
                        Hp12Defect::RowAbsent,
                    )
                    .await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let offset = captured_log_offset(logs);
                    assert_hp12_stop(
                        hp12_catch_up_one_batch(&service, filter_pk, &storage.config(), 64).await,
                        Hp12Defect::RowAbsent,
                        &a_tree_id,
                        &commit_id,
                    );
                    assert_eq!(metrics.counters().view_projection_stops_total, 9);
                    assert_eq!(
                        captured_log_since(logs, offset)
                            .lines()
                            .filter(|line| line.contains("view projection stopped"))
                            .count(),
                        1
                    );

                    let (_temp, storage, _fixtures, _a_tree_id) =
                        hp12_storage_with_history(1000).await;
                    let filter_pk = insert_warming_filter(&storage, 1, ":/c").await;
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let offset = captured_log_offset(logs);
                    assert_eq!(
                        hp12_catch_up(&service, filter_pk).await,
                        CatchUpOutcome::Ready
                    );
                    assert_eq!(metrics.counters().view_projection_stops_total, 9);
                    assert!(!captured_log_since(logs, offset).contains("view projection stopped"));

                    let (_temp, storage, _fixtures, _a_tree_id) =
                        hp12_storage_with_history(1000).await;
                    let filter_pk = insert_warming_filter(&storage, 1, ":/c").await;
                    storage
                        .view_storage()
                        .get_connection()
                        .execute_unprepared("DELETE FROM mega_view_root_chain WHERE seq = 1")
                        .await
                        .unwrap();
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let offset = captured_log_offset(logs);
                    assert_eq!(
                        hp12_catch_up(&service, filter_pk).await,
                        CatchUpOutcome::BatchPremiseFailed
                    );
                    assert_eq!(metrics.counters().view_projection_stops_total, 9);
                    assert!(!captured_log_since(logs, offset).contains("view projection stopped"));

                    let (_temp, storage, _fixtures, _a_tree_id) =
                        hp12_storage_with_history(1000).await;
                    let filter_pk = insert_warming_filter(&storage, 1, ":/c").await;
                    let view_storage = storage.view_storage();
                    let db = view_storage.get_connection();
                    db.execute_unprepared(
                        "ALTER TABLE mega_view_object_ref RENAME COLUMN object_id TO hp12_gone",
                    )
                    .await
                    .unwrap();
                    let service = ViewProjectionService::new(storage.clone(), metrics.clone());
                    let offset = captured_log_offset(logs);
                    assert!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            service.catch_up(filter_pk),
                        )
                        .await
                        .expect("HP-12 query failure timeout")
                        .is_err()
                    );
                    assert_eq!(metrics.counters().view_projection_stops_total, 9);
                    assert!(!captured_log_since(logs, offset).contains("view projection stopped"));
                    assert_eq!(
                        mega_view_filter::Entity::find_by_id(filter_pk)
                            .one(db)
                            .await
                            .unwrap()
                            .unwrap()
                            .projected_seq,
                        0
                    );
                    db.execute_unprepared(
                        "ALTER TABLE mega_view_object_ref RENAME COLUMN hp12_gone TO object_id",
                    )
                    .await
                    .unwrap();
                });
        });
        assert!(!captured.is_empty());
    }

    #[tokio::test]
    async fn non_stop_inputs_reach_ready() {
        let metrics = ViewMetrics::default();
        for defect in [
            Hp12Defect::Premise,
            Hp12Defect::RowAbsent,
            Hp12Defect::Unparsable,
            Hp12Defect::CommitRowMissing,
        ] {
            let (_temp, storage, fixtures, a_tree_id) = hp12_storage_for_defect(1000, defect).await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/c").await;
            let _repair = introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
            let service = ViewProjectionService::new(storage.clone(), metrics.clone());
            assert_eq!(
                hp12_catch_up(&service, filter_pk).await,
                CatchUpOutcome::Ready
            );
            assert_eq!(
                mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap()
                    .ready_seq,
                Some(12)
            );
        }
        for defect in [Hp12Defect::RowAbsent, Hp12Defect::Unparsable] {
            let (_temp, storage, fixtures, a_tree_id) = hp12_storage_for_defect(1000, defect).await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/a:prefix=x").await;
            let _repair = introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
            let service = ViewProjectionService::new(storage.clone(), metrics.clone());
            assert_eq!(
                hp12_catch_up(&service, filter_pk).await,
                CatchUpOutcome::Ready
            );
            assert_eq!(
                mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap()
                    .ready_seq,
                Some(12)
            );
        }
        for roots in [
            vec![
                ("README".to_owned(), b"readme".to_vec()),
                ("c/f".to_owned(), b"c".to_vec()),
            ],
            vec![
                ("README".to_owned(), b"readme".to_vec()),
                ("a".to_owned(), b"not-a-tree".to_vec()),
                ("c/f".to_owned(), b"c".to_vec()),
            ],
        ] {
            let roots = vec![roots.clone(), roots.clone(), roots];
            let (_temp, storage, _fixture_metrics, filter_pk) =
                service_with_history(":/a/b", roots, 1000).await;
            let service = ViewProjectionService::new(storage.clone(), metrics.clone());
            assert_eq!(
                hp12_catch_up(&service, filter_pk).await,
                CatchUpOutcome::Ready
            );
            assert_eq!(
                mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap()
                    .ready_seq,
                Some(3)
            );
            assert_eq!(
                persisted_commit_map(&storage, filter_pk)
                    .await
                    .last()
                    .unwrap()
                    .2,
                empty_tree_id(HashKind::Sha1).unwrap().to_string()
            );
        }
        assert_eq!(metrics.counters().view_projection_stops_total, 0);
    }

    #[test]
    fn stop_dispatch_excludes_not_prefetched() {
        let stop_cases = [
            (
                ProjectError::Premise(RewriteError::PremiseMismatch {
                    commit_id: "p".to_owned(),
                }),
                ViewProjectionStopReason::PremiseCheckFailed,
            ),
            (
                ProjectError::MissingObject(MissingObject {
                    tree_id: "absent".to_owned(),
                    reason: MissingObjectReason::Absent,
                }),
                ViewProjectionStopReason::RowAbsent {
                    tree_id: "absent".to_owned(),
                },
            ),
            (
                ProjectError::MissingObject(MissingObject {
                    tree_id: "malformed".to_owned(),
                    reason: MissingObjectReason::Malformed,
                }),
                ViewProjectionStopReason::Unparsable {
                    tree_id: "malformed".to_owned(),
                },
            ),
            (
                ProjectError::MissingCommit {
                    commit_id: "missing".to_owned(),
                },
                ViewProjectionStopReason::CommitRowMissing,
            ),
        ];
        for (error, expected) in stop_cases {
            assert_eq!(stop_dispatch(&error), StopDispatch::Stop(expected));
        }
        assert_eq!(
            stop_dispatch(&ProjectError::MissingObject(MissingObject {
                tree_id: "unprefetched".to_owned(),
                reason: MissingObjectReason::Unprefetched,
            })),
            StopDispatch::Internal
        );
    }

    #[tokio::test]
    async fn stop_resumes_after_repair() {
        for defect in [
            Hp12Defect::Premise,
            Hp12Defect::RowAbsent,
            Hp12Defect::Unparsable,
            Hp12Defect::CommitRowMissing,
        ] {
            let (_reference_temp, reference_storage, reference_fixtures, reference_a_tree_id) =
                hp12_storage_for_defect(1000, defect).await;
            if matches!(defect, Hp12Defect::Premise) {
                let repair = introduce_hp12_defect(
                    &reference_storage,
                    &reference_fixtures,
                    &reference_a_tree_id,
                    defect,
                )
                .await;
                repair_hp12_defect(&reference_storage, repair).await;
            }
            let reference_filter = insert_warming_filter(&reference_storage, 1, ":/a/b").await;
            let reference_service =
                ViewProjectionService::new(reference_storage.clone(), ViewMetrics::default());
            assert_eq!(
                hp12_catch_up(&reference_service, reference_filter).await,
                CatchUpOutcome::Ready
            );
            let reference = projection_snapshot(&reference_storage).await;

            let (_temp, storage, fixtures, a_tree_id) = hp12_storage_for_defect(1000, defect).await;
            let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
            let commit_id = fixtures[6].commit.id.to_string();
            let repair = introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
            let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
            assert_hp12_stop(
                hp12_catch_up(&service, filter_pk).await,
                defect,
                &a_tree_id,
                &commit_id,
            );
            repair_hp12_defect(&storage, repair).await;
            assert_eq!(
                hp12_catch_up(&service, filter_pk).await,
                CatchUpOutcome::Ready
            );
            assert_eq!(
                mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap()
                    .ready_seq,
                Some(12)
            );
            assert_eq!(projection_snapshot(&storage).await, reference);
        }
    }

    #[tokio::test]
    async fn stop_changes_only_projected_seq() {
        for defect in [
            Hp12Defect::Premise,
            Hp12Defect::RowAbsent,
            Hp12Defect::Unparsable,
            Hp12Defect::CommitRowMissing,
        ] {
            for ready_before_stop in [false, true] {
                let (_temp, storage, fixtures, a_tree_id) =
                    hp12_storage_for_defect(1000, defect).await;
                let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
                let service = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
                if ready_before_stop {
                    let view_storage = storage.view_storage();
                    let db = view_storage.get_connection();
                    db.execute_unprepared("DELETE FROM mega_view_root_chain WHERE seq > 6")
                        .await
                        .unwrap();
                    mega_refs::Entity::update_many()
                        .col_expr(
                            mega_refs::Column::RefCommitHash,
                            sea_orm::sea_query::Expr::value(fixtures[5].commit.id.to_string()),
                        )
                        .col_expr(
                            mega_refs::Column::RefTreeHash,
                            sea_orm::sea_query::Expr::value(fixtures[5].commit.tree_id.to_string()),
                        )
                        .filter(mega_refs::Column::Path.eq("/"))
                        .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
                        .exec(db)
                        .await
                        .unwrap();
                    assert_eq!(
                        hp12_catch_up(&service, filter_pk).await,
                        CatchUpOutcome::Ready
                    );
                    mega_refs::Entity::update_many()
                        .col_expr(
                            mega_refs::Column::RefCommitHash,
                            sea_orm::sea_query::Expr::value(fixtures[11].commit.id.to_string()),
                        )
                        .col_expr(
                            mega_refs::Column::RefTreeHash,
                            sea_orm::sea_query::Expr::value(
                                fixtures[11].commit.tree_id.to_string(),
                            ),
                        )
                        .filter(mega_refs::Column::Path.eq("/"))
                        .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
                        .exec(db)
                        .await
                        .unwrap();
                    view_storage
                        .extend_root_chain(None, 1000, ViewLockMode::Try)
                        .await
                        .unwrap();
                }
                let commit_id = fixtures[6].commit.id.to_string();
                let _repair = introduce_hp12_defect(&storage, &fixtures, &a_tree_id, defect).await;
                let before = mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                assert_hp12_stop(
                    hp12_catch_up(&service, filter_pk).await,
                    defect,
                    &a_tree_id,
                    &commit_id,
                );
                let after = mega_view_filter::Entity::find_by_id(filter_pk)
                    .one(storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                let mut comparable = before;
                comparable.projected_seq = after.projected_seq;
                assert_eq!(comparable, after);
                assert_eq!(after.projected_seq, 6);
            }
        }
    }

    #[tokio::test]
    async fn cold_start_slots_gauge() {
        let (_temp, storage, fixtures, a_tree_id) = hp12_storage_with_history(1000).await;
        let metrics = ViewMetrics::default();
        let service = ViewProjectionService::new(storage.clone(), metrics.clone());
        let snapshot = assert_snapshot_warming_count(&service, &storage, 0).await;
        let json = serde_json::to_value(&snapshot).unwrap();
        for key in [
            "view_batch_premise_failures_total",
            "view_projection_stops_total",
            "view_cold_start_slots_in_use",
        ] {
            assert!(
                json.get(key).is_some_and(serde_json::Value::is_u64),
                "{json}"
            );
        }

        let stopped_filter = insert_warming_filter(&storage, 1, ":/a/b").await;
        let ready_filter = insert_warming_filter(&storage, 2, ":/c").await;
        assert_snapshot_warming_count(&service, &storage, 2).await;
        let commit_id = fixtures[6].commit.id.to_string();
        let _repair =
            introduce_hp12_defect(&storage, &fixtures, &a_tree_id, Hp12Defect::RowAbsent).await;
        assert_hp12_stop(
            hp12_catch_up(&service, stopped_filter).await,
            Hp12Defect::RowAbsent,
            &a_tree_id,
            &commit_id,
        );
        assert_snapshot_warming_count(&service, &storage, 2).await;
        assert_eq!(
            hp12_catch_up(&service, ready_filter).await,
            CatchUpOutcome::Ready
        );
        let snapshot = assert_snapshot_warming_count(&service, &storage, 1).await;
        assert_eq!(snapshot.counters.view_projection_stops_total, 1);
        assert_eq!(snapshot.counters.view_batch_premise_failures_total, 0);
        assert_eq!(snapshot.view_cold_start_slots_in_use, 1);

        let complete_filter = insert_warming_filter(&storage, 3, ":/c:prefix=x").await;
        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::ReadySeq,
                sea_orm::sea_query::Expr::value(Some(12_i64)),
            )
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(sea_orm::Value::ChronoDateTime(None)),
            )
            .filter(mega_view_filter::Column::Id.eq(complete_filter))
            .exec(storage.view_storage().get_connection())
            .await
            .unwrap();
        assert_snapshot_warming_count(&service, &storage, 1).await;

        let (_temp, storage, fixtures, a_tree_id) =
            hp12_storage_for_defect(1000, Hp12Defect::Premise).await;
        let metrics = ViewMetrics::default();
        let service = ViewProjectionService::new(storage.clone(), metrics);
        let filter_pk = insert_warming_filter(&storage, 1, ":/a/b").await;
        assert_snapshot_warming_count(&service, &storage, 1).await;
        let commit_id = fixtures[6].commit.id.to_string();
        let _repair =
            introduce_hp12_defect(&storage, &fixtures, &a_tree_id, Hp12Defect::Premise).await;
        assert_hp12_stop(
            hp12_catch_up(&service, filter_pk).await,
            Hp12Defect::Premise,
            &a_tree_id,
            &commit_id,
        );
        assert_snapshot_warming_count(&service, &storage, 1).await;
    }
}
