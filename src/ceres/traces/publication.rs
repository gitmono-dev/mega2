//! Raw facts for publication. Construction consumes the controlled PACK result;
//! transport callers cannot supply parents, membership or checkpoint bindings.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
};

use git_internal::{hash::HashKind, internal::object::types::ObjectType};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::{
    admission::{BoundedDecodedPack, IngestContinuation, admit_raw_capture_role},
    ingest::{RawTreeMode, verified_commit_links, verified_tree_entries},
};
use crate::{
    callisto::trace_object,
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
    jupiter::{
        service::trace_object_service::{RawObjectType, TraceObjectService, raw_object_oid},
        storage::trace_stream_storage::{
            CheckpointBinding, CommitParentList, ExpectedOldTip, PublishRef,
        },
    },
};

fn invalid() -> MegaError {
    MegaError::Traces(TracesError::ObjectIntegrity)
}
fn io_error(_: impl std::fmt::Display) -> MegaError {
    MegaError::Traces(TracesError::QueryTimeout)
}

/// The client's proposal contains no asserted object facts.
pub(crate) struct PublicationProposal {
    pub(crate) stream_id: String,
    pub(crate) expected_old_tip: ExpectedOldTip,
    pub(crate) new_tip: String,
    pub(crate) ref_names: Vec<String>,
    pub(crate) delete: bool,
    pub(crate) idempotency_key: String,
}

