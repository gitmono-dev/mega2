//! Private metadata-only handoff. No HTTP, authorization or Git content
//! retention adapter constructs these inputs in production.

use mst2_codec::descriptor::ServingDescriptor;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, Statement,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    mst2_retention::PostgresRetentionRepository,
    native_metadata_install::{
        PostgresMetadataInstallRepository, PreparedMetadataReceipt, StoredPlan,
        check_payload_coverage, load_installed_dag, load_plan, verify_graph_root,
    },
};
use crate::ceres::snapshot::{
    error::{SnapshotError, SnapshotErrorCode},
    metadata_install::MetadataInstallPlan,
    retention::RetentionRoot,
    retention_dag::ValidatedMetadataDag,
};

// These opaque inputs deliberately have no production constructors until the
// adopted shared identity factory and actual policy/source adapters exist.
#[derive(Clone)]
pub(crate) struct MetadataOnlyBinding {
    canonical_descriptor: Vec<u8>,
    tagged_root_commit_oid: String,
    publication_binding: [u8; 32],
}

#[derive(Clone)]
pub(crate) struct MetadataAccess {
    subject_id: [u8; 32],
    generation: i64,
    scope: String,
}

pub(crate) struct VerifiedMetadataHandoff {
    binding: MetadataOnlyBinding,
    snapshot_id: [u8; 32],
    prepare_id: String,
    install_operation: String,
    install_digest: [u8; 32],
    storage_uuid: String,
    plan: MetadataInstallPlan,
    dag: ValidatedMetadataDag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseRequest {
    storage_uuid: String,
    prepare_id: String,
    install_operation: String,
    install_digest: [u8; 32],
    snapshot_id: [u8; 32],
    canonical_descriptor: Vec<u8>,
    tagged_root_commit_oid: String,
    publication_binding: [u8; 32],
    subject_id: [u8; 32],
    scope: String,
    generation: i64,
    duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetadataLeaseReceipt {
    operation_id: String,
    operation_digest: [u8; 32],
    prepare_id: String,
    install_digest: [u8; 32],
    snapshot_id: [u8; 32],
    lease_id: String,
    expires_at_ms: i64,
    version: i64,
    request: LeaseRequest,
}

impl MetadataLeaseReceipt {
    pub(crate) fn lease_id(&self) -> &str {
        &self.lease_id
    }
    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub(crate) fn operation_digest(&self) -> [u8; 32] {
        self.operation_digest
    }
    pub(crate) fn snapshot_id(&self) -> [u8; 32] {
        self.snapshot_id
    }
    pub(crate) fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MetadataLeaseError {
    #[error(transparent)]
    Rejected(#[from] SnapshotError),
    #[error("metadata lease commit outcome is unknown for operation {operation_id}")]
    CommitUncertain {
        operation_id: String,
        operation_digest: [u8; 32],
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MetadataLeaseObservation {
    Absent,
    Committed {
        event: Box<MetadataLeaseReceipt>,
        state: String,
        expires_at_ms: i64,
        version: i64,
    },
}

#[derive(Clone)]
pub(crate) struct PostgresMetadataLeaseRepository {
    install: PostgresMetadataInstallRepository,
}

impl PostgresMetadataLeaseRepository {
    pub(crate) async fn new(connection: DatabaseConnection) -> Result<Self, SnapshotError> {
        Ok(Self {
            install: PostgresMetadataInstallRepository::new(connection).await?,
        })
    }

    /// Verifies immutable bytes outside the short graph transaction. The
    /// opaque binding still grants only metadata retention, never read access.
    pub(crate) async fn verify_handoff(
        &self,
        binding: MetadataOnlyBinding,
        receipt: &PreparedMetadataReceipt,
    ) -> Result<VerifiedMetadataHandoff, SnapshotError> {
        let intent = receipt.intent();
        let stored = load_plan(
            self.install.connection(),
            intent.operation_id(),
            &intent.manifest_digest(),
        )
        .await?
        .ok_or_else(|| unavailable("metadata preparation is absent"))?;
        if stored.record.prepare_id != intent.prepare_id()
            || stored.record.state == "PREPARING"
            || stored.plan.root != receipt.metadata_root()
            || stored.plan.total_bytes != receipt.payload_bytes()
        {
            return Err(integrity(
                "metadata handoff does not bind its durable preparation",
            ));
        }
        let descriptor = checked_descriptor(&binding, &stored.plan)?;
        let snapshot_id = descriptor.snapshot_id().map_err(internal)?;
        let dag = load_installed_dag(self.install.connection(), &stored).await?;
        Ok(VerifiedMetadataHandoff {
            binding,
            snapshot_id,
            prepare_id: stored.record.prepare_id,
            install_operation: intent.operation_id().into(),
            install_digest: intent.manifest_digest(),
            storage_uuid: self.install.storage_uuid().into(),
            plan: stored.plan,
            dag,
        })
    }

    pub(crate) async fn consume(
        &self,
        operation: &str,
        handoff: &VerifiedMetadataHandoff,
        access: &MetadataAccess,
        duration_ms: i64,
    ) -> Result<MetadataLeaseReceipt, MetadataLeaseError> {
        let digest = operation_digest(operation, handoff, access, duration_ms)?;
        let stored = load_plan(
            self.install.connection(),
            &handoff.install_operation,
            &handoff.install_digest,
        )
        .await?
        .ok_or_else(|| unavailable("metadata handoff preparation is absent"))?;
        if stored.record.prepare_id != handoff.prepare_id || stored.plan != handoff.plan {
            return Err(integrity("metadata handoff changed before consumption").into());
        }
        // Revalidate the current bounded bytes for each mutation attempt;
        // an old opaque token is not a cache of current storage integrity.
        load_installed_dag(self.install.connection(), &stored).await?;
        let txn = self.install.transaction().await?;
        let result = self
            .consume_in_txn(&txn, operation, digest, handoff, access, duration_ms)
            .await;
        match result {
            Ok(receipt) => {
                txn.commit()
                    .await
                    .map_err(|_| uncertain(operation, digest))?;
                Ok(receipt)
            }
            Err(error) => {
                txn.rollback()
                    .await
                    .map_err(|_| uncertain(operation, digest))?;
                Err(error.into())
            }
        }
    }

    // Provisional values stay private until the wrapper confirms outer COMMIT.
    async fn consume_in_txn(
        &self,
        txn: &DatabaseTransaction,
        operation: &str,
        digest: [u8; 32],
        handoff: &VerifiedMetadataHandoff,
        access: &MetadataAccess,
        duration_ms: i64,
    ) -> Result<MetadataLeaseReceipt, SnapshotError> {
        self.install.barrier(txn).await?;
        if handoff.storage_uuid != self.install.storage_uuid()
            || operation_digest(operation, handoff, access, duration_ms)? != digest
        {
            return Err(integrity("handoff belongs to another storage or operation"));
        }
        txn.query_one_raw(statement(
            "SELECT prepare_id FROM mst2_metadata_prepare WHERE prepare_id=$1 FOR UPDATE",
            [handoff.prepare_id.clone().into()],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| unavailable("handoff preparation is absent"))?;
        let stored = load_plan(txn, &handoff.install_operation, &handoff.install_digest)
            .await?
            .ok_or_else(|| unavailable("handoff plan is absent"))?;
        if stored.record.prepare_id != handoff.prepare_id || stored.plan != handoff.plan {
            return Err(integrity("durable handoff plan changed"));
        }
        require_access(txn, access).await?;
        if let Some(receipt) = load_event(txn, operation, digest).await? {
            if receipt.prepare_id != handoff.prepare_id
                || receipt.snapshot_id != handoff.snapshot_id
            {
                return Err(integrity("lease replay is not bound to this handoff"));
            }
            observe_committed(txn, &stored, &receipt).await?;
            return Ok(receipt);
        }
        if stored.record.state != "COMMITTED" {
            return Err(conflict(
                "preparation is not an unconsumed committed handoff",
            ));
        }
        check_payload_coverage(txn, &stored).await?;
        verify_graph_root(
            txn,
            &stored,
            &format!("prepare:{}", handoff.prepare_id),
            "prepare",
        )
        .await?;
        let descriptor = checked_descriptor(&handoff.binding, &stored.plan)?;
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_catalog(snapshot_id,canonical_descriptor,instance_uuid,namespace_view_id,
             tagged_root_commit_oid,tagged_root_tree_oid,scope,metadata_root,plan_digest,prepare_id)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(snapshot_id) DO NOTHING",
            [handoff.snapshot_id.to_vec().into(),handoff.binding.canonical_descriptor.clone().into(),
             uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string().into(),descriptor.namespace_view_id.to_vec().into(),
             handoff.binding.tagged_root_commit_oid.clone().into(),stored.plan.identity.tagged_root_tree_oid.clone().into(),
             descriptor.scope.clone().into(),descriptor.metadata_root.to_vec().into(),handoff.install_digest.to_vec().into(),
             handoff.prepare_id.clone().into()],
        )).await.map_err(internal)?;
        let row = txn.query_one_raw(statement(
            "SELECT canonical_descriptor,instance_uuid,namespace_view_id,tagged_root_commit_oid,tagged_root_tree_oid,
             scope,metadata_root,plan_digest FROM mst2_metadata_catalog WHERE snapshot_id=$1 FOR SHARE",
            [handoff.snapshot_id.to_vec().into()],
        )).await.map_err(internal)?.ok_or_else(|| integrity("catalog insertion disappeared"))?;
        if row
            .try_get::<Vec<u8>>("", "canonical_descriptor")
            .map_err(internal)?
            != handoff.binding.canonical_descriptor
            || row
                .try_get::<String>("", "instance_uuid")
                .map_err(internal)?
                != uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string()
            || row
                .try_get::<Vec<u8>>("", "namespace_view_id")
                .map_err(internal)?
                != descriptor.namespace_view_id
            || row
                .try_get::<String>("", "tagged_root_commit_oid")
                .map_err(internal)?
                != handoff.binding.tagged_root_commit_oid
            || row
                .try_get::<String>("", "tagged_root_tree_oid")
                .map_err(internal)?
                != stored.plan.identity.tagged_root_tree_oid
            || row.try_get::<String>("", "scope").map_err(internal)? != descriptor.scope
            || row
                .try_get::<Vec<u8>>("", "metadata_root")
                .map_err(internal)?
                != descriptor.metadata_root
            || row
                .try_get::<Vec<u8>>("", "plan_digest")
                .map_err(internal)?
                != handoff.install_digest
        {
            return Err(conflict(
                "snapshot catalog is bound to different immutable metadata or source",
            ));
        }
        // This clock is deliberately sampled after the barrier and row locks.
        let expires_at_ms = clock_after_lock(txn)
            .await?
            .checked_add(duration_ms)
            .ok_or_else(|| invalid("lease deadline overflow"))?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_lease(lease_id,snapshot_id,subject_id,policy_generation,publication_binding,
             state,expires_at_ms,version) VALUES($1,$2,$3,$4,$5,'ACTIVE',$6,1)",
            [lease_id.clone().into(),handoff.snapshot_id.to_vec().into(),access.subject_id.to_vec().into(),
             access.generation.into(),handoff.binding.publication_binding.to_vec().into(),expires_at_ms.into()],
        )).await.map_err(internal)?;
        PostgresRetentionRepository::retain_group_in_txn(
            txn,
            handoff.dag.nodes(),
            handoff.dag.edges(),
            &[RetentionRoot::Lease(lease_id.clone())],
        )
        .await?;
        let receipt = MetadataLeaseReceipt {
            operation_id: operation.into(),
            operation_digest: digest,
            prepare_id: handoff.prepare_id.clone(),
            install_digest: handoff.install_digest,
            snapshot_id: handoff.snapshot_id,
            lease_id: lease_id.clone(),
            expires_at_ms,
            version: 1,
            request: lease_request(handoff, access, duration_ms),
        };
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_prepare_consumption(prepare_id,operation_id,operation_digest,install_digest,snapshot_id,lease_id)
             VALUES($1,$2,$3,$4,$5,$6)",
            [handoff.prepare_id.clone().into(),operation.into(),digest.to_vec().into(),handoff.install_digest.to_vec().into(),
             handoff.snapshot_id.to_vec().into(),lease_id.clone().into()],
        )).await.map_err(internal)?;
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_lease_operation(operation_id,operation_digest,phase,lease_id,snapshot_id,receipt)
             VALUES($1,$2,'CREATE',$3,$4,$5::jsonb)",
            [operation.into(),digest.to_vec().into(),lease_id.clone().into(),handoff.snapshot_id.to_vec().into(),
             serde_json::to_string(&receipt).map_err(internal)?.into()],
        )).await.map_err(internal)?;
        let changed = txn.execute_raw(statement(
            "UPDATE mst2_metadata_prepare SET state='CONSUMED' WHERE prepare_id=$1 AND state='COMMITTED'",
            [handoff.prepare_id.clone().into()],
        )).await.map_err(internal)?;
        if changed.rows_affected() != 1 {
            return Err(integrity("preparation changed during handoff"));
        }
        PostgresRetentionRepository::release_root_in_txn(
            txn,
            &RetentionRoot::Prepare(handoff.prepare_id.clone()),
        )
        .await?;
        verify_graph_root(txn, &stored, &format!("lease:{lease_id}"), "lease").await?;
        observe_committed(txn, &stored, &receipt).await?;
        Ok(receipt)
    }

    /// Recovery evidence describes the original binding and current tombstone.
    /// It never says that a past committed lease still authorizes a read.
    pub(crate) async fn inspect_operation(
        &self,
        fresh_primary: &DatabaseConnection,
        operation: &str,
        digest: [u8; 32],
    ) -> Result<MetadataLeaseObservation, MetadataLeaseError> {
        let txn = fresh_primary
            .begin_with_config(Some(sea_orm::IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| uncertain(operation, digest))?;
        if self.install.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(uncertain(operation, digest));
        }
        let result = async {
            let Some(receipt) = load_event(&txn, operation, digest).await? else {
                return Ok(MetadataLeaseObservation::Absent);
            };
            if receipt.request.storage_uuid != self.install.storage_uuid() {
                return Err(integrity("lease receipt is bound to another storage UUID"));
            }
            let row = txn
                .query_one_raw(statement(
                    "SELECT operation_id FROM mst2_metadata_prepare WHERE prepare_id=$1",
                    [receipt.prepare_id.clone().into()],
                ))
                .await
                .map_err(internal)?
                .ok_or_else(|| integrity("consumed preparation is missing"))?;
            let install_operation: String = row.try_get("", "operation_id").map_err(internal)?;
            let stored = load_plan(&txn, &install_operation, &receipt.install_digest)
                .await?
                .ok_or_else(|| integrity("consumed metadata plan is missing"))?;
            load_installed_dag(&txn, &stored).await?;
            let (state, expires_at_ms, version) =
                observe_committed(&txn, &stored, &receipt).await?;
            Ok(MetadataLeaseObservation::Committed {
                event: Box::new(receipt),
                state,
                expires_at_ms,
                version,
            })
        }
        .await;
        txn.rollback()
            .await
            .map_err(|_| uncertain(operation, digest))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                uncertain(operation, digest)
            } else {
                error.into()
            }
        })
    }
}

