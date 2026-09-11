//! Trunk / storage-only product API write gate (plan-20260904 AW-02).
//!
//! Reuses [`lookup_push_token`] / [`token_covers_repo`] with the same Basic/Bearer
//! credential shapes as LFS and Git smart HTTP. Does **not** consult UserStorage.

use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use base64::Engine;

use crate::{
    api::oauth::bearer_token_from_authorization_value,
    common::errors::ApiError,
    config::{GitConfig, PushAuth},
    contract::git_protocol::{lookup_push_token, token_covers_repo},
};

/// Stable requester identity when `push_auth=none` admits an unauthenticated write.
pub const ANONYMOUS_API_WRITE_REQUESTER: &str = "anonymous";

/// Authorize a trunk product API write against `git.push_auth`.
///
/// On success returns the requester name (token `name`, or
/// [`ANONYMOUS_API_WRITE_REQUESTER`] under `push_auth=none`).
///
/// - `push_auth=token` + missing/invalid credential → **401**
/// - valid token but path not covered → **403**
/// - absent `push_auth` (should not boot on trunk) → **401** fail-closed
pub fn authorize_trunk_api_write(
    git: &GitConfig,
    headers: &HeaderMap,
    path: &str,
) -> Result<String, ApiError> {
    let path = if path.is_empty() { "/" } else { path };
    match git.push_auth {
        Some(PushAuth::None) => Ok(ANONYMOUS_API_WRITE_REQUESTER.to_owned()),
        Some(PushAuth::Token) => {
            let presented = token_from_headers(headers).ok_or_else(api_write_auth_challenge)?;
            let token = lookup_push_token(&git.push_tokens, &presented)
                .ok_or_else(api_write_auth_challenge)?;
            if token_covers_repo(token, path) {
                Ok(token.name.clone())
            } else {
                Err(ApiError::forbidden(anyhow::anyhow!(
                    "token is not authorized for path {path}"
                )))
            }
        }
        None => Err(api_write_auth_challenge()),
    }
}

fn api_write_auth_challenge() -> ApiError {
    ApiError::with_status(
        StatusCode::UNAUTHORIZED,
        anyhow::anyhow!("authentication required"),
    )
}

/// Basic (password field) or Bearer — same shapes as LFS / Git smart HTTP.
fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    if let Some(bearer) = bearer_token_from_authorization_value(value) {
        return Some(bearer.to_owned());
    }
    let stripped = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stripped.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    decoded.split(':').nth(1).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};

    use super::{authorize_trunk_api_write, token_from_headers};
    use crate::config::{GitConfig, PushAuth, PushTokenConfig};

    fn token_cfg(name: &str, secret: &str, paths: Option<Vec<String>>) -> PushTokenConfig {
        PushTokenConfig {
            name: name.to_owned(),
            token: secret.to_owned(),
            paths,
        }
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    fn basic_headers(user: &str, password: &str) -> HeaderMap {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {raw}")).unwrap(),
        );
        h
    }

    fn status_of(err: crate::common::errors::ApiError) -> StatusCode {
        // ApiError does not expose status; round-trip via IntoResponse.
        use axum::response::IntoResponse;
        err.into_response().status()
    }

    #[test]
    fn api_write_auth_token_ok() {
        let git = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_cfg(
                "agent-ci",
                "secret-ok",
                Some(vec!["/project".to_owned()]),
            )],
            ..GitConfig::default()
        };
        let name = authorize_trunk_api_write(&git, &bearer_headers("secret-ok"), "/project/foo")
            .expect("covered path");
        assert_eq!(name, "agent-ci");

        let name = authorize_trunk_api_write(&git, &basic_headers("any", "secret-ok"), "/project")
            .expect("basic password field");
        assert_eq!(name, "agent-ci");

        let git_none = GitConfig {
            push_auth: Some(PushAuth::None),
            ..GitConfig::default()
        };
        let name = authorize_trunk_api_write(&git_none, &HeaderMap::new(), "/anywhere")
            .expect("none admits anonymous");
        assert_eq!(name, super::ANONYMOUS_API_WRITE_REQUESTER);
    }

    #[test]
    fn api_write_auth_token_reject() {
        let git = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_cfg(
                "agent-ci",
                "secret-ok",
                Some(vec!["/project".to_owned()]),
            )],
            ..GitConfig::default()
        };

        let err = authorize_trunk_api_write(&git, &HeaderMap::new(), "/project")
            .expect_err("missing creds");
        assert_eq!(status_of(err), StatusCode::UNAUTHORIZED);

        let err = authorize_trunk_api_write(&git, &bearer_headers("wrong"), "/project")
            .expect_err("bad token");
        assert_eq!(status_of(err), StatusCode::UNAUTHORIZED);

        let err = authorize_trunk_api_write(&git, &bearer_headers("secret-ok"), "/other")
            .expect_err("path not covered");
        assert_eq!(status_of(err), StatusCode::FORBIDDEN);

        // Component-boundary: /project must not authorize /projectX
        let err = authorize_trunk_api_write(&git, &bearer_headers("secret-ok"), "/projectX")
            .expect_err("prefix without boundary");
        assert_eq!(status_of(err), StatusCode::FORBIDDEN);

        assert!(token_from_headers(&bearer_headers("x")).as_deref() == Some("x"));
    }
}
