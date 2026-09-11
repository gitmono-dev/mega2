use axum::http::{HeaderMap, header::AUTHORIZATION};
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::{
    ceres::oci::error::OciError,
    config::{GitConfig, PushAuth},
    contract::git_protocol::{lookup_push_token, token_covers_repo},
};

#[derive(Debug, PartialEq, Eq)]
enum Credentials {
    Missing,
    Invalid,
    Token(String),
}

/// Parses OCI's supported static credential formats.
///
/// Bearer credentials contain the token directly. For Basic credentials the
/// password is the token and the username is intentionally ignored.
pub fn token_from_authorization(headers: &HeaderMap) -> Option<String> {
    match credentials(headers) {
        Credentials::Token(token) => Some(token),
        Credentials::Missing | Credentials::Invalid => None,
    }
}

/// Allows a repository read when the anonymous-read or static-token policy
/// permits it.
pub fn authorize_repo_read(
    git: &GitConfig,
    headers: &HeaderMap,
    _repo: &str,
) -> Result<(), OciError> {
    authorize_read(git, headers)
}

/// Applies storage-only write policy to an OCI repository.
pub fn authorize_repo_write(
    git: &GitConfig,
    headers: &HeaderMap,
    repo: &str,
) -> Result<(), OciError> {
    match git.push_auth {
        Some(PushAuth::None) => Ok(()),
        Some(PushAuth::Token) => {
            let Credentials::Token(token) = credentials(headers) else {
                return Err(OciError::Unauthorized);
            };
            let token =
                lookup_push_token(&git.push_tokens, &token).ok_or(OciError::Unauthorized)?;
            let repo_path = format!("/{repo}");
            if token_covers_repo(token, &repo_path) {
                Ok(())
            } else {
                Err(OciError::Denied)
            }
        }
        None => Err(OciError::Unauthorized),
    }
}

/// Applies the same policy as an OCI repository read to the `/v2/` ping.
pub fn registry_ping_allowed(git: &GitConfig, headers: &HeaderMap) -> Result<(), OciError> {
    authorize_read(git, headers)
}

fn authorize_read(git: &GitConfig, headers: &HeaderMap) -> Result<(), OciError> {
    match credentials(headers) {
        Credentials::Missing if git.anonymous_access => Ok(()),
        Credentials::Missing | Credentials::Invalid => Err(OciError::Unauthorized),
        Credentials::Token(token) => lookup_push_token(&git.push_tokens, &token)
            .map(|_| ())
            .ok_or(OciError::Unauthorized),
    }
}

fn credentials(headers: &HeaderMap) -> Credentials {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Credentials::Missing;
    };
    let Ok(value) = value.to_str() else {
        return Credentials::Invalid;
    };

    if let Some(token) = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
    {
        let token = token.trim();
        return if !token.is_empty() {
            Credentials::Token(token.to_owned())
        } else {
            Credentials::Invalid
        };
    }

    let Some(encoded) = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))
    else {
        return Credentials::Invalid;
    };
    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return Credentials::Invalid;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return Credentials::Invalid;
    };
    let Some((_, password)) = decoded.split_once(':') else {
        return Credentials::Invalid;
    };
    if !password.is_empty() {
        Credentials::Token(password.to_owned())
    } else {
        Credentials::Invalid
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
    use base64::{Engine, engine::general_purpose::STANDARD};

    use super::{
        authorize_repo_read, authorize_repo_write, registry_ping_allowed, token_from_authorization,
    };
    use crate::{
        ceres::oci::error::OciError,
        config::{GitConfig, PushAuth, PushTokenConfig},
    };

    fn token_git(anonymous_access: bool) -> GitConfig {
        GitConfig {
            anonymous_access,
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![PushTokenConfig {
                name: "ci".to_owned(),
                token: "secret".to_owned(),
                paths: Some(vec!["/team".to_owned()]),
            }],
            ssh_receive_pack: Some(false),
        }
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("header"),
        );
        headers
    }

    #[test]
    fn auth_matrix() {
        let no_headers = HeaderMap::new();
        let git = token_git(true);
        assert!(authorize_repo_read(&git, &no_headers, "team/image").is_ok());
        assert!(registry_ping_allowed(&git, &no_headers).is_ok());
        assert_eq!(
            authorize_repo_write(&git, &no_headers, "team/image"),
            Err(OciError::Unauthorized)
        );
        assert_eq!(
            authorize_repo_write(&git, &bearer("secret"), "other/image"),
            Err(OciError::Denied)
        );
        assert!(authorize_repo_write(&git, &bearer("secret"), "team/image").is_ok());

        let closed = token_git(false);
        assert_eq!(
            authorize_repo_read(&closed, &no_headers, "team/image"),
            Err(OciError::Unauthorized)
        );
        assert!(authorize_repo_read(&closed, &bearer("secret"), "team/image").is_ok());
        assert_eq!(
            registry_ping_allowed(&git, &bearer("wrong")),
            Err(OciError::Unauthorized)
        );

        let mut basic = HeaderMap::new();
        basic.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", STANDARD.encode("any-user:secret")))
                .expect("header"),
        );
        assert_eq!(token_from_authorization(&basic).as_deref(), Some("secret"));

        let no_auth = GitConfig {
            push_auth: Some(PushAuth::None),
            ..GitConfig::default()
        };
        assert!(authorize_repo_write(&no_auth, &no_headers, "any/image").is_ok());
    }
}
