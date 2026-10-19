use core::fmt;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

use git_internal::hash::{HashKind, ObjectHash};
use import_refs::{RefCommand, Refs};
use repo::Repo;
use tokio::sync::RwLock;

use crate::{
    ceres::{
        api_service::state::ProtocolApiState,
        pack::{
            RepoHandler, import_repo::ImportRepo, monorepo::Monorepo,
            path_policy::check_import_repo_leaf, push_chain, traces_repo::TraceRepo,
            view_repo::ViewRepo,
        },
    },
    common::{
        errors::{MegaError, ProtocolError},
        utils::{canonicalize_mono_ref_path, is_full_hex_object_id, is_protocol_zero_id},
    },
    contract::git_protocol::{
        ResolvedView,
        path::{TracesLocator, TracesObjectFormat, parse_traces_locator},
    },
};

pub mod import_refs;
pub mod repo;
pub mod smart;
pub mod v2;

#[derive(Clone, Debug)]
pub struct PushUserInfo {
    pub username: String,
}

#[derive(Clone, Debug)]
pub struct AuthContext {
    /// The actor username associated with the protocol operation (if available).
    pub username: Option<String>,
    /// The authenticated push user info (if available).
    pub authenticated_user: Option<PushUserInfo>,
}

#[derive(Clone, Debug)]
pub struct SmartSession {
    pub repo_path: PathBuf,
    pub service_type: ServiceType,
    pub transport_protocol: TransportProtocol,
    pub auth: AuthContext,
    pub capabilities: HashSet<Capability>,
    /// Authoritative repository object hash kind (`MonoConfig.object_hash_kind()`).
    pub hash_kind: HashKind,
    pub(crate) view: Option<ResolvedView>,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum TransportProtocol {
    Local,
    #[default]
    Http,
    Ssh,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ServiceType {
    UploadPack,
    ReceivePack,
}

impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ServiceType::UploadPack => write!(f, "git-upload-pack"),
            ServiceType::ReceivePack => write!(f, "git-receive-pack"),
        }
    }
}

impl FromStr for ServiceType {
    type Err = MegaError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "git-upload-pack" => Ok(ServiceType::UploadPack),
            "git-receive-pack" => Ok(ServiceType::ReceivePack),
            _ => Err(MegaError::Other(format!("Invalid service name: {}", s))),
        }
    }
}

// TODO: Additional Capabilitys need to be supplemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    MultiAck,
    MultiAckDetailed,
    NoDone,
    SideBand,
    SideBand64k,
    ReportStatus,
    ReportStatusv2,
    OfsDelta,
    DeepenSince,
    DeepenNot,
    Shallow,
}

impl FromStr for Capability {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "report-status" => Ok(Capability::ReportStatus),
            "report-status-v2" => Ok(Capability::ReportStatusv2),
            "side-band" => Ok(Capability::SideBand),
            "side-band-64k" => Ok(Capability::SideBand64k),
            "ofs-delta" => Ok(Capability::OfsDelta),
            "multi_ack" => Ok(Capability::MultiAck),
            "multi_ack_detailed" => Ok(Capability::MultiAckDetailed),
            "no-done" => Ok(Capability::NoDone),
            "deepen-since" => Ok(Capability::DeepenSince),
            "deepen-not" => Ok(Capability::DeepenNot),
            "shallow" => Ok(Capability::Shallow),
            _ => Err(()),
        }
    }
}

pub(crate) fn hex_object_id_matches_kind(hex: &str, kind: HashKind) -> bool {
    hex.len() == kind.hex_len() && is_full_hex_object_id(hex)
}

fn repo_path_to_str(path: &Path) -> Result<&str, ProtocolError> {
    path.to_str()
        .ok_or_else(|| ProtocolError::InvalidInput("repository path is not valid UTF-8".to_owned()))
}

pub enum SideBind {
    // sideband 1 will contain packfile data,
    PackfileData,
    // sideband 2 will be used for progress information that the client will generally print to stderr and
    ProgressInfo,
    // sideband 3 is used for error information.
    Error,
}

impl SideBind {
    pub fn value(&self) -> u8 {
        match self {
            Self::PackfileData => b'\x01',
            Self::ProgressInfo => b'\x02',
            Self::Error => b'\x03',
        }
    }
}
pub struct RefUpdateRequest {
    pub commands: Vec<RefCommand>,
}

