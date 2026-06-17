use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use tracing::log;
use url::Url;

use crate::{
    common::errors::MegaError,
    config::DbConfig,
    jupiter::{migration::apply_migrations, utils::id_generator},
};

/// Create a PostgreSQL database connection.
///
/// After a successful connection, applies any pending database migrations.
pub async fn database_connection(db_config: &DbConfig) -> DatabaseConnection {
    id_generator::set_up_options().unwrap();

    let conn = postgres_connection(db_config)
        .await
        .expect("Cannot connect to PostgreSQL database");
    apply_migrations(&conn, false)
        .await
        .expect("Failed to apply migrations");

    conn
}

fn validate_postgres_config(db_config: &DbConfig) -> Result<(), MegaError> {
    if db_config.db_type != "postgres" {
        return Err(MegaError::Other(format!(
            "unsupported database type '{}'; monoengine only supports PostgreSQL",
            db_config.db_type
        )));
    }

    let url = Url::parse(&db_config.db_url)
        .map_err(|e| MegaError::Other(format!("invalid PostgreSQL database URL: {e}")))?;
    match url.scheme() {
        "postgres" | "postgresql" => Ok(()),
        scheme => Err(MegaError::Other(format!(
            "unsupported database URL scheme '{scheme}'; monoengine only supports PostgreSQL"
        ))),
    }
}

async fn postgres_connection(db_config: &DbConfig) -> Result<DatabaseConnection, MegaError> {
    validate_postgres_config(db_config)?;

    let db_url = db_config.db_url.to_owned();
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
        validate_postgres_config(&config).expect("postgres config should be accepted");

        config.db_url = "postgresql://mono:mono@localhost:5432/mono_test".to_owned();
        validate_postgres_config(&config).expect("postgresql config should be accepted");

        config.db_type = "mysql".to_owned();
        config.db_url = "mysql://mono:mono@localhost:3306/mono_test".to_owned();
        assert!(validate_postgres_config(&config).is_err());
    }
}
