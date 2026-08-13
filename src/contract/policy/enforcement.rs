//! Single-source three-state authorization enforcement helper (ADR-UN-01).
//!
//! `off` = do not build or consume authorization data (no behavior change);
//! `shadow` = build the store, evaluate, record would-deny logs, but do not
//! change allow decisions; `enforce` = build the store, evaluate, and deny.

use std::fmt::{self, Display};

/// Three-state enforcement mode (ADR-UN-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Off,
    Shadow,
    Enforce,
}

impl Enforcement {
    /// Parse a config `[cedar].enforcement` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    /// Whether this mode builds and consumes the authorization store.
    pub fn builds(self) -> bool {
        matches!(self, Self::Shadow | Self::Enforce)
    }

    /// Whether this mode records would-deny logs (shadow) without changing allow.
    pub fn records_would_deny(self) -> bool {
        matches!(self, Self::Shadow)
    }

    /// Whether this mode enforces denials.
    pub fn enforces(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

impl Display for Enforcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Outcome of an enforcement decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementDecision {
    Allow,
    Deny,
}

/// Decide whether a request is allowed under the given enforcement mode.
///
/// - `off`: always allow (no build, no consume).
/// - `shadow`: allow, but record would-deny (caller logs it).
/// - `enforce`: deny when the evaluation would deny, or when the store is empty
///   (fail-closed, ADR-UN-01).
pub fn decide(
    enforcement: Enforcement,
    would_deny: bool,
    store_empty: bool,
) -> EnforcementDecision {
    match enforcement {
        Enforcement::Off | Enforcement::Shadow => EnforcementDecision::Allow,
        Enforcement::Enforce => {
            if would_deny || store_empty {
                EnforcementDecision::Deny
            } else {
                EnforcementDecision::Allow
            }
        }
    }
}

/// `enforce` + empty store => deny (fail-closed). Convenience wrapper.
pub fn enforce_empty_store_denies() -> EnforcementDecision {
    decide(Enforcement::Enforce, false, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_valid_modes() {
        assert_eq!(Enforcement::parse("off"), Some(Enforcement::Off));
        assert_eq!(Enforcement::parse("shadow"), Some(Enforcement::Shadow));
        assert_eq!(Enforcement::parse("enforce"), Some(Enforcement::Enforce));
    }

    #[test]
    fn parse_rejects_invalid_mode() {
        assert_eq!(Enforcement::parse("on"), None);
        assert_eq!(Enforcement::parse(""), None);
    }

    #[test]
    fn off_never_builds_or_denies() {
        assert!(!Enforcement::Off.builds());
        assert!(!Enforcement::Off.records_would_deny());
        assert!(!Enforcement::Off.enforces());
        assert_eq!(
            decide(Enforcement::Off, true, true),
            EnforcementDecision::Allow
        );
    }

    #[test]
    fn shadow_allows_but_records() {
        assert!(Enforcement::Shadow.builds());
        assert!(Enforcement::Shadow.records_would_deny());
        assert!(!Enforcement::Shadow.enforces());
        assert_eq!(
            decide(Enforcement::Shadow, true, false),
            EnforcementDecision::Allow
        );
    }

    #[test]
    fn enforce_denies_would_deny() {
        assert_eq!(
            decide(Enforcement::Enforce, true, false),
            EnforcementDecision::Deny
        );
        assert_eq!(
            decide(Enforcement::Enforce, false, false),
            EnforcementDecision::Allow
        );
    }

    #[test]
    fn enforce_empty_store_denies_returns_deny() {
        assert_eq!(
            decide(Enforcement::Enforce, false, true),
            EnforcementDecision::Deny
        );
        assert_eq!(enforce_empty_store_denies(), EnforcementDecision::Deny);
    }
}
