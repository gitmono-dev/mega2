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
    collections::BTreeMap,
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
        Self::with_config_append("")
    }

    fn with_config_append(append: &str) -> Self {
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
        let full_config_path = {
            let path = case_dir.join("config.toml");
            common::write_full_config_with_append(&path, append);
            path
        };
        // Keep objects under CASE/ssh/base so `${base_dir}/objects` and the
        // MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR override always resolve to the
        // same tree (avoids pack generation looking for hashes that were
        // written to a different root).
        let object_root = base_dir.join("objects");
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
            // Isolate IT from shared compose Redis git-object cache poisoning across
            // cases / consecutive upload-pack sessions (see GitObjectCache).
            .env("MEGA_GIT_OBJECT_CACHE_PREFIX", "disabled")
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

#[test]
fn integration_git_ssh_authenticated_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    // SSH client fixture allowlist markers (ADR-GM-06 / Deliverables).
    const _: &str = "client_key_mode=0600";
    const _: &str = "ssh_keys_row_matches_keypair";
    const _: &str = "finger=ssh-keygen_-lf_sha256_col2";
    const _: &str = "known_hosts=case_port_only";
    const _: &str = "GIT_SSH_COMMAND=ssh -i CASE/ssh/client_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=CASE/ssh/known_hosts -o StrictHostKeyChecking=yes -p PORT";

    let env = GitSshEnv::new();
    let client_key = env.ssh_dir.join("client_ed25519");
    let client_pub = env.ssh_dir.join("client_ed25519.pub");
    let known_hosts = env.ssh_dir.join("known_hosts");

    // client_key_mode=0600
    git_cli::generate_client_ed25519(&client_key);
    let pubkey = fs::read_to_string(&client_pub).expect("read client public key");
    // finger=ssh-keygen_-lf_sha256_col2
    let finger = git_cli::ssh_fingerprint_sha256_col2(&client_pub);

    // ADR-GM-05: migrations must complete before ssh_keys seed; seed before the
    // Git-facing SSH listen. First boot applies migrations + Vault host key, then
    // we shut down, seed, and boot again for the authenticated clone.
    {
        let (mut migrate_service, _migrate_port, _out, err) = boot_service_ssh(&env);
        let status = migrate_service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "migrate boot did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&err),
        );
        drop(migrate_service);
    }

    git_cli::seed_ssh_key(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        "gm-06a",
        pubkey.trim(),
        &finger,
    );
    // ssh_keys_row_matches_keypair
    git_cli::assert_ssh_keys_row_matches_keypair(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        &finger,
    );

    let (mut service, port, _stdout_path, stderr_path) = boot_service_ssh(&env);
    let service_pid = service.pid();

    // known_hosts=case_port_only
    git_cli::write_known_hosts_via_keyscan(&known_hosts, port);
    let known_body = fs::read_to_string(&known_hosts).expect("read known_hosts");
    let known_host = git_cli::monoengine_reachable_host();
    assert!(
        known_body
            .lines()
            .all(|line| { line.contains(known_host) || line.starts_with('#') || line.is_empty() }),
        "known_hosts=case_port_only must only describe {known_host} for this case"
    );

    let git_ssh = git_cli::git_ssh_command(&env.case_dir, port);
    assert!(
        git_ssh.contains(&format!("-p {port}")),
        "GIT_SSH_COMMAND must pin the case port: {git_ssh}"
    );

    let remote = git_cli::monoengine_ssh_repo_url(port, git_cli::DEFAULT_SSH_AUTH_USER);
    let clone_name = "ssh-auth-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "authenticated SSH clone",
    );

    let clone_dir = env.case_dir.join(clone_name);
    let actual = snapshot_workdir(&clone_dir);
    assert_eq!(
        actual,
        expected_init_monorepo_fixture(),
        "authenticated clone worktree must match the complete seeded monorepo fixture byte-for-byte"
    );

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
}

fn git_ssh_command_loopback_password(case_dir: &Path, port: u16) -> String {
    let known = case_dir.join("ssh").join("known_hosts");
    // BatchMode must stay off: OpenSSH disables password/ASKPASS when BatchMode=yes.
    format!(
        "ssh -o IdentitiesOnly=yes -o IdentityFile=/dev/null -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o BatchMode=no -p {port}",
        known.display(),
    )
}

fn git_ssh_command_loopback(case_dir: &Path, port: u16, none_only: bool) -> String {
    let key = case_dir.join("ssh").join("client_ed25519");
    let known = case_dir.join("ssh").join("known_hosts");
    let mut cmd = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -o BatchMode=yes -p {port}",
        key.display(),
        known.display(),
    );
    if none_only {
        cmd.push_str(" -o PreferredAuthentications=none -o PubkeyAuthentication=no -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no");
    } else {
        cmd.push_str(" -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no");
    }
    cmd
}

fn git_host_ssh(case_dir: &Path, git_ssh: &str, git_args: &[&str]) -> std::process::Output {
    let isolated_home = case_dir.join("git-home-ssh-loopback");
    fs::create_dir_all(&isolated_home).expect("create isolated git HOME");
    let mut command = Command::new("timeout");
    command
        .args(["-k", "5", "45"])
        .arg("git")
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("HOME", &isolated_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", git_ssh)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_0", "false")
        .args(git_args);
    command.output().expect("host git via timeout")
}

