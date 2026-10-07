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
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement, Value};
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
            "mega2_git_ssh_{}_{}",
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
fn integration_git_ssh_service_lifecycle_isolated() {
    // GM-06: no SKIP-green path. Lifecycle does not need the git-cli runner,
    // but an explicit skip request must still fail the test.
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
    let known_host = git_cli::mega2_reachable_host();
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

    let remote = git_cli::mega2_ssh_repo_url(port, git_cli::DEFAULT_SSH_AUTH_USER);
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

#[test]
fn integration_git_ssh_v0_shallow_fetch_resumes_after_shallow_info() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();
    let env = GitSshEnv::new();
    let (mut service, port, _stdout_path, stderr_path, git_ssh, remote) =
        prepare_authenticated_ssh(&env);
    let git_stdout = |args: &[&str]| {
        let output = git_cli::git_cli_ssh(&env.case_dir, &git_ssh, args);
        git_cli::assert_git_success(&output, "SSH shallow fetch fixture/verification");
        String::from_utf8(output.stdout).expect("Git output UTF-8")
    };

    // Publish one child of the initial monorepo commit as a CL. Removing a
    // parent-only file makes an over-deep pack observable in the object set.
    let seed_name = "ssh-shallow-seed";
    git_stdout(&["clone", &remote, seed_name]);
    git_stdout(&["-C", seed_name, "config", "user.name", "SSH Shallow Test"]);
    git_stdout(&[
        "-C",
        seed_name,
        "config",
        "user.email",
        "ssh-shallow@example.invalid",
    ]);
    // Run both the removal and index update through the selected Git runner.
    // With the container runner, changing the bind-mounted worktree from the
    // host and immediately running `git add -A` can expose a stale directory
    // entry to Git on macOS.
    git_stdout(&["-C", seed_name, "rm", "project/.gitkeep"]);
    git_stdout(&["-C", seed_name, "commit", "-m", "SSH shallow tip"]);
    let before = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote);
    git_stdout(&[
        "-C",
        seed_name,
        "-c",
        "pack.window=0",
        "-c",
        "pack.depth=0",
        "push",
        "origin",
        "HEAD:refs/heads/ssh-shallow-test",
    ]);
    let cl_ref = ls_remote_cl_refs_ssh(&env.case_dir, &git_ssh, &remote)
        .into_iter()
        .find(|reference| !before.contains(reference))
        .expect("new SSH shallow CL ref");

    // Freeze the actual server history as the oracle; server CL publication
    // may assign commit identities independently of the client's seed.
    let reference = "ssh-shallow-reference";
    git_stdout(&["init", reference]);
    git_stdout(&["-C", reference, "fetch", &remote, &cl_ref]);
    git_stdout(&["-C", reference, "checkout", "--detach", "FETCH_HEAD"]);
    let history: Vec<String> = git_stdout(&["-C", reference, "rev-list", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(history.len(), 2, "fixture must have a tip and its parent");
    let expected_worktree = snapshot_workdir(&env.case_dir.join(reference));
    assert_eq!(
        expected_worktree,
        snapshot_workdir(&env.case_dir.join(seed_name)),
        "server must preserve the fixture tree"
    );

    for depth in [1, 2, 5] {
        let repo = format!("ssh-shallow-depth-{depth}");
        let depth_arg = depth.to_string();
        git_stdout(&["init", &repo]);
        // v0 SSH sends want/deepen/flush, consumes shallow-info, and sends
        // only `done` next. Fresh repositories exercise the clone handshake.
        git_stdout(&[
            "-C",
            &repo,
            "-c",
            "protocol.version=0",
            "fetch",
            "--depth",
            &depth_arg,
            &remote,
            &cl_ref,
        ]);
        git_stdout(&["-C", &repo, "checkout", "--detach", "FETCH_HEAD"]);
        let included = (depth as usize).min(history.len());
        let actual_history: Vec<String> = git_stdout(&["-C", &repo, "rev-list", "HEAD"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            actual_history,
            history[..included],
            "depth {depth}: commits"
        );
        let shallow_path = env.case_dir.join(&repo).join(".git/shallow");
        let boundaries: Vec<String> = fs::read_to_string(shallow_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        if included < history.len() {
            assert_eq!(
                boundaries,
                vec![history[included - 1].clone()],
                "depth {depth}: boundary"
            );
        } else {
            // Git may retain a shallow marker at the root even when every
            // commit fits within the requested depth. It cuts no real edge.
            assert!(
                boundaries.is_empty() || boundaries == vec![history.last().unwrap().clone()],
                "depth {depth}: only a root marker is valid for complete history: {boundaries:?}"
            );
        }

        let mut expected_objects = HashSet::new();
        for commit in &history[..included] {
            expected_objects.insert(commit.clone());
            let tree = git_stdout(&["-C", reference, "rev-parse", &format!("{commit}^{{tree}}")])
                .trim()
                .to_owned();
            expected_objects.insert(tree.clone());
            let entries = git_stdout(&["-C", reference, "ls-tree", "-r", "-t", &tree]);
            for entry in entries.lines() {
                expected_objects.insert(
                    entry
                        .split_whitespace()
                        .nth(2)
                        .expect("tree object id")
                        .to_owned(),
                );
            }
        }
        let actual_objects: HashSet<String> = git_stdout(&[
            "-C",
            &repo,
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .lines()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            actual_objects, expected_objects,
            "depth {depth}: exact objects"
        );
        assert_eq!(
            snapshot_workdir(&env.case_dir.join(&repo)),
            expected_worktree,
            "depth {depth}: worktree"
        );
        git_stdout(&["-C", &repo, "fsck", "--full", "--strict"]);
    }
    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(
        status.success(),
        "SSH service shutdown failed: {status}\n{}",
        read_log(&stderr_path)
    );
    git_cli::wait_until_port_closed(port, Duration::from_secs(5));
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

fn raw_ssh_upload_pack(
    case_dir: &Path,
    port: u16,
    protocol_v2: bool,
    stdin_body: &[u8],
) -> std::process::Output {
    let mut ssh = git_ssh_command_loopback(case_dir, port, true);
    if protocol_v2 {
        ssh.push_str(" -o SetEnv=GIT_PROTOCOL=version=2");
    }
    let command = format!("{ssh} git@127.0.0.1 \"git-upload-pack '/'\"");
    let mut child = Command::new("timeout")
        .args(["-k", "5", "45", "sh", "-c", &command])
        .current_dir(case_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn raw SSH upload-pack");
    {
        let mut stdin = child.stdin.take().expect("raw SSH stdin");
        stdin
            .write_all(stdin_body)
            .expect("write raw SSH upload-pack request");
    }
    child
        .wait_with_output()
        .expect("wait for raw SSH upload-pack")
}

fn first_pkt_line_payload(output: &[u8]) -> &[u8] {
    assert!(
        output.len() >= 4,
        "SSH output must start with a pkt-line: {output:?}"
    );
    let declared = std::str::from_utf8(&output[..4]).expect("pkt-line length header");
    let length = usize::from_str_radix(declared, 16).expect("pkt-line hexadecimal length");
    assert!(
        (4..=output.len()).contains(&length),
        "pkt-line length {length} outside output length {}",
        output.len()
    );
    &output[4..length]
}

#[test]
fn integration_git_ssh_view_ssh_err_plain_paths_exit_zero() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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

    let v0 = raw_ssh_upload_pack(&env.case_dir, port, false, b"");
    assert!(
        v0.status.success(),
        "v0 raw SSH upload-pack must exit 0; status={:?}\nstdout:\n{}\nstderr:\n{}\n--- service stdout ---\n{}\n--- service stderr ---\n{}",
        v0.status,
        String::from_utf8_lossy(&v0.stdout),
        String::from_utf8_lossy(&v0.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    let first = first_pkt_line_payload(&v0.stdout);
    assert!(
        first.windows(b"HEAD".len()).any(|bytes| bytes == b"HEAD")
            || first
                .windows(b"capabilities^{}".len())
                .any(|bytes| bytes == b"capabilities^{}"),
        "v0 SSH advertisement first pkt-line must contain HEAD or capabilities^{{}}: {first:?}"
    );

    let ls_refs = raw_ssh_upload_pack(&env.case_dir, port, true, b"0014command=ls-refs\n00010000");
    assert!(
        ls_refs.status.success(),
        "v2 ls-refs raw SSH upload-pack must exit 0; status={:?}\nstdout:\n{}\nstderr:\n{}\n--- service stdout ---\n{}\n--- service stderr ---\n{}",
        ls_refs.status,
        String::from_utf8_lossy(&ls_refs.stdout),
        String::from_utf8_lossy(&ls_refs.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    let ls_refs_stdout = String::from_utf8_lossy(&ls_refs.stdout);
    assert!(
        ls_refs_stdout.contains("version 2"),
        "v2 ls-refs response must advertise version 2: {:?}",
        ls_refs.stdout
    );
    assert!(
        !ls_refs_stdout.contains("error:"),
        "v2 ls-refs response must not contain a plain error: {:?}",
        ls_refs.stdout
    );

    let bogus = raw_ssh_upload_pack(&env.case_dir, port, true, b"0012command=bogus\n0000");
    assert!(
        bogus.status.success(),
        "v2 bogus raw SSH upload-pack must exit 0; status={:?}\nstdout:\n{}\nstderr:\n{}\n--- service stdout ---\n{}\n--- service stderr ---\n{}",
        bogus.status,
        String::from_utf8_lossy(&bogus.stdout),
        String::from_utf8_lossy(&bogus.stderr),
        read_log(&stdout_path),
        read_log(&stderr_path),
    );
    assert!(
        String::from_utf8_lossy(&bogus.stdout).contains("error: unsupported v2 command: bogus"),
        "v2 bogus response must keep its plain error: {:?}",
        bogus.stdout
    );

    for (name, output) in [("v0", &v0), ("v2 ls-refs", &ls_refs), ("v2 bogus", &bogus)] {
        assert!(
            !output
                .stdout
                .windows(b"ERR ".len())
                .any(|bytes| bytes == b"ERR "),
            "{name} plain path must not emit an ERR pkt-line: {:?}",
            output.stdout
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
fn integration_git_ssh_trunk_none_anon_on_clone() {
    assert!(
        !git_cli::git_cli_skip_requested(),
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
    let remote = git_cli::mega2_ssh_repo_url(port, git_cli::DEFAULT_SSH_AUTH_USER);
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    let case_id = format!(
        "gm07-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fixture_payload = format!(
        "mega2 gm-07 ssh pull fixture pid={} case={}\n",
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
    );
    git_cli::require_git_cli_runner();

    let env = GitSshEnv::new();
    let branch = format!(
        "gm08-{}-{}",
        std::process::id(),
        CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fixture_payload = format!(
        "mega2 gm-08 ssh push fixture pid={} case={}\n",
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
    boot_service_multi_with_extra(env, enforcement, session_stub_port, &[])
}

fn boot_service_multi_with_extra(
    env: &GitSshEnv,
    enforcement: &str,
    session_stub_port: Option<u16>,
    extra_env: &[(&str, &str)],
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
    for (key, value) in extra_env {
        command.env(key, value);
    }
    git_cli::apply_mega2_public_http_base_env(&mut command, http_port);
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
    let remote = git_cli::mega2_ssh_repo_url(ssh_port, git_cli::DEFAULT_SSH_AUTH_USER);
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
        "MEGA2_IT_SKIP_GIT_CLI must be unset/0 for integration_git_ssh; skipping is not a green path"
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
    let http_remote = git_cli::mega2_http_repo_url(http_port);

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
    let listener = git_cli::bind_ephemeral_listener();
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

/// Complete expected worktree for `MegaModelConverter::init` against the review sample
/// `config/config-review.toml` monorepo settings (`admin = ["benjamin_747"]`, the eight
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

fn raw_view_ssh(
    case_dir: &Path,
    port: u16,
    protocol_v2: bool,
    command: &str,
    path: &str,
    operation: Option<&str>,
) -> std::process::Output {
    let mut ssh = git_ssh_command_loopback(case_dir, port, true);
    if protocol_v2 {
        ssh.push_str(" -o SetEnv=GIT_PROTOCOL=version=2");
    }
    let remote = match operation {
        Some(operation) => format!("{command} '{path}' {operation}"),
        None => format!("{command} '{path}'"),
    };
    let shell = format!("{ssh} git@127.0.0.1 \"{remote}\"");
    Command::new("timeout")
        .args(["-k", "5", "45", "sh", "-c", &shell])
        .current_dir(case_dir)
        .stdin(Stdio::null())
        .output()
        .expect("raw view SSH request")
}

fn view_ssh_env() -> GitSshEnv {
    let env = GitSshEnv::with_config_append(
        r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false
"#,
    );
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    env
}

fn boot_view_ssh(
    env: &GitSshEnv,
    enabled: &'static str,
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let extra = [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__ANONYMOUS_ACCESS", "true"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
        ("MEGA_CEDAR__ENFORCEMENT", "off"),
        ("MEGA_VIEWS__ENABLED", enabled),
    ];
    boot_storage_only_ssh(env, &extra)
}

fn assert_view_ssh_reply(output: &std::process::Output, expected: &[u8]) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, expected, "{output:?}");
}

#[test]
fn integration_git_ssh_view_layer2_ssh_not_found() {
    git_cli::require_git_cli_runner();
    let filter_id = "a".repeat(64);
    for enabled in ["false", "true"] {
        let env = view_ssh_env();
        let (mut service, port, _out, err) = boot_view_ssh(&env, enabled);
        for path in [
            "/.view/x.git".to_owned(),
            ".view/x.git".to_owned(),
            format!("/.filter/{filter_id}.git"),
        ] {
            let commands = if enabled == "false" {
                &["git-upload-pack", "git-receive-pack"][..]
            } else {
                &["git-upload-pack"][..]
            };
            for command in commands {
                for v2 in [false, true] {
                    let output = raw_view_ssh(&env.case_dir, port, v2, command, &path, None);
                    assert_view_ssh_reply(&output, b"0017ERR view not found\n");
                }
            }
        }
        let invalid = raw_view_ssh(
            &env.case_dir,
            port,
            false,
            "git-upload-pack",
            "/.view/a@0.git",
            None,
        );
        assert_view_ssh_reply(&invalid, b"0017ERR view not found\n");
        let status = service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(status.success(), "{}", read_log(&err));
    }
}

#[test]
fn integration_git_ssh_view_layer2_ssh_read_only() {
    git_cli::require_git_cli_runner();
    let env = view_ssh_env();
    let (mut service, port, _out, err) = boot_view_ssh(&env, "true");
    let output = raw_view_ssh(
        &env.case_dir,
        port,
        false,
        "git-receive-pack",
        "/.view/x.git",
        None,
    );
    assert_view_ssh_reply(&output, b"0020ERR view URLs are read-only\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SSH receive-pack is disabled"));
    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(status.success(), "{}", read_log(&err));
}

#[test]
fn integration_git_ssh_view_layer2_ssh_lfs_rejected() {
    git_cli::require_git_cli_runner();
    for enabled in ["false", "true"] {
        let env = view_ssh_env();
        let (mut service, port, _out, err) = boot_view_ssh(&env, enabled);
        for (command, operation) in [
            ("git-lfs-authenticate", "upload"),
            ("git-lfs-authenticate", "download"),
            ("git-lfs-transfer", "upload"),
            ("git-lfs-authenticate", "verify"),
        ] {
            let output = raw_view_ssh(
                &env.case_dir,
                port,
                false,
                command,
                "/.view/x.git",
                Some(operation),
            );
            assert_view_ssh_reply(&output, b"0017ERR view not found\n");
            assert!(!output.stdout.windows(4).any(|window| window == b"href"));
        }
        let status = service.shutdown_via_sigint(Duration::from_secs(10));
        assert!(status.success(), "{}", read_log(&err));
    }
}

fn boot_view_multi(
    env: &GitSshEnv,
    enabled: &'static str,
) -> (ServiceProcess, u16, u16, PathBuf, PathBuf) {
    let extra = [
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_VIEWS__ENABLED", enabled),
        ("MEGA_VIEWS__WORKER_INTERVAL_SECS", "3600"),
        ("MEGA_VIEWS__MAX_CONCURRENT_COLD_STARTS", "1"),
    ];
    let mut init = env.full_config_command();
    for (key, value) in extra {
        init.env(key, value);
    }
    init.env("MEGA_CEDAR__ENFORCEMENT", "off");
    let result = init.args(["service", "init", "--yes"]).output().unwrap();
    assert!(
        result.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let (service, http_port, ssh_port, stdout, stderr) =
        boot_service_multi_with_extra(env, "off", None, &extra);
    write_known_hosts_via_host_keyscan(&env.ssh_dir.join("known_hosts"), ssh_port);
    (service, http_port, ssh_port, stdout, stderr)
}

fn view_ssh_execute(env: &GitSshEnv, statement: Statement) {
    with_runtime(async {
        let db = Database::connect(&env.database.db_url).await.unwrap();
        db.execute_raw(statement).await.unwrap();
    });
}

fn layer3_ssh_env() -> GitSshEnv {
    let env = GitSshEnv::with_config_append(
        "\n[git]\nanonymous_access = true\npush_auth = \"token\"\nssh_receive_pack = false\n[[git.push_tokens]]\nname = \"hp20\"\ntoken = \"hp20-local-token\"\npaths = [\"/\"]\n",
    );
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    env
}

fn layer3_ssh_seed_project(env: &GitSshEnv, port: u16) {
    let remote = git_cli::mega2_http_url(port, "/project");
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(&env.case_dir, &["clone", &remote, "hp20-project"]),
        "HP-20 clone project",
    );
    fs::write(env.case_dir.join("hp20-project/hp20.txt"), b"hp20\n").unwrap();
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(&env.case_dir, &["-C", "hp20-project", "add", "hp20.txt"]),
        "HP-20 add file",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli_no_auth(
            &env.case_dir,
            &[
                "-c",
                "user.name=HP20",
                "-c",
                "user.email=hp20@example.invalid",
                "-C",
                "hp20-project",
                "commit",
                "-m",
                "HP-20 source commit",
            ],
        ),
        "HP-20 commit",
    );
    git_cli::assert_git_success(
        &git_cli::git_cli(
            &env.case_dir,
            "hp20-local-token",
            &[
                "-C",
                "hp20-project",
                "push",
                "--no-thin",
                "origin",
                "HEAD:refs/heads/main",
            ],
        ),
        "HP-20 trunk push",
    );
}

fn layer3_ssh_register(env: &GitSshEnv, port: u16, name: &str, spec: &str) -> (i64, String) {
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/api/v1/views?wait=true"))
        .bearer_auth("hp20-local-token")
        .json(&serde_json::json!({"name": name, "filter_spec": spec}))
        .send()
        .unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap();
    assert!(status.is_success(), "register {spec}: {status} {body}");
    assert_eq!(body["data"]["ready"], true, "{body}");
    let filter_id = body["data"]["filter_id"].as_str().unwrap().to_owned();
    let pk = with_runtime(async {
        let db = Database::connect(&env.database.db_url).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT id FROM mega_view_filter WHERE filter_id = $1",
                [Value::from(filter_id.clone())],
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "id").unwrap()
    });
    (pk, filter_id)
}

fn layer3_ssh_tip(env: &GitSshEnv, pk: i64) -> String {
    with_runtime(async {
        let db = Database::connect(&env.database.db_url).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT m.view_commit FROM mega_view_commit_map m JOIN mega_view_filter f ON f.id = m.filter_pk \
             WHERE m.filter_pk = $1 AND m.seq_from <= f.projected_seq \
             ORDER BY m.seq_from DESC LIMIT 1",
                [Value::from(pk)],
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "view_commit").unwrap()
    })
}

fn layer3_ssh_state(env: &GitSshEnv, pk: i64) -> (Option<i64>, i64, bool) {
    with_runtime(async {
        let db = Database::connect(&env.database.db_url).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT ready_seq, projected_seq, warming_since IS NOT NULL AS warming \
             FROM mega_view_filter WHERE id = $1",
                [Value::from(pk)],
            ))
            .await
            .unwrap()
            .unwrap();
        (
            row.try_get("", "ready_seq").unwrap(),
            row.try_get("", "projected_seq").unwrap(),
            row.try_get("", "warming").unwrap(),
        )
    })
}

fn layer3_ssh_wait_idle(env: &GitSshEnv, pks: &[i64]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut previous = None;
    loop {
        let current: Vec<_> = pks.iter().map(|pk| layer3_ssh_state(env, *pk)).collect();
        if current.iter().all(|state| state.0.is_some()) && previous.as_ref() == Some(&current) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "view worker did not settle: {current:?}"
        );
        previous = Some(current);
        sleep(Duration::from_millis(500));
    }
}

fn layer3_ssh_recycle(env: &GitSshEnv, pk: i64) {
    for table in ["mega_view_commit_map", "mega_view_object_ref"] {
        view_ssh_execute(
            env,
            Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                format!("DELETE FROM {table} WHERE filter_pk = $1"),
                [Value::from(pk)],
            ),
        );
    }
    view_ssh_execute(
        env,
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE mega_view_filter SET ready_seq = NULL, warming_since = NULL, projected_seq = 0 WHERE id = $1",
            [Value::from(pk)],
        ),
    );
}

fn layer3_ssh_clear_halt(env: &GitSshEnv) {
    view_ssh_execute(
        env,
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM mega_view_root_chain_scan WHERE commit_id = $1",
            [Value::from("e".repeat(40))],
        ),
    );
}

fn view_ssh_set_ready(env: &GitSshEnv, pk: i64, warming: bool) {
    view_ssh_execute(
        env,
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE mega_view_filter SET ready_seq = NULL, warming_since = \
             CASE WHEN $1 THEN now() ELSE NULL END WHERE id = $2",
            [Value::from(warming), Value::from(pk)],
        ),
    );
}

fn view_ssh_halt_root_chain(env: &GitSshEnv) {
    view_ssh_execute(
        env,
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO mega_view_root_chain_scan \
             (pos, commit_id, tree_id, parent_count, first_parent) \
             SELECT COALESCE(MAX(pos), 0) + 1, $1, \
             (SELECT ref_tree_hash FROM mega_refs WHERE path = '/' AND ref_name = 'refs/heads/main'), \
             2, NULL FROM mega_view_root_chain_scan",
            [Value::from("e".repeat(40))],
        ),
    );
}

fn view_ssh_container_id() -> String {
    let output = Command::new("docker")
        .args([
            "ps",
            "-q",
            "--filter",
            "label=com.docker.compose.project=mega2-it",
            "--filter",
            "label=com.docker.compose.service=git-cli",
            "--filter",
            "status=running",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .next()
        .expect("git-cli container")
        .to_owned()
}

fn raw_view_ssh_container(
    case_dir: &Path,
    port: u16,
    v2: bool,
    path: &str,
) -> std::process::Output {
    let mut ssh = git_cli::git_ssh_command(case_dir, port);
    if v2 {
        ssh.push_str(" -o SetEnv=GIT_PROTOCOL=version=2");
    }
    let shell = format!(
        "{ssh} git@{} \"git-upload-pack '{path}'\"",
        git_cli::mega2_reachable_host()
    );
    Command::new("docker")
        .args(["exec", "-i", &view_ssh_container_id(), "sh", "-c", &shell])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn integration_git_ssh_view_layer3_unknown() {
    git_cli::require_git_cli_runner();
    let env = layer3_ssh_env();
    let (mut service, http_port, port, _, err) = boot_view_multi(&env, "true");
    git_cli::write_known_hosts_via_keyscan(&env.ssh_dir.join("known_hosts"), port);
    layer3_ssh_seed_project(&env, http_port);
    layer3_ssh_register(&env, http_port, "known", ":/project");
    let paths = [
        "/.view/missing.git".to_owned(),
        "/.view/known@9.git".to_owned(),
        format!("/.filter/{}.git", "a".repeat(64)),
    ];
    for path in paths {
        for v2 in [false, true] {
            let output = raw_view_ssh_container(&env.case_dir, port, v2, &path);
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert_eq!(output.stdout, b"0017ERR view not found\n", "{output:?}");
        }
        let remote = format!("ssh://git@{}:{port}{path}", git_cli::mega2_reachable_host());
        let command = git_cli::git_ssh_command(&env.case_dir, port);
        let output = git_cli::git_cli_ssh(&env.case_dir, &command, &["ls-remote", &remote]);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("remote error: view not found"),
            "{output:?}"
        );
    }
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

#[test]
fn integration_git_ssh_view_layer3_unready() {
    git_cli::require_git_cli_runner();
    let env = layer3_ssh_env();
    let (mut service, http_port, port, _, err) = boot_view_multi(&env, "true");
    git_cli::write_known_hosts_via_keyscan(&env.ssh_dir.join("known_hosts"), port);
    layer3_ssh_seed_project(&env, http_port);
    let (cold_pk, cold) = layer3_ssh_register(&env, http_port, "cold", ":/project");
    let (recycled_pk, recycled) =
        layer3_ssh_register(&env, http_port, "recycled", ":/project:prefix=recycled");
    let (ready_pk, ready) = layer3_ssh_register(&env, http_port, "ready", ":/project:prefix=ready");
    layer3_ssh_wait_idle(&env, &[cold_pk, recycled_pk, ready_pk]);
    view_ssh_set_ready(&env, cold_pk, true);
    layer3_ssh_recycle(&env, recycled_pk);
    for (name, filter_id, reason) in [
        ("cold", &cold, "warming up"),
        ("recycled", &recycled, "warming up"),
    ] {
        for v2 in [false, true] {
            let output =
                raw_view_ssh_container(&env.case_dir, port, v2, &format!("/.view/{name}.git"));
            assert_eq!(output.status.code(), Some(75), "{output:?}");
            assert_eq!(
                &output.stdout[4..],
                format!("ERR view {filter_id} unavailable: {reason}\n").as_bytes(),
                "{output:?}"
            );
        }
        let remote = format!(
            "ssh://git@{}:{port}/.view/{name}.git",
            git_cli::mega2_reachable_host()
        );
        let output = git_cli::git_cli_ssh(
            &env.case_dir,
            &git_cli::git_ssh_command(&env.case_dir, port),
            &["ls-remote", &remote],
        );
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("remote error: view {filter_id} unavailable")),
            "{output:?}"
        );
    }
    view_ssh_halt_root_chain(&env);
    for v2 in [false, true] {
        let output = raw_view_ssh_container(&env.case_dir, port, v2, "/.view/ready.git");
        assert_eq!(output.status.code(), Some(75), "{output:?}");
        assert_eq!(
            &output.stdout[4..],
            format!("ERR view {ready} unavailable: root chain halted\n").as_bytes(),
            "{output:?}"
        );
    }
    let remote = format!(
        "ssh://git@{}:{port}/.view/ready.git",
        git_cli::mega2_reachable_host()
    );
    let output = git_cli::git_cli_ssh(
        &env.case_dir,
        &git_cli::git_ssh_command(&env.case_dir, port),
        &["ls-remote", &remote],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&format!("remote error: view {ready} unavailable")),
        "{output:?}"
    );
    assert_eq!(layer3_ssh_state(&env, cold_pk).0, None);
    assert!(!layer3_ssh_state(&env, recycled_pk).2);
    layer3_ssh_clear_halt(&env);
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

#[test]
fn integration_git_ssh_view_layer3_ls_remote() {
    git_cli::require_git_cli_runner();
    let env = layer3_ssh_env();
    let (mut service, http_port, port, _, err) = boot_view_multi(&env, "true");
    git_cli::write_known_hosts_via_keyscan(&env.ssh_dir.join("known_hosts"), port);
    layer3_ssh_seed_project(&env, http_port);
    let (ready_pk, _) = layer3_ssh_register(&env, http_port, "ready", ":/project");
    let tip = layer3_ssh_tip(&env, ready_pk);
    let remote = format!(
        "ssh://git@{}:{port}/.view/ready.git",
        git_cli::mega2_reachable_host()
    );
    let ssh = git_cli::git_ssh_command(&env.case_dir, port);
    for v0 in [true, false] {
        let args = if v0 {
            vec!["-c", "protocol.version=0", "ls-remote", &remote]
        } else {
            vec!["ls-remote", &remote]
        };
        let output = git_cli::git_cli_ssh(&env.case_dir, &ssh, &args);
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!("{tip}\tHEAD\n")), "{stdout}");
        assert!(
            stdout.contains(&format!("{tip}\trefs/heads/main\n")),
            "{stdout}"
        );
    }
    let output = git_cli::git_cli_ssh(&env.case_dir, &ssh, &["ls-remote", "--symref", &remote]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ref: refs/heads/main\tHEAD"),
        "{output:?}"
    );
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

fn hp22_ssh_boot() -> (GitSshEnv, ServiceProcess, u16, u16, PathBuf) {
    assert!(!git_cli::git_cli_skip_requested());
    git_cli::require_git_cli_runner();
    let env = GitSshEnv::new();
    git_cli::generate_client_ed25519(&env.ssh_dir.join("client_ed25519"));
    let extra = [
        ("MEGA_VIEWS__ENABLED", "true"),
        ("MEGA_VIEWS__ALLOW_ANONYMOUS_REGISTER", "true"),
        ("MEGA_MONOREPO__PUSH_POLICY", "trunk"),
        ("MEGA_GIT__PUSH_AUTH", "none"),
        ("MEGA_GIT__SSH_RECEIVE_PACK", "false"),
        ("MEGA_GIT__ANONYMOUS_ACCESS", "true"),
    ];
    let (mut first, _, _, _, first_err) = boot_service_multi_with_extra(&env, "off", None, &extra);
    assert!(
        first.shutdown_via_sigint(Duration::from_secs(10)).success(),
        "{}",
        read_log(&first_err)
    );
    drop(first);
    let (service, http_port, ssh_port, _, err) =
        boot_service_multi_with_extra(&env, "off", None, &extra);
    git_cli::write_known_hosts_via_keyscan(&env.ssh_dir.join("known_hosts"), ssh_port);
    (env, service, http_port, ssh_port, err)
}

fn hp23_ssh_git_input(case_dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .current_dir(case_dir)
        .args(["-C", "hp23-source"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    git_cli::assert_git_success(&output, "HP-23 SSH fixture git");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Hp23SshFixture {
    root_tip: String,
    prior_root: String,
    views: Vec<(String, String)>,
    mode_tree: String,
    gbk_tree: String,
    missing_tree: String,
}

fn hp23_ssh_fixture(env: &GitSshEnv, http_port: u16) -> Hp23SshFixture {
    let case_dir = env.case_dir.as_path();
    let project_url = git_cli::mega2_host_http_url(http_port, "/project");
    hp22_ssh_host_git(case_dir, &["clone", &project_url, "hp23-source"]);
    hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "config", "user.name", "HP23"],
    );
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp23-source",
            "config",
            "user.email",
            "hp23@example.invalid",
        ],
    );
    for (name, file) in [
        ("mode", "good.txt"),
        ("gbk", "good.txt"),
        ("ok", "good.txt"),
        ("miss/d", "hp23-only-in-miss-d.txt"),
    ] {
        let path = case_dir
            .join("hp23-source")
            .join(format!("hp23-{name}"))
            .join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, name).unwrap();
    }
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp23-source",
            "add",
            "hp23-mode",
            "hp23-gbk",
            "hp23-ok",
            "hp23-miss",
        ],
    );
    hp22_ssh_host_git(case_dir, &["-C", "hp23-source", "commit", "-m", "HP23 R1"]);
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp23-source",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    let blob = hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "rev-parse", "HEAD:hp23-mode/good.txt"],
    )
    .trim()
    .to_owned();
    let mode_input = format!("100664 blob {blob}\tmode.txt\n");
    let mode_tree = hp23_ssh_git_input(case_dir, &["mktree"], mode_input.as_bytes());
    let gbk_input = format!("100644 blob {blob}\t\"\\304\\343\\272\\303.txt\"\n");
    let gbk_tree = hp23_ssh_git_input(case_dir, &["mktree"], gbk_input.as_bytes());
    let ok_tree = hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "rev-parse", "HEAD:hp23-ok"],
    )
    .trim()
    .to_owned();
    let miss_tree = hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "rev-parse", "HEAD:hp23-miss"],
    )
    .trim()
    .to_owned();
    let missing_tree = hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "rev-parse", "HEAD:hp23-miss/d"],
    )
    .trim()
    .to_owned();
    let prior_tree = hp22_ssh_host_git(case_dir, &["-C", "hp23-source", "ls-tree", "HEAD"]);
    let root_input = prior_tree
        .lines()
        .map(|line| {
            let name = line.rsplit_once('\t').unwrap().1;
            let replacement = match name {
                "hp23-mode" => Some(&mode_tree),
                "hp23-gbk" => Some(&gbk_tree),
                "hp23-ok" => Some(&ok_tree),
                "hp23-miss" => Some(&miss_tree),
                _ => None,
            };
            replacement.map_or_else(
                || format!("{line}\n"),
                |id| format!("040000 tree {id}\t{name}\n"),
            )
        })
        .collect::<String>();
    let root_tree = hp23_ssh_git_input(case_dir, &["mktree"], root_input.as_bytes());
    let parent = hp22_ssh_host_git(case_dir, &["-C", "hp23-source", "rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let r2 = hp23_ssh_git_input(
        case_dir,
        &["commit-tree", &root_tree, "-p", &parent, "-m", "HP23 R2"],
        b"",
    );
    hp22_ssh_host_git(
        case_dir,
        &["-C", "hp23-source", "update-ref", "refs/heads/main", &r2],
    );
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp23-source",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
    with_runtime(async {
        let db = Database::connect(env.database.db_url.as_str())
            .await
            .unwrap();
        for id in [&mode_tree, &gbk_tree] {
            let row = db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("SELECT sub_trees FROM mega_tree WHERE tree_id = '{id}'"),
                ))
                .await
                .unwrap()
                .expect("L0 tree row");
            let bytes: Vec<u8> = row.try_get("", "sub_trees").unwrap();
            let actual = git_internal::hash::ObjectHash::from_type_and_data_for_kind(
                git_internal::hash::HashKind::Sha1,
                git_internal::internal::object::types::ObjectType::Tree,
                &bytes,
            )
            .unwrap();
            assert_ne!(actual.to_string(), *id, "L0 fixture must be noncanonical");
        }
    });
    let mut views = Vec::new();
    for (name, spec) in [
        ("mode", ":/project/hp23-mode"),
        ("gbk", ":/project/hp23-gbk:prefix=p"),
        ("ok", ":/project/hp23-ok"),
        ("miss", ":/project/hp23-miss"),
    ] {
        let id = hp22_ssh_register(http_port, &format!("hp23-{name}"), spec);
        hp22_ssh_wait_lag_zero(http_port, &id);
        views.push((name.to_owned(), id));
    }
    Hp23SshFixture {
        root_tip: r2,
        prior_root: parent,
        views,
        mode_tree,
        gbk_tree,
        missing_tree,
    }
}

