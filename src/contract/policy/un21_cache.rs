//! UN-21 cache-residue gate: the UN-14 snapshot cache must never serve a
//! previous dimension's decision after a rebuild.
//!
//! The matrix in [`super::un21_matrix`] already evaluates against the cached
//! `Entities`, so a stale cache fails there too. These cases pin the cache
//! semantics themselves: a rebuild publishes a *new* immutable snapshot rather
//! than mutating the old one, an already-held handle keeps serving its own
//! (older) content, and repeated rebuilds across every dimension leave no
//! residue in either direction.

use std::sync::Arc;

use crate::contract::policy::{
    entitystore::SharedEntityStore,
    un21_matrix::{GROUPS_ADMIN_MAINTAINER_READER, REPO_DEFAULT_ATTRS, authz_json, decide, user},
};

#[test]
fn un21_cache_rebuild_publishes_a_new_snapshot_instance() {
    let shared = SharedEntityStore::new();
    shared
        .ensure(&authz_json(
            &user("cache1", &["reader"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        ))
        .expect("first build");
    let first = shared.snapshot().expect("first snapshot");

    shared
        .swap(&authz_json(
            &user("cache1", &[]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        ))
        .expect("rebuild");
    let second = shared.snapshot().expect("second snapshot");

    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rebuild must publish a new snapshot, not mutate the old one"
    );
    // The handle taken before the rebuild keeps its own content: readers that
    // are mid-request are not torn, and the new handle carries the new answer.
    assert!(
        decide(
            first.entities(),
            r#"User::"cache1""#,
            r#"Action::"pullRepo""#,
            r#"Repository::"/""#
        ),
        "the previously held snapshot keeps serving its own content"
    );
    assert!(
        !decide(
            second.entities(),
            r#"User::"cache1""#,
            r#"Action::"pullRepo""#,
            r#"Repository::"/""#
        ),
        "the snapshot taken after the rebuild must reflect the new data source"
    );
}

#[test]
fn un21_cache_repeated_rebuilds_leave_no_residue_in_either_direction() {
    let granted = authz_json(
        &user("cache2", &["reader"]),
        GROUPS_ADMIN_MAINTAINER_READER,
        REPO_DEFAULT_ATTRS,
    );
    let revoked = authz_json(
        &user("cache2", &[]),
        GROUPS_ADMIN_MAINTAINER_READER,
        REPO_DEFAULT_ATTRS,
    );

    let shared = SharedEntityStore::new();
    shared.ensure(&granted).expect("first build");

    // Grant → revoke → grant → revoke: every rebuild must be decided by the
    // content it was built from, never by a cached predecessor.
    for (round, (json, expected)) in [
        (&revoked, false),
        (&granted, true),
        (&revoked, false),
        (&granted, true),
    ]
    .into_iter()
    .enumerate()
    {
        shared.swap(json).expect("rebuild");
        let snapshot = shared.snapshot().expect("snapshot");
        assert_eq!(
            decide(
                snapshot.entities(),
                r#"User::"cache2""#,
                r#"Action::"pullRepo""#,
                r#"Repository::"/""#
            ),
            expected,
            "round {round}: rebuilt snapshot must match the content it was built from"
        );
    }
}

#[test]
fn un21_cache_failed_rebuild_keeps_the_previous_snapshot_and_marks_dirty() {
    let shared = SharedEntityStore::new();
    shared
        .ensure(&authz_json(
            &user("cache3", &["reader"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        ))
        .expect("first build");
    let before = shared.snapshot().expect("first snapshot");

    // A dangling group reference fails build-time validation (UN-14), so the
    // swap must keep the old snapshot rather than leave the cache empty.
    let broken = authz_json(
        &user("cache3", &["missing-group"]),
        GROUPS_ADMIN_MAINTAINER_READER,
        REPO_DEFAULT_ATTRS,
    );
    shared
        .swap(&broken)
        .expect_err("invalid content must fail the rebuild");

    let after = shared
        .snapshot()
        .expect("snapshot survives a failed rebuild");
    assert!(
        Arc::ptr_eq(&before, &after),
        "a failed rebuild keeps the previous snapshot instance"
    );
    assert!(
        shared.is_dirty(),
        "a failed rebuild marks the snapshot dirty"
    );
    assert!(
        decide(
            after.entities(),
            r#"User::"cache3""#,
            r#"Action::"pullRepo""#,
            r#"Repository::"/""#
        ),
        "the kept snapshot still serves its own content"
    );
}
