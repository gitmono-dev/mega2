// 本文件是进程级黑盒集成测试，覆盖 Vault 运维命令以及 `service http` 启动 smoke。
//
// 设计目标：
// 1. 通过 `CARGO_BIN_EXE_monoengine` 启动真实 CLI，验证用户实际执行命令时会走到的路径。
// 2. 跟随 `docs/refactoring/integration.md` 的集成测试架构，使用 Docker Compose 提供的
//    PostgreSQL/Redis/SMTP 捕获服务，而不是在测试里使用轻量本地数据库替身。
// 3. 对每个需要数据库的测试创建独立 PostgreSQL 数据库，避免并发测试或失败重跑污染状态。
// 4. 直接查询 PostgreSQL 验证数据落点，防止测试误连到非目标数据库。
// 5. 所有 secret value 只通过 stdin 传给 CLI，并断言 stdout/stderr 不泄露明文。
// 6. 服务启动 smoke（integration.md 场景 4）用空闲端口启动真实 `service http`，用裸 HTTP/1.1
//    请求探活，再用 SIGINT 验证可诊断的优雅退出与 fail-closed 行为。

use std::{
    fs,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

// 这些常量模拟当前 P0 集成测试中唯一允许写入 Vault 的配置项：
// `mail.password`。数据库、Redis、对象存储等 bootstrap 阶段就要消费的
// 配置不能依赖 monoengine 自己的 Vault，否则会形成启动环。
const MAIL_PASSWORD_PATH: &str = "config/it/mail/password";
const MAIL_PASSWORD_REF: &str = "vault://secret/config/it/mail/password#value";
const SECRET_VALUE: &str = "smtp-test-password";

// 默认连接信息与 `docker-compose.test.yml`、`.env.test.example` 保持一致。
// 如果 CI 或开发机需要改端口，可以通过 `.env.test` 中的环境变量覆盖。
const DEFAULT_POSTGRES_URL: &str =
    "postgres://mono:mono_test_password@127.0.0.1:15432/monoengine_it";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

// 同一个测试进程内可能创建多个临时数据库。计数器只用于生成唯一库名，
// 不承载测试语义，因此使用 Relaxed ordering 即可。
static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

// 单个 CLI 测试用例的完整隔离环境。
//
// 注意这里同时保存两份配置：
// - bootstrap_config_path：只包含 `[database]`，用于验证 `config secret set/check`
//   只走 DB + Vault bootstrap，不触发完整 AppContext。
// - full_config_path：来自仓库默认 `config/config.toml`，通过环境变量覆盖到测试数据库、
//   本地对象存储和 mail.password_ref，用于验证 `config validate --resolve-secrets`。
struct VaultCliEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    bootstrap_config_path: PathBuf,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
}

impl VaultCliEnv {
    fn new() -> Self {
        // TempDir 负责清理 MEGA_BASE_DIR、MEGA_CACHE_DIR 和对象存储目录。
        // TestDatabase 的 Drop 负责清理 PostgreSQL 临时数据库。
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let database = TestDatabase::create();
        let bootstrap_config_path = temp_dir.path().join("bootstrap-config.toml");
        let full_config_path = temp_dir.path().join("config.toml");
        let base_dir = temp_dir.path().join("base");
        let cache_dir = temp_dir.path().join("cache");
        let object_root = temp_dir.path().join("objects");

        // 最小 bootstrap 配置只提供数据库字段。这样可以证明 secret set/check
        // 不依赖 Redis、对象存储、邮件或 HTTP service 的完整初始化链路。
        fs::write(
            &bootstrap_config_path,
            format!(
                r#"
                [database]
                db_type = "postgres"
                db_path = ""
                db_url = "{}"
                max_connection = 4
                min_connection = 1
                acquire_timeout = 5
                connect_timeout = 5
                sqlx_logging = false
                "#,
                database.db_url
            ),
        )
        .expect("write bootstrap config");

        // 完整配置从仓库默认配置复制出来，再由 command_with_config 注入环境变量覆盖。
        // 这样既验证真实配置结构可加载，也避免测试修改仓库里的 config/config.toml。
        fs::write(&full_config_path, include_str!("../../config/config.toml"))
            .expect("write full config");

        Self {
            temp_dir,
            database,
            bootstrap_config_path,
            full_config_path,
            base_dir,
            cache_dir,
            object_root,
        }
    }