fn hp23_ssh_view<'a>(fixture: &'a Hp23SshFixture, name: &str) -> &'a str {
    &fixture.views.iter().find(|(key, _)| key == name).unwrap().1
}

fn hp23_ssh_commits(db_url: &str, filter_id: &str) -> Vec<String> {
    with_runtime(async {
        let db = Database::connect(db_url).await.unwrap();
        db.query_all_raw(Statement::from_string(DatabaseBackend::Postgres,
            format!("SELECT m.view_commit FROM mega_view_commit_map m JOIN mega_view_filter f ON f.id = m.filter_pk WHERE f.filter_id = '{filter_id}' AND m.view_commit IS NOT NULL ORDER BY m.seq_from")))
            .await.unwrap().into_iter().map(|row| row.try_get("", "view_commit").unwrap()).collect()
    })
}

fn hp23_ssh_fetch_request(v2: bool, want: &str, have: Option<&str>, done: bool) -> Vec<u8> {
    let mut body = Vec::new();
    if v2 {
        body.extend(layer3_pkt("command=fetch\n"));
        body.extend(b"0001");
        body.extend(layer3_pkt(&format!("want {want}\n")));
    } else {
        let capability = if have.is_some() {
            " multi_ack_detailed"
        } else {
            ""
        };
        body.extend(layer3_pkt(&format!("want {want}{capability}\n")));
    }
    if let Some(have) = have {
        body.extend(layer3_pkt(&format!("have {have}\n")));
    }
    if v2 && done {
        body.extend(layer3_pkt("done\n"));
    }
    body.extend(b"0000");
    body
}

