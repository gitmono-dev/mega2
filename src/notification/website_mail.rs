use std::time::Duration;

use reqwest::redirect::Policy;
use serde_json::{Value, json};

use crate::{common::errors::MegaError, config::secret::SecretString};

/// Best-effort client for the website-owned product-email API.
///
/// The API owns rendering, queueing, and SMTP/provider delivery. This client
/// only submits the notification event and intentionally has no retry queue.
///
/// Every request carries an `Idempotency-Key` derived from the delivery's own
/// identity (see [`idempotency_key`]), so a duplicate submission of the same
/// logical notification collapses into one email on the website side.
pub struct WebsiteMailClient {
    client: reqwest::Client,
    endpoint: String,
    bearer: SecretString,
}

impl WebsiteMailClient {
    pub fn new(base_url: &str, bearer: SecretString) -> Result<Self, MegaError> {
        let endpoint = format!(
            "{}/api/internal/notifications/email",
            base_url.trim_end_matches('/')
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(Policy::none())
            .build()
            .map_err(|_| {
                MegaError::Other("failed to build website mail HTTP client".to_string())
            })?;
        Ok(Self {
            client,
            endpoint,
            bearer,
        })
    }

    /// Submit a product notification per `docs/refactoring/website-mail.md`.
    ///
    /// `payload` must be the event-specific business fields (not pre-rendered
    /// subject/HTML). Bearer and full payload are never logged here.
    pub async fn send(
        &self,
        event_type: &str,
        username: &str,
        email: &str,
        locale: &str,
        payload: &Value,
    ) -> Result<(), MegaError> {
        // Send the *canonical* bytes, not `.json(&body)`. The key is the hash
        // of this exact string, so `key equal <=> wire bytes equal` holds by
        // construction. Hashing a canonical form while sending a differently
        // ordered body would break that: the website fingerprints the body it
        // receives, so the same key could arrive with a fingerprint it has
        // already bound to different bytes — a permanent 409 on what is really
        // the same delivery.
        let body = request_body(event_type, username, email, locale, payload);
        let canonical = canonical_json(&body);
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.bearer.expose_secret())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idempotency_key_of_canonical(&canonical))
            .body(canonical)
            .send()
            .await
            .map_err(|error| {
                MegaError::Other(format!(
                    "website mail delivery failed: {}",
                    transport_error_kind(&error)
                ))
            })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return Err(MegaError::Other(format!(
                "website mail delivery returned HTTP {status} ({})",
                failure_disposition(status)
            )));
        }
        Ok(())
    }
}

/// Build the request body. Shared with [`idempotency_key_for`] so the key is
/// always derived from exactly the bytes that get sent.
fn request_body(
    event_type: &str,
    username: &str,
    email: &str,
    locale: &str,
    payload: &Value,
) -> Value {
    json!({
        "event_type": event_type,
        "recipient": {
            "username": username,
            "email": email,
        },
        "locale": locale,
        "payload": payload,
    })
}

/// The `Idempotency-Key` this client will send for a given delivery.
///
/// Public because it is contract surface, not an implementation detail:
/// `docs/refactoring/website-mail.md` §2.3 names the derivation, and the
/// stack-level test asserts that a raw request carrying this key lands on the
/// website's duplicate path — which is the only way to prove from outside that
/// [`WebsiteMailClient::send`] is idempotent rather than merely successful.
/// The exact request body [`WebsiteMailClient::send`] transmits for a given
/// delivery.
///
/// Public for the same reason as [`idempotency_key_for`]: the contract's
/// central invariant is that the key and these bytes are 1:1
/// (`docs/refactoring/website-mail.md` §2.3), and the only way to check that
/// from outside the client is to replay both together. A caller that builds
/// its own JSON will produce a different byte string, and the website will
/// correctly answer `409` — which is exactly what §2.3 means by "409 only
/// appears when the key is hand-constructed by some other caller".
pub fn canonical_request_body(
    event_type: &str,
    username: &str,
    email: &str,
    locale: &str,
    payload: &Value,
) -> String {
    canonical_json(&request_body(event_type, username, email, locale, payload))
}

pub fn idempotency_key_for(
    event_type: &str,
    username: &str,
    email: &str,
    locale: &str,
    payload: &Value,
) -> String {
    idempotency_key_of_canonical(&canonical_request_body(
        event_type, username, email, locale, payload,
    ))
}

