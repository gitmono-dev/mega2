//! Durable native HTTP sessions. Deployment bearer authentication is enforced
//! by the router; leases retain data and do not grant per-principal permission.

use std::{collections::HashMap, sync::Arc};

use mst2_codec::descriptor::ServingDescriptor;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, EntityTrait,
    IsolationLevel, QueryResult, Statement, TransactionTrait,
};
use tokio::sync::{Mutex, OnceCell};

use super::{
    mono_storage::MonoStorage,
    mst2_retention::{PostgresRetentionRepository, RETENTION_LOCK_KEY},
    native_metadata_install::{
        MetadataInstallError, PostgresMetadataInstallRepository, PreparedMetadataReceipt,
    },
    native_publication_storage::{NativePublicationHead, decode_native_observation},
};
use crate::{
    callisto::mst2_snapshot_context,
    ceres::snapshot::{
        descriptor::BuiltDescriptor,
        error::{SnapshotError, SnapshotErrorCode},
        pages::PreparedNativeMetadataRetention,
        retention::RetentionRoot,
        runtime::{LeaseRenewed, SnapshotContext},
        view::{SnapshotView, hex},
    },
};

pub(crate) struct PostgresNativeSessionRepository {
    connection: DatabaseConnection,
    installer: OnceCell<PostgresMetadataInstallRepository>,
    qualified_session_sql: OnceCell<String>,
    restored: Mutex<HashMap<String, Arc<OnceCell<()>>>>,
}

impl PostgresNativeSessionRepository {
    pub(crate) fn new(connection: DatabaseConnection) -> Self {
        Self {
            connection,
            installer: OnceCell::new(),
            qualified_session_sql: OnceCell::new(),
            restored: Mutex::new(HashMap::new()),
        }
    }

    async fn installer(&self) -> Result<&PostgresMetadataInstallRepository, SnapshotError> {
        self.installer
            .get_or_try_init(|| PostgresMetadataInstallRepository::new(self.connection.clone()))
            .await
    }

    async fn session_sql(&self, installer: &PostgresMetadataInstallRepository) -> &str {
        self.qualified_session_sql
            .get_or_init(|| async { routes::session_sql(installer) })
            .await
    }

