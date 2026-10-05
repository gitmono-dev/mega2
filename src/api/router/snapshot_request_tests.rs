use std::{
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
    middleware,
    response::Response,
    routing::get,
};
use bytes::Bytes;
use futures::{Stream, stream};
use serde_json::Value;
use tower::ServiceExt;

use super::{JSON_REQUEST_LIMIT, request::JSON_REQUEST_TIMEOUT, routers};
use crate::{
    api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
    ceres::{
        api_service::cache::GitObjectCache,
        snapshot::{descriptor::build, runtime::runtime, view::SnapshotView},
    },
    config::testing::isolated_config,
    jupiter::tests::{test_redis_manager, test_storage_with_config},
    server::trace_context::{TraceContext, inject_trace_context},
};

struct Fixture {
    app: Router,
    snapshot_id: String,
    lease_id: String,
    expires: u64,
    _temp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.mst2.enabled = true;
        config.mst2.instance_uuid = Some(uuid::Uuid::new_v4().to_string());
        config.mst2.auth_token = Some("mst2-input-test".to_string());
        let storage = test_storage_with_config(temp.path(), config).await;
        let state = MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage,
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_string(),
        };
        let view = SnapshotView::from_commit(&"1".repeat(40), &"2".repeat(40));
        let built = build(&state.storage.config().mst2, &view, "/", [3; 32]).unwrap();
        let ctx = runtime().insert_context(built, &view.commit_oid, &view.root_tree_oid, 60);
        // Reproduce the server's nest-after-layer order: MST must establish
        // its own request context even when the earlier layer does not run.
        let app = Router::new()
            .route("/outside", get(|| async { "ok" }))
            .layer(middleware::from_fn(inject_trace_context))
            .nest("/api/v2", routers(state.clone()).with_state(state));
        Self {
            app,
            snapshot_id: ctx.built.snapshot_id,
            lease_id: ctx.lease_id,
            expires: ctx.lease_expires_at_unix,
            _temp: temp,
        }
    }

    fn request(&self, suffix: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v2/snapshots/{suffix}"))
            .header("authorization", "Bearer mst2-input-test")
            .header("x-mega-snapshot-lease", &self.lease_id)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    fn renew_path(&self) -> String {
        format!("leases/{}/renew", self.lease_id)
    }

    fn assert_lease_unchanged(&self) {
        let ctx = runtime().context(&self.snapshot_id).unwrap();
        assert_eq!(ctx.lease_id, self.lease_id);
        assert_eq!(ctx.lease_expires_at_unix, self.expires);
    }
}

async fn assert_error(response: Response, status: u16, code: &str, retryable: bool) -> Value {
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(response.headers()["content-type"], "application/json");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!request_id.is_empty());
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["request_id"], request_id);
    assert_eq!(value["error"]["retryable"], retryable);
    assert!(!value["error"]["message"].as_str().unwrap().is_empty());
    value
}

fn chunked(bytes: Vec<u8>) -> Body {
    let chunks: Vec<Result<Bytes, Infallible>> = bytes
        .chunks(4096)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    Body::from_stream(stream::iter(chunks))
}