    fn bootstrap_command(&self) -> Command {
        // 用最小配置执行 Vault bootstrap 类命令。
        self.command_with_config(&self.bootstrap_config_path)
    }

    fn full_config_command(&self) -> Command {
        // 用完整配置执行需要解析 mail.password_ref 的命令。
        self.command_with_config(&self.full_config_path)
    }

    fn command_with_config(&self, config_path: &Path) -> Command {
        let mut command = isolated_command(self.temp_dir.path(), &self.base_dir, &self.cache_dir);
        command.arg("--config").arg(config_path);
        command
            // 数据库配置通过环境变量覆盖，确保完整配置和最小配置都指向
            // 当前测试专属的 PostgreSQL 数据库。
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_PATH", "")
            .env("MEGA_DATABASE__DB_URL", &self.database.db_url)
            .env("MEGA_DATABASE__MAX_CONNECTION", "4")
            .env("MEGA_DATABASE__MIN_CONNECTION", "1")
            .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
            .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
            .env("MEGA_DATABASE__SQLX_LOGGING", "false")
            // 禁止测试时向 stdout 打日志，避免 CLI 机器可读输出被日志污染。
            .env("MEGA_LOG__PRINT_STD", "false")
            .env("MEGA_LOG__WITH_ANSI", "false")
            // Redis 和对象存储只在完整配置验证路径可能被读取；secret set/check
            // 的 bootstrap 路径不应触达它们。
            .env("MEGA_REDIS__URL", integration_redis_url())
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root)
            // mail.password_ref 是当前集成测试验证的 Vault SecretRef 落点。
            .env("MEGA_MAIL__ENABLED", "true")
            .env("MEGA_MAIL__SMTP_HOST", "localhost")
            .env("MEGA_MAIL__FROM", "no-reply@example.test")
            .env("MEGA_MAIL__PASSWORD_REF", MAIL_PASSWORD_REF);
        command
    }

    fn core_key_path(&self) -> PathBuf {
        // VaultCore 会在 MEGA_BASE_DIR 下生成 core key。检查这个文件能确认
        // 测试没有写入开发机默认 home/cache 位置。
        self.base_dir.join("vault").join("core_key.json")
    }
}

// PostgreSQL 测试数据库的 RAII 包装。
//
// 测试连接到 compose 提供的 admin database，然后为每个用例创建独立数据库：
// `monoengine_it_<pid>_<counter>`。CLI 子进程拿到的是这个专属数据库的连接串。
// 这样 migrations、Vault 表和 secret 数据都隔离在单个测试里。
struct TestDatabase {
    admin_url: String,
    db_name: String,
    db_url: String,
}

impl TestDatabase {
    fn create() -> Self {
        let admin_url = integration_postgres_url();
        // pid + 进程内递增序号足以避免同一次测试运行中的数据库名冲突。
        let db_name = format!(
            "monoengine_it_{}_{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let db_url = database_url_for_name(&admin_url, &db_name);

        with_runtime(async {
            // 这里故意连接 admin_url，而不是即将创建的 db_url。
            // 如果 compose PostgreSQL 没有启动，错误信息直接提示开发者先启动测试环境。
            let db = Database::connect(admin_url.as_str()).await.unwrap_or_else(|_| {
                panic!(
                    "integration PostgreSQL is not available; run `docker compose -f docker-compose.test.yml up -d` first"
                )
            });

            // 先 drop 再 create，使异常中断后重新运行同一个 pid/counter 组合时仍然可恢复。
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
            // Drop 阶段不能让清理失败覆盖测试本身的结果，因此连接或删除失败都被忽略。
            let Ok(db) = Database::connect(admin_url.as_str()).await else {
                return;
            };

            // CLI 子进程或连接池可能仍持有数据库连接。PostgreSQL 不允许 drop
            // 正在被连接的数据库，所以先终止该数据库上的后端连接。
            let terminate_sql = format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}'"
            );
            let _ = db
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    terminate_sql,
                ))
                .await;
            let _ = db
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("DROP DATABASE IF EXISTS {db_name}"),
                ))
                .await;
        });
    }
}

