//! FC-15: two-process FastCDC interop gate (mega2 HTTP + Libra client).
//!
//! Compiled only with `--features fastcdc`. Default `cargo test --all` does not
//! require a sibling Libra checkout. Live cases are `#[ignore]` and fail closed
//! when `LIBRA_DIR` is missing, dirty, or at the wrong revision.

#![cfg(feature = "fastcdc")]

mod common;
#[path = "common/fastcdc_gate.rs"]
mod fastcdc_gate;
#[allow(
    dead_code,
    reason = "path-included git-cli helpers are only used for token seed / pid evidence"
)]
#[path = "common/git_cli.rs"]
mod git_cli;

use std::{
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str = "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";
const REPO_PREFIX: &str = "/acme/app.git";
// Boot + Libra child + shutdown + port close stay under the 10-minute card budget.
const GATE_TIMEOUT: Duration = Duration::from_secs(420);
const SERVICE_BOOT_TIMEOUT: Duration = Duration::from_secs(90);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const PORT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);
static REDIS_COUNTER: AtomicUsize = AtomicUsize::new(1);

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "mega2_fastcdc_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);
        with_runtime(async {
            let db = Database::connect(admin_url.as_str()).await.unwrap_or_else(|_| {
                panic!(
                    "integration PostgreSQL is not available; run `docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait` first"
                )
            });
            execute_postgres(&db, format!("DROP DATABASE IF EXISTS {db_name}")).await;
            execute_postgres(&db, format!("CREATE DATABASE {db_name}")).await;
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
        });
    }
}

struct FastcdcEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    redis_url: String,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
}

impl FastcdcEnv {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let redis_url = isolated_redis_url();
        let full_config_path = temp_dir.path().join("config.toml");
        let base_dir = temp_dir.path().join("base");
        let cache_dir = temp_dir.path().join("cache");
        let object_root = temp_dir.path().join("objects");
        common::write_full_config(&full_config_path);
        Self {
            temp_dir,
            database,
            redis_url,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
        }
    }

    fn command_for_bin(&self, bin: &Path) -> Command {
        let mut command = Command::new(bin);
        command
            .current_dir(self.temp_dir.path())
            .env_clear()
            .env("MEGA_BASE_DIR", &self.base_dir)
            .env("MEGA_CACHE_DIR", &self.cache_dir)
            .env("RUST_BACKTRACE", "0")
            .arg("--config")
            .arg(&self.full_config_path)
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
            .env("MEGA_REDIS__URL", &self.redis_url)
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(ld_library_path) = std::env::var_os("LD_LIBRARY_PATH") {
            command.env("LD_LIBRARY_PATH", ld_library_path);
        }
        command
    }
}

struct ServiceProcess {
    child: Child,
    reaped: bool,
}

impl ServiceProcess {
    fn spawn(mut command: Command) -> Self {
        let child = command.spawn().expect("spawn mega2 service");
        let service = Self {
            child,
            reaped: false,
        };
        git_cli::record_service_pid(service.child.id());
        service
    }

    fn wait_until_listening(
        &mut self,
        port: u16,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                panic!(
                    "service exited before binding port {port} (status {status})\nstdout:\n{}\nstderr:\n{}",
                    read_log(stdout_path),
                    read_log(stderr_path),
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "service did not bind port {port} within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                    read_log(stdout_path),
                    read_log(stderr_path),
                );
            }
            sleep(Duration::from_millis(200));
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
            sleep(Duration::from_millis(200));
        }
    }

    fn shutdown_via_sigint(&mut self, timeout: Duration) -> ExitStatus {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: SIGINT is sent only to the child process owned by this guard.
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
        self.wait_for_exit(timeout).unwrap_or_else(|| {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
            panic!("service did not exit within {timeout:?} after shutdown signal");
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
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

struct ChildGroup {
    child: Child,
}

impl ChildGroup {
    fn spawn(mut command: Command) -> Self {
        // SAFETY: the child is a cargo test we own; a new process group lets
        // timeout/panic paths reap rustc grandchildren.
        command.process_group(0);
        let child = command.spawn().expect("spawn Libra FastCDC child");
        Self { child }
    }

    fn wait_with_timeout(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll Libra child") {
                return status;
            }
            if Instant::now() >= deadline {
                self.kill_group();
                panic!("Libra FastCDC child exceeded {timeout:?}; process group reaped");
            }
            sleep(Duration::from_millis(200));
        }
    }

    fn kill_group(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: pid is the process-group leader we created with process_group(0).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill_group();
        }
    }
}