#[tokio::test]
async fn mst2_json_actual_chunked_limit_has_typed_errors_on_every_post() {
    let fixture = Fixture::new().await;
    let paths = [
        "resolve".to_string(),
        fixture.renew_path(),
        format!("{}/lookup", fixture.snapshot_id),
        format!("{}/metadata/pages", fixture.snapshot_id),
        format!("{}/objects", fixture.snapshot_id),
        format!("{}/chunks", fixture.snapshot_id),
    ];
    for path in paths {
        for declared in [None, Some("2")] {
            let mut request = fixture.request(&path, chunked(vec![b' '; JSON_REQUEST_LIMIT + 1]));
            if let Some(length) = declared {
                request
                    .headers_mut()
                    .insert("content-length", length.parse().unwrap());
            }
            let response = fixture.app.clone().oneshot(request).await.unwrap();
            assert_error(response, 413, "LIMIT_EXCEEDED", false).await;
            fixture.assert_lease_unchanged();
        }
    }

    let mut body = br#"{"lease_seconds":120}"#.to_vec();
    body.resize(JSON_REQUEST_LIMIT, b' ');
    let response = fixture
        .app
        .clone()
        .oneshot(fixture.request(&fixture.renew_path(), chunked(body)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.headers()["x-request-id"].is_empty());
    let renewed = runtime().context(&fixture.snapshot_id).unwrap();
    assert!(renewed.lease_expires_at_unix > fixture.expires);
}

#[tokio::test]
async fn mst2_json_declared_oversize_is_rejected_without_reading_body() {
    let fixture = Fixture::new().await;
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    let body = Body::from_stream(stream::poll_fn(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Poll::<Option<Result<Bytes, Infallible>>>::Pending
    }));
    let mut request = fixture.request(&fixture.renew_path(), body);
    request.headers_mut().insert(
        "content-length",
        (JSON_REQUEST_LIMIT + 1).to_string().parse().unwrap(),
    );
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    assert_error(response, 413, "LIMIT_EXCEEDED", false).await;
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    fixture.assert_lease_unchanged();
}