#[test]
fn config_secret_ref_does_not_load_config() {
    // `config secret ref` 只是把配置字段和 vault path 拼成 SecretRef。
    // 它不应读取配置文件、不应连接数据库，也不应初始化 Vault。
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let base_dir = temp_dir.path().join("base");
    let cache_dir = temp_dir.path().join("cache");
    let missing_config = temp_dir.path().join("missing.toml");

    let mut command = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    // 故意把 MEGA_CONFIG 指向不存在的文件。如果命令错误地加载配置，
    // 这个测试会因为找不到配置文件而失败。
    command.env("MEGA_CONFIG", &missing_config).args([
        "config",
        "secret",
        "ref",
        "mail.password",
        "--vault-path",
        MAIL_PASSWORD_PATH,
        "--field",
        "value",
    ]);

    let output = run(command);
    let (stdout, stderr) = assert_success(&output);

    // 成功路径只能输出 SecretRef 本身，不能夹带日志或其他诊断文本。
    assert_eq!(stdout.trim(), MAIL_PASSWORD_REF);
    assert!(stderr.trim().is_empty(), "unexpected stderr: {stderr}");
}

#[test]
fn config_secret_set_check_and_validate_resolve_secret() {
    // 这是 Vault P0 正向链路：
    // 1. `config secret set` 通过 stdin 写入 secret。
    // 2. `config secret check` 验证同一个 SecretRef 可读。
    // 3. `config validate --resolve-secrets` 在完整配置上解析 mail.password_ref。
    let env = VaultCliEnv::new();

    let mut set = env.bootstrap_command();
    set.args([
        "config",
        "secret",
        "set",
        "mail.password",
        "--vault-path",
        MAIL_PASSWORD_PATH,
        "--field",
        "value",
        "--value-stdin",
    ]);
    let output = run_with_stdin(set, SECRET_VALUE);
    let (stdout, stderr) = assert_success(&output);

    // 写入命令只回显 SecretRef，不回显 secret value。
    assert_eq!(stdout.trim(), format!("stored {MAIL_PASSWORD_REF}"));
    assert_does_not_leak_secret(&stdout, &stderr);

    // 直接查询 PostgreSQL 的 vault 表，保证 secret set 的状态确实落在 compose
    // PostgreSQL，而不是其他本地存储。
    assert_postgres_count_at_least(
        &env.database.db_url,
        "SELECT COUNT(*) AS count FROM vault",
        1,
    );

    // core_key.json 必须写在测试临时 MEGA_BASE_DIR 下，避免污染开发机全局状态。
    assert!(
        env.core_key_path().exists(),
        "vault core key should be created under MEGA_BASE_DIR"
    );

    // check 命令仍使用最小 bootstrap 配置，证明读取 secret 不需要完整服务上下文。
    let mut check = env.bootstrap_command();
    check.args([
        "config",
        "secret",
        "check",
        "mail.password",
        "--ref",
        MAIL_PASSWORD_REF,
    ]);
    let output = run(check);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), format!("ok {MAIL_PASSWORD_REF}"));
    assert_does_not_leak_secret(&stdout, &stderr);

    // validate 命令改用完整配置，覆盖真实配置加载、环境变量 overlay、
    // Vault SecretRef 解析和配置合法性检查的组合路径。
    let mut validate = env.full_config_command();
    validate.args(["config", "validate", "--resolve-secrets"]);
    let output = run(validate);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), "config valid");
    assert_does_not_leak_secret(&stdout, &stderr);
}