async fn require_access(
    txn: &DatabaseTransaction,
    access: &MetadataAccess,
) -> Result<(), SnapshotError> {
    let row = txn.query_one_raw(statement(
        "SELECT generation,scope,enabled FROM mst2_metadata_access_generation WHERE subject_id=$1 FOR SHARE",
        [access.subject_id.to_vec().into()],
    )).await.map_err(internal)?.ok_or_else(|| SnapshotError::new(SnapshotErrorCode::ScopeForbidden,"metadata access is not enabled"))?;
    if !row.try_get::<bool>("", "enabled").map_err(internal)?
        || row.try_get::<i64>("", "generation").map_err(internal)? != access.generation
        || row.try_get::<String>("", "scope").map_err(internal)? != access.scope
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ScopeForbidden,
            "metadata access generation or scope changed",
        ));
    }
    Ok(())
}

async fn load_event<C: ConnectionTrait>(
    connection: &C,
    operation: &str,
    digest: [u8; 32],
) -> Result<Option<MetadataLeaseReceipt>, SnapshotError> {
    let Some(row) = connection.query_one_raw(statement(
        "SELECT operation_digest,phase,lease_id,snapshot_id,receipt::text AS receipt FROM mst2_metadata_lease_operation WHERE operation_id=$1",
        [operation.into()],
    )).await.map_err(internal)? else { return Ok(None); };
    if row
        .try_get::<Vec<u8>>("", "operation_digest")
        .map_err(internal)?
        != digest
    {
        return Err(conflict(
            "lease operation ID is bound to a different request",
        ));
    }
    let receipt: MetadataLeaseReceipt =
        serde_json::from_str(&row.try_get::<String>("", "receipt").map_err(internal)?)
            .map_err(|_| integrity("invalid immutable lease receipt"))?;
    if receipt.operation_id != operation
        || receipt.operation_digest != digest
        || receipt.version != 1
        || receipt.expires_at_ms <= 0
        || row.try_get::<String>("", "phase").map_err(internal)? != "CREATE"
        || row.try_get::<String>("", "lease_id").map_err(internal)? != receipt.lease_id
        || row
            .try_get::<Vec<u8>>("", "snapshot_id")
            .map_err(internal)?
            != receipt.snapshot_id
    {
        return Err(integrity("immutable lease receipt binding mismatch"));
    }
    if request_digest(operation, &receipt.request)? != digest
        || receipt.prepare_id != receipt.request.prepare_id
        || receipt.install_digest != receipt.request.install_digest
        || receipt.snapshot_id != receipt.request.snapshot_id
    {
        return Err(integrity(
            "immutable lease event disagrees with its canonical request",
        ));
    }
    let lease = uuid::Uuid::parse_str(&receipt.lease_id)
        .map_err(|_| integrity("invalid lease identity"))?;
    if lease.is_nil() || lease.to_string() != receipt.lease_id {
        return Err(integrity("noncanonical lease identity"));
    }
    Ok(Some(receipt))
}

