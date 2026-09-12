use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    sea_query::OnConflict,
};
use sea_orm_migration::SchemaManagerConnection;

use crate::common::errors::{MegaError, db_err_is_retryable_serialization};

const INSERT_RETRY_ATTEMPTS: u32 = 5;
const INSERT_RETRY_BASE_MS: u64 = 10;

/// Decision after a failed (or no-op) batch insert attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertRetry {
    Ok,
    Sleep(u64),
    Fail,
}

pub(crate) fn next_insert_retry(attempt: u32, err: &DbErr) -> InsertRetry {
    if matches!(err, DbErr::RecordNotInserted) {
        return InsertRetry::Ok;
    }
    if db_err_is_retryable_serialization(err) && attempt < INSERT_RETRY_ATTEMPTS {
        InsertRetry::Sleep(INSERT_RETRY_BASE_MS << (attempt - 1))
    } else {
        InsertRetry::Fail
    }
}

#[cfg(test)]
async fn retry_transient_insert<F, Fut>(mut insert: F) -> Result<(), MegaError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), DbErr>>,
{
    let mut attempt = 0u32;
    let mut last_backoff_ms = 0u64;
    loop {
        attempt += 1;
        match insert().await {
            Ok(_) => return Ok(()),
            Err(e) => match next_insert_retry(attempt, &e) {
                InsertRetry::Ok => return Ok(()),
                InsertRetry::Sleep(backoff_ms) => {
                    last_backoff_ms = backoff_ms;
                    tracing::warn!(
                        attempt,
                        backoff_ms,
                        error_kind = "deadlock_or_serialization",
                        "retrying batch insert after deadlock or serialization failure"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
                InsertRetry::Fail => {
                    if db_err_is_retryable_serialization(&e) {
                        tracing::error!(
                            attempt,
                            backoff_ms = last_backoff_ms,
                            error_kind = "deadlock_or_serialization",
                            "batch insert failed after retries"
                        );
                    }
                    return Err(e.into());
                }
            },
        }
    }
}

async fn insert_many_with_deadlock_retry<E, A>(
    conn: &DatabaseConnection,
    txn: Option<&DatabaseTransaction>,
    models: Vec<A>,
    onconflict: &OnConflict,
) -> Result<(), MegaError>
where
    E: EntityTrait,
    A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
{
    let mut attempt = 0u32;
    let mut last_backoff_ms = 0u64;
    loop {
        attempt += 1;
        let insert = E::insert_many(models.clone()).on_conflict(onconflict.clone());
        let result = if let Some(txn) = txn {
            insert.exec(txn).await
        } else {
            insert.exec(conn).await
        };
        match result {
            Ok(_) => return Ok(()),
            Err(e) => match next_insert_retry(attempt, &e) {
                InsertRetry::Ok => return Ok(()),
                InsertRetry::Sleep(backoff_ms) => {
                    last_backoff_ms = backoff_ms;
                    tracing::warn!(
                        attempt,
                        backoff_ms,
                        error_kind = "deadlock_or_serialization",
                        "retrying batch insert after deadlock or serialization failure"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
                InsertRetry::Fail => {
                    if db_err_is_retryable_serialization(&e) {
                        tracing::error!(
                            attempt,
                            backoff_ms = last_backoff_ms,
                            error_kind = "deadlock_or_serialization",
                            "batch insert failed after retries"
                        );
                    }
                    return Err(e.into());
                }
            },
        }
    }
}

#[async_trait]
pub trait StorageConnector {
    const BATCH_CHUNK_SIZE: usize = 1000;

    fn get_connection(&self) -> &DatabaseConnection;

    fn mock() -> Self;

    fn new(connection: Arc<DatabaseConnection>) -> Self;

    fn build_connection_with_txn<'a>(
        &'a self,
        txn: Option<&'a DatabaseTransaction>,
    ) -> SchemaManagerConnection<'a> {
        if let Some(txn) = txn {
            SchemaManagerConnection::Transaction(txn)
        } else {
            SchemaManagerConnection::Connection(self.get_connection())
        }
    }

    /// Performs batch saving of models in the database.
    ///
    /// The method takes a vector of models to be saved and performs batch inserts using the given entity type `E`.
    /// The models should implement the `ActiveModelTrait` trait, which provides the necessary functionality for saving and inserting the models.
    ///
    /// The method splits the models into smaller chunks, each containing models configured by chunk_size, and inserts them sequentially with bounded deadlock/serialization retry.
    ///
    /// Note: Currently, SQLx does not support packets larger than 16MB.
    /// # Arguments
    ///
    /// * `save_models` - A vector of models to be saved.
    ///
    /// # Generic Constraints
    ///
    /// * `E` - The entity type that implements the `EntityTrait` trait.
    /// * `A` - The model type that implements the `ActiveModelTrait` trait and is convertible from the corresponding model type of `E`.
    ///
    /// # Errors
    ///
    /// Returns a `MegaError` if an error occurs during the batch save operation.
    async fn batch_save_model<E, A>(&self, save_models: Vec<A>) -> Result<(), MegaError>
    where
        E: EntityTrait,
        A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
    {
        let onconflict = OnConflict::new().do_nothing().to_owned();
        Self::batch_save_model_with_conflict(self, save_models, onconflict).await
    }

    async fn batch_save_model_with_txn<E, A>(
        &self,
        save_models: Vec<A>,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError>
    where
        E: EntityTrait,
        A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
    {
        let onconflict = OnConflict::new().do_nothing().to_owned();
        Self::batch_save_model_with_conflict_and_txn(self, save_models, onconflict, txn).await
    }

    async fn batch_save_model_with_conflict_and_txn<E, A>(
        &self,
        save_models: Vec<A>,
        onconflict: OnConflict,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError>
    where
        E: EntityTrait,
        A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
    {
        let mut i = 0;
        let len = save_models.len();

        while i < len {
            let end = (i + Self::BATCH_CHUNK_SIZE).min(len);
            insert_many_with_deadlock_retry::<E, A>(
                self.get_connection(),
                txn,
                save_models[i..end].to_vec(),
                &onconflict,
            )
            .await?;
            i = end;
        }
        Ok(())
    }

    /// Performs batch saving of models in the database with conflict resolution.
    ///
    /// This function allows saving models in batches while specifying conflict resolution behavior using the `OnConflict` parameter.
    /// It is intended for advanced use cases where fine-grained control over conflict handling is required.
    ///
    /// # Arguments
    ///
    /// * `save_models` - A vector of models to be saved.
    /// * `onconflict` - Specifies the conflict resolution strategy to be used during insertion.
    ///
    /// # Generic Constraints
    ///
    /// * `E` - The entity type that implements the `EntityTrait` trait.
    /// * `A` - The model type that implements the `ActiveModelTrait` trait and is convertible from the corresponding model type of `E`.
    ///
    /// # Errors
    ///
    /// Returns a `MegaError` if an error occurs during the batch save operation.
    /// Note: The function ignores `DbErr::RecordNotInserted` errors, which may lead to silent failures.
    /// Use this function with caution and ensure that the `OnConflict` parameter is configured correctly to avoid unintended consequences.
    async fn batch_save_model_with_conflict<E, A>(
        &self,
        save_models: Vec<A>,
        onconflict: OnConflict,
    ) -> Result<(), MegaError>
    where
        E: EntityTrait,
        A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
    {
        Self::batch_save_model_with_conflict_and_txn(self, save_models, onconflict, None).await
    }
}

#[derive(Debug, Clone)]
pub struct BaseStorage {
    pub connection: Arc<DatabaseConnection>,
}

impl StorageConnector for BaseStorage {
    fn get_connection(&self) -> &DatabaseConnection {
        &self.connection
    }

    fn mock() -> Self {
        Self {
            connection: Arc::new(DatabaseConnection::default()),
        }
    }

    fn new(connection: Arc<DatabaseConnection>) -> Self {
        Self { connection }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_10_20_40_80_then_fail() {
        let deadlock = DbErr::Custom("deadlock detected".into());
        assert_eq!(next_insert_retry(1, &deadlock), InsertRetry::Sleep(10));
        assert_eq!(next_insert_retry(2, &deadlock), InsertRetry::Sleep(20));
        assert_eq!(next_insert_retry(3, &deadlock), InsertRetry::Sleep(40));
        assert_eq!(next_insert_retry(4, &deadlock), InsertRetry::Sleep(80));
        assert_eq!(next_insert_retry(5, &deadlock), InsertRetry::Fail);
        let total_wait: u64 = [10, 20, 40, 80].into_iter().sum();
        assert!(total_wait <= 150);
    }

    #[test]
    fn record_not_inserted_is_success() {
        assert_eq!(
            next_insert_retry(1, &DbErr::RecordNotInserted),
            InsertRetry::Ok
        );
    }

    #[test]
    fn unique_violation_is_not_retried() {
        let err = DbErr::Custom(
            "duplicate key value violates unique constraint \"git_repo_pkey\"".into(),
        );
        assert_eq!(next_insert_retry(1, &err), InsertRetry::Fail);
    }

    #[test]
    fn serialization_failure_is_retried() {
        let err = DbErr::Custom("ERROR: 40001 could not serialize access".into());
        assert_eq!(next_insert_retry(1, &err), InsertRetry::Sleep(10));
    }

    #[tokio::test]
    async fn first_success_does_not_sleep() {
        let started = std::time::Instant::now();
        retry_transient_insert(|| async { Ok(()) })
            .await
            .expect("first success");
        assert!(started.elapsed() < Duration::from_millis(5));
    }

    #[tokio::test]
    async fn unique_violation_fails_immediately() {
        let started = std::time::Instant::now();
        let err = retry_transient_insert(|| async {
            Err(DbErr::Custom(
                "duplicate key value violates unique constraint \"git_repo_pkey\"".into(),
            ))
        })
        .await
        .expect_err("permanent errors must propagate");
        assert!(!err.is_retryable_db_serialization());
        assert!(started.elapsed() < Duration::from_millis(5));
    }

    #[tokio::test]
    async fn deadlock_retries_then_succeeds() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        retry_transient_insert(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(DbErr::Custom("deadlock detected".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .expect("retryable deadlock should succeed after backoff");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn deadlock_gives_up_after_five_attempts() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let err = retry_transient_insert(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err(DbErr::Custom("ERROR: 40P01 deadlock detected".into())) }
        })
        .await
        .expect_err("exhausted retries must fail");
        assert!(err.is_retryable_db_serialization());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    }
}
