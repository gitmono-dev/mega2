//! OAuth / session extractors for Axum API routes.
//!
//! Git smart HTTP (`/git-receive-pack`, etc.) is handled by [`crate::server::http_server::handle_smart_protocol`],
//! which takes a raw [`axum::http::Request`] and does not run `FromRequestParts`. For the same Mono access-token
//! validation as [`AccessTokenUser`], call [`bearer_token_from_authorization_value`] and
//! [`login_user_from_mono_access_token`] from that code path instead of the extractor.

use axum::{
    RequestPartsExt,
    extract::{FromRef, FromRequestParts},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use axum_extra::{
    TypedHeader,
    headers::{Authorization, authorization::Bearer},
};
use http::{header::COOKIE, request::Parts};
use model::LoginUser;

use crate::{
    api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
    callisto::{bot_tokens, bots},
    common::errors::MegaError,
    jupiter::storage::user_storage::UserStorage,
};

pub mod api_store;
pub mod model;
pub mod website_session_store;

pub struct AuthRedirect;

impl IntoResponse for AuthRedirect {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, "Login first").into_response()
    }
}

pub struct BotIdentity {
    pub bot: bots::Model,
    pub token: bot_tokens::Model,
}

pub struct AccessTokenUser(pub LoginUser);

/// Authenticated user resolved from a website browser-session cookie, not from
/// `Authorization: Bearer` or the Mono DB access-token table.
///
/// The Axum extractor reads the HTTP `Cookie` header and delegates matching
/// configured Better Auth cookie names to [`BrowserSessionStore`].
/// For API clients that send a Mono access token in `Authorization`, use [`AccessTokenUser`] instead.
pub struct SessionUser(pub LoginUser);

/// Authenticated user resolved from a browser session, or `None` for an
/// anonymous request. Unlike [`SessionUser`] this extractor **never rejects**:
/// callers that must serve anonymous requests (and decide for themselves what
/// that means) use it instead of turning "no session" into a 401.
pub struct OptionalSessionUser(pub Option<LoginUser>);

/// Request-scoped result of resolving the authenticated subject (UN-22).
///
/// The session store is consulted **once per request**; every consumer — the
/// authorization guard and the handler extractors alike — reads this cached
/// extension. Resolving twice was not merely wasteful: the guard and the
/// handler could observe different subjects for the same request if the
/// session expired or was revoked in between.
///
/// Three source outcomes are normalized into one value: a live session becomes
/// `Some(user)`; no session and a store failure both become `None`. Collapsing
/// the failure case keeps the existing behavior (a lookup error has always been
/// treated as "not logged in", ADR-WA-03) while still logging a warning at the
/// point of resolution.
#[derive(Clone, Debug)]
pub struct ResolvedSessionPrincipal(pub Option<LoginUser>);

/// Resolve the request's subject once, caching the result in the request
/// extensions. A second call on the same request returns the cached value
/// without touching the session store.
pub async fn resolve_session_principal(
    parts: &mut Parts,
    session_store: &BrowserSessionStore,
) -> ResolvedSessionPrincipal {
    if let Some(cached) = parts.extensions.get::<ResolvedSessionPrincipal>() {
        return cached.clone();
    }

    let cookie_header = parts
        .headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok());

    let resolved = match session_store.load_user(cookie_header).await {
        Ok(user) => ResolvedSessionPrincipal(user),
        Err(error) => {
            tracing::warn!("session lookup failed, treating request as anonymous: {error}");
            ResolvedSessionPrincipal(None)
        }
    };

    parts.extensions.insert(resolved.clone());
    resolved
}

/// Parses a raw `Authorization` header value for `Bearer <token>` (case-insensitive `bearer` prefix).
/// Matches the Git HTTP receive-pack path so CLI clients and API routes share one rule.
pub fn bearer_token_from_authorization_value(value: &str) -> Option<&str> {
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
}

/// Validates a Mono DB access token; same as [`AccessTokenUser`] but usable outside Axum extractors.
pub async fn login_user_from_mono_access_token(
    user_storage: &UserStorage,
    token: &str,
) -> Result<Option<LoginUser>, MegaError> {
    let Some(username) = user_storage.find_user_by_token(token).await? else {
        return Ok(None);
    };
    Ok(Some(LoginUser {
        username,
        ..Default::default()
    }))
}