#[cfg(test)]
thread_local! {
    static LAST_SPOOL: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

struct Spool {
    path: PathBuf,
}

impl Spool {
    fn new() -> Result<Self, MegaError> {
        let path = std::env::temp_dir().join(format!("mega2-traces-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(io_error)?;
        #[cfg(test)]
        LAST_SPOOL.with(|last| *last.borrow_mut() = Some(path.clone()));
        Ok(Self { path })
    }

    fn create(&self, name: &str) -> Result<File, MegaError> {
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(self.path.join(name)).map_err(io_error)
    }

    fn open(&self, name: &str) -> Result<File, MegaError> {
        File::open(self.path.join(name)).map_err(io_error)
    }

    async fn write_bytes(
        &self,
        name: &str,
        bytes: &[u8],
        control: &IngestContinuation,
    ) -> Result<(), MegaError> {
        let mut file = self.create(name)?;
        for chunk in bytes.chunks(64 * 1024) {
            control.checkpoint().await?;
            file.write_all(chunk).map_err(io_error)?;
        }
        control.check()
    }

    async fn read_bytes(
        &self,
        name: &str,
        control: &IngestContinuation,
    ) -> Result<Vec<u8>, MegaError> {
        let mut file = self.open(name)?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 64 * 1024];
        loop {
            control.checkpoint().await?;
            let count = file.read(&mut chunk).map_err(io_error)?;
            if count == 0 {
                break;
            }
            if bytes.len() as u64 + count as u64 > TracesProtocolLimits::MAX_OBJECT_BYTES {
                return Err(MegaError::Traces(TracesError::ResponseBudget));
            }
            bytes.try_reserve_exact(count).map_err(io_error)?;
            bytes.extend_from_slice(&chunk[..count]);
        }
        Ok(bytes)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Serialize, Deserialize)]
struct Edge {
    mode: u8,
    name: String,
    oid: String,
}

#[derive(Serialize, Deserialize)]
struct Visit {
    oid: String,
    kind: u8,
    depth: u64,
}

fn write_record(file: &mut File, record: &impl Serialize) -> Result<(), MegaError> {
    serde_json::to_writer(&mut *file, record).map_err(io_error)?;
    file.write_all(b"\n").map_err(io_error)
}

fn read_record<T: serde::de::DeserializeOwned>(
    reader: &mut BufReader<File>,
) -> Result<Option<T>, MegaError> {
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(io_error)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line).map(Some).map_err(io_error)
}

/// A sealed checkpoint map, stored once for each distinct root tree. Reading
/// it never exposes a writable path or a mutable binding to a transport caller.
pub(crate) struct CheckpointMap {
    records: PathBuf,
    index_prefix: PathBuf,
    schema_id: String,
}

impl CheckpointMap {
    pub(crate) fn get(&self, key: &str) -> Result<Option<CheckpointBinding>, MegaError> {
        let name = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let path = self.index_prefix.join(name);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        let (checkpoint_key, original_tree): (String, String) =
            serde_json::from_reader(file).map_err(io_error)?;
        if checkpoint_key != key {
            return Err(invalid());
        }
        Ok(Some(CheckpointBinding {
            checkpoint_key,
            original_tree,
        }))
    }

    pub(crate) fn iter(&self) -> Result<CheckpointIter, MegaError> {
        Ok(CheckpointIter {
            reader: BufReader::new(File::open(&self.records).map_err(io_error)?),
        })
    }
}

pub(crate) struct CheckpointIter {
    reader: BufReader<File>,
}
impl Iterator for CheckpointIter {
    type Item = Result<CheckpointBinding, MegaError>;
    fn next(&mut self) -> Option<Self::Item> {
        match read_record::<(String, String)>(&mut self.reader) {
            Ok(Some((checkpoint_key, original_tree))) => Some(Ok(CheckpointBinding {
                checkpoint_key,
                original_tree,
            })),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

pub(crate) struct CommitFacts {
    oid: String,
    parents: Vec<String>,
    tree: String,
}
impl CommitFacts {
    pub(crate) fn oid(&self) -> &str {
        &self.oid
    }
    pub(crate) fn parents(&self) -> &[String] {
        &self.parents
    }
    pub(crate) fn tree(&self) -> &str {
        &self.tree
    }
}

/// It is deliberately not a wire DTO and has no deserializer or mutable access.
/// MTR-101 must additionally grant history-policy eligibility before wire use.
pub(crate) struct PreparedPublication {
    update: PublishRef,
    commits: Vec<CommitFacts>,
    maps: HashMap<String, CheckpointMap>,
    tip_tree: String,
    baseline_tree: Option<String>,
    incoming: HashSet<String>,
    spool: Spool,
    control: IngestContinuation,
}

impl PreparedPublication {
    pub(crate) fn update(&self) -> &PublishRef {
        &self.update
    }
    pub(crate) fn commits(&self) -> &[CommitFacts] {
        &self.commits
    }
    pub(crate) fn checkpoints(&self, tree: &str) -> Option<&CheckpointMap> {
        self.maps.get(tree)
    }
    pub(crate) fn final_checkpoints(&self) -> Result<&CheckpointMap, MegaError> {
        self.maps.get(&self.tip_tree).ok_or_else(invalid)
    }
    pub(crate) fn baseline_checkpoints(&self) -> Result<Option<&CheckpointMap>, MegaError> {
        self.baseline_tree
            .as_ref()
            .map(|tree| self.maps.get(tree).ok_or_else(invalid))
            .transpose()
    }
    #[cfg(test)]
    pub(crate) fn spool_path_for_test(&self) -> &std::path::Path {
        &self.spool.path
    }
    pub(crate) fn control(&self) -> &IngestContinuation {
        &self.control
    }
    pub(crate) async fn incoming_bytes(&self, oid: &str) -> Result<Option<Vec<u8>>, MegaError> {
        if !self.incoming.contains(oid) {
            return Ok(None);
        }
        self.spool
            .read_bytes(&format!("raw-{oid}"), &self.control)
            .await
            .map(Some)
    }
}

struct Builder<'a> {
    db: &'a DatabaseConnection,
    service: &'a TraceObjectService,
    stream: &'a str,
    hash_kind: &'a str,
    width: usize,
    control: &'a IngestContinuation,
    spool: &'a Spool,
    incoming: HashMap<String, RawObjectType>,
    membership: HashSet<String>,
    fresh: HashSet<String>,
}

impl Builder<'_> {
    async fn published(&self, oid: &str) -> Result<bool, MegaError> {
        self.control
            .run(async {
                Ok(trace_object::Entity::find()
                    .filter(trace_object::Column::StreamId.eq(self.stream))
                    .filter(trace_object::Column::HashKind.eq(self.hash_kind))
                    .filter(trace_object::Column::Oid.eq(oid))
                    .filter(trace_object::Column::Visible.eq(true))
                    .one(self.db)
                    .await
                    .map_err(io_error)?
                    .is_some())
            })
            .await
    }

    async fn object(&mut self, oid: &str, expected: RawObjectType) -> Result<Vec<u8>, MegaError> {
        self.control.checkpoint().await?;
        if let Some(kind) = self.incoming.get(oid) {
            if *kind != expected {
                return Err(invalid());
            }
            if self.membership.insert(oid.to_owned()) && !self.published(oid).await? {
                self.fresh.insert(oid.to_owned());
            }
            return self
                .spool
                .read_bytes(&format!("raw-{oid}"), self.control)
                .await;
        }
        if !self.published(oid).await? {
            return Err(invalid());
        }
        let object = self
            .service
            .read_verified_raw(self.stream, self.hash_kind, oid, self.control)
            .await?
            .ok_or_else(invalid)?;
        if object.object_type() != expected {
            return Err(invalid());
        }
        Ok(object.into_bytes())
    }

    async fn closure(&mut self, tip: &str) -> Result<(), MegaError> {
        let mut queue = self.spool.create("queue")?;
        let mut reader = BufReader::new(self.spool.open("queue")?);
        write_record(
            &mut queue,
            &Visit {
                oid: tip.to_owned(),
                kind: 1,
                depth: 0,
            },
        )?;
        while let Some(visit) = read_record::<Visit>(&mut reader)? {
            self.control.checkpoint().await?;
            if visit.depth > TracesProtocolLimits::MAX_TREE_DEPTH {
                return Err(MegaError::Traces(TracesError::ResponseBudget));
            }
            let expected = match visit.kind {
                1 => RawObjectType::Commit,
                2 => RawObjectType::Tree,
                3 => RawObjectType::Blob,
                _ => return Err(invalid()),
            };
            let marker = format!("seen-{}", visit.oid);
            let marker_path = self.spool.path.join(&marker);
            let previous = if marker_path.exists() {
                let (kind, depth): (u8, u64) =
                    serde_json::from_reader(self.spool.open(&marker)?).map_err(io_error)?;
                if kind != visit.kind {
                    return Err(invalid());
                }
                if depth >= visit.depth {
                    continue;
                }
                true
            } else {
                false
            };
            let bytes = self.object(&visit.oid, expected).await?;
            match expected {
                RawObjectType::Commit => {
                    let links = verified_commit_links(&bytes, self.width, self.control)?;
                    if links.parents.len() > 1 {
                        return Err(invalid());
                    }
                    if !previous {
                        let mut file = self.spool.create(&format!("commit-{}", visit.oid))?;
                        serde_json::to_writer(&mut file, &(links.tree.clone(), &links.parents))
                            .map_err(io_error)?;
                    }
                    write_record(
                        &mut queue,
                        &Visit {
                            oid: links.tree,
                            kind: 2,
                            depth: 1,
                        },
                    )?;
                    for oid in links.parents {
                        write_record(
                            &mut queue,
                            &Visit {
                                oid,
                                kind: 1,
                                depth: 0,
                            },
                        )?;
                    }
                }
                RawObjectType::Tree => {
                    let entries = verified_tree_entries(&bytes, self.width / 2, self.control)?;
                    let mut edges = if previous {
                        None
                    } else {
                        Some(self.spool.create(&format!("tree-{}", visit.oid))?)
                    };
                    for entry in entries {
                        self.control.checkpoint().await?;
                        let mode = match entry.mode {
                            RawTreeMode::Tree => 2,
                            RawTreeMode::Blob => 3,
                            RawTreeMode::Gitlink => 4,
                        };
                        if let Some(file) = &mut edges {
                            write_record(
                                file,
                                &Edge {
                                    mode,
                                    name: hex::encode(entry.name),
                                    oid: entry.oid.clone(),
                                },
                            )?;
                        }
                        if mode != 4 {
                            write_record(
                                &mut queue,
                                &Visit {
                                    oid: entry.oid,
                                    kind: mode,
                                    depth: visit.depth + u64::from(mode == 2),
                                },
                            )?;
                        }
                    }
                }
                RawObjectType::Blob => {}
                RawObjectType::Tag => return Err(invalid()),
            }
            drop(bytes);
            let mut marker_file = if previous {
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(marker_path)
                    .map_err(io_error)?
            } else {
                self.spool.create(&marker)?
            };
            serde_json::to_writer(&mut marker_file, &(visit.kind, visit.depth))
                .map_err(io_error)?;
        }
        self.control.check()
    }

    fn edges(&self, oid: &str) -> Result<BufReader<File>, MegaError> {
        self.spool.open(&format!("tree-{oid}")).map(BufReader::new)
    }

    async fn checkpoint_map(&mut self, tree: &str) -> Result<CheckpointMap, MegaError> {
        let records = self.spool.path.join(format!("map-{tree}"));
        let index_prefix = self.spool.path.join(format!("index-{tree}"));
        // The enclosing request directory is private; the nested directory is
        // never exposed before its owner is dropped.
        std::fs::create_dir(&index_prefix).map_err(io_error)?;
        let mut output = self.spool.create(&format!("map-{tree}"))?;
        let mut root = self.edges(tree)?;
        let mut known = None;
        let mut unsupported = false;
        while let Some(edge) = read_record::<Edge>(&mut root)? {
            self.control.checkpoint().await?;
            if edge.mode != 2 || edge.name != hex::encode(b"checkpoint") {
                continue;
            }
            let mut prefixes = self.edges(&edge.oid)?;
            while let Some(prefix) = read_record::<Edge>(&mut prefixes)? {
                self.control.checkpoint().await?;
                if prefix.mode != 2 {
                    continue;
                }
                let prefix_name = hex::decode(prefix.name).map_err(io_error)?;
                let mut leaves = self.edges(&prefix.oid)?;
                while let Some(leaf) = read_record::<Edge>(&mut leaves)? {
                    self.control.checkpoint().await?;
                    if leaf.mode != 2 {
                        continue;
                    }
                    let rest = hex::decode(&leaf.name).map_err(io_error)?;
                    // A slash cannot occur in a tree component. This disjoint
                    // encoding retains non-capture paths without lossy UTF-8
                    // conversion or collisions with ordinary capture IDs.
                    let key = match (
                        prefix_name.len(),
                        std::str::from_utf8(&prefix_name),
                        std::str::from_utf8(&rest),
                    ) {
                        (2, Ok(prefix), Ok(rest)) => format!("{prefix}{rest}"),
                        _ => {
                            unsupported = true;
                            format!("/{}/{}", hex::encode(&prefix_name), hex::encode(&rest))
                        }
                    };
                    let binding = (key.clone(), leaf.oid.clone());
                    write_record(&mut output, &binding)?;
                    let index = format!(
                        "index-{tree}/{}",
                        hex::encode(sha2::Sha256::digest(key.as_bytes()))
                    );
                    serde_json::to_writer(self.spool.create(&index)?, &binding)
                        .map_err(io_error)?;
                    let mut roles = self.edges(&leaf.oid)?;
                    while let Some(role) = read_record::<Edge>(&mut roles)? {
                        self.control.checkpoint().await?;
                        if role.mode != 3 {
                            continue;
                        }
                        let role_name = if role.name == hex::encode(b"metadata.json") {
                            "metadata"
                        } else if role.name == hex::encode(b"manifest.json") {
                            "manifest"
                        } else {
                            continue;
                        };
                        let bytes = self.object(&role.oid, RawObjectType::Blob).await?;
                        admit_raw_capture_role(role_name, bytes.len() as u64)?;
                        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
                        let version = value
                            .as_ref()
                            .and_then(|value| value.get("schema_version"))
                            .and_then(serde_json::Value::as_u64);
                        match (role_name, version) {
                            ("metadata", Some(2)) => known = Some("metadata-v2"),
                            ("manifest", Some(1)) if known.is_none() => known = Some("manifest-v1"),
                            ("manifest", Some(1)) => {}
                            _ => unsupported = true,
                        }
                    }
                }
            }
        }
        Ok(CheckpointMap {
            records,
            index_prefix,
            schema_id: if unsupported {
                "unsupported"
            } else {
                known.unwrap_or("unsupported")
            }
            .to_owned(),
        })
    }
}

pub(crate) async fn prepare(
    db: &DatabaseConnection,
    service: &TraceObjectService,
    decoded: BoundedDecodedPack,
    proposal: PublicationProposal,
) -> Result<PreparedPublication, MegaError> {
    let (kind, entries, control) = decoded.into_parts();
    control.check()?;
    let hash_kind = match kind {
        HashKind::Sha1 => "sha1",
        HashKind::Sha256 => "sha256",
        _ => return Err(MegaError::Traces(TracesError::InvalidParameter)),
    };
    let width = if kind == HashKind::Sha1 { 40 } else { 64 };
    let tip = proposal.new_tip.to_ascii_lowercase();
    if tip.len() != width
        || !tip.bytes().all(|byte| byte.is_ascii_hexdigit())
        || uuid::Uuid::parse_str(&proposal.stream_id).is_err()
        || proposal.ref_names != ["refs/libra/traces"]
        || proposal.delete
        || proposal.idempotency_key.is_empty()
    {
        return Err(MegaError::Traces(TracesError::InvalidParameter));
    }
    if uuid::Uuid::parse_str(&proposal.stream_id)
        .map_err(|_| invalid())?
        .to_string()
        != proposal.stream_id
    {
        return Err(MegaError::Traces(TracesError::InvalidParameter));
    }
    if let ExpectedOldTip::At(old) = &proposal.expected_old_tip
        && (old.len() != width || !old.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(invalid());
    }
    let spool = Spool::new()?;
    let mut incoming = HashMap::new();
    for (kind, bytes) in entries {
        control.checkpoint().await?;
        let raw_type = match kind {
            ObjectType::Commit => RawObjectType::Commit,
            ObjectType::Tree => RawObjectType::Tree,
            ObjectType::Blob => RawObjectType::Blob,
            ObjectType::Tag => RawObjectType::Tag,
            _ => return Err(invalid()),
        };
        let oid = raw_object_oid(hash_kind, raw_type, &bytes, &control).await?;
        if let Some(previous) = incoming.insert(oid.clone(), raw_type) {
            if previous != raw_type
                || spool.read_bytes(&format!("raw-{oid}"), &control).await? != bytes
            {
                return Err(invalid());
            }
            continue;
        }
        spool
            .write_bytes(&format!("raw-{oid}"), &bytes, &control)
            .await?;
    }
    let mut builder = Builder {
        db,
        service,
        stream: &proposal.stream_id,
        hash_kind,
        width,
        control: &control,
        spool: &spool,
        incoming,
        membership: HashSet::new(),
        fresh: HashSet::new(),
    };
    builder.closure(&tip).await?;
    let (tip_tree, _): (String, Vec<String>) =
        serde_json::from_reader(spool.open(&format!("commit-{tip}"))?).map_err(io_error)?;
    let expected_old_tip = match &proposal.expected_old_tip {
        ExpectedOldTip::At(old) => ExpectedOldTip::At(old.to_ascii_lowercase()),
        other => other.clone(),
    };
    let mut commits = Vec::new();
    let mut maps = HashMap::new();
    let mut baseline_tree = None;
    let mut next = tip.clone();
    let mut seen = HashSet::new();
    loop {
        control.checkpoint().await?;
        if let ExpectedOldTip::At(old) = &expected_old_tip
            && old == &next
        {
            let (tree, _): (String, Vec<String>) =
                serde_json::from_reader(spool.open(&format!("commit-{next}"))?)
                    .map_err(io_error)?;
            if let std::collections::hash_map::Entry::Vacant(entry) = maps.entry(tree.clone()) {
                entry.insert(builder.checkpoint_map(&tree).await?);
            }
            baseline_tree = Some(tree);
            break;
        }
        // Unspecified is retained for raw-fact diagnostics only. Production
        // history validation requires an explicit request baseline.
        if matches!(expected_old_tip, ExpectedOldTip::Unspecified) && !builder.fresh.contains(&next)
        {
            break;
        }
        if !seen.insert(next.clone()) {
            return Err(invalid());
        }
        let (tree, parents): (String, Vec<String>) =
            serde_json::from_reader(spool.open(&format!("commit-{next}"))?).map_err(io_error)?;
        if let std::collections::hash_map::Entry::Vacant(entry) = maps.entry(tree.clone()) {
            entry.insert(builder.checkpoint_map(&tree).await?);
        }
        let parent = parents.first().cloned();
        commits.push(CommitFacts {
            oid: next,
            parents,
            tree,
        });
        match parent {
            Some(parent) => next = parent,
            None if matches!(expected_old_tip, ExpectedOldTip::At(_)) => return Err(invalid()),
            None => break,
        }
    }
    commits.reverse();
    if let std::collections::hash_map::Entry::Vacant(entry) = maps.entry(tip_tree.clone()) {
        entry.insert(builder.checkpoint_map(&tip_tree).await?);
    }
    let mut object_oids: Vec<_> = builder.membership.iter().cloned().collect();
    if !builder.membership.contains(&tip) {
        object_oids.push(tip.clone());
    }
    object_oids.extend(commits.iter().map(|commit| commit.oid.clone()));
    object_oids.sort();
    object_oids.dedup();
    let schema_id = maps.get(&tip_tree).ok_or_else(invalid)?.schema_id.clone();
    let update = PublishRef {
        stream_id: proposal.stream_id.clone(),
        expected_old_tip,
        new_tip: tip,
        ref_names: proposal.ref_names,
        delete: false,
        commits: commits
            .iter()
            .map(|fact| CommitParentList {
                oid: fact.oid.clone(),
                parents: fact.parents.clone(),
            })
            .collect(),
        checkpoints: Vec::new(),
        object_oids,
        hash_kind: hash_kind.to_owned(),
        idempotency_key: proposal.idempotency_key,
        schema_id,
    };
    let incoming = builder.membership;
    control.check()?;
    Ok(PreparedPublication {
        update,
        commits,
        maps,
        tip_tree,
        baseline_tree,
        incoming,
        spool,
        control,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::process::{Command, Stdio};

    use super::*;
    use crate::{
        ceres::traces::admission::{IngestAdmission, decode_wire_pack},
        jupiter::{
            migration::apply_migrations, storage::object_storage::MegaObjectStorageWrapper,
            tests::test_db_connection,
        },
        orbit_api::factory::{LocalConfig, ObjectStorageConfig},
    };

    pub(crate) struct Fixture {
        directory: tempfile::TempDir,
        pub(crate) kind: HashKind,
        pub(crate) objects: Vec<(String, String, Vec<u8>)>,
    }

    impl Fixture {
        pub(crate) fn new(kind: HashKind) -> Self {
            let fixture = Self {
                directory: tempfile::tempdir().unwrap(),
                kind,
                objects: Vec::new(),
            };
            fixture.git(&["init", "-q", "--object-format", fixture.hash_kind()], b"");
            fixture
        }
        pub(crate) fn hash_kind(&self) -> &'static str {
            match self.kind {
                HashKind::Sha1 => "sha1",
                HashKind::Sha256 => "sha256",
                _ => panic!("unsupported fixture"),
            }
        }
        pub(crate) fn git(&self, args: &[&str], input: &[u8]) -> Vec<u8> {
            let mut child = Command::new("git")
                .current_dir(self.directory.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
        pub(crate) fn object(&mut self, kind: &str, bytes: &[u8]) -> String {
            let oid = String::from_utf8(self.git(
                &["hash-object", "--literally", "-w", "-t", kind, "--stdin"],
                bytes,
            ))
            .unwrap()
            .trim()
            .to_owned();
            if !self.objects.iter().any(|(existing, _, _)| existing == &oid) {
                self.objects
                    .push((oid.clone(), kind.to_owned(), bytes.to_vec()));
            }
            assert_eq!(self.git(&["cat-file", kind, &oid], b""), bytes);
            oid
        }
        pub(crate) fn tree(&mut self, entries: &[(&str, &[u8], &str)]) -> String {
            let mut bytes = Vec::new();
            for (mode, name, oid) in entries {
                bytes.extend_from_slice(mode.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(name);
                bytes.push(0);
                bytes.extend_from_slice(&hex::decode(oid).unwrap());
            }
            self.object("tree", &bytes)
        }
        pub(crate) fn commit(&mut self, tree: &str, parent: Option<&str>) -> String {
            let mut bytes = format!("tree {tree}\n").into_bytes();
            if let Some(parent) = parent {
                bytes.extend_from_slice(format!("parent {parent}\n").as_bytes());
            }
            bytes.extend_from_slice(b"author Test <test@example.test> 0 +0000\ncommitter Test <test@example.test> 0 +0000\nx-unknown retained\ngpgsig -----BEGIN PGP SIGNATURE-----\n test\n -----END PGP SIGNATURE-----\n\nraw \xff\n");
            self.object("commit", &bytes)
        }
        pub(crate) fn capture(&mut self, metadata: &[u8]) -> (String, String) {
            let blob = self.object("blob", metadata);
            let leaf = self.tree(&[("100644", b"metadata.json", &blob)]);
            let prefix = self.tree(&[("40000", b"cd", &leaf)]);
            let checkpoints = self.tree(&[("40000", b"ab", &prefix)]);
            let root = self.tree(&[("40000", b"checkpoint", &checkpoints)]);
            (root, leaf)
        }
        pub(crate) fn pack(&self, included: Option<&[String]>) -> Vec<u8> {
            let list = included
                .map(|oids| oids.to_vec())
                .unwrap_or_else(|| self.objects.iter().map(|(oid, _, _)| oid.clone()).collect());
            self.git(
                &["pack-objects", "--stdout", "--window=0", "--depth=0"],
                format!("{}\n", list.join("\n")).as_bytes(),
            )
        }
        pub(crate) async fn decoded(&self, included: Option<&[String]>) -> BoundedDecodedPack {
            let pack = self.pack(included);
            decode_wire_pack(
                &mut IngestAdmission::start(),
                self.kind,
                pack.as_slice(),
                None,
            )
            .await
            .unwrap()
        }
        pub(crate) async fn prepare(
            &self,
            db: &DatabaseConnection,
            store: &MegaObjectStorageWrapper,
            stream: &str,
            tip: &str,
            key: &str,
            included: Option<&[String]>,
        ) -> Result<PreparedPublication, MegaError> {
            prepare(
                db,
                &TraceObjectService::new(store.clone()),
                self.decoded(included).await,
                PublicationProposal {
                    stream_id: stream.to_owned(),
                    expected_old_tip: ExpectedOldTip::Unspecified,
                    new_tip: tip.to_owned(),
                    ref_names: vec!["refs/libra/traces".to_owned()],
                    delete: false,
                    idempotency_key: key.to_owned(),
                },
            )
            .await
        }
    }

    pub(crate) async fn local_store() -> (tempfile::TempDir, MegaObjectStorageWrapper) {
        let directory = tempfile::tempdir().unwrap();
        let config = ObjectStorageConfig {
            local: LocalConfig {
                root_dir: directory.path().to_str().unwrap().into(),
            },
            ..Default::default()
        };
        let inner = crate::orbit::factory::ObjectStorageFactory::build(&config)
            .await
            .unwrap();
        (directory, inner)
    }

    async fn db() -> (tempfile::TempDir, DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        (directory, db)
    }

    #[tokio::test]
    async fn raw_publication_facts_git_history_sha1_and_sha256() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let extra = fixture.object("blob", b"not reachable");
            let (root1, leaf1) = fixture.capture(br#"{"schema_version":2}"#);
            let first = fixture.commit(&root1, None);
            let (root2, leaf2) = fixture.capture(br#"{"schema_version":999,"unknown":true}"#);
            let second = fixture.commit(&root2, Some(&first));
            let blob = fixture.object("blob", b"\x00\xffraw");
            let absent_gitlink = "a".repeat(if kind == HashKind::Sha1 { 40 } else { 64 });
            let root3 = fixture.tree(&[
                ("100755", b"executable", &blob),
                ("160000", b"gitlink", &absent_gitlink),
                ("120000", b"symlink", &blob),
                ("100644", b"unknown-\xff", &blob),
            ]);
            let third = fixture.commit(&root3, Some(&second));
            let stream = uuid::Uuid::new_v4().to_string();
            let prepared = fixture
                .prepare(&db, &store, &stream, &third, "history", None)
                .await
                .unwrap();
            assert_eq!(
                prepared
                    .commits
                    .iter()
                    .map(|c| c.oid.as_str())
                    .collect::<Vec<_>>(),
                [first.as_str(), second.as_str(), third.as_str()]
            );
            assert_eq!(prepared.commits[1].parents, std::slice::from_ref(&first));
            assert_eq!(prepared.commits[2].parents, std::slice::from_ref(&second));
            assert_eq!(
                prepared
                    .checkpoints(&root1)
                    .unwrap()
                    .get("abcd")
                    .unwrap()
                    .unwrap()
                    .original_tree,
                leaf1
            );
            assert_eq!(
                prepared
                    .checkpoints(&root2)
                    .unwrap()
                    .get("abcd")
                    .unwrap()
                    .unwrap()
                    .original_tree,
                leaf2
            );
            assert!(
                prepared
                    .checkpoints(&root3)
                    .unwrap()
                    .iter()
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert_eq!(
                prepared.checkpoints(&root1).unwrap().schema_id,
                "metadata-v2"
            );
            assert_eq!(
                prepared.checkpoints(&root2).unwrap().schema_id,
                "unsupported"
            );
            assert_eq!(prepared.update.schema_id, "unsupported");
            assert!(!prepared.update.object_oids.contains(&extra));
            assert!(!prepared.update.object_oids.contains(&absent_gitlink));
            for (oid, _, bytes) in &fixture.objects {
                if oid != &extra {
                    assert_eq!(
                        prepared.incoming_bytes(oid).await.unwrap().as_ref(),
                        Some(bytes)
                    );
                }
            }
            let spool = prepared.spool.path.clone();
            assert!(spool.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            drop(prepared);
            assert!(!spool.exists(), "request facts have one cleanup owner");
        }
    }

    #[tokio::test]
    async fn raw_publication_facts_reject_type_missing_and_ambiguous_tree() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let stream = uuid::Uuid::new_v4().to_string();
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let good = fixture.commit(&tree, None);
        assert!(
            fixture
                .prepare(&db, &store, &stream, &tree, "tree-tip", None)
                .await
                .is_err()
        );
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &stream,
                    &good,
                    "missing",
                    Some(std::slice::from_ref(&good))
                )
                .await
                .is_err()
        );
        let ambiguous = fixture.object(
            "commit",
            format!("tree {tree}\ntree {tree}\n\ninvalid\n").as_bytes(),
        );
        assert!(
            fixture
                .prepare(&db, &store, &stream, &ambiguous, "multi-tree", None)
                .await
                .is_err()
        );
        let no_tree = fixture.object(
            "commit",
            b"author Test <test@example.test> 0 +0000\n\nmissing tree\n",
        );
        assert!(
            fixture
                .prepare(&db, &store, &stream, &no_tree, "no-tree", None)
                .await
                .is_err()
        );
        let missing = "f".repeat(40);
        let bad_tree = fixture.tree(&[("100644", b"missing", &missing)]);
        let bad = fixture.commit(&bad_tree, None);
        assert!(
            fixture
                .prepare(&db, &store, &stream, &bad, "missing-blob", None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn raw_publication_facts_raw_residue_is_not_published_qualification() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let stream = uuid::Uuid::new_v4().to_string();
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let first = fixture.commit(&tree, None);
        let second = fixture.commit(&tree, Some(&first));
        let (_, _, raw) = fixture
            .objects
            .iter()
            .find(|(oid, _, _)| oid == &first)
            .unwrap();
        TraceObjectService::new(store.clone())
            .put_raw(&stream, "sha1", &first, raw)
            .await
            .unwrap();
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &stream,
                    &second,
                    "residue",
                    Some(&[tree, second.clone()])
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn raw_publication_facts_role_limit_uses_each_actual_tree() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let stream = uuid::Uuid::new_v4().to_string();
        let mut fixture = Fixture::new(HashKind::Sha1);
        let (root, _) = fixture.capture(&vec![
            b'x';
            TracesProtocolLimits::MAX_METADATA_BYTES as usize + 1
        ]);
        let first = fixture.commit(&root, None);
        let empty = fixture.tree(&[]);
        let tip = fixture.commit(&empty, Some(&first));
        assert!(matches!(
            fixture
                .prepare(&db, &store, &stream, &tip, "role-budget", None)
                .await,
            Err(MegaError::Traces(TracesError::ResponseBudget))
        ));
    }

    #[tokio::test]
    async fn raw_publication_facts_cancellation_after_decoder_success() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let tip = fixture.commit(&tree, None);
        let pack = fixture.pack(None);
        let mut admission = IngestAdmission::start();
        let decoded = decode_wire_pack(&mut admission, HashKind::Sha1, pack.as_slice(), None)
            .await
            .unwrap();
        assert!(
            decoded.continuation().check().is_ok(),
            "worker shutdown does not cancel continuation"
        );
        admission.cancel();
        assert!(matches!(
            prepare(
                &db,
                &TraceObjectService::new(store),
                decoded,
                PublicationProposal {
                    stream_id: uuid::Uuid::new_v4().to_string(),
                    expected_old_tip: ExpectedOldTip::Unspecified,
                    new_tip: tip,
                    ref_names: vec!["refs/libra/traces".to_owned()],
                    delete: false,
                    idempotency_key: "cancel".into(),
                }
            )
            .await,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
    }
    #[tokio::test]
    async fn raw_publication_facts_non_utf8_checkpoint_keys_are_lossless() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let leaf = fixture.tree(&[]);
        let prefix = fixture.tree(&[("40000", b"\xff", &leaf)]);
        let checkpoints = fixture.tree(&[("40000", b"ab", &prefix)]);
        let root = fixture.tree(&[("40000", b"checkpoint", &checkpoints)]);
        let tip = fixture.commit(&root, None);
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &uuid::Uuid::new_v4().to_string(),
                &tip,
                "non-utf8",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            prepared
                .final_checkpoints()
                .unwrap()
                .get("/6162/ff")
                .unwrap()
                .unwrap()
                .original_tree,
            leaf
        );
        assert_eq!(prepared.update().schema_id, "unsupported");
    }

    #[tokio::test]
    async fn raw_publication_facts_dropped_prepare_cleans_private_spool() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let tip = fixture.commit(&tree, None);
        let decoded = fixture.decoded(None).await;
        let service = TraceObjectService::new(store);
        let mut future = Box::pin(prepare(
            &db,
            &service,
            decoded,
            PublicationProposal {
                stream_id: uuid::Uuid::new_v4().to_string(),
                expected_old_tip: ExpectedOldTip::Unspecified,
                new_tip: tip,
                ref_names: vec!["refs/libra/traces".into()],
                delete: false,
                idempotency_key: "drop-prepare".into(),
            },
        ));
        assert!(futures::poll!(&mut future).is_pending());
        let path = LAST_SPOOL.with(|last| last.borrow().clone().unwrap());
        assert!(path.exists());
        drop(future);
        assert!(
            !path.exists(),
            "dropping the prepare future releases its request directory"
        );
    }
    #[tokio::test]
    async fn raw_publication_facts_reject_parent_after_author_and_unsorted_tree() {
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let parent = fixture.commit(&tree, None);
        let tip = fixture.object("commit", format!("tree {tree}\nauthor Test <test@example.test> 0 +0000\nparent {parent}\ncommitter Test <test@example.test> 0 +0000\n\nlate parent\n").as_bytes());
        let stream = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            fixture
                .prepare(&db, &store, &stream, &tip, "late-parent", None)
                .await,
            Err(MegaError::Traces(TracesError::InvalidParameter))
        ));
        let blob = fixture.object("blob", b"payload");
        let unsorted = fixture.tree(&[("100644", b"z", &blob), ("100644", b"a", &blob)]);
        let tip = fixture.commit(&unsorted, None);
        assert!(matches!(
            fixture
                .prepare(&db, &store, &stream, &tip, "unsorted-tree", None)
                .await,
            Err(MegaError::Traces(TracesError::InvalidParameter))
        ));
        // Git compares directories as name + '/', including prefix names.
        let directory = fixture.tree(&[]);
        let sorted = fixture.tree(&[
            ("100644", b"a.c", &blob),
            ("40000", b"a", &directory),
            ("100644", b"a0", &blob),
        ]);
        let tip = fixture.commit(&sorted, None);
        assert!(
            fixture
                .prepare(&db, &store, &stream, &tip, "git-sort", None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn raw_publication_facts_original_deadline_expires_during_database_wait() {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

        use super::super::admission::{PackBudget, decode_wire_pack_with_budget};
        let (_database, db) = db().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let tip = fixture.commit(&tree, None);
        let blocker = db.begin().await.unwrap();
        blocker
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "LOCK TABLE trace_object IN ACCESS EXCLUSIVE MODE",
            ))
            .await
            .unwrap();
        // Keep a second connection ready before starting the bounded request.
        // The first is held by the blocking transaction.
        db.begin().await.unwrap().rollback().await.unwrap();
        let pack = fixture.pack(None);
        let decoded = decode_wire_pack_with_budget(
            &mut IngestAdmission::start(),
            HashKind::Sha1,
            pack.as_slice(),
            PackBudget::for_test(4096, 4096, 4096, 10, 10, 5),
        )
        .await
        .unwrap();
        let original_control = decoded.continuation().clone();
        let service = TraceObjectService::new(store);
        let work = prepare(
            &db,
            &service,
            decoded,
            PublicationProposal {
                stream_id: uuid::Uuid::new_v4().to_string(),
                expected_old_tip: ExpectedOldTip::Unspecified,
                new_tip: tip,
                ref_names: vec!["refs/libra/traces".into()],
                delete: false,
                idempotency_key: "db-deadline".into(),
            },
        );
        tokio::pin!(work);
        let observation = async {
            loop {
                let row = blocker.query_one_raw(Statement::from_string(DatabaseBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE relation = 'trace_object'::regclass AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND NOT granted) AS waiting")).await.unwrap().unwrap();
                if row.try_get::<bool>("", "waiting").unwrap() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            result = &mut work => panic!("prepare ended before a real blocked membership read: {}", result.is_ok()),
            result = tokio::time::timeout(std::time::Duration::from_secs(4), observation) => result.unwrap(),
        }
        let path = LAST_SPOOL.with(|last| last.borrow().clone().unwrap());
        assert!(path.exists());
        assert!(matches!(
            work.await,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
        assert!(
            matches!(
                original_control.check(),
                Err(MegaError::Traces(TracesError::QueryTimeout))
            ),
            "the original decoder deadline itself expired, not merely a DB error mapped to timeout"
        );
        assert!(!path.exists());
        blocker.rollback().await.unwrap();
    }
}
