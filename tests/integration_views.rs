mod common;

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        ObjectTrait,
        blob::Blob,
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
        types::ObjectType,
    },
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
        Self::boot_with_env(enabled, push_auth, anonymous_access, &[])
    }

    fn boot_with_env(
        enabled: bool,
        push_auth: &str,
        anonymous_access: bool,
        extra_env: &[(&str, &str)],
    ) -> Self {
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
        for (key, value) in extra_env {
            init.env(key, value);
        }
        let result = init.args(["service", "init", "--yes"]).output().unwrap();
        assert!(
            result.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = temp.path().join("service.out");
        let stderr = temp.path().join("service.err");
        let mut command = Self::command(&temp, &database, &config_path, enabled, port);
        for (key, value) in extra_env {
            command.env(key, value);
        }
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

    fn stop_service(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }

    fn restart_rewarm(
        &mut self,
        enabled: bool,
        anonymous_access: bool,
        max_filters: u64,
        cold_slots: u64,
    ) {
        self.stop_service();
        let config_path = self._temp.path().join("config.toml");
        let git = format!(
            "[git]\nanonymous_access = {anonymous_access}\npush_auth = \"none\"\nssh_receive_pack = false\n"
        );
        common::write_full_config_with_append(&config_path, &git);
        let stdout = self._temp.path().join("rewarm-service.out");
        let stderr = self._temp.path().join("rewarm-service.err");
        let mut command = Self::command(
            &self._temp,
            &self._database,
            &config_path,
            enabled,
            self.port,
        );
        command
            .env("MEGA_VIEWS__ALLOW_ANONYMOUS_REGISTER", "true")
            .env("MEGA_VIEWS__WORKER_INTERVAL_SECS", "1")
            .env("MEGA_VIEWS__MAX_FILTERS", max_filters.to_string())
            .env(
                "MEGA_VIEWS__MAX_CONCURRENT_COLD_STARTS",
                cold_slots.to_string(),
            );
        self.child = command
            .args([
                "service",
                "http",
                "--host",
                "127.0.0.1",
                "-p",
                &self.port.to_string(),
            ])
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                break;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "service exited {status}: {}",
                    fs::read_to_string(&stderr).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "service restart timed out: {}",
                fs::read_to_string(&stderr).unwrap_or_default()
            );
            sleep(Duration::from_millis(100));
        }
    }

    fn register_filter(&self, spec: &str) -> (i64, String) {
        let response = self
            .client
            .post(format!("http://127.0.0.1:{}/api/v1/views", self.port))
            .json(&serde_json::json!({"filter_spec": spec}))
            .send()
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().unwrap();
        assert!(status.is_success(), "register {spec}: {status} {body}");
        let filter_id = body["data"]["filter_id"].as_str().unwrap().to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = self
                .client
                .get(format!(
                    "http://127.0.0.1:{}/api/v1/views/{filter_id}",
                    self.port
                ))
                .send()
                .unwrap();
            assert_eq!(status.status(), StatusCode::OK);
            let details: serde_json::Value = status.json().unwrap();
            if details["data"]["ready"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "view did not become ready: {filter_id}"
            );
            sleep(Duration::from_millis(500));
        }
        let url = self._database.url.clone();
        let id = filter_id.clone();
        let pk = with_runtime(async move {
            let db = Database::connect(&url).await.unwrap();
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT id FROM mega_view_filter WHERE filter_id = $1",
                    [Value::from(id)],
                ))
                .await
                .unwrap()
                .unwrap();
            row.try_get("", "id").unwrap()
        });
        (pk, filter_id)
    }

    fn table_rows(&self, table: &str) -> Vec<String> {
        let url = self._database.url.clone();
        let table = table.to_owned();
        with_runtime(async move {
            let db = Database::connect(&url).await.unwrap();
            db.query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT to_jsonb(t)::text AS row FROM {table} t ORDER BY id"),
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get("", "row").unwrap())
            .collect()
        })
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

