// Git-cli black-box helpers (IT-03 / IT-10 / IT-12; extended by GM-05/GM-06..08).
//
// Included via `#[path = "common/git_cli.rs"]` only by the git-facing targets
// (`integration_git_cli.rs`, `integration_git_lfs.rs`, `integration_git_ssh.rs`)
// so vault and other bin test targets do not compile these symbols.

use std::{
    env, fs,
    io::Read,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
    thread::{self, sleep},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};

pub const GIT_CLI_UNAVAILABLE: &str = "git-cli runner unavailable";
pub const COMPOSE_PROJECT: &str = "monoengine-it";
pub const GIT_ASKPASS_ENV: &str = "MONOENGINE_IT_GIT_PASSWORD";
pub const DEFAULT_GIT_AUTH_USER: &str = "it-git-cli";
/// Host loopback for probes from the cargo test process (curl, TcpStream, service bind).
pub const HOST_LOOPBACK: &str = "127.0.0.1";
/// Docker Desktop / Compose gateway hostname for reaching host-bound services from git-cli.
pub const DOCKER_HOST_GATEWAY: &str = "host.docker.internal";
/// Must match `docs/refactoring/test-infra.md` / compose `alpine/git:v2.49.1`.
pub const PINNED_GIT_CLI_VERSION: &str = "git version 2.49.1";
/// Per-invocation wall-clock budget so a stalled protocol cannot hang `cargo test`.
pub const GIT_CLI_COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
/// Bound the initial docker runner probe so `cargo test` cannot wedge
/// indefinitely when the docker daemon is unhealthy.
pub const DOCKER_RUNNER_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitRunnerKind {
    Container,
    Host,
}

static RUNNER: OnceLock<GitRunnerKind> = OnceLock::new();

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("resolve repository root")
}

pub fn git_cli_workdir() -> PathBuf {
    // Default matches compose `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}`.
    // Relative values are resolved against the repo root (compose file directory),
    // not Cargo's `bin/` CWD, so host paths stay aligned with the bind mount.
    match env::var("MONOENGINE_IT_GIT_WORKDIR") {
        Ok(raw) => {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                repo_root().join(path)
            }
        }
        Err(_) => PathBuf::from("/tmp/monoengine-git"),
    }
}

pub fn git_cli_skip_requested() -> bool {
    matches!(env::var("MONOENGINE_IT_SKIP_GIT_CLI").as_deref(), Ok("1"))
}

fn host_git_opt_in_allowed() -> bool {
    // Compose `git-cli` is the only supported runner for CI / VER. Host git is
    // an explicit local opt-in (`MONOENGINE_IT_ALLOW_HOST_GIT=1`), not a
    // cross-platform compatibility path.
    matches!(env::var("MONOENGINE_IT_ALLOW_HOST_GIT").as_deref(), Ok("1"))
}

/// Resolve the one-shot access token used for receive-pack tests.
/// Prefers `MONOENGINE_IT_SEED_TOKEN`; otherwise generates a random value.
pub fn resolve_seed_token() -> String {
    env::var("MONOENGINE_IT_SEED_TOKEN").unwrap_or_else(|_| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        format!("it-git-token-{nanos}-{}", std::process::id())
    })
}

pub fn append_evidence_line(path_env: &str, line: &str) {
    let Some(path) = env::var_os(path_env) else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|err| panic!("open evidence file {}: {err}", path.display()));
    writeln!(file, "{line}").expect("write evidence line");
}

pub fn record_allocated_port(port: u16) {
    append_evidence_line("MONOENGINE_IT_PORTS_FILE", &port.to_string());
}

pub fn record_service_pid(pid: u32) {
    append_evidence_line("MONOENGINE_IT_PIDS_FILE", &pid.to_string());
}

/// Bind `127.0.0.1:0`, capture the OS-assigned port, then drop the listener.
#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn reserve_ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn wait_until_port_closed(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        sleep(Duration::from_millis(100));
    }
    panic!("owned service port {port} still accepts connections after shutdown");
}

#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_port_refuses(port: u16) {
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port {port} must refuse connections after SSH lifecycle cleanup"
    );
}

#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_process_reaped(pid: u32) {
    // SAFETY: signal 0 only probes whether the former child PID still exists.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    assert_eq!(result, -1, "owned service process {pid} must be gone");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "owned service process {pid} must be fully reaped"
    );
}

