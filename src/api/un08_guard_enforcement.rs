//! UN-08: the `/api/v1` guard evaluates against the shared snapshot.
//!
//! These cases drive the real `cedar_guard` middleware over a real router, so
//! the wiring — enforcement mode, principal resolution, resource lookup through
//! `get_cl`, and the status code a denial produces — is what is under test.
//!
//! 403 and 401 are kept apart deliberately: a denial from the guard is 403
//! ("you are known and may not do this"), while 401 comes from a mandatory
//! extractor that could not authenticate the request at all.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
    routing::get,
};
use sea_orm::ConnectionTrait;
use tower::ServiceExt;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{
            SessionUser,
            api_store::{BrowserSessionStore, CountingSessionStore, FixedUserSessionStore},
            model::LoginUser,
        },
    },
    bellatrix::Bellatrix,
    ceres::api_service::cache::GitObjectCache,
    contract::policy::{
        entitystore::{SharedEntityStore, generate_entity},
        guard::cedar_guard::cedar_guard,
    },
    jupiter::storage::{Storage, base_storage::StorageConnector},
};

fn login_user(name: &str) -> LoginUser {
    LoginUser {
        username: name.to_string(),
        ..Default::default()
    }
}

/// Storage whose `[cedar].enforcement` is the given mode.
async fn storage_with_enforcement(temp: &std::path::Path, mode: &str) -> Storage {
    let mut storage = crate::jupiter::tests::test_storage(temp).await;
    let mut config = (*storage.config).clone();
    config.cedar.enforcement = mode.to_string();
    let config = Arc::new(config);
    // `Storage::config()` reads through the handle, so both have to be replaced
    // or the override is silently ignored.
    storage.config_handle = crate::config::reload::ConfigHandle::from_arc(config.clone());
    storage.config = config;
    storage
}

fn api_state(
    storage: Storage,
    session_store: BrowserSessionStore,
    entity_store: Arc<SharedEntityStore>,
) -> MonoApiServiceState {
    MonoApiServiceState {
        session_store,
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un08-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store,
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
        storage,
    }
}

/// A snapshot in which `admin` is the only admin of the root repository.
fn snapshot_store(admin: &str) -> Arc<SharedEntityStore> {
    let store = Arc::new(SharedEntityStore::new());
    let json = generate_entity(&[admin.to_string()], "/").expect("generate authz json");
    store.ensure(&json).expect("build snapshot");
    store
}

async fn seed_open_cl(storage: &Storage, id: i64, cl_link: &str, path: &str) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{cl_link}', 'un08', 'open', '{path}', 'from', 'to', now(), now(), \
             'un08-author', 'main')"
        ))
        .await
        .expect("seed open CL");
}

/// A guarded route shaped like the real one: the guard runs as middleware, and
/// the handler below it only reports that it was reached.
fn guarded_router(state: MonoApiServiceState) -> Router {
    async fn reached() -> &'static str {
        "reached"
    }
    Router::new()
        .route("/api/v1/cl/{link}/detail", get(reached))
        .layer(middleware::from_fn_with_state(state.clone(), cedar_guard))
        .with_state(state)
}

/// A route whose handler requires a session, so an anonymous request is a 401
/// from the extractor rather than a guard decision.
fn mandatory_auth_router(state: MonoApiServiceState) -> Router {
    async fn whoami(SessionUser(user): SessionUser) -> String {
        user.username
    }
    Router::new()
        .route("/api/v1/cl/{link}/detail", get(whoami))
        .layer(middleware::from_fn_with_state(state.clone(), cedar_guard))
        .with_state(state)
}

/// Requests use the production path shape (`/api/v1/...`), since that prefix is
/// exactly what the guard has to strip before it can match anything.
async fn get_detail(router: Router, link: &str) -> StatusCode {
    router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/cl/{link}/detail"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

#[tokio::test]
async fn un08_enforce_denies_an_unauthorized_principal_with_403() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_001, "UN08DENY", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("outsider"),
        }),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08DENY").await,
        StatusCode::FORBIDDEN,
        "a known principal without permission gets 403, not 401"
    );
}

