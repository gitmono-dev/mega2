use std::{
    ops::Deref,
    sync::{Arc, Mutex},
    time::Duration,
};

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, EntityTrait, QueryFilter,
    RuntimeErr, Statement, Value, sqlx,
};
use serde_json::Value as JsonValue;

use crate::{
    callisto::mega_view_filter,
    common::errors::MegaError,
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        view_root_chain::DiscontinuityReason,
    },
};

pub(crate) const VIEW_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const VIEW_LOCK_NS: i32 = 1_297_043_025;
pub(crate) const VIEW_FILTER_LOCK_NS: i32 = 1_297_043_026;
pub(crate) const ROOT_CHAIN_KEY: i32 = 1;
pub(crate) const OBJECT_GC_KEY: i32 = 2;
pub(crate) const REGISTER_KEY: i32 = 3;

#[derive(Clone, Debug)]
pub(crate) struct ViewByName {
    pub(crate) filter_pk: i64,
    pub(crate) filter_id: String,
    pub(crate) name: String,
    pub(crate) version: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct ViewStatusRow {
    pub(crate) canonical_spec: String,
    pub(crate) src_paths: JsonValue,
    pub(crate) push_enabled: bool,
    pub(crate) ready: bool,
    pub(crate) projected_seq: i64,
    pub(crate) lag_commits: Option<i64>,
}

#[derive(Clone)]
pub struct ViewStorage {
    base: BaseStorage,
    pub(super) discontinuity_alert: Arc<Mutex<Option<(DiscontinuityReason, String)>>>,
}

impl ViewStorage {
    pub fn new(base: BaseStorage) -> Self {
        Self {
            base,
            discontinuity_alert: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn get_filter_by_filter_id(
        &self,
        filter_id: &str,
    ) -> Result<Option<mega_view_filter::Model>, MegaError> {
        Ok(mega_view_filter::Entity::find()
            .filter(mega_view_filter::Column::FilterId.eq(filter_id))
            .one(self.get_connection())
            .await?)
    }

    pub(crate) async fn find_view_by_name(
        &self,
        name: &str,
        version: Option<i32>,
    ) -> Result<Option<ViewByName>, MegaError> {
        let row = self
            .get_connection()
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT f.id AS filter_pk, f.filter_id, v.name, v.version \
                 FROM mega_view v JOIN mega_view_filter f ON f.id = v.filter_pk \
                 WHERE v.name = $1 AND ($2::integer IS NULL OR v.version = $2) \
                 ORDER BY v.version DESC LIMIT 1",
                [Value::from(name.to_owned()), Value::from(version)],
            ))
            .await?;
        row.map(|row| {
            Ok(ViewByName {
                filter_pk: row.try_get("", "filter_pk")?,
                filter_id: row.try_get("", "filter_id")?,
                name: row.try_get("", "name")?,
                version: row.try_get("", "version")?,
            })
        })
        .transpose()
    }

