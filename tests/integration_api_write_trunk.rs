//! Trunk product API write IT (plan-20260904 / AW-03).
//!
//! Boots real `service http` under `push_policy=trunk` + `push_auth=token`, then:
//! - unauthenticated create-entry → 401
//! - token create-entry advances `/project` tip, no mega_cl / refs/cl
//! - token edit/save advances tip again
//!
//! plan-20260917 LB-02 adds one `delete_entry_*` case per acceptance gate
//! (`EX-LB-01`), LB-03 one `move_entry_*` case per gate (`EX-LB-02`) and
//! LB-04 the `tag_*` cases for the storage-only tag routes, each booting its
//! own service.

mod common;
#[allow(dead_code)]
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
use serde_json::Value;
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str = "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

const PUSH_TOKEN: &str = "aw03-api-write-token";
const TOKEN_NAME: &str = "aw03-ci";

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
            "mega2_aw03_{}_{}",
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

struct ApiWriteEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
    /// `MEGA_MONOREPO__PUSH_POLICY` handed to the service.
    push_policy: &'static str,
    /// Extra `MEGA_*` overrides for the service (e.g. custom `root_dirs`).
    extra_env: Vec<(&'static str, String)>,
}

impl ApiWriteEnv {
    /// Trunk + `push_auth=token` with one token scoped to `/project`.
    fn with_token_config() -> Self {
        Self::with_token_config_paths(Some(&["/project"]))
    }

    /// Trunk + `push_auth=token`; `paths = None` omits the key (whole repo).
    fn with_token_config_paths(paths: Option<&[&str]>) -> Self {
        let paths_line = match paths {
            Some(paths) => {
                let quoted: Vec<String> = paths.iter().map(|p| format!("{p:?}")).collect();
                format!("paths = [{}]\n", quoted.join(", "))
            }
            None => String::new(),
        };
        let append = format!(
            r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "{TOKEN_NAME}"
token = "{PUSH_TOKEN}"
{paths_line}"#
        );
        Self::with_git_append("trunk", &append)
    }

    /// Trunk + `push_auth=none`: unauthenticated writes are admitted.
    fn with_auth_none_config() -> Self {
        Self::with_git_append(
            "trunk",
            r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false
"#,
        )
    }

    /// Review morphology: the repo default config, no `push_auth`.
    fn with_review_config() -> Self {
        Self::with_git_append("review", "")
    }

    fn with_git_append(push_policy: &'static str, append: &str) -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let case_name = format!(
            "aw03-case-{}-{}",
            std::process::id(),
            CASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let case_dir = temp_dir.path().join(case_name);
        fs::create_dir_all(&case_dir).expect("create case dir");

        let base_dir = case_dir.join("base");
        let cache_dir = case_dir.join("cache");
        let object_root = case_dir.join("objects");
        fs::create_dir_all(&base_dir).expect("create base");
        fs::create_dir_all(&cache_dir).expect("create cache");
        fs::create_dir_all(&object_root).expect("create objects");
        git_cli::write_git_askpass(&case_dir.join("git-askpass.sh"));

        let database = TestDatabase::create();
        let full_config_path = case_dir.join("config.toml");
        common::write_full_config_with_append(&full_config_path, append);

        Self {
            temp_dir,
            database,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
            case_dir,
            push_policy,
            extra_env: Vec::new(),
        }
    }

    fn with_env(mut self, key: &'static str, value: &str) -> Self {
        self.extra_env.push((key, value.to_owned()));
        self
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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            .env("MEGA_MONOREPO__PUSH_POLICY", self.push_policy)
            .env("MEGA_GIT__SSH_RECEIVE_PACK", "false");
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }
        command
    }
}

struct ServiceProcess {
    child: Child,
    reaped: bool,
}

impl ServiceProcess {
    fn spawn(mut command: Command) -> Self {
        let child = command.spawn().expect("spawn mega2 service");
        Self {
            child,
            reaped: false,
        }
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
fn integration_api_write_trunk() {
    let env = ApiWriteEnv::with_token_config();
    let (mut service, port, _stdout, stderr) = boot_service_http(&env);
    let client = http_client();
    let api = format!("http://127.0.0.1:{port}/api/v1");

    // Init only tips `/`; lander B0 needs a non-root path tip.
    seed_project_tip(&env.case_dir, port, PUSH_TOKEN);
    let tip_before = path_tip(&env.database.db_url, "/project");

    let unauth = client
        .post(format!("{api}/create-entry"))
        .json(&serde_json::json!({
            "is_directory": false,
            "name": "aw03-unauth.txt",
            "path": "/project",
            "content": "should-not-land\n",
            "skip_build": true
        }))
        .send()
        .expect("unauth create");
    assert_eq!(
        unauth.status().as_u16(),
        401,
        "token morphology: unauthenticated create-entry must 401; body={}",
        unauth.text().unwrap_or_default()
    );
    assert_eq!(
        path_tip(&env.database.db_url, "/project"),
        tip_before,
        "failed create must not advance tip"
    );

    let create = client
        .post(format!("{api}/create-entry"))
        .header("Authorization", format!("Bearer {PUSH_TOKEN}"))
        .json(&serde_json::json!({
            "is_directory": false,
            "name": "aw03-created.txt",
            "path": "/project",
            "content": "hello from api create\n",
            "skip_build": true
        }))
        .send()
        .expect("token create");
    let create_status = create.status().as_u16();
    let create_body = create.text().expect("create body");
    assert_eq!(
        create_status, 200,
        "token create-entry must 200; body={create_body}"
    );
    let create_json: Value = serde_json::from_str(&create_body).expect("create json");
    assert!(
        create_json["req_result"].as_bool().unwrap_or(false),
        "create CommonResult.req_result: {create_json}"
    );
    let create_data = &create_json["data"];
    assert!(
        create_data["cl_link"].is_null(),
        "trunk create must not return cl_link: {create_json}"
    );
    let create_commit = create_data["commit_id"]
        .as_str()
        .expect("commit_id")
        .to_owned();
    let tip_after_create = path_tip(&env.database.db_url, "/project");
    assert_eq!(
        tip_after_create, create_commit,
        "create must advance /project tip"
    );
    assert_ne!(tip_after_create, tip_before, "tip must change after create");

    let (cls, cl_refs) = count_cl_artifacts(&env.database.db_url);
    assert_eq!(cls, 0, "create must not insert mega_cl");
    assert_eq!(cl_refs, 0, "create must not insert refs/cl/*");

    let save = client
        .post(format!("{api}/edit/save"))
        .header("Authorization", format!("Bearer {PUSH_TOKEN}"))
        .json(&serde_json::json!({
            "path": "/project/aw03-created.txt",
            "content": "hello from api save\n",
            "commit_message": "aw03 edit/save",
            "skip_build": true
        }))
        .send()
        .expect("token save");
    let save_status = save.status().as_u16();
    let save_body = save.text().expect("save body");
    assert_eq!(
        save_status, 200,
        "token edit/save must 200; body={save_body}"
    );
    let save_json: Value = serde_json::from_str(&save_body).expect("save json");
    assert!(
        save_json["req_result"].as_bool().unwrap_or(false),
        "save CommonResult.req_result: {save_json}"
    );
    assert!(
        save_json["data"]["cl_link"].is_null(),
        "trunk save must not return cl_link: {save_json}"
    );
    let save_commit = save_json["data"]["commit_id"]
        .as_str()
        .expect("save commit_id")
        .to_owned();
    let tip_after_save = path_tip(&env.database.db_url, "/project");
    assert_eq!(
        tip_after_save, save_commit,
        "save must advance /project tip"
    );
    assert_ne!(
        tip_after_save, tip_after_create,
        "tip must change after save"
    );

    let (cls2, cl_refs2) = count_cl_artifacts(&env.database.db_url);
    assert_eq!(cls2, 0, "save must not insert mega_cl");
    assert_eq!(cl_refs2, 0, "save must not insert refs/cl/*");

    let requesters = push_queue_requesters(&env.database.db_url);
    assert!(
        requesters.iter().any(|r| r.as_deref() == Some(TOKEN_NAME)),
        "push_queue requester must be token name {TOKEN_NAME}: {requesters:?}"
    );

    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

/// Create a tip at `/project` so API create/save can land (B0 rejects `/`).
fn seed_project_tip(case_dir: &Path, port: u16, token: &str) {
    let project_url = format!(
        "{}/",
        git_cli::mega2_host_http_url(port, "/project").trim_end_matches('/')
    );
    host_git_ok(case_dir, token, &["clone", &project_url, "project-seed"]);
    host_git_ok(
        case_dir,
        token,
        &["-C", "project-seed", "config", "user.name", "AW03 IT"],
    );
    host_git_ok(
        case_dir,
        token,
        &[
            "-C",
            "project-seed",
            "config",
            "user.email",
            "aw03@example.com",
        ],
    );
    let seed_file = case_dir.join("project-seed").join("seed.txt");
    fs::write(&seed_file, "seed\n").expect("write seed");
    host_git_ok(case_dir, token, &["-C", "project-seed", "add", "seed.txt"]);
    host_git_ok(
        case_dir,
        token,
        &["-C", "project-seed", "commit", "-m", "seed /project tip"],
    );
    host_git_ok(
        case_dir,
        token,
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

fn host_git_ok(case_dir: &Path, token: &str, args: &[&str]) {
    let output = host_git_command(case_dir, token, args)
        .output()
        .expect("host git");
    git_cli::assert_git_success(&output, &format!("git {}", args.join(" ")));
}

fn host_git_stdout(case_dir: &Path, token: &str, args: &[&str]) -> String {
    let output = host_git_command(case_dir, token, args)
        .output()
        .expect("host git");
    git_cli::assert_git_success(&output, &format!("git {}", args.join(" ")));
    String::from_utf8(output.stdout)
        .expect("git stdout utf8")
        .trim_end()
        .to_owned()
}

fn host_git_command(case_dir: &Path, token: &str, args: &[&str]) -> Command {
    let isolated_home = case_dir.join("git-home");
    fs::create_dir_all(&isolated_home).expect("git home");
    let null_config = PathBuf::from("/dev/null");
    let askpass = case_dir.join("git-askpass.sh");
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
        .env("GIT_ASKPASS", &askpass)
        .env("GIT_CONFIG_COUNT", "6")
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
        .env("GIT_CONFIG_KEY_5", "credential.username")
        .env("GIT_CONFIG_VALUE_5", git_cli::DEFAULT_GIT_AUTH_USER)
        .args(args);
    command
}

fn boot_service_http(env: &ApiWriteEnv) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join(format!("service-{port}.out"));
    let stderr_path = env.temp_dir.path().join(format!("service-{port}.err"));

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    command.env(
        "MEGA_HTTP__PUBLIC_BASE_URL",
        format!("http://127.0.0.1:{port}"),
    );
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

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}

fn path_tip(db_url: &str, path: &str) -> String {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for tip: {err}"));
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT ref_commit_hash AS h FROM mega_refs \
                     WHERE path = '{path}' AND ref_name = 'refs/heads/main' AND NOT is_cl \
                     LIMIT 1"
                ),
            ))
            .await
            .expect("query tip")
            .expect("tip row");
        row.try_get::<String>("", "h").expect("h")
    })
}

/// Whether `refs/tags/<name>` exists in `mega_refs` (any path).
fn tag_ref_exists(db_url: &str, name: &str) -> bool {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for tag ref: {err}"));
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT 1 AS one FROM mega_refs WHERE ref_name = 'refs/tags/{name}' LIMIT 1"),
        ))
        .await
        .expect("query tag ref")
        .is_some()
    })
}

fn count_cl_artifacts(db_url: &str) -> (i64, i64) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for CL count: {err}"));
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

fn push_queue_requesters(db_url: &str) -> Vec<Option<String>> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for push_queue: {err}"));
        let rows = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT requester FROM push_queue WHERE kind::text = 'push' ORDER BY id"
                    .to_string(),
            ))
            .await
            .unwrap_or_else(|err| panic!("query push_queue: {err}"));
        rows.iter()
            .map(|row| row.try_get("", "requester").ok())
            .collect()
    })
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

// ---------------------------------------------------------------------------
// plan-20260917 LB-02: `POST /api/v1/delete-entry` — one booted service per
// gate (EX-LB-01), all against the real HTTP surface.
// ---------------------------------------------------------------------------

const LB02_AUTHOR: &str = "lb02-author";

/// A booted service plus the HTTP client and URLs one directory-change case
/// (delete-entry / move-entry) needs.
struct EntryCase {
    env: ApiWriteEnv,
    service: ServiceProcess,
    port: u16,
    api: String,
    client: reqwest::blocking::Client,
    stderr: PathBuf,
}

impl EntryCase {
    fn boot(env: ApiWriteEnv) -> Self {
        let (service, port, _stdout, stderr) = boot_service_http(&env);
        Self {
            env,
            service,
            port,
            api: format!("http://127.0.0.1:{port}/api/v1"),
            client: http_client(),
            stderr,
        }
    }

    /// Give `/project` a non-root tip so trunk writes can land (B0).
    fn seed(&self) {
        seed_project_tip(&self.env.case_dir, self.port, PUSH_TOKEN);
    }

    fn bearer() -> String {
        format!("Bearer {PUSH_TOKEN}")
    }

    fn post(&self, route: &str, auth: Option<&str>, body: Value) -> (u16, Value) {
        let mut request = self
            .client
            .post(format!("{}/{route}", self.api))
            .json(&body);
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        let response = request
            .send()
            .unwrap_or_else(|err| panic!("POST {route}: {err}"));
        let status = response.status().as_u16();
        let text = response.text().expect("response body");
        let json = serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
        (status, json)
    }

    fn exchange(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
        auth: Option<&str>,
        what: &str,
    ) -> (u16, Value) {
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        let response = request.send().unwrap_or_else(|err| panic!("{what}: {err}"));
        let status = response.status().as_u16();
        let text = response.text().expect("response body");
        let json = serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
        (status, json)
    }

    fn get(&self, route: &str, auth: Option<&str>) -> (u16, Value) {
        self.exchange(
            self.client.get(format!("{}/{route}", self.api)),
            auth,
            &format!("GET {route}"),
        )
    }

    fn delete(&self, route: &str, auth: Option<&str>) -> (u16, Value) {
        self.exchange(
            self.client.delete(format!("{}/{route}", self.api)),
            auth,
            &format!("DELETE {route}"),
        )
    }

    /// create-entry under `/project` (directory or file) as `LB02_AUTHOR`.
    fn create_entry(&self, auth: Option<&str>, name: &str, is_directory: bool) -> Value {
        let mut body = serde_json::json!({
            "is_directory": is_directory,
            "name": name,
            "path": "/project",
            "author_username": LB02_AUTHOR,
            "skip_build": true
        });
        if !is_directory {
            body["content"] = Value::String("lb02\n".to_string());
        }
        let (status, json) = self.post("create-entry", auth, body);
        assert_eq!(status, 200, "create-entry {name} must 200: {json}");
        json
    }

    /// create-entry of a directory under an explicit parent (missing parents
    /// are built by create-entry itself).
    fn create_dir_at(&self, auth: Option<&str>, path: &str, name: &str) -> Value {
        let (status, json) = self.post(
            "create-entry",
            auth,
            serde_json::json!({
                "is_directory": true,
                "name": name,
                "path": path,
                "author_username": LB02_AUTHOR,
                "skip_build": true
            }),
        );
        assert_eq!(status, 200, "create-entry {path}/{name} must 200: {json}");
        json
    }

    fn move_entry(
        &self,
        auth: Option<&str>,
        from_path: &str,
        from_name: &str,
        to_path: &str,
        to_name: &str,
    ) -> (u16, Value) {
        self.move_entry_as(auth, from_path, from_name, to_path, to_name, true)
    }

    fn move_entry_as(
        &self,
        auth: Option<&str>,
        from_path: &str,
        from_name: &str,
        to_path: &str,
        to_name: &str,
        is_directory: bool,
    ) -> (u16, Value) {
        self.post(
            "move-entry",
            auth,
            serde_json::json!({
                "from_path": from_path,
                "from_name": from_name,
                "to_path": to_path,
                "to_name": to_name,
                "is_directory": is_directory,
                "author_username": LB02_AUTHOR,
                "skip_build": true
            }),
        )
    }

