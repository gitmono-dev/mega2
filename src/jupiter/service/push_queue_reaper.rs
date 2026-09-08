//! TP-05: MonoWriteQueue reaper (trunk-push.md §1.6).
//!
//! Try-locks `MONO_WRITE_LOCK` so reap is mutex with B3 (no TOCTOU). Running
//! rows are cleaned immediately; `stuck_timeout` is an alarm only.

use std::time::Duration;

use sea_orm::TransactionTrait;
use tokio_util::sync::CancellationToken;

use crate::{
    callisto::{
        push_queue,
        sea_orm_active_enums::{PushQueueKindEnum, PushQueuePendingEnum},
    },
    common::{
        errors::MegaError,
        utils::{MEGA_BRANCH_NAME, ZERO_ID},
    },
    jupiter::{
        service::push_queue_service::PushQueueService,
        storage::{
            base_storage::StorageConnector,
            mono_storage::MonoStorage,
            push_queue_storage::{MonoWriteLockHolder, PushQueueStorage},
        },
    },
};

/// Default `started_at` grace: skip a just-claimed row one cycle (observability).
pub const DEFAULT_STARTED_AT_GRACE: Duration = Duration::from_secs(2);
/// Default heartbeat orphan timeout (must be ≫ poll, ≪ wait).
pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
/// Alarm threshold only — does not terminalize a live B3.
pub const DEFAULT_STUCK_TIMEOUT: Duration = Duration::from_secs(60);
/// Background reap interval (independent of stuck_timeout).
pub const DEFAULT_REAP_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub lock_acquired: bool,
    pub skipped_started_at_grace: u64,
    pub running_failed: u64,
    pub running_reset_queued: u64,
    pub conflict_requeued: u64,
    pub stale_queued_cancelled: u64,
    pub bypass_detected: u64,
    pub i3_tombstoned: u64,
    pub stuck_alarm: bool,
    pub lock_holders: Vec<MonoWriteLockHolder>,
}

#[derive(Clone)]
pub struct PushQueueReaper {
    service: PushQueueService,
    heartbeat_timeout: Duration,
    started_at_grace: Duration,
    stuck_timeout: Duration,
    watchdog_auto: bool,
}

impl PushQueueReaper {
    pub fn from_service(service: PushQueueService) -> Self {
        Self {
            service,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            started_at_grace: DEFAULT_STARTED_AT_GRACE,
            stuck_timeout: DEFAULT_STUCK_TIMEOUT,
            watchdog_auto: false,
        }
    }

    pub fn with_timeouts(
        mut self,
        heartbeat_timeout: Duration,
        started_at_grace: Duration,
        stuck_timeout: Duration,
    ) -> Self {
        self.heartbeat_timeout = heartbeat_timeout;
        self.started_at_grace = started_at_grace;
        self.stuck_timeout = stuck_timeout;
        self
    }

    pub fn with_watchdog_auto(mut self, enabled: bool) -> Self {
        self.watchdog_auto = enabled;
        self
    }

    fn storage(&self) -> &PushQueueStorage {
        self.service.storage()
    }

    fn mono(&self) -> &MonoStorage {
        self.service.mono_storage_for_reaper()
    }

