//! UN-26: auditing the authorization ACL against an approved baseline.
//!
//! Turning enforcement on trusts whatever the ACL currently says. If it was
//! tampered with while nobody was enforcing it, enabling `enforce` would
//! cement that tampering — so the ACL has to be compared against a baseline a
//! human approved.
//!
//! The trusted source is the **approved artifact** (a normalized snapshot plus
//! its role projection), not a bare digest. A digest alone cannot be trusted to
//! detect tampering: whoever can replace the snapshot can recompute it. The
//! digest is an integrity index over the artifact, and the *approval* is what
//! makes an artifact trusted — which is why `compare` also checks the digest
//! recorded out-of-band in the approval ledger.
//!
//! Two phases, deliberately asymmetric:
//!
//! * `bootstrap-candidate` produces a candidate artifact and **cannot report a
//!   pass**. Its verdict is always `not_compared`: there is nothing to compare
//!   against yet, and a first run reporting "audit passed" would defeat the
//!   entire purpose.
//! * `compare` checks the artifact's integrity, then the current snapshot
//!   against it, and reports what differs.
//!
//! Output is split by sensitivity. The sanitized report carries four fields and
//! no names; the full closure and the role-level differences are findings for
//! the restricted channel only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::contract::policy::builder::{BuilderError, build_from_json};

/// Why an audit could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("snapshot is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot failed build-time validation: {0}")]
    Build(#[from] BuilderError),
    /// A section's map key disagrees with the entity's own `euid`. Lookups go
    /// through the key while evaluation goes through the `euid`, so a mismatch
    /// is an entity that is one principal to the reader and another to the
    /// policy engine.
    #[error("entity key `{key}` disagrees with its euid `{euid}`")]
    KeyEuidMismatch { key: String, euid: String },
    #[error(
        "baseline artifact is internally inconsistent: recorded digest {recorded} does not match its snapshot ({actual})"
    )]
    BaselineTampered { recorded: String, actual: String },
    #[error(
        "baseline digest {found} does not match the approved digest {expected} from the ledger"
    )]
    BaselineNotApproved { expected: String, found: String },
}

/// Verdict of an audit run. `not_compared` is a first-class outcome, not a
/// missing value: it is how a bootstrap run says "no judgement was made".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffVerdict {
    NotCompared,
    Match,
    Mismatch,
}

/// The sanitized report: no usernames, no paths (UN-29 embeds
/// `source_summary`; this core returns the first three fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizedReport {
    pub digest: String,
    pub closure_count: usize,
    pub diff_verdict: DiffVerdict,
}

/// Who holds which role, after group hierarchies are expanded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProjection {
    pub admins: BTreeSet<String>,
    pub maintainers: BTreeSet<String>,
    pub readers: BTreeSet<String>,
    /// Repository → the groups it points at for each role. A repository
    /// silently repointed at another group is a privilege change even when no
    /// user moved.
    pub repo_role_refs: BTreeMap<String, RepoRoleRefs>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRoleRefs {
    pub admins: String,
    pub maintainers: String,
    pub readers: String,
}

/// An approved baseline: the normalized snapshot, its role projection, and the
/// digest that indexes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineArtifact {
    pub digest: String,
    pub normalized_snapshot: String,
    pub roles: RoleProjection,
}

/// Differences that name people or repositories: restricted channel only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFindings {
    /// Admins after expanding group hierarchy and repository references.
    pub admin_closure: BTreeSet<String>,
    /// Admins as the effective-list helper reports them (direct membership of
    /// the literal `admin` group).
    pub effective_admins: BTreeSet<String>,
    /// Members the closure grants that the effective list does not name — the
    /// shape a transitive-group escalation takes.
    pub closure_only_admins: BTreeSet<String>,
    /// Role membership and repository-reference changes against the baseline.
    pub role_differences: Vec<String>,
}

/// Result of an audit run: sanitized for reporting, findings for the restricted
/// channel, and the artifact a bootstrap run produced.
#[derive(Debug, Clone)]
pub struct AuditOutcome {
    pub report: SanitizedReport,
    pub findings: AuditFindings,
    pub artifact: BaselineArtifact,
}