    pub(crate) async fn view_status(
        &self,
        filter_id: &str,
        walk_limit: u64,
    ) -> Result<Option<ViewStatusRow>, MegaError> {
        let walk_limit = i64::try_from(walk_limit)
            .map_err(|_| MegaError::Other("view walk limit exceeds i64".to_owned()))?;
        let row = self
            .get_connection()
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "WITH RECURSIVE tail AS ( \
                     SELECT max(seq) AS tail_seq FROM mega_view_root_chain \
                 ), walk AS ( \
                     SELECT r.ref_commit_hash AS commit_id, 1::bigint AS depth, 0::bigint AS unseen \
                     FROM mega_refs r WHERE r.path = '/' AND r.ref_name = 'refs/heads/main' \
                     UNION ALL \
                     SELECT c.parents_id::jsonb ->> 0, w.depth + 1, w.unseen + 1 \
                     FROM walk w \
                     JOIN mega_commit c ON c.commit_id = w.commit_id \
                     LEFT JOIN mega_view_root_chain chain ON chain.commit_id = w.commit_id \
                     WHERE chain.seq IS NULL AND jsonb_array_length(c.parents_id::jsonb) = 1 \
                       AND w.depth < $2 \
                 ) \
                 SELECT f.canonical_spec, f.src_paths, f.push_enabled, \
                        (f.ready_seq IS NOT NULL) AS ready, f.projected_seq, \
                        (SELECT CASE \
                             WHEN chain.seq = tail.tail_seq \
                                 THEN tail.tail_seq + w.unseen - f.projected_seq \
                             WHEN tail.tail_seq IS NULL AND c.commit_id IS NOT NULL \
                                  AND jsonb_array_length(c.parents_id::jsonb) = 0 \
                                 THEN w.unseen + 1 - f.projected_seq \
                             ELSE NULL END \
                         FROM walk w \
                         LEFT JOIN mega_view_root_chain chain ON chain.commit_id = w.commit_id \
                         LEFT JOIN mega_commit c ON c.commit_id = w.commit_id \
                         CROSS JOIN tail \
                         WHERE chain.seq IS NOT NULL \
                            OR (tail.tail_seq IS NULL AND c.commit_id IS NOT NULL \
                                AND jsonb_array_length(c.parents_id::jsonb) = 0) \
                         ORDER BY w.depth LIMIT 1) AS lag_commits \
                 FROM mega_view_filter f WHERE f.filter_id = $1",
                [Value::from(filter_id.to_owned()), Value::from(walk_limit)],
            ))
            .await?;
        row.map(|row| {
            Ok(ViewStatusRow {
                canonical_spec: row.try_get("", "canonical_spec")?,
                src_paths: row.try_get("", "src_paths")?,
                push_enabled: row.try_get("", "push_enabled")?,
                ready: row.try_get("", "ready")?,
                projected_seq: row.try_get("", "projected_seq")?,
                lag_commits: row.try_get("", "lag_commits")?,
            })
        })
        .transpose()
    }

    pub(super) fn should_log_discontinuity(
        &self,
        reason: DiscontinuityReason,
        commit_id: String,
    ) -> bool {
        let mut alert = match self.discontinuity_alert.lock() {
            Ok(alert) => alert,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = (reason, commit_id);
        if alert.as_ref() == Some(&next) {
            false
        } else {
            *alert = Some(next);
            true
        }
    }

    pub(super) fn clear_discontinuity_alert(&self) {
        let mut alert = match self.discontinuity_alert.lock() {
            Ok(alert) => alert,
            Err(poisoned) => poisoned.into_inner(),
        };
        *alert = None;
    }
}