fn write_known_hosts_via_host_keyscan(known_hosts: &Path, port: u16) {
    if let Some(parent) = known_hosts.parent() {
        fs::create_dir_all(parent).expect("create known_hosts parent");
    }
    let port_arg = port.to_string();
    let output = Command::new("ssh-keyscan")
        .args(["-T", "5", "-p", &port_arg, "127.0.0.1"])
        .output()
        .expect("spawn host ssh-keyscan");
    let body = String::from_utf8_lossy(&output.stdout);
    assert!(
        !body.trim().is_empty(),
        "host ssh-keyscan produced empty known_hosts for port {port}; status={:?} stderr={} stdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        body,
    );
    fs::write(known_hosts, body.as_bytes()).expect("write known_hosts");
}

fn boot_storage_only_ssh(
    env: &GitSshEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    {
        let (mut migrate_service, _migrate_port, _out, err) =
            boot_service_ssh_with_env(env, extra_env);
        let status = migrate_service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "migrate boot did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&err),
        );
        drop(migrate_service);
    }
    let (service, port, stdout_path, stderr_path) = boot_service_ssh_with_env(env, extra_env);
    write_known_hosts_via_host_keyscan(&env.ssh_dir.join("known_hosts"), port);
    (service, port, stdout_path, stderr_path)
}

#[test]
fn integration_git_ssh_trunk_none_anon_on_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::with_config_append(
        r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false
"#,
    );
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    let extra = [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
        ("MEGA_GIT__ANONYMOUS_ACCESS", "true"),
    ];
    let (mut service, port, stdout_path, stderr_path) = boot_storage_only_ssh(&env, &extra);
    let git_ssh = git_ssh_command_loopback(&env.case_dir, port, true);
    let remote = format!("ssh://git@127.0.0.1:{port}/");
    let output = git_host_ssh(
        &env.case_dir,
        &git_ssh,
        &["clone", &remote, "ssh-none-anon-on"],
    );
    if !output.status.success() {
        panic!(
            "storage-only none + anonymous SSH clone failed ({})\nstdout:\n{}\nstderr:\n{}\n--- service stdout ---\n{}\n--- service stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            read_log(&stdout_path),
            read_log(&stderr_path),
        );
    }

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
fn integration_git_ssh_trunk_none_anon_off_clone_fail() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::with_config_append(
        r#"
[git]
anonymous_access = false
push_auth = "none"
ssh_receive_pack = false
"#,
    );
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    let extra = [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
        ("MEGA_GIT__ANONYMOUS_ACCESS", "false"),
    ];
    let (mut service, port, _stdout_path, stderr_path) = boot_storage_only_ssh(&env, &extra);
    let git_ssh = git_ssh_command_loopback(&env.case_dir, port, false);
    let remote = format!("ssh://git@127.0.0.1:{port}/");
    let output = git_host_ssh(
        &env.case_dir,
        &git_ssh,
        &["clone", &remote, "ssh-none-anon-off"],
    );
    assert!(
        !output.status.success(),
        "none + anonymous_access=false SSH clone must fail; status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
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
fn integration_git_ssh_trunk_token_anon_on_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::with_config_append(
        r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "agent-ci"
token = "sp01-unused-for-anon-clone"
"#,
    );
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    let extra = [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "token"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
        ("MEGA_GIT__ANONYMOUS_ACCESS", "true"),
    ];
    let (mut service, port, _stdout_path, stderr_path) = boot_storage_only_ssh(&env, &extra);
    let git_ssh = git_ssh_command_loopback(&env.case_dir, port, true);
    let remote = format!("ssh://git@127.0.0.1:{port}/");
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["clone", &remote, "ssh-token-anon-on"],
        ),
        "storage-only token + anonymous SSH clone without password",
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

const SP02_PUSH_TOKEN: &str = "sp02-it-password-token";

fn boot_token_anon_off_ssh() -> (GitSshEnv, ServiceProcess, u16, PathBuf, PathBuf, String) {
    let env = GitSshEnv::with_config_append(&format!(
        r#"
[git]
anonymous_access = false
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "agent-ci"
token = "{SP02_PUSH_TOKEN}"
"#
    ));
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    let extra = [("MEGA_MONOREPO__PUSH_POLICY", "trunk")];
    let (service, port, stdout, stderr) = boot_storage_only_ssh(&env, &extra);
    (
        env,
        service,
        port,
        stdout,
        stderr,
        format!("ssh://git@127.0.0.1:{port}/"),
    )
}

fn ls_remote_head(case_dir: &Path, git_ssh: &str, remote: &str, password: Option<&str>) -> String {
    let output = match password {
        Some(password) => git_cli::git_cli_ssh_with_password(
            case_dir,
            git_ssh,
            password,
            &["ls-remote", remote, "HEAD"],
        ),
        None => git_host_ssh(case_dir, git_ssh, &["ls-remote", remote, "HEAD"]),
    };
    git_cli::assert_git_success(&output, "ls-remote HEAD");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

fn prepare_review_pubkey_loopback(
    anonymous_access: bool,
) -> (GitSshEnv, ServiceProcess, u16, PathBuf, String, String) {
    let env = GitSshEnv::with_config_append(&format!(
        r#"
[git]
anonymous_access = {anonymous_access}
"#
    ));
    let client_key = env.ssh_dir.join("client_ed25519");
    let client_pub = env.ssh_dir.join("client_ed25519.pub");
    git_cli::generate_client_ed25519(&client_key);
    let pubkey = fs::read_to_string(&client_pub).expect("read client public key");
    let finger = git_cli::ssh_fingerprint_sha256_col2(&client_pub);
    let extra = [(
        "MEGA_GIT__ANONYMOUS_ACCESS",
        if anonymous_access { "true" } else { "false" },
    )];
    {
        let (mut migrate_service, _migrate_port, _out, err) =
            boot_service_ssh_with_env(&env, &extra);
        let status = migrate_service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "migrate boot did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&err),
        );
        drop(migrate_service);
    }
    git_cli::seed_ssh_key(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        "sp02-review",
        pubkey.trim(),
        &finger,
    );
    let (service, port, _stdout, stderr) = boot_service_ssh_with_env(&env, &extra);
    write_known_hosts_via_host_keyscan(&env.ssh_dir.join("known_hosts"), port);
    let git_ssh = git_ssh_command_loopback(&env.case_dir, port, false);
    let remote = format!("ssh://{}@127.0.0.1:{port}/", git_cli::DEFAULT_SSH_AUTH_USER);
    (env, service, port, stderr, git_ssh, remote)
}

