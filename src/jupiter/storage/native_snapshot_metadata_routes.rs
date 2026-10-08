//! Fixed-root persisted META reads under the original generic lease authority.

use std::collections::{BTreeSet, HashMap};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES, Page, page_id};
use sea_orm::{ConnectionTrait, DatabaseTransaction, QueryResult};

use super::{
    PostgresNativeSessionRepository, SnapshotContext, SnapshotError, SnapshotErrorCode,
    decode_context, expired, finish, integrity, internal, routes, statement,
};
use crate::ceres::snapshot::{
    retention_dag::MetadataDagLimits, view::validate_scope_relative_path,
};

pub(crate) struct MetadataRouteRequest<'a> {
    pub directory_path: &'a str,
    pub route: &'a [u8],
    pub expected_digest: Option<&'a str>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PersistedMetadataReadWork {
    pub page_queries: u64,
    pub pages_loaded: u64,
    pub payload_bytes: u64,
    pub walk_visits: u64,
    pub edge_references_checked: u64,
}

pub(crate) struct PersistedMetadataRouteBatch {
    pub pages: Vec<([u8; 32], Vec<u8>)>,
    pub work: PersistedMetadataReadWork,
}

#[cfg(test)]
tokio::task_local! {
    static METADATA_READ_BARRIERS: (std::sync::Arc<tokio::sync::Barrier>, std::sync::Arc<tokio::sync::Barrier>);
}

#[cfg(test)]
pub(crate) async fn with_metadata_read_barriers<F: std::future::Future>(
    captured: std::sync::Arc<tokio::sync::Barrier>,
    release: std::sync::Arc<tokio::sync::Barrier>,
    future: F,
) -> F::Output {
    METADATA_READ_BARRIERS
        .scope((captured, release), future)
        .await
}

impl PostgresNativeSessionRepository {
    pub(crate) async fn metadata_routes(
        &self,
        context: &SnapshotContext,
        requests: &[MetadataRouteRequest<'_>],
    ) -> Result<PersistedMetadataRouteBatch, SnapshotError> {
        if requests.is_empty() || requests.len() > 64 {
            return Err(limit("items must hold 1..64 entries"));
        }
        for request in requests {
            validate_scope_relative_path(request.directory_path)?;
            let scope = &context.built.descriptor.scope;
            let absolute = if scope == "/" {
                request.directory_path.to_owned()
            } else if request.directory_path == "/" {
                scope.clone()
            } else {
                format!("{scope}{}", request.directory_path)
            };
            validate_scope_relative_path(&absolute)?;
        }
        let installer = self.installer().await?;
        let txn = self.transaction().await?;
        let result = async {
            installer.verify_primary_connection(&txn).await?;
            if routes::lease(&txn, installer, &context.lease_id)
                .await?
                .as_deref()
                != Some(context.built.snapshot_id.as_str())
            {
                return Err(expired());
            }
            // This barrier captures the namespace and uses bounded lock acquisition.
            // Do not acquire the publication/route writer lock after retention.
            installer.metadata_read_barrier(&txn).await?;
            if routes::lease(&txn, installer, &context.lease_id)
                .await?
                .as_deref()
                != Some(context.built.snapshot_id.as_str())
            {
                return Err(expired());
            }
            let row = txn
                .query_one_raw(statement(
                    self.session_sql(installer).await,
                    [
                        context.built.snapshot_id.clone().into(),
                        context.lease_id.clone().into(),
                    ],
                ))
                .await
                .map_err(internal)?
                .ok_or_else(expired)?;
            installer.verify_primary_scope_row(&row)?;
            let current = decode_context(
                &row,
                &context.built.snapshot_id,
                &context.lease_id,
                &context.built.instance_id,
            )?;
            if current.built.descriptor != context.built.descriptor
                || current.commit_oid != context.commit_oid
                || current.root_tree_oid != context.root_tree_oid
                || current.authorization_epoch != context.authorization_epoch
            {
                return Err(integrity("fixed session changed during metadata read"));
            }
            let prepare_id: String = row.try_get("", "prepare_id").map_err(internal)?;
            let mut reader = Reader {
                txn: &txn,
                prepare_id,
                codec: context.built.descriptor.metadata_codec() as i16,
                cache: HashMap::new(),
                work: PersistedMetadataReadWork::default(),
                limits: MetadataDagLimits::default(),
            };
            let mut seen = BTreeSet::new();
            let mut pages = Vec::new();
            for request in requests {
                let root = reader
                    .directory(
                        context.built.descriptor.metadata_root,
                        request.directory_path,
                    )
                    .await?;
                let route = reader.route(root, request.route).await?;
                let reached = route
                    .last()
                    .ok_or_else(|| integrity("metadata route is empty"))?;
                if let Some(expected) = request.expected_digest
                    && expected != format!("sha256:{}", hex::encode(reached))
                {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::DigestMismatch,
                        format!(
                            "{}: route does not reach expected_digest",
                            request.directory_path
                        ),
                    ));
                }
                for id in route {
                    if seen.insert(id) {
                        let page = reader
                            .cache
                            .get(&id)
                            .ok_or_else(|| integrity("read metadata page is missing"))?;
                        pages.push((id, page.bytes.clone()));
                    }
                }
            }
            Ok(PersistedMetadataRouteBatch {
                pages,
                work: reader.work,
            })
        }
        .await;
        // All bytes are owned before releasing protection. Frame delivery keeps
        // the existing per-frame authentication and lease revalidation.
        finish(txn, result).await
    }
}

