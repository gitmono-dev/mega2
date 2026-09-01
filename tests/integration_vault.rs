// 本文件是进程级黑盒集成测试，覆盖 Vault 运维命令以及 `service http` 启动 smoke。
//
// 设计目标：
// 1. 通过 `CARGO_BIN_EXE_monoengine` 启动真实 CLI，验证用户实际执行命令时会走到的路径。
// 2. 跟随 `docs/refactoring/integration.md` 的集成测试架构，使用 Docker Compose 提供的
//    PostgreSQL/Redis 服务，而不是在测试里使用轻量本地数据库替身。
// 3. 对每个需要数据库的测试创建独立 PostgreSQL 数据库，避免并发测试或失败重跑污染状态。
// 4. 直接查询 PostgreSQL 验证数据落点，防止测试误连到非目标数据库。
// 5. 所有 secret value 只通过 stdin 传给 CLI，并断言 stdout/stderr 不泄露明文。
// 6. 服务启动 smoke（integration.md 场景 4）用空闲端口启动真实 `service http`，用裸 HTTP/1.1
//    请求探活，再用 SIGINT 验证可诊断的优雅退出与 fail-closed 行为。

mod common;

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

#[cfg(feature = "fastcdc")]
use base64::Engine as _;
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use tempfile::TempDir;

// 这些常量模拟当前 P0/P2 集成测试中允许写入 Vault 的配置项：
// `redis.url` 与 `object_storage.s3.access_key_id` / `secret_access_key`。
// 数据库凭据在 bootstrap 阶段就要消费，不能依赖 monoengine 自己的 Vault，
// 否则会形成启动环；Redis URL 在 Vault 就绪后连接，可用 SecretRef 覆盖。
const OBJECT_STORAGE_ACCESS_KEY_PATH: &str = "config/it/object_storage/access_key_id";
const OBJECT_STORAGE_SECRET_KEY_PATH: &str = "config/it/object_storage/secret_access_key";
const OBJECT_STORAGE_ACCESS_KEY_REF: &str =
    "vault://secret/config/it/object_storage/access_key_id#value";
const OBJECT_STORAGE_SECRET_KEY_REF: &str =
    "vault://secret/config/it/object_storage/secret_access_key#value";
const S3_ACCESS_KEY_VALUE: &str = "AKIA-test-access-key";
const S3_SECRET_KEY_VALUE: &str = "wJalrXUtnFEMI/test/secret/key/EXAMPLE";

const REDIS_URL_PATH: &str = "config/it/redis/url";
const REDIS_URL_REF: &str = "vault://secret/config/it/redis/url#value";

// 默认连接信息与 `docker-compose.test.yml`、`.env.test.example` 保持一致。
// 如果 CI 或开发机需要改端口，可以通过 `.env.test` 中的环境变量覆盖。
const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
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
//   本地对象存储和 redis.url SecretRef，用于验证 `config validate --resolve-secrets`。
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
        write_bootstrap_config(&bootstrap_config_path, &database.db_url);

        // 完整配置从仓库默认配置复制出来，再由 command_with_config 注入环境变量覆盖。
        // 这样既验证真实配置结构可加载，也避免测试修改仓库里的 config/config.toml。
        common::write_full_config(&full_config_path);

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
        // 用完整配置执行需要解析 redis.url SecretRef 的命令。
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
            .env("MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR", &self.object_root);
        command
    }

    fn core_key_path(&self) -> PathBuf {
        // VaultCore 会在 MEGA_BASE_DIR 下生成 core key。检查这个文件能确认
        // 测试没有写入开发机默认 home/cache 位置。
        self.base_dir.join("vault").join("core_key.json")
    }

    fn write_profile(&self, name: &str, content: &str) -> PathBuf {
        // 按照 ConfigLoader 的约定，profile 文件位于基础配置同目录、同 stem 的
        // `.<profile>.toml`，例如 `config.toml` → `config.it.toml`。
        let path = self
            .full_config_path
            .parent()
            .expect("config path has parent")
            .join(format!("config.{}.toml", name));
        fs::write(&path, content).expect("write profile config");
        path
    }
}

fn write_bootstrap_config(path: &Path, db_url: &str) {
    fs::write(
        path,
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
            db_url
        ),
    )
    .expect("write bootstrap config");
}