#[test]
fn integration_git_ssh_trunk_token_anon_off_clone_fail() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, port, _stdout_path, stderr_path, remote) = boot_token_anon_off_ssh();
    let git_ssh = git_ssh_command_loopback(&env.case_dir, port, false);
    let output = git_host_ssh(
        &env.case_dir,
        &git_ssh,
        &["clone", &remote, "ssh-token-anon-off-fail"],
    );
    assert!(
        !output.status.success(),
        "token + anonymous_access=false without password must fail; status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
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
fn integration_git_ssh_trunk_token_anon_off_password_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, port, stdout_path, stderr_path, remote) = boot_token_anon_off_ssh();
    let git_ssh = git_ssh_command_loopback_password(&env.case_dir, port);
    let output = git_cli::git_cli_ssh_with_password(
        &env.case_dir,
        &git_ssh,
        SP02_PUSH_TOKEN,
        &["clone", &remote, "ssh-token-password-clone"],
    );
    if !output.status.success() {
        panic!(
            "token + password SSH clone failed ({})\nstdout:\n{}\nstderr:\n{}\n--- service stdout ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            read_log(&stdout_path),
        );
    }
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
fn integration_git_ssh_trunk_token_anon_off_password_fetch() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, port, _stdout_path, stderr_path, remote) = boot_token_anon_off_ssh();
    let git_ssh = git_ssh_command_loopback_password(&env.case_dir, port);
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh_with_password(
            &env.case_dir,
            &git_ssh,
            SP02_PUSH_TOKEN,
            &["clone", &remote, "ssh-token-password-fetch"],
        ),
        "clone before password fetch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh_with_password(
            &env.case_dir,
            &git_ssh,
            SP02_PUSH_TOKEN,
            &["-C", "ssh-token-password-fetch", "fetch", "origin"],
        ),
        "token + password SSH fetch",
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
fn integration_git_ssh_trunk_token_anon_off_password_pull() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, port, _stdout_path, stderr_path, remote) = boot_token_anon_off_ssh();
    let git_ssh = git_ssh_command_loopback_password(&env.case_dir, port);
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh_with_password(
            &env.case_dir,
            &git_ssh,
            SP02_PUSH_TOKEN,
            &["clone", &remote, "ssh-token-password-pull"],
        ),
        "clone before password pull",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh_with_password(
            &env.case_dir,
            &git_ssh,
            SP02_PUSH_TOKEN,
            &["-C", "ssh-token-password-pull", "pull", "--ff-only"],
        ),
        "token + password SSH pull",
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
fn integration_git_ssh_trunk_token_push_receive_pack_disabled() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, port, _stdout_path, stderr_path, remote) = boot_token_anon_off_ssh();
    let git_ssh = git_ssh_command_loopback_password(&env.case_dir, port);
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh_with_password(
            &env.case_dir,
            &git_ssh,
            SP02_PUSH_TOKEN,
            &["clone", &remote, "ssh-token-rp-disabled"],
        ),
        "clone before receive-pack negative",
    );
    let tip_before = ls_remote_head(&env.case_dir, &git_ssh, &remote, Some(SP02_PUSH_TOKEN));
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-token-rp-disabled",
                "config",
                "user.name",
                "SP-02",
            ],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-token-rp-disabled",
                "config",
                "user.email",
                "sp02@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::write(
        env.case_dir.join("ssh-token-rp-disabled").join("sp02.txt"),
        "sp02 receive-pack must stay disabled\n",
    )
    .expect("write local commit file");
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", "ssh-token-rp-disabled", "add", "sp02.txt"],
        ),
        "git add",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", "ssh-token-rp-disabled", "commit", "-m", "sp02"],
        ),
        "git commit",
    );
    let push = git_cli::git_cli_ssh_with_password(
        &env.case_dir,
        &git_ssh,
        SP02_PUSH_TOKEN,
        &[
            "-C",
            "ssh-token-rp-disabled",
            "push",
            "origin",
            "HEAD:refs/heads/sp02-disabled",
        ],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        !push.status.success(),
        "storage-only SSH push must fail after password auth"
    );
    assert!(
        combined.contains("SSH receive-pack is disabled"),
        "push stderr/stdout must mention disabled receive-pack, got: {combined}"
    );
    let tip_after = ls_remote_head(&env.case_dir, &git_ssh, &remote, Some(SP02_PUSH_TOKEN));
    assert_eq!(tip_before, tip_after, "server tip must be unchanged");
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
fn integration_git_ssh_review_pubkey_anon_off_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, _port, stderr_path, git_ssh, remote) =
        prepare_review_pubkey_loopback(false);
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["clone", &remote, "ssh-review-anon-off"],
        ),
        "review publickey clone with anonymous_access=false",
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
fn integration_git_ssh_review_pubkey_anon_on_push() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let (env, mut service, _port, stderr_path, git_ssh, remote) =
        prepare_review_pubkey_loopback(true);
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["clone", &remote, "ssh-review-anon-on-push"],
        ),
        "review publickey clone with anonymous_access=true",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-review-anon-on-push",
                "checkout",
                "-b",
                "sp02-review-push",
            ],
        ),
        "create push branch",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-review-anon-on-push",
                "config",
                "user.name",
                "SP-02 Review",
            ],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-review-anon-on-push",
                "config",
                "user.email",
                "sp02-review@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::write(
        env.case_dir
            .join("ssh-review-anon-on-push")
            .join("sp02-review.txt"),
        "review pubkey push still works when anonymous_access=true\n",
    )
    .expect("write review push fixture");
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", "ssh-review-anon-on-push", "add", "sp02-review.txt"],
        ),
        "git add",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-review-anon-on-push",
                "commit",
                "-m",
                "sp02 review pubkey push",
            ],
        ),
        "git commit",
    );
    git_cli::assert_git_success(
        &git_host_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                "ssh-review-anon-on-push",
                "-c",
                "pack.window=0",
                "-c",
                "pack.depth=0",
                "push",
                "origin",
                "HEAD:refs/heads/sp02-review-push",
            ],
        ),
        "review publickey SSH push with anonymous_access=true",
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