    /// One reap cycle. Non-blocking try-lock; heartbeat cleanup still runs
    /// when the lock is held unless `hard_stopped`.
    pub async fn reap_once(&self) -> Result<ReapReport, MegaError> {
        let conn = self.storage().get_connection();
        let txn = conn.begin().await?;
        let mut report = ReapReport {
            lock_acquired: PushQueueStorage::try_mono_write_lock(&txn).await?,
            ..ReapReport::default()
        };

        let hard_stopped_at_start = PushQueueStorage::is_hard_stopped_in_txn(&txn).await?;

        if !hard_stopped_at_start {
            let cutoff = chrono::Utc::now().fixed_offset()
                - chrono::Duration::from_std(self.heartbeat_timeout)
                    .unwrap_or(chrono::Duration::seconds(15));
            report.stale_queued_cancelled =
                PushQueueStorage::cancel_stale_queued_in_txn(&txn, cutoff).await?;
        }

        if !report.lock_acquired {
            let running = PushQueueStorage::list_running_in_txn(&txn).await?;
            if !running.is_empty() {
                report.lock_holders = PushQueueStorage::list_mono_write_lock_holders(&txn).await?;
                let stuck = chrono::Duration::from_std(self.stuck_timeout)
                    .unwrap_or(chrono::Duration::seconds(60));
                let now = chrono::Utc::now().fixed_offset();
                report.stuck_alarm = report
                    .lock_holders
                    .iter()
                    .any(|h| h.query_start.map(|t| now - t > stuck).unwrap_or(false))
                    || running
                        .iter()
                        .any(|row| row.started_at.map(|t| now - t > stuck).unwrap_or(false));
                if report.stuck_alarm {
                    self.service
                        .metrics()
                        .lock_timeout_alarms
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        event = "mono_write_lock_stuck",
                        holders = ?report.lock_holders,
                        running = running.len(),
                        watchdog_auto = self.watchdog_auto,
                        "MONO_WRITE_LOCK held past stuck_timeout with Running rows"
                    );
                }
            }
            txn.commit().await?;
            return Ok(report);
        }

        let running = PushQueueStorage::list_running_in_txn(&txn).await?;
        let grace_cutoff = chrono::Utc::now().fixed_offset()
            - chrono::Duration::from_std(self.started_at_grace)
                .unwrap_or(chrono::Duration::seconds(2));

        for row in running {
            if let Some(started) = row.started_at
                && started > grace_cutoff
            {
                report.skipped_started_at_grace += 1;
                continue;
            }
            let (kind, tombstoned) = self
                .reap_running_row(&txn, &row, hard_stopped_at_start)
                .await?;
            if tombstoned {
                report.i3_tombstoned += 1;
            }
            match kind {
                RunningReap::FailedBypass => {
                    report.bypass_detected += 1;
                    report.running_failed += 1;
                }
                RunningReap::FailedOrdinary => report.running_failed += 1,
                RunningReap::ResetQueued => report.running_reset_queued += 1,
                RunningReap::ConflictRequeued => report.conflict_requeued += 1,
                RunningReap::TombstonedThenFailed => report.running_failed += 1,
            }
        }

