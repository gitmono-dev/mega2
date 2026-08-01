// Git-cli black-box helpers (IT-03 / IT-10 / IT-12).
//
// Included only by `integration_git_cli.rs` via `#[path = "common/git_cli.rs"]`
// so vault and other bin test targets do not compile these symbols.

use std::{
    env, fs,
    io::Read,
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
/// Must match `docs/refactoring/test-infra.md` / compose `alpine/git:v2.49.1`.
pub const PINNED_GIT_CLI_VERSION: &str = "git version 2.49.1";
/// Per-invocation wall-clock budget so a stalled protocol cannot hang `cargo test`.
pub const GIT_CLI_COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
/// Bound the initial docker compose runner probe so `cargo test` cannot wedge
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

pub fn docker_compose_file() -> PathBuf {
    repo_root().join("docker-compose.test.yml")
}

pub fn git_cli_workdir() -> PathBuf {
    // Integration harness target OS is Linux; default matches compose
    // `${MONOENGINE_IT_GIT_WORKDIR:-/tmp/monoengine-git}`. Relative values
    // are resolved against the repo root (compose file directory), not Cargo's
    // `bin/` CWD, so host paths stay aligned with the bind mount.
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
    // an explicit Linux local opt-in (`MONOENGINE_IT_ALLOW_HOST_GIT=1`), not a
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

fn compose_command() -> Command {
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("-p")
        .arg(COMPOSE_PROJECT)
        .arg("-f")
        .arg(docker_compose_file());
    command
}

fn try_container_git_version() -> Result<String, String> {
    let mut command = compose_command();
    command.args(["exec", "-T", "git-cli", "git", "--version"]);
    // Must return Err (not panic) so opt-in host-git remains reachable when
    // docker is missing or the daemon/probe stalls.
    let output =
        try_output_with_timeout(command, DOCKER_RUNNER_PROBE_TIMEOUT, "docker runner probe")?;
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

/// Resolve the runner once per process. Prefer compose `git-cli`; host git only
/// when `MONOENGINE_IT_ALLOW_HOST_GIT=1` (Linux local experiments).
pub fn require_git_cli_runner() {
    if git_cli_skip_requested() {
        eprintln!("SKIP: MONOENGINE_IT_SKIP_GIT_CLI=1");
        return;
    }

    if RUNNER.get().is_some() {
        return;
    }

    let container_err = match try_container_git_version() {
        Ok(version) if version == PINNED_GIT_CLI_VERSION => {
            let _ = RUNNER.set(GitRunnerKind::Container);
            return;
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
                let _ = RUNNER.set(GitRunnerKind::Host);
            }
            Err(host_err) => {
                eprintln!("{GIT_CLI_UNAVAILABLE}: compose={container_err}; host={host_err}");
                panic!("{GIT_CLI_UNAVAILABLE}");
            }
        }
    } else {
        eprintln!(
            "{GIT_CLI_UNAVAILABLE}: docker compose exec failed: {container_err}\n\
             hint (Linux): start the opt-in runner with \
             `docker compose -p monoengine-it -f docker-compose.test.yml --profile git up -d --wait git-cli` \
             (see docs/refactoring/test-infra.md)"
        );
        panic!("{GIT_CLI_UNAVAILABLE}");
    }
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

fn container_path_for_host(host_path: &Path) -> String {
    let workdir = git_cli_workdir();
    let abs = host_path
        .canonicalize()
        .unwrap_or_else(|_| host_path.to_path_buf());
    let work_abs = workdir.canonicalize().unwrap_or_else(|_| workdir.clone());
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
    require_git_cli_runner();
    if git_cli_skip_requested() {
        panic!("{GIT_CLI_UNAVAILABLE}: skipped runner cannot execute git");
    }

    let askpass_host = case_dir.join("git-askpass.sh");
    if !askpass_host.exists() {
        write_git_askpass(&askpass_host);
    }

    match runner_kind() {
        GitRunnerKind::Container => git_cli_container(case_dir, &askpass_host, token, git_args),
        GitRunnerKind::Host => git_cli_host(case_dir, &askpass_host, token, git_args),
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

    let mut command = compose_command();
    command
        .args(["exec", "-T"])
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
        .arg("git-cli");
    append_container_git_timeout_wrapper(&mut command);
    command.args(git_args);

    output_with_timeout(
        command,
        GIT_CLI_COMMAND_TIMEOUT + Duration::from_secs(15),
        "compose git-cli (no-auth)",
    )
}

/// In-container wall-clock wrapper around `git`.
///
/// Important: do **not** wrap the script itself in `setsid`. BusyBox `setsid`
/// starts a new session, and `docker compose exec` then reports exit status 0
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
    token: &str,
    git_args: &[&str],
) -> Output {
    let askpass_container = container_path_for_host(askpass_host);
    let work_container = container_path_for_host(case_dir);

    let mut command = compose_command();
    command.env(GIT_ASKPASS_ENV, token);
    command
        .args(["exec", "-T"])
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
        .arg(format!("GIT_CONFIG_VALUE_0={DEFAULT_GIT_AUTH_USER}"))
        .arg("git-cli");
    // Enforce the wall-clock budget *inside* the container so a stalled
    // git/helper is reaped even if only the host-side docker client dies.
    // `docker compose exec -T` has no TTY, so BusyBox `set -m` is a no-op.
    // Inner `setsid git` gives Git its own session/process group so timeout
    // can `kill -KILL -$gpid` and reap helpers. The wrapper shell itself must
    // *not* be setsid'd — see `append_container_git_timeout_wrapper`.
    append_container_git_timeout_wrapper(&mut command);
    command.args(git_args);

    // Outer budget must exceed the in-container deadline so the container-side
    // kill wins first; otherwise we may only reap the host `docker compose`
    // client and leave git running against the shared workdir.
    output_with_timeout(
        command,
        GIT_CLI_COMMAND_TIMEOUT + Duration::from_secs(15),
        "compose git-cli",
    )
}

fn git_cli_host(case_dir: &Path, askpass_host: &Path, token: &str, git_args: &[&str]) -> Output {
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
        .env("GIT_CONFIG_VALUE_0", DEFAULT_GIT_AUTH_USER)
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_2", "false")
        .args(git_args);
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
        db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
            .await
            .unwrap_or_else(|err| panic!("seed access_token: {err}"));
    });
}

fn with_runtime<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