#[test]
fn config_validate_resolve_secrets_fails_when_secret_is_missing() {
    // 负向路径：完整配置声明了 mail.password_ref，但测试没有提前写入 secret。
    // 期望 validate 返回非 0，并给出可诊断的缺失 secret 错误。
    let env = VaultCliEnv::new();

    let mut validate = env.full_config_command();
    validate.args(["config", "validate", "--resolve-secrets"]);
    let output = run(validate);
    let (stdout, stderr) = assert_failure(&output);

    assert!(stdout.trim().is_empty(), "unexpected stdout: {stdout}");
    // 失败诊断保留 SecretRef 类型信息，但默认不暴露具体 vault path。
    assert!(
        stderr.contains("secret not found for vault://secret/***#***"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains(MAIL_PASSWORD_PATH),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains(MAIL_PASSWORD_REF),
        "unexpected stderr: {stderr}"
    );
    assert_does_not_leak_secret(&stdout, &stderr);
}

#[test]
fn config_secret_ref_rejects_bootstrap_secret_fields() {
    // 数据库 URL 属于 Vault bootstrap 之前就必须存在的配置。
    // 如果允许它引用 monoengine 自己的 Vault，会导致“先解析 Vault 才能连接 DB，
    // 但先连接 DB 才能打开 Vault”的启动循环，因此 CLI 必须拒绝。
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let base_dir = temp_dir.path().join("base");
    let cache_dir = temp_dir.path().join("cache");

    let mut command = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    command.args([
        "config",
        "secret",
        "ref",
        "database.db_url",
        "--vault-path",
        "config/it/database/url",
    ]);

    let output = run(command);
    let (stdout, stderr) = assert_failure(&output);

    assert!(stdout.trim().is_empty(), "unexpected stdout: {stdout}");
    assert!(
        stderr.contains("only mail.password"),
        "unexpected stderr: {stderr}"
    );
}

fn isolated_command(current_dir: &Path, base_dir: &Path, cache_dir: &Path) -> Command {
    // 进程级测试需要尽量隔离外部环境，避免开发机上的 MEGA_* 变量影响结果。
    let mut command = Command::new(env!("CARGO_BIN_EXE_monoengine"));
    command
        .current_dir(current_dir)
        .env_clear()
        .env("MEGA_BASE_DIR", base_dir)
        .env("MEGA_CACHE_DIR", cache_dir)
        .env("RUST_BACKTRACE", "0");

    // env_clear 会清空 PATH。保留 PATH 让子进程仍能找到动态链接器需要的辅助命令或库路径。
    // LD_LIBRARY_PATH 在部分本地工具链和 CI 镜像中也可能是运行测试二进制所必需的。
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(ld_library_path) = std::env::var_os("LD_LIBRARY_PATH") {
        command.env("LD_LIBRARY_PATH", ld_library_path);
    }

    command
}

fn run(mut command: Command) -> Output {
    // 用于没有 stdin 的 CLI 命令，统一收集 stdout/stderr 供断言。
    command.output().expect("run monoengine")
}

fn run_with_stdin(mut command: Command, input: &str) -> Output {
    // secret value 必须通过 stdin 输入，避免出现在命令行参数、shell history 或进程列表中。
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn monoengine");

    {
        let mut stdin = child.stdin.take().expect("child stdin");
        if let Err(err) = stdin.write_all(input.as_bytes()) {
            // 子进程如果在读取 stdin 前就失败，写入端可能收到 BrokenPipe。
            // 这种情况下真正的失败原因会在 wait_with_output 后通过退出码和 stderr 暴露。
            assert_eq!(err.kind(), ErrorKind::BrokenPipe, "write stdin");
        }
    }

    child.wait_with_output().expect("wait monoengine")
}

fn assert_success(output: &Output) -> (String, String) {
    // 失败时把 stdout/stderr 一并打印，便于定位 CLI 解析、配置加载或数据库连接问题。
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "expected success, got {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );

    (stdout, stderr)
}

fn assert_failure(output: &Output) -> (String, String) {
    // 负向测试也要保留 stdout/stderr，因为它们本身就是 CLI 接口兼容性的一部分。
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "expected failure, got {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );

    (stdout, stderr)
}

fn assert_does_not_leak_secret(stdout: &str, stderr: &str) {
    // 这是所有涉及 secret 的测试都要复用的安全断言。
    // 目前只检查测试明文值；更完整的日志 redaction gate 可在后续扩展。
    assert!(
        !stdout.contains(SECRET_VALUE),
        "stdout leaked secret value: {stdout}"
    );
    assert!(
        !stderr.contains(SECRET_VALUE),
        "stderr leaked secret value: {stderr}"
    );
}

fn integration_postgres_url() -> String {
    // `.env.test` 可以覆盖默认值；没有覆盖时使用 docker-compose.test.yml 的本地端口。
    std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string())
}

