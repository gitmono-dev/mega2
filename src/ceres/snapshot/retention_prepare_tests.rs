use git_internal::hash::HashKind;

use super::*;
use crate::ceres::snapshot::retention_dag::{MetadataDagBuilder, MetadataDagLimits};

fn dag() -> Arc<ValidatedMetadataDag> {
    let bytes = Page::build(&[]).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&bytes, &[]).unwrap();
    Arc::new(builder.finish(page_id(&bytes)).unwrap())
}

fn key(kind: HashKind, scope: &str) -> NativeRetentionKey {
    NativeRetentionKey {
        projection: NativeProjectionKey::new(
            ObjectHash::from_hex_for_kind(kind, &"a".repeat(64)).unwrap(),
        ),
        scope: scope.to_owned(),
    }
}

#[test]
fn retention_memo_preserves_tagged_source_scope_and_profile_identity() {
    let cache = NativeProjectionCache::default();
    let original = key(HashKind::Sha256, "/scope");
    let prepared = dag();
    cache.insert_retention_dag(original.clone(), Arc::clone(&prepared));
    assert!(Arc::ptr_eq(
        &prepared,
        &cache.retention_dag(&original).unwrap()
    ));
    assert!(
        cache
            .retention_dag(&key(HashKind::Blake3, "/scope"))
            .is_none()
    );
    assert!(
        cache
            .retention_dag(&key(HashKind::Sha256, "/different"))
            .is_none()
    );
    let mut different = original;
    different.projection.projection_revision += 1;
    assert!(cache.retention_dag(&different).is_none());
}

#[test]
fn retention_memo_limits_total_residency_and_eviction_keeps_active_arc() {
    let cache = NativeProjectionCache::default();
    let first_key = key(HashKind::Sha256, "/first");
    let first = dag();
    let overhead = std::mem::size_of::<NativeRetentionKey>()
        + std::mem::size_of::<Arc<ValidatedMetadataDag>>()
        + first_key.scope.capacity()
        + first_key.projection.tree_oid.capacity();
    let one_entry_bytes = first.residency_bytes().unwrap() + overhead;
    cache.insert_retention_dag_with_budget(
        first_key.clone(),
        Arc::clone(&first),
        one_entry_bytes,
        16,
    );
    assert!(cache.retention_dag(&first_key).is_some());
    let next_key = key(HashKind::Sha256, "/other");
    let next = dag();
    cache.insert_retention_dag_with_budget(
        next_key.clone(),
        Arc::clone(&next),
        one_entry_bytes,
        16,
    );
    assert!(cache.retention_dag(&first_key).is_none());
    assert!(Arc::ptr_eq(&next, &cache.retention_dag(&next_key).unwrap()));
    assert_eq!(first.payloads()[0].bytes, Page::build(&[]).unwrap());
    assert!(cache.state.lock().unwrap().retained_dag_bytes <= one_entry_bytes);
    let skipped_key = key(HashKind::Sha256, "/too-large");
    cache.insert_retention_dag_with_budget(skipped_key.clone(), dag(), 1, 16);
    assert!(cache.retention_dag(&skipped_key).is_none());
    assert!(cache.retention_dag(&next_key).is_some());
}

#[test]
fn retention_memo_entry_cap_bounds_distinct_roots() {
    let cache = NativeProjectionCache::default();
    let first = key(HashKind::Sha256, "/one");
    let second = key(HashKind::Sha256, "/two");
    cache.insert_retention_dag_with_budget(first.clone(), dag(), 64 * 1024 * 1024, 1);
    cache.insert_retention_dag_with_budget(second.clone(), dag(), 64 * 1024 * 1024, 1);
    assert!(cache.retention_dag(&first).is_none());
    assert!(cache.retention_dag(&second).is_some());
    assert_eq!(cache.state.lock().unwrap().retention_dags.len(), 1);
}