#[tokio::test]
async fn un08_enforce_allows_an_authorized_principal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_002, "UN08ALLOW", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("un08-admin"),
        }),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08ALLOW").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn un08_shadow_allows_what_enforce_would_deny() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "shadow").await;
    seed_open_cl(&storage, 990_003, "UN08SHADOW", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("outsider"),
        }),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08SHADOW").await,
        StatusCode::OK,
        "shadow evaluates and records, but never changes the allow decision"
    );
}

/// `off` must be a genuine short circuit, not "evaluate and then allow anyway".
/// A 200 alone cannot tell those apart, so this counts session lookups: under
/// `off` the guard returns before it ever resolves a principal, while the same
/// request under `enforce` resolves one.
#[tokio::test]
async fn un08_off_short_circuits_before_consuming_anything() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_open_cl(&storage, 990_004, "UN08OFF", "/").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("outsider")))]);
    // No snapshot was ever built: under `off` that must not matter either.
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08OFF").await,
        StatusCode::OK,
        "off is a short circuit: no build, no consume, no behavior change"
    );
    assert_eq!(
        counting.call_count(),
        0,
        "off must not even resolve a principal — an 'evaluate then allow' \
         implementation would have"
    );
}

/// The discriminating counterpart: the same request under `enforce` does
/// resolve a principal, so the assertion above is about `off` specifically.
#[tokio::test]
async fn un08_enforce_does_consume_the_principal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_007, "UN08CONSUME", "/").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("un08-admin")))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08CONSUME").await,
        StatusCode::OK
    );
    assert_eq!(
        counting.call_count(),
        1,
        "enforce resolves the principal exactly once (UN-22)"
    );
}

/// A request that arrives before the snapshot exists is a would-deny like any
/// other: denied under `enforce`, allowed under `shadow`.
#[tokio::test]
async fn un08_a_missing_snapshot_is_fail_closed_under_enforce() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_008, "UN08NOSNAP", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("un08-admin"),
        }),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08NOSNAP").await,
        StatusCode::FORBIDDEN,
        "no snapshot means nothing can be authorized"
    );
}

#[tokio::test]
async fn un08_a_missing_snapshot_still_allows_under_shadow() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "shadow").await;
    seed_open_cl(&storage, 990_009, "UN08NOSNAPSHADOW", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("un08-admin"),
        }),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08NOSNAPSHADOW").await,
        StatusCode::OK,
        "shadow records the would-deny but never changes the allow decision"
    );
}

#[tokio::test]
async fn un08_enforce_denies_an_unknown_cl_link() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    // Deliberately seed nothing: the link cannot be resolved to a repository.

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("un08-admin"),
        }),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "NOSUCHLINK").await,
        StatusCode::FORBIDDEN,
        "an unresolvable resource is fail-closed, even for an admin"
    );
}

#[tokio::test]
async fn un08_enforce_denies_an_anonymous_request_without_pretending_it_is_unauthenticated() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_005, "UN08ANON", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
        snapshot_store("un08-admin"),
    );

    assert_eq!(
        get_detail(guarded_router(state), "UN08ANON").await,
        StatusCode::FORBIDDEN,
        "the guard denies the anonymous principal with 403; 401 is the extractor's answer"
    );
}

#[tokio::test]
async fn un08_a_mandatory_extractor_still_answers_401_for_an_anonymous_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    // `off` so the guard passes the request straight through and the 401 can
    // only come from the handler's mandatory `SessionUser`.
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_open_cl(&storage, 990_006, "UN08401", "/").await;

    let state = api_state(
        storage,
        BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(mandatory_auth_router(state), "UN08401").await,
        StatusCode::UNAUTHORIZED,
        "authentication failure stays 401 — it is a different problem from a denial"
    );
}
