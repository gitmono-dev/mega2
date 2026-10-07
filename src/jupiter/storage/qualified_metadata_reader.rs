//! Persisted routes traverse exact certified occurrences, including reused roots.

use std::collections::{BTreeSet, HashMap};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use serde::Deserialize;

use super::*;
use crate::{
    ceres::snapshot::{
        retention_dag::MetadataDagLimits, runtime::SnapshotContext,
        view::validate_scope_relative_path,
    },
    jupiter::storage::native_snapshot_session::{
        MetadataRouteRequest, PersistedMetadataReadWork, PersistedMetadataRouteBatch,
    },
};

type ProofPages = Vec<([u8; 32], Vec<u8>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Binding {
    generation: i64,
    certificate: [u8; 32],
}

#[cfg(test)]
tokio::task_local! {
    static READER_ADMISSION_BARRIERS: (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>);
    static SOURCE_FACT_BARRIERS: (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>);
    pub(super) static READER_TEMP_SOURCE_SHADOW: bool;
}

#[cfg(test)]
pub(crate) async fn with_rooted_source_fact_barriers<F: std::future::Future>(
    admitted: Arc<tokio::sync::Barrier>,
    resume: Arc<tokio::sync::Barrier>,
    future: F,
) -> F::Output {
    SOURCE_FACT_BARRIERS.scope((admitted, resume), future).await
}

#[cfg(test)]
pub(crate) async fn with_rooted_source_temporary_shadow<F: std::future::Future>(
    future: F,
) -> F::Output {
    READER_TEMP_SOURCE_SHADOW.scope(true, future).await
}

#[cfg(test)]
pub(crate) async fn with_rooted_reader_barriers<F: std::future::Future>(
    admitted: Arc<tokio::sync::Barrier>,
    resume: Arc<tokio::sync::Barrier>,
    future: F,
) -> F::Output {
    READER_ADMISSION_BARRIERS
        .scope((admitted, resume), future)
        .await
}

#[derive(Debug, Deserialize)]
struct Reference {
    kind: String,
    child: String,
    generation: i64,
    certificate: String,
    name: Option<String>,
    label: Option<u8>,
    count: Option<u64>,
}

fn digest_hex(value: &str) -> Result<[u8; 32], SnapshotError> {
    hex::decode(value)
        .map_err(internal)?
        .as_slice()
        .try_into()
        .map_err(internal)
}
fn limit(message: &str) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::LimitExceeded,
        message,
    )
}
fn absent(message: &str) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::PathNotFound,
        message,
    )
}

struct StoredPage {
    bytes: Vec<u8>,
    page: Page,
    entries: u64,
}

pub(crate) struct RootedDirectoryWindow {
    pub directory_root: [u8; 32],
    pub entry_count: u64,
    pub entries: Vec<mst2_codec::metapage::Entry>,
    pub has_more: bool,
    pub proof_pages: ProofPages,
    pub ancestors: Vec<(String, [u8; 32])>,
}

pub(crate) enum RootedLookupStatus {
    Directory([u8; 32]),
    File {
        entry: mst2_codec::metapage::Entry,
        git_oid: String,
    },
    Absent,
    NotDirectory {
        symlink: bool,
    },
}
pub(crate) struct RootedLookupBatch {
    pub results: Vec<RootedLookupStatus>,
    pub proof_pages: Vec<([u8; 32], Vec<u8>)>,
}
struct Reader<'a> {
    txn: &'a DatabaseTransaction,
    operation: uuid::Uuid,
    bindings: HashMap<[u8; 32], Binding>,
    cache: HashMap<[u8; 32], StoredPage>,
    work: PersistedMetadataReadWork,
    limits: MetadataDagLimits,
    directories: BTreeMap<String, [u8; 32]>,
    source_ids: BTreeMap<String, uuid::Uuid>,
    file_oids: HashMap<(uuid::Uuid, Vec<u8>), String>,
}

