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
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement, Value};
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
            .env("MEGA_VIEWS__WORKER_INTERVAL_SECS", "3600")
            .env("MEGA_VIEWS__MAX_CONCURRENT_COLD_STARTS", "1")
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
        self.raw_chunked_response(path)
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }

    fn raw_chunked_response(&self, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nzz\r\n",
            self.port
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn execute(&self, statement: Statement) {
        let url = self._database.url.clone();
        with_runtime(async move {
            let db = Database::connect(&url).await.unwrap();
            db.execute_raw(statement).await.unwrap();
        });
    }

    fn register(&self, name: &str, spec: &str) -> i64 {
        let response = self
            .client
            .post(format!(
                "http://127.0.0.1:{}/api/v1/views?wait=true",
                self.port
            ))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({"name": name, "filter_spec": spec}))
            .send()
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().unwrap();
        assert!(status.is_success(), "register {spec}: {status} {body}");
        assert_eq!(body["data"]["ready"], true, "{body}");
        let filter_id = body["data"]["filter_id"].as_str().unwrap().to_owned();
        let url = self._database.url.clone();
        with_runtime(async move {
            let db = Database::connect(&url).await.unwrap();
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT id FROM mega_view_filter WHERE filter_id = $1",
                    [Value::from(filter_id)],
                ))
                .await
                .unwrap()
                .unwrap();
            row.try_get("", "id").unwrap()
        })
    }

    fn set_ready(&self, pk: i64, ready: Option<i64>, warming: bool) {
        self.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE mega_view_filter SET ready_seq = $1, warming_since = \
             CASE WHEN $2 THEN now() ELSE NULL END WHERE id = $3",
            [Value::from(ready), Value::from(warming), Value::from(pk)],
        ));
    }

    fn filter_state(&self, pk: i64) -> (Option<i64>, i64, bool) {
        let url = self._database.url.clone();
        with_runtime(async move {
            let db = Database::connect(&url).await.unwrap();
            let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT ready_seq, projected_seq, warming_since IS NOT NULL AS warming FROM mega_view_filter WHERE id = $1",
                [Value::from(pk)])).await.unwrap().unwrap();
            (
                row.try_get("", "ready_seq").unwrap(),
                row.try_get("", "projected_seq").unwrap(),
                row.try_get("", "warming").unwrap(),
            )
        })
    }

    fn wait_idle(&self, pks: &[i64]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut previous = None;
        loop {
            let current: Vec<_> = pks.iter().map(|pk| self.filter_state(*pk)).collect();
            if current.iter().all(|state| state.0.is_some()) && previous.as_ref() == Some(&current)
            {
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

    fn recycle(&self, pk: i64) {
        for table in ["mega_view_commit_map", "mega_view_object_ref"] {
            self.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                format!("DELETE FROM {table} WHERE filter_pk = $1"),
                [Value::from(pk)],
            ));
        }
        self.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
            "UPDATE mega_view_filter SET ready_seq = NULL, warming_since = NULL, projected_seq = 0 WHERE id = $1",
            [Value::from(pk)]));
    }

    fn clear_root_halt(&self) {
        self.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM mega_view_root_chain_scan WHERE commit_id = $1",
            [Value::from("e".repeat(40))],
        ));
    }

    fn halt_root_chain(&self) {
        self.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO mega_view_root_chain_scan \
             (pos, commit_id, tree_id, parent_count, first_parent) \
             SELECT COALESCE(MAX(pos), 0) + 1, $1, \
             (SELECT ref_tree_hash FROM mega_refs WHERE path = '/' AND ref_name = 'refs/heads/main'), \
             2, NULL FROM mega_view_root_chain_scan",
            [Value::from("e".repeat(40))],
        ));
    }

    fn response(
        &self,
        method: Method,
        path: &str,
        v2: bool,
        token: bool,
        body: Option<&[u8]>,
    ) -> reqwest::blocking::Response {
        let mut request = self
            .client
            .request(method, format!("http://127.0.0.1:{}{path}", self.port));
        if v2 {
            request = request.header("Git-Protocol", "version=2");
        }
        if token {
            request = request.bearer_auth(TOKEN);
        }
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }
        request.send().unwrap()
    }
}

fn assert_retry(response: reqwest::blocking::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u32>()
            .unwrap()
            > 0
    );
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

