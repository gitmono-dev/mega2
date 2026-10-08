//! Source-bound chunk-map receipts with indexed, authenticated selected pages.
//!
//! The object writer is trusted like the verified-object writer. A database
//! row, its checksum or a map's presence is never a full-source proof. Only
//! the opaque full-stream verifier can install a source receipt. Persistent
//! owners, reservations and physical generations bound its retention.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use mst2_codec::chunkmap::{CHUNKS_PER_PAGE, ChunkLeaf, ChunkMap, ProofStep, verify_leaf};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, FromQueryResult,
    IsolationLevel, QueryResult, Statement, TransactionTrait,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::object_storage::MegaObjectStorageWrapper;
use crate::{
    callisto::mst2_verified_object,
    ceres::snapshot::{
        chunk_map_index::{ChunkMapNode, indexed_nodes, proof_intervals, selected_proof},
        chunks::{ChunkMapSource, VerifiedSourceChunkMap},
        content_budget::{MemoryBudget, MemoryLease, projection_budget},
        error::{SnapshotError, SnapshotErrorCode},
    },
    orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
};

const DESCRIPTOR_CREDIT: usize = 32 * 1024;
const PAGE_CREDIT: usize = 64 * 1024;
const RECEIPT_DOMAIN: &[u8] = b"MST2-CHUNK-MAP-RECEIPT\0";

pub(crate) mod retention;

#[derive(Clone)]
pub(crate) struct PostgresChunkMapRepository {
    connection: DatabaseConnection,
    primary_scope: Vec<u8>,
    budget: Arc<MemoryBudget>,
    schema: String,
    receipt_observation: Arc<tokio::sync::Mutex<Option<Arc<retention::BackingObservation>>>>,
    cancelled_installs: Arc<std::sync::Mutex<Vec<retention::CancelledInstall>>>,
    admission_maintenance: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
}

pub(crate) struct PersistedChunkMap {
    pub map: ChunkMap,
    pub map_id: [u8; 32],
    source: ChunkMapSource,
    source_id: [u8; 32],
    reader: retention::ChunkMapReader,
    _memory: MemoryLease,
}

impl PersistedChunkMap {
    pub(crate) fn source_id(&self) -> [u8; 32] {
        self.source_id
    }

    pub(crate) async fn ensure_live(&self) -> Result<(), SnapshotError> {
        self.reader.ensure_live().await
    }

    pub(crate) async fn record_progress(&self) -> Result<(), SnapshotError> {
        self.reader.record_progress().await
    }

    pub(crate) async fn await_backend<F: std::future::Future>(
        &self,
        future: F,
    ) -> Result<F::Output, SnapshotError> {
        self.reader.await_backend(future).await
    }

    #[cfg(test)]
    pub(crate) async fn test_check_next_owner_operation(&self) {
        self.reader.test_check_next_owner_operation().await;
    }
}

pub(crate) struct AuthenticatedChunkPage {
    pub leaf: ChunkLeaf,
    pub proof: Vec<ProofStep>,
    _memory: MemoryLease,
}

impl AuthenticatedChunkPage {
    pub(crate) fn verify_chunk(
        &self,
        map: &ChunkMap,
        index: u64,
        bytes: &[u8],
    ) -> Result<(), SnapshotError> {
        let length = map
            .chunk_len(index)
            .map_err(|_| integrity("invalid requested chunk index"))?;
        if index / CHUNKS_PER_PAGE as u64 != self.leaf.page_index || bytes.len() as u64 != length {
            return Err(integrity(
                "exact range length or selected chunk page disagrees",
            ));
        }
        let slot = (index % CHUNKS_PER_PAGE as u64) as usize;
        let expected = self
            .leaf
            .chunk_sha256
            .get(slot)
            .ok_or_else(|| integrity("selected chunk digest is missing"))?;
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if &digest != expected {
            return Err(integrity(
                "exact range digest disagrees with the authenticated selected page",
            ));
        }
        Ok(())
    }
}