    pub(crate) async fn install(
        &self,
        built: &BuiltDescriptor,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<PreparedMetadataReceipt, SnapshotError> {
        let (receipt, work) = self.install_with_work(built, prepared).await?;
        tracing::debug!(
            snapshot_id = %built.snapshot_id,
            requested_pages = work.requested_pages,
            payload_pages_omitted = work.payload_pages_omitted,
            payload_pages_encoded = work.payload_pages_encoded,
            requested_payload_bytes_validated = work.requested_payload_bytes_validated,
            payload_bytes_omitted = work.payload_bytes_omitted,
            payload_bytes_encoded = work.payload_bytes_encoded,
            metadata_parameter_bytes = work.metadata_parameter_bytes,
            payload_parameter_bytes = work.payload_parameter_bytes,
            payload_transactions = work.transactions,
            registration_queries = work.registration_queries,
            requested_member_queries = work.requested_member_queries,
            classification_batches = work.classification_batches,
            insert_statements = work.insert_statements,
            byte_comparison_queries = work.byte_comparison_queries,
            committed_replay_pages = work.committed_replay_pages,
            payload_batch_elapsed_micros = work.elapsed_micros,
            "native metadata payload batch work"
        );
        Ok(receipt)
    }

    pub(crate) async fn install_with_work(
        &self,
        built: &BuiltDescriptor,
        prepared: &PreparedNativeMetadataRetention,
    ) -> Result<
        (
            PreparedMetadataReceipt,
            super::native_metadata_install::LegacyPayloadInstallWork,
        ),
        SnapshotError,
    > {
        if prepared.dag().root() != built.descriptor.metadata_root
            || prepared.scope() != built.descriptor.scope
        {
            return Err(integrity(
                "prepared DAG differs from the serving descriptor",
            ));
        }
        let installer = self.installer().await?;
        let intent = installer
            .begin_intent(&format!("http:{}", built.snapshot_id), prepared)
            .await
            .map_err(install_error)?;
        let capability = installer
            .mint_legacy_install_capability(&intent)
            .await
            .map_err(install_error)?;
        let mut work = super::native_metadata_install::LegacyPayloadInstallWork::default();
        for pages in prepared.dag().payloads().chunks(64) {
            let batch = installer
                .install_missing_pages_validated(&capability, pages)
                .await
                .map_err(install_error)?;
            work.record(batch);
        }
        let receipt = installer.finalize(&intent).await.map_err(install_error)?;
        Ok((receipt, work))
    }

    /// Projection and payload installation happen before this boundary.
    /// Cold handoff hashes a <=2 MiB plan, without a page/edge database scan.
    /// The formal native head and LIVE root are selected with lease creation.
    pub(crate) async fn open(
        &self,
        expected: &NativePublicationHead,
        built: &BuiltDescriptor,
        receipt: Option<&PreparedMetadataReceipt>,
        lease_seconds: u64,
    ) -> Result<Option<SnapshotContext>, SnapshotError> {
        let installer = self.installer().await?;
        let txn = self.transaction().await?;
        let result = async {
            routes::enter(&txn, installer).await?;
            let current = MonoStorage::read_native_publication_head_from(&txn, &expected.instance_id)
                .await.map_err(|_| not_ready("native publication is not ready"))?;
            if current.root != expected.root || current.token != expected.token {
                return Err(not_ready("native publication advanced during preparation; retry resolve"));
            }
            retention_lock(&txn).await?;
            routes::snapshot(&txn, installer, &built.snapshot_id).await?;
            let existing = mst2_snapshot_context::Entity::find_by_id(built.snapshot_id.clone())
                .one(&txn).await.map_err(internal)?;
            let prepare_id = if let Some(existing) = existing {
                if existing.canonical_descriptor != built.descriptor.encode().map_err(internal)?
                    || existing.commit_oid != expected.root.commit || existing.root_tree_oid != expected.root.tree
                    || existing.instance_id != expected.instance_id
                {
                    return Err(integrity("immutable session source conflicts with selected publication"));
                }
                if existing.state != "READY" || existing.authorization_epoch != 1 {
                    return Err(forbidden());
                }
                existing.prepare_id
            } else {
                let Some(receipt) = receipt else { return Ok(None); };
                if receipt.metadata_root() != built.descriptor.metadata_root {
                    return Err(integrity("metadata receipt root differs from descriptor"));
                }
                let prepare_id = receipt.intent().prepare_id().to_owned();
                let tagged_tree=git_internal::hash::ObjectHash::from_hex_for_kind(
                    git_internal::hash::get_hash_kind(),&expected.root.tree).map_err(internal)?.to_tagged_string();
                self.installer().await?.verify_receipt_in_txn(&txn, receipt,&tagged_tree,&built.descriptor.scope).await?;
                txn.execute_raw(statement(
                    "INSERT INTO mst2_snapshot_context(snapshot_id,canonical_descriptor,instance_id,commit_oid,
                     root_tree_oid,metadata_root,prepare_id,publication_sequence,writer_epoch,certificate_receipt_id,
                     authorization_epoch,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,1,'READY')",
                    [built.snapshot_id.clone().into(),built.descriptor.encode().map_err(internal)?.into(),
                     expected.instance_id.clone().into(),expected.root.commit.clone().into(),expected.root.tree.clone().into(),
                     built.descriptor.metadata_root.to_vec().into(),prepare_id.clone().into(),expected.token.sequence.into(),
                     expected.token.epoch.into(),expected.token.certificate.into()],
                )).await.map_err(internal)?;
                prepare_id
            };
            let node = root_node(built);
            let lease_id = uuid::Uuid::new_v4().to_string();
            PostgresRetentionRepository::acquire_existing_roots_in_txn(&txn,&node,&[
                RetentionRoot::Pin(format!("session:{}",built.snapshot_id)),RetentionRoot::Lease(lease_id.clone())
            ]).await?;
            PostgresRetentionRepository::release_root_in_txn(&txn,&RetentionRoot::Prepare(prepare_id)).await?;
            let row = txn.query_one_raw(statement(
                "INSERT INTO mst2_snapshot_lease(lease_id,snapshot_id,authorization_epoch,expires_at_unix,state,
                 publication_sequence,writer_epoch,certificate_receipt_id)
                 VALUES($1,$2,1,floor(extract(epoch FROM clock_timestamp()))::bigint+$3,'ACTIVE',$4,$5,$6)
                 RETURNING expires_at_unix",
                [lease_id.clone().into(),built.snapshot_id.clone().into(),(lease_seconds.clamp(1,3600) as i64).into(),
                 expected.token.sequence.into(),expected.token.epoch.into(),expected.token.certificate.into()],
            )).await.map_err(internal)?.ok_or_else(|| internal("lease insert returned no deadline"))?;
            expire_locked(&txn).await?;
            Ok(Some(SnapshotContext { built:built.clone(),commit_oid:expected.root.commit.clone(),
                root_tree_oid:expected.root.tree.clone(),lease_id,
                lease_expires_at_unix:row.try_get::<i64>("","expires_at_unix").map_err(internal)? as u64,
                authorization_epoch:1 }))
        }.await;
        let context = finish(txn, result).await?;
        if let Some(context) = &context
            && receipt.is_some()
        {
            let _ = self
                .verification_cell(&context.built.snapshot_id)
                .await
                .set(());
        }
        Ok(context)
    }

    pub(crate) async fn context(
        &self,
        snapshot_id: &str,
        lease_id: &str,
        instance_id: &str,
    ) -> Result<SnapshotContext, SnapshotError> {
        let installer = self.installer().await?;
        if routes::lease(&self.connection, installer, lease_id)
            .await?
            .as_deref()
            != Some(snapshot_id)
        {
            return Err(expired());
        }
        let row = self
            .connection
            .query_one_raw(statement(
                self.session_sql(installer).await,
                [snapshot_id.into(), lease_id.into()],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(expired)?;
        installer.verify_primary_scope_row(&row)?;
        let context = match decode_context(&row, snapshot_id, lease_id, instance_id) {
            Ok(context) => context,
            Err(error) => {
                if error.code == SnapshotErrorCode::LeaseExpired {
                    let txn = self.transaction().await?;
                    let result = async {
                        installer.verify_primary_connection(&txn).await?;
                        routes::lease(&txn, installer, lease_id).await?;
                        routes::generic_path(&txn, installer).await?;
                        retention_lock(&txn).await?;
                        expire_specific_locked(&txn, lease_id).await
                    }
                    .await;
                    finish(txn, result).await?;
                }
                return Err(error);
            }
        };
        let prepare_id: String = row.try_get("", "prepare_id").map_err(internal)?;
        let cell = self.verification_cell(snapshot_id).await;
        cell.get_or_try_init(|| async {
            self.installer()
                .await?
                .restore_session_dag(&prepare_id)
                .await
        })
        .await?;
        Ok(context)
    }

    pub(crate) async fn renew(
        &self,
        lease_id: &str,
        seconds: u64,
        instance: &str,
    ) -> Result<LeaseRenewed, SnapshotError> {
        let installer = self.installer().await?;
        let txn = self.transaction().await?;
        let result = async {
            installer.verify_primary_connection(&txn).await?;
            let selected_sid = routes::lease(&txn, installer, lease_id).await?
                .ok_or_else(|| SnapshotError::new(SnapshotErrorCode::LeaseUnknown,"unknown lease_id"))?;
            routes::generic_path(&txn, installer).await?;
            retention_lock(&txn).await?;
            let lease = txn.query_one_raw(statement(
                "SELECT snapshot_id FROM mst2_snapshot_lease WHERE lease_id=$1 FOR UPDATE",
                [lease_id.into()],
            )).await.map_err(internal)?.ok_or_else(|| SnapshotError::new(SnapshotErrorCode::LeaseUnknown,"unknown lease_id"))?;
            let sid: String = lease.try_get("","snapshot_id").map_err(internal)?;
            if sid != selected_sid {
                return Err(integrity("lease changed after immutable route selection"));
            }
            let row = txn.query_one_raw(statement(self.session_sql(installer).await,[sid.clone().into(),lease_id.into()]))
                .await.map_err(internal)?.ok_or_else(expired)?;
            self.installer().await?.verify_primary_scope_row(&row)?;
            if let Err(error)=decode_context(&row,&sid,lease_id,instance) {
                if error.code==SnapshotErrorCode::LeaseExpired {
                    expire_specific_locked(&txn,lease_id).await?;
                    return Ok(Err(error));
                }
                return Err(error);
            }
            let updated = txn.query_one_raw(statement(
                "UPDATE mst2_snapshot_lease SET expires_at_unix=greatest(expires_at_unix,
                 floor(extract(epoch FROM clock_timestamp()))::bigint)+$2 WHERE lease_id=$1
                 AND state='ACTIVE' AND expires_at_unix>=floor(extract(epoch FROM clock_timestamp()))::bigint
                 RETURNING expires_at_unix",
                [lease_id.into(),(seconds.clamp(1,3600) as i64).into()],
            )).await.map_err(internal)?;
            let Some(updated)=updated else {
                expire_specific_locked(&txn,lease_id).await?;
                return Ok(Err(expired()));
            };
            Ok(Ok(LeaseRenewed { lease_id:lease_id.into(),snapshot_id:sid,
                expires_at_unix:updated.try_get::<i64>("","expires_at_unix").map_err(internal)? as u64 }))
        }.await;
        finish(txn, result).await?
    }

    pub(crate) async fn release(&self, lease_id: &str) -> Result<bool, SnapshotError> {
        let installer = self.installer().await?;
        let txn = self.transaction().await?;
        let result = async {
            installer.verify_primary_connection(&txn).await?;
            if routes::lease(&txn, installer, lease_id).await?.is_none() {
                return Ok(false);
            }
            routes::generic_path(&txn, installer).await?;
            retention_lock(&txn).await?;
            let changed=txn.query_one_raw(statement(
                "UPDATE mst2_snapshot_lease SET state='RELEASED' WHERE lease_id=$1 AND state='ACTIVE' RETURNING snapshot_id",
                [lease_id.into()],
            )).await.map_err(internal)?;
            PostgresRetentionRepository::release_root_in_txn(&txn,&RetentionRoot::Lease(lease_id.into())).await?;
            expire_locked(&txn).await?;
            if let Some(row)=&changed {
                let sid:String=row.try_get("","snapshot_id").map_err(internal)?;
                retire_session_pin_locked(&txn,&sid).await?;
            }
            Ok(changed.is_some())
        }.await;
        finish(txn, result).await
    }

    async fn verification_cell(&self, sid: &str) -> Arc<OnceCell<()>> {
        let mut restored = self.restored.lock().await;
        if restored.len() >= 128
            && !restored.contains_key(sid)
            && let Some(old) = restored.keys().next().cloned()
        {
            restored.remove(&old);
        }
        restored.entry(sid.to_owned()).or_default().clone()
    }

    async fn transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        self.connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(internal)
    }
}

