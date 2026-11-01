//! FIX-MTR-16 acceptance: B3 trunk landing keeps the configured object
//! algorithm.
//!
//! Every scenario drives a real Docker Git client (the `git-cli` compose
//! fixture, local OrbStack) against a real Mega2 service and then verifies the
//! landed objects from the server side:
//!
//! * the cloned tip passes `git fsck --strict`, so every OID in the closure is
//!   the configured algorithm's digest of the canonical content (AC-1);
//! * the landed OID width equals the repository object format;
//! * an `n == 1` landing keeps the client tip byte-for-byte (AC-3..AC-6) and an
//!   `n > 1` landing produces a squash commit with the documented provenance;
//! * a rejected push leaves the ref where it was and the same worker still
//!   lands the next legal push (AC-2, AC-8).
//!
//! `#[path]` module reuse mirrors `tests/integration_native_hash.rs` so the
//! Docker Git and traces database helpers stay single-sourced.

#[allow(
    dead_code,
    reason = "shared Git helper also supplies SSH, LFS and write-auth probes"
)]
#[path = "common/git_cli.rs"]
mod git_cli;
#[path = "common/traces_db.rs"]
mod traces_db;

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use sea_orm::ConnectionTrait;

const PROCESS_LIMIT: Duration = Duration::from_secs(120);

/// A child process this target owns end to end.
struct OwnedChild(Option<Child>);

impl OwnedChild {
    fn spawn(command: &mut Command) -> Self {
        let child = Self(Some(command.spawn().expect("spawn native push fixture")));
        git_cli::record_service_pid(child.0.as_ref().expect("owned child").id());
        child
    }