fn integration_redis_url() -> String {
    // 当前 Vault CLI 测试的 bootstrap 路径不应连接 Redis。
    // 仍提供默认值，是为了完整配置 validate 路径能按集成环境架构解析配置。
    std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| DEFAULT_REDIS_URL.to_string())
}

fn database_url_for_name(admin_url: &str, db_name: &str) -> String {
    // 复用 admin_url 的 scheme、用户名、密码、host 和端口，只替换 path 为测试数据库名。
    // 这样 CI 只需要维护一个 PostgreSQL 入口连接串。
    let mut url = url::Url::parse(admin_url).unwrap_or_else(|_| {
        panic!("MEGA_DATABASE__DB_URL must be a valid PostgreSQL URL for integration tests")
    });
    url.set_path(db_name);
    url.to_string()
}

fn assert_postgres_count_at_least(db_url: &str, sql: &str, expected: i64) {
    // 这个 helper 专门用于“测试数据确实写入 PostgreSQL”的断言。
    // 查询语句要求返回名为 count 的 i64 列，避免 helper 隐式绑定具体表结构。
    let count = with_runtime(async {
        let db = Database::connect(db_url)
            .await
            .unwrap_or_else(|_| panic!("failed to reconnect to integration PostgreSQL database"));
        let row = db
            .query_one(Statement::from_string(DatabaseBackend::Postgres, sql))
            .await
            .unwrap_or_else(|_| panic!("failed to query integration PostgreSQL database"))
            .expect("PostgreSQL count query should return one row");
        row.try_get::<i64>("", "count")
            .expect("PostgreSQL count query should expose count")
    });

    assert!(
        count >= expected,
        "expected PostgreSQL count >= {expected}, got {count}"
    );
}

async fn execute_postgres(db: &sea_orm::DatabaseConnection, sql: String) {
    // PostgreSQL 管理语句集中走这里，便于在失败时给出统一的测试准备错误。
    db.execute(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .unwrap_or_else(|_| panic!("failed to prepare integration PostgreSQL database"));
}

fn with_runtime<T>(future: impl std::future::Future<Output = T>) -> T {
    // integration test 本身是同步 #[test]。SeaORM 连接和查询是 async，
    // 因此用当前线程 runtime 桥接，避免把所有测试函数改成 #[tokio::test]。
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

// ===== service http 启动 smoke（integration.md 场景 4）=====

#[test]
fn integration_service_http_smoke() {
    // 验证真实启动顺序 Config -> Storage(DB+migrations) -> Redis -> VaultCore ->
    // mail resolver -> SmtpMailer/EmailDispatcher -> init_monorepo -> HTTP。
    let env = VaultCliEnv::new();

    // 先写入 mail.password，让启动期的 SMTP mailer SecretRef 解析成功。
    seed_mail_password(&env);

    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    // 把日志输出到 stdout，便于 smoke 断言检查启动日志且不泄露 secret。
    command.env("MEGA_LOG__PRINT_STD", "true");
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

    // 端口必须在超时前可连接（服务成功绑定）；若子进程提前退出则带日志报错。
    service.wait_until_listening(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    // 命中一个稳定、无需鉴权的文档 endpoint，证明 router 已在服务请求。
    let response = http_get(port, "/api/openapi.json");
    let status_line = response.lines().next().unwrap_or_default().to_string();
    assert!(
        status_line.contains(" 200"),
        "smoke endpoint did not return 200; status line: {status_line:?}\nlogs:\n{}",
        read_log(&stdout_path),
    );

    // migrations 已在本测试专属 PostgreSQL 库执行（也证明确实连接到 PostgreSQL）。
    assert_postgres_count_at_least(
        &env.database.db_url,
        "SELECT COUNT(*) AS count FROM seaql_migrations",
        1,
    );

    // 通过 SIGINT 优雅退出；CLI 的 ctrl-c handler 以 0 退出，且不留下后台子进程。
    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path),
    );

    // 日志中不应出现连接到非 PostgreSQL 后端的记录，也不应泄露 mail secret。
    let logs = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        !logs.to_ascii_lowercase().contains("sqlite"),
        "logs unexpectedly mention a non-PostgreSQL backend:\n{logs}"
    );
    assert_does_not_leak_secret(&logs, "");
}

