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
