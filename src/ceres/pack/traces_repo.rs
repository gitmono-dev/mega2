use std::collections::HashMap;

use async_trait::async_trait;
use git_internal::{
    errors::GitError,
    hash::HashKind,
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::tree::Tree,
        pack::entry::Entry,
    },
};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    callisto::{trace_stream, trace_target},
    ceres::{
        pack::RepoHandler,
        protocol::{
            ServiceType,
            import_refs::{RefCommand, Refs},
        },
        traces::auth::{
            TracesAction, TracesAuthDecision, TracesAuthRequest, TracesCaller, authorize_traces,
        },
    },
    common::errors::{MegaError, TracesError},
    config::TracesConfig,
    contract::git_protocol::path::{TracesLocator, TracesObjectFormat},
    jupiter::storage::trace_target_storage::strict_canonical_project_path,
    orbit_api::object_storage::MultiObjectByteStream,
};

const CODE_WRITE_REFUSED: &str = "trace repository does not write code objects";

/// Request-local advertisement data obtained through the traces authorization
/// gate. This is not authority for a later fetch or publication.
#[derive(Debug)]
pub(crate) struct TraceDiscovery {
    server_id: Uuid,
    scope_id: Option<Uuid>,
    hash_kind: HashKind,
    service_type: ServiceType,
}

impl TraceDiscovery {
    pub(crate) fn server_id(&self) -> Uuid {
        self.server_id
    }

    pub(crate) fn scope_id(&self) -> Option<Uuid> {
        self.scope_id
    }

    pub(crate) fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    pub(crate) fn service_type(&self) -> ServiceType {
        self.service_type
    }
}

fn discovery_uuid(raw: &str) -> Result<Uuid, MegaError> {
    let id = Uuid::parse_str(raw).map_err(|_| TracesError::UnknownScope)?;
    if id.to_string() != raw {
        return Err(TracesError::UnknownScope.into());
    }
    Ok(id)
}

async fn discovery_principal(
    db: &DatabaseConnection,
    caller: TracesCaller<'_>,
    action: TracesAction<'_>,
    target_path: &str,
) -> Result<String, MegaError> {
    match authorize_traces(
        db,
        &TracesAuthRequest {
            caller,
            action,
            canonical_target_path: target_path,
            commit_author: None,
        },
    )
    .await?
    {
        TracesAuthDecision::Allow { principal_id } => Ok(principal_id),
        TracesAuthDecision::Unauthenticated => Err(TracesError::Unauthenticated.into()),
        TracesAuthDecision::NotFound => Err(TracesError::UnknownScope.into()),
    }
}

