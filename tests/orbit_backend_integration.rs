//! Integration tests for the local-filesystem backend and the log-storage layer,
//! exercised through the public `ObjectStorageFactory` + trait surface.
//!
//! These give the happy-path coverage the improvement plan (§7) requires before
//! any higher-risk refactor of the adapter or log storage.

use bytes::Bytes;
use futures::StreamExt;
use mega2_core::{
    orbit::factory::{
        LocalConfig, MegaObjectStorageWrapper, ObjectStorageBackend, ObjectStorageConfig,
        ObjectStorageFactory,
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};
use tempfile::TempDir;

/// Builds a Local-backed store rooted in a fresh temp dir (kept alive by the
/// returned `TempDir`).
async fn local_store() -> (TempDir, MegaObjectStorageWrapper) {
    let dir = TempDir::new().unwrap();
    let cfg = ObjectStorageConfig {
        storage_type: ObjectStorageBackend::Local,
        local: LocalConfig {
            root_dir: dir.path().to_str().unwrap().to_string(),
        },
        ..Default::default()
    };
    let store = ObjectStorageFactory::build(&cfg).await.unwrap();
    (dir, store)
}

fn key(ns: ObjectNamespace, k: &str) -> ObjectKey {
    ObjectKey {
        namespace: ns,
        key: k.to_string(),
    }
}

fn stream_of(bytes: &'static [u8]) -> ObjectByteStream {
    let b = Bytes::from_static(bytes);
    Box::pin(futures::stream::once(async move {
        Ok::<Bytes, std::io::Error>(b)
    }))
}

async fn collect(mut s: ObjectByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

// ---- object storage ----

#[tokio::test]
async fn put_get_roundtrip() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Lfs, "abcdef1234567890");
    store
        .inner
        .put_stream(&k, stream_of(b"hello world"), ObjectMeta::default())
        .await
        .unwrap();
    let (s, _meta) = store.inner.get_stream(&k).await.unwrap();
    assert_eq!(collect(s).await, b"hello world");
}

#[tokio::test]
async fn get_range_reads_subslice() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Lfs, "range-key-123456");
    store
        .inner
        .put_stream(&k, stream_of(b"0123456789"), ObjectMeta::default())
        .await
        .unwrap();
    let (s, _m) = store.inner.get_range_stream(&k, 2, Some(5)).await.unwrap();
    assert_eq!(collect(s).await, b"234");
}

#[tokio::test]
async fn exists_reports_not_found_as_false_and_delete_works() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Lfs, "exists-key-123456");
    // Missing object -> NotFound -> Ok(false), never an error.
    assert!(!store.inner.exists(&k).await.unwrap());
    store
        .inner
        .put_stream(&k, stream_of(b"x"), ObjectMeta::default())
        .await
        .unwrap();
    assert!(store.inner.exists(&k).await.unwrap());
    store.inner.delete(&k).await.unwrap();
    assert!(!store.inner.exists(&k).await.unwrap());
}

#[tokio::test]
async fn idempotent_git_write_does_not_error_on_repeat() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Git, "gitobject1234567890");
    store
        .inner
        .put_stream(&k, stream_of(b"content"), ObjectMeta::default())
        .await
        .unwrap();
    // Content-addressed Git write is create-only: a repeat of the same key
    // is swallowed as Ok (AlreadyExists), not an error.
    store
        .inner
        .put_stream(&k, stream_of(b"content"), ObjectMeta::default())
        .await
        .unwrap();
    let (s, _m) = store.inner.get_stream(&k).await.unwrap();
    assert_eq!(collect(s).await, b"content");
}

#[tokio::test]
async fn invalid_key_is_rejected_before_backend() {
    let (_dir, store) = local_store().await;
    let bad = key(ObjectNamespace::Lfs, "../escape");
    assert!(
        store
            .inner
            .put_stream(&bad, stream_of(b"x"), ObjectMeta::default())
            .await
            .is_err()
    );
}

// ---- log storage ----

#[tokio::test]
async fn missing_log_has_empty_manifest() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-empty");
    let m = store.inner.load_manifest(&k).await.unwrap();
    assert_eq!(m.len, 0);
    assert!(m.segments.is_empty());
    assert!(!store.inner.log_exists(&k).await.unwrap());
}