#[path = "native_snapshot_routes.rs"]
mod routes;

#[path = "native_snapshot_metadata_routes.rs"]
mod metadata_routes;

pub(crate) use metadata_routes::MetadataRouteRequest;
#[cfg(test)]
pub(crate) use metadata_routes::with_metadata_read_barriers;

const SESSION_SQL: &str = "SELECT s.snapshot_id,s.canonical_descriptor,s.commit_oid,s.root_tree_oid,
 (SELECT storage_uuid FROM mst2_metadata_storage_scope WHERE singleton=1) AS authority_storage_uuid,
 current_database() AS authority_database,
 (SELECT oid::bigint FROM pg_catalog.pg_database WHERE datname=current_database()) AS authority_database_oid,
 current_schema() AS authority_schema,
 (SELECT oid::bigint FROM pg_catalog.pg_namespace WHERE nspname=current_schema()) AS authority_schema_oid,
 inet_server_addr()::text AS authority_server_address,inet_server_port() AS authority_server_port,
 pg_is_in_recovery() AS authority_replica,
 s.metadata_root,s.prepare_id,s.authorization_epoch,s.state AS session_state,l.lease_id,
 l.authorization_epoch AS lease_epoch,l.expires_at_unix,l.state AS lease_state,
 floor(extract(epoch FROM clock_timestamp()))::bigint AS db_now,n.state AS root_state,
 EXISTS(SELECT 1 FROM mst2_retention_root rr WHERE rr.node_id=n.node_id AND rr.root_key='lease:'||l.lease_id
   AND rr.root_kind='lease') AS lease_covered,
 EXISTS(SELECT 1 FROM mst2_retention_root rr WHERE rr.node_id=n.node_id AND rr.root_key='pin:session:'||s.snapshot_id
   AND rr.root_kind='pin') AS session_covered,
 p.state AS prepare_state,p.metadata_root AS prepared_root,p.tagged_root_tree_oid,p.scope AS prepared_scope,
 p.source_domain,p.schema_version,p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,
 p.verification_revision,p.projection_revision,
 1::bigint AS root_count,s.commit_oid AS root_commit,s.root_tree_oid AS root_tree,s.instance_id,
 l.publication_sequence AS sequence,l.writer_epoch,'READY'::text AS state,s.commit_oid AS head_commit,
 s.root_tree_oid AS head_tree,l.certificate_receipt_id,c.receipt_id AS certificate_id,
 c.namespace AS certificate_namespace,c.instance_id AS certificate_instance,c.sequence AS certificate_sequence,
 c.writer_epoch AS certificate_epoch,c.old_root_commit AS certificate_old_commit,
 c.root_commit AS certificate_commit,c.root_tree AS certificate_tree,c.origin_path,c.origin_ref,c.old_path_commit,c.path_commit,
 r.id AS receipt_id,r.namespace AS receipt_namespace,r.sequence AS receipt_sequence,r.operation_id,r.old_oid,r.new_oid,
 r.writer_epoch AS receipt_epoch,r.writer_kind,r.request_digest,r.request_digest_version,r.native_certificate_version,
 o.id AS outbox_id,o.namespace AS outbox_namespace,o.sequence AS outbox_sequence
 FROM mst2_snapshot_context s JOIN mst2_snapshot_lease l ON l.snapshot_id=s.snapshot_id AND l.lease_id=$2
 LEFT JOIN mst2_retention_node n ON n.node_id='page:sha256:'||encode(s.metadata_root,'hex')
 LEFT JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
 LEFT JOIN mst2_native_publication c ON c.receipt_id=l.certificate_receipt_id
 LEFT JOIN mst2_publication r ON r.id=c.receipt_id
 LEFT JOIN mst2_publication_outbox o ON o.operation_id=r.operation_id WHERE s.snapshot_id=$1";

