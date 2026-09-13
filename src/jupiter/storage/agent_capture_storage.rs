use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
    sea_query::{LockType, OnConflict},
};

use crate::{
    callisto::{
        agent_capture_access_audit, agent_capture_blob, agent_capture_blob_ref,
        agent_capture_checkpoint, agent_capture_deletion_ledger, agent_capture_event,
        agent_capture_file_op, agent_capture_ingest_receipt, agent_capture_session,
        agent_capture_source_stream, agent_capture_tombstone,
    },
    common::{canonical_json, errors::MegaError},
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionNaturalKey {
    pub deployment_id: String,
    pub tenant_id: String,
    pub repo_id: String,
    pub producer_id: String,
    pub session_kind: String,
    pub client_session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertEvent {
    pub capture_id: i64,
    pub event_uid: String,
    pub event_kind: String,
    pub native_id: Option<String>,
    pub lifecycle_seq: Option<i64>,
    pub payload: serde_json::Value,
}

#[derive(Clone)]
pub struct AgentCaptureStorage {
    pub base: BaseStorage,
}

impl Deref for AgentCaptureStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl AgentCaptureStorage {
    fn session_by_natural_key(
        key: &SessionNaturalKey,
    ) -> sea_orm::Select<agent_capture_session::Entity> {
        agent_capture_session::Entity::find()
            .filter(agent_capture_session::Column::DeploymentId.eq(key.deployment_id.clone()))
            .filter(agent_capture_session::Column::TenantId.eq(key.tenant_id.clone()))
            .filter(agent_capture_session::Column::RepoId.eq(key.repo_id.clone()))
            .filter(agent_capture_session::Column::ProducerId.eq(key.producer_id.clone()))
            .filter(agent_capture_session::Column::SessionKind.eq(key.session_kind.clone()))
            .filter(
                agent_capture_session::Column::ClientSessionId.eq(key.client_session_id.clone()),
            )
    }

    /// Allocate or return the server `capture_id` for a session natural key.
    ///
    /// Locks the natural-key row when it exists; otherwise inserts. A unique
    /// conflict (concurrent first PUT) re-reads the winner. Receipts are not
    /// consulted and do not mint `capture_id`.
    pub async fn upsert_session(&self, key: SessionNaturalKey) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        if let Some(existing) = Self::session_by_natural_key(&key)
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            txn.commit().await?;
            return Ok(existing.id);
        }

        let insert = agent_capture_session::ActiveModel {
            deployment_id: Set(key.deployment_id.clone()),
            tenant_id: Set(key.tenant_id.clone()),
            repo_id: Set(key.repo_id.clone()),
            producer_id: Set(key.producer_id.clone()),
            session_kind: Set(key.session_kind.clone()),
            client_session_id: Set(key.client_session_id.clone()),
            completeness: Set("empty".to_owned()),
            ..Default::default()
        };

        let result = agent_capture_session::Entity::insert(insert)
            .on_conflict(
                OnConflict::columns([
                    agent_capture_session::Column::DeploymentId,
                    agent_capture_session::Column::TenantId,
                    agent_capture_session::Column::RepoId,
                    agent_capture_session::Column::ProducerId,
                    agent_capture_session::Column::SessionKind,
                    agent_capture_session::Column::ClientSessionId,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&txn)
            .await;

        let capture_id = match result {
            Ok(inserted) => inserted.last_insert_id,
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                Self::session_by_natural_key(&key)
                    .lock(LockType::Update)
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(
                            "agent_capture_session unique conflict but row missing".to_owned(),
                        )
                    })?
                    .id
            }
            Err(err) => return Err(err.into()),
        };

        if capture_id == 0 {
            let existing = Self::session_by_natural_key(&key)
                .lock(LockType::Update)
                .one(&txn)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(
                        "agent_capture_session insert reported no capture_id".to_owned(),
                    )
                })?;
            txn.commit().await?;
            return Ok(existing.id);
        }

        txn.commit().await?;
        Ok(capture_id)
    }

    /// Insert an event by `(capture_id, event_uid)`. Same payload fingerprint
    /// is a no-op; a different payload is a conflict. Ordinary inserts never
    /// downgrade `complete` / `truncated` session completeness.
    pub async fn insert_event(&self, event: InsertEvent) -> Result<i64, MegaError> {
        let fingerprint = canonical_json::fingerprint(&event.payload.to_string())?;
        let txn = self.get_connection().begin().await?;
        let session = agent_capture_session::Entity::find_by_id(event.capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "agent_capture_session {} does not exist",
                    event.capture_id
                ))
            })?;

        if let Some(existing) = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(event.capture_id))
            .filter(agent_capture_event::Column::EventUid.eq(event.event_uid.clone()))
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            if existing.payload_fingerprint == fingerprint {
                tracing::debug!(
                    capture_id = event.capture_id,
                    event_uid = %event.event_uid,
                    payload_fingerprint = %fingerprint,
                    "agent capture event idempotent"
                );
                txn.commit().await?;
                return Ok(existing.id);
            }
            txn.rollback().await?;
            return Err(MegaError::Other(format!(
                "agent_capture_event uid conflict for capture_id {}",
                event.capture_id
            )));
        }

        let inserted = agent_capture_event::Entity::insert(agent_capture_event::ActiveModel {
            capture_id: Set(event.capture_id),
            event_uid: Set(event.event_uid.clone()),
            event_kind: Set(event.event_kind),
            native_id: Set(event.native_id),
            lifecycle_seq: Set(event.lifecycle_seq),
            payload: Set(event.payload),
            payload_fingerprint: Set(fingerprint.clone()),
            ..Default::default()
        })
        .exec(&txn)
        .await?;

        if session.completeness == "empty" {
            let mut session = session.into_active_model();
            session.completeness = Set("incomplete".to_owned());
            session.update(&txn).await?;
        }

        tracing::debug!(
            capture_id = event.capture_id,
            event_uid = %event.event_uid,
            payload_fingerprint = %fingerprint,
            "agent capture event inserted"
        );
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    /// Advance a source-stream watermark. `(generation, byte_offset)` must
    /// not move backwards; a greater generation may reset offset.
    pub async fn advance_source_stream(
        &self,
        capture_id: i64,
        stream_kind: &str,
        generation: i64,
        byte_offset: i64,
    ) -> Result<(), MegaError> {
        let txn = self.get_connection().begin().await?;
        agent_capture_session::Entity::find_by_id(capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;

        let current = agent_capture_source_stream::Entity::find()
            .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_source_stream::Column::StreamKind.eq(stream_kind))
            .order_by_desc(agent_capture_source_stream::Column::Generation)
            .lock(LockType::Update)
            .one(&txn)
            .await?;

        if let Some(current) = current {
            if generation < current.generation
                || (generation == current.generation && byte_offset < current.byte_offset)
            {
                txn.rollback().await?;
                return Err(MegaError::Other(format!(
                    "agent_capture_source_stream watermark regression for capture_id {capture_id}"
                )));
            }
            if generation == current.generation {
                if byte_offset > current.byte_offset {
                    let mut current = current.into_active_model();
                    current.byte_offset = Set(byte_offset);
                    current.update(&txn).await?;
                }
                txn.commit().await?;
                return Ok(());
            }
        }

        let insert =
            agent_capture_source_stream::Entity::insert(agent_capture_source_stream::ActiveModel {
                capture_id: Set(capture_id),
                stream_kind: Set(stream_kind.to_owned()),
                generation: Set(generation),
                byte_offset: Set(byte_offset),
                ..Default::default()
            })
            .exec(&txn)
            .await;

        match insert {
            Ok(_) => {}
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                let latest = agent_capture_source_stream::Entity::find()
                    .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
                    .filter(agent_capture_source_stream::Column::StreamKind.eq(stream_kind))
                    .filter(agent_capture_source_stream::Column::Generation.eq(generation))
                    .lock(LockType::Update)
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(
                            "agent_capture_source_stream unique conflict but row missing"
                                .to_owned(),
                        )
                    })?;
                if byte_offset < latest.byte_offset {
                    txn.rollback().await?;
                    return Err(MegaError::Other(format!(
                        "agent_capture_source_stream watermark regression for capture_id {capture_id}"
                    )));
                }
                if byte_offset > latest.byte_offset {
                    let mut latest = latest.into_active_model();
                    latest.byte_offset = Set(byte_offset);
                    latest.update(&txn).await?;
                }
            }
            Err(err) => return Err(err.into()),
        }
        txn.commit().await?;
        Ok(())
    }

    /// Insert a checkpoint. The parent session must be `external_capture`.
    pub async fn insert_checkpoint(
        &self,
        capture_id: i64,
        checkpoint_id: &str,
        transcript_digest: Option<String>,
        metadata: Option<serde_json::Value>,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;
        if session.session_kind != "external_capture" {
            txn.rollback().await?;
            return Err(MegaError::Other(format!(
                "checkpoint requires session_kind=external_capture for capture_id {capture_id}"
            )));
        }

        let inserted =
            agent_capture_checkpoint::Entity::insert(agent_capture_checkpoint::ActiveModel {
                capture_id: Set(capture_id),
                checkpoint_id: Set(checkpoint_id.to_owned()),
                transcript_digest: Set(transcript_digest),
                metadata: Set(metadata),
                ..Default::default()
            })
            .exec(&txn)
            .await?;
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    /// Insert a file_op. `source_event_uid` must exist on the same capture.
    pub async fn insert_file_op(
        &self,
        capture_id: i64,
        source_event_uid: &str,
        op: &str,
        path: &str,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let source = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_event::Column::EventUid.eq(source_event_uid))
            .one(&txn)
            .await?;
        if source.is_none() {
            txn.rollback().await?;
            return Err(MegaError::Other(format!(
                "file_op source event {source_event_uid} missing for capture_id {capture_id}"
            )));
        }

        let inserted = agent_capture_file_op::Entity::insert(agent_capture_file_op::ActiveModel {
            capture_id: Set(capture_id),
            source_event_uid: Set(source_event_uid.to_owned()),
            op: Set(op.to_owned()),
            path: Set(path.to_owned()),
            ..Default::default()
        })
        .exec(&txn)
        .await?;
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    fn blob_by_digest_key(
        deployment_id: &str,
        tenant_id: &str,
        digest: &str,
        visibility: &str,
    ) -> sea_orm::Select<agent_capture_blob::Entity> {
        agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_blob::Column::TenantId.eq(tenant_id.to_owned()))
            .filter(agent_capture_blob::Column::Digest.eq(digest.to_owned()))
            .filter(agent_capture_blob::Column::Visibility.eq(visibility.to_owned()))
    }

    fn receipt_by_scope(
        deployment_id: &str,
        tenant_id: &str,
        producer_id: &str,
        capture_id: i64,
        operation: &str,
        idempotency_key: &str,
    ) -> sea_orm::Select<agent_capture_ingest_receipt::Entity> {
        agent_capture_ingest_receipt::Entity::find()
            .filter(agent_capture_ingest_receipt::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_ingest_receipt::Column::TenantId.eq(tenant_id.to_owned()))
            .filter(agent_capture_ingest_receipt::Column::ProducerId.eq(producer_id.to_owned()))
            .filter(agent_capture_ingest_receipt::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_ingest_receipt::Column::Operation.eq(operation.to_owned()))
            .filter(
                agent_capture_ingest_receipt::Column::IdempotencyKey.eq(idempotency_key.to_owned()),
            )
    }

    /// Insert a staging blob, then in the same transaction mark it committed
    /// and attach a session `blob_ref`. Identical digest/visibility rows are
    /// shared; a second finalize only adds the owner ref.
    pub async fn finalize_blob_with_session_ref(
        &self,
        capture_id: i64,
        digest: &str,
        visibility: &str,
        object_key: &str,
        size_bytes: i64,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;

        let existing = Self::blob_by_digest_key(
            &session.deployment_id,
            &session.tenant_id,
            digest,
            visibility,
        )
        .lock(LockType::Update)
        .one(&txn)
        .await?;

        let blob_id = if let Some(existing) = existing {
            if existing.lease_state == "committed" {
                existing.id
            } else {
                txn.rollback().await?;
                return Err(MegaError::Other(format!(
                    "agent_capture_blob digest {digest} has uncommitted lease_state {}",
                    existing.lease_state
                )));
            }
        } else {
            let result = agent_capture_blob::Entity::insert(agent_capture_blob::ActiveModel {
                deployment_id: Set(session.deployment_id.clone()),
                tenant_id: Set(session.tenant_id.clone()),
                digest: Set(digest.to_owned()),
                visibility: Set(visibility.to_owned()),
                object_key: Set(object_key.to_owned()),
                size_bytes: Set(size_bytes),
                lease_state: Set("staging".to_owned()),
                lease_generation: Set(0),
                capture_id: Set(Some(capture_id)),
                upload_intent: Set(Some("stage".to_owned())),
                ..Default::default()
            })
            .on_conflict(
                OnConflict::columns([
                    agent_capture_blob::Column::DeploymentId,
                    agent_capture_blob::Column::TenantId,
                    agent_capture_blob::Column::Digest,
                    agent_capture_blob::Column::Visibility,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&txn)
            .await;

            let inserted_id = match result {
                Ok(inserted) if inserted.last_insert_id != 0 => Some(inserted.last_insert_id),
                Ok(_) => None,
                Err(err)
                    if matches!(err, DbErr::RecordNotInserted)
                        || is_unique_constraint_error(&err) =>
                {
                    None
                }
                Err(err) => return Err(err.into()),
            };

            let blob = Self::blob_by_digest_key(
                &session.deployment_id,
                &session.tenant_id,
                digest,
                visibility,
            )
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other("agent_capture_blob missing after staging insert".to_owned())
            })?;

            if let Some(inserted_id) = inserted_id {
                if blob.id != inserted_id || blob.lease_state != "staging" {
                    txn.rollback().await?;
                    return Err(MegaError::Other(format!(
                        "agent_capture_blob digest {digest} was not staged by this transaction"
                    )));
                }
                let mut blob = blob.into_active_model();
                blob.lease_state = Set("committed".to_owned());
                blob.lease_generation = Set(1);
                blob.upload_intent = Set(None);
                blob.update(&txn).await?;
                inserted_id
            } else if blob.lease_state == "committed" {
                blob.id
            } else {
                txn.rollback().await?;
                return Err(MegaError::Other(format!(
                    "agent_capture_blob digest {digest} has uncommitted lease_state {}",
                    blob.lease_state
                )));
            }
        };

        let existing_ref = agent_capture_blob_ref::Entity::find()
            .filter(agent_capture_blob_ref::Column::BlobId.eq(blob_id))
            .filter(agent_capture_blob_ref::Column::OwnerSessionId.eq(capture_id))
            .one(&txn)
            .await?;
        if existing_ref.is_none() {
            agent_capture_blob_ref::Entity::insert(agent_capture_blob_ref::ActiveModel {
                blob_id: Set(blob_id),
                owner_session_id: Set(Some(capture_id)),
                owner_event_id: Set(None),
                owner_checkpoint_id: Set(None),
                owner_file_op_id: Set(None),
                ..Default::default()
            })
            .exec(&txn)
            .await?;
        }
        txn.commit().await?;
        Ok(blob_id)
    }

    pub async fn upsert_ingest_receipt(
        &self,
        capture_id: i64,
        operation: &str,
        idempotency_key: &str,
        body: &serde_json::Value,
        response: Option<serde_json::Value>,
    ) -> Result<i64, MegaError> {
        let fingerprint = canonical_json::fingerprint(&body.to_string())?;
        let txn = self.get_connection().begin().await?;
        let session = agent_capture_session::Entity::find_by_id(capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;

        if let Some(existing) = Self::receipt_by_scope(
            &session.deployment_id,
            &session.tenant_id,
            &session.producer_id,
            capture_id,
            operation,
            idempotency_key,
        )
        .lock(LockType::Update)
        .one(&txn)
        .await?
        {
            if existing.fingerprint == fingerprint {
                txn.commit().await?;
                return Ok(existing.id);
            }
            txn.rollback().await?;
            return Err(MegaError::Other(format!(
                "ingest receipt fingerprint conflict for capture_id {capture_id}"
            )));
        }

        let result = agent_capture_ingest_receipt::Entity::insert(
            agent_capture_ingest_receipt::ActiveModel {
                deployment_id: Set(session.deployment_id.clone()),
                tenant_id: Set(session.tenant_id.clone()),
                producer_id: Set(session.producer_id.clone()),
                capture_id: Set(capture_id),
                operation: Set(operation.to_owned()),
                idempotency_key: Set(idempotency_key.to_owned()),
                fingerprint: Set(fingerprint.clone()),
                response: Set(response),
                ..Default::default()
            },
        )
        .on_conflict(
            OnConflict::columns([
                agent_capture_ingest_receipt::Column::DeploymentId,
                agent_capture_ingest_receipt::Column::TenantId,
                agent_capture_ingest_receipt::Column::ProducerId,
                agent_capture_ingest_receipt::Column::CaptureId,
                agent_capture_ingest_receipt::Column::Operation,
                agent_capture_ingest_receipt::Column::IdempotencyKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec(&txn)
        .await;

        let receipt_id = match result {
            Ok(inserted) if inserted.last_insert_id != 0 => inserted.last_insert_id,
            Ok(_) => {
                let existing = Self::receipt_by_scope(
                    &session.deployment_id,
                    &session.tenant_id,
                    &session.producer_id,
                    capture_id,
                    operation,
                    idempotency_key,
                )
                .lock(LockType::Update)
                .one(&txn)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(
                        "agent_capture_ingest_receipt insert reported no receipt_id".to_owned(),
                    )
                })?;
                if existing.fingerprint != fingerprint {
                    txn.rollback().await?;
                    return Err(MegaError::Other(format!(
                        "ingest receipt fingerprint conflict for capture_id {capture_id}"
                    )));
                }
                existing.id
            }
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                let existing = Self::receipt_by_scope(
                    &session.deployment_id,
                    &session.tenant_id,
                    &session.producer_id,
                    capture_id,
                    operation,
                    idempotency_key,
                )
                .lock(LockType::Update)
                .one(&txn)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(
                        "agent_capture_ingest_receipt unique conflict but row missing".to_owned(),
                    )
                })?;
                if existing.fingerprint != fingerprint {
                    txn.rollback().await?;
                    return Err(MegaError::Other(format!(
                        "ingest receipt fingerprint conflict for capture_id {capture_id}"
                    )));
                }
                existing.id
            }
            Err(err) => return Err(err.into()),
        };

        txn.commit().await?;
        Ok(receipt_id)
    }

    fn scoped_session(
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
    ) -> sea_orm::Select<agent_capture_session::Entity> {
        agent_capture_session::Entity::find_by_id(capture_id)
            .filter(agent_capture_session::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_session::Column::TenantId.eq(tenant_id.to_owned()))
    }

    pub async fn insert_access_audit(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        capture_id: i64,
        actor: &str,
        action: &str,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let session = Self::scoped_session(capture_id, deployment_id, tenant_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "agent_capture_session {capture_id} does not exist in scope"
                ))
            })?;

        let inserted =
            agent_capture_access_audit::Entity::insert(agent_capture_access_audit::ActiveModel {
                deployment_id: Set(session.deployment_id),
                tenant_id: Set(session.tenant_id),
                capture_id: Set(capture_id),
                actor: Set(actor.to_owned()),
                action: Set(action.to_owned()),
                ..Default::default()
            })
            .exec(&txn)
            .await?;
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    pub async fn insert_tombstone(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        capture_id: i64,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let session = Self::scoped_session(capture_id, deployment_id, tenant_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "agent_capture_session {capture_id} does not exist in scope"
                ))
            })?;

        if let Some(existing) = agent_capture_tombstone::Entity::find()
            .filter(agent_capture_tombstone::Column::DeploymentId.eq(session.deployment_id.clone()))
            .filter(agent_capture_tombstone::Column::TenantId.eq(session.tenant_id.clone()))
            .filter(agent_capture_tombstone::Column::RepoId.eq(session.repo_id.clone()))
            .filter(agent_capture_tombstone::Column::ProducerId.eq(session.producer_id.clone()))
            .filter(agent_capture_tombstone::Column::SessionKind.eq(session.session_kind.clone()))
            .filter(
                agent_capture_tombstone::Column::ClientSessionId
                    .eq(session.client_session_id.clone()),
            )
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            txn.commit().await?;
            return Ok(existing.id);
        }

        let result =
            agent_capture_tombstone::Entity::insert(agent_capture_tombstone::ActiveModel {
                deployment_id: Set(session.deployment_id.clone()),
                tenant_id: Set(session.tenant_id.clone()),
                repo_id: Set(session.repo_id.clone()),
                producer_id: Set(session.producer_id.clone()),
                session_kind: Set(session.session_kind.clone()),
                client_session_id: Set(session.client_session_id.clone()),
                capture_id: Set(Some(capture_id)),
                ..Default::default()
            })
            .on_conflict(
                OnConflict::columns([
                    agent_capture_tombstone::Column::DeploymentId,
                    agent_capture_tombstone::Column::TenantId,
                    agent_capture_tombstone::Column::RepoId,
                    agent_capture_tombstone::Column::ProducerId,
                    agent_capture_tombstone::Column::SessionKind,
                    agent_capture_tombstone::Column::ClientSessionId,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&txn)
            .await;

        let tombstone_id = match result {
            Ok(inserted) if inserted.last_insert_id != 0 => inserted.last_insert_id,
            Ok(_) => {
                agent_capture_tombstone::Entity::find()
                    .filter(
                        agent_capture_tombstone::Column::DeploymentId
                            .eq(session.deployment_id.clone()),
                    )
                    .filter(agent_capture_tombstone::Column::TenantId.eq(session.tenant_id.clone()))
                    .filter(agent_capture_tombstone::Column::RepoId.eq(session.repo_id.clone()))
                    .filter(
                        agent_capture_tombstone::Column::ProducerId.eq(session.producer_id.clone()),
                    )
                    .filter(
                        agent_capture_tombstone::Column::SessionKind
                            .eq(session.session_kind.clone()),
                    )
                    .filter(
                        agent_capture_tombstone::Column::ClientSessionId
                            .eq(session.client_session_id.clone()),
                    )
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other("agent_capture_tombstone insert reported no id".to_owned())
                    })?
                    .id
            }
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                agent_capture_tombstone::Entity::find()
                    .filter(
                        agent_capture_tombstone::Column::DeploymentId
                            .eq(session.deployment_id.clone()),
                    )
                    .filter(agent_capture_tombstone::Column::TenantId.eq(session.tenant_id.clone()))
                    .filter(agent_capture_tombstone::Column::RepoId.eq(session.repo_id.clone()))
                    .filter(
                        agent_capture_tombstone::Column::ProducerId.eq(session.producer_id.clone()),
                    )
                    .filter(
                        agent_capture_tombstone::Column::SessionKind
                            .eq(session.session_kind.clone()),
                    )
                    .filter(
                        agent_capture_tombstone::Column::ClientSessionId
                            .eq(session.client_session_id.clone()),
                    )
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(
                            "agent_capture_tombstone unique conflict but row missing".to_owned(),
                        )
                    })?
                    .id
            }
            Err(err) => return Err(err.into()),
        };

        txn.commit().await?;
        Ok(tombstone_id)
    }

    pub async fn is_tombstoned(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        capture_id: i64,
    ) -> Result<bool, MegaError> {
        let found = agent_capture_tombstone::Entity::find()
            .filter(agent_capture_tombstone::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_tombstone::Column::TenantId.eq(tenant_id.to_owned()))
            .filter(agent_capture_tombstone::Column::CaptureId.eq(capture_id))
            .one(self.get_connection())
            .await?;
        Ok(found.is_some())
    }

    pub async fn insert_deletion_ledger(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        capture_id: i64,
        blob_id: Option<i64>,
        intent: &str,
    ) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        let session = Self::scoped_session(capture_id, deployment_id, tenant_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "agent_capture_session {capture_id} does not exist in scope"
                ))
            })?;

        let inserted = agent_capture_deletion_ledger::Entity::insert(
            agent_capture_deletion_ledger::ActiveModel {
                deployment_id: Set(session.deployment_id),
                tenant_id: Set(session.tenant_id),
                blob_id: Set(blob_id),
                capture_id: Set(Some(capture_id)),
                intent: Set(intent.to_owned()),
                ..Default::default()
            },
        )
        .exec(&txn)
        .await?;
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }
}

