// Process-level SSH service lifecycle integration test (GM-06).
//
// Boots `service ssh` with ADR-GM-05 cargo-native self-start topology:
// ephemeral port from 127.0.0.1:0 → --ssh-port, per-case MEGA_BASE_DIR under
// CASE/ssh/base, Vault host-key ciphertext only in the temp DB, then
// SIGINT/kill cleanup that leaves the port refusing and CASE/ssh gone.

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
    net::TcpStream,
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
            "monoengine_git_ssh_{}_{}",
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

struct GitSshEnv {
    /// Scratch logs / object root (not the ADR MEGA_BASE_DIR).
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    /// ADR: `CASE/ssh/base` → Vault core key at `…/vault/core_key.json`.
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
    ssh_dir: PathBuf,
}

impl GitSshEnv {
    fn new() -> Self {
        let work_root = git_cli::git_cli_workdir();
        fs::create_dir_all(&work_root).unwrap_or_else(|err| {
            panic!("create shared git workdir {}: {err}", work_root.display())
        });
        let case_name = format!(
            "ssh-case-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let case_dir = work_root.join(case_name);
        if case_dir.exists() {
            fs::remove_dir_all(&case_dir).expect("clean stale SSH case dir");
        }
        fs::create_dir_all(&case_dir).expect("create SSH case dir");

        let ssh_dir = case_dir.join("ssh");
        let base_dir = ssh_dir.join("base");
        let cache_dir = ssh_dir.join("cache");
        fs::create_dir_all(&base_dir).expect("create CASE/ssh/base");
        fs::create_dir_all(&cache_dir).expect("create CASE/ssh/cache");

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let full_config_path = common::write_case_config(&case_dir);
        let object_root = temp_dir.path().join("objects");
        fs::create_dir_all(&object_root).expect("create object root");

        Self {
            temp_dir,
            database,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
            case_dir,
            ssh_dir,
        }
    }

    fn full_config_command(&self) -> Command {
        let mut command = isolated_command(&self.case_dir, &self.base_dir, &self.cache_dir);
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

    fn core_key_path(&self) -> PathBuf {
        self.base_dir.join("vault").join("core_key.json")
    }
}

impl Drop for GitSshEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.ssh_dir);
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
fn integration_git_ssh_service_lifecycle_isolated() {
    // GM-06: no SKIP-green path. Lifecycle does not need the git-cli runner,
    // but an explicit skip request must still fail the test.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );

    // SSH service lifecycle allowlist markers (ADR-GM-06 / Deliverables).
    // Keep these literal fixed strings in this file for Verification set-equality.
    const _: &str = "ephemeral_port_from_127.0.0.1:0_to_--ssh-port";
    const _: &str = "listen_within_90s";
    const _: &str = "vault_core_key=CASE/ssh/base/vault/core_key.json";
    const _: &str = "ssh_server_key_ciphertext_in_temp_db_only";
    const _: &str = "SIGINT_then_kill_wait_le_10s";
    const _: &str = "cleanup_invariant=port_refuses_and_temp_db_and_CASE/ssh_absent";

    let (admin_url, db_name, ssh_dir, port) = {
        let env = GitSshEnv::new();
        let admin_url = env.database.admin_url.clone();
        let db_name = env.database.db_name.clone();
        let ssh_dir = env.ssh_dir.clone();
        let core_key = env.core_key_path();

        // ephemeral_port_from_127.0.0.1:0_to_--ssh-port
        let (mut service, port, _stdout_path, stderr_path) = boot_service_ssh(&env);
        // listen_within_90s (enforced inside boot_service_ssh)
        let service_pid = service.pid();

        // vault_core_key=CASE/ssh/base/vault/core_key.json
        assert!(
            core_key.is_file(),
            "vault_core_key=CASE/ssh/base/vault/core_key.json missing at {}",
            core_key.display()
        );

        // ssh_server_key_ciphertext_in_temp_db_only
        git_cli::assert_ssh_server_key_ciphertext_in_db(&env.database.db_url);
        git_cli::assert_no_openssh_private_key_under(&env.ssh_dir);

        // SIGINT_then_kill_wait_le_10s
        let status = service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "service did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&stderr_path),
        );
        git_cli::assert_process_reaped(service_pid);
        git_cli::wait_until_port_closed(port, Duration::from_secs(5));
        drop(service);
        drop(env);

        (admin_url, db_name, ssh_dir, port)
    };

    // cleanup_invariant=port_refuses_and_temp_db_and_CASE/ssh_absent
    git_cli::assert_port_refuses(port);
    git_cli::assert_database_absent(&admin_url, &db_name);
    assert!(
        !ssh_dir.exists(),
        "cleanup_invariant=port_refuses_and_temp_db_and_CASE/ssh_absent: ssh dir still present at {}",
        ssh_dir.display()
    );
}

fn boot_service_ssh(env: &GitSshEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    // ephemeral_port_from_127.0.0.1:0_to_--ssh-port
    let port = git_cli::reserve_ephemeral_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    let port_arg = port.to_string();
    command.args([
        "service",
        "ssh",
        "--host",
        "127.0.0.1",
        "--ssh-port",
        &port_arg,
    ]);
    command
        .stdout(Stdio::from(create_log_file(&stdout_path)))
        .stderr(Stdio::from(create_log_file(&stderr_path)));

    let mut service = ServiceProcess::spawn(command);
    // listen_within_90s
    service.wait_until_listening(port, Duration::from_secs(90), &stdout_path, &stderr_path);
    (service, port, stdout_path, stderr_path)
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