// PostgreSQL 测试数据库的 RAII 包装。
//
// 测试连接到 compose 提供的 admin database，然后为每个用例创建独立数据库：
// `monoengine_<pid>_<counter>`。CLI 子进程拿到的是这个专属数据库的连接串。
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
            "monoengine_{}_{}",
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
        "redis.url",
        "--vault-path",
        REDIS_URL_PATH,
        "--field",
        "value",
    ]);

    let output = run(command);
    let (stdout, stderr) = assert_success(&output);

    // 成功路径只能输出 SecretRef 本身，不能夹带日志或其他诊断文本。
    assert_eq!(stdout.trim(), REDIS_URL_REF);
    assert!(stderr.trim().is_empty(), "unexpected stderr: {stderr}");
}

#[test]
fn config_secret_set_check_and_validate_resolve_redis_url_secret_ref() {
    // 这是 Vault P0 正向链路：
    // 1. `config secret set` 通过 stdin 写入 secret。
    // 2. `config secret check` 验证同一个 SecretRef 可读。
    // 3. `config validate --resolve-secrets` 在完整配置上解析 redis.url SecretRef。
    let env = VaultCliEnv::new();
    let redis_url = integration_redis_url();

    let mut set = env.bootstrap_command();
    set.args([
        "config",
        "secret",
        "set",
        "redis.url",
        "--vault-path",
        REDIS_URL_PATH,
        "--field",
        "value",
        "--value-stdin",
    ]);
    let output = run_with_stdin(set, &redis_url);
    let (stdout, stderr) = assert_success(&output);

    // 写入命令只回显 SecretRef，不回显 secret value。
    assert_eq!(stdout.trim(), format!("stored {REDIS_URL_REF}"));
    assert!(
        !stdout.contains(&redis_url) && !stderr.contains(&redis_url),
        "stdout/stderr leaked Redis URL"
    );

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
        "redis.url",
        "--ref",
        REDIS_URL_REF,
    ]);
    let output = run(check);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), format!("ok {REDIS_URL_REF}"));
    assert!(
        !stdout.contains(&redis_url) && !stderr.contains(&redis_url),
        "stdout/stderr leaked Redis URL"
    );

    // validate 命令改用完整配置，覆盖真实配置加载、环境变量 overlay、
    // Vault SecretRef 解析和配置合法性检查的组合路径。
    let mut validate = env.full_config_command();
    validate.env("MEGA_REDIS__URL", REDIS_URL_REF);
    validate.args(["config", "validate", "--resolve-secrets"]);
    let output = run(validate);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), "config valid");
    assert!(
        !stdout.contains(&redis_url) && !stderr.contains(&redis_url),
        "validate leaked Redis URL"
    );
}