impl PostgresChunkMapRepository {
    pub(crate) async fn new(connection: DatabaseConnection) -> Result<Self, SnapshotError> {
        let schema = capture_schema(&connection).await?;
        let primary_scope = read_scope(&connection, &schema).await?;
        Ok(Self {
            connection,
            primary_scope,
            budget: projection_budget().clone(),
            schema,
            receipt_observation: Arc::default(),
            cancelled_installs: Arc::default(),
            admission_maintenance: Arc::default(),
        })
    }

    pub(crate) async fn read(
        &self,
        source: &ChunkMapSource,
        objects: &MegaObjectStorageWrapper,
    ) -> Result<Option<Arc<PersistedChunkMap>>, SnapshotError> {
        for pass in 0..2 {
            let (map, pending) = self.read_current(source, objects).await?;
            if !pending {
                return Ok(map);
            }
            if pass == 0 {
                self.maintain(objects, 8).await?;
            }
        }
        Err(SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "previous source receipt deletion is still pending",
        ))
    }

    async fn read_current(
        &self,
        source: &ChunkMapSource,
        objects: &MegaObjectStorageWrapper,
    ) -> Result<(Option<Arc<PersistedChunkMap>>, bool), SnapshotError> {
        let memory = self.budget.reserve(DESCRIPTOR_CREDIT)?;
        let txn = self.transaction().await?;
        let result = async {
            self.require_scope(&txn).await?;
            require_current_source(&txn, source, &self.schema).await?;
            let Some(row) = source_row(&txn, source, &self.schema).await? else {
                let pending = self.require_missing_source_retired(&txn, source).await?;
                return Ok((None, pending));
            };
            let (map, source_id) = self.validate_source_row(source, &row)?;
            let generation_state: Option<String> =
                row.try_get("", "generation_state").map_err(db_error)?;
            if generation_state.as_deref() == Some("DELETING") {
                // A legitimate retirement can still retain its source row
                // until the physical delete completes. Replay it just like
                // a missing retired row, after checking every source fact;
                // damaged LIVE rows never enter this cold-rebuild path.
                return Ok((None, true));
            }
            let bytes = receipt(&self.primary_scope, source, &map)?;
            let reader = self.admit_reader(&txn, source, &row, &map).await?;
            let key = ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: row.try_get("", "receipt_key").map_err(db_error)?,
            };
            Ok((
                Some((
                    Arc::new(PersistedChunkMap {
                        map_id: map.map_id(),
                        map,
                        source: source.clone(),
                        source_id,
                        reader,
                        _memory: memory,
                    }),
                    key,
                    bytes,
                )),
                false,
            ))
        }
        .await;
        let (admitted, pending) = finish(txn, result).await?;
        let Some((map, key, bytes)) = admitted else {
            return Ok((None, pending));
        };
        // Admission is durable before I/O. No completion barrier or fact row
        // lock is held while the independent backend receipt is consumed.
        map.await_backend(read_receipt(objects, &key, &bytes))
            .await??;
        map.reader.validate_after_receipt().await?;
        Ok((Some(map), false))
    }

    /// Publication accepts no caller-provided map or "verified" flag.
    pub(crate) async fn install(
        &self,
        verified: VerifiedSourceChunkMap,
        objects: &MegaObjectStorageWrapper,
        admission: &retention::ChunkMapInstall,
    ) -> Result<(), SnapshotError> {
        let result = self.install_reserved(verified, objects, admission).await;
        if result.is_err() {
            if let Err(error) = admission.retire().await {
                tracing::warn!(
                    ?error,
                    "failed chunk-map installation will retire by database deadline"
                );
            } else if let Err(error) = self.collect_maps(64).await {
                tracing::warn!(
                    ?error,
                    "retired partial chunk-map indexes await bounded collection replay"
                );
            }
        }
        result
    }

    async fn install_reserved(
        &self,
        verified: VerifiedSourceChunkMap,
        objects: &MegaObjectStorageWrapper,
        admission: &retention::ChunkMapInstall,
    ) -> Result<(), SnapshotError> {
        let source = verified.source();
        let map = verified.map();
        let nodes = indexed_nodes(verified.leaf_hashes())?;
        if nodes.last().map(|node| node.digest) != Some(map.pages_root) {
            return Err(integrity(
                "verified chunk map index disagrees with canonical root",
            ));
        }
        let bytes = admission.prepare(&verified).await?;
        let key = admission.key();
        admission
            .await_backend(objects.inner.put_metadata_atomic_create(
                key,
                Bytes::copy_from_slice(&bytes),
                ObjectMeta {
                    size: bytes.len() as i64,
                    ..Default::default()
                },
            ))
            .await?
            .map_err(storage_error)?;
        admission.acknowledge_create().await?;
        admission
            .await_backend(read_receipt(objects, key, &bytes))
            .await??;
        let observation = retention::inventory(self, objects).await?;
        let map_generation = self
            .stage_map_start(&verified, admission, &observation)
            .await?;
        for batch in verified.leaves().chunks(64) {
            self.stage_map_batch(&verified, admission, map_generation, Some(batch), None)
                .await?;
        }
        for batch in nodes.chunks(1024) {
            self.stage_map_batch(&verified, admission, map_generation, None, Some(batch))
                .await?;
        }
        self.compare_staged_map(&verified, &nodes, admission, map_generation)
            .await?;
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            require_current_source(&txn,source,&self.schema).await?;
            retention::map_barrier(&txn,map.map_id()).await?;
            retention::barrier(&txn,&self.schema).await?;
            retention::ensure_quotas(&txn,&self.schema,&observation,0,0,0,Some(admission.owner())).await?;
            let admitted=txn.query_one_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET state='LIVE',owner=NULL,deadline=NULL,reserved_pages=0,reserved_nodes=0,reserved_bytes=0,last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state='CREATING' AND deadline>pg_catalog.clock_timestamp() AND create_completed AND map_id=$4 AND map_generation=$5 AND receipt_bytes=$6 AND EXISTS(SELECT 1 FROM mst2_chunk_map_lifetime WHERE map_id=$4 AND generation=$5 AND state='LIVE') RETURNING generation",[key.key.clone().into(),admission.generation().into(),admission.owner().to_string().into(),map.map_id().to_vec().into(),map_generation.into(),bytes.clone().into()])).await.map_err(db_error)?;
            if admitted.is_none() {return Err(SnapshotError::new(SnapshotErrorCode::LeaseExpired,"chunk-map publication lost its exact live install reservation"));}
            let fact=source.fact();
            let source_bytes=source.canonical_bytes()?;
            let source_id=source_id(&self.primary_scope,&source_bytes);
            txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map_source(storage_domain,git_oid,object_kind,fact_id,source_id,source_bytes,primary_scope,map_id,receipt_digest,receipt_generation) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(storage_domain,git_oid,object_kind) DO NOTHING",[fact.storage_domain.clone().into(),fact.git_oid.clone().into(),fact.object_kind.clone().into(),fact.id.into(),source_id.to_vec().into(),source_bytes.into(),self.primary_scope.clone().into(),map.map_id().to_vec().into(),Sha256::digest(&bytes).to_vec().into(),admission.generation().into()])).await.map_err(db_error)?;
            let row=source_row(&txn,source,&self.schema).await?.ok_or_else(||integrity("installed source receipt is missing"))?;
            let (stored,_)=self.validate_source_row(source,&row)?;
            if stored!=*map || row.try_get::<String>("","receipt_key").map_err(db_error)?!=key.key {return Err(integrity("concurrent source installation conflicts"));}
            Ok(())
        }.await;
        finish(txn, result).await
    }
    pub(crate) async fn selected_page(
        &self,
        map: &PersistedChunkMap,
        page_index: u64,
    ) -> Result<Arc<AuthenticatedChunkPage>, SnapshotError> {
        map.ensure_live().await?;
        let intervals = proof_intervals(map.map.page_count, page_index)?;
        let memory = self.budget.reserve(PAGE_CREDIT)?;
        let txn = self.transaction().await?;
        let result = async {
            self.require_scope(&txn).await?;
            require_current_source(&txn, &map.source, &self.schema).await?;
            let row = txn.query_one_raw(stmt(&self.schema, "SELECT pg_catalog.octet_length(payload) AS size,CASE WHEN pg_catalog.octet_length(payload) BETWEEN 48 AND 8208 THEN payload ELSE NULL END AS payload FROM mst2_chunk_map_leaf WHERE map_id=$1 AND page_index=$2",
                [map.map_id.to_vec().into(), (page_index as i32).into()])).await.map_err(db_error)?
                .ok_or_else(|| integrity("admitted chunk map selected leaf is missing"))?;
            let bytes = row.try_get::<Option<Vec<u8>>>("", "payload").map_err(db_error)?
                .ok_or_else(|| integrity("admitted chunk map selected leaf exceeds its byte profile"))?;
            let leaf = ChunkLeaf::decode(&bytes).map_err(|_| integrity("admitted chunk map selected leaf is not canonical MCL2"))?;
            if leaf.page_index != page_index || leaf.chunk_sha256.len() as u64 != ChunkLeaf::expected_count(map.map.chunk_count, page_index)
                || leaf.encode().map_err(|_| integrity("invalid selected leaf"))? != bytes {
                return Err(integrity("selected chunk map leaf identity or chunk count disagrees"));
            }
            let requested: Vec<_> = intervals.iter().map(|&(_, start, pages)| json!({"first_page":start,"page_count":pages})).collect();
            let encoded = serde_json::to_string(&requested).map_err(db_error)?;
            let rows = txn.query_all_raw(stmt(&self.schema, "SELECT n.first_page,n.page_count,CASE WHEN pg_catalog.octet_length(n.digest)=32 THEN n.digest ELSE NULL END AS digest FROM pg_catalog.jsonb_to_recordset($2::jsonb) AS p(first_page integer,page_count integer) JOIN mst2_chunk_map_node n ON n.map_id=$1 AND n.first_page=p.first_page AND n.page_count=p.page_count",
                [map.map_id.to_vec().into(), encoded.into()])).await.map_err(db_error)?;
            let mut nodes = Vec::with_capacity(rows.len());
            for row in rows {
                let digest: Option<Vec<u8>> = row.try_get("", "digest").map_err(db_error)?;
                nodes.push(ChunkMapNode {
                    start: row.try_get::<i32>("", "first_page").map_err(db_error)? as u64,
                    pages: row.try_get::<i32>("", "page_count").map_err(db_error)? as u64,
                    digest: digest.ok_or_else(|| integrity("admitted chunk map sibling has invalid digest size"))?.as_slice().try_into().map_err(|_| integrity("invalid persisted sibling digest"))?,
                });
            }
            let proof = selected_proof(&intervals, &nodes)?;
            verify_leaf(map.map.page_count, page_index, leaf.leaf_hash().map_err(|_| integrity("invalid leaf digest"))?, &proof, map.map.pages_root)
                .map_err(|_| integrity("selected chunk map leaf or proof authentication failed"))?;
            tracing::debug!(target:"mst2::chunk_map", selected_leaf_bytes = bytes.len(), selected_sibling_rows = nodes.len(), page_count = map.map.page_count, "authenticated persisted chunk map selected page");
            Ok(Arc::new(AuthenticatedChunkPage { leaf, proof, _memory: memory }))
        }.await;
        let page = finish(txn, result).await?;
        map.ensure_live().await?;
        map.record_progress().await?;
        Ok(page)
    }

    async fn transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        let txn = self
            .connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(db_error)?;
        let search_path = format!("{},pg_catalog,pg_temp", quoted(&self.schema));
        txn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_catalog.set_config('search_path',$1,true)",
            [search_path.into()],
        ))
        .await
        .map_err(db_error)?;
        txn.execute_unprepared("SET LOCAL statement_timeout='5s'; SET LOCAL lock_timeout='5s'")
            .await
            .map_err(db_error)?;
        Ok(txn)
    }

    async fn require_scope<C: ConnectionTrait>(&self, connection: &C) -> Result<(), SnapshotError> {
        if read_scope(connection, &self.schema).await? != self.primary_scope {
            return Err(integrity(
                "chunk map repository no longer targets its captured primary scope",
            ));
        }
        Ok(())
    }

    pub(crate) fn memory_budget(&self) -> &Arc<MemoryBudget> {
        &self.budget
    }

    pub(crate) fn source_identity(
        &self,
        source: &ChunkMapSource,
    ) -> Result<[u8; 32], SnapshotError> {
        Ok(source_id(&self.primary_scope, &source.canonical_bytes()?))
    }

    #[cfg(test)]
    pub(crate) fn with_test_budget(mut self, budget: Arc<MemoryBudget>) -> Self {
        self.budget = budget;
        self
    }

    #[cfg(test)]
    pub(crate) fn test_primary_scope(&self) -> &[u8] {
        &self.primary_scope
    }

    fn validate_source_row(
        &self,
        source: &ChunkMapSource,
        row: &QueryResult,
    ) -> Result<(ChunkMap, [u8; 32]), SnapshotError> {
        let source_bytes = source.canonical_bytes()?;
        let source_id = source_id(&self.primary_scope, &source_bytes);
        let source_bytes_row = bounded_bytes(row, "source_bytes")?;
        let primary_scope_row = bounded_bytes(row, "primary_scope")?;
        if row.try_get::<i64>("", "fact_id").map_err(db_error)? != source.fact().id
            || source_bytes_row != source_bytes
            || primary_scope_row != self.primary_scope
            || bounded_bytes(row, "source_id")? != source_id
        {
            return Err(integrity(
                "persisted map admission disagrees with the exact current source fact or primary",
            ));
        }
        let bytes = row
            .try_get::<Option<Vec<u8>>>("", "descriptor")
            .map_err(db_error)?
            .ok_or_else(|| {
                integrity("admitted chunk map descriptor is missing or outside its byte profile")
            })?;
        let map = ChunkMap::decode(&bytes)
            .map_err(|_| integrity("admitted chunk map descriptor is invalid MCM2"))?;
        let receipt_digest: [u8; 32] =
            Sha256::digest(receipt(&self.primary_scope, source, &map)?).into();
        if map.encode() != bytes
            || map.map_id().as_slice() != bounded_bytes(row, "map_id")?.as_slice()
            || map.file_content_id.as_slice() != source.fact().raw_sha256.as_slice()
            || map.file_size != source.fact().size as u64
            || map.page_count != row.try_get::<i32>("", "page_count").map_err(db_error)? as u64
            || map.pages_root.as_slice() != bounded_bytes(row, "pages_root")?.as_slice()
            || bounded_bytes(row, "receipt_digest")? != receipt_digest
        {
            return Err(integrity(
                "admitted chunk map descriptor or receipt identity disagrees",
            ));
        }
        Ok((map, source_id))
    }
}