fn prepare_authenticated_ssh(
    env: &GitSshEnv,
) -> (ServiceProcess, u16, PathBuf, PathBuf, String, String) {
    let client_key = env.ssh_dir.join("client_ed25519");
    let client_pub = env.ssh_dir.join("client_ed25519.pub");
    let known_hosts = env.ssh_dir.join("known_hosts");

    git_cli::generate_client_ed25519(&client_key);
    let pubkey = fs::read_to_string(&client_pub).expect("read client public key");
    let finger = git_cli::ssh_fingerprint_sha256_col2(&client_pub);

    {
        let (mut migrate_service, _migrate_port, _out, err) = boot_service_ssh(env);
        let status = migrate_service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "migrate boot did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&err),
        );
        drop(migrate_service);
    }

    git_cli::seed_ssh_key(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        "gm-ssh",
        pubkey.trim(),
        &finger,
    );
    git_cli::assert_ssh_keys_row_matches_keypair(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        &finger,
    );

    let (service, port, stdout_path, stderr_path) = boot_service_ssh(env);
    git_cli::write_known_hosts_via_keyscan(&known_hosts, port);
    let git_ssh = git_cli::git_ssh_command(&env.case_dir, port);
    let remote = git_cli::monoengine_ssh_repo_url(port, git_cli::DEFAULT_SSH_AUTH_USER);
    (service, port, stdout_path, stderr_path, git_ssh, remote)
}

fn ls_remote_cl_refs_ssh(case_dir: &Path, git_ssh: &str, remote: &str) -> Vec<String> {
    let output = git_cli::git_cli_ssh(case_dir, git_ssh, &["ls-remote", remote, "refs/cl/*"]);
    git_cli::assert_git_success(&output, "list remote CL refs over SSH");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .filter(|name| !name.ends_with("^{}"))
        .collect()
}

#[test]
fn integration_git_ssh_pull_cl_ref_round_trip() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    let case_id = format!(
        "gm07-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fixture_payload = format!(
        "monoengine gm-07 ssh pull fixture pid={} case={}\n",
        std::process::id(),
        env.case_dir.display()
    );
    let fixture_rel = Path::new("gm-07-ssh-pull-fixture.txt");
    let fixture_host = env.case_dir.join("fixture").join(fixture_rel);
    fs::create_dir_all(fixture_host.parent().expect("fixture parent")).expect("mkdir fixture");
    fs::write(&fixture_host, &fixture_payload).expect("write fixture");

    let (mut service, port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh(&env);
    let service_pid = service.pid();

    let sender_name = "ssh-pull-sender";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, sender_name]),
        "clone SSH sender worktree",
    );
    let sender = env.case_dir.join(sender_name);
    let expected_seed = expected_init_monorepo_fixture();
    assert_eq!(
        snapshot_workdir(&sender),
        expected_seed,
        "sender clone must match seeded monorepo fixture before pull case mutates a CL tip"
    );

    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", sender_name, "checkout", "-b", &case_id],
        ),
        "create sender branch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", sender_name, "config", "user.name", "GM-07 SSH Pull"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                sender_name,
                "config",
                "user.email",
                "gm-07-ssh@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::copy(&fixture_host, sender.join(fixture_rel)).expect("copy fixture into sender");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", sender_name, "add", fixture_rel.to_str().unwrap()],
        ),
        "git add pull fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", sender_name, "commit", "-m", "gm-07 ssh pull fixture"],
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

    let before_cl = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote);
    let refspec = format!("HEAD:refs/heads/{case_id}");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
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
        "push sender tip to create refs/cl/* for SSH pull",
    );
    let after_cl = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote);
    let cl_ref = after_cl
        .into_iter()
        .find(|r| !before_cl.contains(r))
        .unwrap_or_else(|| panic!("expected new refs/cl/* after push for case {case_id}"));
    assert!(
        cl_ref.starts_with("refs/cl/"),
        "expected refs/cl/* tip, got {cl_ref}"
    );

    let puller_name = "ssh-pull-receiver";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, puller_name]),
        "clone default tip for literal SSH pull",
    );
    let puller = env.case_dir.join(puller_name);
    assert_eq!(
        snapshot_workdir(&puller),
        expected_seed,
        "pull receiver default main must equal seed tree before literal pull"
    );

    let local_branch = format!("pulled-{case_id}");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", puller_name, "checkout", "-b", &local_branch],
        ),
        "create local branch for literal pull",
    );
    // Literal `git pull origin <refs/cl/…>` (ADR-GM-02). Not fetch+checkout.
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
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
        "literal git pull of refs/cl tip into local branch over SSH",
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
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", puller_name, "checkout", "main"],
        ),
        "return to default main after pull",
    );
    assert_eq!(
        snapshot_workdir(&puller),
        expected_seed,
        "default main must remain the seed tree after literal CL pull"
    );

    let _ = port;
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
}

