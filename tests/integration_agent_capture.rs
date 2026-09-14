//! Process-level Agent Capture HTTP black-box IT (plan-20260911 / AC-13).
//!
//! Boots a real `service http` with an isolated local object backend.

mod common;

use std::{
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, sleep},
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

const INGEST_TOKEN: &str = "agent-it-ingest";
const SENTINEL: &str = "RAW_PROMPT_SENTINEL_AC13_DO_NOT_LOG";
const REPO_SEGMENT: &str = "third-part%2Fmega";

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "monoengine_ac_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);
        with_runtime(async {
            let db = Database::connect(admin_url.as_str()).await.unwrap_or_else(|_| {
                panic!(
                    "integration PostgreSQL is not available; run `docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait` first"
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
        let child = command.spawn().expect("spawn monoengine service");
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

#[test]
fn integration_agent_capture_happy_path() {
    let env = CaptureEnv::storage_only("it-default");
    let (mut service, port, _stdout, stderr) = boot_storage_only(&env);
    let client = http_client();
    let base = api_base(port);

    let discovery = client
        .get(format!("{base}/discovery"))
        .header("Authorization", bearer())
        .send()
        .expect("discovery");
    assert_eq!(discovery.status().as_u16(), 200, "discovery");
    let discovery_json: serde_json::Value = discovery.json().expect("discovery json");
    assert_eq!(discovery_json["raw_accepted"], true);

    let capture_id = put_session(&client, &base, "sess-happy");
    let raw = format!("transcript {SENTINEL}");
    let digest = stage_and_finalize(&client, &base, capture_id, raw.as_bytes());
    let checkpoint = client
        .post(format!("{base}/sessions/{capture_id}/checkpoints"))
        .header("Authorization", bearer())
        .header("Content-Type", "application/json")
        .body(format!(
            r#"{{"checkpoint_id":"cp-1","transcript_digest":"{digest}"}}"#
        ))
        .send()
        .expect("checkpoint");
    assert_eq!(checkpoint.status().as_u16(), 200, "checkpoint");

    let transcript = client
        .get(format!("{base}/sessions/{capture_id}/transcript"))
        .header("Authorization", bearer())
        .send()
        .expect("transcript");
    assert_eq!(transcript.status().as_u16(), 200, "transcript");
    assert_eq!(transcript.bytes().expect("bytes").as_ref(), raw.as_bytes());

    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

#[test]
fn integration_agent_capture_review_404() {
    let env = CaptureEnv::review();
    let (mut service, port, _stdout, stderr) = boot_service_http(&env, &[]);
    let client = http_client();
    let discovery = client
        .get(format!("{}/discovery", api_base(port)))
        .header("Authorization", bearer())
        .send()
        .expect("discovery");
    assert_eq!(discovery.status().as_u16(), 404, "review discovery");
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

#[test]
fn integration_agent_capture_unauthorized() {
    let env = CaptureEnv::storage_only("it-default");
    let (mut service, port, _stdout, stderr) = boot_storage_only(&env);
    let client = http_client();
    let discovery = client
        .get(format!("{}/discovery", api_base(port)))
        .send()
        .expect("discovery");
    assert_eq!(discovery.status().as_u16(), 401, "missing token");
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

#[test]
fn integration_agent_capture_tracing_has_no_raw_sentinel() {
    let env = CaptureEnv::storage_only("it-default");
    let (mut service, port, stdout, stderr) = boot_storage_only(&env);
    let client = http_client();
    let base = api_base(port);
    let capture_id = put_session(&client, &base, "sess-trace");
    let raw = format!("prompt={SENTINEL}");
    let _digest = stage_and_finalize(&client, &base, capture_id, raw.as_bytes());
    let events = client
        .post(format!("{base}/sessions/{capture_id}/events:batch"))
        .header("Authorization", bearer())
        .header("Content-Type", "application/json")
        .body(format!(
            r#"{{"batch_id":"b1","events":[{{"event_uid":"0:0","event_kind":"message","payload":{{"text":"{SENTINEL}"}}}}]}}"#
        ))
        .send()
        .expect("events");
    assert_eq!(events.status().as_u16(), 200, "events");
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
    let logs = format!("{}{}", read_log(&stdout), read_log(&stderr));
    assert!(!logs.contains(SENTINEL), "tracing leaked raw sentinel");
}

#[test]
fn integration_agent_capture_cross_deployment_isolated() {
    let env = CaptureEnv::storage_only("it-a");
    let (mut service_a, port_a, _stdout_a, stderr_a) = boot_service_http(
        &env,
        &[
            ("MEGA_GIT__PUSH_AUTH", "none"),
            ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
            ("MEGA_AGENT_CAPTURE__DEPLOYMENT_ID", "it-a"),
        ],
    );
    let (mut service_b, port_b, _stdout_b, stderr_b) = boot_service_http(
        &env,
        &[
            ("MEGA_GIT__PUSH_AUTH", "none"),
            ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
            ("MEGA_AGENT_CAPTURE__DEPLOYMENT_ID", "it-b"),
        ],
    );
    let client = http_client();
    let capture_id = put_session(&client, &api_base(port_a), "sess-iso");
    let foreign = client
        .get(format!("{}/sessions/{capture_id}", api_base(port_b)))
        .header("Authorization", bearer())
        .send()
        .expect("foreign get");
    assert_eq!(foreign.status().as_u16(), 404, "cross-deployment");
    assert!(
        service_a
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown a failed\n{}",
        read_log(&stderr_a)
    );
    assert!(
        service_b
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown b failed\n{}",
        read_log(&stderr_b)
    );
}

#[test]
fn integration_agent_capture_tombstone_race() {
    let env = CaptureEnv::storage_only("it-default");
    let (mut service, port, _stdout, stderr) = boot_storage_only(&env);
    let client = http_client();
    let base = api_base(port);
    let capture_id = put_session(&client, &base, "sess-race");
    let db_url = env.database.db_url.clone();
    let barrier = Arc::new(Barrier::new(2));
    let tomb_barrier = barrier.clone();
    let tomb = thread::spawn(move || {
        tomb_barrier.wait();
        insert_tombstone_sql(&db_url, capture_id);
    });
    let ingest_barrier = barrier;
    let ingest_base = base.clone();
    let ingest = thread::spawn(move || {
        ingest_barrier.wait();
        http_client()
            .post(format!("{ingest_base}/sessions/{capture_id}/events:batch"))
            .header("Authorization", bearer())
            .header("Content-Type", "application/json")
            .body(r#"{"batch_id":"race","events":[{"event_uid":"0:0","event_kind":"message","payload":{}}]}"#)
            .send()
            .expect("race ingest")
            .status()
            .as_u16()
    });
    tomb.join().expect("tombstone thread");
    let racing_status = ingest.join().expect("ingest thread");
    assert!(
        racing_status == 200 || racing_status == 409,
        "racing ingest must be 200 or 409, got {racing_status}"
    );
    let follow = client
        .post(format!("{base}/sessions/{capture_id}/events:batch"))
        .header("Authorization", bearer())
        .header("Content-Type", "application/json")
        .body(r#"{"batch_id":"after","events":[{"event_uid":"0:1","event_kind":"message","payload":{}}]}"#)
        .send()
        .expect("follow ingest");
    assert_eq!(
        follow.status().as_u16(),
        409,
        "ingest after tombstone must not 200"
    );
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

fn storage_only_append(deployment_id: &str) -> String {
    format!(
        r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false

[agent_capture]
enabled = true
tenant_id = "default"
deployment_id = "{deployment_id}"

[[agent_capture.ingest_tokens]]
name = "hook"
token = "{INGEST_TOKEN}"
paths = ["/third-part"]
"#
    )
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_monoengine"));
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