struct PrecheckHttpFixture {
    case: Case,
    root_tip: String,
    prior_root: String,
    views: HashMap<&'static str, (i64, String, Vec<String>)>,
    mode_tree: String,
    gbk_tree: String,
    missing_tree: String,
    missing_tree_refs: usize,
}

fn hp23_tree(items: Vec<TreeItem>, trees: &mut Vec<Tree>) -> Tree {
    let tree = Tree::from_tree_items_with_kind(HashKind::Sha1, items).unwrap();
    trees.push(tree.clone());
    tree
}

fn hp23_blob(content: &str) -> ObjectHash {
    Blob::from_content_bytes_with_kind(HashKind::Sha1, content.as_bytes().to_vec())
        .unwrap()
        .id
}

fn hp23_raw_l0_tree(mode: &str, name: &[u8], blob: ObjectHash) -> (ObjectHash, Vec<u8>) {
    let mut raw = format!("{mode} ").into_bytes();
    raw.extend_from_slice(name);
    raw.push(0);
    raw.extend(hex::decode(blob.to_string()).unwrap());
    let id =
        ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, &raw).unwrap();
    let stored = <Tree as ObjectTrait>::from_bytes(&raw, id)
        .unwrap()
        .to_data()
        .unwrap();
    assert_ne!(
        ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, &stored).unwrap(),
        id
    );
    (id, stored)
}

fn hp23_pkt(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len() + 4).into_bytes()
}

fn hp23_fetch_request(v2: bool, want: &str, have: Option<&str>, done: bool) -> Vec<u8> {
    let mut body = Vec::new();
    if v2 {
        body.extend(hp23_pkt("command=fetch\n"));
        body.extend(b"0001");
        body.extend(hp23_pkt(&format!("want {want}\n")));
    } else {
        let capability = if have.is_some() {
            " multi_ack_detailed"
        } else {
            ""
        };
        body.extend(hp23_pkt(&format!("want {want}{capability}\n")));
    }
    if let Some(have) = have {
        body.extend(hp23_pkt(&format!("have {have}\n")));
    }
    if v2 && done {
        body.extend(hp23_pkt("done\n"));
    }
    body.extend(b"0000");
    body
}

