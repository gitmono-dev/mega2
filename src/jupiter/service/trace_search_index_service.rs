//! Background GIN readiness for trace fragment search.
//!
//! Storage initialization does not wait for this index. A service process
//! derives its own state from the catalog at least once a minute.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use sea_orm::{DatabaseConnection, sqlx};
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    jupiter::storage::trace_search_index_storage::{
        CatalogFailure, CatalogValidation, validate_catalog,
    },
};

pub const CATALOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    Ready,
    Unavailable(CatalogFailure),
}

#[derive(Clone)]
pub struct TraceSearchIndexService {
    db: DatabaseConnection,
    state: Arc<Mutex<IndexState>>,
}

impl TraceSearchIndexService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            state: Arc::new(Mutex::new(IndexState::Unavailable(CatalogFailure::Missing))),
        }
    }

    pub fn state(&self) -> IndexState {
        match self.state.lock() {
            Ok(state) => state.clone(),
            Err(_) => IndexState::Unavailable(CatalogFailure::CatalogUnreadable),
        }
    }

    pub fn spawn_background(self, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(CATALOG_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        tokio::select! {
                            _ = shutdown.cancelled() => break,
                            result = self.ensure_once() => {
                                if let Err(error) = result {
                                    tracing::error!(error = %error, "trace search index cycle failed");
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn ensure_once(&self) -> Result<IndexState, MegaError> {
        let result = self.ensure_cycle().await;
        if result.is_err() {
            // A failed probe must not leave a previously healthy state in service.
            self.store(IndexState::Unavailable(CatalogFailure::CatalogUnreadable));
        }
        result
    }

    async fn ensure_cycle(&self) -> Result<IndexState, MegaError> {
        match validate_catalog(&self.db).await? {
            CatalogValidation::Ready(_) => {
                self.store(IndexState::Ready);
                return Ok(IndexState::Ready);
            }
            CatalogValidation::Unavailable(CatalogFailure::Missing) => {
                // Not Ready while the build runs, even if it was Ready before.
                self.store(IndexState::Unavailable(CatalogFailure::Missing));
            }
            CatalogValidation::Unavailable(reason) => {
                let state = IndexState::Unavailable(reason);
                self.store(state.clone());
                return Ok(state);
            }
        }

        // PostgreSQL session advisory locks belong to one physical connection.
        // Keep the lease and concurrent DDL on that same connection and close
        // it on drop, including cancellation, so it cannot return a held lock
        // to the pool.
        let mut connection = self
            .db
            .get_postgres_connection_pool()
            .acquire()
            .await
            .map_err(|error| {
                MegaError::Other(format!("acquire trace index connection: {error}"))
            })?;
        connection.close_on_drop();
        // A backend blocked on a lock does not notice that the client went
        // away, so it would keep the session lease. Bound the wait on the
        // server side, and terminate this backend if the cycle is dropped
        // before the DDL returns.
        let pid: i32 = sqlx::query_scalar("SELECT pg_catalog.pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| MegaError::Other(format!("read trace index backend: {error}")))?;
        let mut guard = TerminateOnDrop {
            pool: self.db.get_postgres_connection_pool().clone(),
            pid: Some(pid),
        };
        sqlx::query("SET statement_timeout = '1800s'")
            .execute(&mut *connection)
            .await
            .map_err(|error| MegaError::Other(format!("bound trace index build: {error}")))?;
        sqlx::query("SET lock_timeout = '30s'")
            .execute(&mut *connection)
            .await
            .map_err(|error| MegaError::Other(format!("bound trace index lock wait: {error}")))?;
        let locked: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_lock(hashtextextended('trace_fragment_ascii_fold_trgm_v1:' || to_regclass('trace_fragment')::oid::text, 0))",
        ).fetch_one(&mut *connection).await
            .map_err(|error| MegaError::Other(format!("acquire trace index lease: {error}")))?;
        if !locked {
            guard.disarm();
            let state = IndexState::Unavailable(CatalogFailure::Missing);
            self.store(state.clone());
            return Ok(state);
        }
        let schema: String = sqlx::query_scalar(
            "SELECT pg_catalog.quote_ident(n.nspname) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = pg_catalog.to_regclass('trace_fragment') AND c.relkind = 'r'",
        ).fetch_one(&mut *connection).await
            .map_err(|error| MegaError::Other(format!("resolve trace fragment schema: {error}")))?;
        let ddl = crate::jupiter::storage::trace_search_index_storage::canonical_index_ddl(&schema);
        let result = tokio::time::timeout(
            Duration::from_secs(1800),
            sqlx::query(sqlx::AssertSqlSafe(ddl)).execute(&mut *connection),
        )
        .await;
        // The statement finished on the server, so the backend is idle and
        // closing the connection releases the lease. A client-side timeout
        // leaves the guard armed and terminates the still-running backend.
        if result.is_ok() {
            guard.disarm();
        }
        drop(guard);
        drop(connection);
        let state = match result {
            Ok(Ok(_)) => match validate_catalog(&self.db).await? {
                CatalogValidation::Ready(_) => IndexState::Ready,
                CatalogValidation::Unavailable(reason) => IndexState::Unavailable(reason),
            },
            Ok(Err(error)) => {
                match error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref()
                {
                    Some("42501") => IndexState::Unavailable(CatalogFailure::IndexPermissionDenied),
                    Some("57014") | Some("55P03") => {
                        IndexState::Unavailable(CatalogFailure::BuildTimedOut)
                    }
                    _ => {
                        tracing::warn!(error = %error, "trace GIN build failed");
                        IndexState::Unavailable(CatalogFailure::Missing)
                    }
                }
            }
            Err(_) => IndexState::Unavailable(CatalogFailure::BuildTimedOut),
        };
        self.store(state.clone());
        Ok(state)
    }

    fn store(&self, state: IndexState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }
}

/// Terminates the lease-holding backend when a build cycle is dropped early.
struct TerminateOnDrop {
    pool: sqlx::PgPool,
    pid: Option<i32>,
}

impl TerminateOnDrop {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let pool = self.pool.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = sqlx::query("SELECT pg_catalog.pg_terminate_backend($1)")
                    .bind(pid)
                    .execute(&pool)
                    .await
                {
                    tracing::warn!(error = %error, pid, "terminate trace index backend failed");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend};

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn database() -> (tempfile::TempDir, DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        db.execute_raw(sea_orm::Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner"
                .to_owned(),
        ))
        .await
        .unwrap();
        db.execute_raw(sea_orm::Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6'"
                .to_owned(),
        ))
        .await
        .unwrap();
        (directory, db)
    }

    #[tokio::test]
    async fn gin_ensure_reaches_ready_and_keeps_one_lease() {
        let (_dir, db) = database().await;
        let service = TraceSearchIndexService::new(db.clone());
        assert_eq!(service.ensure_once().await.unwrap(), IndexState::Ready);
        let other = TraceSearchIndexService::new(db);
        assert_eq!(other.ensure_once().await.unwrap(), IndexState::Ready);
    }

    #[tokio::test]
    async fn gin_ensure_permission_denied_reaches_index_state() {
        let (_dir, db) = database().await;
        // A table the restricted role can see but does not own, so the real
        // service build is refused with insufficient_privilege (42501).
        db.execute_raw(sea_orm::Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE TABLE public.trace_fragment (ascii_fold text NOT NULL)".to_owned(),
        ))
        .await
        .unwrap();
        // A separate pool whose every session runs as the pre-provisioned
        // extension owner. No cluster role is created, altered, or dropped.
        let options = db
            .get_postgres_connection_pool()
            .connect_options()
            .as_ref()
            .clone()
            .options([
                ("search_path", "public"),
                ("role", "mega2_traces_ext_owner"),
            ]);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let restricted = sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool);
        let service = TraceSearchIndexService::new(restricted.clone());
        assert_eq!(
            service.ensure_once().await.unwrap(),
            IndexState::Unavailable(CatalogFailure::IndexPermissionDenied)
        );
        assert_eq!(
            service.state(),
            IndexState::Unavailable(CatalogFailure::IndexPermissionDenied)
        );
        restricted.close().await.unwrap();
    }

    #[tokio::test]
    async fn gin_ensure_after_operator_install_reaches_ready() {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let service = TraceSearchIndexService::new(db.clone());
        assert_eq!(
            service.ensure_once().await.unwrap(),
            IndexState::Unavailable(CatalogFailure::ExtensionMissing)
        );
        // Operator install inside this disposable database only.
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner",
            "CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6'",
        ] {
            db.execute_raw(sea_orm::Statement::from_string(
                DatabaseBackend::Postgres,
                sql.to_owned(),
            ))
            .await
            .unwrap();
        }
        assert_eq!(service.ensure_once().await.unwrap(), IndexState::Ready);
        assert!(matches!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Ready(_)
        ));
    }

    #[tokio::test]
    async fn gin_ensure_contending_session_does_not_build() {
        let (_dir, db) = database().await;
        let mut holder = db.get_postgres_connection_pool().acquire().await.unwrap();
        holder.close_on_drop();
        let locked: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_lock(hashtextextended('trace_fragment_ascii_fold_trgm_v1:' || to_regclass('trace_fragment')::oid::text, 0))",
        )
        .fetch_one(&mut *holder)
        .await
        .unwrap();
        assert!(locked);
        let service = TraceSearchIndexService::new(db.clone());
        assert_eq!(
            service.ensure_once().await.unwrap(),
            IndexState::Unavailable(CatalogFailure::Missing)
        );
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Missing)
        );
        let released: bool = sqlx::query_scalar(
            "SELECT pg_advisory_unlock(hashtextextended('trace_fragment_ascii_fold_trgm_v1:' || to_regclass('trace_fragment')::oid::text, 0))",
        )
        .fetch_one(&mut *holder)
        .await
        .unwrap();
        assert!(released);
        drop(holder);
        assert_eq!(service.ensure_once().await.unwrap(), IndexState::Ready);
    }

    #[tokio::test]
    async fn gin_ensure_catalog_error_invalidates_cached_ready() {
        let (_dir, db) = database().await;
        let service = TraceSearchIndexService::new(db.clone());
        service.store(IndexState::Ready);
        db.close().await.unwrap();
        assert!(service.ensure_once().await.is_err());
        assert_ne!(service.state(), IndexState::Ready);
    }

    #[tokio::test]
    async fn gin_external_catalog_adoption_without_restart() {
        async fn wait_for(
            service: &TraceSearchIndexService,
            expected: IndexState,
            limit: Duration,
        ) -> bool {
            tokio::time::timeout(limit, async {
                while service.state() != expected {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await
            .is_ok()
        }

        let started = std::time::Instant::now();
        let (_dir, db) = database().await;
        // An independent session holds a conflicting table lock, so the
        // service's own concurrent build hits the server-side lock_timeout.
        let mut external = db.get_postgres_connection_pool().acquire().await.unwrap();
        external.close_on_drop();
        sqlx::query("BEGIN").execute(&mut *external).await.unwrap();
        sqlx::query("LOCK TABLE trace_fragment IN SHARE UPDATE EXCLUSIVE MODE")
            .execute(&mut *external)
            .await
            .unwrap();

        let service = TraceSearchIndexService::new(db.clone());
        let shutdown = CancellationToken::new();
        service.clone().spawn_background(shutdown.clone());
        let timed_out = wait_for(
            &service,
            IndexState::Unavailable(CatalogFailure::BuildTimedOut),
            Duration::from_secs(50),
        )
        .await;
        sqlx::query("ROLLBACK")
            .execute(&mut *external)
            .await
            .unwrap();
        assert!(
            timed_out,
            "the real build must time out on the held lock: {:?}",
            service.state()
        );

        // A mismatched index under the canonical name must not be adopted.
        sqlx::query(
            "CREATE INDEX trace_fragment_ascii_fold_trgm_v1 ON trace_fragment (ascii_fold)",
        )
        .execute(&mut *external)
        .await
        .unwrap();
        let mismatched = wait_for(
            &service,
            IndexState::Unavailable(CatalogFailure::Method),
            Duration::from_secs(45),
        )
        .await;
        assert!(
            mismatched,
            "a btree index must stay unavailable: {:?}",
            service.state()
        );

        // The external connection replaces it with the canonical DDL; the same
        // live service adopts it on a later 60-second catalog tick.
        sqlx::query("DROP INDEX trace_fragment_ascii_fold_trgm_v1")
            .execute(&mut *external)
            .await
            .unwrap();
        let schema: String = sqlx::query_scalar("SELECT quote_ident(current_schema())")
            .fetch_one(&mut *external)
            .await
            .unwrap();
        let ddl = crate::jupiter::storage::trace_search_index_storage::canonical_index_ddl(&schema);
        sqlx::query(sqlx::AssertSqlSafe(ddl))
            .execute(&mut *external)
            .await
            .unwrap();
        assert_eq!(
            service.state(),
            IndexState::Unavailable(CatalogFailure::Method),
            "adoption happens on the periodic tick, not immediately"
        );
        let ready = wait_for(&service, IndexState::Ready, Duration::from_secs(75)).await;
        shutdown.cancel();
        assert!(
            ready,
            "a live service must adopt the external canonical index"
        );
        assert!(
            started.elapsed() <= Duration::from_secs(180),
            "case budget exceeded: {:?}",
            started.elapsed()
        );
    }
}
