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

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

const PROCESS_LIMIT: Duration = Duration::from_secs(90);

struct OwnedChild(Option<Child>);

impl OwnedChild {
    fn spawn(command: &mut Command) -> Self {
        let child = Self(Some(command.spawn().expect("spawn native hash fixture")));
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
                assert!(status.success(), "native fixture setup failed: {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native fixture setup exceeded 90s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            child.wait().expect("reap native fixture process");
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
            "host Git is not the native hash oracle"
        );
        git_cli::require_git_cli_runner();
        let work = git_cli::git_cli_workdir();
        fs::create_dir_all(&work).expect("Docker Git work root");
        let shared = tempfile::Builder::new()
            .prefix("native-hash-")
            .tempdir_in(work)
            .expect("private Git directory");
        let temp = tempfile::tempdir().expect("private native fixture");
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
            "FIX15 fixture configuration changed: STOP/ER10"
        );
        assert!(
            !config
                .get("mst2")
                .and_then(|v| v.get("publication_enabled"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            "FIX15 publication configuration changed: STOP/ER10"
        );
        eprintln!(
            "FIX15_CONFIG: cedar.enforcement=off mst2.publication_enabled=false object_format={object_format}"
        );
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
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
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
            assert!(Instant::now() < deadline, "service readiness exceeded 90s");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn require_clone(&self, protocol: &str, path: &str, destination: &str) {
        let output = git_cli::git_cli_no_auth(
            self.shared.path(),
            &[
                "-c",
                &format!("protocol.version={protocol}"),
                "clone",
                "--no-checkout",
                &git_cli::mega2_http_url(self.port, path),
                destination,
            ],
        );
        if !output.status.success() {
            let log =
                fs::read_to_string(self.temp.path().join("process.err")).expect("service stderr");
            let lines: Vec<_> = log.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.contains("panicked at src/jupiter/utils/converter.rs") {
                    eprintln!("FIX15_NATIVE_HASH_PANIC: {line}");
                    if let Some(error) = lines.get(i + 1) {
                        eprintln!("{error}");
                    }
                }
            }
        }
        assert!(
            output.status.success(),
            "FIX15_NATIVE_CLONE_FAILED: {}",
            output.status
        );
    }

    fn stop(&mut self) {
        self.service.stop();
        git_cli::wait_until_port_closed(self.port, Duration::from_secs(5));
    }
}

#[tokio::test]
async fn native_hash_sha256_materialize() {
    let (_database, db) = traces_db::TracesDb::migrated().await;
    let mut fixture = Fixture::new(&db, "sha256").await;
    fixture.start().await;
    fixture.require_clone("0", "/project.git", "project");
    let output = git_cli::git_cli_no_auth(
        fixture.shared.path(),
        &["-C", "project", "fsck", "--strict"],
    );
    assert!(
        output.status.success(),
        "native SHA256 objects must pass strict Git fsck"
    );
    fixture.stop();
    db.close().await.expect("close fixture pool");
}

mod native_matrix {
    use std::{
        collections::HashMap,
        io::Cursor,
        sync::{Arc, Mutex},
    };

    use bytes::Bytes;
    use git_internal::{
        hash::{HashKind, ObjectHash},
        internal::{
            object::{
                ObjectTrait,
                blob::Blob,
                commit::Commit,
                signature::Signature,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
            pack::Pack,
        },
    };
    use mega2_core::{
        mega_blob, mega_commit, mega_refs, mega_tree,
        orbit::factory::ObjectStorageFactory,
        orbit_api::{
            factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
            object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
        },
    };
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, Set};

    use super::*;

    type Objects = HashMap<ObjectHash, (ObjectType, Vec<u8>)>;
    struct Seed {
        generations: Vec<Objects>,
        subtree: Objects,
        tip: ObjectHash,
        subtree_tip: ObjectHash,
    }

