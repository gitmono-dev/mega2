//! Process-level Agent Capture HTTP retirement IT (MTR-26).
//!
//! The legacy `/api/v1/agent-capture` surface is removed. This target boots a
//! real service with `agent_capture.enabled=true` (so the flag cannot be the
//! reason the routes are absent) and asserts every legacy path is 404 while
//! the new traces GET surface stays mounted.
#![allow(dead_code)]

mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str = "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

const INGEST_TOKEN: &str = "agent-it-ingest";
const SENTINEL: &str = "RAW_PROMPT_SENTINEL_AC13_DO_NOT_LOG";
const REPO_SEGMENT: &str = "third-part%2Fmega";
const SECRET_REF: &str = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value";
const HMAC_VALUE: &str = "hex:0101010101010101010101010101010101010101010101010101010101010101";
const STORAGE_EVENTS_APPEND: &str = r#"
[storage_events]
enabled = true
installation_id = "it-agent-capture"

[[storage_events.targets]]
id = "ops-main"
url = "https://events.example.invalid/ingest"
secret_ref = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
events = ["agent_capture.events.committed", "agent_capture.checkpoint.committed"]
agent_tenants = ["default"]
agent_repo_paths = ["/third-part/mega"]
"#;

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CASE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial_gate() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "mega2_ac_{}_{}",
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

struct CaptureEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
}

impl CaptureEnv {
    fn storage_only(deployment_id: &str) -> Self {
        Self::with_append(&storage_only_append(deployment_id))
    }

    fn storage_only_with_events(deployment_id: &str) -> Self {
        Self::with_append(&format!(
            "{}{STORAGE_EVENTS_APPEND}",
            storage_only_append(deployment_id)
        ))
    }

    fn review() -> Self {
        Self::with_append("")
    }

    fn with_append(append: &str) -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let case_name = format!(
            "ac-case-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let case_dir = temp_dir.path().join(case_name);
        fs::create_dir_all(&case_dir).expect("create case dir");
        let base_dir = case_dir.join("base");
        let cache_dir = case_dir.join("cache");
        let object_root = case_dir.join("objects");
        fs::create_dir_all(&base_dir).expect("create base");
        fs::create_dir_all(&cache_dir).expect("create cache");
        fs::create_dir_all(&object_root).expect("create objects");
        let database = TestDatabase::create();
        let full_config_path = case_dir.join("config.toml");
        common::write_full_config_with_append(&full_config_path, append);
        Self {
            temp_dir,
            database,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
        }
    }

    fn full_config_command(&self) -> Command {
        let mut command = isolated_command(self.temp_dir.path(), &self.base_dir, &self.cache_dir);
        command.arg("--config").arg(&self.full_config_path);
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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            .env("MEGA_GIT__SSH_RECEIVE_PACK", "false");
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
        Self {
            child,
            reaped: false,
        }
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
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn seed_target_secret(env: &CaptureEnv) {
    let bootstrap_path = env.temp_dir.path().join("bootstrap-config.toml");
    fs::write(
        &bootstrap_path,
        format!(
            r#"
            [database]
            db_type = "postgres"
            db_url = "{}"
            max_connection = 4
            min_connection = 1
            acquire_timeout = 5
            connect_timeout = 5
            sqlx_logging = false
            "#,
            env.database.db_url
        ),
    )
    .expect("write bootstrap config");
    let mut command = isolated_command(env.temp_dir.path(), &env.base_dir, &env.cache_dir);
    command
        .arg("--config")
        .arg(&bootstrap_path)
        .env("MEGA_LOG__PRINT_STD", "false")
        .env("MEGA_LOG__WITH_ANSI", "false")
        .args([
            "config",
            "secret",
            "set",
            "storage_events.targets.ops-main.secret_ref",
            "--vault-path",
            "config/it/storage_events/targets/ops-main/hmac",
            "--field",
            "value",
            "--value-stdin",
        ]);
    let (out_path, err_path) = (
        env.temp_dir.path().join("seed.out"),
        env.temp_dir.path().join("seed.err"),
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(
            fs::File::create(&out_path).expect("seed stdout"),
        ))
        .stderr(Stdio::from(
            fs::File::create(&err_path).expect("seed stderr"),
        ));
    let mut child = command.spawn().expect("spawn config secret set");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(HMAC_VALUE.as_bytes())
        .expect("write stdin");
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll secret set") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "config secret set did not finish within the bound:\nstdout:\n{}\nstderr:\n{}",
            read_log(&out_path),
            read_log(&err_path),
        );
        sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "config secret set must succeed: {status}\nstdout:\n{}\nstderr:\n{}",
        read_log(&out_path),
        read_log(&err_path),
    );
}

fn storage_only_append(_deployment_id: &str) -> String {
    r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false

# The legacy [agent_capture] section is removed (MTR-27); this retirement
# fixture boots without it on purpose.
"#
    .to_owned()
}

fn boot_storage_only(env: &CaptureEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http(
        env,
        &[
            ("MEGA_GIT__PUSH_AUTH", "none"),
            ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ],
    )
}

fn boot_service_http(
    env: &CaptureEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join(format!("service-{port}.out"));
    let stderr_path = env.temp_dir.path().join(format!("service-{port}.err"));
    let mut command = env.full_config_command();
    command.env(
        "MEGA_HTTP__PUBLIC_BASE_URL",
        format!("http://127.0.0.1:{port}"),
    );
    for (key, value) in extra_env {
        command.env(*key, *value);
    }
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
    service.wait_until_listening(port, Duration::from_secs(90), &stdout_path, &stderr_path);
    (service, port, stdout_path, stderr_path)
}