/// Assert the Vault DB holds ciphertext for `ssh_server_key` (no plaintext PEM).
#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_ssh_server_key_ciphertext_in_db(db_url: &str) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect DB to assert vault ssh_server_key: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT key, value FROM vault WHERE key LIKE '%ssh_server_key%'".to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("query vault ssh_server_key: {err}"));
        assert!(
            !rows.is_empty(),
            "expected at least one vault row matching ssh_server_key"
        );
        for row in &rows {
            let key: String = row
                .try_get("", "key")
                .unwrap_or_else(|err| panic!("read vault key: {err}"));
            let value: Vec<u8> = row
                .try_get("", "value")
                .unwrap_or_else(|err| panic!("read vault value for {key}: {err}"));
            assert!(
                !value.is_empty(),
                "vault row {key} ciphertext must be non-empty"
            );
            let as_utf8 = String::from_utf8_lossy(&value);
            assert!(
                !as_utf8.contains("BEGIN OPENSSH PRIVATE KEY"),
                "vault row {key} must store ciphertext, not plaintext OpenSSH PEM"
            );
        }
    });
}

/// Walk `root` and fail if any file contains an OpenSSH private key PEM marker.
#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_no_openssh_private_key_under(root: &Path) {
    if !root.exists() {
        return;
    }
    fn walk(path: &Path) {
        if path.is_dir() {
            for entry in
                fs::read_dir(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
            {
                walk(&entry.expect("dir entry").path());
            }
            return;
        }
        let bytes = fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("BEGIN OPENSSH PRIVATE KEY"),
            "host private key plaintext must not appear on disk at {}",
            path.display()
        );
    }
    walk(root);
}

/// Confirm the named database no longer exists on the admin URL.
#[allow(
    dead_code,
    reason = "SSH lifecycle helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_database_absent(admin_url: &str, db_name: &str) {
    with_runtime(async {
        let db = Database::connect(admin_url)
            .await
            .unwrap_or_else(|err| panic!("connect admin DB to assert drop: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT 1 FROM pg_database WHERE datname = '{db_name}'"),
            ))
            .await
            .unwrap_or_else(|err| panic!("query pg_database for {db_name}: {err}"));
        assert!(
            rows.is_empty(),
            "temporary database {db_name} must be dropped after SSH lifecycle cleanup"
        );
    });
}

fn compose_up_git_cli_hint() -> String {
    format!(
        "hint: start the opt-in runner with \
         `docker compose -p {COMPOSE_PROJECT} -f docker-compose.test.yml --profile git up -d --wait` \
         (see docs/refactoring/test-infra.md / docs/development.md)"
    )
}

/// Hostname git-cli uses to reach per-case monoengine / SSH listeners on the host.
///
/// Container runner: `host.docker.internal` (bridge + extra_hosts).
/// Host opt-in runner: loopback.
pub fn monoengine_reachable_host() -> &'static str {
    require_git_cli_runner();
    match runner_kind() {
        GitRunnerKind::Container => DOCKER_HOST_GATEWAY,
        GitRunnerKind::Host => HOST_LOOPBACK,
    }
}

pub fn monoengine_http_repo_url(port: u16) -> String {
    format!("http://{}:{port}/", monoengine_reachable_host())
}

pub fn monoengine_http_url(port: u16, path: &str) -> String {
    format!("http://{}:{port}{path}", monoengine_reachable_host())
}

#[allow(
    dead_code,
    reason = "SSH remote URL helper; path-included into HTTP targets that do not call it yet"
)]
pub fn monoengine_ssh_repo_url(port: u16, user: &str) -> String {
    format!("ssh://{user}@{}:{port}/", monoengine_reachable_host())
}

/// Running container id for compose service `git-cli` in project `monoengine-it`.
///
/// Uses `docker ps` label filters instead of `docker compose exec` so the probe
/// does not contend on the Compose project lock (parallel cargo tests were
/// timing out the 15s probe while waiting on that lock).
fn running_git_cli_container_id() -> Result<String, String> {
    let mut command = Command::new("docker");
    command.args([
        "ps",
        "-q",
        "--filter",
        &format!("label=com.docker.compose.project={COMPOSE_PROJECT}"),
        "--filter",
        "label=com.docker.compose.service=git-cli",
        "--filter",
        "status=running",
    ]);
    let output = try_output_with_timeout(
        command,
        DOCKER_RUNNER_PROBE_TIMEOUT,
        "docker ps git-cli filter",
    )?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        Err(format!(
            "no running git-cli container for project {COMPOSE_PROJECT}"
        ))
    } else {
        // `docker ps -q` may return multiple lines if replicas exist; take first.
        Ok(id.lines().next().unwrap_or(&id).trim().to_string())
    }
}

fn docker_exec_base() -> Command {
    let mut command = Command::new("docker");
    // Options (`-w`, `-e`, …) must be appended *before* the container id.
    command.arg("exec");
    command
}