impl RootedQualifiedMetadataRepository {
    pub(crate) async fn fixed_path_metadata(
        &self,
        pinned: &SnapshotContext,
        path: &str,
    ) -> Result<RootedLookupStatus, SnapshotError> {
        validate_scope_relative_path(path)?;
        let absolute = if pinned.built.descriptor.scope == "/" {
            path.to_owned()
        } else if path == "/" {
            pinned.built.descriptor.scope.clone()
        } else {
            format!("{}{}", pinned.built.descriptor.scope, path)
        };
        validate_scope_relative_path(&absolute)?;
        let (operation, binding, source) = self.admit_reader(pinned).await?;
        let result = async {
            let txn = self.read_transaction().await?;
            let root = pinned.built.descriptor.metadata_root;
            let mut reader = Reader::new(&txn, operation, root, binding, source);
            let result = reader.lookup(root, path).await;
            sessions::finish(txn, result).await
        }
        .await;
        self.finish_reader(operation, result).await
    }

    pub(crate) async fn directory_window(
        &self,
        pinned: &SnapshotContext,
        path: &str,
        after: Option<&str>,
        count: usize,
    ) -> Result<RootedDirectoryWindow, SnapshotError> {
        validate_scope_relative_path(path)?;
        if !(1..=256).contains(&count) {
            return Err(limit("directory limit must be 1..256"));
        }
        let absolute = if pinned.built.descriptor.scope == "/" {
            path.to_owned()
        } else if path == "/" {
            pinned.built.descriptor.scope.clone()
        } else {
            format!("{}{}", pinned.built.descriptor.scope, path)
        };
        validate_scope_relative_path(&absolute)?;
        let (operation, binding, source) = self.admit_reader(pinned).await?;
        let result = async {
            let txn = self.read_transaction().await?;
            let read = async {
                let root = pinned.built.descriptor.metadata_root;
                let mut reader = Reader::new(&txn, operation, root, binding, source);
                let directory = reader.directory(root, path).await?;
                reader.load(directory).await?;
                let total = reader.cache[&directory].entries;
                let mut entries = Vec::with_capacity(count + 1);
                reader
                    .range(directory, after.map(str::as_bytes), count + 1, &mut entries)
                    .await?;
                let has_more = entries.len() > count;
                entries.truncate(count);
                reader
                    .source_entries(reader.source_ids[path], directory, &entries)
                    .await?;
                Ok(RootedDirectoryWindow {
                    directory_root: directory,
                    entry_count: total,
                    entries,
                    has_more,
                    proof_pages: reader.proofs()?,
                    ancestors: reader.directories.into_iter().collect(),
                })
            }
            .await;
            sessions::finish(txn, read).await
        }
        .await;
        self.finish_reader(operation, result).await
    }