#[tokio::test]
async fn mst2_json_closed_dtos_duplicates_and_malformed_input_do_not_renew() {
    let fixture = Fixture::new().await;
    let renew = fixture.renew_path();
    let lookup = format!("{}/lookup", fixture.snapshot_id);
    let metadata = format!("{}/metadata/pages", fixture.snapshot_id);
    let objects = format!("{}/objects", fixture.snapshot_id);
    let chunks = format!("{}/chunks", fixture.snapshot_id);
    let cases: Vec<(&str, &[u8])> = vec![
        ("resolve", br#"{"target":{"kind":"latest"},"scope":"/","scope":"/"}"#),
        ("resolve", br#"{"target":{"kind":"latest","kind":"latest"}}"#),
        ("resolve", br#"{"target":{"kind":"latest","extra":true}}"#),
        ("resolve", br#"{"target":{"kind":"latest"},"extra":true}"#),
        (&renew, br#"{"lease_seconds":3600,"lease_seconds":120}"#),
        (&renew, br#"{"lease_seconds":3600,"\u006cease_seconds":120}"#),
        (&renew, br#"{"lease_seconds":null,"lease_seconds":120}"#),
        (&renew, br#"{"lease_seconds":null,"\u006cease_seconds":120}"#),
        (&renew, br#"{"lease_seconds":null,"lease_seconds":null}"#),
        (&renew, br#"{"lease_seconds":3600,"extra":true}"#),
        (&renew, br#"{"lease_seconds":"120"}"#),
        (&renew, br#"{"lease_seconds":1.5}"#),
        (&renew, br#"{"lease_seconds":-1}"#),
        (&renew, br#"{"lease_seconds":NaN}"#),
        (&renew, br#"{"lease_seconds":Infinity}"#),
        (&renew, br#"{"lease_seconds":18446744073709551616}"#),
        (&renew, br#"{"lease_seconds":"#),
        (&renew, b"{\"lease_seconds\":\xff}"),
        (&renew, b"{\"lease_seconds\":3600} {}"),
        (&renew, b"{\"lease_seconds\":3600} x"),
        (&lookup, br#"{"paths":[],"paths":[]}"#),
        (&lookup, br#"{"paths":[],"extra":true}"#),
        (&metadata, br#"{"items":[],"items":[]}"#),
        (&metadata, br#"{"items":[],"extra":true}"#),
        (&metadata, br#"{"items":[{"directory_path":"/","route":[],"route":[]}]}"#),
        (&metadata, br#"{"items":[{"directory_path":"/","extra":true}]}"#),
        (&objects, br#"{"items":[],"items":[]}"#),
        (&objects, br#"{"items":[],"extra":true}"#),
        (&objects, br#"{"items":[{"path":"/a","path":"/b","expected_digest":"x"}]}"#),
        (&objects, br#"{"items":[{"path":"/a","expected_digest":"x","extra":true}]}"#),
        (&chunks, br#"{"items":[],"items":[]}"#),
        (&chunks, br#"{"items":[],"extra":true}"#),
        (&chunks, br#"{"items":[{"path":"/a","expected_digest":"x","map_id":"x","chunk_index":"0","chunk_index":"1"}]}"#),
        (&chunks, br#"{"items":[{"path":"/a","expected_digest":"x","map_id":"x","chunk_index":"0","extra":true}]}"#),
    ];
    for (path, body) in cases {
        let response = fixture
            .app
            .clone()
            .oneshot(fixture.request(path, Body::from(body.to_vec())))
            .await
            .unwrap();
        assert_error(response, 400, "INVALID_REQUEST", false).await;
        fixture.assert_lease_unchanged();
    }

    for body in [Body::empty(), Body::from("{} \r\n\t")] {
        let response = fixture
            .app
            .clone()
            .oneshot(fixture.request(&renew, body))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(!response.headers()["x-request-id"].is_empty());
    }
}

struct SlowBody {
    interval: Option<tokio::time::Interval>,
    chunks: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl Stream for SlowBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match &mut this.interval {
            Some(interval) => match interval.poll_tick(cx) {
                Poll::Ready(_) => {
                    this.chunks.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(Some(Ok(Bytes::from_static(b" "))))
                }
                Poll::Pending => Poll::Pending,
            },
            None => Poll::Pending,
        }
    }
}

impl Drop for SlowBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn mst2_json_real_post_pending_and_trickle_bodies_hit_overall_deadline() {
    let fixture = Fixture::new().await;
    let exercise = |trickle: bool| {
        let app = fixture.app.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let chunks = Arc::new(AtomicUsize::new(0));
        let body = Body::from_stream(SlowBody {
            interval: trickle.then(|| tokio::time::interval(Duration::from_millis(50))),
            chunks: chunks.clone(),
            dropped: dropped.clone(),
        });
        let request = fixture.request(&fixture.renew_path(), body);
        async move {
            let response = tokio::time::timeout(
                JSON_REQUEST_TIMEOUT + Duration::from_secs(5),
                app.oneshot(request),
            )
            .await
            .expect("MST input deadline must terminate the request")
            .unwrap();
            assert_error(response, 503, "TEMPORARY_UNAVAILABLE", true).await;
            assert!(dropped.load(Ordering::SeqCst));
            if trickle {
                assert!(chunks.load(Ordering::SeqCst) > 1);
            } else {
                assert_eq!(chunks.load(Ordering::SeqCst), 0);
            }
        }
    };
    tokio::join!(exercise(false), exercise(true));
    fixture.assert_lease_unchanged();
    let response = fixture
        .app
        .clone()
        .oneshot(fixture.request(&fixture.renew_path(), Body::from("{}")))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn mst2_json_body_read_failure_is_typed_and_preserves_request_context() {
    let fixture = Fixture::new().await;
    let body = Body::from_stream(stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("private backend key must not leak"))
    }));
    let mut request = fixture.request(&fixture.renew_path(), body);
    request
        .headers_mut()
        .insert("x-request-id", "different-inbound-id".parse().unwrap());
    request.extensions_mut().insert(TraceContext {
        trace_id: Arc::from("existing-trace-id"),
    });
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    let value = assert_error(response, 400, "INVALID_REQUEST", false).await;
    assert_eq!(value["error"]["request_id"], "existing-trace-id");
    assert_eq!(value["error"]["message"], "could not read request body");
    fixture.assert_lease_unchanged();

    let mut request = fixture.request(&fixture.renew_path(), Body::from("{"));
    request
        .headers_mut()
        .insert("x-request-id", "accepted-inbound-id".parse().unwrap());
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    let value = assert_error(response, 400, "INVALID_REQUEST", false).await;
    assert_eq!(value["error"]["request_id"], "accepted-inbound-id");
}
