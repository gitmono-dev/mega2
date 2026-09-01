use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    TransactionTrait, sea_query::OnConflict,
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
    if let Some(txn) = txn {
        let savepoint = txn.begin().await?;
        let result = E::insert_many(models)
            .on_conflict(on_conflict.clone())
            .exec(&savepoint)
            .await;

        match result {
            Ok(_) | Err(DbErr::RecordNotInserted) => savepoint.commit().await,
            Err(error) => {
                if let Err(rollback_error) = savepoint.rollback().await {
                    error!(
                        error_type = "savepoint_rollback_failure",
                        "batch insert savepoint rollback failed"
                    );
                    return Err(rollback_error);
                }
                Err(error)
            }
        }
    } else {
        match E::insert_many(models)
            .on_conflict(on_conflict.clone())
            .exec(connection)
            .await
        {
            Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
            Err(error) => Err(error),
        }
    }
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

                if attempt >= INSERT_RETRY_MAX_ATTEMPTS {
                    error!(
                        attempt,
                        backoff_ms = last_backoff_ms,
                        error_type = retry_kind.as_str(),
                        "batch insert retry limit reached"
                    );
                    return Err(error.into());
                }

                let requested_backoff =
                    Duration::from_millis(INSERT_RETRY_BASE_BACKOFF_MS * 2u64.pow(attempt - 1));
                let remaining_budget = INSERT_RETRY_SLEEP_BUDGET.saturating_sub(total_backoff);
                if remaining_budget.is_zero() {
                    error!(
                        attempt,
                        backoff_ms = last_backoff_ms,
                        error_type = retry_kind.as_str(),
                        "batch insert retry sleep budget reached"
                    );
                    return Err(error.into());
                }

                let backoff = requested_backoff.min(remaining_budget);
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
        let mut i = 0;
        let len = save_models.len();

        while i < len {
            let end = (i + Self::BATCH_CHUNK_SIZE).min(len);
            insert_many_with_retry::<E, A>(
                self.get_connection(),
                None,
                save_models[i..end].to_vec(),
                &onconflict,
            )
            .await?;
            i = end;
        }
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
    use std::sync::Arc;

    use sea_orm::{DbBackend, MockDatabase, Set, TransactionTrait};

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
            .append_query_errors([DbErr::Custom("ERROR: 40P01 deadlock detected".to_owned())])
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
        let retry_error = DbErr::Custom("ERROR: 40001 serialization failure".to_owned());
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors((0..INSERT_RETRY_MAX_ATTEMPTS).map(|_| retry_error.clone()))
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));

        let result = storage
            .batch_save_model::<git_blob::Entity, _>(vec![test_blob(2)])
            .await;

        assert!(
            matches!(result, Err(MegaError::Db(DbErr::Custom(message))) if message.contains("40001"))
        );
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
    async fn retries_inside_an_outer_transaction_with_savepoint_rollback() {
        let connection = MockDatabase::new(DbBackend::Postgres)
            .append_query_errors([DbErr::Custom("ERROR: 40P01 deadlock detected".to_owned())])
            .append_query_results([[test_blob_model(4)]])
            .into_connection();
        let storage = BaseStorage::new(Arc::new(connection.clone()));
        let txn = connection.begin().await.expect("begin transaction");

        storage
            .batch_save_model_with_txn::<git_blob::Entity, _>(vec![test_blob(4)], Some(&txn))
            .await
            .expect("transient insert should be retried inside savepoint");
        txn.commit().await.expect("commit transaction");

        let statements = statement_sql(connection);
        assert!(statements.iter().any(|sql| sql.starts_with("SAVEPOINT")));
        assert!(
            statements
                .iter()
                .any(|sql| sql.starts_with("ROLLBACK TO SAVEPOINT"))
        );
        assert!(
            statements
                .iter()
                .any(|sql| sql.starts_with("RELEASE SAVEPOINT"))
        );
        assert_eq!(
            statements
                .iter()
                .filter(|sql| sql.contains("INSERT INTO"))
                .count(),
            2
        );
    }
}
