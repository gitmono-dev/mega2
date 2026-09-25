//! Trunk / storage-only product API write gate (plan-20260904 AW-02).
//!
//! Reuses [`lookup_push_token`] / [`token_covers_repo`] with the same Basic/Bearer
//! credential shapes as LFS and Git smart HTTP. Does **not** consult UserStorage.
//! ImportRepo cleanup has a stricter gate, [`authorize_import_repo_removal`]
//! (plan-20260923 ADR-FU-10 item 4).

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

/// ImportRepo cleanup gate (plan-20260923 ADR-FU-10 item 4): only a push token
/// whose `paths` cover `canonical` passes, and its name is the requester.
/// `push_auth=none` or unset is always 403. Every refusal is one of two fixed
/// bodies (401 `authentication required`, 403 `forbidden`) that never name the
/// path. `canonical` must come from `strict_import_repo_leaf_input`: token
/// coverage does not resolve `..`.
pub fn authorize_import_repo_removal(
    git: &GitConfig,
    headers: &HeaderMap,
    canonical: &str,
) -> Result<String, ApiError> {
    if git.push_auth != Some(PushAuth::Token) {
        return Err(import_repo_removal_forbidden());
    }
    authorize_trunk_api_write(git, headers, canonical).map_err(|err| {
        if err.status() == StatusCode::UNAUTHORIZED {
            api_write_auth_challenge()
        } else {
            import_repo_removal_forbidden()
        }
    })
}

fn import_repo_removal_forbidden() -> ApiError {
    ApiError::forbidden(anyhow::anyhow!("forbidden"))
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

    use super::{authorize_import_repo_removal, authorize_trunk_api_write, token_from_headers};
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
        err.status()
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

    async fn body_of(err: crate::common::errors::ApiError) -> (StatusCode, Vec<u8>) {
        use axum::response::IntoResponse;
        let response = err.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    const UNAUTHORIZED: &[u8] =
        br#"{"req_result":false,"data":null,"err_message":"authentication required"}"#;
    const FORBIDDEN: &[u8] = br#"{"req_result":false,"data":null,"err_message":"forbidden"}"#;

    #[tokio::test]
    async fn import_repo_removal_auth_matrix() {
        let token = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![
                token_cfg(
                    "fu20-ci",
                    "secret-ok",
                    Some(vec!["/third-party/a".to_owned()]),
                ),
                token_cfg("fu20-all", "secret-all", None),
            ],
            ..GitConfig::default()
        };
        let none = GitConfig {
            push_auth: Some(PushAuth::None),
            ..GitConfig::default()
        };
        let unset = GitConfig {
            push_tokens: token.push_tokens.clone(),
            ..GitConfig::default()
        };

        for (headers, path, name) in [
            (bearer_headers("secret-ok"), "/third-party/a", "fu20-ci"),
            (bearer_headers("secret-ok"), "/third-party/a/x", "fu20-ci"),
            (
                basic_headers("any", "secret-ok"),
                "/third-party/a",
                "fu20-ci",
            ),
            (bearer_headers("secret-all"), "/third-party/zzz", "fu20-all"),
        ] {
            assert_eq!(
                authorize_import_repo_removal(&token, &headers, path).unwrap(),
                name
            );
        }

        for path in ["/third-party/a", "/third-party/ab", "/third-party/zz"] {
            for headers in [
                HeaderMap::new(),
                bearer_headers("wrong"),
                basic_headers("u", "wrong"),
            ] {
                let err = authorize_import_repo_removal(&token, &headers, path).unwrap_err();
                assert_eq!(err.status(), StatusCode::UNAUTHORIZED, "{path}");
                assert_eq!(
                    body_of(err).await,
                    (StatusCode::UNAUTHORIZED, UNAUTHORIZED.to_vec())
                );
            }
            let mut refusals = vec![
                authorize_import_repo_removal(&none, &HeaderMap::new(), path).unwrap_err(),
                authorize_import_repo_removal(&none, &bearer_headers("secret-ok"), path)
                    .unwrap_err(),
                authorize_import_repo_removal(&unset, &bearer_headers("secret-ok"), path)
                    .unwrap_err(),
            ];
            if path != "/third-party/a" {
                refusals.push(
                    authorize_import_repo_removal(&token, &bearer_headers("secret-ok"), path)
                        .unwrap_err(),
                );
            }
            for err in refusals {
                assert_eq!(err.status(), StatusCode::FORBIDDEN, "{path}");
                assert_eq!(
                    body_of(err).await,
                    (StatusCode::FORBIDDEN, FORBIDDEN.to_vec())
                );
            }
        }
    }
}