#[test]
fn integration_git_ssh_authenticated_push_creates_cl_ref() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    let branch = format!(
        "gm08-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fixture_payload = format!(
        "monoengine gm-08 ssh push fixture pid={} case={}\n",
        std::process::id(),
        env.case_dir.display()
    );
    let fixture_rel = Path::new("gm-08-ssh-push-fixture.txt");
    let fixture_host = env.case_dir.join("fixture").join(fixture_rel);
    fs::create_dir_all(fixture_host.parent().expect("fixture parent")).expect("mkdir fixture");
    fs::write(&fixture_host, &fixture_payload).expect("write fixture");

    let (mut service, port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh(&env);
    let service_pid = service.pid();

    let clone_name = "ssh-push-src";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "clone SSH worktree before authenticated push",
    );
    let clone = env.case_dir.join(clone_name);
    let expected_seed = expected_init_monorepo_fixture();
    assert_eq!(
        snapshot_workdir(&clone),
        expected_seed,
        "pre-push clone must match seeded monorepo fixture"
    );

    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create push branch",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "config", "user.name", "GM-08 SSH Push"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "gm-08-ssh@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::copy(&fixture_host, clone.join(fixture_rel)).expect("copy fixture into clone");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", fixture_rel.to_str().unwrap()],
        ),
        "git add push fixture",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "gm-08 ssh push fixture"],
        ),
        "git commit push fixture",
    );

    let before_cl = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote);
    let refspec = format!("HEAD:refs/heads/{branch}");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
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
        ),
        "authenticated SSH push",
    );

    let after_cl = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote);
    let cl_ref = after_cl
        .into_iter()
        .find(|r| !before_cl.contains(r))
        .unwrap_or_else(|| panic!("expected a new refs/cl/* after SSH branch push"));
    assert!(
        cl_ref.starts_with("refs/cl/"),
        "expected refs/cl/* tip after authenticated SSH push, got {cl_ref}"
    );

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
}

#[test]
fn integration_git_ssh_wrong_key_is_rejected() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    let (mut service, port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh(&env);
    let service_pid = service.pid();

    let clone_name = "ssh-wrong-key-src";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "clone with seeded key before wrong-key push",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "checkout",
                "-b",
                &format!("gm08-bad-{}", std::process::id()),
            ],
        ),
        "create branch for wrong-key push attempt",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "config", "user.name", "GM-08 Wrong Key"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "gm-08-wrong@example.invalid",
            ],
        ),
        "git user.email",
    );
    let marker = env.case_dir.join(clone_name).join("gm-08-wrong-key.txt");
    fs::write(&marker, b"wrong-key push must fail\n").expect("write marker");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", "gm-08-wrong-key.txt"],
        ),
        "git add wrong-key marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "gm-08 wrong-key attempt"],
        ),
        "git commit wrong-key marker",
    );

    // ADR-GM-05: second unseeded keypair; do not rewrite known_hosts.
    let bad_key = env.ssh_dir.join("client_ed25519_bad");
    git_cli::generate_client_ed25519(&bad_key);
    let bad_git_ssh = git_ssh.replace("client_ed25519", "client_ed25519_bad");
    assert!(
        bad_git_ssh.contains("client_ed25519_bad"),
        "wrong-key GIT_SSH_COMMAND must select the unseeded private key"
    );
    assert!(
        !bad_git_ssh.contains("client_ed25519 "),
        "wrong-key GIT_SSH_COMMAND must not still point at the seeded private key"
    );

    let push = git_cli::git_cli_ssh(
        &env.case_dir,
        &bad_git_ssh,
        &[
            "-C",
            clone_name,
            "-c",
            "pack.window=0",
            "-c",
            "pack.depth=0",
            "push",
            "origin",
            "HEAD:refs/heads/gm08-wrong-key",
        ],
    );
    assert!(
        !push.status.success(),
        "unseeded SSH key must be rejected; status={:?}\nstdout:\n{}\nstderr:\n{}",
        push.status,
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "SSH service must remain listening after wrong-key rejection on port {port}"
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["ls-remote", &remote, "HEAD"]),
        "seeded key must still ls-remote after wrong-key rejection",
    );

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
}

fn boot_service_ssh(env: &GitSshEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    boot_service_ssh_with_env(env, &[])
}