fn try_container_git_version() -> Result<String, String> {
    let container_id = running_git_cli_container_id()?;
    let mut command = docker_exec_base();
    command.arg(&container_id).args(["git", "--version"]);
    // Must return Err (not panic) so opt-in host-git remains reachable when
    // docker is missing or the daemon/probe stalls.
    let output = try_output_with_timeout(
        command,
        DOCKER_RUNNER_PROBE_TIMEOUT,
        "docker exec git-cli probe",
    )?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn try_host_git_version() -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("--version");
    let output = try_output_with_timeout(command, DOCKER_RUNNER_PROBE_TIMEOUT, "host git probe")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn resolve_runner_kind() -> GitRunnerKind {
    let container_err = match try_container_git_version() {
        Ok(version) if version == PINNED_GIT_CLI_VERSION => {
            return GitRunnerKind::Container;
        }
        Ok(version) => {
            format!("unexpected git version [{version}], expected [{PINNED_GIT_CLI_VERSION}]")
        }
        Err(err) => err,
    };

    if host_git_opt_in_allowed() {
        match try_host_git_version() {
            Ok(version) => {
                // Host opt-in is non-CI; keep a visible breadcrumb. Avoid printing on
                // the default container path so `cargo test` keeps `... ok` on one line
                // (IT-10 VER scans for `${case} ... ok`).
                eprintln!(
                    "git-cli runner (host opt-in): {version} (compose git-cli unavailable: {container_err})"
                );
                GitRunnerKind::Host
            }
            Err(host_err) => {
                eprintln!("{GIT_CLI_UNAVAILABLE}: compose={container_err}; host={host_err}");
                panic!("{GIT_CLI_UNAVAILABLE}");
            }
        }
    } else {
        eprintln!(
            "{GIT_CLI_UNAVAILABLE}: docker exec probe failed: {container_err}\n{}",
            compose_up_git_cli_hint()
        );
        panic!("{GIT_CLI_UNAVAILABLE}");
    }
}

/// Resolve the runner once per process. Prefer compose `git-cli`; host git only
/// when `MONOENGINE_IT_ALLOW_HOST_GIT=1` (local experiments).
///
/// Serialized via `OnceLock::get_or_init` so parallel tests do not stampede
/// docker with concurrent probes.
pub fn require_git_cli_runner() {
    if git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    let _ = RUNNER.get_or_init(resolve_runner_kind);
}

fn runner_kind() -> GitRunnerKind {
    require_git_cli_runner();
    *RUNNER
        .get()
        .unwrap_or_else(|| panic!("{GIT_CLI_UNAVAILABLE}: runner not selected"))
}

/// Write a GIT_ASKPASS script that prints `$MONOENGINE_IT_GIT_PASSWORD`.
/// The token itself is never written into the script body.
pub fn write_git_askpass(path: &Path) {
    // Use printf (not echo) so tokens like `-n` or `\c` are not interpreted.
    let script = format!(
        "#!/bin/sh\n# Generated by monoengine IT helpers; reads {GIT_ASKPASS_ENV}.\nprintf '%s\\n' \"${GIT_ASKPASS_ENV}\"\n"
    );
    fs::write(path, script).expect("write GIT_ASKPASS script");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("askpass metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod askpass");
}

/// Canonicalize a path that may not exist yet: resolve the deepest existing
/// ancestor and re-append the remaining components. Keeps both sides of the
/// workdir prefix check symmetric on hosts where the shared workdir sits
/// behind a symlink (macOS `/tmp` -> `/private/tmp`); plain paths on the
/// Linux target OS are unaffected.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(resolved) = cur.canonicalize() {
            let mut out = resolved;
            for component in rest.iter().rev() {
                out.push(component);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn container_path_for_host(host_path: &Path) -> String {
    let workdir = git_cli_workdir();
    let abs = canonicalize_lenient(host_path);
    let work_abs = canonicalize_lenient(&workdir);
    let rel = abs.strip_prefix(&work_abs).unwrap_or_else(|_| {
        panic!(
            "git path {} must live under shared workdir {}",
            abs.display(),
            work_abs.display()
        )
    });
    format!("/work/{}", rel.display())
}

/// Run `git` via the selected runner with credential injection through
/// GIT_ASKPASS. For the container runner the token is placed in the docker CLI
/// process environment and forwarded with `-e NAME` (no `NAME=value` on argv).
pub fn git_cli(case_dir: &Path, token: &str, git_args: &[&str]) -> Output {
    git_cli_with_env(case_dir, token, &[], git_args)
}

/// Run `git` authenticated as an explicit username (UN-16 e2e: the seeded
/// persistent admin `benjamin_747` pushes the `/.mega_cedar.json` change, then
/// the default `it-git-cli` user's push is asserted granted/revoked).
pub fn git_cli_as_user(case_dir: &Path, username: &str, token: &str, git_args: &[&str]) -> Output {
    git_cli_with_user_env(case_dir, username, token, &[], git_args)
}

#[allow(
    dead_code,
    reason = "used by integration_git_lfs; this file is also path-included by integration_git_cli"
)]
pub fn git_cli_lfs_skip_smudge(case_dir: &Path, token: &str, git_args: &[&str]) -> Output {
    git_cli_with_env(case_dir, token, &[("GIT_LFS_SKIP_SMUDGE", "1")], git_args)
}

