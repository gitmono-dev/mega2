//! Build-time validated, immutable authorization snapshot from `/.mega_cedar.json`
//! (UN-14). Single source for building user/group/repo entities; semantic errors
//! (invalid UID / attribute / parent relationship / reserved anonymous principal)
//! are rejected at build time. The Cedar `Entities` are cached so the evaluation
//! path does not rebuild entities per request.

use std::collections::HashSet;

use cedar_policy::{Entities, Schema};
use serde_json::Value;
use thiserror::Error;

use crate::contract::policy::{entitystore::EntityStore, util::SaturnEUid};

/// Reserved anonymous fallback principal (ADR-UN-06 ⑤). ACL input must not
/// occupy it, otherwise an anonymous request could inherit its privileges.
pub const ANONYMOUS_RESERVED: &str = "User::\"__anonymous__\"";

#[derive(Debug, Error)]
pub enum BuilderError {
    #[error("failed to parse /.mega_cedar.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid entity UID: {0}")]
    InvalidUid(String),
    #[error("invalid entity attribute: {0}")]
    InvalidAttribute(String),
    #[error("invalid parent relationship: {0}")]
    InvalidParent(String),
    #[error("reserved anonymous principal {ANONYMOUS_RESERVED} must not be occupied by ACL input")]
    ReservedAnonymous,
    #[error("failed to build Cedar entities: {0}")]
    Entities(String),
}

/// A build-time validated, immutable snapshot of the authorization entities with
/// the Cedar `Entities` cached (no per-request rebuild).
#[derive(Debug)]
pub struct EntitySnapshot {
    store: EntityStore,
    entities: Entities,
}

impl EntitySnapshot {
    pub fn store(&self) -> &EntityStore {
        &self.store
    }

    pub fn entities(&self) -> &Entities {
        &self.entities
    }
}

/// Build a validated immutable snapshot from `/.mega_cedar.json` content.
pub fn build_from_json(json: &str) -> Result<EntitySnapshot, BuilderError> {
    let value: Value = serde_json::from_str(json)?;
    validate_semantics(&value)?;
    let store: EntityStore = serde_json::from_str(json)?;
    let (schema, _) = Schema::from_cedarschema_str(include_str!("mega.cedarschema"))
        .map_err(|e| BuilderError::Entities(e.to_string()))?;
    let entities = store
        .as_entities(&schema)
        .map_err(|e| BuilderError::Entities(e.to_string()))?;
    Ok(EntitySnapshot { store, entities })
}

/// Validate build-time semantics: UID format, required attributes, parent
/// relationships reference existing groups, and the reserved anonymous
/// principal is not occupied by ACL input.
fn validate_semantics(value: &Value) -> Result<(), BuilderError> {
    let mut groups: HashSet<String> = HashSet::new();
    if let Some(user_groups) = value.get("user_groups").and_then(Value::as_object) {
        groups.extend(user_groups.keys().cloned());
    }

    for section in ["users", "repos", "user_groups", "merge_requests", "issues"] {
        let Some(obj) = value.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (key, entity) in obj {
            key.parse::<SaturnEUid>()
                .map_err(|e| BuilderError::InvalidUid(format!("{key}: {e}")))?;
            if key == ANONYMOUS_RESERVED {
                return Err(BuilderError::ReservedAnonymous);
            }
            if let Some(euid) = entity.get("euid").and_then(Value::as_str) {
                euid.parse::<SaturnEUid>()
                    .map_err(|e| BuilderError::InvalidUid(format!("{euid}: {e}")))?;
                if euid == ANONYMOUS_RESERVED {
                    return Err(BuilderError::ReservedAnonymous);
                }
            }
            if let Some(parents) = entity.get("parents").and_then(Value::as_array) {
                for parent in parents {
                    let Some(parent_str) = parent.as_str() else {
                        return Err(BuilderError::InvalidParent(
                            "parent is not a string".to_string(),
                        ));
                    };
                    parent_str
                        .parse::<SaturnEUid>()
                        .map_err(|e| BuilderError::InvalidParent(format!("{parent_str}: {e}")))?;
                    if !groups.contains(parent_str) {
                        return Err(BuilderError::InvalidParent(format!(
                            "{parent_str} references unknown group"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::policy::entitystore::generate_entity;

    fn valid_json() -> String {
        generate_entity(&["admin".to_string()], "repo").expect("generate")
    }

    #[test]
    fn builds_valid_snapshot_and_caches_entities() {
        let snapshot = build_from_json(&valid_json()).expect("build");
        assert!(
            !snapshot.entities().is_empty(),
            "cached entities should be non-empty"
        );
        assert!(!snapshot.store().is_empty());
    }

    #[test]
    fn rejects_invalid_uid() {
        let json = r#"{"users":{"not-a-uid":{"euid":"not-a-uid","parents":[]}},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#;
        let err = build_from_json(json).expect_err("invalid UID should fail");
        assert!(matches!(err, BuilderError::InvalidUid(_)));
    }

    #[test]
    fn rejects_invalid_attribute() {
        // repo missing required is_private/admins/maintainers/readers
        let json = r#"{"users":{},"repos":{"Repository::\"repo\"":{"euid":"Repository::\"repo\"","parents":[]}},"user_groups":{},"merge_requests":{},"issues":{}}"#;
        let err = build_from_json(json).expect_err("missing repo attribute should fail");
        assert!(matches!(err, BuilderError::Json(_)));
    }

    #[test]
    fn rejects_invalid_parent_relationship() {
        let json = r#"{"users":{"User::\"alice\"":{"euid":"User::\"alice\"","parents":["UserGroup::\"nope\""]}},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#;
        let err = build_from_json(json).expect_err("unknown parent group should fail");
        assert!(matches!(err, BuilderError::InvalidParent(_)));
    }

    #[test]
    fn rejects_reserved_anonymous_principal() {
        let json = r#"{"users":{"User::\"__anonymous__\"":{"euid":"User::\"__anonymous__\"","parents":[]}},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#;
        let err = build_from_json(json).expect_err("reserved anonymous should fail");
        assert!(matches!(err, BuilderError::ReservedAnonymous));
    }

    #[test]
    fn scale_10k_entities_within_budget() {
        // 10k users within the 1 MiB JSON budget; first build < 1s in release.
        let mut users = serde_json::Map::new();
        for i in 0..10_000 {
            users.insert(
                format!("User::\"u{i}\""),
                serde_json::json!({"euid": format!("User::\"u{i}\""), "parents": []}),
            );
        }
        let json = serde_json::json!({
            "users": users,
            "repos": {},
            "user_groups": {},
            "merge_requests": {},
            "issues": {},
        })
        .to_string();
        assert!(
            json.len() <= 1_048_576,
            "JSON exceeds 1 MiB: {}",
            json.len()
        );
        let start = std::time::Instant::now();
        let snapshot = build_from_json(&json).expect("build 10k entities");
        let elapsed = start.elapsed();
        if !cfg!(debug_assertions) {
            assert!(
                elapsed.as_secs_f64() < 1.0,
                "first build of 10k entities took {elapsed:?}"
            );
        }
        assert!(!snapshot.entities().is_empty());
    }
}