    /// `(name, oid)` pairs of `GET /tree/content-hash?path=` (blob oid for files).
    fn tree_oids(&self, path: &str) -> Vec<(String, String)> {
        let response = self
            .client
            .get(format!("{}/tree/content-hash?path={path}", self.api))
            .send()
            .expect("GET /tree/content-hash");
        assert_eq!(
            response.status().as_u16(),
            200,
            "GET /tree/content-hash?path={path}"
        );
        let json: Value = response.json().expect("content-hash json");
        json["data"]
            .as_array()
            .expect("content-hash data")
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().expect("hash item name").to_owned(),
                    item["oid"].as_str().expect("hash item oid").to_owned(),
                )
            })
            .collect()
    }

    /// `(name, content_type)` pairs of `GET /tree?path=`.
    fn tree_entries(&self, path: &str) -> Vec<(String, String)> {
        let response = self
            .client
            .get(format!("{}/tree?path={path}", self.api))
            .send()
            .expect("GET /tree");
        assert_eq!(response.status().as_u16(), 200, "GET /tree?path={path}");
        let json: Value = response.json().expect("tree json");
        json["data"]["tree_items"]
            .as_array()
            .expect("tree_items")
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().expect("tree item name").to_owned(),
                    item["content_type"]
                        .as_str()
                        .expect("content_type")
                        .to_owned(),
                )
            })
            .collect()
    }

    fn delete_entry(&self, auth: Option<&str>, path: &str, name: &str) -> (u16, Value) {
        self.delete_entry_as(auth, path, name, true)
    }

    fn delete_entry_as(
        &self,
        auth: Option<&str>,
        path: &str,
        name: &str,
        is_directory: bool,
    ) -> (u16, Value) {
        self.post(
            "delete-entry",
            auth,
            serde_json::json!({
                "path": path,
                "name": name,
                "is_directory": is_directory,
                "author_username": LB02_AUTHOR,
                "skip_build": true
            }),
        )
    }

    /// `tree_items[].name` of `GET /tree?path=`.
    fn tree_names(&self, path: &str) -> Vec<String> {
        let response = self
            .client
            .get(format!("{}/tree?path={path}", self.api))
            .send()
            .expect("GET /tree");
        assert_eq!(response.status().as_u16(), 200, "GET /tree?path={path}");
        let json: Value = response.json().expect("tree json");
        json["data"]["tree_items"]
            .as_array()
            .expect("tree_items")
            .iter()
            .map(|item| item["name"].as_str().expect("tree item name").to_owned())
            .collect()
    }

    fn db_url(&self) -> &str {
        &self.env.database.db_url
    }

    fn finish(mut self) {
        assert!(
            self.service
                .shutdown_via_sigint(Duration::from_secs(60))
                .success(),
            "shutdown failed\n{}",
            read_log(&self.stderr)
        );
    }
}

/// Create `/project/lb02-gone` through the API, delete it with the token, and
/// return the delete response (asserted 200) plus the tip before the delete.
fn delete_success_flow(case: &EntryCase) -> (Value, String) {
    case.create_entry(Some(&EntryCase::bearer()), "lb02-gone", true);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/project", "lb02-gone");
    assert_eq!(status, 200, "token delete-entry must 200: {json}");
    (json, tip_before)
}

/// Push a one-commit repository to `/third-party/<repo>` so the path resolves
/// to an ImportRepo (receive-pack creates the `git_repo` row).
fn seed_import_repo(case_dir: &Path, port: u16, token: &str, repo: &str) {
    let dir = format!("import-seed-{repo}");
    host_git_ok(case_dir, token, &["init", "-b", "main", &dir]);
    host_git_ok(
        case_dir,
        token,
        &["-C", &dir, "config", "user.name", "LB02 IT"],
    );
    host_git_ok(
        case_dir,
        token,
        &["-C", &dir, "config", "user.email", "lb02@example.com"],
    );
    let src = case_dir.join(&dir).join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(src.join("lib.rs"), "// lb02 import seed\n").expect("write lib.rs");
    host_git_ok(case_dir, token, &["-C", &dir, "add", "."]);
    host_git_ok(
        case_dir,
        token,
        &["-C", &dir, "commit", "-m", "lb02 import seed"],
    );
    let url = format!(
        "{}/",
        git_cli::mega2_host_http_url(port, &format!("/third-party/{repo}")).trim_end_matches('/')
    );
    host_git_ok(
        case_dir,
        token,
        &[
            "-C",
            &dir,
            "push",
            "--no-thin",
            &url,
            "HEAD:refs/heads/main",
        ],
    );
}

fn err_message(json: &Value) -> String {
    json["err_message"].as_str().unwrap_or_default().to_owned()
}

#[test]
fn delete_entry_unauth_401() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = case.delete_entry(None, "/project", "anything");
    assert_eq!(status, 401, "unauthenticated delete-entry must 401: {json}");
    assert_eq!(json["req_result"], Value::Bool(false), "{json}");
    assert!(
        !json.to_string().contains(PUSH_TOKEN),
        "error body must not echo the token: {json}"
    );
    // Runtime OpenAPI evidence (LB-02 VER): the booted storage-only surface
    // lists delete-entry and never leaks the token.
    let openapi = case
        .client
        .get(format!("http://127.0.0.1:{}/api/openapi.json", case.port))
        .send()
        .expect("GET /api/openapi.json");
    assert_eq!(openapi.status().as_u16(), 200);
    let openapi = openapi.text().expect("openapi body");
    assert!(
        openapi.contains("delete-entry"),
        "runtime OpenAPI must list delete-entry"
    );
    assert!(
        openapi.contains("move-entry"),
        "runtime OpenAPI must list move-entry (LB-03)"
    );
    let doc: Value = serde_json::from_str(&openapi).expect("openapi json");
    for needle in ["/api/v1/tags", "/api/v1/tags/list", "/api/v1/tags/{name}"] {
        assert!(
            doc["paths"].get(needle).is_some(),
            "runtime OpenAPI must list {needle} (LB-04)"
        );
    }
    assert!(
        !openapi.contains(PUSH_TOKEN),
        "runtime OpenAPI must not contain the token"
    );
    case.finish();
}

#[test]
fn delete_entry_path_token_403() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/other", "anything");
    assert_eq!(
        status, 403,
        "token scoped to /project must 403 on /other: {json}"
    );
    assert!(
        !json.to_string().contains(PUSH_TOKEN),
        "error body must not echo the token: {json}"
    );
    case.finish();
}

#[test]
fn delete_entry_auth_none_ok() {
    let case = EntryCase::boot(ApiWriteEnv::with_auth_none_config());
    case.seed();
    case.create_entry(None, "lb02-none-dir", true);
    let (status, json) = case.delete_entry(None, "/project", "lb02-none-dir");
    assert_eq!(
        status, 200,
        "push_auth=none must admit an unauthenticated delete: {json}"
    );
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert!(json["data"]["cl_link"].is_null(), "{json}");
    case.finish();
}

#[test]
fn delete_entry_reject_file_400() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "lb02-file.txt", false);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/project", "lb02-file.txt");
    assert_eq!(status, 400, "deleting a file must 400: {json}");
    assert!(
        err_message(&json).contains("not a directory"),
        "diagnosable message expected: {json}"
    );
    // A parent path that goes through a file is the same client error.
    let (status, json) =
        case.delete_entry(Some(&EntryCase::bearer()), "/project/lb02-file.txt", "x");
    assert_eq!(status, 400, "a file as parent path must 400: {json}");
    assert!(
        err_message(&json).contains("not a directory"),
        "diagnosable message expected: {json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected delete must not advance tip"
    );
    case.finish();
}

#[test]
fn delete_entry_file_success() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "ft02-file.txt", false);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.delete_entry_as(
        Some(&EntryCase::bearer()),
        "/project",
        "ft02-file.txt",
        false,
    );
    assert_eq!(status, 200, "deleting a file must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert!(json["data"]["cl_link"].is_null(), "{json}");
    assert_eq!(
        json["data"]["path"],
        Value::String("/project/ft02-file.txt".to_string()),
        "{json}"
    );
    assert!(
        json["data"].get("new_oid").is_none(),
        "delete-entry has no new_oid: {json}"
    );
    assert_ne!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "file delete must advance /project tip"
    );
    let after = case.tree_names("/project");
    assert!(
        after.iter().all(|n| n != "ft02-file.txt"),
        "deleted file must vanish: {after:?}"
    );
    case.finish();
}

#[test]
fn delete_entry_reject_directory_as_file_400() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "ft02-dir", true);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) =
        case.delete_entry_as(Some(&EntryCase::bearer()), "/project", "ft02-dir", false);
    assert_eq!(status, 400, "directory as file must 400: {json}");
    assert!(
        err_message(&json).contains("is not a file"),
        "diagnosable message expected: {json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected file-mode delete must not advance tip"
    );
    case.finish();
}

#[test]
fn delete_entry_reject_root_400() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    for (path, name) in [("/", ""), ("", "")] {
        let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), path, name);
        assert_eq!(
            status, 400,
            "deleting the root ({path:?}, {name:?}) must 400: {json}"
        );
        assert_eq!(json["req_result"], Value::Bool(false), "{json}");
    }
    case.finish();
}

#[test]
fn delete_entry_reject_missing_404() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/project", "lb02-missing");
    assert_eq!(status, 404, "missing entry must 404: {json}");
    assert!(err_message(&json).contains("not found"), "{json}");
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/project/lb02-nope", "x");
    assert_eq!(status, 404, "missing parent must 404: {json}");
    assert!(err_message(&json).contains("not found"), "{json}");
    case.finish();
}

#[test]
fn delete_entry_reject_traversal_400() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let tip_before = path_tip(case.db_url(), "/project");
    for (path, name) in [
        ("/project", ".."),
        ("/project/../project", "x"),
        ("/project", "a/b"),
        ("/project//", "x"),
    ] {
        let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), path, name);
        assert_eq!(
            status, 400,
            "traversal ({path:?}, {name:?}) must 400: {json}"
        );
    }
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected delete must not advance tip"
    );
    case.finish();
}

#[test]
fn delete_entry_reject_import_repo_409() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(Some(&[
        "/project",
        "/third-party",
    ])));
    let repo = format!("lb02-import-{}", std::process::id());
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, &repo);
    let (status, json) = case.delete_entry(
        Some(&EntryCase::bearer()),
        &format!("/third-party/{repo}"),
        "src",
    );
    assert_eq!(status, 409, "ImportRepo target must 409: {json}");
    assert!(err_message(&json).contains("import dir"), "{json}");
    assert!(
        !err_message(&json).contains("[code:"),
        "prefix must not reach the wire: {json}"
    );
    case.finish();
}

#[test]
fn delete_entry_success_req_result_true() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let (json, _) = delete_success_flow(&case);
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert_eq!(json["err_message"], Value::String(String::new()), "{json}");
    case.finish();
}

#[test]
fn delete_entry_success_commit_id() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let (json, tip_before) = delete_success_flow(&case);
    let commit_id = json["data"]["commit_id"]
        .as_str()
        .expect("commit_id")
        .to_owned();
    assert!(
        !commit_id.is_empty() && commit_id.chars().all(|c| c.is_ascii_hexdigit()),
        "{json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        commit_id,
        "delete must advance /project tip"
    );
    assert_ne!(commit_id, tip_before, "tip must change after delete");
    assert_eq!(
        json["data"]["path"],
        Value::String("/project/lb02-gone".to_string()),
        "{json}"
    );
    assert!(
        json["data"].get("new_oid").is_none(),
        "delete-entry has no new_oid: {json}"
    );
    case.finish();
}

#[test]
fn delete_entry_success_cl_link_null() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let (json, _) = delete_success_flow(&case);
    assert!(
        json["data"]["cl_link"].is_null(),
        "trunk delete must not return cl_link: {json}"
    );
    case.finish();
}

#[test]
fn delete_entry_success_no_mega_cl() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let _ = delete_success_flow(&case);
    let (cls, cl_refs) = count_cl_artifacts(case.db_url());
    assert_eq!(cls, 0, "trunk delete must not insert mega_cl");
    assert_eq!(cl_refs, 0, "trunk delete must not insert refs/cl/*");
    let requesters = push_queue_requesters(case.db_url());
    assert!(
        requesters.iter().any(|r| r.as_deref() == Some(TOKEN_NAME)),
        "push_queue requester must be the token name {TOKEN_NAME}: {requesters:?}"
    );
    case.finish();
}

#[test]
fn delete_entry_review_uses_existing_cl_branch() {
    // Review morphology: create-entry opens a CL for `/` (the buck root the
    // subtree resolves to); a delete by the same author reuses that open CL
    // (`EditCLMode::TryReuse(None)`), exactly like create-entry does.
    let case = EntryCase::boot(ApiWriteEnv::with_review_config());
    let created = case.create_entry(None, "lb02-review-dir", true);
    let create_link = created["data"]["cl_link"]
        .as_str()
        .expect("review create returns cl_link")
        .to_owned();
    // `doc` is one of the init root directories, so it exists on main.
    let (status, json) = case.delete_entry(None, "/", "doc");
    assert_eq!(status, 200, "review delete-entry must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert_eq!(
        json["data"]["cl_link"].as_str(),
        Some(create_link.as_str()),
        "review delete must reuse the open CL: {json}"
    );
    let (cls, _) = count_cl_artifacts(case.db_url());
    assert_eq!(cls, 1, "create + delete must share one mega_cl row");
    case.finish();
}

#[test]
fn delete_entry_parent_tree_omits_name() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "lb02-gone", true);
    let before = case.tree_names("/project");
    assert!(before.iter().any(|n| n == "lb02-gone"), "{before:?}");
    let (status, json) = case.delete_entry(Some(&EntryCase::bearer()), "/project", "lb02-gone");
    assert_eq!(status, 200, "{json}");
    let after = case.tree_names("/project");
    assert!(
        after.iter().all(|n| n != "lb02-gone"),
        "deleted name must vanish: {after:?}"
    );
    assert!(
        after.iter().any(|n| n == "seed.txt"),
        "siblings must survive: {after:?}"
    );
    case.finish();
}

/// Not a plan gate: the emptied-parent rule the contract page documents — a
/// parent left without entries keeps a timestamped `.gitkeep` and stays a
/// valid (empty) directory.
#[test]
fn delete_entry_emptied_parent_keeps_gitkeep() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    // create-entry builds the missing parent: /project/lb02-parent/child.
    let (status, json) = case.post(
        "create-entry",
        Some(&EntryCase::bearer()),
        serde_json::json!({
            "is_directory": true,
            "name": "child",
            "path": "/project/lb02-parent",
            "author_username": LB02_AUTHOR,
            "skip_build": true
        }),
    );
    assert_eq!(status, 200, "nested create-entry must 200: {json}");
    assert_eq!(
        case.tree_names("/project/lb02-parent"),
        vec!["child".to_string()]
    );
    let (status, json) =
        case.delete_entry(Some(&EntryCase::bearer()), "/project/lb02-parent", "child");
    assert_eq!(status, 200, "deleting the only child must 200: {json}");
    assert_eq!(
        case.tree_names("/project/lb02-parent"),
        vec![".gitkeep".to_string()],
        "an emptied parent keeps a .gitkeep placeholder"
    );
    let names = case.tree_names("/project");
    assert!(
        names.iter().any(|n| n == "lb02-parent"),
        "parent must survive: {names:?}"
    );
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260917 LB-03: `POST /api/v1/move-entry` — one booted service per
// gate (EX-LB-02).
// ---------------------------------------------------------------------------

#[test]
fn move_entry_dual_path_auth() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-auth");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(None, "/project", "lb03-auth", "/project", "lb03-auth2");
    assert_eq!(status, 401, "unauthenticated move-entry must 401: {json}");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-auth",
        "/other",
        "lb03-auth",
    );
    assert_eq!(
        status, 403,
        "destination parent outside the token scope must 403: {json}"
    );
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/other",
        "x",
        "/project",
        "lb03-auth3",
    );
    assert_eq!(
        status, 403,
        "source parent outside the token scope must 403: {json}"
    );
    assert!(
        !json.to_string().contains(PUSH_TOKEN),
        "error body must not echo the token: {json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected moves must not advance tip"
    );
    let names = case.tree_names("/project");
    assert!(
        names.iter().any(|n| n == "lb03-auth"),
        "source must still exist: {names:?}"
    );
    case.finish();
}

