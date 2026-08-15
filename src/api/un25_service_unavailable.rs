//! UN-25: the merge face's "authorization unavailable" answer is 503.
//!
//! Before this, an unmapped `[code:xxx]` fell through to 500 — indistinguishable
//! from a bug, and by contract not retryable. 503 says something different and
//! actionable: the server could not *decide* right now, and the same request may
//! succeed later. That distinction is what lets a caller (and the merge queue)
//! retry instead of treating the change as rejected.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use tower::ServiceExt;

use crate::common::errors::{ApiError, MegaError};

async fn call(router: Router) -> StatusCode {
    router
        .oneshot(
            Request::builder()
                .uri("/probe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds")
        .status()
}

fn router_returning(code: &'static str) -> Router {
    Router::new().route(
        "/probe",
        get(move || async move {
            Err::<&'static str, ApiError>(ApiError::from(MegaError::Other(format!(
                "[code:{code}] authorization is unavailable"
            ))))
        }),
    )
}

#[tokio::test]
async fn un25_a_coded_503_reaches_the_client_as_503() {
    assert_eq!(
        call(router_returning("503")).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "the merge face's authorization-unavailable state must be retryable by contract"
    );
}

#[tokio::test]
async fn un25_503_is_distinguishable_from_an_internal_error() {
    assert_eq!(
        call(router_returning("500")).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_ne!(
        call(router_returning("503")).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "503 must not collapse back into 500 — a caller cannot retry a bug"
    );
}

#[tokio::test]
async fn un25_an_unknown_code_still_falls_back_to_500() {
    // The 503 arm is an addition, not a change to the fallback.
    assert_eq!(
        call(router_returning("599")).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn un25_the_existing_status_mappings_are_unchanged() {
    for (code, expected) in [
        ("400", StatusCode::BAD_REQUEST),
        ("401", StatusCode::UNAUTHORIZED),
        ("403", StatusCode::FORBIDDEN),
        ("404", StatusCode::NOT_FOUND),
        ("409", StatusCode::CONFLICT),
    ] {
        assert_eq!(call(router_returning(code)).await, expected, "code {code}");
    }
}