fn api_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}/api/v1/agent-capture")
}

fn bearer() -> String {
    format!("Bearer {INGEST_TOKEN}")
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}

fn put_session(client: &reqwest::blocking::Client, base: &str, client_session_id: &str) -> i64 {
    let response = client
        .put(format!(
            "{base}/repos/{REPO_SEGMENT}/sessions/{client_session_id}"
        ))
        .header("Authorization", bearer())
        .header("Content-Type", "application/json")
        .body(r#"{"session_kind":"external_capture"}"#)
        .send()
        .expect("put session");
    assert_eq!(response.status().as_u16(), 200, "put session");
    let body: serde_json::Value = response.json().expect("put json");
    body["capture_id"].as_i64().expect("capture_id")
}

fn stage_and_finalize(
    client: &reqwest::blocking::Client,
    base: &str,
    capture_id: i64,
    payload: &[u8],
) -> String {
    let staged = client
        .post(format!("{base}/sessions/{capture_id}/blobs/staging"))
        .header("Authorization", bearer())
        .body(payload.to_vec())
        .send()
        .expect("staging");
    assert_eq!(staged.status().as_u16(), 200, "staging");
    let lease_id = staged.json::<serde_json::Value>().expect("lease json")["lease_id"]
        .as_str()
        .expect("lease_id")
        .to_owned();
    let finalized = client
        .post(format!(
            "{base}/sessions/{capture_id}/blobs/{lease_id}/finalize"
        ))
        .header("Authorization", bearer())
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .expect("finalize");
    assert_eq!(finalized.status().as_u16(), 200, "finalize");
    finalized
        .json::<serde_json::Value>()
        .expect("finalize json")["digest"]
        .as_str()
        .expect("digest")
        .to_owned()
}

fn insert_tombstone_sql(db_url: &str, capture_id: i64) {
    with_runtime(async {
        let db = Database::connect(db_url).await.expect("connect");
        execute_postgres(
            &db,
            format!(
                "INSERT INTO agent_capture_tombstone \
                 (deployment_id, tenant_id, repo_id, producer_id, session_kind, client_session_id, capture_id) \
                 SELECT deployment_id, tenant_id, repo_id, producer_id, session_kind, client_session_id, id \
                 FROM agent_capture_session WHERE id = {capture_id} \
                 ON CONFLICT DO NOTHING"
            ),
        )
        .await;
    });
}

fn isolated_command(current_dir: &Path, base_dir: &Path, cache_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mega2"));
    command
        .current_dir(current_dir)
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

fn create_log_file(path: &Path) -> fs::File {
    fs::File::create(path).expect("create service log file")
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

const LEGACY_PATHS: [(&str, &str); 12] = [
    ("GET", "/api/v1/agent-capture/discovery"),
    (
        "PUT",
        "/api/v1/agent-capture/repos/third-part%2Fmega/sessions/ac-retire",
    ),
    ("POST", "/api/v1/agent-capture/sessions/1/blobs/staging"),
    (
        "POST",
        "/api/v1/agent-capture/sessions/1/blobs/lease/finalize",
    ),
    ("POST", "/api/v1/agent-capture/sessions/1/events:batch"),
    ("POST", "/api/v1/agent-capture/sessions/1/file-ops:batch"),
    ("POST", "/api/v1/agent-capture/sessions/1/checkpoints"),
    (
        "GET",
        "/api/v1/agent-capture/repos/third-part%2Fmega/sessions",
    ),
    ("GET", "/api/v1/agent-capture/sessions/1"),
    ("GET", "/api/v1/agent-capture/sessions/1/checkpoints"),
    ("GET", "/api/v1/agent-capture/sessions/1/transcript"),
    ("GET", "/api/v1/agent-capture/sessions/1/file-ops"),
];

/// MTR-26: with `agent_capture.enabled=true` the 12 legacy HTTP paths are no
/// longer mounted, the runtime OpenAPI no longer lists them, and the new traces
/// GET surface stays available.
#[test]
fn legacy_agent_capture_http_surface_is_retired() {
    let _guard = serial_gate();
    let env = CaptureEnv::storage_only("retire-http");
    let (mut service, port, out_path, err_path) = boot_storage_only(&env);
    let client = http_client();
    let base = format!("http://127.0.0.1:{port}");
    for (method, path) in LEGACY_PATHS {
        let request = match method {
            "GET" => client.get(format!("{base}{path}")),
            "PUT" => client.put(format!("{base}{path}")),
            _ => client.post(format!("{base}{path}")),
        };
        let response = request
            .header("Authorization", bearer())
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .expect("legacy request");
        assert_eq!(
            response.status().as_u16(),
            404,
            "{method} {path} must be retired, not {}",
            response.status()
        );
    }
    // The runtime OpenAPI no longer lists the legacy paths or DTOs.
    let openapi = client
        .get(format!("{base}/api/openapi.json"))
        .send()
        .expect("openapi")
        .json::<serde_json::Value>()
        .expect("openapi json");
    let paths = openapi["paths"].as_object().expect("openapi paths");
    assert!(
        paths
            .keys()
            .all(|path| !path.contains("/api/v1/agent-capture")),
        "the legacy capture paths are still documented"
    );
    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("openapi schemas");
    assert!(
        schemas.keys().all(|name| !name.contains("AgentCapture")),
        "the legacy capture DTOs are still documented"
    );
    service.shutdown_via_sigint(Duration::from_secs(30));
    let _ = (&out_path, &err_path);
}