fn git_cli_with_env(
    case_dir: &Path,
    token: &str,
    command_env: &[(&str, &str)],
    git_args: &[&str],
) -> Output {
    git_cli_with_user_env(
        case_dir,
        DEFAULT_GIT_AUTH_USER,
        token,
        command_env,
        git_args,
    )
}

fn git_cli_with_user_env(
    case_dir: &Path,
    username: &str,
    token: &str,
    command_env: &[(&str, &str)],
    git_args: &[&str],
) -> Output {
    require_git_cli_runner();
    if git_cli_skip_requested() {
        panic!("{GIT_CLI_UNAVAILABLE}: skipped runner cannot execute git");
    }

    let askpass_host = case_dir.join("git-askpass.sh");
    if !askpass_host.exists() {
        write_git_askpass(&askpass_host);
    }

    match runner_kind() {
        GitRunnerKind::Container => git_cli_container(
            case_dir,
            &askpass_host,
            username,
            token,
            command_env,
            git_args,
        ),
        GitRunnerKind::Host => git_cli_host(
            case_dir,
            &askpass_host,
            username,
            token,
            command_env,
            git_args,
        ),
    }
}

/// Run `git` via the selected runner **without** credential injection.
/// Used by IT-10 to assert unauthenticated receive-pack is rejected.
pub fn git_cli_no_auth(case_dir: &Path, git_args: &[&str]) -> Output {
    require_git_cli_runner();
    if git_cli_skip_requested() {
        panic!("{GIT_CLI_UNAVAILABLE}: skipped runner cannot execute git");
    }

    match runner_kind() {
        GitRunnerKind::Container => git_cli_container_no_auth(case_dir, git_args),
        GitRunnerKind::Host => git_cli_host_no_auth(case_dir, git_args),
    }
}

fn git_cli_container_no_auth(case_dir: &Path, git_args: &[&str]) -> Output {
    let work_container = container_path_for_host(case_dir);
    let container_id =
        running_git_cli_container_id().unwrap_or_else(|err| panic!("{GIT_CLI_UNAVAILABLE}: {err}"));

    let mut command = docker_exec_base();
    command
        .arg("-w")
        .arg(&work_container)
        .arg("-e")
        .arg("GIT_TERMINAL_PROMPT=0")
        .arg("-e")
        .arg("GIT_ASKPASS=true")
        .arg("-e")
        .arg("GIT_CONFIG_COUNT=1")
        .arg("-e")
        .arg("GIT_CONFIG_KEY_0=credential.helper")
        .arg("-e")
        .arg("GIT_CONFIG_VALUE_0=")
        .arg(&container_id);
    append_container_git_timeout_wrapper(&mut command);
    command.args(git_args);

    output_with_timeout(
        command,
        GIT_CLI_COMMAND_TIMEOUT + Duration::from_secs(15),
        "docker exec git-cli (no-auth)",
    )
}

/// In-container wall-clock wrapper around `git`.
///
/// Important: do **not** wrap the script itself in `setsid`. BusyBox `setsid`
/// starts a new session, and `docker exec` then reports exit status 0
/// even when the session leader exits non-zero (auth failures looked like
/// success while stderr still showed `fatal: Authentication failed`). Only the
/// `git` child is `setsid`'d so timeout can `kill -KILL -$gpid` its helpers.
fn append_container_git_timeout_wrapper(command: &mut Command) {
    command
        .arg("sh")
        .arg("-c")
        .arg(format!(
            "setsid git \"$@\" &\n\
             gpid=$!\n\
             sleep {secs} &\n\
             spid=$!\n\
             while kill -0 \"$gpid\" 2>/dev/null && kill -0 \"$spid\" 2>/dev/null; do\n\
               sleep 1\n\
             done\n\
             if kill -0 \"$gpid\" 2>/dev/null; then\n\
               kill -KILL -\"$gpid\" 2>/dev/null || kill -KILL \"$gpid\" 2>/dev/null || true\n\
               wait \"$gpid\" 2>/dev/null || true\n\
               kill \"$spid\" 2>/dev/null || true\n\
               wait \"$spid\" 2>/dev/null || true\n\
               exit 137\n\
             fi\n\
             wait \"$gpid\"\n\
             status=$?\n\
             kill \"$spid\" 2>/dev/null || true\n\
             wait \"$spid\" 2>/dev/null || true\n\
             exit \"$status\"\n",
            secs = GIT_CLI_COMMAND_TIMEOUT.as_secs()
        ))
        .arg("git-wrapper");
}