impl SmartSession {
    pub fn new(
        repo_path: PathBuf,
        service_type: ServiceType,
        transport_protocol: TransportProtocol,
    ) -> Self {
        SmartSession {
            repo_path,
            service_type,
            transport_protocol,
            auth: AuthContext {
                username: None,
                authenticated_user: None,
            },
            capabilities: HashSet::new(),
            hash_kind: HashKind::Sha1,
            view: None,
        }
    }

    pub fn with_hash_kind(mut self, hash_kind: HashKind) -> Self {
        self.hash_kind = hash_kind;
        self
    }

    pub fn hash_kind_from_state(state: &ProtocolApiState) -> Result<HashKind, ProtocolError> {
        state
            .storage
            .config()
            .monorepo
            .object_hash_kind()
            .map_err(|e| ProtocolError::InvalidInput(e.to_string()))
    }

    pub fn from_state(
        repo_path: PathBuf,
        service_type: ServiceType,
        transport_protocol: TransportProtocol,
        state: &ProtocolApiState,
    ) -> Result<Self, ProtocolError> {
        let mut session = Self::new(repo_path, service_type, transport_protocol)
            .with_hash_kind(Self::hash_kind_from_state(state)?);
        if let Some(TracesLocator::ByPath { hash_kind, .. }) =
            parse_traces_locator(repo_path_to_str(&session.repo_path)?)?
        {
            session.hash_kind = match hash_kind {
                TracesObjectFormat::Sha1 => HashKind::Sha1,
                TracesObjectFormat::Sha256 => HashKind::Sha256,
            };
        }
        Ok(session)
    }

    pub fn ensure_object_id_hex(&self, hex: &str, what: &str) -> Result<(), ProtocolError> {
        if is_protocol_zero_id(hex) {
            return Ok(());
        }
        ObjectHash::from_hex_for_kind(self.hash_kind, hex).map_err(|error| {
            ProtocolError::InvalidInput(format!(
                "{what} object id is incompatible with object-format {} (expected {} hex chars): {error}",
                self.hash_kind.as_str(),
                self.hash_kind.hex_len(),
            ))
        })?;
        Ok(())
    }

    pub fn ensure_advertised_object_ids(
        &self,
        head_hash: &str,
        refs: &[Refs],
    ) -> Result<(), ProtocolError> {
        self.ensure_object_id_hex(head_hash, "HEAD")?;
        for git_ref in refs {
            self.ensure_object_id_hex(&git_ref.ref_hash, &git_ref.ref_name)?;
        }
        Ok(())
    }

    pub fn set_authenticated_user(&mut self, username: String) {
        self.auth.username = Some(username.clone());
        self.auth.authenticated_user = Some(PushUserInfo { username });
    }

