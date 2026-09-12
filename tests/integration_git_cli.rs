// Process-level black-box Git CLI integration tests (IT-03 / IT-10 / IT-12).
//
// Starts a real `service http` via `CARGO_BIN_EXE_monoengine`, drives the fixed
// compose `git-cli` runner over HTTP smart protocol, and asserts clone→push→
// re-clone working-tree round-trips with per-case DB/port/workdir isolation.
// Monorepo product rules (`docs/monorepo.md`): only public branch is `main`;
// client branch pushes land on `refs/cl/*` (no new public heads); Git-client
// tag push is rejected (tags via Web `/tags` API only).
// IT-10 adds auth-boundary cases (`integration_git_cli_auth_*`).
// IT-12 adds failpath cases (`integration_git_cli_failpath_*`).
// Credential injection lives in `common/git_cli.rs` (shared via `#[path]` with
// the `integration_git_lfs` / `integration_git_ssh` targets since GM-05/GM-06).
// This target does not modify `integration_vault.rs`.

mod common;
#[path = "common/git_cli.rs"]
mod git_cli;

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::{self, sleep},
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);
static CASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "monoengine_git_{}_{}",
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

struct GitCliEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
}

impl GitCliEnv {
    fn new() -> Self {
        Self::with_config_append("")
    }

    fn with_git_anonymous_access(anonymous_access: bool) -> Self {
        Self::with_config_append(&format!("\n[git]\nanonymous_access = {anonymous_access}\n"))
    }

    fn with_config_append(append: &str) -> Self {
        git_cli::require_git_cli_runner();

        let work_root = git_cli::git_cli_workdir();
        fs::create_dir_all(&work_root).unwrap_or_else(|err| {
            panic!("create shared git workdir {}: {err}", work_root.display())
        });

        let case_name = format!(
            "case-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let case_dir = work_root.join(&case_name);
        // Case dirs are retained after the test so IT-10 VER can scan
        // `.git/config` under the shared work root; VER/operators wipe the root.
        if case_dir.exists() {
            fs::remove_dir_all(&case_dir).expect("clean stale case dir");
        }
        fs::create_dir_all(&case_dir).expect("create case dir");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let full_config_path = temp_dir.path().join("config.toml");
        let base_dir = temp_dir.path().join("base");
        let cache_dir = temp_dir.path().join("cache");
        let object_root = temp_dir.path().join("objects");

        common::write_full_config_with_append(&full_config_path, append);
        git_cli::write_git_askpass(&case_dir.join("git-askpass.sh"));

        Self {
            temp_dir,
            database,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
            case_dir,
        }
    }

    fn full_config_command(&self) -> Command {
        self.command_with_config(&self.full_config_path)
    }

    fn command_with_config(&self, config_path: &Path) -> Command {
        let mut command = isolated_command(self.temp_dir.path(), &self.base_dir, &self.cache_dir);
        command.arg("--config").arg(config_path);
        command
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_PATH", "")
            .env("MEGA_DATABASE__DB_URL", &self.database.db_url)
            .env("MEGA_DATABASE__MAX_CONNECTION", "4")
            .env("MEGA_DATABASE__MIN_CONNECTION", "1")
            .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
            .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
            .env("MEGA_DATABASE__SQLX_LOGGING", "false")
            .env("MEGA_LOG__PRINT_STD", "false")
            .env("MEGA_LOG__WITH_ANSI", "false")
            .env("MEGA_REDIS__URL", integration_redis_url())
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root);
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
        // Own the Child before the fallible evidence write so Drop reaps on panic.
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
        // SAFETY: SIGINT to a child we own; CLI installs a ctrl-c handler.
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

    fn assert_alive(&mut self) {
        if let Some(status) = self.child.try_wait().expect("poll service") {
            panic!("service exited unexpectedly with {status}");
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

fn boot_service_http(env: &GitCliEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_enforcement(env, None)
}

/// Boot `service http` under an explicit `MEGA_CEDAR__ENFORCEMENT` (UN-16 e2e:
/// the grant/revoke immediate-effect case runs under `enforce`).
fn boot_service_http_with_enforcement(
    env: &GitCliEnv,
    enforcement: Option<&str>,
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_enforcement_and_session(env, enforcement, None)
}

/// As above, but optionally points the service's browser-session lookup at a
/// stub website (UN-24: privileged API calls now need a real subject).
fn boot_service_http_with_enforcement_and_session(
    env: &GitCliEnv,
    enforcement: Option<&str>,
    session_stub_port: Option<u16>,
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_env(env, enforcement, session_stub_port, &[])
}

fn boot_service_http_with_env(
    env: &GitCliEnv,
    enforcement: Option<&str>,
    session_stub_port: Option<u16>,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    if let Some(enforcement) = enforcement {
        command.env("MEGA_CEDAR__ENFORCEMENT", enforcement);
    }
    if let Some(stub_port) = session_stub_port {
        command.env(
            "MEGA_OAUTH__WEBSITE_API_BASE_URL",
            format!("http://127.0.0.1:{stub_port}"),
        );
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    git_cli::apply_monoengine_public_http_base_env(&mut command, port);
    command.args([
        "service",
        "http",
        "--host",
        git_cli::service_listen_host(),
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

/// Cookie value the session stub accepts; its content is irrelevant because the
/// stub answers every request the same way.
const SESSION_COOKIE_VALUE: &str = "it-un16-session";

/// Minimal stand-in for the website's Better Auth `get-session` endpoint.
///
/// Since UN-24, `merge-no-auth` requires *authorization* (it never required
/// authentication), so this test has to make its privileged merge call as a
/// real subject. The service resolves browser sessions by asking the website;
/// this stub answers with the admin, which is the subject whose ACL change the
/// test is merging. Returns the port it listens on.
fn spawn_website_session_stub(username: &str) -> u16 {
    spawn_website_session_stub_for(&[(SESSION_COOKIE_VALUE, username)])
}

/// Cookie-value → username stub, so one service can be driven as different
/// subjects. UN-19 needs that: the same CL has to be offered to an admin and to
/// a non-admin, and only one of them may merge it.
fn spawn_website_session_stub_for(sessions: &[(&str, &str)]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind session stub");
    let port = listener.local_addr().expect("stub addr").port();
    let sessions: Vec<(String, String)> = sessions
        .iter()
        .map(|(cookie, user)| ((*cookie).to_owned(), (*user).to_owned()))
        .collect();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read just enough to reach the end of the request head.
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..read]).to_string();

            // Match the cookie *value* exactly rather than by substring, so
            // one session value cannot be mistaken for another that contains it.
            let presented = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_default();
            let username = presented
                .split(';')
                .filter_map(|pair| pair.trim().split_once('='))
                .find_map(|(_, value)| {
                    sessions
                        .iter()
                        .find(|(cookie, _)| cookie == value)
                        .map(|(_, user)| user.clone())
                });

            let response = match username {
                Some(username) => {
                    let body = format!(
                        r#"{{"session":{{"id":"it-session","userId":"{username}"}},"user":{{"id":"{username}","name":"{username}","email":"{username}@example.invalid"}}}}"#
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
                // An unknown cookie is simply not a session.
                None => "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_owned(),
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

/// Merge a CL through `POST /cl/{link}/merge-no-auth`, carrying the stub
/// session so the call is authorized (UN-24). Returns the HTTP status code.
fn merge_cl_no_auth(port: u16, cl_link: &str) -> u16 {
    merge_cl_no_auth_as(port, cl_link, SESSION_COOKIE_VALUE)
}

/// Same call, carrying the cookie that maps to `session_value` in the stub.
fn merge_cl_no_auth_as(port: u16, cl_link: &str, session_value: &str) -> u16 {
    let url = format!("http://127.0.0.1:{port}/api/v1/cl/{cl_link}/merge-no-auth");
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "-X",
        "POST",
        "-H",
        &format!("Cookie: better-auth.session_token={session_value}"),
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "30",
        &url,
    ]);
    let output = command.output().expect("curl merge-no-auth");
    assert!(
        output.status.success(),
        "curl merge-no-auth failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad http code from curl merge: {:?}", output.stdout))
}

#[test]
fn integration_git_cli_http_round_trip() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();

    let token = git_cli::resolve_seed_token();
    let fixture_payload = format!(
        "monoengine it-03 fixture pid={} case={}\n",
        std::process::id(),
        env.case_dir.display()
    );
    let fixture_rel = Path::new("it-03-fixture.txt");
    let fixture_host = env.case_dir.join("fixture").join(fixture_rel);
    fs::create_dir_all(fixture_host.parent().expect("fixture parent")).expect("mkdir fixture");
    fs::write(&fixture_host, &fixture_payload).expect("write fixture");

    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);

    // Migrations + access_token table exist only after service bootstrap.
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone1 = env.case_dir.join("clone1");
    let clone1_name = "clone1";
    fs::create_dir_all(&clone1).expect("mkdir clone1");

    let output = git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone1_name]);
    git_cli::assert_git_success(&output, "initial HTTP clone");

    // init_monorepo seeds a deterministic worktree (converter::init_trees). Assert
    // the complete known fixture set — not just a few markers — so upload-pack
    // corruption of Buck/Cedar files cannot silently become the round-trip baseline.
    let initial_tree = snapshot_workdir(&clone1);
    let expected_initial = expected_init_monorepo_fixture();
    assert_eq!(
        initial_tree, expected_initial,
        "initial HTTP clone must match the complete seeded monorepo fixture byte-for-byte"
    );

    let branch = format!("it-03-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone1_name, "checkout", "-b", &branch],
        ),
        "create branch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone1_name, "config", "user.name", "IT Git CLI"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone1_name,
                "config",
                "user.email",
                "it-git-cli@example.invalid",
            ],
        ),
        "git user.email",
    );

    fs::copy(&fixture_host, clone1.join(fixture_rel)).expect("copy fixture into clone1");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone1_name, "add", fixture_rel.to_str().unwrap()],
        ),
        "git add fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone1_name, "commit", "-m", "it-03 fixture"],
        ),
        "git commit",
    );

    let fixture_key = path_bytes(fixture_rel);
    let fixture_bytes = fixture_payload.as_bytes().to_vec();

    let pre_push_tree = snapshot_workdir(&clone1);
    assert_eq!(
        pre_push_tree.get(&fixture_key),
        Some(&fixture_bytes),
        "pre-push working tree must contain the preset fixture bytes"
    );

    let before_refs = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    let before_heads = ls_remote_refs(&env.case_dir, &token, &remote_url, "refs/heads/*");

    let refspec = format!("HEAD:refs/heads/{branch}");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone1_name,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &refspec,
            ],
        ),
        "authenticated HTTP push",
    );

    let after_refs = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    let after_heads = ls_remote_refs(&env.case_dir, &token, &remote_url, "refs/heads/*");
    assert_eq!(
        before_heads, after_heads,
        "Monorepo push must not create/alter public refs/heads/* (docs/monorepo.md §1)"
    );
    assert!(
        !after_heads
            .iter()
            .any(|r| r == &format!("refs/heads/{branch}")),
        "public branch refs/heads/{branch} must not exist after Monorepo CL push",
        branch = branch
    );
    let cl_ref = after_refs
        .into_iter()
        .find(|r| !before_refs.contains(r))
        .unwrap_or_else(|| panic!("expected a new refs/cl/* after branch push"));

    let clone2_name = "clone2";
    let clone2 = env.case_dir.join(clone2_name);
    // Post-push default-tip clone must still succeed (upload-pack advertisement).
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["clone", &remote_url, "clone2-default"],
        ),
        "post-push HTTP re-clone of default tip",
    );
    let default_tip_tree = snapshot_workdir(&env.case_dir.join("clone2-default"));
    assert_eq!(
        default_tip_tree, initial_tree,
        "post-push default tip must remain the seeded monorepo tree (unchanged by branch push)"
    );
    assert!(
        !default_tip_tree.contains_key(&fixture_key),
        "default tip must not yet contain the branch-only fixture (lives on refs/cl/*)"
    );
    // Monorepo branch push lands on refs/cl/*; directed fetch of that tip is how
    // we recover the pushed tree (same pattern as scripts/git_protocol_smoke.sh LFS).
    fs::create_dir_all(&clone2).expect("mkdir clone2");
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["-C", clone2_name, "init"]),
        "init verify worktree",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone2_name, "remote", "add", "origin", &remote_url],
        ),
        "add origin for CL tip fetch",
    );
    let fetch_refspec = format!("{cl_ref}:refs/heads/verify");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone2_name, "fetch", "origin", &fetch_refspec],
        ),
        "fetch pushed CL tip",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone2_name, "checkout", "verify"],
        ),
        "checkout pushed CL tip",
    );

    let post_clone_tree = snapshot_workdir(&clone2);
    assert_eq!(
        post_clone_tree.get(&fixture_key),
        Some(&fixture_bytes),
        "HTTP clone of self-built remote tip must match preset fixture bytes"
    );
    assert_eq!(
        post_clone_tree, pre_push_tree,
        "re-clone working tree must match the pre-push working tree byte-for-byte"
    );

    // Remote URL for the fetch must remain credential-free.
    let remote_get = git_cli::git_cli(
        &env.case_dir,
        &token,
        &["-C", clone2_name, "remote", "get-url", "origin"],
    );
    git_cli::assert_git_success(&remote_get, "remote get-url");
    let remote_printed = String::from_utf8_lossy(&remote_get.stdout);
    assert!(
        !remote_printed.contains(&token),
        "token must not appear in remote URL: {remote_printed}"
    );
    assert!(
        remote_printed.contains(&format!("{}:{port}/", git_cli::monoengine_reachable_host())),
        "unexpected remote URL: {remote_printed}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );

    eprintln!(
        "integration_git_cli ok; monoengine binary={}",
        env!("CARGO_BIN_EXE_monoengine")
    );
}