async fn capture_schema<C: ConnectionTrait>(connection: &C) -> Result<String, SnapshotError> {
    let row = connection.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT n.nspname AS schema FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_class c ON c.relnamespace=n.oid AND c.relname='mst2_metadata_storage_scope' AND c.relkind='r' WHERE n.nspname=pg_catalog.current_schema() AND n.nspname NOT LIKE 'pg_temp_%'"))
        .await.map_err(db_error)?.ok_or_else(|| integrity("chunk map actual primary storage relation is missing"))?;
    row.try_get("", "schema").map_err(db_error)
}

async fn read_scope<C: ConnectionTrait>(
    connection: &C,
    schema: &str,
) -> Result<Vec<u8>, SnapshotError> {
    if connection.get_database_backend() != DbBackend::Postgres {
        return Err(integrity("chunk map repository requires PostgreSQL"));
    }
    let row = connection.query_one_raw(stmt(schema, "SELECT pg_catalog.pg_is_in_recovery() AS replica,CASE WHEN pg_catalog.octet_length(s.storage_uuid)<=255 THEN s.storage_uuid ELSE NULL END AS storage_uuid,pg_catalog.current_database() AS database,d.oid::bigint AS database_oid,n.nspname AS schema,n.oid::bigint AS schema_oid,pg_catalog.inet_server_addr()::text AS address,pg_catalog.inet_server_port() AS port FROM mst2_metadata_storage_scope s JOIN pg_catalog.pg_database d ON d.datname=pg_catalog.current_database() JOIN pg_catalog.pg_namespace n ON n.nspname=$1 JOIN pg_catalog.pg_class c ON c.relnamespace=n.oid AND c.relname='mst2_metadata_storage_scope' AND c.relkind='r' WHERE s.singleton=1", [schema.into()]))
        .await.map_err(db_error)?.ok_or_else(|| integrity("chunk map primary scope is missing"))?;
    if row.try_get::<bool>("", "replica").map_err(db_error)? {
        return Err(integrity("chunk map repository is on a replica"));
    }
    let scope = serde_json::to_vec(&(
        row.try_get::<Option<String>>("", "storage_uuid")
            .map_err(db_error)?
            .ok_or_else(|| integrity("chunk map primary scope UUID exceeds its bounded profile"))?,
        row.try_get::<String>("", "database").map_err(db_error)?,
        row.try_get::<i64>("", "database_oid").map_err(db_error)?,
        row.try_get::<String>("", "schema").map_err(db_error)?,
        row.try_get::<i64>("", "schema_oid").map_err(db_error)?,
        row.try_get::<Option<String>>("", "address")
            .map_err(db_error)?,
        row.try_get::<Option<i32>>("", "port").map_err(db_error)?,
    ))
    .map_err(db_error)?;
    if scope.len() > 1024 {
        return Err(integrity(
            "chunk map primary scope exceeds its admitted profile",
        ));
    }
    Ok(scope)
}

