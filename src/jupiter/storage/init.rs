use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use tracing::log;
use url::Url;

use crate::{
    common::errors::MegaError,
    config::{DbConfig, redaction::redact_db_url, validate::validate_database_config},
    jupiter::{migration::apply_migrations, utils::id_generator},
};

/// Create a PostgreSQL database connection.
///
/// After a successful connection, applies any pending database migrations.
/// This compatibility entry point initializes the standalone generator after
/// the database is ready. Production [`crate::context::AppContext`] uses
/// [`database_connection_without_id_generator`] so it can bind the worker
/// selection before exposing writable storage.
pub async fn database_connection(db_config: &DbConfig) -> Result<DatabaseConnection, MegaError> {
    let connection = database_connection_without_id_generator(db_config).await?;
    id_generator::ensure_initialized()?;
    Ok(connection)
}

/// Create the writable database connection without initializing the Snowflake
/// generator.
///
/// Production bootstrap uses this deferred variant while it resolves Redis
/// credentials and claims the worker lease.
pub(crate) async fn database_connection_without_id_generator(
    db_config: &DbConfig,
) -> Result<DatabaseConnection, MegaError> {
    let conn = postgres_connection(db_config).await?;
    apply_migrations(&conn, false).await?;

    Ok(conn)
}

/// Connect for reading only: no migrations, and the server refuses writes.
///
/// [`database_connection`] applies pending migrations on the way in, which is a
/// schema change performed by whatever process happened to connect first. An
/// audit command must not be that process — a report that begins by migrating
/// the database it is about to describe has already changed the answer.
///
/// Read-only is enforced by the *server*, not by this code being careful:
/// `default_transaction_read_only=on` is set for the session, so any write
/// reaching Postgres through this connection is rejected there. That covers the
/// paths nobody remembered to audit, which are the ones worth covering.
pub async fn read_only_database_connection(
    db_config: &DbConfig,
) -> Result<DatabaseConnection, MegaError> {
    validate_database_config(db_config)?;

    let db_url = read_only_db_url(&db_config.db_url)?;
    log::info!(
        "Connecting to database read-only: {}",
        redact_db_url(&db_url)
    );

    let mut read_only_config = db_config.clone();
    read_only_config.db_url = db_url;
    let opt = setup_option(&read_only_config);
    Database::connect(opt).await.map_err(|e| e.into())
}

/// The connection URL with the read-only session option added.
///
/// Any `options` the URL already carries are kept (all of them, if it repeats
/// the parameter): the test harness isolates
/// each database behind a `search_path` set exactly this way, and dropping it
/// would silently point the connection at a different schema. The read-only
/// option is appended rather than merged in place because libpq applies `-c`
/// settings left to right, so appending last means a URL that tried to turn
/// read-only *off* cannot win.
pub fn read_only_db_url(db_url: &str) -> Result<String, MegaError> {
    const READ_ONLY_OPTION: &str = "-cdefault_transaction_read_only=on";

    let mut url = Url::parse(db_url)
        .map_err(|e| MegaError::Other(format!("database url is not a valid URL: {e}")))?;

    // Every `options` occurrence is kept, not just the last one: libpq reads the
    // whole string as one space-separated list, so overwriting on a repeat would
    // drop settings the caller asked for.
    let mut existing: Vec<String> = Vec::new();
    let mut others: Vec<(String, String)> = Vec::new();
    for (key, value) in url.query_pairs() {
        if key == "options" {
            existing.push(value.into_owned());
        } else {
            others.push((key.into_owned(), value.into_owned()));
        }
    }
    existing.push(READ_ONLY_OPTION.to_string());
    let options = existing.join(" ");

    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in &others {
            query.append_pair(key, value);
        }
        query.append_pair("options", &options);
    }

    Ok(url.to_string())
}

async fn postgres_connection(db_config: &DbConfig) -> Result<DatabaseConnection, MegaError> {
    validate_database_config(db_config)?;

    let db_url = redact_db_url(&db_config.db_url);
    log::info!("Connecting to database: {db_url}");

    let opt = setup_option(db_config);
    Database::connect(opt).await.map_err(|e| e.into())
}

fn setup_option(db_config: &DbConfig) -> ConnectOptions {
    let mut opt = ConnectOptions::new(db_config.db_url.clone());
    opt.max_connections(db_config.max_connection)
        .min_connections(db_config.min_connection)
        .acquire_timeout(Duration::from_secs(db_config.acquire_timeout))
        .connect_timeout(Duration::from_secs(db_config.connect_timeout))
        .idle_timeout(Duration::from_secs(8))
        .max_lifetime(Duration::from_secs(8))
        .sqlx_logging(db_config.sqlx_logging)
        .sqlx_logging_level(log::LevelFilter::Debug);
    opt
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    pub fn accepts_only_postgres_database_config() {
        let mut config = DbConfig {
            db_type: "postgres".to_owned(),
            db_url: "postgres://mono:mono@localhost:5432/mono_test".to_owned(),
            ..Default::default()
        };
        validate_database_config(&config).expect("postgres config should be accepted");

        config.db_url = "postgresql://mono:mono@localhost:5432/mono_test".to_owned();
        validate_database_config(&config).expect("postgresql config should be accepted");

        config.db_type = "mysql".to_owned();
        config.db_url = "mysql://mono:mono@localhost:3306/mono_test".to_owned();
        assert!(validate_database_config(&config).is_err());
    }

    #[tokio::test]
    async fn database_connection_returns_error_for_invalid_config() {
        let config = DbConfig {
            db_type: "mysql".to_owned(),
            db_url: "mysql://localhost:3306/mono_test".to_owned(),
            ..Default::default()
        };

        let err = database_connection(&config)
            .await
            .expect_err("invalid database config should fail before connecting");

        assert!(err.to_string().contains("database.db_type"));
    }
}