/// Host-side git for TP-14 e2e. The compose git-cli runner reaches the host via
/// `host.docker.internal`; on this Linux bridge that path can be firewalled.
/// The product assertions (clone /project/foo, parent merge, pull) are the same.
fn descendant_host_git(env: &GitCliEnv, token: &str, git_args: &[&str]) -> std::process::Output {
    git_cli::write_git_askpass(&env.case_dir.join("git-askpass.sh"));
    let isolated_home = env.case_dir.join("git-home");
    fs::create_dir_all(&isolated_home).expect("git home");
    let null_config = PathBuf::from("/dev/null");
    let mut command = Command::new("git");
    command
        .current_dir(&env.case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("HOME", &isolated_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env(git_cli::GIT_ASKPASS_ENV, token)
        .env("GIT_ASKPASS", env.case_dir.join("git-askpass.sh"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "credential.username")
        .env("GIT_CONFIG_VALUE_0", git_cli::DEFAULT_GIT_AUTH_USER)
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_2", "false")
        .args(git_args);
    let output = command.output().expect("host git");
    git_cli::assert_git_success(&output, &format!("git {}", git_args.join(" ")));
    output
}

#[test]
fn integration_git_cli_descendant_ref_continuation_after_parent_merge() {
    // TP-14: clone /project/foo, local commit, merge at /project, then pull.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let root_url = git_cli::monoengine_host_http_url(port, "/");
    descendant_host_git(&env, &token, &["clone", &root_url, "seed"]);
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        descendant_host_git(&env, &token, &["-C", "seed", "config", key, value]);
    }
    let seed = env.case_dir.join("seed");
    fs::create_dir_all(seed.join("project/foo")).expect("mkdir project/foo");
    fs::write(seed.join("project/foo/from-a.txt"), b"a-base\n").expect("write foo file");
    descendant_host_git(
        &env,
        &token,
        &["-C", "seed", "add", "project/foo/from-a.txt"],
    );
    descendant_host_git(
        &env,
        &token,
        &["-C", "seed", "commit", "-m", "tp14 seed project/foo"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "seed",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            "HEAD:refs/heads/tp14-seed",
        ],
    );
    let seed_cl = latest_cl_link_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        merge_cl_no_auth(port, &seed_cl),
        200,
        "seed merge must succeed; stderr:\n{}",
        read_log(&stderr_path)
    );

    let foo_url = git_cli::monoengine_host_http_url(port, "/project/foo");
    descendant_host_git(&env, &token, &["clone", &foo_url, "clone-foo"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "clone-foo", "config", "user.name", "IT Git CLI"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "clone-foo",
            "config",
            "user.email",
            "it-git-cli@example.invalid",
        ],
    );
    fs::write(
        env.case_dir.join("clone-foo").join("local.txt"),
        b"a-local\n",
    )
    .expect("write local commit");
    descendant_host_git(&env, &token, &["-C", "clone-foo", "add", "local.txt"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "clone-foo", "commit", "-m", "tp14 local on foo"],
    );

    let project_url = git_cli::monoengine_host_http_url(port, "/project");
    descendant_host_git(&env, &token, &["clone", &project_url, "clone-project"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "clone-project", "config", "user.name", "IT Git CLI"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "clone-project",
            "config",
            "user.email",
            "it-git-cli@example.invalid",
        ],
    );
    fs::write(
        env.case_dir.join("clone-project").join("from-b.txt"),
        b"b-sibling\n",
    )
    .expect("write sibling");
    descendant_host_git(&env, &token, &["-C", "clone-project", "add", "from-b.txt"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "clone-project", "commit", "-m", "tp14 parent push"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "clone-project",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            "HEAD:refs/heads/tp14-parent",
        ],
    );
    let parent_cl = latest_cl_link_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        merge_cl_no_auth(port, &parent_cl),
        200,
        "parent merge must succeed; stderr:\n{}",
        read_log(&stderr_path)
    );

    let foo_row = ref_commit_tree(&env.database.db_url, "/project/foo", "refs/heads/main");
    assert!(
        foo_row.is_some(),
        "main@/project/foo must survive parent merge"
    );

    descendant_host_git(
        &env,
        &token,
        &["-C", "clone-foo", "pull", "--no-rebase", "origin", "main"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "clone-foo",
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            "HEAD:refs/heads/tp14-continue",
        ],
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_http_pull_cl_ref_round_trip() {
    // ADR-GM-02 / plan-20260803 GM-02: literal `git pull` of refs/cl/* must
    // update a dedicated local branch to the sender tree; default main stays seed.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let case_id = format!(
        "gm02-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fixture_payload = format!(
        "monoengine gm-02 pull fixture pid={} case={}\n",
        std::process::id(),
        env.case_dir.display()
    );
    let fixture_rel = Path::new("gm-02-pull-fixture.txt");
    let fixture_host = env.case_dir.join("fixture").join(fixture_rel);
    fs::create_dir_all(fixture_host.parent().expect("fixture parent")).expect("mkdir fixture");
    fs::write(&fixture_host, &fixture_payload).expect("write fixture");

    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let sender_name = "pull-sender";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, sender_name]),
        "clone sender worktree",
    );
    let sender = env.case_dir.join(sender_name);
    let seed_tree = snapshot_workdir(&sender);
    let expected_seed = expected_init_monorepo_fixture();
    assert_eq!(
        seed_tree, expected_seed,
        "sender clone must match seeded monorepo fixture before pull case mutates a CL tip"
    );

    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", sender_name, "checkout", "-b", &case_id],
        ),
        "create sender branch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", sender_name, "config", "user.name", "IT Git CLI"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                sender_name,
                "config",
                "user.email",
                "it-git-cli@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::copy(&fixture_host, sender.join(fixture_rel)).expect("copy fixture into sender");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", sender_name, "add", fixture_rel.to_str().unwrap()],
        ),
        "git add pull fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", sender_name, "commit", "-m", "gm-02 pull fixture"],
        ),
        "git commit pull fixture",
    );
    let sender_tree = snapshot_workdir(&sender);
    let fixture_key = path_bytes(fixture_rel);
    let fixture_bytes = fixture_payload.as_bytes().to_vec();
    assert_eq!(
        sender_tree.get(&fixture_key),
        Some(&fixture_bytes),
        "sender tree must contain the pull fixture bytes"
    );

    let before_cl = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    let refspec = format!("HEAD:refs/heads/{case_id}");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                sender_name,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &refspec,
            ],
        ),
        "push sender tip to create refs/cl/*",
    );
    let after_cl = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    let cl_ref = after_cl
        .into_iter()
        .find(|r| !before_cl.contains(r))
        .unwrap_or_else(|| panic!("expected new refs/cl/* after push for case {case_id}"));
    assert!(
        cl_ref.starts_with("refs/cl/"),
        "expected refs/cl/* tip, got {cl_ref}"
    );

    let puller_name = "pull-receiver";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, puller_name]),
        "clone default tip for literal pull",
    );
    let puller = env.case_dir.join(puller_name);
    let puller_main_before = snapshot_workdir(&puller);
    assert_eq!(
        puller_main_before, expected_seed,
        "pull receiver default main must equal seed tree before literal pull"
    );

    let local_branch = format!("pulled-{case_id}");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", puller_name, "checkout", "-b", &local_branch],
        ),
        "create local branch for literal pull",
    );
    // Literal `git pull origin <refs/cl/…>` (ADR-GM-02). Not fetch+checkout.
    // Force protocol v0: v2 ref-prefix from default heads-only remote.fetch can
    // omit refs/cl/* and yield HTTP 400 / "expected 'acknowledgments'".
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                puller_name,
                "-c",
                "protocol.version=0",
                "pull",
                "--ff-only",
                "origin",
                &cl_ref,
            ],
        ),
        "literal git pull of refs/cl tip into local branch",
    );

    let pulled_tree = snapshot_workdir(&puller);
    assert_eq!(
        pulled_tree.get(&fixture_key),
        Some(&fixture_bytes),
        "literal pull worktree must contain sender fixture bytes"
    );
    assert_eq!(
        pulled_tree, sender_tree,
        "literal pull worktree must match sender snapshot byte-for-byte"
    );

    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", puller_name, "checkout", "main"],
        ),
        "return to default main after pull",
    );
    let puller_main_after = snapshot_workdir(&puller);
    assert_eq!(
        puller_main_after, expected_seed,
        "default main must remain the seed tree after literal CL pull"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_auth_anonymous_disabled_rejects_clone() {
    // plan-20260803 GM-03: per-case `[git] anonymous_access = false` must reject
    // unauthenticated clone while the same service still accepts token clone.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::with_git_anonymous_access(false);
    let config_text = fs::read_to_string(&env.full_config_path).expect("read per-case config");
    assert!(
        config_text.contains("[git]"),
        "per-case config must contain [git] override:\n{config_text}"
    );
    assert!(
        config_text
            .lines()
            .any(|l| l.trim() == "anonymous_access = false"),
        "per-case config must set anonymous_access = false:\n{config_text}"
    );

    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let anon_clone = git_cli::git_cli_no_auth(
        &env.case_dir,
        &["clone", &remote_url, "anon-disabled-clone"],
    );
    assert!(
        !anon_clone.status.success(),
        "anonymous clone must fail when anonymous_access=false; status={:?}\nstdout:\n{}\nstderr:\n{}",
        anon_clone.status,
        String::from_utf8_lossy(&anon_clone.stdout),
        String::from_utf8_lossy(&anon_clone.stderr)
    );
    let anon_err = format!(
        "{}{}",
        String::from_utf8_lossy(&anon_clone.stdout),
        String::from_utf8_lossy(&anon_clone.stderr)
    )
    .to_ascii_lowercase();
    assert!(
        anon_err.contains("authentication")
            || anon_err.contains("401")
            || anon_err.contains("unauthorized")
            || anon_err.contains("auth"),
        "anonymous failure must look like an auth rejection, got:\n{anon_err}"
    );

    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["clone", &remote_url, "token-enabled-clone"],
        ),
        "token clone must succeed on same service with anonymous_access=false",
    );
    let token_tree = snapshot_workdir(&env.case_dir.join("token-enabled-clone"));
    assert_eq!(
        token_tree,
        expected_init_monorepo_fixture(),
        "authenticated clone must still receive the seeded monorepo tree"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_http_rejects_git_client_tag_push() {
    // docs/monorepo.md §2 — Monorepo tags are Web/API only.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "tag-reject-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone before tag reject",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "config", "user.name", "IT Git CLI"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "it-git-cli@example.invalid",
            ],
        ),
        "git user.email",
    );

    let tag = format!("it-tag-reject-{}", std::process::id());
    let tag_file = format!("{tag}.txt");
    fs::write(
        env.case_dir.join(clone_name).join(&tag_file),
        b"tag reject\n",
    )
    .expect("write tag fixture");
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["-C", clone_name, "add", &tag_file]),
        "git add tag fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "tag reject fixture"],
        ),
        "git commit tag fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["-C", clone_name, "tag", &tag]),
        "git tag local",
    );

    let push = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("refs/tags/{tag}"),
        ],
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        !push.status.success(),
        "Monorepo must reject Git-client tag push; status={:?}\n{combined}",
        push.status
    );
    assert!(
        combined.contains("tag pushes are not supported"),
        "tag-only ng must happen before persist; got:\n{combined}"
    );
    let remote_tags = ls_remote_refs(
        &env.case_dir,
        &token,
        &remote_url,
        &format!("refs/tags/{tag}"),
    );
    assert!(
        remote_tags.is_empty(),
        "rejected tag push must not leave refs/tags/{tag} on remote: {remote_tags:?}",
        tag = tag,
        remote_tags = remote_tags
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_http_packless_missing_commit_is_ng() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let missing = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let ref_name = format!("refs/heads/fc08-missing-{}", std::process::id());
    let report = post_packless_receive_pack(port, &token, ZERO_SHA1, missing, &ref_name);
    assert!(
        report.contains("target object") && report.contains(missing),
        "pack-less missing commit must ng with target-object reason; got:\n{report}"
    );
    assert!(
        cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER).is_empty(),
        "pack-less missing commit must not persist a CL"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_http_packless_existing_commit_skips_unpack() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let head = git_stdout(&env.case_dir, &token, &["ls-remote", &remote_url, "HEAD"])
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("ls-remote HEAD produced no sha"))
        .to_string();
    let ref_name = format!("refs/heads/fc08-packless-{}", std::process::id());
    let report = post_packless_receive_pack(port, &token, ZERO_SHA1, &head, &ref_name);
    assert!(
        report.contains("unpack ok") && report.contains(&format!("ok {ref_name}")),
        "pack-less existing commit must skip unpack and report ok; got:\n{report}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_auth_push_without_token_returns_401_challenge() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);

    // Challenge probe: no Authorization header → 401 + WWW-Authenticate.
    let headers = git_cli::probe_receive_pack_challenge(port);
    assert!(
        headers.lines().next().is_some_and(|l| l.contains("401")),
        "expected HTTP 401 for unauthenticated receive-pack info/refs, got:\n{headers}"
    );
    assert!(
        headers.to_ascii_lowercase().contains("www-authenticate:"),
        "expected WWW-Authenticate challenge header, got:\n{headers}"
    );

    // Real client path: anonymous clone (upload-pack) then unauthenticated push fails.
    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "auth-unauthed-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(&env.case_dir, &["clone", &remote_url, clone_name]),
        "anonymous HTTP clone before unauthenticated push",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &["-C", clone_name, "config", "user.name", "IT Auth"],
        ),
        "user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "it-auth@example.invalid",
            ],
        ),
        "user.email",
    );
    let branch = format!("it-10-unauth-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create unauth branch",
    );
    fs::write(
        env.case_dir.join(clone_name).join("it-10-unauth.txt"),
        b"unauthenticated push must fail\n",
    )
    .expect("write unauth file");
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &["-C", clone_name, "add", "it-10-unauth.txt"],
        ),
        "git add",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &["-C", clone_name, "commit", "-m", "it-10 unauth"],
        ),
        "git commit",
    );

    let push = git_cli::git_cli_no_auth(
        &env.case_dir,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{branch}"),
        ],
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        !push.status.success(),
        "unauthenticated push must fail with non-zero exit (status={:?} code={:?}):\n{combined}",
        push.status,
        push.status.code()
    );
    assert!(
        combined.contains("401")
            || combined.to_ascii_lowercase().contains("authentication")
            || combined.to_ascii_lowercase().contains("unauthorized"),
        "unauthenticated push stderr/stdout must mention auth failure, got:\n{combined}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

fn probe_receive_pack_headers(port: u16, repo_path: &str, bearer: Option<&str>) -> String {
    let suffix = "/info/refs?service=git-receive-pack";
    let path = if repo_path == "/" {
        suffix.to_string()
    } else {
        format!("{}{suffix}", repo_path.trim_end_matches('/'))
    };
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut command = Command::new("curl");
    command.args(["-sS", "-D", "-", "-o", "/dev/null", "--max-time", "15"]);
    if let Some(token) = bearer {
        command
            .arg("-H")
            .arg(format!("Authorization: Bearer {token}"));
    }
    command.arg(&url);
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("curl receive-pack probe failed to spawn: {err}"));
    assert!(
        output.status.success(),
        "curl probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn pkt_line(payload: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    out
}

fn post_packless_receive_pack(
    port: u16,
    token: &str,
    old_id: &str,
    new_id: &str,
    ref_name: &str,
) -> String {
    let mut line = format!("{old_id} {new_id} {ref_name}");
    line.push('\0');
    line.push_str("report-status\n");
    let mut body = pkt_line(&line);
    body.extend_from_slice(b"0000");

    let url = format!("http://127.0.0.1:{port}/git-receive-pack");
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            "-H",
            "Content-Type: application/x-git-receive-pack-request",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "--data-binary",
            "@-",
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn curl receive-pack: {err}"));
    {
        let stdin = child.stdin.as_mut().expect("curl stdin");
        stdin
            .write_all(&body)
            .unwrap_or_else(|err| panic!("write receive-pack body: {err}"));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("curl receive-pack: {err}"));
    assert!(
        output.status.success(),
        "curl receive-pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn integration_git_cli_push_auth_token_rejects_without_token_and_out_of_path() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("ci.token");
    let token = format!("tp19-ci-{}", std::process::id());
    fs::write(&token_path, format!("{token}\n")).expect("write file-mounted token");
    let append = format!(
        r#"
[git]
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "ci"
token = "${{file:{}}}"
paths = ["/project/foo"]
"#,
        token_path.display()
    );
    let env = GitCliEnv::with_config_append(&append);
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_with_env(&env, None, None, &[("MEGA_MONOREPO__PUSH_POLICY", "trunk")]);

    let unauth = probe_receive_pack_headers(port, "/", None);
    assert!(
        unauth.lines().next().is_some_and(|l| l.contains("401")),
        "push_auth=token must 401 without a token, got:\n{unauth}"
    );
    assert!(
        unauth.to_ascii_lowercase().contains("www-authenticate:"),
        "401 must include WWW-Authenticate, got:\n{unauth}"
    );

    let sibling = probe_receive_pack_headers(port, "/project/foobar", Some(&token));
    assert!(
        sibling.lines().next().is_some_and(|l| l.contains("403")),
        "token authorized for /project/foo must not authorize /project/foobar, got:\n{sibling}"
    );

    let allowed = probe_receive_pack_headers(port, "/project/foo", Some(&token));
    let status_line = allowed.lines().next().unwrap_or_default();
    assert!(
        !status_line.contains("401") && !status_line.contains("403"),
        "token must pass auth for /project/foo, got:\n{allowed}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    drop(token_dir);
}

fn trunk_subpath_url(port: u16, path: &str) -> String {
    format!(
        "{}/",
        git_cli::monoengine_host_http_url(port, path).trim_end_matches('/')
    )
}

fn trunk_boot_env() -> [(&'static str, &'static str); 3] {
    [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
    ]
}

/// Host-side git for TP-17 e2e. The compose git-cli runner reaches the host via
/// `host.docker.internal`; on this Linux bridge that path is firewalled (same
/// as `descendant_host_git`).
fn trunk_host_git(case_dir: &Path, git_args: &[&str]) -> std::process::Output {
    let isolated_home = case_dir.join("git-home-trunk");
    fs::create_dir_all(&isolated_home).expect("trunk git home");
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
    command.output().expect("host git")
}

fn git_ok_no_auth(case_dir: &Path, args: &[&str]) {
    git_cli::assert_git_success(
        &trunk_host_git(case_dir, args),
        &format!("git {}", args.join(" ")),
    );
}

fn git_stdout_no_auth(case_dir: &Path, args: &[&str]) -> String {
    let output = trunk_host_git(case_dir, args);
    git_cli::assert_git_success(&output, &format!("git {}", args.join(" ")));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn trunk_push(case_dir: &Path, repo: &str) -> std::process::Output {
    trunk_host_git(
        case_dir,
        &[
            "-C",
            repo,
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    )
}

fn configure_git_identity_no_auth(case_dir: &Path, clone: &str) {
    git_ok_no_auth(case_dir, &["-C", clone, "config", "user.name", "IT Trunk"]);
    git_ok_no_auth(
        case_dir,
        &[
            "-C",
            clone,
            "config",
            "user.email",
            "it-trunk@example.invalid",
        ],
    );
}

fn probe_upload_pack_body(port: u16, repo_path: &str) -> (String, String) {
    let suffix = "/info/refs?service=git-upload-pack";
    let path = if repo_path == "/" {
        suffix.to_string()
    } else {
        format!("{}{suffix}", repo_path.trim_end_matches('/'))
    };
    let url = format!("http://127.0.0.1:{port}{path}");
    let headers_path = std::env::temp_dir().join(format!("tp17-headers-{port}"));
    let output = Command::new("curl")
        .args([
            "-sS",
            "-D",
            headers_path.to_str().expect("utf8"),
            "--max-time",
            "15",
        ])
        .arg(&url)
        .output()
        .unwrap_or_else(|err| panic!("curl upload-pack probe failed to spawn: {err}"));
    assert!(
        output.status.success(),
        "curl upload-pack probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let headers = fs::read_to_string(&headers_path).unwrap_or_default();
    let _ = fs::remove_file(&headers_path);
    let body = String::from_utf8_lossy(&output.stdout).into_owned();
    (headers, body)
}

fn assert_upload_pack_advertises_main(port: u16, repo_path: &str) {
    let (headers, body) = probe_upload_pack_body(port, repo_path);
    assert!(
        headers.lines().next().is_some_and(|l| l.contains("200")),
        "trunk {repo_path} upload-pack must 200, got:\n{headers}\nbody:\n{body:?}"
    );
    assert!(
        body.contains("refs/heads/main") || body.contains("refs/heads/master"),
        "trunk {repo_path} advertise must list main, headers:\n{headers}\nbody:\n{body:?}"
    );
}

/// Create `foo/` under `/project` so `/project/foo` can be cloned (B0 rejects `/`).
fn seed_project_foo(case_dir: &Path, port: u16) {
    assert_upload_pack_advertises_main(port, "/");
    assert_upload_pack_advertises_main(port, "/project");
    let project_url = trunk_subpath_url(port, "/project");
    git_ok_no_auth(case_dir, &["clone", &project_url, "project-seed"]);
    configure_git_identity_no_auth(case_dir, "project-seed");
    let foo_file = case_dir.join("project-seed").join("foo").join("seed.txt");
    fs::create_dir_all(foo_file.parent().expect("foo parent")).expect("mkdir foo");
    fs::write(&foo_file, "seed\n").expect("write foo/seed.txt");
    git_ok_no_auth(case_dir, &["-C", "project-seed", "add", "foo/seed.txt"]);
    git_ok_no_auth(
        case_dir,
        &["-C", "project-seed", "commit", "-m", "seed /project/foo"],
    );
    git_ok_no_auth(
        case_dir,
        &[
            "-C",
            "project-seed",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
}

fn ls_remote_main(case_dir: &Path, remote_url: &str) -> String {
    let output = trunk_host_git(case_dir, &["ls-remote", remote_url, "refs/heads/main"]);
    git_cli::assert_git_success(&output, "ls-remote refs/heads/main");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("")
        .to_string()
}

fn push_queue_requesters(db_url: &str) -> Vec<Option<String>> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for push_queue: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT requester FROM push_queue WHERE kind::text = 'push' ORDER BY id"
                    .to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("query push_queue requesters: {err}"));
        rows.iter()
            .map(|row| row.try_get("", "requester").ok())
            .collect()
    })
}

fn count_cl_artifacts(db_url: &str) -> (i64, i64) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for CL count: {err}"));
        let cl = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS n FROM mega_cl".to_string(),
            ))
            .await
            .expect("count mega_cl")
            .expect("mega_cl count row");
        let refs = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS n FROM mega_refs WHERE is_cl".to_string(),
            ))
            .await
            .expect("count mega_refs")
            .expect("mega_refs count row");
        (
            cl.try_get("", "n").expect("n"),
            refs.try_get("", "n").expect("n"),
        )
    })
}

