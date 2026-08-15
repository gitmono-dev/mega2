//! UN-19: only an admin may merge a change to the authorization file.
//!
//! Editing `/.mega_cedar.json` *is* how permissions are granted, so whoever can
//! merge such a change can grant themselves anything. A maintainer holds
//! `approveMergeRequest`, which would have been exactly that: a self-promotion
//! path. The check therefore asks for `addAdmin`, which only the admin group
//! holds — the same decision for all three merge entry points, because they all
//! funnel through `merge_cl_unchecked`.
//!
//! A check that cannot be completed is not a pass. Three things can go wrong —
//! the changed-file list cannot be read, main's copy of the file cannot be
//! resolved, or it is absent from main — and all three mean the change was
//! never examined, so `enforce` refuses.

use crate::{
    ceres::api_service::mono_api_service::{AclChangeDecision, decide_acl_change},
    contract::policy::{builder::EntitySnapshot, enforcement::Enforcement},
};

const ADMIN: &str = "un19-admin";
const MAINTAINER: &str = "un19-maintainer";
const READER: &str = "un19-reader";
const ANONYMOUS: &str = "__anonymous__";

fn role_snapshot() -> EntitySnapshot {
    let json = serde_json::json!({
        "users": {
            format!(r#"User::"{ADMIN}""#): {
                "euid": format!(r#"User::"{ADMIN}""#),
                "parents": [r#"UserGroup::"admin""#]
            },
            format!(r#"User::"{MAINTAINER}""#): {
                "euid": format!(r#"User::"{MAINTAINER}""#),
                "parents": [r#"UserGroup::"matainer""#]
            },
            format!(r#"User::"{READER}""#): {
                "euid": format!(r#"User::"{READER}""#),
                "parents": [r#"UserGroup::"reader""#]
            }
        },
        "repos": {
            r#"Repository::"/""#: {
                "euid": r#"Repository::"/""#,
                "is_private": true,
                "admins": r#"UserGroup::"admin""#,
                "maintainers": r#"UserGroup::"matainer""#,
                "readers": r#"UserGroup::"reader""#,
                "parents": []
            }
        },
        "user_groups": {
            r#"UserGroup::"admin""#: {
                "euid": r#"UserGroup::"admin""#,
                "parents": [r#"UserGroup::"matainer""#]
            },
            r#"UserGroup::"matainer""#: {
                "euid": r#"UserGroup::"matainer""#,
                "parents": [r#"UserGroup::"reader""#]
            },
            r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
        },
        "merge_requests": {},
        "issues": {}
    })
    .to_string();
    crate::contract::policy::builder::build_from_json(&json).expect("build snapshot")
}

fn proceeds(decision: &AclChangeDecision) -> bool {
    matches!(decision, AclChangeDecision::Proceed)
}

/// The self-promotion channel this card closes: a maintainer can approve
/// ordinary merges but must not be able to merge a change to the ACL.
#[test]
fn un19_only_an_admin_may_merge_an_acl_change() {
    let snapshot = role_snapshot();
    let cases: &[(&str, bool)] = &[
        (ADMIN, true),
        (MAINTAINER, false),
        (READER, false),
        (ANONYMOUS, false),
    ];

    for (principal, allowed) in cases {
        let decision = decide_acl_change(Enforcement::Enforce, Some(&snapshot), true, principal);
        assert_eq!(
            proceeds(&decision),
            *allowed,
            "{principal} merging an ACL change"
        );
        if !*allowed {
            assert!(
                matches!(decision, AclChangeDecision::Refuse { .. }),
                "a known-but-unauthorized principal is a refusal, not an unavailable check"
            );
        }
    }
}

/// A CL that leaves the ACL alone is unaffected — the check must not become a
/// general merge gate.
#[test]
fn un19_a_cl_that_does_not_touch_the_acl_is_unaffected() {
    let snapshot = role_snapshot();
    for principal in [ADMIN, MAINTAINER, READER, ANONYMOUS] {
        assert!(
            proceeds(&decide_acl_change(
                Enforcement::Enforce,
                Some(&snapshot),
                false,
                principal
            )),
            "{principal} merging an ordinary change"
        );
    }
}

#[test]
fn un19_off_does_not_detect_or_consume_anything() {
    // Not even for a maintainer editing the ACL: `off` is a short circuit.
    assert!(proceeds(&decide_acl_change(
        Enforcement::Off,
        Some(&role_snapshot()),
        true,
        MAINTAINER
    )));
    assert!(proceeds(&decide_acl_change(
        Enforcement::Off,
        None,
        true,
        MAINTAINER
    )));
}

#[test]
fn un19_shadow_reports_but_still_proceeds() {
    // `shadow` returns the refusal for the caller to record; the caller is what
    // decides not to act on it (asserted in `un19_fail_closed`).
    let decision = decide_acl_change(
        Enforcement::Shadow,
        Some(&role_snapshot()),
        true,
        MAINTAINER,
    );
    assert!(matches!(decision, AclChangeDecision::Refuse { .. }));
}

#[test]
fn un19_a_missing_snapshot_is_unavailable_not_a_refusal() {
    // The distinction matters: a refusal says "you may not", an unavailable
    // check says "nobody looked" — and only the latter is retryable.
    let decision = decide_acl_change(Enforcement::Enforce, None, true, ADMIN);
    assert!(matches!(decision, AclChangeDecision::Unavailable { .. }));
}

#[test]
fn un19_an_empty_store_refuses_even_an_admin() {
    let empty = crate::contract::policy::builder::build_from_json(
        r#"{"users":{},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#,
    )
    .expect("build empty snapshot");

    assert!(
        !proceeds(&decide_acl_change(
            Enforcement::Enforce,
            Some(&empty),
            true,
            ADMIN
        )),
        "enforce + empty store = deny (fail-closed, ADR-UN-01)"
    );
}

#[test]
fn un19_the_refusal_explains_why() {
    let decision = decide_acl_change(
        Enforcement::Enforce,
        Some(&role_snapshot()),
        true,
        MAINTAINER,
    );
    let AclChangeDecision::Refuse { reason } = decision else {
        panic!("expected a refusal");
    };
    assert!(reason.contains(MAINTAINER), "names the principal: {reason}");
    assert!(reason.contains("admin"), "says what is required: {reason}");
    assert!(
        reason.contains(".mega_cedar.json"),
        "names the file: {reason}"
    );
}