#[tokio::test]
async fn append_then_read_range() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-one");
    store
        .inner
        .append(&k, stream_of(b"hello\nworld\n"), ObjectMeta::default())
        .await
        .unwrap();
    let m = store.inner.load_manifest(&k).await.unwrap();
    assert_eq!(m.len, 12);
    assert!(store.inner.log_exists(&k).await.unwrap());
    let s = store.inner.read_range(&k, 0, 5).await.unwrap();
    assert_eq!(collect(s).await, b"hello");
}

#[tokio::test]
async fn multi_segment_append_reads_across_boundary() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-multi");
    store
        .inner
        .append(&k, stream_of(b"aaa"), ObjectMeta::default())
        .await
        .unwrap();
    store
        .inner
        .append(&k, stream_of(b"bbb"), ObjectMeta::default())
        .await
        .unwrap();
    let m = store.inner.load_manifest(&k).await.unwrap();
    assert_eq!(m.len, 6);
    assert_eq!(m.segments.len(), 2);
    // Bytes [2, 4) straddle the "aaa"/"bbb" segment boundary.
    let s = store.inner.read_range(&k, 2, 2).await.unwrap();
    assert_eq!(collect(s).await, b"ab");
}

#[tokio::test]
async fn read_lines_range_returns_requested_line() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-lines");
    store
        .inner
        .append(&k, stream_of(b"l1\nl2\nl3\n"), ObjectMeta::default())
        .await
        .unwrap();
    // Lines are 0-indexed, [start, end); line 1 is the second line, incl. newline.
    let s = store.inner.read_lines_range(&k, 1, 2).await.unwrap();
    assert_eq!(collect(s).await, b"l2\n");
}

#[tokio::test]
async fn append_concurrently_unsupported_on_local() {
    // The local backend has no conditional-write support, so concurrent-safe
    // append must fail loudly rather than silently risk lost updates.
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-cc");
    let res = store
        .inner
        .append_concurrently(&k, stream_of(b"x"), ObjectMeta::default())
        .await;
    assert!(
        res.is_err(),
        "local backend must reject append_concurrently"
    );
    // Plain append remains available for single-writer local logs.
    store
        .inner
        .append(&k, stream_of(b"x"), ObjectMeta::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn read_lines_range_handles_last_line_without_newline() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-nonl");
    // Final line "c" has no trailing newline.
    store
        .inner
        .append(&k, stream_of(b"a\nb\nc"), ObjectMeta::default())
        .await
        .unwrap();
    let s = store.inner.read_lines_range(&k, 2, 3).await.unwrap();
    assert_eq!(collect(s).await, b"c");
}

#[tokio::test]
async fn read_lines_range_across_segment_boundary() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Log, "task/repo/build-seglines");
    // Two appends => two segments; the first logical line spans the boundary.
    store
        .inner
        .append(&k, stream_of(b"first-"), ObjectMeta::default())
        .await
        .unwrap();
    store
        .inner
        .append(&k, stream_of(b"line\nsecond\n"), ObjectMeta::default())
        .await
        .unwrap();
    let s = store.inner.read_lines_range(&k, 0, 1).await.unwrap();
    assert_eq!(collect(s).await, b"first-line\n");
}

// ---- object metadata ----

#[tokio::test]
async fn get_stream_populates_size() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Lfs, "meta-size-key-123");
    store
        .inner
        .put_stream(&k, stream_of(b"hello world"), ObjectMeta::default())
        .await
        .unwrap();
    let (_s, meta) = store.inner.get_stream(&k).await.unwrap();
    assert_eq!(meta.size, 11);
}

#[tokio::test]
async fn get_range_stream_reports_full_object_size() {
    let (_dir, store) = local_store().await;
    let k = key(ObjectNamespace::Lfs, "meta-range-key-123");
    store
        .inner
        .put_stream(&k, stream_of(b"0123456789"), ObjectMeta::default())
        .await
        .unwrap();
    let (_s, meta) = store.inner.get_range_stream(&k, 2, Some(5)).await.unwrap();
    // Full object size (10), not the range length (3).
    assert_eq!(meta.size, 10);
}