#[test]
fn integration_git_cli_trunk_n1_identity_three_ff_and_no_cl_refs() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let extra = trunk_boot_env();
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_with_env(&env, None, None, &extra);

    seed_project_foo(&env.case_dir, port);
    let foo_url = trunk_subpath_url(port, "/project/foo");
    git_ok_no_auth(&env.case_dir, &["clone", &foo_url, "foo"]);
    configure_git_identity_no_auth(&env.case_dir, "foo");

    for round in 1..=3 {
        let rel = format!("round-{round}.txt");
        fs::write(
            env.case_dir.join("foo").join(&rel),
            format!("trunk n1 round {round}\n"),
        )
        .expect("write round file");
        git_ok_no_auth(&env.case_dir, &["-C", "foo", "add", &rel]);
        git_ok_no_auth(
            &env.case_dir,
            &["-C", "foo", "commit", "-m", &format!("n1 round {round}")],
        );
        let pretty = git_stdout_no_auth(&env.case_dir, &["-C", "foo", "cat-file", "-p", "HEAD"]);
        let head = git_stdout_no_auth(&env.case_dir, &["-C", "foo", "rev-parse", "HEAD"]);
        git_cli::assert_git_success(&trunk_push(&env.case_dir, "foo"), "n1 trunk push");
        let remote = ls_remote_main(&env.case_dir, &foo_url);
        assert_eq!(
            remote, head,
            "round {round}: ls-remote main@/project/foo must equal the client commit"
        );
        git_ok_no_auth(&env.case_dir, &["-C", "foo", "fetch", "origin"]);
        git_ok_no_auth(
            &env.case_dir,
            &["-C", "foo", "reset", "--hard", "origin/main"],
        );
        let after = git_stdout_no_auth(&env.case_dir, &["-C", "foo", "rev-parse", "HEAD"]);
        assert_eq!(after, head, "round {round}: fetch+reset is a no-op");
        let pretty_after =
            git_stdout_no_auth(&env.case_dir, &["-C", "foo", "cat-file", "-p", "HEAD"]);
        assert_eq!(
            pretty_after, pretty,
            "round {round}: author/time/message identical"
        );
    }

    let (cls, cl_refs) = count_cl_artifacts(&env.database.db_url);
    assert_eq!(cls, 0, "trunk pushes must not create mega_cl rows");
    assert_eq!(cl_refs, 0, "trunk pushes must not create refs/cl/*");
    let advertised = trunk_host_git(&env.case_dir, &["ls-remote", &foo_url, "refs/cl/*"]);
    git_cli::assert_git_success(&advertised, "ls-remote refs/cl/*");
    assert!(
        String::from_utf8_lossy(&advertised.stdout)
            .trim()
            .is_empty(),
        "trunk must not advertise CL refs:\n{}",
        String::from_utf8_lossy(&advertised.stdout)
    );
    let requesters = push_queue_requesters(&env.database.db_url);
    assert!(
        requesters.iter().any(|r| r.is_none()),
        "push_auth=none must record NULL requester: {requesters:?}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_trunk_n_gt1_squash_sideband_and_nff_align() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let extra = trunk_boot_env();
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_with_env(&env, None, None, &extra);

    seed_project_foo(&env.case_dir, port);
    let foo_url = trunk_subpath_url(port, "/project/foo");
    git_ok_no_auth(&env.case_dir, &["clone", &foo_url, "foo-batch"]);
    configure_git_identity_no_auth(&env.case_dir, "foo-batch");
    let pre_push = ls_remote_main(&env.case_dir, &foo_url);

    for i in 1..=3 {
        let rel = format!("batch-{i}.txt");
        fs::write(
            env.case_dir.join("foo-batch").join(&rel),
            format!("batch {i}\n"),
        )
        .expect("write batch file");
        git_ok_no_auth(&env.case_dir, &["-C", "foo-batch", "add", &rel]);
        git_ok_no_auth(
            &env.case_dir,
            &["-C", "foo-batch", "commit", "-m", &format!("batch {i}")],
        );
    }
    let tip = git_stdout_no_auth(&env.case_dir, &["-C", "foo-batch", "rev-parse", "HEAD"]);
    let tip_tree = git_stdout_no_auth(
        &env.case_dir,
        &["-C", "foo-batch", "rev-parse", "HEAD^{tree}"],
    );
    let push = trunk_push(&env.case_dir, "foo-batch");
    git_cli::assert_git_success(&push, "3-commit trunk push");
    let remote_out = format!(
        "{}{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    let landed = ls_remote_main(&env.case_dir, &foo_url);
    assert_ne!(landed, tip, "N>1 must squash, not keep the client tip");
    assert!(
        remote_out.contains(&landed),
        "sideband must name the squash id {landed}:\n{remote_out}"
    );
    assert!(
        remote_out.contains("git fetch && git reset --hard origin/main"),
        "ADR-TP-18 squash notice missing:\n{remote_out}"
    );

    git_ok_no_auth(
        &env.case_dir,
        &[
            "-C",
            "foo-batch",
            "commit",
            "--allow-empty",
            "-m",
            "unaligned",
        ],
    );
    // After squash, advertise is a new SHA the client does not have, so Git
    // refuses with "fetch first" unless forced. --force still sends the update;
    // B3 must reject it as non-fast-forward (ADR-TP-18).
    let repush = trunk_host_git(
        &env.case_dir,
        &[
            "-C",
            "foo-batch",
            "push",
            "--no-thin",
            "--force",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    assert!(
        !repush.status.success(),
        "unaligned re-push must be rejected"
    );
    let nff = format!(
        "{}{}",
        String::from_utf8_lossy(&repush.stdout),
        String::from_utf8_lossy(&repush.stderr)
    );
    assert!(
        nff.contains("non-fast-forward") || nff.contains("push chain is broken"),
        "unaligned re-push must be rejected as NFF or broken chain:\n{nff}"
    );
    assert!(
        nff.contains("git fetch && git reset --hard origin/main"),
        "NFF must include the align command:\n{nff}"
    );

    git_ok_no_auth(&env.case_dir, &["-C", "foo-batch", "fetch", "origin"]);
    let origin_tree = git_stdout_no_auth(
        &env.case_dir,
        &["-C", "foo-batch", "rev-parse", "origin/main^{tree}"],
    );
    assert_eq!(origin_tree, tip_tree, "squash tree equals client tip tree");
    let origin_parent = git_stdout_no_auth(
        &env.case_dir,
        &["-C", "foo-batch", "rev-parse", "origin/main^"],
    );
    assert_eq!(origin_parent, pre_push, "squash parent is the pre-push tip");

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_trunk_requester_token_name() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("ci.token");
    let token = format!("tp17-ci-{}", std::process::id());
    fs::write(&token_path, format!("{token}\n")).expect("write file-mounted token");
    let append = format!(
        r#"
[git]
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "ci"
token = "${{file:{}}}"
paths = ["/project"]
"#,
        token_path.display()
    );
    let env = GitCliEnv::with_config_append(&append);
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_with_env(&env, None, None, &[("MEGA_MONOREPO__PUSH_POLICY", "trunk")]);

    let project_url = trunk_subpath_url(port, "/project");
    descendant_host_git(&env, &token, &["clone", &project_url, "project-seed"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "project-seed", "config", "user.name", "Not The Token"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "project-seed",
            "config",
            "user.email",
            "author@example.invalid",
        ],
    );
    let foo_file = env
        .case_dir
        .join("project-seed")
        .join("foo")
        .join("seed.txt");
    fs::create_dir_all(foo_file.parent().expect("foo parent")).expect("mkdir foo");
    fs::write(&foo_file, "seed\n").expect("write foo/seed.txt");
    descendant_host_git(&env, &token, &["-C", "project-seed", "add", "foo/seed.txt"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "project-seed", "commit", "-m", "seed /project/foo"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "project-seed",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );

    let foo_url = trunk_subpath_url(port, "/project/foo");
    descendant_host_git(&env, &token, &["clone", &foo_url, "foo"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "foo", "config", "user.name", "Not The Token"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "foo",
            "config",
            "user.email",
            "author@example.invalid",
        ],
    );
    fs::write(env.case_dir.join("foo").join("n1.txt"), "n1\n").expect("write n1");
    descendant_host_git(&env, &token, &["-C", "foo", "add", "n1.txt"]);
    descendant_host_git(
        &env,
        &token,
        &["-C", "foo", "commit", "-m", "n1 from mismatched author"],
    );
    descendant_host_git(
        &env,
        &token,
        &[
            "-C",
            "foo",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    let requesters = push_queue_requesters(&env.database.db_url);
    assert!(
        requesters.iter().all(|r| r.as_deref() == Some("ci")),
        "token mode must record token name, not commit author: {requesters:?}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    drop(token_dir);
}

#[test]
fn integration_git_cli_auth_token_never_leaks() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "auth-leak-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "authenticated clone for leak audit",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "config", "user.name", "IT Leak"],
        ),
        "user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "it-leak@example.invalid",
            ],
        ),
        "user.email",
    );
    let branch = format!("it-10-leak-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create leak-audit branch",
    );
    fs::write(
        env.case_dir.join(clone_name).join("it-10-leak.txt"),
        b"token must stay out of remotes and workdir files\n",
    )
    .expect("write leak fixture");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "it-10-leak.txt"],
        ),
        "git add",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "it-10 leak audit"],
        ),
        "git commit",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        ),
        "authenticated push for leak audit",
    );

    let remote_get = git_cli::git_cli(
        &env.case_dir,
        &token,
        &["-C", clone_name, "remote", "get-url", "origin"],
    );
    git_cli::assert_git_success(&remote_get, "remote get-url");
    let remote_printed = String::from_utf8_lossy(&remote_get.stdout);
    assert!(
        !remote_printed.contains(&token),
        "token must not appear in remote URL: {remote_printed}"
    );

    // Scan every .git/config under this case dir (VER also scans the workdir root).
    let mut configs = Vec::new();
    collect_git_configs(&env.case_dir, &mut configs);
    assert!(
        !configs.is_empty(),
        "expected at least one .git/config under {}",
        env.case_dir.display()
    );
    for cfg in &configs {
        let text = fs::read_to_string(cfg).unwrap_or_else(|err| {
            panic!("read {}: {err}", cfg.display());
        });
        assert!(
            !text.contains(&token),
            "token leaked into {}",
            cfg.display()
        );
    }

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_authz_revoke_grant_immediate_effect() {
    // UN-16: 撤权/授权即时生效 e2e. The shared authz snapshot is rebuilt from
    // main's `/.mega_cedar.json` via `notify_authz_changed` after a merge
    // funnel commit (`apply_update_result`). Under `enforce`, a non-admin push
    // is denied; after the admin merges a grant change, the same push is
    // allowed; after the admin merges a revoke change, it is denied again —
    // all without a service restart (the snapshot is swapped in-process).
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let admin_token = git_cli::resolve_seed_token();
    let user_token = format!(
        "it-git-token-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    // The ACL-change merges below go through `merge-no-auth`, which since
    // UN-24 is authorized like any other merge entry point. The stub lets this
    // test present the admin's session for those calls; the pushes it asserts
    // on stay anonymous-to-the-website (they authenticate with a git token).
    let session_stub_port = spawn_website_session_stub("benjamin_747");
    let (mut service, port, stdout_path, stderr_path) =
        boot_service_http_with_enforcement_and_session(
            &env,
            Some("enforce"),
            Some(session_stub_port),
        );

    // Migrations + access_token table exist only after service bootstrap.
    git_cli::seed_access_token(&env.database.db_url, "benjamin_747", &admin_token);
    git_cli::seed_access_token(
        &env.database.db_url,
        git_cli::DEFAULT_GIT_AUTH_USER,
        &user_token,
    );

    let remote_url = git_cli::monoengine_http_repo_url(port);

    // --- baseline: non-admin push denied under enforce ---
    let clone_name = "un16-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["clone", &remote_url, clone_name],
        ),
        "clone as non-admin",
    );
    let clone = env.case_dir.join(clone_name);
    let baseline_branch = format!("un16-baseline-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "checkout", "-b", &baseline_branch],
        ),
        "create baseline branch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "config", "user.name", "IT Git CLI"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "it-git-cli@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::write(
        clone.join("un16-baseline.txt"),
        b"baseline push must fail\n",
    )
    .expect("write marker");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "add", "un16-baseline.txt"],
        ),
        "git add baseline marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "commit", "-m", "un16 baseline"],
        ),
        "git commit baseline marker",
    );
    let baseline_push = git_cli::git_cli(
        &env.case_dir,
        &user_token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{baseline_branch}"),
        ],
    );
    assert!(
        !baseline_push.status.success(),
        "non-admin push must be denied under enforce; status={:?}\nstdout:\n{}\nstderr:\n{}",
        baseline_push.status,
        String::from_utf8_lossy(&baseline_push.stdout),
        String::from_utf8_lossy(&baseline_push.stderr)
    );

    // --- grant: admin merges a change adding it-git-cli as admin ---
    let admin_clone = "un16-admin-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["clone", &remote_url, admin_clone],
        ),
        "clone as admin",
    );
    let admin_dir = env.case_dir.join(admin_clone);
    let grant_json = authz_json_with_admins(&["benjamin_747", git_cli::DEFAULT_GIT_AUTH_USER]);
    fs::write(admin_dir.join(".mega_cedar.json"), &grant_json).expect("write grant authz json");
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone, "config", "user.name", "Admin"],
        ),
        "admin git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone,
                "config",
                "user.email",
                "admin@example.invalid",
            ],
        ),
        "admin git user.email",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone, "add", ".mega_cedar.json"],
        ),
        "admin git add authz json",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone,
                "commit",
                "-m",
                "un16 grant it-git-cli admin",
            ],
        ),
        "admin git commit grant",
    );
    let grant_branch = format!("un16-grant-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &format!("HEAD:refs/heads/{grant_branch}"),
            ],
        ),
        "admin push grant change",
    );
    let grant_cl_link = latest_cl_link_for_user(&env.database.db_url, "benjamin_747");
    let merge_status = merge_cl_no_auth(port, &grant_cl_link);
    assert_eq!(merge_status, 200, "grant merge must succeed");

    // --- grant takes effect immediately: non-admin push now allowed ---
    // Fetch the merged main first so the pushed commit's parent is known to the
    // server (a stale clone would otherwise send the parent commit too, which
    // the single-commit-per-push Monorepo rule rejects).
    let fetch_grant = git_cli::git_cli(
        &env.case_dir,
        &user_token,
        &["-C", clone_name, "fetch", "origin"],
    );
    assert!(
        fetch_grant.status.success(),
        "fetch new main after grant merge failed; status={:?}\nstdout:\n{}\nstderr:\n{}\nservice.out:\n{}\nservice.err:\n{}",
        fetch_grant.status,
        String::from_utf8_lossy(&fetch_grant.stdout),
        String::from_utf8_lossy(&fetch_grant.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    let granted_branch = format!("un16-granted-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &[
                "-C",
                clone_name,
                "checkout",
                "-b",
                &granted_branch,
                "origin/main",
            ],
        ),
        "checkout granted branch from origin/main",
    );
    fs::write(
        clone.join("un16-granted.txt"),
        b"granted push must succeed\n",
    )
    .expect("write granted marker");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "add", "un16-granted.txt"],
        ),
        "git add granted marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "commit", "-m", "un16 granted push"],
        ),
        "git commit granted marker",
    );
    let granted_push = git_cli::git_cli(
        &env.case_dir,
        &user_token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{granted_branch}"),
        ],
    );
    assert!(
        granted_push.status.success(),
        "granted push must succeed after merge; status={:?}\nstdout:\n{}\nstderr:\n{}\nservice.out:\n{}\nservice.err:\n{}",
        granted_push.status,
        String::from_utf8_lossy(&granted_push.stdout),
        String::from_utf8_lossy(&granted_push.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );

    // --- revoke: admin merges a change removing it-git-cli from admin ---
    let admin_clone2 = "un16-admin-clone2";
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["clone", &remote_url, admin_clone2],
        ),
        "clone as admin (revoke)",
    );
    let admin_dir2 = env.case_dir.join(admin_clone2);
    let revoke_json = authz_json_with_admins(&["benjamin_747"]);
    fs::write(admin_dir2.join(".mega_cedar.json"), &revoke_json).expect("write revoke authz json");
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone2, "config", "user.name", "Admin"],
        ),
        "admin2 git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone2,
                "config",
                "user.email",
                "admin@example.invalid",
            ],
        ),
        "admin2 git user.email",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone2, "add", ".mega_cedar.json"],
        ),
        "admin2 git add authz json",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone2,
                "commit",
                "-m",
                "un16 revoke it-git-cli admin",
            ],
        ),
        "admin2 git commit revoke",
    );
    let revoke_branch = format!("un16-revoke-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone2,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &format!("HEAD:refs/heads/{revoke_branch}"),
            ],
        ),
        "admin push revoke change",
    );
    let revoke_cl_link = latest_cl_link_for_user(&env.database.db_url, "benjamin_747");
    let merge_status = merge_cl_no_auth(port, &revoke_cl_link);
    assert_eq!(merge_status, 200, "revoke merge must succeed");

    // --- revoke takes effect immediately: non-admin push denied again ---
    // Fetch the merged main first (same stale-clone rationale as the grant push).
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "fetch", "origin"],
        ),
        "fetch new main after revoke merge",
    );
    let revoked_branch = format!("un16-revoked-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &[
                "-C",
                clone_name,
                "checkout",
                "-b",
                &revoked_branch,
                "origin/main",
            ],
        ),
        "checkout revoked branch from origin/main",
    );
    fs::write(
        clone.join("un16-revoked.txt"),
        b"revoked push must be denied\n",
    )
    .expect("write revoked marker");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "add", "un16-revoked.txt"],
        ),
        "git add revoked marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &user_token,
            &["-C", clone_name, "commit", "-m", "un16 revoked push"],
        ),
        "git commit revoked marker",
    );
    let revoked_push = git_cli::git_cli(
        &env.case_dir,
        &user_token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{revoked_branch}"),
        ],
    );
    assert!(
        !revoked_push.status.success(),
        "revoked push must be denied after merge; status={:?}\nstdout:\n{}\nstderr:\n{}",
        revoked_push.status,
        String::from_utf8_lossy(&revoked_push.stdout),
        String::from_utf8_lossy(&revoked_push.stderr)
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_acl_change_requires_admin_to_merge() {
    // UN-19: editing `/.mega_cedar.json` is how permissions are granted, so a
    // maintainer being able to merge such a change would be a self-promotion
    // path. Under `enforce` only an admin may merge it — end to end, through
    // the real merge entry point.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let admin_token = git_cli::resolve_seed_token();

    // One service, two subjects: the cookie decides which one the request is.
    const ADMIN_SESSION: &str = "un19-admin-session";
    const MAINTAINER_SESSION: &str = "un19-maintainer-session";
    let session_stub_port = spawn_website_session_stub_for(&[
        (ADMIN_SESSION, "benjamin_747"),
        (MAINTAINER_SESSION, git_cli::DEFAULT_GIT_AUTH_USER),
    ]);
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_with_enforcement_and_session(
            &env,
            Some("enforce"),
            Some(session_stub_port),
        );

    git_cli::seed_access_token(&env.database.db_url, "benjamin_747", &admin_token);
    let remote_url = git_cli::monoengine_http_repo_url(port);

    // The admin pushes a CL that grants the maintainer the maintainer role —
    // exactly the kind of change that must not be self-mergeable.
    let admin_clone = "un19-admin-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["clone", &remote_url, admin_clone],
        ),
        "clone as admin",
    );
    let admin_dir = env.case_dir.join(admin_clone);
    // Must differ from what init seeded, or there is no change to merge.
    fs::write(
        admin_dir.join(".mega_cedar.json"),
        authz_json_with_admins(&["benjamin_747", "un19-extra-admin"]),
    )
    .expect("write acl change");
    for (key, value) in [
        ("user.name", "Admin"),
        ("user.email", "admin@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli_as_user(
                &env.case_dir,
                "benjamin_747",
                &admin_token,
                &["-C", admin_clone, "config", key, value],
            ),
            "admin git config",
        );
    }
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone, "add", ".mega_cedar.json"],
        ),
        "admin git add acl",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["-C", admin_clone, "commit", "-m", "un19 acl change"],
        ),
        "admin git commit acl",
    );
    let branch = format!("un19-acl-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &[
                "-C",
                admin_clone,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        ),
        "admin push acl change",
    );
    let cl_link = latest_cl_link_for_user(&env.database.db_url, "benjamin_747");

    // A non-admin may not merge it, even though the CL itself is ordinary.
    let refused = merge_cl_no_auth_as(port, &cl_link, MAINTAINER_SESSION);
    assert_eq!(
        refused, 403,
        "a non-admin merging an ACL change must be refused (403), not merely fail: \
         503 would mean the check could not run, which is a different outcome"
    );

    // The admin may.
    let allowed = merge_cl_no_auth_as(port, &cl_link, ADMIN_SESSION);
    assert_eq!(
        allowed,
        200,
        "an admin merging the same CL must succeed; service.err:\n{}",
        read_log(&stderr_path)
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    drop(service);
    drop(env);
}

