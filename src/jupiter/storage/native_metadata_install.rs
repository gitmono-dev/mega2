//! Immutable metadata payload CAS and durable native preparation receipts.
//! The native HTTP session authority installs batches and transfers prepare pins.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES, Page, page_id};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    IsolationLevel, QueryFilter, QueryResult, QuerySelect, Statement, TransactionTrait,
};
use serde_json::json;

use super::mst2_retention::{PostgresRetentionRepository, RETENTION_LOCK_KEY};
use crate::{
    callisto::{
        mst2_metadata_payload, mst2_metadata_prepare, mst2_metadata_prepare_page,
        mst2_retention_edge, mst2_retention_node, mst2_retention_root,
    },
    ceres::snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        metadata_install::{MAX_PLAN_BYTES, MetadataInstallIdentity, MetadataInstallPlan},
        pages::PreparedNativeMetadataRetention,
        retention::RetentionRoot,
        retention_dag::{
            MetadataDagCandidate, MetadataDagLimits, MetadataPagePayload, ValidatedMetadataDag,
        },
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataCommitPhase {
    Intent,
    Payload,
    Finalize,
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataInstallError {
    #[error(transparent)]
    Rejected(#[from] SnapshotError),
    #[error("native metadata {phase:?} commit outcome is unknown for operation {operation_id}")]
    CommitUncertain {
        operation_id: String,
        manifest_digest: [u8; 32],
        phase: MetadataCommitPhase,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPrepareIntent {
    prepare_id: String,
    operation_id: String,
    manifest_digest: [u8; 32],
}

impl MetadataPrepareIntent {
    pub fn prepare_id(&self) -> &str {
        &self.prepare_id
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn manifest_digest(&self) -> [u8; 32] {
        self.manifest_digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedMetadataReceipt {
    intent: MetadataPrepareIntent,
    identity: MetadataInstallIdentity,
    metadata_root: [u8; 32],
    payload_bytes: u64,
    root_payload_bytes: u64,
    node_count: usize,
    edge_count: usize,
}

impl PreparedMetadataReceipt {
    pub fn intent(&self) -> &MetadataPrepareIntent {
        &self.intent
    }
    pub fn metadata_root(&self) -> [u8; 32] {
        self.metadata_root
    }
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataPrepareObservation {
    Absent,
    Preparing(MetadataPrepareIntent),
    Committed(PreparedMetadataReceipt),
}

struct StoredPlan {
    record: mst2_metadata_prepare::Model,
    plan: MetadataInstallPlan,
    prepare_pages: Vec<mst2_metadata_prepare_page::Model>,
}

impl StoredPlan {
    fn intent(&self) -> Result<MetadataPrepareIntent, SnapshotError> {
        Ok(MetadataPrepareIntent {
            prepare_id: self.record.prepare_id.clone(),
            operation_id: self.record.operation_id.clone(),
            manifest_digest: self
                .record
                .manifest_digest
                .as_slice()
                .try_into()
                .map_err(|_| integrity("invalid stored metadata manifest digest"))?,
        })
    }
    fn receipt(&self) -> Result<PreparedMetadataReceipt, SnapshotError> {
        Ok(PreparedMetadataReceipt {
            intent: self.intent()?,
            identity: self.plan.identity.clone(),
            metadata_root: self.plan.root,
            payload_bytes: self.plan.total_bytes,
            root_payload_bytes: *self
                .plan
                .pages
                .get(&self.plan.root)
                .ok_or_else(|| integrity("metadata preparation root page is missing"))?,
            node_count: self.plan.pages.len(),
            edge_count: self.plan.edges.len(),
        })
    }
}

#[derive(Clone)]
pub struct PostgresMetadataInstallRepository {
    connection: DatabaseConnection,
    barrier_timeout: Duration,
    storage_scope: PrimaryStorageScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrimaryStorageScope {
    storage_uuid: String,
    database: String,
    database_oid: i64,
    schema: String,
    schema_oid: i64,
    server_address: Option<String>,
    server_port: Option<i32>,
}

impl PostgresMetadataInstallRepository {
    /// The connection must target the deployment's primary PostgreSQL database.
    pub async fn new(connection: DatabaseConnection) -> Result<Self, SnapshotError> {
        let storage_scope = read_storage_scope(&connection).await?;
        Ok(Self {
            connection,
            barrier_timeout: Duration::from_secs(5),
            storage_scope,
        })
    }

    /// These columns are returned by the same query that authorizes a lease,
    /// so a warm cache cannot authorize a stale replica or another schema.
    pub(crate) fn verify_primary_scope_row(&self, row: &QueryResult) -> Result<(), SnapshotError> {
        let actual = PrimaryStorageScope {
            storage_uuid: row
                .try_get::<Option<String>>("", "authority_storage_uuid")
                .map_err(internal)?
                .ok_or_else(|| internal("session authority storage scope is missing"))?,
            database: row.try_get("", "authority_database").map_err(internal)?,
            database_oid: row
                .try_get("", "authority_database_oid")
                .map_err(internal)?,
            schema: row.try_get("", "authority_schema").map_err(internal)?,
            schema_oid: row.try_get("", "authority_schema_oid").map_err(internal)?,
            server_address: row
                .try_get("", "authority_server_address")
                .map_err(internal)?,
            server_port: row.try_get("", "authority_server_port").map_err(internal)?,
        };
        if row
            .try_get::<bool>("", "authority_replica")
            .map_err(internal)?
            || actual != self.storage_scope
        {
            return Err(internal(
                "session authority no longer targets its captured primary storage scope",
            ));
        }
        Ok(())
    }

    pub(crate) async fn verify_primary_connection<C: ConnectionTrait>(
        &self,
        connection: &C,
    ) -> Result<(), SnapshotError> {
        if read_storage_scope(connection).await? != self.storage_scope {
            return Err(internal(
                "session mutation no longer targets its captured primary storage scope",
            ));
        }
        Ok(())
    }

    pub async fn begin_intent(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<MetadataPrepareIntent, MetadataInstallError> {
        validate_operation_id(operation_id)?;
        let plan = prepared.install_plan()?;
        let digest = plan.digest()?;
        let txn = self.transaction().await?;
        let result = async {
            self.barrier(&txn).await?;
            if let Some(stored) = load_plan(&txn, operation_id, &digest).await? {
                return stored.intent();
            }
            let id = uuid::Uuid::new_v4().to_string();
            let identity = &plan.identity;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_prepare (prepare_id, operation_id, manifest_digest, canonical_plan,
                 source_domain, tagged_root_tree_oid, scope, schema_version, metadata_codec, materialization_policy,
                 fs_semantics, access_projection, verification_revision, projection_revision, metadata_root,
                 node_count, edge_count, total_bytes, state)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,'PREPARING')",
                [id.clone().into(), operation_id.into(), digest.to_vec().into(), plan.encode()?.into(),
                 identity.source_domain.clone().into(), identity.tagged_root_tree_oid.clone().into(), identity.scope.clone().into(),
                 (identity.schema_version as i16).into(), (identity.metadata_codec as i16).into(),
                 (identity.materialization_policy as i16).into(), (identity.fs_semantics as i16).into(),
                 (identity.access_projection as i16).into(), identity.verification_revision.into(),
                 (identity.projection_revision as i16).into(), plan.root.to_vec().into(), (plan.pages.len() as i32).into(),
                 (plan.edges.len() as i32).into(), (plan.total_bytes as i64).into()],
            )).await.map_err(internal)?;
            let pages: Vec<_> = plan.pages.iter().map(|(id,size)| json!({"page_id":hex::encode(id),"size":size})).collect();
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_prepare_page (prepare_id,page_id,expected_size)
                 SELECT $1,decode(p.page_id,'hex'),p.size FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer)",
                [id.clone().into(), serde_json::to_string(&pages).map_err(internal)?.into()],
            )).await.map_err(internal)?;
            Ok(MetadataPrepareIntent { prepare_id:id, operation_id:operation_id.into(), manifest_digest:digest })
        }.await;
        commit(
            txn,
            result,
            operation_id,
            digest,
            MetadataCommitPhase::Intent,
        )
        .await
    }

    pub async fn install_page(
        &self,
        intent: &MetadataPrepareIntent,
        payload: &MetadataPagePayload,
    ) -> Result<(), MetadataInstallError> {
        self.install_pages(intent, std::slice::from_ref(payload))
            .await
    }

    /// At most 64 pages (1 MiB of canonical payload) per transaction.
    pub async fn install_pages(
        &self,
        intent: &MetadataPrepareIntent,
        payloads: &[MetadataPagePayload],
    ) -> Result<(), MetadataInstallError> {
        if payloads.is_empty() || payloads.len() > 64 {
            return Err(SnapshotError::new(
                SnapshotErrorCode::LimitExceeded,
                "metadata installation batch must contain 1..=64 pages",
            )
            .into());
        }
        let mut ids = BTreeSet::new();
        for payload in payloads {
            validate_payload(payload)?;
            if !ids.insert(payload.id) {
                return Err(integrity("duplicate page in metadata installation batch").into());
            }
        }
        let txn = self.transaction().await?;
        let result = async {
            self.barrier(&txn).await?;
            let stored = require_plan(&txn, intent).await?;
            for payload in payloads {
                if stored.plan.pages.get(&payload.id) != Some(&payload.size) {
                    return Err(integrity("metadata payload is not a member of this fixed installation"));
                }
            }
            let pages: Vec<_> = payloads.iter().map(|p| json!({
                "page_id":hex::encode(p.id), "size":p.size, "payload":hex::encode(&p.bytes)
            })).collect();
            let encoded = serde_json::to_string(&pages).map_err(internal)?;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload)
                 SELECT decode(p.page_id,'hex'),$1,p.size,decode(p.payload,'hex')
                 FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer,payload text)
                 ON CONFLICT(page_id) DO NOTHING",
                [(stored.plan.identity.metadata_codec as i16).into(),encoded.clone().into()],
            )).await.map_err(internal)?;
            let bad = txn.query_one_raw(statement(
                "SELECT p.page_id FROM jsonb_to_recordset($2::jsonb) AS p(page_id text,size integer,payload text)
                 LEFT JOIN mst2_metadata_payload b ON b.page_id=decode(p.page_id,'hex')
                 WHERE b.page_id IS NULL OR b.metadata_codec<>$1 OR b.byte_size<>p.size
                   OR b.payload<>decode(p.payload,'hex') LIMIT 1",
                [(stored.plan.identity.metadata_codec as i16).into(),encoded.into()],
            )).await.map_err(internal)?;
            if bad.is_some() {
                return Err(integrity("immutable metadata payload identity conflicts with stored bytes"));
            }
            Ok(())
        }.await;
        commit(
            txn,
            result,
            &intent.operation_id,
            intent.manifest_digest,
            MetadataCommitPhase::Payload,
        )
        .await
    }

    pub(crate) async fn verify_receipt_in_txn(
        &self,
        txn: &DatabaseTransaction,
        receipt: &PreparedMetadataReceipt,
        tagged_root_tree_oid: &str,
        scope: &str,
    ) -> Result<(), SnapshotError> {
        self.barrier(txn).await?;
        let identity = &receipt.identity;
        if identity.tagged_root_tree_oid != tagged_root_tree_oid || identity.scope != scope {
            return Err(integrity(
                "metadata receipt belongs to another fixed source",
            ));
        }
        // The private receipt can only be issued after full validation and a
        // definitive commit. Hash the bounded canonical plan in PostgreSQL,
        // bind all redundant identity/summary columns to that receipt, and
        // check its still-protected LIVE root under the retention barrier.
        // This hashes <=2 MiB; it does not transfer or scan pages/edges here.
        let row = txn.query_one_raw(statement(
            "SELECT p.operation_id,p.manifest_digest,
             CASE WHEN octet_length(p.canonical_plan)<=$2 THEN sha256(p.canonical_plan) END AS plan_digest,
             p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,p.metadata_codec,
             p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,
             p.projection_revision,p.metadata_root,p.node_count,p.edge_count,p.total_bytes,
             p.state,p.committed_at IS NOT NULL AS committed,
             n.kind AS root_kind,n.state AS root_state,n.bytes AS root_bytes,
             EXISTS(SELECT 1 FROM mst2_retention_root r WHERE r.node_id=n.node_id
               AND r.root_key='prepare:'||p.prepare_id AND r.root_kind='prepare') AS prepare_covered
             FROM mst2_metadata_prepare p LEFT JOIN mst2_retention_node n
               ON n.node_id='page:sha256:'||encode(p.metadata_root,'hex') WHERE p.prepare_id=$1",
            [receipt.intent.prepare_id.clone().into(), (MAX_PLAN_BYTES as i32).into()],
        )).await.map_err(internal)?.ok_or_else(|| unavailable("metadata preparation is missing"))?;
        if row
            .try_get::<String>("", "operation_id")
            .map_err(internal)?
            != receipt.intent.operation_id
            || row
                .try_get::<Vec<u8>>("", "manifest_digest")
                .map_err(internal)?
                != receipt.intent.manifest_digest
            || row
                .try_get::<Option<Vec<u8>>>("", "plan_digest")
                .map_err(internal)?
                .as_deref()
                != Some(receipt.intent.manifest_digest.as_slice())
            || row
                .try_get::<String>("", "source_domain")
                .map_err(internal)?
                != identity.source_domain
            || row
                .try_get::<String>("", "tagged_root_tree_oid")
                .map_err(internal)?
                != identity.tagged_root_tree_oid
            || row.try_get::<String>("", "scope").map_err(internal)? != identity.scope
            || row.try_get::<i16>("", "schema_version").map_err(internal)?
                != identity.schema_version as i16
            || row.try_get::<i16>("", "metadata_codec").map_err(internal)?
                != identity.metadata_codec as i16
            || row
                .try_get::<i16>("", "materialization_policy")
                .map_err(internal)?
                != identity.materialization_policy as i16
            || row.try_get::<i16>("", "fs_semantics").map_err(internal)?
                != identity.fs_semantics as i16
            || row
                .try_get::<i16>("", "access_projection")
                .map_err(internal)?
                != identity.access_projection as i16
            || row
                .try_get::<i32>("", "verification_revision")
                .map_err(internal)?
                != identity.verification_revision
            || row
                .try_get::<i16>("", "projection_revision")
                .map_err(internal)?
                != identity.projection_revision as i16
            || row
                .try_get::<Vec<u8>>("", "metadata_root")
                .map_err(internal)?
                != receipt.metadata_root
            || row.try_get::<i32>("", "node_count").map_err(internal)? != receipt.node_count as i32
            || row.try_get::<i32>("", "edge_count").map_err(internal)? != receipt.edge_count as i32
            || row.try_get::<i64>("", "total_bytes").map_err(internal)?
                != receipt.payload_bytes as i64
            || row.try_get::<String>("", "state").map_err(internal)? != "COMMITTED"
            || !row.try_get::<bool>("", "committed").map_err(internal)?
        {
            return Err(integrity(
                "metadata preparation differs from its definitive receipt",
            ));
        }
        if row
            .try_get::<Option<String>>("", "root_kind")
            .map_err(internal)?
            .as_deref()
            != Some("page")
            || row
                .try_get::<Option<String>>("", "root_state")
                .map_err(internal)?
                .as_deref()
                != Some("LIVE")
            || row
                .try_get::<Option<i64>>("", "root_bytes")
                .map_err(internal)?
                != Some(receipt.root_payload_bytes as i64)
            || !row
                .try_get::<bool>("", "prepare_covered")
                .map_err(internal)?
        {
            return Err(unavailable(
                "metadata receipt root is not LIVE and protected by its prepare",
            ));
        }
        Ok(())
    }

    pub(crate) async fn restore_session_dag(&self, prepare_id: &str) -> Result<(), SnapshotError> {
        let record = mst2_metadata_prepare::Entity::find_by_id(prepare_id.to_owned())
            .one(&self.connection)
            .await
            .map_err(internal)?
            .ok_or_else(|| unavailable("snapshot preparation is missing"))?;
        let digest = record
            .manifest_digest
            .as_slice()
            .try_into()
            .map_err(|_| integrity("invalid metadata preparation digest"))?;
        let stored = load_plan(&self.connection, &record.operation_id, &digest)
            .await?
            .ok_or_else(|| unavailable("snapshot preparation disappeared"))?;
        if stored.record.state != "COMMITTED" {
            return Err(unavailable("snapshot preparation is not committed"));
        }
        load_installed_dag(&self.connection, &stored).await?;
        let txn = self.transaction().await?;
        let result = async {
            self.barrier(&txn).await?;
            verify_graph(&txn, &stored).await
        }
        .await;
        match result {
            Ok(()) => txn.commit().await.map_err(internal),
            Err(error) => {
                txn.rollback().await.map_err(internal)?;
                Err(error)
            }
        }
    }

    /// Verify all stored bytes outside the graph transaction. Immutable payloads
    /// cannot change; LIVE state and coverage are checked again under the lock.
    pub async fn load_installed_dag(
        &self,
        intent: &MetadataPrepareIntent,
    ) -> Result<ValidatedMetadataDag, SnapshotError> {
        let stored = require_plan(&self.connection, intent).await?;
        load_installed_dag(&self.connection, &stored).await
    }

    pub async fn finalize(
        &self,
        intent: &MetadataPrepareIntent,
    ) -> Result<PreparedMetadataReceipt, MetadataInstallError> {
        let dag = self.load_installed_dag(intent).await?;
        let txn = self.transaction().await?;
        let result = self.finalize_in_txn(&txn, intent, &dag).await;
        commit(
            txn,
            result,
            &intent.operation_id,
            intent.manifest_digest,
            MetadataCommitPhase::Finalize,
        )
        .await
    }

    // This provisional result is private: only finalize's successful outer
    // commit or the recovery barrier can issue a durable receipt to a caller.
    async fn finalize_in_txn(
        &self,
        txn: &DatabaseTransaction,
        intent: &MetadataPrepareIntent,
        dag: &ValidatedMetadataDag,
    ) -> Result<PreparedMetadataReceipt, SnapshotError> {
        self.barrier(txn).await?;
        let stored = require_plan(txn, intent).await?;
        self.finalize_stored_plan_in_txn(txn, intent, dag, stored)
            .await
    }

    async fn finalize_stored_plan_in_txn(
        &self,
        txn: &DatabaseTransaction,
        intent: &MetadataPrepareIntent,
        dag: &ValidatedMetadataDag,
        stored: StoredPlan,
    ) -> Result<PreparedMetadataReceipt, SnapshotError> {
        let expected: BTreeSet<_> = dag
            .payloads()
            .iter()
            .map(|page| (page.id, page.size))
            .collect();
        if dag.root() != stored.plan.root
            || expected
                != stored
                    .plan
                    .pages
                    .iter()
                    .map(|(id, size)| (*id, *size))
                    .collect()
            || dag
                .edges()
                .iter()
                .map(|edge| (edge.parent.clone(), edge.child.clone()))
                .collect::<BTreeSet<_>>()
                != stored
                    .plan
                    .edges
                    .iter()
                    .map(|(parent, child)| (node_id(parent), node_id(child)))
                    .collect()
        {
            return Err(integrity(
                "validated installed DAG differs from durable preparation plan",
            ));
        }
        check_payload_coverage(txn, &stored).await?;
        if stored.record.state == "COMMITTED" {
            verify_graph(txn, &stored).await?;
            return stored.receipt();
        }
        PostgresRetentionRepository::retain_group_in_txn(
            txn,
            dag.nodes(),
            dag.edges(),
            &[RetentionRoot::Prepare(intent.prepare_id.clone())],
        )
        .await?;
        verify_graph(txn, &stored).await?;
        let result = txn
            .execute_raw(statement(
                "UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=now()
             WHERE prepare_id=$1 AND manifest_digest=$2 AND state='PREPARING'",
                [
                    intent.prepare_id.clone().into(),
                    intent.manifest_digest.to_vec().into(),
                ],
            ))
            .await
            .map_err(internal)?;
        if result.rows_affected() != 1 {
            return Err(integrity("metadata prepare state changed during finalize"));
        }
        stored.receipt()
    }

    /// Call with a fresh connection to the same primary after a commit error.
    /// Lock acquisition is the completion barrier; absence before it proves nothing.
    pub async fn inspect_prepare(
        &self,
        fresh_primary: &DatabaseConnection,
        operation_id: &str,
        digest: [u8; 32],
        phase: MetadataCommitPhase,
    ) -> Result<MetadataPrepareObservation, MetadataInstallError> {
        validate_operation_id(operation_id)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| uncertain(operation_id, digest, phase))?;
        if self.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(uncertain(operation_id, digest, phase));
        }
        let result = async {
            match load_plan(&txn, operation_id, &digest).await? {
                None => Ok(MetadataPrepareObservation::Absent),
                Some(stored) if stored.record.state == "COMMITTED" => {
                    // Recovery rechecks the bounded bytes/DAG under the barrier,
                    // so corruption cannot turn a stored state into a receipt.
                    // This may read 64 MiB; normal finalization verifies outside
                    // its short graph transaction instead.
                    load_installed_dag(&txn, &stored).await?;
                    check_payload_coverage(&txn, &stored).await?;
                    verify_graph(&txn, &stored).await?;
                    Ok(MetadataPrepareObservation::Committed(stored.receipt()?))
                }
                Some(stored) => Ok(MetadataPrepareObservation::Preparing(stored.intent()?)),
            }
        }
        .await;
        txn.rollback()
            .await
            .map_err(|_| uncertain(operation_id, digest, phase))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                uncertain(operation_id, digest, phase)
            } else {
                MetadataInstallError::Rejected(error)
            }
        })
    }

    async fn transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        if self.connection.get_database_backend() != DbBackend::Postgres {
            return Err(internal("metadata installation requires PostgreSQL"));
        }
        self.connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(internal)
    }

    async fn barrier(&self, txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
        if txn.get_database_backend() != DbBackend::Postgres {
            return Err(internal("metadata installation requires PostgreSQL"));
        }
        if read_storage_scope(txn).await? != self.storage_scope {
            return Err(internal(
                "metadata recovery connection is outside the captured primary storage scope",
            ));
        }
        let isolation = txn
            .query_one_raw(statement("SHOW transaction_isolation", []))
            .await
            .map_err(internal)?
            .ok_or_else(|| internal("missing transaction isolation"))?
            .try_get_by_index::<String>(0)
            .map_err(internal)?;
        if isolation != "read committed" {
            return Err(internal("metadata installation requires READ COMMITTED"));
        }
        let recovery = txn
            .query_one_raw(statement("SELECT pg_is_in_recovery()", []))
            .await
            .map_err(internal)?
            .ok_or_else(|| internal("missing primary status"))?
            .try_get_by_index::<bool>(0)
            .map_err(internal)?;
        if recovery {
            return Err(internal(
                "metadata installation recovery requires the primary",
            ));
        }
        let timeout = format!("{}ms", self.barrier_timeout.as_millis().clamp(1, 5000));
        txn.execute_raw(statement(
            "SELECT set_config('lock_timeout',$1,true)",
            [timeout.into()],
        ))
        .await
        .map_err(internal)?;
        txn.execute_raw(statement(
            "SELECT pg_advisory_xact_lock($1,hashtext(current_schema()))",
            [RETENTION_LOCK_KEY.into()],
        ))
        .await
        .map_err(internal)?;
        Ok(())
    }
}