fn git_cli_host_no_auth(case_dir: &Path, git_args: &[&str]) -> Output {
    let isolated_home = case_dir.join("git-home-noauth");
    fs::create_dir_all(&isolated_home).expect("create isolated git HOME");
    let null_config = PathBuf::from("/dev/null");

    let mut command = Command::new("git");
    command
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove(GIT_ASKPASS_ENV)
        .env_remove("GIT_ASKPASS")
        .env("HOME", &isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home.join("xdg-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_1", "false")
        .args(git_args);
    output_with_timeout(command, GIT_CLI_COMMAND_TIMEOUT, "host git (no-auth)")
}

/// Probe receive-pack info/refs without credentials; returns status + headers body.
pub fn probe_receive_pack_challenge(port: u16) -> String {
    let url = format!("http://127.0.0.1:{port}/info/refs?service=git-receive-pack");
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "-D",
        "-",
        "-o",
        "/dev/null",
        "--max-time",
        "15",
        &url,
    ]);
    let output = output_with_timeout(command, Duration::from_secs(20), "curl receive-pack probe");
    assert!(
        output.status.success(),
        "curl probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git_cli_container(
    case_dir: &Path,
    askpass_host: &Path,
    username: &str,
    token: &str,
    command_env: &[(&str, &str)],
    git_args: &[&str],
) -> Output {
    let askpass_container = container_path_for_host(askpass_host);
    let work_container = container_path_for_host(case_dir);
    let container_id =
        running_git_cli_container_id().unwrap_or_else(|err| panic!("{GIT_CLI_UNAVAILABLE}: {err}"));

    let mut command = docker_exec_base();
    command.env(GIT_ASKPASS_ENV, token);
    command
        .arg("-w")
        .arg(&work_container)
        .arg("-e")
        .arg(GIT_ASKPASS_ENV)
        .arg("-e")
        .arg(format!("GIT_ASKPASS={askpass_container}"))
        .arg("-e")
        .arg("GIT_TERMINAL_PROMPT=0")
        .arg("-e")
        .arg("GIT_CONFIG_COUNT=1")
        .arg("-e")
        .arg("GIT_CONFIG_KEY_0=credential.username")
        .arg("-e")
        .arg(format!("GIT_CONFIG_VALUE_0={username}"));
    for (name, value) in command_env {
        command.env(name, value).arg("-e").arg(name);
    }
    command.arg(&container_id);
    // Enforce the wall-clock budget *inside* the container so a stalled
    // git/helper is reaped even if only the host-side docker client dies.
    // Non-interactive `docker exec` has no TTY, so BusyBox `set -m` is a no-op.
    // Inner `setsid git` gives Git its own session/process group so timeout
    // can `kill -KILL -$gpid` and reap helpers. The wrapper shell itself must
    // *not* be setsid'd — see `append_container_git_timeout_wrapper`.
    append_container_git_timeout_wrapper(&mut command);
    command.args(git_args);

    // Outer budget must exceed the in-container deadline so the container-side
    // kill wins first; otherwise we may only reap the host `docker`
    // client and leave git running against the shared workdir.
    output_with_timeout(
        command,
        GIT_CLI_COMMAND_TIMEOUT + Duration::from_secs(15),
        "docker exec git-cli",
    )
}

