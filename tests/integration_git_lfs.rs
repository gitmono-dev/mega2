// Process-level HTTP Git LFS round-trip integration test (GM-05).
//
// Starts a real `service http`, drives the pinned compose Git/Git LFS runner,
// pushes an LFS-tracked binary into a Monorepo CL ref, then performs a directed
// fetch and explicit `git lfs pull` from a peer worktree.

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
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use sha2::Digest as _;
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str = "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";
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
            "mega2_git_lfs_{}_{}",
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

struct GitLfsEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
}

impl GitLfsEnv {
    fn new() -> Self {
        Self::with_config_append("")
    }

    fn with_config_append(append: &str) -> Self {
        git_cli::require_git_cli_runner();

        let work_root = git_cli::git_cli_workdir();
        fs::create_dir_all(&work_root).unwrap_or_else(|err| {
            panic!("create shared git workdir {}: {err}", work_root.display())
        });
        let case_name = format!(
            "lfs-case-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let case_dir = work_root.join(case_name);
        if case_dir.exists() {
            fs::remove_dir_all(&case_dir).expect("clean stale LFS case dir");
        }
        fs::create_dir_all(&case_dir).expect("create LFS case dir");

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
            .env("MEGA_LOG__PRINT_STD", "false")
            .env("MEGA_LOG__WITH_ANSI", "false")
            .env("MEGA_REDIS__URL", integration_redis_url())
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root);
        command
    }
}

impl Drop for GitLfsEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.case_dir);
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

#[test]
fn integration_git_lfs_http_round_trip() {
    // GM-05: no SKIP-green path. Unavailable runner already panics via
    // require_git_cli_runner; an explicit skip request must also fail.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let token = git_cli::resolve_seed_token();
    assert_pinned_git_lfs(&token);

    let case_dir = {
        let env = GitLfsEnv::new();
        let case_dir = env.case_dir.clone();
        let payload = binary_payload();
        let (mut service, port, _stdout_path, stderr_path) = boot_service_http(&env);
        let service_pid = service.pid();
        git_cli::seed_access_token(&env.database.db_url, git_cli::DEFAULT_GIT_AUTH_USER, &token);

        let remote_url = git_cli::mega2_http_repo_url(port);
        let lfs_url = format!("{}/info/lfs", remote_url.trim_end_matches('/'));
        let source_name = "lfs-source";
        let source = env.case_dir.join(source_name);
        git_cli::assert_git_success(
            &git_cli::git_cli(&env.case_dir, &token, &["clone", &remote_url, source_name]),
            "clone LFS source",
        );
        configure_git_identity(&env.case_dir, &token, source_name);
        configure_lfs(&env.case_dir, &token, source_name, &lfs_url);

        let branch = format!("gm-05-lfs-{}", std::process::id());
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", source_name, "checkout", "-b", &branch],
            ),
            "create LFS source branch",
        );
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", source_name, "lfs", "track", "*.bin"],
            ),
            "track binary files with Git LFS",
        );
        let binary_name = "gm-05-round-trip.bin";
        fs::write(source.join(binary_name), &payload).expect("write LFS binary fixture");
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", source_name, "add", ".gitattributes", binary_name],
            ),
            "add LFS binary fixture",
        );
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", source_name, "commit", "-m", "gm-05 LFS round trip"],
            ),
            "commit LFS binary fixture",
        );

        let pointer = git_cli::git_cli(
            &env.case_dir,
            &token,
            &["-C", source_name, "show", &format!("HEAD:{binary_name}")],
        );
        git_cli::assert_git_success(&pointer, "read committed LFS pointer");
        assert!(
            pointer
                .stdout
                .starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
            "committed blob must be an LFS pointer, got {} bytes",
            pointer.stdout.len()
        );
        assert_eq!(
            fs::read(source.join(binary_name)).expect("read source LFS fixture"),
            payload,
            "source worktree must retain the original binary bytes"
        );

        let before_refs = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
        let refspec = format!("HEAD:refs/heads/{branch}");
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &[
                    "-C",
                    source_name,
                    "-c",
                    "pack.window=0",
                    "-c",
                    "pack.depth=0",
                    "push",
                    "origin",
                    &refspec,
                ],
            ),
            "push LFS branch into a CL ref",
        );
        let after_refs = ls_remote_cl_refs(&env.case_dir, &token, &remote_url);
        let new_refs = after_refs
            .iter()
            .filter(|candidate| !before_refs.contains(candidate))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            new_refs.len(),
            1,
            "LFS push must create exactly one new refs/cl/*; before={before_refs:?} after={after_refs:?}"
        );
        let cl_ref = &new_refs[0];

        let peer_name = "lfs-peer";
        let peer = env.case_dir.join(peer_name);
        fs::create_dir_all(&peer).expect("create LFS peer dir");
        git_cli::assert_git_success(
            &git_cli::git_cli(&env.case_dir, &token, &["-C", peer_name, "init"]),
            "initialize LFS peer",
        );
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", peer_name, "remote", "add", "origin", &remote_url],
            ),
            "add LFS peer origin",
        );
        configure_lfs(&env.case_dir, &token, peer_name, &lfs_url);
        let peer_refspec = format!("{cl_ref}:refs/heads/lfs-verify");
        git_cli::assert_git_success(
            &git_cli::git_cli(
                &env.case_dir,
                &token,
                &["-C", peer_name, "fetch", "origin", &peer_refspec],
            ),
            "fetch LFS CL ref into peer",
        );
        git_cli::assert_git_success(
            &git_cli::git_cli_lfs_skip_smudge(
                &env.case_dir,
                &token,
                &["-C", peer_name, "checkout", "lfs-verify"],
            ),
            "checkout LFS CL ref without smudging",
        );
        let pre_pull = fs::read(peer.join(binary_name)).expect("read peer LFS pointer");
        assert!(
            pre_pull.starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
            "peer checkout must contain the pointer before explicit git lfs pull"
        );
        git_cli::assert_git_success(
            &git_cli::git_cli(&env.case_dir, &token, &["-C", peer_name, "lfs", "pull"]),
            "explicit peer git lfs pull",
        );
        assert_eq!(
            fs::read(peer.join(binary_name)).expect("read pulled LFS binary"),
            payload,
            "peer LFS bytes must match the source fixture byte-for-byte"
        );

        assert_lfs_tmp_clean(&source);
        assert_lfs_tmp_clean(&peer);
        assert_token_absent(&env.case_dir, token.as_bytes());

        let status = service.shutdown_via_sigint(Duration::from_secs(60));
        assert!(
            status.success(),
            "service did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&stderr_path),
        );
        assert_process_reaped(service_pid);
        wait_until_port_closed(port, Duration::from_secs(5));
        drop(service);
        case_dir
    };

    assert!(
        !case_dir.exists(),
        "LFS case directory must be removed with temporary credentials and state: {}",
        case_dir.display()
    );
}

