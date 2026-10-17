//! Isolated database for traces schema integration tests.
//!
//! Included only by `tests/integration_traces.rs`. This helper calls the real
//! migration runner through the public seam and does not copy DDL.
//!
//! The SeaORM pool stays on the test runtime. Dropping it from another
//! runtime while this runtime waits deadlocks, so [`TracesDb`] only removes
//! the database after that pool is already closed.

use std::sync::atomic::{AtomicUsize, Ordering};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
};
use url::Url;

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct TracesDb {
    admin_url: String,
    name: String,
}

impl TracesDb {
    pub async fn migrated() -> (Self, DatabaseConnection) {
        let admin_url = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
            "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
        });
        let name = format!(
            "mega2_traces_it_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = Database::connect(admin_url.as_str())
            .await
            .expect("connect PostgreSQL admin database");
        admin
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("CREATE DATABASE {name}"),
            ))
            .await
            .expect("create traces integration database");
        let _ = admin.close().await;

        let mut parsed = Url::parse(&admin_url).expect("PostgreSQL URL");
        parsed.set_path(&name);
        let mut opt = ConnectOptions::new(parsed.to_string());
        opt.max_connections(4)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(180))
            .connect_timeout(std::time::Duration::from_secs(60))
            .sqlx_logging(false);
        let db = Database::connect(opt)
            .await
            .expect("connect traces integration database");
        mega2_core::traces_migration_ops::apply_migrations(&db, true)
            .await
            .expect("apply migrations");
        (Self { admin_url, name }, db)
    }
}

impl Drop for TracesDb {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let worker = std::thread::Builder::new()
            .name(format!("drop-{name}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        eprintln!("traces integration database {name} left behind: {err}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let Ok(admin) = Database::connect(&admin_url).await else {
                        eprintln!(
                            "traces integration database {name} left behind: admin connect failed"
                        );
                        return;
                    };
                    let _ = admin
                        .execute_raw(Statement::from_string(
                            DatabaseBackend::Postgres,
                            format!(
                                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                                 WHERE datname = '{name}' AND pid <> pg_backend_pid()"
                            ),
                        ))
                        .await;
                    let dropped = admin
                        .execute_raw(Statement::from_string(
                            DatabaseBackend::Postgres,
                            format!("DROP DATABASE IF EXISTS {name}"),
                        ))
                        .await;
                    if let Err(err) = dropped {
                        eprintln!("traces integration database {name} left behind: {err}");
                    }
                    let _ = admin.close().await;
                });
            });
        if let Ok(worker) = worker {
            let _ = worker.join();
        }
    }
}
