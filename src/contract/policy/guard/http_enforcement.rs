//! HTTP wiring of `cedar_guard`.
//!
//! These cases drive the real middleware over a real router, so enforcement
//! mode, principal resolution, resource lookup through `get_cl`, and the
//! status code a denial produces are what is under test. The handler below
//! the guard only reports that it was reached.
//!
//! 403 and 401 stay apart: a denial is 403 ("you are known and may not do
//! this"), while 401 comes from a mandatory extractor that could not
//! authenticate the request at all.
//!
//! `merge` and `merge-no-auth` are both mapped to `approveMergeRequest`. An
//! anonymous caller is authorized as the reserved anonymous principal. A 401
//! there would answer an authorization question with an authentication
//! status, and a missing session can produce that status on its own.
//!
//! Every action in the schema takes a `User` principal. A bot is
//! `Bot::"<id>"`, which is not that type, so under `enforce` a bot request
//! to a protected CL endpoint is refused. `shadow` records the would-deny
//! and still serves the request. A full bot authorization model (schema
//! plus identity mapping) is deferred. A bot request must never consult the
//! browser-session store; these cases count those lookups.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware,
    routing::{get, post},
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
    callisto::sea_orm_active_enums::{BotStatusEnum, PermissionScopeEnum},
    ceres::api_service::cache::GitObjectCache,
    contract::policy::{
        entitystore::{SharedEntityStore, generate_entity},
        guard::cedar_guard::cedar_guard,
    },
    jupiter::storage::{Storage, base_storage::StorageConnector},
};

/// Roles of the seeded ACL: admin and maintainer may approve a merge.
const MERGE_ADMIN: &str = "merge-admin";
const MERGE_MAINTAINER: &str = "merge-maintainer";
const MERGE_READER: &str = "merge-reader";

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
            prefix: "guard-http-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store,
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

async fn seed_open_cl(storage: &Storage, id: i64, cl_link: &str) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{cl_link}', 'open', 'open', '/', 'from', 'to', now(), now(), \
             'guard-http', 'main')"
        ))
        .await
        .expect("seed open CL");
}

fn fixed_session(username: &str) -> BrowserSessionStore {
    BrowserSessionStore::Fixed(FixedUserSessionStore {
        user: login_user(username),
    })
}

fn anonymous_session() -> BrowserSessionStore {
    BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)]))
}

/// A guarded route shaped like the real detail route.
fn guarded_detail_router(state: MonoApiServiceState) -> Router {
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

async fn get_detail_as_bearer(router: Router, link: &str, token: &str) -> StatusCode {
    router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/cl/{link}/detail"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

#[tokio::test]
async fn enforce_denies_an_unauthorized_principal_with_403() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_001, "GUARDDENY").await;

    let state = api_state(
        storage,
        fixed_session("outsider"),
        snapshot_store("guard-admin"),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDDENY").await,
        StatusCode::FORBIDDEN,
        "a known principal without permission gets 403, not 401"
    );
}

#[tokio::test]
async fn enforce_allows_an_authorized_principal() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_002, "GUARDALLOW").await;

    let state = api_state(
        storage,
        fixed_session("guard-admin"),
        snapshot_store("guard-admin"),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDALLOW").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn shadow_allows_what_enforce_would_deny() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "shadow").await;
    seed_open_cl(&storage, 990_003, "GUARDSHADOW").await;

    let state = api_state(
        storage,
        fixed_session("outsider"),
        snapshot_store("guard-admin"),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDSHADOW").await,
        StatusCode::OK,
        "shadow evaluates and records, but never changes the allow decision"
    );
}

/// `off` must be a genuine short circuit, not "evaluate and then allow anyway".
/// A 200 alone cannot tell those apart, so this counts session lookups: under
/// `off` the guard returns before it ever resolves a principal, while the same
/// request under `enforce` resolves one.
#[tokio::test]
async fn off_short_circuits_before_consuming_anything() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_open_cl(&storage, 990_004, "GUARDOFF").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("outsider")))]);
    // No snapshot was ever built: under `off` that must not matter either.
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDOFF").await,
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
async fn enforce_resolves_the_principal_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_007, "GUARDCONSUME").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("guard-admin")))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store("guard-admin"),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDCONSUME").await,
        StatusCode::OK
    );
    assert_eq!(
        counting.call_count(),
        1,
        "enforce resolves the principal exactly once"
    );
}

