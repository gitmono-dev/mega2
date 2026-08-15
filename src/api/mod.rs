use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::extract::FromRef;
use tower_sessions::MemoryStore;

use crate::{
    api::oauth::api_store::BrowserSessionStore,
    bellatrix::Bellatrix,
    ceres::{
        api_service::{
            ApiHandler, cache::GitObjectCache, import_api_service::ImportApiService,
            mono_api_service::MonoApiService, state::ProtocolApiState,
        },
        build_trigger::service::BuildTriggerService,
        protocol::repo::Repo,
    },
    common::errors::ProtocolError,
    contract::policy::entitystore::{EntityStore, SharedEntityStore},
    jupiter::{
        service::webhook_service::WebhookService,
        storage::{
            Storage, cl_storage::ClStorage, conversation_storage::ConversationStorage,
            dynamic_sidebar_storage::DynamicSidebarStorage, gpg_storage::GpgStorage,
            issue_storage::IssueStorage, user_storage::UserStorage,
            webhook_storage::WebhookStorage,
        },
    },
};
pub mod api_common;
pub mod api_doc;
pub mod api_router;
pub mod oauth;
pub mod router;
#[cfg(test)]
mod un08_guard_enforcement;
#[cfg(test)]
mod un20_queue_requester;
#[cfg(test)]
mod un22_principal;
#[cfg(test)]
mod un24_merge_authz;

#[derive(Clone)]
pub struct MonoApiServiceState {
    pub storage: Storage,
    pub session_store: BrowserSessionStore,
    pub git_object_cache: Arc<GitObjectCache>,
    pub listen_addr: String,
    pub entity_store: Arc<SharedEntityStore>,
    pub bellatrix: Arc<Bellatrix>,
}

impl FromRef<MonoApiServiceState> for MemoryStore {
    fn from_ref(_: &MonoApiServiceState) -> Self {
        MemoryStore::default()
    }
}

impl FromRef<MonoApiServiceState> for BrowserSessionStore {
    fn from_ref(state: &MonoApiServiceState) -> Self {
        state.session_store.clone()
    }
}

impl FromRef<MonoApiServiceState> for UserStorage {
    fn from_ref(state: &MonoApiServiceState) -> Self {
        state.storage.user_storage()
    }
}

impl FromRef<MonoApiServiceState> for EntityStore {
    fn from_ref(state: &MonoApiServiceState) -> Self {
        // The guard consumes the shared snapshot's store; empty when not built
        // (off mode). UN-08 switches the guard to the three-state helper.
        state
            .entity_store
            .snapshot()
            .map(|s| s.store().clone())
            .unwrap_or_default()
    }
}

impl From<&MonoApiServiceState> for MonoApiService {
    fn from(state: &MonoApiServiceState) -> Self {
        MonoApiService {
            storage: state.storage.clone(),
            git_object_cache: state.git_object_cache.clone(),
        }
    }
}

impl FromRef<MonoApiServiceState> for ProtocolApiState {
    fn from_ref(state: &MonoApiServiceState) -> ProtocolApiState {
        ProtocolApiState {
            storage: state.storage.clone(),
            git_object_cache: state.git_object_cache.clone(),
            entity_store: state.entity_store.clone(),
        }
    }
}

impl MonoApiServiceState {
    fn monorepo(&self) -> MonoApiService {
        self.into()
    }

    fn issue_stg(&self) -> IssueStorage {
        self.storage.issue_storage()
    }

    fn gpg_stg(&self) -> GpgStorage {
        self.storage.gpg_storage()
    }

    fn cl_stg(&self) -> ClStorage {
        self.storage.cl_storage()
    }

    fn user_stg(&self) -> UserStorage {
        self.storage.user_storage()
    }

    fn conv_stg(&self) -> ConversationStorage {
        self.storage.conversation_storage()
    }

    fn webhook_stg(&self) -> WebhookStorage {
        self.storage.webhook_storage()
    }

    fn webhook_svc(&self) -> WebhookService {
        self.storage.webhook_service.clone()
    }

    fn dynamic_sidebar_stg(&self) -> DynamicSidebarStorage {
        self.storage.dynamic_sidebar_storage()
    }

    pub fn build_trigger_service(&self) -> BuildTriggerService {
        BuildTriggerService::new(
            self.storage.clone(),
            self.git_object_cache.clone(),
            self.bellatrix.clone(),
        )
    }

    async fn api_handler(&self, path: &Path) -> Result<Box<dyn ApiHandler>, ProtocolError> {
        // Normalize path to ensure it has a root component
        let path = if path.has_root() {
            path.to_path_buf()
        } else {
            PathBuf::from("/").join(path)
        };

        let import_dir = self.storage.config().monorepo.import_dir.clone();
        if path.starts_with(&import_dir)
            && path != import_dir
            && let Some(model) = self
                .storage
                .git_db_storage()
                .find_git_repo_like_path(path.to_str().unwrap())
                .await
                .unwrap()
        {
            let repo: Repo = model.into();
            return Ok(Box::new(ImportApiService {
                storage: self.storage.clone(),
                repo,
                git_object_cache: self.git_object_cache.clone(),
            }));
        }
        let ret: Box<dyn ApiHandler> = Box::<MonoApiService>::new(self.into());

        // Rust-analyzer cannot infer the type of `ret` correctly and always reports an error.
        // Use `.into()` to workaround this issue.
        #[allow(clippy::useless_conversion)]
        Ok(ret.into())
    }
}
