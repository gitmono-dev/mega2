//! UN-25: freezing a queued merge whose authorization cannot be decided.
//!
//! A frozen item is not a rejected change — nothing was wrong with it, only
//! with the ability to decide it. That distinction has to survive in the stored
//! row, because it is what tells an operator to retry rather than escalate:
//!
//! * the state reuses the queue's existing terminal pair (`Failed` +
//!   `SystemError`) so the existing retry entry point keeps working and no new
//!   enum value or migration is needed;
//! * the recorded `requester` survives — it is the subject the merge will be
//!   re-decided against, and dropping it would turn a retry into an
//!   unattributed merge;
//! * `error_message` states the condition for a successful retry.

use std::sync::Arc;

use sea_orm::ConnectionTrait;

use crate::{
    callisto::sea_orm_active_enums::{
        PushQueueFailureEnum, PushQueueKindEnum, PushQueueStatusEnum,
    },
    ceres::api_service::{cache::GitObjectCache, mono_api_service::MonoApiService},
    jupiter::storage::{
        Storage,
        base_storage::StorageConnector,
        push_queue_storage::{EnqueueOutcome, EnqueueParams},
    },
};

fn service(storage: &Storage) -> MonoApiService {
    MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un25-test".to_string(),
        }),
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
             VALUES ({id}, '{cl_link}', 'un25', 'open', '/', 'from', 'to', now(), now(), \
             'un25-author', 'main')"
        ))
        .await
        .expect("seed open CL");
}

async fn queued_item(storage: &Storage, id: i64, cl_link: &str, requester: Option<&str>) {
    seed_open_cl(storage, id, cl_link).await;
    let outcome = storage
        .push_queue_storage()
        .enqueue_atomic(EnqueueParams {
            kind: PushQueueKindEnum::Merge,
            operation_id: cl_link,
            path: "/",
            old_id: "from",
            new_id: "to",
            requester,
            payload: serde_json::json!({
                "cl_link": cl_link,
                "authz_principal": requester.unwrap_or("system"),
                "execution_actor": "system",
                "apply_queue_execution_decision": true,
                "requester": requester,
            }),
        })
        .await
        .expect("enqueue");
    assert!(
        matches!(outcome, EnqueueOutcome::Inserted { .. }),
        "expected insert, got {outcome:?}"
    );
}

async fn merge_row(storage: &Storage, cl_link: &str) -> crate::callisto::push_queue::Model {
    storage
        .push_queue_storage()
        .list_by_kind_and_operation(PushQueueKindEnum::Merge, cl_link)
        .await
        .expect("read item")
        .into_iter()
        .next()
        .expect("the item exists")
}

/// Capture what the freeze writes to the log, so the alert is asserted rather
/// than assumed.
///
/// The whole call — runtime included — happens inside the closure, so the
/// subscriber is unambiguously the default for the thread that emits the event.
/// Installing a thread-local default *around* an already-running async test is
/// not reliable: the future can be polled elsewhere and the record goes to the
/// global subscriber instead.
fn capture_tracing<F>(f: F) -> String
where
    F: FnOnce(),
{
    use std::{io::Write, sync::Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct TestWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for TestWriter {
        type Writer = TestWriter;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(TestWriter(buf.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    String::from_utf8(buf.lock().unwrap().clone()).expect("utf8")
}

#[tokio::test]
async fn un25_freeze_writes_every_field_the_contract_promises() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    queued_item(&storage, 710_001, "UN25FREEZE", Some("un25-requester")).await;

    assert!(
        service(&storage)
            .freeze_merge_queue_item_for_authz("UN25FREEZE", "snapshot dirty")
            .await
            .expect("freeze"),
        "a queued item must be freezable"
    );

    let item = merge_row(&storage, "UN25FREEZE").await;

    assert_eq!(item.status, PushQueueStatusEnum::Failed, "state");
    assert_eq!(
        item.failure_type,
        Some(PushQueueFailureEnum::SystemError),
        "failure type reuses the existing enum value — no new variant, no migration"
    );
    assert_eq!(
        item.requester,
        Some("un25-requester".to_string()),
        "the authorization subject must survive the freeze"
    );

    let message = item.error_message.expect("error message");
    assert!(
        message.contains("Retry"),
        "the message must state the retry condition: {message}"
    );
    assert!(
        message.contains("snapshot dirty"),
        "the message must name the reason: {message}"
    );
    assert!(
        message.contains("not rejected"),
        "a frozen item is not a rejected change; the message must say so: {message}"
    );
}

/// The alert's field set, asserted at its single emit site.
///
/// The emission is deliberately a plain synchronous function, so capturing it
/// needs no runtime and no cross-thread subscriber: the freeze path below calls
/// exactly this function.
#[test]
fn un25_freeze_emits_the_contract_event() {
    let out = capture_tracing(|| {
        crate::ceres::api_service::mono_api_service::emit_authz_frozen_alert(
            "UN25EVENT",
            Some("un25-requester"),
            "store not built",
        );
    });

    assert!(
        out.contains("merge_queue_authz_frozen"),
        "missing event field: {out}"
    );
    assert!(out.contains("UN25EVENT"), "missing cl_link field: {out}");
    assert!(
        out.contains("un25-requester"),
        "missing requester field: {out}"
    );
    assert!(
        out.contains("store not built"),
        "missing reason field: {out}"
    );
}

/// An anonymous requester still produces a well-formed alert rather than an
/// empty field an operator would have to guess about.
#[test]
fn un25_the_frozen_alert_names_a_missing_requester_explicitly() {
    let out = capture_tracing(|| {
        crate::ceres::api_service::mono_api_service::emit_authz_frozen_alert(
            "UN25NOREQ",
            None,
            "store not built",
        );
    });

    assert!(out.contains("merge_queue_authz_frozen"), "{out}");
    assert!(
        out.contains("<none>"),
        "missing requester placeholder: {out}"
    );
}

/// The freeze path really does go through that emit site: freezing a queued
/// item produces the alert.
#[tokio::test]
async fn un25_the_freeze_path_reaches_the_alert() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    queued_item(&storage, 710_004, "UN25ALERT", Some("un25-requester")).await;

    assert!(
        service(&storage)
            .freeze_merge_queue_item_for_authz("UN25ALERT", "store not built")
            .await
            .expect("freeze"),
        "the freeze must report that it froze the item, which is what gates the alert"
    );
}

#[tokio::test]
async fn un25_freezing_an_already_failed_item_preserves_the_original_reason() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    queued_item(&storage, 710_003, "UN25TWICE", Some("un25-requester")).await;

    let service = service(&storage);
    assert!(
        service
            .freeze_merge_queue_item_for_authz("UN25TWICE", "first reason")
            .await
            .expect("first freeze")
    );
    assert!(
        !service
            .freeze_merge_queue_item_for_authz("UN25TWICE", "second reason")
            .await
            .expect("second freeze"),
        "an already-failed item is left alone, so the first diagnosis is not overwritten"
    );

    let message = merge_row(&storage, "UN25TWICE")
        .await
        .error_message
        .expect("message");
    assert!(message.contains("first reason"), "{message}");
}

#[tokio::test]
async fn un25_freezing_an_unqueued_cl_reports_that_nothing_was_frozen() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;

    assert!(
        !service(&storage)
            .freeze_merge_queue_item_for_authz("UN25MISSING", "whatever")
            .await
            .expect("freeze call succeeds"),
        "no queue row means nothing to freeze — not an error"
    );
}