pub(crate) async fn resolve_trace_discovery(
    db: &DatabaseConnection,
    config: &TracesConfig,
    locator: &TracesLocator,
    caller: TracesCaller<'_>,
    service_type: ServiceType,
) -> Result<TraceDiscovery, MegaError> {
    if !config.enabled {
        return Err(TracesError::UnknownScope.into());
    }
    match caller {
        TracesCaller::Anonymous => return Err(TracesError::Unauthenticated.into()),
        TracesCaller::StaticPushToken {
            credential_id: "", ..
        } => {
            return Err(TracesError::Unauthenticated.into());
        }
        TracesCaller::MonoAccount {
            credential_id,
            username,
            ..
        } if credential_id.is_empty() || username.is_empty() => {
            return Err(TracesError::Unauthenticated.into());
        }
        _ => {}
    }
    let server_id = config
        .deployment_server_id()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .filter(|id| !id.is_nil())
        .ok_or(TracesError::UnknownScope)?;
    let (scope_id, hash_kind) = match locator {
        TracesLocator::ByPath {
            target_path,
            source_id,
            epoch,
            hash_kind,
        } => {
            if target_path.contains(['\0', '\\'])
                || strict_canonical_project_path(target_path).is_err()
                || source_id.is_empty()
                || source_id.contains('\0')
            {
                return Err(TracesError::UnknownScope.into());
            }
            discovery_uuid(epoch)?;
            let action = match service_type {
                ServiceType::UploadPack => TracesAction::Read,
                ServiceType::ReceivePack => TracesAction::Create,
            };
            // Resolve permissions before looking for either target or stream.
            let principal_id = discovery_principal(db, caller, action, target_path).await?;
            let target = trace_target::Entity::find()
                .filter(trace_target::Column::ProjectPath.eq(target_path))
                .filter(trace_target::Column::Active.eq(true))
                .one(db)
                .await?;
            let kind = match hash_kind {
                TracesObjectFormat::Sha1 => HashKind::Sha1,
                TracesObjectFormat::Sha256 => HashKind::Sha256,
            };
            let stream = if let Some(target) = target {
                trace_stream::Entity::find()
                    .filter(trace_stream::Column::PrincipalId.eq(principal_id))
                    .filter(trace_stream::Column::TargetId.eq(target.id))
                    .filter(trace_stream::Column::SourceRepoId.eq(source_id))
                    .filter(trace_stream::Column::EpochId.eq(epoch))
                    .one(db)
                    .await?
            } else {
                None
            };
            let scope_id = match stream {
                Some(stream) => {
                    if stream.hash_kind != kind.as_str() {
                        return Err(TracesError::UnknownScope.into());
                    }
                    Some(discovery_uuid(&stream.id)?)
                }
                None => None,
            };
            (scope_id, kind)
        }
        TracesLocator::ByScope { stream_id } => {
            discovery_uuid(stream_id)?;
            let stream = trace_stream::Entity::find_by_id(stream_id)
                .one(db)
                .await?
                .ok_or(TracesError::UnknownScope)?;
            let target = trace_target::Entity::find_by_id(&stream.target_id)
                .filter(trace_target::Column::Active.eq(true))
                .one(db)
                .await?
                .ok_or(TracesError::UnknownScope)?;
            if target.project_path.contains(['\0', '\\'])
                || strict_canonical_project_path(&target.project_path).is_err()
            {
                return Err(TracesError::UnknownScope.into());
            }
            let action = match service_type {
                ServiceType::UploadPack => TracesAction::Read,
                ServiceType::ReceivePack => TracesAction::Update {
                    owner_principal_id: &stream.principal_id,
                },
            };
            discovery_principal(db, caller, action, &target.project_path).await?;
            let kind = match stream.hash_kind.as_str() {
                "sha1" => HashKind::Sha1,
                "sha256" => HashKind::Sha256,
                _ => return Err(TracesError::UnknownScope.into()),
            };
            (Some(discovery_uuid(&stream.id)?), kind)
        }
    };
    Ok(TraceDiscovery {
        server_id,
        scope_id,
        hash_kind,
        service_type,
    })
}

/// Virtual repository selected before Native and ImportRepo.
///
/// Discovery of a legal locator returns an empty ref list. This handler does
/// not create `git_repo` or `.gitkeep`, and it does not call MonoWriteQueue,
/// PushChain, or CL writers.
pub struct TraceRepo {
    locator: TracesLocator,
    hash_kind: HashKind,
}

impl TraceRepo {
    pub fn new(locator: TracesLocator, session_hash_kind: HashKind) -> Self {
        let hash_kind = match &locator {
            TracesLocator::ByPath { hash_kind, .. } => match hash_kind {
                TracesObjectFormat::Sha1 => HashKind::Sha1,
                TracesObjectFormat::Sha256 => HashKind::Sha256,
            },
            TracesLocator::ByScope { .. } => session_hash_kind,
        };
        Self { locator, hash_kind }
    }

    fn zero_head(&self) -> String {
        match &self.locator {
            TracesLocator::ByPath { hash_kind, .. } => hash_kind.zero_id(),
            TracesLocator::ByScope { .. } => "0".repeat(self.hash_kind.hex_len()),
        }
    }
}

fn refused_mega() -> MegaError {
    MegaError::Other(CODE_WRITE_REFUSED.to_owned())
}

fn refused_git() -> GitError {
    GitError::CustomError(CODE_WRITE_REFUSED.to_owned())
}

#[async_trait]
impl RepoHandler for TraceRepo {
    fn is_monorepo(&self) -> bool {
        false
    }