    async fn require_success(&mut self) {
        let deadline = Instant::now() + PROCESS_LIMIT;
        loop {
            if let Some(status) = self
                .0
                .as_mut()
                .expect("owned child")
                .try_wait()
                .expect("poll fixture process")
            {
                self.0.take();
                assert!(
                    status.success(),
                    "native push fixture setup failed: {status}"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native push fixture setup exceeded {PROCESS_LIMIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            child.wait().expect("reap native push fixture process");
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    shared: tempfile::TempDir,
    database_url: String,
    object_format: &'static str,
    service: OwnedChild,
    port: u16,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.service.stop();
    }
}

impl Fixture {
    async fn new(db: &sea_orm::DatabaseConnection, object_format: &'static str) -> Self {
        assert!(!git_cli::git_cli_skip_requested(), "Docker Git is required");
        assert_ne!(
            std::env::var("MEGA2_IT_ALLOW_HOST_GIT").as_deref(),
            Ok("1"),
            "host Git is not the object-kind oracle"
        );
        git_cli::require_git_cli_runner();
        let work = git_cli::git_cli_workdir();
        fs::create_dir_all(&work).expect("Docker Git work root");
        let shared = tempfile::Builder::new()
            .prefix("native-push-hash-")
            .tempdir_in(work)
            .expect("private Git directory");
        let temp = tempfile::tempdir().expect("private native push fixture");
        let mut config: toml::Value = toml::from_str(
            &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"))
                .expect("read fixture config"),
        )
        .expect("parse fixture config");
        assert_eq!(
            config
                .get("cedar")
                .and_then(|v| v.get("enforcement"))
                .and_then(toml::Value::as_str),
            Some("off"),
            "fixture configuration changed: STOP/ER10"
        );
        assert!(
            !config
                .get("mst2")
                .and_then(|v| v.get("publication_enabled"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            "publication configuration changed: STOP/ER10"
        );
        eprintln!("FIX16_CONFIG: object_format={object_format}");
        config
            .get_mut("git")
            .and_then(toml::Value::as_table_mut)
            .expect("Git config")
            .remove("push_auth");
        fs::write(
            temp.path().join("config.toml"),
            toml::to_string(&config).expect("config TOML"),
        )
        .expect("write private config");
        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT current_database() AS name".to_owned(),
            ))
            .await
            .expect("database identity")
            .expect("database identity row");
        let name: String = row.try_get("", "name").expect("database name");
        let mut url = url::Url::parse(
            &std::env::var("MEGA_DATABASE__DB_URL").expect("source .env.test before this target"),
        )
        .expect("test database URL");
        url.set_path(&name);
        Self {
            temp,
            shared,
            database_url: url.to_string(),
            object_format,
            service: OwnedChild(None),
            port: 0,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mega2"));
        command.env_clear().current_dir(self.temp.path());
        for key in ["PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .arg("--config")
            .arg(self.temp.path().join("config.toml"))
            .env("MEGA_BASE_DIR", self.temp.path().join("base"))
            .env("MEGA_CACHE_DIR", self.temp.path().join("cache"))
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_DATABASE__DB_URL", &self.database_url)
            .env("MEGA_DATABASE__MAX_CONNECTION", "4")
            .env("MEGA_DATABASE__MIN_CONNECTION", "1")
            .env("MEGA_DATABASE__ACQUIRE_TIMEOUT", "5")
            .env("MEGA_DATABASE__CONNECT_TIMEOUT", "5")
            .env("MEGA_DATABASE__SQLX_LOGGING", "false")
            .env(
                "MEGA_REDIS__URL",
                std::env::var("MEGA_REDIS__URL").expect("test Redis URL"),
            )
            .env("MEGA_OBJECT_STORAGE__STORAGE_TYPE", "local")
            .env(
                "MEGA_OBJECT_STORAGE__LOCAL__ROOT_DIR",
                self.temp.path().join("objects"),
            )
            .env("MEGA_MONOREPO__OBJECT_FORMAT", self.object_format)
            .env("MEGA_MONOREPO__ROOT_DIRS", "third-party,project")
            .env("MEGA_MONOREPO__PUSH_POLICY", "trunk")
            .env("MEGA_GIT__PUSH_AUTH", "none")
            .env("MEGA_GIT__ANONYMOUS_ACCESS", "true")
            .env("MEGA_NOTIFICATION__ENABLED", "false")
            .env("MEGA_BUCK__ENABLE_SESSION_CLEANUP", "false")
            .env("MEGA_ARTIFACTS_GC__ENABLE", "false")
            .env("MEGA_VIEWS__ENABLED", "false")
            .env("MEGA_STORAGE_EVENTS__ENABLED", "false")
            .env("MEGA_LOG__PRINT_STD", "true")
            .env("MEGA_LOG__WITH_ANSI", "false")
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.temp.path().join("process.out"))
                    .expect("process stdout"),
            ))
            .stderr(Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.temp.path().join("process.err"))
                    .expect("process stderr"),
            ));
        command
    }

    async fn start(&mut self) {
        let mut init = self.command();
        init.args(["service", "init", "--yes"]);
        OwnedChild::spawn(&mut init).require_success().await;
        self.port = git_cli::reserve_ephemeral_port();
        git_cli::record_allocated_port(self.port);
        let mut command = self.command();
        command
            .args([
                "service",
                "http",
                "--host",
                git_cli::service_listen_host(),
                "-p",
            ])
            .arg(self.port.to_string());
        self.service = OwnedChild::spawn(&mut command);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("readiness client");
        let deadline = Instant::now() + PROCESS_LIMIT;
        loop {
            assert!(
                self.service
                    .0
                    .as_mut()
                    .expect("service")
                    .try_wait()
                    .expect("service status")
                    .is_none(),
                "service exited before readiness"
            );
            if let Ok(response) = client
                .get(git_cli::mega2_host_http_url(self.port, "/api/openapi.json"))
                .send()
                .await
                && response.status() == reqwest::StatusCode::OK
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "service readiness exceeded {PROCESS_LIMIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn stop(&mut self) {
        self.service.stop();
        git_cli::wait_until_port_closed(self.port, Duration::from_secs(5));
    }

    /// Object width for the configured algorithm.
    fn width(&self) -> usize {
        match self.object_format {
            "sha256" => 64,
            _ => 40,
        }
    }

    fn git(&self, args: &[&str]) -> std::process::Output {
        let output = git_cli::git_cli_no_auth(self.shared.path(), args);
        assert!(
            output.status.success(),
            "Docker Git {:?} failed: status={} stdout={} stderr={}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_text(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.git(args).stdout)
            .trim()
            .to_owned()
    }

    /// Clone `path` (e.g. `/project.git`) into `destination` and assert the
    /// advertised object format matches the fixture configuration.
    fn clone(&self, path: &str, destination: &str) {
        self.git(&[
            "-c",
            "protocol.version=0",
            "clone",
            "--no-checkout",
            &git_cli::mega2_http_url(self.port, &format!("{path}.git")),
            destination,
        ]);
        assert_eq!(
            self.git_text(&["-C", destination, "rev-parse", "--show-object-format"]),
            self.object_format,
            "FIX16 clone advertises the configured object format"
        );
    }

    fn commit(&self, repo: &str, file: &str, body: &str, message: &str) -> String {
        fs::write(self.shared.path().join(repo).join(file), body.as_bytes())
            .unwrap_or_else(|error| panic!("write {repo}/{file}: {error}"));
        // Docker Desktop / OrbStack bind mounts can lag behind a host write, so
        // retry staging until the index actually holds the change instead of
        // racing one `git add` against the mount.
        let mut staged = false;
        for _ in 0..40 {
            self.git(&["-C", repo, "add", "--all"]);
            // `diff --cached --quiet` exits 1 exactly when the index differs
            // from HEAD, i.e. the write reached the container and was staged.
            let probe = git_cli::git_cli_no_auth(
                self.shared.path(),
                &["-C", repo, "diff", "--cached", "--quiet"],
            );
            if !probe.status.success() {
                staged = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(staged, "FIX16 could not stage {repo}/{file}");
        self.git(&[
            "-C",
            repo,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            message,
        ]);
        self.git_text(&["-C", repo, "rev-parse", "HEAD"])
    }

    fn push(&self, repo: &str, path: &str, refspec: &str, force: bool) -> std::process::Output {
        let remote = git_cli::mega2_http_url(self.port, &format!("{path}.git"));
        let mut args = vec!["-C", repo, "push"];
        if force {
            args.push("--force");
        }
        args.push(&remote);
        args.push(refspec);
        git_cli::git_cli_no_auth(self.shared.path(), &args)
    }

    /// Assert the landed tip equals `expected` and its whole closure is
    /// self-consistent under the configured algorithm.
    fn assert_landed(&self, path: &str, repo: &str, expected: &str) {
        self.git(&["-C", repo, "fetch", "--no-tags", "origin"]);
        let remote_tip = self.git_text(&["-C", repo, "rev-parse", "origin/main"]);
        assert_eq!(
            remote_tip, expected,
            "FIX16_AC1 origin/main must equal the expected landed tip"
        );
        self.git(&["-C", repo, "checkout", "--detach", expected]);
        let head = self.git_text(&["-C", repo, "rev-parse", "HEAD"]);
        assert_eq!(head, expected, "FIX16_AC1 landed tip");
        assert_eq!(
            head.len(),
            self.width(),
            "FIX16_AC1 landed tip width for {}",
            self.object_format
        );
        assert!(
            head.bytes().all(|b| b.is_ascii_hexdigit()),
            "FIX16_AC1 landed tip is hex"
        );
        let fsck = self.git(&["-C", repo, "fsck", "--strict"]);
        assert!(
            fsck.status.success(),
            "FIX16_AC1 {} closure must pass strict fsck",
            path
        );

        // Both the `/project` ancestor clone and the monorepo root "/" clone
        // were taken before provisioning and pushing, so refreshing them here
        // proves the ancestor/root commits this landing materialized are well
        // formed, and not only the pushed subtree.
        for repo in ["root", "monorepo"] {
            self.git(&["-C", repo, "fetch", "--no-tags", "origin"]);
            let fsck = self.git(&["-C", repo, "fsck", "--strict"]);
            assert!(
                fsck.status.success(),
                "FIX16_AC1 {repo} closure must pass strict fsck after landing {path}"
            );
        }
    }

    /// Provision `path` (and missing parents) so a trunk create can land.
    ///
    /// A trunk create on a never-provisioned path is refused with
    /// `MONO_PATH_UNINITIALIZED`, which is product behaviour, not a landing
    /// defect; ADR-FU-05 exposes `POST /path/provision` for it.
    async fn provision(&self, path: &str) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("provision client");
        let response = client
            .post(git_cli::mega2_host_http_url(
                self.port,
                "/api/v1/path/provision",
            ))
            .json(&serde_json::json!({ "path": path }))
            .send()
            .await
            .expect("provision request");
        assert!(
            response.status().is_success(),
            "FIX16 provision {path} failed: {} body={} server_log={}",
            response.status(),
            response.text().await.unwrap_or_default(),
            self.tail_server_log()
        );
    }

    /// Last lines of the fixture's server stderr, for a real failure diagnosis.
    fn tail_server_log(&self) -> String {
        let mut out = String::new();
        for name in ["process.out", "process.err"] {
            let log = fs::read_to_string(self.temp.path().join(name)).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            out.push_str(&lines[lines.len().saturating_sub(25)..].join(" | "));
            out.push(' ');
        }
        out
    }

    fn cat_commit(&self, repo: &str, oid: &str) -> String {
        String::from_utf8_lossy(&self.git(&["-C", repo, "cat-file", "commit", oid]).stdout)
            .to_string()
    }

    fn parents(&self, repo: &str, oid: &str) -> Vec<String> {
        let raw = self.git_text(&["-C", repo, "rev-list", "--parents", "-n", "1", oid]);
        raw.split_whitespace().skip(1).map(str::to_owned).collect()
    }
}

/// Create-only landing: provision `path`, clone it, then land the first commit.
async fn create_scenario(object_format: &'static str, path: &str) {
    let (_database, db) = traces_db::TracesDb::migrated().await;
    let mut fixture = Fixture::new(&db, object_format).await;
    fixture.start().await;
    // `/project.git` must exist before a child path can be provisioned.
    fixture.clone("/project", "root");
    fixture.clone("/", "monorepo");
    fixture.provision(path).await;
    fixture.clone(path, "work");

    let tip = fixture.commit("work", "create.txt", "FIX16 create\n", "feat: FIX16 create");
    assert_eq!(tip.len(), fixture.width(), "FIX16 source tip width");

    let push = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        push.status.success(),
        "FIX16 first-landing push failed for {object_format}: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    fixture.clone(path, "landed");
    fixture.assert_landed(path, "landed", &tip);
    // AC-3..AC-6: an n == 1 landing keeps the client commit verbatim.
    assert_eq!(
        fixture.cat_commit("landed", &tip),
        fixture.cat_commit("work", &tip),
        "FIX16_AC3/4/5/6 landing must preserve the client commit"
    );
    println!("FIX16_CREATE {object_format} {path} {tip} PASS");
    fixture.stop();
    db.close().await.expect("close fixture pool");
}

/// Update, multi-commit squash, policy rejection and recovery on one path.
async fn matrix_scenario(object_format: &'static str, path: &str) {
    let (_database, db) = traces_db::TracesDb::migrated().await;
    let mut fixture = Fixture::new(&db, object_format).await;
    fixture.start().await;
    fixture.clone("/project", "root");
    fixture.clone("/", "monorepo");
    fixture.provision(path).await;
    fixture.clone(path, "work");

    // 1) First landing on the provisioned path.
    let first = fixture.commit("work", "app.txt", "one\n", "feat: FIX16 one");
    let push = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        push.status.success(),
        "FIX16 initial push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    fixture.clone(path, "landed");
    fixture.assert_landed(path, "landed", &first);

    // 2) Single-commit update keeps the client commit and its parent edge.
    let second = fixture.commit("work", "app.txt", "two\n", "fix: FIX16 two");
    let push = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        push.status.success(),
        "FIX16 update push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    fixture.git(&["-C", "landed", "fetch", "--no-tags", "origin"]);
    fixture.assert_landed(path, "landed", &second);
    assert_eq!(
        fixture.parents("landed", &second),
        fixture.parents("work", &second),
        "FIX16_AC5 update keeps the trunk parent edge"
    );

    // 3) Two commits in one push land as a squash commit whose message follows
    //    the provenance rule and whose author is the client tip author.
    let _third = fixture.commit("work", "app.txt", "three\n", "feat: FIX16 three");
    let fourth = fixture.commit("work", "app.txt", "four\n", "feat: FIX16 four");
    let push = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        push.status.success(),
        "FIX16 squash push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    fixture.git(&["-C", "landed", "fetch", "--no-tags", "origin"]);
    let landed_tip = fixture.git_text(&["-C", "landed", "rev-parse", "origin/main"]);
    assert_eq!(
        landed_tip.len(),
        fixture.width(),
        "FIX16_AC1 squash tip width for {object_format}"
    );
    fixture.assert_landed(path, "landed", &landed_tip);
    let landed_commit = fixture.cat_commit("landed", &landed_tip);
    assert!(
        landed_commit.contains("Squash 2 commits at"),
        "FIX16_AC6 squash message: {landed_commit}"
    );
    let src_tip_commit = fixture.cat_commit("work", &fourth);
    let field = |raw: &str, prefix: &str| {
        raw.lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        field(&landed_commit, "author "),
        field(&src_tip_commit, "author "),
        "FIX16_AC3 squash author follows the tip author"
    );
    assert!(
        field(&landed_commit, "committer ").contains("Fixture"),
        "FIX16_AC4 squash committer keeps the trunk rule"
    );
    assert!(
        !landed_commit.contains("four\nfour"),
        "FIX16 squash must not duplicate bodies"
    );

    // The server replaced commits three/four with one squash commit, so the
    // client branch is now divergent. Resynchronize it before the recovery
    // push; otherwise Git would refuse the next push as a non-fast-forward and
    // the AC-8 recovery would be untested.
    fixture.git(&["-C", "work", "fetch", "--no-tags", "origin"]);
    fixture.git(&["-C", "work", "reset", "--hard", "origin/main"]);
    assert_eq!(
        fixture.git_text(&["-C", "work", "rev-parse", "HEAD"]),
        landed_tip,
        "FIX16 resynced client branch must sit on the landed squash"
    );

    // 4) AC-2/AC-8: a creation outside the configured root dirs is rejected, the
    //    landed ref keeps its pre-transaction value, and the same worker still
    //    lands the next legal push.
    let rejected = fixture.push(
        "work",
        "/other/fix16-outside",
        "HEAD:refs/heads/main",
        false,
    );
    println!(
        "FIX16_REJECT status={} first_line={}",
        rejected.status,
        String::from_utf8_lossy(&rejected.stderr)
            .lines()
            .next()
            .unwrap_or("")
    );
    assert!(
        !rejected.status.success(),
        "FIX16_AC2 a creation outside the configured root dirs must be rejected"
    );
    fixture.git(&["-C", "landed", "fetch", "--no-tags", "origin"]);
    assert_eq!(
        fixture.git_text(&["-C", "landed", "rev-parse", "origin/main"]),
        landed_tip,
        "FIX16_AC2 rejection must keep the pre-transaction ref snapshot"
    );

    let legal = fixture.commit("work", "app.txt", "five\n", "fix: FIX16 legal after reject");
    let recovered = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        recovered.status.success(),
        "FIX16_AC8 the worker must land the next legal push: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    fixture.git(&["-C", "landed", "fetch", "--no-tags", "origin"]);
    let after = fixture.git_text(&["-C", "landed", "rev-parse", "origin/main"]);
    assert_eq!(after, legal, "FIX16_AC8 landed tip after recovery");
    fixture.assert_landed(path, "landed", &after);

    println!("FIX16_MATRIX {object_format} {path} PASS");
    fixture.stop();
    db.close().await.expect("close fixture pool");
}

/// VER-1 RED target: SHA-256 first-landing.
#[tokio::test]
async fn hash_red_sha256_create() {
    create_scenario("sha256", "/project/fix16-create").await;
}

/// VER-1 RED target: SHA-256 update landing.
#[tokio::test]
async fn hash_red_sha256_update() {
    matrix_scenario("sha256", "/project/fix16-update").await;
}

#[tokio::test]
async fn native_push_hash_sha1_matrix() {
    create_scenario("sha1", "/project/fix16-sha1-create").await;
    matrix_scenario("sha1", "/project/fix16-sha1").await;
}

#[tokio::test]
async fn native_push_hash_sha256_matrix() {
    create_scenario("sha256", "/project/fix16-sha256-create").await;
    matrix_scenario("sha256", "/project/fix16-sha256").await;
}

/// AC-4's client-visible surface on a native Monorepo path: over v2 a
/// `blob:none` clone must withhold every blob while still resolving the tip,
/// and the objects it holds must be exactly the full closure minus the blobs.
async fn filtered_clone_scenario(object_format: &'static str, path: &str) {
    let (_database, db) = traces_db::TracesDb::migrated().await;
    let mut fixture = Fixture::new(&db, object_format).await;
    fixture.start().await;
    fixture.clone("/project", "root");
    fixture.clone("/", "monorepo");
    fixture.provision(path).await;
    fixture.clone(path, "work");
    fixture.clone(path, "full");

    let tip = fixture.commit("work", "filter.txt", "FIX13 filter\n", "feat: FIX13 filter");
    let push = fixture.push("work", path, "HEAD:refs/heads/main", false);
    assert!(
        push.status.success(),
        "FIX13_AC4 seeding push failed for {object_format}: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let url = git_cli::mega2_http_url(fixture.port, &format!("{path}.git"));
    fixture.git(&[
        "-c",
        "protocol.version=2",
        "clone",
        "--no-checkout",
        "--filter=blob:none",
        &url,
        "blobless",
    ]);

    assert_eq!(
        fixture.git_text(&["-C", "blobless", "rev-parse", "HEAD"]),
        tip,
        "FIX13_AC4 a filtered clone must still resolve the landed tip"
    );

    let blobless_types = fixture.git_text(&[
        "-C",
        "blobless",
        "cat-file",
        "--batch-check=%(objecttype)",
        "--batch-all-objects",
    ]);
    let blobless_blobs = blobless_types
        .lines()
        .filter(|kind| kind.trim() == "blob")
        .count();
    // Count only objects that are actually present: `rev-list --objects` keeps
    // listing blob OIDs read out of trees even when those blobs were filtered
    // away, so it overcounts in a partial clone.
    let blobless_total = blobless_types.lines().count();

    // The full clone is the reference closure for this path.
    fixture.git(&["-C", "full", "fetch", "--no-tags", "origin"]);
    fixture.git(&["-C", "full", "reset", "--hard", "origin/main"]);
    let full_types = fixture.git_text(&[
        "-C",
        "full",
        "cat-file",
        "--batch-check=%(objecttype)",
        "--batch-all-objects",
    ]);
    let full_total = full_types.lines().count();
    let full_blobs = full_types
        .lines()
        .filter(|kind| kind.trim() == "blob")
        .count();

    println!(
        "FIX13_AC4_FILTERED {object_format} blobless_blobs={blobless_blobs} blobless={blobless_total} full={full_total} full_blobs={full_blobs}"
    );
    assert_eq!(
        blobless_blobs, 0,
        "FIX13_AC4 a v2 blob:none clone must hold no blob objects"
    );
    assert_eq!(
        blobless_total,
        full_total - full_blobs,
        "FIX13_AC4 the filtered closure must be the full closure minus the blobs"
    );

    println!("FIX13_AC4_FILTERED {object_format} {path} PASS");
    fixture.stop();
    db.close().await.expect("close fixture pool");
}

/// AC-4 over the native Monorepo path, both configured algorithms.
#[tokio::test]
async fn native_filtered_clone_withholds_blobs() {
    filtered_clone_scenario("sha1", "/project/fix13-sha1-filter").await;
    filtered_clone_scenario("sha256", "/project/fix13-sha256-filter").await;
}