struct StoredPage {
    bytes: Vec<u8>,
    page: Page,
    entries: u64,
}

struct Reader<'a> {
    txn: &'a DatabaseTransaction,
    prepare_id: String,
    codec: i16,
    cache: HashMap<[u8; 32], StoredPage>,
    work: PersistedMetadataReadWork,
    limits: MetadataDagLimits,
}

impl Reader<'_> {
    async fn load(&mut self, id: [u8; 32]) -> Result<(), SnapshotError> {
        self.work.walk_visits += 1;
        if self.work.walk_visits > self.limits.prepare_entry_visits as u64 {
            return Err(limit("metadata route work budget exceeded"));
        }
        if self.cache.contains_key(&id) {
            return Ok(());
        }
        if self.cache.len() >= self.limits.nodes {
            return Err(limit("metadata route page budget exceeded"));
        }
        self.work.page_queries += 1;
        let row = self
            .txn
            .query_one_raw(statement(
                PAGE_SQL,
                [self.prepare_id.clone().into(), id.to_vec().into()],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| unavailable("metadata page is not a prepared member"))?;
        let page = decode_page(&row, id, self.codec)?;
        self.work.payload_bytes = self
            .work
            .payload_bytes
            .checked_add(page.bytes.len() as u64)
            .filter(|bytes| *bytes <= self.limits.payload_bytes)
            .ok_or_else(|| limit("metadata route payload budget exceeded"))?;
        let expected = page_references(&page.page);
        self.work.edge_references_checked += expected.len() as u64;
        if self.work.edge_references_checked > self.limits.edges as u64 {
            return Err(limit("metadata route edge budget exceeded"));
        }
        let edges: String = row.try_get("", "outgoing_edges").map_err(internal)?;
        let edges: Vec<String> = serde_json::from_str(&edges).map_err(internal)?;
        let actual: BTreeSet<_> = edges.iter().cloned().collect();
        if edges.len() != actual.len() || actual != expected {
            return Err(integrity(
                "metadata graph edges disagree with fixed page references",
            ));
        }
        self.work.pages_loaded += 1;
        self.cache.insert(id, page);
        #[cfg(test)]
        if self.cache.len() == 1
            && let Ok((captured, release)) = METADATA_READ_BARRIERS.try_with(Clone::clone)
        {
            captured.wait().await;
            release.wait().await;
        }
        Ok(())
    }

    async fn descend(&mut self, parent: [u8; 32], label: u8) -> Result<[u8; 32], SnapshotError> {
        self.load(parent).await?;
        let (prefix, child) = match &self.cache[&parent].page {
            Page::Branch {
                prefix, children, ..
            } => {
                let child = children
                    .iter()
                    .find(|child| child.label == label)
                    .ok_or_else(|| absent("route label is absent in fixed metadata page"))?;
                (prefix.clone(), child.clone())
            }
            Page::Leaf { .. } => return Err(absent("route descends past a leaf page")),
        };
        let id = child.child_page_id;
        self.load(id).await?;
        let received = &self.cache[&id];
        let mut partition = prefix;
        partition.push(label);
        let valid_prefix = match &received.page {
            Page::Leaf { entries } => entries
                .iter()
                .all(|entry| entry.name.starts_with(&partition)),
            Page::Branch { prefix, .. } => prefix.starts_with(&partition),
        };
        if received.entries != child.subtree_entries || !valid_prefix {
            return Err(integrity(
                "metadata child differs from fixed radix partition",
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
                                return Err(absent("name is absent in fixed directory"));
                            }
                            let label = name
                                .as_bytes()
                                .get(prefix.len())
                                .copied()
                                .ok_or_else(|| absent("name is absent in fixed directory"))?;
                            id = self.descend(id, label).await?;
                            continue;
                        }
                    }
                }
                .ok_or_else(|| absent("name is absent in fixed directory"))?;
                if !entry.is_dir() {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::NotDirectory,
                        format!("{path} is not a directory"),
                    ));
                }
                root = entry.child_root;
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

