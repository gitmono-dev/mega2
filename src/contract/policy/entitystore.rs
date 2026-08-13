use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use cedar_policy::{Entities, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, to_string_pretty};

use crate::{
    common::errors::SaturnContextError,
    contract::policy::{
        builder::{BuilderError, EntitySnapshot, build_from_json},
        objects::{Issue, MergeRequest, Repo, User, UserGroup},
        util::SaturnEUid,
    },
};

/// An in-memory store for entities used in Cedar policies.
///
/// Internal state is shared behind `Arc<RwLock<Inner>>`; `clone` shares the
/// same state (no deep copy). `new()` semantics and all existing construction
/// points are unchanged (UN-12).
#[derive(Debug, Default)]
pub struct EntityStore {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Inner {
    users: HashMap<SaturnEUid, User>,
    repos: HashMap<SaturnEUid, Repo>,
    merge_requests: HashMap<SaturnEUid, MergeRequest>,
    issues: HashMap<SaturnEUid, Issue>,
    user_groups: HashMap<SaturnEUid, UserGroup>,
}

impl Clone for EntityStore {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<'de> Deserialize<'de> for EntityStore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let inner = Inner::deserialize(deserializer)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
        })
    }
}

impl Serialize for EntityStore {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let inner = self.inner.read().unwrap();
        inner.serialize(serializer)
    }
}

impl EntityStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::default())),
        }
    }

    pub fn as_entities(&self, schema: &Schema) -> Result<Entities, SaturnContextError> {
        let inner = self.inner.read().unwrap();
        let users = inner.users.values().map(|user| user.clone().into());
        let repos = inner.repos.values().map(|repo| repo.clone().into());
        let merge_requests = inner
            .merge_requests
            .values()
            .map(|user| user.clone().into());
        let issues = inner.issues.values().map(|repo| repo.clone().into());
        let user_groups = inner.user_groups.values().map(|group| group.clone().into());
        let all = users
            .chain(repos)
            .chain(user_groups)
            .chain(merge_requests)
            .chain(issues);
        Entities::from_entities(all, Some(schema))
            .map_err(|e| SaturnContextError::Entities(e.to_string()))
    }

    pub fn merge(&mut self, other: EntityStore) {
        if Arc::ptr_eq(&self.inner, &other.inner) {
            return;
        }
        let mut self_inner = self.inner.write().unwrap();
        let other_inner = other.inner.read().unwrap();
        self_inner.users.extend(other_inner.users.clone());
        self_inner.repos.extend(other_inner.repos.clone());
        self_inner
            .merge_requests
            .extend(other_inner.merge_requests.clone());
        self_inner.issues.extend(other_inner.issues.clone());
        self_inner
            .user_groups
            .extend(other_inner.user_groups.clone());
    }

    pub fn is_empty(&self) -> bool {
        let inner = self.inner.read().unwrap();
        inner.users.is_empty()
            && inner.repos.is_empty()
            && inner.merge_requests.is_empty()
            && inner.issues.is_empty()
            && inner.user_groups.is_empty()
    }

    pub fn extract_admin_usernames(&self) -> HashSet<String> {
        const ADMIN_GROUP: &str = "UserGroup::\"admin\"";

        let inner = self.inner.read().unwrap();
        let mut admins = HashSet::new();
        for user in inner.users.values() {
            let is_admin = user.parents().iter().any(|p| p.to_string() == ADMIN_GROUP);
            if is_admin {
                let username: &str = user.euid().id().as_ref();
                admins.insert(username.to_string());
            }
        }
        admins
    }
}

/// Path of the in-repo authorization data source (ADR-UN-02).
pub const MEGA_CEDAR_PATH: &str = "/.mega_cedar.json";

/// Runtime holder for the shared authorization snapshot (UN-15): build-then-swap
/// under a write lock, consistent read view, dirty flag on rebuild failure, and
/// idempotent first build.
pub struct SharedEntityStore {
    inner: RwLock<SharedInner>,
}

#[derive(Default)]
struct SharedInner {
    snapshot: Option<Arc<EntitySnapshot>>,
    dirty: bool,
}

impl Default for SharedEntityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedEntityStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(SharedInner::default()),
        }
    }

    /// Idempotent first build: if a snapshot already exists, no-op.
    pub fn ensure(&self, json: &str) -> Result<(), BuilderError> {
        if self.inner.read().unwrap().snapshot.is_some() {
            return Ok(());
        }
        self.swap(json)
    }

    /// Build-then-swap: build a new snapshot; on success replace under a write
    /// lock; on failure keep the old snapshot, set dirty, and log
    /// `event=authz_rebuild_failed`.
    pub fn swap(&self, json: &str) -> Result<(), BuilderError> {
        let new_snapshot = match build_from_json(json) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let mut inner = self.inner.write().unwrap();
                inner.dirty = true;
                tracing::error!(
                    event = "authz_rebuild_failed",
                    path = MEGA_CEDAR_PATH,
                    error = %error,
                    "authorization snapshot rebuild failed; keeping previous snapshot"
                );
                return Err(error);
            }
        };
        let mut inner = self.inner.write().unwrap();
        inner.snapshot = Some(Arc::new(new_snapshot));
        inner.dirty = false;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.inner.read().unwrap().dirty
    }

    /// Consistent read view: returns a shared handle to the current immutable
    /// snapshot (or `None` if not yet built).
    pub fn snapshot(&self) -> Option<Arc<EntitySnapshot>> {
        self.inner.read().unwrap().snapshot.clone()
    }
}