fn boot_service_ssh_with_env(
    env: &GitSshEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    // ephemeral_port_from_127.0.0.1:0_to_--ssh-port
    let port = git_cli::reserve_ephemeral_port();
    git_cli::record_allocated_port(port);
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let port_arg = port.to_string();
    command.args([
        "service",
        "ssh",
        "--host",
        git_cli::service_listen_host(),
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

/// Boot `service multi http ssh` (UN-03): the SSH leg shares the `AppContext`
/// instance with the HTTP leg, so authorization enforcement (`shadow`/`enforce`)
/// is available over SSH. Standalone `service ssh` refuses `enforcement != off`,
/// so the real-CLI push rejection cases must go through `multi`.
fn boot_service_multi(
    env: &GitSshEnv,
    enforcement: &str,
) -> (ServiceProcess, u16, u16, PathBuf, PathBuf) {
    boot_service_multi_with_session(env, enforcement, None)
}

/// As above, but optionally points the service's browser-session lookup at a
/// stub website (UN-24: privileged API calls now need a real subject).
fn boot_service_multi_with_session(
    env: &GitSshEnv,
    enforcement: &str,
    session_stub_port: Option<u16>,
) -> (ServiceProcess, u16, u16, PathBuf, PathBuf) {
    let http_port = git_cli::reserve_ephemeral_port();
    let ssh_port = git_cli::reserve_ephemeral_port();
    git_cli::record_allocated_port(http_port);
    git_cli::record_allocated_port(ssh_port);
    let stdout_path = env.temp_dir.path().join("multi.out");
    let stderr_path = env.temp_dir.path().join("multi.err");

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    command.env("MEGA_CEDAR__ENFORCEMENT", enforcement);
    if let Some(stub_port) = session_stub_port {
        command.env(
            "MEGA_OAUTH__WEBSITE_API_BASE_URL",
            format!("http://127.0.0.1:{stub_port}"),
        );
    }
    git_cli::apply_monoengine_public_http_base_env(&mut command, http_port);
    let http_port_arg = http_port.to_string();
    let ssh_port_arg = ssh_port.to_string();
    command.args([
        "service",
        "multi",
        "http",
        "ssh",
        "--host",
        git_cli::service_listen_host(),
        "-p",
        &http_port_arg,
        "--ssh-port",
        &ssh_port_arg,
    ]);
    command
        .stdout(Stdio::from(create_log_file(&stdout_path)))
        .stderr(Stdio::from(create_log_file(&stderr_path)));

    let mut service = ServiceProcess::spawn(command);
    service.wait_until_listening(
        ssh_port,
        Duration::from_secs(90),
        &stdout_path,
        &stderr_path,
    );
    (service, http_port, ssh_port, stdout_path, stderr_path)
}

/// Migrate-boot (off), seed the SSH key, then boot `service multi` under the
/// given enforcement. Returns the live service plus the SSH port / git_ssh /
/// remote for the case.
fn prepare_authenticated_ssh_multi(
    env: &GitSshEnv,
    enforcement: &str,
) -> (ServiceProcess, u16, PathBuf, PathBuf, String, String) {
    let (service, _http_port, ssh_port, stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh_multi_with_http(env, enforcement);
    (service, ssh_port, stdout_path, stderr_path, git_ssh, remote)
}

/// Same as [`prepare_authenticated_ssh_multi`], but also returns the HTTP port
/// of the `multi` process (UN-16 e2e drives the ACL change over the HTTP leg
/// and asserts the SSH leg sees it, which is only true for a shared instance).
fn prepare_authenticated_ssh_multi_with_http(
    env: &GitSshEnv,
    enforcement: &str,
) -> (ServiceProcess, u16, u16, PathBuf, PathBuf, String, String) {
    prepare_authenticated_ssh_multi_with_session(env, enforcement, None)
}

fn prepare_authenticated_ssh_multi_with_session(
    env: &GitSshEnv,
    enforcement: &str,
    session_stub_port: Option<u16>,
) -> (ServiceProcess, u16, u16, PathBuf, PathBuf, String, String) {
    let client_key = env.ssh_dir.join("client_ed25519");
    let client_pub = env.ssh_dir.join("client_ed25519.pub");
    let known_hosts = env.ssh_dir.join("known_hosts");

    git_cli::generate_client_ed25519(&client_key);
    let pubkey = fs::read_to_string(&client_pub).expect("read client public key");
    let finger = git_cli::ssh_fingerprint_sha256_col2(&client_pub);

    // Migrations must complete before ssh_keys seed (ADR-GM-05). Boot multi with
    // `off` (the default) to apply migrations + Vault host key, then shut down.
    {
        let (mut migrate_service, _hp, _sp, _out, err) = boot_service_multi(env, "off");
        let status = migrate_service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(
            status.success(),
            "migrate boot did not shut down cleanly: {status}\nstderr:\n{}",
            read_log(&err),
        );
        drop(migrate_service);
    }

    git_cli::seed_ssh_key(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        "un03-multi",
        pubkey.trim(),
        &finger,
    );
    git_cli::assert_ssh_keys_row_matches_keypair(
        &env.database.db_url,
        git_cli::DEFAULT_SSH_AUTH_USER,
        &finger,
    );

    let (service, http_port, ssh_port, stdout_path, stderr_path) =
        boot_service_multi_with_session(env, enforcement, session_stub_port);
    git_cli::write_known_hosts_via_keyscan(&known_hosts, ssh_port);
    let git_ssh = git_cli::git_ssh_command(&env.case_dir, ssh_port);
    let remote = git_cli::monoengine_ssh_repo_url(ssh_port, git_cli::DEFAULT_SSH_AUTH_USER);
    (
        service,
        http_port,
        ssh_port,
        stdout_path,
        stderr_path,
        git_ssh,
        remote,
    )
}

#[test]
fn integration_git_ssh_enforce_rejects_unauthorized_push() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    // UN-03 allowlist markers (three-state gate over SSH via `service multi`).
    const _: &str = "enforce_rejects_non_admin_ssh_push";
    const _: &str = "service_multi_shared_instance_http_ssh";
    const _: &str = "ssh_push_uses_check_push_permission_three_state_gate";

    let env = GitSshEnv::new();
    let (mut service, ssh_port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh_multi(&env, "enforce");
    let service_pid = service.pid();

    let clone_name = "un03-enforce-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "clone before enforce push",
    );
    let clone = env.case_dir.join(clone_name);
    let branch = format!("un03-enforce-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create branch for enforce push attempt",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "config", "user.name", "UN-03 Enforce"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "un03-enforce@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::write(clone.join("un03-enforce.txt"), b"enforce push must fail\n").expect("write marker");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", "un03-enforce.txt"],
        ),
        "git add enforce marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "un03 enforce attempt"],
        ),
        "git commit enforce marker",
    );

    // Non-admin user (`it-git-ssh`) pushing to the private root repo must be
    // denied under `enforce` (fail-closed three-state gate, ADR-UN-01).
    let push = git_cli::git_cli_ssh(
        &env.case_dir,
        &git_ssh,
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
    assert!(
        !push.status.success(),
        "enforce must reject non-admin SSH push; status={:?}\nstdout:\n{}\nstderr:\n{}",
        push.status,
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(
        TcpStream::connect(("127.0.0.1", ssh_port)).is_ok(),
        "SSH service must remain listening after enforce rejection on port {ssh_port}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    git_cli::assert_process_reaped(service_pid);
    git_cli::wait_until_port_closed(ssh_port, Duration::from_secs(5));
    drop(service);
    drop(env);
}

#[test]
fn integration_git_ssh_shadow_allows_push_but_records_would_deny() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    // UN-03 allowlist markers (three-state gate over SSH via `service multi`).
    const _: &str = "shadow_allows_ssh_push_but_records_would_deny";
    const _: &str = "service_multi_shared_instance_http_ssh";
    const _: &str = "ssh_push_uses_check_push_permission_three_state_gate";

    let env = GitSshEnv::new();
    let (mut service, ssh_port, stdout_path, _stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh_multi(&env, "shadow");
    let service_pid = service.pid();

    let clone_name = "un03-shadow-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "clone before shadow push",
    );
    let clone = env.case_dir.join(clone_name);
    let branch = format!("un03-shadow-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "checkout", "-b", &branch],
        ),
        "create branch for shadow push",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "config", "user.name", "UN-03 Shadow"],
        ),
        "git user.name",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "config",
                "user.email",
                "un03-shadow@example.invalid",
            ],
        ),
        "git user.email",
    );
    fs::write(clone.join("un03-shadow.txt"), b"shadow push allowed\n").expect("write marker");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", "un03-shadow.txt"],
        ),
        "git add shadow marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "un03 shadow push"],
        ),
        "git commit shadow marker",
    );

    // `shadow` allows the push (no behavior change) but records would-deny.
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
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
        "shadow must allow non-admin SSH push",
    );
    // The three-state gate must have evaluated and recorded would-deny.
    let stdout_body = read_log(&stdout_path);
    assert!(
        stdout_body.contains("authz_would_deny"),
        "shadow push must record authz_would_deny in service stdout:\n{stdout_body}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&_stderr_path),
    );
    git_cli::assert_process_reaped(service_pid);
    git_cli::wait_until_port_closed(ssh_port, Duration::from_secs(5));
    drop(service);
    drop(env);
}