fn decode_context(
    row: &QueryResult,
    sid: &str,
    lease: &str,
    instance: &str,
) -> Result<SnapshotContext, SnapshotError> {
    let deadline: i64 = row.try_get("", "expires_at_unix").map_err(internal)?;
    if row.try_get::<String>("", "lease_state").map_err(internal)? != "ACTIVE"
        || deadline < row.try_get::<i64>("", "db_now").map_err(internal)?
    {
        return Err(expired());
    }
    let epoch: i64 = row.try_get("", "authorization_epoch").map_err(internal)?;
    if row
        .try_get::<String>("", "session_state")
        .map_err(internal)?
        != "READY"
        || epoch != 1
        || row.try_get::<i64>("", "lease_epoch").map_err(internal)? != epoch
    {
        return Err(forbidden());
    }
    if row
        .try_get::<Option<String>>("", "root_state")
        .map_err(internal)?
        .as_deref()
        != Some("LIVE")
        || !row.try_get::<bool>("", "lease_covered").map_err(internal)?
        || !row
            .try_get::<bool>("", "session_covered")
            .map_err(internal)?
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::ObjectUnavailable,
            "snapshot root protection is unavailable",
        ));
    }
    let head = decode_native_observation(row)
        .map_err(|_| integrity("durable native source certificate is invalid"))?
        .head
        .ok_or_else(|| integrity("durable native source is missing"))?;
    if head.instance_id != instance {
        return Err(forbidden());
    }
    let canonical: Vec<u8> = row.try_get("", "canonical_descriptor").map_err(internal)?;
    let descriptor = ServingDescriptor::decode(&canonical)
        .map_err(|_| integrity("invalid durable serving descriptor"))?;
    let view = SnapshotView::from_commit(&head.root.commit, &head.root.tree);
    let root: Vec<u8> = row.try_get("", "metadata_root").map_err(internal)?;
    let prepared_root: Option<Vec<u8>> = row.try_get("", "prepared_root").map_err(internal)?;
    let tagged_tree = git_internal::hash::ObjectHash::from_hex_for_kind(
        git_internal::hash::get_hash_kind(),
        &head.root.tree,
    )
    .map_err(|_| integrity("invalid durable tree identity"))?
    .to_tagged_string();
    if format!(
        "sha256:{}",
        hex(&descriptor.snapshot_id().map_err(internal)?)
    ) != sid
        || uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string() != instance
        || format!("sha256:{}", hex(&descriptor.namespace_view_id)) != view.view_id
        || descriptor.metadata_root.as_slice() != root.as_slice()
        || prepared_root.as_deref() != Some(root.as_slice())
        || row
            .try_get::<Option<String>>("", "prepare_state")
            .map_err(internal)?
            .as_deref()
            != Some("COMMITTED")
        || row
            .try_get::<Option<String>>("", "tagged_root_tree_oid")
            .map_err(internal)?
            .as_deref()
            != Some(tagged_tree.as_str())
        || row
            .try_get::<Option<String>>("", "prepared_scope")
            .map_err(internal)?
            .as_deref()
            != Some(descriptor.scope.as_str())
    {
        return Err(integrity(
            "durable snapshot identity or source proof is inconsistent",
        ));
    }
    let profile = [
        ("schema_version", mst2_codec::descriptor::SCHEMA_VERSION),
        ("metadata_codec", mst2_codec::descriptor::METADATA_CODEC),
        (
            "materialization_policy",
            mst2_codec::descriptor::MATERIALIZATION_POLICY_GIT_RAW_V1,
        ),
        (
            "fs_semantics",
            mst2_codec::descriptor::FS_SEMANTICS_LINUX_CODE_V1,
        ),
        (
            "access_projection",
            mst2_codec::descriptor::ACCESS_PROJECTION_EXACT_FULL,
        ),
        (
            "projection_revision",
            crate::ceres::snapshot::projection_observation::NATIVE_PROJECTION_REVISION,
        ),
    ];
    if profile.iter().any(|(column, value)| {
        row.try_get::<Option<i16>>("", column).ok().flatten() != Some(*value as i16)
    }) || row
        .try_get::<Option<String>>("", "source_domain")
        .map_err(internal)?
        .as_deref()
        != Some("native-git")
        || row
            .try_get::<Option<i32>>("", "verification_revision")
            .map_err(internal)?
            != Some(super::mono_storage::MST2_VERIFICATION_VERSION)
    {
        return Err(integrity(
            "durable snapshot materialization profile changed",
        ));
    }
    Ok(SnapshotContext {
        built: BuiltDescriptor {
            instance_id: instance.into(),
            snapshot_id: sid.into(),
            metadata_root: format!("sha256:{}", hex(&descriptor.metadata_root)),
            descriptor,
        },
        commit_oid: head.root.commit,
        root_tree_oid: head.root.tree,
        lease_id: lease.into(),
        lease_expires_at_unix: deadline as u64,
        authorization_epoch: epoch as u64,
    })
}