impl<S> FromRequestParts<S> for BotIdentity
where
    MonoApiServiceState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRedirect;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Extract Authorization: Bearer <token> header
        let bearer = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|e| {
                tracing::debug!("BotIdentity: missing or invalid Authorization header: {e}");
                AuthRedirect
            })?
            .0
            .0;

        let raw_token = bearer.token();
        const BOT_PREFIX: &str = "bot_";

        // Enforce bot_ prefix for bot identity routes
        if !raw_token.starts_with(BOT_PREFIX) {
            tracing::debug!("BotIdentity: bearer token does not start with expected bot_ prefix");
            return Err(AuthRedirect);
        }

        // Delegate token validation to Jupiter storage (BotsStorage)
        let state_ref = MonoApiServiceState::from_ref(state);
        let bots_storage = state_ref.storage.bots_storage();

        // BotsStorage::find_bot_by_token is tolerant to presence/absence of the prefix,
        // but we pass the original token string here for clarity.
        match bots_storage.find_bot_by_token(raw_token).await {
            Ok(Some((bot, token))) => Ok(BotIdentity { bot, token }),
            Ok(None) => {
                tracing::warn!("BotIdentity: bot token not found, revoked, or expired");
                Err(AuthRedirect)
            }
            Err(e) => {
                tracing::error!("BotIdentity: error while validating bot token: {:?}", e);
                Err(AuthRedirect)
            }
        }
    }
}

impl<S> FromRequestParts<S> for AccessTokenUser
where
    UserStorage: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRedirect;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user_storage = UserStorage::from_ref(state);

        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|e| {
                tracing::debug!("AccessTokenUser: missing or invalid bearer token: {e}");
                AuthRedirect
            })?;

        match login_user_from_mono_access_token(&user_storage, bearer.token()).await {
            Ok(Some(user)) => Ok(AccessTokenUser(user)),
            Ok(None) => {
                tracing::debug!("AccessTokenUser: invalid or expired bearer token");
                Err(AuthRedirect)
            }
            Err(e) => {
                tracing::warn!("AccessTokenUser: error validating bearer token: {e:?}");
                Err(AuthRedirect)
            }
        }
    }
}

impl<S> FromRequestParts<S> for SessionUser
where
    BrowserSessionStore: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRedirect;

    /// Resolves the request's subject (once per request, UN-22) and rejects
    /// anonymous requests — unchanged behavior.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session_store = BrowserSessionStore::from_ref(state);
        match resolve_session_principal(parts, &session_store).await {
            ResolvedSessionPrincipal(Some(user)) => Ok(Self(user)),
            ResolvedSessionPrincipal(None) => Err(AuthRedirect),
        }
    }
}

impl<S> FromRequestParts<S> for OptionalSessionUser
where
    BrowserSessionStore: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    /// Same single resolution as [`SessionUser`], but an anonymous request is a
    /// value rather than a rejection: this extractor never produces a 401.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let session_store = BrowserSessionStore::from_ref(state);
        let ResolvedSessionPrincipal(user) = resolve_session_principal(parts, &session_store).await;
        Ok(Self(user))
    }
}

// Backward-compatible extractor: `LoginUser` now maps to cookie session only.
// Use `AccessTokenUser` explicitly where bearer token auth is required.
impl<S> FromRequestParts<S> for LoginUser
where
    BrowserSessionStore: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRedirect;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let SessionUser(user) = SessionUser::from_request_parts(parts, state).await?;
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    use super::{
        SessionUser,
        api_store::{BrowserSessionStore, FixedUserSessionStore},
        model::LoginUser,
    };

    async fn current_username(SessionUser(user): SessionUser) -> String {
        user.username
    }

    #[tokio::test]
    async fn fixed_user_session_is_visible_to_handler() {
        let app = Router::new()
            .route("/me", get(current_username))
            .with_state(BrowserSessionStore::Fixed(FixedUserSessionStore {
                user: LoginUser {
                    website_user_id: "website-user-1".to_string(),
                    username: "fixed-session-user".to_string(),
                    avatar_url: String::new(),
                    email: "fixed-session@example.com".to_string(),
                },
            }));

        let response = app
            .oneshot(Request::builder().uri("/me").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "fixed-session-user"
        );
    }
}