const PAGE_SQL: &str = "SELECT m.expected_size,m.generation AS member_generation,p.graph_domain AS prepare_graph_domain,
 b.metadata_codec AS payload_codec,b.byte_size AS payload_size,octet_length(b.payload) AS actual_size,
 CASE WHEN octet_length(b.payload) BETWEEN 20 AND 16384 THEN b.payload END AS payload,
 b.generation AS payload_generation,c.generation AS current_generation,
 l.generation AS lifetime_generation,l.graph_domain,l.state AS lifetime_state,
 l.metadata_codec AS lifetime_codec,l.expected_size AS lifetime_size,
 n.state AS graph_state,n.kind AS graph_kind,n.bytes AS graph_bytes,
 EXISTS(SELECT 1 FROM mst2_retention_gc_op g WHERE g.node_id='page:sha256:'||encode(m.page_id,'hex')
   AND g.operation='REMOVE' AND g.state IN ('PENDING','APPLIED')) AS tombstone,
 COALESCE((SELECT jsonb_agg(e.child_id) FROM
   (SELECT child_id FROM mst2_retention_edge WHERE parent_id='page:sha256:'||encode(m.page_id,'hex')
    ORDER BY child_id LIMIT 258) e),'[]'::jsonb)::text AS outgoing_edges
 FROM mst2_metadata_prepare_page m
 JOIN mst2_metadata_prepare p ON p.prepare_id=m.prepare_id
 LEFT JOIN mst2_metadata_payload b ON b.page_id=m.page_id
 LEFT JOIN mst2_metadata_current c ON c.page_id=m.page_id
 LEFT JOIN mst2_metadata_lifetime l ON l.page_id=c.page_id AND l.generation=c.generation
 LEFT JOIN mst2_retention_node n ON n.node_id='page:sha256:'||encode(m.page_id,'hex')
 WHERE m.prepare_id=$1 AND m.page_id=$2";