impl Deref for ViewStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewLockMode {
    Try,
    Blocking,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewLock {
    RootChain,
    ObjectGcShared,
    ObjectGcExclusive,
    Register,
    Filter(i64),
}

pub(crate) fn hash32(value: i64) -> i32 {
    ((value as u64) ^ ((value as u64) >> 32)) as u32 as i32
}

fn lock_key(lock: ViewLock) -> (i32, i32, &'static str) {
    match lock {
        ViewLock::RootChain => (VIEW_LOCK_NS, ROOT_CHAIN_KEY, "root_chain"),
        ViewLock::ObjectGcShared | ViewLock::ObjectGcExclusive => {
            (VIEW_LOCK_NS, OBJECT_GC_KEY, "object_gc")
        }
        ViewLock::Register => (VIEW_LOCK_NS, REGISTER_KEY, "register"),
        ViewLock::Filter(filter_pk) => (VIEW_FILTER_LOCK_NS, hash32(filter_pk), "filter"),
    }
}

fn is_shared(lock: ViewLock) -> bool {
    matches!(lock, ViewLock::ObjectGcShared)
}

fn statement_sql(lock: ViewLock, mode: ViewLockMode, test_schema: bool) -> String {
    let operation = match (mode, is_shared(lock)) {
        (ViewLockMode::Try, false) => "pg_try_advisory_xact_lock",
        (ViewLockMode::Try, true) => "pg_try_advisory_xact_lock_shared",
        (ViewLockMode::Blocking, false) => "pg_advisory_xact_lock",
        (ViewLockMode::Blocking, true) => "pg_advisory_xact_lock_shared",
    };
    let key2 = if test_schema {
        "hashtext(current_schema() || ':' || $2)"
    } else {
        "$2"
    };
    match mode {
        ViewLockMode::Try => format!("SELECT {operation}($1, {key2}) AS locked"),
        ViewLockMode::Blocking => format!("SELECT {operation}($1, {key2})"),
    }
}

pub(crate) fn view_lock_stmt_prod(lock: ViewLock, mode: ViewLockMode) -> Statement {
    let (key1, key2, _) = lock_key(lock);
    Statement::from_sql_and_values(
        DbBackend::Postgres,
        statement_sql(lock, mode, false),
        [Value::from(key1), Value::from(key2)],
    )
}

pub(crate) fn view_lock_stmt_test(lock: ViewLock, mode: ViewLockMode) -> Statement {
    let (key1, _, descriptor) = lock_key(lock);
    let descriptor = match lock {
        ViewLock::Filter(filter_pk) => filter_pk.to_string(),
        _ => descriptor.to_owned(),
    };
    Statement::from_sql_and_values(
        DbBackend::Postgres,
        statement_sql(lock, mode, true),
        [Value::from(key1), Value::from(descriptor)],
    )
}

fn lock_timeout_value() -> String {
    format!("{}ms", VIEW_LOCK_TIMEOUT.as_millis())
}

fn is_lock_timeout(error: &DbErr) -> bool {
    let runtime = match error {
        DbErr::Exec(runtime) | DbErr::Query(runtime) => runtime,
        _ => return false,
    };
    let RuntimeErr::SqlxError(sqlx_error) = runtime else {
        return false;
    };
    let sqlx::Error::Database(database_error) = sqlx_error.as_ref() else {
        return false;
    };
    database_error.code().as_deref() == Some("55P03")
}

/// Acquires a transaction-scoped history-view advisory lock.
///
/// A blocking timeout aborts the PostgreSQL transaction before returning
/// `Ok(false)`, so callers must roll that transaction back immediately.
pub(crate) async fn acquire_view_lock(
    txn: &DatabaseTransaction,
    lock: ViewLock,
    mode: ViewLockMode,
) -> Result<bool, MegaError> {
    let statement = if cfg!(test) {
        view_lock_stmt_test(lock, mode)
    } else {
        view_lock_stmt_prod(lock, mode)
    };

    match mode {
        ViewLockMode::Try => {
            let row = txn
                .query_one_raw(statement)
                .await?
                .ok_or_else(|| MegaError::Other("view try-lock returned no row".to_owned()))?;
            Ok(row.try_get("", "locked")?)
        }
        ViewLockMode::Blocking => {
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT set_config('lock_timeout', $1, true)",
                [Value::from(lock_timeout_value())],
            ))
            .await?;
            match txn.execute_raw(statement).await {
                Ok(_) => Ok(true),
                Err(error) if is_lock_timeout(&error) => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{TransactionTrait, Value, Values};

    use super::*;
    use crate::jupiter::{
        storage::{init::database_connection, push_queue_storage::MONO_WRITE_LOCK_KEY1},
        tests::{test_db_config, test_db_connection},
    };

    #[tokio::test]
    async fn lock_keys_match_design() {
        assert_ne!(VIEW_LOCK_NS, VIEW_FILTER_LOCK_NS);
        assert_ne!(VIEW_LOCK_NS, MONO_WRITE_LOCK_KEY1);
        assert_ne!(VIEW_FILTER_LOCK_NS, MONO_WRITE_LOCK_KEY1);
        assert_ne!(VIEW_LOCK_NS, 1_297_043_023);
        assert_ne!(VIEW_FILTER_LOCK_NS, 1_297_043_023);
        assert_ne!(MONO_WRITE_LOCK_KEY1, 1_297_043_023);
        assert_ne!(ROOT_CHAIN_KEY, OBJECT_GC_KEY);
        assert_ne!(ROOT_CHAIN_KEY, REGISTER_KEY);
        assert_ne!(OBJECT_GC_KEY, REGISTER_KEY);
        assert_eq!(hash32(1), 1);
        assert_eq!(hash32(1_i64 << 32), 1);
        assert_eq!(hash32(0x1234_5678_9abc_def0_u64 as i64), -2_004_318_072);

        for (lock, key1, key2, descriptor) in [
            (
                ViewLock::RootChain,
                VIEW_LOCK_NS,
                ROOT_CHAIN_KEY,
                "root_chain",
            ),
            (
                ViewLock::ObjectGcShared,
                VIEW_LOCK_NS,
                OBJECT_GC_KEY,
                "object_gc",
            ),
            (
                ViewLock::ObjectGcExclusive,
                VIEW_LOCK_NS,
                OBJECT_GC_KEY,
                "object_gc",
            ),
            (ViewLock::Register, VIEW_LOCK_NS, REGISTER_KEY, "register"),
            (ViewLock::Filter(42), VIEW_FILTER_LOCK_NS, hash32(42), "42"),
        ] {
            for mode in [ViewLockMode::Try, ViewLockMode::Blocking] {
                let prod = view_lock_stmt_prod(lock, mode);
                let test = view_lock_stmt_test(lock, mode);
                let operation = match (mode, lock) {
                    (ViewLockMode::Try, ViewLock::ObjectGcShared) => {
                        "pg_try_advisory_xact_lock_shared"
                    }
                    (ViewLockMode::Try, _) => "pg_try_advisory_xact_lock",
                    (ViewLockMode::Blocking, ViewLock::ObjectGcShared) => {
                        "pg_advisory_xact_lock_shared"
                    }
                    (ViewLockMode::Blocking, _) => "pg_advisory_xact_lock",
                };
                let expected_prod = match mode {
                    ViewLockMode::Try => format!("SELECT {operation}($1, $2) AS locked"),
                    ViewLockMode::Blocking => format!("SELECT {operation}($1, $2)"),
                };
                let expected_test = match mode {
                    ViewLockMode::Try => format!(
                        "SELECT {operation}($1, hashtext(current_schema() || ':' || $2)) AS locked"
                    ),
                    ViewLockMode::Blocking => {
                        format!("SELECT {operation}($1, hashtext(current_schema() || ':' || $2))")
                    }
                };
                assert!(!prod.sql.contains("lock_timeout"));
                assert!(!test.sql.contains("lock_timeout"));
                assert_eq!(prod.sql, expected_prod);
                assert_eq!(
                    prod.values,
                    Some(Values(vec![Value::from(key1), Value::from(key2)]))
                );
                assert_eq!(test.sql, expected_test);
                assert_eq!(
                    test.values,
                    Some(Values(vec![
                        Value::from(key1),
                        Value::from(descriptor.to_owned())
                    ]))
                );
            }
        }

        let first_temp = tempfile::tempdir().unwrap();
        let second_temp = tempfile::tempdir().unwrap();
        let first_schema = test_db_connection(first_temp.path()).await;
        let second_schema = test_db_connection(second_temp.path()).await;
        let first = first_schema.begin().await.unwrap();
        let second = second_schema.begin().await.unwrap();
        assert!(
            acquire_view_lock(&first, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap()
        );
        assert!(
            acquire_view_lock(&second, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap(),
            "different test schemas must not share a view lock"
        );
        first.rollback().await.unwrap();
        second.rollback().await.unwrap();

        let temp = tempfile::tempdir().unwrap();
        let (config, _schema) = test_db_config(temp.path()).await;
        let first = database_connection(&config).await.unwrap();
        let second = database_connection(&config).await.unwrap();
        let independently_keyed = [
            ViewLock::RootChain,
            ViewLock::ObjectGcExclusive,
            ViewLock::Register,
            ViewLock::Filter(1),
            ViewLock::Filter(2),
        ];
        for (index, holder_lock) in independently_keyed.iter().enumerate() {
            for contender_lock in independently_keyed.iter().skip(index + 1) {
                let holder = first.begin().await.unwrap();
                assert!(
                    acquire_view_lock(&holder, *holder_lock, ViewLockMode::Try)
                        .await
                        .unwrap()
                );
                let contender = second.begin().await.unwrap();
                assert!(
                    acquire_view_lock(&contender, *contender_lock, ViewLockMode::Try)
                        .await
                        .unwrap(),
                    "{holder_lock:?} must not contend with {contender_lock:?}"
                );
                contender.rollback().await.unwrap();
                holder.rollback().await.unwrap();
            }
        }

        let holder = first.begin().await.unwrap();
        assert!(
            acquire_view_lock(&holder, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap()
        );
        let contender = second.begin().await.unwrap();
        assert!(
            !acquire_view_lock(&contender, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap(),
            "the same schema and lock must contend"
        );
        contender.rollback().await.unwrap();
        holder.rollback().await.unwrap();

        let shared_holder = first.begin().await.unwrap();
        assert!(
            acquire_view_lock(&shared_holder, ViewLock::ObjectGcShared, ViewLockMode::Try)
                .await
                .unwrap()
        );
        let exclusive_contender = second.begin().await.unwrap();
        assert!(
            !acquire_view_lock(
                &exclusive_contender,
                ViewLock::ObjectGcExclusive,
                ViewLockMode::Try
            )
            .await
            .unwrap()
        );
        exclusive_contender.rollback().await.unwrap();
        shared_holder.rollback().await.unwrap();
    }
}