impl PrecheckHttpFixture {
    fn new() -> Self {
        let case = Case::boot_with_env(
            true,
            "none",
            true,
            &[("MEGA_VIEWS__ALLOW_ANONYMOUS_REGISTER", "true")],
        );
        let db_url = case._database.url.clone();
        let (root_tip, prior_root, mode_tree, gbk_tree, missing_tree, missing_tree_refs) =
            with_runtime(async {
                let db = Database::connect(&db_url).await.unwrap();
                let row = db
                    .query_one_raw(Statement::from_string(
                        DatabaseBackend::Postgres,
                        "SELECT ref_commit_hash FROM mega_refs \
                     WHERE path = '/' AND ref_name = 'refs/heads/main' AND NOT is_cl"
                            .to_owned(),
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                let r0: String = row.try_get("", "ref_commit_hash").unwrap();
                let r0_hash = ObjectHash::from_hex_for_kind(HashKind::Sha1, &r0).unwrap();
                let mut trees = Vec::new();
                let mode_good = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Blob,
                        hp23_blob("mode good"),
                        "good.txt".to_owned(),
                    )],
                    &mut trees,
                );
                let gbk_good = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Blob,
                        hp23_blob("gbk good"),
                        "good.txt".to_owned(),
                    )],
                    &mut trees,
                );
                let missing = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Blob,
                        hp23_blob("missing only"),
                        "hp23-only-in-miss-d.txt".to_owned(),
                    )],
                    &mut trees,
                );
                let miss = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Tree,
                        missing.id,
                        "d".to_owned(),
                    )],
                    &mut trees,
                );
                let ok = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Blob,
                        hp23_blob("ok good"),
                        "ok.txt".to_owned(),
                    )],
                    &mut trees,
                );
                let hp23_r1 = hp23_tree(
                    vec![
                        TreeItem::new(TreeItemMode::Tree, gbk_good.id, "gbk".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, miss.id, "miss".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, mode_good.id, "mode".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, ok.id, "ok".to_owned()),
                    ],
                    &mut trees,
                );
                let root_r1 = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Tree,
                        hp23_r1.id,
                        "hp23".to_owned(),
                    )],
                    &mut trees,
                );
                let (mode_id, mode_stored) =
                    hp23_raw_l0_tree("100664", b"mode.txt", hp23_blob("mode raw"));
                let (gbk_id, gbk_stored) = hp23_raw_l0_tree(
                    "100644",
                    &[0xc4, 0xe3, 0xba, 0xc3, b'.', b't', b'x', b't'],
                    hp23_blob("gbk raw"),
                );
                let hp23_r2 = hp23_tree(
                    vec![
                        TreeItem::new(TreeItemMode::Tree, gbk_id, "gbk".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, miss.id, "miss".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, mode_id, "mode".to_owned()),
                        TreeItem::new(TreeItemMode::Tree, ok.id, "ok".to_owned()),
                    ],
                    &mut trees,
                );
                let root_r2 = hp23_tree(
                    vec![TreeItem::new(
                        TreeItemMode::Tree,
                        hp23_r2.id,
                        "hp23".to_owned(),
                    )],
                    &mut trees,
                );
                let r1 = Commit::from_tree_id_with_kind(
                    HashKind::Sha1,
                    root_r1.id,
                    vec![r0_hash],
                    "hp23 R1",
                )
                .unwrap();
                let r2 = Commit::from_tree_id_with_kind(
                    HashKind::Sha1,
                    root_r2.id,
                    vec![r1.id],
                    "hp23 R2",
                )
                .unwrap();
                let missing_tree_refs = trees
                    .iter()
                    .flat_map(|tree| &tree.tree_items)
                    .filter(|item| item.id == missing.id)
                    .count();
                let mut next_id = 8_000_000_i64;
                for (tree_id, bytes) in trees
                    .into_iter()
                    .map(|tree| (tree.id, tree.to_data().unwrap()))
                    .chain([(mode_id, mode_stored), (gbk_id, gbk_stored)])
                {
                    db.execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "INSERT INTO mega_tree \
                     (id, tree_id, sub_trees, size, created_at, pack_id, pack_offset, commit_id) \
                     VALUES ($1, $2, $3, 0, now(), '', 0, $4) \
                     ON CONFLICT (tree_id) DO NOTHING",
                        [
                            Value::from(next_id),
                            Value::from(tree_id.to_string()),
                            Value::from(bytes),
                            Value::from(r2.id.to_string()),
                        ],
                    ))
                    .await
                    .unwrap();
                    next_id += 1;
                }
                for commit in [&r1, &r2] {
                    db.execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "INSERT INTO mega_commit \
                     (id, commit_id, tree, parents_id, author, committer, content, \
                      created_at, pack_id, pack_offset) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, now(), '', 0)",
                        [
                            Value::from(next_id),
                            Value::from(commit.id.to_string()),
                            Value::from(commit.tree_id.to_string()),
                            Value::from(serde_json::json!(
                                commit
                                    .parent_commit_ids
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                            )),
                            Value::from(
                                String::from_utf8(commit.author.to_data().unwrap()).unwrap(),
                            ),
                            Value::from(
                                String::from_utf8(commit.committer.to_data().unwrap()).unwrap(),
                            ),
                            Value::from(commit.message.clone()),
                        ],
                    ))
                    .await
                    .unwrap();
                    next_id += 1;
                }
                let result = db
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "UPDATE mega_refs SET ref_commit_hash = $1, ref_tree_hash = $2, \
                     updated_at = now() WHERE path = '/' AND ref_name = 'refs/heads/main' \
                     AND NOT is_cl AND ref_commit_hash = $3",
                        [
                            Value::from(r2.id.to_string()),
                            Value::from(root_r2.id.to_string()),
                            Value::from(r0),
                        ],
                    ))
                    .await
                    .unwrap();
                assert_eq!(result.rows_affected(), 1);
                (
                    r2.id.to_string(),
                    r1.id.to_string(),
                    mode_id.to_string(),
                    gbk_id.to_string(),
                    missing.id.to_string(),
                    missing_tree_refs,
                )
            });
        let mut views = HashMap::new();
        for (name, spec) in [
            ("mode", ":/hp23/mode"),
            ("gbk", ":/hp23/gbk:prefix=p"),
            ("ok", ":/hp23/ok"),
            ("miss", ":/hp23/miss"),
        ] {
            let (pk, filter_id) = case.register_filter(spec);
            let url = case._database.url.clone();
            let commits = with_runtime(async move {
                let db = Database::connect(&url).await.unwrap();
                db.query_all_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT view_commit FROM mega_view_commit_map \
                     WHERE filter_pk = $1 AND view_commit IS NOT NULL ORDER BY seq_from",
                    [Value::from(pk)],
                ))
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get("", "view_commit").unwrap())
                .collect::<Vec<String>>()
            });
            assert!(!commits.is_empty(), "{name}");
            views.insert(name, (pk, filter_id, commits));
        }
        Self {
            case,
            root_tip,
            prior_root,
            views,
            mode_tree,
            gbk_tree,
            missing_tree,
            missing_tree_refs,
        }
    }

    fn metric(&self) -> u64 {
        let response = self
            .case
            .response(Method::GET, "/api/v1/views/metrics", false, false, None);
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = response.json().unwrap();
        body["data"]["view_pack_tree_mismatch_total"]
            .as_u64()
            .unwrap()
    }

    fn fetch(&self, name: &str, v2: bool, body: &[u8]) -> Vec<u8> {
        let filter_id = &self.views[name].1;
        let response = self.case.response(
            Method::POST,
            &format!("/.filter/{filter_id}.git/git-upload-pack"),
            v2,
            false,
            Some(body),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "application/x-git-upload-pack-result"
        );
        response.bytes().unwrap().to_vec()
    }
}

