//! Background GIN readiness for trace fragment search.
//!
//! Storage initialization does not wait for this index. A service process
//! derives its own state from the catalog at least once a minute.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    jupiter::storage::trace_search_index_storage::{
        CatalogFailure, CatalogValidation, create_canonical_index, validate_catalog,
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
        self.state.lock().expect("search index state").clone()
    }

    pub fn spawn_background(self, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(CATALOG_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        let _ = self.release_lease().await;
                        break;
                    }
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            let _ = self.release_lease().await;
                            break;
                        }
                        if let Err(error) = self.ensure_once().await {
                            tracing::error!(error = %error, "trace search index cycle failed");
                        }
                    }
                }
            }
        });
    }

    pub async fn ensure_once(&self) -> Result<IndexState, MegaError> {
        let validation = validate_catalog(&self.db).await?;
        if let CatalogValidation::Ready(_) = validation {
            self.store(IndexState::Ready);
            return Ok(IndexState::Ready);
        }
        if !self.try_lease().await? {
            let state = IndexState::Unavailable(CatalogFailure::IndexPermissionDenied);
            self.store(state.clone());
            return Ok(state);
        }
        let built = create_canonical_index(&self.db).await;
        let _ = self.release_lease().await;
        let state = match built {
            Ok(()) => match validate_catalog(&self.db).await? {
                CatalogValidation::Ready(_) => IndexState::Ready,
                CatalogValidation::Unavailable(reason) => IndexState::Unavailable(reason),
            },
            Err(error) if format!("{error}").contains("permission denied") => {
                IndexState::Unavailable(CatalogFailure::IndexPermissionDenied)
            }
            Err(_) => IndexState::Unavailable(CatalogFailure::BuildTimedOut),
        };
        self.store(state.clone());
        Ok(state)
    }

    fn store(&self, state: IndexState) {
        *self.state.lock().expect("search index state") = state;
    }

    async fn try_lease(&self) -> Result<bool, MegaError> {
        let row = self
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_try_advisory_lock(hashtext('trace_fragment_ascii_fold_trgm_v1')) AS locked".to_owned(),
            ))
            .await?
            .ok_or_else(|| MegaError::Other("search index lease returned no row".into()))?;
        row.try_get("", "locked")
            .map_err(|error| MegaError::Other(error.to_string()))
    }

    async fn release_lease(&self) -> Result<(), MegaError> {
        self.db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_advisory_unlock(hashtext('trace_fragment_ascii_fold_trgm_v1'))"
                    .to_owned(),
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jupiter::{
        migration::apply_migrations, storage::trace_search_index_storage::create_canonical_index,
        tests::test_db_connection,
    };

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
    async fn gin_external_catalog_adoption_without_restart() {
        let (_dir, db) = database().await;
        let service = TraceSearchIndexService::new(db.clone());
        service.store(IndexState::Unavailable(CatalogFailure::BuildTimedOut));
        create_canonical_index(&db).await.unwrap();
        assert_eq!(service.ensure_once().await.unwrap(), IndexState::Ready);
        assert_eq!(service.state(), IndexState::Ready);
    }
}