    fn is_trace_repo(&self) -> bool {
        true
    }

    fn object_hash_kind(&self) -> Result<HashKind, MegaError> {
        Ok(self.hash_kind)
    }

    fn bind_tip_after_receive(&self) -> bool {
        false
    }

    async fn refs_with_head_hash(&self) -> Result<(String, Vec<Refs>), MegaError> {
        // A legal but absent scope advertises no refs and does not invent
        // `HEAD -> refs/heads/main`. Capability lines stay with the protocol.
        Ok((self.zero_head(), Vec::new()))
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn save_entry(
        &self,
        _entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn update_pack_id(&self, _temp_pack_id: &str, _pack_id: &str) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn check_entry(&self, _entry: &Entry) -> Result<(), GitError> {
        Err(refused_git())
    }

    async fn full_pack(&self, _want: Vec<String>) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(refused_git())
    }

    async fn incremental_pack(
        &self,
        _want: Vec<String>,
        _have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(refused_git())
    }

    async fn get_trees_by_hashes(&self, _hashes: Vec<String>) -> Result<Vec<Tree>, MegaError> {
        Err(refused_mega())
    }

    async fn get_blobs_by_hashes(
        &self,
        _hashes: Vec<String>,
    ) -> Result<MultiObjectByteStream<'_>, MegaError> {
        Err(refused_mega())
    }

    async fn get_blob_metadata_by_hashes(
        &self,
        _hashes: Vec<String>,
    ) -> Result<HashMap<String, EntryMeta>, MegaError> {
        Err(refused_mega())
    }

    async fn update_refs(&self, _refs: &RefCommand) -> Result<(), GitError> {
        Err(refused_git())
    }

    async fn check_commit_exist(&self, _hash: &str) -> bool {
        false
    }

    async fn check_object_exist(&self, _hash: &str) -> bool {
        false
    }

    async fn check_default_branch(&self) -> bool {
        false
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        Err(refused_mega())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
        QueryOrder, Set,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{
            git_repo, mega_blob, mega_cl, push_queue, trace_credential_binding,
            trace_ingest_attempt, trace_principal, trace_ref_update, trace_stream, trace_target,
        },
        ceres::{
            api_service::{cache::GitObjectCache, state::ProtocolApiState},
            protocol::{ServiceType, SmartSession, TransportProtocol},
        },
        common::utils::is_protocol_zero_id,
        config::{PushTokenConfig, testing::isolated_config},
        contract::{
            git_protocol::path::{TracesLocator, TracesObjectFormat},
            policy::entitystore::SharedEntityStore,
        },
        jupiter::{storage::base_storage::StorageConnector, tests::test_storage_with_config},
    };

    const STREAM_ID: &str = "abcdef01-2345-4678-89ab-cdef01234567";

    pub(crate) struct DiscoveryFixture {
        _temp: TempDir,
        pub(crate) db: DatabaseConnection,
        pub(crate) config: TracesConfig,
        pub(crate) token: PushTokenConfig,
        pub(crate) stream_id: String,
        pub(crate) locator: TracesLocator,
        principal_id: String,
        target_id: String,
    }