/// Classify a non-2xx response so the operator-facing log says whether the
/// delivery can ever succeed on a retry.
///
/// Mapping is `docs/refactoring/website-mail.md` §2.2. The distinction that
/// matters most is 409 vs 425: both are triggered by the `Idempotency-Key`,
/// but 409 means the key was reused for different content (a caller bug worth
/// investigating) while 425 is only concurrency (benign). Reporting a race as
/// a permanent conflict sends operators chasing a contract violation that did
/// not happen.
fn failure_disposition(status: u16) -> &'static str {
    match status {
        // Same key, still in flight with the same body — retrying the SAME key
        // later would succeed. This repo is best-effort and does not retry.
        425 | 429 => "transient",
        // 5xx is the website's own provider/infrastructure failing.
        500..=599 => "transient; upstream unavailable",
        // 409 here would mean we reused a key across different bodies. Since
        // the key IS the body hash (see `idempotency_key`), that indicates the
        // contract or the derivation drifted, not a race.
        409 => "permanent; idempotency key reused across different bodies",
        400 | 401 | 403 | 422 => "permanent; configuration or contract error",
        _ => "permanent",
    }
}

/// Derive the `Idempotency-Key` from the delivery's identity.
///
/// Contract: `docs/refactoring/website-mail.md` §2.3. The key MUST be a stable
/// function of `event_type` + recipient + `locale` + `payload`, never a fresh
/// random value: with a random key per call the website side can never
/// recognise a replay, so the documented "same key must not send twice"
/// guarantee is dead on arrival.
///
/// Hash of the canonical body — and [`WebsiteMailClient::send`] transmits that
/// same string, so the key and the wire bytes are always 1:1.
///
/// Canonical means object keys sorted recursively, rather than
/// `Value::to_string`. `serde_json`'s key order depends on the `preserve_order`
/// feature, which is active here only because another crate in the tree
/// (`cedar-policy-core`) turns it on. Deriving the key from `to_string` would
/// tie the key space to a transitive feature flag we do not control: if that
/// crate ever dropped it, `Map` would become a `BTreeMap`, every derived key
/// would change, and two differently-built replicas would disagree about what
/// "the same delivery" hashes to.
///
/// Because the key IS the hash of the bytes we send, `key equal <=> body equal`
/// holds, which makes a `409 idempotency_conflict` structurally impossible for
/// this caller.
fn idempotency_key_of_canonical(canonical: &str) -> String {
    blake3::hash(canonical.as_bytes()).to_hex().to_string()
}