async fn retention_lock(txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
    txn.execute_raw(statement(
        "SELECT pg_advisory_xact_lock($1,hashtext(current_schema()))",
        [RETENTION_LOCK_KEY.into()],
    ))
    .await
    .map_err(internal)
    .map(|_| ())
}

async fn retire_session_pin_locked(
    txn: &DatabaseTransaction,
    sid: &str,
) -> Result<(), SnapshotError> {
    txn.execute_raw(statement(
        "DELETE FROM mst2_retention_root r WHERE r.root_kind='pin' AND r.root_key='pin:session:'||$1
         AND NOT EXISTS(SELECT 1 FROM mst2_snapshot_lease l WHERE l.snapshot_id=$1 AND l.state='ACTIVE'
           AND l.expires_at_unix>=floor(extract(epoch FROM clock_timestamp()))::bigint)",[sid.into()]
    )).await.map_err(internal).map(|_| ())
}

async fn expire_specific_locked(
    txn: &DatabaseTransaction,
    lease: &str,
) -> Result<(), SnapshotError> {
    let row=txn.query_one_raw(statement(
        "UPDATE mst2_snapshot_lease SET state='EXPIRED' WHERE lease_id=$1 AND state='ACTIVE'
         AND expires_at_unix<floor(extract(epoch FROM clock_timestamp()))::bigint RETURNING snapshot_id",
        [lease.into()],
    )).await.map_err(internal)?;
    if let Some(row) = row {
        PostgresRetentionRepository::release_root_in_txn(txn, &RetentionRoot::Lease(lease.into()))
            .await?;
        retire_session_pin_locked(
            txn,
            &row.try_get::<String>("", "snapshot_id").map_err(internal)?,
        )
        .await?;
    }
    Ok(())
}