#[test]
fn integration_git_ssh_authz_grant_immediate_effect() {
    // UN-16: 授权即时生效 e2e over the SSH channel. The ACL change is merged
    // through the HTTP leg's merge funnel (`apply_update_result` →
    // `notify_authz_changed`), and the SSH leg — which shares the same
    // `AppContext`/`EntityStore` instance under `service multi` — must honour
    // the new snapshot on the very next push, with no restart.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MONOENGINE_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    // The HTTP leg drives the ACL change, so the git credential askpass helper
    // must exist under the case dir (GitSshEnv only prepares SSH material).
    git_cli::write_git_askpass(&env.case_dir.join("git-askpass.sh"));

    // The ACL-change merge below goes through `merge-no-auth`, which since
    // UN-24 is authorized like any other merge entry point; the stub lets this
    // test present the admin's session for that call.
    let session_stub_port = spawn_website_session_stub("benjamin_747");
    let (mut service, http_port, ssh_port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh_multi_with_session(&env, "enforce", Some(session_stub_port));
    let service_pid = service.pid();

    let admin_token = git_cli::resolve_seed_token();
    git_cli::seed_access_token(&env.database.db_url, "benjamin_747", &admin_token);
    let http_remote = git_cli::monoengine_http_repo_url(http_port);

    // --- baseline: the SSH user is not an admin, so `enforce` denies its push ---
    let clone_name = "un16-ssh-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &git_ssh, &["clone", &remote, clone_name]),
        "SSH clone before grant",
    );
    let clone = env.case_dir.join(clone_name);
    for (key, value) in [
        ("user.name", "UN-16 SSH"),
        ("user.email", "un16-ssh@example.invalid"),
    ] {
        git_cli::assert_git_success(
            &git_cli::git_cli_ssh(
                &env.case_dir,
                &git_ssh,
                &["-C", clone_name, "config", key, value],
            ),
            "SSH clone git config",
        );
    }
    let baseline_branch = format!("un16-ssh-baseline-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "checkout", "-b", &baseline_branch],
        ),
        "create SSH baseline branch",
    );
    fs::write(
        clone.join("un16-ssh-baseline.txt"),
        b"ssh baseline push must fail\n",
    )
    .expect("write SSH baseline marker");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", "un16-ssh-baseline.txt"],
        ),
        "git add SSH baseline marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "un16 ssh baseline"],
        ),
        "git commit SSH baseline marker",
    );
    let baseline_push = git_cli::git_cli_ssh(
        &env.case_dir,
        &git_ssh,
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
        "non-admin SSH push must be denied under enforce; status={:?}\nstdout:\n{}\nstderr:\n{}",
        baseline_push.status,
        String::from_utf8_lossy(&baseline_push.stdout),
        String::from_utf8_lossy(&baseline_push.stderr)
    );

    // --- grant: the admin merges an ACL change over the HTTP leg ---
    let admin_clone = "un16-ssh-admin-clone";
    git_cli::assert_git_success(
        &git_cli::git_cli_as_user(
            &env.case_dir,
            "benjamin_747",
            &admin_token,
            &["clone", &http_remote, admin_clone],
        ),
        "HTTP clone as admin",
    );
    let admin_dir = env.case_dir.join(admin_clone);
    fs::write(
        admin_dir.join(".mega_cedar.json"),
        authz_json_with_admins(&["benjamin_747", git_cli::DEFAULT_SSH_AUTH_USER]),
    )
    .expect("write grant authz json");
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
                "un16 grant it-git-ssh admin",
            ],
        ),
        "admin git commit grant",
    );
    let grant_branch = format!("un16-ssh-grant-{}", std::process::id());
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
    assert_eq!(
        merge_cl_no_auth(http_port, &grant_cl_link),
        200,
        "grant merge must succeed"
    );

    // --- the SSH leg must honour the new snapshot immediately ---
    // Refresh from the merged main first: Monorepo rejects packs carrying more
    // than one commit, so the new commit's parent must already be on main.
    let fetch = git_cli::git_cli_ssh(
        &env.case_dir,
        &git_ssh,
        &["-C", clone_name, "fetch", "origin"],
    );
    assert!(
        fetch.status.success(),
        "SSH fetch of merged main failed; status={:?}\nstdout:\n{}\nstderr:\n{}",
        fetch.status,
        String::from_utf8_lossy(&fetch.stdout),
        String::from_utf8_lossy(&fetch.stderr)
    );
    let granted_branch = format!("un16-ssh-granted-{}", std::process::id());
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &[
                "-C",
                clone_name,
                "checkout",
                "-b",
                &granted_branch,
                "origin/main",
            ],
        ),
        "branch SSH granted push off merged main",
    );
    fs::write(
        clone.join("un16-ssh-granted.txt"),
        b"ssh granted push must succeed\n",
    )
    .expect("write SSH granted marker");
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "add", "un16-ssh-granted.txt"],
        ),
        "git add SSH granted marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
            &["-C", clone_name, "commit", "-m", "un16 ssh granted push"],
        ),
        "git commit SSH granted marker",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(
            &env.case_dir,
            &git_ssh,
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
        ),
        "granted SSH push must succeed without a restart",
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );
    git_cli::assert_process_reaped(service_pid);
    git_cli::wait_until_port_closed(ssh_port, Duration::from_secs(5));
    drop(service);
    drop(env);
}

