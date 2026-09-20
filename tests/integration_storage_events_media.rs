// Process-level WH-06 gates (plan-20260912): the FastCDC media finalize
// adapter on real binaries.
//
// This target compiles under DEFAULT features (so `cargo test --all` covers
// it) and boots two independently-built binaries, each in its own target dir:
//   * feature-off: `<repo>.git/info/lfs/libra/media/v1/...` routes are absent
//     (404) and no media event can exist;
//   * feature-on: the AccessTokenUser auth matrix is unchanged by the event
//     hook — anonymous and static-push-token-shaped requests stay 401 with
//     zero events, a valid DB access token passes auth (404 on the unknown
//     pending session) with zero events, and the service exits through the
//     WH-13 cleanup tail.
// The positive event itself is covered at lib level by
// `ceres::lfs::media::finalize::tests::storage_event_finalize_matrix` and
// `api::router::lfs_media::tests::storage_event_auth_reachability` (both run
// with `--features fastcdc`).

mod common;
#[allow(
    dead_code,
    reason = "the path-included helper also contains git-cli-only helpers"
)]
#[path = "common/git_cli.rs"]
mod git_cli;

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
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
const RECEIPT: &str = "storage_events_shutdown_complete";
// External route shape: `<repo>.git/info/lfs/libra/media/v1/...` (the rewrite
// middleware restores the repo context from the URL prefix). OpenAPI
// registers the same routes as `/api/v1/lfs/libra/media/v1/...`.
const MEDIA_FINALIZE: &str = "/acme/app.git/info/lfs/libra/media/v1/manifests/wh06/finalize";
const MEDIA_CAPABILITIES: &str = "/acme/app.git/info/lfs/libra/media/v1/capabilities";
const SECRET_REF: &str = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value";
const HMAC_VALUE: &str = "hex:0101010101010101010101010101010101010101010101010101010101010101";
const SENTINEL_PAYLOAD: &str = "0101010101010101010101010101010101010101010101010101010101010101";

static CASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = std::env::var("MEGA_DATABASE__DB_URL")
            .unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string());
        let db_name = format!(
            "mega2_wh06_{}_{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);
        with_runtime(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let db = Database::connect(admin_url.as_str())
                    .await
                    .expect("integration PostgreSQL unavailable; start docker/docker-compose.test.yml");
                execute_postgres(&db, format!("DROP DATABASE IF EXISTS {db_name}")).await;
                execute_postgres(&db, format!("CREATE DATABASE {db_name}")).await;
            })
            .await
            .expect("database setup timed out");
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
struct MediaCase {
    temp_dir: TempDir,
    database: TestDatabase,
    config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    token_morphology: bool,
}

impl MediaCase {
    fn new(storage_events_enabled: bool) -> Self {
        Self::with_morphology(storage_events_enabled, false)
    }

    /// `token_morphology` adds a configured static push token and the service
    /// boots with `push_auth=token` (ADR-WH-05 requires the matrix under both
    /// `token` and `none`).
    fn with_morphology(storage_events_enabled: bool, token_morphology: bool) -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let config_path = temp_dir.path().join("config.toml");
        let events_append = if storage_events_enabled {
            r#"
[storage_events]
enabled = true
installation_id = "it-wh06-process"

[[storage_events.targets]]
id = "ops-main"
url = "https://events.example.invalid/ingest"
secret_ref = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
events = ["lfs.media.finalized"]
lfs_paths = ["/acme/app.git"]
"#
        } else {
            ""
        };
        let token_append = if token_morphology {
            r#"
[git]
push_auth = "token"
ssh_receive_pack = false

[[git.push_tokens]]
name = "wh06-static"
token = "wh06-static-push-token"
paths = ["/acme"]
"#
        } else {
            ""
        };
        let append = format!("{events_append}{token_append}");
        common::write_full_config_with_append(&config_path, &append);
        Self {
            base_dir: temp_dir.path().join("base"),
            cache_dir: temp_dir.path().join("cache"),
            object_root: temp_dir.path().join("objects"),
            temp_dir,
            database,
            config_path,
            token_morphology,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("MEGA_BASE_DIR", &self.base_dir)
            .env("MEGA_CACHE_DIR", &self.cache_dir)
            .env("RUST_BACKTRACE", "0");
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(ld) = std::env::var_os("LD_LIBRARY_PATH") {
            command.env("LD_LIBRARY_PATH", ld);
        }
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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            // storage-only morphology ([storage_events] enabled requires it)
            .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
            .env("MEGA_GIT__SSH_RECEIVE_PACK", "false")
            .env(
                "MEGA_GIT__PUSH_AUTH",
                if self.token_morphology {
                    "token"
                } else {
                    "none"
                },
            );
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
        let child = command.spawn().expect("spawn mega2 service");
        Self {
            child,
            reaped: false,
        }
    }