fn git_cli_host(
    case_dir: &Path,
    askpass_host: &Path,
    username: &str,
    token: &str,
    command_env: &[(&str, &str)],
    git_args: &[&str],
) -> Output {
    // Isolate from the developer's system/global Git config so the opt-in host
    // path does not inherit gpgSign / autocrlf / credential.helper.
    let isolated_home = case_dir.join("git-home");
    fs::create_dir_all(&isolated_home).expect("create isolated git HOME");
    let null_config = PathBuf::from("/dev/null");

    let mut command = Command::new("git");
    command
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env("HOME", &isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home.join("xdg-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env(GIT_ASKPASS_ENV, token)
        .env("GIT_ASKPASS", askpass_host)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "credential.username")
        .env("GIT_CONFIG_VALUE_0", username)
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_2", "false");
    for (name, value) in command_env {
        command.env(name, value);
    }
    command.args(git_args);
    output_with_timeout(command, GIT_CLI_COMMAND_TIMEOUT, "host git")
}

fn output_with_timeout(command: Command, timeout: Duration, label: &str) -> Output {
    try_output_with_timeout(command, timeout, label).unwrap_or_else(|err| {
        panic!("{GIT_CLI_UNAVAILABLE}: {err}");
    })
}

fn put_in_own_process_group(command: &mut Command) {
    // Linux-only harness: new process group so timeout can SIGKILL helpers
    // (e.g. git-remote-http) that would otherwise keep stdout pipes open.
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs in the child between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn kill_process_tree(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    // SAFETY: kill the child's process group; negative pid is POSIX killpg.
    unsafe {
        let _ = libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn join_reader_bounded(handle: thread::JoinHandle<Vec<u8>>, bound: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(handle.join().unwrap_or_default());
    });
    rx.recv_timeout(bound).unwrap_or_default()
}

fn try_output_with_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    put_in_own_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn {label}: {err}"))?;
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} stdout missing"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} stderr missing"))?;
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_process_tree(&mut child);
                // Bound the pipe-reader joins so a lingering helper cannot hang
                // the test after the wall-clock budget has already expired.
                let _ = join_reader_bounded(stdout_handle, Duration::from_secs(2));
                let _ = join_reader_bounded(stderr_handle, Duration::from_secs(2));
                return Err(format!("{label} exceeded timeout {timeout:?} (killed)"));
            }
            Ok(None) => sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("poll {label}: {err}")),
        }
    };

    Ok(Output {
        status,
        // Bound joins on the success path too: a helper that still holds an
        // inherited pipe must not wedge cargo test after the child has exited.
        stdout: join_reader_bounded(stdout_handle, Duration::from_secs(5)),
        stderr: join_reader_bounded(stderr_handle, Duration::from_secs(5)),
    })
}

pub fn assert_git_success(output: &Output, context: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{context} failed ({})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}

/// Seed a one-shot row into `access_token` for Basic Auth (password = token).
pub fn seed_access_token(db_url: &str, username: &str, token: &str) {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_micros() as i64;
    let username = username.replace('\'', "''");
    let token = token.replace('\'', "''");
    let sql = format!(
        "INSERT INTO access_token (id, username, token, created_at) \
         VALUES ({id}, '{username}', '{token}', now())"
    );

    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect integration DB for token seed: {err}"));
        // The caller boots the service first, but "the port answers" does not
        // prove *this* case's migrations finished — under a parallel
        // `cargo test --all` the reserved port can briefly be answered by a
        // neighbouring case's service. Wait (bounded) for the table instead of
        // failing the case on that race.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match db
                .execute_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    sql.clone(),
                ))
                .await
            {
                Ok(_) => break,
                Err(err) => {
                    let missing_table = err.to_string().contains("access_token")
                        && err.to_string().contains("does not exist");
                    if !missing_table || Instant::now() >= deadline {
                        panic!("seed access_token: {err}");
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

/// Default username seeded into `ssh_keys` for cargo-native SSH ITs.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub const DEFAULT_SSH_AUTH_USER: &str = "it-git-ssh";

/// Generate `CASE/ssh/client_ed25519` (+ `.pub`) with mode `0600` (ADR-GM-05).
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn generate_client_ed25519(private_key_path: &Path) {
    if let Some(parent) = private_key_path.parent() {
        fs::create_dir_all(parent).expect("create client key parent");
    }
    if private_key_path.exists() {
        let _ = fs::remove_file(private_key_path);
    }
    let pub_path = PathBuf::from(format!("{}.pub", private_key_path.display()));
    if pub_path.exists() {
        let _ = fs::remove_file(&pub_path);
    }
    let output = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            private_key_path
                .to_str()
                .expect("client key path must be utf-8"),
        ])
        .output()
        .unwrap_or_else(|err| panic!("ssh-keygen failed to start: {err}"));
    assert!(
        output.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(private_key_path)
        .expect("client key metadata")
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(private_key_path, perms).expect("chmod client key 0600");
    let mode = fs::metadata(private_key_path)
        .expect("client key metadata after chmod")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "client_key_mode=0600");
}

/// Second column of `ssh-keygen -lf <pub> -E sha256` (`SHA256:…`).
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn ssh_fingerprint_sha256_col2(public_key_path: &Path) -> String {
    let output = Command::new("ssh-keygen")
        .args([
            "-lf",
            public_key_path
                .to_str()
                .expect("public key path must be utf-8"),
            "-E",
            "sha256",
        ])
        .output()
        .unwrap_or_else(|err| panic!("ssh-keygen -lf failed to start: {err}"));
    assert!(
        output.status.success(),
        "ssh-keygen -lf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = String::from_utf8_lossy(&output.stdout);
    let finger = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("missing ssh-keygen fingerprint column 2 in: {line}"))
        .to_string();
    assert!(
        finger.starts_with("SHA256:"),
        "finger=ssh-keygen_-lf_sha256_col2 got {finger}"
    );
    finger
}

/// Seed `ssh_keys` with the public key + fingerprint used by the SSH server.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn seed_ssh_key(db_url: &str, username: &str, title: &str, public_key: &str, finger: &str) {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_micros() as i64;
    let username_sql = username.replace('\'', "''");
    let title_sql = title.replace('\'', "''");
    let key_sql = public_key.replace('\'', "''");
    let finger_sql = finger.replace('\'', "''");
    let sql = format!(
        "INSERT INTO ssh_keys (id, username, title, ssh_key, finger, created_at) \
         VALUES ({id}, '{username_sql}', '{title_sql}', '{key_sql}', '{finger_sql}', now())"
    );
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect DB for ssh_keys seed: {err}"));
        db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
            .await
            .unwrap_or_else(|err| panic!("seed ssh_keys: {err}"));
    });
}