async fn read_storage_scope<C: ConnectionTrait>(
    connection: &C,
) -> Result<PrimaryStorageScope, SnapshotError> {
    if connection.get_database_backend() != DbBackend::Postgres {
        return Err(internal("metadata storage scope requires PostgreSQL"));
    }
    let row = connection
        .query_one_raw(statement(
            "SELECT s.storage_uuid, current_database() AS database, d.oid::bigint AS database_oid,
          current_schema() AS schema, n.oid::bigint AS schema_oid,
          inet_server_addr()::text AS server_address, inet_server_port() AS server_port,
          pg_is_in_recovery() AS replica FROM mst2_metadata_storage_scope s
          JOIN pg_catalog.pg_database d ON d.datname=current_database()
          JOIN pg_catalog.pg_namespace n ON n.nspname=current_schema() WHERE s.singleton=1",
            [],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("metadata primary storage scope is missing"))?;
    if row.try_get::<bool>("", "replica").map_err(internal)? {
        return Err(internal("metadata storage scope requires the primary"));
    }
    let storage_uuid: String = row.try_get("", "storage_uuid").map_err(internal)?;
    let id = uuid::Uuid::parse_str(&storage_uuid)
        .map_err(|_| internal("invalid metadata storage UUID"))?;
    if id.is_nil() || id.to_string() != storage_uuid {
        return Err(internal("noncanonical metadata storage UUID"));
    }
    Ok(PrimaryStorageScope {
        storage_uuid,
        database: row.try_get("", "database").map_err(internal)?,
        database_oid: row.try_get("", "database_oid").map_err(internal)?,
        schema: row.try_get("", "schema").map_err(internal)?,
        schema_oid: row.try_get("", "schema_oid").map_err(internal)?,
        server_address: row.try_get("", "server_address").map_err(internal)?,
        server_port: row.try_get("", "server_port").map_err(internal)?,
    })
}

async fn load_plan<C: ConnectionTrait>(
    connection: &C,
    operation_id: &str,
    digest: &[u8; 32],
) -> Result<Option<StoredPlan>, SnapshotError> {
    let Some(record) = mst2_metadata_prepare::Entity::find()
        .filter(mst2_metadata_prepare::Column::OperationId.eq(operation_id))
        .one(connection)
        .await
        .map_err(internal)?
    else {
        return Ok(None);
    };
    if record.manifest_digest.as_slice() != digest {
        return Err(SnapshotError::new(
            SnapshotErrorCode::Conflict,
            "metadata operation ID is bound to a different manifest",
        ));
    }
    let plan = MetadataInstallPlan::decode(&record.canonical_plan, digest)?;
    let identity = &plan.identity;
    if record.source_domain != identity.source_domain
        || record.tagged_root_tree_oid != identity.tagged_root_tree_oid
        || record.scope != identity.scope
        || record.schema_version != identity.schema_version as i16
        || record.metadata_codec != identity.metadata_codec as i16
        || record.materialization_policy != identity.materialization_policy as i16
        || record.fs_semantics != identity.fs_semantics as i16
        || record.access_projection != identity.access_projection as i16
        || record.verification_revision != identity.verification_revision
        || record.projection_revision != identity.projection_revision as i16
        || record.metadata_root.as_slice() != plan.root
        || record.node_count as usize != plan.pages.len()
        || record.edge_count as usize != plan.edges.len()
        || record.total_bytes as u64 != plan.total_bytes
        || !["PREPARING", "COMMITTED"].contains(&record.state.as_str())
        || (record.state == "COMMITTED") != record.committed_at.is_some()
    {
        return Err(integrity(
            "stored metadata preparation fields disagree with their canonical plan",
        ));
    }
    let id = uuid::Uuid::parse_str(&record.prepare_id)
        .map_err(|_| integrity("invalid stored preparation identity"))?;
    if id.is_nil() || id.to_string() != record.prepare_id {
        return Err(integrity("noncanonical stored preparation identity"));
    }
    let rows = mst2_metadata_prepare_page::Entity::find()
        .filter(mst2_metadata_prepare_page::Column::PrepareId.eq(&record.prepare_id))
        .limit((MetadataDagLimits::default().nodes + 1) as u64)
        .all(connection)
        .await
        .map_err(internal)?;
    let mut coverage = BTreeSet::new();
    for row in &rows {
        let page: [u8; 32] = row
            .page_id
            .as_slice()
            .try_into()
            .map_err(|_| integrity("invalid stored preparation page ID"))?;
        coverage.insert((page, row.expected_size as u64));
    }
    if coverage != plan.pages.iter().map(|(id, size)| (*id, *size)).collect() {
        return Err(integrity(
            "stored metadata preparation coverage differs from its canonical plan",
        ));
    }
    Ok(Some(StoredPlan {
        record,
        plan,
        prepare_pages: rows,
    }))
}

async fn require_plan<C: ConnectionTrait>(
    connection: &C,
    intent: &MetadataPrepareIntent,
) -> Result<StoredPlan, SnapshotError> {
    validate_operation_id(&intent.operation_id)?;
    let stored = load_plan(connection, &intent.operation_id, &intent.manifest_digest)
        .await?
        .ok_or_else(|| unavailable("metadata preparation intent is not durable"))?;
    if stored.record.prepare_id != intent.prepare_id {
        return Err(integrity("metadata intent identity mismatch"));
    }
    Ok(stored)
}

async fn load_installed_dag<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<ValidatedMetadataDag, SnapshotError> {
    let rows = mst2_metadata_payload::Entity::find()
        .filter(
            mst2_metadata_payload::Column::PageId
                .is_in(stored.plan.pages.keys().map(|id| id.to_vec())),
        )
        .all(connection)
        .await
        .map_err(internal)?;
    let mut pages = Vec::with_capacity(rows.len());
    for row in rows {
        let id: [u8; 32] = row
            .page_id
            .as_slice()
            .try_into()
            .map_err(|_| integrity("invalid stored page ID"))?;
        if row.metadata_codec != stored.plan.identity.metadata_codec as i16
            || stored.plan.pages.get(&id) != Some(&(row.byte_size as u64))
        {
            return Err(integrity(
                "stored page profile or size disagrees with its manifest",
            ));
        }
        let page = MetadataPagePayload {
            id,
            size: row.byte_size as u64,
            bytes: row.payload,
        };
        validate_payload(&page)?;
        pages.push(page);
    }
    if pages.len() != stored.plan.pages.len() {
        return Err(unavailable(
            "native metadata installation has missing payloads",
        ));
    }
    ValidatedMetadataDag::validate(
        MetadataDagCandidate {
            metadata_codec: stored.plan.identity.metadata_codec,
            root: stored.plan.root,
            pages,
            edges: stored.plan.edges.iter().copied().collect(),
        },
        MetadataDagLimits::default(),
    )
}

async fn check_payload_coverage<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<(), SnapshotError> {
    let missing=connection.query_one_raw(statement(
        "SELECT p.page_id FROM mst2_metadata_prepare_page p LEFT JOIN mst2_metadata_payload b ON b.page_id=p.page_id
         WHERE p.prepare_id=$1 AND (b.page_id IS NULL OR b.byte_size<>p.expected_size OR b.metadata_codec<>$2
           OR octet_length(b.payload)<>p.expected_size) LIMIT 1",
        [stored.record.prepare_id.clone().into(),stored.record.metadata_codec.into()],
    )).await.map_err(internal)?;
    if missing.is_some() {
        return Err(unavailable(
            "durable metadata payload coverage is incomplete",
        ));
    }
    Ok(())
}

async fn verify_graph<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<(), SnapshotError> {
    let sizes: BTreeMap<_, _> = stored
        .plan
        .pages
        .iter()
        .map(|(id, size)| (node_id(id), *size))
        .collect();
    let ids: Vec<_> = sizes.keys().cloned().collect();
    let nodes = mst2_retention_node::Entity::find()
        .filter(mst2_retention_node::Column::NodeId.is_in(ids.clone()))
        .all(connection)
        .await
        .map_err(internal)?;
    if nodes.len() != ids.len() {
        return Err(unavailable("prepared metadata graph has missing nodes"));
    }
    for node in nodes {
        if node.state != "LIVE" || node.kind != "page" {
            return Err(unavailable("prepared metadata graph is not LIVE"));
        }
        if sizes.get(&node.node_id) != Some(&(node.bytes as u64)) {
            return Err(integrity(
                "prepared metadata graph bytes differ from its plan",
            ));
        }
    }
    let edges = mst2_retention_edge::Entity::find()
        .filter(mst2_retention_edge::Column::ParentId.is_in(ids.clone()))
        .limit((MetadataDagLimits::default().edges + 1) as u64)
        .all(connection)
        .await
        .map_err(internal)?;
    let actual: BTreeSet<_> = edges
        .into_iter()
        .map(|edge| (edge.parent_id, edge.child_id))
        .collect();
    let expected = stored
        .plan
        .edges
        .iter()
        .map(|(parent, child)| (node_id(parent), node_id(child)))
        .collect();
    if actual != expected {
        return Err(integrity(
            "prepared metadata retention edges differ from its plan",
        ));
    }
    let roots = mst2_retention_root::Entity::find()
        .filter(
            mst2_retention_root::Column::RootKey
                .eq(format!("prepare:{}", stored.record.prepare_id)),
        )
        .limit((MetadataDagLimits::default().nodes + 1) as u64)
        .all(connection)
        .await
        .map_err(internal)?;
    if roots.is_empty() {
        let transferred = connection
            .query_one_raw(statement(
                "SELECT s.snapshot_id FROM mst2_snapshot_context s
             JOIN mst2_retention_root r ON r.root_key='pin:session:'||s.snapshot_id
               AND r.node_id='page:sha256:'||encode(s.metadata_root,'hex') AND r.root_kind='pin'
             WHERE s.prepare_id=$1 LIMIT 1",
                [stored.record.prepare_id.clone().into()],
            ))
            .await
            .map_err(internal)?;
        if transferred.is_none() {
            return Err(unavailable("metadata protection handoff is incomplete"));
        }
    } else if roots.iter().any(|root| root.root_kind != "prepare")
        || roots
            .into_iter()
            .map(|root| root.node_id)
            .collect::<BTreeSet<_>>()
            != ids.iter().cloned().collect()
    {
        return Err(unavailable("prepared metadata pin coverage is incomplete"));
    }
    let bad=connection.query_one_raw(statement(
        "SELECT n.node_id FROM mst2_retention_node n WHERE n.node_id IN (SELECT jsonb_array_elements_text($1::jsonb))
         AND n.incoming_refs<>(SELECT count(*) FROM mst2_retention_edge e WHERE e.child_id=n.node_id) LIMIT 1",
        [serde_json::to_string(&ids).map_err(internal)?.into()],
    )).await.map_err(internal)?;
    if bad.is_some() {
        return Err(integrity("prepared metadata graph counter audit failed"));
    }
    Ok(())
}

fn validate_payload(payload: &MetadataPagePayload) -> Result<(), SnapshotError> {
    if !(HEADER_LEN..=PAGE_MAX_BYTES).contains(&payload.bytes.len()) {
        return Err(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "metadata page exceeds protocol length bounds",
        ));
    }
    if payload.size != payload.bytes.len() as u64 || page_id(&payload.bytes) != payload.id {
        return Err(SnapshotError::new(
            SnapshotErrorCode::DigestMismatch,
            "metadata payload size or protocol page ID mismatch",
        ));
    }
    Page::decode(&payload.bytes)
        .map_err(|error| integrity(&format!("invalid canonical metadata page: {error}")))?;
    Ok(())
}