async fn require_current_source<C: ConnectionTrait>(
    connection: &C,
    source: &ChunkMapSource,
    schema: &str,
) -> Result<(), SnapshotError> {
    let row = connection
        .query_one_raw(stmt(schema,
            "SELECT id,CASE WHEN storage_domain='git' THEN storage_domain ELSE NULL END AS storage_domain,CASE WHEN git_oid=$2 AND pg_catalog.octet_length(git_oid) IN (40,64) THEN git_oid ELSE NULL END AS git_oid,CASE WHEN object_kind='blob' THEN object_kind ELSE NULL END AS object_kind,CASE WHEN pg_catalog.octet_length(raw_sha256)=32 THEN raw_sha256 ELSE NULL END AS raw_sha256,CASE WHEN size BETWEEN 1 AND 8796093022208 THEN size ELSE NULL END AS size,CASE WHEN verification_version=2 THEN verification_version ELSE NULL END AS verification_version,CASE WHEN state='VERIFIED' THEN state ELSE NULL END AS state,created_at FROM mst2_verified_object WHERE id=$1 FOR SHARE",
            [source.fact().id.into(), source.fact().git_oid.clone().into()],
        ))
        .await
        .map_err(db_error)?
        .ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::MetadataNotReady,
                "chunk map source has no current verified fact",
            )
        })?;
    let current = mst2_verified_object::Model::from_query_result(&row, "").map_err(|_| {
        integrity("chunk map current source fact is outside its bounded verified profile")
    })?;
    if &current != source.fact() {
        return Err(integrity(
            "chunk map source fact changed during request or verification",
        ));
    }
    Ok(())
}

