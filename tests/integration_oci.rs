//! Process-level OCI Distribution API black-box IT (plan-20260902 / DR-12).
//!
//! Boots a real `service http` under storage-only + `[oci] enabled = true`, then
//! drives `/v2` with raw HTTP (reqwest). Three filterable groups:
//! - `integration_oci_auth_matrix`
//! - `integration_oci_protocol_walkthrough`
//! - `integration_oci_docker_gated` (SKIP when docker daemon is unavailable)

mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const DEFAULT_POSTGRES_URL: &str =
    "postgres://monoengine:monoengine_test_password@127.0.0.1:15432/monoengine";
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:16379";

const PUSH_TOKEN: &str = "oci-it-push-token";
const REPO: &str = "team/image";
const MOUNT_REPO: &str = "team/app";

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
            "monoengine_oci_{}_{}",
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

struct OciEnv {
    temp_dir: TempDir,
    database: TestDatabase,
    full_config_path: PathBuf,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    object_root: PathBuf,
}

impl OciEnv {
    fn with_config_append(append: &str) -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let case_name = format!(
            "oci-case-{}-{}",
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
fn integration_oci_auth_matrix() {
    // Token morphology: unauth write 401; Basic write success; out-of-path DENIED.
    {
        let env = OciEnv::with_config_append(&token_oci_append(true));
        let (mut service, port, _stdout, stderr) =
            boot_service_http(&env, &[("MEGA_GIT__PUSH_AUTH", "token")]);
        let base = registry_base(port);
        let client = http_client();

        let unauth = client
            .post(format!("{base}/{REPO}/blobs/uploads/"))
            .send()
            .expect("unauth write");
        assert_eq!(
            unauth.status().as_u16(),
            401,
            "token morphology: unauthenticated write must 401"
        );

        let basic = client
            .post(format!("{base}/{REPO}/blobs/uploads/"))
            .header("Authorization", basic_auth("any", PUSH_TOKEN))
            .send()
            .expect("basic write");
        assert_eq!(
            basic.status().as_u16(),
            202,
            "token morphology: Basic credentials write must succeed"
        );

        let denied = client
            .post(format!("{base}/other/image/blobs/uploads/"))
            .header("Authorization", basic_auth("any", PUSH_TOKEN))
            .send()
            .expect("denied write");
        assert_eq!(
            denied.status().as_u16(),
            403,
            "valid token not covering repo must be DENIED"
        );
        let denied_body = denied.text().expect("denied body");
        assert!(
            denied_body.contains("DENIED"),
            "DENIED envelope missing: {denied_body}"
        );

        assert!(
            service
                .shutdown_via_sigint(Duration::from_secs(60))
                .success(),
            "shutdown failed\n{}",
            read_log(&stderr)
        );
    }

    // push_auth=none: unauthenticated write allowed.
    {
        let env = OciEnv::with_config_append(
            r#"
[git]
anonymous_access = true
push_auth = "none"
ssh_receive_pack = false

[oci]
enabled = true
"#,
        );
        let (mut service, port, _stdout, stderr) =
            boot_service_http(&env, &[("MEGA_GIT__PUSH_AUTH", "none")]);
        let base = registry_base(port);
        let client = http_client();

        let allowed = client
            .post(format!("{base}/{REPO}/blobs/uploads/"))
            .send()
            .expect("none write");
        assert_eq!(
            allowed.status().as_u16(),
            202,
            "push_auth=none must allow unauthenticated write"
        );

        assert!(
            service
                .shutdown_via_sigint(Duration::from_secs(60))
                .success(),
            "shutdown failed\n{}",
            read_log(&stderr)
        );
    }

    // anonymous_access=false: read 401 without token; token read success.
    {
        let env = OciEnv::with_config_append(&token_oci_append(false));
        let (mut service, port, _stdout, stderr) = boot_service_http(
            &env,
            &[
                ("MEGA_GIT__PUSH_AUTH", "token"),
                ("MEGA_GIT__ANONYMOUS_ACCESS", "false"),
            ],
        );
        let base = registry_base(port);
        let client = http_client();

        let unauth_ping = client.get(format!("{base}/")).send().expect("unauth ping");
        assert_eq!(
            unauth_ping.status().as_u16(),
            401,
            "anonymous_access=false: unauthenticated read must 401"
        );

        let auth_ping = client
            .get(format!("{base}/"))
            .header("Authorization", basic_auth("any", PUSH_TOKEN))
            .send()
            .expect("auth ping");
        assert_eq!(
            auth_ping.status().as_u16(),
            200,
            "anonymous_access=false: token read must succeed"
        );

        assert!(
            service
                .shutdown_via_sigint(Duration::from_secs(60))
                .success(),
            "shutdown failed\n{}",
            read_log(&stderr)
        );
    }
}

#[test]
fn integration_oci_protocol_walkthrough() {
    let env = OciEnv::with_config_append(&token_oci_append(true));
    let (mut service, port, _stdout, stderr) =
        boot_service_http(&env, &[("MEGA_GIT__PUSH_AUTH", "token")]);
    let base = registry_base(port);
    let client = http_client();
    let auth = basic_auth("any", PUSH_TOKEN);

    // Anonymous registry ping (anonymous_access=true).
    let ping = client.get(format!("{base}/")).send().expect("ping");
    assert_eq!(ping.status().as_u16(), 200, "anonymous ping");
    assert_eq!(
        ping.headers()
            .get("Docker-Distribution-API-Version")
            .and_then(|v| v.to_str().ok()),
        Some("registry/2.0")
    );

    // Config blob via monolithic upload.
    let config_bytes = br#"{"architecture":"amd64","os":"linux"}"#;
    let config_digest = sha256_digest(config_bytes);
    let mono_config = client
        .post(format!(
            "{base}/{REPO}/blobs/uploads/?digest={config_digest}"
        ))
        .header("Authorization", &auth)
        .header("Content-Length", config_bytes.len().to_string())
        .body(config_bytes.to_vec())
        .send()
        .expect("monolithic config");
    assert_eq!(
        mono_config.status().as_u16(),
        201,
        "monolithic upload must create blob"
    );

    // Layer via chunked upload (POST → PATCH → PUT ?digest=).
    let layer_bytes = b"oci-it-layer-chunked";
    let layer_digest = sha256_digest(layer_bytes);
    let init = client
        .post(format!("{base}/{REPO}/blobs/uploads/"))
        .header("Authorization", &auth)
        .send()
        .expect("init upload");
    assert_eq!(init.status().as_u16(), 202, "chunked init");
    let uuid = init
        .headers()
        .get("Docker-Upload-UUID")
        .and_then(|v| v.to_str().ok())
        .expect("upload uuid")
        .to_owned();
    let patch = client
        .request(
            reqwest::Method::PATCH,
            format!("{base}/{REPO}/blobs/uploads/{uuid}"),
        )
        .header("Authorization", &auth)
        .header("Content-Length", layer_bytes.len().to_string())
        .body(layer_bytes.to_vec())
        .send()
        .expect("patch chunk");
    assert_eq!(patch.status().as_u16(), 202, "chunked patch");
    let complete = client
        .put(format!(
            "{base}/{REPO}/blobs/uploads/{uuid}?digest={layer_digest}"
        ))
        .header("Authorization", &auth)
        .send()
        .expect("complete upload");
    assert_eq!(complete.status().as_u16(), 201, "chunked complete");

    // Mount blob into another multi-segment repo under the same token prefix.
    let mount = client
        .post(format!(
            "{base}/{MOUNT_REPO}/blobs/uploads/?mount={layer_digest}&from={REPO}"
        ))
        .header("Authorization", &auth)
        .send()
        .expect("mount");
    assert_eq!(mount.status().as_u16(), 201, "mount must succeed");

    // Manifest PUT / GET / 304.
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_digest}","size":{}}}]}}"#,
        config_bytes.len(),
        layer_bytes.len()
    );
    let put_manifest = client
        .put(format!("{base}/{REPO}/manifests/v1"))
        .header("Authorization", &auth)
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.clone())
        .send()
        .expect("put manifest");
    assert_eq!(put_manifest.status().as_u16(), 201, "manifest PUT");
    let manifest_digest = put_manifest
        .headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .expect("manifest digest")
        .to_owned();

    let get_manifest = client
        .get(format!("{base}/{REPO}/manifests/v1"))
        .send()
        .expect("get manifest");
    assert_eq!(get_manifest.status().as_u16(), 200, "manifest GET");
    assert_eq!(get_manifest.text().expect("body"), manifest);

    let not_modified = client
        .get(format!("{base}/{REPO}/manifests/v1"))
        .header("If-None-Match", &manifest_digest)
        .send()
        .expect("manifest 304");
    assert_eq!(
        not_modified.status().as_u16(),
        304,
        "manifest If-None-Match"
    );

    // Blob GET + Range 206.
    let blob = client
        .get(format!("{base}/{REPO}/blobs/{layer_digest}"))
        .send()
        .expect("blob get");
    assert_eq!(blob.status().as_u16(), 200, "blob GET");
    assert_eq!(blob.bytes().expect("bytes").as_ref(), layer_bytes);

    let ranged = client
        .get(format!("{base}/{REPO}/blobs/{layer_digest}"))
        .header("Range", "bytes=0-3")
        .send()
        .expect("blob range");
    assert_eq!(ranged.status().as_u16(), 206, "blob Range");
    assert_eq!(
        ranged.bytes().expect("range bytes").as_ref(),
        &layer_bytes[..4]
    );

    // tags/list
    let tags = client
        .get(format!("{base}/{REPO}/tags/list"))
        .send()
        .expect("tags list");
    assert_eq!(tags.status().as_u16(), 200, "tags/list");
    let tags_json: serde_json::Value = tags.json().expect("tags json");
    assert_eq!(tags_json["name"], REPO);
    let tag_list = tags_json["tags"].as_array().expect("tags array");
    assert!(
        tag_list.iter().any(|t| t.as_str() == Some("v1")),
        "tags/list must include v1: {tags_json}"
    );

    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