#[test]
fn layer3_unknown_not_found() {
    let case = Case::boot(true, "token", true);
    case.register("known", ":/project");
    for prefix in [
        "/.view/missing.git".to_owned(),
        "/.view/known@9.git".to_owned(),
        format!("/.filter/{}.git", "a".repeat(64)),
    ] {
        for v2 in [false, true] {
            assert_eq!(
                case.response(
                    Method::GET,
                    &format!("{prefix}/info/refs?service=git-upload-pack"),
                    v2,
                    false,
                    None,
                )
                .status(),
                StatusCode::NOT_FOUND
            );
        }
        assert_eq!(
            case.response(
                Method::POST,
                &format!("{prefix}/git-upload-pack"),
                false,
                false,
                Some(b"0000"),
            )
            .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[test]
fn layer3_unready_first_advertise() {
    let case = Case::boot(true, "token", true);
    let ready = case.register("ready", ":/project");
    let cold = case.register("cold", ":/project:prefix=cold");
    let recycled = case.register("recycled", ":/project:prefix=recycled");
    case.wait_idle(&[ready, cold, recycled]);
    case.set_ready(cold, None, true);
    case.recycle(recycled);
    for name in ["cold", "recycled"] {
        for v2 in [false, true] {
            assert_retry(case.response(
                Method::GET,
                &format!("/.view/{name}.git/info/refs?service=git-upload-pack"),
                v2,
                false,
                None,
            ));
            assert_eq!(
                case.response(
                    Method::GET,
                    "/.view/ready.git/info/refs?service=git-upload-pack",
                    v2,
                    false,
                    None,
                )
                .status(),
                StatusCode::OK
            );
        }
    }
    case.halt_root_chain();
    for v2 in [false, true] {
        assert_retry(case.response(
            Method::GET,
            "/.view/ready.git/info/refs?service=git-upload-pack",
            v2,
            false,
            None,
        ));
    }
    assert_eq!(case.filter_state(cold).0, None);
    assert!(!case.filter_state(recycled).2);
    case.clear_root_halt();
}

#[test]
fn layer3_unready_after_advertise() {
    let case = Case::boot(true, "token", true);
    let ready = case.register("ready", ":/project");
    case.wait_idle(&[ready]);
    let info = "/.view/ready.git/info/refs?service=git-upload-pack";
    let pack = "/.view/ready.git/git-upload-pack";
    for v2 in [false, true] {
        assert_eq!(
            case.response(Method::GET, info, v2, false, None).status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        case.response(
            Method::POST,
            pack,
            true,
            false,
            Some(b"0014command=ls-refs\n0001000csymrefs\n0000"),
        )
        .status(),
        StatusCode::OK
    );
    case.set_ready(ready, None, false);
    for body in [
        b"0014command=ls-refs\n0001000csymrefs\n0000".as_slice(),
        b"0012command=fetch\n00010032want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n0009done\n0000"
            .as_slice(),
        b"0012command=fetch\n00010032want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n0000"
            .as_slice(),
    ] {
        assert_retry(case.response(Method::POST, pack, true, false, Some(body)));
    }
    assert_retry(case.response(
        Method::POST,
        pack,
        false,
        false,
        Some(b"0032want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n0000"),
    ));
    assert_eq!(case.filter_state(ready).0, None);
}

#[test]
fn layer3_before_resolve_auth() {
    let case = Case::boot(true, "token", false);
    let ready = case.register("ready", ":/project");
    let cold = case.register("cold", ":/project:prefix=cold");
    case.wait_idle(&[ready, cold]);
    case.set_ready(cold, None, true);
    let prefixes = [
        "/.view/ready.git",
        "/.view/cold.git",
        "/.view/missing.git",
        "/project.git",
    ];
    let mut baseline = Vec::new();
    for v2 in [false, true] {
        let mut responses = Vec::new();
        for prefix in prefixes {
            let response = case.response(
                Method::GET,
                &format!("{prefix}/info/refs?service=git-upload-pack"),
                v2,
                false,
                None,
            );
            let status = response.status();
            let auth = response.headers().get("www-authenticate").cloned();
            let body = response.bytes().unwrap().to_vec();
            responses.push((status, auth, body));
        }
        assert!(responses.iter().all(|response| response == &responses[0]));
        baseline.extend(responses);
    }
    let mut responses = Vec::new();
    for prefix in prefixes {
        let response = case.response(
            Method::POST,
            &format!("{prefix}/git-upload-pack"),
            false,
            false,
            Some(b"0000"),
        );
        let status = response.status();
        let auth = response.headers().get("www-authenticate").cloned();
        let body = response.bytes().unwrap().to_vec();
        responses.push((status, auth, body));
    }
    assert!(responses.iter().all(|response| response == &responses[0]));
    baseline.extend(responses);
    assert_eq!(
        case.response(
            Method::GET,
            "/.view/ready.git/info/refs?service=git-upload-pack",
            false,
            true,
            None,
        )
        .status(),
        StatusCode::OK
    );
    case.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        "ALTER TABLE mega_view_filter RENAME COLUMN canonical_spec TO hp_gone".to_owned(),
    ));
    for (index, (v2, method)) in [
        (false, Method::GET),
        (true, Method::GET),
        (false, Method::POST),
    ]
    .into_iter()
    .enumerate()
    {
        for (offset, prefix) in prefixes.into_iter().enumerate() {
            let path = if method == Method::GET {
                format!("{prefix}/info/refs?service=git-upload-pack")
            } else {
                format!("{prefix}/git-upload-pack")
            };
            let request_body = (method == Method::POST).then_some(b"0000".as_slice());
            let response = case.response(method.clone(), &path, v2, false, request_body);
            let status = response.status();
            let auth = response.headers().get("www-authenticate").cloned();
            let body = response.bytes().unwrap().to_vec();
            assert_eq!((status, auth, body), baseline[index * 4 + offset]);
        }
    }
    case.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        "ALTER TABLE mega_view_filter RENAME COLUMN hp_gone TO canonical_spec".to_owned(),
    ));
    assert_eq!(case.filter_state(cold).0, None);
}

#[test]
fn layer3_before_body() {
    let case = Case::boot(true, "token", true);
    let cold = case.register("cold", ":/project");
    case.wait_idle(&[cold]);
    case.set_ready(cold, None, true);
    assert_eq!(
        case.raw_chunked_status("/.view/missing.git/git-upload-pack"),
        404
    );
    let response = case.raw_chunked_response("/.view/cold.git/git-upload-pack");
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    let retry = response
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("retry-after: ")
                .map(str::to_owned)
        })
        .unwrap();
    assert!(retry.trim().parse::<u32>().unwrap() > 0);
    assert_eq!(case.raw_chunked_status("/project.git/git-upload-pack"), 400);
    assert_eq!(case.filter_state(cold).0, None);
}
