use sea_orm::{
    ConnectionTrait, DatabaseTransaction, DbBackend, Statement, TransactionTrait, Value,
};
use tracing::error;

use crate::{
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    jupiter::storage::{
        base_storage::StorageConnector,
        view_storage::{ViewLock, ViewLockMode, ViewStorage, acquire_view_lock},
    },
};

const ROOT_CHAIN_SEGMENT_ROWS: usize = 10_000;

/// A self-contained expression readers can embed in their snapshot query.
/// It preserves the ordered root-chain decision: an anchored scan row is
/// halted only when it is no longer the chain tail; otherwise the three
/// terminal scan shapes are checked in order.
pub(crate) const ROOT_CHAIN_HALTED_SQL: &str = r#"(
    COALESCE((
        SELECT CASE
            WHEN EXISTS (
                SELECT 1 FROM mega_view_root_chain chain
                WHERE chain.commit_id = scan.commit_id
            ) THEN NOT EXISTS (
                SELECT 1 FROM mega_view_root_chain tail
                WHERE tail.commit_id = scan.commit_id
                  AND tail.seq = (SELECT max(seq) FROM mega_view_root_chain)
            )
            WHEN scan.parent_count = 0 THEN EXISTS (
                SELECT 1 FROM mega_view_root_chain
            )
            WHEN scan.parent_count > 1 THEN TRUE
            WHEN NOT EXISTS (
                SELECT 1 FROM mega_commit parent
                WHERE parent.commit_id = scan.first_parent
            ) THEN TRUE
            ELSE FALSE
        END
        FROM (
            SELECT commit_id, parent_count, first_parent
            FROM mega_view_root_chain_scan
            ORDER BY pos DESC
            LIMIT 1
        ) scan
    ), FALSE)
)"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscontinuityReason {
    RolledBack,
    Forked,
    UnrelatedHistory,
    MultiParent,
    MissingFirstParent,
    RowConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootChainOutcome {
    CaughtUp,
    NotCaughtUp,
    Discontinuous(DiscontinuityReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScanRow {
    pos: i64,
    commit_id: String,
    tree_id: String,
    parent_count: i16,
    first_parent: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootChainRow {
    scan_pos: i64,
    seq: i64,
    commit_id: String,
    tree_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Discontinuity {
    reason: DiscontinuityReason,
    commit_id: String,
}

#[derive(Clone, Debug)]
struct ChainTail {
    seq: i64,
    commit_id: String,
}

enum ScanState {
    Connected { tail: Option<ChainTail> },
    ScanMore { first_parent: String },
    Discontinuous(Discontinuity),
}

impl ViewStorage {
    /// Evaluates the persisted root-chain stop condition without taking the
    /// root-chain advisory lock. Callers may embed [`ROOT_CHAIN_HALTED_SQL`]
    /// in a larger read snapshot when they also need other state.
    pub(crate) async fn root_chain_halted(&self) -> Result<bool, MegaError> {
        let row = self
            .get_connection()
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT {ROOT_CHAIN_HALTED_SQL} AS halted"),
            ))
            .await?
            .ok_or_else(|| {
                MegaError::Other("root-chain halted query returned no row".to_owned())
            })?;
        Ok(row.try_get("", "halted")?)
    }

    pub async fn extend_root_chain(
        &self,
        budget: Option<usize>,
        batch_size: usize,
        lock_mode: ViewLockMode,
    ) -> Result<RootChainOutcome, MegaError> {
        self.extend_root_chain_inner(budget, batch_size, lock_mode, ROOT_CHAIN_SEGMENT_ROWS)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn extend_root_chain_with_segment_rows(
        &self,
        budget: Option<usize>,
        batch_size: usize,
        lock_mode: ViewLockMode,
        segment_limit: usize,
    ) -> Result<RootChainOutcome, MegaError> {
        self.extend_root_chain_inner(budget, batch_size, lock_mode, segment_limit)
            .await
    }

    async fn extend_root_chain_inner(
        &self,
        budget: Option<usize>,
        batch_size: usize,
        lock_mode: ViewLockMode,
        segment_limit: usize,
    ) -> Result<RootChainOutcome, MegaError> {
        if batch_size == 0 || segment_limit == 0 {
            return Err(MegaError::Other(
                "root-chain batch and segment sizes must be non-zero".to_owned(),
            ));
        }

        let mut remaining = budget;
        loop {
            let txn = self.get_connection().begin().await?;
            if !acquire_view_lock(&txn, ViewLock::RootChain, lock_mode).await? {
                let _ = txn.rollback().await;
                return Ok(RootChainOutcome::NotCaughtUp);
            }

            let anchor = scan_anchor(&txn).await?;
            let outcome = match anchor {
                None => {
                    let root = main_root(&txn).await?;
                    let tail = chain_tail(&txn).await?;
                    if tail
                        .as_ref()
                        .is_some_and(|tail| tail.commit_id == root.commit_id)
                    {
                        txn.commit().await?;
                        return Ok(self.finish_outcome(RootChainOutcome::CaughtUp));
                    }
                    if exhausted(remaining) {
                        txn.rollback().await?;
                        return Ok(RootChainOutcome::NotCaughtUp);
                    }
                    insert_scan_rows(&txn, 1, std::slice::from_ref(&root)).await?;
                    consume(&mut remaining, 1);
                    txn.commit().await?;
                    RootChainOutcome::NotCaughtUp
                }
                Some(anchor) => match classify_anchor(&txn, &anchor).await? {
                    ScanState::Discontinuous(discontinuity) => {
                        txn.rollback().await?;
                        return Ok(self.finish_discontinuity(discontinuity));
                    }
                    ScanState::ScanMore { first_parent } => {
                        if exhausted(remaining) {
                            txn.rollback().await?;
                            return Ok(RootChainOutcome::NotCaughtUp);
                        }
                        let take = remaining.map_or(batch_size, |left| left.min(batch_size));
                        let rows = scan_first_parents(&txn, &first_parent, take).await?;
                        if rows.is_empty() {
                            txn.rollback().await?;
                            return Ok(self.finish_discontinuity(Discontinuity {
                                reason: DiscontinuityReason::MissingFirstParent,
                                commit_id: anchor.commit_id,
                            }));
                        }
                        insert_scan_rows(&txn, anchor.pos + 1, &rows).await?;
                        consume(&mut remaining, rows.len());
                        txn.commit().await?;
                        RootChainOutcome::NotCaughtUp
                    }
                    ScanState::Connected { tail } => {
                        if exhausted(remaining) {
                            txn.rollback().await?;
                            return Ok(RootChainOutcome::NotCaughtUp);
                        }
                        let take = remaining.map_or(segment_limit, |left| left.min(segment_limit));
                        let cold_start = tail.is_none();
                        let rows =
                            segment_rows(&txn, &anchor, tail.as_ref(), cold_start, take).await?;
                        if rows.is_empty() {
                            txn.execute_unprepared("DELETE FROM mega_view_root_chain_scan")
                                .await?;
                            txn.commit().await?;
                            return Ok(self.finish_outcome(RootChainOutcome::CaughtUp));
                        }
                        if let Some(discontinuity) = insert_segment(&txn, &rows).await? {
                            txn.rollback().await?;
                            return Ok(self.finish_discontinuity(discontinuity));
                        }
                        let lowest_scan_pos =
                            rows.iter().map(|row| row.scan_pos).min().ok_or_else(|| {
                                MegaError::Other(
                                    "root-chain segment unexpectedly has no rows".to_owned(),
                                )
                            })?;
                        if lowest_scan_pos == 1 {
                            txn.execute_unprepared("DELETE FROM mega_view_root_chain_scan")
                                .await?;
                        } else {
                            txn.execute_raw(Statement::from_sql_and_values(
                                DbBackend::Postgres,
                                "DELETE FROM mega_view_root_chain_scan WHERE pos > $1",
                                [Value::from(lowest_scan_pos)],
                            ))
                            .await?;
                        }
                        consume(&mut remaining, rows.len());
                        txn.commit().await?;
                        if lowest_scan_pos == 1 {
                            RootChainOutcome::CaughtUp
                        } else {
                            RootChainOutcome::NotCaughtUp
                        }
                    }
                },
            };

            if matches!(outcome, RootChainOutcome::CaughtUp) {
                return Ok(self.finish_outcome(outcome));
            }
            // A scan batch may have written the terminal row. Re-enter the
            // decision step before honoring an exhausted budget so a terminal
            // discontinuity is reported by the call that discovered it.
        }
    }

    fn finish_outcome(&self, outcome: RootChainOutcome) -> RootChainOutcome {
        if matches!(outcome, RootChainOutcome::CaughtUp) {
            self.clear_discontinuity_alert();
        }
        outcome
    }

    fn finish_discontinuity(&self, discontinuity: Discontinuity) -> RootChainOutcome {
        if self.should_log_discontinuity(discontinuity.reason, discontinuity.commit_id.clone()) {
            error!(
                reason = ?discontinuity.reason,
                commit_id = %discontinuity.commit_id,
                "view root chain discontinuity"
            );
        }
        RootChainOutcome::Discontinuous(discontinuity.reason)
    }
}

fn exhausted(remaining: Option<usize>) -> bool {
    matches!(remaining, Some(0))
}

fn consume(remaining: &mut Option<usize>, count: usize) {
    if let Some(remaining) = remaining {
        *remaining = remaining.saturating_sub(count);
    }
}

async fn scan_anchor(txn: &DatabaseTransaction) -> Result<Option<ScanRow>, MegaError> {
    let row = txn
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT pos, commit_id, tree_id, parent_count, first_parent \
             FROM mega_view_root_chain_scan ORDER BY pos DESC LIMIT 1"
                .to_owned(),
        ))
        .await?;
    row.map(scan_row).transpose()
}

async fn chain_tail(txn: &DatabaseTransaction) -> Result<Option<ChainTail>, MegaError> {
    let row = txn
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT seq, commit_id FROM mega_view_root_chain ORDER BY seq DESC LIMIT 1".to_owned(),
        ))
        .await?;
    row.map(|row| {
        Ok(ChainTail {
            seq: row.try_get("", "seq")?,
            commit_id: row.try_get("", "commit_id")?,
        })
    })
    .transpose()
}

