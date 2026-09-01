use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    sea_query::OnConflict,
};
use sea_orm_migration::SchemaManagerConnection;
use tracing::{error, warn};

use crate::common::errors::{MegaError, classify_db_error};

const INSERT_RETRY_MAX_ATTEMPTS: u32 = 5;
const INSERT_RETRY_BASE_BACKOFF_MS: u64 = 10;
const INSERT_RETRY_SLEEP_BUDGET: Duration = Duration::from_millis(150);

async fn execute_insert_many<E, A>(
    connection: &DatabaseConnection,
    txn: Option<&DatabaseTransaction>,
    models: Vec<A>,
    on_conflict: &OnConflict,
) -> Result<(), DbErr>
where
    E: EntityTrait,
    A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
{
    let result = if let Some(txn) = txn {
        E::insert_many(models)
            .on_conflict(on_conflict.clone())
            .exec(txn)
            .await
    } else {
        E::insert_many(models)
            .on_conflict(on_conflict.clone())
            .exec(connection)
            .await
    };

    match result {
        Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
        Err(error) => Err(error),
    }
}

fn retry_backoff(attempt: u32, total_backoff: Duration) -> Option<Duration> {
    if attempt >= INSERT_RETRY_MAX_ATTEMPTS {
        return None;
    }

    let requested_backoff =
        Duration::from_millis(INSERT_RETRY_BASE_BACKOFF_MS * 2u64.pow(attempt - 1));
    let remaining_budget = INSERT_RETRY_SLEEP_BUDGET.saturating_sub(total_backoff);
    (!remaining_budget.is_zero()).then_some(requested_backoff.min(remaining_budget))
}

