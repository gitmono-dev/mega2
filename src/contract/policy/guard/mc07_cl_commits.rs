//! MC-07 (plan-20260827, REL-MC-01 family child 2/3): the Cedar authorization
//! surface for `GET /api/v1/cl/{link}/commits` — `guarded_endpoints.json`
//! maps it to `viewRepo` like the sibling `/{link}/detail`.
//!
//! These cases drive the real `cedar_guard` middleware over the real MC-05
//! endpoint, so the wiring under test is the mapping itself, principal
//! resolution, resource lookup through `get_cl`, and the status code each
//! state produces:
//!
//! - 401 = anonymous request, decided by the endpoint's mandatory session
//!   extractor (UN-23), exercised under `off` where the guard is inert;
//! - 403 = logged-in principal without the permission under `enforce`;
//! - 200 = an authorized principal under `enforce`, with the real listing
//!   payload coming back.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
};
use sea_orm::{ConnectionTrait, EntityTrait, IntoActiveModel};
use tempfile::TempDir;
use tower::ServiceExt;
use utoipa_axum::router::OpenApiRouter;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{
            api_store::{BrowserSessionStore, CountingSessionStore, FixedUserSessionStore},
            model::LoginUser,
        },
        router::cl_router,
    },
    bellatrix::Bellatrix,
    callisto::{mega_cl_commits, mega_commit},
    ceres::api_service::cache::GitObjectCache,
    contract::policy::{
        ActionEnum,
        entitystore::{SharedEntityStore, generate_entity},
        guard::cedar_guard::{cedar_guard, resolve_cl_action},
    },
    jupiter::{
        storage::{Storage, base_storage::StorageConnector},
        tests::test_storage,
    },
};

const ADMIN: &str = "mc07-admin";
const OUTSIDER: &str = "mc07-outsider";
const LINK: &str = "MC07C01";

fn login_user(name: &str) -> LoginUser {
    LoginUser {
        username: name.to_string(),
        ..Default::default()
    }
}

/// Storage whose `[cedar].enforcement` is the given mode (same shape as the
/// UN-08 guard cases).
async fn storage_with_enforcement(temp: &std::path::Path, mode: &str) -> Storage {
    let mut storage = test_storage(temp).await;
    let mut config = (*storage.config).clone();
    config.cedar.enforcement = mode.to_string();
    let config = Arc::new(config);
    // `Storage::config()` reads through the handle, so both have to be
    // replaced or the override is silently ignored.
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
            prefix: "mc07-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store,
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
        storage,
    }
}

/// A snapshot in which `mc07-admin` is the only admin of the root repository.
fn snapshot_store() -> Arc<SharedEntityStore> {
    let store = Arc::new(SharedEntityStore::new());
    let json = generate_entity(&[ADMIN.to_string()], "/").expect("generate authz json");
    store.ensure(&json).expect("build snapshot");
    store
}

fn mc07_sha(n: u64) -> String {
    format!("{n:040x}")
}

/// Seed the CL, its commit rows and a two-member listing, so the authorized
/// case returns the real payload rather than just any 200.
async fn seed_cl_with_listing(storage: &Storage) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES (770001, '{LINK}', 'mc07', 'open', '/', '{}', '{}', now(), now(), \
             '{ADMIN}', 'main')",
            mc07_sha(100),
            mc07_sha(102)
        ))
        .await
        .expect("seed open CL");

    let mono_storage = storage.mono_storage();
    let conn = mono_storage.get_connection();
    let rows: Vec<mega_commit::ActiveModel> = (100..=102u64)
        .map(|n| {
            let parents: Vec<String> = if n == 100 {
                vec![]
            } else {
                vec![mc07_sha(n - 1)]
            };
            mega_commit::Model {
                id: crate::callisto::entity_ext::generate_id()
                    .expect("test ID generator initialized"),
                commit_id: mc07_sha(n),
                tree: mc07_sha(900_000),
                parents_id: serde_json::json!(parents),
                author: Some("author MC07 User <mc07@example.invalid> 1750000000 +0000".into()),
                committer: Some(
                    "committer MC07 User <mc07@example.invalid> 1750000000 +0000".into(),
                ),
                content: Some(format!("mc07 message {n}")),
                created_at: chrono::Utc::now().naive_utc(),
                pack_id: String::new(),
                pack_offset: 0,
            }
            .into_active_model()
        })
        .collect();
    mega_commit::Entity::insert_many(rows)
        .exec(conn)
        .await
        .expect("insert commits");

    let now = chrono::Utc::now().naive_utc();
    let listing: Vec<mega_cl_commits::ActiveModel> = [101u64, 102]
        .iter()
        .map(|&n| mega_cl_commits::ActiveModel {
            cl_link: sea_orm::Set(LINK.to_string()),
            commit_sha: sea_orm::Set(mc07_sha(n)),
            author_name: sea_orm::Set("MC07 User".to_string()),
            author_email: sea_orm::Set("mc07@example.invalid".to_string()),
            message: sea_orm::Set(format!("mc07 message {n}")),
            created_at: sea_orm::Set(now),
            updated_at: sea_orm::Set(now),
        })
        .collect();
    mega_cl_commits::Entity::insert_many(listing)
        .exec(conn)
        .await
        .expect("seed listing");
}