#[test]
#[ignore = "explicit FC-15 gate; requires LIBRA_DIR at LIBRA_INTEROP_REV"]
fn mega2_libra_fastcdc_interop() {
    let libra_dir = fastcdc_gate::require_libra_checkout();
    let token = git_cli::resolve_seed_token();
    let env = FastcdcEnv::new();
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http(&env, Path::new(env!("CARGO_BIN_EXE_mega2")));
    let service_pid = service.pid();
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);
    git_cli::seed_access_token(
        &env.database.db_url,
        "it-fastcdc-other",
        "other-user-not-in-ready-file",
    );

    let lfs_url = format!("http://127.0.0.1:{port}{REPO_PREFIX}/info/lfs/");
    let ready = fastcdc_gate::ReadyFile::write(env.temp_dir.path(), &lfs_url, &token);

    let authed = http_get(
        &format!("{lfs_url}libra/media/v1/capabilities"),
        Some(&token),
        &token,
        &lfs_url,
    );
    assert_eq!(
        authed.status().as_u16(),
        200,
        "authenticated capabilities must succeed: {}",
        fastcdc_gate::redact_secrets(&authed.text().unwrap_or_default(), &token, &lfs_url)
    );
    let unauth = http_get(
        &format!("{lfs_url}libra/media/v1/capabilities"),
        None,
        &token,
        &lfs_url,
    );
    assert_eq!(
        unauth.status().as_u16(),
        401,
        "unauthenticated Media capabilities must be rejected"
    );

    let child_stdout = env.temp_dir.path().join("libra-child.out");
    let child_stderr = env.temp_dir.path().join("libra-child.err");
    let mut child_cmd = Command::new("cargo");
    child_cmd
        .current_dir(&libra_dir)
        .args([
            "test",
            "--features",
            "fastcdc",
            "--test",
            "media_fastcdc_test",
            "--",
            "--ignored",
            "--exact",
            "mega2_fastcdc_http_interop",
            "--nocapture",
        ])
        .env("MEGA2_FASTCDC_READY_FILE", &ready.path)
        .env("CARGO_TERM_COLOR", "never")
        .stdout(Stdio::from(create_log_file(&child_stdout)))
        .stderr(Stdio::from(create_log_file(&child_stderr)));

    let mut child = ChildGroup::spawn(child_cmd);
    let status = child.wait_with_timeout(GATE_TIMEOUT);
    let stdout = fastcdc_gate::redact_secrets(&read_log(&child_stdout), &token, &lfs_url);
    let stderr = fastcdc_gate::redact_secrets(&read_log(&child_stderr), &token, &lfs_url);
    assert!(
        status.success(),
        "Libra mega2_fastcdc_http_interop failed ({status})\nstdout:\n{stdout}\nstderr:\n{stderr}\nservice stderr:\n{}",
        fastcdc_gate::redact_secrets(&read_log(&stderr_path), &token, &lfs_url)
    );

    drop(ready);
    assert!(
        !env.temp_dir
            .path()
            .join("mega2-fastcdc-ready.json")
            .exists(),
        "ready-file must be deleted after the Libra child exits"
    );

    let shutdown = service.shutdown_via_sigint(SHUTDOWN_TIMEOUT);
    assert!(
        shutdown.success(),
        "feature-on service shutdown failed: {shutdown}"
    );
    assert_process_reaped(service_pid);
    wait_until_port_closed(port, PORT_CLOSE_TIMEOUT);
}