#[test]
fn integration_git_lfs_trunk_push_auth_none_round_trip() {
    // LF-03: trunk + push_auth=none — anonymous LFS upload + push to main.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let probe_token = git_cli::resolve_seed_token();
    assert_pinned_git_lfs(&probe_token);

    let env = GitLfsEnv::new();
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_trunk(&env, &trunk_boot_env());
    let payload = binary_payload();
    assert_host_git_lfs_pinned();

    seed_project_foo_trunk(&env.case_dir, port);
    let foo_url = trunk_subpath_url(port, "/project/foo");
    let lfs_url = format!("{}/info/lfs", foo_url.trim_end_matches('/'));

    trunk_git_ok(&env.case_dir, None, &["clone", &foo_url, "lfs-trunk-src"]);
    configure_git_identity_trunk(&env.case_dir, None, "lfs-trunk-src");
    configure_lfs_trunk(&env.case_dir, None, "lfs-trunk-src", &lfs_url);

    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", "lfs-trunk-src", "lfs", "track", "*.bin"],
    );
    let binary_name = "lf-03-none.bin";
    fs::write(
        env.case_dir.join("lfs-trunk-src").join(binary_name),
        &payload,
    )
    .expect("write trunk LFS fixture");
    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", "lfs-trunk-src", "add", ".gitattributes", binary_name],
    );
    trunk_git_ok(
        &env.case_dir,
        None,
        &[
            "-C",
            "lfs-trunk-src",
            "commit",
            "-m",
            "lf-03 trunk LFS none round trip",
        ],
    );
    let pointer = trunk_host_git(
        &env.case_dir,
        None,
        &[
            "-C",
            "lfs-trunk-src",
            "show",
            &format!("HEAD:{binary_name}"),
        ],
    );
    git_cli::assert_git_success(&pointer, "read committed trunk LFS pointer");
    assert!(
        pointer
            .stdout
            .starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
        "committed blob must be an LFS pointer"
    );
    git_cli::assert_git_success(
        &trunk_push_main(&env.case_dir, None, "lfs-trunk-src"),
        "anonymous trunk LFS push to main",
    );

    let peer_name = "lfs-trunk-peer";
    fs::create_dir_all(env.case_dir.join(peer_name)).expect("peer dir");
    trunk_git_ok(&env.case_dir, None, &["-C", peer_name, "init"]);
    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", peer_name, "remote", "add", "origin", &foo_url],
    );
    configure_lfs_trunk(&env.case_dir, None, peer_name, &lfs_url);
    trunk_git_ok(
        &env.case_dir,
        None,
        &[
            "-C",
            peer_name,
            "fetch",
            "origin",
            "refs/heads/main:refs/heads/main",
        ],
    );
    let mut skip_smudge = trunk_host_git_command(&env.case_dir, None);
    skip_smudge.env("GIT_LFS_SKIP_SMUDGE", "1");
    skip_smudge.args(["-C", peer_name, "checkout", "main"]);
    git_cli::assert_git_success(
        &skip_smudge.output().expect("checkout skip-smudge"),
        "checkout main without smudging",
    );
    let pre_pull = fs::read(env.case_dir.join(peer_name).join(binary_name)).expect("peer pointer");
    assert!(
        pre_pull.starts_with(b"version https://git-lfs.github.com/spec/v1\n"),
        "peer checkout must keep the pointer before git lfs pull"
    );
    trunk_git_ok(&env.case_dir, None, &["-C", peer_name, "lfs", "pull"]);
    assert_eq!(
        fs::read(env.case_dir.join(peer_name).join(binary_name)).expect("pulled bytes"),
        payload,
        "peer LFS bytes must match source fixture"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
}