/// Canonical form of a snapshot: object keys sorted, `parents` membership
/// sorted, no insignificant whitespace.
///
/// The digest must describe the ACL, not its formatting. Key order and the
/// order of a `parents` list are both presentation — `parents` is a set of
/// groups — so a reformatted file that grants exactly the same access has to
/// digest identically, or every reformat would read as tampering and the
/// audit would be ignored.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: Map<String, Value> = map
                .iter()
                .map(|(key, value)| {
                    let value = canonicalize(value);
                    let value = if key == "parents" {
                        sort_membership(value)
                    } else {
                        value
                    };
                    (key.clone(), value)
                })
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect();
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// Sort a `parents` list. Only string members are reordered; anything else is
/// left as-is for the builder's validation to reject.
fn sort_membership(value: Value) -> Value {
    let Value::Array(items) = value else {
        return value;
    };
    if !items.iter().all(Value::is_string) {
        return Value::Array(items);
    }
    let mut members: Vec<String> = items
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    members.sort();
    Value::Array(members.into_iter().map(Value::String).collect())
}

fn digest_of(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// Every section's map key must equal the entity's own `euid`.
fn check_key_euid_agreement(value: &Value) -> Result<(), AuditError> {
    for section in ["users", "repos", "user_groups", "merge_requests", "issues"] {
        let Some(obj) = value.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (key, entity) in obj {
            let Some(euid) = entity.get("euid").and_then(Value::as_str) else {
                continue;
            };
            if key != euid {
                return Err(AuditError::KeyEuidMismatch {
                    key: key.clone(),
                    euid: euid.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// Groups reachable from `start` by following `parents` edges, including
/// `start`.
fn reachable_groups(start: &str, parents_of: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start.to_owned());
    while let Some(group) = queue.pop_front() {
        if !seen.insert(group.clone()) {
            continue;
        }
        for parent in parents_of.get(&group).into_iter().flatten() {
            queue.push_back(parent.clone());
        }
    }
    seen
}

/// Expand the ACL into who actually holds each role.
///
/// Membership is transitive: a user in `admin` also holds every group `admin`
/// inherits. The closure therefore has to follow the hierarchy rather than
/// read direct membership — reading only direct membership is exactly how a
/// transitive escalation stays invisible.
fn project_roles(value: &Value) -> RoleProjection {
    let group_parents: BTreeMap<String, Vec<String>> = value
        .get("user_groups")
        .and_then(Value::as_object)
        .map(|groups| {
            groups
                .iter()
                .map(|(key, group)| {
                    let parents = group
                        .get("parents")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    (key.clone(), parents)
                })
                .collect()
        })
        .unwrap_or_default();

    // Which groups each user effectively belongs to.
    let mut user_groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(users) = value.get("users").and_then(Value::as_object) {
        for (key, user) in users {
            let mut groups = BTreeSet::new();
            for parent in user
                .get("parents")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                groups.extend(reachable_groups(parent, &group_parents));
            }
            user_groups.insert(key.clone(), groups);
        }
    }

    let mut projection = RoleProjection::default();
    if let Some(repos) = value.get("repos").and_then(Value::as_object) {
        for (repo_key, repo) in repos {
            let role_ref = |role: &str| {
                repo.get(role)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let refs = RepoRoleRefs {
                admins: role_ref("admins"),
                maintainers: role_ref("maintainers"),
                readers: role_ref("readers"),
            };

            for (group, bucket) in [
                (&refs.admins, &mut projection.admins),
                (&refs.maintainers, &mut projection.maintainers),
                (&refs.readers, &mut projection.readers),
            ] {
                if group.is_empty() {
                    continue;
                }
                for (user, groups) in &user_groups {
                    if groups.contains(group) {
                        bucket.insert(user.clone());
                    }
                }
            }

            projection.repo_role_refs.insert(repo_key.clone(), refs);
        }
    }

    projection
}

/// Admins named by direct membership of the literal `admin` group — what the
/// runtime's admin helper reports.
fn effective_admins(value: &Value) -> BTreeSet<String> {
    const ADMIN_GROUP: &str = "UserGroup::\"admin\"";
    value
        .get("users")
        .and_then(Value::as_object)
        .map(|users| {
            users
                .iter()
                .filter(|(_, user)| {
                    user.get("parents")
                        .and_then(Value::as_array)
                        .map(|parents| {
                            parents
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|p| p == ADMIN_GROUP)
                        })
                        .unwrap_or(false)
                })
                .map(|(key, _)| key.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Validate and normalize a snapshot, returning its canonical text, digest,
/// role projection and findings.
fn analyze(snapshot_json: &str) -> Result<(BaselineArtifact, AuditFindings), AuditError> {
    let value: Value = serde_json::from_str(snapshot_json)?;
    check_key_euid_agreement(&value)?;
    // Reuse UN-14's build-time validation rather than re-implementing it: an
    // audit that accepts snapshots the runtime would reject proves nothing.
    build_from_json(snapshot_json)?;

    let normalized = serde_json::to_string(&canonicalize(&value))?;
    let digest = digest_of(&normalized);
    let roles = project_roles(&value);
    let effective = effective_admins(&value);
    let closure_only: BTreeSet<String> = roles.admins.difference(&effective).cloned().collect();

    let findings = AuditFindings {
        admin_closure: roles.admins.clone(),
        effective_admins: effective,
        closure_only_admins: closure_only,
        role_differences: Vec::new(),
    };

    Ok((
        BaselineArtifact {
            digest,
            normalized_snapshot: normalized,
            roles,
        },
        findings,
    ))
}

/// Produce a candidate baseline from the current ACL.
///
/// The verdict is always `not_compared`. There is nothing to compare against,
/// and a bootstrap run that reported "pass" would let an already-tampered ACL
/// be blessed as the baseline — precisely the failure this audit exists to
/// prevent.
pub fn bootstrap_candidate(snapshot_json: &str) -> Result<AuditOutcome, AuditError> {
    let (artifact, findings) = analyze(snapshot_json)?;
    Ok(AuditOutcome {
        report: SanitizedReport {
            digest: artifact.digest.clone(),
            closure_count: artifact.roles.admins.len(),
            diff_verdict: DiffVerdict::NotCompared,
        },
        findings,
        artifact,
    })
}

/// Compare the current ACL against an approved baseline.
///
/// `expected_digest` comes from the approval ledger — a medium the ACL's writer
/// does not control. Without it, replacing the whole artifact (and recomputing
/// its internal digest) would be undetectable.
pub fn compare(
    snapshot_json: &str,
    baseline: &BaselineArtifact,
    expected_digest: &str,
) -> Result<AuditOutcome, AuditError> {
    // The artifact must be internally consistent…
    let recomputed = digest_of(&baseline.normalized_snapshot);
    if recomputed != baseline.digest {
        return Err(AuditError::BaselineTampered {
            recorded: baseline.digest.clone(),
            actual: recomputed,
        });
    }
    // …and it must be the artifact that was actually approved.
    if baseline.digest != expected_digest {
        return Err(AuditError::BaselineNotApproved {
            expected: expected_digest.to_owned(),
            found: baseline.digest.clone(),
        });
    }

    let (artifact, mut findings) = analyze(snapshot_json)?;
    let verdict = if artifact.digest == baseline.digest {
        DiffVerdict::Match
    } else {
        DiffVerdict::Mismatch
    };
    findings.role_differences = role_differences(&baseline.roles, &artifact.roles);

    Ok(AuditOutcome {
        report: SanitizedReport {
            digest: artifact.digest.clone(),
            closure_count: artifact.roles.admins.len(),
            diff_verdict: verdict,
        },
        findings,
        artifact,
    })
}

/// Role-level differences, in a form an operator can act on. Restricted
/// channel: these name people and repositories.
fn role_differences(baseline: &RoleProjection, current: &RoleProjection) -> Vec<String> {
    let mut differences = Vec::new();

    for (role, before, after) in [
        ("admin", &baseline.admins, &current.admins),
        ("maintainer", &baseline.maintainers, &current.maintainers),
        ("reader", &baseline.readers, &current.readers),
    ] {
        for added in after.difference(before) {
            differences.push(format!("{role}: `{added}` gained the role"));
        }
        for removed in before.difference(after) {
            differences.push(format!("{role}: `{removed}` lost the role"));
        }
    }

    for (repo, before) in &baseline.repo_role_refs {
        match current.repo_role_refs.get(repo) {
            None => differences.push(format!("repo `{repo}` is gone")),
            Some(after) => {
                for (role, before_ref, after_ref) in [
                    ("admins", &before.admins, &after.admins),
                    ("maintainers", &before.maintainers, &after.maintainers),
                    ("readers", &before.readers, &after.readers),
                ] {
                    if before_ref != after_ref {
                        differences.push(format!(
                            "repo `{repo}` {role} now points at `{after_ref}` (was `{before_ref}`)"
                        ));
                    }
                }
            }
        }
    }
    for repo in current.repo_role_refs.keys() {
        if !baseline.repo_role_refs.contains_key(repo) {
            differences.push(format!("repo `{repo}` is new"));
        }
    }

    differences
}
