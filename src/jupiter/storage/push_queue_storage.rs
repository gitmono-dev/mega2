use std::ops::Deref;

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, PaginatorTrait,
    QueryFilter, Statement, TransactionTrait, Value,
};
use serde_json::Value as JsonValue;

use crate::{
    callisto::{
        mega_refs, push_queue, queue_control,
        sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
    },
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

/// MonoWriteQueue B3 serialization lock (trunk-push ADR-TP-03).
/// Distinct from monorepo initialization advisory lock keys.
pub const MONO_WRITE_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock(1297043024, 1229867349)";

/// Result of the B1 conditional INSERT (or its zero-row classification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// New row inserted; caller proceeds to B2 with this id.
    Inserted { id: i64 },
    /// Three-state hit on Queued/Running — adopt the existing row.
    Adopted { id: i64 },
    /// Three-state hit on Done — replay the persisted landing tip.
    Replay {
        id: i64,
        landed_commit_id: Option<String>,
    },
    /// Admission refused (pause / hard-stop / capacity / path conflict).
    Rejected { reason: EnqueueRejectReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueRejectReason {
    Paused,
    HardStopped,
    CapacityExceeded {
        max_depth: i32,
    },
    /// Same path already has a different push `operation_id` in Queued/Running.
    ActivePushPathConflict,
}

/// Outcome of a B2.5 claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Row is now Running with `expected_*` baselines persisted.
    Claimed,
    /// Hard-stop is set; caller stays in B2 (row remains Queued).
    HardStopped,
    /// Predicate missed (not min Queued, another Running, etc.) — back to B2.
    Missed,
}

#[derive(Clone)]
pub struct PushQueueStorage {
    base: BaseStorage,
}

impl Deref for PushQueueStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

#[derive(Debug, Clone)]
pub struct EnqueueParams<'a> {
    pub kind: PushQueueKindEnum,
    pub operation_id: &'a str,
    pub path: &'a str,
    pub old_id: &'a str,
    pub new_id: &'a str,
    pub requester: Option<&'a str>,
    pub payload: JsonValue,
}

impl PushQueueStorage {
    pub fn new(base: BaseStorage) -> Self {
        Self { base }
    }

