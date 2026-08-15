// UN-30：只读读取面的「零写入」黑盒前后比对。
//
// 一条审计命令的全部价值在于它能说「我什么都没改」。这里不靠阅读代码来相信这句话，而是
// 用一份真实的数据库前后对照来证明：
//
//  1. 先用**真实二进制**（`monoengine service http`）把库播种成生产形态——迁移、
//     `init_monorepo()` 写下的 refs 与对象、默认 sidebar 全都在；
//  2. 拍下快照：schema（表 + 列）、每张表的行数、refs 全行、对象三张表全行，以及本地对象
//     存储目录的内容散列；
//  3. 跑只读装配并真的读几样东西；
//  4. 再拍一次，逐项比对。
//
// 播种走真实二进制而不是在测试里手搓状态：手搓出来的库只包含我们想到的东西，而「零写入」
// 恰恰是关于没想到的那些。

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{
    ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement, TryGetable,
};

mod common;

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------- object store

/// 与 `bin/src/main.rs` 里注册的是同一个 provider。
///
/// 只读装配要真的构造对象存储，因此测试进程必须像真实二进制那样注册它——换一个替身就等于
/// 在测另一套装配。
struct OrbitObjectStorageProvider;

#[async_trait::async_trait]
impl monoengine_core::ObjectStorageProvider for OrbitObjectStorageProvider {
    async fn build(
        &self,
        cfg: &orbit_api::factory::ObjectStorageConfig,
    ) -> Result<orbit_api::factory::MegaObjectStorageWrapper, monoengine_core::MegaError> {
        orbit::factory::ObjectStorageFactory::build(cfg)
            .await
            .map_err(Into::into)
    }
}

// ---------------------------------------------------------------- environment

fn integration_postgres_url() -> String {
    std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string())
}

fn integration_redis_url() -> String {
    std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string())
}

fn database_url_for_name(admin_url: &str, name: &str) -> String {
    let mut url = url::Url::parse(admin_url).expect("admin url");
    url.set_path(name);
    url.to_string()
}

fn with_runtime<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
        .block_on(future)
}

async fn execute_postgres(db: &DatabaseConnection, sql: String) {
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .expect("execute admin statement");
}

struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        let db_name = format!(
            "monoengine_audit_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);

        with_runtime(async {
            let db = Database::connect(admin_url.as_str()).await.unwrap_or_else(|_| {
                panic!(
                    "integration PostgreSQL is not available; run `docker compose -f docker-compose.test.yml up -d` first"
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
            let _ = db
                .execute_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                         WHERE datname = '{db_name}'"
                    ),
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

// ---------------------------------------------------------------- the service

fn reserve_free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve free port");
    listener.local_addr().expect("local addr").port()
}

struct ServiceProcess {
    child: Child,
    reaped: bool,
}

impl ServiceProcess {
    fn spawn(mut command: Command) -> Self {
        Self {
            child: command.spawn().expect("spawn monoengine service"),
            reaped: false,
        }
    }

    fn wait_until_listening(&mut self, port: u16, timeout: Duration, logs: &[&Path]) {
        let deadline = Instant::now() + timeout;
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                // 端口通了还要确认是**这个**子进程在听：端口是先绑后放拿到的，理论上可能被
                // 别人抢走，那样「播种完成」就是个假象。
                if let Some(status) = self.child.try_wait().expect("poll service") {
                    self.reaped = true;
                    panic!(
                        "port {port} accepted a connection but the service had already exited \
                         (status {status}):\n{}",
                        read_logs(logs)
                    );
                }
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                panic!(
                    "service exited before binding port {port} (status {status}):\n{}",
                    read_logs(logs)
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "service did not bind port {port} within {timeout:?}:\n{}",
                    read_logs(logs)
                );
            }
            sleep(Duration::from_millis(200));
        }
    }

    fn shutdown(&mut self, timeout: Duration) -> ExitStatus {
        // SAFETY: 向运行中的子进程发送 SIGINT；CLI 的 ctrl-c handler 会干净退出。
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGINT);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.reaped = true;
                panic!("service did not exit within {timeout:?} after SIGINT");
            }
            sleep(Duration::from_millis(200));
        }
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

fn read_logs(paths: &[&Path]) -> String {
    paths
        .iter()
        .map(|path| {
            let mut body = String::new();
            if let Ok(mut file) = fs::File::open(path) {
                let _ = file.read_to_string(&mut body);
            }
            format!("--- {}\n{body}", path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- snapshots

/// 一次拍摄里的四个面，分开存放，这样比对失败时能直接说出是哪一面变了。
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    schema: Vec<String>,
    /// 每张表的**内容**摘要，不是行数。
    ///
    /// 只数行数会漏掉原地改写：一次 UPDATE 不改变计数。这里把整行强转成文本再排序聚合，
    /// 任意一列的任意一次改动都会让摘要变。
    table_digests: BTreeMap<String, String>,
    /// 序列的当前值。回滚掉的插入不留行，却会推进序列——那也是一次写。
    sequences: BTreeMap<String, String>,
    refs: Vec<String>,
    objects: Vec<String>,
    object_files: BTreeMap<String, String>,
    /// MEGA_BASE_DIR 与 MEGA_CACHE_DIR 下的逐文件内容散列（UN-43）。
    ///
    /// Vault 的 core key 文件就在这里面：只读装配若走了 bootstrap 路径，它会被重写
    /// （轮换 runtime 凭据后回写），这一面就会变。
    filesystem: BTreeMap<String, String>,
}

/// 受限根目录产物的排除项（UN-43）。
///
/// 只读命令允许在**登记过的**受限根目录下写产物（run 目录等，由 UN-32 引入）；除此之外
/// 文件系统必须一字未动。今天还没有这样的目录，所以这里是空的——留着是因为「排除了什么」
/// 必须是一份明写的清单，而不是某处 diff 里悄悄少掉的几行。
const RESTRICTED_ROOTS: &[&str] = &[];

async fn query_column<T: TryGetable>(db: &DatabaseConnection, sql: &str, column: &str) -> Vec<T> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("query failed: {sql}: {e}"))
    .into_iter()
    .map(|row| row.try_get::<T>("", column).expect("column"))
    .collect()
}

async fn snapshot(db: &DatabaseConnection, object_root: &Path, watched: &[&Path]) -> Snapshot {
    // Schema：表名 + 列名 + 类型。一个多出来的列或一次类型变更都会在这里现形。
    let schema: Vec<String> = query_column(
        db,
        "SELECT table_schema || '.' || table_name || '.' || column_name || ':' || data_type \
         AS entry FROM information_schema.columns \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         AND table_schema NOT LIKE 'pg_toast%' \
         ORDER BY table_schema, table_name, column_name",
        "entry",
    )
    .await;

    // 不限于 public：只排除系统 schema。只读装配如果在别处留下东西，那也是一次写。
    let tables: Vec<String> = query_column(
        db,
        "SELECT table_schema || '.' || table_name AS name FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         AND table_schema NOT LIKE 'pg_toast%' AND table_type = 'BASE TABLE' \
         ORDER BY table_schema, table_name",
        "name",
    )
    .await;

    let mut table_digests = BTreeMap::new();
    for qualified in &tables {
        let (schema_name, table_name) = qualified
            .split_once('.')
            .expect("qualified name from information_schema");
        let digests: Vec<String> = query_column(
            db,
            &format!(
                "SELECT coalesce(md5(string_agg(t::text, '|' ORDER BY t::text)), 'empty') \
                 AS digest FROM \"{schema_name}\".\"{table_name}\" t"
            ),
            "digest",
        )
        .await;
        table_digests.insert(qualified.clone(), digests[0].clone());
    }

    // 序列也拍下来：一次被回滚的插入不留下行，却会推进序列，而那同样是一次写。
    let sequence_rows: Vec<String> = query_column(
        db,
        "SELECT schemaname || '.' || sequencename || '=' || coalesce(last_value::text, 'null') \
         AS entry FROM pg_sequences ORDER BY schemaname, sequencename",
        "entry",
    )
    .await;
    let sequences: BTreeMap<String, String> = sequence_rows
        .into_iter()
        .filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(name, value)| (name.to_string(), value.to_string()))
        })
        .collect();

    // refs 与对象各自全行拍摄，而不是只数行数：一次原地改写不会改变计数。
    let refs: Vec<String> = query_column(
        db,
        "SELECT ref_name || ' ' || ref_commit_hash || ' ' || ref_tree_hash AS entry \
         FROM mega_refs ORDER BY id",
        "entry",
    )
    .await;

    let mut objects: Vec<String> = Vec::new();
    for (table, column) in [
        ("mega_commit", "commit_id"),
        ("mega_tree", "tree_id"),
        ("mega_blob", "blob_id"),
    ] {
        let ids: Vec<String> = query_column(
            db,
            &format!("SELECT '{table}:' || {column} AS entry FROM {table} ORDER BY {column}"),
            "entry",
        )
        .await;
        objects.extend(ids);
    }

    Snapshot {
        schema,
        table_digests,
        sequences,
        refs,
        objects,
        object_files: hash_tree(object_root),
        filesystem: watched
            .iter()
            .flat_map(|root| {
                hash_tree(root)
                    .into_iter()
                    // 排除判定用的是**相对于受监视根**的路径，因为受限根目录也是这样登记的。
                    // 拿拼好的绝对路径去比，前缀永远对不上，排除清单会变成一个从不生效的
                    // 摆设。前缀匹配而不是子串匹配：受限根目录是路径祖先，contains 既会误伤
                    // 中间路径同名的文件，也会漏掉本该排除的。
                    .filter(|(relative, _)| {
                        !RESTRICTED_ROOTS
                            .iter()
                            .any(|restricted| Path::new(relative).starts_with(restricted))
                    })
                    .map(move |(relative, digest)| {
                        (format!("{}/{}", root.display(), relative), digest)
                    })
            })
            .collect(),
    }
}