#[test]
fn config_validate_resolve_secrets_fails_when_redis_url_secret_is_missing() {
    // 负向路径：完整配置声明了 redis.url SecretRef，但测试没有提前写入 secret。
    // 期望 validate 返回非 0，并给出可诊断的缺失 secret 错误。
    let env = VaultCliEnv::new();

    let mut validate = env.full_config_command();
    validate.env("MEGA_REDIS__URL", REDIS_URL_REF);
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
        !stderr.contains(REDIS_URL_PATH),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains(REDIS_URL_REF),
        "unexpected stderr: {stderr}"
    );
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
        stderr.contains("cannot be stored in monoengine vault")
            && stderr.contains("supported fields are"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn config_secret_set_check_and_validate_resolve_object_storage_s3_secret_refs() {
    // P2 对象存储 S3 凭据的进程级黑盒 gate：
    // 1. `config secret set` 把 access_key_id / secret_access_key 写入 Vault。
    // 2. `config secret check` 验证两个 SecretRef 可读。
    // 3. `config validate --resolve-secrets` 在 S3 后端配置下解析它们。
    // 该测试不依赖真实 S3/GCS 服务端，只验证 Vault SecretRef 在 validate/CLI 链路
    // 中的解析与 namespace 对齐（与 `docs/refactoring/integration.md` 中 P2 gate 对应）。
    let env = VaultCliEnv::new();

    let mut set_access = env.bootstrap_command();
    set_access.args([
        "config",
        "secret",
        "set",
        "object_storage.s3.access_key_id",
        "--vault-path",
        OBJECT_STORAGE_ACCESS_KEY_PATH,
        "--field",
        "value",
        "--value-stdin",
    ]);
    let output = run_with_stdin(set_access, S3_ACCESS_KEY_VALUE);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(
        stdout.trim(),
        format!("stored {OBJECT_STORAGE_ACCESS_KEY_REF}")
    );
    assert!(
        !stdout.contains(S3_ACCESS_KEY_VALUE) && !stderr.contains(S3_ACCESS_KEY_VALUE),
        "stdout/stderr leaked S3 access key"
    );

    let mut set_secret = env.bootstrap_command();
    set_secret.args([
        "config",
        "secret",
        "set",
        "object_storage.s3.secret_access_key",
        "--vault-path",
        OBJECT_STORAGE_SECRET_KEY_PATH,
        "--field",
        "value",
        "--value-stdin",
    ]);
    let output = run_with_stdin(set_secret, S3_SECRET_KEY_VALUE);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(
        stdout.trim(),
        format!("stored {OBJECT_STORAGE_SECRET_KEY_REF}")
    );
    assert!(
        !stdout.contains(S3_SECRET_KEY_VALUE) && !stderr.contains(S3_SECRET_KEY_VALUE),
        "stdout/stderr leaked S3 secret key"
    );

    let mut check_access = env.bootstrap_command();
    check_access.args([
        "config",
        "secret",
        "check",
        "object_storage.s3.access_key_id",
        "--ref",
        OBJECT_STORAGE_ACCESS_KEY_REF,
    ]);
    let output = run(check_access);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), format!("ok {OBJECT_STORAGE_ACCESS_KEY_REF}"));
    assert!(
        !stdout.contains(S3_ACCESS_KEY_VALUE) && !stderr.contains(S3_ACCESS_KEY_VALUE),
        "check leaked S3 access key"
    );

    let mut check_secret = env.bootstrap_command();
    check_secret.args([
        "config",
        "secret",
        "check",
        "object_storage.s3.secret_access_key",
        "--ref",
        OBJECT_STORAGE_SECRET_KEY_REF,
    ]);
    let output = run(check_secret);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), format!("ok {OBJECT_STORAGE_SECRET_KEY_REF}"));
    assert!(
        !stdout.contains(S3_SECRET_KEY_VALUE) && !stderr.contains(S3_SECRET_KEY_VALUE),
        "check leaked S3 secret key"
    );

    // 完整配置路径：把对象存储切到 S3，并用 Vault SecretRef 填充 S3 凭据。
    let mut validate = env.full_config_command();
    validate
        .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "s3")
        .env("MEGA_OBJECT_STORAGE__S3__REGION", "us-east-1")
        .env("MEGA_OBJECT_STORAGE__S3__BUCKET", "monoengine-test")
        .env(
            "MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID",
            OBJECT_STORAGE_ACCESS_KEY_REF,
        )
        .env(
            "MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY",
            OBJECT_STORAGE_SECRET_KEY_REF,
        );
    validate.args(["config", "validate", "--resolve-secrets"]);
    let output = run(validate);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), "config valid");
    assert!(
        !stdout.contains(S3_ACCESS_KEY_VALUE)
            && !stderr.contains(S3_ACCESS_KEY_VALUE)
            && !stdout.contains(S3_SECRET_KEY_VALUE)
            && !stderr.contains(S3_SECRET_KEY_VALUE),
        "validate leaked S3 credentials"
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
        .env("MEGA_ID_GENERATOR_LAYOUT_VERSION", "8+8-v1")
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

fn integration_postgres_url() -> String {
    // `.env.test` 可以覆盖默认值；没有覆盖时使用 docker-compose.test.yml 的本地端口。
    std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| DEFAULT_POSTGRES_URL.to_string())
}

