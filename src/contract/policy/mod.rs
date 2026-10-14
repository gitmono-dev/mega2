use std::fmt::{self, Display};

pub mod admin_resolver;
pub mod authz_audit;
pub mod baseline_pointer;
pub mod baseline_promotion;
pub mod builder;
pub mod context;
pub mod enforcement;
pub mod entitystore;
pub mod guard;
pub mod notify;
mod objects;
pub mod resource;
pub mod secure_admission;
pub mod secure_artifact;
pub mod secure_capacity;
pub mod secure_counter;
pub mod secure_hardcap;
pub mod secure_lifecycle;
pub mod secure_producer;
pub mod secure_sweep;
#[cfg(test)]
mod tests;
// The secure_* suites are Linux-only by design (`require_linux` fails closed
// elsewhere): their guarantees rest on renameat2/RENAME_NOREPLACE and flock
// semantics. `capacity_policy`, `admission`, and `producer_mapping` stay
// portable because they only lock formulas and pure judgments.
pub mod util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionEnum {
    // ** Anyone
    UnprotectedRequest,
    ViewRepo,
    // PullRepo,
    // ForkRepo,
    PushRepo,
    // OpenIssue,
    // ** Maintainer
    CreateMergeRequest,
    EditIssue,
    EditMergeRequest,
    AssignIssue,
    ApproveMergeRequest,
    // ** Admin
    AddMaintainer,
    AddAdmin,
    DeleteRepo,
    DeleteIssue,
    DeleteMergeRequest,
}

impl Display for ActionEnum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ActionEnum::UnprotectedRequest => "unprotectedRequest",
            ActionEnum::ViewRepo => "viewRepo",
            ActionEnum::PushRepo => "pushRepo",
            ActionEnum::CreateMergeRequest => "createMergeRequest",
            ActionEnum::EditIssue => "editIssue",
            ActionEnum::EditMergeRequest => "editMergeRequest",
            ActionEnum::AssignIssue => "assignIssue",
            ActionEnum::ApproveMergeRequest => "approveMergeRequest",
            ActionEnum::AddMaintainer => "addMaintainer",
            ActionEnum::AddAdmin => "addAdmin",
            ActionEnum::DeleteRepo => "deleteRepo",
            ActionEnum::DeleteIssue => "deleteIssue",
            ActionEnum::DeleteMergeRequest => "deleteMergeRequest",
        };
        write!(f, "{s}")
    }
}

