//! Stack-level website internal product-email API (WE-06).
//!
//! Talks to compose-hosted `website-next` only (bearer + Idempotency-Key).
//! Opt-in: `WEBSITE_IT=1` after `--profile web` (or app+web). A requested run
//! fails if website-next is unreachable.
//!
//! Two layers on purpose:
//!
//! * the hand-written `reqwest` cases pin the **wire contract** independently of
//!   our client, so a bug in `WebsiteMailClient` cannot mask a contract break;
//! * `mega2_client_*` drive the real
//!   [`mega2_core::notification::website_mail::WebsiteMailClient`], so a
//!   regression in *our own* endpoint/bearer/idempotency-key wiring fails here
//!   instead of shipping green. Without it the whole `WEBSITE_IT` gate never
//!   executes a single line of the client that production uses — the compose
//!   stack cannot close that gap on its own, because
//!   `notification.default_delivery_mode` is `in_app`, so no trigger ever
//!   reaches the mail leg.

use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use mega2_core::{
    config::secret::SecretString,
    notification::website_mail::{WebsiteMailClient, canonical_request_body, idempotency_key_for},
};
use reqwest::{
    blocking::Client,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::{Value, json};

const WEBSITE_BASE_URL: &str = "http://127.0.0.1:17001";
const IT_BEARER: &str = "mega2-it-website-mail-bearer-0001";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn integration_website_mail_accepts_allowlisted_event() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("build HTTP client");

    let response = client
        .post(format!(
            "{WEBSITE_BASE_URL}/api/internal/notifications/email"
        ))
        .header(AUTHORIZATION, format!("Bearer {IT_BEARER}"))
        .header(CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", format!("we06-ok-{}", unix_millis()))
        .json(&sample_body())
        .send()
        .expect("POST internal email");

    let status = response.status().as_u16();
    let body = response
        .json::<Value>()
        .unwrap_or_else(|_| json!({ "decode": "failed" }));
    assert_eq!(
        status, 202,
        "allowlisted event must be accepted (got {status}): {body}"
    );
    assert_eq!(body.get("accepted"), Some(&json!(true)));
    assert_eq!(body.get("duplicate"), Some(&json!(false)));
    assert!(
        body.get("delivery_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        "delivery_id required: {body}"
    );
}

#[test]
fn unauthorized_bearer_is_rejected() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("build HTTP client");

    let response = client
        .post(format!(
            "{WEBSITE_BASE_URL}/api/internal/notifications/email"
        ))
        .header(AUTHORIZATION, "Bearer wrong-it-bearer-value")
        .header(CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", format!("we06-401-{}", unix_millis()))
        .json(&sample_body())
        .send()
        .expect("POST internal email with bad bearer");

    let status = response.status().as_u16();
    let body = response
        .json::<Value>()
        .unwrap_or_else(|_| json!({ "decode": "failed" }));
    assert_eq!(status, 401, "bad bearer must be 401 (got {status}): {body}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("unauthorized"),
        "sanitized unauthorized code: {body}"
    );
    let serialized = body.to_string();
    assert!(
        !serialized.contains(IT_BEARER),
        "response must not echo the real bearer"
    );
}

fn sample_body() -> Value {
    json!({
        "event_type": "cl.comment.created",
        "recipient": {
            "username": "alice",
            "email": "alice@example.test"
        },
        "locale": "en",
        "payload": {
            "cl_link": "CL-WE06",
            "actor_username": "bob",
            "comment_excerpt": "stack mail ok"
        }
    })
}

/// Drive the production client end to end against the live website-next.
///
/// Also proves the `Idempotency-Key` derivation (website-mail.md §2.3): sending
/// the *same* delivery twice must be accepted both times, because the key is a
/// stable function of the request identity and the second call therefore lands
/// on the duplicate path rather than sending a second email.
#[test]
fn mega2_client_send_is_accepted_and_idempotent() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let client = WebsiteMailClient::new(WEBSITE_BASE_URL, SecretString::new(IT_BEARER))
        .expect("build WebsiteMailClient");

    // Unique per run so a previous run's accepted key cannot mask a failure.
    let excerpt = format!("client leg {}", unix_millis());
    let payload = json!({
        "cl_link": "CL-WE06-CLIENT",
        "actor_username": "bob",
        "comment_excerpt": excerpt,
    });

    runtime
        .block_on(client.send(
            "cl.comment.created",
            "alice",
            "alice@example.test",
            "en",
            &payload,
        ))
        .expect("WebsiteMailClient must accept an allowlisted event");

    // Identical delivery => identical derived key => duplicate, still 2xx.
    runtime
        .block_on(client.send(
            "cl.comment.created",
            "alice",
            "alice@example.test",
            "en",
            &payload,
        ))
        .expect("a replay of the same delivery must stay accepted (duplicate path)");

    // `send` returns `Result<(), _>`, so a green replay above proves only that
    // the second call was accepted — not that it was recognised as a replay
    // instead of sending a second email. Re-issue the same delivery on the wire
    // with the key AND the bytes the client itself derives: `duplicate: true`
    // is the website confirming it already holds that key bound to that exact
    // fingerprint, which is only possible if `send` presented the identical key
    // both times.
    //
    // The body must be `canonical_request_body`, not a hand-built `json!`: the
    // website fingerprints the bytes it receives, and a differently ordered
    // rendering of the same content is a different fingerprint under the same
    // key — i.e. a 409. That is the contract working as designed (§2.3), and it
    // is precisely why the client hashes the bytes it sends rather than some
    // other rendering.
    let key = idempotency_key_for(
        "cl.comment.created",
        "alice",
        "alice@example.test",
        "en",
        &payload,
    );
    let body = canonical_request_body(
        "cl.comment.created",
        "alice",
        "alice@example.test",
        "en",
        &payload,
    );
    let http = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("build HTTP client");
    let response = http
        .post(format!(
            "{WEBSITE_BASE_URL}/api/internal/notifications/email"
        ))
        .header(AUTHORIZATION, format!("Bearer {IT_BEARER}"))
        .header(CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", &key)
        .body(body)
        .send()
        .expect("replay the client-derived key on the wire");
    let status = response.status().as_u16();
    let body = response
        .json::<Value>()
        .unwrap_or_else(|_| json!({ "decode": "failed" }));
    assert_eq!(status, 202, "replay must be accepted: {body}");
    assert_eq!(
        body.get("duplicate"),
        Some(&json!(true)),
        "the client's derived Idempotency-Key must already be known to the website \
         (i.e. `send` used a stable key, not a fresh random one): {body}"
    );
}

/// A misconfigured bearer must fail loudly through our client, and the error we
/// hand to the logs must never contain the bearer itself.
#[test]
fn mega2_client_rejects_wrong_bearer_without_leaking_it() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let wrong = "wrong-it-bearer-value";
    let client = WebsiteMailClient::new(WEBSITE_BASE_URL, SecretString::new(wrong))
        .expect("build WebsiteMailClient");

    let error = runtime
        .block_on(client.send(
            "cl.comment.created",
            "alice",
            "alice@example.test",
            "en",
            &json!({
                "cl_link": "CL-WE06-CLIENT-401",
                "actor_username": "bob",
                "comment_excerpt": "unauthorized leg",
            }),
        ))
        .expect_err("a wrong bearer must not be accepted");

    let message = error.to_string();
    assert!(
        message.contains("401"),
        "error should carry the HTTP status, got: {message}"
    );
    assert!(
        message.contains("permanent"),
        "error should classify 401 as permanent so operators do not retry: {message}"
    );
    assert!(
        !message.contains(wrong) && !message.contains(IT_BEARER),
        "error must not echo any bearer"
    );
}

/// The blank-bearer hole, end to end: config validation now rejects it, but if
/// one ever reached the client the website side must refuse it too.
#[test]
fn mega2_client_blank_bearer_is_rejected_by_the_website() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let client = WebsiteMailClient::new(WEBSITE_BASE_URL, SecretString::new("   "))
        .expect("build WebsiteMailClient");

    let error = runtime
        .block_on(client.send(
            "cl.comment.created",
            "alice",
            "alice@example.test",
            "en",
            &json!({
                "cl_link": "CL-WE06-CLIENT-BLANK",
                "actor_username": "bob",
                "comment_excerpt": "blank bearer leg",
            }),
        ))
        .expect_err("a blank bearer must never be accepted");
    assert!(
        error.to_string().contains("401"),
        "blank bearer must be a 401, got: {error}"
    );
}