fn is_unique_constraint_error(err: &sea_orm::DbErr) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("unique constraint")
        || msg.contains("unique violation")
        || msg.contains("duplicate key")
        || msg.contains("is not unique")
}

#[cfg(test)]
mod tests {
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, ModelTrait, PaginatorTrait,
    };

    use super::*;
    use crate::{
        callisto::{
            agent_capture_access_audit, agent_capture_blob, agent_capture_blob_ref,
            agent_capture_deletion_ledger, agent_capture_event, agent_capture_ingest_receipt,
            agent_capture_source_stream,
        },
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    fn sample_key() -> SessionNaturalKey {
        SessionNaturalKey {
            deployment_id: "default".to_owned(),
            tenant_id: "default".to_owned(),
            repo_id: "/third-part/mega".to_owned(),
            producer_id: "hook".to_owned(),
            session_kind: "external_capture".to_owned(),
            client_session_id: "provider__abc".to_owned(),
        }
    }

    async fn storage() -> (tempfile::TempDir, AgentCaptureStorage) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (
            temp_dir,
            AgentCaptureStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
        )
    }

    #[tokio::test]
    async fn upsert_session_returns_capture_id() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage
            .upsert_session(sample_key())
            .await
            .expect("upsert session");
        assert!(capture_id > 0, "capture_id must be a positive i64");
    }

    #[tokio::test]
    async fn upsert_session_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let first = storage
            .upsert_session(key.clone())
            .await
            .expect("first upsert");
        let second = storage
            .upsert_session(key.clone())
            .await
            .expect("second upsert");
        assert_eq!(first, second);
        let rows = AgentCaptureStorage::session_by_natural_key(&key)
            .count(storage.get_connection())
            .await
            .expect("count sessions");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn upsert_session_concurrent_first_insert_is_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let left = storage.clone();
        let right = storage.clone();
        let (first, second) = tokio::join!(
            left.upsert_session(key.clone()),
            right.upsert_session(key.clone())
        );
        let first = first.expect("concurrent upsert left");
        let second = second.expect("concurrent upsert right");
        assert_eq!(first, second);
        assert!(first > 0);
        let rows = AgentCaptureStorage::session_by_natural_key(&key)
            .count(storage.get_connection())
            .await
            .expect("count sessions");
        assert_eq!(rows, 1);
    }

    fn sample_event(capture_id: i64, payload: serde_json::Value) -> InsertEvent {
        InsertEvent {
            capture_id,
            event_uid: "0:0".to_owned(),
            event_kind: "message".to_owned(),
            native_id: None,
            lifecycle_seq: None,
            payload,
        }
    }

    #[tokio::test]
    async fn event_uid_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        let payload = serde_json::json!({"role": "user", "n": 1});
        let first = storage
            .insert_event(sample_event(capture_id, payload.clone()))
            .await
            .expect("first event");
        let second = storage
            .insert_event(sample_event(capture_id, payload))
            .await
            .expect("second event");
        assert_eq!(first, second);
        let rows = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .count(storage.get_connection())
            .await
            .expect("count events");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn event_uid_conflict_different_payload() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .insert_event(sample_event(
                capture_id,
                serde_json::json!({"role": "user"}),
            ))
            .await
            .expect("first event");
        let err = storage
            .insert_event(sample_event(
                capture_id,
                serde_json::json!({"role": "assistant"}),
            ))
            .await
            .expect_err("different payload must conflict");
        assert!(err.to_string().contains("uid conflict"));
    }

    #[tokio::test]
    async fn source_stream_watermark_monotonic() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 10)
            .await
            .expect("first watermark");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 5)
            .await
            .expect_err("offset regression");
        storage
            .advance_source_stream(capture_id, "jsonl", 1, 0)
            .await
            .expect("new generation may reset offset");
    }

    #[tokio::test]
    async fn source_stream_concurrent_generation_transition() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 10)
            .await
            .expect("seed generation 0");
        let left = storage.clone();
        let right = storage.clone();
        let (first, second) = tokio::join!(
            left.advance_source_stream(capture_id, "jsonl", 1, 200),
            right.advance_source_stream(capture_id, "jsonl", 1, 200)
        );
        first.expect("concurrent generation left");
        second.expect("concurrent generation right");
        let row = agent_capture_source_stream::Entity::find()
            .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_source_stream::Column::StreamKind.eq("jsonl"))
            .filter(agent_capture_source_stream::Column::Generation.eq(1))
            .one(storage.get_connection())
            .await
            .expect("load generation 1")
            .expect("generation 1 exists");
        assert_eq!(row.byte_offset, 200);
    }

    #[tokio::test]
    async fn event_requires_session() {
        let (_temp_dir, storage) = storage().await;
        storage
            .insert_event(sample_event(9_999_999, serde_json::json!({"ok": true})))
            .await
            .expect_err("missing session");
    }

    fn internal_key() -> SessionNaturalKey {
        let mut key = sample_key();
        key.session_kind = "internal_code".to_owned();
        key
    }

    #[tokio::test]
    async fn checkpoint_rejects_internal_code_parent() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage
            .upsert_session(internal_key())
            .await
            .expect("internal session");
        storage
            .insert_checkpoint(capture_id, "cp-1", None, None)
            .await
            .expect_err("internal_code cannot host checkpoints");
    }

    #[tokio::test]
    async fn checkpoint_accepts_external_capture_parent() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage
            .upsert_session(sample_key())
            .await
            .expect("external session");
        let id = storage
            .insert_checkpoint(capture_id, "cp-1", None, None)
            .await
            .expect("checkpoint");
        assert!(id > 0);
    }

    #[tokio::test]
    async fn file_op_requires_source_event() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .insert_file_op(capture_id, "0:0", "write", "src/main.rs")
            .await
            .expect_err("missing source event");
    }

    #[tokio::test]
    async fn file_op_inserts_when_source_exists() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .insert_event(sample_event(capture_id, serde_json::json!({"op": "write"})))
            .await
            .expect("source event");
        let id = storage
            .insert_file_op(capture_id, "0:0", "write", "src/main.rs")
            .await
            .expect("file_op");
        assert!(id > 0);
    }

    #[tokio::test]
    async fn finalize_writes_blob_ref_same_txn() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        let blob_id = storage
            .finalize_blob_with_session_ref(
                capture_id,
                "sha256:abc",
                "raw",
                "default/default/raw/sha256/abc",
                12,
            )
            .await
            .expect("finalize blob");
        let blob = agent_capture_blob::Entity::find_by_id(blob_id)
            .one(storage.get_connection())
            .await
            .expect("load blob")
            .expect("blob exists");
        assert_eq!(blob.lease_state, "committed");
        let refs = agent_capture_blob_ref::Entity::find()
            .filter(agent_capture_blob_ref::Column::BlobId.eq(blob_id))
            .all(storage.get_connection())
            .await
            .expect("load blob_ref");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].owner_session_id, Some(capture_id));
    }

    #[tokio::test]
    async fn ingest_receipt_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        let body = serde_json::json!({"op": "events"});
        let first = storage
            .upsert_ingest_receipt(capture_id, "events", "k1", &body, None)
            .await
            .expect("first receipt");
        let second = storage
            .upsert_ingest_receipt(capture_id, "events", "k1", &body, None)
            .await
            .expect("second receipt");
        assert_eq!(first, second);
        let rows = agent_capture_ingest_receipt::Entity::find()
            .filter(agent_capture_ingest_receipt::Column::CaptureId.eq(capture_id))
            .count(storage.get_connection())
            .await
            .expect("count receipts");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn ingest_receipt_fingerprint_conflict() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .upsert_ingest_receipt(
                capture_id,
                "events",
                "k1",
                &serde_json::json!({"op": "events"}),
                None,
            )
            .await
            .expect("first receipt");
        let err = storage
            .upsert_ingest_receipt(
                capture_id,
                "events",
                "k1",
                &serde_json::json!({"op": "other"}),
                None,
            )
            .await
            .expect_err("different body must conflict");
        assert!(err.to_string().contains("fingerprint conflict"));
    }

    #[tokio::test]
    async fn finalize_shares_committed_blob_by_digest() {
        let (_temp_dir, storage) = storage().await;
        let first_session = storage.upsert_session(sample_key()).await.expect("session");
        let mut other = sample_key();
        other.client_session_id = "provider__other".to_owned();
        let second_session = storage.upsert_session(other).await.expect("other session");
        let first = storage
            .finalize_blob_with_session_ref(
                first_session,
                "sha256:abc",
                "raw",
                "default/default/raw/sha256/abc",
                12,
            )
            .await
            .expect("first finalize");
        let second = storage
            .finalize_blob_with_session_ref(
                second_session,
                "sha256:abc",
                "raw",
                "default/default/raw/sha256/abc",
                12,
            )
            .await
            .expect("second finalize");
        assert_eq!(first, second);
        let blob_rows = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::Digest.eq("sha256:abc"))
            .count(storage.get_connection())
            .await
            .expect("count blobs");
        assert_eq!(blob_rows, 1);
        let refs = agent_capture_blob_ref::Entity::find()
            .filter(agent_capture_blob_ref::Column::BlobId.eq(first))
            .count(storage.get_connection())
            .await
            .expect("count refs");
        assert_eq!(refs, 2);
    }

    #[tokio::test]
    async fn finalize_rejects_foreign_staging_blob() {
        let (_temp_dir, storage) = storage().await;
        let owner = storage
            .upsert_session(sample_key())
            .await
            .expect("owner session");
        agent_capture_blob::Entity::insert(agent_capture_blob::ActiveModel {
            deployment_id: Set("default".to_owned()),
            tenant_id: Set("default".to_owned()),
            digest: Set("sha256:live".to_owned()),
            visibility: Set("raw".to_owned()),
            object_key: Set("default/default/staging/lease-owner".to_owned()),
            size_bytes: Set(4),
            lease_state: Set("staging".to_owned()),
            lease_generation: Set(0),
            capture_id: Set(Some(owner)),
            upload_intent: Set(Some("stage".to_owned())),
            ..Default::default()
        })
        .exec(storage.get_connection())
        .await
        .expect("foreign staging");

        let mut other = sample_key();
        other.client_session_id = "provider__other".to_owned();
        let other_id = storage.upsert_session(other).await.expect("other session");
        storage
            .finalize_blob_with_session_ref(
                other_id,
                "sha256:live",
                "raw",
                "default/default/raw/sha256/live",
                4,
            )
            .await
            .expect_err("must not steal live staging");

        let blob = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::Digest.eq("sha256:live"))
            .one(storage.get_connection())
            .await
            .expect("load blob")
            .expect("blob exists");
        assert_eq!(blob.lease_state, "staging");
        assert_eq!(blob.capture_id, Some(owner));
        let refs = agent_capture_blob_ref::Entity::find()
            .filter(agent_capture_blob_ref::Column::BlobId.eq(blob.id))
            .count(storage.get_connection())
            .await
            .expect("count refs");
        assert_eq!(refs, 0);
    }

    #[tokio::test]
    async fn tombstone_is_detected() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        assert!(
            !storage
                .is_tombstoned(&key.deployment_id, &key.tenant_id, capture_id)
                .await
                .expect("empty")
        );
        storage
            .insert_tombstone(&key.deployment_id, &key.tenant_id, capture_id)
            .await
            .expect("insert tombstone");
        assert!(
            storage
                .is_tombstoned(&key.deployment_id, &key.tenant_id, capture_id)
                .await
                .expect("detected")
        );
    }

    #[tokio::test]
    async fn access_audit_inserts_row() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        let id = storage
            .insert_access_audit(
                &key.deployment_id,
                &key.tenant_id,
                capture_id,
                "ingest-token",
                "read_transcript",
            )
            .await
            .expect("insert audit");
        assert!(id > 0);
        let row = agent_capture_access_audit::Entity::find_by_id(id)
            .one(storage.get_connection())
            .await
            .expect("load audit")
            .expect("audit exists");
        assert_eq!(row.capture_id, capture_id);
        assert_eq!(row.action, "read_transcript");
    }

    #[tokio::test]
    async fn access_audit_rejects_update() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        let id = storage
            .insert_access_audit(
                &key.deployment_id,
                &key.tenant_id,
                capture_id,
                "ingest-token",
                "read_transcript",
            )
            .await
            .expect("insert audit");
        let row = agent_capture_access_audit::Entity::find_by_id(id)
            .one(storage.get_connection())
            .await
            .expect("load audit")
            .expect("audit exists");
        let mut am = row.into_active_model();
        am.actor = Set("other".to_owned());
        am.update(storage.get_connection())
            .await
            .expect_err("access_audit is append-only");
    }

    #[tokio::test]
    async fn access_audit_rejects_delete() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        let id = storage
            .insert_access_audit(
                &key.deployment_id,
                &key.tenant_id,
                capture_id,
                "ingest-token",
                "read_transcript",
            )
            .await
            .expect("insert audit");
        let row = agent_capture_access_audit::Entity::find_by_id(id)
            .one(storage.get_connection())
            .await
            .expect("load audit")
            .expect("audit exists");
        row.delete(storage.get_connection())
            .await
            .expect_err("access_audit is append-only");
        let still = agent_capture_access_audit::Entity::find_by_id(id)
            .one(storage.get_connection())
            .await
            .expect("reload");
        assert!(still.is_some());
    }

    #[tokio::test]
    async fn deletion_ledger_inserts_intent() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        let id = storage
            .insert_deletion_ledger(
                &key.deployment_id,
                &key.tenant_id,
                capture_id,
                None,
                "expire_staging",
            )
            .await
            .expect("ledger");
        assert!(id > 0);
        let row = agent_capture_deletion_ledger::Entity::find_by_id(id)
            .one(storage.get_connection())
            .await
            .expect("load ledger")
            .expect("ledger exists");
        assert_eq!(row.intent, "expire_staging");
        assert_eq!(row.capture_id, Some(capture_id));
        assert!(row.blob_id.is_none());
    }

    #[tokio::test]
    async fn tombstone_and_audit_reject_foreign_scope() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let capture_id = storage.upsert_session(key.clone()).await.expect("session");
        storage
            .insert_tombstone(&key.deployment_id, &key.tenant_id, capture_id)
            .await
            .expect("insert tombstone");
        assert!(
            !storage
                .is_tombstoned("other-deploy", &key.tenant_id, capture_id)
                .await
                .expect("foreign deployment")
        );
        assert!(
            !storage
                .is_tombstoned(&key.deployment_id, "other-tenant", capture_id)
                .await
                .expect("foreign tenant")
        );
        storage
            .insert_access_audit(
                "other-deploy",
                &key.tenant_id,
                capture_id,
                "ingest-token",
                "read_transcript",
            )
            .await
            .expect_err("foreign audit scope");
        storage
            .insert_tombstone("other-deploy", &key.tenant_id, capture_id)
            .await
            .expect_err("foreign tombstone scope");
        storage
            .insert_deletion_ledger(
                &key.deployment_id,
                "other-tenant",
                capture_id,
                None,
                "expire_staging",
            )
            .await
            .expect_err("foreign ledger scope");
    }
}