async fn main_root(txn: &DatabaseTransaction) -> Result<ScanRow, MegaError> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT ref_commit_hash FROM mega_refs WHERE path = '/' AND ref_name = $1",
            [Value::from(MEGA_BRANCH_NAME.to_owned())],
        ))
        .await?
        .ok_or_else(|| MegaError::Other("main root ref is missing".to_owned()))?;
    let commit_id: String = row.try_get("", "ref_commit_hash")?;
    commit_row(txn, &commit_id)
        .await?
        .ok_or_else(|| MegaError::Other(format!("main root commit {commit_id} is missing")))
        .map(|mut row| {
            row.pos = 1;
            row
        })
}

async fn commit_row(
    txn: &DatabaseTransaction,
    commit_id: &str,
) -> Result<Option<ScanRow>, MegaError> {
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 0::bigint AS pos, commit_id, tree AS tree_id, json_array_length(parents_id)::smallint AS parent_count, \
             CASE WHEN json_array_length(parents_id) = 0 THEN NULL ELSE parents_id ->> 0 END AS first_parent \
             FROM mega_commit WHERE commit_id = $1",
            [Value::from(commit_id.to_owned())],
        ))
        .await?;
    row.map(|row| {
        let mut scan = scan_row(row)?;
        scan.pos = 0;
        Ok(scan)
    })
    .transpose()
}

fn scan_row(row: sea_orm::QueryResult) -> Result<ScanRow, MegaError> {
    Ok(ScanRow {
        pos: row.try_get("", "pos")?,
        commit_id: row.try_get("", "commit_id")?,
        tree_id: row.try_get("", "tree_id")?,
        parent_count: row.try_get("", "parent_count")?,
        first_parent: row.try_get("", "first_parent")?,
    })
}

async fn classify_anchor(
    txn: &DatabaseTransaction,
    anchor: &ScanRow,
) -> Result<ScanState, MegaError> {
    let chain_row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT seq FROM mega_view_root_chain WHERE commit_id = $1",
            [Value::from(anchor.commit_id.clone())],
        ))
        .await?;
    let tail = chain_tail(txn).await?;
    if chain_row.is_some() {
        return Ok(match tail {
            Some(tail) if tail.commit_id == anchor.commit_id => {
                ScanState::Connected { tail: Some(tail) }
            }
            _ if anchor.pos == 1 => ScanState::Discontinuous(Discontinuity {
                reason: DiscontinuityReason::RolledBack,
                commit_id: anchor.commit_id.clone(),
            }),
            _ => ScanState::Discontinuous(Discontinuity {
                reason: DiscontinuityReason::Forked,
                commit_id: anchor.commit_id.clone(),
            }),
        });
    }
    if anchor.parent_count == 0 {
        return Ok(match tail {
            None => ScanState::Connected { tail: None },
            Some(_) => ScanState::Discontinuous(Discontinuity {
                reason: DiscontinuityReason::UnrelatedHistory,
                commit_id: anchor.commit_id.clone(),
            }),
        });
    }
    if anchor.parent_count > 1 {
        return Ok(ScanState::Discontinuous(Discontinuity {
            reason: DiscontinuityReason::MultiParent,
            commit_id: anchor.commit_id.clone(),
        }));
    }
    let Some(first_parent) = &anchor.first_parent else {
        return Ok(ScanState::Discontinuous(Discontinuity {
            reason: DiscontinuityReason::MissingFirstParent,
            commit_id: anchor.commit_id.clone(),
        }));
    };
    if commit_row(txn, first_parent).await?.is_none() {
        return Ok(ScanState::Discontinuous(Discontinuity {
            reason: DiscontinuityReason::MissingFirstParent,
            commit_id: anchor.commit_id.clone(),
        }));
    }
    Ok(ScanState::ScanMore {
        first_parent: first_parent.clone(),
    })
}

async fn scan_first_parents(
    txn: &DatabaseTransaction,
    first_parent: &str,
    limit: usize,
) -> Result<Vec<ScanRow>, MegaError> {
    let rows = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "WITH RECURSIVE walk AS ( \
                SELECT commit_id, tree AS tree_id, json_array_length(parents_id)::smallint AS parent_count, \
                       CASE WHEN json_array_length(parents_id) = 0 THEN NULL ELSE parents_id ->> 0 END AS first_parent, \
                       1::bigint AS depth \
                FROM mega_commit WHERE commit_id = $1 \
                UNION ALL \
                SELECT next.commit_id, next.tree, json_array_length(next.parents_id)::smallint, \
                       CASE WHEN json_array_length(next.parents_id) = 0 THEN NULL ELSE next.parents_id ->> 0 END, \
                       walk.depth + 1 \
                FROM walk JOIN mega_commit next ON next.commit_id = walk.first_parent \
                WHERE walk.parent_count = 1 \
                  AND walk.depth < $2 \
                  AND NOT EXISTS (SELECT 1 FROM mega_view_root_chain chain WHERE chain.commit_id = walk.commit_id) \
             ) \
             SELECT commit_id, tree_id, parent_count, first_parent FROM walk ORDER BY depth LIMIT $2",
            [Value::from(first_parent.to_owned()), Value::from(limit as i64)],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(ScanRow {
                pos: 0,
                commit_id: row.try_get("", "commit_id")?,
                tree_id: row.try_get("", "tree_id")?,
                parent_count: row.try_get("", "parent_count")?,
                first_parent: row.try_get("", "first_parent")?,
            })
        })
        .collect()
}

async fn insert_scan_rows(
    txn: &DatabaseTransaction,
    first_pos: i64,
    rows: &[ScanRow],
) -> Result<(), MegaError> {
    for (offset, row) in rows.iter().enumerate() {
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO mega_view_root_chain_scan \
             (pos, commit_id, tree_id, parent_count, first_parent) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            [
                Value::from(first_pos + offset as i64),
                Value::from(row.commit_id.clone()),
                Value::from(row.tree_id.clone()),
                Value::from(row.parent_count),
                Value::from(row.first_parent.clone()),
            ],
        ))
        .await?;
    }
    Ok(())
}