fn validate_operation_id(operation: &str) -> Result<(), SnapshotError> {
    if operation.is_empty() || operation.len() > 255 || operation.contains('\0') {
        return Err(SnapshotError::new(
            SnapshotErrorCode::InvalidRequest,
            "metadata operation ID must be 1..=255 UTF8 bytes",
        ));
    }
    Ok(())
}

async fn commit<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
    operation: &str,
    digest: [u8; 32],
    phase: MetadataCommitPhase,
) -> Result<T, MetadataInstallError> {
    match result {
        Ok(value) => {
            txn.commit()
                .await
                .map_err(|_| uncertain(operation, digest, phase))?;
            Ok(value)
        }
        Err(error) => {
            txn.rollback().await.map_err(internal)?;
            Err(error.into())
        }
    }
}
fn uncertain(
    operation: &str,
    digest: [u8; 32],
    phase: MetadataCommitPhase,
) -> MetadataInstallError {
    MetadataInstallError::CommitUncertain {
        operation_id: operation.into(),
        manifest_digest: digest,
        phase,
    }
}
fn node_id(id: &[u8; 32]) -> String {
    format!("page:sha256:{}", hex::encode(id))
}
fn statement<const N: usize>(sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}
fn internal(error: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, error.to_string())
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::ObjectUnavailable, message)
}

// Additive repository only. HTTP sessions continue using the existing installer
// until session anchors and lease adoption bind the generation seal.
#[path = "native_metadata_generations.rs"]
pub mod generations;

#[cfg(test)]
#[path = "native_metadata_install_tests.rs"]
mod tests;