#[test]
fn integration_git_cli_rejects_main_branch_delete() {
    // UN-16: receive-pack Delete branch rejects deleting the main branch ref
    // (`refs/heads/main`). The shared authz snapshot is keyed on main's
    // `/.mega_cedar.json`; deleting main would leave the snapshot without a
    // source of truth. The error must be actionable for git clients. This is a
    // deliberate safety closure (not an enforcement gate), so it holds under
    // the default `off` enforcement too.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();

    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "un16-main-delete-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for main-delete rejection",
    );

    let delete = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            ":refs/heads/main",
        ],
    );
    assert!(
        !delete.status.success(),
        "deleting the main branch ref must be rejected; status={:?}\nstdout:\n{}\nstderr:\n{}",
        delete.status,
        String::from_utf8_lossy(&delete.stdout),
        String::from_utf8_lossy(&delete.stderr)
    );
    let stderr = String::from_utf8_lossy(&delete.stderr);
    assert!(
        stderr.contains("refusing to delete the main branch ref"),
        "main-delete rejection must be actionable for git clients; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("refs/heads/main"),
        "main-delete rejection must name the main ref; stderr:\n{stderr}"
    );

    // The main ref must still be present after the rejected delete.
    let heads = ls_remote_refs(&env.case_dir, &token, &remote_url, "refs/heads/main");
    assert!(
        heads.iter().any(|r| r == "refs/heads/main"),
        "main ref must survive the rejected delete; heads={heads:?}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_multicommit_chain_push_acceptance() {
    // plan-20260827 MC-06: a 3-commit linear chain push is admitted end to end
    // — exactly one CL with from = fork point / to = chain tip, CL ref fields
    // sourced from the tip, files introduced/renamed by non-first commits
    // indexed in mega_blob.file_path, and the merge advances refs/heads/main by
    // exactly one new single-parent commit (ADR-MC-01).
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "mc06-chain-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for multicommit chain push",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let base_head = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "origin/main"],
    );
    let branch = format!("mc06-chain-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create chain branch",
    );

    // c1 adds a root file, c2 adds a file in a new subdirectory, c3 renames the
    // c1 file — the c2/c3 changes are the non-first-commit indexing surface.
    let clone = env.case_dir.join(clone_name);
    fs::write(clone.join("mc06-fileA.txt"), b"mc06 chain file A\n").expect("write file A");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-fileA.txt"],
        ),
        "git add file A",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 c1 add fileA"],
        ),
        "commit c1",
    );
    fs::create_dir_all(clone.join("mc06-dir")).expect("mkdir mc06-dir");
    fs::write(
        clone.join("mc06-dir/mc06-fileB.txt"),
        b"mc06 chain file B\n",
    )
    .expect("write file B");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-dir/mc06-fileB.txt"],
        ),
        "git add file B",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 c2 add fileB"],
        ),
        "commit c2",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "mv",
                "mc06-fileA.txt",
                "mc06-fileA-renamed.txt",
            ],
        ),
        "git mv file A",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 c3 rename fileA"],
        ),
        "commit c3",
    );

    let c1 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD~2"],
    );
    let c2 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD~1"],
    );
    let c3 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );
    let c3_tree = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD^{tree}"],
    );
    let blob_b = git_stdout(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "rev-parse",
            "HEAD:mc06-dir/mc06-fileB.txt",
        ],
    );
    let blob_a = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD:mc06-fileA-renamed.txt"],
    );
    let pre_push_tree = snapshot_workdir(&clone);

    let refspec = format!("HEAD:refs/heads/{branch}");
    let push = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &refspec,
        ],
    );
    git_cli::assert_git_success(&push, "3-commit chain push must be admitted (MC-06)");

    // AC: exactly one CL for (path, user); from = fork point, to = chain tip.
    let cls = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        cls.len(),
        1,
        "a chain push must create exactly one CL: {cls:?}"
    );
    let cl = &cls[0];
    assert_eq!(
        cl.from_hash, base_head,
        "CL from_hash must be the chain base (fork point)"
    );
    assert_eq!(cl.to_hash, c3, "CL to_hash must be the chain tip");
    assert_eq!(cl.status, "open", "CL must be open before merge");

    // CL ref fields must be sourced from the same tip commit. The push-side CL
    // ref is discovered by `is_cl`: its name is generated independently of the
    // CL row's link (pre-existing behavior, not MC-06 scope).
    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs.len(),
        1,
        "exactly one refs/cl/* must exist after the chain push: {cl_refs:?}"
    );
    let (_cl_ref_name, ref_commit, ref_tree) = &cl_refs[0];
    assert_eq!(
        ref_commit, &c3,
        "CL ref ref_commit_hash must be the chain tip"
    );
    assert_eq!(
        ref_tree, &c3_tree,
        "CL ref ref_tree_hash must be the chain tip tree"
    );

    // AC: files added/renamed by non-first commits are indexed (the post-push
    // tip-tree walk covers the whole chain). Paths are rooted (`/…`) after
    // ADR-TP-11 (`blob_paths` / `mega_blob.file_path` display fallback).
    assert_eq!(
        blob_file_paths(&env.database.db_url, &blob_b),
        vec!["/mc06-dir/mc06-fileB.txt".to_string()],
        "blob added in c2 must be indexed at its path"
    );
    assert_eq!(
        blob_file_paths(&env.database.db_url, &blob_a),
        vec!["/mc06-fileA-renamed.txt".to_string()],
        "blob renamed in c3 must be indexed at the renamed path"
    );

    // Codex R1 P1-2 (positive path): the accepted chain's commits are bound to
    // the authenticated pusher — post-finalize, chain-scoped (not pack-scoped).
    let bindings = commit_auth_rows(&env.database.db_url);
    for sha in [&c1, &c2, &c3] {
        assert!(
            bindings
                .iter()
                .any(|(s, u)| s == sha && u.as_deref() == Some(git_cli::DEFAULT_GIT_AUTH_USER)),
            "accepted chain commit {sha} must be bound to the pusher: {bindings:?}"
        );
    }

    // Codex R3 P1: an idempotent re-push of the same tip is an ADR-MC-05 no-op
    // (the CL ref tip is advertised, so the client sends an empty pack) and
    // must not rewrite any `commit_auths` field — Monorepo owns binding via its
    // pipeline, the protocol layer's generic tip upsert is disabled for it.
    let auth_snapshot_before = commit_auth_snapshot(&env.database.db_url);
    let cl_refs_before = cl_ref_rows(&env.database.db_url);
    let repush = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &refspec,
        ],
    );
    git_cli::assert_git_success(&repush, "idempotent empty-pack re-push must succeed");
    let repush_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&repush.stdout),
        String::from_utf8_lossy(&repush.stderr)
    );
    assert!(
        repush_text.contains("no change list was created or updated"),
        "the no-op re-push must carry the ADR-MC-05 remote notice; got:\n{repush_text}"
    );
    assert_eq!(
        commit_auth_snapshot(&env.database.db_url),
        auth_snapshot_before,
        "an idempotent re-push must leave every commit_auths field byte-identical"
    );
    assert_eq!(
        cl_ref_rows(&env.database.db_url),
        cl_refs_before,
        "an idempotent re-push must not move any CL ref"
    );
    assert_eq!(
        cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER).len(),
        1,
        "an idempotent re-push must not create a second CL"
    );

    // AC (ADR-MC-01): merging the CL moves refs/heads/main to exactly one new
    // single-parent commit — never a fast-forward to the user's chain tip.
    //
    // GAP-07 coupling: the strict `parents == [pre-merge main head]` shape
    // below currently rests on an accident of the CL ref naming divergence —
    // the push-side CL ref name (L1, from `fetch_or_new_cl_link`) differs from
    // the CL row's link (L2, from `create_new_cl`), and this CL only ever saw
    // one push, so `refs/cl/<L2>` is absent from the merge's ref candidate set
    // and `process_ref_updates` falls through to main's head. Once GAP-07 is
    // fixed (same-source link), the merge parent becomes the chain tip and
    // this assertion must be re-reviewed together with ADR-MC-01.
    let merge_status = merge_cl_no_auth(port, &cl.link);
    assert_eq!(merge_status, 200, "chain CL merge must succeed");
    let (main_commit, main_tree) = ref_commit_tree(&env.database.db_url, "/", "refs/heads/main")
        .expect("main ref must exist after merge");
    assert!(
        main_commit != c1 && main_commit != c2 && main_commit != c3,
        "main must advance to a server-synthesized commit, not a chain commit: {main_commit}"
    );
    assert_eq!(
        main_tree, c3_tree,
        "root CL merge adopts the chain tip tree (aggregate diff)"
    );
    let parents = commit_parents(&env.database.db_url, &main_commit);
    assert_eq!(
        parents,
        vec![base_head.clone()],
        "the merge commit must be single-parented on the pre-merge main head \
         (ADR-MC-01: the pushed chain never lands on trunk; GAP-07 coupling: \
         this shape depends on the CL ref name differing from the CL link — \
         re-review with ADR-MC-01 when GAP-07 is fixed): {parents:?}"
    );
    let cl_after = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(cl_after.len(), 1, "still exactly one CL after merge");
    assert_eq!(cl_after[0].status, "merged", "CL must be merged");

    // End-to-end aggregate proof: a fresh clone of main matches the pre-push
    // working tree byte-for-byte.
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["clone", &remote_url, "mc06-verify-clone"],
        ),
        "post-merge clone of main",
    );
    let merged_tree = snapshot_workdir(&env.case_dir.join("mc06-verify-clone"));
    assert_eq!(
        merged_tree, pre_push_tree,
        "post-merge main worktree must equal the pushed 3-commit working tree"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_multicommit_two_commit_chain_acceptance() {
    // plan-20260827 MC-06: the lower boundary of the opened range — a 2-commit
    // chain push is admitted and lands as one CL with from = base / to = tip.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "mc06-two-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for two-commit chain push",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let base_head = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "origin/main"],
    );
    let branch = format!("mc06-two-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create two-commit branch",
    );

    let clone = env.case_dir.join(clone_name);
    fs::write(clone.join("mc06-two-1.txt"), b"mc06 two-commit c1\n").expect("write c1 file");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-two-1.txt"],
        ),
        "git add c1 file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 two c1"],
        ),
        "commit c1",
    );
    fs::write(clone.join("mc06-two-2.txt"), b"mc06 two-commit c2\n").expect("write c2 file");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-two-2.txt"],
        ),
        "git add c2 file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 two c2"],
        ),
        "commit c2",
    );
    let tip = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );
    let tip_tree = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD^{tree}"],
    );

    let refspec = format!("HEAD:refs/heads/{branch}");
    let push = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &refspec,
        ],
    );
    git_cli::assert_git_success(&push, "2-commit chain push must be admitted (MC-06)");

    let cls = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        cls.len(),
        1,
        "a 2-commit push must create exactly one CL: {cls:?}"
    );
    let cl = &cls[0];
    assert_eq!(
        cl.from_hash, base_head,
        "CL from_hash must be the chain base"
    );
    assert_eq!(cl.to_hash, tip, "CL to_hash must be the chain tip");

    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs.len(),
        1,
        "exactly one refs/cl/* must exist after the 2-commit push: {cl_refs:?}"
    );
    let (_cl_ref_name, ref_commit, ref_tree) = &cl_refs[0];
    assert_eq!(
        ref_commit, &tip,
        "CL ref ref_commit_hash must be the chain tip"
    );
    assert_eq!(
        ref_tree, &tip_tree,
        "CL ref ref_tree_hash must be the chain tip tree"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_multicommit_multi_branch_mixed_status() {
    // plan-20260901 FC-08: extra non-delete branch commands are ng'd; the first
    // surviving branch may still finalize. The overall git push fails.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "mc06-multibranch-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for multi-branch rejection",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let branch = format!("mc06-mb-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create multi-branch source branch",
    );
    let clone = env.case_dir.join(clone_name);
    fs::write(clone.join("mc06-mb.txt"), b"mc06 multi-branch rejection\n").expect("write mb file");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-mb.txt"],
        ),
        "git add mb file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 multi-branch"],
        ),
        "git commit mb file",
    );
    let pushed_commit = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );

    let before_cl = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    let push = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{branch}-a"),
            &format!("HEAD:refs/heads/{branch}-b"),
        ],
    );
    assert!(
        !push.status.success(),
        "a two-branch receive-pack must fail overall because the extra branch is ng; status={:?}\nstdout:\n{}\nstderr:\n{}",
        push.status,
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        combined.contains("at most one branch update"),
        "extra branch ng must state the v1 constraint; got:\n{combined}"
    );

    // Mixed status: the first surviving branch still creates a CL.
    let after_cl = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    assert!(
        after_cl.len() > before_cl.len(),
        "the first branch of a mixed-status push must still create a CL ref; before={before_cl:?} after={after_cl:?}"
    );
    let cls = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        cls.len(),
        1,
        "exactly one CL for the surviving branch; got {cls:?}"
    );
    assert_eq!(cls[0].to_hash, pushed_commit);
    assert!(
        commit_auth_rows(&env.database.db_url)
            .iter()
            .any(|(s, _)| s == &pushed_commit),
        "the surviving branch must bind its tip"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_multicommit_mixed_delete_and_update_accepted() {
    // plan-20260827 MC-06 / ADR-MC-04 (Codex R1 P2): delete commands do not
    // count towards the one-branch-update limit — one receive-pack carrying a
    // delete plus a legitimate update is accepted; a delete-only push still
    // skips unpack entirely.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "mc06-mixed-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for mixed delete+update case",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let base_head = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "origin/main"],
    );
    let branch = format!("mc06-mixed-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create mixed-case branch",
    );
    let clone = env.case_dir.join(clone_name);
    fs::write(clone.join("mc06-mixed-1.txt"), b"mc06 mixed c1\n").expect("write c1 file");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-mixed-1.txt"],
        ),
        "git add c1 file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 mixed c1"],
        ),
        "commit c1",
    );
    let c1 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );

    // First push: creates the CL and the push-side CL ref (name generated
    // independently of the CL row's link — discovered via `is_cl`).
    let refspec = format!("HEAD:refs/heads/{branch}");
    let push1 = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &refspec,
        ],
    );
    git_cli::assert_git_success(&push1, "first push creates the CL");
    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(cl_refs.len(), 1, "exactly one CL ref after the first push");
    let first_ref = cl_refs[0].0.clone();
    assert_eq!(cl_refs[0].1, c1, "first CL ref must point at c1");

    // c2 on top of c1.
    fs::write(clone.join("mc06-mixed-2.txt"), b"mc06 mixed c2\n").expect("write c2 file");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "mc06-mixed-2.txt"],
        ),
        "git add c2 file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "mc06 mixed c2"],
        ),
        "commit c2",
    );
    let c2 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );

    // ONE receive-pack: delete the first CL ref + push the c2 update. The
    // delete must not trip the ADR-MC-04 multi-branch refusal.
    let mixed = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!(":{first_ref}"),
            &refspec,
        ],
    );
    git_cli::assert_git_success(
        &mixed,
        "a receive-pack with one delete plus one update must be accepted",
    );

    // The delete took effect; the update advanced the CL and its ref.
    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs.len(),
        1,
        "exactly one CL ref after the mixed push: {cl_refs:?}"
    );
    assert_ne!(
        cl_refs[0].0, first_ref,
        "the deleted CL ref must be gone: {cl_refs:?}"
    );
    assert_eq!(cl_refs[0].1, c2, "the CL ref must advance to c2");
    let cls = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(cls.len(), 1, "still exactly one CL");
    assert_eq!(
        cls[0].from_hash, base_head,
        "CL from_hash stays frozen at the fork point"
    );
    assert_eq!(cls[0].to_hash, c2, "CL to_hash advances to c2");
    let remote_cls = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
    assert!(
        !remote_cls.iter().any(|r| r == &first_ref),
        "deleted CL ref must not be advertised: {remote_cls:?}"
    );
    assert!(
        remote_cls.iter().any(|r| r == &cl_refs[0].0),
        "the advanced CL ref must be advertised: {remote_cls:?}"
    );

    // Delete-only regression: skips unpack entirely and removes the ref.
    let second_ref = cl_refs[0].0.clone();
    let delete_only = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "push",
            "origin",
            &format!(":{second_ref}"),
        ],
    );
    git_cli::assert_git_success(&delete_only, "delete-only push must succeed");
    assert!(
        cl_ref_rows(&env.database.db_url).is_empty(),
        "delete-only push must remove the CL ref row"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_stale_delete_does_not_remove_updated_ref() {
    // plan-20260901 FC-09: a delete whose advertised old id no longer matches
    // must not remove a ref that moved after discovery.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "fc09-cas-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for CAS delete",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let branch = format!("fc09-cas-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create CAS source branch",
    );
    let clone = env.case_dir.join(clone_name);
    fs::write(clone.join("fc09-c1.txt"), b"fc09 c1\n").expect("write c1");
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "add", "fc09-c1.txt"],
        ),
        "git add c1",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", clone_name, "commit", "-m", "fc09 c1"],
        ),
        "commit c1",
    );
    let c1 = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD"],
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        ),
        "first push opens the CL",
    );
    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs.len(),
        1,
        "exactly one CL ref after the first push: {cl_refs:?}"
    );
    let cl_name = cl_refs[0].0.clone();
    assert_eq!(cl_refs[0].1, c1);

    // Simulate a racing writer that moved the same mega_refs row after the
    // client discovered c1. A second `HEAD:refs/heads/<branch>` push would
    // open a new CL rather than mutate this row.
    let moved = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    update_cl_ref_commit(&env.database.db_url, &cl_name, moved);
    let after_update = cl_ref_rows(&env.database.db_url);
    assert_eq!(after_update.len(), 1);
    assert_eq!(after_update[0].1, moved);

    let report = post_packless_receive_pack(port, &token, &c1, ZERO_SHA1, &cl_name);
    assert!(
        report.contains("moved since advertisement") || report.contains("ng "),
        "stale delete must ng; got:\n{report}"
    );
    let still = cl_ref_rows(&env.database.db_url);
    assert_eq!(still.len(), 1, "CAS miss must leave the updated CL ref");
    assert_eq!(
        still[0].1, moved,
        "updated tip {moved} must survive stale delete of {c1}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_cli_multicommit_cumulative_limit_rejected() {
    // plan-20260827 MC-06 / ADR-MC-07: the 250 bound applies to the CL's
    // cumulative (from_hash → to_hash) range. A 200-commit push opens the CL;
    // a follow-up 100-commit push is a legal increment on its own but crosses
    // the cumulative limit and must be rejected at push time, leaving the CL
    // and its ref untouched.
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let token = git_cli::resolve_seed_token();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
    git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

    let remote_url = git_cli::monoengine_http_repo_url(port);
    let clone_name = "mc06-cumulative-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, clone_name]),
        "clone for cumulative limit case",
    );
    for (key, value) in [
        ("user.name", "IT Git CLI"),
        ("user.email", "it-git-cli@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", clone_name, "config", key, value],
            ),
            "git config identity",
        );
    }
    let base_head = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "origin/main"],
    );
    // All commits share the seed tree: the chain exercises commit-count limits
    // without packing 300 trees/blobs.
    let seed_tree = git_stdout(
        &env.case_dir,
        &token,
        &["-C", clone_name, "rev-parse", "HEAD^{tree}"],
    );
    let mut parent = base_head.clone();
    for i in 1..=200 {
        parent = git_stdout(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "commit-tree",
                &seed_tree,
                "-p",
                &parent,
                "-m",
                &format!("mc06 cumulative {i}"),
            ],
        );
    }
    let tip200 = parent.clone();
    let branch = format!("mc06-cumulative-{}", std::process::id());

    let push200 = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("{tip200}:refs/heads/{branch}"),
        ],
    );
    git_cli::assert_git_success(&push200, "the 200-commit push must pass the 250 bound");

    let cls = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        cls.len(),
        1,
        "the 200-commit push must create exactly one CL: {cls:?}"
    );
    let cl = &cls[0];
    assert_eq!(
        cl.from_hash, base_head,
        "CL from_hash must be the fork point"
    );
    assert_eq!(cl.to_hash, tip200, "CL to_hash must be the first push tip");
    let cl_refs = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs.len(),
        1,
        "exactly one refs/cl/* must exist after the 200-commit push: {cl_refs:?}"
    );

    // Second push: another 100 commits on top of the first tip — the increment
    // is legal, the cumulative range (300 > 250) is not.
    let mut mid_commit = String::new();
    for i in 201..=300 {
        parent = git_stdout(
            &env.case_dir,
            &token,
            &[
                "-C",
                clone_name,
                "commit-tree",
                &seed_tree,
                "-p",
                &parent,
                "-m",
                &format!("mc06 cumulative {i}"),
            ],
        );
        if i == 250 {
            mid_commit = parent.clone();
        }
    }
    let tip300 = parent.clone();
    let push300 = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("{tip300}:refs/heads/{branch}"),
        ],
    );
    assert!(
        !push300.status.success(),
        "the 200+100 cumulative overflow push must be rejected; status={:?}\nstdout:\n{}\nstderr:\n{}",
        push300.status,
        String::from_utf8_lossy(&push300.stdout),
        String::from_utf8_lossy(&push300.stderr)
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&push300.stdout),
        String::from_utf8_lossy(&push300.stderr)
    );
    assert!(
        combined.contains("cumulative limit"),
        "cumulative rejection must name the limit; got:\n{combined}"
    );
    assert!(
        combined.contains("merge the current CL first or open a new CL"),
        "cumulative rejection must be actionable; got:\n{combined}"
    );

    // The rejected push must not move the CL or its ref.
    let cls_after = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(
        cls_after.len(),
        1,
        "the rejected push must not create a second CL"
    );
    assert_eq!(
        cls_after[0].to_hash, tip200,
        "the rejected push must not advance the CL to_hash"
    );
    let cl_refs_after = cl_ref_rows(&env.database.db_url);
    assert_eq!(
        cl_refs_after.len(),
        1,
        "the rejected push must not create a second CL ref: {cl_refs_after:?}"
    );
    assert_eq!(
        cl_refs_after[0].1, tip200,
        "the rejected push must not move the CL ref"
    );

    // Codex R1 P1-2 / R2: bindings follow acceptance — the accepted first
    // chain's commits are bound, the rejected second chain's are not.
    let bindings = commit_auth_rows(&env.database.db_url);
    assert!(
        bindings
            .iter()
            .any(|(s, u)| s == &tip200 && u.as_deref() == Some(git_cli::DEFAULT_GIT_AUTH_USER)),
        "the accepted 200-commit tip must be bound to the pusher"
    );
    for sha in [&mid_commit, &tip300] {
        assert!(
            !bindings.iter().any(|(s, _)| s == sha),
            "rejected chain commit {sha} must not be bound"
        );
    }

    // Codex R2 P1-1 (sticky rejection): a verbatim retry of the rejected push
    // re-carries the same 100 commits — all already stored from the rejected
    // attempt, so nothing is newly introduced — and must be rejected
    // identically, leaving CL, ref and bindings untouched.
    let push_retry = git_cli::git_cli(
        &env.case_dir,
        &token,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            &format!("{tip300}:refs/heads/{branch}"),
        ],
    );
    assert!(
        !push_retry.status.success(),
        "a verbatim retry of the rejected push must be rejected (sticky); status={:?}\nstdout:\n{}\nstderr:\n{}",
        push_retry.status,
        String::from_utf8_lossy(&push_retry.stdout),
        String::from_utf8_lossy(&push_retry.stderr)
    );
    // Codex R3 P2: the retry must fail with the *byte-identical* server
    // diagnostic — not merely another message that mentions the limit.
    let first_rejection = remote_rejected_lines(&push300);
    let retry_rejection = remote_rejected_lines(&push_retry);
    assert!(
        !first_rejection.is_empty(),
        "the first rejection must carry a server diagnostic line; stderr:\n{}",
        String::from_utf8_lossy(&push300.stderr)
    );
    assert_eq!(
        first_rejection, retry_rejection,
        "a verbatim retry must fail with the byte-identical server diagnostic"
    );
    let cls_retry = cl_rows_for_user(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER);
    assert_eq!(cls_retry.len(), 1, "the retry must not create a second CL");
    assert_eq!(
        cls_retry[0].to_hash, tip200,
        "the retry must not advance the CL"
    );
    let cl_refs_retry = cl_ref_rows(&env.database.db_url);
    assert_eq!(cl_refs_retry.len(), 1, "the retry must not add a CL ref");
    assert_eq!(
        cl_refs_retry[0].1, tip200,
        "the retry must not move the CL ref"
    );
    assert_eq!(
        commit_auth_rows(&env.database.db_url),
        bindings,
        "the retry must not change commit_auths"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

fn collect_git_configs(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == ".git") {
                let cfg = path.join("config");
                if cfg.is_file() {
                    out.push(cfg);
                }
            } else {
                collect_git_configs(&path, out);
            }
        }
    }
}

