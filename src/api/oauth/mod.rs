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
        http::{Request, StatusCode, header::COOKIE},
        routing::get,
    };
    use http::request::Parts;
    use tower::ServiceExt;

    use super::{
        AuthRedirect, OptionalSessionUser, ResolvedSessionPrincipal, SessionUser,
        api_store::{BrowserSessionStore, CountingSessionStore, FixedUserSessionStore},
        model::LoginUser,
        resolve_session_principal,
    };
    use crate::common::errors::MegaError;

    async fn current_username(SessionUser(user): SessionUser) -> String {
        user.username
    }

    fn user(name: &str) -> LoginUser {
        LoginUser {
            website_user_id: format!("website-{name}"),
            username: name.to_string(),
            avatar_url: String::new(),
            email: format!("{name}@example.invalid"),
        }
    }

    fn counting_state(
        answers: Vec<Result<Option<LoginUser>, MegaError>>,
    ) -> (BrowserSessionStore, CountingSessionStore) {
        let store = CountingSessionStore::new(answers);
        (BrowserSessionStore::Counting(store.clone()), store)
    }

    /// Two consumers in one request: the answer they see must be identical,
    /// which with a drifting store is only possible if the resolution was cached.
    async fn two_consumers(
        OptionalSessionUser(optional): OptionalSessionUser,
        SessionUser(session): SessionUser,
    ) -> String {
        format!(
            "{}|{}",
            optional.map(|u| u.username).unwrap_or_else(|| "-".into()),
            session.username
        )
    }

    async fn optional_only(OptionalSessionUser(user): OptionalSessionUser) -> String {
        user.map(|u| u.username)
            .unwrap_or_else(|| "anonymous".into())
    }

    async fn session_only(SessionUser(user): SessionUser) -> String {
        user.username
    }

    fn app(
        state: BrowserSessionStore,
        handler: axum::routing::MethodRouter<BrowserSessionStore>,
    ) -> Router {
        Router::new().route("/probe", handler).with_state(state)
    }

    async fn call(router: Router) -> (StatusCode, String) {
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(COOKIE, "better-auth.session_token=whatever")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, String::from_utf8_lossy(&body).to_string())
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

    #[tokio::test]
    async fn resolves_the_session_store_at_most_once_per_request() {
        // The store hands out a different user on the second call, so agreement
        // between the two extractors can only come from the cached resolution.
        let (state, counter) = counting_state(vec![
            Ok(Some(user("first-answer"))),
            Ok(Some(user("second-answer"))),
        ]);
        let (status, body) = call(app(state, get(two_consumers))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "first-answer|first-answer",
            "both consumers must see the same resolution"
        );
        assert_eq!(
            counter.call_count(),
            1,
            "the session store must be consulted once per request"
        );
    }

    #[tokio::test]
    async fn optional_session_user_never_rejects_an_anonymous_request() {
        let (state, counter) = counting_state(vec![Ok(None)]);
        let (status, body) = call(app(state, get(optional_only))).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "OptionalSessionUser must not turn 'no session' into a 401"
        );
        assert_eq!(body, "anonymous");
        assert_eq!(counter.call_count(), 1);
    }

    #[tokio::test]
    async fn session_user_still_rejects_an_anonymous_request() {
        let (state, _counter) = counting_state(vec![Ok(None)]);
        let (status, _body) = call(app(state, get(session_only))).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "SessionUser keeps its existing reject-anonymous behavior"
        );
    }

    #[tokio::test]
    async fn store_failure_is_normalized_to_anonymous() {
        let (state, counter) = counting_state(vec![Err(MegaError::Other("store down".into()))]);
        let (status, body) = call(app(state.clone(), get(optional_only))).await;
        assert_eq!(status, StatusCode::OK, "a store failure is not a 401 here");
        assert_eq!(body, "anonymous");
        assert_eq!(counter.call_count(), 1);

        // The same failure still rejects on the mandatory extractor. A lookup
        // error has always read as "not logged in".
        let (state, _counter) = counting_state(vec![Err(MegaError::Other("store down".into()))]);
        let (status, _body) = call(app(state, get(session_only))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn resolution_is_cached_in_the_request_extensions() {
        let (state, counter) = counting_state(vec![
            Ok(Some(user("cached"))),
            Ok(Some(user("must-not-be-read"))),
        ]);
        let request = Request::builder()
            .uri("/probe")
            .body(Body::empty())
            .unwrap();
        let (mut parts, _body): (Parts, _) = request.into_parts();

        assert!(parts.extensions.get::<ResolvedSessionPrincipal>().is_none());
        let first = resolve_session_principal(&mut parts, &state).await;
        assert!(parts.extensions.get::<ResolvedSessionPrincipal>().is_some());

        let second = resolve_session_principal(&mut parts, &state).await;
        assert_eq!(
            first.0.map(|u| u.username),
            second.0.map(|u| u.username),
            "the second call must return the cached value"
        );
        assert_eq!(counter.call_count(), 1);
    }

    #[tokio::test]
    async fn login_user_consumes_the_same_resolution() {
        async fn login_user_handler(user: LoginUser) -> String {
            user.username
        }
        let (state, counter) = counting_state(vec![Ok(Some(user("login-user")))]);
        let (status, body) = call(app(state, get(login_user_handler))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "login-user");
        assert_eq!(counter.call_count(), 1);
    }

    /// A middleware resolves the subject before the handler does, exactly as
    /// `cedar_guard` does. The handler must observe that answer, not a fresh lookup.
    #[tokio::test]
    async fn pre_handler_resolution_is_reused_by_the_handler() {
        async fn guard_like(
            axum::extract::State(state): axum::extract::State<BrowserSessionStore>,
            request: Request<Body>,
            next: axum::middleware::Next,
        ) -> Result<axum::response::Response, AuthRedirect> {
            let (mut parts, body) = request.into_parts();
            let _ = resolve_session_principal(&mut parts, &state).await;
            Ok(next.run(Request::from_parts(parts, body)).await)
        }

        let (state, counter) = counting_state(vec![
            Ok(Some(user("guard-saw-this"))),
            Ok(Some(user("handler-must-not-see-this"))),
        ]);
        let router = Router::new()
            .route("/probe", get(optional_only))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                guard_like,
            ))
            .with_state(state);

        let (status, body) = call(router).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "guard-saw-this",
            "the handler must reuse the guard's resolution"
        );
        assert_eq!(
            counter.call_count(),
            1,
            "guard + handler must share one lookup"
        );
    }
}