async fn insert_many_with_retry<E, A>(
    connection: &DatabaseConnection,
    txn: Option<&DatabaseTransaction>,
    models: Vec<A>,
    on_conflict: &OnConflict,
) -> Result<(), MegaError>
where
    E: EntityTrait,
    A: ActiveModelTrait<Entity = E> + From<<E as EntityTrait>::Model> + Send + Clone,
{
    // A caller-owned transaction may contain reads and other writes before
    // this insert. PostgreSQL requires a serialization/deadlock retry to
    // replay that complete transaction, which only the caller can do. Keep
    // the operation in that transaction and propagate the classified error.
    if txn.is_some() {
        return match execute_insert_many::<E, A>(connection, txn, models, on_conflict).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let error_type = classify_db_error(&error)
                    .map(|kind| kind.as_str())
                    .unwrap_or("non_retryable_db_error");
                error!(
                    attempt = 1u32,
                    backoff_ms = 0u64,
                    error_type,
                    "batch insert failed in caller-owned transaction; retry the complete transaction"
                );
                Err(error.into())
            }
        };
    }

    let mut attempt = 0;
    let mut total_backoff = Duration::ZERO;
    let mut last_backoff_ms = 0;

    loop {
        attempt += 1;
        match execute_insert_many::<E, A>(connection, txn, models.clone(), on_conflict).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retry_kind = classify_db_error(&error);
                let Some(retry_kind) = retry_kind else {
                    error!(
                        attempt,
                        backoff_ms = last_backoff_ms,
                        error_type = "non_retryable_db_error",
                        "batch insert failed without retry"
                    );
                    return Err(error.into());
                };

                let Some(backoff) = retry_backoff(attempt, total_backoff) else {
                    error!(
                        attempt,
                        backoff_ms = last_backoff_ms,
                        error_type = retry_kind.as_str(),
                        "batch insert retry limit reached"
                    );
                    return Err(error.into());
                };

                let backoff_ms = backoff.as_millis() as u64;
                warn!(
                    attempt,
                    next_attempt = attempt + 1,
                    backoff_ms,
                    error_type = retry_kind.as_str(),
                    "retrying batch insert after transient database conflict"
                );
                tokio::time::sleep(backoff).await;
                total_backoff += backoff;
                last_backoff_ms = backoff_ms;
            }
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
    /// The method splits the models into smaller chunks, each containing models configured by chunk_size, and inserts them into the database using the `E::insert_many` function.
    /// The results of each insertion are collected into a vector of futures.
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
            insert_many_with_retry::<E, A>(
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
        let futures = save_models.chunks(Self::BATCH_CHUNK_SIZE).map(|chunk| {
            insert_many_with_retry::<E, A>(self.get_connection(), None, chunk.to_vec(), &onconflict)
        });
        futures::future::try_join_all(futures).await?;
        Ok(())
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
    use std::{borrow::Cow, error::Error, sync::Arc};

    use sea_orm::{DbBackend, EntityTrait, MockDatabase, Set, TransactionTrait};

    use super::*;
    use crate::callisto::git_blob;

    fn test_blob(id: i64) -> git_blob::ActiveModel {
        git_blob::ActiveModel {
            id: Set(id),
            repo_id: Set(1),
            blob_id: Set(format!("blob-{id}")),
            name: Set(Some(format!("file-{id}"))),
            size: Set(1),
            created_at: Set(chrono::Utc::now().naive_utc()),
            pack_id: Set("pack".to_owned()),
            file_path: Set(String::new()),
            pack_offset: Set(0),
            is_delta_in_pack: Set(false),
        }
    }

    fn test_blob_model(id: i64) -> git_blob::Model {
        git_blob::Model {
            id,
            repo_id: 1,
            blob_id: format!("blob-{id}"),
            name: Some(format!("file-{id}")),
            size: 1,
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: "pack".to_owned(),
            file_path: String::new(),
            pack_offset: 0,
            is_delta_in_pack: false,
        }
    }

    fn structured_database_error(code: &'static str) -> DbErr {
        #[derive(Debug)]
        struct FakeDatabaseError {
            code: &'static str,
        }

        impl std::fmt::Display for FakeDatabaseError {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "database error")
            }
        }

        impl Error for FakeDatabaseError {}

        impl sea_orm::sqlx::error::DatabaseError for FakeDatabaseError {
            fn message(&self) -> &str {
                "database error"
            }

            fn code(&self) -> Option<Cow<'_, str>> {
                Some(Cow::Borrowed(self.code))
            }

            fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
                self
            }

            fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
                self
            }

            fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
                self
            }

            fn kind(&self) -> sea_orm::sqlx::error::ErrorKind {
                sea_orm::sqlx::error::ErrorKind::Other
            }
        }

        DbErr::Exec(sea_orm::RuntimeErr::SqlxError(Arc::new(
            sea_orm::SqlxError::Database(Box::new(FakeDatabaseError { code })),
        )))
    }

    fn statement_sql(connection: sea_orm::DatabaseConnection) -> Vec<String> {
        connection
            .into_transaction_log()
            .into_iter()
            .flat_map(|transaction| {
                transaction
                    .statements()
                    .iter()
                    .map(|statement| statement.sql.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[tokio::test]
    async fn retries_transient_batch_insert_and_preserves_on_conflict() {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors([structured_database_error("40P01")])
            .append_query_results([[test_blob_model(1)]])
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));

        storage
            .batch_save_model::<git_blob::Entity, _>(vec![test_blob(1)])
            .await
            .expect("transient insert should be retried");

        let statements = statement_sql(connection);
        assert_eq!(statements.len(), 2);
        assert!(
            statements
                .iter()
                .all(|sql| { sql.contains("ON CONFLICT") && sql.contains("DO NOTHING") })
        );
    }

    #[tokio::test]
    async fn retries_at_most_five_times() {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors(
                (0..INSERT_RETRY_MAX_ATTEMPTS).map(|_| structured_database_error("40001")),
            )
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));

        let result = storage
            .batch_save_model::<git_blob::Entity, _>(vec![test_blob(2)])
            .await;

        assert!(matches!(
            result,
            Err(MegaError::Db(error))
                if classify_db_error(&error) == Some(crate::common::errors::DbRetryKind::SerializationFailure)
        ));
        assert_eq!(
            statement_sql(connection).len(),
            INSERT_RETRY_MAX_ATTEMPTS as usize
        );
    }

    #[tokio::test]
    async fn propagates_non_retryable_batch_error_without_retry() {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors([DbErr::Custom(
                "duplicate key value violates unique constraint".to_owned(),
            )])
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));

        let result = storage
            .batch_save_model::<git_blob::Entity, _>(vec![test_blob(3)])
            .await;

        assert!(
            matches!(result, Err(MegaError::Db(DbErr::Custom(message))) if message.contains("duplicate key"))
        );
        assert_eq!(statement_sql(connection).len(), 1);
    }

    #[tokio::test]
    async fn propagates_transient_error_in_caller_owned_transaction() {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors([structured_database_error("40P01")])
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));
        let txn = connection.begin().await.expect("begin transaction");

        let result = storage
            .batch_save_model_with_txn::<git_blob::Entity, _>(vec![test_blob(4)], Some(&txn))
            .await;
        assert!(matches!(
            result,
            Err(MegaError::Db(error))
                if classify_db_error(&error) == Some(crate::common::errors::DbRetryKind::Deadlock)
        ));
        txn.rollback().await.expect("rollback transaction");

        let statements = statement_sql(connection);
        assert!(!statements.iter().any(|sql| sql.starts_with("SAVEPOINT")));
        assert_eq!(
            statements
                .iter()
                .filter(|sql| sql.contains("INSERT INTO"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn real_postgres_unique_violation_is_not_retried() {
        let temp_dir = tempfile::tempdir().expect("test temp directory");
        let connection = crate::jupiter::tests::test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&connection, true)
            .await
            .expect("apply test migrations");

        let id = 9_000_000_000 + i64::from(std::process::id());
        let model = test_blob(id);
        git_blob::Entity::insert(model.clone())
            .exec(&connection)
            .await
            .expect("seed real test blob");

        let storage = BaseStorage::new(Arc::new(connection.clone()));
        let result = storage
            .batch_save_model_with_conflict::<git_blob::Entity, _>(
                vec![model],
                OnConflict::new().to_owned(),
            )
            .await;
        let error = match result {
            Err(MegaError::Db(error)) => error,
            other => panic!("expected database error, got {other:?}"),
        };

        assert_eq!(classify_db_error(&error), None);
        assert!(matches!(error, DbErr::Exec(_) | DbErr::Query(_)));
    }

    #[test]
    fn retry_backoff_is_exponential_and_bounded() {
        let mut total = Duration::ZERO;
        let mut schedule = Vec::new();
        for attempt in 1..INSERT_RETRY_MAX_ATTEMPTS {
            let backoff = retry_backoff(attempt, total).expect("retry should be available");
            total += backoff;
            schedule.push(backoff);
        }

        assert_eq!(
            schedule,
            vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Duration::from_millis(80),
            ]
        );
        assert_eq!(total, INSERT_RETRY_SLEEP_BUDGET);
        assert_eq!(retry_backoff(INSERT_RETRY_MAX_ATTEMPTS, total), None);
        assert_eq!(
            retry_backoff(INSERT_RETRY_MAX_ATTEMPTS - 1, Duration::from_millis(140)),
            Some(Duration::from_millis(10))
        );
    }
}