async fn expire_locked(txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
    let rows = txn
        .query_all_raw(statement(
            "UPDATE mst2_snapshot_lease SET state='EXPIRED' WHERE lease_id IN
         (SELECT lease_id FROM mst2_snapshot_lease WHERE state='ACTIVE'
          AND expires_at_unix<floor(extract(epoch FROM clock_timestamp()))::bigint
          ORDER BY expires_at_unix LIMIT 128) RETURNING lease_id,snapshot_id",
            [],
        ))
        .await
        .map_err(internal)?;
    if rows.is_empty() {
        return Ok(());
    }
    let rows = rows
        .iter()
        .map(|row| {
            Ok(serde_json::json!({
                "lease":row.try_get::<String>("","lease_id").map_err(internal)?,
                "sid":row.try_get::<String>("","snapshot_id").map_err(internal)?
            }))
        })
        .collect::<Result<Vec<_>, SnapshotError>>()?;
    let encoded = serde_json::to_string(&rows).map_err(internal)?;
    txn.execute_raw(statement(
        "DELETE FROM mst2_retention_root r USING jsonb_to_recordset($1::jsonb) AS p(lease text,sid text)
         WHERE r.root_kind='lease' AND r.root_key='lease:'||p.lease",[encoded.clone().into()]
    )).await.map_err(internal)?;
    txn.execute_raw(statement(
        "DELETE FROM mst2_retention_root r USING jsonb_to_recordset($1::jsonb) AS p(lease text,sid text)
         WHERE r.root_kind='pin' AND r.root_key='pin:session:'||p.sid
         AND NOT EXISTS(SELECT 1 FROM mst2_snapshot_lease l WHERE l.snapshot_id=p.sid AND l.state='ACTIVE'
           AND l.expires_at_unix>=floor(extract(epoch FROM clock_timestamp()))::bigint)",[encoded.into()]
    )).await.map_err(internal).map(|_| ())
}