#[test]
fn integration_service_http_fails_when_mailer_secret_missing() {
    // mail 已启用且声明 password_ref，但从不写入 secret。启动期 mail resolver 必须
    // fail-closed：进程以非 0 退出并给出脱敏诊断，且从不绑定 HTTP 端口（而非静默禁用）。
    let env = VaultCliEnv::new();
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
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
    // 在等待退出的整个过程中持续探测端口：startup 必须在绑定 HTTP 之前 fail-closed。
    let status = service
        .wait_for_exit_without_binding(port, Duration::from_secs(90))
        .expect("service must exit when the mail secret is missing");

    assert!(
        !status.success(),
        "mailer initialization failure must fail the process, not silently disable mail"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "HTTP port must not be bound after a fail-closed exit"
    );

    let stderr = read_log(&stderr_path);
    assert!(
        stderr.contains("secret not found"),
        "expected a missing-secret diagnostic; stderr:\n{stderr}"
    );
    // 诊断不得泄露具体 vault path / ref / 明文。
    assert!(
        !stderr.contains(MAIL_PASSWORD_PATH) && !stderr.contains(MAIL_PASSWORD_REF),
        "diagnostic leaked the vault path/ref:\n{stderr}"
    );
    assert_does_not_leak_secret("", &stderr);
}

// ===== 错误诊断脱敏 gate（integration.md 场景 7）=====

#[test]
fn integration_error_redaction_does_not_leak_db_password() {
    // 用带哨兵密码、拒绝连接端口的坏 DB URL 启动 service：进程必须 fail-closed，
    // 且日志与 stderr 都不得包含明文密码（DB 连接日志经 redact_db_url 脱敏，
    // 连接错误经 stderr 输出）。这是 integration.md 场景 7 中 DB URL 脱敏 bullet 的活进程门禁。
    const DB_SENTINEL: &str = "s3ntineldbpw";

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let base_dir = temp_dir.path().join("base");
    let cache_dir = temp_dir.path().join("cache");
    let object_root = temp_dir.path().join("objects");
    let config_path = temp_dir.path().join("config.toml");
    fs::write(&config_path, include_str!("../../config/config.toml")).expect("write config");

    let port = reserve_free_port();
    let stdout_path = temp_dir.path().join("service.out");
    let stderr_path = temp_dir.path().join("service.err");
    // 端口 1 几乎必然拒绝连接，确保 DB 连接快速失败。
    let bad_db_url = format!("postgres://mono:{DB_SENTINEL}@127.0.0.1:1/monoengine_redaction");

    let mut command = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    command.arg("--config").arg(&config_path);
    command
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_DATABASE__DB_PATH", "")
        .env("MEGA_DATABASE__DB_URL", &bad_db_url)
        .env("MEGA_DATABASE__CONNECT_TIMEOUT", "3")
        .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "3")
        .env("MEGA_LOG__PRINT_STD", "true")
        .env("MEGA_LOG__WITH_ANSI", "false")
        .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
        .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &object_root)
        // mail 在 DB 之后才初始化；关掉它让失败点确定落在 DB 连接。
        .env("MEGA_MAIL__ENABLED", "false")
        .args([
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
    let status = service
        .wait_for_exit(Duration::from_secs(60))
        .expect("service must exit when the database is unreachable");
    assert!(
        !status.success(),
        "service must fail closed on an unreachable database"
    );

    let logs = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        !logs.contains(DB_SENTINEL),
        "database password leaked into logs/stderr:\n{logs}"
    );
}