#[test]
fn integration_oci_docker_gated() {
    match docker_daemon_probe() {
        Ok(()) => {}
        Err(reason) => {
            eprintln!("SKIP: docker daemon unavailable ({reason})");
            return;
        }
    }

    let env = OciEnv::with_config_append(&token_oci_append(true));
    let (mut service, port, _stdout, stderr) =
        boot_service_http(&env, &[("MEGA_GIT__PUSH_AUTH", "token")]);
    let registry = format!("127.0.0.1:{port}");
    let image = format!("{registry}/{REPO}:docker-it");

    let work = tempfile::tempdir().expect("docker workdir");
    let rootfs = work.path().join("rootfs");
    fs::create_dir_all(&rootfs).expect("rootfs");
    fs::write(rootfs.join("hello"), b"oci-docker-it\n").expect("hello");
    let tar_path = work.path().join("rootfs.tar");
    let tar_status = Command::new("tar")
        .args(["-cf"])
        .arg(&tar_path)
        .arg("-C")
        .arg(&rootfs)
        .arg(".")
        .status()
        .expect("tar");
    assert!(tar_status.success(), "tar rootfs");

    let import = Command::new("docker")
        .args(["import", &tar_path.to_string_lossy(), &image])
        .output()
        .expect("docker import");
    assert!(
        import.status.success(),
        "docker import failed: {}",
        sanitize_output(&import.stderr)
    );

    let mut login = Command::new("docker")
        .args(["login", "-u", "oci-it", "--password-stdin", &registry])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn docker login");
    {
        let mut stdin = login.stdin.take().expect("login stdin");
        stdin
            .write_all(PUSH_TOKEN.as_bytes())
            .expect("write login password");
        stdin.write_all(b"\n").expect("write login newline");
    }
    let login_out = login.wait_with_output().expect("docker login wait");
    assert!(
        login_out.status.success(),
        "docker login failed: {}",
        sanitize_output(&login_out.stderr)
    );

    let push = Command::new("docker")
        .args(["push", &image])
        .output()
        .expect("docker push");
    assert!(
        push.status.success(),
        "docker push failed: {}",
        sanitize_output(&push.stderr)
    );

    let _ = Command::new("docker").args(["rmi", "-f", &image]).output();
    let pull = Command::new("docker")
        .args(["pull", &image])
        .output()
        .expect("docker pull");
    assert!(
        pull.status.success(),
        "docker pull failed: {}",
        sanitize_output(&pull.stderr)
    );

    let _ = Command::new("docker").args(["rmi", "-f", &image]).output();
    let _ = Command::new("docker").args(["logout", &registry]).output();

    assert!(
        service
            .shutdown_via_sigint(Duration::from_secs(60))
            .success(),
        "shutdown failed\n{}",
        read_log(&stderr)
    );
}