async fn finish<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    match result {
        Ok(value) => {
            txn.commit().await.map_err(|error| {
                tracing::error!(error=%error,"native session commit outcome unknown");
                SnapshotError::new(
                    SnapshotErrorCode::TemporaryUnavailable,
                    "session commit outcome unknown; retry resolve",
                )
            })?;
            Ok(value)
        }
        Err(error) => {
            txn.rollback().await.map_err(internal)?;
            Err(error)
        }
    }
}
fn statement<const N: usize>(sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}
fn root_node(built: &BuiltDescriptor) -> String {
    format!("page:{}", built.metadata_root)
}
fn internal(error: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, error.to_string())
}
fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn not_ready(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::SnapshotNotReady, message)
}
fn expired() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::LeaseExpired,
        "lease is not active for this snapshot; re-resolve",
    )
}
fn forbidden() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::ScopeForbidden,
        "snapshot serving state or authorization epoch changed",
    )
}
fn install_error(error: MetadataInstallError) -> SnapshotError {
    match error {
        MetadataInstallError::Rejected(error) if error.code == SnapshotErrorCode::Internal => {
            tracing::error!(%error,"native metadata installation unavailable");
            SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "native metadata installation unavailable",
            )
        }
        MetadataInstallError::Rejected(error) => error,
        MetadataInstallError::CommitUncertain {
            operation_id,
            phase,
            ..
        } => {
            tracing::error!(%operation_id,?phase,"native metadata commit outcome unknown");
            SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "metadata commit outcome unknown; retry resolve",
            )
        }
    }
}

