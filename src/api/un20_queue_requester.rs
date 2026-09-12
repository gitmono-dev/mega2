//! UN-20: the merge-queue handlers capture the requesting subject.
//!
//! A queued merge is executed via MonoWriteQueue, so the subject that asked
//! for it is recorded on the `push_queue` row. The capture is *optional*:
//! `OptionalSessionUser` never rejects, so an anonymous enqueue stays possible
//! — this adds no new rejection surface to the endpoint.
//!
//! These cases drive the real routers through axum so the extractor wiring
//! itself is under test, not just the storage chain underneath it.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header::CONTENT_TYPE},
};
use git_internal::{
    hash::{ObjectHash, get_hash_kind},
    internal::object::{
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
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
    callisto::{push_queue, sea_orm_active_enums::PushQueueKindEnum},
    ceres::api_service::cache::GitObjectCache,
    common::utils::MEGA_BRANCH_NAME,
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
            // Lazy: a live Redis is not part of what these cases prove.
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un20-test".to_string(),
        }),
        listen_addr: "http://127.0.0.1:0".to_string(),
        entity_store: Arc::new(SharedEntityStore::new()),
        storage,
    }
}

fn blob_item(name: &str, hex: &str) -> TreeItem {
    TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_hex_for_kind(get_hash_kind(), hex).unwrap(),
        name.to_string(),
    )
}

/// A mergeable Open CL so `/merge-queue/add` can land via MonoWriteQueue.
async fn seed_mergeable_cl(storage: &Storage, link: &str) {
    let mono = storage.mono_storage();
    let old_tree = Tree::from_tree_items(vec![blob_item(
        ".gitkeep",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )])
    .expect("old tree");
    let new_tree = Tree::from_tree_items(vec![blob_item(
        "queued.txt",
        "cccccccccccccccccccccccccccccccccccccccc",
    )])
    .expect("new tree");
    let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
    let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
    mono.save_mega_trees(
        vec![old_tree.clone(), new_tree.clone()],
        old_commit.id,
        None,
    )
    .await
    .expect("save trees");
    mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
        .await
        .expect("save commits");
    mono.save_refs(
        crate::callisto::mega_refs::Model {
            id: 1,
            path: "/".to_string(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: old_commit.id.to_string(),
            ref_tree_hash: old_tree.id.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
        None,
    )
    .await
    .expect("save main ref");
    mono.save_or_update_cl_ref(
        "/",
        &format!("refs/cl/{link}"),
        &new_commit.id.to_string(),
        &new_tree.id.to_string(),
    )
    .await
    .expect("save cl ref");
    storage
        .cl_storage()
        .new_cl_model(
            "/",
            link,
            "un20",
            "main",
            &old_commit.id.to_string(),
            &new_commit.id.to_string(),
            "un20-author",
        )
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

async fn push_queue_merge_row(storage: &Storage, cl_link: &str) -> push_queue::Model {
    push_queue::Entity::find()
        .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Merge))
        .filter(push_queue::Column::OperationId.eq(cl_link))
        .one(storage.cl_storage().get_connection())
        .await
        .expect("read push_queue")
        .expect("merge row must exist")
}

#[tokio::test]
async fn un20_add_handler_records_the_session_subject() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_mergeable_cl(&storage, "UN20ADD").await;

    let state = api_state(
        storage.clone(),
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("queue-requester"),
        }),
    );
    assert_eq!(post_add(state, "UN20ADD").await, StatusCode::OK);

    let row = push_queue_merge_row(&storage, "UN20ADD").await;
    assert_eq!(
        row.requester.as_deref(),
        Some("queue-requester"),
        "the enqueue must record the session subject on push_queue"
    );
}

#[tokio::test]
async fn un20_add_handler_accepts_an_anonymous_request_and_records_null() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_mergeable_cl(&storage, "UN20ANONADD").await;

    let session_store = BrowserSessionStore::Counting(CountingSessionStore::new(vec![Ok(None)]));
    let state = api_state(storage.clone(), session_store);

    assert_eq!(
        post_add(state, "UN20ANONADD").await,
        StatusCode::OK,
        "an anonymous enqueue must still be served — the capture is optional"
    );
    let row = push_queue_merge_row(&storage, "UN20ANONADD").await;
    let payload_requester = row
        .payload
        .get("requester")
        .and_then(|v| {
            if v.is_null() {
                Some(None)
            } else {
                v.as_str().map(|s| Some(s.to_string()))
            }
        })
        .unwrap_or(Some("missing".into()));
    assert_eq!(
        payload_requester, None,
        "an anonymous request is recorded as NULL in the merge payload, not as a made-up subject"
    );
}

#[tokio::test]
async fn un20_retry_handler_records_the_session_subject() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    seed_mergeable_cl(&storage, "UN20RETRYAPI").await;

    let state = api_state(
        storage.clone(),
        BrowserSessionStore::Fixed(FixedUserSessionStore {
            user: login_user("retry-requester"),
        }),
    );
    assert_eq!(post_retry(state, "UN20RETRYAPI").await, StatusCode::OK);

    let row = push_queue_merge_row(&storage, "UN20RETRYAPI").await;
    assert_eq!(
        row.requester.as_deref(),
        Some("retry-requester"),
        "the retry must record who asked for it on push_queue"
    );
}