    impl Fixture {
        fn matrix_git(&self, args: &[&str], capture: Option<&str>) -> Vec<u8> {
            let alias = match capture {
                Some(case) => format!(
                    "alias.nativehash=!GIT_NO_LAZY_FETCH=1 GIT_TRACE_PACKET=\"$PWD/{case}.packet\" GIT_TRACE_PACKFILE=\"$PWD/{case}.pack\" git"
                ),
                None => "alias.nativehash=!GIT_NO_LAZY_FETCH=1 git".to_owned(),
            };
            let mut command = vec!["-c", &alias, "nativehash"];
            command.extend_from_slice(args);
            let output = git_cli::git_cli_no_auth(self.shared.path(), &command);
            if let Some(case) = capture {
                for (extension, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
                    fs::write(
                        self.shared.path().join(format!("{case}.{extension}")),
                        bytes,
                    )
                    .expect("capture Git output");
                }
                if let Some(directory) = std::env::var_os("MEGA2_IT_NATIVE_HASH_EVIDENCE_DIR") {
                    let directory = std::path::PathBuf::from(directory);
                    fs::create_dir_all(&directory).expect("matrix original directory");
                    for extension in ["pack", "packet", "stdout", "stderr"] {
                        let filename = format!("{case}.{extension}");
                        let source = self.shared.path().join(&filename);
                        if source.exists() {
                            fs::copy(source, directory.join(filename))
                                .expect("preserve original before assertions");
                        }
                    }
                }
            }
            assert!(
                output.status.success(),
                "native matrix Git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        async fn seed_matrix(&self, db: &sea_orm::DatabaseConnection, kind: HashKind) -> Seed {
            let cfg = ObjectStorageConfig {
                storage_type: ObjectStorageBackend::Local,
                local: LocalConfig {
                    root_dir: self
                        .temp
                        .path()
                        .join("objects")
                        .to_string_lossy()
                        .into_owned(),
                },
                s3: Default::default(),
                gcs: Default::default(),
            };
            let objects = ObjectStorageFactory::build(&cfg)
                .await
                .expect("private object storage");
            let author =
                Signature::from_data(b"author Fixture <native@example.invalid> 1 +0000".to_vec())
                    .expect("author");
            let committer = Signature::from_data(
                b"committer Fixture <native@example.invalid> 1 +0000".to_vec(),
            )
            .expect("committer");
            let mut commits: Vec<Commit> = Vec::new();
            let mut generations = Vec::new();
            let mut subtree_objects = HashMap::new();
            let mut subtree_tip = ObjectHash::zero_for_kind(kind);
            for n in 0..2 {
                let blob = Blob::from_content_bytes_with_kind(
                    kind,
                    format!("native matrix generation {n}\n").into_bytes(),
                )
                .expect("explicit blob");
                let subtree = Tree::from_tree_items_with_kind(
                    kind,
                    vec![TreeItem::new(
                        TreeItemMode::Blob,
                        blob.id,
                        "data.txt".to_owned(),
                    )],
                )
                .expect("explicit subtree");
                let root = Tree::from_tree_items_with_kind(
                    kind,
                    vec![TreeItem::new(
                        TreeItemMode::Tree,
                        subtree.id,
                        "project".to_owned(),
                    )],
                )
                .expect("explicit root");
                let message = format!("\nnative matrix generation {n}\n");
                let commit = Commit::new_with_kind(
                    kind,
                    author.clone(),
                    committer.clone(),
                    root.id,
                    commits.last().map(|c| vec![c.id]).unwrap_or_default(),
                    &message,
                )
                .expect("explicit root commit");
                let now = chrono::Utc::now().naive_utc();
                objects
                    .inner
                    .put_stream(
                        &ObjectKey {
                            namespace: ObjectNamespace::Git,
                            key: blob.id.to_string(),
                        },
                        Box::pin(futures::stream::iter([Ok(Bytes::from(blob.data.clone()))])),
                        ObjectMeta {
                            size: blob.data.len() as i64,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("store own blob bytes");
                mega_blob::ActiveModel {
                    id: Set(-100 - n * 10),
                    blob_id: Set(blob.id.to_string()),
                    name: Set("data.txt".to_owned()),
                    size: Set(blob.data.len() as i32),
                    created_at: Set(now),
                    pack_id: Set(String::new()),
                    file_path: Set(String::new()),
                    pack_offset: Set(0),
                    is_delta_in_pack: Set(false),
                    commit_id: Set(commit.id.to_string()),
                }
                .insert(db)
                .await
                .expect("seed blob metadata");
                for (offset, tree) in [&subtree, &root].into_iter().enumerate() {
                    let data = tree.to_data().expect("tree bytes");
                    mega_tree::ActiveModel {
                        id: Set(-101 - n * 10 - offset as i64),
                        tree_id: Set(tree.id.to_string()),
                        size: Set(data.len() as i32),
                        sub_trees: Set(data),
                        created_at: Set(now),
                        pack_id: Set(String::new()),
                        pack_offset: Set(0),
                        commit_id: Set(commit.id.to_string()),
                    }
                    .insert(db)
                    .await
                    .expect("seed tree metadata");
                }
                mega_commit::ActiveModel {
                    id: Set(-103 - n * 10),
                    commit_id: Set(commit.id.to_string()),
                    tree: Set(root.id.to_string()),
                    parents_id: Set(serde_json::json!(
                        commit
                            .parent_commit_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                    )),
                    author: Set(Some(
                        String::from_utf8(author.to_data().expect("author bytes"))
                            .expect("author utf8"),
                    )),
                    committer: Set(Some(
                        String::from_utf8(committer.to_data().expect("committer bytes"))
                            .expect("committer utf8"),
                    )),
                    content: Set(Some(message.clone())),
                    created_at: Set(now),
                    pack_id: Set(String::new()),
                    pack_offset: Set(0),
                }
                .insert(db)
                .await
                .expect("seed commit metadata");
                generations.push(HashMap::from([
                    (blob.id, (ObjectType::Blob, blob.data.clone())),
                    (
                        subtree.id,
                        (ObjectType::Tree, subtree.to_data().expect("subtree bytes")),
                    ),
                    (
                        root.id,
                        (ObjectType::Tree, root.to_data().expect("root bytes")),
                    ),
                    (
                        commit.id,
                        (ObjectType::Commit, commit.to_data().expect("commit bytes")),
                    ),
                ]));
                let path_commit = Commit::new_with_kind(
                    kind,
                    author.clone(),
                    committer.clone(),
                    subtree.id,
                    Vec::new(),
                    &message,
                )
                .expect("independent materialization oracle");
                subtree_tip = path_commit.id;
                subtree_objects = HashMap::from([
                    (blob.id, (ObjectType::Blob, blob.data)),
                    (
                        subtree.id,
                        (ObjectType::Tree, subtree.to_data().expect("subtree bytes")),
                    ),
                    (
                        path_commit.id,
                        (
                            ObjectType::Commit,
                            path_commit.to_data().expect("path commit bytes"),
                        ),
                    ),
                ]);
                commits.push(commit);
            }
            let tip = commits.last().expect("tip");
            let row = mega_refs::Entity::find()
                .filter(mega_refs::Column::Path.eq("/"))
                .filter(mega_refs::Column::RefName.eq("refs/heads/main"))
                .one(db)
                .await
                .expect("query root ref")
                .expect("initialized root ref");
            let mut row = row.into_active_model();
            row.ref_commit_hash = Set(tip.id.to_string());
            row.ref_tree_hash = Set(tip.tree_id.to_string());
            row.update(db)
                .await
                .expect("point own fixture root at seeded history");
            Seed {
                generations,
                subtree: subtree_objects,
                tip: tip.id,
                subtree_tip,
            }
        }

        fn assert_matrix_pack(&self, kind: HashKind, case: &str, expected: &Objects) {
            let pack = fs::read(self.shared.path().join(format!("{case}.pack")))
                .expect("actual received PACK");
            assert_eq!(&pack[..8], b"PACK\0\0\0\x02");
            assert_eq!(
                u32::from_be_bytes(pack[8..12].try_into().expect("count")) as usize,
                expected.len()
            );
            let end = pack.len() - kind.size();
            assert_eq!(
                &pack[end..],
                ObjectHash::new_for_kind(kind, &pack[..end]).as_ref()
            );
            let mut decoder = Pack::new_with_hash_kind(
                kind,
                Some(1),
                Some(64 * 1024 * 1024),
                Some(self.temp.path().to_path_buf()),
                true,
            );
            let found = Arc::new(Mutex::new(Vec::new()));
            let sink = found.clone();
            decoder
                .decode(
                    &mut Cursor::new(pack),
                    move |entry| sink.lock().expect("sink").push(entry.inner),
                    None::<fn(ObjectHash)>,
                )
                .expect("strict received PACK");
            let entries = found.lock().expect("received entries");
            assert_eq!(entries.len(), expected.len());
            let mut actual = HashMap::new();
            for entry in entries.iter() {
                assert_eq!(
                    entry.hash,
                    ObjectHash::from_type_and_data_for_kind(kind, entry.obj_type, &entry.data)
                        .expect("object hash")
                );
                assert!(
                    actual
                        .insert(entry.hash, (entry.obj_type, entry.data.clone()))
                        .is_none()
                );
            }
            assert_eq!(
                &actual, expected,
                "exact received PACK object set and bytes for {case}"
            );
            for (oid, (kind, data)) in expected {
                assert_eq!(
                    &self.matrix_git(
                        &["-C", case, "cat-file", &kind.to_string(), &oid.to_string()],
                        None
                    ),
                    data,
                    "downloaded object bytes {case} {oid}"
                );
            }
            let local = self.matrix_git(
                &[
                    "-C",
                    case,
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objectname)",
                ],
                None,
            );
            let mut local: Vec<_> = String::from_utf8(local)
                .expect("object listing")
                .lines()
                .map(str::to_owned)
                .collect();
            local.sort();
            let mut expected: Vec<_> = expected.keys().map(ToString::to_string).collect();
            expected.sort();
            assert_eq!(local, expected, "no extra lazy-fetched objects");
        }
    }

    async fn matrix(format: &'static str, kind: HashKind) {
        let (_database, db) = traces_db::TracesDb::migrated().await;
        let mut fixture = Fixture::new(&db, format).await;
        fixture.start().await;
        let seed = fixture.seed_matrix(&db, kind).await;
        let all: Objects = seed
            .generations
            .iter()
            .flat_map(|g| g.iter().map(|(id, object)| (*id, object.clone())))
            .collect();
        let mut count = 0;
        for protocol in ["0", "2"] {
            for (path_name, path, tip) in [
                ("root", "/", seed.tip),
                ("child", "/project.git", seed.subtree_tip),
            ] {
                for mode in ["normal", "depth", "filter"] {
                    let case = format!("{format}-v{protocol}-{path_name}-{mode}");
                    let version = format!("protocol.version={protocol}");
                    let url = git_cli::mega2_http_url(fixture.port, path);
                    let mut args = vec!["-c", &version, "clone", "--no-checkout"];
                    if mode == "depth" {
                        args.push("--depth=1");
                    }
                    if mode == "filter" {
                        args.push("--filter=blob:none");
                    }
                    args.extend([url.as_str(), case.as_str()]);
                    fixture.matrix_git(&args, Some(&case));
                    let packet =
                        fs::read_to_string(fixture.shared.path().join(format!("{case}.packet")))
                            .expect("actual protocol transcript");
                    assert_eq!(
                        packet.contains("command=fetch"),
                        protocol == "2",
                        "actual negotiated protocol"
                    );
                    let filter_requested = mode == "filter";
                    let filter_applied = filter_requested && protocol == "2";
                    let v0_filter_fallback = filter_requested && protocol == "0";
                    let stderr =
                        fs::read_to_string(fixture.shared.path().join(format!("{case}.stderr")))
                            .expect("actual Git stderr");
                    let filter_warning = "filtering not recognized by server, ignoring";
                    assert_eq!(stderr.contains(filter_warning), v0_filter_fallback);
                    let sent_filter = packet
                        .lines()
                        .any(|line| line.contains("> filter blob:none"));
                    assert_eq!(sent_filter, filter_applied, "actual filter request");
                    if protocol == "0" {
                        assert!(
                            !packet.lines().any(|line| {
                                line.contains("< ")
                                    && line.split_whitespace().any(|token| token == "filter")
                            }),
                            "published v0 capability set must not advertise filter"
                        );
                    } else {
                        assert!(
                            packet.lines().any(|line| {
                                line.contains("< fetch=")
                                    && line.split_whitespace().any(|token| token == "filter")
                            }),
                            "v2 must advertise its implemented filter capability"
                        );
                    }
                    let tip_output = fixture.matrix_git(&["-C", &case, "rev-parse", "HEAD"], None);
                    assert_eq!(
                        String::from_utf8(tip_output).expect("HEAD text").trim(),
                        tip.to_string()
                    );
                    let source = if path_name == "child" {
                        &seed.subtree
                    } else if mode == "depth" {
                        &seed.generations[1]
                    } else {
                        &all
                    };
                    let expected: Objects = source
                        .iter()
                        .filter(|(_, (object_type, _))| {
                            !filter_applied || *object_type != ObjectType::Blob
                        })
                        .map(|(id, object)| (*id, object.clone()))
                        .collect();
                    fixture.assert_matrix_pack(kind, &case, &expected);
                    if mode == "normal" || v0_filter_fallback {
                        fixture.matrix_git(&["-C", &case, "fsck", "--strict"], None);
                    }
                    if let Some(directory) = std::env::var_os("MEGA2_IT_NATIVE_HASH_EVIDENCE_DIR") {
                        let directory = std::path::PathBuf::from(directory);
                        fs::create_dir_all(&directory).expect("native matrix evidence directory");
                        let mut hashes = std::collections::BTreeMap::new();
                        for extension in ["pack", "packet", "stdout", "stderr"] {
                            let filename = format!("{case}.{extension}");
                            let bytes = fs::read(fixture.shared.path().join(&filename))
                                .expect("matrix original");
                            hashes.insert(
                                extension,
                                ObjectHash::new_for_kind(HashKind::Sha256, &bytes).to_string(),
                            );
                            fs::write(directory.join(filename), bytes)
                                .expect("preserve matrix original");
                        }
                        git_cli::append_evidence_line("MEGA2_IT_NATIVE_HASH_CASES", &serde_json::json!({"case": case, "filter_requested": filter_requested, "filter_applied": filter_applied, "v0_filter_fallback": v0_filter_fallback, "objects": expected.len(), "hashes": hashes, "strict_object_bytes": true, "lazy_fetch": false, "mst2_publication_enabled": false, "cedar_enforcement": "off"}).to_string());
                    }
                    eprintln!("FIX15_MATRIX_PASS: {case} objects={}", expected.len());
                    count += 1;
                }
            }
        }
        assert_eq!(count, 12);
        fixture.stop();
        db.close().await.expect("close native matrix database");
    }

    #[tokio::test]
    async fn native_hash_sha1_http_matrix() {
        matrix("sha1", HashKind::Sha1).await;
    }

    #[tokio::test]
    async fn native_hash_sha256_http_matrix() {
        matrix("sha256", HashKind::Sha256).await;
    }
}
