//! TP-01: `push_queue` and `queue_control` — the persistence base of
//! `MonoWriteQueue` (`docs/refactoring/trunk-push.md` 1.4).
//!
//! `push_queue` is a new entity rather than a reuse of `merge_queue`: the
//! existing `queue_status_enum` describes the CL workflow
//! (`Waiting/Testing/Merging/Merged/Failed`), which is not isomorphic to this
//! queue's lifecycle (queued / running / terminal), and
//! `queue_failure_type_enum` has no `QueueBypassDetected` / `PushFailure` /
//! `AttachFailure` variant. Only the four-layer *shape* is reused.
//!
//! `id` is database-assigned, not the application-assigned `pk_bigint` used
//! elsewhere: it is the authoritative global order of queue operations
//! (ADR-TP-06), shared by the `push`, `merge` and `attach` rows. `sea_query`
//! spells `auto_increment` as an identity column rather than the `bigserial`
//! of 1.4; both draw from a sequence, so the ordering property is the same.
//!
//! `queue_control` is seeded here because B1 serializes admission by taking
//! `FOR UPDATE` on its single row — with no row there is nothing to lock. The
//! seeded `max_depth` is 64, a value chosen here rather than in 1.4, which
//! leaves the column's initial depth unspecified. Startup re-asserts the row
//! through [`ensure_queue_control_seed`], so a database whose row was dropped
//! recovers without a new migration.
//!
//! Forward-only (`down` is empty): the repository exposes no `migrate down`
//! entry point, so recovery is a follow-up migration. `up` is nevertheless
//! written to be re-runnable — an operator who lost the migration tracking
//! table can replay it against a schema that already exists.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, EnumIter, Iterable};
use sea_orm_migration::{prelude::*, schema::*};

/// Seeds the single `queue_control` row. Idempotent, so both the migration and
/// a later startup check can run it.
pub(crate) const SEED_QUEUE_CONTROL: &str = "INSERT INTO queue_control \
     (id, paused, hard_stopped, last_policy, max_depth, updated_at) \
     VALUES (1, false, false, 'review', 64, now()) \
     ON CONFLICT (id) DO NOTHING";

const STATUS_ENUM: &str = "push_queue_status_enum";
const KIND_ENUM: &str = "push_queue_kind_enum";
const FAILURE_ENUM: &str = "push_queue_failure_enum";
const PENDING_ENUM: &str = "push_queue_pending_enum";

/// Re-asserts the single `queue_control` row on startup, as 1.4 asks for
/// ("迁移/启动播种").
///
/// The migration seeds the row, but a database restored from an older dump, or
/// one whose row was deleted by hand, would leave B1 with nothing to take
/// `FOR UPDATE` on and silently lose admission serialization. `ON CONFLICT DO
/// NOTHING` makes this a no-op when the row is there, so an operator's
/// `paused` / `hard_stopped` / `max_depth` / `last_policy` survive every
/// restart.
pub async fn ensure_queue_control_seed(db: &DatabaseConnection) -> Result<(), DbErr> {
    db.execute_unprepared(SEED_QUEUE_CONTROL).await.map(|_| ())
}