fn integration_redis_url() -> String {
    // Vault CLI bootstrap commands should not connect to Redis. Full-config validation
    // may still validate or resolve redis.url, including the post-vault SecretRef path.
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
            .query_one_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
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
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
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
    // init_monorepo -> HTTP。
    let env = VaultCliEnv::new();

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

    // 日志中不应出现连接到非 PostgreSQL 后端的记录。
    let logs = format!("{}\n{}", read_log(&stdout_path), read_log(&stderr_path));
    assert!(
        !logs.to_ascii_lowercase().contains("sqlite"),
        "logs unexpectedly mention a non-PostgreSQL backend:\n{logs}"
    );
}

#[cfg(feature = "fastcdc")]
#[test]
fn integration_fastcdc_media_http_contract() {
    // 通过真实 service http 进程验证 FastCDC Media 的最终 repository-scoped
    // URL、Mono Bearer token、capabilities 载荷与 runtime OpenAPI，而非只测
    // in-process router。VaultCliEnv 为此 case 提供独立 Vault/DB/对象存储目录。
    let env = VaultCliEnv::new();
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join("fastcdc-service.out");
    let stderr_path = env.temp_dir.path().join("fastcdc-service.err");

    let mut command = env.full_config_command();
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
    service.wait_until_listening(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    let token_id = 9_000_000_000_i64 + DB_COUNTER.fetch_add(1, Ordering::Relaxed) as i64;
    let token = format!("fastcdc-http-token-{token_id}");
    let username = "fastcdc-http-user";
    let sql = format!(
        "INSERT INTO access_token (id, username, token, created_at) \
         VALUES ({token_id}, '{username}', '{token}', now())"
    );
    with_runtime(async {
        let db = Database::connect(&env.database.db_url)
            .await
            .expect("connect FastCDC integration database");
        execute_postgres(&db, sql).await;
    });

    let capabilities_path = "/project/demo.git/info/lfs/libra/media/v1/capabilities";
    let anonymous = http_get(port, capabilities_path);
    assert!(
        anonymous
            .lines()
            .next()
            .is_some_and(|line| line.contains(" 401 ")),
        "FastCDC Media capabilities must require a Mono access token"
    );

    let valid_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"))
    );
    for (credential, authorization) in [
        (
            "unknown Bearer credential",
            "Bearer unknown-fastcdc-http-token",
        ),
        (
            "ordinary Basic credentials carrying a valid Mono token",
            valid_basic.as_str(),
        ),
    ] {
        let response = http_get_authorization(port, capabilities_path, authorization);
        assert!(
            response
                .lines()
                .next()
                .is_some_and(|line| line.contains(" 401 ")),
            "FastCDC Media capabilities must reject {credential}"
        );
    }

    let bearer = format!("Bearer {token}");
    let capabilities_response = http_get_authorization(port, capabilities_path, &bearer);
    let (capabilities_headers, capabilities_body) = capabilities_response
        .split_once("\r\n\r\n")
        .expect("FastCDC capabilities response must contain headers and JSON body");
    assert!(
        capabilities_headers
            .lines()
            .next()
            .is_some_and(|line| line.contains(" 200 ")),
        "authenticated FastCDC capabilities request failed: {}",
        capabilities_headers.lines().next().unwrap_or_default()
    );
    let capabilities: serde_json::Value =
        serde_json::from_str(capabilities_body).expect("FastCDC capabilities JSON");
    assert_eq!(capabilities["version"], "1");
    assert_eq!(
        capabilities["chunk_algorithms"],
        serde_json::json!(["fastcdc-v1"])
    );
    assert_eq!(
        capabilities["hash_algorithms"],
        serde_json::json!(["sha256"])
    );
    assert_eq!(capabilities["max_chunk_size"], 8 * 1024 * 1024);
    assert_eq!(capabilities["max_manifest_size"], 10 * 1024 * 1024);
    assert_eq!(capabilities["supports_batch_exists"], true);
    assert_eq!(capabilities["supports_standard_lfs_fallback"], true);

    let openapi_response = http_get(port, "/api/openapi.json");
    let (openapi_headers, openapi_body) = openapi_response
        .split_once("\r\n\r\n")
        .expect("runtime OpenAPI response must contain headers and JSON body");
    assert!(
        openapi_headers
            .lines()
            .next()
            .is_some_and(|line| line.contains(" 200 ")),
        "runtime OpenAPI request failed: {}",
        openapi_headers.lines().next().unwrap_or_default()
    );
    let openapi: serde_json::Value =
        serde_json::from_str(openapi_body).expect("runtime OpenAPI JSON");
    let paths = openapi["paths"]
        .as_object()
        .expect("runtime OpenAPI paths object");
    for (documented_path, method) in [
        ("/info/lfs/libra/media/v1/capabilities", "get"),
        ("/info/lfs/libra/media/v1/manifests", "post"),
        (
            "/info/lfs/libra/media/v1/manifests/{manifest_id}/chunks/{hash}",
            "put",
        ),
        (
            "/info/lfs/libra/media/v1/manifests/{manifest_id}/finalize",
            "post",
        ),
        (
            "/info/lfs/libra/media/v1/manifests/by-media/{media_oid}",
            "get",
        ),
        (
            "/info/lfs/libra/media/v1/manifests/by-media/{media_oid}/chunks/{hash}",
            "get",
        ),
    ] {
        assert!(
            paths.contains_key(documented_path),
            "runtime OpenAPI must describe {method} {documented_path}"
        );
        assert_eq!(
            openapi["paths"][documented_path][method]["security"],
            serde_json::json!([{"monoAccessToken": []}]),
            "runtime OpenAPI must require a Mono access token for {method} {documented_path}"
        );
        assert_eq!(
            openapi["paths"][documented_path]["servers"][0]["url"], "/{repository}",
            "runtime OpenAPI must scope {method} {documented_path} by repository"
        );
        assert_eq!(
            openapi["paths"][documented_path]["servers"][0]["variables"]["repository"]["default"],
            "project/demo.git",
            "runtime OpenAPI must provide a canonical repository default for {method} {documented_path}"
        );
    }
    assert!(
        !paths.contains_key("/api/v1/lfs/libra/media/v1/capabilities"),
        "runtime OpenAPI must not document a repository-free Media alias"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "FastCDC service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path)
    );
}

