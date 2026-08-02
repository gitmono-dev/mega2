//! Stack-level website internal product-email API (WE-06).
//!
//! Talks to compose-hosted `website-next` only (bearer + Idempotency-Key).
//! Opt-in: `WEBSITE_IT=1` after `--profile web` (or app+web). A requested run
//! fails if website-next is unreachable.

use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use reqwest::{
    blocking::Client,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::{Value, json};

const WEBSITE_BASE_URL: &str = "http://127.0.0.1:17001";
const IT_BEARER: &str = "monoengine-it-website-mail-bearer-0001";
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

fn website_it_mail_is_requested_and_available() -> bool {
    if std::env::var("WEBSITE_IT").as_deref() != Ok("1") {
        eprintln!(
            "SKIP: integration_website_mail requires WEBSITE_IT=1 and compose profile web"
        );
        return false;
    }
    assert!(
        is_reachable("127.0.0.1:17001"),
        "WEBSITE_IT=1 requires website-next at 127.0.0.1:17001; start with \
         `docker compose -p monoengine-it -f docker-compose.test.yml \
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
