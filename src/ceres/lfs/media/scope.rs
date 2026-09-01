//! Server-owned FastCDC media scope and private object-key construction.

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::orbit_api::object_storage::{ObjectKey, ObjectNamespace};

const MEDIA_V1_PREFIX: &str = "media-v1";

/// A private media scope derived from the authenticated actor and canonical
/// repository path. The digest is intentionally not exposed to HTTP callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaScope {
    digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MediaScopeError {
    #[error("a canonical repository and authenticated actor are required")]
    InvalidActorOrRepository,
    #[error("a media object ID must be lowercase SHA-256 hex")]
    InvalidObjectId,
    #[error("failed to construct the media scope")]
    Canonicalization,
}

impl MediaScope {
    /// Builds a scope from the server-resolved `AccessTokenUser.0.username`.
    ///
    /// The actor must be resolved from the validated Mono access token before
    /// this call; request JSON (including `created_by`) must never supply it.
    pub fn from_access_token_username(
        username: &str,
        repository: &str,
    ) -> Result<Self, MediaScopeError> {
        if username.is_empty() || !is_canonical_repository(repository) {
            return Err(MediaScopeError::InvalidActorOrRepository);
        }

        let input = serde_json::to_vec(&(username, repository))
            .map_err(|_| MediaScopeError::Canonicalization)?;
        Ok(Self {
            digest: hex::encode(Sha256::digest(input)),
        })
    }

    pub fn pending_manifest_key(&self, manifest_id: &str) -> Result<ObjectKey, MediaScopeError> {
        self.key("pending", manifest_id)
    }

    pub fn chunk_key(&self, chunk_hash: &str) -> Result<ObjectKey, MediaScopeError> {
        self.key("chunks", chunk_hash)
    }

    pub fn finalized_manifest_key(&self, media_oid: &str) -> Result<ObjectKey, MediaScopeError> {
        self.key("finalized", media_oid)
    }

    fn key(&self, kind: &str, object_id: &str) -> Result<ObjectKey, MediaScopeError> {
        if !is_lowercase_sha256(object_id) {
            return Err(MediaScopeError::InvalidObjectId);
        }

        Ok(ObjectKey {
            namespace: ObjectNamespace::Media,
            key: format!("{MEDIA_V1_PREFIX}/{}/{kind}/{object_id}", self.digest),
        })
    }
}

fn is_canonical_repository(repository: &str) -> bool {
    !repository.is_empty()
        && repository.starts_with('/')
        && !repository.contains('\\')
        && !repository.contains('%')
        && !repository.contains('?')
        && !repository
            .split('/')
            .skip(1)
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE_DEMO_SCOPE: &str =
        "55300d34e19445a7db58b03ba77390116fa49ba62dbe3544f7241962aad5c3f8";

    #[test]
    fn scope_isolation_and_key_layout_match_the_pinned_contract() {
        let manifest_id = "a".repeat(64);
        let chunk_hash = "b".repeat(64);
        let scope = MediaScope::from_access_token_username("alice", "/project/demo.git").unwrap();
        let same_scope =
            MediaScope::from_access_token_username("alice", "/project/demo.git").unwrap();
        let other_actor =
            MediaScope::from_access_token_username("bob", "/project/demo.git").unwrap();
        let other_repository =
            MediaScope::from_access_token_username("alice", "/project/other.git").unwrap();

        let pending = scope.pending_manifest_key(&manifest_id).unwrap();
        assert_eq!(
            pending,
            ObjectKey {
                namespace: ObjectNamespace::Media,
                key: format!("media-v1/{ALICE_DEMO_SCOPE}/pending/{manifest_id}"),
            }
        );
        assert_eq!(
            pending.default_sharding(),
            format!("media/me/di/a-/v1/{ALICE_DEMO_SCOPE}/pending/{manifest_id}")
        );
        assert!(pending.validate().is_ok());
        assert_eq!(
            scope.chunk_key(&chunk_hash).unwrap().key,
            format!("media-v1/{ALICE_DEMO_SCOPE}/chunks/{chunk_hash}")
        );
        assert_eq!(
            scope.finalized_manifest_key(&manifest_id).unwrap().key,
            format!("media-v1/{ALICE_DEMO_SCOPE}/finalized/{manifest_id}")
        );
        assert_eq!(
            pending,
            same_scope.pending_manifest_key(&manifest_id).unwrap()
        );
        assert_ne!(
            pending,
            other_actor.pending_manifest_key(&manifest_id).unwrap()
        );
        assert_ne!(
            pending,
            other_repository.pending_manifest_key(&manifest_id).unwrap()
        );
    }

    #[test]
    fn rejects_noncanonical_scope_inputs_and_unsafe_object_ids() {
        for repository in [
            "",
            "relative",
            "/",
            "/../secret",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/%2fsecret",
            "/a\\b",
            "/a?query",
        ] {
            let error = MediaScope::from_access_token_username("alice", repository).unwrap_err();
            assert_eq!(error, MediaScopeError::InvalidActorOrRepository);
            if !repository.is_empty() {
                assert!(!error.to_string().contains(repository));
            }
        }
        assert_eq!(
            MediaScope::from_access_token_username("", "/project/demo.git"),
            Err(MediaScopeError::InvalidActorOrRepository)
        );

        let scope = MediaScope::from_access_token_username("alice", "/project/demo.git").unwrap();
        let error = scope.chunk_key("not-a-sha256").unwrap_err();
        assert_eq!(error, MediaScopeError::InvalidObjectId);
        assert!(!error.to_string().contains("not-a-sha256"));
        assert_eq!(
            scope.chunk_key(&"A".repeat(64)),
            Err(MediaScopeError::InvalidObjectId)
        );
    }
}