fn hp23_ssh_raw(
    case_dir: &Path,
    port: u16,
    id: &str,
    v2: bool,
    request: &[u8],
    err: bool,
) -> (Vec<u8>, i32) {
    let mut child = layer3_spawn_ssh(case_dir, port, v2, &format!("/.filter/{id}.git"));
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let advertisement = layer3_read_to_flush(&mut stdout);
    assert!(!advertisement.is_empty());
    stdin.write_all(request).unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let response = if err {
        layer3_read_pkt(&mut stdout)
    } else {
        layer3_read_to_flush(&mut stdout)
    };
    let mut trailing = Vec::new();
    stdout.read_to_end(&mut trailing).unwrap();
    assert!(
        trailing.is_empty(),
        "unexpected SSH trailing bytes: {trailing:?}"
    );
    let code = child.wait().unwrap().code().unwrap();
    (response, code)
}

#[test]
fn integration_git_ssh_view_precheck_raw() {
    let (env, mut service, http_port, ssh_port, err) = hp22_ssh_boot();
    let fixture = hp23_ssh_fixture(&env, http_port);
    let db_url = env.database.db_url.as_str();
    let ok_id = hp23_ssh_view(&fixture, "ok");
    let ok_tip = hp23_ssh_commits(db_url, ok_id).last().unwrap().clone();
    let not_our_ref = format!("ERR upload-pack: not our ref {}\n", fixture.root_tip);
    for (v2, have, done) in [
        (false, None, false),
        (false, Some(ok_tip.as_str()), false),
        (true, None, true),
        (true, Some(fixture.prior_root.as_str()), false),
        (true, Some(ok_tip.as_str()), false),
    ] {
        let request = hp23_ssh_fetch_request(v2, &fixture.root_tip, have, done);
        let (body, code) = hp23_ssh_raw(&env.case_dir, ssh_port, ok_id, v2, &request, true);
        assert_eq!(body, layer3_pkt(&not_our_ref));
        assert_eq!(code, 1);
    }
    let mut nak = layer3_pkt("acknowledgments\n");
    nak.extend(layer3_pkt("NAK\n"));
    nak.extend(b"0000");
    for name in ["ok", "mode", "gbk"] {
        let id = hp23_ssh_view(&fixture, name);
        let commits = hp23_ssh_commits(db_url, id);
        assert!(!commits.is_empty(), "{name}: {commits:?}");
        let want = commits.last().unwrap();
        if name != "ok" {
            assert!(commits.len() >= 2, "{name}: {commits:?}");
            let tree = if name == "mode" {
                &fixture.mode_tree
            } else {
                &fixture.gbk_tree
            };
            let error = format!(
                "ERR view {id} pack aborted: tree {tree} does not match its stored entries\n"
            );
            for (v2, have, done) in [
                (false, None, false),
                (false, Some(commits[commits.len() - 2].as_str()), false),
                (true, None, true),
                (true, Some(commits[commits.len() - 2].as_str()), false),
            ] {
                let request = hp23_ssh_fetch_request(v2, want, have, done);
                let (body, code) = hp23_ssh_raw(&env.case_dir, ssh_port, id, v2, &request, true);
                assert_eq!(body, layer3_pkt(&error));
                assert_eq!(code, 1);
            }
        }
        let request = hp23_ssh_fetch_request(true, want, Some(&fixture.prior_root), false);
        let (body, code) = hp23_ssh_raw(&env.case_dir, ssh_port, id, true, &request, false);
        assert_eq!(body, nak);
        assert_eq!(code, 0);
    }
    for rev in ["HEAD", &fixture.prior_root] {
        let tree_listing = hp22_ssh_host_git(
            &env.case_dir,
            &["-C", "hp23-source", "ls-tree", "-r", "-t", rev],
        );
        assert_eq!(tree_listing.matches(&fixture.missing_tree).count(), 1);
    }
    with_runtime(async {
        let db = Database::connect(db_url).await.unwrap();
        let deleted = db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "DELETE FROM mega_tree WHERE tree_id = '{}'",
                    fixture.missing_tree
                ),
            ))
            .await
            .unwrap();
        assert_eq!(deleted.rows_affected(), 1);
    });
    let miss_id = hp23_ssh_view(&fixture, "miss");
    let want = hp23_ssh_commits(db_url, miss_id).last().unwrap().clone();
    let error = format!(
        "ERR view {miss_id} pack aborted: tree {} is missing\n",
        fixture.missing_tree
    );
    for (v2, done) in [(false, false), (true, true)] {
        let request = hp23_ssh_fetch_request(v2, &want, None, done);
        let (body, code) = hp23_ssh_raw(&env.case_dir, ssh_port, miss_id, v2, &request, true);
        assert_eq!(body, layer3_pkt(&error));
        assert_eq!(code, 1);
    }
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

