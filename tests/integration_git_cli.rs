// Process-level black-box Git CLI integration tests (IT-03 / IT-10 / IT-12).
//
// Starts a real `service http` via `CARGO_BIN_EXE_monoengine`, drives the fixed
// compose `git-cli` runner over HTTP smart protocol, and asserts clone→push→
// re-clone working-tree round-trips with per-case DB/port/workdir isolation.
// MonoRepo product rules (`docs/monorepo.md`): only public branch is `main`;
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
    git_cli::apply_monoengine_public_http_base_env(&mut command, port);
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
        "MonoRepo push must not create/alter public refs/heads/* (docs/monorepo.md §1)"
    );
    assert!(
        !after_heads
            .iter()
            .any(|r| r == &format!("refs/heads/{branch}")),
        "public branch refs/heads/{branch} must not exist after MonoRepo CL push",
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
    // docs/monorepo.md §2 — MonoRepo tags are Web/API only.
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
    assert!(
        !push.status.success(),
        "MonoRepo must reject Git-client tag push; status={:?}\nstdout:\n{}\nstderr:\n{}",
        push.status,
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
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
    // the single-commit-per-push MonoRepo rule rejects).
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