fn decode_page(row: &QueryResult, id: [u8; 32], codec: i16) -> Result<StoredPage, SnapshotError> {
    if row
        .try_get::<Option<String>>("", "prepare_graph_domain")
        .map_err(internal)?
        .as_deref()
        .is_some_and(|domain| domain != "generic-v1")
    {
        return Err(unavailable(
            "fixed metadata preparation is outside the generic namespace",
        ));
    }
    let size: i32 = row.try_get("", "expected_size").map_err(internal)?;
    if !(HEADER_LEN..=PAGE_MAX_BYTES).contains(&(size as usize))
        || row
            .try_get::<Option<i16>>("", "payload_codec")
            .map_err(internal)?
            != Some(codec)
        || row
            .try_get::<Option<i32>>("", "payload_size")
            .map_err(internal)?
            != Some(size)
        || row
            .try_get::<Option<i32>>("", "actual_size")
            .map_err(internal)?
            != Some(size)
    {
        return Err(unavailable(
            "fixed metadata payload profile or size is unavailable",
        ));
    }
    if row.try_get::<bool>("", "tombstone").map_err(internal)?
        || row
            .try_get::<Option<String>>("", "graph_state")
            .map_err(internal)?
            .as_deref()
            != Some("LIVE")
        || row
            .try_get::<Option<String>>("", "graph_kind")
            .map_err(internal)?
            .as_deref()
            != Some("page")
        || row
            .try_get::<Option<i64>>("", "graph_bytes")
            .map_err(internal)?
            != Some(size as i64)
    {
        return Err(unavailable("fixed generic metadata graph is unavailable"));
    }
    let member: Option<i64> = row.try_get("", "member_generation").map_err(internal)?;
    let payload: Option<i64> = row.try_get("", "payload_generation").map_err(internal)?;
    let current: Option<i64> = row.try_get("", "current_generation").map_err(internal)?;
    let lifetime: Option<i64> = row.try_get("", "lifetime_generation").map_err(internal)?;
    if payload != current || payload != lifetime || member.is_some() && member != payload {
        return Err(integrity(
            "fixed generic page differs from its exact physical lifetime",
        ));
    }
    if let Some(generation) = payload
        && (generation <= 0
            || row
                .try_get::<Option<String>>("", "graph_domain")
                .map_err(internal)?
                .as_deref()
                != Some("generic-v1")
            || row
                .try_get::<Option<String>>("", "lifetime_state")
                .map_err(internal)?
                .as_deref()
                != Some("LIVE")
            || row
                .try_get::<Option<i16>>("", "lifetime_codec")
                .map_err(internal)?
                != Some(codec)
            || row
                .try_get::<Option<i32>>("", "lifetime_size")
                .map_err(internal)?
                != Some(size))
    {
        return Err(unavailable(
            "fixed metadata lifetime cannot be served in the generic namespace",
        ));
    }
    let bytes: Vec<u8> = row
        .try_get::<Option<Vec<u8>>>("", "payload")
        .map_err(internal)?
        .ok_or_else(|| unavailable("fixed metadata payload is missing"))?;
    if page_id(&bytes) != id {
        return Err(integrity("fixed metadata payload digest mismatch"));
    }
    let (page, entries) = Page::decode(&bytes)
        .map_err(|_| integrity("fixed metadata payload is not a canonical page"))?;
    Ok(StoredPage {
        bytes,
        page,
        entries,
    })
}

fn page_references(page: &Page) -> BTreeSet<String> {
    let mut edges = BTreeSet::new();
    let entries = match page {
        Page::Leaf { entries } => entries.as_slice(),
        Page::Branch {
            terminal, children, ..
        } => {
            edges.extend(
                children
                    .iter()
                    .map(|child| format!("page:sha256:{}", hex::encode(child.child_page_id))),
            );
            terminal.as_slice()
        }
    };
    edges.extend(
        entries
            .iter()
            .filter(|entry| entry.is_dir())
            .map(|entry| format!("page:sha256:{}", hex::encode(entry.child_root))),
    );
    edges
}

fn absent(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::PathNotFound, message)
}
fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::ObjectUnavailable, message)
}
fn limit(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LimitExceeded, message)
}
