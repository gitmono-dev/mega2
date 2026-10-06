use std::time::Duration;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{NotSet, Set},
    ColumnTrait, Condition, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait, Value,
    sea_query::OnConflict,
};
use serde_json::Value as JsonValue;

use crate::{
    callisto::{mega_view, mega_view_filter, mega_view_register_log},
    common::{errors::MegaError, utils::generate_id},
    config::ViewsConfig,
    jupiter::storage::{
        base_storage::StorageConnector,
        view_storage::{
            ViewLock, ViewLockMode, ViewStorage, view_lock_stmt_prod, view_lock_stmt_test,
        },
    },
};

const REJECT_RETRY_AFTER: Duration = Duration::from_secs(30);
const REGISTER_RATE_WINDOW_SECS: u64 = 3600;

fn register_rate_window_stmt(requester: &str) -> Statement {
    Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!(
            "SELECT count(*)::bigint AS count, \
             least({REGISTER_RATE_WINDOW_SECS}, \
             ceil(extract(epoch FROM min(created_at) + interval '{REGISTER_RATE_WINDOW_SECS} seconds' - now())))::bigint \
             AS retry_after \
             FROM mega_view_register_log \
             WHERE requester = $1 \
             AND created_at > now() - interval '{REGISTER_RATE_WINDOW_SECS} seconds'"
        ),
        [Value::from(requester.to_owned())],
    )
}