async fn source_row<C: ConnectionTrait>(
    connection: &C,
    source: &ChunkMapSource,
    schema: &str,
) -> Result<Option<QueryResult>, SnapshotError> {
    connection.query_one_raw(stmt(schema, "SELECT s.fact_id,s.receipt_key,s.receipt_generation,g.state AS generation_state,l.state AS map_state,l.generation AS map_generation,CASE WHEN pg_catalog.octet_length(g.receipt_bytes)<=4096 THEN g.receipt_bytes ELSE NULL END AS generation_receipt_bytes,CASE WHEN pg_catalog.octet_length(s.source_id)=32 THEN s.source_id ELSE NULL END AS source_id,CASE WHEN pg_catalog.octet_length(s.source_bytes)<=2048 THEN s.source_bytes ELSE NULL END AS source_bytes,CASE WHEN pg_catalog.octet_length(s.primary_scope)<=1024 THEN s.primary_scope ELSE NULL END AS primary_scope,CASE WHEN pg_catalog.octet_length(s.map_id)=32 THEN s.map_id ELSE NULL END AS map_id,CASE WHEN pg_catalog.octet_length(s.receipt_digest)=32 THEN s.receipt_digest ELSE NULL END AS receipt_digest,CASE WHEN pg_catalog.octet_length(m.descriptor)=100 THEN m.descriptor ELSE NULL END AS descriptor,m.page_count,CASE WHEN pg_catalog.octet_length(m.pages_root)=32 THEN m.pages_root ELSE NULL END AS pages_root FROM mst2_chunk_map_source s LEFT JOIN mst2_chunk_map m ON m.map_id=s.map_id LEFT JOIN mst2_chunk_receipt_generation g ON g.receipt_key=s.receipt_key AND g.generation=s.receipt_generation AND g.map_id=s.map_id LEFT JOIN mst2_chunk_map_lifetime l ON l.map_id=s.map_id AND l.generation=g.map_generation WHERE s.storage_domain=$1 AND s.git_oid=$2 AND s.object_kind=$3",
        [source.fact().storage_domain.clone().into(), source.fact().git_oid.clone().into(), source.fact().object_kind.clone().into()])).await.map_err(db_error)
}

