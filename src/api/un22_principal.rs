//! UN-22: request-scoped single resolution of the authenticated subject.
//!
//! The session store must be consulted at most once per request, and every
//! consumer must see that same answer. Resolving twice was not merely wasteful:
//! the guard and the handler could observe *different* subjects for one request
//! if the session expired or was revoked in between.
//!
//! The counting store double returns a different user on each call, so a test
//! that asserts two consumers agree is genuinely proving the caching — with a
//! fixed store both would agree even without it.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header::COOKIE},
    routing::get,
};
use http::request::Parts;
use tower::ServiceExt;

use crate::{
    api::oauth::{
        AuthRedirect, OptionalSessionUser, ResolvedSessionPrincipal, SessionUser,
        api_store::{BrowserSessionStore, CountingSessionStore},
        model::LoginUser,
        resolve_session_principal,
    },
    common::errors::MegaError,
};

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

/// Two consumers in one request: the answer they see must be identical, which
/// with a drifting store is only possible if the resolution was cached.
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
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn un22_resolves_the_session_store_at_most_once_per_request() {
    // The store hands out a *different* user on the second call, so agreement
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
async fn un22_optional_session_user_never_rejects_an_anonymous_request() {
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
async fn un22_session_user_still_rejects_an_anonymous_request() {
    let (state, _counter) = counting_state(vec![Ok(None)]);
    let (status, _body) = call(app(state, get(session_only))).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "SessionUser keeps its existing reject-anonymous behavior"
    );
}

#[tokio::test]
async fn un22_store_failure_is_normalized_to_anonymous() {
    let (state, counter) = counting_state(vec![Err(MegaError::Other("store down".into()))]);
    let (status, body) = call(app(state.clone(), get(optional_only))).await;
    assert_eq!(status, StatusCode::OK, "a store failure is not a 401 here");
    assert_eq!(body, "anonymous");
    assert_eq!(counter.call_count(), 1);

    // The same failure still rejects on the mandatory extractor — unchanged
    // behavior (a lookup error has always read as "not logged in").
    let (state, _counter) = counting_state(vec![Err(MegaError::Other("store down".into()))]);
    let (status, _body) = call(app(state, get(session_only))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn un22_resolution_is_cached_in_the_request_extensions() {
    let (state, counter) = counting_state(vec![
        Ok(Some(user("cached"))),
        Ok(Some(user("must-not-be-read"))),
    ]);
    let request = Request::builder()
        .uri("/probe")
        .body(Body::empty())
        .unwrap();
    let (mut parts, _body): (Parts, _) = request.into_parts();

    // Nothing cached yet: the first call resolves and backfills the extension.
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
async fn un22_login_user_consumes_the_same_resolution() {
    async fn login_user_handler(user: LoginUser) -> String {
        user.username
    }
    let (state, counter) = counting_state(vec![Ok(Some(user("login-user")))]);
    let (status, body) = call(app(state, get(login_user_handler))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "login-user");
    assert_eq!(counter.call_count(), 1);
}

/// Guard-shaped probe: a middleware resolves the subject before the handler
/// does, exactly as `cedar_guard` now does. The handler must observe the
/// guard's answer, not a fresh lookup.
#[tokio::test]
async fn un22_a_pre_handler_resolution_is_reused_by_the_handler() {
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