#[test]
fn move_entry_reject_same_source_dest() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-same");
    let tip_before = path_tip(case.db_url(), "/project");
    for from_path in ["/project", "/project/"] {
        let (status, json) = case.move_entry(
            Some(&EntryCase::bearer()),
            from_path,
            "lb03-same",
            "/project",
            "lb03-same",
        );
        assert_eq!(
            status, 400,
            "same source and destination ({from_path:?}) must 400: {json}"
        );
        assert!(err_message(&json).contains("the same"), "{json}");
    }
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_reject_dest_exists() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-a");
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-b");
    case.create_dir_at(Some(&EntryCase::bearer()), "/project/lb03-dest", "lb03-a");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-a",
        "/project",
        "lb03-b",
    );
    assert_eq!(
        status, 400,
        "rename onto an existing sibling must 400: {json}"
    );
    assert!(err_message(&json).contains("already exists"), "{json}");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-a",
        "/project/lb03-dest",
        "lb03-a",
    );
    assert_eq!(
        status, 400,
        "move onto an existing destination name must 400: {json}"
    );
    assert!(err_message(&json).contains("already exists"), "{json}");
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_reject_into_own_subtree() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project/lb03-own", "inner");
    let tip_before = path_tip(case.db_url(), "/project");
    for to_path in ["/project/lb03-own/inner", "/project/lb03-own"] {
        let (status, json) = case.move_entry(
            Some(&EntryCase::bearer()),
            "/project",
            "lb03-own",
            to_path,
            "x",
        );
        assert_eq!(
            status, 400,
            "moving into its own subtree ({to_path}) must 400: {json}"
        );
        assert!(err_message(&json).contains("own subtree"), "{json}");
    }
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_reject_file_source() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "lb03-f.txt", false);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-f.txt",
        "/project",
        "lb03-g",
    );
    assert_eq!(status, 400, "a file source must 400: {json}");
    assert!(err_message(&json).contains("not a directory"), "{json}");
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_file_success_preserves_oid() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let created = case.create_entry(Some(&EntryCase::bearer()), "ft03-file.txt", false);
    let oid = created["data"]["new_oid"]
        .as_str()
        .expect("create-entry new_oid")
        .to_owned();
    assert!(!oid.is_empty(), "{created}");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry_as(
        Some(&EntryCase::bearer()),
        "/project",
        "ft03-file.txt",
        "/project",
        "ft03-moved.txt",
        false,
    );
    assert_eq!(status, 200, "moving a file must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert!(json["data"]["cl_link"].is_null(), "{json}");
    assert_eq!(
        json["data"]["from_path"],
        Value::String("/project/ft03-file.txt".to_string()),
        "{json}"
    );
    assert_eq!(
        json["data"]["to_path"],
        Value::String("/project/ft03-moved.txt".to_string()),
        "{json}"
    );
    assert!(
        json["data"].get("new_oid").is_none(),
        "move-entry has no new_oid: {json}"
    );
    assert_ne!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "file move must advance /project tip"
    );
    let entries = case.tree_entries("/project");
    assert!(
        entries.iter().all(|(n, _)| n != "ft03-file.txt"),
        "source name must vanish: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|(n, t)| n == "ft03-moved.txt" && t == "file"),
        "destination must list the moved file: {entries:?}"
    );
    let after_oid = case
        .tree_oids("/project")
        .into_iter()
        .find(|(n, _)| n == "ft03-moved.txt")
        .map(|(_, o)| o)
        .expect("moved file oid");
    assert_eq!(after_oid, oid, "file move must keep the same blob oid");
    case.finish();
}

#[test]
fn move_entry_reject_directory_as_file_400() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "ft03-dir", true);
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry_as(
        Some(&EntryCase::bearer()),
        "/project",
        "ft03-dir",
        "/project",
        "ft03-dir-as-file",
        false,
    );
    assert_eq!(status, 400, "directory as file must 400: {json}");
    assert!(
        err_message(&json).contains("is not a file"),
        "diagnosable message expected: {json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected file-mode move must not advance tip"
    );
    case.finish();
}

#[test]
fn move_entry_reject_missing_file_404() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry_as(
        Some(&EntryCase::bearer()),
        "/project",
        "ft03-absent.txt",
        "/project",
        "ft03-elsewhere.txt",
        false,
    );
    assert_eq!(status, 404, "missing file name must 404: {json}");
    assert!(
        err_message(&json).contains("not found"),
        "diagnosable message expected: {json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        tip_before,
        "rejected missing-name move must not advance tip"
    );
    case.finish();
}

#[test]
fn move_entry_reject_root() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    for (from_path, from_name) in [("/", ""), ("", "")] {
        let (status, json) = case.move_entry(
            Some(&EntryCase::bearer()),
            from_path,
            from_name,
            "/project",
            "x",
        );
        assert_eq!(
            status, 400,
            "moving the root ({from_path:?}, {from_name:?}) must 400: {json}"
        );
        assert_eq!(json["req_result"], Value::Bool(false), "{json}");
    }
    // Cross-top-level move: the common parent is `/`, which trunk API writes
    // never land on → 400 MONO_PATH_NOT_ALLOWED before any write
    // (plan-20260923 ADR-FU-06; formerly the B0 "no non-root path tip" text).
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-top");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-top",
        "/doc",
        "lb03-top",
    );
    assert_eq!(
        status, 400,
        "cross-top-level move on trunk must 400: {json}"
    );
    assert!(
        err_message(&json).starts_with("MONO_PATH_NOT_ALLOWED: "),
        "diagnosable path policy message expected: {json}"
    );
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    let names = case.tree_names("/project");
    assert!(names.iter().any(|n| n == "lb03-top"), "{names:?}");
    case.finish();
}

#[test]
fn move_entry_reject_traversal() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    let tip_before = path_tip(case.db_url(), "/project");
    for (from_path, from_name, to_path, to_name) in [
        ("/project", "..", "/project", "x"),
        ("/project", "x", "/project/../project", "y"),
        ("/project//", "x", "/project", "y"),
        ("/project", "x", "/project", "a/b"),
    ] {
        let (status, json) = case.move_entry(
            Some(&EntryCase::bearer()),
            from_path,
            from_name,
            to_path,
            to_name,
        );
        assert_eq!(
            status, 400,
            "traversal ({from_path:?}/{from_name:?} -> {to_path:?}/{to_name:?}) must 400: {json}"
        );
    }
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_reject_import_repo() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(Some(&[
        "/project",
        "/third-party",
    ])));
    case.seed();
    let repo = format!("lb03-import-{}", std::process::id());
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, &repo);
    let import_path = format!("/third-party/{repo}");
    // source under an ImportRepo → ImportApiService refuses
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        &import_path,
        "src",
        "/project",
        "lb03-x",
    );
    assert_eq!(status, 409, "ImportRepo source must 409: {json}");
    assert!(err_message(&json).contains("import dir"), "{json}");
    // destination under an ImportRepo → the monorepo handler refuses
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-imp");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-imp",
        &import_path,
        "lb03-imp",
    );
    assert_eq!(status, 409, "ImportRepo destination must 409: {json}");
    assert!(err_message(&json).contains("import dir"), "{json}");
    assert!(
        !err_message(&json).contains("[code:"),
        "prefix must not reach the wire: {json}"
    );
    case.finish();
}

#[test]
fn move_entry_reject_missing_dest_parent() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-m");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-m",
        "/project/lb03-nope",
        "x",
    );
    assert_eq!(status, 404, "missing destination parent must 404: {json}");
    assert!(err_message(&json).contains("destination parent"), "{json}");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project/lb03-nope",
        "y",
        "/project",
        "z",
    );
    assert_eq!(status, 404, "missing source parent must 404: {json}");
    assert!(err_message(&json).contains("source parent"), "{json}");
    assert_eq!(path_tip(case.db_url(), "/project"), tip_before);
    case.finish();
}

#[test]
fn move_entry_success_cl_link_and_trees() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-src");
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-dest");
    let tip_before = path_tip(case.db_url(), "/project");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-src",
        "/project/lb03-dest",
        "moved",
    );
    assert_eq!(status, 200, "token move-entry must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert!(
        json["data"]["cl_link"].is_null(),
        "trunk move must not return cl_link: {json}"
    );
    let commit_id = json["data"]["commit_id"]
        .as_str()
        .expect("commit_id")
        .to_owned();
    assert!(
        !commit_id.is_empty() && commit_id.chars().all(|c| c.is_ascii_hexdigit()),
        "{json}"
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        commit_id,
        "move must advance /project tip"
    );
    assert_ne!(commit_id, tip_before);
    assert_eq!(
        json["data"]["from_path"],
        Value::String("/project/lb03-src".into()),
        "{json}"
    );
    assert_eq!(
        json["data"]["to_path"],
        Value::String("/project/lb03-dest/moved".into()),
        "{json}"
    );
    assert!(
        json["data"].get("new_oid").is_none(),
        "move-entry has no new_oid: {json}"
    );
    let names = case.tree_names("/project");
    assert!(
        names.iter().all(|n| n != "lb03-src"),
        "source name must vanish: {names:?}"
    );
    assert!(names.iter().any(|n| n == "lb03-dest"), "{names:?}");
    let dest = case.tree_entries("/project/lb03-dest");
    assert!(
        dest.iter().any(|(n, t)| n == "moved" && t == "directory"),
        "destination parent must list the moved directory: {dest:?}"
    );
    assert_eq!(
        case.tree_names("/project/lb03-dest/moved"),
        vec![".gitkeep".to_string()],
        "moved subtree keeps its content"
    );
    let (cls, cl_refs) = count_cl_artifacts(case.db_url());
    assert_eq!(
        (cls, cl_refs),
        (0, 0),
        "trunk move must not create CL artifacts"
    );
    case.finish();
}

#[test]
fn move_entry_rename_and_cross_parent() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    // Rename: same parent, new name.
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-r1");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "lb03-r1",
        "/project",
        "lb03-r2",
    );
    assert_eq!(status, 200, "rename must 200: {json}");
    let entries = case.tree_entries("/project");
    assert!(entries.iter().all(|(n, _)| n != "lb03-r1"), "{entries:?}");
    assert!(
        entries
            .iter()
            .any(|(n, t)| n == "lb03-r2" && t == "directory"),
        "{entries:?}"
    );
    assert_eq!(
        case.tree_names("/project/lb03-r2"),
        vec![".gitkeep".to_string()]
    );
    // Cross-parent move where the destination parent is an ancestor of the
    // source parent; the emptied source parent keeps a .gitkeep.
    case.create_dir_at(Some(&EntryCase::bearer()), "/project/lb03-p", "child");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project/lb03-p",
        "child",
        "/project",
        "lb03-moved",
    );
    assert_eq!(status, 200, "cross-parent move must 200: {json}");
    assert_eq!(
        json["data"]["to_path"],
        Value::String("/project/lb03-moved".into()),
        "{json}"
    );
    assert_eq!(
        case.tree_names("/project/lb03-p"),
        vec![".gitkeep".to_string()],
        "emptied source parent keeps .gitkeep"
    );
    let entries = case.tree_entries("/project");
    assert!(
        entries
            .iter()
            .any(|(n, t)| n == "lb03-moved" && t == "directory"),
        "{entries:?}"
    );
    assert_eq!(
        case.tree_names("/project/lb03-moved"),
        vec![".gitkeep".to_string()]
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        json["data"]["commit_id"].as_str().expect("commit_id")
    );
    // Sibling parents: /project/lb03-s1/child → /project/lb03-s2/child; both
    // parents are rebuilt before their shared parent /project.
    case.create_dir_at(Some(&EntryCase::bearer()), "/project/lb03-s1", "child");
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "lb03-s2");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project/lb03-s1",
        "child",
        "/project/lb03-s2",
        "child",
    );
    assert_eq!(status, 200, "sibling-parent move must 200: {json}");
    assert_eq!(
        case.tree_names("/project/lb03-s1"),
        vec![".gitkeep".to_string()]
    );
    let s2 = case.tree_entries("/project/lb03-s2");
    assert!(
        s2.iter().any(|(n, t)| n == "child" && t == "directory"),
        "{s2:?}"
    );
    assert_eq!(
        case.tree_names("/project/lb03-s2/child"),
        vec![".gitkeep".to_string()]
    );
    assert_eq!(
        path_tip(case.db_url(), "/project"),
        json["data"]["commit_id"].as_str().expect("commit_id")
    );
    case.finish();
}

#[test]
fn move_entry_review_uses_existing_cl_branch() {
    // Review morphology: create-entry opens a CL for `/`; a move by the same
    // author reuses it (`EditCLMode::TryReuse(None)`), like create/delete.
    let case = EntryCase::boot(ApiWriteEnv::with_review_config());
    let created = case.create_entry(None, "lb03-review-dir", true);
    let create_link = created["data"]["cl_link"]
        .as_str()
        .expect("review create returns cl_link")
        .to_owned();
    // `doc` and `data` are init root directories, so both exist on main.
    let (status, json) = case.move_entry(None, "/", "doc", "/data", "doc");
    assert_eq!(status, 200, "review move-entry must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert_eq!(
        json["data"]["cl_link"].as_str(),
        Some(create_link.as_str()),
        "review move must reuse the open CL: {json}"
    );
    let (cls, _) = count_cl_artifacts(case.db_url());
    assert_eq!(cls, 1, "create + move must share one mega_cl row");
    case.finish();
}

/// Not a plan gate: `push_auth = "none"` admits a move without a header, as
/// it does for delete-entry (same authorizer, both paths).
#[test]
fn move_entry_auth_none_ok() {
    let case = EntryCase::boot(ApiWriteEnv::with_auth_none_config());
    case.seed();
    case.create_dir_at(None, "/project", "lb03-none");
    let (status, json) = case.move_entry(None, "/project", "lb03-none", "/project", "lb03-none2");
    assert_eq!(
        status, 200,
        "push_auth=none must admit an unauthenticated move: {json}"
    );
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert!(json["data"]["cl_link"].is_null(), "{json}");
    let names = case.tree_names("/project");
    assert!(names.iter().any(|n| n == "lb03-none2"), "{names:?}");
    assert!(names.iter().all(|n| n != "lb03-none"), "{names:?}");
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260917 LB-04: storage-only tag routes + trunk write gate.
// ---------------------------------------------------------------------------

fn tag_body(name: &str, path_context: Option<&str>, message: Option<&str>) -> Value {
    let mut body = serde_json::json!({ "name": name });
    if let Some(path_context) = path_context {
        body["path_context"] = Value::String(path_context.to_string());
    }
    if let Some(message) = message {
        body["message"] = Value::String(message.to_string());
        body["tagger_name"] = Value::String("lb04".to_string());
        body["tagger_email"] = Value::String("lb04@example.com".to_string());
    }
    body
}

/// `GET /tags/list` without Authorization; returns the page's tag names.
fn list_tag_names(case: &EntryCase, path: &str) -> Vec<String> {
    let route = format!("tags/list?page=1&per_page=20&path={path}");
    let (status, json) = case.get(&route, None);
    assert_eq!(status, 200, "tags/list ({path}) must 200: {json}");
    assert!(json["data"]["total"].is_u64(), "{json}");
    json["data"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items array: {json}"))
        .iter()
        .map(|t| t["name"].as_str().expect("tag name").to_string())
        .collect()
}

/// LB-04 AC-1/2/3: storage-only mounts the tag routes; a create without a
/// credential is 401 before anything is written; the runtime OpenAPI lists
/// the three tag paths with create = 200 (not 201) and still omits
/// `/cl`, `/auth`, `/user`.
#[test]
fn tag_create_unauth_401() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = case.post("tags", None, tag_body("lb04-unauth", None, None));
    assert_eq!(
        status, 401,
        "create-tag without credential must 401: {json}"
    );
    assert!(
        !json.to_string().contains(PUSH_TOKEN),
        "error body must not echo the token: {json}"
    );
    assert!(!tag_ref_exists(case.db_url(), "lb04-unauth"));
    let (status, doc) = case.exchange(
        case.client
            .get(format!("http://127.0.0.1:{}/api/openapi.json", case.port)),
        None,
        "GET /api/openapi.json",
    );
    assert_eq!(status, 200, "GET /api/openapi.json");
    for needle in ["/api/v1/tags", "/api/v1/tags/list", "/api/v1/tags/{name}"] {
        assert!(
            doc["paths"].get(needle).is_some(),
            "runtime OpenAPI must list {needle}"
        );
    }
    let codes: Vec<String> = doc["paths"]["/api/v1/tags"]["post"]["responses"]
        .as_object()
        .expect("create-tag responses")
        .keys()
        .cloned()
        .collect();
    assert!(codes.iter().any(|c| c == "200"), "{codes:?}");
    assert!(codes.iter().all(|c| c != "201"), "{codes:?}");
    assert!(doc["paths"]["/api/v1/tags/list"]["get"].is_object());
    assert!(doc["paths"]["/api/v1/tags/list"]["post"].is_null());
    assert!(doc["paths"]["/api/v1/tags/{name}"]["get"].is_object());
    assert!(doc["paths"]["/api/v1/tags/{name}"]["delete"].is_object());
    let paths: Vec<&String> = doc["paths"].as_object().expect("paths").keys().collect();
    for forbidden in ["/cl", "/auth", "/user"] {
        assert!(
            paths.iter().all(|p| !p.contains(forbidden)),
            "storage-only OpenAPI must omit {forbidden}: {paths:?}"
        );
    }
    assert!(!doc.to_string().contains(PUSH_TOKEN));
    case.finish();
}

/// LB-04 AC-3: a token scoped to `/project` is 403 for a create whose
/// `path_context` is omitted or `/` and for every delete (authorization
/// path `/`), while `path_context = "/project"` is authorized and lands the
/// tag under `/project`.
#[test]
fn tag_write_path_scoped_token_403() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    for path_context in [None, Some("/")] {
        let (status, json) = case.post(
            "tags",
            Some(&EntryCase::bearer()),
            tag_body("lb04-scoped", path_context, None),
        );
        assert_eq!(
            status, 403,
            "create with a /project-scoped token and path_context {path_context:?} must 403: {json}"
        );
        assert!(!json.to_string().contains(PUSH_TOKEN), "{json}");
    }
    assert!(!tag_ref_exists(case.db_url(), "lb04-scoped"));
    // delete is 403 before any existence check (not 404).
    let (status, json) = case.delete("tags/lb04-scoped", Some(&EntryCase::bearer()));
    assert_eq!(
        status, 403,
        "delete with a /project-scoped token must 403: {json}"
    );
    let (status, json) = case.post(
        "tags",
        Some(&EntryCase::bearer()),
        tag_body("lb04-covered", Some("/project"), None),
    );
    assert_eq!(
        status, 200,
        "covered path_context must be authorized: {json}"
    );
    assert_eq!(json["data"]["name"], Value::String("lb04-covered".into()));
    assert!(tag_ref_exists(case.db_url(), "lb04-covered"));
    let names = list_tag_names(&case, "/project");
    assert!(names.iter().any(|n| n == "lb04-covered"), "{names:?}");
    case.finish();
}