async fn segment_rows(
    txn: &DatabaseTransaction,
    anchor: &ScanRow,
    tail: Option<&ChainTail>,
    cold_start: bool,
    limit: usize,
) -> Result<Vec<RootChainRow>, MegaError> {
    let predicate = if cold_start { "pos <= $1" } else { "pos < $1" };
    let statement = Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!(
            "SELECT pos, commit_id, tree_id FROM mega_view_root_chain_scan \
             WHERE {predicate} ORDER BY pos DESC LIMIT $2"
        ),
        [Value::from(anchor.pos), Value::from(limit as i64)],
    );
    let rows = txn.query_all_raw(statement).await?;
    rows.into_iter()
        .map(|row| {
            let pos: i64 = row.try_get("", "pos")?;
            let seq = match tail {
                Some(tail) => tail.seq + (anchor.pos - pos),
                None => anchor.pos - pos + 1,
            };
            Ok(RootChainRow {
                scan_pos: pos,
                seq,
                commit_id: row.try_get("", "commit_id")?,
                tree_id: row.try_get("", "tree_id")?,
            })
        })
        .collect()
}

async fn insert_segment(
    txn: &DatabaseTransaction,
    rows: &[RootChainRow],
) -> Result<Option<Discontinuity>, MegaError> {
    let mut inserted = 0;
    for row in rows {
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "INSERT INTO mega_view_root_chain (seq, commit_id, tree_id) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                [
                    Value::from(row.seq),
                    Value::from(row.commit_id.clone()),
                    Value::from(row.tree_id.clone()),
                ],
            ))
            .await?;
        inserted += result.rows_affected() as usize;
    }
    if inserted == rows.len() {
        return Ok(None);
    }

    let mut clauses = Vec::with_capacity(rows.len());
    let mut values = Vec::with_capacity(rows.len() * 2);
    for (index, row) in rows.iter().enumerate() {
        let seq_index = index * 2 + 1;
        let commit_index = seq_index + 1;
        clauses.push(format!(
            "(seq = ${seq_index} OR commit_id = ${commit_index})"
        ));
        values.push(Value::from(row.seq));
        values.push(Value::from(row.commit_id.clone()));
    }
    let existing = txn
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT seq, commit_id FROM mega_view_root_chain WHERE {}",
                clauses.join(" OR ")
            ),
            values,
        ))
        .await?;
    for existing in existing {
        let seq: i64 = existing.try_get("", "seq")?;
        let commit_id: String = existing.try_get("", "commit_id")?;
        if !rows
            .iter()
            .any(|row| row.seq == seq && row.commit_id == commit_id)
        {
            let conflict = rows
                .iter()
                .find(|row| row.seq == seq || row.commit_id == commit_id)
                .ok_or_else(|| {
                    MegaError::Other(
                        "root-chain conflict query returned a row outside the segment".to_owned(),
                    )
                })?;
            return Ok(Some(Discontinuity {
                reason: DiscontinuityReason::RowConflict,
                commit_id: conflict.commit_id.clone(),
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use git_internal::{hash::HashKind, internal::object::blob::Blob};
    use sea_orm::{
        ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait, Value,
    };

    use super::*;
    use crate::{
        common::errors::MegaError,
        config::DbConfig,
        jupiter::{
            migration::apply_migrations,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                init::database_connection,
                view_storage::{VIEW_LOCK_TIMEOUT, ViewLock, acquire_view_lock},
                view_test_fixtures::{
                    RootCommitFixture, cas_fixture_main, root_tree_from_paths,
                    seed_linear_root_history, seed_missing_first_parent_root_commit_with_tree,
                    seed_multi_parent_root_commit_with_tree,
                    seed_single_parent_root_commit_with_tree,
                    seed_unrelated_root_history_with_tree,
                },
            },
            tests::{TestSchemaGuard, test_db_config, test_db_connection, test_storage},
        },
    };

    fn fixture_tree(label: &str) -> crate::jupiter::storage::view_test_fixtures::RootTreeFixture {
        root_tree_from_paths(
            HashKind::Sha1,
            &[(
                format!("{label}.txt"),
                format!("root-chain fixture {label}").into_bytes(),
            )],
        )
    }

    #[derive(Clone, Copy, Debug)]
    enum RootDiscontinuityCase {
        RolledBack,
        Forked,
        UnrelatedHistory,
        MultiParent,
        MissingFirstParent,
    }

    impl RootDiscontinuityCase {
        const ALL: [Self; 5] = [
            Self::RolledBack,
            Self::Forked,
            Self::UnrelatedHistory,
            Self::MultiParent,
            Self::MissingFirstParent,
        ];
    }

    async fn append_history(
        db: &DatabaseConnection,
        parent: &RootCommitFixture,
        count: usize,
        label: &str,
    ) -> Vec<RootCommitFixture> {
        let mut parent = parent.clone();
        let mut commits = Vec::with_capacity(count);
        for number in 1..=count {
            let commit = seed_single_parent_root_commit_with_tree(
                db,
                HashKind::Sha1,
                fixture_tree(&format!("{label}-{number}")),
                &parent,
                &format!("fixture root-chain {label} {number}"),
            )
            .await;
            parent = commit.clone();
            commits.push(commit);
        }
        commits
    }

    async fn prepare_discontinuity(
        db: &DatabaseConnection,
        storage: &ViewStorage,
        case: RootDiscontinuityCase,
        history_len: usize,
    ) -> (
        Vec<RootCommitFixture>,
        RootCommitFixture,
        DiscontinuityReason,
    ) {
        assert!(
            history_len >= 3,
            "a discontinuity fixture needs three commits"
        );
        let history = seed_linear_root_history(db, history_len).await;
        run_to_caught_up(storage, history_len.saturating_mul(3)).await;
        let tail = history.last().unwrap().clone();
        let (active, expected) = match case {
            RootDiscontinuityCase::RolledBack => {
                (history[2].clone(), DiscontinuityReason::RolledBack)
            }
            RootDiscontinuityCase::Forked => {
                let fork = seed_single_parent_root_commit_with_tree(
                    db,
                    HashKind::Sha1,
                    fixture_tree("hp28-fork"),
                    &history[2],
                    "HP-28 fork",
                )
                .await;
                (fork, DiscontinuityReason::Forked)
            }
            RootDiscontinuityCase::UnrelatedHistory => {
                let root = seed_unrelated_root_history_with_tree(
                    db,
                    HashKind::Sha1,
                    fixture_tree("hp28-unrelated-root"),
                )
                .await;
                let extension = append_history(db, &root, history_len - 1, "hp28-unrelated").await;
                (
                    extension.last().unwrap().clone(),
                    DiscontinuityReason::UnrelatedHistory,
                )
            }
            RootDiscontinuityCase::MultiParent => {
                let merge = seed_multi_parent_root_commit_with_tree(
                    db,
                    HashKind::Sha1,
                    fixture_tree("hp28-merge"),
                    &tail,
                    &history[history_len - 2],
                )
                .await;
                (merge, DiscontinuityReason::MultiParent)
            }
            RootDiscontinuityCase::MissingFirstParent => {
                let missing_parent = Blob::from_content_with_kind(HashKind::Sha1, "hp28-absent")
                    .unwrap()
                    .id;
                let missing = seed_missing_first_parent_root_commit_with_tree(
                    db,
                    HashKind::Sha1,
                    fixture_tree("hp28-missing"),
                    missing_parent,
                )
                .await;
                (missing, DiscontinuityReason::MissingFirstParent)
            }
        };
        assert!(cas_fixture_main(db, &tail, &active).await);
        assert_eq!(
            storage
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::Discontinuous(expected),
            "failed to prepare {case:?}"
        );
        (history, active, expected)
    }

    async fn clear_scan(db: &DatabaseConnection) {
        db.execute_unprepared("DELETE FROM mega_view_root_chain_scan")
            .await
            .unwrap();
    }

    async fn halted_from_sql(db: &DatabaseConnection) -> bool {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT {ROOT_CHAIN_HALTED_SQL} AS halted"),
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "halted").unwrap()
    }

    fn new_view_storage(db: Arc<DatabaseConnection>) -> ViewStorage {
        ViewStorage::new(BaseStorage::new(db))
    }

    async fn view_storage_with_history(
        commits: usize,
    ) -> (DatabaseConnection, ViewStorage, Vec<RootCommitFixture>) {
        let temp = tempfile::tempdir().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let history = seed_linear_root_history(&db, commits).await;
        let storage = ViewStorage::new(BaseStorage::new(Arc::new(db.clone())));
        (db, storage, history)
    }

    async fn configured_history(
        commits: usize,
    ) -> (
        DbConfig,
        TestSchemaGuard,
        Arc<DatabaseConnection>,
        Vec<RootCommitFixture>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let (config, schema) = test_db_config(temp.path()).await;
        let db = Arc::new(database_connection(&config).await.unwrap());
        let history = seed_linear_root_history(db.as_ref(), commits).await;
        (config, schema, db, history)
    }

    async fn configured_empty() -> (DbConfig, TestSchemaGuard, Arc<DatabaseConnection>) {
        let temp = tempfile::tempdir().unwrap();
        let (config, schema) = test_db_config(temp.path()).await;
        let db = Arc::new(database_connection(&config).await.unwrap());
        (config, schema, db)
    }

    async fn counted_empty() -> (
        DbConfig,
        TestSchemaGuard,
        Arc<DatabaseConnection>,
        Arc<AtomicUsize>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let (config, schema) = test_db_config(temp.path()).await;
        let counter = Arc::new(AtomicUsize::new(0));
        let callback_counter = counter.clone();
        let mut db = database_connection(&config).await.unwrap();
        db.set_metric_callback(move |_| {
            callback_counter.fetch_add(1, Ordering::Relaxed);
        });
        (config, schema, Arc::new(db), counter)
    }

    async fn open_view_storage(config: &DbConfig) -> (Arc<DatabaseConnection>, ViewStorage) {
        let db = Arc::new(database_connection(config).await.unwrap());
        let storage = new_view_storage(db.clone());
        (db, storage)
    }

    async fn close_view_storage(db: Arc<DatabaseConnection>, storage: ViewStorage) {
        drop(storage);
        let db = match Arc::try_unwrap(db) {
            Ok(db) => db,
            Err(_) => panic!("view storage must be the only owner of its connection pool"),
        };
        db.close().await.unwrap();
    }

    async fn root_chain(db: &DatabaseConnection) -> Vec<(i64, String, String)> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT seq, commit_id, tree_id FROM mega_view_root_chain ORDER BY seq".to_owned(),
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.try_get("", "seq").unwrap(),
                row.try_get("", "commit_id").unwrap(),
                row.try_get("", "tree_id").unwrap(),
            )
        })
        .collect()
    }

    async fn scan_chain(
        db: &DatabaseConnection,
    ) -> Vec<(i64, String, String, i16, Option<String>)> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT pos, commit_id, tree_id, parent_count, first_parent \
             FROM mega_view_root_chain_scan ORDER BY pos"
                .to_owned(),
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.try_get("", "pos").unwrap(),
                row.try_get("", "commit_id").unwrap(),
                row.try_get("", "tree_id").unwrap(),
                row.try_get("", "parent_count").unwrap(),
                row.try_get("", "first_parent").unwrap(),
            )
        })
        .collect()
    }

    fn expected_root_chain(commits: &[RootCommitFixture]) -> Vec<(i64, String, String)> {
        commits
            .iter()
            .enumerate()
            .map(|(index, commit)| {
                (
                    index as i64 + 1,
                    commit.commit.id.to_string(),
                    commit.commit.tree_id.to_string(),
                )
            })
            .collect()
    }

    async fn assert_root_chain_matches(db: &DatabaseConnection, commits: &[RootCommitFixture]) {
        assert_eq!(root_chain(db).await, expected_root_chain(commits));
    }

    async fn assert_scan_anchor_is_tail(db: &DatabaseConnection) {
        let scan = scan_chain(db).await;
        if let Some((_, commit_id, _, _, _)) = scan.last() {
            let tail = root_chain(db).await;
            assert_eq!(tail.last().map(|(_, id, _)| id), Some(commit_id));
        }
    }

    async fn run_to_caught_up(storage: &ViewStorage, max_calls: usize) {
        for _ in 0..max_calls {
            if storage
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap()
                == RootChainOutcome::CaughtUp
            {
                return;
            }
        }
        panic!("root chain did not catch up in {max_calls} calls");
    }

    async fn prepare_segment_history(
        observer: &Arc<DatabaseConnection>,
        incremental: bool,
    ) -> Vec<RootCommitFixture> {
        let mut expected =
            seed_linear_root_history(observer.as_ref(), if incremental { 6 } else { 11 }).await;
        if incremental {
            let seed_storage = new_view_storage(observer.clone());
            run_to_caught_up(&seed_storage, 20).await;
            let previous_tip = expected.last().unwrap().clone();
            let extension =
                append_history(observer.as_ref(), &previous_tip, 10, "incremental").await;
            assert!(
                cas_fixture_main(observer.as_ref(), &previous_tip, extension.last().unwrap()).await
            );
            expected.extend(extension);
        }
        expected
    }

    async fn insert_root_chain_row(
        txn: &sea_orm::DatabaseTransaction,
        seq: i64,
        commit_id: &str,
        tree_id: &str,
    ) {
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO mega_view_root_chain (seq, commit_id, tree_id) VALUES ($1, $2, $3)",
            [
                Value::from(seq),
                Value::from(commit_id.to_owned()),
                Value::from(tree_id.to_owned()),
            ],
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn linear_chain_matches_first_parent() {
        let (db, storage, mut expected) = view_storage_with_history(21).await;
        assert_eq!(
            storage
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        assert_root_chain_matches(&db, &expected).await;

        let previous_tip = expected.last().unwrap().clone();
        let extension = append_history(&db, &previous_tip, 5, "linear-increment").await;
        assert!(cas_fixture_main(&db, &previous_tip, extension.last().unwrap()).await);
        expected.extend(extension);
        assert_eq!(
            storage
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        assert_root_chain_matches(&db, &expected).await;
    }

    #[tokio::test]
    async fn budget_exhausted_then_resumed() {
        for incremental in [false, true] {
            let (_config, _schema, db, mut expected) =
                configured_history(if incremental { 6 } else { 21 }).await;
            let storage = new_view_storage(db.clone());
            if incremental {
                run_to_caught_up(&storage, 20).await;
                let previous_tip = expected.last().unwrap().clone();
                let extension =
                    append_history(db.as_ref(), &previous_tip, 20, "budget-increment").await;
                assert!(
                    cas_fixture_main(db.as_ref(), &previous_tip, extension.last().unwrap()).await
                );
                expected.extend(extension);
            }

            for call in 1..=9 {
                let roots_before = root_chain(db.as_ref()).await.len();
                let scan_before = scan_chain(db.as_ref()).await.len();
                let outcome = storage
                    .extend_root_chain(Some(5), 5, ViewLockMode::Try)
                    .await
                    .unwrap();
                let roots_after = root_chain(db.as_ref()).await.len();
                let scan_after = scan_chain(db.as_ref()).await.len();
                assert!(roots_after - roots_before <= 5);
                if roots_after == roots_before {
                    assert!(scan_after - scan_before <= 5);
                }
                assert_eq!(
                    outcome,
                    if call == 9 {
                        RootChainOutcome::CaughtUp
                    } else {
                        RootChainOutcome::NotCaughtUp
                    }
                );
            }
            assert_root_chain_matches(db.as_ref(), &expected).await;
            drop(storage);
            drop(db);

            let (config, _schema, first_db, mut expected) =
                configured_history(if incremental { 6 } else { 21 }).await;
            let first_storage = new_view_storage(first_db.clone());
            if incremental {
                run_to_caught_up(&first_storage, 20).await;
                let previous_tip = expected.last().unwrap().clone();
                let extension =
                    append_history(first_db.as_ref(), &previous_tip, 20, "restart-increment").await;
                assert!(
                    cas_fixture_main(first_db.as_ref(), &previous_tip, extension.last().unwrap())
                        .await
                );
                expected.extend(extension);
            }
            let roots_before = root_chain(first_db.as_ref()).await;
            assert_eq!(
                first_storage
                    .extend_root_chain(Some(5), 5, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::NotCaughtUp
            );
            assert!(!scan_chain(first_db.as_ref()).await.is_empty());
            assert_eq!(root_chain(first_db.as_ref()).await, roots_before);
            close_view_storage(first_db, first_storage).await;

            let resumed_db = Arc::new(database_connection(&config).await.unwrap());
            let resumed_storage = new_view_storage(resumed_db.clone());
            assert_eq!(
                resumed_storage
                    .extend_root_chain(None, 5, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::CaughtUp
            );
            assert_root_chain_matches(resumed_db.as_ref(), &expected).await;
            close_view_storage(resumed_db, resumed_storage).await;
        }
    }

    #[tokio::test]
    async fn segment_interleavings_equivalent() {
        for incremental in [false, true] {
            let (config, _schema, observer) = configured_empty().await;
            let expected = prepare_segment_history(&observer, incremental).await;
            let (mut connection, mut storage) = open_view_storage(&config).await;
            let mut insertions = Vec::new();
            for _ in 0..40 {
                let before = root_chain(observer.as_ref()).await.len();
                let outcome = storage
                    .extend_root_chain_with_segment_rows(Some(3), 3, ViewLockMode::Try, 3)
                    .await
                    .unwrap();
                let after = root_chain(observer.as_ref()).await.len();
                if after > before {
                    insertions.push(after - before);
                    assert_scan_anchor_is_tail(observer.as_ref()).await;
                    let previous = (connection, storage);
                    close_view_storage(previous.0, previous.1).await;
                    (connection, storage) = open_view_storage(&config).await;
                }
                if outcome == RootChainOutcome::CaughtUp {
                    break;
                }
            }
            assert_eq!(
                insertions,
                if incremental {
                    vec![1, 3, 3, 3]
                } else {
                    vec![1, 3, 3, 3, 1]
                }
            );
            assert_root_chain_matches(observer.as_ref(), &expected).await;
            assert!(scan_chain(observer.as_ref()).await.is_empty());
            close_view_storage(connection, storage).await;

            let (config, _schema, observer) = configured_empty().await;
            let expected = prepare_segment_history(&observer, incremental).await;
            let (left_connection, left) = open_view_storage(&config).await;
            let (right_connection, right) = open_view_storage(&config).await;
            let mut use_left = true;
            let mut insertions = Vec::new();
            for _ in 0..40 {
                let before = root_chain(observer.as_ref()).await.len();
                let outcome = if use_left {
                    left.extend_root_chain_with_segment_rows(Some(3), 3, ViewLockMode::Try, 3)
                        .await
                        .unwrap()
                } else {
                    right
                        .extend_root_chain_with_segment_rows(Some(3), 3, ViewLockMode::Blocking, 3)
                        .await
                        .unwrap()
                };
                let after = root_chain(observer.as_ref()).await.len();
                if after > before {
                    insertions.push(after - before);
                    assert_scan_anchor_is_tail(observer.as_ref()).await;
                    use_left = !use_left;
                }
                if outcome == RootChainOutcome::CaughtUp {
                    break;
                }
            }
            assert_eq!(
                insertions,
                if incremental {
                    vec![1, 3, 3, 3]
                } else {
                    vec![1, 3, 3, 3, 1]
                }
            );
            assert_root_chain_matches(observer.as_ref(), &expected).await;
            close_view_storage(left_connection, left).await;
            close_view_storage(right_connection, right).await;

            let (config, _schema, observer) = configured_empty().await;
            let mut expected = prepare_segment_history(&observer, incremental).await;
            let (left_connection, left) = open_view_storage(&config).await;
            let (right_connection, right) = open_view_storage(&config).await;
            let mut use_left = true;
            let mut completed_segments = 0;
            let mut advanced_main = false;
            for _ in 0..40 {
                let before = root_chain(observer.as_ref()).await.len();
                let outcome = if use_left {
                    left.extend_root_chain_with_segment_rows(Some(3), 3, ViewLockMode::Try, 3)
                        .await
                        .unwrap()
                } else {
                    right
                        .extend_root_chain_with_segment_rows(Some(3), 3, ViewLockMode::Try, 3)
                        .await
                        .unwrap()
                };
                use_left = !use_left;
                let after = root_chain(observer.as_ref()).await.len();
                if after > before {
                    assert!(after - before <= 3);
                    assert_scan_anchor_is_tail(observer.as_ref()).await;
                    completed_segments += 1;
                    if completed_segments == 2 {
                        let previous_tip = expected.last().unwrap().clone();
                        let extension =
                            append_history(observer.as_ref(), &previous_tip, 4, "advance-main")
                                .await;
                        assert!(
                            cas_fixture_main(
                                observer.as_ref(),
                                &previous_tip,
                                extension.last().unwrap()
                            )
                            .await
                        );
                        expected.extend(extension);
                        advanced_main = true;
                    }
                }
                if outcome == RootChainOutcome::CaughtUp
                    && root_chain(observer.as_ref()).await.last().is_some_and(
                        |(_, commit_id, _)| {
                            commit_id == &expected.last().unwrap().commit.id.to_string()
                        },
                    )
                {
                    break;
                }
            }
            assert!(advanced_main);
            assert_root_chain_matches(observer.as_ref(), &expected).await;
            assert!(scan_chain(observer.as_ref()).await.is_empty());
            close_view_storage(left_connection, left).await;
            close_view_storage(right_connection, right).await;
        }
    }

    #[tokio::test]
    async fn discontinuities_classified() {
        use tracing_subscriber::{Layer, filter::filter_fn, fmt::MakeWriter, layer::SubscriberExt};

        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for Capture {
            type Writer = Capture;

            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let _pin_registry = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let captured = Arc::new(Mutex::new(Vec::new()));
        let target = module_path!().replace("::tests", "");
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(Capture(captured.clone()))
                .with_ansi(false)
                .with_filter(filter_fn(move |metadata| {
                    metadata.level() == &tracing::Level::ERROR && metadata.target() == target
                })),
        );
        let _capture = tracing::subscriber::set_default(subscriber);
        let error_events = || -> Vec<String> {
            String::from_utf8(captured.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter(|line| line.contains("view root chain discontinuity"))
                .map(str::to_owned)
                .collect()
        };

        for case in ["rollback", "fork", "unrelated", "multi", "missing"] {
            let temp = tempfile::tempdir().unwrap();
            let storage = test_storage(temp.path()).await;
            let db = storage.view_storage().get_connection().clone();
            let history = seed_linear_root_history(&db, 6).await;
            assert_eq!(
                storage
                    .view_storage()
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::CaughtUp
            );
            let expected = match case {
                "rollback" => {
                    assert!(cas_fixture_main(&db, &history[5], &history[2]).await);
                    (
                        DiscontinuityReason::RolledBack,
                        history[2].commit.id.to_string(),
                    )
                }
                "fork" => {
                    let fork = seed_single_parent_root_commit_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("fork"),
                        &history[2],
                        "fixture fork",
                    )
                    .await;
                    assert!(cas_fixture_main(&db, &history[5], &fork).await);
                    (
                        DiscontinuityReason::Forked,
                        history[2].commit.id.to_string(),
                    )
                }
                "unrelated" => {
                    let root = seed_unrelated_root_history_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("unrelated"),
                    )
                    .await;
                    let tip = seed_single_parent_root_commit_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("unrelated-child"),
                        &root,
                        "fixture unrelated child",
                    )
                    .await;
                    assert!(cas_fixture_main(&db, &history[5], &tip).await);
                    (
                        DiscontinuityReason::UnrelatedHistory,
                        root.commit.id.to_string(),
                    )
                }
                "multi" => {
                    let merge = seed_multi_parent_root_commit_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("merge"),
                        &history[5],
                        &history[4],
                    )
                    .await;
                    assert!(cas_fixture_main(&db, &history[5], &merge).await);
                    (
                        DiscontinuityReason::MultiParent,
                        merge.commit.id.to_string(),
                    )
                }
                "missing" => {
                    let missing_parent = Blob::from_content_with_kind(HashKind::Sha1, "absent")
                        .unwrap()
                        .id;
                    let missing = seed_missing_first_parent_root_commit_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("missing"),
                        missing_parent,
                    )
                    .await;
                    assert!(cas_fixture_main(&db, &history[5], &missing).await);
                    (
                        DiscontinuityReason::MissingFirstParent,
                        missing.commit.id.to_string(),
                    )
                }
                _ => unreachable!(),
            };
            let roots_before = root_chain(&db).await;
            let events_before = error_events().len();
            assert_eq!(
                storage
                    .view_storage()
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::Discontinuous(expected.0)
            );
            assert_eq!(root_chain(&db).await, roots_before);
            let scan_after_first = scan_chain(&db).await;
            let events_after_first = error_events();
            assert_eq!(events_after_first.len(), events_before + 1);
            let line = events_after_first.last().unwrap();
            assert!(line.contains(&format!("reason={:?}", expected.0)), "{line}");
            assert!(
                line.contains(&format!("commit_id={}", expected.1)),
                "{line}"
            );
            assert_eq!(
                storage
                    .view_storage()
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::Discontinuous(expected.0)
            );
            assert_eq!(root_chain(&db).await, roots_before);
            assert_eq!(scan_chain(&db).await, scan_after_first);
            assert_eq!(error_events().len(), events_before + 1);

            if case == "rollback" {
                assert!(cas_fixture_main(&db, &history[2], &history[5]).await);
                db.execute_unprepared("DELETE FROM mega_view_root_chain_scan")
                    .await
                    .unwrap();
                assert_eq!(
                    storage
                        .view_storage()
                        .extend_root_chain(None, 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::CaughtUp
                );
                assert!(cas_fixture_main(&db, &history[5], &history[2]).await);
                assert_eq!(
                    storage
                        .view_storage()
                        .extend_root_chain(None, 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::Discontinuous(DiscontinuityReason::RolledBack)
                );
                assert_eq!(error_events().len(), events_before + 2);
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let db = storage.view_storage().get_connection().clone();
        let history = seed_linear_root_history(&db, 6).await;
        assert_eq!(
            storage
                .view_storage()
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        let extension = append_history(&db, history.last().unwrap(), 3, "row-conflict").await;
        assert!(cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await);
        let events_before_prepare = error_events().len();
        let mut row_conflict_prepared = false;
        for _ in 0..8 {
            assert_eq!(
                storage
                    .view_storage()
                    .extend_root_chain(Some(1), 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::NotCaughtUp
            );
            assert_eq!(error_events().len(), events_before_prepare);
            if root_chain(&db).await.len() == history.len()
                && scan_chain(&db)
                    .await
                    .last()
                    .is_some_and(|(_, commit_id, _, _, _)| {
                        commit_id == &history.last().unwrap().commit.id.to_string()
                    })
            {
                row_conflict_prepared = true;
                break;
            }
        }
        assert!(
            row_conflict_prepared,
            "row-conflict scan did not reach the old tail"
        );
        assert_eq!(root_chain(&db).await.len(), history.len());
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_view_root_chain SET commit_id = $1 WHERE seq = 3",
            [Value::from(extension[1].commit.id.to_string())],
        ))
        .await
        .unwrap();
        let roots_before = root_chain(&db).await;
        let events_before = error_events().len();
        assert_eq!(
            storage
                .view_storage()
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::Discontinuous(DiscontinuityReason::RowConflict)
        );
        assert_eq!(root_chain(&db).await, roots_before);
        let events_after_first = error_events();
        assert_eq!(events_after_first.len(), events_before + 1);
        let line = events_after_first.last().unwrap();
        assert!(line.contains("reason=RowConflict"), "{line}");
        assert!(
            line.contains(&format!("commit_id={}", extension[1].commit.id)),
            "{line}"
        );
        let scan_after_first = scan_chain(&db).await;
        assert_eq!(
            storage
                .view_storage()
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::Discontinuous(DiscontinuityReason::RowConflict)
        );
        assert_eq!(root_chain(&db).await, roots_before);
        assert_eq!(scan_chain(&db).await, scan_after_first);
        assert_eq!(error_events().len(), events_before + 1);

        let (db, storage, history) = view_storage_with_history(6).await;
        run_to_caught_up(&storage, 20).await;
        let first = seed_single_parent_root_commit_with_tree(
            &db,
            HashKind::Sha1,
            fixture_tree("segment-first"),
            history.last().unwrap(),
            "fixture segment first",
        )
        .await;
        let second = seed_single_parent_root_commit_with_tree(
            &db,
            HashKind::Sha1,
            fixture_tree("segment-second"),
            &first,
            "fixture segment second",
        )
        .await;
        let rows = [
            RootChainRow {
                scan_pos: 2,
                seq: 7,
                commit_id: first.commit.id.to_string(),
                tree_id: first.commit.tree_id.to_string(),
            },
            RootChainRow {
                scan_pos: 1,
                seq: 8,
                commit_id: second.commit.id.to_string(),
                tree_id: second.commit.tree_id.to_string(),
            },
        ];
        let txn = db.begin().await.unwrap();
        insert_root_chain_row(
            &txn,
            7,
            &second.commit.id.to_string(),
            &second.commit.tree_id.to_string(),
        )
        .await;
        assert_eq!(
            insert_segment(&txn, &rows).await.unwrap(),
            Some(Discontinuity {
                reason: DiscontinuityReason::RowConflict,
                commit_id: first.commit.id.to_string(),
            })
        );
        txn.rollback().await.unwrap();
        let txn = db.begin().await.unwrap();
        let duplicate_id_rows = [
            rows[0].clone(),
            RootChainRow {
                scan_pos: 1,
                seq: 8,
                commit_id: history[1].commit.id.to_string(),
                tree_id: history[1].commit.tree_id.to_string(),
            },
        ];
        assert_eq!(
            insert_segment(&txn, &duplicate_id_rows).await.unwrap(),
            Some(Discontinuity {
                reason: DiscontinuityReason::RowConflict,
                commit_id: history[1].commit.id.to_string(),
            })
        );
        txn.rollback().await.unwrap();
        let txn = db.begin().await.unwrap();
        insert_root_chain_row(
            &txn,
            7,
            &first.commit.id.to_string(),
            &first.commit.tree_id.to_string(),
        )
        .await;
        assert_eq!(insert_segment(&txn, &rows).await.unwrap(), None);
        txn.rollback().await.unwrap();

        let extension = append_history(&db, history.last().unwrap(), 4, "second-segment").await;
        assert!(cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await);
        let mut second_segment_prepared = false;
        for _ in 0..8 {
            let outcome = storage
                .extend_root_chain(Some(1), 3, ViewLockMode::Try)
                .await
                .unwrap();
            if root_chain(&db).await.len() == history.len()
                && scan_chain(&db)
                    .await
                    .last()
                    .is_some_and(|(_, commit_id, _, _, _)| {
                        commit_id == &history.last().unwrap().commit.id.to_string()
                    })
            {
                assert_eq!(outcome, RootChainOutcome::NotCaughtUp);
                second_segment_prepared = true;
                break;
            }
        }
        assert!(
            second_segment_prepared,
            "second-segment scan did not reach the old tail"
        );
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_view_root_chain SET commit_id = $1 WHERE seq = 3",
            [Value::from(extension[2].commit.id.to_string())],
        ))
        .await
        .unwrap();
        let roots_before_second_segment = root_chain(&db).await;
        assert_eq!(
            storage
                .extend_root_chain_with_segment_rows(None, 3, ViewLockMode::Try, 2)
                .await
                .unwrap(),
            RootChainOutcome::Discontinuous(DiscontinuityReason::RowConflict)
        );
        let mut expected_after_first_segment = roots_before_second_segment;
        expected_after_first_segment.extend([
            (
                7,
                extension[0].commit.id.to_string(),
                extension[0].commit.tree_id.to_string(),
            ),
            (
                8,
                extension[1].commit.id.to_string(),
                extension[1].commit.tree_id.to_string(),
            ),
        ]);
        assert_eq!(root_chain(&db).await, expected_after_first_segment);
    }

    #[tokio::test]
    async fn discontinuity_persists_across_pools() {
        for case in RootDiscontinuityCase::ALL {
            let (config, _schema, first_db) = configured_empty().await;
            let first_storage = new_view_storage(first_db.clone());
            let (_, _, expected) =
                prepare_discontinuity(first_db.as_ref(), &first_storage, case, 6).await;
            close_view_storage(first_db, first_storage).await;

            let (resumed_db, resumed_storage) = open_view_storage(&config).await;
            assert_eq!(
                resumed_storage
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::Discontinuous(expected),
                "restart must preserve {case:?}"
            );
            close_view_storage(resumed_db, resumed_storage).await;
        }
    }

    #[tokio::test]
    async fn discontinuity_writes_nothing() {
        for case in RootDiscontinuityCase::ALL {
            let temp = tempfile::tempdir().unwrap();
            let db = test_db_connection(temp.path()).await;
            apply_migrations(&db, true).await.unwrap();
            let storage = new_view_storage(Arc::new(db.clone()));
            let (_, mut active, expected) = prepare_discontinuity(&db, &storage, case, 6).await;
            let roots_before = root_chain(&db).await;
            let scan_before = scan_chain(&db).await;

            for call in 0..3 {
                if call == 1 {
                    let descendant = seed_single_parent_root_commit_with_tree(
                        &db,
                        HashKind::Sha1,
                        fixture_tree("hp28-frozen-scan"),
                        &active,
                        "HP-28 main advancement after halt",
                    )
                    .await;
                    assert!(cas_fixture_main(&db, &active, &descendant).await);
                    active = descendant;
                }
                assert_eq!(
                    storage
                        .extend_root_chain(None, 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::Discontinuous(expected),
                    "halted state changed for {case:?}"
                );
                assert_eq!(root_chain(&db).await, roots_before);
                assert_eq!(scan_chain(&db).await, scan_before);
            }
        }
    }

    #[tokio::test]
    async fn halted_true_after_discontinuity() {
        for case in RootDiscontinuityCase::ALL {
            let (config, _schema, holder_db) = configured_empty().await;
            let holder_storage = new_view_storage(holder_db.clone());
            prepare_discontinuity(holder_db.as_ref(), &holder_storage, case, 6).await;
            let (observer_db, observer_storage) = open_view_storage(&config).await;

            assert!(observer_storage.root_chain_halted().await.unwrap());
            assert!(halted_from_sql(observer_db.as_ref()).await);

            let holder = holder_db.begin().await.unwrap();
            assert!(
                acquire_view_lock(&holder, ViewLock::RootChain, ViewLockMode::Try)
                    .await
                    .unwrap()
            );
            let contender = observer_db.begin().await.unwrap();
            assert!(
                !acquire_view_lock(&contender, ViewLock::RootChain, ViewLockMode::Try)
                    .await
                    .unwrap()
            );
            contender.rollback().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), observer_storage.root_chain_halted())
                    .await
                    .unwrap()
                    .unwrap(),
                "unlocked reader did not observe {case:?}"
            );
            holder.rollback().await.unwrap();
            close_view_storage(observer_db, observer_storage).await;
            close_view_storage(holder_db, holder_storage).await;
        }
    }

    #[tokio::test]
    async fn clear_scan_relinks_after_restore() {
        for case in RootDiscontinuityCase::ALL {
            let temp = tempfile::tempdir().unwrap();
            let db = test_db_connection(temp.path()).await;
            apply_migrations(&db, true).await.unwrap();
            let storage = new_view_storage(Arc::new(db.clone()));
            let (mut history, active, _) = prepare_discontinuity(&db, &storage, case, 6).await;
            let recovered = append_history(&db, history.last().unwrap(), 2, "hp28-recovered").await;
            assert!(cas_fixture_main(&db, &active, recovered.last().unwrap()).await);
            clear_scan(&db).await;

            assert_eq!(
                storage
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::CaughtUp,
                "restored history did not relink for {case:?}"
            );
            history.extend(recovered);
            assert_root_chain_matches(&db, &history).await;
            assert!(!storage.root_chain_halted().await.unwrap());
        }
    }

    #[tokio::test]
    async fn clear_scan_without_restore_halts_again() {
        for case in RootDiscontinuityCase::ALL {
            let temp = tempfile::tempdir().unwrap();
            let db = test_db_connection(temp.path()).await;
            apply_migrations(&db, true).await.unwrap();
            let storage = new_view_storage(Arc::new(db.clone()));
            let (_, _, expected) = prepare_discontinuity(&db, &storage, case, 6).await;
            clear_scan(&db).await;
            assert_eq!(
                storage
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::Discontinuous(expected),
                "clearing a scan silently repaired {case:?}"
            );
            assert!(storage.root_chain_halted().await.unwrap());
        }
    }

    #[tokio::test]
    async fn halted_statement_counts() {
        for case in RootDiscontinuityCase::ALL {
            let mut resumed_counts = Vec::new();
            for history_len in [8, 256] {
                let (_config, _schema, db, counter) = counted_empty().await;
                let storage = new_view_storage(db.clone());
                let (_, _, expected) =
                    prepare_discontinuity(db.as_ref(), &storage, case, history_len).await;

                counter.store(0, Ordering::Relaxed);
                assert_eq!(
                    storage
                        .extend_root_chain(None, 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::Discontinuous(expected)
                );
                let resumed = counter.load(Ordering::Relaxed);
                assert!(resumed > 0, "resuming {case:?} issued no SQL");
                resumed_counts.push(resumed);

                counter.store(0, Ordering::Relaxed);
                assert!(storage.root_chain_halted().await.unwrap());
                assert_eq!(
                    counter.load(Ordering::Relaxed),
                    1,
                    "root_chain_halted must use one statement"
                );
            }
            assert_eq!(
                resumed_counts[0], resumed_counts[1],
                "resuming {case:?} must be independent of history length"
            );
        }
    }

    #[tokio::test]
    async fn halted_false_in_normal_states() {
        {
            let (db, storage, _) = view_storage_with_history(6).await;
            assert!(root_chain(&db).await.is_empty());
            assert!(scan_chain(&db).await.is_empty());
            assert!(!storage.root_chain_halted().await.unwrap());
            assert!(!halted_from_sql(&db).await);
        }

        {
            let (db, storage, history) = view_storage_with_history(6).await;
            run_to_caught_up(&storage, 20).await;
            let extension = append_history(&db, history.last().unwrap(), 3, "hp28-caught-up").await;
            assert!(
                cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await
            );
            assert_eq!(
                storage
                    .extend_root_chain(None, 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::CaughtUp
            );
            assert!(scan_chain(&db).await.is_empty());
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (db, storage, history) = view_storage_with_history(6).await;
            run_to_caught_up(&storage, 20).await;
            let extension = append_history(&db, history.last().unwrap(), 10, "hp28-budget").await;
            assert!(
                cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await
            );
            assert_eq!(
                storage
                    .extend_root_chain(Some(3), 3, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::NotCaughtUp
            );
            let anchor = scan_chain(&db).await.pop().unwrap();
            assert_eq!(anchor.3, 1);
            assert!(
                !root_chain(&db)
                    .await
                    .iter()
                    .any(|(_, commit_id, _)| commit_id == &anchor.1)
            );
            let first_parent = anchor.4.as_ref().unwrap();
            let parent_exists = db
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM mega_commit WHERE commit_id = $1) AS present",
                    [Value::from(first_parent.clone())],
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<bool>("", "present")
                .unwrap();
            assert!(parent_exists);
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (db, storage, history) = view_storage_with_history(6).await;
            run_to_caught_up(&storage, 20).await;
            let extension = append_history(&db, history.last().unwrap(), 3, "hp28-segment").await;
            assert!(
                cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await
            );
            let mut saw_segment = false;
            for _ in 0..20 {
                let outcome = storage
                    .extend_root_chain_with_segment_rows(Some(1), 3, ViewLockMode::Try, 1)
                    .await
                    .unwrap();
                if root_chain(&db).await.len() > history.len() {
                    assert_eq!(outcome, RootChainOutcome::NotCaughtUp);
                    assert!(scan_chain(&db).await.len() > 1);
                    assert_scan_anchor_is_tail(&db).await;
                    saw_segment = true;
                    break;
                }
            }
            assert!(saw_segment, "incremental segment was not observed");
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (db, storage, history) = view_storage_with_history(1).await;
            run_to_caught_up(&storage, 20).await;
            let extension = append_history(&db, history.last().unwrap(), 2, "hp28-root-tail").await;
            assert!(
                cas_fixture_main(&db, history.last().unwrap(), extension.last().unwrap()).await
            );
            let root_id = history[0].commit.id.to_string();
            let mut saw_root_anchor = false;
            for _ in 0..20 {
                assert_eq!(
                    storage
                        .extend_root_chain(Some(1), 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::NotCaughtUp
                );
                if root_chain(&db).await.len() == 1
                    && scan_chain(&db)
                        .await
                        .last()
                        .is_some_and(|(_, commit_id, _, _, _)| commit_id == &root_id)
                {
                    saw_root_anchor = true;
                    break;
                }
            }
            assert!(saw_root_anchor, "root-tail scan anchor was not observed");
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (db, storage, history) = view_storage_with_history(3).await;
            let root_id = history[0].commit.id.to_string();
            let mut saw_first_segment = false;
            for _ in 0..20 {
                let outcome = storage
                    .extend_root_chain_with_segment_rows(Some(1), 3, ViewLockMode::Try, 1)
                    .await
                    .unwrap();
                if root_chain(&db).await.len() == 1 {
                    assert_eq!(outcome, RootChainOutcome::NotCaughtUp);
                    assert!(
                        scan_chain(&db)
                            .await
                            .last()
                            .is_some_and(|(_, commit_id, _, _, _)| commit_id == &root_id)
                    );
                    saw_first_segment = true;
                    break;
                }
            }
            assert!(
                saw_first_segment,
                "cold-start first segment was not observed"
            );
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (db, storage, history) = view_storage_with_history(3).await;
            let root_id = history[0].commit.id.to_string();
            let mut saw_unattached_root = false;
            for _ in 0..20 {
                assert_eq!(
                    storage
                        .extend_root_chain(Some(1), 3, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    RootChainOutcome::NotCaughtUp
                );
                if root_chain(&db).await.is_empty()
                    && scan_chain(&db)
                        .await
                        .last()
                        .is_some_and(|(_, commit_id, _, _, _)| commit_id == &root_id)
                {
                    saw_unattached_root = true;
                    break;
                }
            }
            assert!(
                saw_unattached_root,
                "unattached root scan anchor was not observed"
            );
            assert!(!storage.root_chain_halted().await.unwrap());
        }

        {
            let (config, _schema, worker_db) = configured_empty().await;
            let history = seed_linear_root_history(worker_db.as_ref(), 31).await;
            let worker_storage = new_view_storage(worker_db.clone());
            let (observer_db, observer_storage) = open_view_storage(&config).await;
            let worker = worker_storage.clone();
            let task = tokio::spawn(async move {
                worker
                    .extend_root_chain_with_segment_rows(None, 3, ViewLockMode::Try, 1)
                    .await
            });
            tokio::task::yield_now().await;
            let mut saw_in_progress = false;
            let outcome = tokio::time::timeout(Duration::from_secs(10), async {
                while !task.is_finished() {
                    let row = observer_db
                        .query_one_raw(Statement::from_string(
                            DbBackend::Postgres,
                            format!(
                                "SELECT {ROOT_CHAIN_HALTED_SQL} AS halted, \
                             (SELECT count(*) FROM mega_view_root_chain) AS roots, \
                             (SELECT count(*) FROM mega_view_root_chain_scan) AS scans"
                            ),
                        ))
                        .await
                        .unwrap()
                        .unwrap();
                    let halted: bool = row.try_get("", "halted").unwrap();
                    let roots: i64 = row.try_get("", "roots").unwrap();
                    let scans: i64 = row.try_get("", "scans").unwrap();
                    assert!(!halted);
                    saw_in_progress |= (roots > 0 && roots < history.len() as i64) || scans > 0;
                    tokio::task::yield_now().await;
                }
                task.await.unwrap().unwrap()
            })
            .await
            .expect("segmented root-chain extension timed out");
            assert_eq!(outcome, RootChainOutcome::CaughtUp);
            assert!(
                saw_in_progress,
                "did not observe a committed segment or scan"
            );
            assert!(!observer_storage.root_chain_halted().await.unwrap());
            close_view_storage(observer_db, observer_storage).await;
            close_view_storage(worker_db, worker_storage).await;
        }
    }

    #[tokio::test]
    async fn lock_failure_outcomes() {
        assert_eq!(VIEW_LOCK_TIMEOUT, Duration::from_secs(2));
        let (config, _schema, holder_db, history) = configured_history(6).await;
        let caller_db = Arc::new(database_connection(&config).await.unwrap());
        let storage = new_view_storage(caller_db.clone());
        let extension =
            append_history(holder_db.as_ref(), history.last().unwrap(), 3, "lock").await;
        assert!(
            cas_fixture_main(
                holder_db.as_ref(),
                history.last().unwrap(),
                extension.last().unwrap()
            )
            .await
        );
        let before = (
            root_chain(caller_db.as_ref()).await,
            scan_chain(caller_db.as_ref()).await,
        );
        let txn = holder_db.begin().await.unwrap();
        assert!(
            acquire_view_lock(&txn, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap()
        );

        let started = Instant::now();
        assert_eq!(
            storage
                .extend_root_chain(None, 3, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::NotCaughtUp
        );
        assert!(started.elapsed() < VIEW_LOCK_TIMEOUT);

        let started = Instant::now();
        assert_eq!(
            storage
                .extend_root_chain(None, 3, ViewLockMode::Blocking)
                .await
                .unwrap(),
            RootChainOutcome::NotCaughtUp
        );
        assert!(started.elapsed() >= VIEW_LOCK_TIMEOUT);

        let cancel_deadline = Instant::now() + Duration::from_secs(1);
        let cancel_waiter = async {
            loop {
                let row = holder_db
                    .query_one_raw(Statement::from_string(
                        DbBackend::Postgres,
                        "SELECT pid FROM pg_stat_activity \
                         WHERE wait_event_type = 'Lock' AND wait_event = 'advisory' \
                           AND application_name = current_schema() \
                         ORDER BY pid LIMIT 1"
                            .to_owned(),
                    ))
                    .await
                    .unwrap();
                if let Some(row) = row {
                    let pid: i32 = row.try_get("", "pid").unwrap();
                    holder_db
                        .execute_raw(Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "SELECT pg_cancel_backend($1)",
                            [pid.into()],
                        ))
                        .await
                        .unwrap();
                    return;
                }
                assert!(
                    Instant::now() < cancel_deadline,
                    "blocked advisory-lock waiter was not observed within one second"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (waiting, ()) = tokio::join!(
            storage.extend_root_chain(None, 3, ViewLockMode::Blocking),
            cancel_waiter
        );
        let error = waiting.unwrap_err();
        assert_eq!(db_error_code(&error).as_deref(), Some("57014"));
        txn.rollback().await.unwrap();
        assert_eq!(
            (
                root_chain(caller_db.as_ref()).await,
                scan_chain(caller_db.as_ref()).await,
            ),
            before
        );
    }

    fn db_error_code(error: &MegaError) -> Option<String> {
        let MegaError::Db(sea_orm::DbErr::Exec(runtime) | sea_orm::DbErr::Query(runtime)) = error
        else {
            return None;
        };
        let sea_orm::RuntimeErr::SqlxError(sqlx_error) = runtime else {
            return None;
        };
        let sea_orm::sqlx::Error::Database(database_error) = sqlx_error.as_ref() else {
            return None;
        };
        database_error.code().map(|code| code.to_string())
    }
}