#[test]
#[ignore = "explicit FC-15 feature-off companion; requires LIBRA_DIR and MEGA2_FASTCDC_OFF_BIN"]
fn mega2_fastcdc_feature_off_falls_back() {
    let libra_dir = fastcdc_gate::require_libra_checkout();
    let off_bin = std::env::var("MEGA2_FASTCDC_OFF_BIN")
        .unwrap_or_else(|_| panic!("MEGA2_FASTCDC_OFF_BIN is required (feature-off mega2 binary)"));
    let off_bin = PathBuf::from(off_bin);
    assert!(
        off_bin.is_file(),
        "MEGA2_FASTCDC_OFF_BIN {} is not a file",
        off_bin.display()
    );

    let token = git_cli::resolve_seed_token();
    let env = FastcdcEnv::new();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env, &off_bin);
    let service_pid = service.pid();
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let lfs_url = format!("http://127.0.0.1:{port}{REPO_PREFIX}/info/lfs/");
    let cap = format!("{lfs_url}libra/media/v1/capabilities");
    let authed = http_get(&cap, Some(&token), &token, &lfs_url);
    assert_eq!(
        authed.status().as_u16(),
        404,
        "feature-off Media capabilities must be 404, got {} body {}",
        authed.status(),
        fastcdc_gate::redact_secrets(&authed.text().unwrap_or_default(), &token, &lfs_url)
    );
    let unauth = http_get(&cap, None, &token, &lfs_url);
    assert_eq!(
        unauth.status().as_u16(),
        404,
        "feature-off Media capabilities must be 404 without a token too"
    );

    let libra_bin = ensure_libra_fastcdc_bin(&libra_dir);
    let probe_dir = tempfile::tempdir().expect("libra probe repo");
    run_libra(&libra_bin, probe_dir.path(), &["init"], &token, &lfs_url);
    run_libra(
        &libra_bin,
        probe_dir.path(),
        &[
            "config",
            "remote.origin.url",
            &format!("http://127.0.0.1:{port}{REPO_PREFIX}"),
        ],
        &token,
        &lfs_url,
    );
    let probe = libra_output(
        &libra_bin,
        probe_dir.path(),
        &["--json", "media", "probe", "--remote", "origin"],
        &token,
        &lfs_url,
    );
    let stdout = fastcdc_gate::redact_secrets(&probe, &token, &lfs_url);
    let js: serde_json::Value =
        serde_json::from_str(&probe).unwrap_or_else(|_| panic!("media probe json: {stdout}"));
    assert_eq!(
        js["data"]["chunked"].as_bool(),
        Some(false),
        "feature-off server must not negotiate FastCDC: {stdout}"
    );
    let decision = js["data"]["decision"].as_str().unwrap_or_default();
    assert!(
        decision.contains("standard-lfs") || decision.contains("fallback"),
        "Libra must select standard LFS fallback, got {stdout}"
    );

    let shutdown = service.shutdown_via_sigint(SHUTDOWN_TIMEOUT);
    assert!(
        shutdown.success(),
        "feature-off service shutdown failed: {shutdown}\n{}",
        fastcdc_gate::redact_secrets(&read_log(&stderr_path), &token, &lfs_url)
    );
    assert_process_reaped(service_pid);
    wait_until_port_closed(port, PORT_CLOSE_TIMEOUT);
}

fn boot_service_http(env: &FastcdcEnv, bin: &Path) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");
    let mut command = env.command_for_bin(bin);
    command.env(
        "MEGA_HTTP__PUBLIC_BASE_URL",
        format!("http://127.0.0.1:{port}"),
    );
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    command
        .stdout(Stdio::from(create_log_file(&stdout_path)))
        .stderr(Stdio::from(create_log_file(&stderr_path)));
    let mut service = ServiceProcess::spawn(command);
    service.wait_until_listening(port, SERVICE_BOOT_TIMEOUT, &stdout_path, &stderr_path);
    (service, port, stdout_path, stderr_path)
}

