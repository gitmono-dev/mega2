//! Private preparation deadlines and terminal metadata-only pin release.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, IsolationLevel, Statement,
    TransactionTrait,
};
use sha2::{Digest, Sha256};

use super::{
    mst2_retention::PostgresRetentionRepository,
    native_metadata_install::{
        MetadataPrepareIntent, PostgresMetadataInstallRepository, StoredPlan,
        check_payload_coverage, load_installed_dag, load_plan, lock_prepare_operation,
        validate_operation_id, verify_graph_root,
    },
};
use crate::{
    callisto::mst2_metadata_prepare,
    ceres::snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        retention::RetentionRoot,
    },
};

pub(super) const DEFAULT_PREPARE_DURATION_MS: i64 = 3_600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrepareDeadline {
    pub(super) storage_uuid: String,
    pub(super) prepare_id: String,
    pub(super) operation_id: String,
    pub(super) manifest_digest: [u8; 32],
    pub(super) request_digest: [u8; 32],
    pub(super) grant_digest: [u8; 32],
    pub(super) duration_ms: i64,
    pub(super) granted_at_ms: i64,
    pub(super) expires_at_ms: i64,
}

pub(super) fn request_digest(
    storage_uuid: &str,
    operation_id: &str,
    manifest_digest: &[u8; 32],
    duration_ms: i64,
) -> Result<[u8; 32], SnapshotError> {
    validate_operation_id(operation_id)?;
    if !(1..=DEFAULT_PREPARE_DURATION_MS).contains(&duration_ms) {
        return Err(SnapshotError::new(
            SnapshotErrorCode::InvalidRequest,
            "metadata preparation duration must be 1..=3600000 milliseconds",
        ));
    }
    let storage = uuid::Uuid::parse_str(storage_uuid).map_err(internal)?;
    if storage.is_nil() || storage.to_string() != storage_uuid {
        return Err(integrity("invalid preparation storage UUID"));
    }
    Ok(framed_digest(
        b"mega.mst2.prepare-request.v1\0",
        &[
            storage_uuid.as_bytes(),
            operation_id.as_bytes(),
            manifest_digest,
            &duration_ms.to_be_bytes(),
        ],
    ))
}

impl PrepareDeadline {
    pub(super) fn new(
        storage_uuid: &str,
        prepare_id: &str,
        operation_id: &str,
        manifest_digest: [u8; 32],
        duration_ms: i64,
        granted_at_ms: i64,
    ) -> Result<Self, SnapshotError> {
        let request_digest =
            request_digest(storage_uuid, operation_id, &manifest_digest, duration_ms)?;
        let id = uuid::Uuid::parse_str(prepare_id).map_err(internal)?;
        if id.is_nil() || id.to_string() != prepare_id || granted_at_ms <= 0 {
            return Err(integrity("invalid preparation deadline identity or clock"));
        }
        let expires_at_ms = granted_at_ms
            .checked_add(duration_ms)
            .ok_or_else(|| integrity("preparation deadline overflow"))?;
        let grant_digest = framed_digest(
            b"mega.mst2.prepare-grant.v1\0",
            &[
                &request_digest,
                prepare_id.as_bytes(),
                &granted_at_ms.to_be_bytes(),
                &expires_at_ms.to_be_bytes(),
            ],
        );
        Ok(Self {
            storage_uuid: storage_uuid.into(),
            prepare_id: prepare_id.into(),
            operation_id: operation_id.into(),
            manifest_digest,
            request_digest,
            grant_digest,
            duration_ms,
            granted_at_ms,
            expires_at_ms,
        })
    }