/// LB-04 AC-5/6/7: with a whole-repo token the root tag lifecycle works —
/// lightweight and annotated create (200, seven string fields), anonymous get
/// and POST list, duplicate 400, invalid name 400, delete 200 then 404, and
/// no CL artifacts.
#[test]
fn tag_root_token_lifecycle() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let bearer = EntryCase::bearer();
    let (status, json) = case.post("tags", Some(&bearer), tag_body("bad name", None, None));
    assert_eq!(status, 400, "invalid tag name must 400: {json}");

    let (status, json) = case.post("tags", Some(&bearer), tag_body("lb04-light", None, None));
    assert_eq!(status, 200, "lightweight create must 200: {json}");
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    let data = &json["data"];
    for key in [
        "name",
        "tag_id",
        "object_id",
        "object_type",
        "tagger",
        "message",
        "created_at",
    ] {
        assert!(data[key].is_string(), "{key} must be a string: {json}");
    }
    assert_eq!(data["name"], Value::String("lb04-light".into()));
    assert_eq!(data["object_type"], Value::String("commit".into()));
    assert_eq!(data["message"], Value::String(String::new()));
    let object_id = data["object_id"].as_str().expect("object_id").to_string();
    assert!(tag_ref_exists(case.db_url(), "lb04-light"));

    let (status, json) = case.get("tags/lb04-light", None);
    assert_eq!(status, 200, "anonymous get must 200: {json}");
    assert_eq!(json["data"]["object_id"], Value::String(object_id.clone()));
    let names = list_tag_names(&case, "/");
    assert!(names.iter().any(|n| n == "lb04-light"), "{names:?}");

    let (status, json) = case.post("tags", Some(&bearer), tag_body("lb04-light", None, None));
    assert_eq!(status, 400, "duplicate tag must 400: {json}");
    assert!(err_message(&json).contains("already exists"), "{json}");
    assert!(!err_message(&json).contains("[code:"), "{json}");
    assert!(!json.to_string().contains(PUSH_TOKEN), "{json}");

    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("lb04-anno", None, Some("annotated by LB-04")),
    );
    assert_eq!(status, 200, "annotated create must 200: {json}");
    assert_eq!(
        json["data"]["message"],
        Value::String("annotated by LB-04".into()),
        "{json}"
    );
    assert!(
        json["data"]["tagger"]
            .as_str()
            .expect("tagger")
            .contains("lb04"),
        "{json}"
    );
    assert!(tag_ref_exists(case.db_url(), "lb04-anno"));
    let (status, json) = case.get("tags/lb04-anno", None);
    assert_eq!(status, 200, "{json}");
    assert_eq!(
        json["data"]["message"],
        Value::String("annotated by LB-04".into())
    );

    let (status, json) = case.delete("tags/lb04-light", Some(&bearer));
    assert_eq!(status, 200, "delete must 200: {json}");
    assert_eq!(
        json["data"]["deleted_tag"],
        Value::String("lb04-light".into()),
        "{json}"
    );
    assert!(json["data"]["message"].is_string(), "{json}");
    assert!(!tag_ref_exists(case.db_url(), "lb04-light"));
    let (status, json) = case.get("tags/lb04-light", None);
    assert_eq!(status, 404, "deleted tag must 404 on get: {json}");
    let names = list_tag_names(&case, "/");
    assert!(names.iter().all(|n| n != "lb04-light"), "{names:?}");
    let (status, json) = case.delete("tags/lb04-light", Some(&bearer));
    assert_eq!(status, 404, "deleting a missing tag must 404: {json}");

    let (status, json) = case.delete("tags/lb04-anno", Some(&bearer));
    assert_eq!(status, 200, "annotated delete must 200: {json}");
    assert!(!tag_ref_exists(case.db_url(), "lb04-anno"));
    let (status, _) = case.get("tags/lb04-anno", None);
    assert_eq!(status, 404);
    let names = list_tag_names(&case, "/");
    assert!(names.iter().all(|n| n != "lb04-anno"), "{names:?}");

    let (cls, cl_refs) = count_cl_artifacts(case.db_url());
    assert_eq!((cls, cl_refs), (0, 0), "tag writes must not create CLs");
    case.finish();
}

/// LB-04 AC-3: delete without a credential is 401 and leaves the tag.
#[test]
fn tag_delete_unauth_401() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let (status, json) = case.post(
        "tags",
        Some(&EntryCase::bearer()),
        tag_body("lb04-keep", None, None),
    );
    assert_eq!(status, 200, "{json}");
    let (status, json) = case.delete("tags/lb04-keep", None);
    assert_eq!(status, 401, "delete without credential must 401: {json}");
    assert!(!json.to_string().contains(PUSH_TOKEN), "{json}");
    assert!(tag_ref_exists(case.db_url(), "lb04-keep"));
    let (status, _) = case.get("tags/lb04-keep", None);
    assert_eq!(status, 200);
    case.finish();
}

/// LB-04 AC-3: `push_auth = "none"` admits tag create and delete without a
/// header.
#[test]
fn tag_auth_none_write_ok() {
    let case = EntryCase::boot(ApiWriteEnv::with_auth_none_config());
    let (status, json) = case.post("tags", None, tag_body("lb04-none", None, None));
    assert_eq!(status, 200, "push_auth=none create must 200: {json}");
    assert!(tag_ref_exists(case.db_url(), "lb04-none"));
    let (status, json) = case.delete("tags/lb04-none", None);
    assert_eq!(status, 200, "push_auth=none delete must 200: {json}");
    assert!(!tag_ref_exists(case.db_url(), "lb04-none"));
    case.finish();
}

/// LB-04 AC-4: the Review morphology does not gain the trunk gate — create
/// and delete without a header keep answering 200.
#[test]
fn tag_review_form_no_trunk_gate() {
    let case = EntryCase::boot(ApiWriteEnv::with_review_config());
    let (status, json) = case.post("tags", None, tag_body("lb04-review", None, None));
    assert_eq!(status, 200, "Review create-tag must not 401: {json}");
    assert!(tag_ref_exists(case.db_url(), "lb04-review"));
    let (status, json) = case.get("tags/lb04-review", None);
    assert_eq!(status, 200, "{json}");
    let (status, json) = case.delete("tags/lb04-review", None);
    assert_eq!(status, 200, "Review delete-tag must not 401: {json}");
    assert!(!tag_ref_exists(case.db_url(), "lb04-review"));
    case.finish();
}

/// FT-04: GET list requires `page`, `per_page`, `path` (extractor 400);
/// `per_page=0` is handler 400; POST is 405.
#[test]
fn tag_list_requires_query_keys() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = case.get("tags/list", None);
    assert_eq!(status, 400, "missing query must 400: {json}");
    let (status, json) = case.get("tags/list?page=1&per_page=20", None);
    assert_eq!(status, 400, "missing path must 400: {json}");
    let (status, json) = case.get("tags/list?page=1&path=/", None);
    assert_eq!(status, 400, "missing per_page must 400: {json}");
    let (status, json) = case.get("tags/list?page=1&per_page=0&path=/", None);
    assert_eq!(status, 400, "per_page=0 must 400: {json}");
    assert!(
        err_message(&json).contains("per_page must be >= 1"),
        "{json}"
    );
    let (status, json) = case.post("tags/list", None, serde_json::json!({}));
    assert_eq!(status, 405, "POST list must 405: {json}");
    let (status, json) = case.get("tags/list?page=1&per_page=20&path=/", None);
    assert_eq!(status, 200, "{json}");
    assert!(json["data"]["total"].is_u64(), "{json}");
    assert!(json["data"]["items"].is_array(), "{json}");
    case.finish();
}

/// FT-06: same tag name at `/` and `/project` is isolated by `?path=`.
#[test]
fn tag_path_isolation_same_name() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let bearer = EntryCase::bearer();
    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-same", None, Some("root tag")),
    );
    assert_eq!(status, 200, "root create: {json}");
    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-same", Some("/project"), Some("project tag")),
    );
    assert_eq!(status, 200, "project create: {json}");
    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-same", Some("/project"), Some("dup")),
    );
    assert_eq!(status, 400, "same path_context duplicate must 400: {json}");

    let (status, json) = case.get("tags/ft06-same", None);
    assert_eq!(status, 200, "omit path is /: {json}");
    assert_eq!(json["data"]["message"], Value::String("root tag".into()));
    let (status, json) = case.get("tags/ft06-same?path=/project", None);
    assert_eq!(status, 200, "project get: {json}");
    assert_eq!(json["data"]["message"], Value::String("project tag".into()));
    let (status, json) = case.get("tags/ft06-same?path=/other", None);
    assert_eq!(status, 404, "other path: {json}");

    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-root-only", None, Some("root only")),
    );
    assert_eq!(status, 200, "root-only create: {json}");
    let (status, json) = case.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-proj-only", Some("/project"), Some("proj only")),
    );
    assert_eq!(status, 200, "project-only create: {json}");
    let root_names = list_tag_names(&case, "/");
    let project_names = list_tag_names(&case, "/project");
    assert!(
        root_names.iter().any(|n| n == "ft06-same"),
        "{root_names:?}"
    );
    assert!(
        project_names.iter().any(|n| n == "ft06-same"),
        "{project_names:?}"
    );
    assert!(
        root_names.iter().any(|n| n == "ft06-root-only")
            && root_names.iter().all(|n| n != "ft06-proj-only"),
        "list / must filter annotated tags: {root_names:?}"
    );
    assert!(
        project_names.iter().any(|n| n == "ft06-proj-only")
            && project_names.iter().all(|n| n != "ft06-root-only"),
        "list /project must filter annotated tags: {project_names:?}"
    );

    let (status, json) = case.delete("tags/ft06-same?path=/project", Some(&bearer));
    assert_eq!(status, 200, "delete project: {json}");
    let (status, json) = case.get("tags/ft06-same?path=/project", None);
    assert_eq!(status, 404, "project gone: {json}");
    let (status, json) = case.get("tags/ft06-same", None);
    assert_eq!(status, 200, "root remains: {json}");
    case.finish();

    let scoped = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = scoped.post(
        "tags",
        Some(&bearer),
        tag_body("ft06-scoped", Some("/project"), Some("scoped")),
    );
    assert_eq!(status, 200, "scoped create: {json}");
    let (status, json) = scoped.delete("tags/ft06-scoped?path=/project", Some(&bearer));
    assert_eq!(status, 200, "scoped token delete at /project: {json}");
    let (status, json) = scoped.delete("tags/ft06-scoped", Some(&bearer));
    assert_eq!(status, 403, "scoped token delete at / must 403: {json}");
    scoped.finish();
}

// ---------------------------------------------------------------------------
// plan-20260921 AR-02: artifacts protocol on storage-only — token upload →
// batch → commit → sets → byte read-back; the same writes without a
// credential → 401. The seeded token covers `/project`, so `repos/project`
// passes the ADR-AR-02 authorization path.
// ---------------------------------------------------------------------------

const AR02_NAMESPACE: &str = "ar02-it";
const AR02_OBJECT_TYPE: &str = "snapshot";

fn artifacts_url(api: &str, repo: &str, suffix: &str) -> String {
    format!("{api}/repos/{repo}/artifacts{suffix}")
}

#[test]
fn artifacts_storage_only_token_lifecycle() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let bearer = EntryCase::bearer();
    let api = &case.api;
    let repo = "project";

    // Unauthenticated writes are rejected before touching storage.
    let unauth_oid = uuid::Uuid::new_v4().to_string();
    let unauth_put = case
        .client
        .put(artifacts_url(api, repo, &format!("/objects/{unauth_oid}")))
        .body("nope")
        .send()
        .expect("unauth PUT object");
    assert_eq!(
        unauth_put.status().as_u16(),
        401,
        "PUT object without credential must 401"
    );
    let (status, json) = case.post(
        &format!("repos/{repo}/artifacts/batch"),
        None,
        serde_json::json!({
            "namespace": AR02_NAMESPACE,
            "object_type": AR02_OBJECT_TYPE,
            "intent": "upload",
            "objects": [{"path": "bin/app", "oid": unauth_oid, "size": 4}]
        }),
    );
    assert_eq!(status, 401, "batch without credential must 401: {json}");
    assert!(
        !json.to_string().contains(PUSH_TOKEN),
        "error body must not echo the token: {json}"
    );
    let (status, json) = case.post(
        &format!("repos/{repo}/artifacts/commit"),
        None,
        serde_json::json!({
            "namespace": AR02_NAMESPACE,
            "object_type": AR02_OBJECT_TYPE,
            "files": [{"path": "bin/app", "oid": unauth_oid, "size": 4}]
        }),
    );
    assert_eq!(status, 401, "commit without credential must 401: {json}");

    // Reads stay anonymous (ADR-AR-03): discovery works without a header.
    let (status, json) = case.get(&format!("repos/{repo}/artifacts/discovery"), None);
    assert_eq!(status, 200, "anonymous discovery must 200: {json}");
    assert_eq!(
        json["protocol_version"],
        Value::String("artifacts/v1".into()),
        "{json}"
    );

    // Authorized lifecycle: PUT bytes → batch → commit → sets → read-back.
    let oid = uuid::Uuid::new_v4().to_string();
    let artifact_set_id = uuid::Uuid::new_v4().to_string();
    let payload = b"ar02 artifact bytes\n";
    let put = case
        .client
        .put(artifacts_url(api, repo, &format!("/objects/{oid}")))
        .header("Authorization", &bearer)
        .body(payload.to_vec())
        .send()
        .expect("token PUT object");
    assert_eq!(
        put.status().as_u16(),
        204,
        "token PUT object must 204; body={}",
        put.text().unwrap_or_default()
    );

    let (status, json) = case.post(
        &format!("repos/{repo}/artifacts/batch"),
        Some(&bearer),
        serde_json::json!({
            "namespace": AR02_NAMESPACE,
            "object_type": AR02_OBJECT_TYPE,
            "intent": "upload",
            "objects": [{"path": "bin/app", "oid": oid, "size": payload.len()}]
        }),
    );
    assert_eq!(status, 200, "token batch must 200: {json}");
    assert_eq!(
        json["objects"][0]["exists"],
        Value::Bool(true),
        "uploaded object must report exists: {json}"
    );

    let (status, json) = case.post(
        &format!("repos/{repo}/artifacts/commit"),
        Some(&bearer),
        serde_json::json!({
            "namespace": AR02_NAMESPACE,
            "object_type": AR02_OBJECT_TYPE,
            "artifact_set_id": artifact_set_id,
            "files": [{"path": "bin/app", "oid": oid, "size": payload.len()}]
        }),
    );
    assert_eq!(status, 200, "token commit must 200: {json}");
    assert_eq!(
        json["status"],
        Value::String("ok".into()),
        "commit status: {json}"
    );
    assert_eq!(
        json["artifact_set_id"],
        Value::String(artifact_set_id.clone()),
        "{json}"
    );

    let (status, json) = case.get(
        &format!(
            "repos/{repo}/artifacts/sets?namespace={AR02_NAMESPACE}&object_type={AR02_OBJECT_TYPE}"
        ),
        None,
    );
    assert_eq!(status, 200, "anonymous sets list must 200: {json}");
    let sets = json["sets"]
        .as_array()
        .unwrap_or_else(|| panic!("sets: {json}"));
    assert!(
        sets.iter()
            .any(|s| s["artifact_set_id"].as_str() == Some(artifact_set_id.as_str())),
        "committed set must be listed: {json}"
    );

    let download = case
        .client
        .get(artifacts_url(api, repo, &format!("/objects/{oid}")))
        .send()
        .expect("anonymous GET object");
    assert_eq!(
        download.status().as_u16(),
        200,
        "anonymous object download must 200"
    );
    assert_eq!(
        download.bytes().expect("object bytes").as_ref(),
        payload,
        "downloaded bytes must match the upload"
    );

    // Runtime OpenAPI evidence (AR-01): the booted storage-only surface lists
    // the artifacts routes.
    let (status, doc) = case.exchange(
        case.client
            .get(format!("http://127.0.0.1:{}/api/openapi.json", case.port)),
        None,
        "GET /api/openapi.json",
    );
    assert_eq!(status, 200, "GET /api/openapi.json");
    for needle in [
        "/api/v1/repos/{repo}/artifacts/discovery",
        "/api/v1/repos/{repo}/artifacts/batch",
        "/api/v1/repos/{repo}/artifacts/commit",
        "/api/v1/repos/{repo}/artifacts/objects/{oid}",
    ] {
        assert!(
            doc["paths"].get(needle).is_some(),
            "runtime OpenAPI must list {needle}"
        );
    }

    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260923 FU-03: unsigned API / continuation commits carry the
