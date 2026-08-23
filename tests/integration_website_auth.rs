//! Stack-level Better Auth session integration (ITW-03).
//!
//! This target talks only to the compose-hosted `website-next` and
//! `monoengine` services. It is opt-in outside CI: set `WEBSITE_IT=1` after
//! starting `--profile app --profile web`. A requested run fails if either
//! service is unavailable, so a skipped test cannot mask a missing CI gate.

use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use reqwest::{
    blocking::{Client, Response},
    header::{CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE},
};
use serde_json::{Value, json};

const WEBSITE_BASE_URL: &str = "http://127.0.0.1:17001";
const MONOENGINE_BASE_URL: &str = "http://127.0.0.1:19180";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn integration_website_auth_session_matches_get_session_and_rejects_anonymous() {
    if !website_it_stack_is_requested_and_available() {
        return;
    }

    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("build HTTP client");
    let suffix = format!("{}-{}", std::process::id(), unix_timestamp_millis());
    let name = format!("itw03-{suffix}");
    let email = format!("itw03-{suffix}@example.test");
    let password = "itw03-throwaway-password";

    let signup = client
        .post(format!("{WEBSITE_BASE_URL}/api/auth/sign-up/email"))
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, WEBSITE_BASE_URL)
        .json(&json!({
            "name": name,
            "email": email,
            "password": password,
        }))
        .send()
        .expect("sign up against website-next");
    assert_success(&signup, "sign-up");

    // Exercise the explicit sign-in contract too, and use its fresh session
    // cookie for all downstream requests.
    let signin = client
        .post(format!("{WEBSITE_BASE_URL}/api/auth/sign-in/email"))
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, WEBSITE_BASE_URL)
        .json(&json!({
            "email": email,
            "password": password,
        }))
        .send()
        .expect("sign in against website-next");
    let cookie = session_cookie_header(&signin);
    assert_success(&signin, "sign-in");
    assert!(
        cookie.split(';').any(|pair| {
            pair.starts_with("better-auth.session_token=")
                || pair.starts_with("__Secure-better-auth.session_token=")
        }),
        "sign-in did not issue a Better Auth session cookie"
    );

    let website_session = client
        .get(format!("{WEBSITE_BASE_URL}/api/auth/get-session"))
        .header(COOKIE, &cookie)
        .send()
        .expect("get website session")
        .json::<Value>()
        .expect("decode website get-session response");
    let website_user = website_session
        .get("user")
        .and_then(Value::as_object)
        .expect("get-session must return its authenticated user");
    let expected_id = website_user
        .get("id")
        .and_then(Value::as_str)
        .expect("get-session user id")
        .to_owned();
    let expected_name = website_user
        .get("name")
        .and_then(Value::as_str)
        .expect("get-session user name")
        .to_owned();

    let monoengine_user = client
        .get(format!("{MONOENGINE_BASE_URL}/api/v1/user"))
        .header(COOKIE, &cookie)
        .send()
        .expect("request monoengine user with website session cookie");
    assert_success(&monoengine_user, "authenticated monoengine /api/v1/user");
    let monoengine_user = monoengine_user
        .json::<Value>()
        .expect("decode monoengine user response");
    let login_user = monoengine_user
        .get("data")
        .and_then(Value::as_object)
        .expect("monoengine user response data");
    assert_eq!(
        login_user.get("username").and_then(Value::as_str),
        Some(expected_name.as_str()),
        "monoengine username must match website get-session user.name"
    );
    assert_eq!(
        login_user.get("website_user_id").and_then(Value::as_str),
        Some(expected_id.as_str()),
        "monoengine website_user_id must match website get-session user.id"
    );

    let anonymous = client
        .get(format!("{MONOENGINE_BASE_URL}/api/v1/user"))
        .send()
        .expect("request monoengine user without cookie");
    assert_eq!(
        anonymous.status().as_u16(),
        401,
        "anonymous /api/v1/user must be 401 Login first (got {})",
        anonymous.status()
    );
}

fn website_it_stack_is_requested_and_available() -> bool {
    if std::env::var("WEBSITE_IT").as_deref() != Ok("1") {
        eprintln!(
            "SKIP: integration_website_auth requires WEBSITE_IT=1 and compose profiles app + web"
        );
        return false;
    }

    for (name, address) in [
        ("website-next", "127.0.0.1:17001"),
        ("monoengine", "127.0.0.1:19180"),
    ] {
        assert!(
            is_reachable(address),
            "WEBSITE_IT=1 requires {name} at {address}; start with \
             `docker compose -p monoengine-it -f docker-compose.test.yml \
             --profile app --profile web up -d --wait`"
        );
    }
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

fn session_cookie_header(response: &Response) -> String {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .filter_map(|header| header.split(';').next())
        .filter(|pair| {
            pair.starts_with("better-auth.session_token=")
                || pair.starts_with("__Secure-better-auth.session_token=")
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("; ")
}

fn assert_success(response: &Response, operation: &str) {
    let status = response.status();
    assert!(
        status.is_success(),
        "{operation} failed with status {status}; response body is intentionally omitted"
    );
}

fn unix_timestamp_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis()
}