/// Assert the seeded `ssh_keys` row matches the on-disk keypair fingerprint.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn assert_ssh_keys_row_matches_keypair(db_url: &str, username: &str, finger: &str) {
    let username_sql = username.replace('\'', "''");
    let finger_sql = finger.replace('\'', "''");
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect DB to assert ssh_keys: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT username, finger FROM ssh_keys \
                     WHERE username = '{username_sql}' AND finger = '{finger_sql}'"
                ),
            ))
            .await
            .unwrap_or_else(|err| panic!("query ssh_keys: {err}"));
        assert_eq!(
            rows.len(),
            1,
            "ssh_keys_row_matches_keypair: expected one row for {username}/{finger}"
        );
    });
}

/// Write `known_hosts` for the case SSH port via `ssh-keyscan` on the selected runner.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn write_known_hosts_via_keyscan(known_hosts: &Path, port: u16) {
    require_git_cli_runner();
    ensure_git_cli_passwd_for_runtime_uid();
    if let Some(parent) = known_hosts.parent() {
        fs::create_dir_all(parent).expect("create known_hosts parent");
    }
    let port_arg = port.to_string();
    let scan_host = monoengine_reachable_host();
    let output = match runner_kind() {
        GitRunnerKind::Container => {
            let known_container = container_path_for_host(known_hosts);
            let container_id = running_git_cli_container_id()
                .unwrap_or_else(|err| panic!("{GIT_CLI_UNAVAILABLE}: {err}"));
            let script =
                format!("ssh-keyscan -p {port_arg} {scan_host} > '{known_container}'");
            let mut command = docker_exec_base();
            command.arg(&container_id).args(["sh", "-c", &script]);
            output_with_timeout(command, Duration::from_secs(30), "docker exec ssh-keyscan")
        }
        GitRunnerKind::Host => {
            let mut command = Command::new("ssh-keyscan");
            command.args(["-p", &port_arg, scan_host]);
            let output = output_with_timeout(command, Duration::from_secs(30), "host ssh-keyscan");
            fs::write(known_hosts, &output.stdout).expect("write known_hosts");
            output
        }
    };
    assert!(
        output.status.success(),
        "ssh-keyscan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = fs::read_to_string(known_hosts).expect("read known_hosts");
    assert!(
        !body.trim().is_empty(),
        "ssh-keyscan produced empty known_hosts for port {port}"
    );
    assert!(
        body.contains(scan_host),
        "known_hosts=case_port_only missing {scan_host}"
    );
}