#[test]
fn view_precheck_http_want_not_ours() {
    let fixture = PrecheckHttpFixture::new();
    let tip = fixture.views["ok"].2.last().unwrap();
    let initial_metric = fixture.metric();
    for (v2, have, done) in [
        (false, None, false),
        (false, Some(tip.as_str()), false),
        (true, None, true),
        (true, Some(fixture.prior_root.as_str()), false),
        (true, Some(tip.as_str()), false),
    ] {
        let body = hp23_fetch_request(v2, &fixture.root_tip, have, done);
        assert_eq!(
            fixture.fetch("ok", v2, &body),
            hp23_pkt(&format!(
                "ERR upload-pack: not our ref {}\n",
                fixture.root_tip
            ))
        );
    }
    let body = hp23_fetch_request(true, tip, Some(&fixture.prior_root), false);
    let mut expected = hp23_pkt("acknowledgments\n");
    expected.extend(hp23_pkt("NAK\n"));
    expected.extend(b"0000");
    assert_eq!(fixture.fetch("ok", true, &body), expected);
    assert_eq!(fixture.metric(), initial_metric);
}

#[test]
fn view_precheck_http_l0_mismatch() {
    let fixture = PrecheckHttpFixture::new();
    for (name, tree_id) in [("mode", &fixture.mode_tree), ("gbk", &fixture.gbk_tree)] {
        let (_, filter_id, commits) = &fixture.views[name];
        assert_eq!(commits.len(), 2);
        let want = &commits[1];
        let have = &commits[0];
        let message = format!(
            "ERR view {filter_id} pack aborted: tree {tree_id} does not match its stored entries\n"
        );
        for (v2, have, done) in [
            (false, None, false),
            (false, Some(have.as_str()), false),
            (true, None, true),
            (true, Some(have.as_str()), false),
        ] {
            let before = fixture.metric();
            let body = hp23_fetch_request(v2, want, have, done);
            assert_eq!(fixture.fetch(name, v2, &body), hp23_pkt(&message));
            assert_eq!(fixture.metric(), before + 1);
            for v2 in [false, true] {
                let response = fixture.case.response(
                    Method::GET,
                    &format!("/.filter/{filter_id}.git/info/refs?service=git-upload-pack"),
                    v2,
                    false,
                    None,
                );
                assert_eq!(response.status(), StatusCode::OK);
            }
        }
        let before = fixture.metric();
        let body = hp23_fetch_request(true, want, Some(&fixture.prior_root), false);
        let mut expected = hp23_pkt("acknowledgments\n");
        expected.extend(hp23_pkt("NAK\n"));
        expected.extend(b"0000");
        assert_eq!(fixture.fetch(name, true, &body), expected);
        assert_eq!(fixture.metric(), before);
        let body = hp23_fetch_request(true, want, Some(&fixture.prior_root), true);
        assert_eq!(fixture.fetch(name, true, &body), hp23_pkt(&message));
        assert_eq!(fixture.metric(), before + 1);
    }
}