#[test]
fn integration_git_lfs_trunk_push_auth_token_round_trip() {
    // LF-03: trunk + push_auth=token — static token round trip; missing token fails.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let probe_token = git_cli::resolve_seed_token();
    assert_pinned_git_lfs(&probe_token);

    let token_dir = tempfile::tempdir().expect("token dir");
    let token_path = token_dir.path().join("ci.token");
    let push_token = format!("lf03-ci-{}", std::process::id());
    fs::write(&token_path, format!("{push_token}\n")).expect("write file-mounted token");
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
    let env = GitLfsEnv::with_config_append(&append);
    let (mut service, port, _stdout_path, stderr_path) =
        boot_service_http_trunk(&env, &[("MEGA_MONOREPO__PUSH_POLICY", "trunk")]);
    let payload = binary_payload();
    assert_host_git_lfs_pinned();

    // Unauthenticated LFS upload batch must 401 (observable write failure).
    let unauth = probe_lfs_upload_batch(port, "/project/foo", None);
    assert!(
        unauth.lines().next().is_some_and(|l| l.contains("401")),
        "push_auth=token must 401 LFS upload without a token, got:\n{unauth}"
    );

    seed_project_foo_trunk_with_token(&env.case_dir, port, &push_token);
    let foo_url = trunk_subpath_url(port, "/project/foo");
    let lfs_url = format!("{}/info/lfs", foo_url.trim_end_matches('/'));

    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &["clone", &foo_url, "lfs-token-src"],
    );
    configure_git_identity_trunk(&env.case_dir, Some(&push_token), "lfs-token-src");
    configure_lfs_trunk(&env.case_dir, Some(&push_token), "lfs-token-src", &lfs_url);
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &["-C", "lfs-token-src", "lfs", "track", "*.bin"],
    );
    let binary_name = "lf-03-token.bin";
    fs::write(
        env.case_dir.join("lfs-token-src").join(binary_name),
        &payload,
    )
    .expect("write token LFS fixture");
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &["-C", "lfs-token-src", "add", ".gitattributes", binary_name],
    );
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &[
            "-C",
            "lfs-token-src",
            "commit",
            "-m",
            "lf-03 trunk LFS token round trip",
        ],
    );
    git_cli::assert_git_success(
        &trunk_push_main(&env.case_dir, Some(&push_token), "lfs-token-src"),
        "token trunk LFS push to main",
    );

    let peer_name = "lfs-token-peer";
    fs::create_dir_all(env.case_dir.join(peer_name)).expect("peer dir");
    trunk_git_ok(&env.case_dir, Some(&push_token), &["-C", peer_name, "init"]);
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &["-C", peer_name, "remote", "add", "origin", &foo_url],
    );
    configure_lfs_trunk(&env.case_dir, Some(&push_token), peer_name, &lfs_url);
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &[
            "-C",
            peer_name,
            "fetch",
            "origin",
            "refs/heads/main:refs/heads/main",
        ],
    );
    let mut skip_smudge = trunk_host_git_command(&env.case_dir, Some(&push_token));
    skip_smudge.env("GIT_LFS_SKIP_SMUDGE", "1");
    skip_smudge.args(["-C", peer_name, "checkout", "main"]);
    git_cli::assert_git_success(
        &skip_smudge.output().expect("checkout skip-smudge"),
        "checkout main without smudging",
    );
    trunk_git_ok(
        &env.case_dir,
        Some(&push_token),
        &["-C", peer_name, "lfs", "pull"],
    );
    assert_eq!(
        fs::read(env.case_dir.join(peer_name).join(binary_name)).expect("pulled bytes"),
        payload,
        "peer LFS bytes must match source fixture"
    );

    assert_token_absent(&env.case_dir, push_token.as_bytes());

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    drop(token_dir);
}

