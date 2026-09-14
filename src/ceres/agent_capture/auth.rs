use sha2::{Digest, Sha256};

use crate::config::{AgentCaptureIngestTokenConfig, token_path_authorizes};

/// Constant-time scan of `[[agent_capture.ingest_tokens]]`.
///
/// Digests are compared so secret length is not leaked by an early return;
/// every configured token is hashed and compared even after a match.
/// `producer_id` for a hit is the token `name`.
pub fn lookup_ingest_token<'a>(
    tokens: &'a [AgentCaptureIngestTokenConfig],
    presented: &str,
) -> Option<&'a AgentCaptureIngestTokenConfig> {
    let presented_digest = sha256_bytes(presented);
    let mut matched = None;
    for token in tokens {
        let stored_digest = sha256_bytes(&token.token);
        if ct_eq_bytes(&presented_digest, &stored_digest) && matched.is_none() {
            matched = Some(token);
        }
    }
    matched
}

/// Whether an ingest token authorizes `repo_path` (component-boundary prefixes
/// via [`token_path_authorizes`]). Omitted or empty `paths` cover the whole
/// repository.
pub fn token_covers_capture_repo(token: &AgentCaptureIngestTokenConfig, repo_path: &str) -> bool {
    match &token.paths {
        None => true,
        Some(paths) if paths.is_empty() => true,
        Some(paths) => paths
            .iter()
            .any(|authorized| token_path_authorizes(authorized, repo_path)),
    }
}

fn sha256_bytes(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentCaptureIngestTokenConfig;

    fn sample_tokens() -> Vec<AgentCaptureIngestTokenConfig> {
        vec![AgentCaptureIngestTokenConfig {
            name: "hook".to_owned(),
            token: "secret-ci".to_owned(),
            paths: Some(vec!["/third-part/mega".to_owned()]),
            tenant_id: None,
        }]
    }

    #[test]
    fn lookup_matches_name() {
        let tokens = sample_tokens();
        let hit = lookup_ingest_token(&tokens, "secret-ci").expect("hit");
        assert_eq!(hit.name, "hook");
    }

    #[test]
    fn lookup_rejects_wrong_secret() {
        let tokens = sample_tokens();
        assert!(lookup_ingest_token(&tokens, "secret-missing").is_none());
        assert!(lookup_ingest_token(&tokens, "secret-c").is_none());
    }

    #[test]
    fn path_prefix_does_not_cover_sibling() {
        let tokens = sample_tokens();
        let hit = lookup_ingest_token(&tokens, "secret-ci").expect("hit");
        assert!(token_covers_capture_repo(hit, "/third-part/mega"));
        assert!(token_covers_capture_repo(hit, "/third-part/mega/src"));
        assert!(!token_covers_capture_repo(hit, "/third-part/megaphone"));
    }
}