/// `CREATE TYPE` has no `IF NOT EXISTS`, and `sea_query` offers no guarded
/// form, so a replay of `up` against an existing schema would fail on the
/// first enum while every other statement here is already idempotent. The `DO`
/// block narrows the tolerance to `duplicate_object`; anything else still
/// propagates.
async fn create_enum_if_absent<T>(manager: &SchemaManager<'_>, type_name: &str) -> Result<(), DbErr>
where
    T: Iden + Iterable,
{
    let labels = T::iter()
        .map(|value| format!("'{}'", Iden::to_string(&value)))
        .collect::<Vec<_>>()
        .join(",");

    manager
        .get_connection()
        .execute_unprepared(&format!(
            "DO $$ BEGIN CREATE TYPE {type_name} AS ENUM ({labels}); \
             EXCEPTION WHEN duplicate_object THEN NULL; END $$"
        ))
        .await
        .map(|_| ())
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(DbErr::Migration(format!(
                "push_queue requires PostgreSQL, got {:?}",
                manager.get_database_backend()
            )));
        }

        create_enum_if_absent::<PushQueueStatus>(manager, STATUS_ENUM).await?;
        create_enum_if_absent::<PushQueueKind>(manager, KIND_ENUM).await?;
        create_enum_if_absent::<PushQueueFailure>(manager, FAILURE_ENUM).await?;
        create_enum_if_absent::<PushQueuePending>(manager, PENDING_ENUM).await?;

        manager
            .create_table(
                Table::create()
                    .table(PushQueue::Table)
                    .if_not_exists()
                    .col(big_integer(PushQueue::Id).primary_key().auto_increment())
                    .col(enumeration(
                        PushQueue::Kind,
                        Alias::new(KIND_ENUM),
                        PushQueueKind::iter(),
                    ))
                    .col(text(PushQueue::OperationId))
                    .col(text(PushQueue::Path))
                    .col(text(PushQueue::OldId))
                    .col(text(PushQueue::NewId))
                    .col(json_binary(PushQueue::Payload))
                    .col(text_null(PushQueue::LandedCommitId))
                    .col(enumeration(
                        PushQueue::Status,
                        Alias::new(STATUS_ENUM),
                        PushQueueStatus::iter(),
                    ))
                    .col(text_null(PushQueue::Requester))
                    .col(enumeration_null(
                        PushQueue::FailureType,
                        Alias::new(FAILURE_ENUM),
                        PushQueueFailure::iter(),
                    ))
                    .col(text_null(PushQueue::ErrorMessage))
                    .col(timestamp_with_time_zone(PushQueue::HeartbeatAt))
                    .col(big_integer_null(PushQueue::SupersededBy))
                    .col(text_null(PushQueue::ExpectedCommitHash))
                    .col(text_null(PushQueue::ExpectedTreeHash))
                    .col(enumeration_null(
                        PushQueue::PendingAction,
                        Alias::new(PENDING_ENUM),
                        PushQueuePending::iter(),
                    ))
                    // B1 INSERT (trunk-push.md 1.5) supplies heartbeat_at but not
                    // enqueued_at/updated_at — defaults keep that contract valid.
                    .col(
                        timestamp_with_time_zone(PushQueue::EnqueuedAt)
                            .default(Expr::current_timestamp())
                            .to_owned(),
                    )
                    .col(timestamp_with_time_zone_null(PushQueue::StartedAt))
                    .col(timestamp_with_time_zone_null(PushQueue::FinishedAt))
                    .col(
                        timestamp_with_time_zone(PushQueue::UpdatedAt)
                            .default(Expr::current_timestamp())
                            .to_owned(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(QueueControl::Table)
                    .if_not_exists()
                    .col(
                        integer(QueueControl::Id)
                            .primary_key()
                            .default(1)
                            .check((
                                "queue_control_single_row",
                                Expr::col(QueueControl::Id).eq(1),
                            ))
                            .to_owned(),
                    )
                    .col(boolean(QueueControl::Paused).default(false).to_owned())
                    .col(boolean(QueueControl::HardStopped).default(false).to_owned())
                    .col(text(QueueControl::LastPolicy).default("review").to_owned())
                    .col(integer(QueueControl::MaxDepth))
                    .col(timestamp_with_time_zone(QueueControl::UpdatedAt))
                    .to_owned(),
            )
            .await?;

        let conn = manager.get_connection();

        // Partial unique indexes have no `sea_query` builder, so both are raw
        // SQL. The predicates are the authoritative ones from 1.4.
        //
        // Only one *push* per path may be in flight (ADR-TP-10): a second
        // one is necessarily non-fast-forward. `merge` and `attach` rows are
        // exempt — their correctness comes from the queue's total order and
        // B3's re-read under the advisory lock.
        conn.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS push_queue_active_push_path \
             ON push_queue(path) WHERE status IN ('Queued','Running') AND kind = 'push'",
        )
        .await?;

        // At most one non-terminal-or-done row per logical operation. B1's
        // conditional INSERT is the authoritative check; this index is the
        // concurrency backstop. Conflict requeues are unaffected: the original
        // row is already terminal, so it is outside the three states.
        conn.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS push_queue_operation_states \
             ON push_queue(kind, path, operation_id) \
             WHERE status IN ('Queued','Running','Done')",
        )
        .await?;

        conn.execute_unprepared(SEED_QUEUE_CONTROL).await?;

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum PushQueue {
    Table,
    Id,
    Kind,
    OperationId,
    Path,
    OldId,
    NewId,
    Payload,
    LandedCommitId,
    Status,
    Requester,
    FailureType,
    ErrorMessage,
    HeartbeatAt,
    SupersededBy,
    ExpectedCommitHash,
    ExpectedTreeHash,
    PendingAction,
    EnqueuedAt,
    StartedAt,
    FinishedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum QueueControl {
    Table,
    Id,
    Paused,
    HardStopped,
    LastPolicy,
    MaxDepth,
    UpdatedAt,
}

// The `#[iden]` labels below are the PostgreSQL enum labels, and they are what
// `sea_orm(string_value = ...)` in `sea_orm_active_enums` must match. They are
// spelled out rather than derived because 1.4 mixes casings deliberately:
// the lifecycle/failure/intent labels are PascalCase, `kind` is lowercase.
#[derive(Iden, EnumIter)]
enum PushQueueStatus {
    #[iden = "Queued"]
    Queued,
    #[iden = "Running"]
    Running,
    #[iden = "Done"]
    Done,
    #[iden = "Failed"]
    Failed,
    #[iden = "Cancelled"]
    Cancelled,
}

#[derive(Iden, EnumIter)]
enum PushQueueKind {
    #[iden = "push"]
    Push,
    #[iden = "merge"]
    Merge,
    #[iden = "attach"]
    Attach,
}

#[derive(Iden, EnumIter)]
enum PushQueueFailure {
    #[iden = "PushFailure"]
    PushFailure,
    #[iden = "MergeFailure"]
    MergeFailure,
    #[iden = "AttachFailure"]
    AttachFailure,
    #[iden = "Conflict"]
    Conflict,
    #[iden = "QueueBypassDetected"]
    QueueBypassDetected,
    #[iden = "WaitTimeout"]
    WaitTimeout,
    #[iden = "ClaimLost"]
    ClaimLost,
    #[iden = "SystemError"]
    SystemError,
}

#[derive(Iden, EnumIter)]
enum PushQueuePending {
    #[iden = "requeue_conflict"]
    RequeueConflict,
}