impl From<&str> for ActionEnum {
    fn from(s: &str) -> Self {
        match s {
            "viewRepo" => ActionEnum::ViewRepo,
            "pushRepo" => ActionEnum::PushRepo,
            "createMergeRequest" => ActionEnum::CreateMergeRequest,
            "editIssue" => ActionEnum::EditIssue,
            "editMergeRequest" => ActionEnum::EditMergeRequest,
            "assignIssue" => ActionEnum::AssignIssue,
            "approveMergeRequest" => ActionEnum::ApproveMergeRequest,
            "addMaintainer" => ActionEnum::AddMaintainer,
            "addAdmin" => ActionEnum::AddAdmin,
            "deleteRepo" => ActionEnum::DeleteRepo,
            "deleteIssue" => ActionEnum::DeleteIssue,
            "deleteMergeRequest" => ActionEnum::DeleteMergeRequest,
            _ => ActionEnum::UnprotectedRequest,
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::Once;

    use cedar_policy::{Authorizer, Context, Entities, PolicySet, Request};

    use crate::{
        common::errors::SaturnContextError,
        contract::policy::{context::CedarContext, entitystore::EntityStore, util::SaturnEUid},
    };

    static INIT: Once = Once::new();

    fn init_tracing() {
        INIT.call_once(|| {
            let _ = tracing_subscriber::fmt().pretty().try_init();
        });
    }

    #[test]
    fn test_without_entity() {
        const POLICY_SRC: &str = r#"
    permit(principal == User::"alice", action == Action::"view", resource == File::"93");
    "#;
        let policy: PolicySet = POLICY_SRC.parse().unwrap();

        let action = r#"Action::"view""#.parse().unwrap();
        let alice = r#"User::"alice""#.parse().unwrap();
        let file = r#"File::"93""#.parse().unwrap();
        let request = Request::new(alice, action, file, Context::empty(), None).unwrap();

        let entities = Entities::empty();
        let authorizer = Authorizer::new();
        let answer = authorizer.is_authorized(&request, &policy, &entities);

        // Should output `Allow`
        println!("{:?}", answer.decision());

        let action = r#"Action::"view""#.parse().unwrap();
        let bob = r#"User::"bob""#.parse().unwrap();
        let file = r#"File::"93""#.parse().unwrap();
        let request = Request::new(bob, action, file, Context::empty(), None).unwrap();

        let answer = authorizer.is_authorized(&request, &policy, &entities);

        // Should output `Deny`
        println!("{:?}", answer.decision());
    }

    fn load_context(entities: EntityStore) -> CedarContext {
        CedarContext::new(entities).unwrap()
    }

    #[test]
    fn test_project_path_policy() {
        init_tracing();
        let entities: EntityStore =
            serde_json::from_str(include_str!("fixtures/project.mega.json")).unwrap();

        let app_context = load_context(entities);
        let admin: SaturnEUid = r#"User::"benjamin_747""#.parse().unwrap();
        let maintainer: SaturnEUid = r#"User::"besscroft""#.parse().unwrap();
        let anyone: SaturnEUid = r#"User::"anyone""#.parse().unwrap();
        let resource: SaturnEUid = r#"Repository::"project""#.parse().unwrap();

        // admin can view repo
        assert!(
            app_context
                .is_authorized(
                    &admin,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_ok()
        );
        // admin can delete repo
        assert!(
            app_context
                .is_authorized(
                    &admin,
                    r#"Action::"deleteRepo""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_ok()
        );

        // anyone can view public_repo
        assert!(
            app_context
                .is_authorized(
                    &anyone,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_ok()
        );

        assert!(
            app_context
                .is_authorized(
                    &anyone,
                    r#"Action::"openIssue""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty(),
                )
                .is_ok()
        );

        // normal user can't assign issue
        assert!(
            app_context
                .is_authorized(
                    &anyone,
                    r#"Action::"assignIssue""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_err_and(|e| matches!(e, SaturnContextError::AuthDenied(_)))
        );
        assert!(
            app_context
                .is_authorized(
                    &anyone,
                    r#"Action::"approveMergeRequest""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_err_and(|e| matches!(e, SaturnContextError::AuthDenied(_)))
        );

        assert!(
            app_context
                .is_authorized(
                    &maintainer,
                    r#"Action::"approveMergeRequest""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_ok()
        );
    }

    #[test]
    fn test_private_path_policy() {
        init_tracing();
        let parent_entities: EntityStore =
            serde_json::from_str(include_str!("fixtures/project.mega.json")).unwrap();
        let mut entities: EntityStore =
            serde_json::from_str(include_str!("fixtures/private.mega.json")).unwrap();

        entities.merge(parent_entities);

        let app_context = load_context(entities);
        let p_admin: SaturnEUid = r#"User::"benjamin_747""#.parse().unwrap();
        let admin: SaturnEUid = r#"User::"private""#.parse().unwrap();
        let anyone: SaturnEUid = r#"User::"anyone""#.parse().unwrap();
        let private_project: SaturnEUid = r#"Repository::"/project/bens_private""#.parse().unwrap();

        // admin under project should also have permisisons
        assert!(
            app_context
                .is_authorized(
                    &p_admin,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &private_project,
                    Context::empty()
                )
                .is_ok()
        );

        assert!(
            app_context
                .is_authorized(
                    &admin,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &private_project,
                    Context::empty()
                )
                .is_ok()
        );

        // not public, should deny
        assert!(
            app_context
                .is_authorized(
                    &anyone,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &private_project,
                    Context::empty()
                )
                .is_err_and(|e| matches!(e, SaturnContextError::AuthDenied(_)))
        );
    }
}

#[cfg(test)]
mod acl_audit_scenarios {
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
}

#[cfg(test)]
mod admin_source {
    //! UN-04: admin power comes from the ACL, and only from the ACL.
    //!
    //! The policy file used to permit two named individuals everything on every
    //! resource. That made it a second, invisible source of admin: reading the ACL
    //! did not tell you who was privileged, and removing someone from the admin
    //! group did not take their access away. Admin is now exactly membership of
    //! `UserGroup::"admin"` in the entity store, seeded from `monorepo.admin`.
    //!
    //! These cases pin both halves — a listed member is an admin, and the formerly
    //! hardcoded names are not — because only the pair proves the source moved
    //! rather than widened.

    use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request, Schema};

    use crate::contract::policy::{
        builder::build_from_json, entitystore::generate_entity, util::SaturnEUid,
    };

    /// The names that used to be hardcoded into the policy file.
    const FORMERLY_HARDCODED: &[&str] = &["genedna", "benjamin-747"];

    /// Admin-only actions: if the source of admin power moved, every one of these
    /// must follow it.
    const ADMIN_ACTIONS: &[&str] = &[
        "addMaintainer",
        "addAdmin",
        "deleteRepo",
        "deleteIssue",
        "deleteMergeRequest",
    ];

    fn entities_with_admin(admin: &str) -> Entities {
        let json = generate_entity(&[admin.to_string()], "/").expect("real init product");
        build_from_json(&json)
            .expect("fixture builds")
            .entities()
            .clone()
    }

    fn euid(s: &str) -> SaturnEUid {
        s.parse().unwrap_or_else(|e| panic!("parse euid {s}: {e}"))
    }

    fn allows(entities: &Entities, principal: &str, action: &str) -> bool {
        let (schema, _) =
            Schema::from_cedarschema_str(include_str!("mega.cedarschema")).expect("schema");
        let policies: PolicySet = include_str!("mega_policies.cedar")
            .parse()
            .expect("policies");
        let request = Request::new(
            euid(&format!(r#"User::"{principal}""#)).into(),
            euid(&format!(r#"Action::"{action}""#)).into(),
            euid(r#"Repository::"/""#).into(),
            Context::empty(),
            Some(&schema),
        )
        .expect("request");
        Authorizer::new()
            .is_authorized(&request, &policies, entities)
            .decision()
            == Decision::Allow
    }

    #[test]
    fn un04_the_acl_admin_holds_every_admin_action() {
        let entities = entities_with_admin("un04-listed-admin");
        for action in ADMIN_ACTIONS {
            assert!(
                allows(&entities, "un04-listed-admin", action),
                "an ACL admin must hold {action}"
            );
        }
    }

    /// The formerly hardcoded names get no special treatment: with someone else in
    /// the admin group, they are ordinary principals.
    #[test]
    fn un04_the_formerly_hardcoded_names_have_no_privilege() {
        let entities = entities_with_admin("un04-listed-admin");
        for name in FORMERLY_HARDCODED {
            for action in ADMIN_ACTIONS {
                assert!(
                    !allows(&entities, name, action),
                    "{name} must not hold {action} — admin comes from the ACL now"
                );
            }
            assert!(
                !allows(&entities, name, "pushRepo"),
                "{name} must not be able to push either"
            );
        }
    }

    /// …and they are not *banned* either: listing one in the ACL grants admin
    /// exactly as it would for any other name. The fix removed a special case, it
    /// did not add one.
    #[test]
    fn un04_a_formerly_hardcoded_name_becomes_admin_only_by_being_listed() {
        for name in FORMERLY_HARDCODED {
            let entities = entities_with_admin(name);
            for action in ADMIN_ACTIONS {
                assert!(
                    allows(&entities, name, action),
                    "{name} listed in the ACL must hold {action} like anyone else"
                );
            }
        }
    }

    /// Removing someone from the admin group actually removes their power — the
    /// property the hardcoded rule quietly broke.
    #[test]
    fn un04_dropping_an_admin_from_the_acl_removes_their_privilege() {
        let before = entities_with_admin("un04-departing-admin");
        assert!(allows(&before, "un04-departing-admin", "deleteRepo"));

        let after = entities_with_admin("un04-remaining-admin");
        assert!(
            !allows(&after, "un04-departing-admin", "deleteRepo"),
            "an ACL that no longer lists them must no longer authorize them"
        );
    }

    /// The rest of the policy is untouched: the roles UN-09 settled still behave.
    #[test]
    fn un04_removing_the_hardcoded_rule_left_the_role_policies_intact() {
        let entities = entities_with_admin("un04-listed-admin");

        assert!(allows(&entities, "un04-listed-admin", "pushRepo"));
        assert!(
            !allows(&entities, "un04-nobody", "viewRepo"),
            "a private repository still refuses an unlisted principal"
        );
    }
}

#[cfg(test)]
mod push_matrix {
    //! UN-09: who may push, per role and per repository visibility.
    //!
    //! `pushRepo` used to sit in two places it did not belong: the public-repository
    //! block (so *any* principal could push to a public repo) and the reader block
    //! (so read access implied write access). Both readings contradict what the
    //! role names promise. It now lives with the maintainer and admin actions.
    //!
    //! The fixture is derived from the real init product — `generate_entity` builds
    //! the same shape the server seeds, including the historical `matainer`
    //! spelling (DEFER-UN-06) — and then role members are added, so the matrix is
    //! evaluated against the entity shape production actually produces rather than
    //! a hand-drawn approximation.

    use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request, Schema};

    use crate::contract::policy::{
        builder::build_from_json, entitystore::generate_entity, util::SaturnEUid,
    };

    const ADMIN: &str = "un09-admin";
    const MAINTAINER: &str = "un09-maintainer";
    const READER: &str = "un09-reader";
    const OUTSIDER: &str = "un09-outsider";

    /// Derivation chain, made explicit: start from the real init product for the
    /// root repository, then add one member per role and set visibility.
    fn fixture(is_private: bool) -> Entities {
        let seeded: serde_json::Value = serde_json::from_str(
            &generate_entity(&[ADMIN.to_string()], "/").expect("real init product"),
        )
        .expect("init product is JSON");

        let mut value = seeded;
        let users = value
            .get_mut("users")
            .and_then(serde_json::Value::as_object_mut)
            .expect("users section");
        for (name, group) in [(MAINTAINER, "matainer"), (READER, "reader")] {
            users.insert(
                format!(r#"User::"{name}""#),
                serde_json::json!({
                    "euid": format!(r#"User::"{name}""#),
                    "parents": [format!(r#"UserGroup::"{group}""#)]
                }),
            );
        }
        // `un09-outsider` is deliberately absent: an unknown principal is exactly
        // the "anyone" case the public-repository block used to let push.

        value
            .get_mut("repos")
            .and_then(serde_json::Value::as_object_mut)
            .expect("repos section")
            .get_mut(r#"Repository::"/""#)
            .and_then(serde_json::Value::as_object_mut)
            .expect("root repository")
            .insert("is_private".to_string(), serde_json::json!(is_private));

        build_from_json(&value.to_string())
            .expect("fixture builds")
            .entities()
            .clone()
    }

    fn euid(s: &str) -> SaturnEUid {
        s.parse().unwrap_or_else(|e| panic!("parse euid {s}: {e}"))
    }

    fn allows(entities: &Entities, principal: &str, action: &str) -> bool {
        let (schema, _) =
            Schema::from_cedarschema_str(include_str!("mega.cedarschema")).expect("schema");
        let policies: PolicySet = include_str!("mega_policies.cedar")
            .parse()
            .expect("policies");
        let request = Request::new(
            euid(&format!(r#"User::"{principal}""#)).into(),
            euid(&format!(r#"Action::"{action}""#)).into(),
            euid(r#"Repository::"/""#).into(),
            Context::empty(),
            Some(&schema),
        )
        .expect("request");
        Authorizer::new()
            .is_authorized(&request, &policies, entities)
            .decision()
            == Decision::Allow
    }

    /// `(role, private repo, public repo)` — pushing depends on the role, never on
    /// the repository being public.
    const PUSH_MATRIX: &[(&str, bool, bool)] = &[
        (ADMIN, true, true),
        (MAINTAINER, true, true),
        (READER, false, false),
        (OUTSIDER, false, false),
    ];

    #[test]
    fn un09_push_matrix_across_roles_and_visibility() {
        let private = fixture(true);
        let public = fixture(false);

        for (role, on_private, on_public) in PUSH_MATRIX {
            assert_eq!(
                allows(&private, role, "pushRepo"),
                *on_private,
                "{role} pushing to a private repo"
            );
            assert_eq!(
                allows(&public, role, "pushRepo"),
                *on_public,
                "{role} pushing to a public repo"
            );
        }
    }

    /// The specific hole this card closes: a public repository used to hand write
    /// access to principals who are in no group at all.
    #[test]
    fn un09_a_public_repository_no_longer_grants_push_to_anyone() {
        let public = fixture(false);

        assert!(
            !allows(&public, OUTSIDER, "pushRepo"),
            "public means readable, not writable"
        );
        assert!(
            allows(&public, OUTSIDER, "viewRepo"),
            "reading a public repo must still work — the fix must not over-shoot"
        );
        assert!(
            allows(&public, OUTSIDER, "pullRepo"),
            "cloning a public repo must still work"
        );
    }

    /// The second hole: read access used to imply write access.
    #[test]
    fn un09_a_reader_can_read_but_not_push() {
        for entities in [fixture(true), fixture(false)] {
            assert!(allows(&entities, READER, "viewRepo"));
            assert!(allows(&entities, READER, "pullRepo"));
            assert!(allows(&entities, READER, "createMergeRequest"));
            assert!(
                !allows(&entities, READER, "pushRepo"),
                "a reader proposes changes; it does not write to the repository"
            );
        }
    }

    /// Admin's grant must not rely on the ACL keeping admin inside the maintainer
    /// group — the fixture's hierarchy could be edited away.
    #[test]
    fn un09_admin_may_push_without_inheriting_the_maintainer_group() {
        let json = serde_json::json!({
            "users": {
                format!(r#"User::"{ADMIN}""#): {
                    "euid": format!(r#"User::"{ADMIN}""#),
                    "parents": [r#"UserGroup::"admin""#]
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
                // admin deliberately does NOT inherit matainer here.
                r#"UserGroup::"admin""#: { "euid": r#"UserGroup::"admin""#, "parents": [] },
                r#"UserGroup::"matainer""#: { "euid": r#"UserGroup::"matainer""#, "parents": [] },
                r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
            },
            "merge_requests": {},
            "issues": {}
        })
        .to_string();
        let entities = build_from_json(&json)
            .expect("fixture builds")
            .entities()
            .clone();

        assert!(allows(&entities, ADMIN, "pushRepo"));
    }

    /// The other actions in the two edited blocks must be exactly as before.
    #[test]
    fn un09_no_other_action_changed_in_the_edited_blocks() {
        let public = fixture(false);
        let private = fixture(true);

        for action in [
            "viewRepo",
            "pullRepo",
            "forkRepo",
            "openIssue",
            "createMergeRequest",
        ] {
            assert!(
                allows(&public, OUTSIDER, action),
                "public block still grants {action}"
            );
            assert!(
                allows(&private, READER, action),
                "reader block still grants {action}"
            );
        }
    }
}

#[cfg(test)]
mod snapshot_cache {
    //! UN-21 cache-residue gate: the UN-14 snapshot cache must never serve a
    //! previous dimension's decision after a rebuild.
    //!
    //! The matrix in [`super::snapshot_matrix`] already evaluates against the cached
    //! `Entities`, so a stale cache fails there too. These cases pin the cache
    //! semantics themselves: a rebuild publishes a *new* immutable snapshot rather
    //! than mutating the old one, an already-held handle keeps serving its own
    //! (older) content, and repeated rebuilds across every dimension leave no
    //! residue in either direction.

    use std::sync::Arc;

    use crate::contract::policy::{
        entitystore::SharedEntityStore,
        snapshot_matrix::{
            GROUPS_ADMIN_MAINTAINER_READER, REPO_DEFAULT_ATTRS, authz_json, decide, user,
        },
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
}

#[cfg(test)]
mod snapshot_matrix {
    //! UN-21: 快照重建全维即时生效矩阵.
    //!
    //! Locks the claim that a rebuilt shared snapshot reflects a change on *every*
    //! authorization input dimension of `/.mega_cedar.json`, with no residue from
    //! the previous snapshot. The real schema (`objects.rs` / `mega.cedarschema`)
    //! has eight atomic input dimensions:
    //!
    //! | 维 | 数据源字段 | 翻转的判定 |
    //! |---|---|---|
    //! | 一 | `users[].parents` 加入 `admin` | admin-only action |
    //! | 二 | `users[].parents` 加入/移出 `matainer` | maintainer-only action |
    //! | 三 | `users[].parents` 加入/移出 `reader` | reader action |
    //! | 四 | `user_groups[].parents` 组层级 | 继承链上的 reader action |
    //! | 五 | `repos[].is_private` | 无角色主体的匿名可见性 |
    //! | 六 | `repos[].admins` 组引用 | admin-only action |
    //! | 七 | `repos[].maintainers` 组引用 | maintainer-only action |
    //! | 八 | `repos[].readers` 组引用 | reader action |
    //!
    //! `merge_requests` / `issues` are seeded empty and do not participate in
    //! evaluation, so they are not input dimensions (see the card's evidence
    //! table).
    //!
    //! Every assertion evaluates against the snapshot's **cached** `Entities`
    //! (UN-14), not a freshly derived store, so a stale cache would fail the
    //! matrix rather than hide behind a re-derivation.

    use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request, Schema};

    use crate::contract::policy::{entitystore::SharedEntityStore, util::SaturnEUid};

    /// Group hierarchy of the real seed: admin → matainer → reader (`matainer` is
    /// the current spelling, DEFER-UN-06).
    pub(crate) const GROUPS_ADMIN_MAINTAINER_READER: &str = r#"
        "UserGroup::\"admin\"": { "euid": "UserGroup::\"admin\"", "parents": ["UserGroup::\"matainer\""] },
        "UserGroup::\"matainer\"": { "euid": "UserGroup::\"matainer\"", "parents": ["UserGroup::\"reader\""] },
        "UserGroup::\"reader\"": { "euid": "UserGroup::\"reader\"", "parents": [] }
    "#;

    /// Build a `/.mega_cedar.json` body from the parts each dimension varies.
    ///
    /// `users` / `groups` are raw object bodies (no surrounding braces) so a test
    /// can vary exactly one dimension and leave the rest identical.
    pub(crate) fn authz_json(users: &str, groups: &str, repo_attrs: &str) -> String {
        format!(
            r#"{{
                "users": {{ {users} }},
                "repos": {{
                    "Repository::\"/\"": {{
                        "euid": "Repository::\"/\"",
                        {repo_attrs},
                        "parents": []
                    }}
                }},
                "user_groups": {{ {groups} }},
                "merge_requests": {{}},
                "issues": {{}}
            }}"#
        )
    }

    /// Repository attributes of the real seed (private, groups wired to the
    /// same-named groups).
    pub(crate) const REPO_DEFAULT_ATTRS: &str = r#""is_private": true,
                        "admins": "UserGroup::\"admin\"",
                        "maintainers": "UserGroup::\"matainer\"",
                        "readers": "UserGroup::\"reader\"""#;

    pub(crate) fn user(name: &str, parents: &[&str]) -> String {
        let parents = parents
            .iter()
            .map(|g| format!(r#""UserGroup::\"{g}\"""#))
            .collect::<Vec<_>>()
            .join(", ");
        format!(r#""User::\"{name}\"": {{ "euid": "User::\"{name}\"", "parents": [{parents}] }}"#)
    }

    fn euid(s: &str) -> SaturnEUid {
        s.parse().unwrap_or_else(|e| panic!("parse euid {s}: {e}"))
    }

    /// Decide against the snapshot's cached entities, using the same schema and
    /// policy set the runtime uses.
    pub(crate) fn decide(
        entities: &Entities,
        principal: &str,
        action: &str,
        resource: &str,
    ) -> bool {
        let (schema, _) =
            Schema::from_cedarschema_str(include_str!("mega.cedarschema")).expect("schema parses");
        let policies: PolicySet = include_str!("mega_policies.cedar")
            .parse()
            .expect("policies parse");
        let request = Request::new(
            euid(principal).into(),
            euid(action).into(),
            euid(resource).into(),
            Context::empty(),
            Some(&schema),
        )
        .expect("request builds");
        Authorizer::new()
            .is_authorized(&request, &policies, entities)
            .decision()
            == Decision::Allow
    }

    /// Build the first snapshot, assert `before`, rebuild from `after_json`, assert
    /// `after` — the shape every dimension test shares.
    fn assert_rebuild_flips(
        before_json: &str,
        after_json: &str,
        principal: &str,
        action: &str,
        before: bool,
        after: bool,
        what: &str,
    ) {
        let shared = SharedEntityStore::new();
        shared.ensure(before_json).expect("first build");
        let first = shared.snapshot().expect("snapshot after first build");
        assert_eq!(
            decide(first.entities(), principal, action, r#"Repository::"/""#),
            before,
            "{what}: decision before the rebuild"
        );

        shared.swap(after_json).expect("rebuild");
        let second = shared.snapshot().expect("snapshot after rebuild");
        assert_eq!(
            decide(second.entities(), principal, action, r#"Repository::"/""#),
            after,
            "{what}: decision after the rebuild must reflect the new data source"
        );
        assert!(
            !shared.is_dirty(),
            "{what}: a successful rebuild clears dirty"
        );
    }

    #[test]
    fn un21_dim1_user_parents_admin_membership_takes_effect() {
        let before = authz_json(
            &user("dim1", &["reader"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim1", &["admin"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim1""#,
            r#"Action::"addAdmin""#,
            false,
            true,
            "维一 users[].parents 加入 admin",
        );
    }

    #[test]
    fn un21_dim2_user_parents_maintainer_membership_takes_effect() {
        let before = authz_json(
            &user("dim2", &["matainer"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim2", &["reader"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim2""#,
            r#"Action::"approveMergeRequest""#,
            true,
            false,
            "维二 users[].parents 移出 matainer",
        );
    }

    #[test]
    fn un21_dim3_user_parents_reader_membership_takes_effect() {
        let before = authz_json(
            &user("dim3", &["reader"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim3", &[]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim3""#,
            r#"Action::"pullRepo""#,
            true,
            false,
            "维三 users[].parents 移出 reader",
        );
    }

    #[test]
    fn un21_dim4_user_group_parents_hierarchy_takes_effect() {
        // The user only ever belongs to `admin`; what changes is whether `admin`
        // still inherits from `matainer` (and thus `reader`).
        let detached_groups = r#"
            "UserGroup::\"admin\"": { "euid": "UserGroup::\"admin\"", "parents": [] },
            "UserGroup::\"matainer\"": { "euid": "UserGroup::\"matainer\"", "parents": ["UserGroup::\"reader\""] },
            "UserGroup::\"reader\"": { "euid": "UserGroup::\"reader\"", "parents": [] }
        "#;
        let before = authz_json(
            &user("dim4", &["admin"]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim4", &["admin"]),
            detached_groups,
            REPO_DEFAULT_ATTRS,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim4""#,
            r#"Action::"pullRepo""#,
            true,
            false,
            "维四 user_groups[].parents 断开继承链",
        );
    }

    #[test]
    fn un21_dim5_repo_is_private_flip_takes_effect() {
        // A principal with no group membership at all: only `is_private` decides.
        let public_repo = r#""is_private": false,
                        "admins": "UserGroup::\"admin\"",
                        "maintainers": "UserGroup::\"matainer\"",
                        "readers": "UserGroup::\"reader\"""#;
        let before = authz_json(
            &user("dim5", &[]),
            GROUPS_ADMIN_MAINTAINER_READER,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim5", &[]),
            GROUPS_ADMIN_MAINTAINER_READER,
            public_repo,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim5""#,
            r#"Action::"viewRepo""#,
            false,
            true,
            "维五 repos[].is_private 翻转",
        );
    }

    /// Group set with a second, initially unused group per role, so a test can
    /// repoint one repo role reference without touching user membership.
    const GROUPS_WITH_ALTERNATES: &str = r#"
        "UserGroup::\"admin\"": { "euid": "UserGroup::\"admin\"", "parents": ["UserGroup::\"matainer\""] },
        "UserGroup::\"matainer\"": { "euid": "UserGroup::\"matainer\"", "parents": ["UserGroup::\"reader\""] },
        "UserGroup::\"reader\"": { "euid": "UserGroup::\"reader\"", "parents": [] },
        "UserGroup::\"other\"": { "euid": "UserGroup::\"other\"", "parents": [] }
    "#;

    #[test]
    fn un21_dim6_repo_admins_group_reference_takes_effect() {
        let repointed = r#""is_private": true,
                        "admins": "UserGroup::\"other\"",
                        "maintainers": "UserGroup::\"matainer\"",
                        "readers": "UserGroup::\"reader\"""#;
        let before = authz_json(
            &user("dim6", &["admin"]),
            GROUPS_WITH_ALTERNATES,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(&user("dim6", &["admin"]), GROUPS_WITH_ALTERNATES, repointed);
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim6""#,
            r#"Action::"addAdmin""#,
            true,
            false,
            "维六 repos[].admins 组引用改指",
        );
    }

    #[test]
    fn un21_dim7_repo_maintainers_group_reference_takes_effect() {
        let repointed = r#""is_private": true,
                        "admins": "UserGroup::\"admin\"",
                        "maintainers": "UserGroup::\"other\"",
                        "readers": "UserGroup::\"reader\"""#;
        let before = authz_json(
            &user("dim7", &["matainer"]),
            GROUPS_WITH_ALTERNATES,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim7", &["matainer"]),
            GROUPS_WITH_ALTERNATES,
            repointed,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim7""#,
            r#"Action::"approveMergeRequest""#,
            true,
            false,
            "维七 repos[].maintainers 组引用改指",
        );
    }

    #[test]
    fn un21_dim8_repo_readers_group_reference_takes_effect() {
        let repointed = r#""is_private": true,
                        "admins": "UserGroup::\"admin\"",
                        "maintainers": "UserGroup::\"matainer\"",
                        "readers": "UserGroup::\"other\"""#;
        let before = authz_json(
            &user("dim8", &["reader"]),
            GROUPS_WITH_ALTERNATES,
            REPO_DEFAULT_ATTRS,
        );
        let after = authz_json(
            &user("dim8", &["reader"]),
            GROUPS_WITH_ALTERNATES,
            repointed,
        );
        assert_rebuild_flips(
            &before,
            &after,
            r#"User::"dim8""#,
            r#"Action::"pullRepo""#,
            true,
            false,
            "维八 repos[].readers 组引用改指",
        );
    }
}