/// The production mount shape: the real MC-05 router nested under `/api/v1`
/// (the guard strips exactly that prefix before matching), with the guard as
/// the route layer — the same wiring as `http_server.rs`.
fn guarded_router(state: MonoApiServiceState) -> Router {
    let (api_router, _api) = OpenApiRouter::new()
        .merge(cl_router::routers())
        .split_for_parts();
    Router::new()
        .nest("/api/v1", api_router.with_state(state.clone()))
        .layer(middleware::from_fn_with_state(state.clone(), cedar_guard))
        .with_state(state)
}

async fn get_commits(router: Router, link: &str) -> (StatusCode, String) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/cl/{link}/commits"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// AC②: an anonymous request gets 401 — decided by the endpoint's mandatory
/// session extractor (UN-23), exercised under `off` where the guard is inert.
/// (Under `enforce` the same request is the guard's 403: anonymous requests
/// resolve to the reserved anonymous principal (UN-22) and a denial is 403
/// (UN-23); UN-24 is only the adjacent precedent of bringing another entry
/// point under authorization.)
#[tokio::test]
async fn cl_commits_anonymous_gets_401_without_enforcement() {
    let temp = TempDir::new().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "off").await;
    seed_cl_with_listing(&storage).await;

    let state = api_state(
        storage,
        BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)])),
        snapshot_store(),
    );

    let (status, body) = get_commits(guarded_router(state), LINK).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an anonymous request must be a 401 from the session extractor; body: {body}"
    );
}

/// AC③: a logged-in principal without the permission gets 403 under
/// `enforce` (the guard's decision, before the handler runs).
#[tokio::test]
async fn cl_commits_enforce_denies_logged_in_outsider_with_403() {
    let temp = TempDir::new().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_cl_with_listing(&storage).await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user(OUTSIDER),
        }),
        snapshot_store(),
    );

    let (status, body) = get_commits(guarded_router(state), LINK).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a known principal without permission gets the guard's 403; body: {body}"
    );
}

/// AC④: an authorized principal gets 200 under `enforce` — with the real
/// listing payload (chain order intact) coming through the MC-05 endpoint.
#[tokio::test]
async fn cl_commits_enforce_allows_authorized_admin_with_200() {
    let temp = TempDir::new().expect("temp dir");
    let storage = storage_with_enforcement(temp.path(), "enforce").await;
    seed_cl_with_listing(&storage).await;

    let state = api_state(
        storage,
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user(ADMIN),
        }),
        snapshot_store(),
    );

    let (status, body) = get_commits(guarded_router(state), LINK).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the admin must be allowed; body: {body}"
    );
    let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(json["req_result"], true);
    let shas: Vec<&str> = json["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|c| c["sha"].as_str().unwrap())
        .collect();
    assert_eq!(
        shas,
        vec![mc07_sha(101), mc07_sha(102)],
        "the authorized response carries the real listing in chain order"
    );
}

/// AC① (Codex R1 P1): the mapping resolves precisely — the guard's own
/// resolver turns the production request path into `ViewRepo` with the CL
/// link extracted, so this case cannot pass while the mapping points at a
/// write action (an admin inherits read + maintainer rights through the group
/// hierarchy, which is why the three-state cases alone cannot tell
/// `viewRepo` apart from `editMergeRequest`/`approveMergeRequest`).
///
/// The UN-23 fixed matrix lives in `cedar_guard.rs` (in this card's write
/// set only for the one added matrix row); this is its equivalent assertion
/// for the new row.
#[test]
fn cl_commits_mapping_resolves_to_view_repo_with_link() {
    let (action, link) =
        resolve_cl_action("GET", "/api/v1/cl/MC07C01/commits").expect("resolver must not error");
    assert_eq!(action, ActionEnum::ViewRepo, "the mapping must be ViewRepo");
    assert_eq!(link, "MC07C01", "the CL link must be extracted");

    // Same resolution with the prefix already stripped (the guard strips
    // `/api/v1` before matching — both shapes must agree).
    let (action2, link2) = resolve_cl_action("GET", "/cl/MC07C01/commits").expect("resolver");
    assert_eq!((action2, link2), (action, link));
}