/// Ensure the compose git-cli runtime UID has a passwd entry so OpenSSH works.
///
/// CI sets `MONOENGINE_IT_GIT_UID` to the host UID (often 1001); a build-time
/// `adduser -u 1000` alone is not enough.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn ensure_git_cli_passwd_for_runtime_uid() {
    if runner_kind() != GitRunnerKind::Container {
        return;
    }
    let container_id =
        running_git_cli_container_id().unwrap_or_else(|err| panic!("{GIT_CLI_UNAVAILABLE}: {err}"));

    let mut id_cmd = docker_exec_base();
    id_cmd
        .arg(&container_id)
        .args(["sh", "-c", "printf '%s:%s' \"$(id -u)\" \"$(id -g)\""]);
    let id_out = output_with_timeout(id_cmd, Duration::from_secs(15), "docker exec id");
    assert!(
        id_out.status.success(),
        "failed to read git-cli runtime uid/gid: {}",
        String::from_utf8_lossy(&id_out.stderr)
    );
    let id_pair = String::from_utf8_lossy(&id_out.stdout);
    let mut parts = id_pair.trim().split(':');
    let uid = parts.next().unwrap_or_default().to_string();
    let gid = parts.next().unwrap_or_default().to_string();
    assert!(
        !uid.is_empty() && !gid.is_empty(),
        "unexpected git-cli id output: {id_pair}"
    );

    let script = format!(
        "if getent passwd {uid} >/dev/null 2>&1; then exit 0; fi; \
         if ! getent group {gid} >/dev/null 2>&1; then addgroup -g {gid} gitcliruntime || true; fi; \
         group_name=\"$(getent group {gid} | cut -d: -f1)\"; \
         adduser -D -u {uid} -G \"$group_name\" -h /home/gitcli -s /bin/sh gitcliruntime"
    );
    let mut command = docker_exec_base();
    command
        .arg("-u")
        .arg("0")
        .arg(&container_id)
        .args(["sh", "-c", &script]);
    let output = output_with_timeout(command, Duration::from_secs(30), "docker exec adduser");
    assert!(
        output.status.success(),
        "failed to ensure passwd for git-cli uid {uid}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build ADR-GM-05 `GIT_SSH_COMMAND` for the selected runner (container paths under `/work`).
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn git_ssh_command(case_dir: &Path, port: u16) -> String {
    require_git_cli_runner();
    let key = case_dir.join("ssh").join("client_ed25519");
    let known = case_dir.join("ssh").join("known_hosts");
    let (key_arg, known_arg) = match runner_kind() {
        GitRunnerKind::Container => (
            container_path_for_host(&key),
            container_path_for_host(&known),
        ),
        GitRunnerKind::Host => (
            key.to_str().expect("utf-8 key").to_string(),
            known.to_str().expect("utf-8 known_hosts").to_string(),
        ),
    };
    format!(
        "ssh -i {key_arg} -o IdentitiesOnly=yes -o UserKnownHostsFile={known_arg} -o StrictHostKeyChecking=yes -p {port}"
    )
}

/// Run `git` with `GIT_SSH_COMMAND` and no HTTP credential injection.
#[allow(
    dead_code,
    reason = "SSH auth helper; path-included into HTTP targets that do not call it yet"
)]
pub fn git_cli_ssh(case_dir: &Path, git_ssh_command: &str, git_args: &[&str]) -> Output {
    require_git_cli_runner();
    ensure_git_cli_passwd_for_runtime_uid();
    if git_cli_skip_requested() {
        panic!("{GIT_CLI_UNAVAILABLE}: skipped runner cannot execute git");
    }
    match runner_kind() {
        GitRunnerKind::Container => git_cli_container_ssh(case_dir, git_ssh_command, git_args),
        GitRunnerKind::Host => git_cli_host_ssh(case_dir, git_ssh_command, git_args),
    }
}

fn git_cli_container_ssh(case_dir: &Path, git_ssh_command: &str, git_args: &[&str]) -> Output {
    let work_container = container_path_for_host(case_dir);
    let container_id =
        running_git_cli_container_id().unwrap_or_else(|err| panic!("{GIT_CLI_UNAVAILABLE}: {err}"));
    let mut command = docker_exec_base();
    command.env("GIT_SSH_COMMAND", git_ssh_command);
    command
        .arg("-w")
        .arg(&work_container)
        .arg("-e")
        .arg("GIT_SSH_COMMAND")
        .arg("-e")
        .arg("GIT_TERMINAL_PROMPT=0")
        .arg("-e")
        .arg("GIT_CONFIG_COUNT=1")
        .arg("-e")
        .arg("GIT_CONFIG_KEY_0=core.autocrlf")
        .arg("-e")
        .arg("GIT_CONFIG_VALUE_0=false")
        .arg(&container_id);
    append_container_git_timeout_wrapper(&mut command);
    command.args(git_args);
    output_with_timeout(
        command,
        GIT_CLI_COMMAND_TIMEOUT + Duration::from_secs(15),
        "docker exec git-cli (ssh)",
    )
}

fn git_cli_host_ssh(case_dir: &Path, git_ssh_command: &str, git_args: &[&str]) -> Output {
    let isolated_home = case_dir.join("git-home-ssh");
    fs::create_dir_all(&isolated_home).expect("create isolated git HOME");
    let null_config = PathBuf::from("/dev/null");
    let mut command = Command::new("git");
    command
        .current_dir(case_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove(GIT_ASKPASS_ENV)
        .env_remove("GIT_ASKPASS")
        .env("HOME", &isolated_home)
        .env("XDG_CONFIG_HOME", isolated_home.join("xdg-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &null_config)
        .env("GIT_CONFIG_SYSTEM", &null_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", git_ssh_command)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_0", "false")
        .args(git_args);
    output_with_timeout(command, GIT_CLI_COMMAND_TIMEOUT, "host git (ssh)")
}

fn with_runtime<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
