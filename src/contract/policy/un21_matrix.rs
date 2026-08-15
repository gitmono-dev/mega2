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
pub(crate) fn decide(entities: &Entities, principal: &str, action: &str, resource: &str) -> bool {
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
