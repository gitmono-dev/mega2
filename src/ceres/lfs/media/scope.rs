//! Media object scope: server-side actor + canonical repository.
//!
//! Request-body actor/repository/`created_by` never enter these constructors.

use crate::{
    ceres::lfs::digest::LfsDigest,
    orbit_api::object_storage::{ObjectKey, ObjectNamespace},
};

/// Algorithm-scoped Media object namespace (C-08 / MF-02).
/// Old `v1/` objects are retained unread and undeleted.
const MEDIA_KEY_PREFIX: &str = "fastcdc-v2020-32k";

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("invalid media actor")]
    Actor,
    #[error("invalid media repository")]
    Repository,
    #[error("invalid media object identity")]
    ObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaObjectKind {
    Pending,
    Chunk,
    Manifest,
    Finalized,
    /// Immutable page blob under `(manifest_id, page_no)`.
    Page,
}

impl MediaObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Chunk => "chunk",
            Self::Manifest => "manifest",
            Self::Finalized => "finalized",
            Self::Page => "page",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaScope {
    actor: String,
    repository: String,
}

impl MediaScope {
    /// Build a scope from **server-resolved** identity only.
    pub fn from_server(actor: &str, repository: &str) -> Result<Self, ScopeError> {
        Ok(Self {
            actor: parse_actor(actor)?,
            repository: canonicalize_repository(repository)?,
        })
    }

    /// The canonical repository this scope was constructed with (WH-06:
    /// read-only; never reversed from a digest).
    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn digest(&self) -> String {
        let mut payload = Vec::with_capacity(self.actor.len() + self.repository.len() + 1);
        payload.extend_from_slice(self.actor.as_bytes());
        payload.push(0);
        payload.extend_from_slice(self.repository.as_bytes());
        LfsDigest::sha256_of(&payload).hex().to_owned()
    }

    pub fn object_key(
        &self,
        kind: MediaObjectKind,
        object_id: &str,
    ) -> Result<ObjectKey, ScopeError> {
        if !is_key_token(object_id) {
            return Err(ScopeError::ObjectId);
        }
        let key = format!(
            "{MEDIA_KEY_PREFIX}/{}/{}/{}",
            self.digest(),
            kind.as_str(),
            object_id
        );
        Ok(ObjectKey {
            namespace: ObjectNamespace::Media,
            key,
        })
    }
}

pub fn parse_actor(actor: &str) -> Result<String, ScopeError> {
    if actor.is_empty() || !actor.is_ascii() || actor.bytes().any(|b| b.is_ascii_control()) {
        return Err(ScopeError::Actor);
    }
    Ok(actor.to_string())
}

pub fn canonicalize_repository(repository: &str) -> Result<String, ScopeError> {
    if !repository.starts_with('/') || repository.contains('\\') {
        return Err(ScopeError::Repository);
    }
    if repository
        .bytes()
        .any(|b| b == b'%' || b == b'?' || b.is_ascii_control() || !b.is_ascii())
    {
        return Err(ScopeError::Repository);
    }
    let mut parts = Vec::new();
    for (i, seg) in repository.split('/').enumerate() {
        if seg.is_empty() {
            if i == 0 {
                continue;
            }
            return Err(ScopeError::Repository);
        }
        if seg == "." || seg == ".." {
            return Err(ScopeError::Repository);
        }
        parts.push(seg);
    }
    if parts.is_empty() {
        return Err(ScopeError::Repository);
    }
    Ok(format!("/{}", parts.join("/")))
}

fn is_key_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Map storage errors for Media responses: never include scope, key, or actor.
pub fn redact_storage_error(_err: &impl std::fmt::Display) -> String {
    "media object store error".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_actor_and_unsafe_repository() {
        assert!(MediaScope::from_server("", "/org/repo").is_err());
        assert!(MediaScope::from_server("user-1", "org/repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org\\repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org%20repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org?repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org//repo").is_err());
        assert!(MediaScope::from_server("user-1", "//org/repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org/./repo").is_err());
        assert!(MediaScope::from_server("user-1", "/org/../repo").is_err());
        assert!(MediaScope::from_server("user-1", "/").is_err());
    }

    #[test]
    fn scope_digest_includes_actor_and_repository() {
        let a = MediaScope::from_server("alice", "/org/repo").unwrap();
        let b = MediaScope::from_server("bob", "/org/repo").unwrap();
        let c = MediaScope::from_server("alice", "/org/other").unwrap();
        assert_ne!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        let chunk = "b".repeat(64);
        let ka = a.object_key(MediaObjectKind::Chunk, &chunk).unwrap();
        let kb = b.object_key(MediaObjectKind::Chunk, &chunk).unwrap();
        assert_ne!(ka.key, kb.key);
        assert_eq!(ka.namespace, ObjectNamespace::Media);
        assert!(ka.key.starts_with("fastcdc-v2020-32k/"));
        assert!(
            ka.default_sharding()
                .starts_with("media/fastcdc-v2020-32k/")
        );
        assert!(!ka.default_sharding().contains("alice"));
        // Legacy v1 keys must not be produced by the new prefix.
        assert!(!ka.key.starts_with("v1/"));
    }

    #[test]
    fn kinds_do_not_reuse_keys_across_scope() {
        let scope = MediaScope::from_server("user-1", "/acme/app").unwrap();
        let id = "c".repeat(64);
        let pending = scope.object_key(MediaObjectKind::Pending, &id).unwrap();
        let chunk = scope.object_key(MediaObjectKind::Chunk, &id).unwrap();
        let manifest = scope.object_key(MediaObjectKind::Manifest, &id).unwrap();
        let finalized = scope.object_key(MediaObjectKind::Finalized, &id).unwrap();
        let keys = [
            pending.key.clone(),
            chunk.key.clone(),
            manifest.key.clone(),
            finalized.key.clone(),
        ];
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), 4);
        for key in [pending, chunk, manifest, finalized] {
            assert!(key.validate().is_ok());
        }
    }

    #[test]
    fn request_body_created_by_cannot_enter_scope() {
        let scope = MediaScope::from_server("server-actor", "/acme/app").unwrap();
        let spoofed = MediaScope::from_server("body-actor", "/acme/app").unwrap();
        assert_ne!(scope.digest(), spoofed.digest());
        assert!(
            !redact_storage_error(&format!(
                "failed key={} actor=server-actor digest={}",
                scope.object_key(MediaObjectKind::Chunk, "abc").unwrap().key,
                scope.digest()
            ))
            .contains("server-actor")
        );
    }

    #[test]
    fn namespace_path_is_media() {
        assert_eq!(ObjectNamespace::Media.to_string(), "media");
        assert_eq!(ObjectNamespace::Attachment.to_string(), "attachment");
    }
}