#[test]
fn integration_git_ssh_view_precheck_git_client_not_our_ref() {
    let (env, mut service, http_port, ssh_port, err) = hp22_ssh_boot();
    let fixture = hp23_ssh_fixture(&env, http_port);
    let id = hp23_ssh_view(&fixture, "ok");
    let url = format!(
        "{}.filter/{id}.git",
        git_cli::mega2_ssh_repo_url(ssh_port, "git")
    );
    let ssh_command = git_cli::git_ssh_command(&env.case_dir, ssh_port);
    git_cli::assert_git_success(
        &git_cli::git_cli_ssh(&env.case_dir, &ssh_command, &["init", "hp23-fetch"]),
        "HP-23 SSH fetch client init",
    );
    let output = git_cli::git_cli_ssh(
        &env.case_dir,
        &ssh_command,
        &[
            "-C",
            "hp23-fetch",
            "-c",
            "protocol.version=2",
            "fetch",
            &url,
            &fixture.root_tip,
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "remote error: upload-pack: not our ref {}",
            fixture.root_tip
        )),
        "{stderr}"
    );
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

#[test]
fn integration_git_ssh_view_precheck_git_client_l0_clone() {
    let (env, mut service, http_port, ssh_port, err) = hp22_ssh_boot();
    let fixture = hp23_ssh_fixture(&env, http_port);
    let ssh_command = git_cli::git_ssh_command(&env.case_dir, ssh_port);
    for (name, id) in &fixture.views {
        let url = format!(
            "{}.filter/{id}.git",
            git_cli::mega2_ssh_repo_url(ssh_port, "git")
        );
        let tree_id = if name == "mode" {
            &fixture.mode_tree
        } else {
            &fixture.gbk_tree
        };
        for version in ["0", "2"] {
            let clone = format!("hp23-{name}-v{version}");
            let output = git_cli::git_cli_ssh(
                &env.case_dir,
                &ssh_command,
                &[
                    "-c",
                    &format!("protocol.version={version}"),
                    "clone",
                    &url,
                    &clone,
                ],
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            if name == "ok" || name == "miss" {
                git_cli::assert_git_success(&output, "HP-23 SSH control clone");
            } else {
                assert!(!output.status.success(), "{output:?}");
                assert!(
                    stderr.contains("remote error:") && stderr.contains(tree_id),
                    "{stderr}"
                );
                assert!(
                    !stderr.contains("did not send all necessary objects"),
                    "{stderr}"
                );
            }
        }
    }
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

fn hp22_ssh_register(port: u16, name: &str, spec: &str) -> String {
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(git_cli::mega2_host_http_url(
            port,
            "/api/v1/views?wait=true",
        ))
        .json(&serde_json::json!({"name": name, "filter_spec": spec}))
        .send()
        .unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap();
    assert!(status.is_success(), "register {spec}: {status} {body}");
    assert_eq!(body["data"]["ready"], true, "{body}");
    body["data"]["filter_id"].as_str().unwrap().to_owned()
}

fn hp22_ssh_wait_lag_zero(port: u16, filter_id: &str) {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let path = format!("/api/v1/views/{filter_id}");
        let response = client
            .get(git_cli::mega2_host_http_url(port, &path))
            .send()
            .unwrap();
        if response.status().is_success() {
            let body: serde_json::Value = response.json().unwrap();
            if body["data"]["lag_commits"] == 0 {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "view {filter_id} did not catch up"
        );
        sleep(Duration::from_millis(250));
    }
}

fn hp22_ssh_host_git(case_dir: &Path, args: &[&str]) -> String {
    let output = git_host_ssh(case_dir, "", args);
    git_cli::assert_git_success(&output, "HP-22 host HTTP git");
    String::from_utf8(output.stdout).unwrap()
}

fn hp22_ssh_source_commit(case_dir: &Path, relative_path: &str, message: &str) {
    let path = case_dir.join("hp22-source").join(relative_path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"contents1\n").unwrap();
    hp22_ssh_host_git(case_dir, &["-C", "hp22-source", "add", relative_path]);
    hp22_ssh_host_git(case_dir, &["-C", "hp22-source", "commit", "-m", message]);
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp22-source",
            "push",
            "--no-thin",
            "origin",
            "HEAD:refs/heads/main",
        ],
    );
}

