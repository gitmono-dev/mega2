//! UN-27: what a bot token means to the authorization guard.
//!
//! Every action in `mega.cedarschema` takes a `User` principal. A bot is
//! `Bot::"<id>"`, which is not that type, so under `enforce` a bot request to a
//! protected CL endpoint cannot be authorized — and must be refused rather than
//! silently allowed. `shadow` records the would-deny and still serves the
//! request, which is the window bot owners have to move to a user token before
//! enforcement is switched on. A full bot authorization model (schema plus
//! identity mapping) is deferred.
//!
//! The other half of the contract is negative: a bot request must never consult
//! the browser-session store. These cases count those lookups rather than
//! trusting the ordering of the code.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware,
    routing::get,
};
use sea_orm::ConnectionTrait;
use tower::ServiceExt;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{
            api_store::{BrowserSessionStore, CountingSessionStore},
            model::LoginUser,
        },
    },
    bellatrix::Bellatrix,
    callisto::sea_orm_active_enums::{BotStatusEnum, PermissionScopeEnum},
    ceres::api_service::cache::GitObjectCache,
    contract::policy::{
        entitystore::{SharedEntityStore, generate_entity},
        guard::cedar_guard::cedar_guard,
    },
    jupiter::storage::{Storage, base_storage::StorageConnector},
};

async fn storage_in(temp: &std::path::Path, mode: &str) -> Storage {
    let mut storage = crate::jupiter::tests::test_storage(temp).await;
    let mut config = (*storage.config).clone();
    config.cedar.enforcement = mode.to_string();
    let config = Arc::new(config);
    storage.config_handle = crate::config::reload::ConfigHandle::from_arc(config.clone());
    storage.config = config;
    storage
}

fn snapshot_store() -> Arc<SharedEntityStore> {
    let store = Arc::new(SharedEntityStore::new());
    store
        .ensure(&generate_entity(&["un27-admin".to_string()], "/").expect("init product"))
        .expect("build snapshot");
    store
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
            prefix: "un27-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store,
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
             VALUES ({id}, '{cl_link}', 'un27', 'open', '/', 'from', 'to', now(), now(), \
             'un27-author', 'main')"
        ))
        .await
        .expect("seed CL");
}

/// A real, non-revoked bot token — the guard resolves bots through storage, so
/// a made-up string would just be "no bot" and prove nothing.
async fn seed_bot_token(storage: &Storage) -> String {
    // The token hash is HMAC-keyed; the key comes from the environment and has
    // a minimum length.
    unsafe {
        std::env::set_var(
            "MEGA_BOT_TOKEN_HMAC_SECRET",
            "un27-test-secret-at-least-32-characters-long",
        )
    };
    let bot = storage
        .bots_storage()
        .new_bot_model(
            "un27-bot",
            None,
            1,
            PermissionScopeEnum::Read,
            BotStatusEnum::Enabled,
        )
        .await
        .expect("create bot");
    let (_token, plain) = storage
        .bots_storage()
        .generate_bot_token(bot.id, "un27", None)
        .await
        .expect("issue bot token");
    plain
}

fn guarded_router(state: MonoApiServiceState) -> Router {
    async fn reached() -> &'static str {
        "reached"
    }
    Router::new()
        .route("/api/v1/cl/{link}/detail", get(reached))
        .layer(middleware::from_fn_with_state(state.clone(), cedar_guard))
        .with_state(state)
}

async fn get_as_bot(router: Router, link: &str, token: &str) -> StatusCode {
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
async fn un27_bot_is_denied_under_enforce() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_open_cl(&storage, 730_001, "UN27DENY").await;
    let token = seed_bot_token(&storage).await;

    let counting = CountingSessionStore::new(vec![Ok(Some(LoginUser {
        username: "un27-admin".to_string(),
        ..Default::default()
    }))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store(),
    );

    assert_eq!(
        get_as_bot(guarded_router(state), "UN27DENY", &token).await,
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
async fn un27_bot_still_passes_under_shadow() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "shadow").await;
    seed_open_cl(&storage, 730_002, "UN27SHADOW").await;
    let token = seed_bot_token(&storage).await;

    let counting = CountingSessionStore::new(vec![Ok(None)]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store(),
    );

    assert_eq!(
        get_as_bot(guarded_router(state), "UN27SHADOW", &token).await,
        StatusCode::OK,
        "shadow records the would-deny but keeps serving — that window is how \
         bot owners migrate before enforce"
    );
    assert_eq!(counting.call_count(), 0);
}

#[tokio::test]
async fn un27_bot_is_unaffected_under_off() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "off").await;
    seed_open_cl(&storage, 730_003, "UN27OFF").await;
    let token = seed_bot_token(&storage).await;

    let state = api_state(
        storage,
        BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
        Arc::new(SharedEntityStore::new()),
    );

    assert_eq!(
        get_as_bot(guarded_router(state), "UN27OFF", &token).await,
        StatusCode::OK,
        "off is a short circuit for every principal kind"
    );
}

/// A bearer token that is not a bot token must not be mistaken for one: the
/// request falls through to session resolution, which does get consulted.
#[tokio::test]
async fn un27_a_non_bot_bearer_falls_through_to_session_resolution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_open_cl(&storage, 730_004, "UN27NOTBOT").await;

    let counting = CountingSessionStore::new(vec![Ok(Some(LoginUser {
        username: "un27-admin".to_string(),
        ..Default::default()
    }))]);
    let state = api_state(
        storage,
        BrowserSessionStore::Counting(counting.clone()),
        snapshot_store(),
    );

    assert_eq!(
        get_as_bot(guarded_router(state), "UN27NOTBOT", "not-a-bot-token").await,
        StatusCode::OK,
        "the admin session authorizes the request"
    );
    assert_eq!(
        counting.call_count(),
        1,
        "a non-bot request resolves its session exactly once (UN-22)"
    );
}