        PushQueueStorage::notify_mono_write_queue(&txn).await?;
        txn.commit().await?;
        Ok(report)
    }

    /// Periodic background loop. Errors are logged; the loop continues until
    /// `shutdown` is cancelled (same token as notification workers in `AppContext`).
    pub fn spawn_background(self, interval: Duration, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if let Err(e) = self.reap_once().await {
                            tracing::error!(error = %e, "push_queue reaper cycle failed");
                        }
                    }
                }
            }
        });
    }

    async fn reap_running_row(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        hard_stopped_at_start: bool,
    ) -> Result<(RunningReap, bool), MegaError> {
        let root = self.mono().get_main_ref_in_txn("/", txn).await?;
        let (cur_commit, cur_tree) = match &root {
            Some(r) => (
                Some(r.ref_commit_hash.as_str()),
                Some(r.ref_tree_hash.as_str()),
            ),
            None => (None, None),
        };
        let baseline_ok = cur_commit == row.expected_commit_hash.as_deref()
            && cur_tree == row.expected_tree_hash.as_deref();

        let i3 = self.apply_i3(txn, row, root.as_ref()).await?;

        if !baseline_ok {
            // Bypass wins: drop any requeue_conflict intent.
            PushQueueStorage::set_hard_stopped_in_txn(txn, true).await?;
            PushQueueStorage::mark_failed_if_running_in_txn(
                txn,
                row.id,
                "QueueBypassDetected",
                "reaper expected_* baseline mismatch",
            )
            .await?;
            return Ok((RunningReap::FailedBypass, i3));
        }

        if row.pending_action == Some(PushQueuePendingEnum::RequeueConflict) {
            PushQueueStorage::complete_conflict_requeue_in_txn(txn, row).await?;
            return Ok((RunningReap::ConflictRequeued, i3));
        }

        if hard_stopped_at_start {
            PushQueueStorage::reset_running_to_queued_in_txn(txn, row.id).await?;
            return Ok((RunningReap::ResetQueued, i3));
        }

        let msg = if i3 {
            "I3 tombstone repair after Running crash"
        } else {
            "reaper terminalized Running after crash"
        };
        PushQueueStorage::mark_failed_if_running_in_txn(txn, row.id, "SystemError", msg).await?;
        Ok(if i3 {
            (RunningReap::TombstonedThenFailed, true)
        } else {
            (RunningReap::FailedOrdinary, false)
        })
    }

    /// Kind-specific I3. Returns true when a tombstone was written.
    async fn apply_i3(
        &self,
        txn: &sea_orm::DatabaseTransaction,
        row: &push_queue::Model,
        root: Option<&crate::callisto::mega_refs::Model>,
    ) -> Result<bool, MegaError> {
        match row.kind {
            PushQueueKindEnum::Attach => {
                if row.path == "/" {
                    return Ok(false);
                }
                let path_ref = self.mono().get_main_ref_in_txn(&row.path, txn).await?;
                if path_ref.is_none() {
                    return Ok(false);
                }
                tracing::warn!(
                    id = row.id,
                    path = %row.path,
                    "reaper I3: attach path has a main ref (unexpected); tombstoning"
                );
                self.mono()
                    .tombstone_and_delete_main_ref_in_txn(&row.path, txn)
                    .await?;
                Ok(true)
            }
            PushQueueKindEnum::Push | PushQueueKindEnum::Merge => {
                let path_ref = self.mono().get_main_ref_in_txn(&row.path, txn).await?;
                match path_ref {
                    None => {
                        if row.kind == PushQueueKindEnum::Merge {
                            tracing::warn!(
                                id = row.id,
                                path = %row.path,
                                "reaper I3: merge row missing main@path (no repair)"
                            );
                            return Ok(false);
                        }
                        let tomb = self
                            .mono()
                            .get_tombstone_in_txn(&row.path, MEGA_BRANCH_NAME, txn)
                            .await?;
                        if tomb.is_some() {
                            tracing::warn!(
                                id = row.id,
                                path = %row.path,
                                "reaper I3: push missing main@path but tombstone exists"
                            );
                        } else if row.old_id != ZERO_ID {
                            tracing::warn!(
                                id = row.id,
                                path = %row.path,
                                "reaper I3: push missing main@path with non-ZERO old_id (no tombstone)"
                            );
                        } else {
                            tracing::debug!(
                                id = row.id,
                                path = %row.path,
                                "reaper I3: push missing main@path without tombstone (create mid-crash)"
                            );
                        }
                        Ok(false)
                    }
                    Some(pref) => {
                        let Some(root) = root else {
                            return Ok(false);
                        };
                        let resolved = self
                            .mono()
                            .resolve_path_tree_hash_in_txn(&root.ref_tree_hash, &row.path, txn)
                            .await?;
                        let stale = resolved.as_deref() != Some(pref.ref_tree_hash.as_str());
                        if !stale {
                            return Ok(false);
                        }
                        self.mono()
                            .tombstone_and_delete_main_ref_in_txn(&row.path, txn)
                            .await?;
                        Ok(true)
                    }
                }
            }
        }
    }
}