// header/body blank line (ADR-FU-02), so strict Git clients accept them.
// ---------------------------------------------------------------------------

fn fu03_repo_url(port: u16, path: &str) -> String {
    format!(
        "{}/",
        git_cli::mega2_host_http_url(port, path).trim_end_matches('/')
    )
}

/// Clone `path`, run `git fsck --strict` on every object and return the clone
/// directory name.
fn fu03_clone_strict_fsck(case: &EntryCase, path: &str, dir: &str) -> String {
    let url = fu03_repo_url(case.port, path);
    host_git_ok(&case.env.case_dir, PUSH_TOKEN, &["clone", &url, dir]);
    let output = host_git_command(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["-C", dir, "fsck", "--strict", "--no-dangling"],
    )
    .output()
    .expect("git fsck");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success() && !report.contains("unterminatedHeader"),
        "git fsck --strict {path} must pass:\n{report}"
    );
    dir.to_owned()
}

/// create-entry, edit/save, move-entry and delete-entry on `/project`.
fn fu03_api_writes(case: &EntryCase) {
    let bearer = EntryCase::bearer();
    let auth = Some(bearer.as_str());
    case.create_entry(auth, "fu03-file.txt", false);
    let (status, json) = case.post(
        "edit/save",
        auth,
        serde_json::json!({
            "path": "/project/fu03-file.txt",
            "content": "fu03 saved\n",
            "commit_message": "fu03 edit/save",
            "skip_build": true
        }),
    );
    assert_eq!(status, 200, "edit/save must 200: {json}");
    case.create_entry(auth, "fu03-dir", true);
    let (status, json) = case.move_entry(auth, "/project", "fu03-dir", "/project", "fu03-moved");
    assert_eq!(status, 200, "move-entry must 200: {json}");
    let (status, json) = case.delete_entry(auth, "/project", "fu03-moved");
    assert_eq!(status, 200, "delete-entry must 200: {json}");
}

#[test]
fn monorepo_api_commits_strict_fsck() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    fu03_api_writes(&case);
    let clone = fu03_clone_strict_fsck(&case, "/project", "fu03-project");
    let subjects = host_git_stdout(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["-C", &clone, "log", "--format=%s", "-5"],
    );
    for subject in [
        "delete directory fu03-moved",
        "rename directory fu03-dir to fu03-moved",
        "create new directory fu03-dir",
        "fu03 edit/save",
        "create new file fu03-file.txt",
    ] {
        assert!(
            subjects.lines().any(|line| line == subject),
            "API commit {subject:?} must be in /project history:\n{subjects}"
        );
    }
    case.finish();
}

#[test]
fn import_repo_edit_save_commit_strict_fsck() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, "fu03lib");
    let (status, json) = case.post(
        "edit/save",
        Some(&EntryCase::bearer()),
        serde_json::json!({
            "path": "/third-party/fu03lib/src/lib.rs",
            "content": "// fu03 import edit\n",
            "commit_message": "fu03 import edit/save",
            "skip_build": true
        }),
    );
    assert_eq!(status, 200, "ImportRepo edit/save must 200: {json}");
    let clone = fu03_clone_strict_fsck(&case, "/third-party/fu03lib", "fu03-import");
    let subject = host_git_stdout(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["-C", &clone, "log", "--format=%s", "-1"],
    );
    assert_eq!(subject, "fu03 import edit/save");
    case.finish();
}

#[test]
fn api_history_strict_clone() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    fu03_api_writes(&case);
    for (path, dir) in [
        ("/project", "fu03-strict-project"),
        ("/", "fu03-strict-root"),
    ] {
        let url = fu03_repo_url(case.port, path);
        host_git_ok(
            &case.env.case_dir,
            PUSH_TOKEN,
            &["-c", "transfer.fsckObjects=true", "clone", &url, dir],
        );
    }
    case.finish();
}

#[test]
fn api_commit_subject_visible() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.seed();
    case.create_entry(Some(&EntryCase::bearer()), "fu03-subject", true);
    let url = fu03_repo_url(case.port, "/project");
    host_git_ok(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["clone", &url, "fu03-subject-clone"],
    );
    let subject = host_git_stdout(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["-C", "fu03-subject-clone", "log", "--format=%s", "-1"],
    );
    assert_eq!(subject, "create new directory fu03-subject");
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260923 FU-04A: blame reads commit_message / commit_summary from the
// commit body, so a framed unsigned commit pushed by a Git client shows its
// subject instead of the empty line before it.
// ---------------------------------------------------------------------------

#[test]
fn import_repo_blame_summary_framed() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, "fu04alib");
    let (status, json) = case.get(
        "blame?path=/third-party/fu04alib/src/lib.rs",
        Some(&EntryCase::bearer()),
    );
    assert_eq!(status, 200, "ImportRepo blame must 200: {json}");
    let info = &json["data"]["blocks"][0]["blame_info"];
    assert_eq!(info["commit_summary"], "lb02 import seed", "{json}");
    assert_eq!(info["commit_message"], "lb02 import seed\n", "{json}");
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260923 FU-06: product writes on a fresh stack (no non-root tip) land
// on the lazily materialized first-level root; paths outside the roots and
// the ImportRepo namespace return MONO_PATH_NOT_ALLOWED (ADR-FU-06).
// ---------------------------------------------------------------------------

/// Number of `main` rows at `prefix` or below it.
fn fu06_main_rows_under(db_url: &str, prefix: &str) -> i64 {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for main rows: {err}"));
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT COUNT(*)::bigint AS n FROM mega_refs \
                     WHERE (path = '{prefix}' OR path LIKE '{prefix}/%') \
                     AND ref_name = 'refs/heads/main' AND NOT is_cl"
                ),
            ))
            .await
            .expect("count main rows")
            .expect("count row");
        row.try_get::<i64>("", "n").expect("n")
    })
}

/// Insert a `main@<path>` row by hand, as a pre-FU-06 or externally created
/// state would have left it.
fn fu06_insert_main_row(db_url: &str, path: &str, commit: &str, tree: &str) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for insert: {err}"));
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO mega_refs (id, path, ref_name, ref_commit_hash, ref_tree_hash, \
                 created_at, updated_at, is_cl) SELECT COALESCE(MAX(id), 0) + 1, '{path}', \
                 'refs/heads/main', '{commit}', '{tree}', now(), now(), false FROM mega_refs"
            ),
        ))
        .await
        .expect("insert main row");
    })
}

/// Remove the `git_repo` row of an ImportRepo (its tree leaf stays), as a
/// cleanup would.
fn fu06_drop_git_repo(db_url: &str, repo_path: &str) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect for git_repo delete: {err}"));
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("DELETE FROM git_repo WHERE repo_path = '{repo_path}'"),
        ))
        .await
        .expect("delete git_repo row");
    })
}

fn fu06_assert_not_allowed(status: u16, json: &Value, what: &str) {
    assert_eq!(status, 400, "{what} must 400: {json}");
    assert!(
        err_message(json).starts_with("MONO_PATH_NOT_ALLOWED: "),
        "{what} must return MONO_PATH_NOT_ALLOWED: {json}"
    );
}

fn fu06_create(case: &EntryCase, path: &str, name: &str, is_directory: bool) -> (u16, Value) {
    let mut body = serde_json::json!({
        "is_directory": is_directory,
        "name": name,
        "path": path,
        "author_username": LB02_AUTHOR,
        "skip_build": true
    });
    if !is_directory {
        body["content"] = Value::String("fu06\n".to_string());
    }
    case.post("create-entry", Some(&EntryCase::bearer()), body)
}

fn fu06_oid(case: &EntryCase, path: &str, name: &str) -> String {
    case.tree_oids(path)
        .into_iter()
        .find(|(item, _)| item == name)
        .unwrap_or_else(|| panic!("{name} under {path}"))
        .1
}

#[test]
fn create_entry_fresh_stack_without_clone() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    assert_eq!(fu06_main_rows_under(case.db_url(), "/project"), 0);
    let project_tree = fu06_oid(&case, "/", "project");
    let json = case.create_entry(Some(&EntryCase::bearer()), "fu06-fresh.txt", false);
    let commit = json["data"]["commit_id"].as_str().expect("commit_id");
    assert_eq!(path_tip(case.db_url(), "/project"), commit);

    // The landed tip continues the materialized /project history.
    let url = fu03_repo_url(case.port, "/project");
    host_git_ok(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["clone", &url, "fu06-fresh"],
    );
    let git = |args: &[&str]| {
        let mut all = vec!["-C", "fu06-fresh"];
        all.extend_from_slice(args);
        host_git_stdout(&case.env.case_dir, PUSH_TOKEN, &all)
    };
    assert_eq!(git(&["rev-parse", "HEAD"]), commit);
    assert_eq!(git(&["rev-parse", "HEAD^^{tree}"]), project_tree);
    assert_eq!(
        git(&["rev-list", "--count", "HEAD"]),
        "2",
        "materialized commit is the only ancestor"
    );
    assert!(git(&["ls-tree", "--name-only", "HEAD"]).contains("fu06-fresh.txt"));
    case.finish();
}

#[test]
fn fresh_stack_concurrent_writes() {
    // Two fresh-stack writes under different first-level roots race through
    // the fallback (classify + materialize + queued landing on the shared
    // root). Same-path concurrency is bounded by ADR-TP-10 (`DEFER-FU-12`).
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    std::thread::scope(|scope| {
        let a = scope.spawn(|| fu06_create(&case, "/project", "fu06-a", true));
        let b = scope.spawn(|| fu06_create(&case, "/doc", "fu06-b", true));
        for handle in [a, b] {
            let (status, json) = handle.join().expect("writer thread");
            assert_eq!(
                status, 200,
                "concurrent fresh-stack write must land: {json}"
            );
        }
    });
    assert!(case.tree_names("/project").iter().any(|n| n == "fu06-a"));
    assert!(case.tree_names("/doc").iter().any(|n| n == "fu06-b"));
    for root in ["/project", "/doc"] {
        assert_eq!(fu06_main_rows_under(case.db_url(), root), 1, "{root}");
    }
    case.finish();
}

#[test]
fn api_write_outside_roots_path_policy() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let root_before = path_tip(case.db_url(), "/");
    for (path, name, what) in [
        ("/", "fu06-top", "new top-level directory under /"),
        ("/fu06-vendor", "lib", "path outside the roots"),
        ("/", "fu06\\top", "top-level name with a backslash"),
        (
            "/.cedar",
            "fu06.txt",
            "existing top-level entry outside root_dirs",
        ),
    ] {
        let (status, json) = fu06_create(&case, path, name, path == "/");
        fu06_assert_not_allowed(status, &json, what);
        // The error names the path the user wrote, not the landing directory.
        let written = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        assert!(
            err_message(&json).contains(&format!("{written:?}")),
            "{what}: {json}"
        );
    }
    assert_eq!(path_tip(case.db_url(), "/"), root_before);
    // A move across two roots would land on `/`, where product writes never land.
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "fu06-cross");
    let (status, json) = case.move_entry(
        Some(&EntryCase::bearer()),
        "/project",
        "fu06-cross",
        "/doc",
        "fu06-cross",
    );
    fu06_assert_not_allowed(status, &json, "move across two roots");
    let root_before = path_tip(case.db_url(), "/");
    let (status, json) = fu06_create(&case, "/", "fu06-top2", true);
    fu06_assert_not_allowed(status, &json, "new top-level directory after seeding");
    assert_eq!(path_tip(case.db_url(), "/"), root_before);
    assert_eq!(fu06_main_rows_under(case.db_url(), "/fu06-vendor"), 0);
    case.finish();
}

#[test]
fn api_write_custom_root_dirs() {
    let case = EntryCase::boot(
        ApiWriteEnv::with_token_config_paths(None)
            .with_env("MEGA_MONOREPO__ROOT_DIRS", "apps,third-party"),
    );
    let (status, json) = fu06_create(&case, "/apps", "fu06-web", true);
    assert_eq!(status, 200, "custom root write must land: {json}");
    assert!(case.tree_names("/apps").iter().any(|n| n == "fu06-web"));
    // /project is not a root in this configuration (and was never created).
    let (status, json) = fu06_create(&case, "/project", "fu06-x", true);
    fu06_assert_not_allowed(status, &json, "/project outside custom roots");
    case.finish();
}

#[test]
fn api_write_nested_root_fresh_stack() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.create_dir_at(Some(&EntryCase::bearer()), "/project/fu06-a", "b");
    assert!(case.tree_names("/project/fu06-a").iter().any(|n| n == "b"));
    assert_eq!(fu06_main_rows_under(case.db_url(), "/project/fu06-a"), 0);
    assert_eq!(fu06_main_rows_under(case.db_url(), "/project"), 1);
    case.finish();

    // Nested import_dir: the first-level root /third-party is a strict
    // ancestor of the import directory and is never materialized (GC-FU-04);
    // a write below it that has no tip yet needs provisioning first.
    let nested = EntryCase::boot(
        ApiWriteEnv::with_token_config_paths(None)
            .with_env("MEGA_MONOREPO__IMPORT_DIR", "/third-party/vendor"),
    );
    let root_before = path_tip(nested.db_url(), "/");
    let (status, json) = fu06_create(&nested, "/third-party/tools", "fu06", true);
    assert_eq!(status, 409, "nested import_dir ancestor: {json}");
    assert!(
        err_message(&json).starts_with("MONO_PATH_UNINITIALIZED: \"/third-party/tools/fu06\""),
        "a directory create names the directory: {json}"
    );
    let (status, json) = fu06_create(&nested, "/third-party/tools", "fu06.txt", false);
    assert_eq!(status, 409, "nested import_dir ancestor (file): {json}");
    assert!(
        err_message(&json).starts_with("MONO_PATH_UNINITIALIZED: \"/third-party/tools\""),
        "a file create names its parent directory: {json}"
    );
    let (status, json) = fu06_create(&nested, "/third-party/vendor", "fu06", true);
    fu06_assert_not_allowed(status, &json, "create under nested import_dir");
    assert_eq!(fu06_main_rows_under(nested.db_url(), "/third-party"), 0);
    assert_eq!(path_tip(nested.db_url(), "/"), root_before);

    // With a (legacy) main@/third-party tip, content next to the nested import
    // dir is writable, but moving it into the import dir is refused by the
    // destination guard (without it the move would fail later: /third-party/vendor
    // does not exist in the tree yet).
    let third_party_tree = fu06_oid(&nested, "/", "third-party");
    fu06_insert_main_row(
        nested.db_url(),
        "/third-party",
        &root_before,
        &third_party_tree,
    );
    nested.create_dir_at(Some(&EntryCase::bearer()), "/third-party/tools", "a");
    let tip = path_tip(nested.db_url(), "/third-party");
    let (status, json) = nested.move_entry(
        Some(&EntryCase::bearer()),
        "/third-party/tools",
        "a",
        "/third-party/vendor",
        "a",
    );
    fu06_assert_not_allowed(status, &json, "move into a nested import_dir");
    assert_eq!(path_tip(nested.db_url(), "/third-party"), tip);
    assert!(
        nested
            .tree_names("/third-party/tools")
            .iter()
            .any(|n| n == "a")
    );
    nested.finish();
}