fn source_id(scope: &[u8], source: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(RECEIPT_DOMAIN);
    hash.update((scope.len() as u32).to_be_bytes());
    hash.update(scope);
    hash.update((source.len() as u32).to_be_bytes());
    hash.update(source);
    hash.finalize().into()
}

fn receipt(
    scope: &[u8],
    source: &ChunkMapSource,
    map: &ChunkMap,
) -> Result<Vec<u8>, SnapshotError> {
    let source = source.canonical_bytes()?;
    if scope.len() > 1024 || source.len() > 2048 {
        return Err(integrity(
            "chunk map source receipt exceeds its fixed profile",
        ));
    }
    let mut bytes = Vec::with_capacity(RECEIPT_DOMAIN.len() + 8 + scope.len() + source.len() + 100);
    bytes.extend_from_slice(RECEIPT_DOMAIN);
    bytes.extend_from_slice(&(scope.len() as u32).to_be_bytes());
    bytes.extend_from_slice(scope);
    bytes.extend_from_slice(&(source.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&source);
    bytes.extend_from_slice(&map.encode());
    Ok(bytes)
}

async fn read_receipt(
    objects: &MegaObjectStorageWrapper,
    key: &ObjectKey,
    expected: &[u8],
) -> Result<(), SnapshotError> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (mut stream, meta) = objects
            .inner
            .get_stream(key)
            .await
            .map_err(|_| integrity("admitted chunk map trusted receipt is unavailable"))?;
        if meta.size != expected.len() as i64 {
            return Err(integrity(
                "chunk map trusted receipt has invalid total size",
            ));
        }
        let mut offset = 0;
        while let Some(part) = stream.next().await {
            let bytes = part.map_err(|_| integrity("chunk map trusted receipt stream failed"))?;
            if bytes.len() > expected.len() - offset
                || &expected[offset..offset + bytes.len()] != bytes.as_ref()
            {
                return Err(integrity(
                    "chunk map trusted receipt bytes disagree with source and descriptor",
                ));
            }
            offset += bytes.len();
        }
        if offset != expected.len() {
            return Err(integrity("chunk map trusted receipt is truncated"));
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "chunk map receipt read timed out",
        )
    })?
}