fn delete_expired_register_log_stmt(requester: &str) -> Statement {
    Statement::from_sql_and_values(
        DbBackend::Postgres,
        format!(
            "DELETE FROM mega_view_register_log \
             WHERE requester = $1 \
             AND created_at <= now() - interval '{REGISTER_RATE_WINDOW_SECS} seconds'"
        ),
        [Value::from(requester.to_owned())],
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmitRequest {
    pub(crate) mode: AdmitMode,
}

impl AdmitRequest {
    pub(crate) fn register(
        definition: FilterDefinition,
        name: Option<String>,
        requester: String,
    ) -> Self {
        Self {
            mode: AdmitMode::Register {
                definition,
                name,
                requester,
            },
        }
    }

    pub(crate) fn rewarm(filter_pk: i64) -> Self {
        Self {
            mode: AdmitMode::Rewarm { filter_pk },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmitMode {
    Register {
        definition: FilterDefinition,
        name: Option<String>,
        requester: String,
    },
    Rewarm {
        filter_pk: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FilterDefinition {
    pub(crate) filter_id: String,
    pub(crate) canonical_spec: String,
    pub(crate) algo_version: i16,
    pub(crate) object_format: String,
    pub(crate) src_paths: JsonValue,
    pub(crate) push_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmitLimits {
    pub(crate) max_filters: u64,
    pub(crate) max_concurrent_cold_starts: u64,
    pub(crate) register_rate_per_token: u64,
}

impl From<&ViewsConfig> for AdmitLimits {
    fn from(views: &ViewsConfig) -> Self {
        Self {
            max_filters: views.max_filters,
            max_concurrent_cold_starts: views.max_concurrent_cold_starts,
            register_rate_per_token: views.register_rate_per_token,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmitOutcome {
    Admitted {
        version: Option<i32>,
        ready: bool,
    },
    Idempotent {
        version: Option<i32>,
        ready: bool,
    },
    Rejected {
        reason: RejectReason,
        retry_after: Duration,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectReason {
    MaxFilters,
    ColdStartSlots,
    Rate,
}

/// Chooses the HP-10 register lock statement without adding another lock-key
/// implementation. Admission executes this statement directly because its
/// blocking wait deliberately has no `lock_timeout`.
pub(crate) fn admit_lock_stmt(test: bool) -> Statement {
    match test {
        true => view_lock_stmt_test(ViewLock::Register, ViewLockMode::Blocking),
        false => view_lock_stmt_prod(ViewLock::Register, ViewLockMode::Blocking),
    }
}

impl ViewStorage {
    /// Atomically admits a registration or rewarming request under the register
    /// advisory lock.
    pub(crate) async fn admit(
        &self,
        request: AdmitRequest,
        limits: AdmitLimits,
    ) -> Result<AdmitOutcome, MegaError> {
        let txn = self.get_connection().begin().await?;
        txn.execute_raw(admit_lock_stmt(cfg!(test))).await?;

        let now = Utc::now().naive_utc();
        let (existing_filter, definition, name, requester) = match &request.mode {
            AdmitMode::Register {
                definition,
                name,
                requester,
            } => (
                mega_view_filter::Entity::find()
                    .filter(mega_view_filter::Column::FilterId.eq(&definition.filter_id))
                    .one(&txn)
                    .await?,
                Some(definition),
                name.as_deref(),
                Some(requester.as_str()),
            ),
            AdmitMode::Rewarm { filter_pk } => (
                mega_view_filter::Entity::find_by_id(*filter_pk)
                    .one(&txn)
                    .await?,
                None,
                None,
                None,
            ),
        };

        let existing_filter = match (existing_filter, &request.mode) {
            (Some(filter), _) => Some(filter),
            (None, AdmitMode::Register { .. }) => None,
            (None, AdmitMode::Rewarm { filter_pk }) => {
                txn.rollback().await?;
                return Err(MegaError::NotFound(format!(
                    "view filter {filter_pk} not found"
                )));
            }
        };

        let cold_start = existing_filter.as_ref().is_none_or(|filter| {
            filter.projected_seq == 0
                && filter.ready_seq.is_none()
                && filter.warming_since.is_none()
        });
        let ready = existing_filter
            .as_ref()
            .is_some_and(|filter| filter.ready_seq.is_some());

        let latest_view = match name {
            Some(name) => {
                mega_view::Entity::find()
                    .select_only()
                    .column(mega_view::Column::Version)
                    .column(mega_view::Column::FilterPk)
                    .filter(mega_view::Column::Name.eq(name))
                    .order_by_desc(mega_view::Column::Version)
                    .into_tuple::<(i32, i64)>()
                    .one(&txn)
                    .await?
            }
            None => None,
        };

        let needs_version = match (name, existing_filter.as_ref(), latest_view) {
            (Some(_), Some(filter), Some((_, filter_pk))) => filter.id != filter_pk,
            (Some(_), _, None) => true,
            (Some(_), None, Some(_)) => true,
            (None, _, _) => false,
        };

        if !cold_start && !needs_version {
            txn.commit().await?;
            return Ok(AdmitOutcome::Idempotent {
                version: latest_view.map(|(version, _)| version),
                ready,
            });
        }

        let rate_requester = match &request.mode {
            AdmitMode::Register { .. } => Some(requester.map(str::to_owned).ok_or_else(|| {
                MegaError::Other("missing requester for Register admission".to_owned())
            })?),
            AdmitMode::Rewarm { .. } => None,
        };

        if let Some(requester) = rate_requester.as_deref() {
            let rate_row = txn
                .query_one_raw(register_rate_window_stmt(requester))
                .await?
                .ok_or_else(|| {
                    MegaError::Other("missing register-rate aggregate row".to_owned())
                })?;
            let count: i64 = rate_row.try_get("", "count")?;
            let retry_after: i64 = rate_row.try_get("", "retry_after")?;
            let count = u64::try_from(count)
                .map_err(|_| MegaError::Other("negative register-rate count".to_owned()))?;
            let retry_after = u64::try_from(retry_after)
                .map_err(|_| MegaError::Other("negative register-rate retry-after".to_owned()))?;
            if count >= limits.register_rate_per_token {
                txn.rollback().await?;
                return Ok(AdmitOutcome::Rejected {
                    reason: RejectReason::Rate,
                    retry_after: Duration::from_secs(retry_after),
                });
            }
        }

        if cold_start {
            let active_filters = mega_view_filter::Entity::find()
                .filter(
                    Condition::any()
                        .add(mega_view_filter::Column::WarmingSince.is_not_null())
                        .add(mega_view_filter::Column::ProjectedSeq.gt(0)),
                )
                .count(&txn)
                .await?;
            if active_filters >= limits.max_filters {
                txn.rollback().await?;
                return Ok(AdmitOutcome::Rejected {
                    reason: RejectReason::MaxFilters,
                    retry_after: REJECT_RETRY_AFTER,
                });
            }

            let cold_start_slots = mega_view_filter::Entity::find()
                .filter(mega_view_filter::Column::WarmingSince.is_not_null())
                .count(&txn)
                .await?;
            if cold_start_slots >= limits.max_concurrent_cold_starts {
                txn.rollback().await?;
                return Ok(AdmitOutcome::Rejected {
                    reason: RejectReason::ColdStartSlots,
                    retry_after: REJECT_RETRY_AFTER,
                });
            }
        }

        let filter_pk = match existing_filter {
            Some(filter) if cold_start => {
                let mut active: mega_view_filter::ActiveModel = filter.into();
                active.warming_since = Set(Some(now));
                active.update(&txn).await?.id
            }
            Some(filter) => filter.id,
            None => {
                let definition = definition.ok_or_else(|| {
                    MegaError::Other("missing definition for Register admission".to_owned())
                })?;
                let inserted = mega_view_filter::Entity::insert(mega_view_filter::ActiveModel {
                    id: Set(generate_id()),
                    filter_id: Set(definition.filter_id.clone()),
                    canonical_spec: Set(definition.canonical_spec.clone()),
                    algo_version: Set(definition.algo_version),
                    object_format: Set(definition.object_format.clone()),
                    src_paths: Set(definition.src_paths.clone()),
                    push_enabled: Set(definition.push_enabled),
                    projected_seq: Set(0),
                    ready_seq: Set(None),
                    warming_since: Set(Some(now)),
                    last_access_at: Set(None),
                    created_at: Set(now),
                })
                .on_conflict(
                    OnConflict::column(mega_view_filter::Column::FilterId)
                        .do_nothing()
                        .to_owned(),
                )
                .exec_with_returning(&txn)
                .await;
                match inserted {
                    Ok(filter) => filter.id,
                    // SeaORM 2.0.2 maps Postgres RETURNING zero rows to
                    // RecordNotFound. The admission contract classifies this
                    // expected conflict as RecordNotInserted.
                    Err(DbErr::RecordNotFound(_)) => {
                        txn.rollback().await?;
                        return Err(MegaError::Db(DbErr::RecordNotInserted));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };

        let version = if needs_version {
            let name = name.ok_or_else(|| {
                MegaError::Other("missing name for versioned admission".to_owned())
            })?;
            let requester = requester.ok_or_else(|| {
                MegaError::Other("missing requester for versioned admission".to_owned())
            })?;
            let version = latest_view.map_or(1, |(version, _)| version + 1);
            mega_view::ActiveModel {
                id: Set(generate_id()),
                name: Set(name.to_owned()),
                version: Set(version),
                filter_pk: Set(filter_pk),
                created_by: Set(requester.to_owned()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
            Some(version)
        } else {
            latest_view.map(|(version, _)| version)
        };

        if let Some(requester) = rate_requester {
            mega_view_register_log::ActiveModel {
                id: NotSet,
                requester: Set(requester.clone()),
                created_at: NotSet,
            }
            .insert(&txn)
            .await?;
            txn.execute_raw(delete_expired_register_log_stmt(&requester))
                .await?;
        }

        txn.commit().await?;
        Ok(AdmitOutcome::Admitted { version, ready })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use chrono::Utc;
    use sea_orm::{
        ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
        DbBackend, EntityTrait, QueryFilter, QueryOrder, Statement, TransactionTrait,
    };

    use super::*;
    use crate::{
        callisto::mega_view_register_log,
        config::{ViewsConfig, reload::ConfigHandle, testing::isolated_config},
        jupiter::{
            migration::apply_migrations,
            storage::{
                Storage,
                base_storage::BaseStorage,
                init::database_connection,
                view_storage::{VIEW_LOCK_NS, VIEW_LOCK_TIMEOUT, acquire_view_lock},
            },
            tests::{
                TestSchemaGuard, test_db_config, test_db_connection, test_storage_with_config,
            },
        },
    };

    #[derive(Clone, Copy)]
    enum FilterState {
        Ready,
        Warming,
        Partial,
        Reclaimed,
    }

    #[derive(Clone, Copy)]
    enum RegisterRateCase {
        NewFilter,
        NewVersion,
        ReclaimedFilter,
    }

    fn definition(filter_id: impl Into<String>) -> FilterDefinition {
        let filter_id = filter_id.into();
        FilterDefinition {
            canonical_spec: format!("subdir:/{filter_id}"),
            filter_id,
            algo_version: 1,
            object_format: "sha1".to_owned(),
            src_paths: serde_json::json!(["/"]),
            push_enabled: false,
        }
    }

    fn limits(max_filters: u64, max_concurrent_cold_starts: u64) -> AdmitLimits {
        AdmitLimits::from(&ViewsConfig {
            max_filters,
            max_concurrent_cold_starts,
            ..ViewsConfig::default()
        })
    }

    fn register(filter_id: impl Into<String>, name: Option<impl Into<String>>) -> AdmitRequest {
        AdmitRequest::register(definition(filter_id), name.map(Into::into), "r1".to_owned())
    }

    fn register_as(
        filter_id: impl Into<String>,
        name: Option<impl Into<String>>,
        requester: impl Into<String>,
    ) -> AdmitRequest {
        AdmitRequest::register(
            definition(filter_id),
            name.map(Into::into),
            requester.into(),
        )
    }

    fn rate_case_request(case: RegisterRateCase, suffix: &str, requester: &str) -> AdmitRequest {
        match case {
            RegisterRateCase::NewFilter => {
                register_as(format!("new-{suffix}"), None::<String>, requester)
            }
            RegisterRateCase::NewVersion => {
                register_as("ready", Some(format!("ready-{suffix}")), requester)
            }
            RegisterRateCase::ReclaimedFilter => {
                register_as("reclaimed", None::<String>, requester)
            }
        }
    }

    async fn prepare_rate_case(db: &DatabaseConnection, case: RegisterRateCase) {
        if matches!(case, RegisterRateCase::ReclaimedFilter) {
            reset_reclaimed_filter(db, 2).await;
        }
    }

    fn rate_limits(
        register_rate_per_token: u64,
        max_filters: u64,
        max_concurrent_cold_starts: u64,
    ) -> AdmitLimits {
        AdmitLimits::from(&ViewsConfig {
            register_rate_per_token,
            max_filters,
            max_concurrent_cold_starts,
            ..ViewsConfig::default()
        })
    }

    async fn storage_for(temp: &tempfile::TempDir) -> (Arc<DatabaseConnection>, ViewStorage) {
        let db = Arc::new(test_db_connection(temp.path()).await);
        apply_migrations(db.as_ref(), true).await.unwrap();
        let storage = ViewStorage::new(BaseStorage::new(db.clone()));
        (db, storage)
    }

    async fn insert_filter(
        db: &DatabaseConnection,
        id: i64,
        filter_id: &str,
        state: FilterState,
    ) -> mega_view_filter::Model {
        let now = Utc::now().naive_utc();
        let (projected_seq, ready_seq, warming_since) = match state {
            FilterState::Ready => (5, Some(5), None),
            FilterState::Warming => (0, None, Some(now)),
            FilterState::Partial => (3, None, Some(now)),
            FilterState::Reclaimed => (0, None, None),
        };
        mega_view_filter::ActiveModel {
            id: Set(id),
            filter_id: Set(filter_id.to_owned()),
            canonical_spec: Set(format!("subdir:/{filter_id}")),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!(["/"])),
            push_enabled: Set(false),
            projected_seq: Set(projected_seq),
            ready_seq: Set(ready_seq),
            warming_since: Set(warming_since),
            last_access_at: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn insert_view(
        db: &DatabaseConnection,
        id: i64,
        name: &str,
        version: i32,
        filter_pk: i64,
    ) -> mega_view::Model {
        mega_view::ActiveModel {
            id: Set(id),
            name: Set(name.to_owned()),
            version: Set(version),
            filter_pk: Set(filter_pk),
            created_by: Set("seed".to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .unwrap()
    }

    async fn table_rows(db: &DatabaseConnection, table: &str) -> Vec<String> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT to_jsonb(t)::text AS row FROM {table} t ORDER BY id"),
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("", "row").unwrap())
        .collect()
    }

    async fn clear_register_logs(db: &DatabaseConnection) {
        mega_view_register_log::Entity::delete_many()
            .exec(db)
            .await
            .unwrap();
    }

    async fn insert_register_log(db: &DatabaseConnection, requester: &str, offset_secs: i64) {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO mega_view_register_log (requester, created_at) \
             VALUES ($1, localtimestamp + ($2::bigint * interval '1 second'))",
            [
                sea_orm::Value::from(requester.to_owned()),
                sea_orm::Value::from(offset_secs),
            ],
        ))
        .await
        .unwrap();
    }

    async fn insert_window_rows(db: &DatabaseConnection, requester: &str, count: usize) {
        for age_secs in 1..=count {
            insert_register_log(db, requester, -(age_secs as i64)).await;
        }
    }

    async fn register_log_models(db: &DatabaseConnection) -> Vec<mega_view_register_log::Model> {
        mega_view_register_log::Entity::find()
            .order_by_asc(mega_view_register_log::Column::Id)
            .all(db)
            .await
            .unwrap()
    }

    async fn reset_reclaimed_filter(db: &DatabaseConnection, id: i64) {
        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::ProjectedSeq,
                sea_orm::sea_query::Expr::value(0),
            )
            .col_expr(
                mega_view_filter::Column::ReadySeq,
                sea_orm::sea_query::Expr::value(None::<i64>),
            )
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(None::<chrono::NaiveDateTime>),
            )
            .filter(mega_view_filter::Column::Id.eq(id))
            .exec(db)
            .await
            .unwrap();
    }

    async fn retry_after_upper_bound(db: &DatabaseConnection, requester: &str) -> i64 {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT ceil(extract(epoch FROM (SELECT min(created_at) \
             FROM mega_view_register_log WHERE requester = $1) \
             + interval '3600 seconds' - localtimestamp))::bigint AS retry_after",
            [sea_orm::Value::from(requester.to_owned())],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "retry_after")
        .unwrap()
    }

    async fn three_table_snapshot(
        db: &DatabaseConnection,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            table_rows(db, "mega_view_filter").await,
            table_rows(db, "mega_view").await,
            table_rows(db, "mega_view_register_log").await,
        )
    }

    async fn two_table_snapshot(db: &DatabaseConnection) -> (Vec<String>, Vec<String>) {
        (
            table_rows(db, "mega_view_filter").await,
            table_rows(db, "mega_view").await,
        )
    }

    async fn count_waiting_register_locks(db: &DatabaseConnection) -> i64 {
        db.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT count(*)::bigint AS count FROM pg_locks \
             WHERE locktype = 'advisory' AND NOT granted \
             AND classid = $1::int4::oid \
             AND objid = hashtext(current_schema() || ':register')::oid \
             AND objsubid = 2",
            [sea_orm::Value::from(VIEW_LOCK_NS)],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "count")
        .unwrap()
    }

    async fn wait_for_register_waiters(db: &DatabaseConnection, expected: i64) -> bool {
        for _ in 0..1_000 {
            if count_waiting_register_locks(db).await >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn multi_pool_storages(
        temp: &tempfile::TempDir,
    ) -> (
        Arc<DatabaseConnection>,
        Arc<DatabaseConnection>,
        Arc<DatabaseConnection>,
        ViewStorage,
        ViewStorage,
        TestSchemaGuard,
    ) {
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let first = Arc::new(database_connection(&db_config).await.unwrap());
        let second = Arc::new(database_connection(&db_config).await.unwrap());
        let holder = Arc::new(database_connection(&db_config).await.unwrap());
        for db in [&first, &second, &holder] {
            db.execute_unprepared("SELECT 1").await.unwrap();
        }
        (
            first.clone(),
            second.clone(),
            holder,
            ViewStorage::new(BaseStorage::new(first)),
            ViewStorage::new(BaseStorage::new(second)),
            _schema,
        )
    }

    #[tokio::test]
    async fn concurrent_last_slot() {
        let prod = admit_lock_stmt(false);
        let test = admit_lock_stmt(true);
        assert_eq!(
            prod,
            view_lock_stmt_prod(ViewLock::Register, ViewLockMode::Blocking)
        );
        assert_eq!(
            test,
            view_lock_stmt_test(ViewLock::Register, ViewLockMode::Blocking)
        );
        assert!(!prod.sql.contains("lock_timeout"));
        assert!(!prod.sql.contains("try"));
        assert!(!prod.sql.contains("_shared"));

        let temp = tempfile::tempdir().unwrap();
        let (first, _second, holder, a, b, _guard) = multi_pool_storages(&temp).await;
        insert_filter(first.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(first.as_ref(), 2, "warming", FilterState::Warming).await;
        let limits = limits(3, 2);

        for round in 0..5 {
            let before = two_table_snapshot(first.as_ref()).await;
            let txn = holder.begin().await.unwrap();
            assert!(
                acquire_view_lock(&txn, ViewLock::Register, ViewLockMode::Blocking)
                    .await
                    .unwrap()
            );
            let filter_a = format!("a-{round}");
            let filter_b = format!("b-{round}");
            let name_a = format!("a-name-{round}");
            let name_b = format!("b-name-{round}");
            let admission_a = a.admit(register(filter_a.clone(), Some(name_a.clone())), limits);
            let admission_b = b.admit(register(filter_b.clone(), Some(name_b.clone())), limits);
            let gate_holder = holder.clone();
            let holder_gate = async move {
                if !wait_for_register_waiters(gate_holder.as_ref(), 2).await {
                    txn.rollback().await.unwrap();
                    panic!("did not observe two register waiters");
                }
                if round == 4 {
                    tokio::time::sleep(VIEW_LOCK_TIMEOUT + Duration::from_secs(1)).await;
                }
                let committed = std::time::Instant::now();
                txn.commit().await.unwrap();
                committed
            };
            let ((a_at, a_result), (b_at, b_result), committed) =
                tokio::time::timeout(Duration::from_secs(30), async {
                    tokio::join!(
                        async {
                            let result = admission_a.await;
                            (std::time::Instant::now(), result)
                        },
                        async {
                            let result = admission_b.await;
                            (std::time::Instant::now(), result)
                        },
                        holder_gate,
                    )
                })
                .await
                .expect("register gate and admissions finish within 30 seconds");
            let a_result = a_result.unwrap();
            let b_result = b_result.unwrap();
            if round == 4 {
                assert!(a_at > committed, "A returned before L_R was released");
                assert!(b_at > committed, "B returned before L_R was released");
            }
            let (winner_filter, winner_name) = match (&a_result, &b_result) {
                (
                    AdmitOutcome::Admitted {
                        version: Some(1),
                        ready: false,
                    },
                    AdmitOutcome::Rejected {
                        reason: RejectReason::MaxFilters,
                        retry_after,
                    },
                ) if *retry_after == Duration::from_secs(30) => (&filter_a, &name_a),
                (
                    AdmitOutcome::Rejected {
                        reason: RejectReason::MaxFilters,
                        retry_after,
                    },
                    AdmitOutcome::Admitted {
                        version: Some(1),
                        ready: false,
                    },
                ) if *retry_after == Duration::from_secs(30) => (&filter_b, &name_b),
                _ => panic!("unexpected concurrent admission outcomes: {a_result:?}, {b_result:?}"),
            };
            let after = two_table_snapshot(first.as_ref()).await;
            assert_eq!(after.0.len(), before.0.len() + 1);
            assert_eq!(after.1.len(), before.1.len() + 1);
            assert!(before.0.iter().all(|row| after.0.contains(row)));
            assert!(before.1.iter().all(|row| after.1.contains(row)));
            let winner_filter_row = mega_view_filter::Entity::find()
                .filter(mega_view_filter::Column::FilterId.eq(winner_filter))
                .one(first.as_ref())
                .await
                .unwrap()
                .unwrap();
            let winner_view = mega_view::Entity::find()
                .filter(mega_view::Column::Name.eq(winner_name))
                .one(first.as_ref())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(winner_view.version, 1);
            assert_eq!(winner_view.filter_pk, winner_filter_row.id);
            for loser_filter in [&filter_a, &filter_b] {
                if loser_filter != winner_filter {
                    assert!(
                        mega_view_filter::Entity::find()
                            .filter(mega_view_filter::Column::FilterId.eq(loser_filter))
                            .one(first.as_ref())
                            .await
                            .unwrap()
                            .is_none()
                    );
                }
            }
            mega_view::Entity::delete_many()
                .filter(mega_view::Column::Name.contains(format!("-{round}")))
                .exec(first.as_ref())
                .await
                .unwrap();
            mega_view_filter::Entity::delete_many()
                .filter(mega_view_filter::Column::FilterId.contains(format!("-{round}")))
                .exec(first.as_ref())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn same_filter_concurrent() {
        let temp = tempfile::tempdir().unwrap();
        let (first, _second, holder, a, b, _guard) = multi_pool_storages(&temp).await;
        let limits = limits(100, 10);
        for round in 0..5 {
            let warming_before = mega_view_filter::Entity::find()
                .filter(mega_view_filter::Column::WarmingSince.is_not_null())
                .count(first.as_ref())
                .await
                .unwrap();
            let txn = holder.begin().await.unwrap();
            assert!(
                acquire_view_lock(&txn, ViewLock::Register, ViewLockMode::Blocking)
                    .await
                    .unwrap()
            );
            let request = register(format!("same-{round}"), Some(format!("same-name-{round}")));
            let admission_a = a.admit(request.clone(), limits);
            let admission_b = b.admit(request, limits);
            let gate_holder = holder.clone();
            let holder_gate = async move {
                if !wait_for_register_waiters(gate_holder.as_ref(), 2).await {
                    txn.rollback().await.unwrap();
                    panic!("did not observe two same-filter register waiters");
                }
                txn.commit().await.unwrap();
            };
            let (a_result, b_result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(admission_a, admission_b, holder_gate)
            })
            .await
            .expect("same-filter gate and admissions finish within 30 seconds");
            let outcomes = [a_result.unwrap(), b_result.unwrap()];
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(
                        outcome,
                        AdmitOutcome::Admitted {
                            version: Some(1),
                            ready: false
                        }
                    ))
                    .count(),
                1
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(
                        outcome,
                        AdmitOutcome::Idempotent {
                            version: Some(1),
                            ready: false
                        }
                    ))
                    .count(),
                1
            );
            let filters = mega_view_filter::Entity::find()
                .filter(mega_view_filter::Column::FilterId.eq(format!("same-{round}")))
                .all(first.as_ref())
                .await
                .unwrap();
            assert_eq!(filters.len(), 1);
            assert!(filters[0].warming_since.is_some());
            assert_eq!(
                mega_view_filter::Entity::find()
                    .filter(mega_view_filter::Column::WarmingSince.is_not_null())
                    .count(first.as_ref())
                    .await
                    .unwrap(),
                warming_before + 1
            );
            let views = mega_view::Entity::find()
                .filter(mega_view::Column::Name.eq(format!("same-name-{round}")))
                .all(first.as_ref())
                .await
                .unwrap();
            assert_eq!(views.len(), 1);
            assert_eq!(views[0].version, 1);
            assert_eq!(views[0].filter_pk, filters[0].id);
        }
    }

    #[tokio::test]
    async fn rejection_and_idempotent_hit() {
        for (state, max_filters, slots, reason) in [
            (FilterState::Ready, 1, 2, RejectReason::MaxFilters),
            (FilterState::Warming, 10, 1, RejectReason::ColdStartSlots),
            (FilterState::Warming, 1, 1, RejectReason::MaxFilters),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (db, storage) = storage_for(&temp).await;
            insert_filter(db.as_ref(), 1, "active", state).await;
            insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
            for request in [
                register("new", Some("new-name")),
                register("reclaimed", Some("x-name")),
                AdmitRequest::rewarm(2),
            ] {
                let before = three_table_snapshot(db.as_ref()).await;
                assert_eq!(
                    storage
                        .admit(request, limits(max_filters, slots))
                        .await
                        .unwrap(),
                    AdmitOutcome::Rejected {
                        reason,
                        retry_after: Duration::from_secs(30),
                    }
                );
                assert_eq!(before, three_table_snapshot(db.as_ref()).await);
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "warming", FilterState::Warming).await;
        insert_view(db.as_ref(), 11, "ready-name", 1, 1).await;
        insert_view(db.as_ref(), 12, "warming-name", 1, 2).await;
        for (filter_id, filter_pk, ready, name) in [
            ("ready", 1, true, "ready-name"),
            ("warming", 2, false, "warming-name"),
        ] {
            for (request, version) in [
                (register(filter_id, None::<String>), None),
                (register(filter_id, Some(name)), Some(1)),
                (AdmitRequest::rewarm(filter_pk), None),
            ] {
                let before = three_table_snapshot(db.as_ref()).await;
                assert_eq!(
                    storage.admit(request, limits(100, 2)).await.unwrap(),
                    AdmitOutcome::Idempotent { version, ready }
                );
                assert_eq!(before, three_table_snapshot(db.as_ref()).await);
            }
        }

        let before = three_table_snapshot(db.as_ref()).await;
        assert!(matches!(
            storage
                .admit(AdmitRequest::rewarm(99), limits(100, 2))
                .await,
            Err(MegaError::NotFound(_))
        ));
        assert_eq!(before, three_table_snapshot(db.as_ref()).await);

        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_view(db.as_ref(), 11, "n0", 1, 1).await;
        db.execute_unprepared("ALTER TABLE mega_view RENAME COLUMN created_by TO hp_gone")
            .await
            .unwrap();
        assert_eq!(
            storage
                .admit(register("ready", Some("n0")), limits(100, 2))
                .await
                .unwrap(),
            AdmitOutcome::Idempotent {
                version: Some(1),
                ready: true,
            }
        );
        let before = three_table_snapshot(db.as_ref()).await;
        assert!(matches!(
            storage
                .admit(register("new", Some("new-name")), limits(100, 2))
                .await,
            Err(MegaError::Db(_))
        ));
        assert_eq!(before, three_table_snapshot(db.as_ref()).await);

        let temp = tempfile::tempdir().unwrap();
        let (db, monitor, writer_db, storage, _other_storage, _schema) =
            multi_pool_storages(&temp).await;
        let before = three_table_snapshot(db.as_ref()).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let writer = async move {
            let txn = writer_db.begin().await.unwrap();
            let writer_pid: i32 = txn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT pg_backend_pid() AS pid".to_owned(),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get("", "pid")
                .unwrap();
            mega_view_filter::ActiveModel {
                id: Set(99),
                filter_id: Set("outside-writer".to_owned()),
                canonical_spec: Set("subdir:/outside-writer".to_owned()),
                algo_version: Set(1),
                object_format: Set("sha1".to_owned()),
                src_paths: Set(serde_json::json!(["/"])),
                push_enabled: Set(false),
                projected_seq: Set(0),
                ready_seq: Set(None),
                warming_since: Set(None),
                last_access_at: Set(None),
                created_at: Set(Utc::now().naive_utc()),
            }
            .insert(&txn)
            .await
            .unwrap();
            started_tx.send(()).unwrap();
            let mut observed = false;
            for _ in 0..1_000 {
                let waiting: i64 = monitor
                    .query_one_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "SELECT count(*)::bigint AS count FROM pg_locks w \
                         WHERE w.locktype = 'transactionid' AND NOT w.granted \
                         AND w.transactionid = (SELECT h.transactionid FROM pg_locks h \
                             WHERE h.locktype = 'transactionid' AND h.granted AND h.pid = $1 \
                             LIMIT 1)",
                        [sea_orm::Value::from(writer_pid)],
                    ))
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get("", "count")
                    .unwrap();
                if waiting == 1 {
                    observed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if !observed {
                txn.rollback().await.unwrap();
                panic!("admission did not wait for the external writer");
            }
            txn.commit().await.unwrap();
        };
        let admission = async move {
            started_rx.await.unwrap();
            storage
                .admit(
                    register("outside-writer", Some("outside-name")),
                    limits(100, 2),
                )
                .await
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(admission, writer)
        })
        .await
        .expect("external-writer gate and admission finish within 30 seconds");
        let error = result.unwrap_err();
        assert!(
            matches!(error, MegaError::Db(sea_orm::DbErr::RecordNotInserted)),
            "expected RecordNotInserted, got {error:?}"
        );
        let after = three_table_snapshot(db.as_ref()).await;
        assert_eq!(after.0.len(), before.0.len() + 1);
        assert_eq!(after.1, before.1);
        assert_eq!(after.2, before.2);
    }

    #[tokio::test]
    async fn checks_follow_design() {
        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "warming", FilterState::Warming).await;
        insert_filter(db.as_ref(), 3, "partial", FilterState::Partial).await;
        insert_filter(db.as_ref(), 4, "reclaimed", FilterState::Reclaimed).await;

        assert_eq!(
            storage
                .admit(register("new-a", None::<String>), limits(4, 3))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: None,
                ready: false
            }
        );
        mega_view_filter::Entity::delete_many()
            .filter(mega_view_filter::Column::FilterId.eq("new-a"))
            .exec(db.as_ref())
            .await
            .unwrap();
        assert!(matches!(
            storage
                .admit(register("new-b", None::<String>), limits(3, 3))
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::MaxFilters,
                ..
            }
        ));
        assert!(matches!(
            storage
                .admit(register("new-c", None::<String>), limits(10, 2))
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::ColdStartSlots,
                ..
            }
        ));
        assert_eq!(
            storage
                .admit(register("new-d", None::<String>), limits(10, 3))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: None,
                ready: false
            }
        );

        mega_view_filter::Entity::delete_many()
            .filter(mega_view_filter::Column::FilterId.eq("new-d"))
            .exec(db.as_ref())
            .await
            .unwrap();
        assert_eq!(
            storage
                .admit(register("ready", Some("ready-version")), limits(3, 2))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: Some(1),
                ready: true
            }
        );
        let ready_view = mega_view::Entity::find()
            .filter(mega_view::Column::Name.eq("ready-version"))
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready_view.version, 1);
        assert_eq!(ready_view.filter_pk, 1);
        assert_eq!(
            storage
                .admit(register("partial", Some("partial-version")), limits(3, 2))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: Some(1),
                ready: false
            }
        );
        let partial_view = mega_view::Entity::find()
            .filter(mega_view::Column::Name.eq("partial-version"))
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(partial_view.version, 1);
        assert_eq!(partial_view.filter_pk, 3);
        assert_eq!(
            storage
                .admit(register("warming", None::<String>), limits(3, 2))
                .await
                .unwrap(),
            AdmitOutcome::Idempotent {
                version: None,
                ready: false
            }
        );
    }

    #[tokio::test]
    async fn cold_start_marks_warming() {
        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        let before_views = table_rows(db.as_ref(), "mega_view").await;
        let request = register("new", None::<String>);
        assert_eq!(
            storage
                .admit(request.clone(), limits(100, 2))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: None,
                ready: false
            }
        );
        let new = mega_view_filter::Entity::find()
            .filter(mega_view_filter::Column::FilterId.eq("new"))
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(new.filter_id, "new");
        assert_eq!(new.canonical_spec, "subdir:/new");
        assert_eq!(new.algo_version, 1);
        assert_eq!(new.object_format, "sha1");
        assert_eq!(new.src_paths, serde_json::json!(["/"]));
        assert!(!new.push_enabled);
        assert_eq!(new.projected_seq, 0);
        assert_eq!(new.ready_seq, None);
        assert_eq!(new.last_access_at, None);
        assert!(new.warming_since.is_some());
        assert!(new.created_at.and_utc().timestamp() > 0);
        assert_eq!(before_views, table_rows(db.as_ref(), "mega_view").await);

        insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
        let filter_count_before = mega_view_filter::Entity::find()
            .count(db.as_ref())
            .await
            .unwrap();
        let views_before_reclaim = table_rows(db.as_ref(), "mega_view").await;
        let before = mega_view_filter::Entity::find_by_id(2)
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .admit(register("reclaimed", None::<String>), limits(100, 2))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: None,
                ready: false
            }
        );
        let after = mega_view_filter::Entity::find_by_id(2)
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert!(after.warming_since.is_some());
        assert_eq!(after.id, before.id);
        assert_eq!(after.filter_id, before.filter_id);
        assert_eq!(after.canonical_spec, before.canonical_spec);
        assert_eq!(after.projected_seq, before.projected_seq);
        assert_eq!(after.ready_seq, before.ready_seq);
        assert_eq!(after.last_access_at, before.last_access_at);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(
            mega_view_filter::Entity::find()
                .count(db.as_ref())
                .await
                .unwrap(),
            filter_count_before
        );
        assert_eq!(
            views_before_reclaim,
            table_rows(db.as_ref(), "mega_view").await
        );
        let logs_before = mega_view_register_log::Entity::find()
            .all(db.as_ref())
            .await
            .unwrap();

        mega_view_filter::Entity::update_many()
            .col_expr(
                mega_view_filter::Column::WarmingSince,
                sea_orm::sea_query::Expr::value(None::<chrono::NaiveDateTime>),
            )
            .filter(mega_view_filter::Column::Id.eq(2))
            .exec(db.as_ref())
            .await
            .unwrap();
        let rewarm_before = mega_view_filter::Entity::find_by_id(2)
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        let views_before_rewarm = table_rows(db.as_ref(), "mega_view").await;
        assert_eq!(
            storage
                .admit(AdmitRequest::rewarm(2), limits(100, 2))
                .await
                .unwrap(),
            AdmitOutcome::Admitted {
                version: None,
                ready: false
            }
        );
        let rewarm_after = mega_view_filter::Entity::find_by_id(2)
            .one(db.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert!(rewarm_after.warming_since.is_some());
        assert_eq!(rewarm_after.id, rewarm_before.id);
        assert_eq!(rewarm_after.filter_id, rewarm_before.filter_id);
        assert_eq!(rewarm_after.canonical_spec, rewarm_before.canonical_spec);
        assert_eq!(rewarm_after.algo_version, rewarm_before.algo_version);
        assert_eq!(rewarm_after.object_format, rewarm_before.object_format);
        assert_eq!(rewarm_after.src_paths, rewarm_before.src_paths);
        assert_eq!(rewarm_after.push_enabled, rewarm_before.push_enabled);
        assert_eq!(rewarm_after.projected_seq, rewarm_before.projected_seq);
        assert_eq!(rewarm_after.ready_seq, rewarm_before.ready_seq);
        assert_eq!(rewarm_after.last_access_at, rewarm_before.last_access_at);
        assert_eq!(rewarm_after.created_at, rewarm_before.created_at);
        assert_eq!(
            views_before_rewarm,
            table_rows(db.as_ref(), "mega_view").await
        );
        assert_eq!(
            logs_before,
            mega_view_register_log::Entity::find()
                .all(db.as_ref())
                .await
                .unwrap()
        );
    }

    type ExistingViews = Option<(FilterState, &'static str, Vec<(i32, i64)>)>;
    type TargetFilter = Option<(FilterState, &'static str)>;

    #[tokio::test]
    async fn named_register_versions() {
        async fn run_case(
            existing: ExistingViews,
            target: TargetFilter,
            name: &str,
            expected: AdmitOutcome,
            expected_new_view: bool,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let (db, storage) = storage_for(&temp).await;
            let mut next_id = 1;
            if let Some((state, filter_id, versions)) = existing {
                let other = insert_filter(db.as_ref(), next_id, filter_id, state).await;
                next_id += 1;
                for (version, filter_pk) in versions {
                    insert_view(db.as_ref(), 100 + version as i64, name, version, filter_pk).await;
                }
                let _ = other;
            }
            let (state, target_id) = target.unwrap_or((FilterState::Reclaimed, "new"));
            let target_was_reclaimed = target.is_none() || matches!(state, FilterState::Reclaimed);
            if target.is_some() {
                insert_filter(db.as_ref(), next_id, target_id, state).await;
            }
            let before = table_rows(db.as_ref(), "mega_view").await;
            let outcome = storage
                .admit(register(target_id, Some(name)), limits(100, 10))
                .await
                .unwrap();
            assert_eq!(outcome, expected);
            let target_row = mega_view_filter::Entity::find()
                .filter(mega_view_filter::Column::FilterId.eq(target_id))
                .one(db.as_ref())
                .await
                .unwrap()
                .unwrap();
            if target_was_reclaimed {
                assert!(target_row.warming_since.is_some());
            }
            let after = table_rows(db.as_ref(), "mega_view").await;
            if expected_new_view {
                assert_eq!(after.len(), before.len() + 1);
                assert!(before.iter().all(|row| after.contains(row)));
                let latest = mega_view::Entity::find()
                    .filter(mega_view::Column::Name.eq(name))
                    .order_by_desc(mega_view::Column::Version)
                    .one(db.as_ref())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    latest.version,
                    match outcome {
                        AdmitOutcome::Admitted {
                            version: Some(v), ..
                        } => v,
                        _ => unreachable!(),
                    }
                );
                assert_eq!(latest.filter_pk, target_row.id);
                assert_eq!(latest.created_by, "r1");
                assert!(latest.created_at.and_utc().timestamp() > 0);
            } else {
                assert_eq!(after, before);
            }
        }

        run_case(
            None,
            None,
            "new-name",
            AdmitOutcome::Admitted {
                version: Some(1),
                ready: false,
            },
            true,
        )
        .await;
        run_case(
            Some((FilterState::Ready, "other", vec![(1, 1), (2, 1)])),
            None,
            "other-name",
            AdmitOutcome::Admitted {
                version: Some(3),
                ready: false,
            },
            true,
        )
        .await;
        run_case(
            Some((FilterState::Ready, "other", vec![(1, 1), (2, 1)])),
            Some((FilterState::Ready, "target")),
            "other-name",
            AdmitOutcome::Admitted {
                version: Some(3),
                ready: true,
            },
            true,
        )
        .await;
        run_case(
            Some((FilterState::Ready, "other", vec![(1, 2), (2, 1)])),
            Some((FilterState::Ready, "target")),
            "split-name",
            AdmitOutcome::Admitted {
                version: Some(3),
                ready: true,
            },
            true,
        )
        .await;
        run_case(
            None,
            Some((FilterState::Reclaimed, "reclaimed")),
            "reclaimed-name",
            AdmitOutcome::Admitted {
                version: Some(1),
                ready: false,
            },
            true,
        )
        .await;
        run_case(
            Some((FilterState::Ready, "other", vec![(1, 1), (2, 2)])),
            Some((FilterState::Reclaimed, "reclaimed")),
            "reclaimed-existing-name",
            AdmitOutcome::Admitted {
                version: Some(2),
                ready: false,
            },
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn limits_follow_reload() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.views.max_concurrent_cold_starts = 1;
        let storage: Storage = test_storage_with_config(temp.path(), config).await;
        let views = storage.view_storage();
        let db = views.get_connection();
        insert_filter(db, 1, "warming", FilterState::Warming).await;
        let before = three_table_snapshot(db).await;
        assert!(matches!(
            views
                .admit(
                    register("a", None::<String>),
                    AdmitLimits::from(&storage.config().views)
                )
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::ColdStartSlots,
                ..
            }
        ));
        assert_eq!(before, three_table_snapshot(db).await);

        let mut candidate = storage.config().as_ref().clone();
        candidate.views.max_concurrent_cold_starts = 2;
        let report = storage.config_handle().reload(candidate).unwrap();
        assert!(
            report
                .applied_fields
                .contains(&"views.max_concurrent_cold_starts")
        );
        assert!(matches!(
            views
                .admit(
                    register("a", None::<String>),
                    AdmitLimits::from(&storage.config().views)
                )
                .await
                .unwrap(),
            AdmitOutcome::Admitted { .. }
        ));

        let mut candidate = storage.config().as_ref().clone();
        candidate.views.max_filters = 1;
        let report = storage.config_handle().reload(candidate).unwrap();
        assert!(report.applied_fields.contains(&"views.max_filters"));
        let before = three_table_snapshot(db).await;
        assert!(matches!(
            views
                .admit(
                    register("b", None::<String>),
                    AdmitLimits::from(&storage.config().views)
                )
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::MaxFilters,
                ..
            }
        ));
        assert_eq!(before, three_table_snapshot(db).await);
    }

    #[tokio::test]
    async fn rate_rejects_iff_requester_window_full() {
        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
        let limits = rate_limits(3, 100, 100);
        let cases = [
            RegisterRateCase::NewFilter,
            RegisterRateCase::NewVersion,
            RegisterRateCase::ReclaimedFilter,
        ];

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "empty", "r1"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Admitted { .. }
            ));
        }

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            insert_window_rows(db.as_ref(), "r1", 2).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "two", "r1"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Admitted { .. }
            ));
        }

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            insert_window_rows(db.as_ref(), "r1", 3).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "full", "r1"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Rejected {
                    reason: RejectReason::Rate,
                    ..
                }
            ));
        }

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            for _ in 0..5 {
                insert_register_log(db.as_ref(), "r1", -3601).await;
            }
            insert_window_rows(db.as_ref(), "r1", 2).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "expired", "r1"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Admitted { .. }
            ));
        }

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            insert_window_rows(db.as_ref(), "r1", 2).await;
            insert_window_rows(db.as_ref(), "r2", 3).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "isolation-r1", "r1"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Admitted { .. }
            ));

            clear_register_logs(db.as_ref()).await;
            insert_window_rows(db.as_ref(), "r1", 2).await;
            insert_window_rows(db.as_ref(), "r2", 3).await;
            prepare_rate_case(db.as_ref(), case).await;
            assert!(matches!(
                storage
                    .admit(rate_case_request(case, "isolation-r2", "r2"), limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Rejected {
                    reason: RejectReason::Rate,
                    ..
                }
            ));
        }

        for case in cases {
            clear_register_logs(db.as_ref()).await;
            insert_window_rows(db.as_ref(), "r1", 3).await;
            let mut config = isolated_config(temp.path().join("rate-config"));
            config.views.register_rate_per_token = 3;
            config.views.max_filters = 100;
            config.views.max_concurrent_cold_starts = 100;
            let handle = ConfigHandle::new(config);
            prepare_rate_case(db.as_ref(), case).await;
            let request = rate_case_request(case, "reloaded", "r1");
            let current_limits = AdmitLimits::from(&handle.snapshot().unwrap().views);
            assert!(matches!(
                storage
                    .admit(request.clone(), current_limits)
                    .await
                    .unwrap(),
                AdmitOutcome::Rejected {
                    reason: RejectReason::Rate,
                    ..
                }
            ));
            let mut candidate = handle.snapshot().unwrap().as_ref().clone();
            candidate.views.register_rate_per_token = 4;
            let report = handle.reload(candidate).unwrap();
            assert!(
                report
                    .applied_fields
                    .contains(&"views.register_rate_per_token")
            );
            let reloaded_limits = AdmitLimits::from(&handle.snapshot().unwrap().views);
            assert!(matches!(
                storage.admit(request, reloaded_limits).await.unwrap(),
                AdmitOutcome::Admitted { .. }
            ));
        }

        let ordering_temp = tempfile::tempdir().unwrap();
        let (ordering_db, ordering_storage) = storage_for(&ordering_temp).await;
        insert_filter(ordering_db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(ordering_db.as_ref(), 2, "warming-a", FilterState::Warming).await;
        insert_filter(ordering_db.as_ref(), 3, "warming-b", FilterState::Warming).await;
        let full_limits = rate_limits(3, 2, 1);
        insert_window_rows(ordering_db.as_ref(), "r1", 3).await;
        assert!(matches!(
            ordering_storage
                .admit(
                    register("new-rate-before-quotas", None::<String>),
                    full_limits
                )
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::Rate,
                ..
            }
        ));
        clear_register_logs(ordering_db.as_ref()).await;
        insert_window_rows(ordering_db.as_ref(), "r1", 2).await;
        assert!(matches!(
            ordering_storage
                .admit(
                    register("new-quotas-after-rate", None::<String>),
                    full_limits
                )
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::MaxFilters,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn rate_rejection_writes_nothing() {
        async fn assert_rejected_unchanged(
            db: &DatabaseConnection,
            storage: &ViewStorage,
            request: AdmitRequest,
            limits: AdmitLimits,
            reason: RejectReason,
        ) {
            let before = three_table_snapshot(db).await;
            let outcome = storage.admit(request, limits).await.unwrap();
            assert!(matches!(
                outcome,
                AdmitOutcome::Rejected { reason: actual, .. } if actual == reason
            ));
            assert_eq!(before, three_table_snapshot(db).await);
        }

        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
        let ordinary_limits = rate_limits(3, 100, 100);

        for request in [
            register("new-rate-rejected", None::<String>),
            register("ready", Some("ready-rate-rejected")),
            register("reclaimed", None::<String>),
        ] {
            clear_register_logs(db.as_ref()).await;
            insert_register_log(db.as_ref(), "r1", -7200).await;
            insert_window_rows(db.as_ref(), "r1", 3).await;
            assert_rejected_unchanged(
                db.as_ref(),
                &storage,
                request,
                ordinary_limits,
                RejectReason::Rate,
            )
            .await;
        }

        insert_filter(db.as_ref(), 3, "warming-a", FilterState::Warming).await;
        insert_filter(db.as_ref(), 4, "warming-b", FilterState::Warming).await;
        let full_limits = rate_limits(3, 2, 1);
        for request in [
            register("new-max-filters", None::<String>),
            register("reclaimed", None::<String>),
        ] {
            clear_register_logs(db.as_ref()).await;
            insert_register_log(db.as_ref(), "r1", -7200).await;
            insert_window_rows(db.as_ref(), "r1", 2).await;
            assert_rejected_unchanged(
                db.as_ref(),
                &storage,
                request,
                full_limits,
                RejectReason::MaxFilters,
            )
            .await;
            let reclaimed = mega_view_filter::Entity::find_by_id(2)
                .one(db.as_ref())
                .await
                .unwrap()
                .unwrap();
            assert!(reclaimed.warming_since.is_none());
        }

        mega_view_filter::Entity::delete_by_id(3)
            .exec(db.as_ref())
            .await
            .unwrap();
        mega_view_filter::Entity::delete_by_id(4)
            .exec(db.as_ref())
            .await
            .unwrap();
        insert_filter(db.as_ref(), 5, "warming-slot", FilterState::Warming).await;
        let slot_limits = rate_limits(3, 100, 1);
        for request in [
            register("new-slot", None::<String>),
            register("reclaimed", None::<String>),
        ] {
            clear_register_logs(db.as_ref()).await;
            insert_register_log(db.as_ref(), "r1", -7200).await;
            insert_window_rows(db.as_ref(), "r1", 2).await;
            assert_rejected_unchanged(
                db.as_ref(),
                &storage,
                request,
                slot_limits,
                RejectReason::ColdStartSlots,
            )
            .await;
        }
        assert!(
            mega_view_filter::Entity::find_by_id(2)
                .one(db.as_ref())
                .await
                .unwrap()
                .unwrap()
                .warming_since
                .is_none()
        );
    }

    #[tokio::test]
    async fn rate_retry_after_from_oldest_row() {
        async fn insert_retry_window(db: &DatabaseConnection) {
            clear_register_logs(db).await;
            db.execute_unprepared(
                "INSERT INTO mega_view_register_log (requester, created_at) VALUES \
                 ('r1', localtimestamp - interval '3000.4 seconds'), \
                 ('r1', localtimestamp - interval '1200 seconds'), \
                 ('r1', localtimestamp - interval '10 seconds'), \
                 ('r2', localtimestamp - interval '3500 seconds')",
            )
            .await
            .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
        let limits = rate_limits(3, 100, 100);

        for request in [
            register("new-retry", None::<String>),
            register("ready", Some("ready-retry")),
            register("reclaimed", None::<String>),
        ] {
            insert_retry_window(db.as_ref()).await;
            let upper = retry_after_upper_bound(db.as_ref(), "r1").await;
            let outcome = storage.admit(request, limits).await.unwrap();
            let lower = retry_after_upper_bound(db.as_ref(), "r1").await;
            let AdmitOutcome::Rejected {
                reason: RejectReason::Rate,
                retry_after,
            } = outcome
            else {
                panic!("rate window should reject");
            };
            let retry_after = i64::try_from(retry_after.as_secs()).unwrap();
            assert!(lower <= retry_after && retry_after <= upper);
        }

        clear_register_logs(db.as_ref()).await;
        for offset in [10, 20, 30] {
            insert_register_log(db.as_ref(), "r1", offset).await;
        }
        assert_eq!(
            storage
                .admit(register("new-clamped", None::<String>), limits)
                .await
                .unwrap(),
            AdmitOutcome::Rejected {
                reason: RejectReason::Rate,
                retry_after: Duration::from_secs(REGISTER_RATE_WINDOW_SECS),
            }
        );
    }

    #[tokio::test]
    async fn rate_admitted_appends_and_trims() {
        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "reclaimed", FilterState::Reclaimed).await;
        let limits = rate_limits(3, 100, 100);

        for case in 0..4 {
            clear_register_logs(db.as_ref()).await;
            insert_register_log(db.as_ref(), "r1", -7200).await;
            insert_register_log(db.as_ref(), "r1", -3601).await;
            insert_register_log(db.as_ref(), "r1", -60).await;
            insert_register_log(db.as_ref(), "r2", -7200).await;
            insert_register_log(db.as_ref(), "r2", -60).await;
            if matches!(case, 2 | 3) {
                reset_reclaimed_filter(db.as_ref(), 2).await;
            }

            let before = register_log_models(db.as_ref()).await;
            let r1_expired = before
                .iter()
                .filter(|row| row.requester == "r1")
                .take(2)
                .map(|row| row.id)
                .collect::<Vec<_>>();
            let views_before = mega_view::Entity::find()
                .filter(mega_view::Column::FilterPk.eq(2))
                .count(db.as_ref())
                .await
                .unwrap();
            let request = match case {
                0 => register("new-log", None::<String>),
                1 => register("ready", Some("ready-log")),
                2 => register("reclaimed", None::<String>),
                3 => register("reclaimed", Some("reclaimed-log")),
                _ => unreachable!(),
            };
            assert!(matches!(
                storage.admit(request, limits).await.unwrap(),
                AdmitOutcome::Admitted { .. }
            ));
            let after = register_log_models(db.as_ref()).await;
            let before_ids = before.iter().map(|row| row.id).collect::<Vec<_>>();
            let after_ids = after.iter().map(|row| row.id).collect::<Vec<_>>();
            let added = after
                .iter()
                .filter(|row| !before_ids.contains(&row.id))
                .collect::<Vec<_>>();
            let removed = before
                .iter()
                .filter(|row| !after_ids.contains(&row.id))
                .map(|row| row.id)
                .collect::<Vec<_>>();
            assert_eq!(added.len(), 1);
            assert_eq!(added[0].requester, "r1");
            assert_eq!(removed, r1_expired);
            assert!(after.iter().any(|row| row.requester == "r1"));
            assert_eq!(after.iter().filter(|row| row.requester == "r2").count(), 2);
            if case == 3 {
                assert!(
                    mega_view_filter::Entity::find_by_id(2)
                        .one(db.as_ref())
                        .await
                        .unwrap()
                        .unwrap()
                        .warming_since
                        .is_some()
                );
                assert_eq!(
                    mega_view::Entity::find()
                        .filter(mega_view::Column::FilterPk.eq(2))
                        .count(db.as_ref())
                        .await
                        .unwrap(),
                    views_before + 1
                );
            }
        }
    }

    #[tokio::test]
    async fn rate_idempotent_and_rewarm_not_counted() {
        let temp = tempfile::tempdir().unwrap();
        let (db, storage) = storage_for(&temp).await;
        insert_filter(db.as_ref(), 1, "ready", FilterState::Ready).await;
        insert_filter(db.as_ref(), 2, "warming", FilterState::Warming).await;
        insert_filter(db.as_ref(), 3, "reclaimed", FilterState::Reclaimed).await;
        insert_view(db.as_ref(), 1, "n1", 1, 1).await;
        insert_window_rows(db.as_ref(), "r1", 3).await;
        insert_register_log(db.as_ref(), "r1", -7200).await;
        insert_window_rows(db.as_ref(), "anonymous", 3).await;
        let limits = rate_limits(3, 100, 100);

        let before = register_log_models(db.as_ref()).await;
        assert!(matches!(
            storage
                .admit(register("ready", Some("n1")), limits)
                .await
                .unwrap(),
            AdmitOutcome::Idempotent { .. }
        ));
        assert_eq!(before, register_log_models(db.as_ref()).await);
        assert!(matches!(
            storage
                .admit(register("warming", None::<String>), limits)
                .await
                .unwrap(),
            AdmitOutcome::Idempotent { .. }
        ));
        assert_eq!(before, register_log_models(db.as_ref()).await);
        assert!(matches!(
            storage
                .admit(AdmitRequest::rewarm(3), limits)
                .await
                .unwrap(),
            AdmitOutcome::Admitted { .. }
        ));
        assert_eq!(before, register_log_models(db.as_ref()).await);
    }

    #[tokio::test]
    async fn rate_concurrent_last_quota() {
        let temp = tempfile::tempdir().unwrap();
        let (first, _second, holder, a, b, _guard) = multi_pool_storages(&temp).await;
        let limits = rate_limits(3, 100, 100);

        for round in 0..10 {
            clear_register_logs(first.as_ref()).await;
            insert_window_rows(first.as_ref(), "r1", 2).await;
            let filters_before = mega_view_filter::Entity::find()
                .count(first.as_ref())
                .await
                .unwrap();
            let txn = holder.begin().await.unwrap();
            assert!(
                acquire_view_lock(&txn, ViewLock::Register, ViewLockMode::Blocking)
                    .await
                    .unwrap()
            );
            let admission_a = a.admit(register(format!("rate-a-{round}"), None::<String>), limits);
            let admission_b = b.admit(register(format!("rate-b-{round}"), None::<String>), limits);
            let gate_holder = holder.clone();
            let gate = async move {
                if !wait_for_register_waiters(gate_holder.as_ref(), 2).await {
                    txn.rollback().await.unwrap();
                    panic!("did not observe two rate-register waiters");
                }
                txn.commit().await.unwrap();
            };
            let (a_result, b_result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::join!(admission_a, admission_b, gate)
            })
            .await
            .expect("rate gate and admissions finish within 30 seconds");
            let outcomes = [a_result.unwrap(), b_result.unwrap()];
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, AdmitOutcome::Admitted { .. }))
                    .count(),
                1
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(
                        outcome,
                        AdmitOutcome::Rejected {
                            reason: RejectReason::Rate,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert_eq!(register_log_models(first.as_ref()).await.len(), 3);
            assert_eq!(
                mega_view_filter::Entity::find()
                    .count(first.as_ref())
                    .await
                    .unwrap(),
                filters_before + 1
            );
        }
    }
}
