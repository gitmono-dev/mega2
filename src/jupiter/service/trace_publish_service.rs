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
        authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
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
        trace_stream_storage::publish_ref(db, &request.update, authority).await
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

    #[derive(Clone)]
    struct Scope {
        stream_id: String,
        principal_id: String,
        target_id: String,
        path: String,
        credential_id: String,
    }

    async fn stream(db: &sea_orm::DatabaseConnection) -> Scope {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let path = format!("/team/{target_id}");
        let credential_id = format!("publish-{target_id}");
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
            project_path: Set(path.clone()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set(credential_id.clone()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("binding");
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("stream")
        .id;
        Scope {
            stream_id,
            principal_id,
            target_id,
            path,
            credential_id,
        }
    }

    fn token(scope: &Scope) -> crate::config::PushTokenConfig {
        crate::config::PushTokenConfig {
            name: scope.credential_id.clone(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec![scope.path.clone()]),
        }
    }

    async fn publish(
        service: &TracePublishService,
        db: &sea_orm::DatabaseConnection,
        scope: &Scope,
        request: &PublishBytes,
    ) -> Result<PublishedRef, MegaError> {
        use crate::ceres::traces::auth::{PublishAuthority, TracesCaller};
        let token = token(scope);
        let authority = PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            authorized_path: &scope.path,
            expected_generation: 1,
            policy: None,
        };
        service.publish(db, request, &authority).await
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
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        publish(
            &service,
            &db,
            &scope,
            &request(&stream_id, OID_A, Vec::new(), "first", b"commit-a"),
        )
        .await
        .expect("first");
        let rejected = request(&stream_id, OID_B, Vec::new(), "orphan", b"commit-b\xff");
        assert!(publish(&service, &db, &scope, &rejected).await.is_err());
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
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        let left_db = db.clone();
        let right_db = db.clone();
        let left_service = service.clone();
        let right_service = service.clone();
        let left_scope = scope.clone();
        let right_scope = scope.clone();
        let left = tokio::spawn(async move {
            let stream_id = left_scope.stream_id.clone();
            publish(
                &left_service,
                &left_db,
                &left_scope,
                &request(&stream_id, OID_A, Vec::new(), "cas-left", b"left"),
            )
            .await
        });
        let right = tokio::spawn(async move {
            let stream_id = right_scope.stream_id.clone();
            publish(
                &right_service,
                &right_db,
                &right_scope,
                &request(&stream_id, OID_B, Vec::new(), "cas-right", b"right"),
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
        let scope = stream(&db).await;
        let stream_id = scope.stream_id.clone();
        let mut request = request(&stream_id, OID_A, Vec::new(), "extra", b"ignored");
        request.update.ref_names = vec!["refs/heads/main".to_owned()];
        assert!(publish(&service, &db, &scope, &request).await.is_err());
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

    #[tokio::test]
    async fn publish_reauthorize_rejects_revoked_credential_and_rebound_target() {
        use sea_orm::{ColumnTrait, IntoActiveModel, QueryFilter};

        let (_dir, db) = open_db().await;
        let service = TracePublishService::mock();
        let revoked = stream(&db).await;
        let binding = crate::callisto::trace_credential_binding::Entity::find()
            .filter(
                crate::callisto::trace_credential_binding::Column::CredentialId
                    .eq(&revoked.credential_id),
            )
            .filter(
                crate::callisto::trace_credential_binding::Column::PrincipalId
                    .eq(&revoked.principal_id),
            )
            .one(&db)
            .await
            .expect("binding query")
            .expect("binding");
        let mut binding = binding.into_active_model();
        binding.retired_at = Set(Some(chrono::Utc::now().fixed_offset()));
        binding.update(&db).await.expect("retire");
        let revoked_request = request(
            &revoked.stream_id,
            OID_A,
            Vec::new(),
            "revoked",
            b"revoked-bytes",
        );
        assert!(
            publish(&service, &db, &revoked, &revoked_request)
                .await
                .is_err()
        );
        assert!(
            trace_stream::Entity::find_by_id(&revoked.stream_id)
                .one(&db)
                .await
                .expect("stream")
                .expect("row")
                .tip_oid
                .is_none()
        );

        let rebound = stream(&db).await;
        let target = trace_target::Entity::find_by_id(&rebound.target_id)
            .one(&db)
            .await
            .expect("target query")
            .expect("target");
        let mut target = target.into_active_model();
        target.project_path = Set("/other/project".to_owned());
        target.update(&db).await.expect("rebind");
        assert!(
            publish(
                &service,
                &db,
                &rebound,
                &request(&rebound.stream_id, OID_A, Vec::new(), "rebind", b"rebind"),
            )
            .await
            .is_err()
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&rebound.stream_id))
                .count(&db)
                .await
                .expect("membership"),
            0
        );

        let uncovered = stream(&db).await;
        let narrow = crate::config::PushTokenConfig {
            name: uncovered.credential_id.clone(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec!["/elsewhere".to_owned()]),
        };
        let authority = crate::ceres::traces::auth::PublishAuthority {
            caller: crate::ceres::traces::auth::TracesCaller::StaticPushToken {
                credential_id: &narrow.name,
                token: &narrow,
            },
            principal_id: &uncovered.principal_id,
            target_id: &uncovered.target_id,
            authorized_path: &uncovered.path,
            expected_generation: 1,
            policy: None,
        };
        assert!(
            service
                .publish(
                    &db,
                    &request(
                        &uncovered.stream_id,
                        OID_A,
                        Vec::new(),
                        "uncovered",
                        b"uncovered"
                    ),
                    &authority,
                )
                .await
                .is_err()
        );
        assert!(
            trace_stream::Entity::find_by_id(&uncovered.stream_id)
                .one(&db)
                .await
                .expect("stream")
                .expect("row")
                .tip_oid
                .is_none()
        );
    }
}