    fn wait_until_openapi_ready(&mut self, port: u16, out: &Path, err: &Path) {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build HTTP client");
        let url = format!("http://127.0.0.1:{port}/api/openapi.json");
        let deadline = Instant::now() + Duration::from_secs(90);
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
                    read_log(out),
                    read_log(err),
                );
            }
            assert!(
                Instant::now() < deadline,
                "service OpenAPI not ready on port {port}\nstdout:\n{}\nstderr:\n{}",
                read_log(out),
                read_log(err),
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

/// Build (or reuse the incremental build of) the two WH-06 binaries, each in
/// its own target dir so the default-feature `cargo test` build lock is never
/// contended. Bounded at 20 minutes per binary (cold cache).
fn ensure_binary(feature_on: bool) -> PathBuf {
    let target_dir = if feature_on {
        "target/wh06-fastcdc-on"
    } else {
        "target/wh06-fastcdc-off"
    };
    let binary = PathBuf::from(target_dir).join("debug/mega2");
    let mut command = Command::new("cargo");
    command.arg("build").arg("-p").arg("mega2");
    if feature_on {
        command.arg("--features").arg("fastcdc");
    }
    command
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_NET_OFFLINE", "true");
    // Bounded build: spawn + poll, kill+reap on timeout so a stuck build or
    // lock wait can never hang the test.
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cargo build for the WH-06 binary");
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll cargo build") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "building the feature-{} binary timed out",
                if feature_on { "on" } else { "off" }
            );
        }
        sleep(Duration::from_secs(5));
    };
    assert!(
        status.success(),
        "building the feature-{} binary failed: {status}",
        if feature_on { "on" } else { "off" }
    );
    assert!(
        binary.is_file(),
        "expected the built binary at {}",
        binary.display()
    );
    binary
}

/// Bounded wait for a log line across both captured streams.
fn wait_log_line(
    stdout_path: &Path,
    stderr_path: &Path,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let combined = format!("{}\n{}", read_log(stdout_path), read_log(stderr_path));
        if let Some(line) = combined.lines().find(|line| line.contains(needle)) {
            return line.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "`{needle}` not observed within {timeout:?}\nstdout:\n{}\nstderr:\n{}",
            read_log(stdout_path),
            read_log(stderr_path),
        );
        sleep(Duration::from_millis(100));
    }
}

/// SIGINT -> receipt observed in a bounded window -> exit 0; returns the full
/// captured logs for content assertions.
fn shutdown_and_capture(
    service: &mut ServiceProcess,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    service.send_sigint();
    wait_log_line(stdout_path, stderr_path, RECEIPT, Duration::from_secs(60));
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .expect("service must exit after the shutdown receipt");
    assert!(
        status.success(),
        "service must exit 0 after graceful shutdown: {status}\nstdout:\n{}\nstderr:\n{}",
        read_log(stdout_path),
        read_log(stderr_path),
    );
    format!("{}\n{}", read_log(stdout_path), read_log(stderr_path))
}

fn emitter_delivery_count(captured: &str) -> usize {
    captured
        .lines()
        .filter(|line| line.contains("storage_events delivery") && line.contains("category="))
        .count()
}

fn assert_logs_sanitized(captured: &str) {
    assert!(
        !captured.contains(HMAC_VALUE) && !captured.contains(SENTINEL_PAYLOAD),
        "captured logs must not contain the seeded secret in either form"
    );
    assert!(
        !captured.contains(SECRET_REF),
        "captured logs must not contain the full SecretRef URI"
    );
}