/// A request that arrives before the snapshot exists is a would-deny like any
/// other: denied under `enforce`, allowed under `shadow`.
#[tokio::test]
async fn missing_snapshot_is_fail_closed_under_enforce() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_008, "GUARDNOSNAP").await;

    let state = api_state(
        storage,
        fixed_session("guard-admin"),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDNOSNAP").await,
        StatusCode::FORBIDDEN,
        "no snapshot means nothing can be authorized"
    );
}

#[tokio::test]
async fn missing_snapshot_still_allows_under_shadow() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "shadow").await;
    seed_open_cl(&storage, 990_009, "GUARDNOSNAPSHADOW").await;

    let state = api_state(
        storage,
        fixed_session("guard-admin"),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDNOSNAPSHADOW").await,
        StatusCode::OK,
        "shadow records the would-deny but never changes the allow decision"
    );
}

#[tokio::test]
async fn enforce_denies_an_unknown_cl_link() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;

    let state = api_state(
        storage,
        fixed_session("guard-admin"),
        snapshot_store("guard-admin"),
    );

    assert_eq!(
        get_detail(guarded_detail_router(state), "NOSUCHLINK").await,
        StatusCode::FORBIDDEN,
        "an unresolvable resource is fail-closed, even for an admin"
    );
}

#[tokio::test]
async fn enforce_denies_anonymous_without_pretending_it_is_unauthenticated() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 990_005, "GUARDANON").await;

    let state = api_state(storage, anonymous_session(), snapshot_store("guard-admin"));

    assert_eq!(
        get_detail(guarded_detail_router(state), "GUARDANON").await,
        StatusCode::FORBIDDEN,
        "the guard denies the anonymous principal with 403; 401 is the extractor's answer"
    );
}

#[tokio::test]
async fn mandatory_extractor_answers_401_for_an_anonymous_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    // `off` so the guard passes the request straight through and the 401 can
    // only come from the handler's mandatory `SessionUser`.
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_open_cl(&storage, 990_006, "GUARD401").await;

    let state = api_state(
        storage,
        anonymous_session(),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail(mandatory_auth_router(state), "GUARD401").await,
        StatusCode::UNAUTHORIZED,
        "authentication failure stays 401 — it is a different problem from a denial"
    );
}

