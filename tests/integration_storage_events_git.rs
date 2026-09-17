// Process-level WH-03 gates (plan-20260912): the `repo.push` adapter on the
// real `mega2` binary.
//
// Per the plan's verification boundary, the process target verifies real
// startup / config / auth / business results only — the positive outbound
// event is asserted by the B3 lib collectors
// (`jupiter::service::push_queue_service::tests::storage_event_commit_matrix`,
// `ceres::pack::api_tip_lander::tests::storage_event_api_commit`). What this
// target proves end-to-end:
//   1. storage-only + `[storage_events]` enabled + a vault-seeded target: a
//      real host-git trunk push lands (receive-pack business result), the
//      emitter's real transport logs a bounded single delivery attempt
//      (category line only — the `.invalid` destination never resolves), and
//      SIGINT still exits through the WH-13 cleanup tail;
//   2. review morphology rejects `enabled=true` at config validation;
//   3. review-morphology regression: a branch push still creates its CL row
//      and the service shuts down cleanly.
//
// Uses the host git binary exactly like `integration_git_cli`'s trunk cases
// (the compose git-cli runner cannot reach the host on this Linux bridge).

mod common;
#[allow(
    dead_code,
    reason = "the path-included helper also contains integration_git_cli-only auth probes"
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
const SHUTDOWN_RECEIPT: &str = "storage_events_shutdown_complete";
const DELIVERY_LOG: &str = "storage_events delivery";
/// Distinctive seeded sentinel; never allowed in captured logs.
const SENTINEL_PAYLOAD: &str = "abababababababababababababababababababababababababababababababab";
const HMAC_VALUE: &str = "hex:abababababababababababababababababababababababababababababababab";
const TARGET_ID: &str = "ops-main";
const SECRET_REF: &str = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value";

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
            "mega2_wh03_{}_{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);
        with_runtime(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let db = Database::connect(admin_url.as_str())
                    .await
                    .expect("integration PostgreSQL unavailable; start docker-compose.test.yml");
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

/// Per-case isolation: temp config + dirs, dedicated database, own port, and
/// a case dir for git working copies under the shared IT work root.
struct GitCase {
    temp_dir: TempDir,
    database: TestDatabase,
    config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
}

impl GitCase {
    fn new(config_append: &str) -> Self {
        git_cli::require_git_cli_runner();
        let work_root = git_cli::git_cli_workdir();
        fs::create_dir_all(&work_root).expect("create shared git workdir");
        let case_dir = work_root.join(format!(
            "case-wh03-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if case_dir.exists() {
            fs::remove_dir_all(&case_dir).expect("clean stale case dir");
        }
        fs::create_dir_all(&case_dir).expect("create case dir");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let config_path = temp_dir.path().join("config.toml");
        common::write_full_config_with_append(&config_path, config_append);
        Self {
            base_dir: temp_dir.path().join("base"),
            cache_dir: temp_dir.path().join("cache"),
            object_root: temp_dir.path().join("objects"),
            temp_dir,
            database,
            config_path,
            case_dir,
        }
    }

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
        let child = command.spawn().expect("spawn mega2 service");
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

/// WH-13 ordering: observe the receipt in a bounded window first, then the
/// exit code (the exit code alone is not evidence the cleanup tail ran).
fn assert_sigint_cleanup_and_exit(
    service: &mut ServiceProcess,
    stdout_path: &Path,
    stderr_path: &Path,
) {
    service.send_sigint();
    wait_for_log_line(
        stdout_path,
        stderr_path,
        SHUTDOWN_RECEIPT,
        Duration::from_secs(60),
    );
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

fn wait_for_log_line(
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

/// Emitter-side delivery outcome lines (the transport logs its own
/// `outcome=…` line next to the emitter's `category=…` line — count only the
/// emitter's to get deliveries).
fn emitter_delivery_lines(captured: &str) -> usize {
    captured
        .lines()
        .filter(|line| line.contains(DELIVERY_LOG) && line.contains("category="))
        .count()
}

/// WH-15 AC5: every emitter delivery line carries the deployment's
/// `installation_id`, and drop lines (message `storage_events dropped`)
/// never match the delivery filter above. Checked on real process output
/// because the delivery line is emitted inside a spawned send task.
fn count_drop_lines(captured: &str) -> usize {
    captured
        .lines()
        .filter(|line| line.contains("storage_events dropped"))
        .count()
}

fn assert_emitter_lines_carry_installation_id(captured: &str, installation_id: &str) {
    let needle = format!("installation_id={installation_id}");
    for line in captured.lines() {
        let delivery = line.contains(DELIVERY_LOG) && line.contains("category=");
        let dropped = line.contains("storage_events dropped");
        if delivery || dropped {
            assert!(
                line.contains(&needle),
                "emitter line lacks installation id: {line}"
            );
        }
        if dropped {
            assert!(
                !line.contains("category="),
                "drop line must not satisfy the delivery filter: {line}"
            );
        }
    }
}

/// The seeded HMAC secret and its SecretRef URI must never appear in captured
/// logs (WH-11 AC6/AC7); the receipt/delivery lines carry category fields
/// only (ADR-WH-02).
fn assert_logs_sanitized(stdout_path: &Path, stderr_path: &Path) {
    let captured = format!("{}\n{}", read_log(stdout_path), read_log(stderr_path));
    assert!(
        !captured.contains(HMAC_VALUE) && !captured.contains(SENTINEL_PAYLOAD),
        "captured logs must not contain the seeded secret in either form"
    );
    assert!(
        !captured.contains(SECRET_REF),
        "captured logs must not contain the full SecretRef URI"
    );
}

/// Host git exactly like `integration_git_cli`'s trunk cases: isolated HOME,
/// no credential helper (push_auth=none), HTTP/1.1, deterministic packing.
fn host_git(case_dir: &Path, git_args: &[&str]) -> std::process::Output {
    let isolated_home = case_dir.join("git-home");
    fs::create_dir_all(&isolated_home).expect("git home");
    let null_config = PathBuf::from("/dev/null");
    let mut command = Command::new("git");
    command
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("HOME", &isolated_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_CONFIG_COUNT", "5")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "http.version")
        .env("GIT_CONFIG_VALUE_2", "HTTP/1.1")
        .env("GIT_CONFIG_KEY_3", "pack.window")
        .env("GIT_CONFIG_VALUE_3", "0")
        .env("GIT_CONFIG_KEY_4", "pack.depth")
        .env("GIT_CONFIG_VALUE_4", "0")
        .args(git_args);
    let (out_path, err_path) = subprocess_log_paths(case_dir, "host-git");
    run_output_bounded(
        &mut command,
        Duration::from_secs(60),
        "host git",
        &out_path,
        &err_path,
    )
}

fn git_ok(case_dir: &Path, args: &[&str]) {
    let output = host_git(case_dir, args);
    assert!(
        output.status.success(),
        "git {} failed: {}\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git_stdout(case_dir: &Path, args: &[&str]) -> String {
    let output = host_git(case_dir, args);
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn configure_identity(case_dir: &Path, clone: &str) {
    git_ok(case_dir, &["-C", clone, "config", "user.name", "IT WH03"]);
    git_ok(
        case_dir,
        &[
            "-C",
            clone,
            "config",
            "user.email",
            "it-wh03@example.invalid",
        ],
    );
}

/// Host git with the seeded access token via GIT_ASKPASS (same credential
/// injection as `integration_git_cli`'s descendant cases).
fn host_git_auth(case_dir: &Path, token: &str, git_args: &[&str]) -> std::process::Output {
    git_cli::write_git_askpass(&case_dir.join("git-askpass.sh"));
    let isolated_home = case_dir.join("git-home-auth");
    fs::create_dir_all(&isolated_home).expect("git auth home");
    let null_config = PathBuf::from("/dev/null");
    let mut command = Command::new("git");
    command
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("HOME", &isolated_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(git_cli::GIT_ASKPASS_ENV, token)
        .env("GIT_ASKPASS", case_dir.join("git-askpass.sh"))
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "credential.username")
        .env("GIT_CONFIG_VALUE_0", git_cli::DEFAULT_GIT_AUTH_USER)
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_2", "false")
        .args(git_args);
    let (out_path, err_path) = subprocess_log_paths(case_dir, "host-git-auth");
    run_output_bounded(
        &mut command,
        Duration::from_secs(60),
        "host git auth",
        &out_path,
        &err_path,
    )
}

fn git_ok_auth(case_dir: &Path, token: &str, args: &[&str]) {
    let output = host_git_auth(case_dir, token, args);
    assert!(
        output.status.success(),
        "git {} failed: {}
{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn configure_identity_auth(case_dir: &Path, token: &str, clone: &str) {
    git_ok_auth(
        case_dir,
        token,
        &["-C", clone, "config", "user.name", "IT WH03"],
    );
    git_ok_auth(
        case_dir,
        token,
        &[
            "-C",
            clone,
            "config",
            "user.email",
            "it-wh03@example.invalid",
        ],
    );
}

/// storage-only (`push_auth=none`) `service http` command for this case.
fn storage_only_service_command(case: &GitCase, port: u16) -> Command {
    let mut command = case.command();
    command
        .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
        .env("MEGA_GIT__PUSH_AUTH", "none")
        .env("MEGA_GIT__SSH_RECEIVE_PACK", "false");
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &port.to_string(),
    ]);
    command
}

/// Seed one target HMAC secret through the real `config secret set` CLI flow
/// (same vault the service later resolves from: `MEGA_BASE_DIR` key + test
/// database).
fn seed_storage_events_hmac(case: &GitCase, value: &str) {
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

    let vault_path = format!("config/it/storage_events/targets/{TARGET_ID}/hmac");
    let mut command = isolated_command(&case.base_dir, &case.cache_dir);
    command
        .arg("--config")
        .arg(&bootstrap_path)
        .env("MEGA_LOG__PRINT_STD", "false")
        .env("MEGA_LOG__WITH_ANSI", "false");
    command.args([
        "config",
        "secret",
        "set",
        &format!("storage_events.targets.{TARGET_ID}.secret_ref"),
        "--vault-path",
        &vault_path,
        "--field",
        "value",
        "--value-stdin",
    ]);
    command.stdin(Stdio::piped());
    let (out_path, err_path) = subprocess_log_paths(case.temp_dir.path(), "seed");
    command
        .stdout(Stdio::from(
            fs::File::create(&out_path).expect("seed stdout file"),
        ))
        .stderr(Stdio::from(
            fs::File::create(&err_path).expect("seed stderr file"),
        ));
    let mut child = command.spawn().expect("spawn config secret set");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(value.as_bytes())
        .expect("write stdin");
    // Close stdin so `--value-stdin` sees EOF, then wait with a bound.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll secret set") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("config secret set did not finish within the bound");
        }
        sleep(Duration::from_millis(100));
    };
    let output = std::process::Output {
        status,
        stdout: fs::read(&out_path).unwrap_or_default(),
        stderr: fs::read(&err_path).unwrap_or_default(),
    };
    assert!(
        output.status.success(),
        "config secret set must succeed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

const STORAGE_EVENTS_CONFIG: &str = r#"
[storage_events]
enabled = true
installation_id = "it-wh03-process"

[[storage_events.targets]]
id = "ops-main"
url = "https://events.example.invalid/ingest"
secret_ref = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
events = ["repo.push"]
git_paths = ["/project/wh03"]
"#;

#[test]
fn integration_storage_events_git_trunk_push_events_enabled() {
    let case = GitCase::new(STORAGE_EVENTS_CONFIG);
    seed_storage_events_hmac(&case, HMAC_VALUE);
    let port = reserve_free_port();
    let (stdout_path, stderr_path) = case.log_paths("git-trunk-events");
    let command = storage_only_service_command(&case, port);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);
    service.wait_until_openapi_ready(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    // Real receive-pack business result: clone the (empty) /team/a subpath,
    // commit, and push HEAD:refs/heads/main; the remote tip must equal the
    // client commit (ADR-WH-04 — the event never changes the write result).
    // Monorepo directories are created by the parent repo's push: first land
    // `wh03/` content on `/project` (initialized at bootstrap via
    // `monorepo.root_dirs`), then push to the now-existing `/project/wh03`.
    let project_url = git_cli::mega2_host_http_url(port, "/project");
    git_ok(&case.case_dir, &["clone", &project_url, "seed"]);
    configure_identity(&case.case_dir, "seed");
    let seed = case.case_dir.join("seed");
    fs::create_dir_all(seed.join("wh03")).expect("mkdir wh03");
    fs::write(seed.join("wh03/one.txt"), b"wh03\n").expect("write file");
    git_ok(&case.case_dir, &["-C", "seed", "add", "wh03/one.txt"]);
    git_ok(&case.case_dir, &["-C", "seed", "commit", "-m", "wh03 seed"]);
    git_ok(
        &case.case_dir,
        &[
            "-C",
            "seed",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );

    let repo_url = git_cli::mega2_host_http_url(port, "/project/wh03");
    git_ok(&case.case_dir, &["clone", &repo_url, "work"]);
    configure_identity(&case.case_dir, "work");
    fs::write(case.case_dir.join("work").join("two.txt"), b"wh03 two\n").expect("write file");
    git_ok(&case.case_dir, &["-C", "work", "add", "two.txt"]);
    git_ok(
        &case.case_dir,
        &["-C", "work", "commit", "-m", "wh03 second"],
    );
    let head = git_stdout(&case.case_dir, &["-C", "work", "rev-parse", "HEAD"]);
    git_ok(
        &case.case_dir,
        &[
            "-C",
            "work",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    let remote = git_stdout(&case.case_dir, &["ls-remote", &repo_url, "refs/heads/main"]);
    let remote_head = remote
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("");
    assert_eq!(
        remote_head, head,
        "pushed commit must land as main@/project/wh03"
    );

    // The committed write produced exactly one bounded outbound attempt on
    // the real transport (the `.invalid` destination never resolves): the
    // sanitized delivery log carries target id / event type / category only.
    // The seed push to `/project` does NOT match `git_paths = ["/project/wh03"]`,
    // so the only delivery observed is the `/project/wh03` push's.
    let delivery = wait_for_log_line(
        &stdout_path,
        &stderr_path,
        DELIVERY_LOG,
        Duration::from_secs(30),
    );
    assert!(
        delivery.contains("repo.push") && delivery.contains(TARGET_ID),
        "delivery log must name the event type and target id only: {delivery}"
    );
    assert!(
        !delivery.contains("http"),
        "delivery log must not contain URLs: {delivery}"
    );

    assert_sigint_cleanup_and_exit(&mut service, &stdout_path, &stderr_path);
    // One committed write = one emitter delivery outcome line
    // (`category=…`); the transport logs its own `outcome=…` line alongside.
    let captured = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    let delivery_lines = emitter_delivery_lines(&captured);
    assert_eq!(
        delivery_lines, 1,
        "exactly one delivery attempt after drain (the filtered seed push emits none):\n{captured}"
    );
    assert_emitter_lines_carry_installation_id(&captured, "it-wh03-process");
    // WH-15: the seed push to `/project` is the one event the static filter
    // rejects (`git_paths = ["/project/wh03"]`), so the process must log
    // exactly one drop line, and it must be the filter disposition — a real
    // drop line on real stdout, not a vacuous helper pass.
    assert_eq!(
        count_drop_lines(&captured),
        1,
        "exactly one filtered seed push expected:\n{captured}"
    );
    assert!(
        captured
            .lines()
            .any(|l| l.contains("storage_events dropped") && l.contains("dropped_filter")),
        "the seed push must be recorded as dropped_filter:\n{captured}"
    );
    assert_logs_sanitized(&stdout_path, &stderr_path);
}

#[test]
fn integration_storage_events_git_review_rejects_enabled() {
    // WH-01 AC4 at process level: review morphology (default config) with
    // `[storage_events] enabled = true` fails `config validate`.
    let case = GitCase::new(STORAGE_EVENTS_CONFIG);
    let (stdout_path, stderr_path) = case.log_paths("git-review-rejects");
    let mut command = case.command();
    command.args(["config", "validate"]);
    let mut service = ServiceProcess::spawn(command, &stdout_path, &stderr_path);
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .expect("config validate must exit within the bound");
    assert!(
        !status.success(),
        "enabled storage_events under review morphology must be rejected"
    );
    let combined = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        combined.contains("storage-only"),
        "the rejection must name the storage-only requirement:\n{combined}"
    );
    assert!(
        !combined.contains(SHUTDOWN_RECEIPT),
        "config validate never enters the service cleanup tail:\n{combined}"
    );
}

#[test]
fn integration_storage_events_git_review_cl_regression() {
    // Review-morphology regression (storage_events absent): a branch push
    // still lands and still creates its CL row; the service shuts down
    // through the cleanup tail.
    let case = GitCase::new("");
    let port = reserve_free_port();
    let (stdout_path, stderr_path) = case.log_paths("git-review-cl");
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

    // Review morphology authenticates pushes with a DB access token.
    let token = git_cli::resolve_seed_token();
    git_cli::seed_access_token(
        &case.database.db_url,
        git_cli::DEFAULT_GIT_AUTH_USER,
        &token,
    );
    let root_url = git_cli::mega2_host_http_url(port, "/");
    git_ok_auth(&case.case_dir, &token, &["clone", &root_url, "seed"]);
    configure_identity_auth(&case.case_dir, &token, "seed");
    let seed = case.case_dir.join("seed");
    fs::create_dir_all(seed.join("project/foo")).expect("mkdir project/foo");
    fs::write(seed.join("project/foo/seed.txt"), b"seed\n").expect("write seed file");
    git_ok_auth(
        &case.case_dir,
        &token,
        &["-C", "seed", "add", "project/foo/seed.txt"],
    );
    git_ok_auth(
        &case.case_dir,
        &token,
        &["-C", "seed", "commit", "-m", "wh03 review seed"],
    );
    let branch = format!("refs/heads/wh03-review-{}", std::process::id());
    // pack.window/depth=0 disables deltas: a thin pack would panic the
    // server's decoder (same reason tp14 seeds push with these flags).
    let push = host_git_auth(
        &case.case_dir,
        &token,
        &[
            "-C",
            "seed",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "--no-thin",
            "origin",
            &format!("HEAD:{branch}"),
        ],
    );
    assert!(
        push.status.success(),
        "review branch push must succeed: {}
stdout:
{}
stderr:
{}
service stdout:
{}
service stderr:
{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr),
        String::from_utf8_lossy(&push.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    let (cls, _) = count_cl_artifacts(&case.database.db_url);
    assert_eq!(
        cls, 1,
        "review branch push must create exactly one mega_cl row"
    );

    assert_sigint_cleanup_and_exit(&mut service, &stdout_path, &stderr_path);
}

/// WH-03 AC3 at process level: the product API write (`create-entry`) lands
/// through the same B3 commit point — business result plus one bounded
/// delivery attempt per committed write (seed push + API write = 2).
#[test]
fn integration_storage_events_git_api_write_events_enabled() {
    const PUSH_TOKEN: &str = "wh03-api-write-token";
    let token_config = format!(
        r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "wh03-api"
token = "{PUSH_TOKEN}"
paths = ["/project"]

[storage_events]
enabled = true
installation_id = "it-wh03-api"

[[storage_events.targets]]
id = "ops-main"
url = "https://events.example.invalid/ingest"
secret_ref = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
events = ["repo.push"]
git_paths = ["/project"]
"#
    );
    let case = GitCase::new(&token_config);
    seed_storage_events_hmac(&case, HMAC_VALUE);
    let port = reserve_free_port();
    let (stdout_path, stderr_path) = case.log_paths("git-api-write");
    let mut command = case.command();
    command.env("MEGA_MONOREPO__PUSH_POLICY", "trunk");
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

    // Seed the /project tip with a real token push (lander B0 needs a
    // non-root path tip).
    let project_url = git_cli::mega2_host_http_url(port, "/project");
    let project_url = format!("{}/", project_url.trim_end_matches('/'));
    git_ok_auth(&case.case_dir, PUSH_TOKEN, &["clone", &project_url, "seed"]);
    configure_identity_auth(&case.case_dir, PUSH_TOKEN, "seed");
    let seed = case.case_dir.join("seed");
    fs::write(seed.join("seed.txt"), b"seed\n").expect("write seed");
    git_ok_auth(
        &case.case_dir,
        PUSH_TOKEN,
        &["-C", "seed", "add", "seed.txt"],
    );
    git_ok_auth(
        &case.case_dir,
        PUSH_TOKEN,
        &["-C", "seed", "commit", "-m", "wh03 api seed"],
    );
    git_ok_auth(
        &case.case_dir,
        PUSH_TOKEN,
        &[
            "-C",
            "seed",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    let tip_before = path_tip(&case.database.db_url, "/project");

    // The product API write lands through `land_api_tip_push` (same B3 point).
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build HTTP client");
    let create = client
        .post(format!("http://127.0.0.1:{port}/api/v1/create-entry"))
        .header("Authorization", format!("Bearer {PUSH_TOKEN}"))
        .json(&serde_json::json!({
            "is_directory": false,
            "name": "wh03-api.txt",
            "path": "/project",
            "content": "hello from api create\n",
            "skip_build": true
        }))
        .send()
        .expect("token create-entry");
    let create_status = create.status().as_u16();
    let create_body = create.text().expect("create body");
    assert_eq!(
        create_status, 200,
        "token create-entry must 200: {create_body}"
    );
    let tip_after = path_tip(&case.database.db_url, "/project");
    assert_ne!(
        tip_after, tip_before,
        "API write must advance the /project tip"
    );

    assert_sigint_cleanup_and_exit(&mut service, &stdout_path, &stderr_path);
    let captured = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    let delivery_lines = emitter_delivery_lines(&captured);
    assert_eq!(
        delivery_lines, 2,
        "exactly one delivery per committed write (seed push + API write):\n{captured}"
    );
    assert_emitter_lines_carry_installation_id(&captured, "it-wh03-api");
    // Both committed writes match `git_paths = ["/project"]`: no drop line.
    assert_eq!(
        count_drop_lines(&captured),
        0,
        "no drop expected:\n{captured}"
    );
    assert_logs_sanitized(&stdout_path, &stderr_path);
}

/// Current `main` tip commit of `path` (mega_refs), empty when absent.
fn path_tip(db_url: &str, path: &str) -> String {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .expect("connect integration DB for path tip");
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT ref_commit_hash FROM mega_refs \
                     WHERE path = '{path}' AND ref_name = 'refs/heads/main' AND NOT is_cl"
                ),
            ))
            .await
            .expect("query path tip");
        row.map(|row| row.try_get("", "ref_commit_hash").expect("tip column"))
            .unwrap_or_default()
    })
}

fn count_cl_artifacts(db_url: &str) -> (i64, i64) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .expect("connect integration DB for CL count");
        let cl = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS n FROM mega_cl".to_string(),
            ))
            .await
            .expect("count mega_cl")
            .expect("mega_cl count row");
        let cl_refs = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS n FROM mega_refs WHERE ref_name LIKE 'refs/cl/%'"
                    .to_string(),
            ))
            .await
            .expect("count cl refs")
            .expect("cl refs count row");
        let read_count =
            |row: sea_orm::QueryResult| -> i64 { row.try_get("", "n").expect("count column") };
        (read_count(cl), read_count(cl_refs))
    })
}

/// Bounded subprocess run with output redirected to per-call files: no pipe
/// buffer can deadlock the child, and a descendant holding the descriptors
/// cannot block the reads afterwards. Kill + reap on timeout.
fn run_output_bounded(
    command: &mut Command,
    timeout: Duration,
    what: &str,
    out_path: &Path,
    err_path: &Path,
) -> std::process::Output {
    command
        .stdout(Stdio::from(
            fs::File::create(out_path).expect("stdout file"),
        ))
        .stderr(Stdio::from(
            fs::File::create(err_path).expect("stderr file"),
        ));
    let mut child = command.spawn().expect("spawn subprocess");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{what} did not finish within {timeout:?}
stdout file:\n{}
stderr file:\n{}",
                read_log(out_path),
                read_log(err_path),
            );
        }
        sleep(Duration::from_millis(100));
    };
    std::process::Output {
        status,
        stdout: fs::read(out_path).unwrap_or_default(),
        stderr: fs::read(err_path).unwrap_or_default(),
    }
}

/// Per-invocation output files so concurrent git/CLI calls never share a
/// pipe or a path.
fn subprocess_log_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    static SUBPROCESS_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = SUBPROCESS_COUNTER.fetch_add(1, Ordering::Relaxed);
    (
        dir.join(format!("{name}-{}-{n}.out", std::process::id())),
        dir.join(format!("{name}-{}-{n}.err", std::process::id())),
    )
}

fn isolated_command(base_dir: &Path, cache_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mega2"));
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
