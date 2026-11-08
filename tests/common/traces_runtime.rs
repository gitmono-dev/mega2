//! Real-binary runtime fixture for the traces HTTP surface (MTR-89).
//!
//! This helper is included only by `tests/integration_traces.rs` through an
//! explicit `#[path]` module, so it does not widen `tests/common/mod.rs`.
//! It owns everything it creates: the temporary directory, the generated
//! config, the synthetic credential file, the child process, and the port.
//! The black-box case observes only public HTTP responses and the runtime
//! OpenAPI document; it never imports the private API or server modules.

use std::{
    fs,
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const STARTUP_BUDGET: Duration = Duration::from_secs(120);
const REQUEST_BUDGET: Duration = Duration::from_secs(10);
const TEARDOWN_BUDGET: Duration = Duration::from_secs(30);

/// One running `mega2` HTTP service with traces enabled.
pub struct TracesRuntime {
    child: Option<Child>,
    root: tempfile::TempDir,
    port: u16,
    token: String,
    database_url: String,
    database: Option<crate::traces_db::TracesDb>,
    db: Option<sea_orm::DatabaseConnection>,
}

impl TracesRuntime {
    /// Start the real binary with a generated config that only enables
    /// traces and names one synthetic static token.
    pub async fn start() -> Self {
        let root = tempfile::tempdir().expect("traces runtime directory");
        let (database, db) = crate::traces_db::TracesDb::migrated().await;
        let database_url = database_url(&db).await;
        let token = format!("mtr89-synthetic-{}", uuid::Uuid::new_v4().simple());
        let token_file = root.path().join("token");
        fs::write(&token_file, &token).expect("write synthetic token file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600))
                .expect("restrict synthetic token file");
        }

        let config_path = root.path().join("config.toml");
        fs::write(&config_path, runtime_config(&token)).expect("write runtime config");

        let port = reserve_port();
        let binary = env!("CARGO_BIN_EXE_mega2");
        let mut init = Command::new(binary);
        init.arg("--config")
            .arg(&config_path)
            .args(["service", "init", "--yes"])
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for key in [
            "MEGA_DATABASE__DB_TYPE",
            "MEGA_REDIS__URL",
            "MEGA_BASE_DIR",
            "MEGA_CACHE_DIR",
            "MEGA_OBJECT_STORAGE__STORAGE_TYPE",
            "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
            "MEGA_GIT__PUSH_AUTH",
            "MEGA_MONOREPO__PUSH_POLICY",
            "MEGA_TRACES__ENABLED",
            "MEGA_TRACES__SERVER_ID",
            "MEGA_CEDAR__ENFORCEMENT",
        ] {
            if let Some(value) = std::env::var_os(key) {
                init.env(key, value);
            }
        }
        init.env("MEGA_BASE_DIR", root.path().join("base"));
        init.env("MEGA_CACHE_DIR", root.path().join("cache"));
        init.env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local");
        init.env(
            "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
            root.path().join("objects"),
        );
        init.env("MEGA_GIT__PUSH_AUTH", "token");
        init.env("MEGA_MONOREPO__PUSH_POLICY", "trunk");
        init.env("MEGA_TRACES__ENABLED", "true");
        init.env(
            "MEGA_TRACES__SERVER_ID",
            "11111111-1111-4111-8111-111111111111",
        );
        init.env("MEGA_CEDAR__ENFORCEMENT", "off");
        init.env("MEGA_DATABASE__DB_URL", &database_url);
        let status = init.status().expect("run service init");
        assert!(
            status.success(),
            "real mega2 binary failed service init with {status}"
        );

        let mut command = Command::new(binary);
        command
            .arg("--config")
            .arg(&config_path)
            .args(["service", "http", "--host", "127.0.0.1", "-p"])
            .arg(port.to_string())
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("MEGA_GIT__PUSH_AUTH", "token")
            .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
            .env("MEGA_TRACES__ENABLED", "true")
            .env(
                "MEGA_TRACES__SERVER_ID",
                "11111111-1111-4111-8111-111111111111",
            )
            .env("MEGA_CEDAR__ENFORCEMENT", "off")
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env(
                "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
                root.path().join("objects"),
            )
            .env("MEGA_BASE_DIR", root.path().join("base"))
            .env("MEGA_CACHE_DIR", root.path().join("cache"))
            .env(
                "MEGA_DATABASE__DB_TYPE",
                std::env::var("MEGA_DATABASE__DB_TYPE").unwrap_or_else(|_| "postgres".to_owned()),
            )
            .env("MEGA_DATABASE__DB_URL", &database_url)
            .env(
                "MEGA_REDIS__URL",
                std::env::var("MEGA_REDIS__URL")
                    .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_owned()),
            );
        let log_path = root.path().join("service.log");
        let log_file = fs::File::create(&log_path).expect("create service log");
        command.stderr(Stdio::from(log_file.try_clone().expect("clone log file")));
        command.stdout(Stdio::from(log_file));
        let child = command.spawn().expect("spawn real mega2 binary");
        let runtime = Self {
            child: Some(child),
            root,
            port,
            token,
            database_url,
            database: None,
            db: None,
        };
        let mut runtime = runtime;
        runtime.await_ready().await;
        runtime.database = Some(database);
        runtime.db = Some(db);
        runtime
    }

    /// Base URL of the running service.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Database connection of this runtime, for public SQL fixtures only.
    pub fn db(&self) -> &sea_orm::DatabaseConnection {
        self.db.as_ref().expect("runtime database connection")
    }

    /// GET a path with the synthetic credential and return the status and body.
    pub async fn get(&self, path: &str) -> (u16, String) {
        let url = format!("{}{path}", self.base_url());
        let request = reqwest::Client::builder()
            .timeout(REQUEST_BUDGET)
            .build()
            .expect("runtime client");
        let response = request
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("runtime request");
        let status = response.status().as_u16();
        let body = response.text().await.expect("runtime response body");
        (status, body)
    }

    async fn await_ready(&mut self) {
        let deadline = Instant::now() + STARTUP_BUDGET;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("readiness client");
        loop {
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let log =
                    fs::read_to_string(self.root.path().join("service.log")).unwrap_or_default();
                panic!("real mega2 binary exited before readiness with {status}: {log}");
            }
            if let Ok(response) = client
                .get(format!("{}/api/openapi.json", self.base_url()))
                .send()
                .await
                && response.status().is_success()
            {
                return;
            }
            if Instant::now() >= deadline {
                panic!("real mega2 binary did not become ready within 120s");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for TracesRuntime {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let deadline = Instant::now() + TEARDOWN_BUDGET;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => break,
                }
            }
            let _ = child.wait();
        }
        // The database guard removes its database only after the pool that
        // still holds it is closed.
        let _ = self.database_url.clone();
        self.database.take();
    }
}

fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .expect("reserved port")
        .port()
}

async fn database_url(db: &sea_orm::DatabaseConnection) -> String {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let name: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database() AS value",
        ))
        .await
        .expect("current database name")
        .expect("current database row")
        .try_get("", "value")
        .expect("current database value");
    let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
        "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
    });
    let mut url = url::Url::parse(&admin).expect("test database URL");
    url.set_path(&name);
    url.to_string()
}

fn runtime_config(token: &str) -> String {
    let mut config = include_str!("../../config/config.toml").to_owned();
    config.push_str("\n[[git.push_tokens]]\n");
    config.push_str(&format!("name = \"mtr89\"\ntoken = \"{token}\"\n"));
    config
}