async fn observe_committed<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
    receipt: &MetadataLeaseReceipt,
) -> Result<(String, i64, i64), SnapshotError> {
    let row = connection.query_one_raw(statement(
        "SELECT c.operation_id,c.operation_digest,c.install_digest,c.snapshot_id,c.lease_id,l.state,l.expires_at_ms,l.version,
         l.snapshot_id AS lease_snapshot,l.subject_id,l.policy_generation,l.publication_binding FROM mst2_metadata_prepare_consumption c
         JOIN mst2_metadata_lease l ON l.lease_id=c.lease_id WHERE c.prepare_id=$1",
        [receipt.prepare_id.clone().into()],
    )).await.map_err(internal)?.ok_or_else(|| integrity("durable consumption binding is missing"))?;
    // The freshly loaded record sees CONSUMED on recovery. Within the create
    // transaction StoredPlan still carries the pre-update COMMITTED record.
    let state_row = connection
        .query_one_raw(statement(
            "SELECT state FROM mst2_metadata_prepare WHERE prepare_id=$1",
            [receipt.prepare_id.clone().into()],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| integrity("consumed preparation is missing"))?;
    if state_row.try_get::<String>("", "state").map_err(internal)? != "CONSUMED"
        || stored.record.prepare_id != receipt.prepare_id
        || row
            .try_get::<String>("", "operation_id")
            .map_err(internal)?
            != receipt.operation_id
        || row
            .try_get::<Vec<u8>>("", "operation_digest")
            .map_err(internal)?
            != receipt.operation_digest
        || row
            .try_get::<Vec<u8>>("", "install_digest")
            .map_err(internal)?
            != receipt.install_digest
        || row
            .try_get::<Vec<u8>>("", "snapshot_id")
            .map_err(internal)?
            != receipt.snapshot_id
        || row
            .try_get::<Vec<u8>>("", "lease_snapshot")
            .map_err(internal)?
            != receipt.snapshot_id
        || row.try_get::<String>("", "lease_id").map_err(internal)? != receipt.lease_id
        || row.try_get::<Vec<u8>>("", "subject_id").map_err(internal)? != receipt.request.subject_id
        || row
            .try_get::<i64>("", "policy_generation")
            .map_err(internal)?
            != receipt.request.generation
        || row
            .try_get::<Vec<u8>>("", "publication_binding")
            .map_err(internal)?
            != receipt.request.publication_binding
    {
        return Err(integrity(
            "durable consumption fields disagree with their receipt",
        ));
    }
    let binding = MetadataOnlyBinding {
        canonical_descriptor: receipt.request.canonical_descriptor.clone(),
        tagged_root_commit_oid: receipt.request.tagged_root_commit_oid.clone(),
        publication_binding: receipt.request.publication_binding,
    };
    let descriptor = checked_descriptor(&binding, &stored.plan)?;
    if descriptor.snapshot_id().map_err(internal)? != receipt.snapshot_id
        || stored.plan.digest()? != receipt.install_digest
    {
        return Err(integrity(
            "lease event does not bind the stored metadata plan and SID",
        ));
    }
    let catalog = connection.query_one_raw(statement(
        "SELECT canonical_descriptor,instance_uuid,namespace_view_id,tagged_root_commit_oid,tagged_root_tree_oid,
         scope,metadata_root,plan_digest FROM mst2_metadata_catalog WHERE snapshot_id=$1",
        [receipt.snapshot_id.to_vec().into()],
    )).await.map_err(internal)?.ok_or_else(|| integrity("lease catalog is missing"))?;
    if catalog
        .try_get::<Vec<u8>>("", "canonical_descriptor")
        .map_err(internal)?
        != binding.canonical_descriptor
        || catalog
            .try_get::<String>("", "instance_uuid")
            .map_err(internal)?
            != uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string()
        || catalog
            .try_get::<Vec<u8>>("", "namespace_view_id")
            .map_err(internal)?
            != descriptor.namespace_view_id
        || catalog
            .try_get::<String>("", "tagged_root_commit_oid")
            .map_err(internal)?
            != binding.tagged_root_commit_oid
        || catalog
            .try_get::<String>("", "tagged_root_tree_oid")
            .map_err(internal)?
            != stored.plan.identity.tagged_root_tree_oid
        || catalog.try_get::<String>("", "scope").map_err(internal)? != descriptor.scope
        || catalog
            .try_get::<Vec<u8>>("", "metadata_root")
            .map_err(internal)?
            != descriptor.metadata_root
        || catalog
            .try_get::<Vec<u8>>("", "plan_digest")
            .map_err(internal)?
            != receipt.install_digest
    {
        return Err(integrity(
            "immutable catalog disagrees with its committed lease event",
        ));
    }
    check_payload_coverage(connection, stored).await?;
    if connection
        .query_one_raw(statement(
            "SELECT node_id FROM mst2_retention_root WHERE root_key=$1 LIMIT 1",
            [format!("prepare:{}", receipt.prepare_id).into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity("consumed preparation still has a prepare pin"));
    }
    let state: String = row.try_get("", "state").map_err(internal)?;
    let expires: i64 = row.try_get("", "expires_at_ms").map_err(internal)?;
    let version: i64 = row.try_get("", "version").map_err(internal)?;
    if expires < receipt.expires_at_ms || version < receipt.version {
        return Err(integrity("lease state regressed from its immutable event"));
    }
    if state == "ACTIVE" {
        verify_graph_root(
            connection,
            stored,
            &format!("lease:{}", receipt.lease_id),
            "lease",
        )
        .await?;
    } else if !["RELEASED", "EXPIRED"].contains(&state.as_str()) {
        return Err(integrity("unsupported durable lease state"));
    } else if connection
        .query_one_raw(statement(
            "SELECT node_id FROM mst2_retention_root WHERE root_key=$1 LIMIT 1",
            [format!("lease:{}", receipt.lease_id).into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity(
            "terminal lease still has an active retention root",
        ));
    }
    Ok((state, expires, version))
}

fn checked_descriptor(
    binding: &MetadataOnlyBinding,
    plan: &MetadataInstallPlan,
) -> Result<ServingDescriptor, SnapshotError> {
    let descriptor = ServingDescriptor::decode(&binding.canonical_descriptor).map_err(internal)?;
    crate::ceres::snapshot::view::validate_scope_relative_path(&descriptor.scope)?;
    if descriptor.encode().map_err(internal)? != binding.canonical_descriptor
        || descriptor.scope != plan.identity.scope
        || descriptor.metadata_root != plan.root
        || descriptor.schema_version() != plan.identity.schema_version
        || descriptor.metadata_codec() != plan.identity.metadata_codec
        || descriptor.materialization_policy() != plan.identity.materialization_policy
        || descriptor.fs_semantics() != plan.identity.fs_semantics
        || descriptor.access_projection() != plan.identity.access_projection
        || uuid::Uuid::from_bytes(descriptor.instance_uuid).is_nil()
    {
        return Err(integrity("descriptor and fixed metadata plan disagree"));
    }
    let (algorithm, oid) = binding
        .tagged_root_commit_oid
        .split_once(':')
        .ok_or_else(|| integrity("fixed commit must use a tagged object identity"))?;
    let length = match algorithm {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(integrity("unsupported fixed object format")),
    };
    if oid.len() != length
        || !oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !plan
            .identity
            .tagged_root_tree_oid
            .starts_with(&format!("{algorithm}:"))
    {
        return Err(integrity("fixed commit and tree object formats disagree"));
    }
    Ok(descriptor)
}

fn operation_digest(
    operation: &str,
    handoff: &VerifiedMetadataHandoff,
    access: &MetadataAccess,
    duration_ms: i64,
) -> Result<[u8; 32], SnapshotError> {
    if access.scope != handoff.plan.identity.scope {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ScopeForbidden,
            "access does not cover this fixed scope",
        ));
    }
    request_digest(operation, &lease_request(handoff, access, duration_ms))
}

fn lease_request(
    handoff: &VerifiedMetadataHandoff,
    access: &MetadataAccess,
    duration_ms: i64,
) -> LeaseRequest {
    LeaseRequest {
        storage_uuid: handoff.storage_uuid.clone(),
        prepare_id: handoff.prepare_id.clone(),
        install_operation: handoff.install_operation.clone(),
        install_digest: handoff.install_digest,
        snapshot_id: handoff.snapshot_id,
        canonical_descriptor: handoff.binding.canonical_descriptor.clone(),
        tagged_root_commit_oid: handoff.binding.tagged_root_commit_oid.clone(),
        publication_binding: handoff.binding.publication_binding,
        subject_id: access.subject_id,
        scope: access.scope.clone(),
        generation: access.generation,
        duration_ms,
    }
}

fn request_digest(operation: &str, request: &LeaseRequest) -> Result<[u8; 32], SnapshotError> {
    if operation.is_empty() || operation.len() > 255 || operation.contains('\0') {
        return Err(invalid("lease operation ID must be 1..=255 UTF8 bytes"));
    }
    if !(1..=3_600_000).contains(&request.duration_ms)
        || request.generation <= 0
        || request.canonical_descriptor.len() > 4194
        || request.scope.len() > 4096
        || request.install_operation.is_empty()
        || request.install_operation.len() > 255
    {
        return Err(invalid("invalid bounded metadata lease request"));
    }
    crate::ceres::snapshot::view::validate_scope_relative_path(&request.scope)?;
    let mut digest = Sha256::new();
    digest.update(b"mega.mst2.metadata-lease-consume.v1\0");
    for bytes in [
        operation.as_bytes(),
        request.storage_uuid.as_bytes(),
        request.prepare_id.as_bytes(),
        request.install_operation.as_bytes(),
        &request.install_digest,
        &request.snapshot_id,
        &request.canonical_descriptor,
        request.tagged_root_commit_oid.as_bytes(),
        &request.publication_binding,
        &request.subject_id,
        request.scope.as_bytes(),
        &request.generation.to_be_bytes(),
        &request.duration_ms.to_be_bytes(),
    ] {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Ok(digest.finalize().into())
}

async fn clock_after_lock(txn: &DatabaseTransaction) -> Result<i64, SnapshotError> {
    txn.query_one_raw(statement(
        "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
        [],
    ))
    .await
    .map_err(internal)?
    .ok_or_else(|| internal("database clock is missing"))?
    .try_get_by_index(0)
    .map_err(internal)
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
fn invalid(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::InvalidRequest, message)
}
fn conflict(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Conflict, message)
}
fn uncertain(operation: &str, digest: [u8; 32]) -> MetadataLeaseError {
    MetadataLeaseError::CommitUncertain {
        operation_id: operation.into(),
        operation_digest: digest,
    }
}

#[cfg(test)]
#[path = "native_metadata_lease_tests.rs"]
mod tests;