#[test]
fn integration_git_ssh_view_pack_ssh_josh_proxy() {
    let (env, mut service, http_port, ssh_port, err) = hp22_ssh_boot();
    let case_dir = env.case_dir.as_path();
    let project_url = git_cli::mega2_host_http_url(http_port, "/project");
    hp22_ssh_host_git(case_dir, &["clone", &project_url, "hp22-source"]);
    hp22_ssh_host_git(
        case_dir,
        &["-C", "hp22-source", "config", "user.name", "HP22"],
    );
    hp22_ssh_host_git(
        case_dir,
        &[
            "-C",
            "hp22-source",
            "config",
            "user.email",
            "hp22@example.invalid",
        ],
    );
    let ssh_command = git_cli::git_ssh_command(case_dir, ssh_port);
    for (case, first) in [
        ("subtree", "sub1/file1"),
        ("subsubtree", "sub1/subsub/file1"),
        ("prefix", "sub1/file1"),
    ] {
        hp22_ssh_source_commit(case_dir, &format!("hp22-{case}/{first}"), "add file1");
        hp22_ssh_source_commit(case_dir, &format!("hp22-{case}/sub2/file2"), "add file2");
        let spec = match case {
            "subtree" => ":/project/hp22-subtree/sub1",
            "subsubtree" => ":/project/hp22-subsubtree/sub1/subsub",
            _ => ":/project/hp22-prefix:prefix=pre",
        };
        let id = hp22_ssh_register(http_port, &format!("hp22-{case}"), spec);
        hp22_ssh_wait_lag_zero(http_port, &id);
        let path = if case == "prefix" {
            ".view/hp22-prefix@1.git".to_owned()
        } else {
            format!(".filter/{id}.git")
        };
        let url = format!("{}{}", git_cli::mega2_ssh_repo_url(ssh_port, "git"), path);
        let expected_log = fs::read(format!("tests/fixtures/views/josh_proxy/{case}.log")).unwrap();
        let expected_tree =
            fs::read(format!("tests/fixtures/views/josh_proxy/{case}.tree")).unwrap();
        for (label, v0) in [("v0", true), ("v2", false)] {
            let clone = format!("hp22-{case}-{label}");
            let mut args = Vec::new();
            if v0 {
                args.extend(["-c", "protocol.version=0"]);
            }
            args.extend(["clone", &url, &clone]);
            let output = git_cli::git_cli_ssh(case_dir, &ssh_command, &args);
            git_cli::assert_git_success(&output, "HP-22 SSH clone");
            let log = git_cli::git_cli_ssh(
                case_dir,
                &ssh_command,
                &["-C", &clone, "log", "--graph", "--pretty=%s"],
            );
            git_cli::assert_git_success(&log, "HP-22 SSH log");
            assert_eq!(log.stdout, expected_log);
            let tree = git_cli::git_cli_ssh(
                case_dir,
                &ssh_command,
                &["-C", &clone, "ls-tree", "-r", "--name-only", "HEAD"],
            );
            git_cli::assert_git_success(&tree, "HP-22 SSH tree");
            assert_eq!(tree.stdout, expected_tree);
            git_cli::assert_git_success(
                &git_cli::git_cli_ssh(case_dir, &ssh_command, &["-C", &clone, "fsck", "--strict"]),
                "HP-22 SSH fsck",
            );
        }
        let third = if case == "subsubtree" {
            "sub1/subsub/file3"
        } else {
            "sub1/file3"
        };
        hp22_ssh_source_commit(case_dir, &format!("hp22-{case}/{third}"), "add file3");
        hp22_ssh_wait_lag_zero(http_port, &id);
        let expected_log =
            fs::read(format!("tests/fixtures/views/josh_proxy/{case}.fetch.log")).unwrap();
        let expected_tree =
            fs::read(format!("tests/fixtures/views/josh_proxy/{case}.fetch.tree")).unwrap();
        for (label, v0) in [("v0", true), ("v2", false)] {
            let clone = format!("hp22-{case}-{label}");
            let mut args = vec!["-C", clone.as_str()];
            if v0 {
                args.extend(["-c", "protocol.version=0"]);
            }
            args.push("fetch");
            let fetch = git_cli::git_cli_ssh(case_dir, &ssh_command, &args);
            git_cli::assert_git_success(&fetch, "HP-22 SSH Josh fetch");
            let merge = git_cli::git_cli_ssh(
                case_dir,
                &ssh_command,
                &["-C", &clone, "merge", "--ff-only", "@{upstream}"],
            );
            git_cli::assert_git_success(&merge, "HP-22 SSH Josh fast forward");
            let log = git_cli::git_cli_ssh(
                case_dir,
                &ssh_command,
                &["-C", &clone, "log", "--graph", "--pretty=%s"],
            );
            git_cli::assert_git_success(&log, "HP-22 SSH Josh fetched log");
            assert_eq!(log.stdout, expected_log);
            let tree = git_cli::git_cli_ssh(
                case_dir,
                &ssh_command,
                &["-C", &clone, "ls-tree", "-r", "--name-only", "HEAD"],
            );
            git_cli::assert_git_success(&tree, "HP-22 SSH Josh fetched tree");
            assert_eq!(tree.stdout, expected_tree);
            git_cli::assert_git_success(
                &git_cli::git_cli_ssh(case_dir, &ssh_command, &["-C", &clone, "fsck", "--strict"]),
                "HP-22 SSH Josh fetched fsck",
            );
        }
    }
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

#[test]
fn integration_git_ssh_view_pack_ssh_ready_empty() {
    let (env, mut service, http_port, ssh_port, err) = hp22_ssh_boot();
    let id = hp22_ssh_register(http_port, "hp22-empty", ":/project/hp22-absent");
    hp22_ssh_wait_lag_zero(http_port, &id);
    let url = format!(
        "{}.filter/{id}.git",
        git_cli::mega2_ssh_repo_url(ssh_port, "git")
    );
    let ssh_command = git_cli::git_ssh_command(&env.case_dir, ssh_port);
    for (clone, v0) in [("hp22-empty-v0", true), ("hp22-empty-v2", false)] {
        let mut args = Vec::new();
        if v0 {
            args.extend(["-c", "protocol.version=0"]);
        }
        args.extend(["clone", &url, clone]);
        let output = git_cli::git_cli_ssh(&env.case_dir, &ssh_command, &args);
        git_cli::assert_git_success(&output, "HP-22 SSH empty clone");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("You appear to have cloned an empty repository")
        );
        let refs =
            git_cli::git_cli_ssh(&env.case_dir, &ssh_command, &["-C", clone, "for-each-ref"]);
        git_cli::assert_git_success(&refs, "HP-22 SSH empty refs");
        assert!(refs.stdout.is_empty());
    }
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

fn layer3_pkt(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

fn layer3_read_pkt(reader: &mut impl Read) -> Vec<u8> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).unwrap();
    let size = usize::from_str_radix(std::str::from_utf8(&header).unwrap(), 16).unwrap();
    let mut bytes = header.to_vec();
    if size > 4 {
        let mut payload = vec![0_u8; size - 4];
        reader.read_exact(&mut payload).unwrap();
        bytes.extend(payload);
    }
    bytes
}