#[test]
fn integration_error_redaction_does_not_leak_redis_password() {
    // 好 DB + 带哨兵密码的坏 Redis URL 启动 service：DB/migrations 成功后 Redis 连接失败，
    // 进程必须 fail-closed，且日志与 stderr 都不得包含 Redis 明文密码
    // （redis init 的错误经 redact_redis_url 脱敏）。
    const REDIS_SENTINEL: &str = "s3ntinelredispw";

    let env = VaultCliEnv::new();
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");
    let bad_redis_url = format!("redis://:{REDIS_SENTINEL}@127.0.0.1:1");

    let mut command = env.full_config_command();
    command
        .env("MEGA_REDIS__URL", &bad_redis_url)
        .env("MEGA_LOG__PRINT_STD", "true")
        .args([
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
    let status = service
        .wait_for_exit(Duration::from_secs(90))
        .expect("service must exit when Redis is unreachable");
    assert!(
        !status.success(),
        "service must fail closed on an unreachable Redis"
    );

    let logs = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        !logs.contains(REDIS_SENTINEL),
        "redis password leaked into logs/stderr:\n{logs}"
    );
}

fn seed_mail_password(env: &VaultCliEnv) {
    // 复用 secret-set 正路径：通过 stdin 把 mail.password 写入测试专属 Vault。
    let mut set = env.bootstrap_command();
    set.args([
        "config",
        "secret",
        "set",
        "mail.password",
        "--vault-path",
        MAIL_PASSWORD_PATH,
        "--field",
        "value",
        "--value-stdin",
    ]);
    let output = run_with_stdin(set, SECRET_VALUE);
    assert_success(&output);
}

fn create_log_file(path: &Path) -> fs::File {
    fs::File::create(path).expect("create service log file")
}

fn read_log(path: &Path) -> String {
    // 子进程的 ctrl-c handler 用 process::exit 退出，可能未 flush 用户态缓冲，
    // 因此这里尽力读取已落盘的内容；缺失的尾部日志不作为硬断言依据。
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn reserve_free_port() -> u16 {
    // 绑定临时端口拿到端口号后立即释放，交给随后启动的 service 复用。
    // 单机集成测试里这个短暂的复用窗口可以接受。
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve free port");
    listener.local_addr().expect("listener local addr").port()
}

fn http_get(port: u16, path: &str) -> String {
    // 极简 HTTP/1.1 客户端，避免给 bin 测试 crate 引入 HTTP 客户端依赖；
    // `Connection: close` 让我们可以读到 EOF。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                // 一旦连上就给 I/O 设定超时：避免端点挂起导致 read_to_string 永久阻塞，
                // 那样测试不会 panic，ServiceProcess::drop 也不会运行，从而泄露子进程。
                stream
                    .set_write_timeout(Some(Duration::from_secs(15)))
                    .expect("set write timeout");
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .expect("set read timeout");
                let request = format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(request.as_bytes())
                    .expect("write http request");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("read http response within timeout");
                return response;
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("failed to GET {path} on port {port}: {err}");
                }
                sleep(Duration::from_millis(200));
            }
        }
    }
}

// 受控的服务子进程包装：保证测试无论成功失败都不会泄露后台进程。
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
            // 子进程在绑定端口前退出，说明启动失败。
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

    // 与 wait_for_exit 相同，但在等待退出的整个窗口内持续探测端口：一旦端口可连接就立即
    // 失败。这样才能证明 fail-closed 启动“从不绑定”HTTP，而不仅是退出后未绑定。
    fn wait_for_exit_without_binding(
        &mut self,
        port: u16,
        timeout: Duration,
    ) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                panic!(
                    "service bound port {port} but was expected to fail closed before binding HTTP"
                );
            }
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
        // SAFETY: 向运行中的子进程发送 SIGINT；CLI 安装的 ctrl-c handler 会干净退出。
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
        self.wait_for_exit(timeout).unwrap_or_else(|| {
            // 兜底升级到 SIGKILL，确保测试永远不泄露子进程。
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
            panic!("service did not exit within {timeout:?} after SIGINT");
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
