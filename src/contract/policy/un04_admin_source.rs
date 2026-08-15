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