/// Lock the *invariant* behind the 425 row: same key + same body is NEVER a
/// permanent `409`.
///
/// Honest scope: with the IT stack's in-memory `EMAIL_PROVIDER=test` the send
/// completes instantly, so in practice both requests here come back `202`
/// (accepted, then duplicate) and the `425` branch itself is not reached. The
/// 425 branch is covered on the front-end side by
/// `tests/unit/email/internal-notifications-email-idempotency.test.ts`, which
/// gates a slow sender past the 2s wait window. What this case does gate is the
/// regression that matters at the wire level: a front end that answered `409`
/// for a concurrent same-body duplicate — the behaviour before monoui
/// `48e4acb` — fails here.
#[test]
fn concurrent_same_body_duplicate_is_425_not_409() {
    if !website_it_mail_is_requested_and_available() {
        return;
    }

    let key = format!("we06-inflight-{}", unix_millis());
    let payload = json!({
        "cl_link": "CL-WE06-INFLIGHT",
        "actor_username": "bob",
        "comment_excerpt": format!("inflight {}", unix_millis()),
    });
    let body = canonical_request_body(
        "cl.comment.created",
        "alice",
        "alice@example.test",
        "en",
        &payload,
    );

    let post = |body: String, key: String| {
        std::thread::spawn(move || {
            let client = Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("build HTTP client");
            let response = client
                .post(format!(
                    "{WEBSITE_BASE_URL}/api/internal/notifications/email"
                ))
                .header(AUTHORIZATION, format!("Bearer {IT_BEARER}"))
                .header(CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key)
                .body(body)
                .send()
                .expect("POST internal email");
            let status = response.status().as_u16();
            let json = response
                .json::<Value>()
                .unwrap_or_else(|_| json!({ "decode": "failed" }));
            (status, json)
        })
    };

    // Two identical requests under one key. Whatever the interleaving, neither
    // may be a permanent 409: same key + same bytes is never a conflict. Both
    // 202 (fast provider) and 425 (slow provider) are acceptable outcomes.
    let first = post(body.clone(), key.clone());
    let second = post(body, key);
    let results = [
        first.join().expect("first request thread"),
        second.join().expect("second request thread"),
    ];

    for (status, json) in &results {
        assert_ne!(
            *status, 409,
            "same key + same body must never be a permanent conflict: {json}"
        );
        assert!(
            matches!(*status, 202 | 425),
            "expected 202 (accepted/duplicate) or 425 (in flight), got {status}: {json}"
        );
        if *status == 425 {
            assert_eq!(
                json.get("code").and_then(Value::as_str),
                Some("idempotency_in_progress"),
                "425 must carry the transient code: {json}"
            );
        }
    }
    assert!(
        results.iter().any(|(status, _)| *status == 202),
        "at least one of the two concurrent requests must be accepted: {results:?}"
    );
}

fn website_it_mail_is_requested_and_available() -> bool {
    if std::env::var("WEBSITE_IT").as_deref() != Ok("1") {
        eprintln!("SKIP: integration_website_mail requires WEBSITE_IT=1 and compose profile web");
        return false;
    }
    assert!(
        is_reachable("127.0.0.1:17001"),
        "WEBSITE_IT=1 requires website-next at 127.0.0.1:17001; start with \
         `docker compose -p mega2-it -f docker-compose.test.yml \
         --profile web up -d --wait website-next`"
    );
    true
}

fn is_reachable(address: &str) -> bool {
    address
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