    impl DiscoveryFixture {
        pub(crate) async fn new(kind: HashKind, existing: bool) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let db = crate::jupiter::tests::test_db_connection(temp.path()).await;
            crate::jupiter::migration::apply_migrations(&db, true)
                .await
                .unwrap();
            let principal_id = Uuid::new_v4().to_string();
            let target_id = Uuid::new_v4().to_string();
            let stream_id = Uuid::new_v4().to_string();
            trace_principal::ActiveModel {
                id: Set(principal_id.clone()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .unwrap();
            let token = PushTokenConfig {
                name: "discovery-owner".to_owned(),
                token: "synthetic-discovery-secret".to_owned(),
                paths: Some(vec!["/team/app".to_owned()]),
            };
            discovery_binding(&db, &principal_id, "static_push_token", &token.name).await;
            trace_target::ActiveModel {
                id: Set(target_id.clone()),
                target_kind: Set("native".to_owned()),
                project_path: Set("/team/app".to_owned()),
                backing_import_repo_id: Set(None),
                active: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .unwrap();
            let epoch = Uuid::new_v4().to_string();
            let source_id = " legacy/repo id ".to_owned();
            if existing {
                trace_stream::ActiveModel {
                    id: Set(stream_id.clone()),
                    principal_id: Set(principal_id.clone()),
                    target_id: Set(target_id.clone()),
                    source_repo_id: Set(source_id.clone()),
                    epoch_id: Set(epoch.clone()),
                    hash_kind: Set(kind.as_str().to_owned()),
                    tip_oid: Set(None),
                    created_at: Set(Utc::now().fixed_offset()),
                }
                .insert(&db)
                .await
                .unwrap();
            }
            Self {
                _temp: temp,
                db,
                config: TracesConfig {
                    enabled: true,
                    server_id: Some("abcdef01-2345-4678-89ab-cdef01234567".to_owned()),
                    scope: Some("operator-label-not-scope-identity".to_owned()),
                },
                token,
                stream_id,
                locator: TracesLocator::ByPath {
                    target_path: "/team/app".to_owned(),
                    source_id,
                    epoch,
                    hash_kind: match kind {
                        HashKind::Sha1 => TracesObjectFormat::Sha1,
                        HashKind::Sha256 => TracesObjectFormat::Sha256,
                        HashKind::Blake3 => panic!("traces fixture only supports SHA-1/SHA-256"),
                    },
                },
                principal_id,
                target_id,
            }
        }

        pub(crate) fn caller(&self) -> TracesCaller<'_> {
            TracesCaller::StaticPushToken {
                credential_id: &self.token.name,
                token: &self.token,
            }
        }

        pub(crate) async fn metadata(&self, service: ServiceType) -> TraceDiscovery {
            resolve_trace_discovery(
                &self.db,
                &self.config,
                &self.locator,
                self.caller(),
                service,
            )
            .await
            .unwrap()
        }

        fn by_scope(&self) -> TracesLocator {
            TracesLocator::ByScope {
                stream_id: self.stream_id.clone(),
            }
        }

        async fn rows(&self) -> serde_json::Value {
            serde_json::json!([
                trace_target::Entity::find()
                    .order_by_asc(trace_target::Column::Id)
                    .all(&self.db)
                    .await
                    .unwrap(),
                trace_stream::Entity::find()
                    .order_by_asc(trace_stream::Column::Id)
                    .all(&self.db)
                    .await
                    .unwrap(),
                trace_credential_binding::Entity::find()
                    .order_by_asc(trace_credential_binding::Column::Id)
                    .all(&self.db)
                    .await
                    .unwrap(),
                trace_ingest_attempt::Entity::find()
                    .count(&self.db)
                    .await
                    .unwrap(),
                trace_ref_update::Entity::find()
                    .count(&self.db)
                    .await
                    .unwrap()
            ])
        }
    }