#[test]
fn integration_git_cli_failpath_clone_missing_repo_keeps_service_alive() {
    if git_cli::git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let env = GitCliEnv::new();
    let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);

    // Legacy-disallowed root repo: parse_git_protocol_path rejects it before any
    // empty-repo advertisement (monorepo otherwise serves arbitrary paths as empty).
    let missing_url = git_cli::monoengine_http_url(port, "/third-party.git/");
    let clone_dir = "failpath-missing";

    let first = git_cli::git_cli_no_auth(&env.case_dir, &["clone", &missing_url, clone_dir]);
    // Remove any partial clone dir so the second attempt is byte-identical input.
    let _ = fs::remove_dir_all(env.case_dir.join(clone_dir));
    let second = git_cli::git_cli_no_auth(&env.case_dir, &["clone", &missing_url, clone_dir]);

    assert!(
        !first.status.success(),
        "clone of disallowed/missing repo must fail (status={:?} code={:?})\nstdout:\n{}\nstderr:\n{}",
        first.status,
        first.status.code(),
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        !second.status.success(),
        "repeat clone of disallowed/missing repo must fail (status={:?})",
        second.status
    );

    let msg1 = format!(
        "{}\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let msg2 = format!(
        "{}\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        normalize_git_cli_error(&msg1),
        normalize_git_cli_error(&msg2),
        "missing-repo clone error must be stable across identical inputs\nfirst:\n{msg1}\nsecond:\n{msg2}"
    );
    // Fixed substring from the real git client against the 400 rejection.
    const STABLE_ERR: &str = "The requested URL returned error: 400";
    assert!(
        msg1.contains(STABLE_ERR),
        "missing-repo clone error must contain `{STABLE_ERR}`, got:\n{msg1}"
    );

    // Liveness probe before teardown (VER reads MONOENGINE_IT_LIVENESS_FILE).
    service.assert_alive();
    let status = http_status(port, "/api/openapi.json");
    assert_eq!(
        status, 200,
        "service must still answer HTTP 200 after missing-repo clone failure"
    );
    let pid = service.pid();
    git_cli::append_evidence_line(
        "MONOENGINE_IT_LIVENESS_FILE",
        &format!("status={status} pid={pid}"),
    );
    // PID must still be alive at evidence time.
    service.assert_alive();

    let exit = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        exit.success(),
        "service did not shut down cleanly: {exit}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

