// Process-level HTTP Git LFS round-trip integration test (GM-05).
//
// Starts a real `service http`, drives the pinned compose Git/Git LFS runner,
// pushes an LFS-tracked binary into a MonoRepo CL ref, then performs a directed
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
    io::Read,
    net::{TcpListener, TcpStream},
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
            "monoengine_git_lfs_{}_{}",
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

        common::write_full_config_with_append(&full_config_path, "");
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
        let child = command.spawn().expect("spawn monoengine service");
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
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_lfs; skipping is not a green path"
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

        let remote_url = format!("http://127.0.0.1:{port}/");
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
    let port = reserve_free_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
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