#[test]
fn integration_config_hot_reload() {
    // integration.md P2 热加载黑盒 gate：在运行的 `service http` 上通过修改 profile
    // 文件触发 config reload watcher，验证白名单字段热生效、非白名单字段仅报告需
    // 重启且旧配置继续服务、坏 TOML 被拒绝且服务不中断。
    let env = VaultCliEnv::new();
    env.write_profile(
        "it",
        r#"
[log]
level = "info"
print_std = true
"#,
    );

    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join("service.out");
    let stderr_path = env.temp_dir.path().join("service.err");

    let mut command = env.full_config_command();
    command.env("MEGA_PROFILE", "it");
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
    service.wait_until_listening(port, Duration::from_secs(90), &stdout_path, &stderr_path);

    let initial_logs = read_log(&stdout_path);
    assert!(
        initial_logs.contains("config reload watcher started"),
        "watcher should start and log to stdout; logs:\n{initial_logs}"
    );

    // 1. 白名单字段变更（log.level）应热生效并输出 reload report。
    // 先在 info 级别触发一条本会在 debug 输出的日志，确认它当前不存在，避免假阳性。
    let _ = http_get(port, "/trigger-debug/info/lfs/objects/123");
    let logs_before = read_log(&stdout_path);
    assert!(
        !logs_before.contains("rewrite: old uri"),
        "info level should suppress debug logs before reload; logs:\n{logs_before}"
    );

    env.write_profile(
        "it",
        r#"
[log]
level = "debug"
print_std = true
"#,
    );
    let logs = wait_for_log_marker(
        &stdout_path,
        "config reload watcher applied changed config",
        Duration::from_secs(15),
    );
    assert!(
        logs.contains("\"log.level\""),
        "reload report should list log.level in applied_fields; logs:\n{logs}"
    );

    // 通过触发 rewrite_lfs_request_uri 的 debug 日志，证明 level filter 确实已重新加载。
    let _ = http_get(port, "/trigger-debug/info/lfs/objects/123");
    let logs_after = wait_for_log_marker(&stdout_path, "rewrite: old uri", Duration::from_secs(15));
    assert!(
        logs_after.contains(" DEBUG "),
        "reloaded level should emit DEBUG lines; logs:\n{logs_after}"
    );

    // 2. 非白名单字段变更应被标记为 restart-required，旧配置继续服务。
    env.write_profile(
        "it",
        r#"
[log]
level = "debug"
print_std = true

[monorepo]
import_dir = "/tmp/hot-reload-restart-required"
"#,
    );
    let logs = wait_for_log_marker(
        &stdout_path,
        "\"monorepo.import_dir\"",
        Duration::from_secs(15),
    );
    assert!(
        logs.contains("restart_required_fields"),
        "reload report should include restart_required_fields; logs:\n{logs}"
    );
    // 用依赖 monorepo.import_dir 的路由证明旧配置仍在服务：旧 import_dir 为 /third-party，
    // 请求路径不在其下，因此应返回 true；若 restart-required 值被错误热应用则会返回 false。
    let clone_response = http_get(
        port,
        "/api/v1/tree/path-can-clone?path=/tmp/hot-reload-restart-required/foo",
    );
    assert!(
        clone_response.contains("\"data\":true"),
        "service should continue using old import_dir when restart is required; response:\n{clone_response}"
    );

    // 3. 非法 TOML 应被 watcher 拒绝，服务继续运行。
    env.write_profile("it", "this is not valid TOML [[");
    let _logs = wait_for_log_marker(
        &stdout_path,
        "config reload watcher rejected changed config",
        Duration::from_secs(15),
    );
    let status_line = http_get(port, "/api/openapi.json")
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(
        status_line.contains(" 200"),
        "service should continue serving after rejected invalid profile; status: {status_line:?}"
    );

    let status = service.shutdown_via_sigint(Duration::from_secs(60));
    assert!(
        status.success(),
        "service did not shut down cleanly: {status}\nstderr:\n{}",
        read_log(&stderr_path)
    );
}

