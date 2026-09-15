// Process-level WH-13 gates (plan-20260912): the storage-events shutdown
// wiring in the real `monoengine` binary.
//
// Drives `CARGO_BIN_EXE_monoengine` with per-case PostgreSQL database, port
// and directory isolation (same pattern as `integration_git_cli`):
//   a. default-disabled `service http` boots, SIGINT exits 0 and the logs
//      carry the `storage_events_shutdown_complete` receipt;
//   b. an occupied listen port fails startup but still exits through the
//      cleanup tail (receipt present, non-zero exit);
//   c. `service multi http` stops on SIGINT through the same tail;
//   d. non-service regression: `config validate` keeps its old exit behavior
//      and never prints the receipt.
// The receipt line must carry at most category/count fields — never URLs,
// secrets or bodies (ADR-WH-02).

mod common;

use std::{
    fs,
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";
const SHUTDOWN_RECEIPT: &str = "storage_events_shutdown_complete";

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "monoengine_wh13_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);

        with_runtime(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let db = Database::connect(admin_url.as_str()).await.unwrap_or_else(|_| {
                    panic!(
                        "integration PostgreSQL is not available; run `docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait` first"
                    )
                });
                execute_postgres(&db, format!("DROP DATABASE IF EXISTS {db_name}")).await;
                execute_postgres(&db, format!("CREATE DATABASE {db_name}")).await;
            })
            .await
            .expect("integration PostgreSQL setup timed out after 30s");
        });

        Self {
            admin_url,
            db_name,
            db_url,
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let db_name = self.db_name.clone();
        with_runtime(async move {
            let _ = tokio::time::timeout(Duration::from_secs(30), async {
                let Ok(db) = Database::connect(admin_url.as_str()).await else {
                    return;
                };
                let terminate_sql = format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}'"
                );
                let _ = db
                    .execute_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        terminate_sql,
                    ))
                    .await;
                let _ = db
                    .execute_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        format!("DROP DATABASE IF EXISTS {db_name}"),
                    ))
                    .await;
            })
            .await;
        });
    }
}

/// Per-case isolation: temp config + dirs, dedicated database, own port.
struct RuntimeCase {
    temp_dir: TempDir,
    database: TestDatabase,
    config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
}

impl RuntimeCase {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let config_path = temp_dir.path().join("config.toml");
        let base_dir = temp_dir.path().join("base");
        let cache_dir = temp_dir.path().join("cache");
        let object_root = temp_dir.path().join("objects");
        common::write_full_config(&config_path);

        Self {
            temp_dir,
            database,
            config_path,
            base_dir,
            cache_dir,
            object_root,
        }
    }

    /// The default config keeps `[storage_events]` commented out, so the
    /// emitter starts default-disabled — exactly the WH-13 baseline.
    fn command(&self) -> Command {
        let mut command = isolated_command(&self.base_dir, &self.cache_dir);
        command.arg("--config").arg(&self.config_path);
        command
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_PATH", "")
            .env("MEGA_DATABASE__DB_URL", &self.database.db_url)
            .env("MEGA_DATABASE__MAX_CONNECTION", "4")
            .env("MEGA_DATABASE__MIN_CONNECTION", "1")
            .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
            .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
            .env("MEGA_DATABASE__SQLX_LOGGING", "false")
            .env("MEGA_LOG__PRINT_STD", "true")
            .env("MEGA_LOG__WITH_ANSI", "false")
            .env("MEGA_REDIS__URL", integration_redis_url())
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root);
        command
    }

    fn log_paths(&self, name: &str) -> (PathBuf, PathBuf) {
        (
            self.temp_dir.path().join(format!("{name}.out")),
            self.temp_dir.path().join(format!("{name}.err")),
        )
    }
}

struct ServiceProcess {
    child: Child,
    reaped: bool,
}

impl ServiceProcess {
    fn spawn(mut command: Command, stdout_path: &Path, stderr_path: &Path) -> Self {
        command
            .stdout(Stdio::from(
                fs::File::create(stdout_path).expect("stdout log"),
            ))
            .stderr(Stdio::from(
                fs::File::create(stderr_path).expect("stderr log"),
            ));
        let child = command.spawn().expect("spawn monoengine service");
        Self {
            child,
            reaped: false,
        }
    }

    fn wait_until_openapi_ready(
        &mut self,
        port: u16,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
    ) {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build HTTP client");
        let url = format!("http://127.0.0.1:{port}/api/openapi.json");
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(response) = client.get(&url).send()
                && response.status().is_success()
            {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                panic!(
                    "service exited before OpenAPI was ready on port {port} (status {status})\nstdout:\n{}\nstderr:\n{}",
                    read_log(stdout_path),
                    read_log(stderr_path),
                );
            }
            assert!(
                Instant::now() < deadline,
                "service OpenAPI not ready on port {port} within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                read_log(stdout_path),
                read_log(stderr_path),
            );
            sleep(Duration::from_millis(200));
        }
    }

    fn send_sigint(&self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: SIGINT to a child we own; WH-13 routes it to the async layer.
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// WH-13 AC1/AC6: after SIGINT the receipt must be observed in a bounded
/// window *before* the exit code is asserted — the exit code alone is not
/// evidence that the cleanup tail ran.
fn assert_sigint_cleanup_and_exit(
    service: &mut ServiceProcess,
    stdout_path: &Path,
    stderr_path: &Path,
) {
    service.send_sigint();
    let receipt = wait_for_shutdown_receipt(stdout_path, stderr_path, Duration::from_secs(60));
    assert_shutdown_receipt_sanitized(&receipt);
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .unwrap_or_else(|| {
            panic!(
                "service did not exit after the shutdown receipt\nstdout:\n{}\nstderr:\n{}",
                read_log(stdout_path),
                read_log(stderr_path),
            )
        });
    assert!(
        status.success(),
        "service must exit 0 after graceful shutdown: {status}\nstdout:\n{}\nstderr:\n{}",
        read_log(stdout_path),
        read_log(stderr_path),
    );
}

fn wait_for_shutdown_receipt(stdout_path: &Path, stderr_path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let combined = format!("{}\n{}", read_log(stdout_path), read_log(stderr_path));
        if let Some(line) = combined
            .lines()
            .find(|line| line.contains(SHUTDOWN_RECEIPT))
        {
            return line.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "{SHUTDOWN_RECEIPT} not observed within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
            read_log(stdout_path),
            read_log(stderr_path),
        );
        sleep(Duration::from_millis(100));
    }
}