fn encode_leaves(leaves: &[ChunkLeaf]) -> Result<String, SnapshotError> {
    let values: Result<Vec<serde_json::Value>, SnapshotError> = leaves.iter().map(|l| Ok(json!({"page_index":l.page_index,"payload":hex::encode(l.encode().map_err(|_| integrity("invalid verified leaf"))?)}))).collect();
    serde_json::to_string(&values?).map_err(db_error)
}

fn encode_nodes(nodes: &[ChunkMapNode]) -> Result<String, SnapshotError> {
    serde_json::to_string(&nodes.iter().map(|n| json!({"first_page":n.start,"page_count":n.pages,"digest":hex::encode(n.digest)})).collect::<Vec<_>>()).map_err(db_error)
}

fn quoted(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

/// The static statements use these exact relation tokens. Catalog functions
/// are explicitly qualified; relations never resolve through pg_temp.
fn stmt<const N: usize>(schema: &str, sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, qualify_relations(schema, sql), values)
}

fn qualify_relations(schema: &str, sql: &str) -> String {
    let mut qualified = String::with_capacity(sql.len() + 256);
    let prefix = quoted(schema);
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let start = cursor;
        let byte = bytes[cursor];
        if byte == b'\'' || byte == b'"' {
            cursor += 1;
            while cursor < bytes.len() {
                if bytes[cursor] == byte {
                    cursor += 1;
                    if cursor < bytes.len() && bytes[cursor] == byte {
                        cursor += 1;
                    } else {
                        break;
                    }
                } else {
                    cursor += 1;
                }
            }
        } else if byte.is_ascii_alphanumeric() || byte == b'_' {
            cursor += 1;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
            {
                cursor += 1;
            }
            if [
                "mst2_metadata_storage_scope",
                "mst2_verified_object",
                "mst2_chunk_map",
                "mst2_chunk_map_source",
                "mst2_chunk_map_leaf",
                "mst2_chunk_map_node",
                "mst2_chunk_receipt_generation",
                "mst2_chunk_map_lifetime",
                "mst2_chunk_reader",
                "mst2_chunk_map_gc",
                "mst2_chunk_retention_barrier",
            ]
            .contains(&&sql[start..cursor])
            {
                qualified.push_str(&prefix);
                qualified.push('.');
            }
        } else {
            cursor += sql[cursor..].chars().next().map_or(1, char::len_utf8);
        }
        qualified.push_str(&sql[start..cursor]);
    }
    qualified
}