// ===== 真实 S3-compatible 对象存储后端 gate（integration.md P2）=====

#[test]
fn integration_compose_monoengine_http_smoke() {
    // Standing compose `monoengine` service (profile `app`, host 19180). Soft
    // skip when the profile is not up so `cargo test --all` stays usable with
    // only the default data-plane stack.
    let available = std::net::TcpStream::connect("127.0.0.1:19180").is_ok();
    if !available {
        eprintln!(
            "integration_compose_monoengine_http_smoke requires compose monoengine at \
             127.0.0.1:19180; build/start with \
             `docker compose -p monoengine-it -f docker-compose.test.yml --profile app up -d --wait monoengine` \
             (see docs/refactoring/test-infra.md), skipping"
        );
        return;
    }

    let body = http_get_openapi();
    assert!(
        body.contains("openapi") || body.contains("paths") || body.contains('{'),
        "compose monoengine openapi body looked empty/unexpected: {body}"
    );
}

fn http_get_openapi() -> String {
    use std::{
        io::{Read, Write},
        net::TcpStream,
    };

    let mut stream = TcpStream::connect("127.0.0.1:19180").expect("connect compose monoengine");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let req =
        "GET /api/openapi.json HTTP/1.1\r\nHost: 127.0.0.1:19180\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    let text = String::from_utf8_lossy(&buf);
    let status_ok = text
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "));
    assert!(
        status_ok,
        "expected HTTP 200 from compose monoengine openapi; got:\n{text}"
    );
    text.into_owned()
}

#[test]
fn integration_object_storage_s3_compatible_smoke() {
    // 启动真实 RustFS 服务后，通过 `debug storage-smoke` 对 S3-compatible 后端
    // 执行 put/get/delete  round-trip，验证对象存储后端在 post-vault 启动路径
    // 正确解析 endpoint、bucket、credential 并完成真实 I/O。
    let rustfs_endpoint = "http://127.0.0.1:19000";
    let rustfs_available = std::net::TcpStream::connect("127.0.0.1:19000").is_ok();
    if !rustfs_available {
        eprintln!(
            "integration_object_storage_s3_compatible_smoke requires RustFS at {}; \
             run `docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait` \
             first (includes rustfs-init creating monoengine and monoui buckets), skipping",
            rustfs_endpoint
        );
        return;
    }

    let env = VaultCliEnv::new();
    let mut command = env.full_config_command();
    command
        // 覆盖为 S3-compatible（RustFS）配置。
        .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "s3compatible")
        .env("MEGA_OBJECT_STORAGE__S3__REGION", "us-east-1")
        .env("MEGA_OBJECT_STORAGE__S3__BUCKET", "monoengine")
        .env("MEGA_OBJECT_STORAGE__S3__ENDPOINT_URL", rustfs_endpoint)
        .env("MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID", "rustfs")
        .env(
            "MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY",
            "rustfs_secret",
        );

    command.args([
        "debug",
        "storage-smoke",
        "--key",
        "it-s3-smoke/test-object.bin",
    ]);
    let output = run(command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "debug storage-smoke should succeed against RustFS; stderr: {stderr}"
    );
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
    fs::write(&config_path, include_str!("../config/config.toml")).expect("write config");

    let port = reserve_free_port();
    let stdout_path = temp_dir.path().join("service.out");
    let stderr_path = temp_dir.path().join("service.err");
    // 端口 1 几乎必然拒绝连接，确保 DB 连接快速失败。
    let bad_db_url =
        format!("postgres://monoengine:{DB_SENTINEL}@127.0.0.1:1/monoengine_redaction");

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