fn token_oci_append(anonymous_access: bool) -> String {
    format!(
        r#"
[git]
anonymous_access = {anonymous_access}
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "oci-it"
token = "{PUSH_TOKEN}"
paths = ["/team"]

[oci]
enabled = true
"#
    )
}

fn boot_service_http(
    env: &OciEnv,
    extra_env: &[(&str, &str)],
) -> (ServiceProcess, u16, PathBuf, PathBuf) {
    let port = reserve_free_port();
    let stdout_path = env.temp_dir.path().join(format!("service-{port}.out"));
    let stderr_path = env.temp_dir.path().join(format!("service-{port}.err"));

    let mut command = env.full_config_command();
    command.env("MEGA_LOG__PRINT_STD", "true");
    command.env(
        "MEGA_HTTP__PUBLIC_BASE_URL",
        format!("http://127.0.0.1:{port}"),
    );
    for (key, value) in extra_env {
        command.env(*key, *value);
    }
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

fn registry_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v2")
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}

fn basic_auth(user: &str, token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{token}")))
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn docker_daemon_probe() -> Result<(), String> {
    let mut command = Command::new("docker");
    command.args(["info"]);
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());
    let output = command.output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            "docker binary not found".to_owned()
        } else {
            format!("failed to spawn docker: {}", err.kind())
        }
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = sanitize_output(&output.stderr);
        let reason = if detail.is_empty() {
            format!("docker info exited {}", output.status)
        } else {
            format!("docker info failed: {detail}")
        };
        Err(reason)
    }
}

fn sanitize_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.replace(PUSH_TOKEN, "<redacted>")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join(" | ")
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
