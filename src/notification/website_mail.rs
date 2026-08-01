use std::time::Duration;

use reqwest::redirect::Policy;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{common::errors::MegaError, config::secret::SecretString};

/// Best-effort client for the website-owned product-email API.
///
/// The API owns rendering, queueing, and SMTP/provider delivery. This client
/// only submits the notification event and intentionally has no retry queue.
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
        let body = json!({
            "event_type": event_type,
            "recipient": {
                "username": username,
                "email": email,
            },
            "locale": locale,
            "payload": payload,
        });
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.bearer.expose_secret())
            .header("Idempotency-Key", Uuid::new_v4().to_string())
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                MegaError::Other(format!(
                    "website mail delivery failed: {}",
                    transport_error_kind(&error)
                ))
            })?;
        if !response.status().is_success() {
            return Err(MegaError::Other(format!(
                "website mail delivery returned HTTP {}",
                response.status().as_u16()
            )));
        }
        Ok(())
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