fn trunk_boot_env() -> [(&'static str, &'static str); 3] {
    [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
    ]
}

fn trunk_subpath_url(port: u16, path: &str) -> String {
    format!(
        "{}/",
        git_cli::mega2_host_http_url(port, path).trim_end_matches('/')
    )
}

fn trunk_host_git_command(case_dir: &Path, token: Option<&str>) -> Command {
    let isolated_home = case_dir.join(match token {
        Some(_) => "git-home-token",
        None => "git-home-none",
    });
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
        .env("GIT_CONFIG_VALUE_4", "0");
    if let Some(token) = token {
        git_cli::write_git_askpass(&case_dir.join("git-askpass.sh"));
        command
            .env(git_cli::GIT_ASKPASS_ENV, token)
            .env("GIT_ASKPASS", case_dir.join("git-askpass.sh"))
            .env("GIT_CONFIG_COUNT", "6")
            .env("GIT_CONFIG_KEY_5", "credential.username")
            .env("GIT_CONFIG_VALUE_5", git_cli::DEFAULT_GIT_AUTH_USER);
    } else {
        command.env("GIT_ASKPASS", "true");
    }
    // Ensure host git-lfs (installed under ~/.local/bin) is on PATH.
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    let local_bin = dirs_next_home().join(".local/bin");
    if local_bin.is_dir() {
        let mut prefix = local_bin.into_os_string();
        prefix.push(":");
        prefix.push(&path);
        path = prefix;
    }
    command.env("PATH", path);
    command
}

fn dirs_next_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn trunk_host_git(case_dir: &Path, token: Option<&str>, git_args: &[&str]) -> std::process::Output {
    let mut command = trunk_host_git_command(case_dir, token);
    command.args(git_args);
    command.output().expect("host git")
}

fn trunk_git_ok(case_dir: &Path, token: Option<&str>, args: &[&str]) {
    git_cli::assert_git_success(
        &trunk_host_git(case_dir, token, args),
        &format!("git {}", args.join(" ")),
    );
}

