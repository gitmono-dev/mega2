//! UN-26: the ACL audit's named scenarios.
//!
//! Each case is a way an ACL can be wrong that a naive check would miss —
//! reading direct membership instead of the closure, trusting a digest the
//! attacker can recompute, or comparing only the top-level snapshot while a
//! repository quietly points at a different group.

use crate::contract::policy::authz_audit::{
    AuditError, BaselineArtifact, DiffVerdict, bootstrap_candidate, compare,
};

/// A three-role ACL; `admin` inherits `matainer`, which inherits `reader`.
fn acl(users: serde_json::Value, groups: serde_json::Value, repo: serde_json::Value) -> String {
    serde_json::json!({
        "users": users,
        "repos": { r#"Repository::"/""#: repo },
        "user_groups": groups,
        "merge_requests": {},
        "issues": {}
    })
    .to_string()
}

fn user(name: &str, group: &str) -> (String, serde_json::Value) {
    (
        format!(r#"User::"{name}""#),
        serde_json::json!({
            "euid": format!(r#"User::"{name}""#),
            "parents": [format!(r#"UserGroup::"{group}""#)]
        }),
    )
}

fn standard_groups() -> serde_json::Value {
    serde_json::json!({
        r#"UserGroup::"admin""#: {
            "euid": r#"UserGroup::"admin""#,
            "parents": [r#"UserGroup::"matainer""#]
        },
        r#"UserGroup::"matainer""#: {
            "euid": r#"UserGroup::"matainer""#,
            "parents": [r#"UserGroup::"reader""#]
        },
        r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
    })
}

fn standard_repo() -> serde_json::Value {
    serde_json::json!({
        "euid": r#"Repository::"/""#,
        "is_private": true,
        "admins": r#"UserGroup::"admin""#,
        "maintainers": r#"UserGroup::"matainer""#,
        "readers": r#"UserGroup::"reader""#,
        "parents": []
    })
}

fn baseline_acl() -> String {
    let (admin_key, admin) = user("un26-admin", "admin");
    let (maint_key, maint) = user("un26-maintainer", "matainer");
    acl(
        serde_json::json!({ admin_key: admin, maint_key: maint }),
        standard_groups(),
        standard_repo(),
    )
}

fn approved_baseline() -> BaselineArtifact {
    bootstrap_candidate(&baseline_acl())
        .expect("baseline builds")
        .artifact
}

#[test]
fn un26_audit_an_unchanged_acl_compares_clean() {
    let baseline = approved_baseline();
    let outcome = compare(&baseline_acl(), &baseline, &baseline.digest).expect("compare");

    assert_eq!(outcome.report.diff_verdict, DiffVerdict::Match);
    assert!(
        outcome.findings.role_differences.is_empty(),
        "an unchanged ACL has nothing to report: {:?}",
        outcome.findings.role_differences
    );
}

/// Key and `euid` are read by different consumers — lookups by key, evaluation
/// by `euid` — so a mismatch is an entity that is two different principals
/// depending on who is asking.
#[test]
fn un26_audit_rejects_a_key_that_disagrees_with_its_euid() {
    let json = serde_json::json!({
        "users": {
            r#"User::"un26-innocent""#: {
                "euid": r#"User::"un26-attacker""#,
                "parents": [r#"UserGroup::"admin""#]
            }
        },
        "repos": { r#"Repository::"/""#: standard_repo() },
        "user_groups": standard_groups(),
        "merge_requests": {},
        "issues": {}
    })
    .to_string();

    let error = bootstrap_candidate(&json).expect_err("must reject");
    assert!(
        matches!(error, AuditError::KeyEuidMismatch { .. }),
        "{error}"
    );
}

/// Escalation by group hierarchy: nobody is added to `admin`, but a group whose
/// members are admins is given `admin` as a parent. Reading direct membership
/// would show no change at all.
#[test]
fn un26_audit_catches_a_transitive_group_escalation() {
    let baseline = approved_baseline();

    // A new group is introduced whose parent is `admin`, and a user is placed
    // in it. Nobody was added to `admin` itself, so a direct-membership check
    // sees nothing; the closure sees a new admin.
    let escalated_groups = serde_json::json!({
        r#"UserGroup::"admin""#: {
            "euid": r#"UserGroup::"admin""#,
            "parents": [r#"UserGroup::"matainer""#]
        },
        r#"UserGroup::"matainer""#: {
            "euid": r#"UserGroup::"matainer""#,
            "parents": [r#"UserGroup::"reader""#]
        },
        r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] },
        r#"UserGroup::"contractors""#: {
            "euid": r#"UserGroup::"contractors""#,
            "parents": [r#"UserGroup::"admin""#]
        }
    });
    let (admin_key, admin) = user("un26-admin", "admin");
    let (maint_key, maint) = user("un26-maintainer", "matainer");
    let (smuggled_key, smuggled) = user("un26-smuggled-admin", "contractors");
    let escalated = acl(
        serde_json::json!({ admin_key: admin, maint_key: maint, smuggled_key: smuggled }),
        escalated_groups,
        standard_repo(),
    );

    let outcome = compare(&escalated, &baseline, &baseline.digest).expect("compare");

    assert_eq!(outcome.report.diff_verdict, DiffVerdict::Mismatch);
    assert!(
        outcome
            .findings
            .admin_closure
            .contains(r#"User::"un26-smuggled-admin""#),
        "the closure must show the new group's member as an admin: {:?}",
        outcome.findings.admin_closure
    );
    assert!(
        outcome
            .findings
            .closure_only_admins
            .contains(r#"User::"un26-smuggled-admin""#),
        "and must flag that direct membership does not name them — which is \
         exactly what a direct-membership check would have missed: {:?}",
        outcome.findings.closure_only_admins
    );
    assert!(
        outcome
            .findings
            .role_differences
            .iter()
            .any(|d| d.contains("un26-smuggled-admin") && d.contains("admin")),
        "{:?}",
        outcome.findings.role_differences
    );
}

/// Escalation by reference: no user and no group changed, but the repository
/// now calls a different group its admins.
#[test]
fn un26_audit_catches_a_repo_admins_redirect() {
    let baseline = approved_baseline();

    let redirected = serde_json::json!({
        "euid": r#"Repository::"/""#,
        "is_private": true,
        "admins": r#"UserGroup::"matainer""#,
        "maintainers": r#"UserGroup::"matainer""#,
        "readers": r#"UserGroup::"reader""#,
        "parents": []
    });
    let (admin_key, admin) = user("un26-admin", "admin");
    let (maint_key, maint) = user("un26-maintainer", "matainer");
    let current = acl(
        serde_json::json!({ admin_key: admin, maint_key: maint }),
        standard_groups(),
        redirected,
    );

    let outcome = compare(&current, &baseline, &baseline.digest).expect("compare");

    assert_eq!(outcome.report.diff_verdict, DiffVerdict::Mismatch);
    assert!(
        outcome
            .findings
            .role_differences
            .iter()
            .any(|d| d.contains("admins now points at")),
        "the redirect itself must be named: {:?}",
        outcome.findings.role_differences
    );
}

/// A member added to a lower role is still a change to review.
#[test]
fn un26_audit_catches_a_pre_seeded_maintainer() {
    let baseline = approved_baseline();

    let (admin_key, admin) = user("un26-admin", "admin");
    let (maint_key, maint) = user("un26-maintainer", "matainer");
    let (extra_key, extra) = user("un26-smuggled", "matainer");
    let current = acl(
        serde_json::json!({ admin_key: admin, maint_key: maint, extra_key: extra }),
        standard_groups(),
        standard_repo(),
    );

    let outcome = compare(&current, &baseline, &baseline.digest).expect("compare");

    assert_eq!(outcome.report.diff_verdict, DiffVerdict::Mismatch);
    assert!(
        outcome
            .findings
            .role_differences
            .iter()
            .any(|d| d.contains("un26-smuggled") && d.contains("maintainer")),
        "{:?}",
        outcome.findings.role_differences
    );
}

/// A bootstrap run has nothing to compare against, so it must never read as a
/// pass — otherwise an already-tampered ACL gets blessed as the baseline.
#[test]
fn un26_audit_bootstrap_never_reports_a_pass() {
    let outcome = bootstrap_candidate(&baseline_acl()).expect("bootstrap");

    assert_eq!(outcome.report.diff_verdict, DiffVerdict::NotCompared);
    assert_ne!(outcome.report.diff_verdict, DiffVerdict::Match);
    assert!(!outcome.artifact.digest.is_empty());
    assert!(
        !outcome.artifact.roles.admins.is_empty(),
        "the candidate carries the role projection a later compare needs"
    );
}

/// The reason the approved digest is checked separately: an attacker who can
/// replace the baseline file can also recompute the digest inside it, so a
/// self-consistent artifact proves nothing on its own.
#[test]
fn un26_audit_rejects_a_wholesale_replaced_baseline_even_with_a_recomputed_digest() {
    let approved = approved_baseline();

    // Someone rewrites the baseline to name themselves admin, and recomputes
    // its internal digest so the artifact is perfectly self-consistent.
    let (attacker_key, attacker) = user("un26-attacker", "admin");
    let forged_acl = acl(
        serde_json::json!({ attacker_key: attacker }),
        standard_groups(),
        standard_repo(),
    );
    let forged = bootstrap_candidate(&forged_acl)
        .expect("forged builds")
        .artifact;
    assert_ne!(forged.digest, approved.digest);

    // Comparing against the ledger's approved digest catches it.
    let error = compare(&forged_acl, &forged, &approved.digest)
        .expect_err("a replaced baseline must not be accepted");
    assert!(
        matches!(error, AuditError::BaselineNotApproved { .. }),
        "{error}"
    );
}

/// An artifact whose recorded digest does not describe its own snapshot is
/// broken regardless of what the ledger says.
#[test]
fn un26_audit_rejects_an_internally_inconsistent_baseline() {
    let mut baseline = approved_baseline();
    let approved_digest = baseline.digest.clone();
    baseline.normalized_snapshot.push(' ');

    let error = compare(&baseline_acl(), &baseline, &approved_digest)
        .expect_err("an inconsistent artifact must not be used");
    assert!(
        matches!(error, AuditError::BaselineTampered { .. }),
        "{error}"
    );
}

/// The sanitized report is what leaves the restricted channel, so it must carry
/// no names.
#[test]
fn un26_audit_the_sanitized_report_names_nobody() {
    let baseline = approved_baseline();
    let outcome = compare(&baseline_acl(), &baseline, &baseline.digest).expect("compare");
    let json = serde_json::to_string(&outcome.report).expect("serialize");

    assert!(!json.contains("un26-admin"), "{json}");
    assert!(!json.contains("un26-maintainer"), "{json}");
    assert!(json.contains("digest"), "{json}");
    assert!(json.contains("closure_count"), "{json}");
    assert!(json.contains("diff_verdict"), "{json}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json)
            .expect("json")
            .as_object()
            .expect("object")
            .len(),
        3,
        "the core returns exactly three fields; UN-29 adds source_summary: {json}"
    );
}

/// `parents` is a set of groups, so its order is presentation too. A reformat
/// that grants exactly the same access must not read as tampering — otherwise
/// the audit cries wolf and stops being read.
#[test]
fn un26_audit_digest_ignores_parents_order() {
    let groups_one = serde_json::json!({
        r#"UserGroup::"admin""#: {
            "euid": r#"UserGroup::"admin""#,
            "parents": [r#"UserGroup::"matainer""#, r#"UserGroup::"reader""#]
        },
        r#"UserGroup::"matainer""#: {
            "euid": r#"UserGroup::"matainer""#,
            "parents": [r#"UserGroup::"reader""#]
        },
        r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
    });
    let groups_two = serde_json::json!({
        r#"UserGroup::"admin""#: {
            "euid": r#"UserGroup::"admin""#,
            // Same two groups, written the other way round.
            "parents": [r#"UserGroup::"reader""#, r#"UserGroup::"matainer""#]
        },
        r#"UserGroup::"matainer""#: {
            "euid": r#"UserGroup::"matainer""#,
            "parents": [r#"UserGroup::"reader""#]
        },
        r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
    });

    let (admin_key, admin) = user("un26-admin", "admin");
    let one = acl(
        serde_json::json!({ admin_key.clone(): admin.clone() }),
        groups_one,
        standard_repo(),
    );
    let two = acl(
        serde_json::json!({ admin_key: admin }),
        groups_two,
        standard_repo(),
    );

    let baseline = bootstrap_candidate(&one).expect("baseline").artifact;
    let outcome = compare(&two, &baseline, &baseline.digest).expect("compare");
    assert_eq!(
        outcome.report.diff_verdict,
        DiffVerdict::Match,
        "reordering a parents list is not a change"
    );
}

/// Key order is presentation, not content: the same ACL must digest the same.
#[test]
fn un26_audit_digest_ignores_key_order() {
    let baseline = approved_baseline();

    let (maint_key, maint) = user("un26-maintainer", "matainer");
    let (admin_key, admin) = user("un26-admin", "admin");
    let reordered = acl(
        serde_json::json!({ maint_key: maint, admin_key: admin }),
        standard_groups(),
        standard_repo(),
    );

    let outcome = compare(&reordered, &baseline, &baseline.digest).expect("compare");
    assert_eq!(
        outcome.report.diff_verdict,
        DiffVerdict::Match,
        "reordering keys is not a change"
    );
}