/// Canonical rendering of `value`: object keys sorted, arrays order-preserving,
/// scalars via `serde_json`'s own escaping.
fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn transport_error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection error"
    } else if error.is_redirect() {
        "unexpected redirect (refused)"
    } else if error.is_request() {
        "request error"
    } else {
        "transport error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror of what `send` does: canonicalise, then hash.
    fn key_of(value: &Value) -> String {
        idempotency_key_of_canonical(&canonical_json(value))
    }

    fn body(event: &str, username: &str, excerpt: &str) -> Value {
        request_body(
            event,
            username,
            "a@example.test",
            "en",
            &json!({ "cl_link": "CL-1", "actor_username": "bob", "comment_excerpt": excerpt }),
        )
    }

    #[test]
    fn idempotency_key_is_stable_across_calls_for_the_same_delivery() {
        // The whole point: a replay of the same logical delivery must present
        // the same key, or the website side can never recognise it as a
        // duplicate (docs/refactoring/website-mail.md §2.3).
        let request = body("cl.comment.created", "alice", "hello");
        let first = key_of(&request);
        let second = key_of(&request);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn idempotency_key_differs_per_recipient_event_and_payload() {
        let base = key_of(&body("cl.comment.created", "alice", "hello"));
        // Each identity dimension the contract names must move the key.
        assert_ne!(
            base,
            key_of(&body("cl.comment.created", "bob", "hello")),
            "recipient must be part of the key"
        );
        assert_ne!(
            base,
            key_of(&body("cl.merged", "alice", "hello")),
            "event_type must be part of the key"
        );
        assert_ne!(
            base,
            key_of(&body("cl.comment.created", "alice", "goodbye")),
            "payload must be part of the key"
        );
    }

    #[test]
    fn idempotency_key_never_leaks_recipient_or_payload() {
        // The key travels in a header and shows up in the website's logs.
        let key = key_of(&body("cl.comment.created", "alice", "secret-text"));
        assert!(!key.contains("alice"));
        assert!(!key.contains("secret-text"));
        assert!(!key.contains("example.test"));
        assert!(
            key.chars().all(|c| c.is_ascii_hexdigit()),
            "key must be a bare hex digest, got {key}"
        );
    }

    #[test]
    fn public_key_helper_matches_what_send_would_use() {
        // The stack test relies on this equality to prove idempotency from the
        // outside; if the two ever diverge that proof silently becomes vacuous.
        let payload =
            json!({ "cl_link": "CL-1", "actor_username": "bob", "comment_excerpt": "hi" });
        let via_helper = idempotency_key_for(
            "cl.comment.created",
            "alice",
            "a@example.test",
            "en",
            &payload,
        );
        let via_body = key_of(&request_body(
            "cl.comment.created",
            "alice",
            "a@example.test",
            "en",
            &payload,
        ));
        assert_eq!(via_helper, via_body);
    }

    #[test]
    fn idempotency_key_is_independent_of_object_key_order() {
        // The whole point of canonicalising: two Values that differ only in the
        // order their keys were inserted are the SAME delivery and must hash
        // identically, regardless of whether serde_json's `preserve_order`
        // feature happens to be active in the build.
        let a = json!({ "b": 1, "a": 2, "nested": { "y": true, "x": false } });
        let mut b = serde_json::Map::new();
        b.insert("nested".into(), json!({ "x": false, "y": true }));
        b.insert("a".into(), json!(2));
        b.insert("b".into(), json!(1));
        assert_eq!(key_of(&a), key_of(&Value::Object(b)));
    }

    #[test]
    fn idempotency_key_still_separates_arrays_by_order() {
        // Arrays are ordered data, not a set: reordering them is a different
        // delivery and must move the key.
        assert_ne!(
            key_of(&json!({ "k": [1, 2] })),
            key_of(&json!({ "k": [2, 1] }))
        );
    }

    #[test]
    fn idempotency_key_does_not_confuse_adjacent_fields() {
        // A naive concatenation would collide these; the canonical form keeps
        // the JSON delimiters, so they stay distinct.
        assert_ne!(
            key_of(&json!({ "a": "xy", "b": "z" })),
            key_of(&json!({ "a": "x", "b": "yz" }))
        );
    }

    #[test]
    fn canonical_json_is_what_goes_on_the_wire_and_reparses_to_the_same_value() {
        // The invariant the contract rests on: the key is the hash of exactly
        // the bytes we transmit. If these ever diverge, the same key can reach
        // the website with bytes it already bound to a different fingerprint,
        // and a benign replay becomes a permanent 409.
        let payload = json!({ "z": 1, "a": { "d": 4, "c": 3 }, "arr": [3, 1, 2] });
        let request = request_body("cl.merged", "alice", "a@example.test", "en", &payload);
        let canonical = canonical_json(&request);

        assert_eq!(
            idempotency_key_of_canonical(&canonical),
            idempotency_key_for("cl.merged", "alice", "a@example.test", "en", &payload),
            "the public helper must predict the key `send` transmits"
        );

        let reparsed: Value =
            serde_json::from_str(&canonical).expect("canonical output must be valid JSON");
        assert_eq!(
            reparsed, request,
            "canonicalising must not change the value"
        );
        assert_eq!(
            canonical_json(&reparsed),
            canonical,
            "canonical form must be a fixed point"
        );
    }

    #[test]
    fn failure_disposition_separates_transient_from_permanent() {
        // 425 vs 409 is the distinction that matters: a benign concurrency race
        // must not read as a caller bug (website-mail.md §2.2).
        assert_eq!(failure_disposition(425), "transient");
        assert_eq!(failure_disposition(429), "transient");
        assert_eq!(failure_disposition(503), "transient; upstream unavailable");
        assert!(failure_disposition(409).starts_with("permanent"));
        assert!(failure_disposition(401).starts_with("permanent"));
        assert!(failure_disposition(422).starts_with("permanent"));
    }
}