/// An ACL with one member per role, so the matrix exercises the real group
/// hierarchy rather than "admin vs nobody".
fn role_acl_json() -> String {
    serde_json::json!({
        "users": {
            format!("User::\"{MERGE_ADMIN}\""): {
                "euid": format!("User::\"{MERGE_ADMIN}\""),
                "parents": ["UserGroup::\"admin\""]
            },
            format!("User::\"{MERGE_MAINTAINER}\""): {
                "euid": format!("User::\"{MERGE_MAINTAINER}\""),
                "parents": ["UserGroup::\"matainer\""]
            },
            format!("User::\"{MERGE_READER}\""): {
                "euid": format!("User::\"{MERGE_READER}\""),
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

fn session_for(role: Option<&str>) -> BrowserSessionStore {
    match role {
        Some(username) => fixed_session(username),
        None => anonymous_session(),
    }
}

/// Route the guard in front of a stub handler: this asserts the authorization
/// decision, not the merge itself (which needs a real ref graph).
fn guarded_post(state: MonoApiServiceState, path: &'static str) -> Router {
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

/// `(role, expected)` for `approveMergeRequest` under the current policy: it
/// is a maintainer-level action, and admin inherits it through the group
/// hierarchy. A reader may not approve, and an anonymous caller is denied as
/// a principal — 403, not the 401 of an unauthenticated request.
const ROLE_MATRIX: &[(Option<&str>, StatusCode)] = &[
    (Some(MERGE_ADMIN), StatusCode::OK),
    (Some(MERGE_MAINTAINER), StatusCode::OK),
    (Some(MERGE_READER), StatusCode::FORBIDDEN),
    (None, StatusCode::FORBIDDEN),
];

#[tokio::test]
async fn merge_role_matrix() {
    for (index, (role, expected)) in ROLE_MATRIX.iter().enumerate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_with_enforcement(temp.path(), "enforce").await;
        let link = format!("MERGE{index}");
        seed_open_cl(&storage, 700_100 + index as i64, &link).await;

        let state = api_state(storage, session_for(*role), role_snapshot());
        let status = post_status(
            guarded_post(state, "/api/v1/cl/{link}/merge"),
            format!("/api/v1/cl/{link}/merge"),
        )
        .await;

        assert_eq!(status, *expected, "merge as {role:?} must be {expected}");
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
async fn merge_no_auth_role_matrix() {
    for (index, (role, expected)) in ROLE_MATRIX.iter().enumerate() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_with_enforcement(temp.path(), "enforce").await;
        let link = format!("MERGENOAUTH{index}");
        seed_open_cl(&storage, 700_200 + index as i64, &link).await;

        let state = api_state(storage, session_for(*role), role_snapshot());
        let status = post_status(
            guarded_post(state, "/api/v1/cl/{link}/merge-no-auth"),
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

/// A real, non-revoked bot token. The guard resolves bots through storage, so
/// a made-up string would just be "no bot" and prove nothing.
async fn seed_bot_token(storage: &Storage) -> String {
    // The token hash is HMAC-keyed; the key comes from the environment and has
    // a minimum length.
    unsafe {
        std::env::set_var(
            "MEGA_BOT_TOKEN_HMAC_SECRET",
            "guard-http-test-secret-at-least-32-characters",
        )
    };
    let bot = storage
        .bots_storage()
        .new_bot_model(
            "guard-http-bot",
            None,
            1,
            PermissionScopeEnum::Read,
            BotStatusEnum::Enabled,
        )
        .await
        .expect("create bot");
    let (_token, plain) = storage
        .bots_storage()
        .generate_bot_token(bot.id, "guard-http", None)
        .await
        .expect("issue bot token");
    plain
}

#[tokio::test]
async fn bot_is_denied_under_enforce() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 730_001, "BOTDENY").await;
    let token = seed_bot_token(&storage).await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("bot-admin")))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store("bot-admin"),
    );

    assert_eq!(
        get_detail_as_bearer(guarded_detail_router(state), "BOTDENY", &token).await,
        StatusCode::FORBIDDEN,
        "the schema has no Bot principal, so a bot cannot be authorized"
    );
    assert_eq!(
        counting.call_count(),
        0,
        "a bot request must not consult the browser-session store — and must not \
         fall back to whatever session happens to be resolvable"
    );
}

#[tokio::test]
async fn bot_still_passes_under_shadow() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "shadow").await;
    seed_open_cl(&storage, 730_002, "BOTSHADOW").await;
    let token = seed_bot_token(&storage).await;

    let counting = CountingSessionStore::new(vec![Ok(None)]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store("bot-admin"),
    );

    assert_eq!(
        get_detail_as_bearer(guarded_detail_router(state), "BOTSHADOW", &token).await,
        StatusCode::OK,
        "shadow records the would-deny but keeps serving — that window is how \
         bot owners migrate before enforce"
    );
    assert_eq!(counting.call_count(), 0);
}

#[tokio::test]
async fn bot_is_unaffected_under_off() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_open_cl(&storage, 730_003, "BOTOFF").await;
    let token = seed_bot_token(&storage).await;

    let state = api_state(
        storage,
        anonymous_session(),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_detail_as_bearer(guarded_detail_router(state), "BOTOFF", &token).await,
        StatusCode::OK,
        "off is a short circuit for every principal kind"
    );
}

/// A bearer token that is not a bot token must not be mistaken for one: the
/// request falls through to session resolution, which does get consulted.
#[tokio::test]
async fn non_bot_bearer_falls_through_to_session_resolution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_open_cl(&storage, 730_004, "NOTBOT").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(login_user("bot-admin")))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store("bot-admin"),
    );

    assert_eq!(
        get_detail_as_bearer(guarded_detail_router(state), "NOTBOT", "not-a-bot-token").await,
        StatusCode::OK,
        "the admin session authorizes the request"
    );
    assert_eq!(
        counting.call_count(),
        1,
        "a non-bot request resolves its session exactly once"
    );
}