fn bounded_bytes(row: &QueryResult, name: &str) -> Result<Vec<u8>, SnapshotError> {
    row.try_get::<Option<Vec<u8>>>("", name)
        .map_err(db_error)?
        .ok_or_else(|| {
            integrity("persisted chunk map field is missing or outside its bounded profile")
        })
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn db_error(error: impl std::fmt::Display) -> SnapshotError {
    tracing::warn!(%error, "persisted chunk map storage operation failed");
    SnapshotError::new(
        SnapshotErrorCode::TemporaryUnavailable,
        "persisted chunk map storage operation failed",
    )
}
fn storage_error(error: impl std::fmt::Display) -> SnapshotError {
    tracing::warn!(%error, "trusted chunk map receipt publication failed");
    SnapshotError::new(
        SnapshotErrorCode::TemporaryUnavailable,
        "trusted chunk map receipt publication failed",
    )
}
async fn finish<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    match result {
        Ok(value) => {
            txn.commit().await.map_err(db_error)?;
            Ok(value)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

impl super::Storage {
    pub(crate) async fn chunk_maps(&self) -> Result<&PostgresChunkMapRepository, SnapshotError> {
        use super::base_storage::StorageConnector;
        self.native_chunk_maps
            .get_or_try_init(|| async {
                PostgresChunkMapRepository::new(self.mono_storage().get_connection().clone()).await
            })
            .await
    }
}

#[cfg(test)]
mod sql_tests {
    use super::*;

    #[test]
    fn authority_relations_are_qualified_without_rewriting_catalog_literals() {
        let sql = "SELECT 'mst2_metadata_storage_scope','quoted ''mst2_chunk_map''',\"mst2_chunk_map\" FROM mst2_metadata_storage_scope s JOIN mst2_verified_object f ON s.singleton=1 JOIN mst2_chunk_map m ON true JOIN mst2_chunk_map_leaf l ON true JOIN mst2_chunk_map_node n ON true JOIN mst2_chunk_map_source r ON true WHERE c.relname='mst2_metadata_storage_scope'";
        let qualified = qualify_relations("schema\"name", sql);
        assert!(qualified.contains(&format!(
            "FROM {}.mst2_metadata_storage_scope",
            quoted("schema\"name")
        )));
        for name in [
            "mst2_verified_object",
            "mst2_chunk_map",
            "mst2_chunk_map_leaf",
            "mst2_chunk_map_node",
            "mst2_chunk_map_source",
        ] {
            assert!(qualified.contains(&format!("JOIN {}.{name}", quoted("schema\"name"))));
        }
        assert!(qualified.starts_with(
            "SELECT 'mst2_metadata_storage_scope','quoted ''mst2_chunk_map''',\"mst2_chunk_map\""
        ));
        assert!(qualified.ends_with("WHERE c.relname='mst2_metadata_storage_scope'"));
    }
}