fn http_get(
    url: &str,
    token: Option<&str>,
    redact_token: &str,
    lfs_url: &str,
) -> reqwest::blocking::Response {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    let mut req = client.get(url).header("Accept", "application/json");
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    req.send().unwrap_or_else(|err| {
        panic!(
            "GET media endpoint failed: {}",
            fastcdc_gate::redact_secrets(&err.to_string(), redact_token, lfs_url)
        )
    })
}

fn ensure_libra_fastcdc_bin(libra_dir: &Path) -> PathBuf {
    let bin = libra_dir.join("target/debug/libra");
    let status = Command::new("cargo")
        .current_dir(libra_dir)
        .args(["build", "--features", "fastcdc", "--bin", "libra"])
        .status()
        .expect("build feature-on libra");
    assert!(
        status.success(),
        "cargo build --features fastcdc --bin libra failed"
    );
    assert!(bin.is_file(), "missing libra feature-on binary");
    bin
}

fn run_libra(bin: &Path, cwd: &Path, args: &[&str], token: &str, lfs_url: &str) {
    let output = libra_command(bin, cwd, args)
        .output()
        .unwrap_or_else(|err| panic!("run libra child: {err}"));
    assert!(
        output.status.success(),
        "libra child failed: {}",
        fastcdc_gate::redact_secrets(&String::from_utf8_lossy(&output.stderr), token, lfs_url)
    );
}

fn libra_output(bin: &Path, cwd: &Path, args: &[&str], token: &str, lfs_url: &str) -> String {
    let output = libra_command(bin, cwd, args)
        .output()
        .unwrap_or_else(|err| panic!("run libra child: {err}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "libra child failed ({})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        fastcdc_gate::redact_secrets(&stdout, token, lfs_url),
        fastcdc_gate::redact_secrets(&stderr, token, lfs_url)
    );
    stdout
}

fn libra_command(bin: &Path, cwd: &Path, args: &[&str]) -> Command {
    let home = cwd.join(".libra-test-home");
    fs::create_dir_all(home.join(".config")).expect("libra test home");
    let mut command = Command::new(bin);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env(
            "LIBRA_CONFIG_GLOBAL_DB",
            home.join(".libra").join("config.db"),
        )
        .env("LANG", "C")
        .env("LC_ALL", "C");
    command
}

fn assert_process_reaped(pid: u32) {
    // SAFETY: signal 0 only probes whether the former child PID still exists.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    assert_eq!(result, -1, "owned service process {pid} must be gone");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "owned service process {pid} must be fully reaped"
    );
}

fn wait_until_port_closed(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        sleep(Duration::from_millis(100));
    }
    panic!("owned service port {port} still accepts connections after shutdown");
}

fn reserve_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn create_log_file(path: &Path) -> fs::File {
    fs::File::create(path).unwrap_or_else(|err| panic!("create {}: {err}", path.display()))
}

fn read_log(path: &Path) -> String {
    let mut buf = String::new();
    let _ = fs::File::open(path).and_then(|mut f| f.read_to_string(&mut buf));
    buf
}

fn integration_postgres_url() -> String {
    std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string())
}

fn integration_redis_url() -> String {
    std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string())
}

fn isolated_redis_url() -> String {
    let base = integration_redis_url();
    let db = REDIS_COUNTER.fetch_add(1, Ordering::Relaxed) % 14 + 1;
    let mut url = url::Url::parse(&base).unwrap_or_else(|_| {
        panic!("MEGA_REDIS__URL must be a valid Redis URL for integration tests")
    });
    url.set_path(&db.to_string());
    url.to_string()
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