fn normalize_git_cli_error(combined: &str) -> String {
    combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn http_status(port: u16, path: &str) -> u16 {
    let url = git_cli::monoengine_host_http_url(port, path);
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "15",
        &url,
    ]);
    let output = command.output().expect("curl liveness probe");
    assert!(
        output.status.success(),
        "curl liveness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad http code from curl: {:?}", output.stdout))
}

fn ls_remote_cl_refs(case_dir: &Path, token: &str, remote_url: &str) -> Vec<String> {
    ls_remote_refs(case_dir, token, remote_url, "refs/cl/*")
}

fn ls_remote_refs(case_dir: &Path, token: &str, remote_url: &str, pattern: &str) -> Vec<String> {
    let output = git_cli::git_cli(case_dir, token, &["ls-remote", remote_url, pattern]);
    git_cli::assert_git_success(&output, &format!("ls-remote {pattern}"));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

fn snapshot_workdir(root: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    snapshot_workdir_rec(root, root, &mut out);
    out
}

fn snapshot_workdir_rec(root: &Path, dir: &Path, out: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()))
    {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if path.is_dir() {
            snapshot_workdir_rec(root, &path, out);
            continue;
        }
        let rel = path.strip_prefix(root).expect("path under root");
        let rel_key = path_bytes(rel);
        let bytes = fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        out.insert(rel_key, bytes);
    }
}