    pub async fn repo_handler_with_commands(
        &self,
        state: &ProtocolApiState,
        commands: Vec<RefCommand>,
    ) -> Result<Arc<dyn RepoHandler>, ProtocolError> {
        if let Some(view) = &self.view {
            return Ok(Arc::new(ViewRepo::new(
                state.storage.clone(),
                view.config.clone(),
                state.storage.view_projection_service(),
                view.filter_pk,
                view.filter_id.clone(),
            )) as Arc<dyn RepoHandler>);
        }
        let raw_repo_path = repo_path_to_str(&self.repo_path)?;
        if let Some(locator) = parse_traces_locator(raw_repo_path)? {
            return Ok(Arc::new(TraceRepo::new(locator, self.hash_kind)) as Arc<dyn RepoHandler>);
        }
        let config = state.storage.config();
        let import_dir = config.monorepo.import_dir.clone();

        if self.repo_path.starts_with(import_dir.clone()) {
            let storage = state.storage.git_db_storage();
            // Canonicalize before lookup/create so alias paths cannot split
            // git_repo identity from the monorepo attach target (TP-08).
            let raw_path = repo_path_to_str(&self.repo_path)?;
            let path_str = canonicalize_mono_ref_path(raw_path)
                .map_err(|e| ProtocolError::InvalidInput(e.to_string()))?;
            // plan-20260923 ADR-FU-08 item 7: only a path strictly below
            // `import_dir` is an ImportRepo; checked before any lookup, relabel
            // or registration, for both services.
            check_import_repo_leaf(&config.monorepo, &path_str).map_err(MegaError::from)?;
            let path_buf = PathBuf::from(&path_str);
            // Prefer canonical lookup; fall back to the raw path so pre-TP-08
            // aliased `git_repo` rows remain reachable under the same repo_id.
            let model = match storage.find_git_repo_exact_match(&path_str).await? {
                Some(m) => Some(m),
                None if raw_path != path_str => {
                    match storage.find_git_repo_exact_match(raw_path).await? {
                        Some(legacy) => {
                            // Heal alias rows onto the canonical path so a later
                            // canonical ReceivePack cannot create a second repo_id.
                            storage.relabel_git_repo_path(legacy.id, &path_str).await?;
                            storage.find_git_repo_exact_match(&path_str).await?
                        }
                        None => None,
                    }
                }
                None => None,
            };
            let repo = if let Some(model) = model {
                let mut repo: Repo = model.into();
                repo.repo_path = path_str;
                repo
            } else {
                match self.service_type {
                    ServiceType::UploadPack => {
                        return Err(ProtocolError::NotFound("Repository not found.".to_owned()));
                    }
                    ServiceType::ReceivePack => {
                        let repo = Repo::new(path_buf, false)?;
                        // Ancestor lifecycle fence (ADR-FU-09 item 2).
                        storage.register_import_repo(repo.clone().into()).await?;
                        repo
                    }
                }
            };

            Ok(Arc::new(ImportRepo {
                git_object_cache: state.git_object_cache.clone(),
                storage: state.storage.clone(),
                repo,
                command_list: Mutex::new(commands),
                receive_pack_extra_timings_ms: Mutex::new(Vec::new()),
            }) as Arc<dyn RepoHandler>)
        } else {
            let mut res = Monorepo {
                git_object_cache: state.git_object_cache.clone(),
                storage: state.storage.clone(),
                path: self.repo_path.clone(),
                base_branch: "main".to_string(),
                pack_commit_ids: Mutex::new(HashSet::new()),
                new_commit_ids: Mutex::new(HashSet::new()),
                no_op_notice: Mutex::new(None),
                push_chain_cache: Mutex::new(HashMap::new()),
                cl_link: Arc::new(RwLock::new(None)),
                username: self.auth.username.clone(),
                command_list: Mutex::new(commands.clone()),
            };
            // base_branch comes from the primary (first non-delete) branch
            // command — a mixed delete+update push must not take the delete
            // command's ref name (MC01-R2 P1-1).
            if let Some(command) = push_chain::primary_branch_command(&commands) {
                res.base_branch = command
                    .ref_name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(command.ref_name.as_str())
                    .to_string();
            }
            Ok(Arc::new(res) as Arc<dyn RepoHandler>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn repo_path_to_str_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
        let err = repo_path_to_str(&path).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
    }

    #[test]
    fn repo_path_to_str_accepts_utf8_paths() {
        let path = PathBuf::from("/tmp/repo.git");

        assert_eq!(repo_path_to_str(&path).unwrap(), "/tmp/repo.git");
    }

    #[test]
    fn injected_kind_rejects_wrong_root_ref_hex_width() {
        let sha1_id = "a".repeat(40);
        let sha256_id = "b".repeat(64);
        assert!(hex_object_id_matches_kind(&sha1_id, HashKind::Sha1));
        assert!(!hex_object_id_matches_kind(&sha1_id, HashKind::Sha256));
        assert!(hex_object_id_matches_kind(&sha256_id, HashKind::Sha256));
        assert!(!hex_object_id_matches_kind(&sha256_id, HashKind::Sha1));
        assert!(hex_object_id_matches_kind(&sha256_id, HashKind::Blake3));
        assert!(!hex_object_id_matches_kind(&sha1_id, HashKind::Blake3));

        let session = SmartSession::new(
            PathBuf::from("/tmp/repo.git"),
            ServiceType::UploadPack,
            TransportProtocol::Http,
        )
        .with_hash_kind(HashKind::Sha256);
        let err = session
            .ensure_object_id_hex(&sha1_id, "HEAD")
            .expect_err("40-hex HEAD is incompatible with injected sha256");
        assert!(err.to_string().contains("object-format sha256"), "{err}");
        session
            .ensure_object_id_hex(&sha256_id, "HEAD")
            .expect("64-hex HEAD matches sha256");
    }

    const STREAM_ID: &str = "abcdef01-2345-4678-89ab-cdef01234567";

    async fn state_with_traces_import_dir() -> (
        tempfile::TempDir,
        crate::ceres::api_service::state::ProtocolApiState,
    ) {
        use std::sync::Arc;

        use crate::{
            ceres::api_service::cache::GitObjectCache, config::testing::isolated_config,
            contract::policy::entitystore::SharedEntityStore,
            jupiter::tests::test_storage_with_config,
        };

        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("cfg"));
        config.monorepo.import_dir = PathBuf::from("/.traces");
        let storage = test_storage_with_config(temp.path(), config).await;
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        let state = crate::ceres::api_service::state::ProtocolApiState {
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
        };
        (temp, state)
    }

    async fn git_repo_count(state: &crate::ceres::api_service::state::ProtocolApiState) -> u64 {
        use sea_orm::{EntityTrait, PaginatorTrait};

        use crate::jupiter::storage::base_storage::StorageConnector;

        let git_db = state.storage.git_db_storage();
        crate::callisto::git_repo::Entity::find()
            .count(git_db.get_connection())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn trace_locator_dispatch_selects_trace_repo_before_import_or_native() {
        let (_temp, state) = state_with_traces_import_dir().await;
        let traces_path = PathBuf::from(format!("/.traces/by-scope/{STREAM_ID}"));
        let session = SmartSession::from_state(
            traces_path,
            ServiceType::ReceivePack,
            TransportProtocol::Http,
            &state,
        )
        .unwrap();
        let before = git_repo_count(&state).await;
        let handler = session
            .repo_handler_with_commands(&state, Vec::new())
            .await
            .unwrap();
        assert!(handler.is_trace_repo());
        assert!(!handler.is_monorepo());
        let (head, refs) = handler.refs_with_head_hash().await.unwrap();
        assert!(is_protocol_zero_id(&head));
        assert!(refs.is_empty());
        let advertised = session.git_info_refs(&state).await.unwrap();
        let text = String::from_utf8_lossy(&advertised);
        assert!(text.contains("capabilities^{}"));
        assert!(!text.contains("refs/heads/main"));
        assert!(!text.contains("HEAD"));
        assert_eq!(git_repo_count(&state).await, before);
        assert_eq!(before, 0);

        let illegal_session = match SmartSession::from_state(
            PathBuf::from("/.traces/vendor"),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
            &state,
        ) {
            Err(err) => err,
            Ok(_) => panic!("illegal traces locator built a session"),
        };
        assert!(
            illegal_session
                .to_string()
                .contains("traces locator not found"),
            "{illegal_session}"
        );
        let illegal = SmartSession::new(
            PathBuf::from("/.traces/vendor"),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
        );
        let err = match illegal.repo_handler_with_commands(&state, Vec::new()).await {
            Err(err) => err,
            Ok(_) => panic!("illegal traces locator opened a repository"),
        };
        assert!(
            err.to_string().contains("traces locator not found"),
            "{err}"
        );
        assert_eq!(git_repo_count(&state).await, 0);

        let native = SmartSession::from_state(
            PathBuf::from("/project/app"),
            ServiceType::UploadPack,
            TransportProtocol::Http,
            &state,
        )
        .unwrap();
        let native_handler = native
            .repo_handler_with_commands(&state, Vec::new())
            .await
            .unwrap();
        assert!(!native_handler.is_trace_repo());
        assert!(native_handler.is_monorepo());
        assert_eq!(session.hash_kind, HashKind::Sha1);
        assert_eq!(git_repo_count(&state).await, 0);

        let sha256 = SmartSession::from_state(
            PathBuf::from(
                "/.traces/by-path/L3Byb2plY3QvZm9v/self/bGVnYWN5L3JlcG8gaWQ/22222222-2222-4222-8222-222222222222/sha256",
            ),
            ServiceType::UploadPack,
            TransportProtocol::Http,
            &state,
        )
        .unwrap();
        assert_eq!(sha256.hash_kind, HashKind::Sha256);
        let advertised = sha256.git_info_refs(&state).await.unwrap();
        let text = String::from_utf8_lossy(&advertised);
        assert!(text.contains("object-format=sha256"), "{text}");
        assert!(text.contains(&"0".repeat(64)), "{text}");
        assert!(!text.contains("object-format=sha1"), "{text}");
        assert!(!text.contains("refs/heads/main"), "{text}");
        assert_eq!(git_repo_count(&state).await, 0);
    }
}