#[test]
fn integration_error_redaction_bad_toml_does_not_leak_values() {
    // 坏 TOML 配置：进程必须在配置解析阶段 fail-closed，且错误输出只包含
    // 配置文件路径和字段路径，不包含哨兵 secret 值。这是 integration.md 场景 7
    // 中 "坏 TOML" bullet 的活进程门禁，把原本由 CI 脚本和单测分散覆盖的场景
    // 收拢到统一的 error_redaction 命名 gate。
    const TOML_SENTINEL: &str = "s3ntineltomlpw";

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let base_dir = temp_dir.path().join("base");
    let cache_dir = temp_dir.path().join("cache");
    let config_path = temp_dir.path().join("config.toml");

    // Write a config with a TOML syntax error: unterminated string value
    // that contains the sentinel. The parser must report the file path and
    // line/column but must not echo the raw sentinel value.
    let bad_toml = format!(
        r#"[database]
db_type = "postgres"
db_url = "postgres://monoengine:{TOML_SENTINEL}@127.0.0.1:5432/monoengine"

[redis]
url = "{TOML_SENTINEL}
# unterminated string above — TOML syntax error
"#
    );
    fs::write(&config_path, bad_toml).expect("write bad config");

    let mut command = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    command.arg("--config").arg(&config_path);
    command
        .env("MEGA_LOG__PRINT_STD", "true")
        .env("MEGA_LOG__WITH_ANSI", "false")
        .args(["config", "validate"]);

    let output = command.output().expect("run monoengine config validate");

    assert!(
        !output.status.success(),
        "config validate must fail on bad TOML"
    );

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The sentinel must not appear in the output.
    assert!(
        !combined.contains(TOML_SENTINEL),
        "TOML sentinel value leaked into output:\n{combined}"
    );

    // The config file path should be mentioned for diagnostics.
    assert!(
        combined.contains("config.toml") || combined.contains("config"),
        "error should reference the config file path:\n{combined}"
    );
}

// ===== P2 未来能力 gate：config init 黑盒用例 =====