    /// B1: lock `queue_control`, conditional INSERT, classify on zero rows.
    pub async fn enqueue_atomic(
        &self,
        params: EnqueueParams<'_>,
    ) -> Result<EnqueueOutcome, MegaError> {
        let EnqueueParams {
            kind,
            operation_id,
            path,
            old_id,
            new_id,
            requester,
            payload,
        } = params;
        let conn = self.get_connection();
        let txn = conn.begin().await?;

        // ① Admission lock first — id order = commit order = FIFO (I6).
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id FROM queue_control WHERE id = 1 FOR UPDATE".to_owned(),
        ))
        .await?;

        let kind_db = kind_to_db(&kind);
        // Bind order: $1=kind $2=operation_id $3=path $4=old $5=new $6=requester $7=payload
        // Terminal retry (Failed / Cancelled without successor) copies payload from
        // the latest predecessor (trunk-push §1.11); otherwise use caller $7.
        let insert = Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            WITH ctrl AS (
                SELECT paused, hard_stopped, max_depth
                FROM queue_control WHERE id = 1 FOR UPDATE
            ),
            pred AS (
                SELECT payload
                  FROM push_queue
                 WHERE kind = $1::push_queue_kind_enum
                   AND path = $3
                   AND operation_id = $2
                   AND (
                        status = 'Failed'::push_queue_status_enum
                        OR (
                            status = 'Cancelled'::push_queue_status_enum
                            AND superseded_by IS NULL
                        )
                   )
                 ORDER BY id DESC
                 LIMIT 1
            )
            INSERT INTO push_queue (
                kind, operation_id, path, old_id, new_id,
                requester, payload, status, heartbeat_at
            )
            SELECT
                $1::push_queue_kind_enum,
                $2,
                $3,
                $4,
                $5,
                $6,
                COALESCE((SELECT payload FROM pred), $7::jsonb),
                'Queued'::push_queue_status_enum,
                now()
            WHERE (SELECT NOT paused FROM ctrl)
              AND (SELECT NOT hard_stopped FROM ctrl)
              AND (
                    SELECT count(*)::int FROM push_queue
                    WHERE status IN (
                        'Queued'::push_queue_status_enum,
                        'Running'::push_queue_status_enum
                    )
                  ) < (SELECT max_depth FROM ctrl)
              AND NOT EXISTS (
                    SELECT 1 FROM push_queue
                     WHERE kind = $1::push_queue_kind_enum
                       AND path = $3
                       AND operation_id = $2
                       AND status IN (
                            'Queued'::push_queue_status_enum,
                            'Running'::push_queue_status_enum,
                            'Done'::push_queue_status_enum
                       )
                  )
            RETURNING id
            "#,
            [
                Value::from(kind_db),
                Value::from(operation_id.to_owned()),
                Value::from(path.to_owned()),
                Value::from(old_id.to_owned()),
                Value::from(new_id.to_owned()),
                Value::from(requester.map(str::to_owned)),
                Value::from(payload.to_string()),
            ],
        );

        let insert_result = txn.query_one_raw(insert).await;
        match insert_result {
            Ok(Some(row)) => {
                let id: i64 = row.try_get("", "id")?;
                txn.commit().await?;
                return Ok(EnqueueOutcome::Inserted { id });
            }
            Ok(None) => {}
            Err(err) if is_unique_violation(&err.to_string()) => {
                let msg = err.to_string();
                txn.rollback().await?;
                if msg.contains("push_queue_active_push_path") {
                    return Ok(EnqueueOutcome::Rejected {
                        reason: EnqueueRejectReason::ActivePushPathConflict,
                    });
                }
                // operation_states race: winner already committed — classify by read.
                if let Some(existing) = push_queue::Entity::find()
                    .filter(push_queue::Column::Kind.eq(kind))
                    .filter(push_queue::Column::Path.eq(path))
                    .filter(push_queue::Column::OperationId.eq(operation_id))
                    .filter(push_queue::Column::Status.is_in([
                        PushQueueStatusEnum::Queued,
                        PushQueueStatusEnum::Running,
                        PushQueueStatusEnum::Done,
                    ]))
                    .one(self.get_connection())
                    .await?
                {
                    return match existing.status {
                        PushQueueStatusEnum::Done => Ok(EnqueueOutcome::Replay {
                            id: existing.id,
                            landed_commit_id: existing.landed_commit_id,
                        }),
                        PushQueueStatusEnum::Queued | PushQueueStatusEnum::Running => {
                            Ok(EnqueueOutcome::Adopted { id: existing.id })
                        }
                        _ => unreachable!("filter restricts status"),
                    };
                }
                return Err(MegaError::Other(format!(
                    "unique conflict without classifiable row: {msg}"
                )));
            }
            Err(e) => return Err(e.into()),
        }

        // Zero rows — classify inside the same locked txn. If an active
        // predecessor raced to Failed/Cancelled between INSERT and classify
        // (terminal updates do not take the admission lock), retry the
        // conditional INSERT once so the terminal-retry payload copy path runs.
        for attempt in 0..2 {
            match self
                .classify_zero_row(&txn, kind.clone(), operation_id, path)
                .await?
            {
                Some(outcome) => {
                    match &outcome {
                        EnqueueOutcome::Adopted { .. } | EnqueueOutcome::Replay { .. } => {
                            txn.commit().await?;
                        }
                        EnqueueOutcome::Rejected { .. } => {
                            txn.rollback().await?;
                        }
                        EnqueueOutcome::Inserted { .. } => unreachable!("zero-row path"),
                    }
                    return Ok(outcome);
                }
                None if attempt == 0 => {
                    let retry = Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        r#"
                        WITH ctrl AS (
                            SELECT paused, hard_stopped, max_depth
                            FROM queue_control WHERE id = 1 FOR UPDATE
                        ),
                        pred AS (
                            SELECT payload
                              FROM push_queue
                             WHERE kind = $1::push_queue_kind_enum
                               AND path = $3
                               AND operation_id = $2
                               AND (
                                    status = 'Failed'::push_queue_status_enum
                                    OR (
                                        status = 'Cancelled'::push_queue_status_enum
                                        AND superseded_by IS NULL
                                    )
                               )
                             ORDER BY id DESC
                             LIMIT 1
                        )
                        INSERT INTO push_queue (
                            kind, operation_id, path, old_id, new_id,
                            requester, payload, status, heartbeat_at
                        )
                        SELECT
                            $1::push_queue_kind_enum,
                            $2,
                            $3,
                            $4,
                            $5,
                            $6,
                            COALESCE((SELECT payload FROM pred), $7::jsonb),
                            'Queued'::push_queue_status_enum,
                            now()
                        WHERE (SELECT NOT paused FROM ctrl)
                          AND (SELECT NOT hard_stopped FROM ctrl)
                          AND (
                                SELECT count(*)::int FROM push_queue
                                WHERE status IN (
                                    'Queued'::push_queue_status_enum,
                                    'Running'::push_queue_status_enum
                                )
                              ) < (SELECT max_depth FROM ctrl)
                          AND NOT EXISTS (
                                SELECT 1 FROM push_queue
                                 WHERE kind = $1::push_queue_kind_enum
                                   AND path = $3
                                   AND operation_id = $2
                                   AND status IN (
                                        'Queued'::push_queue_status_enum,
                                        'Running'::push_queue_status_enum,
                                        'Done'::push_queue_status_enum
                                   )
                              )
                        RETURNING id
                        "#,
                        [
                            Value::from(kind_to_db(&kind)),
                            Value::from(operation_id.to_owned()),
                            Value::from(path.to_owned()),
                            Value::from(old_id.to_owned()),
                            Value::from(new_id.to_owned()),
                            Value::from(requester.map(str::to_owned)),
                            Value::from(payload.to_string()),
                        ],
                    );
                    match txn.query_one_raw(retry).await? {
                        Some(row) => {
                            let id: i64 = row.try_get("", "id")?;
                            txn.commit().await?;
                            return Ok(EnqueueOutcome::Inserted { id });
                        }
                        None => continue,
                    }
                }
                None => {
                    txn.rollback().await?;
                    return Err(MegaError::Other(
                        "B1 INSERT returned 0 rows without a classifiable cause".into(),
                    ));
                }
            }
        }
        unreachable!("B1 retry loop exhausted");
    }

    /// Classify a zero-row conditional INSERT. Returns `None` when the only
    /// explanation is an active→terminal race — caller should retry INSERT.
    async fn classify_zero_row(
        &self,
        txn: &DatabaseTransaction,
        kind: PushQueueKindEnum,
        operation_id: &str,
        path: &str,
    ) -> Result<Option<EnqueueOutcome>, MegaError> {
        // Done replay is available even under pause / hard-stop / capacity.
        if let Some(existing) = push_queue::Entity::find()
            .filter(push_queue::Column::Kind.eq(kind.clone()))
            .filter(push_queue::Column::Path.eq(path))
            .filter(push_queue::Column::OperationId.eq(operation_id))
            .filter(push_queue::Column::Status.is_in([
                PushQueueStatusEnum::Queued,
                PushQueueStatusEnum::Running,
                PushQueueStatusEnum::Done,
            ]))
            .one(txn)
            .await?
        {
            return match existing.status {
                PushQueueStatusEnum::Done => Ok(Some(EnqueueOutcome::Replay {
                    id: existing.id,
                    landed_commit_id: existing.landed_commit_id,
                })),
                PushQueueStatusEnum::Queued | PushQueueStatusEnum::Running => {
                    Ok(Some(EnqueueOutcome::Adopted { id: existing.id }))
                }
                _ => unreachable!("filter restricts status"),
            };
        }

        let ctrl = queue_control::Entity::find_by_id(1)
            .one(txn)
            .await?
            .ok_or_else(|| MegaError::Other("queue_control row missing".into()))?;

        if ctrl.hard_stopped {
            return Ok(Some(EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::HardStopped,
            }));
        }
        if ctrl.paused {
            return Ok(Some(EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::Paused,
            }));
        }

        let active = push_queue::Entity::find()
            .filter(
                push_queue::Column::Status
                    .is_in([PushQueueStatusEnum::Queued, PushQueueStatusEnum::Running]),
            )
            .count(txn)
            .await?;
        if active as i32 >= ctrl.max_depth {
            return Ok(Some(EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::CapacityExceeded {
                    max_depth: ctrl.max_depth,
                },
            }));
        }

        if kind == PushQueueKindEnum::Push {
            let conflict = push_queue::Entity::find()
                .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Push))
                .filter(push_queue::Column::Path.eq(path))
                .filter(
                    push_queue::Column::Status
                        .is_in([PushQueueStatusEnum::Queued, PushQueueStatusEnum::Running]),
                )
                .one(txn)
                .await?;
            if conflict.is_some() {
                return Ok(Some(EnqueueOutcome::Rejected {
                    reason: EnqueueRejectReason::ActivePushPathConflict,
                }));
            }
        }

        // No adopt/replay/reject cause — likely active→terminal race.
        Ok(None)
    }

    /// Refresh heartbeat for a B2 waiter (does not change status).
    pub async fn touch_heartbeat(&self, id: i64) -> Result<(), MegaError> {
        let conn = self.get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE push_queue SET heartbeat_at = now(), updated_at = now() WHERE id = $1",
            [Value::from(id)],
        ))
        .await?;
        Ok(())
    }

    pub async fn get_by_id(&self, id: i64) -> Result<Option<push_queue::Model>, MegaError> {
        Ok(push_queue::Entity::find_by_id(id)
            .one(self.get_connection())
            .await?)
    }

    /// B2.5: claim under `queue_control FOR UPDATE`, then snapshot root
    /// **after** the claim UPDATE succeeds (same txn, plain SELECT) and persist
    /// that snapshot as `expected_*`.
    pub async fn claim_for_execution(&self, id: i64) -> Result<ClaimOutcome, MegaError> {
        let conn = self.get_connection();
        let txn = conn.begin().await?;

        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id FROM queue_control WHERE id = 1 FOR UPDATE".to_owned(),
        ))
        .await?;

        let ctrl = queue_control::Entity::find_by_id(1)
            .one(&txn)
            .await?
            .ok_or_else(|| MegaError::Other("queue_control row missing".into()))?;

        if ctrl.hard_stopped {
            txn.rollback().await?;
            return Ok(ClaimOutcome::HardStopped);
        }

        let claimed = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET status = 'Running'::push_queue_status_enum,
                       started_at = now(),
                       heartbeat_at = now(),
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Queued'::push_queue_status_enum
                   AND (SELECT NOT hard_stopped FROM queue_control WHERE id = 1)
                   AND NOT EXISTS (
                        SELECT 1 FROM push_queue
                         WHERE status = 'Running'::push_queue_status_enum
                   )
                   AND id = (
                        SELECT min(id) FROM push_queue
                         WHERE status = 'Queued'::push_queue_status_enum
                   )
                "#,
                [Value::from(id)],
            ))
            .await?;

        if claimed.rows_affected() == 0 {
            txn.rollback().await?;
            return Ok(ClaimOutcome::Missed);
        }

        // Snapshot root AFTER the claim UPDATE succeeds (still in this txn).
        // Ordering matters:
        // - Before UPDATE: a legitimate predecessor B3 can commit Done + new
        //   root in the gap → stale expected_* → false QueueBypassDetected.
        // - FOR SHARE before UPDATE: blocks admissions while a writer holds
        //   the root row exclusively (stalls past wait_timeout).
        // - After UPDATE: NOT EXISTS(Running) already held, so no legitimate
        //   B3 can still be writing; plain SELECT needs no row lock.
        let root = mega_refs::Entity::find()
            .filter(mega_refs::Column::Path.eq("/"))
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
            .one(&txn)
            .await?;
        let (commit, tree) = match root {
            Some(r) => (Some(r.ref_commit_hash), Some(r.ref_tree_hash)),
            None => (None, None),
        };

        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE push_queue
               SET expected_commit_hash = $2,
                   expected_tree_hash = $3,
                   updated_at = now()
             WHERE id = $1
            "#,
            [Value::from(id), Value::from(commit), Value::from(tree)],
        ))
        .await?;

        txn.commit().await?;
        Ok(ClaimOutcome::Claimed)
    }

    pub async fn set_control_flags(
        &self,
        paused: Option<bool>,
        hard_stopped: Option<bool>,
        max_depth: Option<i32>,
    ) -> Result<(), MegaError> {
        let conn = self.get_connection();
        let txn = conn.begin().await?;
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id FROM queue_control WHERE id = 1 FOR UPDATE".to_owned(),
        ))
        .await?;
        if let Some(paused) = paused {
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE queue_control SET paused = $1, updated_at = now() WHERE id = 1",
                [Value::from(paused)],
            ))
            .await?;
        }
        if let Some(hard_stopped) = hard_stopped {
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE queue_control SET hard_stopped = $1, updated_at = now() WHERE id = 1",
                [Value::from(hard_stopped)],
            ))
            .await?;
        }
        if let Some(max_depth) = max_depth {
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE queue_control SET max_depth = $1, updated_at = now() WHERE id = 1",
                [Value::from(max_depth)],
            ))
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Acquire MonoWriteQueue B3 advisory lock inside an open transaction.
    pub async fn acquire_mono_write_lock(txn: &DatabaseTransaction) -> Result<(), MegaError> {
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            MONO_WRITE_LOCK_SQL.to_owned(),
        ))
        .await?;
        Ok(())
    }

    /// Whether `queue_control.hard_stopped` is set (plain SELECT, no lock).
    pub async fn is_hard_stopped_in_txn(txn: &DatabaseTransaction) -> Result<bool, MegaError> {
        let ctrl = queue_control::Entity::find_by_id(1)
            .one(txn)
            .await?
            .ok_or_else(|| MegaError::Other("queue_control row missing".into()))?;
        Ok(ctrl.hard_stopped)
    }

    /// Reset a Running row back to Queued (hard-stop abandon; independent txn).
    pub async fn reset_running_to_queued(&self, id: i64) -> Result<bool, MegaError> {
        let conn = self.get_connection();
        let result = conn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET status = 'Queued'::push_queue_status_enum,
                       started_at = NULL,
                       expected_commit_hash = NULL,
                       expected_tree_hash = NULL,
                       pending_action = NULL,
                       heartbeat_at = now(),
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Running'::push_queue_status_enum
                "#,
                [Value::from(id)],
            ))
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Persist `requeue_conflict` intent (B4 phase 1; independent short txn).
    pub async fn persist_requeue_conflict_intent(&self, id: i64) -> Result<bool, MegaError> {
        let conn = self.get_connection();
        let result = conn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET pending_action = 'requeue_conflict'::push_queue_pending_enum,
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Running'::push_queue_status_enum
                "#,
                [Value::from(id)],
            ))
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Mark Done when still Running (returns whether the row was updated).
    pub async fn mark_done_if_running_in_txn(
        txn: &DatabaseTransaction,
        id: i64,
        landed_commit_id: &str,
    ) -> Result<bool, MegaError> {
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET status = 'Done'::push_queue_status_enum,
                       landed_commit_id = $2,
                       finished_at = now(),
                       pending_action = NULL,
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Running'::push_queue_status_enum
                "#,
                [Value::from(id), Value::from(landed_commit_id.to_owned())],
            ))
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Mark Failed when still Running (returns whether the row was updated).
    pub async fn mark_failed_if_running_in_txn(
        txn: &DatabaseTransaction,
        id: i64,
        failure: &str,
        message: &str,
    ) -> Result<bool, MegaError> {
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET status = 'Failed'::push_queue_status_enum,
                       failure_type = $2::push_queue_failure_enum,
                       error_message = $3,
                       finished_at = now(),
                       pending_action = NULL,
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Running'::push_queue_status_enum
                "#,
                [
                    Value::from(id),
                    Value::from(failure.to_owned()),
                    Value::from(message.to_owned()),
                ],
            ))
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Set hard_stopped inside an open transaction.
    pub async fn set_hard_stopped_in_txn(
        txn: &DatabaseTransaction,
        hard_stopped: bool,
    ) -> Result<(), MegaError> {
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE queue_control SET hard_stopped = $1, updated_at = now() WHERE id = 1",
            [Value::from(hard_stopped)],
        ))
        .await?;
        Ok(())
    }

    pub async fn notify_mono_write_queue(txn: &DatabaseTransaction) -> Result<(), MegaError> {
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            "NOTIFY mono_write_queue".to_owned(),
        ))
        .await?;
        Ok(())
    }

    /// Establish a SAVEPOINT for B3 kind / B4 business writes (trunk-push B3/B4).
    /// `name` must be a fixed identifier (letters/digits/underscore only).
    pub async fn savepoint(txn: &DatabaseTransaction, name: &'static str) -> Result<(), MegaError> {
        debug_assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "savepoint name must be a static SQL identifier"
        );
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SAVEPOINT {name}"),
        ))
        .await?;
        Ok(())
    }

    /// Roll business writes back to a SAVEPOINT; keeps the outer txn + advisory lock.
    pub async fn rollback_to_savepoint(
        txn: &DatabaseTransaction,
        name: &'static str,
    ) -> Result<(), MegaError> {
        debug_assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "savepoint name must be a static SQL identifier"
        );
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("ROLLBACK TO SAVEPOINT {name}"),
        ))
        .await?;
        Ok(())
    }

    /// Complete conflict requeue under admission lock (B4 phase 2).
    /// Returns the successor id.
    pub async fn complete_conflict_requeue_in_txn(
        txn: &DatabaseTransaction,
        old: &push_queue::Model,
    ) -> Result<i64, MegaError> {
        txn.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id FROM queue_control WHERE id = 1 FOR UPDATE".to_owned(),
        ))
        .await?;

        let cancelled = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE push_queue
                   SET status = 'Cancelled'::push_queue_status_enum,
                       failure_type = 'Conflict'::push_queue_failure_enum,
                       finished_at = now(),
                       pending_action = NULL,
                       updated_at = now()
                 WHERE id = $1
                   AND status = 'Running'::push_queue_status_enum
                   AND pending_action = 'requeue_conflict'::push_queue_pending_enum
                "#,
                [Value::from(old.id)],
            ))
            .await?;
        if cancelled.rows_affected() == 0 {
            return Err(MegaError::Other(
                "conflict requeue: old row not Running with requeue_conflict intent".into(),
            ));
        }

        let successor = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                INSERT INTO push_queue (
                    kind, operation_id, path, old_id, new_id,
                    requester, payload, status, heartbeat_at
                ) VALUES (
                    $1::push_queue_kind_enum, $2, $3, $4, $5, $6, $7::jsonb,
                    'Queued'::push_queue_status_enum, now()
                )
                RETURNING id
                "#,
                [
                    Value::from(kind_to_db(&old.kind)),
                    Value::from(old.operation_id.clone()),
                    Value::from(old.path.clone()),
                    Value::from(old.old_id.clone()),
                    Value::from(old.new_id.clone()),
                    Value::from(old.requester.clone()),
                    Value::from(old.payload.to_string()),
                ],
            ))
            .await?
            .ok_or_else(|| MegaError::Other("conflict requeue INSERT returned no id".into()))?;
        let successor_id: i64 = successor.try_get("", "id")?;

        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE push_queue SET superseded_by = $2, updated_at = now() WHERE id = $1",
            [Value::from(old.id), Value::from(successor_id)],
        ))
        .await?;

        Ok(successor_id)
    }

    /// Test helper: force a row into Done with a landed tip (for replay tests).
    #[cfg(test)]
    pub async fn mark_done_for_test(
        &self,
        id: i64,
        landed_commit_id: &str,
    ) -> Result<(), MegaError> {
        let conn = self.get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE push_queue
               SET status = 'Done'::push_queue_status_enum,
                   landed_commit_id = $2,
                   finished_at = now(),
                   updated_at = now()
             WHERE id = $1
            "#,
            [Value::from(id), Value::from(landed_commit_id.to_owned())],
        ))
        .await?;
        Ok(())
    }

    /// Test helper: force a row into Failed (for terminal-retry payload tests).
    #[cfg(test)]
    pub async fn mark_failed_for_test(&self, id: i64) -> Result<(), MegaError> {
        let conn = self.get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE push_queue
               SET status = 'Failed'::push_queue_status_enum,
                   failure_type = 'Conflict'::push_queue_failure_enum,
                   error_message = 'test failure',
                   finished_at = now(),
                   updated_at = now()
             WHERE id = $1
            "#,
            [Value::from(id)],
        ))
        .await?;
        Ok(())
    }
    /// Test helper: mark Cancelled with an optional conflict-requeue successor.
    #[cfg(test)]
    pub async fn mark_cancelled_with_successor_for_test(
        &self,
        id: i64,
        successor: Option<i64>,
    ) -> Result<(), MegaError> {
        let conn = self.get_connection();
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            r#"
            UPDATE push_queue
               SET status = 'Cancelled'::push_queue_status_enum,
                   failure_type = 'Conflict'::push_queue_failure_enum,
                   superseded_by = $2,
                   finished_at = now(),
                   updated_at = now()
             WHERE id = $1
            "#,
            [Value::from(id), Value::from(successor)],
        ))
        .await?;
        Ok(())
    }
}