    async fn discovery_binding(
        db: &DatabaseConnection,
        principal_id: &str,
        namespace: &str,
        credential_id: &str,
    ) {
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            namespace: Set(namespace.to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn rejected(
        fixture: &DiscoveryFixture,
        locator: &TracesLocator,
        caller: TracesCaller<'_>,
        service: ServiceType,
    ) -> String {
        resolve_trace_discovery(&fixture.db, &fixture.config, locator, caller, service)
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn discovery_capability_existing_scope_identity_and_no_writes() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let f = DiscoveryFixture::new(kind, true).await;
            let before = f.rows().await;
            for service in [ServiceType::UploadPack, ServiceType::ReceivePack] {
                for locator in [&f.locator, &f.by_scope()] {
                    let data =
                        resolve_trace_discovery(&f.db, &f.config, locator, f.caller(), service)
                            .await
                            .unwrap();
                    assert_eq!(data.scope_id().unwrap().to_string(), f.stream_id);
                    assert_eq!(
                        data.server_id().to_string(),
                        f.config.server_id.clone().unwrap()
                    );
                    assert_eq!(data.hash_kind(), kind);
                    assert_eq!(data.service_type(), service);
                }
            }
            assert_eq!(before, f.rows().await);
        }
    }

    #[tokio::test]
    async fn discovery_capability_empty_scope_and_missing_target_are_read_only() {
        let f = DiscoveryFixture::new(HashKind::Sha256, false).await;
        let before = f.rows().await;
        assert_eq!(f.metadata(ServiceType::ReceivePack).await.scope_id(), None);
        assert_eq!(before, f.rows().await);
        trace_target::Entity::delete_many()
            .exec(&f.db)
            .await
            .unwrap();
        let before = f.rows().await;
        assert_eq!(f.metadata(ServiceType::ReceivePack).await.scope_id(), None);
        assert_eq!(
            f.metadata(ServiceType::UploadPack).await.hash_kind(),
            HashKind::Sha256
        );
        assert_eq!(before, f.rows().await);
    }

    #[tokio::test]
    async fn discovery_capability_natural_key_isolation_and_hash_mismatch() {
        let f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        for axis in 0..3 {
            let mut locator = f.locator.clone();
            let TracesLocator::ByPath {
                source_id,
                epoch,
                target_path,
                ..
            } = &mut locator
            else {
                unreachable!()
            };
            match axis {
                0 => *source_id = source_id.trim().to_owned(),
                1 => *epoch = Uuid::new_v4().to_string(),
                _ => *target_path = "/team/other".to_owned(),
            }
            let mut token = f.token.clone();
            token.paths = Some(vec!["/team".to_owned()]);
            let caller = TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            };
            let data = resolve_trace_discovery(
                &f.db,
                &f.config,
                &locator,
                caller,
                ServiceType::ReceivePack,
            )
            .await
            .unwrap();
            assert_eq!(data.scope_id(), None);
        }
        let mut locator = f.locator.clone();
        if let TracesLocator::ByPath { hash_kind, .. } = &mut locator {
            *hash_kind = TracesObjectFormat::Sha256;
        }
        assert_eq!(
            rejected(&f, &locator, f.caller(), ServiceType::UploadPack).await,
            TracesError::UnknownScope.to_string()
        );
    }

    #[tokio::test]
    async fn discovery_capability_unauthorized_existence_is_indistinguishable() {
        let f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        let mut absent = f.locator.clone();
        if let TracesLocator::ByPath { source_id, .. } = &mut absent {
            *source_id = "absent".to_owned();
        }
        let missing = TracesLocator::ByScope {
            stream_id: Uuid::new_v4().to_string(),
        };
        let locators = [&f.locator, &absent, &f.by_scope(), &missing];
        let before = f.rows().await;
        let mut denied = f.token.clone();
        denied.paths = Some(vec!["/forbidden".to_owned()]);
        let mut wrong_name = f.token.clone();
        wrong_name.name = "wrong-name".to_owned();
        for service in [ServiceType::UploadPack, ServiceType::ReceivePack] {
            for locator in locators {
                for token in [&denied, &wrong_name] {
                    let caller = TracesCaller::StaticPushToken {
                        credential_id: &f.token.name,
                        token,
                    };
                    assert_eq!(
                        rejected(&f, locator, caller, service).await,
                        TracesError::UnknownScope.to_string()
                    );
                }
                assert_eq!(
                    rejected(&f, locator, TracesCaller::Anonymous, service).await,
                    TracesError::Unauthenticated.to_string()
                );
                let empty = TracesCaller::StaticPushToken {
                    credential_id: "",
                    token: &f.token,
                };
                assert_eq!(
                    rejected(&f, locator, empty, service).await,
                    TracesError::Unauthenticated.to_string()
                );
                let unbound = TracesCaller::StaticPushToken {
                    credential_id: "unbound",
                    token: &f.token,
                };
                assert_eq!(
                    rejected(&f, locator, unbound, service).await,
                    TracesError::UnknownScope.to_string()
                );
            }
        }
        assert_eq!(before, f.rows().await);
    }

