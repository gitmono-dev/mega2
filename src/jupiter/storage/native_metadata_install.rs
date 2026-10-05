//! Immutable metadata payload CAS and durable native preparation receipts.
//! No runtime, publication, lease or physical collector is enabled here.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES, Page, page_id};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    IsolationLevel, QueryFilter, QuerySelect, Statement, TransactionTrait,
};
use serde_json::json;

use super::{
    mst2_retention::{PostgresRetentionRepository, RETENTION_LOCK_KEY},
    native_metadata_prepare_expiry::{
        DEFAULT_PREPARE_DURATION_MS, PrepareDeadline, clock, load_deadline, load_expiry,
        request_digest,
    },
};
use crate::{
    callisto::{
        mst2_metadata_payload, mst2_metadata_prepare, mst2_metadata_prepare_page,
        mst2_retention_edge, mst2_retention_node, mst2_retention_root,
    },
    ceres::snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        metadata_install::MetadataInstallPlan,
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
        recovery: Box<MetadataPrepareRecovery>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPrepareIntent {
    prepare_id: String,
    operation_id: String,
    manifest_digest: [u8; 32],
    deadline_digest: Option<[u8; 32]>,
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
    pub fn deadline_digest(&self) -> Option<[u8; 32]> {
        self.deadline_digest
    }
    pub fn recovery(&self, phase: MetadataCommitPhase) -> MetadataPrepareRecovery {
        MetadataPrepareRecovery {
            operation_id: self.operation_id.clone(),
            manifest_digest: self.manifest_digest,
            phase,
            binding: PrepareRecoveryBinding::Exact(self.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPrepareRecovery {
    operation_id: String,
    manifest_digest: [u8; 32],
    phase: MetadataCommitPhase,
    binding: PrepareRecoveryBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrepareRecoveryBinding {
    Exact(MetadataPrepareIntent),
    Request([u8; 32]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedMetadataReceipt {
    intent: MetadataPrepareIntent,
    metadata_root: [u8; 32],
    payload_bytes: u64,
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
    DeadlinePassed {
        intent: MetadataPrepareIntent,
        expires_at_ms: i64,
    },
}

pub(super) struct StoredPlan {
    pub(super) record: mst2_metadata_prepare::Model,
    pub(super) plan: MetadataInstallPlan,
    pub(super) deadline: Option<PrepareDeadline>,
}

impl StoredPlan {
    pub(super) fn intent(&self) -> Result<MetadataPrepareIntent, SnapshotError> {
        Ok(MetadataPrepareIntent {
            prepare_id: self.record.prepare_id.clone(),
            operation_id: self.record.operation_id.clone(),
            manifest_digest: self
                .record
                .manifest_digest
                .as_slice()
                .try_into()
                .map_err(|_| integrity("invalid stored metadata manifest digest"))?,
            deadline_digest: self.deadline.as_ref().map(|grant| grant.grant_digest),
        })
    }
    fn receipt(&self) -> Result<PreparedMetadataReceipt, SnapshotError> {
        Ok(PreparedMetadataReceipt {
            intent: self.intent()?,
            metadata_root: self.plan.root,
            payload_bytes: self.plan.total_bytes,
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

    pub async fn begin_intent(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<MetadataPrepareIntent, MetadataInstallError> {
        self.begin_intent_with_duration(operation_id, prepared, DEFAULT_PREPARE_DURATION_MS)
            .await
    }

    pub(crate) async fn begin_intent_with_duration(
        &self,
        operation_id: &str,
        prepared: &PreparedNativeMetadataRetention,
        duration_ms: i64,
    ) -> Result<MetadataPrepareIntent, MetadataInstallError> {
        validate_operation_id(operation_id)?;
        let plan = prepared.install_plan()?;
        let digest = plan.digest()?;
        let expected_request =
            request_digest(self.storage_uuid(), operation_id, &digest, duration_ms)?;
        let txn = self.transaction().await?;
        let result = async {
            self.barrier(&txn).await?;
            lock_prepare_operation(&txn, operation_id).await?;
            if let Some(stored) = load_plan(&txn, operation_id, &digest).await? {
                reject_terminal(&stored)?;
                if stored.deadline.as_ref().map(|grant| grant.request_digest) != Some(expected_request) {
                    return Err(conflict("metadata operation does not bind this preparation duration"));
                }
                require_unelapsed(&txn, &stored).await?;
                return stored.intent();
            }
            let id = uuid::Uuid::new_v4().to_string();
            let identity = &plan.identity;
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_prepare (prepare_id, operation_id, manifest_digest, canonical_plan,
                 source_domain, tagged_root_tree_oid, scope, schema_version, metadata_codec, materialization_policy,
                 fs_semantics, access_projection, verification_revision, projection_revision, metadata_root,
                 node_count, edge_count, total_bytes, state, deadline_managed)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,'PREPARING',true)",
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
            let deadline = PrepareDeadline::new(
                self.storage_uuid(), &id, operation_id, digest, duration_ms, clock(&txn).await?,
            )?;
            deadline.insert(&txn).await?;
            Ok(MetadataPrepareIntent { prepare_id:id, operation_id:operation_id.into(), manifest_digest:digest,
                deadline_digest:Some(deadline.grant_digest) })
        }.await;
        commit(txn, result, |intent| {
            intent.recovery(MetadataCommitPhase::Intent)
        })
        .await
    }

    pub async fn install_page(
        &self,
        intent: &MetadataPrepareIntent,
        payload: &MetadataPagePayload,
    ) -> Result<(), MetadataInstallError> {
        validate_payload(payload)?;
        let txn = self.transaction().await?;
        let result = async {
            self.barrier(&txn).await?;
            lock_prepare_operation(&txn, &intent.operation_id).await?;
            let stored = require_plan(&txn, intent).await?;
            if stored.plan.pages.get(&payload.id) != Some(&payload.size) {
                return Err(integrity(
                    "metadata payload is not a member of this fixed installation",
                ));
            }
            txn.execute_raw(statement(
                "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload)
                 VALUES ($1,$2,$3,$4) ON CONFLICT(page_id) DO NOTHING",
                [
                    payload.id.to_vec().into(),
                    (stored.plan.identity.metadata_codec as i16).into(),
                    (payload.size as i32).into(),
                    payload.bytes.clone().into(),
                ],
            ))
            .await
            .map_err(internal)?;
            let existing = mst2_metadata_payload::Entity::find_by_id(payload.id.to_vec())
                .one(&txn)
                .await
                .map_err(internal)?
                .ok_or_else(|| integrity("installed metadata payload disappeared"))?;
            if existing.payload != payload.bytes
                || existing.byte_size as u64 != payload.size
                || existing.metadata_codec != stored.plan.identity.metadata_codec as i16
            {
                return Err(integrity(
                    "immutable metadata payload identity conflicts with stored bytes",
                ));
            }
            Ok(())
        }
        .await;
        commit(txn, result, |_| {
            intent.recovery(MetadataCommitPhase::Payload)
        })
        .await
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
        commit(txn, result, |_| {
            intent.recovery(MetadataCommitPhase::Finalize)
        })
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
        lock_prepare_operation(txn, &intent.operation_id).await?;
        let stored = require_plan(txn, intent).await?;
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
        recovery: &MetadataPrepareRecovery,
    ) -> Result<MetadataPrepareObservation, MetadataInstallError> {
        let operation_id = &recovery.operation_id;
        let digest = recovery.manifest_digest;
        validate_operation_id(operation_id)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| uncertain(recovery))?;
        if self.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(uncertain(recovery));
        }
        let result = async {
            lock_prepare_operation(&txn, operation_id).await?;
            match load_plan(&txn, operation_id, &digest).await? {
                None => Ok(MetadataPrepareObservation::Absent),
                Some(stored) => {
                    let matches = match &recovery.binding {
                        PrepareRecoveryBinding::Exact(intent) => &stored.intent()? == intent,
                        PrepareRecoveryBinding::Request(digest) => {
                            stored.deadline.as_ref().map(|grant| grant.request_digest)
                                == Some(*digest)
                        }
                    };
                    if !matches {
                        return Err(conflict(
                            "metadata recovery does not bind this fixed preparation grant",
                        ));
                    }
                    reject_terminal(&stored)?;
                    if let Some(grant) = &stored.deadline
                        && clock(&txn).await? >= grant.expires_at_ms
                    {
                        return Ok(MetadataPrepareObservation::DeadlinePassed {
                            intent: stored.intent()?,
                            expires_at_ms: grant.expires_at_ms,
                        });
                    }
                    if stored.record.state == "COMMITTED" {
                        // Recovery rechecks bounded bytes/DAG under the barrier;
                        // corruption cannot turn a stored state into a receipt.
                        load_installed_dag(&txn, &stored).await?;
                        check_payload_coverage(&txn, &stored).await?;
                        verify_graph(&txn, &stored).await?;
                        Ok(MetadataPrepareObservation::Committed(stored.receipt()?))
                    } else {
                        Ok(MetadataPrepareObservation::Preparing(stored.intent()?))
                    }
                }
            }
        }
        .await;
        txn.rollback().await.map_err(|_| uncertain(recovery))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                uncertain(recovery)
            } else {
                MetadataInstallError::Rejected(error)
            }
        })
    }

    /// A process restart may retain the original operation and requested duration
    /// without a server-issued intent. Recovery must still match that fixed request.
    pub fn recovery_request(
        &self,
        operation: &str,
        digest: [u8; 32],
        phase: MetadataCommitPhase,
    ) -> Result<MetadataPrepareRecovery, SnapshotError> {
        self.recovery_request_with_duration(operation, digest, phase, DEFAULT_PREPARE_DURATION_MS)
    }

    pub(crate) fn recovery_request_with_duration(
        &self,
        operation: &str,
        digest: [u8; 32],
        phase: MetadataCommitPhase,
        duration_ms: i64,
    ) -> Result<MetadataPrepareRecovery, SnapshotError> {
        Ok(MetadataPrepareRecovery {
            operation_id: operation.into(),
            manifest_digest: digest,
            phase,
            binding: PrepareRecoveryBinding::Request(request_digest(
                self.storage_uuid(),
                operation,
                &digest,
                duration_ms,
            )?),
        })
    }

    pub(super) fn storage_uuid(&self) -> &str {
        &self.storage_scope.storage_uuid
    }

    pub(super) fn connection(&self) -> &DatabaseConnection {
        &self.connection
    }

    pub(super) async fn transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        if self.connection.get_database_backend() != DbBackend::Postgres {
            return Err(internal("metadata installation requires PostgreSQL"));
        }
        self.connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(internal)
    }

    pub(super) async fn barrier(&self, txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
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

pub(super) async fn load_plan<C: ConnectionTrait>(
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
        || !["PREPARING", "COMMITTED", "CONSUMED", "EXPIRED"].contains(&record.state.as_str())
        || match record.state.as_str() {
            "PREPARING" => record.committed_at.is_some(),
            "COMMITTED" | "CONSUMED" => record.committed_at.is_none(),
            "EXPIRED" => !record.deadline_managed,
            _ => true,
        }
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
    for row in rows {
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
    let deadline = load_deadline(connection, &record, digest).await?;
    let stored = StoredPlan {
        record,
        plan,
        deadline,
    };
    load_expiry(connection, &stored).await?;
    Ok(Some(stored))
}

async fn require_plan<C: ConnectionTrait>(
    connection: &C,
    intent: &MetadataPrepareIntent,
) -> Result<StoredPlan, SnapshotError> {
    validate_operation_id(&intent.operation_id)?;
    let stored = load_plan(connection, &intent.operation_id, &intent.manifest_digest)
        .await?
        .ok_or_else(|| unavailable("metadata preparation intent is not durable"))?;
    if stored.record.prepare_id != intent.prepare_id
        || stored.deadline.as_ref().map(|grant| grant.grant_digest) != intent.deadline_digest
    {
        return Err(integrity("metadata intent identity mismatch"));
    }
    reject_terminal(&stored)?;
    require_unelapsed(connection, &stored).await?;
    Ok(stored)
}

fn reject_terminal(stored: &StoredPlan) -> Result<(), SnapshotError> {
    if ["CONSUMED", "EXPIRED"].contains(&stored.record.state.as_str()) {
        return Err(conflict("metadata preparation is terminal"));
    }
    Ok(())
}

pub(super) async fn require_unelapsed<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<(), SnapshotError> {
    if let Some(grant) = &stored.deadline
        && clock(connection).await? >= grant.expires_at_ms
    {
        return Err(conflict("metadata preparation deadline has passed"));
    }
    Ok(())
}

pub(super) async fn lock_prepare_operation(
    txn: &DatabaseTransaction,
    operation: &str,
) -> Result<(), SnapshotError> {
    txn.query_one_raw(statement(
        "SELECT prepare_id FROM mst2_metadata_prepare WHERE operation_id=$1 FOR UPDATE",
        [operation.into()],
    ))
    .await
    .map_err(internal)?;
    Ok(())
}

pub(super) async fn load_installed_dag<C: ConnectionTrait>(
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

pub(super) async fn check_payload_coverage<C: ConnectionTrait>(
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
    verify_graph_root(
        connection,
        stored,
        &format!("prepare:{}", stored.record.prepare_id),
        "prepare",
    )
    .await
}

pub(super) async fn verify_graph_root<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
    root_key: &str,
    root_kind: &str,
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
        .filter(mst2_retention_root::Column::RootKey.eq(root_key))
        .limit((MetadataDagLimits::default().nodes + 1) as u64)
        .all(connection)
        .await
        .map_err(internal)?;
    if roots.iter().any(|root| root.root_kind != root_kind)
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

pub(super) fn validate_operation_id(operation: &str) -> Result<(), SnapshotError> {
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
    recovery: impl FnOnce(&T) -> MetadataPrepareRecovery,
) -> Result<T, MetadataInstallError> {
    match result {
        Ok(value) => {
            let recovery = recovery(&value);
            txn.commit().await.map_err(|_| uncertain(&recovery))?;
            Ok(value)
        }
        Err(error) => {
            txn.rollback().await.map_err(internal)?;
            Err(error.into())
        }
    }
}
fn uncertain(recovery: &MetadataPrepareRecovery) -> MetadataInstallError {
    MetadataInstallError::CommitUncertain {
        operation_id: recovery.operation_id.clone(),
        manifest_digest: recovery.manifest_digest,
        phase: recovery.phase,
        recovery: Box::new(recovery.clone()),
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
fn conflict(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Conflict, message)
}

#[cfg(test)]
#[path = "native_metadata_install_tests.rs"]
pub(super) mod tests;
