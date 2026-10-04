//! SQL receipt identity for the existing trunk queue adapter.
//!
//! This is not a complete namespace/projection publication implementation.

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr,
    EntityTrait, QueryFilter, Statement,
};
use sha2::{Digest, Sha256};

use crate::{
    callisto::{
        mst2_publication, mst2_publication_outbox, mst2_queue_noop_receipt, push_queue,
        sea_orm_active_enums::PushQueueKindEnum,
    },
    common::utils::MEGA_BRANCH_NAME,
    jupiter::storage::mono_storage::MonoStorage,
};

const REQUEST_DIGEST_VERSION: i32 = 1;
const WRITER_EPOCH: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PublicationReceiptError {
    #[error("MST2_PUBLICATION_CONFLICT: {0}")]
    Conflict(String),
    #[error("MST2_PUBLICATION_LEGACY_RECEIPT: operation {0} has no supported request digest")]
    LegacyReceipt(String),
    #[error("MST2_PUBLICATION_INTEGRITY_ERROR: {0}")]
    Integrity(String),
    #[error(transparent)]
    Database(#[from] DbErr),
}

/// Constructed from a server-persisted queue row, never a client digest.
#[derive(Debug, Clone)]
pub(crate) struct PublicationRequest {
    operation_id: String,
    namespace: String,
    writer_kind: String,
    request_digest: String,
    legacy_operation_id: Option<String>,
    is_noop: bool,
}

impl PublicationRequest {
    pub(crate) fn from_trunk_queue(
        row: &push_queue::Model,
    ) -> Result<Self, PublicationReceiptError> {
        if row.id <= 0 || row.operation_id.is_empty() {
            return Err(PublicationReceiptError::Integrity(
                "invalid queue operation identity".into(),
            ));
        }
        mst2_codec::descriptor::validate_scope(&row.path)
            .map_err(|error| PublicationReceiptError::Integrity(error.to_string()))?;
        let writer_kind = match &row.kind {
            PushQueueKindEnum::Push => "trunk_push",
            PushQueueKindEnum::Merge => "trunk_merge_stub",
            PushQueueKindEnum::Attach => {
                return Err(PublicationReceiptError::Integrity(
                    "attach is not covered by this adapter".into(),
                ));
            }
        };
        let operation_id = format!("mst2:trunk-queue:{}", row.id);
        let mut hash = request_hasher(&operation_id, &row.path, writer_kind);
        hash_field(&mut hash, row.operation_id.as_bytes());
        hash_field(&mut hash, MEGA_BRANCH_NAME.as_bytes());
        hash_field(&mut hash, row.old_id.as_bytes());
        hash_field(&mut hash, row.new_id.as_bytes());
        hash.update([u8::from(row.requester.is_some())]);
        if let Some(actor) = &row.requester {
            hash_field(&mut hash, actor.as_bytes());
        }
        hash_json(&mut hash, &row.payload);
        Ok(Self {
            operation_id,
            namespace: row.path.clone(),
            writer_kind: writer_kind.to_string(),
            request_digest: format!("sha256:{}", hex::encode(hash.finalize())),
            legacy_operation_id: Some(row.operation_id.clone()),
            is_noop: row.kind == PushQueueKindEnum::Push
                && row.old_id == row.new_id
                && row.payload.get("n").and_then(serde_json::Value::as_u64) == Some(0),
        })
    }

    #[cfg(test)]
    fn for_test(
        operation_id: &str,
        namespace: &str,
        old: &str,
        new: &str,
        writer_kind: &str,
    ) -> Self {
        let mut hash = request_hasher(operation_id, namespace, writer_kind);
        hash_field(&mut hash, old.as_bytes());
        hash_field(&mut hash, new.as_bytes());
        Self {
            operation_id: operation_id.to_owned(),
            namespace: namespace.to_owned(),
            writer_kind: writer_kind.to_owned(),
            request_digest: format!("sha256:{}", hex::encode(hash.finalize())),
            legacy_operation_id: None,
            is_noop: false,
        }
    }
}

fn request_hasher(operation_id: &str, namespace: &str, writer_kind: &str) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(b"mega.mst2.trunk-queue-request\0");
    hash.update(REQUEST_DIGEST_VERSION.to_le_bytes());
    hash.update(WRITER_EPOCH.to_le_bytes());
    hash_field(&mut hash, operation_id.as_bytes());
    hash_field(&mut hash, namespace.as_bytes());
    hash_field(&mut hash, writer_kind.as_bytes());
    hash
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_json(hash: &mut Sha256, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => hash.update([0]),
        Value::Bool(value) => hash.update([1, u8::from(*value)]),
        Value::Number(value) => {
            hash.update([2]);
            hash_field(hash, value.to_string().as_bytes());
        }
        Value::String(value) => {
            hash.update([3]);
            hash_field(hash, value.as_bytes());
        }
        Value::Array(values) => {
            hash.update([4]);
            hash.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_json(hash, value);
            }
        }
        Value::Object(values) => {
            hash.update([5]);
            hash.update((values.len() as u64).to_le_bytes());
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            for key in keys {
                hash_field(hash, key.as_bytes());
                hash_json(hash, &values[key]);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct CommittedPublication {
    pub(crate) receipt: mst2_publication::Model,
    pub(crate) outbox: mst2_publication_outbox::Model,
}

#[derive(Debug)]
pub(crate) enum PublicationPreparation {
    Prepared(PreparedPublication),
    AlreadyCommitted(CommittedPublication),
    AlreadyCommittedNoop(mst2_queue_noop_receipt::Model),
}

/// The SQL transaction id prevents finalizing a reservation in another txn.
#[derive(Debug)]
pub(crate) struct PreparedPublication {
    request: PublicationRequest,
    sequence: i64,
    transaction_id: i64,
}

async fn validate_committed_publication(
    txn: &DatabaseTransaction,
    request: &PublicationRequest,
    receipt: mst2_publication::Model,
) -> Result<CommittedPublication, PublicationReceiptError> {
    if receipt.namespace != request.namespace {
        return Err(PublicationReceiptError::Conflict(
            "operation belongs to another namespace".into(),
        ));
    }
    if receipt.request_digest_version != Some(REQUEST_DIGEST_VERSION)
        || receipt.request_digest.is_none()
    {
        return Err(PublicationReceiptError::LegacyReceipt(
            request.operation_id.clone(),
        ));
    }
    if receipt.writer_kind != request.writer_kind
        || receipt.writer_epoch != WRITER_EPOCH
        || receipt.request_digest.as_deref() != Some(request.request_digest.as_str())
    {
        return Err(PublicationReceiptError::Conflict(
            "operation request changed".into(),
        ));
    }
    let outbox = mst2_publication_outbox::Entity::find()
        .filter(mst2_publication_outbox::Column::OperationId.eq(&request.operation_id))
        .one(txn)
        .await?
        .ok_or_else(|| PublicationReceiptError::Integrity("receipt outbox missing".into()))?;
    if outbox.namespace != receipt.namespace || outbox.sequence != receipt.sequence {
        return Err(PublicationReceiptError::Integrity(
            "receipt outbox identity mismatch".into(),
        ));
    }
    Ok(CommittedPublication { receipt, outbox })
}

async fn reject_legacy_queue_receipt(
    txn: &DatabaseTransaction,
    legacy_operation_id: Option<&str>,
    namespace: &str,
) -> Result<(), PublicationReceiptError> {
    if let Some(legacy_id) = legacy_operation_id
        && let Some(legacy) = mst2_publication::Entity::find()
            .filter(mst2_publication::Column::OperationId.eq(legacy_id))
            .filter(mst2_publication::Column::Namespace.eq(namespace))
            .one(txn)
            .await?
        && (legacy.request_digest_version != Some(REQUEST_DIGEST_VERSION)
            || legacy.request_digest.is_none())
    {
        return Err(PublicationReceiptError::LegacyReceipt(legacy_id.to_owned()));
    }
    Ok(())
}

enum RecordedOperation {
    Publication(mst2_publication::Model),
    Noop(mst2_queue_noop_receipt::Model),
}

async fn find_committed_operation(
    txn: &DatabaseTransaction,
    operation_id: &str,
) -> Result<Option<RecordedOperation>, PublicationReceiptError> {
    let publication = mst2_publication::Entity::find()
        .filter(mst2_publication::Column::OperationId.eq(operation_id))
        .one(txn)
        .await?;
    let noop = mst2_queue_noop_receipt::Entity::find()
        .filter(mst2_queue_noop_receipt::Column::OperationId.eq(operation_id))
        .one(txn)
        .await?;
    match (publication, noop) {
        (Some(_), Some(_)) => Err(PublicationReceiptError::Integrity(
            "operation has both publication and no-op receipts".into(),
        )),
        (Some(receipt), None) => Ok(Some(RecordedOperation::Publication(receipt))),
        (None, Some(receipt)) => Ok(Some(RecordedOperation::Noop(receipt))),
        (None, None) => Ok(None),
    }
}

async fn validate_committed_operation(
    txn: &DatabaseTransaction,
    request: &PublicationRequest,
    existing: RecordedOperation,
) -> Result<PublicationPreparation, PublicationReceiptError> {
    match existing {
        RecordedOperation::Publication(receipt) => Ok(PublicationPreparation::AlreadyCommitted(
            validate_committed_publication(txn, request, receipt).await?,
        )),
        RecordedOperation::Noop(receipt) => {
            if receipt.namespace != request.namespace
                || receipt.writer_kind != request.writer_kind
                || receipt.writer_epoch != WRITER_EPOCH
                || !request.is_noop
                || receipt.request_digest != request.request_digest
            {
                return Err(PublicationReceiptError::Conflict(
                    "operation request changed".into(),
                ));
            }
            if receipt.request_digest_version != REQUEST_DIGEST_VERSION {
                return Err(PublicationReceiptError::LegacyReceipt(
                    request.operation_id.clone(),
                ));
            }
            if mst2_publication_outbox::Entity::find()
                .filter(mst2_publication_outbox::Column::OperationId.eq(&request.operation_id))
                .one(txn)
                .await?
                .is_some()
            {
                return Err(PublicationReceiptError::Integrity(
                    "no-op operation has a publication outbox".into(),
                ));
            }
            Ok(PublicationPreparation::AlreadyCommittedNoop(receipt))
        }
    }
}

impl MonoStorage {
    /// Queue admission can replay Done without executing B3; audit that path too.
    pub(crate) async fn validate_queue_publication_replay_in_txn(
        &self,
        txn: &DatabaseTransaction,
        candidate: &push_queue::Model,
    ) -> Result<(), PublicationReceiptError> {
        let operation_id = format!("mst2:trunk-queue:{}", candidate.id);
        let Some(existing) = find_committed_operation(txn, &operation_id).await? else {
            reject_legacy_queue_receipt(txn, Some(&candidate.operation_id), &candidate.path)
                .await?;
            return Ok(());
        };
        let request = PublicationRequest::from_trunk_queue(candidate)?;
        validate_committed_operation(txn, &request, existing).await?;
        Ok(())
    }

    /// Lock before any business ref write, then recheck immutable receipt identity.
    pub(crate) async fn begin_publication_in_txn(
        &self,
        txn: &DatabaseTransaction,
        request: PublicationRequest,
    ) -> Result<PublicationPreparation, PublicationReceiptError> {
        txn.execute_raw(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "INSERT INTO mst2_namespace_seq (namespace, sequence, epoch) VALUES ($1, 0, $2) \
             ON CONFLICT (namespace) DO NOTHING",
            [request.namespace.clone().into(), WRITER_EPOCH.into()],
        ))
        .await?;
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "SELECT sequence, epoch, txid_current() AS transaction_id \
             FROM mst2_namespace_seq WHERE namespace = $1 FOR UPDATE",
                [request.namespace.clone().into()],
            ))
            .await?
            .ok_or_else(|| PublicationReceiptError::Integrity("namespace row missing".into()))?;
        let sequence: i64 = row.try_get("", "sequence")?;
        let epoch: i64 = row.try_get("", "epoch")?;
        if let Some(existing) = find_committed_operation(txn, &request.operation_id).await? {
            return validate_committed_operation(txn, &request, existing).await;
        }
        if epoch != WRITER_EPOCH || sequence < 0 {
            return Err(PublicationReceiptError::Conflict(
                "writer epoch is fenced".into(),
            ));
        }
        reject_legacy_queue_receipt(
            txn,
            request.legacy_operation_id.as_deref(),
            &request.namespace,
        )
        .await?;
        Ok(PublicationPreparation::Prepared(PreparedPublication {
            request,
            sequence,
            transaction_id: row.try_get("", "transaction_id")?,
        }))
    }

    /// Finalize only the request reserved before this transaction's business writes.
    pub(crate) async fn record_publication_in_txn(
        &self,
        txn: &DatabaseTransaction,
        prepared: PreparedPublication,
        old_oid: &str,
        new_oid: &str,
    ) -> Result<CommittedPublication, PublicationReceiptError> {
        let request = prepared.request;
        if request.is_noop {
            return Err(PublicationReceiptError::Integrity(
                "no-op requests do not publish".into(),
            ));
        }
        let row = txn
            .query_one_raw(Statement::from_sql_and_values(
                txn.get_database_backend(),
                "UPDATE mst2_namespace_seq SET sequence = sequence + 1 \
             WHERE namespace = $1 AND sequence = $2 AND epoch = $3 AND txid_current() = $4 \
             RETURNING sequence",
                [
                    request.namespace.clone().into(),
                    prepared.sequence.into(),
                    WRITER_EPOCH.into(),
                    prepared.transaction_id.into(),
                ],
            ))
            .await?
            .ok_or_else(|| {
                PublicationReceiptError::Conflict(
                    "publication reservation is stale or belongs to another transaction".into(),
                )
            })?;
        let sequence: i64 = row.try_get("", "sequence")?;
        let now = chrono::Utc::now().fixed_offset();
        let receipt = mst2_publication::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            operation_id: Set(request.operation_id.clone()),
            namespace: Set(request.namespace.clone()),
            sequence: Set(sequence),
            old_oid: Set(old_oid.to_owned()),
            new_oid: Set(new_oid.to_owned()),
            writer_epoch: Set(WRITER_EPOCH),
            writer_kind: Set(request.writer_kind),
            request_digest: Set(Some(request.request_digest)),
            request_digest_version: Set(Some(REQUEST_DIGEST_VERSION)),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
        #[cfg(all(test, unix))]
        tests::crash_checkpoint("receipt-written");
        let outbox = mst2_publication_outbox::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            operation_id: Set(request.operation_id),
            namespace: Set(request.namespace),
            sequence: Set(sequence),
            state: Set("PENDING".to_owned()),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
        #[cfg(all(test, unix))]
        tests::crash_checkpoint("outbox-written");
        Ok(CommittedPublication { receipt, outbox })
    }

    pub(crate) async fn record_noop_operation_in_txn(
        &self,
        txn: &DatabaseTransaction,
        prepared: PreparedPublication,
        root_commit: &str,
        root_tree: &str,
        landed_commit_id: &str,
    ) -> Result<mst2_queue_noop_receipt::Model, PublicationReceiptError> {
        if !prepared.request.is_noop {
            return Err(PublicationReceiptError::Integrity(
                "request is not a trunk push no-op".into(),
            ));
        }
        txn.query_one_raw(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "SELECT sequence FROM mst2_namespace_seq \
             WHERE namespace = $1 AND sequence = $2 AND epoch = $3 AND txid_current() = $4 FOR UPDATE",
            [prepared.request.namespace.clone().into(), prepared.sequence.into(), WRITER_EPOCH.into(), prepared.transaction_id.into()],
        )).await?.ok_or_else(|| PublicationReceiptError::Conflict("no-op reservation is stale or belongs to another transaction".into()))?;
        let request = prepared.request;
        let receipt = mst2_queue_noop_receipt::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            operation_id: Set(request.operation_id),
            namespace: Set(request.namespace),
            request_digest: Set(request.request_digest),
            request_digest_version: Set(REQUEST_DIGEST_VERSION),
            writer_epoch: Set(WRITER_EPOCH),
            writer_kind: Set(request.writer_kind),
            observed_sequence: Set(prepared.sequence),
            observed_root_commit: Set(root_commit.to_owned()),
            observed_root_tree: Set(root_tree.to_owned()),
            landed_commit_id: Set(landed_commit_id.to_owned()),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
        #[cfg(all(test, unix))]
        tests::crash_checkpoint("noop-receipt-written");
        Ok(receipt)
    }

    #[cfg(test)]
    pub(crate) async fn record_test_publication_in_txn(
        &self,
        txn: &DatabaseTransaction,
        operation_id: &str,
        namespace: &str,
        old: &str,
        new: &str,
        writer_kind: &str,
    ) -> Result<i64, PublicationReceiptError> {
        let request = PublicationRequest::for_test(operation_id, namespace, old, new, writer_kind);
        let committed = match self.begin_publication_in_txn(txn, request).await? {
            PublicationPreparation::AlreadyCommitted(committed) => committed,
            PublicationPreparation::AlreadyCommittedNoop(_) => {
                return Err(PublicationReceiptError::Integrity(
                    "test publication replayed a no-op".into(),
                ));
            }
            PublicationPreparation::Prepared(prepared) => {
                self.record_publication_in_txn(txn, prepared, old, new)
                    .await?
            }
        };
        Ok(committed.receipt.sequence)
    }
}

#[cfg(test)]
#[path = "mst2_publication_tests.rs"]
mod tests;
