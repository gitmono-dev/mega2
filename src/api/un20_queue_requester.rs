//! UN-20: the merge-queue handlers capture the requesting subject.
//!
//! A queued merge is executed later by a background worker, so the subject that
//! asked for it must be recorded with the queue row. The capture is *optional*:
//! `OptionalSessionUser` never rejects, so an anonymous enqueue stays possible
//! and is stored as NULL — this adds no new rejection surface to the endpoint.
//!
//! These cases drive the real routers through axum so the extractor wiring
//! itself is under test, not just the storage chain underneath it.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header::CONTENT_TYPE},
};
use sea_orm::ConnectionTrait;
use tower::ServiceExt;
use utoipa_axum::router::OpenApiRouter;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{
            api_store::{BrowserSessionStore, CountingSessionStore, FixedUserSessionStore},
            model::LoginUser,
        },
        router::merge_queue_router,
    },
    bellatrix::Bellatrix,
    ceres::api_service::cache::GitObjectCache,
    contract::policy::entitystore::SharedEntityStore,
    jupiter::storage::{Storage, base_storage::StorageConnector},
};

fn login_user(name: &str) -> LoginUser {
    LoginUser {
        username: name.to_string(),
        ..Default::default()
    }
}

fn api_state(storage: Storage, session_store: BrowserSessionStore) -> MonoApiServiceState {
    MonoApiServiceState {
        session_store,
        git_object_cache: Arc::new(GitObjectCache {
            // Lazy: these cases never touch the cache, and a live Redis is not
            // part of what they prove.
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un20-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store: Arc::new(SharedEntityStore::new()),
        bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
        storage,
    }
}

/// An open CL is the precondition for enqueuing; seed one directly so the case
/// stays about the requester capture.
async fn seed_open_cl(storage: &Storage, id: i64, cl_link: &str) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{cl_link}', 'un20', 'open', '/', 'from', 'to', now(), now(), \
             'un20-author', 'main')"
        ))
        .await
        .expect("seed open CL");
}

fn router(state: MonoApiServiceState) -> axum::Router {
    let (router, _api) = OpenApiRouter::new()
        .merge(merge_queue_router::routers())
        .split_for_parts();
    router.with_state(state)
}

async fn post_add(state: MonoApiServiceState, cl_link: &str) -> StatusCode {
    router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/merge-queue/add")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"cl_link":"{cl_link}"}}"#)))
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

async fn post_retry(state: MonoApiServiceState, cl_link: &str) -> StatusCode {
    router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/merge-queue/retry/{cl_link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

#[tokio::test]
async fn un20_add_handler_records_the_session_subject() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_open_cl(&storage, 980_001, "UN20ADD").await;

    let state = api_state(
        storage.clone(),
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("queue-requester"),
        }),
    );
    assert_eq!(post_add(state, "UN20ADD").await, StatusCode::OK);

    assert_eq!(
        storage
            .merge_queue_service
            .get_queue_requester("UN20ADD")
            .await
            .expect("read requester"),
        Some(Some("queue-requester".to_string())),
        "the enqueue must record the session subject"
    );
}

#[tokio::test]
async fn un20_add_handler_accepts_an_anonymous_request_and_records_null() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_open_cl(&storage, 980_002, "UN20ANONADD").await;

    // A store that reports "no session" — the anonymous path.
    let session_store = BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)]));
    let state = api_state(storage.clone(), session_store);

    assert_eq!(
        post_add(state, "UN20ANONADD").await,
        StatusCode::OK,
        "an anonymous enqueue must still be served — the capture is optional"
    );
    assert_eq!(
        storage
            .merge_queue_service
            .get_queue_requester("UN20ANONADD")
            .await
            .expect("read requester"),
        Some(None),
        "an anonymous request is recorded as NULL, not as a made-up subject"
    );
}

#[tokio::test]
async fn un20_retry_handler_records_the_session_subject() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_open_cl(&storage, 980_003, "UN20RETRYAPI").await;

    // Enqueue anonymously, then fail the item so it becomes retryable.
    storage
        .merge_queue_service
        .add_to_queue_with_requester("UN20RETRYAPI".to_string(), None)
        .await
        .expect("enqueue");
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(
            "UPDATE merge_queue SET status = 'failed' WHERE cl_link = 'UN20RETRYAPI'",
        )
        .await
        .expect("mark failed");

    let state = api_state(
        storage.clone(),
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("retry-requester"),
        }),
    );
    assert_eq!(post_retry(state, "UN20RETRYAPI").await, StatusCode::OK);

    assert_eq!(
        storage
            .merge_queue_service
            .get_queue_requester("UN20RETRYAPI")
            .await
            .expect("read requester"),
        Some(Some("retry-requester".to_string())),
        "the retry must record who asked for it"
    );
}