#[test]
fn view_precheck_http_missing_tree() {
    let fixture = PrecheckHttpFixture::new();
    assert_eq!(fixture.missing_tree_refs, 1);
    let (_, filter_id, commits) = &fixture.views["miss"];
    let tree_id = &fixture.missing_tree;
    let url = fixture.case._database.url.clone();
    let id = tree_id.clone();
    with_runtime(async move {
        let db = Database::connect(&url).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS count FROM mega_tree WHERE tree_id = $1",
                [Value::from(id.clone())],
            ))
            .await
            .unwrap()
            .unwrap();
        let count: i64 = row.try_get("", "count").unwrap();
        assert_eq!(count, 1);
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM mega_tree WHERE tree_id = $1",
            [Value::from(id)],
        ))
        .await
        .unwrap();
    });
    let metric = fixture.metric();
    let message = format!("ERR view {filter_id} pack aborted: tree {tree_id} is missing\n");
    let tip = commits.last().unwrap();
    for v2 in [false, true] {
        let body = hp23_fetch_request(v2, tip, None, v2);
        assert_eq!(fixture.fetch("miss", v2, &body), hp23_pkt(&message));
        assert_eq!(fixture.metric(), metric);
        let response = fixture.case.response(
            Method::GET,
            &format!("/.filter/{filter_id}.git/info/refs?service=git-upload-pack"),
            v2,
            false,
            None,
        );
        assert_eq!(response.status(), StatusCode::OK);
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

fn rewarm_path(filter_id: &str) -> String {
    format!("/.filter/{filter_id}.git")
}

fn rewarm_request(
    case: &Case,
    filter_id: &str,
    kind: u8,
    token: bool,
) -> reqwest::blocking::Response {
    let path = rewarm_path(filter_id);
    let mut request = match kind {
        0 | 1 => case.client.get(format!(
            "http://127.0.0.1:{}{path}/info/refs?service=git-upload-pack",
            case.port
        )),
        2 => case
            .client
            .post(format!(
                "http://127.0.0.1:{}{path}/git-upload-pack",
                case.port
            ))
            .header("content-type", "application/x-git-upload-pack-request")
            .body(b"0000".to_vec()),
        _ => panic!("unexpected request kind"),
    };
    if kind == 1 {
        request = request.header("Git-Protocol", "version=2");
    }
    if token {
        request = request.bearer_auth(TOKEN);
    }
    request.send().unwrap()
}

fn rewarm_advertised_tip(response: reqwest::blocking::Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().unwrap();
    let locate = |label: &[u8]| {
        body.windows(label.len())
            .position(|window| window == label)
            .map(|offset| {
                let oid = &body[offset - 40..offset];
                assert!(oid.iter().all(u8::is_ascii_hexdigit));
                String::from_utf8(oid.to_vec()).unwrap()
            })
            .expect("advertised ref")
    };
    let head = locate(b" HEAD");
    assert_eq!(head, locate(b" refs/heads/main"));
    assert_ne!(head, "0".repeat(40));
    head
}

#[test]
fn rewarm_on_access_recovers() {
    let mut case = Case::boot(true, "none", true);
    case.restart_rewarm(true, true, 100, 10);
    let filters: Vec<_> = [":/project", ":/doc", ":/release"]
        .iter()
        .map(|spec| case.register_filter(spec))
        .collect();
    let pks: Vec<_> = filters.iter().map(|(pk, _)| *pk).collect();
    case.wait_idle(&pks);
    let original_tips: Vec<_> = filters
        .iter()
        .map(|(_, id)| rewarm_advertised_tip(rewarm_request(&case, id, 0, false)))
        .collect();
    assert!(original_tips.iter().all(|tip| tip.len() == 40));
    case.stop_service();
    for pk in pks {
        case.recycle(pk);
    }
    case.restart_rewarm(true, true, 100, 10);
    let logs_before = case.table_rows("mega_view_register_log");
    for (kind, (_, id)) in filters.iter().enumerate() {
        assert_retry(rewarm_request(&case, id, kind as u8, false));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    for ((_, id), expected) in filters.iter().zip(original_tips) {
        loop {
            let response = rewarm_request(&case, id, 0, false);
            if response.status() == StatusCode::OK {
                assert_eq!(rewarm_advertised_tip(response), expected);
                break;
            }
            assert_retry(response);
            assert!(Instant::now() < deadline, "rewarming did not finish: {id}");
            sleep(Duration::from_secs(1));
        }
    }
    assert_eq!(case.table_rows("mega_view_register_log"), logs_before);
}

#[test]
fn rewarm_rejected_on_access() {
    let mut case = Case::boot(true, "none", true);
    case.restart_rewarm(true, true, 100, 10);
    let (ready_pk, _) = case.register_filter(":/project");
    let (recycled_pk, recycled_id) = case.register_filter(":/doc");
    case.wait_idle(&[ready_pk, recycled_pk]);
    case.stop_service();
    case.recycle(recycled_pk);
    case.restart_rewarm(true, true, 1, 10);
    let before: Vec<_> = ["mega_view_filter", "mega_view", "mega_view_register_log"]
        .iter()
        .map(|table| case.table_rows(table))
        .collect();
    for kind in 0..3 {
        assert_retry(rewarm_request(&case, &recycled_id, kind, false));
    }
    sleep(Duration::from_secs(3));
    let after: Vec<_> = ["mega_view_filter", "mega_view", "mega_view_register_log"]
        .iter()
        .map(|table| case.table_rows(table))
        .collect();
    assert_eq!(before, after);
    assert_eq!(case.filter_state(recycled_pk), (None, 0, false));
}

#[test]
fn rewarm_not_triggered_before_layer3() {
    let mut case = Case::boot(true, "none", true);
    case.restart_rewarm(true, true, 100, 10);
    let (pk, id) = case.register_filter(":/project");
    case.wait_idle(&[pk]);
    case.stop_service();
    case.recycle(pk);
    let before = case.table_rows("mega_view_filter");
    case.restart_rewarm(false, true, 100, 10);
    for kind in 0..3 {
        assert_eq!(
            rewarm_request(&case, &id, kind, false).status(),
            StatusCode::NOT_FOUND
        );
    }
    sleep(Duration::from_secs(3));
    assert_eq!(case.table_rows("mega_view_filter"), before);
    case.restart_rewarm(true, false, 100, 10);
    for kind in 0..3 {
        let expected = if kind == 2 {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(rewarm_request(&case, &id, kind, false).status(), expected);
    }
    sleep(Duration::from_secs(3));
    assert_eq!(case.table_rows("mega_view_filter"), before);
}