impl super::Storage {
    pub(crate) async fn snapshot_sessions(&self) -> &PostgresNativeSessionRepository {
        use super::base_storage::StorageConnector;
        self.native_snapshot_sessions
            .get_or_init(|| async {
                PostgresNativeSessionRepository::new(self.mono_storage().get_connection().clone())
            })
            .await
    }

    pub(crate) async fn snapshot_context(
        &self,
        sid: &str,
        lease: &str,
    ) -> Result<SnapshotContext, SnapshotError> {
        let config = self.config();
        if config.mst2.publication_enabled {
            let instance = config
                .mst2
                .instance_uuid
                .as_deref()
                .ok_or_else(|| not_ready("native instance missing"))?;
            let instance = uuid::Uuid::parse_str(instance)
                .map_err(|_| not_ready("invalid native instance"))?
                .to_string();
            self.snapshot_sessions()
                .await
                .context(sid, lease, &instance)
                .await
        } else {
            let runtime = crate::ceres::snapshot::runtime::runtime();
            runtime.validate_lease(sid, lease)?;
            runtime.context(sid)
        }
    }

    pub(crate) async fn snapshot_renew(
        &self,
        lease: &str,
        seconds: u64,
    ) -> Result<LeaseRenewed, SnapshotError> {
        let config = self.config();
        if config.mst2.publication_enabled {
            let instance = config
                .mst2
                .instance_uuid
                .as_deref()
                .ok_or_else(|| not_ready("native instance missing"))?;
            let instance = uuid::Uuid::parse_str(instance)
                .map_err(|_| not_ready("invalid native instance"))?
                .to_string();
            self.snapshot_sessions()
                .await
                .renew(lease, seconds, &instance)
                .await
        } else {
            crate::ceres::snapshot::runtime::runtime().renew_lease(lease, seconds)
        }
    }

    pub(crate) async fn snapshot_release(&self, lease: &str) -> Result<bool, SnapshotError> {
        if self.config().mst2.publication_enabled {
            self.snapshot_sessions().await.release(lease).await
        } else {
            Ok(crate::ceres::snapshot::runtime::runtime().release_lease(lease))
        }
    }
}