/// 对象存储目录的内容散列。
///
/// 只列文件名不够：一次原地改写不会改变目录列表。
fn hash_tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // 目录本身也要收：创建/删除/改名一个目录，以及只改目录权限，都是写，
                // 而只收文件的快照看不见它们。
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let mode = fs::metadata(&path)
                    .map(|m| {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode()
                    })
                    .unwrap_or(0);
                out.insert(relative, format!("dir:{mode:o}"));
                walk(&path, base, out);
            } else if let Ok(bytes) = fs::read(&path) {
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                // 指纹里带上尺寸、权限位与 mtime：一次「内容相同」的重写不改内容散列，却仍然
                // 是一次写，只有 mtime 能发现它。
                let meta = fs::metadata(&path).ok();
                let mode = meta
                    .as_ref()
                    .map(|m| {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode()
                    })
                    .unwrap_or(0);
                let mtime = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                out.insert(
                    relative,
                    format!("{:x}:{}:{:o}:{mtime}", md5_like(&bytes), bytes.len(), mode),
                );
            }
        }
    }

    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// 一个够用的内容指纹。
///
/// 这里要的是「内容变了会被发现」，不是密码学强度；为一个测试往 bin crate 里塞一个散列
/// 依赖不值得。长度与逐字节混合一起变化，原地改写躲不过去。
fn md5_like(bytes: &[u8]) -> u128 {
    let mut hash: u128 = 0xcbf2_9ce4_8422_2325;
    for (index, byte) in bytes.iter().enumerate() {
        hash ^= u128::from(*byte).wrapping_add(index as u128);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash ^ (bytes.len() as u128)
}

// ---------------------------------------------------------------- the test

/// 进程级环境变量的保存/还原。
///
/// 单例断言已经让「同进程第二个 fixture」当场炸掉，但用例 panic 后 `Drop` 仍会跑，还原能
/// 让残留的环境不至于影响同一进程里后续任何代码。这是补上「不留痕迹」的最后一段，而不是
/// 单例断言的替代。
struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        Self { saved: Vec::new() }
    }

    fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.saved.push((key.to_string(), std::env::var_os(key)));
        // SAFETY: 见 `Fixture::new` —— 本 target 只有一个用例且门带 `--test-threads=1`。
        unsafe { std::env::set_var(key, value) }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..).rev() {
            // SAFETY: 同上。
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(&key, value),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

struct Fixture {
    /// 先于其它字段声明，因此**最后**析构：还原环境是收尾动作。
    env: std::sync::Mutex<EnvGuard>,
    _temp: tempfile::TempDir,
    database: TestDatabase,
    config_path: PathBuf,
    profile_path: PathBuf,
    object_root: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
}

/// 本 target 里已经创建过 fixture 没有。
///
/// 环境变量是进程级的，第二个 fixture 会覆盖第一个的数据库地址，而且谁都不会报错——于是
/// 「前后比对」会静悄悄地跑在别人的库上。与其在注释里请求后来者小心，不如让它当场炸掉。
static FIXTURE_CREATED: AtomicUsize = AtomicUsize::new(0);

impl Fixture {
    fn new() -> Self {
        assert_eq!(
            FIXTURE_CREATED.fetch_add(1, Ordering::SeqCst),
            0,
            "这个 target 只能有一个 fixture：环境变量是进程级的，第二个会覆盖第一个的数据库 \
             地址且不报错。要加用例，先把配置改成不经环境变量注入"
        );
        let temp = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let config_path = temp.path().join("config.toml");
        let profile_path = temp.path().join("config.it.toml");
        let object_root = temp.path().join("objects");
        let base_dir = temp.path().join("base");
        let cache_dir = temp.path().join("cache");
        for dir in [&object_root, &base_dir, &cache_dir] {
            fs::create_dir_all(dir).expect("create dir");
        }

        common::write_full_config(&config_path);
        // 数据库与对象存储通过 profile 覆盖，而不是环境变量：同一份配置文件既喂给子进程
        // 也喂给进程内的只读装配，两边指向同一个库才谈得上「同一份状态的前后」。
        fs::write(
            &profile_path,
            format!(
                "[database]\n\
                 db_type = \"postgres\"\n\
                 db_path = \"\"\n\
                 db_url = \"{}\"\n\
                 max_connection = 4\n\
                 min_connection = 1\n\
                 acquire_timeout = 5\n\
                 connect_timeout = 5\n\
                 sqlx_logging = false\n\
                 \n\
                 [redis]\n\
                 url = \"{}\"\n\
                 \n\
                 [object_storage]\n\
                 storage_type = \"local\"\n\
                 \n\
                 [object_storage.local]\n\
                 root_dir = \"{}\"\n\
                 \n\
                 [log]\n\
                 print_std = false\n\
                 with_ansi = false\n",
                database.db_url,
                integration_redis_url(),
                object_root.display(),
            ),
        )
        .expect("write profile");

        // `.env.test` 里的 `MEGA_DATABASE__DB_URL` 指向共享的 admin 库，而配置文件的优先级
        // 低于环境变量：不覆盖的话，子进程与进程内装配都会连到共享库，本用例就会在别人的
        // 数据上做「前后比对」。
        //
        // SAFETY: 本 target 只有这一个用例，验收命令也带 `--test-threads=1`，因此没有并发
        // 读取环境变量的线程。加第二个用例会被 `FIXTURE_CREATED` 当场拦下。
        let mut env = EnvGuard::new();
        env.set("MEGA_DATABASE__DB_TYPE", "postgres");
        env.set("MEGA_DATABASE__DB_PATH", "");
        env.set("MEGA_DATABASE__DB_URL", &database.db_url);
        env.set("MEGA_DATABASE__MAX_CONNECTION", "4");
        env.set("MEGA_DATABASE__MIN_CONNECTION", "1");
        env.set("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5");
        env.set("MEGA_DATABASE__CONNECT_TIMEOUT", "5");
        env.set("MEGA_DATABASE__SQLX_LOGGING", "false");
        env.set("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local");
        env.set("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &object_root);
        env.set("MEGA_REDIS__URL", integration_redis_url());
        env.set("MEGA_BASE_DIR", &base_dir);
        env.set("MEGA_CACHE_DIR", &cache_dir);

        Self {
            env: std::sync::Mutex::new(env),
            _temp: temp,
            database,
            config_path,
            profile_path,
            object_root,
            base_dir,
            cache_dir,
        }
    }

    fn command(&self) -> Command {
        // 子进程继承上面设置的环境；这里再显式写一遍关键项，免得「继承」成为一个隐含前提。
        let mut command = Command::new(env!("CARGO_BIN_EXE_monoengine"));
        command
            .env("MEGA_BASE_DIR", &self.base_dir)
            .env("MEGA_CACHE_DIR", &self.cache_dir)
            .env("MEGA_DATABASE__DB_URL", &self.database.db_url)
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            .env("MEGA_REDIS__URL", integration_redis_url())
            .arg("--config")
            .arg(&self.config_path)
            .arg("--profile")
            .arg("it");
        command
    }

    /// 用真实二进制把库播种成生产形态。
    fn seed_through_the_real_binary(&self) {
        let port = reserve_free_port();
        let stdout_path = self._temp.path().join("service.out");
        let stderr_path = self._temp.path().join("service.err");

        let mut command = self.command();
        command
            .args([
                "service",
                "http",
                "--host",
                "127.0.0.1",
                "-p",
                &port.to_string(),
            ])
            .stdout(Stdio::from(
                fs::File::create(&stdout_path).expect("stdout log"),
            ))
            .stderr(Stdio::from(
                fs::File::create(&stderr_path).expect("stderr log"),
            ));

        let mut service = ServiceProcess::spawn(command);
        service.wait_until_listening(
            port,
            Duration::from_secs(120),
            &[&stdout_path, &stderr_path],
        );
        service.shutdown(Duration::from_secs(60));
    }

    /// 除对象存储外还要盯住的文件系统面：mega base（含 vault core key）与 cache。
    fn watched(&self) -> Vec<&Path> {
        vec![self.base_dir.as_path(), self.cache_dir.as_path()]
    }

    /// 把两个对象存储凭据写进 vault，并改用需要它们的 s3compatible 后端。
    ///
    /// 用本地对象存储时只读装配根本不会打开 vault，「Vault 状态零变化」就成了一句空话。
    /// 这里把配置换成真的需要 vault 的形态，后面的比对才有内容。
    fn store_object_storage_credentials_in_vault(&self) {
        for (field, vault_path, value) in [
            (
                "object_storage.s3.access_key_id",
                "config/it/object_storage/access_key_id",
                "AKIA-un43-access-key",
            ),
            (
                "object_storage.s3.secret_access_key",
                "config/it/object_storage/secret_access_key",
                "un43-secret-access-key",
            ),
        ] {
            let mut command = self.command();
            command
                .args([
                    "config",
                    "secret",
                    "set",
                    field,
                    "--vault-path",
                    vault_path,
                    "--field",
                    "value",
                    "--value-stdin",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawn config secret set");
            {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(value.as_bytes())
                    .expect("write secret value");
            }
            let output = child.wait_with_output().expect("config secret set");
            assert!(
                output.status.success(),
                "config secret set failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        // 环境变量优先级高于配置文件，因此后端切换也走环境变量；同样经 guard，退出时还原。
        let mut env = self.env.lock().expect("env guard");
        env.set("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "s3compatible");
        env.set("MEGA_OBJECT_STORAGE__S3__REGION", "us-east-1");
        env.set("MEGA_OBJECT_STORAGE__S3__BUCKET", "un43");
        env.set(
            "MEGA_OBJECT_STORAGE__S3__ENDPOINT_URL",
            "http://127.0.0.1:1",
        );
        env.set(
            "MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID",
            "vault://secret/config/it/object_storage/access_key_id#value",
        );
        env.set(
            "MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY",
            "vault://secret/config/it/object_storage/secret_access_key#value",
        );
    }

    async fn observer(&self) -> DatabaseConnection {
        Database::connect(self.database.db_url.as_str())
            .await
            .expect("observer connection")
    }
}

/// 只读装配跑完，库、refs、对象一字未动。
#[test]
fn integration_readonly_assembly_changes_nothing() {
    monoengine_core::set_object_storage_provider(Arc::new(OrbitObjectStorageProvider));

    let fixture = Fixture::new();
    fixture.seed_through_the_real_binary();

    with_runtime(async {
        let observer = fixture.observer().await;

        let before = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;
        assert!(
            !before.schema.is_empty(),
            "fixture：播种后应当有 schema，否则后面的「零变化」是在比对两个空集"
        );
        assert!(
            !before.refs.is_empty(),
            "fixture：播种后应当有 refs（init_monorepo 写下的根 ref）"
        );
        assert!(!before.objects.is_empty(), "fixture：播种后应当有对象");

        // 只读装配：走 UN-34 的预检把同一份配置解析出来，再开只读上下文。
        let loaded = monoengine_core::config::loader::ConfigLoader::new(
            monoengine_core::config::loader::ConfigInput {
                cli_path: Some(fixture.config_path.clone()),
                env_path: None,
                cli_profile: Some("it".to_string()),
                env_profile: None,
            },
        )
        .load_readonly()
        .expect("只读预检必须能解析这份显式配置");
        let profile = loaded.profile.as_ref().expect("profile resolved");
        assert_eq!(profile.path, fixture.profile_path);

        let summary = monoengine_core::readonly_ops::LoadedConfigSummary {
            source: loaded.source,
            profile_name: Some(profile.name.clone()),
            paths: monoengine_core::readonly_ops::LoadedConfigPaths {
                config: loaded.path.clone(),
                profile: Some(profile.path.clone()),
            },
        };
        let config = monoengine_core::config::Config::new_with_profile(
            loaded.path.to_str().expect("utf-8 config path"),
            Some(profile.path.as_path()),
        )
        .expect("parse config");

        let context =
            monoengine_core::readonly_ops::ReadOnlyContext::open(config, Some(summary.clone()))
                .await
                .expect("只读上下文必须能打开一个已经播种好的部署");

        assert_eq!(
            serde_json::to_string(&context.config_summary.as_ref().unwrap().sanitized())
                .expect("serialize"),
            r#"{"source":"cli","profile":"it"}"#,
            "provenance 必须一路带到上下文里"
        );

        // 真的读：根 ref 必须读得到，否则「读完没变」可能只是因为什么都没读。
        let root_ref = context
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .expect("读根 ref")
            .expect("播种后根 ref 存在");
        assert!(!root_ref.ref_tree_hash.is_empty());

        // 审计真正要读的东西：根目录下的授权源。这条路径同时用到 mono storage（解析 ref 与
        // 树）与对象存储（取 blob），因此它一次覆盖了只读 facade 的两个来源。
        let authz = context
            .storage
            .read_authz_source()
            .await
            .expect("只读 facade 必须能读到授权源");
        assert!(
            authz.contains("UserGroup") || authz.contains("uid"),
            "读回来的应该是实体存储 JSON，而不是别的什么：{authz}"
        );

        let after = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;

        assert_eq!(before.schema, after.schema, "DB schema 变了");
        assert_eq!(before.table_digests, after.table_digests, "DB 行内容变了");
        assert_eq!(before.sequences, after.sequences, "序列被推进过");
        assert_eq!(before.refs, after.refs, "refs 变了");
        assert_eq!(before.objects, after.objects, "对象变了");
        assert_eq!(
            before.object_files, after.object_files,
            "对象存储目录的内容变了"
        );
        assert_eq!(before, after, "只读装配必须什么都没改");

        // 最后校准一下量具：往库里写一行，快照必须变。否则「前后相等」可能只是因为这份
        // 快照什么都没量到。
        observer
            .execute_unprepared(
                "INSERT INTO mega_refs \
                 (id, path, ref_name, ref_commit_hash, ref_tree_hash, created_at, updated_at, is_cl) \
                 VALUES (990001, '/un30-probe', 'refs/heads/un30-probe', 'c', 't', \
                 now(), now(), false)",
            )
            .await
            .expect("写入探针行");
        let probed = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;
        assert_ne!(
            after, probed,
            "快照必须能发现变化，否则上面的相等不构成证据"
        );
        assert_ne!(after.refs, probed.refs, "refs 面必须能发现新增的 ref");
        assert_ne!(
            after.table_digests, probed.table_digests,
            "表内容摘要必须能发现新增的行"
        );

        // 再校准一次**原地改写**：行数不变，摘要必须变。这是「只数行数」会漏掉的那一类。
        observer
            .execute_unprepared("UPDATE mega_refs SET ref_commit_hash = 'c2' WHERE id = 990001")
            .await
            .expect("原地改写探针行");
        let updated = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;
        assert_ne!(
            probed.table_digests, updated.table_digests,
            "原地改写必须被发现——这正是只数行数会漏掉的那一类"
        );

        // ---- 第二幕（UN-43）：真的需要 vault 的那种部署 ----
        //
        // 用本地对象存储时只读装配根本不打开 vault，「Vault 状态零变化」是空话。把凭据放进
        // vault 并切到 s3compatible 之后，只读装配必须**真的**打开它，且开完什么都没变。
        fixture.store_object_storage_credentials_in_vault();

        let key_path = fixture.base_dir.join("vault").join("core_key.json");
        assert!(
            key_path.exists(),
            "fixture：播种后应当已经有 vault core key 文件"
        );

        let before_vault = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;
        assert!(
            before_vault
                .filesystem
                .keys()
                .any(|path| path.ends_with("core_key.json")),
            "fixture：文件系统面必须真的包含 core key 文件，否则后面的比对量不到它"
        );
        assert!(
            before_vault.table_digests.contains_key("public.vault"),
            "fixture：vault 表必须在内容摘要里"
        );

        let config = monoengine_core::config::Config::new_with_profile(
            fixture.config_path.to_str().expect("utf-8 config path"),
            Some(fixture.profile_path.as_path()),
        )
        .expect("parse config");
        let context = monoengine_core::readonly_ops::ReadOnlyContext::open(config, None)
            .await
            .expect("需要 vault 的只读上下文必须能打开");
        let vault = context
            .vault
            .as_ref()
            .expect("凭据是 vault 引用时，只读装配必须打开 vault");
        assert!(
            vault.is_readonly(),
            "而且必须是 UN-31 的只读句柄，不是 bootstrap 出来的那个"
        );
        assert_eq!(
            vault.denied_writes(),
            Some(0),
            "只读装配期间不该有任何写被最终保险拦下"
        );

        let after_vault = snapshot(&observer, &fixture.object_root, &fixture.watched()).await;
        assert_eq!(
            before_vault.table_digests.get("public.vault"),
            after_vault.table_digests.get("public.vault"),
            "Vault 存储状态变了"
        );
        assert_eq!(
            before_vault.filesystem, after_vault.filesystem,
            "文件系统变了（core key 文件被重写是 bootstrap 路径的标志）"
        );
        assert_eq!(
            before_vault, after_vault,
            "打开 vault 的只读装配同样必须零副作用"
        );
    });
}