fn path_bytes(path: &Path) -> Vec<u8> {
    // Linux-only harness: compare path bytes without lossy Windows mapping.
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// Complete expected worktree for `MegaModelConverter::init` against the default
/// `config/config.toml` monorepo settings (`admin = ["benjamin_747"]`, the eight
/// `root_dirs`). Kept in sync with `src/jupiter/utils/converter.rs::init_trees`.
fn expected_init_monorepo_fixture() -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    for dir in [
        "third-party",
        "project",
        "doc",
        "artifact",
        "release",
        "model",
        "data",
        "toolchains",
    ] {
        out.insert(
            path_bytes(Path::new(&format!("{dir}/.gitkeep"))),
            format!("Placeholder file for /{dir} directory").into_bytes(),
        );
    }
    out.insert(path_bytes(Path::new(".buckroot")), Vec::new());
    out.insert(
        path_bytes(Path::new(".buckconfig")),
        expected_buckconfig_bytes(),
    );
    out.insert(
        path_bytes(Path::new("toolchains/BUCK")),
        br#"load("@prelude//toolchains:demo.bzl", "system_demo_toolchains")

# All the default toolchains, suitable for a quick demo or early prototyping.
# Most real projects should copy/paste the implementation to configure them.
system_demo_toolchains()
"#
        .to_vec(),
    );
    out.insert(
        path_bytes(Path::new(".cedar/policies.cedar")),
        br#"permit(action == "code:review", principal, resource)
    when { resource.path.startsWith("") }
    to ["benjamin_747"];
"#
        .to_vec(),
    );
    out.insert(
        path_bytes(Path::new(".mega_cedar.json")),
        expected_mega_cedar_json_bytes(),
    );
    out
}