fn trunk_push_main(case_dir: &Path, token: Option<&str>, repo: &str) -> std::process::Output {
    trunk_host_git(
        case_dir,
        token,
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

fn configure_git_identity_trunk(case_dir: &Path, token: Option<&str>, repo: &str) {
    trunk_git_ok(
        case_dir,
        token,
        &["-C", repo, "config", "user.name", "LF-03 Trunk LFS"],
    );
    trunk_git_ok(
        case_dir,
        token,
        &[
            "-C",
            repo,
            "config",
            "user.email",
            "lf-03-lfs@example.invalid",
        ],
    );
}

fn configure_lfs_trunk(case_dir: &Path, token: Option<&str>, repo: &str, lfs_url: &str) {
    trunk_git_ok(case_dir, token, &["-C", repo, "lfs", "install", "--local"]);
    trunk_git_ok(case_dir, token, &["-C", repo, "config", "lfs.url", lfs_url]);
    trunk_git_ok(
        case_dir,
        token,
        &["-C", repo, "config", "lfs.locksverify", "false"],
    );
}

fn seed_project_foo_trunk(case_dir: &Path, port: u16) {
    let project_url = trunk_subpath_url(port, "/project");
    trunk_git_ok(case_dir, None, &["clone", &project_url, "project-seed"]);
    configure_git_identity_trunk(case_dir, None, "project-seed");
    let foo_file = case_dir.join("project-seed").join("foo").join("seed.txt");
    fs::create_dir_all(foo_file.parent().expect("foo parent")).expect("mkdir foo");
    fs::write(&foo_file, "seed\n").expect("write foo/seed.txt");
    trunk_git_ok(
        case_dir,
        None,
        &["-C", "project-seed", "add", "foo/seed.txt"],
    );
    trunk_git_ok(
        case_dir,
        None,
        &["-C", "project-seed", "commit", "-m", "seed /project/foo"],
    );
    trunk_git_ok(
        case_dir,
        None,
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

fn seed_project_foo_trunk_with_token(case_dir: &Path, port: u16, token: &str) {
    let project_url = trunk_subpath_url(port, "/project");
    trunk_git_ok(
        case_dir,
        Some(token),
        &["clone", &project_url, "project-seed"],
    );
    configure_git_identity_trunk(case_dir, Some(token), "project-seed");
    let foo_file = case_dir.join("project-seed").join("foo").join("seed.txt");
    fs::create_dir_all(foo_file.parent().expect("foo parent")).expect("mkdir foo");
    fs::write(&foo_file, "seed\n").expect("write foo/seed.txt");
    trunk_git_ok(
        case_dir,
        Some(token),
        &["-C", "project-seed", "add", "foo/seed.txt"],
    );
    trunk_git_ok(
        case_dir,
        Some(token),
        &["-C", "project-seed", "commit", "-m", "seed /project/foo"],
    );
    trunk_git_ok(
        case_dir,
        Some(token),
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

fn probe_lfs_upload_batch(port: u16, repo_path: &str, bearer: Option<&str>) -> String {
    let path = format!("{}/info/lfs/objects/batch", repo_path.trim_end_matches('/'));
    let url = format!("http://127.0.0.1:{port}{path}");
    let body = r#"{"operation":"upload","transfers":["basic"],"objects":[{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1}],"hash_algo":"sha256"}"#;
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "-D",
        "-",
        "-o",
        "/dev/null",
        "--max-time",
        "15",
        "-H",
        "Content-Type: application/vnd.git-lfs+json",
        "-H",
        "Accept: application/vnd.git-lfs+json",
        "-d",
        body,
    ]);
    if let Some(token) = bearer {
        command
            .arg("-H")
            .arg(format!("Authorization: Bearer {token}"));
    }
    command.arg(&url);
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("curl LFS batch probe failed to spawn: {err}"));
    assert!(
        output.status.success(),
        "curl LFS batch probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_pinned_git_lfs(token: &str) {
    let work_root = git_cli::git_cli_workdir();
    fs::create_dir_all(&work_root)
        .unwrap_or_else(|err| panic!("create shared git workdir {}: {err}", work_root.display()));
    let probe_dir = tempfile::Builder::new()
        .prefix("lfs-probe-")
        .tempdir_in(&work_root)
        .expect("create Git LFS probe dir");
    let output = git_cli::git_cli(probe_dir.path(), token, &["lfs", "version"]);
    git_cli::assert_git_success(&output, "probe pinned Git LFS runner");
    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        version, "git-lfs/3.7.1",
        "git-cli runner must provide the pinned Git LFS version"
    );
}

fn assert_host_git_lfs_pinned() {
    let mut command = Command::new("git");
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    let local_bin = dirs_next_home().join(".local/bin");
    if local_bin.is_dir() {
        let mut prefix = local_bin.into_os_string();
        prefix.push(":");
        prefix.push(&path);
        path = prefix;
    }
    command.env("PATH", path);
    command.args(["lfs", "version"]);
    let output = command.output().expect("host git lfs version");
    git_cli::assert_git_success(&output, "probe host Git LFS");
    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        version, "git-lfs/3.7.1",
        "host git-lfs used by trunk LFS IT must be the pinned version"
    );
}

fn configure_git_identity(case_dir: &Path, token: &str, repo_name: &str) {
    git_cli::assert_git_success(
        &git_cli::git_cli(
            case_dir,
            token,
            &["-C", repo_name, "config", "user.name", "GM-05 Git LFS"],
        ),
        "configure LFS user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            case_dir,
            token,
            &[
                "-C",
                repo_name,
                "config",
                "user.email",
                "gm-05-lfs@example.invalid",
            ],
        ),
        "configure LFS user.email",
    );
}

fn configure_lfs(case_dir: &Path, token: &str, repo_name: &str, lfs_url: &str) {
    git_cli::assert_git_success(
        &git_cli::git_cli(
            case_dir,
            token,
            &["-C", repo_name, "lfs", "install", "--local"],
        ),
        "install repository-local Git LFS hooks",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            case_dir,
            token,
            &["-C", repo_name, "config", "lfs.url", lfs_url],
        ),
        "configure explicit HTTP LFS endpoint",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            case_dir,
            token,
            &["-C", repo_name, "config", "lfs.locksverify", "false"],
        ),
        "disable LFS lock verification for round trip",
    );
}