#[test]
fn api_write_import_namespace_guard() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    // (a) Fresh stack: nothing is materialized under the import directory.
    let (status, json) = fu06_create(&case, "/third-party", "fu06-lib", true);
    fu06_assert_not_allowed(status, &json, "create under import_dir");
    assert_eq!(fu06_main_rows_under(case.db_url(), "/third-party"), 0);

    // (b) An imported leaf whose git_repo row is gone (as after cleanup),
    // under a hand-materialized main@/third-party: without the guard these
    // writes would land on that tip; with it every one is refused.
    let repo = format!("fu06-gone-{}", std::process::id());
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, &repo);
    let live = format!("fu06-live-{}", std::process::id());
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, &live);
    let leaf = format!("/third-party/{repo}");
    fu06_drop_git_repo(case.db_url(), &leaf);
    let root = path_tip(case.db_url(), "/");
    let third_party_tree = fu06_oid(&case, "/", "third-party");
    fu06_insert_main_row(case.db_url(), "/third-party", &root, &third_party_tree);
    case.seed();
    case.create_dir_at(Some(&EntryCase::bearer()), "/project", "fu06-dir");
    case.create_entry(Some(&EntryCase::bearer()), "fu06-file.txt", false);
    let root_before = path_tip(case.db_url(), "/");
    let bearer = EntryCase::bearer();
    let auth = Some(bearer.as_str());

    let (status, json) = fu06_create(&case, &leaf, "fu06-new", true);
    fu06_assert_not_allowed(status, &json, "create inside a detached import leaf");
    let (status, json) = case.post(
        "edit/save",
        auth,
        serde_json::json!({
            "path": format!("{leaf}/src/lib.rs"),
            "content": "// fu06\n",
            "commit_message": "fu06 edit",
            "skip_build": true
        }),
    );
    fu06_assert_not_allowed(status, &json, "edit/save inside a detached import leaf");
    let (status, json) = case.delete_entry(auth, "/third-party", &repo);
    fu06_assert_not_allowed(status, &json, "delete a detached import leaf");
    let (status, json) =
        case.move_entry(auth, "/third-party", &repo, "/third-party", "fu06-renamed");
    fu06_assert_not_allowed(status, &json, "rename inside import_dir");
    // A live mounted leaf addressed from its parent is refused the same way
    // (only paths inside a live ImportRepo are dispatched to ImportApiService).
    let (status, json) = case.delete_entry(auth, "/third-party", &live);
    fu06_assert_not_allowed(status, &json, "delete a live import leaf from its parent");
    let (status, json) = case.move_entry(
        auth,
        "/third-party",
        &live,
        "/third-party",
        "fu06-live-renamed",
    );
    fu06_assert_not_allowed(status, &json, "rename a live import leaf from its parent");

    // (c) Moving monorepo content into the import namespace is refused too.
    let (status, json) = case.move_entry(auth, "/project", "fu06-dir", "/third-party", "x");
    fu06_assert_not_allowed(status, &json, "move directory into import_dir");
    let (status, json) = case.move_entry_as(
        auth,
        "/project",
        "fu06-file.txt",
        "/third-party",
        "x",
        false,
    );
    fu06_assert_not_allowed(status, &json, "move file into import_dir");

    assert_eq!(
        path_tip(case.db_url(), "/"),
        root_before,
        "root must not move"
    );
    assert_eq!(path_tip(case.db_url(), "/third-party"), root);
    assert_eq!(fu06_oid(&case, "/", "third-party"), third_party_tree);
    let names = case.tree_names("/project");
    for name in ["fu06-dir", "fu06-file.txt"] {
        assert!(names.iter().any(|n| n == name), "{name} kept in {names:?}");
    }
    let imported = case.tree_names("/third-party");
    assert!(
        imported.contains(&repo) && imported.contains(&live),
        "{imported:?}"
    );
    assert!(!imported.iter().any(|n| n == "x" || n == "fu06-renamed"));
    case.finish();
}

#[test]
fn import_attach_after_import_dir_api_write() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let (status, json) = fu06_create(&case, "/third-party", "fu06-lib", true);
    fu06_assert_not_allowed(status, &json, "create under import_dir");
    let repo = format!("fu06-import-{}", std::process::id());
    seed_import_repo(&case.env.case_dir, case.port, PUSH_TOKEN, &repo);
    assert!(
        case.tree_names("/third-party").contains(&repo),
        "new ImportRepo must attach after the refused API write"
    );
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260923 FU-07: `POST /api/v1/path/provision` — idempotent `mkdir -p`
// through MonoWriteQueue, authorized by the highest component it creates
// (ADR-FU-05).
// ---------------------------------------------------------------------------

const FU07_NARROW_TOKEN: &str = "fu07-narrow-token";
const FU07_WIDE_TOKEN: &str = "fu07-wide-token";

fn fu07_provision(case: &EntryCase, auth: Option<&str>, path: &str) -> (u16, Value) {
    case.post("path/provision", auth, serde_json::json!({ "path": path }))
}

fn fu07_expect(json: &Value, path: &str, created: bool) -> Option<String> {
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    let data = &json["data"];
    assert_eq!(data["path"], Value::String(path.to_owned()), "{json}");
    assert_eq!(data["created"], Value::Bool(created), "{json}");
    let commit = data["commit_id"].as_str().map(str::to_owned);
    assert_eq!(commit.is_some(), created, "{json}");
    commit
}

#[test]
fn path_provision_creates_missing_levels() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = fu07_provision(&case, Some(&EntryCase::bearer()), "/project/fu07/a/b");
    assert_eq!(status, 200, "{json}");
    let commit = fu07_expect(&json, "/project/fu07/a/b", true).expect("commit");
    assert_eq!(path_tip(case.db_url(), "/project"), commit);
    assert_eq!(case.tree_names("/project/fu07/a/b"), [".gitkeep"]);

    let url = fu03_repo_url(case.port, "/project");
    host_git_ok(
        &case.env.case_dir,
        PUSH_TOKEN,
        &["clone", &url, "fu07-clone"],
    );
    let git = |args: &[&str]| {
        let mut all = vec!["-C", "fu07-clone"];
        all.extend_from_slice(args);
        host_git_stdout(&case.env.case_dir, PUSH_TOKEN, &all)
    };
    assert_eq!(git(&["rev-list", "--count", "HEAD"]), "2", "one commit");
    assert_eq!(
        git(&["log", "--format=%s", "-1"]),
        "provision /project/fu07/a/b"
    );
    assert_eq!(git(&["ls-files", "fu07"]), "fu07/a/b/.gitkeep");

    // Component names that repeat or are substrings of their parents.
    for path in [
        "/project/pro/x",
        "/project/fu07/fu07/x",
        "/project/c/project/c",
    ] {
        let (status, json) = fu07_provision(&case, Some(&EntryCase::bearer()), path);
        assert_eq!(status, 200, "{path}: {json}");
        fu07_expect(&json, path, true);
        assert_eq!(case.tree_names(path), [".gitkeep"], "{path}");
    }
    case.finish();
}

#[test]
fn path_provision_is_idempotent() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let bearer = EntryCase::bearer();
    let (status, json) = fu07_provision(&case, Some(&bearer), "/project/fu07-idem");
    assert_eq!(status, 200, "{json}");
    let commit = fu07_expect(&json, "/project/fu07-idem", true).expect("commit");
    for path in ["/project/fu07-idem", "/project"] {
        let (status, json) = fu07_provision(&case, Some(&bearer), path);
        assert_eq!(status, 200, "{json}");
        fu07_expect(&json, path, false);
    }
    assert_eq!(path_tip(case.db_url(), "/project"), commit, "no write");
    case.finish();
}

#[test]
fn path_provision_rejects_policy_violations() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config_paths(None));
    let bearer = EntryCase::bearer();
    let root_before = path_tip(case.db_url(), "/");
    for (path, code) in [
        ("/fu07-vendor/x", "MONO_PATH_NOT_ALLOWED"),
        ("/third-party", "MONO_PATH_NOT_ALLOWED"),
        ("/third-party/x", "MONO_PATH_NOT_ALLOWED"),
        ("/", "MONO_PATH_INVALID"),
        ("project/x", "MONO_PATH_INVALID"),
        ("/project//x", "MONO_PATH_INVALID"),
        ("/project/x/", "MONO_PATH_INVALID"),
        ("/project/./x", "MONO_PATH_INVALID"),
        ("/project/../x", "MONO_PATH_INVALID"),
        ("/project/x\\y", "MONO_PATH_INVALID"),
        ("/project/a\nb", "MONO_PATH_INVALID"),
        ("/project/a\tb", "MONO_PATH_INVALID"),
    ] {
        let (status, json) = fu07_provision(&case, Some(&bearer), path);
        assert_eq!(status, 400, "{path}: {json}");
        assert!(
            err_message(&json).starts_with(&format!("{code}: ")),
            "{path}: {json}"
        );
    }
    assert_eq!(path_tip(case.db_url(), "/"), root_before);
    assert_eq!(fu06_main_rows_under(case.db_url(), "/third-party"), 0);
    assert_eq!(fu06_main_rows_under(case.db_url(), "/fu07-vendor"), 0);
    case.finish();

    let custom = EntryCase::boot(
        ApiWriteEnv::with_token_config_paths(None)
            .with_env("MEGA_MONOREPO__ROOT_DIRS", "apps,third-party"),
    );
    let (status, json) = fu07_provision(&custom, Some(&bearer), "/apps/fu07");
    assert_eq!(status, 200, "{json}");
    fu07_expect(&json, "/apps/fu07", true);
    let (status, json) = fu07_provision(&custom, Some(&bearer), "/project/fu07");
    assert_eq!(status, 400, "{json}");
    assert!(
        err_message(&json).starts_with("MONO_PATH_NOT_ALLOWED: "),
        "{json}"
    );
    custom.finish();

    // A coded 409 stays a 409 even when the path spells a retryable queue
    // text (nested import_dir ancestors are never materialized).
    let nested = EntryCase::boot(
        ApiWriteEnv::with_token_config_paths(None)
            .with_env("MEGA_MONOREPO__IMPORT_DIR", "/third-party/vendor"),
    );
    let (status, json) = fu07_provision(
        &nested,
        Some(&bearer),
        "/third-party/non-fast-forward: new_id does not match current tip",
    );
    assert_eq!(status, 409, "{json}");
    assert!(
        err_message(&json).starts_with("MONO_PATH_UNINITIALIZED: "),
        "{json}"
    );
    nested.finish();
}

#[test]
fn path_provision_conflict_on_file_component() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    case.create_entry(Some(&EntryCase::bearer()), "fu07.txt", false);
    let tip = path_tip(case.db_url(), "/project");
    let (status, json) = fu07_provision(&case, Some(&EntryCase::bearer()), "/project/fu07.txt/sub");
    assert_eq!(status, 409, "{json}");
    assert!(
        err_message(&json)
            .starts_with("MONO_PATH_CONFLICT: \"/project/fu07.txt/sub\": \"/project/fu07.txt\""),
        "{json}"
    );
    assert_eq!(path_tip(case.db_url(), "/project"), tip);
    case.finish();
}

#[test]
fn path_provision_token_scope() {
    let env = ApiWriteEnv::with_git_append(
        "trunk",
        &format!(
            r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "fu07-narrow"
token = "{FU07_NARROW_TOKEN}"
paths = ["/project/team/x"]
[[git.push_tokens]]
name = "fu07-wide"
token = "{FU07_WIDE_TOKEN}"
paths = ["/project/team"]
"#
        ),
    );
    let case = EntryCase::boot(env);
    let narrow = format!("Bearer {FU07_NARROW_TOKEN}");
    let wide = format!("Bearer {FU07_WIDE_TOKEN}");
    let root_before = path_tip(case.db_url(), "/");

    let (status, json) = fu07_provision(&case, None, "/project/team/x");
    assert_eq!(status, 401, "{json}");
    // The narrow token covers the target but not the container it would
    // create (the highest new component, /project/team).
    let (status, json) = fu07_provision(&case, Some(&narrow), "/project/team/x");
    assert_eq!(status, 403, "{json}");
    let (status, json) = fu07_provision(&case, Some(&narrow), "/project/other");
    assert_eq!(status, 403, "{json}");
    assert_eq!(path_tip(case.db_url(), "/"), root_before, "no write yet");

    let (status, json) = fu07_provision(&case, Some(&wide), "/project/team");
    assert_eq!(status, 200, "{json}");
    fu07_expect(&json, "/project/team", true);
    // Now the highest new component is the target itself.
    let (status, json) = fu07_provision(&case, Some(&narrow), "/project/team/x");
    assert_eq!(status, 200, "{json}");
    fu07_expect(&json, "/project/team/x", true);
    let (status, json) = fu07_provision(&case, Some(&narrow), "/project/team/x");
    assert_eq!(status, 200, "{json}");
    fu07_expect(&json, "/project/team/x", false);
    case.finish();
}

#[test]
fn path_provision_concurrent_same_path() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let bearer = EntryCase::bearer();
    let start = std::sync::Barrier::new(2);
    let results: Vec<(u16, Value)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    fu07_provision(&case, Some(&bearer), "/project/fu07-race")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("provision thread"))
            .collect()
    });
    let mut created = 0;
    for (status, json) in &results {
        assert_eq!(*status, 200, "{json}");
        if json["data"]["created"] == Value::Bool(true) {
            created += 1;
        }
    }
    assert_eq!(
        created, 1,
        "exactly one request creates the path: {results:?}"
    );
    assert_eq!(case.tree_names("/project/fu07-race"), [".gitkeep"]);
    case.finish();
}

#[test]
fn path_provision_openapi() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, doc) = case.exchange(
        case.client
            .get(format!("http://127.0.0.1:{}/api/openapi.json", case.port)),
        None,
        "GET /api/openapi.json",
    );
    assert_eq!(status, 200, "GET /api/openapi.json");
    let responses = doc["paths"]["/api/v1/path/provision"]["post"]["responses"]
        .as_object()
        .expect("path/provision POST responses");
    for code in ["200", "400", "401", "403", "409"] {
        assert!(responses.contains_key(code), "{code}: {responses:?}");
    }
    case.finish();
}

// ---------------------------------------------------------------------------
// plan-20260923 FU-20: `POST /api/v1/import-repo/remove` (ADR-FU-10). Every
// query below uses bound parameters; paths deliberately contain `%` and `_`.
// ---------------------------------------------------------------------------

const FU20_ROUTE: &str = "import-repo/remove";
const FU20_REQUEST_ID: &str = "fu20-probe";
const FU20_WIDE_TOKEN: &str = "fu20-wide-token";
const FU20_NARROW_TOKEN: &str = "fu20-narrow-token";
const FU20_UNAUTHORIZED: &[u8] =
    br#"{"req_result":false,"data":null,"err_message":"authentication required"}"#;
const FU20_FORBIDDEN: &[u8] = br#"{"req_result":false,"data":null,"err_message":"forbidden"}"#;

/// Trunk + `push_auth=token` with a wide (`/project`, `/third-party`) and a
/// narrow (`/project`) token.
fn fu20_env() -> ApiWriteEnv {
    ApiWriteEnv::with_git_append(
        "trunk",
        &format!(
            r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "fu20-wide"
token = "{FU20_WIDE_TOKEN}"
paths = ["/project", "/third-party"]
[[git.push_tokens]]
name = "fu20-narrow"
token = "{FU20_NARROW_TOKEN}"
paths = ["/project"]
"#
        ),
    )
}

fn fu20_wide() -> String {
    format!("Bearer {FU20_WIDE_TOKEN}")
}

fn fu20_body(path: &str, cleanup_id: Option<i64>) -> Value {
    match cleanup_id {
        Some(id) => serde_json::json!({ "path": path, "cleanup_id": id }),
        None => serde_json::json!({ "path": path }),
    }
}

fn fu20_remove(case: &EntryCase, path: &str, cleanup_id: Option<i64>) -> (u16, Value) {
    case.post(FU20_ROUTE, Some(&fu20_wide()), fu20_body(path, cleanup_id))
}