    pub(crate) async fn lookup_metadata(
        &self,
        pinned: &SnapshotContext,
        paths: &[String],
    ) -> Result<RootedLookupBatch, SnapshotError> {
        if paths.len() > 128 {
            return Err(limit("at most 128 paths per lookup"));
        }
        for path in paths {
            validate_scope_relative_path(path)?;
            let absolute = if pinned.built.descriptor.scope == "/" {
                path.clone()
            } else if path == "/" {
                pinned.built.descriptor.scope.clone()
            } else {
                format!("{}{}", pinned.built.descriptor.scope, path)
            };
            validate_scope_relative_path(&absolute)?;
        }
        let (operation, binding, source) = self.admit_reader(pinned).await?;
        let result = async {
            let txn = self.read_transaction().await?;
            let read = async {
                let root = pinned.built.descriptor.metadata_root;
                let mut reader = Reader::new(&txn, operation, root, binding, source);
                let mut results = Vec::with_capacity(paths.len());
                for path in paths {
                    results.push(reader.lookup(root, path).await?);
                }
                Ok(RootedLookupBatch {
                    results,
                    proof_pages: reader.proofs()?,
                })
            }
            .await;
            sessions::finish(txn, read).await
        }
        .await;
        self.finish_reader(operation, result).await
    }
    pub(crate) async fn metadata_routes(
        &self,
        pinned: &SnapshotContext,
        requests: &[MetadataRouteRequest<'_>],
    ) -> Result<PersistedMetadataRouteBatch, SnapshotError> {
        if requests.is_empty() || requests.len() > 64 {
            return Err(limit("items must hold 1..64 entries"));
        }
        for request in requests {
            validate_scope_relative_path(request.directory_path)?;
            let scope = &pinned.built.descriptor.scope;
            let absolute = if scope == "/" {
                request.directory_path.to_owned()
            } else if request.directory_path == "/" {
                scope.clone()
            } else {
                format!("{scope}{}", request.directory_path)
            };
            validate_scope_relative_path(&absolute)?;
        }
        // Acquire REQUEST and READER ownership atomically, then release the
        // mutation barrier before fetching payloads. Cleanup never precedes
        // ownership of the returned byte buffers.
        let (operation, binding, source) = self.admit_reader(pinned).await?;
        let result = self
            .read_routes(pinned, requests, operation, binding, source)
            .await;
        self.finish_reader(operation, result).await
    }

    async fn admit_reader(
        &self,
        pinned: &SnapshotContext,
    ) -> Result<(uuid::Uuid, Binding, uuid::Uuid), SnapshotError> {
        let txn = self.transaction().await?;
        let admitted = async {
            let row = self
                .session_row(
                    &txn,
                    &pinned.built.snapshot_id,
                    &pinned.lease_id,
                    &pinned.built.instance_id,
                )
                .await?;
            let current = sessions::context(
                &row,
                &pinned.built.snapshot_id,
                &pinned.lease_id,
                &pinned.built.instance_id,
            )?;
            if current.built.descriptor != pinned.built.descriptor
                || current.commit_oid != pinned.commit_oid
                || current.root_tree_oid != pinned.root_tree_oid
                || current.authorization_epoch != pinned.authorization_epoch
            {
                return Err(integrity(
                    "qualified fixed session changed during reader admission",
                ));
            }
            let reader = txn
                .query_one_raw(sql(
                    "SELECT operation_id::text,root_generation,certificate_digest
                FROM mst2_metadata_begin_reader($1,$2,$3)",
                    [
                        pinned.built.snapshot_id.clone().into(),
                        pinned.lease_id.clone().into(),
                        pinned.built.instance_id.clone().into(),
                    ],
                ))
                .await
                .map_err(database_error)?
                .ok_or_else(|| unavailable("qualified reader admission returned no owned root"))?;
            Ok((
                uuid::Uuid::parse_str(
                    &reader
                        .try_get::<String>("", "operation_id")
                        .map_err(internal)?,
                )
                .map_err(internal)?,
                Binding {
                    generation: reader.try_get("", "root_generation").map_err(internal)?,
                    certificate: digest_column(&reader, "certificate_digest")?,
                },
                row.try_get("", "attestation_id").map_err(internal)?,
            ))
        }
        .await;
        let owned = sessions::finish(txn, admitted).await?;
        #[cfg(test)]
        if let Ok((admitted, resume)) = READER_ADMISSION_BARRIERS.try_with(Clone::clone) {
            admitted.wait().await;
            resume.wait().await;
        }
        Ok(owned)
    }

    async fn finish_reader<T>(
        &self,
        operation: uuid::Uuid,
        result: Result<T, SnapshotError>,
    ) -> Result<T, SnapshotError> {
        let cleanup = self.transaction().await;
        let cleanup = match cleanup {
            Ok(txn) => {
                let finished = txn
                    .execute_raw(sql(
                        "SELECT mst2_metadata_finish_reader($1::uuid)",
                        [operation.to_string().into()],
                    ))
                    .await
                    .map_err(internal)
                    .map(|_| ());
                sessions::finish(txn, finished).await
            }
            Err(error) => Err(error),
        };
        // A failed cleanup leaves durable roots until the bounded hard deadline.
        // Report the failure instead of pretending that a protected operation
        // has been definitively finished.
        match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    async fn read_routes(
        &self,
        pinned: &SnapshotContext,
        requests: &[MetadataRouteRequest<'_>],
        operation: uuid::Uuid,
        binding: Binding,
        source: uuid::Uuid,
    ) -> Result<PersistedMetadataRouteBatch, SnapshotError> {
        let txn = self.read_transaction().await?;
        let result = async {
            let root = pinned.built.descriptor.metadata_root;
            let mut reader = Reader::new(&txn, operation, root, binding, source);
            let mut seen = BTreeSet::new();
            let mut pages = Vec::new();
            for request in requests {
                let directory = reader.directory(root, request.directory_path).await?;
                let route = reader.route(directory, request.route).await?;
                let reached = route
                    .last()
                    .ok_or_else(|| integrity("qualified metadata route is empty"))?;
                if let Some(expected) = request.expected_digest
                    && expected != format!("sha256:{}", hex::encode(reached))
                {
                    return Err(SnapshotError::new(
                        crate::ceres::snapshot::error::SnapshotErrorCode::DigestMismatch,
                        "qualified metadata route does not reach expected_digest",
                    ));
                }
                for page in route {
                    if seen.insert(page) {
                        pages.push((page, reader.cache[&page].bytes.clone()));
                    }
                }
            }
            Ok(PersistedMetadataRouteBatch {
                pages,
                work: reader.work,
            })
        }
        .await;
        sessions::finish(txn, result).await
    }
}

impl Reader<'_> {
    fn new(
        txn: &DatabaseTransaction,
        operation: uuid::Uuid,
        root: [u8; 32],
        binding: Binding,
        source: uuid::Uuid,
    ) -> Reader<'_> {
        Reader {
            txn,
            operation,
            bindings: HashMap::from([(root, binding)]),
            cache: HashMap::new(),
            work: PersistedMetadataReadWork::default(),
            limits: MetadataDagLimits::default(),
            directories: BTreeMap::from([("/".into(), root)]),
            source_ids: BTreeMap::from([("/".into(), source)]),
            file_oids: HashMap::new(),
        }
    }

    async fn source_entries(
        &mut self,
        source: uuid::Uuid,
        directory: [u8; 32],
        entries: &[mst2_codec::metapage::Entry],
    ) -> Result<HashMap<Vec<u8>, uuid::Uuid>, SnapshotError> {
        let binding = self
            .bindings
            .get(&directory)
            .ok_or_else(|| integrity("qualified source directory has no certified binding"))?;
        let names: Vec<_> = entries
            .iter()
            .map(|entry| hex::encode(&entry.name))
            .collect();
        let rows = self.txn.query_all_raw(sql("SELECT * FROM mst2_metadata_read_source_entries($1::uuid,$2::uuid,$3,$4,$5,$6::jsonb)",
            [self.operation.to_string().into(), source.to_string().into(), directory.to_vec().into(),
                binding.generation.into(), binding.certificate.to_vec().into(),json!(names).into()])).await.map_err(database_error)?;
        #[cfg(test)]
        if entries.iter().any(|entry| !entry.is_dir())
            && let Ok((admitted, resume)) = SOURCE_FACT_BARRIERS.try_with(Clone::clone)
        {
            admitted.wait().await;
            resume.wait().await;
        }
        if rows.len() != entries.len() {
            return Err(integrity(
                "qualified source-name index differs from selected actual page entries",
            ));
        }
        let mut children = HashMap::new();
        for row in rows {
            let name: Vec<u8> = row.try_get("", "name").map_err(internal)?;
            let entry = entries
                .iter()
                .find(|entry| entry.name == name)
                .ok_or_else(|| {
                    integrity("qualified source-name read returned an unrequested occurrence")
                })?;
            let state: String = row.try_get("", "fact_state").map_err(internal)?;
            match state.as_str() {
                "READY" => {}
                "MISSING" => {
                    return Err(SnapshotError::new(
                        crate::ceres::snapshot::error::SnapshotErrorCode::MetadataNotReady,
                        "selected fixed file has no current verified object metadata",
                    ));
                }
                "SOURCE_UNAVAILABLE" => {
                    return Err(unavailable(
                        "selected fixed directory has no current exact source attestation",
                    ));
                }
                _ => {
                    return Err(integrity(
                        "selected fixed file has invalid or different current verified object metadata",
                    ));
                }
            }
            let kind: i16 = row.try_get("", "kind").map_err(internal)?;
            if kind as u8 != entry.kind as u8 {
                return Err(integrity(
                    "qualified selected occurrence changed its fixed filesystem kind",
                ));
            }
            if entry.is_dir() {
                let child = self.bindings.get(&entry.child_root).ok_or_else(|| {
                    integrity("qualified source child has no certified occurrence")
                })?;
                let root = digest_column(&row, "child_root")?;
                if root != entry.child_root
                    || child.generation
                        != row
                            .try_get::<i64>("", "child_generation")
                            .map_err(internal)?
                    || child.certificate != digest_column(&row, "child_certificate_digest")?
                {
                    return Err(integrity(
                        "qualified source-name directory differs from its exact certified lifetime",
                    ));
                }
                children.insert(
                    name.clone(),
                    row.try_get("", "child_attestation_id").map_err(internal)?,
                );
            } else if row.try_get::<i64>("", "byte_size").map_err(internal)? as u64 != entry.size
                || digest_column(&row, "content_digest")? != entry.content_id
            {
                return Err(integrity(
                    "qualified source-name file differs from its certified actual page",
                ));
            }
            if !entry.is_dir() {
                self.file_oids.insert(
                    (source, name),
                    row.try_get("", "git_oid").map_err(internal)?,
                );
            }
        }
        Ok(children)
    }

    fn proofs(&self) -> Result<ProofPages, SnapshotError> {
        let bytes: usize = self.cache.values().map(|page| page.bytes.len()).sum();
        if bytes > 1_048_576 {
            return Err(SnapshotError::new(
                crate::ceres::snapshot::error::SnapshotErrorCode::ProofBudgetExceeded,
                "qualified proof pages exceed the response budget; use metadata/pages",
            ));
        }
        let mut pages: Vec<_> = self
            .cache
            .iter()
            .map(|(id, page)| (*id, page.bytes.clone()))
            .collect();
        pages.sort_by_key(|item| item.0);
        Ok(pages)
    }

    async fn range(
        &mut self,
        id: [u8; 32],
        after: Option<&[u8]>,
        count: usize,
        out: &mut Vec<mst2_codec::metapage::Entry>,
    ) -> Result<(), SnapshotError> {
        if out.len() >= count {
            return Ok(());
        }
        self.load(id).await?;
        match self.cache[&id].page.clone() {
            Page::Leaf { entries } => {
                let start = after
                    .map(|name| entries.partition_point(|entry| entry.name.as_slice() <= name))
                    .unwrap_or(0);
                out.extend(entries.into_iter().skip(start).take(count - out.len()));
            }
            Page::Branch {
                prefix,
                terminal,
                children,
            } => {
                if let Some(entry) = terminal
                    && after.is_none_or(|name| entry.name.as_slice() > name)
                {
                    out.push(entry);
                }
                for child in children {
                    if out.len() >= count {
                        break;
                    }
                    let mut partition = prefix.clone();
                    partition.push(child.label);
                    if after.is_some_and(|name| {
                        partition.as_slice() < name && !name.starts_with(&partition)
                    }) {
                        continue;
                    }
                    self.descend(id, child.label).await?;
                    Box::pin(self.range(child.child_page_id, after, count, out)).await?;
                }
            }
        }
        Ok(())
    }

    async fn find_entry(
        &mut self,
        mut id: [u8; 32],
        name: &[u8],
    ) -> Result<Option<mst2_codec::metapage::Entry>, SnapshotError> {
        loop {
            self.load(id).await?;
            match &self.cache[&id].page {
                Page::Leaf { entries } => {
                    return Ok(entries.iter().find(|entry| entry.name == name).cloned());
                }
                Page::Branch {
                    prefix,
                    terminal,
                    children,
                } => {
                    if name == prefix {
                        return Ok(terminal.clone());
                    }
                    if !name.starts_with(prefix) {
                        return Ok(None);
                    }
                    let Some(label) = name.get(prefix.len()).copied() else {
                        return Ok(None);
                    };
                    if !children.iter().any(|child| child.label == label) {
                        return Ok(None);
                    }
                    id = self.descend(id, label).await?;
                }
            }
        }
    }

    async fn lookup(
        &mut self,
        mut root: [u8; 32],
        path: &str,
    ) -> Result<RootedLookupStatus, SnapshotError> {
        if path == "/" {
            self.load(root).await?;
            return Ok(RootedLookupStatus::Directory(root));
        }
        let mut prefix = String::new();
        let mut source = self.source_ids["/"];
        let mut parts = path[1..].split('/').peekable();
        while let Some(name) = parts.next() {
            let Some(entry) = self.find_entry(root, name.as_bytes()).await? else {
                return Ok(RootedLookupStatus::Absent);
            };
            if !entry.is_dir() {
                if parts.peek().is_some() {
                    return Ok(RootedLookupStatus::NotDirectory {
                        symlink: entry.kind == mst2_codec::metapage::EntryKind::Symlink,
                    });
                }
                self.source_entries(source, root, std::slice::from_ref(&entry))
                    .await?;
                let git_oid = self
                    .file_oids
                    .get(&(source, entry.name.clone()))
                    .cloned()
                    .ok_or_else(|| {
                        integrity("selected fixed file has no exact source OID binding")
                    })?;
                return Ok(RootedLookupStatus::File { entry, git_oid });
            }
            let children = self
                .source_entries(source, root, std::slice::from_ref(&entry))
                .await?;
            source = *children.get(&entry.name).ok_or_else(|| {
                integrity("qualified named directory has no independently derived source child")
            })?;
            root = entry.child_root;
            prefix.push('/');
            prefix.push_str(name);
            self.directories.insert(prefix.clone(), root);
            self.source_ids.insert(prefix.clone(), source);
        }
        self.load(root).await?;
        Ok(RootedLookupStatus::Directory(root))
    }
    async fn load(&mut self, id: [u8; 32]) -> Result<(), SnapshotError> {
        self.work.walk_visits += 1;
        if self.work.walk_visits > self.limits.prepare_entry_visits as u64 {
            return Err(limit("qualified route work budget exceeded"));
        }
        if self.cache.contains_key(&id) {
            return Ok(());
        }
        if self.cache.len() >= self.limits.nodes {
            return Err(limit("qualified route page budget exceeded"));
        }
        let binding = *self.bindings.get(&id).ok_or_else(|| {
            integrity("qualified page was not reached through a certified occurrence")
        })?;
        self.work.page_queries += 1;
        let row = self
            .txn
            .query_one_raw(sql(
                PAGE_SQL,
                [
                    id.to_vec().into(),
                    binding.generation.into(),
                    binding.certificate.to_vec().into(),
                    self.operation.to_string().into(),
                ],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                unavailable("qualified certified page or reader ownership is unavailable")
            })?;
        let bytes: Vec<u8> = row.try_get("", "payload").map_err(internal)?;
        let size: i32 = row.try_get("", "byte_size").map_err(internal)?;
        if !(HEADER_LEN..=PAGE_MAX_BYTES).contains(&bytes.len())
            || bytes.len() != size as usize
            || page_id(&bytes) != id
        {
            return Err(integrity(
                "qualified durable metadata bytes differ from their certified page",
            ));
        }
        let (page, entries) = Page::decode(&bytes).map_err(internal)?;
        self.work.payload_bytes = self
            .work
            .payload_bytes
            .checked_add(bytes.len() as u64)
            .filter(|value| *value <= self.limits.payload_bytes)
            .ok_or_else(|| limit("qualified route byte budget exceeded"))?;
        let raw_refs: serde_json::Value = row.try_get("", "references").map_err(internal)?;
        let refs: Vec<Reference> = serde_json::from_value(raw_refs).map_err(internal)?;
        if refs.len() > 257 {
            return Err(integrity("qualified canonical reference count is invalid"));
        }
        let mut expected = Vec::new();
        let direct = match &page {
            Page::Leaf { entries } => entries.as_slice(),
            Page::Branch {
                terminal, children, ..
            } => {
                for child in children {
                    expected.push((
                        "RADIX",
                        child.child_page_id,
                        None,
                        Some(child.label),
                        Some(child.subtree_entries),
                    ));
                }
                terminal.as_slice()
            }
        };
        // Reference ordering is direct DIRECTORY occurrences followed by RADIX,
        // as defined by the independent canonical parser. Compare occurrences
        // as a set here; SQL validates the certificate's ordinal completeness.
        for entry in direct.iter().filter(|entry| entry.is_dir()) {
            expected.push((
                "DIRECTORY",
                entry.child_root,
                Some(hex::encode(&entry.name)),
                None,
                None,
            ));
        }
        let mut actual = BTreeSet::new();
        for reference in refs {
            let child = digest_hex(&reference.child)?;
            let binding = Binding {
                generation: reference.generation,
                certificate: digest_hex(&reference.certificate)?,
            };
            if binding.generation <= 0
                || self
                    .bindings
                    .get(&child)
                    .is_some_and(|known| *known != binding)
            {
                return Err(integrity(
                    "qualified occurrence retargeted its exact lifetime or certificate",
                ));
            }
            self.bindings.insert(child, binding);
            if !actual.insert((
                reference.kind,
                child,
                reference.name,
                reference.label,
                reference.count,
            )) {
                return Err(integrity("qualified typed occurrence has a duplicate"));
            }
        }
        let expected: BTreeSet<_> = expected
            .into_iter()
            .map(|(kind, child, name, label, count)| (kind.to_owned(), child, name, label, count))
            .collect();
        if actual != expected {
            return Err(integrity(
                "qualified certificate references differ from its actual durable page",
            ));
        }
        self.work.edge_references_checked += actual.len() as u64;
        if self.work.edge_references_checked > self.limits.edges as u64 {
            return Err(limit("qualified route reference budget exceeded"));
        }
        self.work.pages_loaded += 1;
        self.cache.insert(
            id,
            StoredPage {
                bytes,
                page,
                entries,
            },
        );
        Ok(())
    }

    async fn descend(&mut self, parent: [u8; 32], label: u8) -> Result<[u8; 32], SnapshotError> {
        self.load(parent).await?;
        let (prefix, child) = match &self.cache[&parent].page {
            Page::Branch {
                prefix, children, ..
            } => (
                prefix.clone(),
                children
                    .iter()
                    .find(|child| child.label == label)
                    .ok_or_else(|| absent("route label is absent in fixed qualified metadata"))?
                    .clone(),
            ),
            Page::Leaf { .. } => return Err(absent("route descends past a qualified leaf")),
        };
        let id = child.child_page_id;
        self.load(id).await?;
        let received = &self.cache[&id];
        let mut partition = prefix;
        partition.push(label);
        let valid = match &received.page {
            Page::Leaf { entries } => entries
                .iter()
                .all(|entry| entry.name.starts_with(&partition)),
            Page::Branch { prefix, .. } => prefix.starts_with(&partition),
        };
        if !valid || received.entries != child.subtree_entries {
            return Err(integrity(
                "qualified radix partition differs from its exact child",
            ));
        }
        Ok(id)
    }

    async fn directory(
        &mut self,
        mut root: [u8; 32],
        path: &str,
    ) -> Result<[u8; 32], SnapshotError> {
        if path == "/" {
            return Ok(root);
        }
        let mut directory_path = String::new();
        let mut source = self.source_ids["/"];
        for name in path[1..].split('/') {
            let mut id = root;
            loop {
                self.load(id).await?;
                let entry = match &self.cache[&id].page {
                    Page::Leaf { entries } => entries
                        .iter()
                        .find(|entry| entry.name == name.as_bytes())
                        .cloned(),
                    Page::Branch {
                        prefix, terminal, ..
                    } => {
                        if name.as_bytes() == prefix {
                            terminal.clone()
                        } else {
                            if !name.as_bytes().starts_with(prefix) {
                                return Err(absent("name is absent in qualified directory"));
                            }
                            let label =
                                name.as_bytes().get(prefix.len()).copied().ok_or_else(|| {
                                    absent("name is absent in qualified directory")
                                })?;
                            id = self.descend(id, label).await?;
                            continue;
                        }
                    }
                }
                .ok_or_else(|| absent("name is absent in qualified directory"))?;
                if !entry.is_dir() {
                    return Err(SnapshotError::new(
                        crate::ceres::snapshot::error::SnapshotErrorCode::NotDirectory,
                        "fixed qualified path is not a directory",
                    ));
                }
                let children = self
                    .source_entries(source, root, std::slice::from_ref(&entry))
                    .await?;
                source = *children.get(&entry.name).ok_or_else(|| {
                    integrity("qualified directory route lost its exact source-name binding")
                })?;
                root = entry.child_root;
                directory_path.push('/');
                directory_path.push_str(name);
                self.directories.insert(directory_path.clone(), root);
                self.source_ids.insert(directory_path.clone(), source);
                break;
            }
        }
        Ok(root)
    }
    async fn route(
        &mut self,
        root: [u8; 32],
        labels: &[u8],
    ) -> Result<Vec<[u8; 32]>, SnapshotError> {
        self.load(root).await?;
        let mut pages = vec![root];
        let mut current = root;
        for label in labels {
            current = self.descend(current, *label).await?;
            pages.push(current);
        }
        Ok(pages)
    }
}

const PAGE_SQL:&str="SELECT body.payload,body.byte_size,proof.canonical_proof->'references' AS references
 FROM mst2_metadata_page_certificate proof JOIN mst2_metadata_current cur USING(page_id,generation)
 JOIN mst2_metadata_lifetime life USING(page_id,generation) JOIN mst2_metadata_graph_node node USING(page_id,generation)
 JOIN mst2_metadata_payload body USING(page_id,generation)
 WHERE proof.page_id=$1 AND proof.generation=$2 AND proof.certificate_digest=$3
 AND proof.namespace_uuid=(SELECT namespace_uuid FROM mst2_metadata_family_identity WHERE singleton=1)
 AND life.state='LIVE' AND life.graph_domain='qualified-v1' AND node.state='LIVE'
 AND node.certificate_digest=proof.certificate_digest AND node.bytes=proof.byte_size
 AND life.expected_size=proof.byte_size AND body.byte_size=proof.byte_size AND body.metadata_codec=1
 AND octet_length(body.payload)=proof.byte_size AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc
   WHERE gc.page_id=proof.page_id AND gc.generation=proof.generation)
 AND EXISTS(SELECT 1 FROM mst2_metadata_reader_operation reader JOIN mst2_metadata_root_anchor anchor
   ON anchor.reader_operation_id=reader.operation_id AND anchor.anchor_kind='READER'
     AND anchor.root_page=reader.root_page AND anchor.root_generation=reader.root_generation
   WHERE reader.operation_id=$4::uuid AND reader.state='ACTIVE'
     AND reader.hard_deadline_unix>floor(extract(epoch FROM clock_timestamp()))::bigint)
 AND (SELECT count(*) FROM mst2_metadata_verified_ref ref WHERE ref.parent_page=proof.page_id
   AND ref.parent_generation=proof.generation)=jsonb_array_length(proof.canonical_proof->'references')
 AND NOT EXISTS((SELECT ref.child_page,ref.child_generation FROM mst2_metadata_verified_ref ref
   WHERE ref.parent_page=proof.page_id AND ref.parent_generation=proof.generation) EXCEPT
   (SELECT edge.child_page,edge.child_generation FROM mst2_metadata_graph_edge edge
   WHERE edge.parent_page=proof.page_id AND edge.parent_generation=proof.generation))
 AND NOT EXISTS((SELECT edge.child_page,edge.child_generation FROM mst2_metadata_graph_edge edge
   WHERE edge.parent_page=proof.page_id AND edge.parent_generation=proof.generation) EXCEPT
   (SELECT ref.child_page,ref.child_generation FROM mst2_metadata_verified_ref ref
   WHERE ref.parent_page=proof.page_id AND ref.parent_generation=proof.generation))";