/// Seed the target HMAC secret through the real `config secret set` CLI flow
/// (the feature-off binary is enough — seeding only touches vault+DB).
fn seed_target_secret(case: &MediaCase, off_binary: &Path) {
    let bootstrap_path = case.temp_dir.path().join("bootstrap-config.toml");
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
            case.database.db_url
        ),
    )
    .expect("write bootstrap config");

    let mut command = Command::new(off_binary);
    command
        .env_clear()
        .env("MEGA_BASE_DIR", &case.base_dir)
        .env("MEGA_CACHE_DIR", &case.cache_dir);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .arg("--config")
        .arg(&bootstrap_path)
        .env("MEGA_LOG__PRINT_STD", "false")
        .env("MEGA_LOG__WITH_ANSI", "false");
    command.args([
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
        case.temp_dir.path().join("seed.out"),
        case.temp_dir.path().join("seed.err"),
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

#[test]
fn integration_storage_events_media_feature_matrix() {
    let off_binary = ensure_binary(false);
    let on_binary = ensure_binary(true);

    // --- feature-off binary: media routes are absent, no media event can
    // exist, and the service still shuts down through the cleanup tail.
    let case = MediaCase::new(true);
    seed_target_secret(&case, &off_binary);
    let port = reserve_free_port();
    let (out, err) = case.log_paths("feature-off");
    let mut command = case.command(&off_binary);
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &out, &err);
    service.wait_until_openapi_ready(port, &out, &err);
    let openapi = reqwest::blocking::get(format!("http://127.0.0.1:{port}/api/openapi.json"))
        .expect("openapi")
        .text()
        .expect("openapi body");
    assert!(
        !openapi.contains("libra/media"),
        "feature-off OpenAPI must not list media routes"
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let response = client
        .post(format!("http://127.0.0.1:{port}{MEDIA_FINALIZE}"))
        .send()
        .expect("feature-off finalize request");
    assert_eq!(
        response.status().as_u16(),
        404,
        "feature-off media route must be 404"
    );
    let captured = shutdown_and_capture(&mut service, &out, &err);
    assert_eq!(
        emitter_delivery_count(&captured),
        0,
        "feature-off can never deliver media events:\n{captured}"
    );
    assert_logs_sanitized(&captured);

    // --- feature-on binary: routes exist; the AccessTokenUser matrix is
    // unchanged by the hook; zero events without a real finalize.
    let case = MediaCase::new(true);
    seed_target_secret(&case, &off_binary);
    let port = reserve_free_port();
    let (out, err) = case.log_paths("feature-on");
    let mut command = case.command(&on_binary);
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &out, &err);
    service.wait_until_openapi_ready(port, &out, &err);
    let openapi = reqwest::blocking::get(format!("http://127.0.0.1:{port}/api/openapi.json"))
        .expect("openapi")
        .text()
        .expect("openapi body");
    assert!(
        openapi.contains("libra/media"),
        "feature-on OpenAPI must list media routes"
    );

    // The AccessTokenUser matrix under `push_auth=none`: anonymous 401,
    // unknown bearer 401, DB token reaches the route (404), caps 200.
    run_auth_matrix(&case.database.db_url, port, None);

    let captured = shutdown_and_capture(&mut service, &out, &err);
    assert_eq!(
        emitter_delivery_count(&captured),
        0,
        "no real finalize happened, so zero events:\n{captured}"
    );
    assert_logs_sanitized(&captured);

    // --- feature-on binary under `push_auth=token` with a CONFIGURED static
    // push token: the media routes must still reject it (static push tokens
    // are not DB access tokens) — ADR-WH-05 requires the matrix under both
    // morphologies.
    let case = MediaCase::with_morphology(true, true);
    seed_target_secret(&case, &off_binary);
    let port = reserve_free_port();
    let (out, err) = case.log_paths("feature-on-token");
    let mut command = case.command(&on_binary);
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    let mut service = ServiceProcess::spawn(command, &out, &err);
    service.wait_until_openapi_ready(port, &out, &err);
    run_auth_matrix(&case.database.db_url, port, Some("wh06-static-push-token"));

    let captured = shutdown_and_capture(&mut service, &out, &err);
    assert_eq!(
        emitter_delivery_count(&captured),
        0,
        "no real finalize happened, so zero events:\n{captured}"
    );
    assert_logs_sanitized(&captured);
}

/// The AccessTokenUser matrix against a running service: anonymous 401, the
/// given bearer (if any — a configured static push token in the token
/// morphology) 401, a valid DB access token reaches the route (business 404
/// for an unknown pending session) and capabilities 200. No events can exist
/// because no real finalize happens.
fn run_auth_matrix(db_url: &str, port: u16, configured_static_token: Option<&str>) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    // Anonymous finalize: 401.
    let anon = client
        .post(format!("http://127.0.0.1:{port}{MEDIA_FINALIZE}"))
        .send()
        .expect("anonymous finalize");
    assert_eq!(anon.status().as_u16(), 401, "anonymous must stay 401");
    // A bearer that is not a DB access token must stay 401 — in the token
    // morphology this is the CONFIGURED static push token.
    let bogus = configured_static_token.unwrap_or("wh06-unknown-bearer");
    let static_token = client
        .post(format!("http://127.0.0.1:{port}{MEDIA_FINALIZE}"))
        .header("Authorization", format!("Bearer {bogus}"))
        .send()
        .expect("static token finalize");
    assert_eq!(
        static_token.status().as_u16(),
        401,
        "a non-DB token (even a configured static push token) must stay 401"
    );
    // A valid DB access token passes auth; the unknown pending session is a
    // business 404 (proving auth reachability without a real finalize).
    let token = git_cli::resolve_seed_token();
    git_cli::seed_access_token(db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);
    let authorized = client
        .post(format!("http://127.0.0.1:{port}{MEDIA_FINALIZE}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .expect("authorized finalize");
    assert_eq!(
        authorized.status().as_u16(),
        404,
        "authenticated but unknown pending session must be 404 (auth passed)"
    );
    // Capabilities rounds out the matrix: anonymous 401, the DB token 200.
    let anon_caps = client
        .get(format!("http://127.0.0.1:{port}{MEDIA_CAPABILITIES}"))
        .send()
        .expect("anonymous capabilities");
    assert_eq!(anon_caps.status().as_u16(), 401, "anonymous caps 401");
    let caps = client
        .get(format!("http://127.0.0.1:{port}{MEDIA_CAPABILITIES}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .expect("authorized capabilities");
    assert_eq!(caps.status().as_u16(), 200, "authorized caps 200");
}

fn integration_redis_url() -> String {
    std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string())
}

fn database_url_for_name(admin_url: &str, db_name: &str) -> String {
    let mut url = url::Url::parse(admin_url).expect("valid MEGA_DATABASE__DB_URL");
    url.set_path(db_name);
    url.to_string()
}

async fn execute_postgres(db: &sea_orm::DatabaseConnection, sql: String) {
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .expect("prepare integration PostgreSQL database");
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