/// The same call through a 180 s client, for the volume-driven requests.
fn fu20_remove_slow(case: &EntryCase, path: &str, cleanup_id: Option<i64>) -> (u16, Value) {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()
        .expect("slow http client");
    case.exchange(
        client
            .post(format!("{}/{FU20_ROUTE}", case.api))
            .json(&fu20_body(path, cleanup_id)),
        Some(&fu20_wide()),
        "POST import-repo/remove (slow)",
    )
}

/// Assert a success envelope with exactly the four data keys; return the ids.
fn fu20_expect(json: &Value, path: &str, outcome: &str) -> (Option<i64>, Option<i64>) {
    assert_eq!(json["req_result"], Value::Bool(true), "{json}");
    assert_eq!(json["err_message"].as_str(), Some(""), "{json}");
    let data = json["data"]
        .as_object()
        .unwrap_or_else(|| panic!("data: {json}"));
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["cleanup_id", "outcome", "path", "repo_id"], "{json}");
    assert_eq!(data["path"].as_str(), Some(path), "{json}");
    assert_eq!(data["outcome"].as_str(), Some(outcome), "{json}");
    if outcome == "absent" {
        assert!(
            data["repo_id"].is_null() && data["cleanup_id"].is_null(),
            "{json}"
        );
    } else {
        assert!(
            data["repo_id"].is_i64() && data["cleanup_id"].is_i64(),
            "{json}"
        );
    }
    (data["repo_id"].as_i64(), data["cleanup_id"].as_i64())
}

/// A response as bytes: status, headers (sorted, `date` dropped) and body.
#[derive(Debug, PartialEq, Eq)]
struct Fu20Raw {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

fn fu20_send_raw(request: reqwest::blocking::RequestBuilder, auth: Option<&str>) -> Fu20Raw {
    let mut request = request.header("X-Request-Id", FU20_REQUEST_ID);
    if let Some(auth) = auth {
        request = request.header("Authorization", auth);
    }
    let response = request.send().expect("POST import-repo/remove");
    let status = response.status().as_u16();
    let mut headers: Vec<(String, Vec<u8>)> = response
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str() != "date")
        .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
        .collect();
    headers.sort();
    let body = response.bytes().expect("response body").to_vec();
    Fu20Raw {
        status,
        headers,
        body,
    }
}

fn fu20_raw(case: &EntryCase, auth: Option<&str>, body: &Value) -> Fu20Raw {
    fu20_send_raw(
        case.client
            .post(format!("{}/{FU20_ROUTE}", case.api))
            .json(body),
        auth,
    )
}

fn fu20_raw_text(case: &EntryCase, content_type: Option<&str>, body: &str) -> Fu20Raw {
    let mut request = case
        .client
        .post(format!("{}/{FU20_ROUTE}", case.api))
        .body(body.to_owned());
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    fu20_send_raw(request, Some(&fu20_wide()))
}

fn fu20_raw_message(raw: &Fu20Raw) -> String {
    let json: Value = serde_json::from_slice(&raw.body)
        .unwrap_or_else(|_| panic!("json body: {}", String::from_utf8_lossy(&raw.body)));
    err_message(&json)
}

fn fu20_scalar(db_url: &str, sql: &str, values: Vec<sea_orm::Value>) -> Option<i64> {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect: {err}"));
        db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"))
        .and_then(|row| row.try_get::<Option<i64>>("", "n").expect("column n"))
    })
}

fn fu20_count(db_url: &str, sql: &str, values: Vec<sea_orm::Value>) -> i64 {
    fu20_scalar(db_url, sql, values).unwrap_or(0)
}

fn fu20_exec(db_url: &str, sql: &str, values: Vec<sea_orm::Value>) {
    with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|err| panic!("connect: {err}"));
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    })
}

fn fu20_repo_id(db_url: &str, path: &str) -> Option<i64> {
    fu20_scalar(
        db_url,
        "SELECT id AS n FROM git_repo WHERE repo_path = $1",
        vec![path.into()],
    )
}

/// A `git_repo` row with no refs, objects or mount (B3's row-only detach).
fn fu20_insert_repo_row(db_url: &str, path: &str) -> i64 {
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    fu20_scalar(
        db_url,
        "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
         SELECT COALESCE(MAX(id), 0) + 1, $1, $2, now(), now() FROM git_repo RETURNING id AS n",
        vec![path.into(), name.into()],
    )
    .expect("inserted git_repo id")
}

fn fu20_detach_rows(db_url: &str) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM push_queue WHERE payload->>'op' = $1",
        vec!["detach".into()],
    )
}

fn fu20_queue_rows(db_url: &str) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM push_queue",
        vec![],
    )
}

fn fu20_ledger_rows(db_url: &str) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM import_repo_cleanups",
        vec![],
    )
}

/// `(path, repo_id, state, requester, rows_deleted.git_blob)` of a ledger row.
fn fu20_ledger(db_url: &str, id: i64) -> Option<(String, i64, String, String, i64)> {
    with_runtime(async {
        let db = Database::connect(db_url).await.expect("connect");
        db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT path, repo_id, state, requester, \
             COALESCE((rows_deleted->>'git_blob')::bigint, 0) AS blobs \
             FROM import_repo_cleanups WHERE id = $1",
            vec![id.into()],
        ))
        .await
        .expect("ledger query")
        .map(|row| {
            (
                row.try_get("", "path").expect("path"),
                row.try_get("", "repo_id").expect("repo_id"),
                row.try_get("", "state").expect("state"),
                row.try_get("", "requester").expect("requester"),
                row.try_get("", "blobs").expect("blobs"),
            )
        })
    })
}

fn fu20_audit_rows(db_url: &str, cleanup_id: i64, phase: &str, requester: &str) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM audit_logs \
         WHERE metadata->>'kind' = 'import_repo.remove' AND metadata->>'cleanup_id' = $1 \
         AND metadata->>'phase' = $2 AND metadata->>'requester' = $3",
        vec![
            cleanup_id.to_string().into(),
            phase.into(),
            requester.into(),
        ],
    )
}

fn fu20_all_audit_rows(db_url: &str) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM audit_logs WHERE metadata->>'kind' = 'import_repo.remove'",
        vec![],
    )
}

fn fu20_audit_text(db_url: &str, repo_id: i64) -> String {
    with_runtime(async {
        let db = Database::connect(db_url).await.expect("connect");
        db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(string_agg(metadata::text, ' '), '') AS t FROM audit_logs \
             WHERE target_id = $1 AND metadata->>'kind' = 'import_repo.remove'",
            vec![repo_id.into()],
        ))
        .await
        .expect("audit query")
        .map(|row| row.try_get::<String>("", "t").expect("t"))
        .unwrap_or_default()
    })
}

fn fu20_object_rows(db_url: &str, repo_id: i64) -> i64 {
    fu20_count(
        db_url,
        "SELECT ((SELECT COUNT(*) FROM git_commit WHERE repo_id = $1) \
         + (SELECT COUNT(*) FROM git_tree WHERE repo_id = $1) \
         + (SELECT COUNT(*) FROM git_blob WHERE repo_id = $1) \
         + (SELECT COUNT(*) FROM git_tag WHERE repo_id = $1))::bigint AS n",
        vec![repo_id.into()],
    )
}

fn fu20_blob_rows(db_url: &str, repo_id: i64) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM git_blob WHERE repo_id = $1",
        vec![repo_id.into()],
    )
}

fn fu20_import_refs(db_url: &str, repo_id: i64) -> i64 {
    fu20_count(
        db_url,
        "SELECT COUNT(*)::bigint AS n FROM import_refs WHERE repo_id = $1",
        vec![repo_id.into()],
    )
}

/// `count` more `git_blob` rows for `repo_id`, then fresh planner statistics.
fn fu20_seed_blobs(db_url: &str, repo_id: i64, count: i64) {
    fu20_exec(
        db_url,
        "INSERT INTO git_blob (id, repo_id, blob_id, name, size, created_at, pack_id, file_path, pack_offset, is_delta_in_pack) \
         SELECT m.base + g, $1, 'f' || lpad(g::text, 39, '0'), NULL, 0, now(), '', '', 0, false \
         FROM generate_series(1, $2::bigint) AS g, (SELECT COALESCE(MAX(id), 0) AS base FROM git_blob) AS m",
        vec![repo_id.into(), count.into()],
    );
    fu20_exec(db_url, "ANALYZE git_blob", vec![]);
}

/// Counts of `git_repo`, ledger, detach queue and cleanup audit rows, plus
/// the root tip.
fn fu20_snapshot(case: &EntryCase) -> (i64, i64, i64, i64, String) {
    let db = case.db_url();
    (
        fu20_count(db, "SELECT COUNT(*)::bigint AS n FROM git_repo", vec![]),
        fu20_ledger_rows(db),
        fu20_detach_rows(db),
        fu20_all_audit_rows(db),
        path_tip(db, "/"),
    )
}

fn fu20_upload_pack(case: &EntryCase, path: &str) -> (u16, String) {
    let response = case
        .client
        .get(format!(
            "http://127.0.0.1:{}{path}/info/refs?service=git-upload-pack",
            case.port
        ))
        .send()
        .expect("GET info/refs");
    let status = response.status().as_u16();
    (status, response.text().unwrap_or_default())
}

fn fu20_clone_ok(case: &EntryCase, path: &str, dir: &str) -> bool {
    let url = git_cli::mega2_host_http_url(case.port, path);
    host_git_command(
        &case.env.case_dir,
        FU20_WIDE_TOKEN,
        &["clone", "--quiet", &url, dir],
    )
    .output()
    .expect("git clone")
    .status
    .success()
}

fn fu20_has_leaf(case: &EntryCase, parent: &str, name: &str) -> bool {
    let response = case
        .client
        .get(format!("{}/tree?path={parent}", case.api))
        .send()
        .expect("GET /tree");
    if response.status().as_u16() != 200 {
        return false;
    }
    let json: Value = response.json().expect("tree json");
    json["data"]["tree_items"]
        .as_array()
        .map(|items| items.iter().any(|item| item["name"] == name))
        .unwrap_or(false)
}

fn fu20_assert_gone(case: &EntryCase, path: &str, dir: &str) {
    let (status, text) = fu20_upload_pack(case, path);
    assert_eq!(status, 404, "{path}: {text}");
    assert!(!fu20_clone_ok(case, path, dir), "{path} must not clone");
    assert_eq!(fu20_repo_id(case.db_url(), path), None, "{path}");
}

/// Push the seeded repository of `repo` again (after its removal).
fn fu20_repush(case: &EntryCase, repo: &str) {
    let dir = format!("import-seed-{repo}");
    let url = format!(
        "{}/",
        git_cli::mega2_host_http_url(case.port, &format!("/third-party/{repo}"))
            .trim_end_matches('/')
    );
    host_git_ok(
        &case.env.case_dir,
        FU20_WIDE_TOKEN,
        &[
            "-C",
            &dir,
            "push",
            "--no-thin",
            &url,
            "HEAD:refs/heads/main",
        ],
    );
}

fn fu20_log_paths(case: &EntryCase) -> [PathBuf; 2] {
    let dir = case.env.temp_dir.path();
    [
        dir.join(format!("service-{}.out", case.port)),
        dir.join(format!("service-{}.err", case.port)),
    ]
}

fn fu20_log_offset(case: &EntryCase) -> [usize; 2] {
    fu20_log_paths(case).map(|path| read_log(&path).len())
}

/// Service log text written after `offset`.
fn fu20_log_since(case: &EntryCase, offset: [usize; 2]) -> String {
    sleep(Duration::from_millis(500));
    fu20_log_paths(case)
        .iter()
        .zip(offset)
        .map(|(path, from)| read_log(path).get(from..).unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn import_repo_remove_populated_then_absent() {
    let case = EntryCase::boot(fu20_env());
    let db = case.db_url().to_owned();
    let repo = "fu20-pop";
    let path = "/third-party/fu20-pop";
    seed_import_repo(&case.env.case_dir, case.port, FU20_WIDE_TOKEN, repo);
    let r1 = fu20_repo_id(&db, path).expect("seeded repository");
    assert_eq!(fu20_upload_pack(&case, path).0, 200);
    assert!(fu20_has_leaf(&case, "/third-party", repo));
    assert!(fu20_object_rows(&db, r1) > 0);
    assert!(fu20_import_refs(&db, r1) > 0);
    let root_before = path_tip(&db, "/");

    let (status, json) = fu20_remove(&case, path, None);
    assert_eq!(status, 200, "{json}");
    let (repo_id, cleanup_id) = fu20_expect(&json, path, "removed");
    assert_eq!(repo_id, Some(r1));
    let c1 = cleanup_id.expect("cleanup id");
    assert!(c1 > 0);
    fu20_assert_gone(&case, path, "clone-pop-1");
    assert!(
        !case
            .tree_names("/third-party")
            .iter()
            .any(|name| name == repo)
    );
    assert_ne!(path_tip(&db, "/"), root_before);
    assert_eq!(fu20_object_rows(&db, r1), 0);
    assert_eq!(fu20_import_refs(&db, r1), 0);
    let (ledger_path, ledger_repo, state, requester, _) = fu20_ledger(&db, c1).expect("ledger row");
    assert_eq!(
        (
            ledger_path.as_str(),
            ledger_repo,
            state.as_str(),
            requester.as_str()
        ),
        (path, r1, "swept", "fu20-wide")
    );
    assert_eq!(fu20_audit_rows(&db, c1, "detached", "fu20-wide"), 1);
    assert_eq!(fu20_audit_rows(&db, c1, "swept", "fu20-wide"), 1);
    assert!(!fu20_audit_text(&db, r1).contains(FU20_WIDE_TOKEN));
    assert_eq!(fu20_detach_rows(&db), 1);

    // Repeat: nothing live, nothing pending.
    let before = fu20_snapshot(&case);
    let (status, json) = fu20_remove(&case, path, None);
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, path, "absent"), (None, None));
    assert_eq!(fu20_snapshot(&case), before);

    // A continuation of the finished cleanup is idempotent.
    let (status, json) = fu20_remove(&case, path, Some(c1));
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, path, "removed"), (Some(r1), Some(c1)));
    assert_eq!(fu20_snapshot(&case), before);

    // Re-import at the same path: a new repository, removable again.
    fu20_repush(&case, repo);
    let r2 = fu20_repo_id(&db, path).expect("re-imported repository");
    assert_ne!(r2, r1);
    assert!(fu20_clone_ok(&case, path, "clone-pop-2"));
    let (status, json) = fu20_remove(&case, path, None);
    assert_eq!(status, 200, "{json}");
    let (repo_id, cleanup_id) = fu20_expect(&json, path, "removed");
    assert_eq!(repo_id, Some(r2));
    assert!(cleanup_id.expect("cleanup id") > c1);
    fu20_assert_gone(&case, path, "clone-pop-3");
    case.finish();
}

