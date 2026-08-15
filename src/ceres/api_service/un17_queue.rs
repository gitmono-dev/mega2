//! UN-17: who a queued merge runs as.
//!
//! A queued merge executes long after the request that queued it — the ACL may
//! have changed in between, and the worker itself is not a subject. So the
//! decision re-checks the *recorded requester* at execution time and keeps
//! `system` only as the execution actor (ADR-UN-06 ④).
//!
//! Items queued before requester capture (or queued anonymously) have no
//! subject at all. Under `enforce` those are frozen rather than merged as some
//! privileged default, and the guidance says so: retrying alone can never fix
//! them.

use crate::{
    ceres::api_service::mono_api_service::{
        QUEUE_MISSING_REQUESTER_GUIDANCE, QueueExecutionDecision, decide_queue_execution,
    },
    contract::policy::{builder::EntitySnapshot, enforcement::Enforcement},
};

const ADMIN: &str = "un17-admin";
const MAINTAINER: &str = "un17-maintainer";
const READER: &str = "un17-reader";

/// One member per role, so the matrix exercises the real group hierarchy.
fn role_snapshot() -> EntitySnapshot {
    let json = serde_json::json!({
        "users": {
            format!("User::\"{ADMIN}\""): {
                "euid": format!("User::\"{ADMIN}\""),
                "parents": ["UserGroup::\"admin\""]
            },
            format!("User::\"{MAINTAINER}\""): {
                "euid": format!("User::\"{MAINTAINER}\""),
                "parents": ["UserGroup::\"matainer\""]
            },
            format!("User::\"{READER}\""): {
                "euid": format!("User::\"{READER}\""),
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
    .to_string();
    crate::contract::policy::builder::build_from_json(&json).expect("build snapshot")
}

fn executes_as(decision: &QueueExecutionDecision) -> Option<&str> {
    match decision {
        QueueExecutionDecision::Execute { authz_principal } => Some(authz_principal),
        QueueExecutionDecision::Freeze { .. } => None,
    }
}

fn freeze_reason(decision: &QueueExecutionDecision) -> Option<&str> {
    match decision {
        QueueExecutionDecision::Freeze { reason } => Some(reason),
        QueueExecutionDecision::Execute { .. } => None,
    }
}

/// Under `enforce`, `approveMergeRequest` is a maintainer-level action in the
/// current policy (admin inherits it), so the four recorded requesters split
/// two and two. Whether approving should be admin-only is policy content
/// (UN-09/UN-04), not this card.
#[test]
fn un17_queue_execution_matrix_under_enforce() {
    let snapshot = role_snapshot();
    let cases: &[(Option<&str>, bool)] = &[
        (Some(ADMIN), true),
        (Some(MAINTAINER), true),
        (Some(READER), false),
        (None, false),
    ];

    for (requester, should_execute) in cases {
        let decision = decide_queue_execution(Enforcement::Enforce, Some(&snapshot), *requester);
        assert_eq!(
            executes_as(&decision).is_some(),
            *should_execute,
            "requester {requester:?} under enforce"
        );
        if *should_execute {
            assert_eq!(
                executes_as(&decision),
                *requester,
                "the merge must be authorized as the recorded requester, not as the worker"
            );
        }
    }
}

#[test]
fn un17_a_queued_item_without_a_requester_is_frozen_with_guidance() {
    let snapshot = role_snapshot();
    let decision = decide_queue_execution(Enforcement::Enforce, Some(&snapshot), None);

    let reason = freeze_reason(&decision).expect("must freeze");
    assert_eq!(
        reason, QUEUE_MISSING_REQUESTER_GUIDANCE,
        "a subject-less item must carry the operator guidance verbatim"
    );
    assert!(
        reason.contains("re-queue"),
        "the guidance must say what a human should do: {reason}"
    );
}

#[test]
fn un17_a_missing_snapshot_freezes_with_a_retryable_reason() {
    let decision = decide_queue_execution(Enforcement::Enforce, None, Some(ADMIN));
    let reason = freeze_reason(&decision).expect("must freeze");
    assert!(
        reason.contains("retry"),
        "an undecidable-right-now state is retryable and must say so: {reason}"
    );
}

#[test]
fn un17_off_executes_exactly_as_before() {
    // `off` consumes no authorization data at all (GC-UN-01): even a snapshot
    // that would refuse changes nothing, and a subject-less legacy item still
    // runs as it did before this card.
    let snapshot = role_snapshot();
    assert_eq!(
        executes_as(&decide_queue_execution(
            Enforcement::Off,
            Some(&snapshot),
            Some(READER)
        )),
        Some(READER)
    );
    assert_eq!(
        executes_as(&decide_queue_execution(Enforcement::Off, None, None)),
        Some("system"),
        "a legacy item keeps running as the worker under off"
    );
}

#[test]
fn un17_shadow_never_freezes() {
    let snapshot = role_snapshot();
    for requester in [Some(READER), None] {
        assert!(
            executes_as(&decide_queue_execution(
                Enforcement::Shadow,
                Some(&snapshot),
                requester
            ))
            .is_some(),
            "shadow evaluates and records, but must not change what runs: {requester:?}"
        );
    }
}

/// `shadow` must not freeze even when there is nothing to decide against —
/// it records what `enforce` would have refused and lets the merge run.
#[test]
fn un17_shadow_does_not_freeze_when_the_snapshot_is_missing() {
    assert_eq!(
        executes_as(&decide_queue_execution(
            Enforcement::Shadow,
            None,
            Some(READER)
        )),
        Some(READER)
    );
}

#[test]
fn un17_an_empty_store_freezes_under_enforce() {
    let empty = crate::contract::policy::builder::build_from_json(
        r#"{"users":{},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#,
    )
    .expect("build empty snapshot");

    assert!(
        freeze_reason(&decide_queue_execution(
            Enforcement::Enforce,
            Some(&empty),
            Some(ADMIN)
        ))
        .is_some(),
        "enforce + empty store = deny (fail-closed, ADR-UN-01)"
    );
}
