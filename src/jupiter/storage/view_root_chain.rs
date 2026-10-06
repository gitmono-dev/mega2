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
        sync::{Arc, Mutex},
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
