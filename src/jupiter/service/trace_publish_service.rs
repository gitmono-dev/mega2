//! Publish a traces ref after its raw bytes are already durable.

use sea_orm::{DatabaseConnection, TransactionTrait};

#[cfg(test)]
use crate::jupiter::storage::trace_stream_storage::PublishRef;
use crate::{
    ceres::traces::{auth::PublishAuthority, publication::PreparedPublication},
    common::errors::MegaError,
    jupiter::{
        service::trace_target_service,
        storage::{
            mono_storage::MonoStorage,
            object_storage::MegaObjectStorageWrapper,
            trace_object_storage,
            trace_stream_storage::{self, PublishedRef, ScopeFacts},
            trace_target_storage::{self, TARGET_KIND_NATIVE},
        },
    },
};

/// Raw bytes that belong to one object in a publication.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBytes {
    pub oid: String,
    pub bytes: Vec<u8>,
}

/// Bytes plus the ref update. The bytes are stored before the database transaction.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishBytes {
    pub update: PublishRef,
    pub objects: Vec<ObjectBytes>,
}

/// One first-scope publication request.
#[cfg(test)]
pub struct FirstScopeRequest<'a> {
    pub path: &'a str,
    pub facts: &'a trace_stream_storage::ScopeFacts<'a>,
    pub request: &'a PublishBytes,
    pub authority: &'a crate::ceres::traces::auth::PublishAuthority<'a>,
}

