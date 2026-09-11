//! Trunk product API write IT (plan-20260904 / AW-03).
//!
//! Boots real `service http` under `push_policy=trunk` + `push_auth=token`, then:
//! - unauthenticated create-entry → 401
//! - token create-entry advances `/project` tip, no mega_cl / refs/cl
//! - token edit/save advances tip again

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

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
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
            "monoengine_aw03_{}_{}",
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

struct ApiWriteEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
    case_dir: PathBuf,
}

impl ApiWriteEnv {
    fn with_token_config() -> Self {
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
        let append = format!(
            r#"
[git]
anonymous_access = true
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "{TOKEN_NAME}"
token = "{PUSH_TOKEN}"
paths = ["/project"]
"#
        );
        common::write_full_config_with_append(&full_config_path, &append);

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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
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
        let child = command.spawn().expect("spawn monoengine service");
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
        git_cli::monoengine_host_http_url(port, "/project").trim_end_matches('/')
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