pub fn generate_entity(
    admins: &[String],
    repo: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut json_data = json!({
        "users": {
        },
        "repos": {
        },
        "user_groups": {
            "UserGroup::\"admin\"": {
                "euid": "UserGroup::\"admin\"",
                "parents": [
                    "UserGroup::\"matainer\""
                ]
            },
            "UserGroup::\"matainer\"": {
                "euid": "UserGroup::\"matainer\"",
                "parents": [
                    "UserGroup::\"reader\""
                ]
            },
            "UserGroup::\"reader\"": {
                "euid": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "merge_requests": {
        },
        "issues": {
        }
    });

    // Add all admin users to the admin group
    if let Some(users) = json_data.get_mut("users")
        && let Some(users_map) = users.as_object_mut()
    {
        for user in admins {
            users_map.insert(
                format!("User::\"{user}\""),
                json!({
                        "euid": format!("User::\"{user}\""),
                        "parents": [
                            "UserGroup::\"admin\""
                        ]
                }),
            );
        }
    }

    if let Some(repos) = json_data.get_mut("repos")
        && let Some(repos_map) = repos.as_object_mut()
    {
        repos_map.insert(
            format!("Repository::\"{repo}\""),
            json!({
                    "euid": format!("Repository::\"{repo}\""),
                    "is_private": true,
                    "admins": "UserGroup::\"admin\"",
                    "maintainers": "UserGroup::\"matainer\"",
                    "readers": "UserGroup::\"reader\"",
                    "parents": []
            }),
        );
    }
    Ok(to_string_pretty(&json_data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_shares_same_state() {
        let store = EntityStore::new();
        let clone = store.clone();
        // `clone` shares the same underlying state (no deep copy).
        assert!(Arc::ptr_eq(&store.inner, &clone.inner));
        assert!(store.is_empty());
        assert!(clone.is_empty());
    }

    #[test]
    fn as_entities_returns_result() {
        let store = EntityStore::new();
        let (schema, _) =
            Schema::from_cedarschema_str(include_str!("mega.cedarschema")).expect("schema");
        let result = store.as_entities(&schema);
        assert!(result.is_ok(), "empty store should build entities");
    }

    #[test]
    fn deserialize_wraps_shared_state() {
        let json = r#"{"users":{},"repos":{},"merge_requests":{},"issues":{},"user_groups":{}}"#;
        let store: EntityStore = serde_json::from_str(json).expect("deserialize");
        assert!(store.is_empty());
        let clone = store.clone();
        assert!(Arc::ptr_eq(&store.inner, &clone.inner));
    }

    #[test]
    fn serde_round_trip_preserves_entities() {
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        let store: EntityStore = serde_json::from_str(&json).expect("deserialize");
        let serialized = serde_json::to_string(&store).expect("serialize");
        let store2: EntityStore = serde_json::from_str(&serialized).expect("re-deserialize");
        assert!(!store2.is_empty(), "round-trip should preserve entities");
        let clone = store2.clone();
        assert!(Arc::ptr_eq(&store2.inner, &clone.inner));
    }

    #[test]
    fn shared_store_swap_replaces_snapshot() {
        let store = SharedEntityStore::new();
        assert!(store.snapshot().is_none());
        store
            .swap(&generate_entity(&["admin".to_string()], "repo").expect("generate"))
            .expect("first swap");
        assert!(store.snapshot().is_some());
        assert!(!store.is_dirty());
        // Second swap with a different repo replaces the snapshot.
        store
            .swap(&generate_entity(&["admin".to_string()], "other").expect("generate"))
            .expect("second swap");
        assert!(store.snapshot().is_some());
    }

    #[test]
    fn shared_store_rebuild_failure_keeps_old_and_sets_dirty() {
        let store = SharedEntityStore::new();
        store
            .swap(&generate_entity(&["admin".to_string()], "repo").expect("generate"))
            .expect("first swap");
        let before = store.snapshot().expect("snapshot");
        // Invalid JSON: rebuild fails, old snapshot retained, dirty set.
        let err = store
            .swap("not-json")
            .expect_err("invalid json should fail");
        assert!(matches!(err, BuilderError::Json(_)));
        assert!(store.is_dirty());
        let after = store.snapshot().expect("snapshot");
        assert!(Arc::ptr_eq(&before, &after), "old snapshot retained");
    }

    #[test]
    fn shared_store_ensure_is_idempotent() {
        let store = SharedEntityStore::new();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        store.ensure(&json).expect("first ensure");
        let first = store.snapshot().expect("snapshot");
        // Second ensure is a no-op (does not rebuild).
        store.ensure(&json).expect("second ensure");
        let second = store.snapshot().expect("snapshot");
        assert!(Arc::ptr_eq(&first, &second), "ensure must not rebuild");
    }

    #[test]
    fn shared_store_consistent_read_view() {
        let store = SharedEntityStore::new();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        store.ensure(&json).expect("ensure");
        // Two reads return the same immutable snapshot handle (consistent view).
        let a = store.snapshot().expect("a");
        let b = store.snapshot().expect("b");
        assert!(Arc::ptr_eq(&a, &b));
    }
}