fn binary_payload() -> Vec<u8> {
    (0..16_384)
        .map(|index| ((index * 37 + index / 11) % 256) as u8)
        .collect()
}

fn ls_remote_cl_refs(case_dir: &Path, token: &str, remote_url: &str) -> Vec<String> {
    let output = git_cli::git_cli(case_dir, token, &["ls-remote", remote_url, "refs/cl/*"]);
    git_cli::assert_git_success(&output, "list remote CL refs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        // Ignore peeled tip advertisements (`refs/cl/ID^{}`) when counting new CLs.
        .filter(|name| !name.ends_with("^{}"))
        .collect()
}

fn assert_lfs_tmp_clean(repo: &Path) {
    for relative in [".git/lfs/tmp", ".git/lfs/incomplete"] {
        let path = repo.join(relative);
        if !path.exists() {
            continue;
        }
        let mut entries = fs::read_dir(&path)
            .unwrap_or_else(|err| panic!("read LFS temp dir {}: {err}", path.display()));
        assert!(
            entries.next().is_none(),
            "LFS temp directory must be empty at case end: {}",
            path.display()
        );
    }
}

fn assert_token_absent(root: &Path, token: &[u8]) {
    for entry in fs::read_dir(root).unwrap_or_else(|err| panic!("read {}: {err}", root.display())) {
        let entry = entry.expect("case dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_token_absent(&path, token);
            continue;
        }
        let bytes = fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        assert!(
            !bytes.windows(token.len()).any(|window| window == token),
            "temporary credential leaked into {}",
            path.display()
        );
    }
}

fn boot_service_http(env: &GitLfsEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_extra_env(env, &[])
}

fn boot_service_http_with_extra_env(
    env: &GitLfsEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_options(env, extra_env, false)
}

/// Trunk LFS IT drives host git against `127.0.0.1`. Force the advertised
/// public base to the loopback URL so batch hrefs stay reachable even when the
/// compose git-cli runner would otherwise inject `host.docker.internal`.
fn boot_service_http_trunk(
    env: &GitLfsEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_http_with_options(env, extra_env, true)
}

fn boot_service_http_with_options(
    env: &GitLfsEnv,
    extra_env: &[(&str, &str)],
    force_loopback_public_base: bool,
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    for (key, value) in extra_env {
        command.env(*key, *value);
    }
    if force_loopback_public_base {
        command.env(
            "MEGA_HTTP__PUBLIC_BASE_URL",
            format!("http://127.0.0.1:{port}"),
        );
    } else {
        git_cli::apply_mega2_public_http_base_env(&mut command, port);
    }
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

// ------------------------------------------------------------------
// WH-05 (plan-20260912): `lfs.object.uploaded` on the real binary — basic
// upload emits one bounded attempt; the presigned direct-upload path is the
// documented DEFER-WH-01 gap and emits nothing.
// ------------------------------------------------------------------

const WH05_SECRET_REF: &str = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value";
const WH05_HMAC_VALUE: &str =
    "hex:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";
const WH05_SENTINEL_PAYLOAD: &str =
    "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

const WH05_EVENTS_APPEND: &str = r#"
[storage_events]
enabled = true
installation_id = "it-wh05-process"

[[storage_events.targets]]
id = "ops-main"
url = "https://events.example.invalid/ingest"
secret_ref = "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
events = ["lfs.object.uploaded"]
include_unscoped_lfs = true
"#;

/// Seed the target HMAC secret through the real `config secret set` CLI flow.
fn wh05_seed_target_secret(env: &GitLfsEnv) {
    let bootstrap_path = env.temp_dir.path().join("wh05-bootstrap-config.toml");
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
        env.temp_dir.path().join("wh05-seed.out"),
        env.temp_dir.path().join("wh05-seed.err"),
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
        .write_all(WH05_HMAC_VALUE.as_bytes())
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

/// Bounded wait for a log line across both captured streams.
fn wh05_wait_log_line(
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

/// SIGINT -> receipt observed in a bounded window -> exit 0 (WH-13 ordering),
/// then return the full captured logs for content assertions.
fn wh05_shutdown_and_capture(
    service: &mut ServiceProcess,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    let pid = service.pid() as libc::pid_t;
    // SAFETY: SIGINT to a child we own; WH-13 routes it to the async layer.
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    wh05_wait_log_line(
        stdout_path,
        stderr_path,
        "storage_events_shutdown_complete",
        Duration::from_secs(60),
    );
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

/// Emitter-side delivery outcome lines (`category=…`; the transport logs its
/// own `outcome=…` line alongside).
fn wh05_emitter_delivery_count(captured: &str) -> usize {
    captured
        .lines()
        .filter(|line| line.contains("storage_events delivery") && line.contains("category="))
        .count()
}

fn wh05_assert_logs_sanitized(captured: &str) {
    assert!(
        !captured.contains(WH05_HMAC_VALUE) && !captured.contains(WH05_SENTINEL_PAYLOAD),
        "captured logs must not contain the seeded secret in either form"
    );
    assert!(
        !captured.contains(WH05_SECRET_REF),
        "captured logs must not contain the full SecretRef URI"
    );
}

/// One LFS round trip in trunk + push_auth=none morphology: track a binary,
/// push to main, peer pulls and bytes match. Returns the captured service
/// logs after a clean shutdown.
fn wh05_lfs_round_trip(
    env: &GitLfsEnv,
    extra_object_env: &[(&str, &str)],
    case_label: &str,
    payload: &[u8],
) -> String {
    let port = git_cli::reserve_ephemeral_port();
    let (stdout_path, stderr_path) = (
        env.temp_dir.path().join(format!("{case_label}.out")),
        env.temp_dir.path().join(format!("{case_label}.err")),
    );
    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    for (key, value) in trunk_boot_env() {
        command.env(key, value);
    }
    for (key, value) in extra_object_env {
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

    seed_project_foo_trunk(&env.case_dir, port);
    let foo_url = trunk_subpath_url(port, "/project/foo");
    let lfs_url = format!("{}/info/lfs", foo_url.trim_end_matches('/'));
    let src = format!("lfs-wh05-{case_label}-src");
    trunk_git_ok(&env.case_dir, None, &["clone", &foo_url, &src]);
    configure_git_identity_trunk(&env.case_dir, None, &src);
    configure_lfs_trunk(&env.case_dir, None, &src, &lfs_url);
    trunk_git_ok(&env.case_dir, None, &["-C", &src, "lfs", "track", "*.bin"]);
    let binary_name = format!("wh05-{case_label}.bin");
    fs::write(env.case_dir.join(&src).join(&binary_name), payload).expect("write LFS fixture");
    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", &src, "add", ".gitattributes", &binary_name],
    );
    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", &src, "commit", "-m", "wh05 lfs round trip"],
    );
    git_cli::assert_git_success(
        &trunk_push_main(&env.case_dir, None, &src),
        "anonymous trunk LFS push to main",
    );

    let peer = format!("lfs-wh05-{case_label}-peer");
    fs::create_dir_all(env.case_dir.join(&peer)).expect("peer dir");
    trunk_git_ok(&env.case_dir, None, &["-C", &peer, "init"]);
    trunk_git_ok(
        &env.case_dir,
        None,
        &["-C", &peer, "remote", "add", "origin", &foo_url],
    );
    configure_lfs_trunk(&env.case_dir, None, &peer, &lfs_url);
    trunk_git_ok(
        &env.case_dir,
        None,
        &[
            "-C",
            &peer,
            "fetch",
            "origin",
            "refs/heads/main:refs/heads/main",
        ],
    );
    let mut skip_smudge = trunk_host_git_command(&env.case_dir, None);
    skip_smudge.env("GIT_LFS_SKIP_SMUDGE", "1");
    skip_smudge.args(["-C", &peer, "checkout", "main"]);
    git_cli::assert_git_success(
        &skip_smudge.output().expect("checkout skip-smudge"),
        "checkout main",
    );
    trunk_git_ok(&env.case_dir, None, &["-C", &peer, "lfs", "pull"]);
    assert_eq!(
        fs::read(env.case_dir.join(&peer).join(&binary_name)).expect("pulled bytes"),
        payload,
        "peer LFS bytes must match source fixture"
    );

    wh05_shutdown_and_capture(&mut service, &stdout_path, &stderr_path)
}

#[test]
fn integration_git_lfs_storage_events_basic_upload() {
    // Local object storage => the batch issues basic upload URLs, so the
    // in-process handler stores the object and emits exactly one event.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs"
    );
    git_cli::require_git_cli_runner();
    assert_host_git_lfs_pinned();

    let env = GitLfsEnv::with_config_append(&format!(
        "\n[git]\nanonymous_access = true\n{WH05_EVENTS_APPEND}"
    ));
    wh05_seed_target_secret(&env);
    let captured = wh05_lfs_round_trip(&env, &[], "basic", &wh05_unique_payload());
    assert_eq!(
        wh05_emitter_delivery_count(&captured),
        1,
        "exactly one delivery for the basic upload:\n{captured}"
    );
    wh05_assert_logs_sanitized(&captured);
}

#[test]
fn integration_git_lfs_storage_events_presigned_gap() {
    // RustFS (S3-compatible) object storage => the batch issues presigned PUT
    // URLs and the client uploads directly, bypassing the in-process handler
    // (DEFER-WH-01): zero events, but the round trip must still succeed.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs"
    );
    git_cli::require_git_cli_runner();
    assert_host_git_lfs_pinned();

    let env = GitLfsEnv::with_config_append(&format!(
        "\n[git]\nanonymous_access = true\n{WH05_EVENTS_APPEND}"
    ));
    wh05_seed_target_secret(&env);
    // Unique payload per run: the persistent RustFS bucket must not already
    // hold this OID, otherwise batch would omit the upload action and the
    // case would pass without exercising the presigned PUT.
    let payload = wh05_unique_payload();
    let oid = hex::encode(sha2::Sha256::digest(&payload));

    let s3_env: [(&str, &str); 6] = [
        ("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "s3compatible"),
        ("MEGA_OBJECT_STORAGE__S3__REGION", "us-east-1"),
        ("MEGA_OBJECT_STORAGE__S3__BUCKET", "mega2"),
        (
            "MEGA_OBJECT_STORAGE__S3__ENDPOINT_URL",
            "http://127.0.0.1:19000",
        ),
        ("MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID", "rustfs"),
        (
            "MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY",
            "rustfs_secret",
        ),
    ];
    // Boot a probe service on the same DB and ask the batch endpoint for
    // this OID first: the response MUST carry a presigned upload action
    // pointing at the RustFS endpoint (proof the client upload bypasses the
    // in-process handler before we assert zero events).
    let probe_port = git_cli::reserve_ephemeral_port();
    let (probe_out, probe_err) = (
        env.temp_dir.path().join("presigned-probe.out"),
        env.temp_dir.path().join("presigned-probe.err"),
    );
    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    for (key, value) in trunk_boot_env() {
        command.env(key, value);
    }
    for (key, value) in &s3_env {
        command.env(*key, *value);
    }
    command.args([
        "service",
        "http",
        "--host",
        "127.0.0.1",
        "-p",
        &probe_port.to_string(),
    ]);
    command
        .stdout(Stdio::from(create_log_file(&probe_out)))
        .stderr(Stdio::from(create_log_file(&probe_err)));
    let mut probe = ServiceProcess::spawn(command);
    probe.wait_until_listening(probe_port, Duration::from_secs(90), &probe_out, &probe_err);
    let batch_url = format!(
        "{}/info/lfs/objects/batch",
        trunk_subpath_url(probe_port, "/project/foo").trim_end_matches('/')
    );
    let batch = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("batch client")
        .post(&batch_url)
        .header("Content-Type", "application/vnd.git-lfs+json")
        .body(
            serde_json::json!({
                "operation": "upload",
                "transfers": ["basic"],
                "hash_algo": "sha256",
                "objects": [{"oid": oid, "size": payload.len()}]
            })
            .to_string(),
        )
        .send()
        .expect("batch request");
    assert_eq!(batch.status().as_u16(), 200, "batch must succeed");
    let batch_json: serde_json::Value = batch.json().expect("batch json");
    let upload_href = batch_json["objects"][0]["actions"]["upload"]["href"]
        .as_str()
        .expect("batch must carry an upload action for the fresh OID")
        .to_owned();
    assert!(
        upload_href.contains("127.0.0.1:19000"),
        "upload action must be a presigned RustFS URL, got {upload_href}"
    );
    let probe_status = probe.shutdown_via_sigint(Duration::from_secs(60));
    assert!(probe_status.success(), "probe service clean shutdown");

    let captured = wh05_lfs_round_trip(&env, &s3_env, "presigned", &payload);
    assert_eq!(
        wh05_emitter_delivery_count(&captured),
        0,
        "presigned direct upload emits nothing (DEFER-WH-01):\n{captured}"
    );
    wh05_assert_logs_sanitized(&captured);
}

/// Unique-per-run payload so the persistent RustFS bucket never already holds
/// the OID (presigned-gap case must exercise a real upload every run).
fn wh05_unique_payload() -> Vec<u8> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!(
        "wh05 unique payload {nanos} {}
",
        std::process::id()
    )
    .into_bytes()
}
