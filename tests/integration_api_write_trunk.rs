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
                    "integration PostgreSQL is not available; run `docker compose -p mega2-it -f docker-compose.test.yml up -d --wait` first"
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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            .env("MEGA_MONOREPO__PUSH_POLICY", self.push_policy)
            .env("MEGA_GIT__SSH_RECEIVE_PACK", "false");
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
    let output = command.output().expect("host git");
    git_cli::assert_git_success(&output, &format!("git {}", args.join(" ")));
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
        self.post(
            "move-entry",
            auth,
            serde_json::json!({
                "from_path": from_path,
                "from_name": from_name,
                "to_path": to_path,
                "to_name": to_name,
                "author_username": LB02_AUTHOR,
                "skip_build": true
            }),
        )
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
    // Cross-top-level move: the common parent is `/`, whose tip B0 refuses to
    // advance through the write queue on trunk → 400 before any write.
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
        "cross-top-level move on trunk must 400 (B0): {json}"
    );
    assert!(
        err_message(&json).contains("no non-root path tip"),
        "diagnosable B0 message expected: {json}"
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

fn list_body(additional: &str) -> Value {
    serde_json::json!({ "pagination": { "page": 1, "per_page": 20 }, "additional": additional })
}

/// `POST /tags/list` without Authorization; returns the page's tag names.
fn list_tag_names(case: &EntryCase, additional: &str) -> Vec<String> {
    let (status, json) = case.post("tags/list", None, list_body(additional));
    assert_eq!(status, 200, "tags/list ({additional}) must 200: {json}");
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
    assert!(doc["paths"]["/api/v1/tags/list"]["post"].is_object());
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

/// LB-04 AC-5: `POST /tags/list` needs both `pagination` and `additional`
/// (no serde defaults) — a body missing either is rejected by the JSON
/// extractor (422), a complete body is 200 with `{ total, items }`.
#[test]
fn tag_list_requires_both_keys() {
    let case = EntryCase::boot(ApiWriteEnv::with_token_config());
    let (status, json) = case.post(
        "tags/list",
        None,
        serde_json::json!({ "pagination": { "page": 1, "per_page": 20 } }),
    );
    assert_eq!(status, 422, "missing additional must be rejected: {json}");
    let (status, json) = case.post("tags/list", None, serde_json::json!({ "additional": "/" }));
    assert_eq!(status, 422, "missing pagination must be rejected: {json}");
    let (status, json) = case.post("tags/list", None, list_body("/"));
    assert_eq!(status, 200, "{json}");
    assert!(json["data"]["total"].is_u64(), "{json}");
    assert!(json["data"]["items"].is_array(), "{json}");
    case.finish();
}
