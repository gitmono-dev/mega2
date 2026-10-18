//! Publish a traces ref after its raw bytes are already durable.

use crate::{
    common::errors::MegaError,
    jupiter::storage::{
        object_storage::MegaObjectStorageWrapper,
        trace_object_storage,
        trace_stream_storage::{self, PublishRef, PublishedRef},
    },
};

/// Raw bytes that belong to one object in a publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBytes {
    pub oid: String,
    pub bytes: Vec<u8>,
}

/// Bytes plus the ref update. The bytes are stored before the database transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishBytes {
    pub update: PublishRef,
    pub objects: Vec<ObjectBytes>,
}

/// Stores raw bytes, then publishes membership, the tip, and the journal.
#[derive(Clone)]
pub struct TracePublishService {
    store: MegaObjectStorageWrapper,
}

impl TracePublishService {
    pub fn new(store: MegaObjectStorageWrapper) -> Self {
        Self { store }
    }

    pub fn mock() -> Self {
        Self::new(crate::jupiter::storage::object_storage::mock_object_storage())
    }

    pub async fn publish(
        &self,
        db: &sea_orm::DatabaseConnection,
        request: &PublishBytes,
    ) -> Result<PublishedRef, MegaError> {
        trace_stream_storage::preflight(&request.update)?;
        for oid in &request.update.object_oids {
            let bytes = request
                .objects
                .iter()
                .find(|object| object.oid.eq_ignore_ascii_case(oid))
                .ok_or_else(|| {
                    MegaError::Traces(crate::common::errors::TracesError::InvalidParameter)
                })?;
            trace_object_storage::put_raw(
                &self.store,
                &request.update.stream_id,
                &request.update.hash_kind,
                oid,
                &bytes.bytes,
            )
            .await?;
        }
        trace_stream_storage::publish_ref(db, &request.update).await
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{trace_object, trace_principal, trace_stream, trace_target},
        jupiter::{
            migration::apply_migrations,
            storage::{
                trace_object_storage,
                trace_stream_storage::{CommitParentList, TRACES_REF},
            },
            tests::test_db_connection,
        },
    };

    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    async fn open_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (dir, db)
    }

    async fn stream(db: &sea_orm::DatabaseConnection) -> String {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id),
            target_id: Set(target_id),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("stream")
        .id
    }

    fn request(
        stream_id: &str,
        oid: &str,
        parents: Vec<String>,
        key: &str,
        bytes: &[u8],
    ) -> PublishBytes {
        PublishBytes {
            update: PublishRef {
                stream_id: stream_id.to_owned(),
                ref_names: vec![TRACES_REF.to_owned()],
                delete: false,
                new_tip: oid.to_owned(),
                commits: vec![CommitParentList {
                    oid: oid.to_owned(),
                    parents,
                }],
                checkpoints: Vec::new(),
                object_oids: vec![oid.to_owned()],
                hash_kind: "sha1".to_owned(),
                idempotency_key: key.to_owned(),
                schema_id: "manifest-v1".to_owned(),
            },
            objects: vec![ObjectBytes {
                oid: oid.to_owned(),
                bytes: bytes.to_vec(),
            }],
        }
    }

    #[tokio::test]
    async fn ref_cas_persists_bytes_before_a_rejected_update() {
        let (_dir, db) = open_db().await;
        let service = TracePublishService::mock();
        let stream_id = stream(&db).await;
        service
            .publish(
                &db,
                &request(&stream_id, OID_A, Vec::new(), "first", b"commit-a"),
            )
            .await
            .expect("first");
        let rejected = request(&stream_id, OID_B, Vec::new(), "orphan", b"commit-b\xff");
        assert!(service.publish(&db, &rejected).await.is_err());
        let stored = trace_object_storage::get_raw(&service.store, &stream_id, "sha1", OID_B)
            .await
            .expect("read")
            .expect("durable orphan");
        assert_eq!(stored, b"commit-b\xff");
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::Oid.eq(OID_B))
                .count(&db)
                .await
                .expect("membership"),
            0
        );
        let tip = trace_stream::Entity::find_by_id(&stream_id)
            .one(&db)
            .await
            .expect("stream")
            .expect("row")
            .tip_oid;
        assert_eq!(tip.as_deref(), Some(OID_A));
    }

    #[tokio::test]
    async fn ref_cas_one_winner_publishes_visible_membership() {
        let (_dir, db) = open_db().await;
        let service = TracePublishService::mock();
        let stream_id = stream(&db).await;
        let left_db = db.clone();
        let right_db = db.clone();
        let left_service = service.clone();
        let right_service = service.clone();
        let left_stream = stream_id.clone();
        let right_stream = stream_id.clone();
        let left = tokio::spawn(async move {
            left_service
                .publish(
                    &left_db,
                    &request(&left_stream, OID_A, Vec::new(), "cas-left", b"left"),
                )
                .await
        });
        let right = tokio::spawn(async move {
            right_service
                .publish(
                    &right_db,
                    &request(&right_stream, OID_B, Vec::new(), "cas-right", b"right"),
                )
                .await
        });
        let left = left.await.expect("left");
        let right = right.await.expect("right");
        let tip = trace_stream::Entity::find_by_id(&stream_id)
            .one(&db)
            .await
            .expect("stream")
            .expect("row")
            .tip_oid
            .expect("tip");
        match (left, right) {
            (Ok(published), Err(_)) => assert_eq!(published.new_tip, tip),
            (Err(_), Ok(published)) => assert_eq!(published.new_tip, tip),
            (Ok(_), Ok(_)) => panic!("both publications succeeded"),
            (Err(left_err), Err(right_err)) => panic!("both failed: {left_err}; {right_err}"),
        }
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&stream_id))
                .filter(trace_object::Column::Visible.eq(true))
                .count(&db)
                .await
                .expect("visible"),
            1
        );
    }

    #[tokio::test]
    async fn ref_cas_rejects_extra_ref_without_a_journal() {
        let (_dir, db) = open_db().await;
        let service = TracePublishService::mock();
        let stream_id = stream(&db).await;
        let mut request = request(&stream_id, OID_A, Vec::new(), "extra", b"ignored");
        request.update.ref_names = vec!["refs/heads/main".to_owned()];
        assert!(service.publish(&db, &request).await.is_err());
        assert!(
            trace_stream::Entity::find_by_id(&stream_id)
                .one(&db)
                .await
                .expect("stream")
                .expect("row")
                .tip_oid
                .is_none()
        );
    }
}
