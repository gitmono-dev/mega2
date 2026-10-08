//! Rooted session handoff and lease operations over the captured physical Q.

use mst2_codec::descriptor::ServingDescriptor;

use super::*;
use crate::{
    ceres::snapshot::{
        descriptor::BuiltDescriptor,
        rooted_metadata_projection::PreparedRootedNativeMetadata,
        runtime::{LeaseRenewed, SnapshotContext},
        view::{SnapshotView, hex},
    },
    jupiter::storage::native_publication_storage::NativePublicationHead,
};

fn gone() -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::LeaseExpired,
        "lease is not active for this snapshot; re-resolve",
    )
}

pub(super) async fn finish<T>(
    txn: DatabaseTransaction,
    result: Result<T, SnapshotError>,
) -> Result<T, SnapshotError> {
    match result {
        Ok(value) => {
            txn.commit().await.map_err(|_| {
                SnapshotError::new(
                    crate::ceres::snapshot::error::SnapshotErrorCode::TemporaryUnavailable,
                    "qualified transaction outcome unknown; retry the original operation",
                )
            })?;
            Ok(value)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

pub(super) fn context(
    row: &QueryResult,
    sid: &str,
    lease: &str,
    instance: &str,
) -> Result<SnapshotContext, SnapshotError> {
    let bytes: Vec<u8> = row.try_get("", "canonical_descriptor").map_err(internal)?;
    let descriptor = ServingDescriptor::decode(&bytes).map_err(internal)?;
    let commit: String = row.try_get("", "commit_oid").map_err(internal)?;
    let tree: String = row.try_get("", "root_tree_oid").map_err(internal)?;
    let view = SnapshotView::from_commit(&commit, &tree);
    if format!(
        "sha256:{}",
        hex(&descriptor.snapshot_id().map_err(internal)?)
    ) != sid
        || uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string() != instance
        || format!("sha256:{}", hex(&descriptor.namespace_view_id)) != view.view_id
    {
        return Err(integrity(
            "qualified durable descriptor differs from its fixed source",
        ));
    }
    let deadline = u64::try_from(
        row.try_get::<i64>("", "expires_at_unix")
            .map_err(internal)?,
    )
    .map_err(internal)?;
    let epoch = u64::try_from(
        row.try_get::<i64>("", "authorization_epoch")
            .map_err(internal)?,
    )
    .map_err(internal)?;
    let metadata_root = format!("sha256:{}", hex(&descriptor.metadata_root));
    Ok(SnapshotContext {
        built: BuiltDescriptor {
            descriptor,
            instance_id: instance.into(),
            snapshot_id: sid.into(),
            metadata_root,
        },
        commit_oid: commit,
        root_tree_oid: tree,
        lease_id: lease.into(),
        lease_expires_at_unix: deadline,
        authorization_epoch: epoch,
    })
}

impl RootedQualifiedMetadataRepository {
    pub(crate) async fn install(
        &self,
        built: &BuiltDescriptor,
        prepared: &PreparedRootedNativeMetadata,
    ) -> Result<RootedMetadataReceipt, MetadataInstallError> {
        if prepared.plan.root != built.descriptor.metadata_root
            || prepared.plan.identity.scope != built.descriptor.scope
        {
            return Err(integrity("rooted projection differs from the serving descriptor").into());
        }
        // Each attempt has its own durable identity. A previous retired
        // incarnation may have the same cold plan but a different generation;
        // its historical receipt cannot install or resurrect this attempt.
        let operation = format!("rooted-http:{}:{}", built.snapshot_id, uuid::Uuid::new_v4());
        let intent = self.begin_intent(&operation, &prepared.plan).await?;
        for pages in prepared.payloads.chunks(64) {
            self.install_pages(&intent, pages).await?;
        }
        self.finalize(&intent).await
    }

    pub(crate) async fn open_session(
        &self,
        expected: &NativePublicationHead,
        built: &BuiltDescriptor,
        receipt: Option<&RootedMetadataReceipt>,
        seconds: u64,
    ) -> Result<Option<SnapshotContext>, SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            if receipt.is_none() {
                txn.execute_raw(sql("SELECT mst2_metadata_cleanup_expired(64)",[])).await.map_err(database_error)?;
            }
            let present = txn.query_one_raw(sql("SELECT session_incarnation FROM mst2_qualified_session_incarnation
                WHERE snapshot_id=$1 AND state='READY'", [built.snapshot_id.clone().into()])).await.map_err(database_error)?;
            if present.is_none() && receipt.is_none() { return Ok(None); }
            if let Some(receipt) = receipt {
                self.require_intent(&txn, &receipt.intent).await?;
                if self.receipt(&txn, &receipt.intent).await? != *receipt
                    || receipt.metadata_root() != built.descriptor.metadata_root
                { return Err(integrity("qualified handoff receipt differs from its definitive canonical root")); }
            }
            let request = json!({
                "snapshot_id":built.snapshot_id,"canonical_descriptor":hex::encode(built.descriptor.encode().map_err(internal)?),
                "instance_id":expected.instance_id,"commit_oid":expected.root.commit,"root_tree_oid":expected.root.tree,
                "publication_sequence":expected.token.sequence,"writer_epoch":expected.token.epoch,
                "certificate_receipt_id":expected.token.certificate,"authorization_epoch":1,
                "lease_id":uuid::Uuid::new_v4().to_string(),"lease_seconds":seconds.clamp(1,3600),
                "session_incarnation":uuid::Uuid::new_v4().to_string(),
                "prepare_id":receipt.map(|r|r.intent.prepare_id.as_str()),
                "storage_seal":receipt.map(|r|hex::encode(r.intent.storage_seal)),
                "root_generation":receipt.map(|r|r.intent.root_generation),
                "attestation_id":receipt.map(|r|r.attestation_id.to_string()),
                "attestation_digest":receipt.map(|r|hex::encode(r.attestation_digest)),
                "certificate_digest":receipt.map(|r|hex::encode(r.certificate_digest)),
            });
            let row = txn.query_one_raw(sql("SELECT * FROM mst2_metadata_handoff($1::jsonb)", [request.into()]))
                .await.map_err(database_error)?.ok_or_else(|| integrity("qualified handoff returned no lease"))?;
            let lease: String = row.try_get("", "lease_id").map_err(internal)?;
            let row = self.session_row(&txn, &built.snapshot_id, &lease, &expected.instance_id).await?;
            let context = context(&row, &built.snapshot_id, &lease, &expected.instance_id)?;
            if context.built.descriptor != built.descriptor || context.commit_oid != expected.root.commit
                || context.root_tree_oid != expected.root.tree { return Err(integrity("qualified handoff fixed source changed")); }
            Ok(Some(context))
        }.await;
        finish(txn, result).await
    }

    pub(super) async fn session_row<C: ConnectionTrait>(
        &self,
        db: &C,
        sid: &str,
        lease: &str,
        instance: &str,
    ) -> Result<QueryResult, SnapshotError> {
        db.query_one_raw(sql(
            "SELECT * FROM mst2_metadata_session_row($1,$2,$3)",
            [sid.into(), lease.into(), instance.into()],
        ))
        .await
        .map_err(database_error)?
        .ok_or_else(gone)
    }

    pub(crate) async fn context(
        &self,
        sid: &str,
        lease: &str,
        instance: &str,
    ) -> Result<SnapshotContext, SnapshotError> {
        // Read-only revalidation grants no mutable authority. The helper checks
        // the exact route, incarnation, publication, LIVE root and owned roots.
        let txn = self.read_transaction().await?;
        let result = async {
            let row = self.session_row(&txn, sid, lease, instance).await?;
            context(&row, sid, lease, instance)
        }
        .await;
        finish(txn, result).await
    }

    pub(crate) async fn renew(
        &self,
        lease: &str,
        seconds: u64,
        instance: &str,
    ) -> Result<LeaseRenewed, SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            let row = txn
                .query_one_raw(sql(
                    "SELECT * FROM mst2_metadata_renew_lease($1,$2,$3)",
                    [
                        lease.into(),
                        (seconds.clamp(1, 3600) as i64).into(),
                        instance.into(),
                    ],
                ))
                .await
                .map_err(database_error)?
                .ok_or_else(gone)?;
            Ok(LeaseRenewed {
                lease_id: lease.into(),
                snapshot_id: row.try_get("", "snapshot_id").map_err(internal)?,
                expires_at_unix: u64::try_from(
                    row.try_get::<i64>("", "expires_at_unix")
                        .map_err(internal)?,
                )
                .map_err(internal)?,
            })
        }
        .await;
        finish(txn, result).await
    }

    pub(crate) async fn release(&self, lease: &str) -> Result<bool, SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            txn.query_one_raw(sql(
                "SELECT mst2_metadata_release_lease($1) AS released",
                [lease.into()],
            ))
            .await
            .map_err(database_error)?
            .ok_or_else(|| integrity("qualified release result missing"))?
            .try_get("", "released")
            .map_err(internal)
        }
        .await;
        finish(txn, result).await
    }

    pub(super) async fn read_transaction(&self) -> Result<DatabaseTransaction, SnapshotError> {
        let txn = self
            .connection
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(database_error)?;
        if registered(
            &txn,
            &(self.namespace.core_schema.clone(), self.namespace.core_oid),
        )
        .await
        .map_err(|error| match error {
            crate::common::errors::MegaError::Db(error) => database_error(error),
            error => internal(error),
        })?
        .as_ref()
            != Some(&self.namespace)
        {
            return Err(integrity(
                "qualified read physical namespace or authority catalog changed",
            ));
        }
        let valid: bool = txn
            .query_one_raw(sql(
                "SELECT mst2_metadata_scope_matches($1) AS valid",
                [self.primary_scope.clone().into()],
            ))
            .await
            .map_err(database_error)?
            .ok_or_else(|| integrity("qualified read scope missing"))?
            .try_get("", "valid")
            .map_err(internal)?;
        if !valid {
            return Err(integrity("qualified read left its captured primary scope"));
        }
        #[cfg(test)]
        if super::reader::READER_TEMP_SOURCE_SHADOW
            .try_with(|shadow| *shadow)
            .unwrap_or(false)
        {
            txn.execute_unprepared("CREATE TEMP TABLE mega_tree(id bigint,tree_id text,sub_trees bytea) ON COMMIT DROP;
                CREATE TEMP TABLE mst2_rooted_source_tree_revision(tree_id text,tree_row_id bigint,revision uuid,body_digest bytea,valid boolean) ON COMMIT DROP")
                .await.map_err(database_error)?;
        }
        Ok(txn)
    }
}
