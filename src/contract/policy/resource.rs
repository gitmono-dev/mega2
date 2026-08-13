//! Single-source resource normalization for the single-monorepo model
//! (ADR-UN-05). Every request path in the monorepo normalizes to the root
//! repository entity, whose ACL governs all paths. A missing root repository
//! entity is treated as fail-closed: `enforce` denies, `shadow` records.

use cedar_policy::ParseErrors;

use crate::contract::policy::{
    enforcement::{Enforcement, EnforcementDecision, decide},
    entitystore::EntityStore,
    util::SaturnEUid,
};

/// Root repository entity identifier (ADR-UN-05). Every request path resolves
/// to this single repository, whose ACL governs all paths.
pub const ROOT_REPOSITORY: &str = "Repository::\"/\"";

/// Normalize a request path to the root repository entity identifier.
///
/// Pure O(1) mapping (ADR-UN-05): any legal request path (e.g. `/project`)
/// resolves to the single monorepo root repository `Repository::"/"`.
pub fn normalize_resource(_path: &str) -> Result<SaturnEUid, Box<ParseErrors>> {
    ROOT_REPOSITORY.parse().map_err(Box::new)
}

/// Whether the store contains the root repository entity.
pub fn root_repository_present(store: &EntityStore) -> bool {
    normalize_resource("/")
        .map(|root| store.contains_repository(&root))
        .unwrap_or(false)
}

/// Result of resolving a request path against the store (ADR-UN-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceResolution {
    /// Root repository entity present; the normalized resource identifier.
    Resolved(SaturnEUid),
    /// Root repository entity absent from the store (fail-closed).
    Missing,
}

impl ResourceResolution {
    /// Whether the resource is resolvable (root repository present).
    pub fn is_resolvable(&self) -> bool {
        matches!(self, Self::Resolved(_))
    }

    /// Whether this resolution would deny (missing root repository).
    pub fn would_deny(&self) -> bool {
        !self.is_resolvable()
    }

    /// The normalized resource identifier, if resolvable.
    pub fn resource(&self) -> Option<&SaturnEUid> {
        match self {
            Self::Resolved(euid) => Some(euid),
            Self::Missing => None,
        }
    }
}

/// Resolve a request path to the root repository, checking presence in the store.
pub fn resolve_resource(path: &str, store: &EntityStore) -> ResourceResolution {
    match normalize_resource(path) {
        Ok(root) if store.contains_repository(&root) => ResourceResolution::Resolved(root),
        _ => ResourceResolution::Missing,
    }
}

/// Outcome of a resource enforcement decision, including whether a would-deny
/// record should be produced (ADR-UN-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceDecision {
    /// The enforcement decision for the request.
    pub decision: EnforcementDecision,
    /// Whether the caller should record a would-deny log entry (shadow path).
    pub record_would_deny: bool,
}

/// Decide enforcement for a resource resolution (ADR-UN-05 fail-closed).
///
/// A missing root repository entity is treated as would-deny: `enforce` denies,
/// `shadow` allows but records, `off` allows.
pub fn decide_resource(
    enforcement: Enforcement,
    resolution: &ResourceResolution,
) -> ResourceDecision {
    let would_deny = resolution.would_deny();
    let decision = decide(enforcement, would_deny, false);
    let record_would_deny = enforcement.records_would_deny() && would_deny;
    ResourceDecision {
        decision,
        record_would_deny,
    }
}

#[cfg(test)]
mod tests {
    use cedar_policy::Context;

    use super::*;
    use crate::contract::policy::{
        context::CedarContext,
        entitystore::{EntityStore, generate_entity},
    };

    /// Default init product: unmodified `generate_entity` output for the root
    /// repository (matches `src/jupiter/utils/converter.rs` init path).
    fn default_store() -> EntityStore {
        let json = generate_entity(&["admin".to_string()], "/").expect("generate");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn normalizes_request_path_to_root_repository() {
        let store = default_store();
        let resource = normalize_resource("/project").expect("normalize");
        assert_eq!(resource.to_string(), ROOT_REPOSITORY);
        assert!(root_repository_present(&store));
        assert!(matches!(
            resolve_resource("/project", &store),
            ResourceResolution::Resolved(_)
        ));
    }

    #[test]
    fn root_repository_attributes_complete() {
        let store = default_store();
        let value = serde_json::to_value(&store).expect("serialize");
        let repo = &value["repos"][ROOT_REPOSITORY];
        assert_eq!(repo["euid"], serde_json::json!(ROOT_REPOSITORY));
        assert_eq!(repo["is_private"], serde_json::json!(true));
        assert_eq!(repo["admins"], serde_json::json!("UserGroup::\"admin\""));
        assert_eq!(
            repo["maintainers"],
            serde_json::json!("UserGroup::\"matainer\"")
        );
        assert_eq!(repo["readers"], serde_json::json!("UserGroup::\"reader\""));
        assert_eq!(repo["parents"], serde_json::json!([]));
    }

    #[test]
    fn init_admin_allowed_on_root_repository() {
        let store = default_store();
        let context = CedarContext::new(store).expect("context");
        let admin: SaturnEUid = r#"User::"admin""#.parse().expect("admin");
        let resource = normalize_resource("/project").expect("normalize");
        assert!(
            context
                .is_authorized(
                    &admin,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_ok()
        );
    }

    #[test]
    fn no_membership_principal_denied_on_private_root() {
        let store = default_store();
        let context = CedarContext::new(store).expect("context");
        let nobody: SaturnEUid = r#"User::"nobody""#.parse().expect("nobody");
        let resource = normalize_resource("/project").expect("normalize");
        assert!(
            context
                .is_authorized(
                    &nobody,
                    r#"Action::"viewRepo""#.parse::<SaturnEUid>().unwrap(),
                    &resource,
                    Context::empty()
                )
                .is_err()
        );
    }

    #[test]
    fn missing_root_repository_is_fail_closed() {
        let store = EntityStore::new();
        assert!(!root_repository_present(&store));
        let resolution = resolve_resource("/project", &store);
        assert!(matches!(resolution, ResourceResolution::Missing));
        assert!(resolution.would_deny());

        let enforce = decide_resource(Enforcement::Enforce, &resolution);
        assert_eq!(enforce.decision, EnforcementDecision::Deny);
        assert!(!enforce.record_would_deny);

        let shadow = decide_resource(Enforcement::Shadow, &resolution);
        assert_eq!(shadow.decision, EnforcementDecision::Allow);
        assert!(shadow.record_would_deny, "shadow must record would-deny");

        let off = decide_resource(Enforcement::Off, &resolution);
        assert_eq!(off.decision, EnforcementDecision::Allow);
        assert!(!off.record_would_deny);
    }
}
