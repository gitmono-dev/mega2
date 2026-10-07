mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use reqwest::{Method, StatusCode, blocking::Client};
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TOKEN: &str = "hp18-root-secret";

struct TestDatabase {
    admin_url: String,
    name: String,
    url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
            "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
        });
        let name = format!(
            "mega2_hp18_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut parsed = url::Url::parse(&admin_url).unwrap();
        parsed.set_path(&name);
        let url = parsed.to_string();
        with_runtime(async {
            let db = Database::connect(&admin_url).await.unwrap();
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("CREATE DATABASE {name}"),
            ))
            .await
            .unwrap();
        });
        Self {
            admin_url,
            name,
            url,
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        with_runtime(async move {
            if let Ok(db) = Database::connect(&admin_url).await {
                let _ = db
                    .execute_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
                    ))
                    .await;
            }
        });
    }
}

fn with_runtime<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

struct Case {
    child: Child,
    _temp: TempDir,
    _database: TestDatabase,
    port: u16,
    client: Client,
}

impl Case {
    fn boot(enabled: bool, push_auth: &str, anonymous_access: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let database = TestDatabase::create();
        let config_path = temp.path().join("config.toml");
        let git = format!(
            "[git]\nanonymous_access = {anonymous_access}\npush_auth = {push_auth:?}\nssh_receive_pack = false\n[[git.push_tokens]]\nname = \"hp18-root\"\ntoken = {TOKEN:?}\npaths = [\"/\"]\n"
        );
        common::write_full_config_with_append(&config_path, &git);
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut init = Self::command(&temp, &database, &config_path, enabled, port);
        let result = init.args(["service", "init", "--yes"]).output().unwrap();
        assert!(
            result.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = temp.path().join("service.out");
        let stderr = temp.path().join("service.err");
        let mut command = Self::command(&temp, &database, &config_path, enabled, port);
        let mut child = command
            .args([
                "service",
                "http",
                "--host",
                "127.0.0.1",
                "-p",
                &port.to_string(),
            ])
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "service exited {status}: {}",
                    fs::read_to_string(&stderr).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "service startup timed out: {}",
                fs::read_to_string(&stderr).unwrap_or_default()
            );
            sleep(Duration::from_millis(100));
        }
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        Self {
            child,
            _temp: temp,
            _database: database,
            port,
            client,
        }
    }

    fn command(
        temp: &TempDir,
        database: &TestDatabase,
        config: &PathBuf,
        enabled: bool,
        port: u16,
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mega2"));
        command
            .current_dir(temp.path())
            .env_clear()
            .env("MEGA_BASE_DIR", temp.path().join("base"))
            .env("MEGA_CACHE_DIR", temp.path().join("cache"))
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_URL", &database.url)
            .env("MEGA_DATABASE__MAX_CONNECTION", "4")
            .env("MEGA_DATABASE__MIN_CONNECTION", "1")
            .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
            .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
            .env("MEGA_DATABASE__SQLX_LOGGING", "false")
            .env(
                "MEGA_REDIS__URL",
                std::env::var("MEGA_REDIS__URL")
                    .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_owned()),
            )
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env(
                "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
                temp.path().join("objects"),
            )
            .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
            .env("MEGA_CEDAR__ENFORCEMENT", "off")
            .env("MEGA_VIEWS__ENABLED", enabled.to_string())
            .env(
                "MEGA_HTTP__PUBLIC_BASE_URL",
                format!("http://127.0.0.1:{port}"),
            )
            .env("MEGA_LOG__PRINT_STD", "false")
            .arg("--config")
            .arg(config);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
    }

    fn status(&self, method: Method, path: &str, token: bool, body: Option<&str>) -> StatusCode {
        let mut request = self
            .client
            .request(method, format!("http://127.0.0.1:{}{path}", self.port));
        if token {
            request = request.bearer_auth(TOKEN);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_owned());
        }
        request.send().unwrap().status()
    }

    fn raw_chunked_status(&self, path: &str) -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nbogus\r\n",
            self.port
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn layer2_disabled_not_found() {
    let case = Case::boot(false, "token", false);
    let filter_id = "a".repeat(64);
    for prefix in [
        "/.view/x.git".to_owned(),
        "/.view/x@2.git".to_owned(),
        format!("/.filter/{filter_id}.git"),
    ] {
        for version in [false, true] {
            let path = format!("{prefix}/info/refs?service=git-upload-pack");
            let mut request = case
                .client
                .get(format!("http://127.0.0.1:{}{path}", case.port));
            if version {
                request = request.header("Git-Protocol", "version=2");
            }
            assert_eq!(request.send().unwrap().status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(
            case.status(
                Method::GET,
                &format!("{prefix}/info/refs?service=git-receive-pack"),
                false,
                None
            ),
            StatusCode::NOT_FOUND
        );
        for endpoint in ["git-upload-pack", "git-receive-pack"] {
            assert_eq!(
                case.status(
                    Method::POST,
                    &format!("{prefix}/{endpoint}"),
                    false,
                    Some("")
                ),
                StatusCode::NOT_FOUND
            );
        }
    }
    for prefix in ["/.filter/ABC.git", "/.view/a@0.git"] {
        assert_eq!(
            case.status(
                Method::GET,
                &format!("{prefix}/info/refs?service=git-upload-pack"),
                false,
                None
            ),
            StatusCode::NOT_FOUND
        );
    }
}

#[test]
fn layer2_enabled_unregistered_not_found() {
    let case = Case::boot(true, "none", true);
    let filter_id = "a".repeat(64);
    for prefix in [
        "/.view/x.git".to_owned(),
        "/.view/x@2.git".to_owned(),
        format!("/.filter/{filter_id}.git"),
    ] {
        for version in [false, true] {
            let path = format!("{prefix}/info/refs?service=git-upload-pack");
            let mut request = case
                .client
                .get(format!("http://127.0.0.1:{}{path}", case.port));
            if version {
                request = request.header("Git-Protocol", "version=2");
            }
            assert_eq!(request.send().unwrap().status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(
            case.status(
                Method::POST,
                &format!("{prefix}/git-upload-pack"),
                false,
                Some("")
            ),
            StatusCode::NOT_FOUND
        );
    }
    for prefix in ["/.filter/ABC.git", "/.view/a@0.git"] {
        assert_eq!(
            case.status(
                Method::GET,
                &format!("{prefix}/info/refs?service=git-upload-pack"),
                false,
                None
            ),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        case.status(
            Method::GET,
            "/info/refs?service=git-upload-pack",
            false,
            None
        ),
        StatusCode::OK
    );
}

#[test]
fn layer2_receive_pack_forbidden_before_auth_and_body() {
    let case = Case::boot(true, "token", false);
    let filter_id = "a".repeat(64);
    for prefix in [
        "/.view/x.git".to_owned(),
        format!("/.filter/{filter_id}.git"),
    ] {
        let path = format!("{prefix}/info/refs?service=git-receive-pack");
        for token in [false, true] {
            assert_eq!(
                case.status(Method::GET, &path, token, None),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            case.raw_chunked_status(&format!("{prefix}/git-receive-pack")),
            403
        );
    }
}

#[test]
fn layer2_lfs_not_found() {
    let filter_id = "a".repeat(64);
    for push_auth in ["none", "token"] {
        for enabled in [false, true] {
            let case = Case::boot(enabled, push_auth, true);
            let token = push_auth == "token";
            for prefix in [
                "/.view/x.git".to_owned(),
                format!("/.filter/{filter_id}.git"),
            ] {
                let lfs = format!("{prefix}/info/lfs");
                for operation in ["upload", "download"] {
                    assert_eq!(
                        case.status(
                            Method::POST,
                            &format!("{lfs}/objects/batch"),
                            token,
                            Some(&format!("{{\"operation\":\"{operation}\",\"objects\":[]}}"))
                        ),
                        StatusCode::NOT_FOUND
                    );
                }
                for (method, path) in [
                    (Method::PUT, format!("{lfs}/objects/{}", "a".repeat(64))),
                    (Method::POST, format!("{lfs}/locks")),
                    (Method::GET, format!("{lfs}/locks")),
                    (Method::POST, format!("{lfs}/locks/verify")),
                    (Method::GET, format!("{lfs}/libra/media/v1/capabilities")),
                    (Method::POST, format!("{lfs}/libra/media/v1/manifests")),
                ] {
                    assert_eq!(
                        case.status(method, &path, token, Some("")),
                        StatusCode::NOT_FOUND
                    );
                }
            }
            assert_eq!(
                case.status(
                    Method::POST,
                    "/.view.git/info/lfs/objects/batch",
                    token,
                    Some("")
                ),
                StatusCode::NOT_FOUND
            );
            assert_ne!(
                case.status(
                    Method::POST,
                    "/info/lfs/objects/batch",
                    false,
                    Some("{\"operation\":\"download\",\"objects\":[]}")
                ),
                StatusCode::NOT_FOUND
            );
        }
    }
}