enum RunningReap {
    FailedBypass,
    FailedOrdinary,
    ResetQueued,
    ConflictRequeued,
    TombstonedThenFailed,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
    use serde_json::json;
    use tokio::sync::{Mutex, MutexGuard};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        callisto::{
            mega_refs,
            sea_orm_active_enums::{PushQueueFailureEnum, PushQueueKindEnum, PushQueueStatusEnum},
        },
        config::PushPolicy,
        jupiter::{
            migration::apply_migrations,
            service::push_queue_service::{
                EnqueueRequest, ExecuteOutcome, ExecuteRequest, push_operation_id,
            },
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                push_queue_storage::{ClaimOutcome, EnqueueOutcome, MONO_WRITE_LOCK_SQL},
            },
            tests::test_db_connection,
        },
    };

    /// Advisory locks are database-wide (not per test schema). Serialize this
    /// module so parallel tests do not steal `MONO_WRITE_LOCK` from each other.
    static TEST_LOCK: Mutex<()> = Mutex::const_new(());

    async fn service() -> (tempfile::TempDir, PushQueueService, MutexGuard<'static, ()>) {
        let lock = TEST_LOCK.lock().await;
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(std::sync::Arc::new(db));
        let svc = PushQueueService::new(base, PushPolicy::Trunk)
            .with_timeouts(Duration::from_millis(400), Duration::from_millis(20));
        (temp, svc, lock)
    }

    async fn mutate_root(
        svc: &PushQueueService,
        old_c: &str,
        old_t: &str,
        new_c: &str,
        new_t: &str,
    ) {
        let conn = svc.mono_storage().get_connection();
        let txn = conn.begin().await.unwrap();
        assert!(
            svc.mono_storage()
                .cas_update_root_main_ref_in_txn(&txn, Some(old_c), Some(old_t), new_c, new_t)
                .await
                .unwrap()
        );
        txn.commit().await.unwrap();
    }

    async fn seed_root(svc: &PushQueueService, commit: &str, tree: &str) {
        let model = mega_refs::Model::new(
            "/",
            MEGA_BRANCH_NAME.to_owned(),
            commit.to_owned(),
            tree.to_owned(),
            false,
        );
        svc.mono_storage().save_refs(model, None).await.unwrap();
    }

    async fn enqueue_merge(svc: &PushQueueService, op: &str) -> i64 {
        let outcome = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: op.into(),
                path: format!("/{op}"),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("insert {op}");
        };
        id
    }

    async fn enqueue_and_claim(svc: &PushQueueService, op: &str) -> i64 {
        let id = enqueue_merge(svc, op).await;
        assert_eq!(
            svc.storage().claim_for_execution(id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        id
    }

    fn reaper(svc: &PushQueueService) -> PushQueueReaper {
        svc.reaper().with_timeouts(
            Duration::from_millis(80),
            Duration::from_millis(0),
            Duration::from_millis(50),
        )
    }

    #[tokio::test]
    async fn tp05_kill9_running_reaped_immediately() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-K9").await;
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.lock_acquired);
        assert!(report.running_failed >= 1);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(row.failure_type, Some(PushQueueFailureEnum::SystemError));
        let next = enqueue_and_claim(&svc, "CL-RP-NEXT").await;
        let outcome = svc
            .execute_b3(
                ExecuteRequest {
                    id: next,
                    ..Default::default()
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
    }

    #[tokio::test]
    async fn tp05_stale_queued_heartbeat_cancelled() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_merge(&svc, "CL-RP-HB").await;
        let conn = svc.storage().get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE push_queue SET heartbeat_at = now() - interval '1 hour' WHERE id = $1",
            [sea_orm::Value::from(id)],
        ))
        .await
        .unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.stale_queued_cancelled >= 1);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Cancelled);
        let next = enqueue_and_claim(&svc, "CL-RP-HB2").await;
        assert_ne!(next, id);
    }

    #[tokio::test]
    async fn tp05_baseline_mismatch_hard_stops_and_drops_requeue_intent() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-BY").await;
        svc.storage()
            .persist_requeue_conflict_intent(id)
            .await
            .unwrap();
        mutate_root(
            &svc,
            &"a".repeat(40),
            &"b".repeat(40),
            &"c".repeat(40),
            &"d".repeat(40),
        )
        .await;
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.bypass_detected >= 1);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(
            row.failure_type,
            Some(PushQueueFailureEnum::QueueBypassDetected)
        );
        assert!(row.pending_action.is_none());
        assert!(row.superseded_by.is_none());
        let conn = svc.storage().get_connection();
        let txn = conn.begin().await.unwrap();
        assert!(
            PushQueueStorage::is_hard_stopped_in_txn(&txn)
                .await
                .unwrap()
        );
        txn.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn tp05_conflict_intent_completed_when_baseline_matches() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-RQ").await;
        svc.storage()
            .persist_requeue_conflict_intent(id)
            .await
            .unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.conflict_requeued >= 1);
        let old = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(old.status, PushQueueStatusEnum::Cancelled);
        assert_eq!(old.failure_type, Some(PushQueueFailureEnum::Conflict));
        assert!(old.superseded_by.is_some());
        let succ = svc
            .storage()
            .get_by_id(old.superseded_by.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(succ.status, PushQueueStatusEnum::Queued);
    }

    #[tokio::test]
    async fn tp05_lock_held_skips_running_then_row_can_complete() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-SLOW").await;
        let conn = svc.storage().get_connection();
        let holder = conn.begin().await.unwrap();
        holder
            .execute_raw(Statement::from_string(
                DbBackend::Postgres,
                MONO_WRITE_LOCK_SQL.to_owned(),
            ))
            .await
            .unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(!report.lock_acquired);
        assert_eq!(report.running_failed, 0);
        let still = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(still.status, PushQueueStatusEnum::Running);
        holder.rollback().await.unwrap();
        let outcome = svc
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        let done = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(done.status, PushQueueStatusEnum::Done);
        let after = reaper(&svc).reap_once().await.unwrap();
        assert!(after.lock_acquired);
        let still_done = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(still_done.status, PushQueueStatusEnum::Done);
    }

    async fn force_running(svc: &PushQueueService, id: i64, commit: &str, tree: &str) {
        let conn = svc.storage().get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE push_queue
               SET status = 'Running'::push_queue_status_enum,
                   started_at = now() - interval '1 minute',
                   expected_commit_hash = $2,
                   expected_tree_hash = $3,
                   updated_at = now()
             WHERE id = $1
            "#,
            [
                sea_orm::Value::from(id),
                sea_orm::Value::from(commit.to_owned()),
                sea_orm::Value::from(tree.to_owned()),
            ],
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tp05_i3_push_stale_tombstones_merge_missing_alarms_attach_skips() {
        let (_t, svc, _lock) = service().await;
        let commit = "a".repeat(40);
        let tree = "b".repeat(40);
        seed_root(&svc, &commit, &tree).await;

        let push_id = {
            let outcome = svc
                .enqueue(EnqueueRequest {
                    kind: PushQueueKindEnum::Push,
                    operation_id: push_operation_id(ZERO_ID, &"c".repeat(40)),
                    path: "/stale-p".into(),
                    old_id: ZERO_ID.into(),
                    new_id: "c".repeat(40),
                    requester: None,
                    payload: json!({}),
                    ref_name: Some(MEGA_BRANCH_NAME.into()),
                    is_delete: false,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Inserted { id } = outcome else {
                panic!("push insert");
            };
            svc.mono_storage()
                .save_refs(
                    mega_refs::Model::new(
                        "/stale-p",
                        MEGA_BRANCH_NAME.to_owned(),
                        "s".repeat(40),
                        "t".repeat(40),
                        false,
                    ),
                    None,
                )
                .await
                .unwrap();
            id
        };
        let merge_id = enqueue_merge(&svc, "CL-RP-MM").await;
        let attach_skip_id = {
            let outcome = svc
                .enqueue(EnqueueRequest {
                    kind: PushQueueKindEnum::Attach,
                    operation_id: "att-1".into(),
                    path: "/third-party/x".into(),
                    old_id: "a".repeat(40),
                    new_id: "n".repeat(40),
                    requester: None,
                    payload: json!({}),
                    ref_name: None,
                    is_delete: false,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Inserted { id } = outcome else {
                panic!("attach skip");
            };
            id
        };
        let attach_tomb_id = {
            let outcome = svc
                .enqueue(EnqueueRequest {
                    kind: PushQueueKindEnum::Attach,
                    operation_id: "att-2".into(),
                    path: "/third-party/y".into(),
                    old_id: "a".repeat(40),
                    new_id: "m".repeat(40),
                    requester: None,
                    payload: json!({}),
                    ref_name: None,
                    is_delete: false,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Inserted { id } = outcome else {
                panic!("attach tomb");
            };
            svc.mono_storage()
                .save_refs(
                    mega_refs::Model::new(
                        "/third-party/y",
                        MEGA_BRANCH_NAME.to_owned(),
                        "u".repeat(40),
                        "v".repeat(40),
                        false,
                    ),
                    None,
                )
                .await
                .unwrap();
            id
        };
        force_running(&svc, push_id, &commit, &tree).await;
        force_running(&svc, merge_id, &commit, &tree).await;
        force_running(&svc, attach_skip_id, &commit, &tree).await;
        force_running(&svc, attach_tomb_id, &commit, &tree).await;

        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.lock_acquired);
        assert!(
            svc.mono_storage()
                .get_tombstone("/stale-p", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            svc.mono_storage()
                .get_main_ref("/stale-p")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            svc.storage()
                .get_by_id(push_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            PushQueueStatusEnum::Failed
        );
        assert_eq!(
            svc.storage()
                .get_by_id(merge_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            PushQueueStatusEnum::Failed
        );
        assert_eq!(
            svc.storage()
                .get_by_id(attach_skip_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            PushQueueStatusEnum::Failed
        );
        assert!(
            svc.mono_storage()
                .get_main_ref("/third-party/x")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            svc.storage()
                .get_by_id(attach_tomb_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            PushQueueStatusEnum::Failed
        );
        assert!(
            svc.mono_storage()
                .get_tombstone("/third-party/y", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            svc.mono_storage()
                .get_main_ref("/third-party/y")
                .await
                .unwrap()
                .is_none()
        );
        assert!(report.i3_tombstoned >= 2);
    }

    #[tokio::test]
    async fn tp05_background_loop_reaps_running() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-BG").await;
        force_running(&svc, id, &"a".repeat(40), &"b".repeat(40)).await;
        let shutdown = CancellationToken::new();
        reaper(&svc).spawn_background(Duration::from_millis(20), shutdown.clone());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
            if row.status == PushQueueStatusEnum::Failed {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "background reaper did not terminalize Running row"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn tp05_hard_stop_resets_prior_running_not_this_run_bypass() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let prior = enqueue_and_claim(&svc, "CL-RP-HS1").await;
        svc.storage()
            .set_control_flags(None, Some(true), None)
            .await
            .unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.running_reset_queued >= 1);
        let row = svc.storage().get_by_id(prior).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Queued);

        svc.storage()
            .set_control_flags(None, Some(false), None)
            .await
            .unwrap();
        assert_eq!(
            svc.storage().claim_for_execution(prior).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let outcome = svc
            .execute_b3(
                ExecuteRequest {
                    id: prior,
                    ..Default::default()
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));

        let bypass = enqueue_and_claim(&svc, "CL-RP-HS2").await;
        mutate_root(
            &svc,
            &"a".repeat(40),
            &"b".repeat(40),
            &"c".repeat(40),
            &"d".repeat(40),
        )
        .await;
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.bypass_detected >= 1);
        let row = svc.storage().get_by_id(bypass).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
        assert_eq!(
            row.failure_type,
            Some(PushQueueFailureEnum::QueueBypassDetected)
        );
    }

    #[tokio::test]
    async fn tp05_stuck_alarm_when_lock_held_with_running() {
        let (_t, svc, _lock) = service().await;
        seed_root(&svc, &"a".repeat(40), &"b".repeat(40)).await;
        let id = enqueue_and_claim(&svc, "CL-RP-WD").await;
        let conn = svc.storage().get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE push_queue SET started_at = now() - interval '1 hour' WHERE id = $1",
            [sea_orm::Value::from(id)],
        ))
        .await
        .unwrap();
        let holder = conn.begin().await.unwrap();
        holder
            .execute_raw(Statement::from_string(
                DbBackend::Postgres,
                MONO_WRITE_LOCK_SQL.to_owned(),
            ))
            .await
            .unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(!report.lock_acquired);
        assert!(report.stuck_alarm);
        assert!(!report.lock_holders.is_empty());
        holder.rollback().await.unwrap();
        let report = reaper(&svc).reap_once().await.unwrap();
        assert!(report.lock_acquired);
        let row = svc.storage().get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
    }
}