enum ScopeUpdate<'a> {
    Prepared(&'a PreparedPublication),
    #[cfg(test)]
    Legacy(&'a PublishRef),
}

pub(in crate::jupiter) struct PreparedScopeRequest<'a> {
    pub(in crate::jupiter) path: &'a str,
    pub(in crate::jupiter) facts: &'a ScopeFacts<'a>,
    pub(in crate::jupiter) prepared: &'a PreparedPublication,
    pub(in crate::jupiter) authority: &'a PublishAuthority<'a>,
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

    async fn store_prepared(&self, prepared: &PreparedPublication) -> Result<(), MegaError> {
        trace_stream_storage::preflight(prepared.update())?;
        for oid in &prepared.update().object_oids {
            if let Some(bytes) = prepared.incoming_bytes(oid).await? {
                prepared
                    .control()
                    .run(trace_object_storage::put_raw(
                        &self.store,
                        &prepared.update().stream_id,
                        &prepared.update().hash_kind,
                        oid,
                        &bytes,
                    ))
                    .await
                    .map_err(super::trace_object_service::raw_storage_error)?;
            }
        }
        prepared.control().check()
    }

    // Internal only until the history-policy witness in MTR-101 is enforced.
    pub(in crate::jupiter) async fn publish_prepared(
        &self,
        db: &DatabaseConnection,
        prepared: &PreparedPublication,
        authority: &PublishAuthority<'_>,
    ) -> Result<PublishedRef, MegaError> {
        self.store_prepared(prepared).await?;
        trace_stream_storage::publish_prepared_ref(db, prepared, authority)
            .await
            .map_err(super::trace_object_service::raw_storage_error)
    }

    pub(in crate::jupiter) async fn publish_prepared_first_scope(
        &self,
        config: &crate::config::MonoConfig,
        db: &DatabaseConnection,
        mono: &MonoStorage,
        scope: &PreparedScopeRequest<'_>,
    ) -> Result<PublishedRef, MegaError> {
        self.publish_prepared_first_scope_inner(config, db, mono, scope)
            .await
            .map_err(super::trace_object_service::raw_storage_error)
    }

    async fn publish_prepared_first_scope_inner(
        &self,
        config: &crate::config::MonoConfig,
        db: &DatabaseConnection,
        mono: &MonoStorage,
        scope: &PreparedScopeRequest<'_>,
    ) -> Result<PublishedRef, MegaError> {
        let prepared = scope.prepared;
        if prepared.update().stream_id != scope.facts.stream_id
            || prepared.update().hash_kind != scope.facts.hash_kind
        {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        self.store_prepared(prepared).await?;
        let path = trace_target_storage::strict_canonical_project_path(scope.path)
            .map_err(|_| crate::ceres::traces::auth::publish_rejection())?;
        let control = prepared.control();
        let txn = control.run(async { Ok(db.begin().await?) }).await?;
        let result = control
            .run(Self::publish_first_scope_in_txn(
                &txn,
                config,
                mono,
                &path,
                scope.facts,
                ScopeUpdate::Prepared(prepared),
                scope.authority,
            ))
            .await;
        match result {
            Ok(published) => {
                control.check()?;
                txn.commit().await?;
                Ok(published)
            }
            Err(error) => {
                txn.rollback().await?;
                Err(error)
            }
        }
    }

    #[cfg(test)]
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

    /// Publish a scope's first ref. The native target binding, the stream
    /// identity and the ref publication share one database transaction; raw
    /// bytes are durable before it. A rejection leaves no target or stream row.
    #[cfg(test)]
    pub async fn publish_first_scope(
        &self,
        config: &crate::config::MonoConfig,
        db: &DatabaseConnection,
        mono: &MonoStorage,
        scope: &FirstScopeRequest<'_>,
    ) -> Result<PublishedRef, MegaError> {
        let FirstScopeRequest {
            path,
            facts,
            request,
            authority,
        } = *scope;
        trace_stream_storage::preflight(&request.update)?;
        if request.update.stream_id != facts.stream_id
            || request.update.hash_kind != facts.hash_kind
        {
            return Err(MegaError::Other(
                "first scope facts and ref update disagree".into(),
            ));
        }
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
        let path = trace_target_storage::strict_canonical_project_path(path)
            .map_err(|_| crate::ceres::traces::auth::publish_rejection())?;
        let txn = db.begin().await?;
        let published = Self::publish_first_scope_in_txn(
            &txn,
            config,
            mono,
            &path,
            facts,
            ScopeUpdate::Legacy(&request.update),
            authority,
        )
        .await;
        match published {
            Ok(published) => {
                txn.commit().await?;
                Ok(published)
            }
            Err(err) => {
                txn.rollback().await?;
                Err(err)
            }
        }
    }

    async fn publish_first_scope_in_txn(
        txn: &sea_orm::DatabaseTransaction,
        config: &crate::config::MonoConfig,
        mono: &MonoStorage,
        path: &str,
        facts: &ScopeFacts<'_>,
        update: ScopeUpdate<'_>,
        authority: &PublishAuthority<'_>,
    ) -> Result<PublishedRef, MegaError> {
        let Some(root) = mono.lock_native_root_in_txn(txn).await? else {
            return Err(MegaError::Other("native root is missing".into()));
        };
        let eligible = mono
            .existing_path_trees_in_txn(&root.ref_tree_hash, &[path.to_owned()], txn)
            .await?;
        if !eligible.contains(path) {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        // The caller's authorization decision is binding: its path and expected
        // generation stay untouched, so a rebind or generation change between
        // authorization and this transaction is rejected by the recheck instead
        // of being compared against itself.
        let authorized =
            trace_target_storage::strict_canonical_project_path(authority.authorized_path)
                .map_err(|_| crate::ceres::traces::auth::publish_rejection())?;
        if authorized != path {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        // A path inside the ImportRepo namespace never gets a native binding,
        // even when no target row exists there yet.
        if crate::ceres::pack::path_policy::in_import_namespace(config, path) {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        let target = trace_target_service::bind_first_native_write(txn, path).await?;
        if target.target_kind != TARGET_KIND_NATIVE {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        // A path-level authorization carries no target id; an authorization
        // that named a target must still name the same binding. Otherwise a
        // retired target's grant would silently apply to its replacement.
        if !authority.target_id.is_empty() && authority.target_id != target.id {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        if !facts.target_id.is_empty() && facts.target_id != target.id {
            return Err(crate::ceres::traces::auth::publish_rejection());
        }
        let scoped_authority = PublishAuthority {
            target_id: &target.id,
            ..*authority
        };
        let scoped_facts = ScopeFacts {
            target_id: &target.id,
            ..*facts
        };
        match update {
            ScopeUpdate::Prepared(prepared) => {
                trace_stream_storage::publish_prepared_scope_in_txn(
                    txn,
                    &scoped_facts,
                    prepared,
                    &scoped_authority,
                )
                .await
            }
            #[cfg(test)]
            ScopeUpdate::Legacy(update) => {
                trace_stream_storage::publish_scope_in_txn(
                    txn,
                    &scoped_facts,
                    update,
                    &scoped_authority,
                )
                .await
            }
        }
    }

    /// Stage objects in quarantine, then publish only a closed set.
    #[cfg(test)]
    pub async fn publish_quarantine(
        &self,
        db: &sea_orm::DatabaseConnection,
        ingest_id: &str,
        budget: crate::ceres::traces::ingest::UnpackBudget,
        objects: &[crate::ceres::traces::ingest::LooseObject],
        update: &PublishRef,
        authority: &crate::ceres::traces::auth::PublishAuthority<'_>,
    ) -> Result<PublishedRef, MegaError> {
        trace_stream_storage::preflight(update)?;
        crate::ceres::traces::ingest::unpack_into_quarantine(
            &self.store,
            ingest_id,
            &update.hash_kind,
            budget,
            objects,
        )
        .await?;
        crate::ceres::traces::ingest::require_closure(
            &self.store,
            &update.stream_id,
            &update.hash_kind,
            objects,
        )
        .await?;
        for object in objects {
            trace_object_storage::put_raw(
                &self.store,
                &update.stream_id,
                &update.hash_kind,
                &object.declared_oid,
                &object.payload,
            )
            .await?;
        }
        let mut required = update.object_oids.clone();
        required.push(update.new_tip.clone());
        for commit in &update.commits {
            required.push(commit.oid.clone());
        }
        crate::ceres::traces::ingest::require_durable_oids(
            &self.store,
            &update.stream_id,
            &update.hash_kind,
            &required,
        )
        .await?;
        trace_stream_storage::publish_ref(db, update, authority).await
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
                trace_stream_storage::{CommitParentList, ExpectedOldTip, TRACES_REF},
            },
            tests::test_db_connection,
        },
    };

    fn prepared_authority<'a>(
        scope: &'a Scope,
        token: &'a crate::config::PushTokenConfig,
    ) -> PublishAuthority<'a> {
        PublishAuthority {
            caller: crate::ceres::traces::auth::TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token,
            },
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            authorized_path: &scope.path,
            expected_generation: 1,
            policy: None,
        }
    }

    #[tokio::test]
    async fn prepared_publication_atomic_real_local_both_hashes_retry_and_closure() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::publication::tests::{Fixture, local_store};
        let (_database, db) = open_db().await;
        let (_backend, store) = local_store().await;
        let service = TracePublishService::new(store.clone());
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let scope = stream_with_hash(&db, fixture.hash_kind()).await;
            let token = token(&scope);
            let authority = prepared_authority(&scope, &token);
            let (root, _) = fixture.capture(br#"{"schema_version":2}"#);
            let first = fixture.commit(&root, None);
            let extra = fixture.object("blob", b"extra unpublished pack object");
            let key = Uuid::new_v4().to_string();
            let prepared = fixture
                .prepare(&db, &store, &scope.stream_id, &first, &key, None)
                .await
                .unwrap();
            let published = service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .unwrap();
            assert_eq!(published.new_tip, first);
            assert_eq!(
                published.schema,
                trace_stream_storage::SchemaDisposition::Supported
            );
            assert!(!published.idempotent);
            assert!(
                service
                    .publish_prepared(&db, &prepared, &authority)
                    .await
                    .unwrap()
                    .idempotent
            );
            let replay = fixture
                .prepare(&db, &store, &scope.stream_id, &first, &key, None)
                .await
                .unwrap();
            assert!(replay.commits().is_empty());
            assert!(
                service
                    .publish_prepared(&db, &replay, &authority)
                    .await
                    .unwrap()
                    .idempotent
            );
            assert!(
                trace_object::Entity::find()
                    .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                    .filter(trace_object::Column::Oid.eq(&extra))
                    .one(&db)
                    .await
                    .unwrap()
                    .is_none()
            );
            let second = fixture.commit(&root, Some(&first));
            let prepared = fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &second,
                    &Uuid::new_v4().to_string(),
                    Some(std::slice::from_ref(&second)),
                )
                .await
                .unwrap();
            assert_eq!(prepared.update().object_oids, std::slice::from_ref(&second));
            assert_eq!(prepared.commits()[0].parents(), [first.as_str()]);
            service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .unwrap();
            assert_eq!(
                trace_stream::Entity::find_by_id(&scope.stream_id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap()
                    .tip_oid,
                Some(second)
            );
            assert_eq!(
                crate::callisto::trace_ref_update::Entity::find()
                    .filter(
                        crate::callisto::trace_ref_update::Column::StreamId.eq(&scope.stream_id)
                    )
                    .count(&db)
                    .await
                    .unwrap(),
                2
            );
        }
    }

    #[tokio::test]
    async fn prepared_publication_final_list_defense_and_intermediate_facts() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::publication::tests::{Fixture, local_store};
        let (_database, db) = open_db().await;
        let (_backend, store) = local_store().await;
        let service = TracePublishService::new(store.clone());
        let scope = stream(&db).await;
        let token = token(&scope);
        let authority = prepared_authority(&scope, &token);
        let mut fixture = Fixture::new(HashKind::Sha1);
        let (root, leaf) = fixture.capture(br#"{"schema_version":2}"#);
        let first = fixture.commit(&root, None);
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &first,
                "prepared-first",
                None,
            )
            .await
            .unwrap();
        service
            .publish_prepared(&db, &prepared, &authority)
            .await
            .unwrap();
        let (changed_root, changed_leaf) =
            fixture.capture(br#"{"schema_version":2,"changed":true}"#);
        let changed = fixture.commit(&changed_root, Some(&first));
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &changed,
                "changed-final",
                None,
            )
            .await
            .unwrap();
        assert!(
            service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .is_err()
        );
        assert_eq!(
            trace_stream::Entity::find_by_id(&scope.stream_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .tip_oid
                .as_deref(),
            Some(first.as_str())
        );
        assert!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                .filter(trace_object::Column::Oid.eq(&changed))
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        let empty = fixture.tree(&[]);
        let deleted = fixture.commit(&empty, Some(&first));
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &deleted,
                "deleted-final",
                None,
            )
            .await
            .unwrap();
        assert!(
            service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .is_err()
        );
        let restored = fixture.commit(&root, Some(&changed));
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &restored,
                "restored-final",
                None,
            )
            .await
            .unwrap();
        assert_eq!(prepared.commits().len(), 2);
        assert_eq!(
            prepared
                .checkpoints(prepared.commits()[0].tree())
                .unwrap()
                .get("abcd")
                .unwrap()
                .unwrap()
                .original_tree,
            changed_leaf
        );
        assert_eq!(
            prepared
                .final_checkpoints()
                .unwrap()
                .get("abcd")
                .unwrap()
                .unwrap()
                .original_tree,
            leaf
        );
        // This is only MTR-95's final-list defense. MTR-101 must reject
        // the intermediate mutation before any future wire publication.
        service
            .publish_prepared(&db, &prepared, &authority)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prepared_publication_rechecks_expected_old_and_live_authority() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::publication::{
            self, PublicationProposal,
            tests::{Fixture, local_store},
        };
        let (_database, db) = open_db().await;
        let (_backend, store) = local_store().await;
        let service = TracePublishService::new(store.clone());
        let scope = stream(&db).await;
        let token = token(&scope);
        let authority = prepared_authority(&scope, &token);
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let first = fixture.commit(&tree, None);
        let prepared = publication::prepare(
            &db,
            &super::super::trace_object_service::TraceObjectService::new(store.clone()),
            fixture.decoded(None).await,
            PublicationProposal {
                stream_id: scope.stream_id.clone(),
                expected_old_tip: ExpectedOldTip::At("a".repeat(40)),
                new_tip: first.clone(),
                ref_names: vec![TRACES_REF.into()],
                delete: false,
                idempotency_key: "wrong-expected-old".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .is_err()
        );
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &first,
                "retired-before-publish",
                None,
            )
            .await
            .unwrap();
        crate::callisto::trace_credential_binding::Entity::update_many()
            .col_expr(
                crate::callisto::trace_credential_binding::Column::RetiredAt,
                sea_orm::sea_query::Expr::value(chrono::Utc::now().fixed_offset()),
            )
            .filter(
                crate::callisto::trace_credential_binding::Column::CredentialId
                    .eq(&scope.credential_id),
            )
            .exec(&db)
            .await
            .unwrap();
        assert!(
            service
                .publish_prepared(&db, &prepared, &authority)
                .await
                .is_err()
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            crate::callisto::trace_ref_update::Entity::find()
                .filter(crate::callisto::trace_ref_update::Column::StreamId.eq(&scope.stream_id))
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert!(
            trace_stream::Entity::find_by_id(&scope.stream_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .tip_oid
                .is_none()
        );
    }

    #[tokio::test]
    async fn prepared_publication_first_scope_preserves_atomic_target_binding() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::{
            auth::TracesCaller,
            publication::tests::{Fixture, local_store},
        };
        let (_database, db) = open_db().await;
        let (_backend, store) = local_store().await;
        let mono = mono(&db);
        seed_root(&mono, &["proj", "other"]).await;
        let config = crate::config::MonoConfig::default();
        let service = TracePublishService::new(store.clone());
        let scope = first_scope(&db).await;
        let token = crate::config::PushTokenConfig {
            name: scope.credential_id.clone(),
            token: "synthetic-secret".into(),
            paths: Some(vec!["/proj".into(), "/other".into()]),
        };
        let authority = PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &scope.principal_id,
            target_id: "",
            authorized_path: "/proj",
            expected_generation: 1,
            policy: None,
        };
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: "",
            source_repo_id: "source-opaque",
            epoch_id: &scope.epoch_id,
            hash_kind: "sha1",
        };
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let tip = fixture.commit(&tree, None);
        let prepared = fixture
            .prepare(
                &db,
                &store,
                &scope.stream_id,
                &tip,
                "prepared-first-scope",
                None,
            )
            .await
            .unwrap();
        let request = PreparedScopeRequest {
            path: "/proj",
            facts: &facts,
            prepared: &prepared,
            authority: &authority,
        };
        service
            .publish_prepared_first_scope(&config, &db, &mono, &request)
            .await
            .unwrap();
        assert_eq!(
            trace_target::Entity::find()
                .filter(trace_target::Column::ProjectPath.eq("/proj"))
                .count(&db)
                .await
                .unwrap(),
            1
        );
        let other_id = Uuid::new_v4().to_string();
        let other_epoch = Uuid::new_v4().to_string();
        let facts = ScopeFacts {
            stream_id: &other_id,
            epoch_id: &other_epoch,
            ..facts
        };
        let stale = PublishAuthority {
            authorized_path: "/other",
            expected_generation: 99,
            ..authority
        };
        let prepared = fixture
            .prepare(&db, &store, &other_id, &tip, "prepared-stale-binding", None)
            .await
            .unwrap();
        let request = PreparedScopeRequest {
            path: "/other",
            facts: &facts,
            prepared: &prepared,
            authority: &stale,
        };
        assert!(
            service
                .publish_prepared_first_scope(&config, &db, &mono, &request)
                .await
                .is_err()
        );
        assert_eq!(
            trace_target::Entity::find()
                .filter(trace_target::Column::ProjectPath.eq("/other"))
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert!(
            trace_stream::Entity::find_by_id(&other_id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&other_id))
                .count(&db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn prepared_publication_rejects_other_stream_invisible_and_corrupt_old_objects() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::publication::tests::{Fixture, local_store};
        let (_database, db) = open_db().await;
        let (_backend, store) = local_store().await;
        let scope = stream(&db).await;
        let other = stream(&db).await;
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let first = fixture.commit(&tree, None);
        let second = fixture.commit(&tree, Some(&first));
        let raw = &fixture
            .objects
            .iter()
            .find(|(oid, _, _)| oid == &first)
            .unwrap()
            .2;
        trace_object_storage::put_raw(&store, &scope.stream_id, "sha1", &first, raw)
            .await
            .unwrap();
        let membership = |stream_id: &str, visible| trace_object::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.into()),
            hash_kind: Set("sha1".into()),
            oid: Set(first.clone()),
            visible: Set(visible),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        };
        membership(&other.stream_id, true)
            .insert(&db)
            .await
            .unwrap();
        let included = [tree.clone(), second.clone()];
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &second,
                    "other-stream",
                    Some(&included)
                )
                .await
                .is_err()
        );
        let row = membership(&scope.stream_id, false)
            .insert(&db)
            .await
            .unwrap();
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &second,
                    "invisible",
                    Some(&included)
                )
                .await
                .is_err()
        );
        trace_object::Entity::update_many()
            .col_expr(
                trace_object::Column::Visible,
                sea_orm::sea_query::Expr::value(true),
            )
            .filter(trace_object::Column::Id.eq(&row.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &second,
                    "qualified",
                    Some(&included)
                )
                .await
                .is_ok()
        );
        // Deliberately corrupt the fixture's backend outside the immutable API;
        // published membership alone must not authenticate these bytes.
        let key = trace_object_storage::object_key(&scope.stream_id, "sha1", &first).unwrap();
        store.inner.delete(&key).await.unwrap();
        trace_object_storage::put_raw(
            &store,
            &scope.stream_id,
            "sha1",
            &first,
            b"corrupt stored bytes",
        )
        .await
        .unwrap();
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &second,
                    "corrupt",
                    Some(&included)
                )
                .await
                .is_err()
        );
        // The old DTO primitive could publish a commit without its tree.
        // A visible row therefore is not a historical transitive-closure seal.
        let missing_tree = "f".repeat(40);
        let legacy = fixture.commit(&missing_tree, None);
        let legacy_bytes = &fixture
            .objects
            .iter()
            .find(|(oid, _, _)| oid == &legacy)
            .unwrap()
            .2;
        let legacy_request = request(
            &scope.stream_id,
            &legacy,
            Vec::new(),
            "legacy-open-closure",
            legacy_bytes,
        );
        let service = TracePublishService::new(store.clone());
        publish(&service, &db, &scope, &legacy_request)
            .await
            .unwrap();
        let child = fixture.commit(&tree, Some(&legacy));
        assert!(
            fixture
                .prepare(
                    &db,
                    &store,
                    &scope.stream_id,
                    &child,
                    "unsealed-old-closure",
                    Some(&[tree.clone(), child.clone()])
                )
                .await
                .is_err(),
            "a qualified old commit still needs its referenced tree"
        );
    }

    #[tokio::test]
    async fn prepared_publication_database_failure_is_static() {
        use git_internal::hash::HashKind;
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

        use crate::ceres::traces::publication::tests::{Fixture, local_store};
        let (_database, db) = open_db().await;
        let (_observer_directory, observer) = open_db().await;
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_database() AS name",
            ))
            .await
            .unwrap()
            .unwrap();
        let database_name: String = row.try_get("", "name").unwrap();
        let (_backend, store) = local_store().await;
        let scope = stream(&db).await;
        let token = token(&scope);
        let authority = prepared_authority(&scope, &token);
        let mut fixture = Fixture::new(HashKind::Sha1);
        let tree = fixture.tree(&[]);
        let tip = fixture.commit(&tree, None);
        let prepared = fixture
            .prepare(&db, &store, &scope.stream_id, &tip, "closed-db", None)
            .await
            .unwrap();
        db.clone().close().await.unwrap();
        let result = TracePublishService::new(store)
            .publish_prepared(&db, &prepared, &authority)
            .await;
        assert!(matches!(
            result,
            Err(MegaError::Traces(
                crate::common::errors::TracesError::QueryTimeout
            ))
        ));
        drop(db);
        assert!(
            observer
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT 1 FROM pg_database WHERE datname = $1",
                    [database_name.into()],
                ))
                .await
                .unwrap()
                .is_none(),
            "closed test pool's final owner drops its actual database"
        );
    }

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
        stream_with_hash(db, "sha1").await
    }

    async fn stream_with_hash(db: &sea_orm::DatabaseConnection, hash_kind: &str) -> Scope {
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
            hash_kind: Set(hash_kind.to_owned()),
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

    fn mono(conn: &sea_orm::DatabaseConnection) -> MonoStorage {
        use crate::jupiter::storage::base_storage::StorageConnector;
        MonoStorage {
            base: crate::jupiter::storage::base_storage::BaseStorage::new(std::sync::Arc::new(
                conn.clone(),
            )),
        }
    }

    async fn seed_root(mono: &MonoStorage, directories: &[&str]) {
        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                ObjectTrait,
                commit::Commit,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
        };

        use crate::{callisto::mega_refs, common::utils::MEGA_BRANCH_NAME};
        let leaf = Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .expect("empty leaf tree");
        let root = Tree::from_tree_items(
            directories
                .iter()
                .map(|name| TreeItem {
                    mode: TreeItemMode::Tree,
                    id: leaf.id,
                    name: (*name).into(),
                })
                .collect(),
        )
        .expect("root");
        let commit = Commit::from_tree_id(root.id, vec![], "first scope root");
        mono.save_mega_trees(vec![leaf, root.clone()], commit.id, None)
            .await
            .expect("persist trees");
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .expect("persist commit");
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                commit.id.to_string(),
                root.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .expect("root ref");
    }

    struct FirstScope {
        principal_id: String,
        credential_id: String,
        stream_id: String,
        epoch_id: String,
    }

    async fn first_scope(db: &sea_orm::DatabaseConnection) -> FirstScope {
        let principal_id = Uuid::new_v4().to_string();
        let credential_id = "publish-first-scope".to_owned();
        let now = chrono::Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
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
        FirstScope {
            principal_id,
            credential_id,
            stream_id: Uuid::new_v4().to_string(),
            epoch_id: Uuid::new_v4().to_string(),
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
                expected_old_tip: ExpectedOldTip::Unspecified,
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

    #[tokio::test]
    async fn quarantine_publication_rejects_a_missing_base_before_the_ref() {
        use crate::ceres::traces::ingest::{LooseObject, UnpackBudget};

        let (_dir, db) = open_db().await;
        let service = TracePublishService::mock();
        let scope = stream(&db).await;
        let blob = b"quarantine-blob";
        let blob_oid = git_internal::hash::ObjectHash::from_type_and_data_for_kind(
            git_internal::hash::HashKind::Sha1,
            git_internal::internal::object::types::ObjectType::Blob,
            blob,
        )
        .expect("blob oid")
        .to_string();
        let commit = format!("tree {blob_oid}\n\nmessage\n");
        let commit_oid = git_internal::hash::ObjectHash::from_type_and_data_for_kind(
            git_internal::hash::HashKind::Sha1,
            git_internal::internal::object::types::ObjectType::Commit,
            commit.as_bytes(),
        )
        .expect("commit oid")
        .to_string();
        let open = vec![LooseObject {
            object_type: "commit".to_owned(),
            declared_oid: commit_oid.clone(),
            payload: commit.as_bytes().to_vec(),
        }];
        let ingest_id = "33333333-3333-4333-8333-333333333333";
        let token = token(&scope);
        let authority = crate::ceres::traces::auth::PublishAuthority {
            caller: crate::ceres::traces::auth::TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            authorized_path: &scope.path,
            expected_generation: 1,
            policy: None,
        };
        let absent = "cccccccccccccccccccccccccccccccccccccccc";
        assert!(
            service
                .publish_quarantine(
                    &db,
                    ingest_id,
                    UnpackBudget {
                        compressed_bytes: 1,
                        expanded_bytes: 0,
                    },
                    &[],
                    &PublishRef {
                        stream_id: scope.stream_id.clone(),
                        expected_old_tip: ExpectedOldTip::Unspecified,
                        ref_names: vec![TRACES_REF.to_owned()],
                        delete: false,
                        new_tip: absent.to_owned(),
                        commits: vec![CommitParentList {
                            oid: absent.to_owned(),
                            parents: Vec::new(),
                        }],
                        checkpoints: Vec::new(),
                        object_oids: vec![absent.to_owned()],
                        hash_kind: "sha1".to_owned(),
                        idempotency_key: "missing-bytes".to_owned(),
                        schema_id: "manifest-v1".to_owned(),
                    },
                    &authority,
                )
                .await
                .is_err(),
            "membership requires durable bytes on this stream"
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                .count(&db)
                .await
                .expect("membership before bytes"),
            0
        );
        assert!(
            service
                .publish_quarantine(
                    &db,
                    ingest_id,
                    UnpackBudget {
                        compressed_bytes: commit.len() as u64,
                        expanded_bytes: 0,
                    },
                    &open,
                    &PublishRef {
                        stream_id: scope.stream_id.clone(),
                        expected_old_tip: ExpectedOldTip::Unspecified,
                        ref_names: vec![TRACES_REF.to_owned()],
                        delete: false,
                        new_tip: commit_oid.clone(),
                        commits: vec![CommitParentList {
                            oid: commit_oid.clone(),
                            parents: Vec::new(),
                        }],
                        checkpoints: Vec::new(),
                        object_oids: vec![commit_oid.clone()],
                        hash_kind: "sha1".to_owned(),
                        idempotency_key: "open-closure".to_owned(),
                        schema_id: "manifest-v1".to_owned(),
                    },
                    &authority,
                )
                .await
                .is_err()
        );
        assert_eq!(
            trace_object_storage::get_raw(&service.store, &scope.stream_id, "sha1", &commit_oid)
                .await
                .expect("durable"),
            None
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                .count(&db)
                .await
                .expect("membership"),
            0
        );

        let closed = vec![
            LooseObject {
                object_type: "blob".to_owned(),
                declared_oid: blob_oid.clone(),
                payload: blob.to_vec(),
            },
            LooseObject {
                object_type: "commit".to_owned(),
                declared_oid: commit_oid.clone(),
                payload: commit.into_bytes(),
            },
        ];
        service
            .publish_quarantine(
                &db,
                ingest_id,
                UnpackBudget {
                    compressed_bytes: 64,
                    expanded_bytes: 0,
                },
                &closed,
                &PublishRef {
                    stream_id: scope.stream_id.clone(),
                    expected_old_tip: ExpectedOldTip::Unspecified,
                    ref_names: vec![TRACES_REF.to_owned()],
                    delete: false,
                    new_tip: commit_oid.clone(),
                    commits: vec![CommitParentList {
                        oid: commit_oid.clone(),
                        parents: Vec::new(),
                    }],
                    checkpoints: Vec::new(),
                    object_oids: vec![blob_oid.clone(), commit_oid.clone()],
                    hash_kind: "sha1".to_owned(),
                    idempotency_key: "closed-quarantine".to_owned(),
                    schema_id: "manifest-v1".to_owned(),
                },
                &authority,
            )
            .await
            .expect("closed publication");
        assert_eq!(
            trace_object_storage::get_staging(&service.store, ingest_id, "sha1", &blob_oid)
                .await
                .expect("staging"),
            Some(blob.to_vec())
        );
        assert_eq!(
            trace_object_storage::get_raw(&service.store, &scope.stream_id, "sha1", &blob_oid)
                .await
                .expect("promoted"),
            Some(blob.to_vec())
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&scope.stream_id))
                .filter(trace_object::Column::Visible.eq(true))
                .count(&db)
                .await
                .expect("visible"),
            2
        );
    }
    #[tokio::test]
    async fn first_scope_publish_binds_target_and_rolls_back_rejections() {
        use crate::{
            ceres::traces::auth::{PublishAuthority, TracesCaller},
            jupiter::storage::trace_stream_storage::ScopeFacts,
        };
        let (_dir, db) = open_db().await;
        let mono = mono(&db);
        seed_root(&mono, &["proj", "other", "third", "third-party"]).await;
        let config = crate::config::MonoConfig::default();
        let storage_dir = tempfile::TempDir::new().expect("storage dir");
        let storage_config = crate::orbit_api::factory::ObjectStorageConfig {
            storage_type: crate::orbit_api::factory::ObjectStorageBackend::Local,
            local: crate::orbit_api::factory::LocalConfig {
                root_dir: storage_dir.path().to_string_lossy().to_string(),
            },
            ..Default::default()
        };
        let store = crate::orbit::factory::ObjectStorageFactory::build(&storage_config)
            .await
            .expect("local object storage");
        let scope = first_scope(&db).await;
        let token = crate::config::PushTokenConfig {
            name: scope.credential_id.clone(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec![
                "/proj".to_owned(),
                "/other".to_owned(),
                "/third".to_owned(),
                "/third-party".to_owned(),
            ]),
        };
        let authority = PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &scope.principal_id,
            target_id: "",
            authorized_path: "/proj",
            expected_generation: 1,
            policy: None,
        };
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: "",
            source_repo_id: "source-opaque",
            epoch_id: &scope.epoch_id,
            hash_kind: "sha1",
        };
        let service = TracePublishService::new(store);
        let first_request = request(&scope.stream_id, OID_A, Vec::new(), "first", b"a");
        let published = service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/proj",
                    facts: &facts,
                    request: &first_request,
                    authority: &authority,
                },
            )
            .await
            .expect("first scope publish");
        assert_eq!(published.stream_id, scope.stream_id);
        let target_count = trace_target::Entity::find()
            .filter(trace_target::Column::ProjectPath.eq("/proj"))
            .filter(trace_target::Column::Active.eq(true))
            .count(&db)
            .await
            .expect("target count");
        assert_eq!(target_count, 1);
        let stream_count = trace_stream::Entity::find()
            .filter(trace_stream::Column::Id.eq(&scope.stream_id))
            .count(&db)
            .await
            .expect("stream count");
        assert_eq!(stream_count, 1);

        // The same shape of first request succeeds with the authorized
        // generation on another path.
        let third_stream_id = Uuid::new_v4().to_string();
        let third_epoch_id = Uuid::new_v4().to_string();
        let third_facts = ScopeFacts {
            stream_id: &third_stream_id,
            epoch_id: &third_epoch_id,
            ..facts
        };
        let third_request = request(&third_stream_id, OID_B, Vec::new(), "third", b"b");
        let third_authority = PublishAuthority {
            authorized_path: "/third",
            ..authority
        };
        service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/third",
                    facts: &third_facts,
                    request: &third_request,
                    authority: &third_authority,
                },
            )
            .await
            .expect("authorized generation succeeds");

        // A failure after the target binding still rolls the whole transaction
        // back: the only difference from the successful request is the stale
        // generation, so the recheck is the reason it fails.
        let other_stream_id = Uuid::new_v4().to_string();
        let other_epoch_id = Uuid::new_v4().to_string();
        let other_facts = ScopeFacts {
            stream_id: &other_stream_id,
            epoch_id: &other_epoch_id,
            ..facts
        };
        let other_request = request(&other_stream_id, OID_B, Vec::new(), "other", b"b");
        let stale_generation = PublishAuthority {
            authorized_path: "/other",
            expected_generation: 99,
            ..authority
        };
        assert!(
            service
                .publish_first_scope(
                    &config,
                    &db,
                    &mono,
                    &FirstScopeRequest {
                        path: "/other",
                        facts: &other_facts,
                        request: &other_request,
                        authority: &stale_generation,
                    },
                )
                .await
                .is_err()
        );
        let other_targets = trace_target::Entity::find()
            .filter(trace_target::Column::ProjectPath.eq("/other"))
            .count(&db)
            .await
            .expect("other targets");
        assert_eq!(other_targets, 0);
        let other_streams = trace_stream::Entity::find()
            .filter(trace_stream::Column::Id.eq(&other_stream_id))
            .count(&db)
            .await
            .expect("other streams");
        assert_eq!(other_streams, 0);
        let raw = trace_object_storage::get_raw(&service.store, &other_stream_id, "sha1", OID_B)
            .await
            .expect("raw lookup");
        assert!(
            raw.is_some(),
            "raw bytes stay durable after a rejected publication"
        );
        let membership = trace_object::Entity::find()
            .filter(trace_object::Column::StreamId.eq(&other_stream_id))
            .count(&db)
            .await
            .expect("membership");
        assert_eq!(membership, 0, "rejected publication writes no membership");

        // An authorization that named a target cannot silently apply to a new
        // binding at the same path.
        let replacement = trace_target::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            target_kind: Set("native".to_owned()),
            project_path: Set("/other".to_owned()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(chrono::Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("replacement target");
        let proj_target = trace_target::Entity::find()
            .filter(trace_target::Column::ProjectPath.eq("/proj"))
            .filter(trace_target::Column::Active.eq(true))
            .one(&db)
            .await
            .expect("proj target lookup")
            .expect("proj target");
        let named_target = PublishAuthority {
            target_id: &proj_target.id,
            authorized_path: "/other",
            ..authority
        };
        let error = service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/other",
                    facts: &other_facts,
                    request: &other_request,
                    authority: &named_target,
                },
            )
            .await
            .expect_err("a stale authorized target id must not adopt the replacement");
        assert!(
            matches!(
                error,
                crate::common::errors::MegaError::Traces(
                    crate::common::errors::TracesError::UnknownScope
                )
            ),
            "identity rejection keeps the typed traces contract: {error:?}"
        );
        assert_eq!(
            trace_target::Entity::find()
                .filter(trace_target::Column::Id.eq(&replacement.id))
                .count(&db)
                .await
                .expect("replacement lookup"),
            1
        );

        // A preflight rejection never opens a transaction or binds a target.
        let mut bad = request(&other_stream_id, OID_B, vec![OID_A.to_owned()], "bad", b"b");
        bad.update.ref_names = vec!["refs/heads/main".to_owned()];
        assert!(
            service
                .publish_first_scope(
                    &config,
                    &db,
                    &mono,
                    &FirstScopeRequest {
                        path: "/other",
                        facts: &other_facts,
                        request: &bad,
                        authority: &authority,
                    },
                )
                .await
                .is_err()
        );

        // A missing directory never binds a target and keeps the typed contract.
        let missing = PublishAuthority {
            authorized_path: "/missing",
            ..authority
        };
        let error = service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/missing",
                    facts: &facts,
                    request: &first_request,
                    authority: &missing,
                },
            )
            .await
            .expect_err("missing directory is rejected");
        assert!(matches!(
            error,
            crate::common::errors::MegaError::Traces(
                crate::common::errors::TracesError::UnknownScope
            )
        ));

        // A non-canonical authorized path is rejected with the same typed error.
        let non_canonical = PublishAuthority {
            authorized_path: "proj",
            ..authority
        };
        let error = service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/proj",
                    facts: &facts,
                    request: &first_request,
                    authority: &non_canonical,
                },
            )
            .await
            .expect_err("non-canonical authorization is rejected");
        assert!(matches!(
            error,
            crate::common::errors::MegaError::Traces(
                crate::common::errors::TracesError::UnknownScope
            )
        ));

        // An ImportRepo namespace path never binds a native target, even when
        // the directory exists and no target row is present yet.
        let import_path = PublishAuthority {
            authorized_path: "/third-party",
            ..authority
        };
        let import_facts = ScopeFacts {
            stream_id: &other_stream_id,
            ..other_facts
        };
        let import_request = request(&other_stream_id, OID_B, Vec::new(), "import", b"b");
        let error = service
            .publish_first_scope(
                &config,
                &db,
                &mono,
                &FirstScopeRequest {
                    path: "/third-party",
                    facts: &import_facts,
                    request: &import_request,
                    authority: &import_path,
                },
            )
            .await
            .expect_err("import namespace is not native");
        assert!(matches!(
            error,
            crate::common::errors::MegaError::Traces(
                crate::common::errors::TracesError::UnknownScope
            )
        ));
        assert_eq!(
            trace_target::Entity::find()
                .filter(trace_target::Column::ProjectPath.eq("/third-party"))
                .count(&db)
                .await
                .expect("import namespace targets"),
            0
        );
        let missing_targets = trace_target::Entity::find()
            .filter(trace_target::Column::ProjectPath.eq("/missing"))
            .count(&db)
            .await
            .expect("missing targets");
        assert_eq!(missing_targets, 0);
    }
}