fn expected_buckconfig_bytes() -> Vec<u8> {
    let cells = [
        "  root = .",
        "  prelude = prelude",
        "  toolchains = toolchains",
        "  buckal = toolchains/buckal-bundles",
        "  none = none",
    ]
    .join("\n");
    format!(
        r#"[cells]
{cells}

[cell_aliases]
  config = prelude
  ovr_config = prelude
  fbcode = none
  fbsource = none
  fbcode_macros = none
  buck = none

# Uses a copy of the prelude bundled with the buck2 binary. You can alternatively delete this
# section and vendor a copy of the prelude to the `prelude` directory of your project.
[external_cells]
  prelude = bundled

[parser]
  target_platform_detector_spec = target:root//...->prelude//platforms:default \
    target:prelude//...->prelude//platforms:default \
    target:toolchains//...->prelude//platforms:default

[build]
  execution_platforms = prelude//platforms:default
  default_target_platforms = prelude//platforms:default
"#
    )
    .into_bytes()
}

fn expected_mega_cedar_json_bytes() -> Vec<u8> {
    // Mirrors `contract::policy::entitystore::generate_entity(["benjamin_747"], "/")`.
    let json = serde_json::json!({
        "users": {
            "User::\"benjamin_747\"": {
                "euid": "User::\"benjamin_747\"",
                "parents": ["UserGroup::\"admin\""]
            }
        },
        "repos": {
            "Repository::\"/\"": {
                "euid": "Repository::\"/\"",
                "is_private": true,
                "admins": "UserGroup::\"admin\"",
                "maintainers": "UserGroup::\"matainer\"",
                "readers": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "user_groups": {
            "UserGroup::\"admin\"": {
                "euid": "UserGroup::\"admin\"",
                "parents": ["UserGroup::\"matainer\""]
            },
            "UserGroup::\"matainer\"": {
                "euid": "UserGroup::\"matainer\"",
                "parents": ["UserGroup::\"reader\""]
            },
            "UserGroup::\"reader\"": {
                "euid": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "merge_requests": {},
        "issues": {}
    });
    serde_json::to_string_pretty(&json)
        .expect("serialize expected cedar entity")
        .into_bytes()
}

/// Generate `/.mega_cedar.json` content granting the given users admin
/// membership (mirrors `contract::policy::entitystore::generate_entity`).
fn authz_json_with_admins(admins: &[&str]) -> Vec<u8> {
    let mut users = serde_json::Map::new();
    for admin in admins {
        let key = format!("User::\"{admin}\"");
        users.insert(
            key.clone(),
            serde_json::json!({
                "euid": key,
                "parents": ["UserGroup::\"admin\""]
            }),
        );
    }
    let json = serde_json::json!({
        "users": users,
        "repos": {
            "Repository::\"/\"": {
                "euid": "Repository::\"/\"",
                "is_private": true,
                "admins": "UserGroup::\"admin\"",
                "maintainers": "UserGroup::\"matainer\"",
                "readers": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "user_groups": {
            "UserGroup::\"admin\"": {
                "euid": "UserGroup::\"admin\"",
                "parents": ["UserGroup::\"matainer\""]
            },
            "UserGroup::\"matainer\"": {
                "euid": "UserGroup::\"matainer\"",
                "parents": ["UserGroup::\"reader\""]
            },
            "UserGroup::\"reader\"": {
                "euid": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "merge_requests": {},
        "issues": {}
    });
    serde_json::to_vec_pretty(&json).expect("serialize authz json")
}

/// Query the most recently created CL link for a username (UN-16 e2e: the
/// admin's push creates a CL; the merge API needs its link).
fn latest_cl_link_for_user(db_url: &str, username: &str) -> String {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for CL link: {err}"));
        let username_sql = username.replace('\'', "''");
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT link FROM mega_cl WHERE username = '{username_sql}' \
                     ORDER BY id DESC LIMIT 1"
                ),
            ))
            .await
            .expect("query latest CL link")
            .expect("CL row exists for admin push");
        row.try_get("", "link").expect("link column")
    })
}

/// Run a git command expected to succeed and return its trimmed stdout.
fn git_stdout(case_dir: &Path, token: &str, git_args: &[&str]) -> String {
    let output = git_cli::git_cli(case_dir, token, git_args);
    git_cli::assert_git_success(&output, &format!("git {}", git_args.join(" ")));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// One `mega_cl` row as the MC-06 assertions need it.
#[derive(Debug)]
struct ClRow {
    link: String,
    from_hash: String,
    to_hash: String,
    status: String,
}

/// All CL rows of a user on the root repo path, in creation order (MC-06 e2e:
/// chain pushes must create/update exactly one CL).
fn cl_rows_for_user(db_url: &str, username: &str) -> Vec<ClRow> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for CL rows: {err}"));
        let username_sql = username.replace('\'', "''");
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT link, from_hash, to_hash, status::text AS status FROM mega_cl \
                     WHERE username = '{username_sql}' AND path = '/' ORDER BY id"
                ),
            ))
            .await
            .unwrap_or_else(|err| panic!("query CL rows: {err}"));
        rows.iter()
            .map(|row| ClRow {
                link: row.try_get("", "link").expect("link column"),
                from_hash: row.try_get("", "from_hash").expect("from_hash column"),
                to_hash: row.try_get("", "to_hash").expect("to_hash column"),
                status: row.try_get("", "status").expect("status column"),
            })
            .collect()
    })
}

/// All `refs/cl/*` rows on the root repo path as `(ref_name, ref_commit_hash,
/// ref_tree_hash)`, in name order (MC-06 e2e). Located by `is_cl` rather than
/// by CL link: the push-side CL ref name is generated independently of the CL
/// row's link (pre-existing behavior — see `fetch_or_new_cl_link` vs
/// `create_new_cl`), so the two cannot be joined by name.
fn update_cl_ref_commit(db_url: &str, ref_name: &str, commit: &str) {
    let ref_sql = ref_name.replace('\'', "''");
    let commit_sql = commit.replace('\'', "''");
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB to move CL ref: {err}"));
        execute_postgres(
            &db,
            format!(
                "UPDATE mega_refs SET ref_commit_hash = '{commit_sql}' \
                 WHERE path = '/' AND is_cl AND ref_name = '{ref_sql}'"
            ),
        )
        .await;
    });
}

fn cl_ref_rows(db_url: &str) -> Vec<(String, String, String)> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for CL ref rows: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT ref_name, ref_commit_hash, ref_tree_hash FROM mega_refs \
                 WHERE path = '/' AND is_cl ORDER BY ref_name"
                    .to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("query CL ref rows: {err}"));
        rows.iter()
            .map(|row| {
                (
                    row.try_get("", "ref_name").expect("ref_name column"),
                    row.try_get("", "ref_commit_hash")
                        .expect("ref_commit_hash column"),
                    row.try_get("", "ref_tree_hash")
                        .expect("ref_tree_hash column"),
                )
            })
            .collect()
    })
}

/// All `commit_auths` rows as `(commit_sha, matched_username)`, ordered by sha
/// (Codex R1 P1-2 e2e pins: accepted chains bind their commits post-finalize;
/// rejected pushes leave the table untouched).
fn commit_auth_rows(db_url: &str) -> Vec<(String, Option<String>)> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for commit_auths: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT commit_sha, matched_username FROM commit_auths ORDER BY commit_sha"
                    .to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("query commit_auths: {err}"));
        rows.iter()
            .map(|row| {
                (
                    row.try_get("", "commit_sha").expect("commit_sha column"),
                    row.try_get("", "matched_username")
                        .expect("matched_username column"),
                )
            })
            .collect()
    })
}

/// Full-row snapshot of `commit_auths`, each row formatted with every column
/// and ordered by id (Codex R3 P1: an empty-pack idempotent re-push must leave
/// every field — `matched_at` included — byte-identical).
fn commit_auth_snapshot(db_url: &str) -> Vec<String> {
    with_runtime(async {
        let db = Database::connect(db_url).await.unwrap_or_else(|err| {
            panic!("connect integration DB for commit_auths snapshot: {err}")
        });
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT id, commit_sha, matched_username, is_anonymous, \
                 matched_at::text AS matched_txt, created_at::text AS created_txt \
                 FROM commit_auths ORDER BY id"
                    .to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("snapshot commit_auths: {err}"));
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("", "id").expect("id column");
                let sha: String = row.try_get("", "commit_sha").expect("commit_sha column");
                let user: Option<String> = row
                    .try_get("", "matched_username")
                    .expect("matched_username column");
                let anon: bool = row
                    .try_get("", "is_anonymous")
                    .expect("is_anonymous column");
                let matched: Option<String> = row.try_get("", "matched_txt").expect("matched_txt");
                let created: Option<String> = row.try_get("", "created_txt").expect("created_txt");
                format!("{id}|{sha}|{user:?}|{anon}|{matched:?}|{created:?}")
            })
            .collect()
    })
}

/// The server's per-ref rejection lines from a failed push
/// (`! [remote rejected] <ref> (<reason>)` on stderr) — Codex R3 P2: a
/// verbatim retry must carry the byte-identical server diagnostic.
fn remote_rejected_lines(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.contains("[remote rejected]"))
        .map(str::to_string)
        .collect()
}

/// `(ref_commit_hash, ref_tree_hash)` of one `mega_refs` row, if it exists.
fn ref_commit_tree(db_url: &str, path: &str, ref_name: &str) -> Option<(String, String)> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for ref row: {err}"));
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT ref_commit_hash, ref_tree_hash FROM mega_refs \
                     WHERE path = '{path}' AND ref_name = '{ref_name}'"
                ),
            ))
            .await
            .unwrap_or_else(|err| panic!("query ref row {ref_name}: {err}"));
        row.map(|row| {
            (
                row.try_get("", "ref_commit_hash").expect("ref_commit_hash"),
                row.try_get("", "ref_tree_hash").expect("ref_tree_hash"),
            )
        })
    })
}

/// Distinct `file_path` values stored for a blob id (MC-06 e2e: files
/// introduced or renamed by non-first chain commits must be indexed).
fn blob_file_paths(db_url: &str, blob_id: &str) -> Vec<String> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for blob rows: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT DISTINCT file_path FROM mega_blob WHERE blob_id = '{blob_id}'"),
            ))
            .await
            .unwrap_or_else(|err| panic!("query blob file_path for {blob_id}: {err}"));
        rows.iter()
            .map(|row| row.try_get("", "file_path").expect("file_path column"))
            .collect()
    })
}

/// Parsed `parents_id` of a `mega_commit` row (MC-06 e2e: the merge commit on
/// `refs/heads/main` must be single-parent, ADR-MC-01).
fn commit_parents(db_url: &str, commit_id: &str) -> Vec<String> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for commit row: {err}"));
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT parents_id::text AS parents FROM mega_commit WHERE commit_id = '{commit_id}'"),
            ))
            .await
            .unwrap_or_else(|err| panic!("query commit parents for {commit_id}: {err}"))
            .unwrap_or_else(|| panic!("mega_commit row {commit_id} must exist"));
        let parents: String = row.try_get("", "parents").expect("parents column");
        serde_json::from_str(&parents)
            .unwrap_or_else(|err| panic!("parse parents_id of {commit_id}: {err}"))
    })
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