fn layer3_read_to_flush(reader: &mut impl Read) -> Vec<u8> {
    let mut result = Vec::new();
    loop {
        let packet = layer3_read_pkt(reader);
        let flush = packet == b"0000";
        result.extend(packet);
        if flush {
            return result;
        }
    }
}

fn layer3_spawn_ssh(case_dir: &Path, port: u16, v2: bool, path: &str) -> Child {
    let mut ssh = git_cli::git_ssh_command(case_dir, port);
    if v2 {
        ssh.push_str(" -o SetEnv=GIT_PROTOCOL=version=2");
    }
    let shell = format!(
        "{ssh} git@{} \"git-upload-pack '{path}'\"",
        git_cli::mega2_reachable_host()
    );
    Command::new("timeout")
        .args([
            "-k",
            "5",
            "45",
            "docker",
            "exec",
            "-i",
            &view_ssh_container_id(),
            "sh",
            "-c",
            &shell,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn integration_git_ssh_view_layer3_unready_after_advertise() {
    git_cli::require_git_cli_runner();
    let env = layer3_ssh_env();
    let (mut service, http_port, port, _, err) = boot_view_multi(&env, "true");
    git_cli::write_known_hosts_via_keyscan(&env.ssh_dir.join("known_hosts"), port);
    layer3_ssh_seed_project(&env, http_port);
    let (ready_pk, filter_id) = layer3_ssh_register(&env, http_port, "ready", ":/project");
    layer3_ssh_wait_idle(&env, &[ready_pk]);
    let tip = layer3_ssh_tip(&env, ready_pk);

    let mut v0 = layer3_spawn_ssh(&env.case_dir, port, false, "/.view/ready.git");
    let mut input = v0.stdin.take().unwrap();
    let mut output = v0.stdout.take().unwrap();
    let advertise = layer3_read_to_flush(&mut output);
    assert!(
        advertise
            .windows(tip.len())
            .any(|part| part == tip.as_bytes())
    );
    view_ssh_set_ready(&env, ready_pk, false);
    input
        .write_all(&layer3_pkt(&format!("want {}\n", "b".repeat(40))))
        .unwrap();
    input.write_all(b"0000").unwrap();
    input.flush().unwrap();
    let reply = layer3_read_pkt(&mut output);
    assert_eq!(
        &reply[4..],
        format!("ERR view {filter_id} unavailable: warming up\n").as_bytes()
    );
    drop(input);
    assert_eq!(v0.wait().unwrap().code(), Some(75));
    let mut trailing = Vec::new();
    output.read_to_end(&mut trailing).unwrap();
    assert!(
        trailing.is_empty(),
        "unexpected v0 data after ERR: {trailing:?}"
    );

    view_ssh_execute(
        &env,
        Statement::from_string(
            DatabaseBackend::Postgres,
            format!("UPDATE mega_view_filter SET ready_seq = projected_seq WHERE id = {ready_pk}"),
        ),
    );
    let mut v2 = layer3_spawn_ssh(&env.case_dir, port, true, "/.view/ready.git");
    let mut input = v2.stdin.take().unwrap();
    let mut output = v2.stdout.take().unwrap();
    let capabilities = layer3_read_to_flush(&mut output);
    assert!(capabilities.windows(7).any(|part| part == b"ls-refs"));
    let mut fetch = layer3_pkt("command=fetch\n");
    fetch.extend(b"0001");
    fetch.extend(layer3_pkt(&format!("want {tip}\n")));
    fetch.extend(layer3_pkt(&format!("have {}\n", "b".repeat(40))));
    fetch.extend(b"0000");
    input.write_all(&fetch).unwrap();
    input.flush().unwrap();
    let first = layer3_read_to_flush(&mut output);
    let mut expected = layer3_pkt("acknowledgments\n");
    expected.extend(layer3_pkt("NAK\n"));
    expected.extend(b"0000");
    assert_eq!(first, expected);
    view_ssh_set_ready(&env, ready_pk, false);
    input.write_all(&fetch).unwrap();
    input.flush().unwrap();
    let reply = layer3_read_pkt(&mut output);
    assert_eq!(
        &reply[4..],
        format!("ERR view {filter_id} unavailable: warming up\n").as_bytes()
    );
    drop(input);
    assert_eq!(v2.wait().unwrap().code(), Some(75));
    let mut trailing = Vec::new();
    output.read_to_end(&mut trailing).unwrap();
    assert!(
        trailing.is_empty(),
        "unexpected v2 data after ERR: {trailing:?}"
    );
    assert_eq!(layer3_ssh_state(&env, ready_pk).0, None);
    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(10))
            .success(),
        "{}",
        read_log(&err)
    );
}

fn view_host_git_http(case_dir: &Path, args: &[&str]) -> std::process::Output {
    let home = case_dir.join("git-home-view-http");
    fs::create_dir_all(&home).unwrap();
    Command::new("timeout")
        .args(["-k", "5", "45", "git"])
        .current_dir(case_dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn integration_git_ssh_view_layer2_git_client_not_found() {
    git_cli::require_git_cli_runner();
    let env = view_ssh_env();
    let (mut service, http_port, ssh_port, _out, err) = boot_view_multi(&env, "false");
    let http = format!("http://127.0.0.1:{http_port}/.view/x.git");
    let http_result = view_host_git_http(&env.case_dir, &["ls-remote", &http]);
    assert!(!http_result.status.success(), "{http_result:?}");
    assert!(
        String::from_utf8_lossy(&http_result.stderr).contains("not found"),
        "{http_result:?}"
    );
    let ssh_command = git_ssh_command_loopback(&env.case_dir, ssh_port, true);
    let ssh = format!("ssh://git@127.0.0.1:{ssh_port}/.view/x.git");
    let ssh_result = git_host_ssh(&env.case_dir, &ssh_command, &["ls-remote", &ssh]);
    assert!(!ssh_result.status.success(), "{ssh_result:?}");
    assert!(
        String::from_utf8_lossy(&ssh_result.stderr).contains("remote error: view not found"),
        "{ssh_result:?}"
    );
    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(status.success(), "{}", read_log(&err));
}

#[test]
fn integration_git_ssh_view_layer2_git_client_read_only() {
    git_cli::require_git_cli_runner();
    let env = view_ssh_env();
    let (mut service, http_port, ssh_port, _out, err) = boot_view_multi(&env, "true");
    let root = format!("http://127.0.0.1:{http_port}/");
    let before = view_host_git_http(&env.case_dir, &["ls-remote", &root]);
    assert!(before.status.success(), "{before:?}");
    let clone = view_host_git_http(&env.case_dir, &["clone", &root, "view-client-root"]);
    assert!(clone.status.success(), "{clone:?}");
    fs::write(
        env.case_dir.join("view-client-root/view-client.txt"),
        b"view test\n",
    )
    .unwrap();
    let add = view_host_git_http(&env.case_dir, &["-C", "view-client-root", "add", "."]);
    assert!(add.status.success(), "{add:?}");
    let commit = view_host_git_http(
        &env.case_dir,
        &[
            "-C",
            "view-client-root",
            "-c",
            "user.name=HP18 Test",
            "-c",
            "user.email=hp18@example.invalid",
            "commit",
            "-m",
            "view rejection probe",
        ],
    );
    assert!(commit.status.success(), "{commit:?}");
    let http_view = format!("http://127.0.0.1:{http_port}/.view/x.git");
    let http_push = view_host_git_http(
        &env.case_dir,
        &[
            "-C",
            "view-client-root",
            "push",
            &http_view,
            "HEAD:refs/heads/main",
        ],
    );
    assert!(!http_push.status.success(), "{http_push:?}");
    assert!(
        String::from_utf8_lossy(&http_push.stderr).contains("returned error: 403"),
        "{http_push:?}"
    );
    let ssh_command = git_ssh_command_loopback(&env.case_dir, ssh_port, true);
    let ssh_view = format!("ssh://git@127.0.0.1:{ssh_port}/.view/x.git");
    let ssh_push = git_host_ssh(
        &env.case_dir,
        &ssh_command,
        &[
            "-C",
            "view-client-root",
            "push",
            &ssh_view,
            "HEAD:refs/heads/main",
        ],
    );
    assert!(!ssh_push.status.success(), "{ssh_push:?}");
    assert!(
        String::from_utf8_lossy(&ssh_push.stderr).contains("remote error: view URLs are read-only"),
        "{ssh_push:?}"
    );
    let after = view_host_git_http(&env.case_dir, &["ls-remote", &root]);
    assert!(after.status.success(), "{after:?}");
    assert_eq!(before.stdout, after.stdout);
    let status = service.shutdown_via_sigint(Duration::from_secs(10));
    assert!(status.success(), "{}", read_log(&err));
}