#[test]
fn import_repo_remove_pending_continuation() {
    let case = EntryCase::boot(fu20_env());
    let db = case.db_url().to_owned();
    let repo = "fu20-pend";
    let path = "/third-party/fu20-pend";
    seed_import_repo(&case.env.case_dir, case.port, FU20_WIDE_TOKEN, repo);
    let r1 = fu20_repo_id(&db, path).expect("seeded repository");
    // 100 500 more blobs: more than the 100 sweep statements of one request.
    fu20_seed_blobs(&db, r1, 100_500);
    let b0 = fu20_blob_rows(&db, r1);

    let (status, json) = fu20_remove_slow(&case, path, None);
    assert_eq!(status, 200, "{json}");
    let (repo_id, cleanup_id) = fu20_expect(&json, path, "pending");
    assert_eq!(repo_id, Some(r1));
    let c1 = cleanup_id.expect("cleanup id");
    assert_eq!(fu20_repo_id(&db, path), None);
    assert_eq!(fu20_upload_pack(&case, path).0, 404);
    // 1 (commit) + 1 (tree) + 98 (blob batches of 1 000) statements.
    assert_eq!(fu20_blob_rows(&db, r1), b0 - 98_000);
    assert_eq!(fu20_object_rows(&db, r1), 2_501);
    assert_eq!(
        fu20_ledger(&db, c1),
        Some((
            path.to_owned(),
            r1,
            "detached".to_owned(),
            "fu20-wide".to_owned(),
            98_000
        ))
    );
    assert_eq!(fu20_detach_rows(&db), 1);

    // While the cleanup still has work: unknown ids and ids of another path
    // are 404 with one text and touch nothing.
    let pending = fu20_ledger(&db, c1);
    let unknown = fu20_count(
        &db,
        "SELECT (COALESCE(MAX(id), 0) + 1000000)::bigint AS n FROM push_queue",
        vec![],
    );
    let (status, json) = fu20_remove(&case, path, Some(unknown));
    assert_eq!(status, 404, "{json}");
    assert_eq!(
        err_message(&json),
        format!("IMPORT_REPO_CLEANUP_NOT_FOUND: no cleanup \"{unknown}\" for \"{path}\"")
    );
    let other = "/third-party/fu20-other";
    let (status, json) = fu20_remove(&case, other, Some(c1));
    assert_eq!(status, 404, "{json}");
    assert_eq!(
        err_message(&json),
        format!("IMPORT_REPO_CLEANUP_NOT_FOUND: no cleanup \"{c1}\" for \"{other}\"")
    );
    assert_eq!(fu20_ledger(&db, c1), pending);
    assert_eq!(fu20_blob_rows(&db, r1), b0 - 98_000);

    // Re-import while the cleanup is pending; the continuation leaves it.
    fu20_repush(&case, repo);
    let r2 = fu20_repo_id(&db, path).expect("re-imported repository");
    assert_ne!(r2, r1);
    let n2 = fu20_object_rows(&db, r2);
    let queue = fu20_queue_rows(&db);
    let (status, json) = fu20_remove_slow(&case, path, Some(c1));
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, path, "removed"), (Some(r1), Some(c1)));
    assert_eq!(fu20_object_rows(&db, r1), 0);
    assert_eq!(
        fu20_ledger(&db, c1).map(|row| (row.2, row.4)),
        Some(("swept".to_owned(), 100_501))
    );
    assert_eq!(fu20_queue_rows(&db), queue);
    assert_eq!(fu20_repo_id(&db, path), Some(r2));
    assert_eq!(fu20_object_rows(&db, r2), n2);
    assert!(fu20_clone_ok(&case, path, "clone-pend-1"));

    // Repeating the continuation writes nothing new.
    let audit = fu20_all_audit_rows(&db);
    let (status, json) = fu20_remove(&case, path, Some(c1));
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, path, "removed"), (Some(r1), Some(c1)));
    assert_eq!(fu20_all_audit_rows(&db), audit);

    // A path-only request removes whatever is live now, then nothing is left.
    let (status, json) = fu20_remove(&case, path, None);
    assert_eq!(status, 200, "{json}");
    let (repo_id, cleanup_id) = fu20_expect(&json, path, "removed");
    assert_eq!(repo_id, Some(r2));
    assert_ne!(cleanup_id, Some(c1));
    assert_eq!(fu20_detach_rows(&db), 2);
    let (status, json) = fu20_remove(&case, path, None);
    assert_eq!(status, 200, "{json}");
    fu20_expect(&json, path, "absent");
    case.finish();
}

#[test]
fn import_repo_remove_invalid_paths() {
    let case = EntryCase::boot(fu20_env());
    let db = case.db_url().to_owned();
    let repo = "fu20-inv";
    let leaf = "/third-party/fu20-inv";
    seed_import_repo(&case.env.case_dir, case.port, FU20_WIDE_TOKEN, repo);
    let before = fu20_snapshot(&case);
    let narrow = format!("Bearer {FU20_NARROW_TOKEN}");
    let wide = fu20_wide();
    let unrelated = [
        "/third-party",
        "/third-party/",
        "/",
        "",
        " /third-party/x",
        "third-party/x",
        "/project/x",
        "/third-partyx/a",
        "/third-party/../project",
        "/third-party/x/..",
        "/third-party/a\\b",
        "/third-party/a\0b",
    ];
    let spellings = [
        format!("{leaf}/"),
        "/third-party//fu20-inv".to_owned(),
        "/third-party/./fu20-inv".to_owned(),
        format!(" {leaf}"),
        format!("{leaf} "),
        format!("{leaf}/."),
        "third-party/fu20-inv".to_owned(),
    ];
    let paths: Vec<String> = unrelated
        .iter()
        .map(|path| (*path).to_owned())
        .chain(spellings)
        .collect();
    for path in &paths {
        let body = serde_json::json!({ "path": path });
        let raws: Vec<Fu20Raw> = [None, Some(narrow.as_str()), Some(wide.as_str())]
            .into_iter()
            .map(|auth| fu20_raw(&case, auth, &body))
            .collect();
        for raw in &raws {
            assert_eq!(raw.status, 400, "{path:?}");
            assert!(
                fu20_raw_message(raw).starts_with("IMPORT_REPO_PATH_INVALID: "),
                "{path:?}: {}",
                fu20_raw_message(raw)
            );
        }
        assert!(
            raws.windows(2).all(|pair| pair[0].body == pair[1].body),
            "{path:?}"
        );
        let message = fu20_raw_message(&raws[0]);
        match path.as_str() {
            "/third-party" => assert_eq!(
                message,
                "IMPORT_REPO_PATH_INVALID: \"/third-party\": the ImportRepo directory itself is not an ImportRepo; push to a path below it"
            ),
            "/project/x" => assert_eq!(
                message,
                "IMPORT_REPO_PATH_INVALID: \"/project/x\": an ImportRepo path must lie strictly below \"/third-party\""
            ),
            "/third-party/fu20-inv/" => assert_eq!(
                message,
                "IMPORT_REPO_PATH_INVALID: \"/third-party/fu20-inv/\": path must be canonical (did you mean \"/third-party/fu20-inv\"?)"
            ),
            "/third-party/a\0b" => assert_eq!(
                message,
                "IMPORT_REPO_PATH_INVALID: \"/third-party/a\\0b\": path must not contain NUL"
            ),
            _ => {}
        }
    }

    // Malformed requests never reach validation or authorization.
    let malformed = [
        (fu20_raw_text(&case, Some("application/json"), "{"), 400),
        (
            fu20_raw_text(
                &case,
                None,
                &serde_json::json!({ "path": leaf }).to_string(),
            ),
            415,
        ),
        (fu20_raw(&case, Some(&wide), &serde_json::json!({})), 422),
        (
            fu20_raw(
                &case,
                Some(&wide),
                &serde_json::json!({ "path": leaf, "cleanupId": 1 }),
            ),
            422,
        ),
        (
            fu20_raw(
                &case,
                Some(&wide),
                &serde_json::json!({ "path": leaf, "cleanup_id": "1" }),
            ),
            422,
        ),
    ];
    for (raw, status) in malformed {
        assert_eq!(raw.status, status, "{}", String::from_utf8_lossy(&raw.body));
        assert!(!raw.body.starts_with(br#"{"req_result""#));
    }

    assert_eq!(fu20_snapshot(&case), before);
    assert!(fu20_repo_id(&db, leaf).is_some());
    assert_eq!(fu20_upload_pack(&case, leaf).0, 200);
    case.finish();
}

#[test]
fn import_repo_remove_exact_match_only() {
    let case = EntryCase::boot(fu20_env());
    let db = case.db_url().to_owned();

    // Phase A: `_` is a literal; sub-paths, suffixes and case variants miss.
    let target = "/third-party/fu20_a";
    seed_import_repo(&case.env.case_dir, case.port, FU20_WIDE_TOKEN, "fu20_a");
    let target_id = fu20_repo_id(&db, target).expect("pushed target");
    let decoys: Vec<(&str, i64)> = [
        "/third-party/fu20xa/c",
        "/third-party/fu20_ab",
        "/third-party/fu20xa",
    ]
    .into_iter()
    .map(|path| (path, fu20_insert_repo_row(&db, path)))
    .collect();
    for probe in [
        "/third-party/fu20_a.git",
        "/third-party/fu20_a/src",
        "/third-party/fu20_ab/x",
        "/third-party/FU20_A",
    ] {
        let (status, json) = fu20_remove(&case, probe, None);
        assert_eq!(status, 200, "{probe}: {json}");
        fu20_expect(&json, probe, "absent");
    }
    assert_eq!(fu20_repo_id(&db, target), Some(target_id));
    assert_eq!(fu20_detach_rows(&db), 0);
    let (status, json) = fu20_remove(&case, target, None);
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, target, "removed").0, Some(target_id));
    for (path, id) in &decoys {
        assert_eq!(fu20_repo_id(&db, path), Some(*id), "{path}");
    }
    fu20_assert_gone(&case, target, "clone-exact-a");

    // Phase B: `%` is a literal and an alias row of the target is no child.
    let target2 = "/third-party/fu20%b";
    let target2_id = fu20_insert_repo_row(&db, target2);
    let decoys2: Vec<(&str, i64)> = ["/third-party/fu20zzb/c", "/third-party/fu20%bc"]
        .into_iter()
        .map(|path| (path, fu20_insert_repo_row(&db, path)))
        .collect();
    let alias = "/third-party/fu20%b/";
    let alias_id = fu20_insert_repo_row(&db, alias);
    let (status, json) = fu20_remove(&case, target2, None);
    assert_eq!(status, 200, "{json}");
    assert_eq!(fu20_expect(&json, target2, "removed").0, Some(target2_id));
    for (path, id) in &decoys2 {
        assert_eq!(fu20_repo_id(&db, path), Some(*id), "{path}");
    }
    assert_eq!(fu20_repo_id(&db, alias), Some(alias_id), "alias row stays");

    // Phase C: a parent with a registered child is refused until the child
    // is removed.
    let parent = "/third-party/fu20p";
    let child = "/third-party/fu20p/c";
    fu20_insert_repo_row(&db, parent);
    fu20_insert_repo_row(&db, child);
    let (detached, ledger) = (fu20_detach_rows(&db), fu20_ledger_rows(&db));
    let (status, json) = fu20_remove(&case, parent, None);
    assert_eq!(status, 409, "{json}");
    assert_eq!(
        err_message(&json),
        "IMPORT_REPO_HAS_CHILDREN: \"/third-party/fu20p\" contains other ImportRepos; remove them first"
    );
    assert_eq!(
        (fu20_detach_rows(&db), fu20_ledger_rows(&db)),
        (detached, ledger)
    );
    let (status, json) = fu20_remove(&case, child, None);
    assert_eq!(status, 200, "{json}");
    fu20_expect(&json, child, "removed");
    assert!(fu20_repo_id(&db, parent).is_some());
    let (status, json) = fu20_remove(&case, parent, None);
    assert_eq!(status, 200, "{json}");
    fu20_expect(&json, parent, "removed");
    assert_eq!(fu20_detach_rows(&db), 4);
    case.finish();
}

#[test]
fn import_repo_remove_requires_token() {
    let case = EntryCase::boot(fu20_env());
    let db = case.db_url().to_owned();
    let leaf = "/third-party/fu20-rt-leaf";
    let parent = "/third-party/fu20-rt-par";
    let absent = "/third-party/fu20-rt-none";
    seed_import_repo(
        &case.env.case_dir,
        case.port,
        FU20_WIDE_TOKEN,
        "fu20-rt-leaf",
    );
    fu20_insert_repo_row(&db, parent);
    fu20_insert_repo_row(&db, &format!("{parent}/c"));
    let before = fu20_snapshot(&case);
    let offset = fu20_log_offset(&case);
    let bodies = |path: &str| [fu20_body(path, None), fu20_body(path, Some(1))];

    let mut unauthorized = Vec::new();
    for auth in [
        None,
        Some("Bearer fu20-bogus".to_owned()),
        Some("Basic dTpmdTIwLWJvZ3Vz".to_owned()),
    ] {
        for path in [absent, leaf, parent] {
            for body in bodies(path) {
                unauthorized.push(fu20_raw(&case, auth.as_deref(), &body));
            }
        }
    }
    assert_eq!(unauthorized.len(), 18);
    let mut forbidden = Vec::new();
    for auth in [
        format!("Bearer {FU20_NARROW_TOKEN}"),
        format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("u:{FU20_NARROW_TOKEN}")
            )
        ),
    ] {
        for path in [absent, leaf, parent] {
            for body in bodies(path) {
                forbidden.push(fu20_raw(&case, Some(&auth), &body));
            }
        }
    }
    assert_eq!(forbidden.len(), 12);
    for (raws, status, body) in [
        (&unauthorized, 401, FU20_UNAUTHORIZED),
        (&forbidden, 403, FU20_FORBIDDEN),
    ] {
        for raw in raws.iter() {
            assert_eq!(raw.status, status);
            assert_eq!(raw.body, body, "{}", String::from_utf8_lossy(&raw.body));
            assert!(
                raw.headers
                    .iter()
                    .all(|(name, _)| name != "www-authenticate")
            );
            assert!(
                raw.headers
                    .iter()
                    .any(|(name, value)| name == "x-request-id"
                        && value == FU20_REQUEST_ID.as_bytes()),
                "{:?}",
                raw.headers
            );
        }
        assert!(raws.windows(2).all(|pair| pair[0] == pair[1]));
    }
    let logs = fu20_log_since(&case, offset);
    for secret in [
        absent,
        leaf,
        parent,
        "fu20-rt",
        FU20_WIDE_TOKEN,
        FU20_NARROW_TOKEN,
        "fu20-bogus",
    ] {
        assert!(!logs.contains(secret), "log names {secret}:\n{logs}");
    }
    assert!(
        logs.matches("Application error: authentication required")
            .count()
            >= 18
    );
    assert!(logs.matches("Application error: forbidden").count() >= 12);
    assert_eq!(fu20_snapshot(&case), before);
    assert_eq!(fu20_upload_pack(&case, leaf).0, 200);

    // A covering token gets the real answers.
    let (status, json) = fu20_remove(&case, absent, None);
    assert_eq!(status, 200, "{json}");
    fu20_expect(&json, absent, "absent");
    let (status, json) = fu20_remove(&case, parent, None);
    assert_eq!(status, 409, "{json}");
    assert_eq!(
        err_message(&json),
        "IMPORT_REPO_HAS_CHILDREN: \"/third-party/fu20-rt-par\" contains other ImportRepos; remove them first"
    );
    assert_eq!(fu20_snapshot(&case), before);
    let (status, json) = fu20_remove(&case, leaf, None);
    assert_eq!(status, 200, "{json}");
    let (_, cleanup_id) = fu20_expect(&json, leaf, "removed");
    let requester = fu20_ledger(&db, cleanup_id.expect("cleanup id")).map(|row| row.3);
    assert_eq!(requester.as_deref(), Some("fu20-wide"));
    case.finish();

    // push_auth=none: always 403 once the path is valid; nothing is written.
    let case = EntryCase::boot(ApiWriteEnv::with_auth_none_config());
    let db = case.db_url().to_owned();
    let anon = "/third-party/fu20-anon";
    seed_import_repo(&case.env.case_dir, case.port, "unused", "fu20-anon");
    let reference = unauthorized_forbidden_reference(&forbidden[0]);
    for auth in [None, Some(fu20_wide())] {
        for path in [anon, "/third-party/fu20-none2"] {
            for body in bodies(path) {
                let raw = fu20_raw(&case, auth.as_deref(), &body);
                assert_eq!((raw.status, raw.body.clone()), reference, "{body}");
            }
        }
    }
    assert!(fu20_repo_id(&db, anon).is_some());
    assert!(fu20_clone_ok(&case, anon, "clone-anon"));
    assert_eq!((fu20_detach_rows(&db), fu20_ledger_rows(&db)), (0, 0));
    let raw = fu20_raw(&case, None, &serde_json::json!({ "path": "/third-party" }));
    assert_eq!(raw.status, 400);
    assert!(fu20_raw_message(&raw).starts_with("IMPORT_REPO_PATH_INVALID: "));
    case.finish();
}

fn unauthorized_forbidden_reference(raw: &Fu20Raw) -> (u16, Vec<u8>) {
    (raw.status, raw.body.clone())
}

#[test]
fn import_repo_remove_openapi() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, doc) = case.exchange(
        case.client
            .get(format!("http://127.0.0.1:{}/api/openapi.json", case.port)),
        None,
        "GET /api/openapi.json",
    );
    assert_eq!(status, 200, "GET /api/openapi.json");
    let item = doc["paths"]["/api/v1/import-repo/remove"]
        .as_object()
        .expect("import-repo/remove path item");
    assert_eq!(item.keys().collect::<Vec<_>>(), ["post"]);
    let mut statuses: Vec<&String> = item["post"]["responses"]
        .as_object()
        .expect("responses")
        .keys()
        .collect();
    statuses.sort();
    assert_eq!(statuses, ["200", "400", "401", "403", "404", "409", "500"]);
    assert_eq!(
        item["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ImportRepoRemoveRequest"
    );
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["ImportRepoRemoveRequest"]["additionalProperties"],
        Value::Bool(false)
    );
    assert_eq!(
        schemas["ImportRepoRemoveOutcome"]["enum"],
        serde_json::json!(["removed", "pending", "absent"])
    );
    assert!(!doc.to_string().contains(PUSH_TOKEN));
    case.finish();
}