fn kind_to_db(kind: &PushQueueKindEnum) -> &'static str {
    match kind {
        PushQueueKindEnum::Push => "push",
        PushQueueKindEnum::Merge => "merge",
        PushQueueKindEnum::Attach => "attach",
    }
}

fn is_unique_violation(msg: &str) -> bool {
    msg.contains("push_queue_active_push_path")
        || msg.contains("push_queue_operation_states")
        || msg.contains("duplicate key")
        || msg.contains("23505")
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ConnectionTrait};
    use serde_json::json;

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations, storage::base_storage::StorageConnector,
        tests::test_db_connection,
    };

    async fn storage() -> (tempfile::TempDir, PushQueueStorage) {
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(std::sync::Arc::new(db));
        (temp, PushQueueStorage::new(base))
    }

    fn push_payload() -> JsonValue {
        json!({"commits": [], "fork_base": null, "n": 0})
    }

    #[tokio::test]
    async fn b1_inserts_and_replays_done() {
        let (_t, st) = storage().await;
        let tip = "b".repeat(40);
        let first = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "aaa→bbb",
                path: "/project/x",
                old_id: &"a".repeat(40),
                new_id: &tip,
                requester: Some("alice"),
                payload: push_payload(),
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = first else {
            panic!("expected insert, got {first:?}");
        };

        st.mark_done_for_test(id, &tip).await.unwrap();

        let replay = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "aaa→bbb",
                path: "/project/x",
                old_id: &"a".repeat(40),
                new_id: &tip,
                requester: Some("alice"),
                payload: push_payload(),
            })
            .await
            .unwrap();
        match replay {
            EnqueueOutcome::Replay {
                id: rid,
                landed_commit_id,
            } => {
                assert_eq!(rid, id);
                assert_eq!(landed_commit_id.as_deref(), Some(tip.as_str()));
            }
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn b1_adopts_active_row() {
        let (_t, st) = storage().await;
        let first = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "old→new",
                path: "/project/y",
                old_id: &"1".repeat(40),
                new_id: &"2".repeat(40),
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = first else {
            panic!("expected insert");
        };

        let second = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "old→new",
                path: "/project/y",
                old_id: &"1".repeat(40),
                new_id: &"2".repeat(40),
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        assert_eq!(second, EnqueueOutcome::Adopted { id });
    }

    #[tokio::test]
    async fn b1_rejects_when_paused_or_hard_stopped_or_full() {
        let (_t, st) = storage().await;
        st.set_control_flags(Some(true), None, None).await.unwrap();
        let paused = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CLINK1",
                path: "/project/z",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap();
        assert!(matches!(
            paused,
            EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::Paused
            }
        ));

        st.set_control_flags(Some(false), Some(true), None)
            .await
            .unwrap();
        let hard = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CLINK2",
                path: "/project/z",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap();
        assert!(matches!(
            hard,
            EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::HardStopped
            }
        ));

        st.set_control_flags(Some(false), Some(false), Some(1))
            .await
            .unwrap();
        let _ = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CLINK3",
                path: "/a",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap();
        let full = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CLINK4",
                path: "/b",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap();
        assert!(matches!(
            full,
            EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::CapacityExceeded { .. }
            }
        ));
    }

    #[tokio::test]
    async fn b1_rejects_second_push_on_same_path_different_fingerprint() {
        let (_t, st) = storage().await;
        let _ = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "a→b",
                path: "/project/same",
                old_id: &"a".repeat(40),
                new_id: &"b".repeat(40),
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        let conflict = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "c→d",
                path: "/project/same",
                old_id: &"c".repeat(40),
                new_id: &"d".repeat(40),
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        assert!(matches!(
            conflict,
            EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::ActivePushPathConflict
            }
        ));
    }

    #[tokio::test]
    async fn b25_claim_is_exclusive_under_dual_claimers() {
        let (_t, st) = storage().await;
        let old = "0".repeat(40);
        let new = "1".repeat(40);
        let EnqueueOutcome::Inserted { id: id_a } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-CLAIM-A",
                path: "/project/claim-a",
                old_id: &old,
                new_id: &new,
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("insert a");
        };
        let EnqueueOutcome::Inserted { id: id_b } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-CLAIM-B",
                path: "/project/claim-b",
                old_id: &old,
                new_id: &new,
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("insert b");
        };
        assert!(id_a < id_b);

        // Two claimers on their *own* Queued rows concurrently — exercises
        // NOT EXISTS(Running) + min(id) under queue_control serialization.
        let left = st.clone();
        let right = st.clone();
        let (a, b) = tokio::join!(
            left.claim_for_execution(id_a),
            right.claim_for_execution(id_b),
        );
        let outcomes = [a.unwrap(), b.unwrap()];
        let claimed = outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Claimed))
            .count();
        let missed = outcomes
            .iter()
            .filter(|o| matches!(o, ClaimOutcome::Missed))
            .count();
        assert_eq!(claimed, 1);
        assert_eq!(missed, 1);

        let row_a = st.get_by_id(id_a).await.unwrap().unwrap();
        let row_b = st.get_by_id(id_b).await.unwrap().unwrap();
        let running = [&row_a, &row_b]
            .iter()
            .filter(|r| r.status == PushQueueStatusEnum::Running)
            .count();
        assert_eq!(running, 1);
        assert_eq!(row_a.status, PushQueueStatusEnum::Running);
        assert_eq!(row_b.status, PushQueueStatusEnum::Queued);
        // No root ref yet → NULL sentinel baselines.
        assert!(row_a.expected_commit_hash.is_none());
        assert!(row_a.expected_tree_hash.is_none());
    }

    #[tokio::test]
    async fn done_replay_works_while_paused() {
        let (_t, st) = storage().await;
        let tip = "e".repeat(40);
        let EnqueueOutcome::Inserted { id } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "p→e",
                path: "/project/replay",
                old_id: &"p".repeat(40),
                new_id: &tip,
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        st.mark_done_for_test(id, &tip).await.unwrap();
        st.set_control_flags(Some(true), None, None).await.unwrap();
        let replay = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "p→e",
                path: "/project/replay",
                old_id: &"p".repeat(40),
                new_id: &tip,
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        assert!(matches!(replay, EnqueueOutcome::Replay { .. }));
    }

    #[tokio::test]
    async fn b1_fifo_id_order_matches_commit_order_under_lock() {
        let (_t, st) = storage().await;
        let old = "0".repeat(40);
        let new = "1".repeat(40);

        // Hold the admission lock on a side connection while a concurrent
        // enqueue waits — then insert the earlier row and only then commit.
        // Without FOR UPDATE, the waiter could commit a larger id first and
        // B2.5 would treat it as head; with the lock, id order = commit order
        // and B2.5 refuses the later id while the earlier Queued row exists.
        let holder = st.get_connection().begin().await.unwrap();
        holder
            .execute_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT id FROM queue_control WHERE id = 1 FOR UPDATE".to_owned(),
            ))
            .await
            .unwrap();

        let waiter = st.clone();
        let old_w = old.clone();
        let new_w = new.clone();
        let enqueue_later = tokio::spawn(async move {
            waiter
                .enqueue_atomic(EnqueueParams {
                    kind: PushQueueKindEnum::Merge,
                    operation_id: "CL-FIFO-LATER",
                    path: "/fifo/later",
                    old_id: &old_w,
                    new_id: &new_w,
                    requester: None,
                    payload: json!({}),
                })
                .await
        });
        // Give the waiter time to block on FOR UPDATE.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let early = holder
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                INSERT INTO push_queue (
                    kind, operation_id, path, old_id, new_id,
                    requester, payload, status, heartbeat_at
                ) VALUES (
                    'merge'::push_queue_kind_enum, 'CL-FIFO-EARLY', '/fifo/early',
                    $1, $2, NULL, '{}'::jsonb,
                    'Queued'::push_queue_status_enum, now()
                )
                RETURNING id
                "#,
                [Value::from(old.clone()), Value::from(new.clone())],
            ))
            .await
            .unwrap()
            .expect("early insert");
        let early_id: i64 = early.try_get("", "id").unwrap();
        holder.commit().await.unwrap();

        let later = enqueue_later.await.unwrap().unwrap();
        let EnqueueOutcome::Inserted { id: later_id } = later else {
            panic!("expected later insert, got {later:?}");
        };
        assert!(
            early_id < later_id,
            "admission lock must force earlier commit → smaller id"
        );

        // B2.5 must not treat the later id as head while early is still Queued.
        assert_eq!(
            st.claim_for_execution(later_id).await.unwrap(),
            ClaimOutcome::Missed
        );
        assert_eq!(
            st.claim_for_execution(early_id).await.unwrap(),
            ClaimOutcome::Claimed
        );
    }

    #[tokio::test]
    async fn b25_expected_baselines_come_from_root_inside_claim_txn() {
        use chrono::Utc;
        use sea_orm::Set;

        let (_t, st) = storage().await;
        let commit = "c".repeat(40);
        let tree = "t".repeat(40);
        mega_refs::ActiveModel {
            id: Set(1),
            path: Set("/".into()),
            ref_name: Set(MEGA_BRANCH_NAME.into()),
            ref_commit_hash: Set(commit.clone()),
            ref_tree_hash: Set(tree.clone()),
            is_cl: Set(false),
            created_at: Set(Utc::now().naive_utc()),
            updated_at: Set(Utc::now().naive_utc()),
        }
        .insert(st.get_connection())
        .await
        .unwrap();

        let EnqueueOutcome::Inserted { id } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-BASELINE",
                path: "/baseline",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        assert_eq!(
            st.claim_for_execution(id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let row = st.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.expected_commit_hash.as_deref(), Some(commit.as_str()));
        assert_eq!(row.expected_tree_hash.as_deref(), Some(tree.as_str()));
    }

    #[tokio::test]
    async fn b25_baseline_sees_root_after_predecessor_finishes() {
        use chrono::Utc;
        use sea_orm::Set;

        let (_t, st) = storage().await;
        let r0_commit = "0".repeat(40);
        let r0_tree = "a".repeat(40);
        mega_refs::ActiveModel {
            id: Set(1),
            path: Set("/".into()),
            ref_name: Set(MEGA_BRANCH_NAME.into()),
            ref_commit_hash: Set(r0_commit.clone()),
            ref_tree_hash: Set(r0_tree.clone()),
            is_cl: Set(false),
            created_at: Set(Utc::now().naive_utc()),
            updated_at: Set(Utc::now().naive_utc()),
        }
        .insert(st.get_connection())
        .await
        .unwrap();

        let old = "1".repeat(40);
        let new = "2".repeat(40);
        let EnqueueOutcome::Inserted { id: first } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-PRED",
                path: "/pred",
                old_id: &old,
                new_id: &new,
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("first");
        };
        let EnqueueOutcome::Inserted { id: second } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-NEXT",
                path: "/next",
                old_id: &old,
                new_id: &new,
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("second");
        };

        assert_eq!(
            st.claim_for_execution(first).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let head = st.get_by_id(first).await.unwrap().unwrap();
        assert_eq!(
            head.expected_commit_hash.as_deref(),
            Some(r0_commit.as_str())
        );

        // Predecessor finishes and advances the root (legitimate B3).
        let r1_commit = "9".repeat(40);
        let r1_tree = "b".repeat(40);
        st.get_connection()
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                r#"
                UPDATE mega_refs
                   SET ref_commit_hash = $1, ref_tree_hash = $2, updated_at = now()
                 WHERE path = '/' AND ref_name = $3
                "#,
                [
                    Value::from(r1_commit.clone()),
                    Value::from(r1_tree.clone()),
                    Value::from(MEGA_BRANCH_NAME.to_owned()),
                ],
            ))
            .await
            .unwrap();
        st.mark_done_for_test(first, &r1_commit).await.unwrap();

        assert_eq!(
            st.claim_for_execution(second).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let next = st.get_by_id(second).await.unwrap().unwrap();
        // Must capture the post-predecessor root, not the pre-claim stale tip.
        assert_eq!(
            next.expected_commit_hash.as_deref(),
            Some(r1_commit.as_str())
        );
        assert_eq!(next.expected_tree_hash.as_deref(), Some(r1_tree.as_str()));
    }

    #[tokio::test]
    async fn b1_different_push_baselines_to_same_tip_are_distinct_ops() {
        let (_t, st) = storage().await;
        let tip = "c".repeat(40);
        let first = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "a→c",
                path: "/project/base",
                old_id: &"a".repeat(40),
                new_id: &tip,
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        assert!(matches!(first, EnqueueOutcome::Inserted { .. }));
        // Same path, different fingerprint → path conflict (ADR-TP-10), not adopt.
        let second = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Push,
                operation_id: "b→c",
                path: "/project/base",
                old_id: &"b".repeat(40),
                new_id: &tip,
                requester: None,
                payload: push_payload(),
            })
            .await
            .unwrap();
        assert!(matches!(
            second,
            EnqueueOutcome::Rejected {
                reason: EnqueueRejectReason::ActivePushPathConflict
            }
        ));
    }

    #[tokio::test]
    async fn b25_hard_stopped_refuses_claim() {
        let (_t, st) = storage().await;
        let EnqueueOutcome::Inserted { id } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-HS",
                path: "/hs",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: json!({}),
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        st.set_control_flags(None, Some(true), None).await.unwrap();
        let outcome = st.claim_for_execution(id).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::HardStopped);
        let row = st.get_by_id(id).await.unwrap().unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Queued);
    }

    #[tokio::test]
    async fn b1_terminal_retry_copies_persisted_payload() {
        let (_t, st) = storage().await;
        let original = json!({"cl_link": "CL-ORIG", "n": 1});
        let EnqueueOutcome::Inserted { id: old_id } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-RETRY",
                path: "/retry",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                payload: original.clone(),
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        st.mark_failed_for_test(old_id).await.unwrap();

        let EnqueueOutcome::Inserted { id: new_id } = st
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-RETRY",
                path: "/retry",
                old_id: &"0".repeat(40),
                new_id: &"1".repeat(40),
                requester: None,
                // Caller supplies a divergent payload (e.g. crash recovery
                // without the original descriptor) — B1 must copy the old one.
                payload: json!({"cl_link": "CL-WRONG", "n": 99}),
            })
            .await
            .unwrap()
        else {
            panic!("retry insert");
        };
        assert_ne!(old_id, new_id);
        let row = st.get_by_id(new_id).await.unwrap().unwrap();
        assert_eq!(row.payload, original);
    }
}