    pub(super) async fn insert(&self, txn: &DatabaseTransaction) -> Result<(), SnapshotError> {
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_prepare_deadline(prepare_id,storage_uuid,operation_id,manifest_digest,
             request_digest,grant_digest,duration_ms,granted_at_ms,expires_at_ms)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            [self.prepare_id.clone().into(),self.storage_uuid.clone().into(),self.operation_id.clone().into(),
             self.manifest_digest.to_vec().into(),self.request_digest.to_vec().into(),self.grant_digest.to_vec().into(),
             self.duration_ms.into(),self.granted_at_ms.into(),self.expires_at_ms.into()],
        )).await.map_err(internal)?;
        Ok(())
    }
}

pub(super) async fn load_deadline<C: ConnectionTrait>(
    connection: &C,
    record: &mst2_metadata_prepare::Model,
    manifest_digest: &[u8; 32],
) -> Result<Option<PrepareDeadline>, SnapshotError> {
    let row = connection.query_one_raw(statement(
        "SELECT d.*,s.storage_uuid AS current_storage_uuid FROM mst2_metadata_prepare_deadline d
         JOIN mst2_metadata_storage_scope s ON s.singleton=1 WHERE d.prepare_id=$1",
        [record.prepare_id.clone().into()],
    )).await.map_err(internal)?;
    let Some(row) = row else {
        if record.deadline_managed {
            return Err(integrity(
                "managed preparation is missing its immutable deadline",
            ));
        }
        return Ok(None);
    };
    if !record.deadline_managed {
        return Err(integrity("historical preparation has an unbound deadline"));
    }
    let storage_uuid: String = row.try_get("", "storage_uuid").map_err(internal)?;
    if storage_uuid
        != row
            .try_get::<String>("", "current_storage_uuid")
            .map_err(internal)?
        || row.try_get::<String>("", "prepare_id").map_err(internal)? != record.prepare_id
        || row
            .try_get::<String>("", "operation_id")
            .map_err(internal)?
            != record.operation_id
        || row
            .try_get::<Vec<u8>>("", "manifest_digest")
            .map_err(internal)?
            .as_slice()
            != manifest_digest
    {
        return Err(integrity(
            "preparation deadline does not bind its storage and fixed plan",
        ));
    }
    let duration_ms = row.try_get("", "duration_ms").map_err(internal)?;
    let granted_at_ms = row.try_get("", "granted_at_ms").map_err(internal)?;
    let expected = PrepareDeadline::new(
        &storage_uuid,
        &record.prepare_id,
        &record.operation_id,
        *manifest_digest,
        duration_ms,
        granted_at_ms,
    )
    .map_err(|_| integrity("stored preparation deadline parameters are invalid"))?;
    if row
        .try_get::<Vec<u8>>("", "request_digest")
        .map_err(internal)?
        != expected.request_digest
        || row
            .try_get::<Vec<u8>>("", "grant_digest")
            .map_err(internal)?
            != expected.grant_digest
        || row.try_get::<i64>("", "expires_at_ms").map_err(internal)? != expected.expires_at_ms
    {
        return Err(integrity(
            "preparation deadline fields disagree with their canonical grant",
        ));
    }
    Ok(Some(expected))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrepareExpiryRequest {
    operation_id: String,
    operation_digest: [u8; 32],
    intent: MetadataPrepareIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrepareExpiryReceipt {
    request: PrepareExpiryRequest,
    previous_state: String,
    expires_at_ms: i64,
    expired_at_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareExpiryError {
    #[error(transparent)]
    Rejected(#[from] SnapshotError),
    #[error("preparation expiry commit outcome is unknown")]
    CommitUncertain { recovery: PrepareExpiryRequest },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrepareExpiryObservation {
    NotExpired { state: String },
    Expired(PrepareExpiryReceipt),
}

fn expiry_request(
    storage_uuid: &str,
    operation: &str,
    intent: &MetadataPrepareIntent,
) -> Result<PrepareExpiryRequest, SnapshotError> {
    validate_operation_id(operation)?;
    let grant_digest = intent.deadline_digest().ok_or_else(|| {
        conflict("historical preparation has an unknown deadline and cannot be expired")
    })?;
    let operation_digest = framed_digest(
        b"mega.mst2.prepare-expiry.v1\0",
        &[
            storage_uuid.as_bytes(),
            operation.as_bytes(),
            intent.prepare_id().as_bytes(),
            intent.operation_id().as_bytes(),
            &intent.manifest_digest(),
            &grant_digest,
        ],
    );
    Ok(PrepareExpiryRequest {
        operation_id: operation.into(),
        operation_digest,
        intent: intent.clone(),
    })
}

impl PostgresMetadataInstallRepository {
    pub(crate) async fn expire_prepare(
        &self,
        operation: &str,
        intent: &MetadataPrepareIntent,
    ) -> Result<PrepareExpiryReceipt, PrepareExpiryError> {
        let request = expiry_request(self.storage_uuid(), operation, intent)?;
        let txn = self.transaction().await?;
        let result = self.expire_prepare_in_txn(&txn, &request).await;
        match result {
            Ok(receipt) => {
                txn.commit().await.map_err(|_| expiry_uncertain(&request))?;
                Ok(receipt)
            }
            Err(error) => {
                txn.rollback()
                    .await
                    .map_err(|_| expiry_uncertain(&request))?;
                Err(error.into())
            }
        }
    }

    // Only the successful outer COMMIT or a fresh-primary recovery may expose this receipt.
    async fn expire_prepare_in_txn(
        &self,
        txn: &DatabaseTransaction,
        request: &PrepareExpiryRequest,
    ) -> Result<PrepareExpiryReceipt, SnapshotError> {
        self.barrier(txn).await?;
        validate_expiry_request(self.storage_uuid(), request)?;
        lock_prepare_operation(txn, request.intent.operation_id()).await?;
        let stored = expiry_plan(txn, request).await?;
        if let Some(event) = load_expiry(txn, &stored).await? {
            if event.request != *request {
                return Err(conflict(
                    "preparation was expired by another fixed operation",
                ));
            }
            return Ok(event);
        }
        if !["PREPARING", "COMMITTED"].contains(&stored.record.state.as_str()) {
            return Err(conflict("only an unconsumed preparation can expire"));
        }
        // Do not grant a new operation ID to a different preparation.
        if txn
            .query_one_raw(statement(
                "SELECT prepare_id FROM mst2_metadata_prepare_expiry WHERE operation_id=$1",
                [request.operation_id.clone().into()],
            ))
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(conflict(
                "expiry operation is already bound to another preparation",
            ));
        }
        let grant = stored
            .deadline
            .as_ref()
            .ok_or_else(|| integrity("expiry grant is missing"))?;
        let expired_at_ms = clock(txn).await?;
        if expired_at_ms < grant.expires_at_ms {
            return Err(conflict("preparation deadline has not passed"));
        }
        if stored.record.state == "COMMITTED" {
            load_installed_dag(txn, &stored).await?;
            check_payload_coverage(txn, &stored).await?;
            verify_graph_root(
                txn,
                &stored,
                &format!("prepare:{}", stored.record.prepare_id),
                "prepare",
            )
            .await?;
        } else {
            require_no_prepare_root(txn, &stored.record.prepare_id).await?;
        }
        require_no_consumption(txn, &stored.record.prepare_id).await?;
        let receipt = PrepareExpiryReceipt {
            request: request.clone(),
            previous_state: stored.record.state.clone(),
            expires_at_ms: grant.expires_at_ms,
            expired_at_ms,
        };
        txn.execute_raw(statement(
            "INSERT INTO mst2_metadata_prepare_expiry(prepare_id,operation_id,operation_digest,grant_digest,
             previous_state,pin_root,expires_at_ms,expired_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)",
            [stored.record.prepare_id.clone().into(),request.operation_id.clone().into(),request.operation_digest.to_vec().into(),
             grant.grant_digest.to_vec().into(),stored.record.state.clone().into(),format!("prepare:{}",stored.record.prepare_id).into(),
             grant.expires_at_ms.into(),expired_at_ms.into()],
        )).await.map_err(internal)?;
        let changed = txn
            .execute_raw(statement(
                "UPDATE mst2_metadata_prepare SET state='EXPIRED' WHERE prepare_id=$1 AND state=$2",
                [
                    stored.record.prepare_id.clone().into(),
                    stored.record.state.clone().into(),
                ],
            ))
            .await
            .map_err(internal)?;
        if changed.rows_affected() != 1 {
            return Err(integrity("preparation changed during expiry"));
        }
        PostgresRetentionRepository::release_root_in_txn(
            txn,
            &RetentionRoot::Prepare(stored.record.prepare_id.clone()),
        )
        .await?;
        // Read the final state rather than the earlier in-memory COMMITTED row.
        let final_plan = expiry_plan(txn, request).await?;
        if load_expiry(txn, &final_plan).await? != Some(receipt.clone()) {
            return Err(integrity(
                "expiry event differs from its atomic terminal state",
            ));
        }
        Ok(receipt)
    }

    pub(crate) async fn inspect_prepare_expiry(
        &self,
        fresh_primary: &DatabaseConnection,
        recovery: &PrepareExpiryRequest,
    ) -> Result<PrepareExpiryObservation, PrepareExpiryError> {
        validate_expiry_request(self.storage_uuid(), recovery)?;
        let txn = fresh_primary
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await
            .map_err(|_| expiry_uncertain(recovery))?;
        if self.barrier(&txn).await.is_err() {
            let _ = txn.rollback().await;
            return Err(expiry_uncertain(recovery));
        }
        let result = async {
            lock_prepare_operation(&txn, recovery.intent.operation_id()).await?;
            let stored = expiry_plan(&txn, recovery).await?;
            match load_expiry(&txn, &stored).await? {
                Some(receipt) if receipt.request == *recovery => Ok(PrepareExpiryObservation::Expired(receipt)),
                Some(_) => Err(conflict("recovered preparation expiry belongs to another operation")),
                None => {
                    if txn.query_one_raw(statement(
                        "SELECT prepare_id FROM mst2_metadata_prepare_expiry WHERE operation_id=$1",
                        [recovery.operation_id.clone().into()],
                    )).await.map_err(internal)?.is_some() {
                        return Err(conflict("recovered expiry operation belongs to another preparation"));
                    }
                    if stored.record.state == "COMMITTED" {
                        load_installed_dag(&txn, &stored).await?;
                        check_payload_coverage(&txn, &stored).await?;
                        verify_graph_root(&txn, &stored, &format!("prepare:{}", stored.record.prepare_id), "prepare").await?;
                    }
                    Ok(PrepareExpiryObservation::NotExpired { state: stored.record.state })
                }
            }
        }.await;
        txn.rollback()
            .await
            .map_err(|_| expiry_uncertain(recovery))?;
        result.map_err(|error: SnapshotError| {
            if error.code == SnapshotErrorCode::Internal {
                expiry_uncertain(recovery)
            } else {
                error.into()
            }
        })
    }
}

fn validate_expiry_request(
    storage_uuid: &str,
    request: &PrepareExpiryRequest,
) -> Result<(), SnapshotError> {
    if expiry_request(storage_uuid, &request.operation_id, &request.intent)? != *request {
        return Err(conflict(
            "expiry request does not bind the captured primary storage",
        ));
    }
    Ok(())
}

async fn expiry_plan<C: ConnectionTrait>(
    connection: &C,
    request: &PrepareExpiryRequest,
) -> Result<StoredPlan, SnapshotError> {
    let stored = load_plan(
        connection,
        request.intent.operation_id(),
        &request.intent.manifest_digest(),
    )
    .await?
    .ok_or_else(|| integrity("expiry preparation is missing"))?;
    if stored.intent()? != request.intent {
        return Err(conflict(
            "expiry request does not bind the fixed preparation grant",
        ));
    }
    Ok(stored)
}

pub(super) async fn load_expiry<C: ConnectionTrait>(
    connection: &C,
    stored: &StoredPlan,
) -> Result<Option<PrepareExpiryReceipt>, SnapshotError> {
    let row = connection
        .query_one_raw(statement(
            "SELECT * FROM mst2_metadata_prepare_expiry WHERE prepare_id=$1",
            [stored.record.prepare_id.clone().into()],
        ))
        .await
        .map_err(internal)?;
    let Some(row) = row else {
        if stored.record.state == "EXPIRED" {
            return Err(integrity(
                "expired preparation is missing its immutable event",
            ));
        }
        return Ok(None);
    };
    if stored.record.state != "EXPIRED" {
        return Err(integrity("expiry event has no terminal preparation state"));
    }
    let grant = stored
        .deadline
        .as_ref()
        .ok_or_else(|| integrity("expired preparation has no known grant"))?;
    let operation: String = row.try_get("", "operation_id").map_err(internal)?;
    let request = expiry_request(&grant.storage_uuid, &operation, &stored.intent()?)?;
    let previous_state: String = row.try_get("", "previous_state").map_err(internal)?;
    let expires_at_ms: i64 = row.try_get("", "expires_at_ms").map_err(internal)?;
    let expired_at_ms: i64 = row.try_get("", "expired_at_ms").map_err(internal)?;
    if row
        .try_get::<Vec<u8>>("", "operation_digest")
        .map_err(internal)?
        != request.operation_digest
        || row
            .try_get::<Vec<u8>>("", "grant_digest")
            .map_err(internal)?
            != grant.grant_digest
        || row.try_get::<String>("", "pin_root").map_err(internal)?
            != format!("prepare:{}", stored.record.prepare_id)
        || expires_at_ms != grant.expires_at_ms
        || expired_at_ms < expires_at_ms
        || !["PREPARING", "COMMITTED"].contains(&previous_state.as_str())
        || (previous_state == "COMMITTED") != stored.record.committed_at.is_some()
    {
        return Err(integrity(
            "expiry event disagrees with its fixed grant and unconsumed history",
        ));
    }
    require_no_prepare_root(connection, &stored.record.prepare_id).await?;
    require_no_consumption(connection, &stored.record.prepare_id).await?;
    Ok(Some(PrepareExpiryReceipt {
        request,
        previous_state,
        expires_at_ms,
        expired_at_ms,
    }))
}

async fn require_no_prepare_root<C: ConnectionTrait>(
    connection: &C,
    prepare_id: &str,
) -> Result<(), SnapshotError> {
    if connection
        .query_one_raw(statement(
            "SELECT node_id FROM mst2_retention_root WHERE root_key=$1 LIMIT 1",
            [format!("prepare:{prepare_id}").into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity(
            "terminal or unfinalized preparation still has Prepare pins",
        ));
    }
    Ok(())
}

async fn require_no_consumption<C: ConnectionTrait>(
    connection: &C,
    prepare_id: &str,
) -> Result<(), SnapshotError> {
    if connection
        .query_one_raw(statement(
            "SELECT prepare_id FROM mst2_metadata_prepare_consumption WHERE prepare_id=$1",
            [prepare_id.into()],
        ))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(integrity("expired preparation has a consumption event"));
    }
    Ok(())
}

fn expiry_uncertain(request: &PrepareExpiryRequest) -> PrepareExpiryError {
    PrepareExpiryError::CommitUncertain {
        recovery: request.clone(),
    }
}

pub(super) async fn clock<C: ConnectionTrait>(connection: &C) -> Result<i64, SnapshotError> {
    connection
        .query_one_raw(statement(
            "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
            [],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("database clock is missing"))?
        .try_get_by_index(0)
        .map_err(internal)
}

fn framed_digest(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    for bytes in fields {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    digest.finalize().into()
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
fn conflict(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Conflict, message)
}

#[cfg(test)]
#[path = "native_metadata_prepare_expiry_tests.rs"]
mod tests;
