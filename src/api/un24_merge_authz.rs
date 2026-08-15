//! UN-24: both merge entry points are authorized, `merge-no-auth` included.
//!
//! `merge-no-auth` was a local-testing shortcut that hardcoded a `"system"`
//! actor and sat outside the guard's mapping entirely — anyone who could reach
//! the port could merge. It is now mapped exactly like `merge`, and an
//! anonymous caller is authorized as the reserved anonymous principal rather
//! than as a privileged name.
//!
//! The role matrices below assert the same four-role outcome on both entry
//! points, and that an anonymous call is **403** — a 401 there would be an
//! authentication answer to an authorization question, and would read as a
//! false green (it can be produced by simply having no session).

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
    routing::post,
};
use sea_orm::ConnectionTrait;
use tower::ServiceExt;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{
            api_store::{BrowserSessionStore, CountingSessionStore, FixedUserSessionStore},
            model::LoginUser,
        },
    },
    bellatrix::Bellatrix,
    ceres::api_service::cache::GitObjectCache,
    contract::policy::{entitystore::SharedEntityStore, guard::cedar_guard::cedar_guard},
    jupiter::storage::{Storage, base_storage::StorageConnector},
};

/// Roles of the seeded ACL: only `admin-user` may approve a merge.
const ADMIN: &str = "un24-admin";
const MAINTAINER: &str = "un24-maintainer";
const READER: &str = "un24-reader";

/// An ACL with one member per role, so the matrix exercises the real group
/// hierarchy rather than "admin vs nobody".
fn role_acl_json() -> String {
    serde_json::json!({
        "users": {
            format!("User::\"{ADMIN}\""): {
                "euid": format!("User::\"{ADMIN}\""),
                "parents": ["UserGroup::\"admin\""]
            },
            format!("User::\"{MAINTAINER}\""): {
                "euid": format!("User::\"{MAINTAINER}\""),
                "parents": ["UserGroup::\"matainer\""]
            },
            format!("User::\"{READER}\""): {
                "euid": format!("User::\"{READER}\""),
                "parents": ["UserGroup::\"reader\""]
            }
        },
        "repos": {
            "Repository::\"/\"": {
                "euid": "Repository::\"/\"",
                "is_private": true,
                "admins": "UserGroup::\"admin\"",
                "maintainers": "UserGroup::\"matainer\"",
                "readers": "UserGroup::\"reader\"",
                "parents": []
            }
        },
        "user_groups": {
            "UserGroup::\"admin\"": {
                "euid": "UserGroup::\"admin\"",
                "parents": ["UserGroup::\"matainer\""]
            },
            "UserGroup::\"matainer\"": {
                "euid": "UserGroup::\"matainer\"",
                "parents": ["UserGroup::\"reader\""]
            },
            "UserGroup::\"reader\"": { "euid": "UserGroup::\"reader\"", "parents": [] }
        },
        "merge_requests": {},
        "issues": {}
    })
    .to_string()
}

fn role_snapshot() -> Arc<SharedEntityStore> {
    let store = Arc::new(SharedEntityStore::new());
    store.ensure(&role_acl_json()).expect("build snapshot");
    store
}

async fn storage_enforcing(temp: &std::path::Path) -> Storage {
    let mut storage = crate::jupiter::tests::test_storage(temp).await;
    let mut config = (*storage.config).clone();
    config.cedar.enforcement = "enforce".to_string();
    let config = Arc::new(config);
    storage.config_handle = crate::config::reload::ConfigHandle::from_arc(config.clone());
    storage.config = config;
    storage
}

fn api_state(storage: Storage, session_store: BrowserSessionStore) -> MonoApiServiceState {
    MonoApiServiceState {
        session_store,
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un24-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store: role_snapshot(),
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
        storage,
    }
}

async fn seed_open_cl(storage: &Storage, id: i64, cl_link: &str) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{cl_link}', 'un24', 'open', '/', 'from', 'to', now(), now(), \
             'un24-author', 'main')"
        ))
        .await
        .expect("seed open CL");
}

fn session_for(role: Option<&str>) -> BrowserSessionStore {
    match role {
        Some(username) => BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: LoginUser {
                username: username.to_string(),
                ..Default::default()
            },
        }),
        None => BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
    }
}

/// Route the guard in front of a stub handler: this asserts the *authorization
/// decision*, not the merge itself (which needs a real ref graph).
fn guarded(state: MonoApiServiceState, path: &'static str) -> Router {
    async fn reached() -> &'static str {
        "reached"
    }
    Router::new()
        .route(path, post(reached))
        .layer(middleware::from_fn_with_state(state.clone(), cedar_guard))
        .with_state(state)
}

async fn post_status(router: Router, uri: String) -> StatusCode {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

/// `(role, expected)` for `approveMergeRequest` under the **current** policy
/// (`mega_policies.cedar`): it is a maintainer-level action, and admin inherits
/// it through the group hierarchy. A reader may not approve, and an anonymous
/// caller is denied as a principal — 403, not the 401 of an unauthenticated
/// request.
///
/// Whether approving *should* be admin-only is a policy-content question and
/// belongs to UN-09/UN-04; this card only proves that both merge entry points
/// reach the same decision.
const ROLE_MATRIX: &[(Option<&str>, StatusCode)] = &[
    (Some(ADMIN), StatusCode::OK),
    (Some(MAINTAINER), StatusCode::OK),
    (Some(READER), StatusCode::FORBIDDEN),
    (None, StatusCode::FORBIDDEN),
];

#[tokio::test]
async fn un24_merge_matrix() {
    for (index, (role, expected)) in ROLE_MATRIX.iter().enumerate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_enforcing(temp.path()).await;
        let link = format!("UN24MERGE{index}");
        seed_open_cl(&storage, 700_100 + index as i64, &link).await;

        let state = api_state(storage, session_for(*role));
        let status = post_status(
            guarded(state, "/api/v1/cl/{link}/merge"),
            format!("/api/v1/cl/{link}/merge"),
        )
        .await;

        assert_eq!(status, *expected, "merge as {role:?} must be {expected}",);
        if role.is_none() {
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "an anonymous merge must be denied as a principal, not bounced as unauthenticated"
            );
        }
    }
}

#[tokio::test]
async fn un24_merge_no_auth_matrix() {
    for (index, (role, expected)) in ROLE_MATRIX.iter().enumerate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_enforcing(temp.path()).await;
        let link = format!("UN24NOAUTH{index}");
        seed_open_cl(&storage, 700_200 + index as i64, &link).await;

        let state = api_state(storage, session_for(*role));
        let status = post_status(
            guarded(state, "/api/v1/cl/{link}/merge-no-auth"),
            format!("/api/v1/cl/{link}/merge-no-auth"),
        )
        .await;

        assert_eq!(
            status, *expected,
            "merge-no-auth as {role:?} must be {expected} — the same outcome as merge",
        );
        if role.is_none() {
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "403 is the authorization answer; 401 here would be a false green"
            );
        }
    }
}