/// Build a `/.mega_cedar.json` body whose `admin` group holds exactly `admins`
/// (UN-16 e2e: grant/revoke by merging this file onto main).
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
        row.try_get::<String>("", "link").expect("link column")
    })
}

/// Cookie value the session stub accepts; its content is irrelevant because the
/// stub answers every request the same way.
const SESSION_COOKIE: &str = "better-auth.session_token=it-un16-ssh-session";

/// Minimal stand-in for the website's Better Auth `get-session` endpoint.
///
/// Since UN-24, `merge-no-auth` requires *authorization* (it never required
/// authentication), so this test has to make its privileged merge call as a
/// real subject. The service resolves browser sessions by asking the website;
/// this stub answers with the admin whose ACL change is being merged. Returns
/// the port it listens on.
fn spawn_website_session_stub(username: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind session stub");
    let port = listener.local_addr().expect("stub addr").port();
    let body = format!(
        r#"{{"session":{{"id":"it-session","userId":"{username}"}},"user":{{"id":"{username}","name":"{username}","email":"{username}@example.invalid"}}}}"#
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            let _ = std::io::Write::flush(&mut stream);
        }
    });
    port
}

/// Merge a CL through the `merge-no-auth` API, carrying the stub session so the
/// call is authorized (UN-24). Returns the HTTP status code.
fn merge_cl_no_auth(port: u16, cl_link: &str) -> u16 {
    let url = format!("http://127.0.0.1:{port}/api/v1/cl/{cl_link}/merge-no-auth");
    let output = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            &format!("Cookie: {SESSION_COOKIE}"),
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "30",
            &url,
        ])
        .output()
        .expect("curl merge-no-auth");
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
