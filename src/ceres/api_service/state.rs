use std::sync::Arc;

use crate::{
    ceres::api_service::cache::GitObjectCache, contract::policy::entitystore::SharedEntityStore,
    jupiter::storage::Storage,
};

#[derive(Clone)]
/// Shared state for the protocol API service.
///
/// `ProtocolApiState` provides access to the underlying storage backend and a shared
/// cache for Git objects. It is intended to be passed to API handlers and services
/// that require access to repository data and object caching.
///
/// # Usage
/// Construct a `ProtocolApiState` with the required storage and cache, and share it
/// across API endpoints or service layers that need to interact with repository data.
pub struct ProtocolApiState {
    /// The storage backend used for accessing repository data and objects.
    pub storage: Storage,
    /// Shared cache for Git objects to improve access performance and reduce backend load.
    pub git_object_cache: Arc<GitObjectCache>,
    /// Shared authorization snapshot holder (ADR-UN-02). Same `Arc` instance
    /// as `AppContext.entity_store` / `Storage::entity_store()`.
    pub entity_store: Arc<SharedEntityStore>,
}