/// ADR-WH-02: the receipt carries at most category/count fields — never URLs,
/// secrets or bodies.
fn assert_shutdown_receipt_sanitized(line: &str) {
    for forbidden in ["http://", "https://", "vault://", "hex:", "secret"] {
        assert!(
            !line.contains(forbidden),
            "shutdown receipt must not contain `{forbidden}`: {line}"
        );
    }
}

#[test]
fn integration_storage_events_runtime_http_sigint_clean_shutdown() {
    let case = RuntimeCase::new();
    let port = reserve_free_port();
    let (stdout_path, stderr_path) = case.log_paths("http-sIGINT");
    let mut command = case.command();
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);
    service.wait_until_openapi_ready(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    assert_sigint_cleanup_and_exit(&mut service, &stdout_path, &stderr_path);
}

#[test]
fn integration_storage_events_runtime_occupied_port_exits_through_cleanup() {
    let case = RuntimeCase::new();
    let port = reserve_free_port();
    // Hold the port so the service bind fails after AppContext creation.
    let _blocker = TcpListener::bind(("127.0.0.1", port)).expect("bind blocker port");
    let (stdout_path, stderr_path) = case.log_paths("http-occupied");
    let mut command = case.command();
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);

    // AC2/AC6 ordering, same as the SIGINT cases: the receipt must be
    // observed in the bounded log window first; the exit code alone is not
    // evidence that the cleanup tail ran.
    let receipt = wait_for_shutdown_receipt(&stdout_path, &stderr_path, Duration::from_secs(60));
    assert_shutdown_receipt_sanitized(&receipt);
    // The failure must be the port bind, not some unrelated startup error.
    let combined = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        combined.contains("failed to bind HTTP listener"),
        "occupied-port case must fail at the HTTP bind:\n{combined}"
    );
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .expect("occupied-port startup failure must exit on its own");
    assert!(
        !status.success(),
        "occupied-port startup must exit non-zero: {status}\nstdout:\n{}\nstderr:\n{}",
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
}

#[test]
fn integration_storage_events_runtime_multi_http_sigint_clean_shutdown() {
    let case = RuntimeCase::new();
    let port = reserve_free_port();
    let (stdout_path, stderr_path) = case.log_paths("multi-http");
    let mut command = case.command();
    command.args([
        "service",
        "multi",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);
    service.wait_until_openapi_ready(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    assert_sigint_cleanup_and_exit(&mut service, &stdout_path, &stderr_path);
}

#[test]
fn integration_storage_events_runtime_config_validate_has_no_cleanup_log() {
    let case = RuntimeCase::new();
    let (stdout_path, stderr_path) = case.log_paths("config-validate");
    let mut command = case.command();
    command.args(["config", "validate"]);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .expect("config validate must exit within the bound");
    assert!(
        status.success(),
        "config validate must keep working: {status}\nstdout:\n{}\nstderr:\n{}",
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    // AC7: non-service commands never pass through the service cleanup tail.
    let combined = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        !combined.contains(SHUTDOWN_RECEIPT),
        "non-service command must not log {SHUTDOWN_RECEIPT}:\n{combined}"
    );
}

fn isolated_command(base_dir: &Path, cache_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_monoengine"));
    command
        .env_clear()
        .env("MEGA_BASE_DIR", base_dir)
        .env("MEGA_CACHE_DIR", cache_dir)
        .env("RUST_BACKTRACE", "0");
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(ld_library_path) = std::env::var_os("LD_LIBRARY_PATH") {
        command.env("LD_LIBRARY_PATH", ld_library_path);
    }
    command
}

fn integration_postgres_url() -> String {
    std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string())
}

fn integration_redis_url() -> String {
    std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string())
}

fn database_url_for_name(admin_url: &str, db_name: &str) -> String {
    let mut url = url::Url::parse(admin_url).unwrap_or_else(|_| {
        panic!("MEGA_DATABASE__DB_URL must be a valid PostgreSQL URL for integration tests")
    });
    url.set_path(db_name);
    url.to_string()
}

async fn execute_postgres(db: &sea_orm::DatabaseConnection, sql: String) {
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .unwrap_or_else(|_| panic!("failed to prepare integration PostgreSQL database"));
}

fn with_runtime<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

fn reserve_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn read_log(path: &Path) -> String {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf
}