    #[tokio::test]
    async fn discovery_capability_nonowner_can_read_but_cannot_update() {
        let f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        let other_id = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(other_id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&f.db)
        .await
        .unwrap();
        discovery_binding(&f.db, &other_id, "static_push_token", "other").await;
        let mut token = f.token.clone();
        token.name = "other".to_owned();
        let caller = TracesCaller::StaticPushToken {
            credential_id: &token.name,
            token: &token,
        };
        let read = resolve_trace_discovery(
            &f.db,
            &f.config,
            &f.by_scope(),
            caller,
            ServiceType::UploadPack,
        )
        .await
        .unwrap();
        assert_eq!(read.scope_id().unwrap().to_string(), f.stream_id);
        assert_eq!(
            rejected(&f, &f.by_scope(), caller, ServiceType::ReceivePack).await,
            TracesError::UnknownScope.to_string()
        );
        let own_empty = resolve_trace_discovery(
            &f.db,
            &f.config,
            &f.locator,
            caller,
            ServiceType::ReceivePack,
        )
        .await
        .unwrap();
        assert_eq!(own_empty.scope_id(), None);
    }

    #[tokio::test]
    async fn discovery_capability_retired_binding_and_recreated_target_do_not_alias() {
        let f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        let mut old = trace_target::Entity::find_by_id(&f.target_id)
            .one(&f.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        old.active = Set(false);
        old.update(&f.db).await.unwrap();
        trace_target::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            target_kind: Set("native".to_owned()),
            project_path: Set("/team/app".to_owned()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&f.db)
        .await
        .unwrap();
        assert_eq!(f.metadata(ServiceType::ReceivePack).await.scope_id(), None);
        assert_eq!(
            rejected(&f, &f.by_scope(), f.caller(), ServiceType::UploadPack).await,
            TracesError::UnknownScope.to_string()
        );
        let mut binding = trace_credential_binding::Entity::find()
            .one(&f.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        binding.retired_at = Set(Some(Utc::now().fixed_offset()));
        binding.update(&f.db).await.unwrap();
        assert_eq!(
            rejected(&f, &f.locator, f.caller(), ServiceType::ReceivePack).await,
            TracesError::UnknownScope.to_string()
        );
    }

    #[tokio::test]
    async fn discovery_capability_mono_account_uses_real_policy_and_namespace() {
        use crate::contract::policy::{
            builder::build_from_json, enforcement::Enforcement, entitystore::generate_entity,
        };
        let f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        discovery_binding(&f.db, &f.principal_id, "mono_account", "account").await;
        let mut value: serde_json::Value =
            serde_json::from_str(&generate_entity(&[], "/").unwrap()).unwrap();
        for (user, group) in [("reader", "reader"), ("admin", "admin")] {
            value["users"].as_object_mut().unwrap().insert(format!("User::\"{user}\""), serde_json::json!({
                "euid": format!("User::\"{user}\""), "parents": [format!("UserGroup::\"{group}\"")]
            }));
        }
        let snapshot = build_from_json(&value.to_string()).unwrap();
        for (user, enforcement, snap, read_ok, write_ok) in [
            ("reader", Enforcement::Enforce, Some(&snapshot), true, false),
            ("admin", Enforcement::Enforce, Some(&snapshot), true, true),
            ("admin", Enforcement::Off, Some(&snapshot), false, false),
            (
                "outsider",
                Enforcement::Shadow,
                Some(&snapshot),
                false,
                false,
            ),
            ("admin", Enforcement::Enforce, None, false, false),
        ] {
            let caller = TracesCaller::MonoAccount {
                credential_id: "account",
                username: user,
                enforcement,
                snapshot: snap,
            };
            for (service, allowed) in [
                (ServiceType::UploadPack, read_ok),
                (ServiceType::ReceivePack, write_ok),
            ] {
                let result =
                    resolve_trace_discovery(&f.db, &f.config, &f.by_scope(), caller, service).await;
                assert_eq!(
                    result.is_ok(),
                    allowed,
                    "{user} {enforcement:?} {service:?}"
                );
                if let Err(error) = result {
                    assert_eq!(error.to_string(), TracesError::UnknownScope.to_string());
                }
            }
        }
        let wrong_namespace = TracesCaller::MonoAccount {
            credential_id: &f.token.name,
            username: "admin",
            enforcement: Enforcement::Enforce,
            snapshot: Some(&snapshot),
        };
        assert_eq!(
            rejected(&f, &f.locator, wrong_namespace, ServiceType::UploadPack).await,
            TracesError::UnknownScope.to_string()
        );
    }

    #[tokio::test]
    async fn discovery_capability_invalid_locators_config_and_database_fail_closed() {
        let mut f = DiscoveryFixture::new(HashKind::Sha1, true).await;
        let original = f.config.clone();
        for server_id in [
            None,
            Some("".to_owned()),
            Some("invalid".to_owned()),
            Some(Uuid::nil().to_string()),
        ] {
            f.config.server_id = server_id;
            assert!(
                resolve_trace_discovery(
                    &f.db,
                    &f.config,
                    &f.locator,
                    f.caller(),
                    ServiceType::UploadPack
                )
                .await
                .is_err()
            );
        }
        f.config = original.clone();
        f.config.enabled = false;
        assert_eq!(
            rejected(&f, &f.locator, f.caller(), ServiceType::UploadPack).await,
            TracesError::UnknownScope.to_string()
        );
        f.config = original;
        for axis in 0..6 {
            let mut locator = f.locator.clone();
            if let TracesLocator::ByPath {
                target_path,
                source_id,
                epoch,
                ..
            } = &mut locator
            {
                match axis {
                    0 => *target_path = "/team/../app".to_owned(),
                    1 => *target_path = "/team\\app".to_owned(),
                    2 => *target_path = "/team/\0app".to_owned(),
                    3 => *source_id = String::new(),
                    4 => *source_id = "source\0id".to_owned(),
                    _ => *epoch = "ABCDEF01-2345-4678-89AB-CDEF01234567".to_owned(),
                }
            }
            assert_eq!(
                rejected(&f, &locator, f.caller(), ServiceType::UploadPack).await,
                TracesError::UnknownScope.to_string()
            );
        }
        let mut target = trace_target::Entity::find_by_id(&f.target_id)
            .one(&f.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        target.project_path = Set("/team/../app".to_owned());
        target.update(&f.db).await.unwrap();
        f.token.paths = None;
        assert_eq!(
            rejected(&f, &f.by_scope(), f.caller(), ServiceType::UploadPack).await,
            TracesError::UnknownScope.to_string()
        );
        f.db.close_by_ref().await.unwrap();
        let result = resolve_trace_discovery(
            &f.db,
            &f.config,
            &f.locator,
            f.caller(),
            ServiceType::UploadPack,
        )
        .await;
        assert!(
            matches!(result, Err(MegaError::Db(_))),
            "DB failure must not become empty discovery"
        );
    }

    struct WriterCounts {
        git_repo: u64,
        gitkeep: u64,
        push_queue: u64,
        mega_cl: u64,
        trace_stream: u64,
        trace_target: u64,
    }

    async fn protocol_state(temp: &TempDir) -> ProtocolApiState {
        let mut config = isolated_config(temp.path().join("cfg"));
        config.monorepo.import_dir = std::path::PathBuf::from("/.traces");
        let storage = test_storage_with_config(temp.path(), config).await;
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        ProtocolApiState {
            storage,
            git_object_cache: Arc::new(GitObjectCache {
                connection: crate::jupiter::redis::init_connection(&crate::config::RedisConfig {
                    url: redis_url,
                })
                .await
                .expect("redis connection"),
                prefix: "disabled".to_string(),
            }),
            entity_store: Arc::new(SharedEntityStore::new()),
        }
    }

    async fn writer_counts(state: &ProtocolApiState) -> WriterCounts {
        let git_db = state.storage.git_db_storage();
        let db = git_db.get_connection();
        WriterCounts {
            git_repo: git_repo::Entity::find().count(db).await.unwrap(),
            gitkeep: mega_blob::Entity::find()
                .filter(mega_blob::Column::Name.eq(".gitkeep"))
                .count(db)
                .await
                .unwrap(),
            push_queue: push_queue::Entity::find().count(db).await.unwrap(),
            mega_cl: mega_cl::Entity::find().count(db).await.unwrap(),
            trace_stream: trace_stream::Entity::find().count(db).await.unwrap(),
            trace_target: trace_target::Entity::find().count(db).await.unwrap(),
        }
    }

    fn assert_unchanged(before: &WriterCounts, after: &WriterCounts) {
        assert_eq!(before.git_repo, after.git_repo);
        assert_eq!(before.gitkeep, after.gitkeep);
        assert_eq!(before.push_queue, after.push_queue);
        assert_eq!(before.mega_cl, after.mega_cl);
        assert_eq!(before.trace_stream, after.trace_stream);
        assert_eq!(before.trace_target, after.trace_target);
        assert_eq!(before.git_repo, 0);
        assert_eq!(before.gitkeep, 0);
        assert_eq!(before.push_queue, 0);
        assert_eq!(before.mega_cl, 0);
        assert_eq!(before.trace_stream, 0);
        assert_eq!(before.trace_target, 0);
    }

    #[tokio::test]
    async fn trace_locator_dispatch_discovery_does_not_write_code_rows() {
        let temp = tempfile::tempdir().unwrap();
        let state = protocol_state(&temp).await;
        let path = format!("/.traces/by-scope/{STREAM_ID}");
        let session = SmartSession::from_state(
            std::path::PathBuf::from(&path),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
            &state,
        )
        .unwrap();
        let before = writer_counts(&state).await;
        let handler = session
            .repo_handler_with_commands(&state, Vec::new())
            .await
            .unwrap();
        assert!(handler.is_trace_repo());
        assert!(!handler.is_monorepo());
        assert!(!handler.bind_tip_after_receive());
        let (head, refs) = handler.refs_with_head_hash().await.unwrap();
        assert!(is_protocol_zero_id(&head));
        assert!(refs.is_empty());
        assert!(
            refs.iter()
                .all(|git_ref| git_ref.ref_name != "refs/heads/main")
        );
        let save_err = handler.save_entry(Vec::new()).await.unwrap_err();
        assert!(save_err.to_string().contains(CODE_WRITE_REFUSED));
        let unpack_err = handler
            .unpack_stream(
                &state.storage.config().pack,
                Box::pin(futures::stream::empty()),
            )
            .await
            .expect_err("TraceRepo must refuse a pack before decoding it");
        assert!(matches!(
            unpack_err,
            crate::common::errors::ProtocolError::PackRejected(_)
        ));
        let finalize_err = handler.finalize_receive_pack().await.unwrap_err();
        assert!(finalize_err.to_string().contains(CODE_WRITE_REFUSED));
        let update_err = handler
            .update_refs(&RefCommand::new(
                "0".repeat(40),
                "1".repeat(40),
                "refs/heads/main".to_owned(),
            ))
            .await
            .unwrap_err();
        assert!(update_err.to_string().contains(CODE_WRITE_REFUSED));
        let tree_err = handler
            .traverses_tree_and_update_filepath()
            .await
            .unwrap_err();
        assert!(tree_err.to_string().contains(CODE_WRITE_REFUSED));
        assert!(!handler.check_default_branch().await);
        assert_unchanged(&before, &writer_counts(&state).await);

        let sha256 = TraceRepo::new(
            TracesLocator::ByPath {
                target_path: "/project/foo".to_owned(),
                source_id: "legacy/repo id".to_owned(),
                epoch: "22222222-2222-4222-8222-222222222222".to_owned(),
                hash_kind: TracesObjectFormat::Sha256,
            },
            HashKind::Sha1,
        );
        let (head, refs) = sha256.refs_with_head_hash().await.unwrap();
        assert_eq!(head.len(), 64);
        assert!(refs.is_empty());
        assert_eq!(sha256.object_hash_kind().unwrap(), HashKind::Sha256);
        assert_unchanged(&before, &writer_counts(&state).await);
    }
}