#[test]
fn integration_config_init_creates_safe_skeleton_and_validates() {
    // `config init` 不应读取任何配置、不应连接数据库/Vault/Redis，只生成一份安全骨架配置。
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let base_dir = temp_dir.path().join("base");
    let cache_dir = temp_dir.path().join("cache");
    let output_path = temp_dir.path().join("init-config.toml");

    let mut init = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    init.args([
        "config",
        "init",
        "--output",
        output_path.to_str().expect("utf-8 output path"),
    ]);

    let output = run(init);
    let (stdout, stderr) = assert_success(&output);

    assert!(
        output_path.exists(),
        "config init should create the output file"
    );
    assert!(
        stdout.contains(&output_path.to_string_lossy().to_string()),
        "stdout should mention created path: {stdout}"
    );
    assert!(stderr.trim().is_empty(), "unexpected stderr: {stderr}");
    assert!(
        stdout.contains("config secret set object_storage.s3.secret_access_key"),
        "stdout should include S3 secret key setup guidance: {stdout}"
    );

    let content = fs::read_to_string(&output_path).expect("read init config");
    // 骨架配置为 Redis 提供安全的本地默认值，且不包含可复用的生产凭据。
    assert!(
        content.contains("url = \"redis://127.0.0.1:6379\""),
        "init config should include the local Redis URL"
    );
    assert!(
        !content.contains("password = "),
        "init config should not contain plaintext password ="
    );
    assert!(
        !content.contains("postgres://monoengine:"),
        "init config should not embed predictable postgres credentials"
    );

    // 生成的配置应能通过 config validate（不解析 secret），证明骨架本身是合法的。
    let mut validate = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    validate
        .arg("--config")
        .arg(&output_path)
        .args(["config", "validate"]);
    let output = run(validate);
    let (stdout, stderr) = assert_success(&output);
    assert_eq!(stdout.trim(), "config valid");
    assert!(stderr.trim().is_empty(), "unexpected stderr: {stderr}");

    // 不带 --force 再次写入同一文件应失败。
    let mut init_again = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    init_again.args([
        "config",
        "init",
        "--output",
        output_path.to_str().expect("utf-8 output path"),
    ]);
    let output = run(init_again);
    assert_failure(&output);

    // --force 应覆盖。
    let mut init_force = isolated_command(temp_dir.path(), &base_dir, &cache_dir);
    init_force.args([
        "config",
        "init",
        "--output",
        output_path.to_str().expect("utf-8 output path"),
        "--force",
    ]);
    let output = run(init_force);
    assert_success(&output);
    assert!(output_path.exists(), "force overwrite should keep the file");
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

fn wait_for_log_marker(path: &Path, marker: &str, timeout: Duration) -> String {
    // 轮询日志文件直到出现指定标记，避免固定 sleep 在 CI 负载下超时或等待过久。
    let deadline = Instant::now() + timeout;
    loop {
        let logs = read_log(path);
        if logs.contains(marker) {
            return logs;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for log marker {marker:?} in {path}\nlogs:\n{logs}",
                path = path.display()
            );
        }
        sleep(Duration::from_millis(200));
    }
}

fn reserve_free_port() -> u16 {
    // 绑定临时端口拿到端口号后立即释放，交给随后启动的 service 复用。
    // 单机集成测试里这个短暂的复用窗口可以接受。
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve free port");
    listener.local_addr().expect("listener local addr").port()
}

fn http_get(port: u16, path: &str) -> String {
    http_get_host("127.0.0.1", port, path)
}

#[cfg(feature = "fastcdc")]
fn http_get_authorization(port: u16, path: &str, authorization: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream
                    .set_write_timeout(Some(Duration::from_secs(15)))
                    .expect("set FastCDC authorized HTTP write timeout");
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .expect("set FastCDC authorized HTTP read timeout");
                let request = format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(request.as_bytes())
                    .expect("write FastCDC authorized HTTP request");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("read FastCDC authorized HTTP response");
                return response;
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!(
                        "failed to connect FastCDC authorized request on 127.0.0.1:{port}: {err}"
                    );
                }
                sleep(Duration::from_millis(200));
            }
        }
    }
}

fn http_get_host(host: &str, port: u16, path: &str) -> String {
    // 极简 HTTP/1.1 客户端，避免给 bin 测试 crate 引入 HTTP 客户端依赖；
    // `Connection: close` 让我们可以读到 EOF。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match TcpStream::connect((host, port)) {
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
                    "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
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
                    panic!("failed to GET {path} on {host}:{port}: {err}");
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
            // 先确认受测子进程仍活着，再接受端口可连；否则并发进程抢占该端口时会
            // 把别的服务误判为本 case 成功启动。
            if let Some(status) = self.child.try_wait().expect("poll service") {
                self.reaped = true;
                panic!(
                    "service exited before binding port {port} (status {status})\nstdout:\n{}\nstderr:\n{}",
                    read_log(stdout_path),
                    read_log(stderr_path),
                );
            }
            // `HTTP server started up` 是受测子进程在 bind 成功后写入其专属 stdout
            // 文件的 readiness signal。结合存活检查和端口探测，避免 reserve/free
            // port 的短暂 TOCTOU 窗口命中无关进程。
            if read_log(stdout_path).contains("HTTP server started up")
                && TcpStream::connect(("127.0.0.1", port)).is_ok()
            {
                return;
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
