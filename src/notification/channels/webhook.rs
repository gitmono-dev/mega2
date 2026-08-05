use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use serde_json::json;

use super::{CHANNEL_WEBHOOK, NotificationChannel, OutboundMessage};
use crate::{common::errors::MegaError, config::secret::SecretString};

/// Generic outbound webhook channel (docs/notification.md phase 3).
///
/// POSTs a JSON summary of each notification to a configured URL, optionally with
/// an `Authorization: Bearer <token>` whose value is resolved from a vault
/// `SecretRef` after startup (so credentials are never stored in plaintext
/// config and only resolved once vault is ready — the phase-3 acceptance).
///
/// Errors never include the destination URL or the token: a transport failure is
/// reported as a coarse category and an HTTP failure as the status code only, so
/// a token-bearing request can never leak the token or a sensitive URL into logs.
/// Redirects are refused (`Policy::none()`) to avoid redirect-based SSRF; the URL
/// itself is operator-trusted configuration.
pub struct WebhookChannel {
    client: reqwest::Client,
    url: String,
    token: Option<SecretString>,
}

impl WebhookChannel {
    pub fn new(url: String, token: Option<SecretString>) -> Result<Self, MegaError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .build()
            .map_err(|_| MegaError::Other("failed to build webhook HTTP client".to_string()))?;
        Ok(Self { client, url, token })
    }
}

#[async_trait]
impl NotificationChannel for WebhookChannel {
    fn name(&self) -> &'static str {
        CHANNEL_WEBHOOK
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        let payload = json!({
            "username": message.username,
            "event_type": message.event_type_code,
            "subject": message.subject,
            "body_html": message.body_html,
            "body_text": message.body_text,
        });
        let mut request = self.client.post(&self.url).json(&payload);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        let response = request.send().await.map_err(|error| {
            MegaError::Other(format!(
                "webhook delivery failed: {}",
                transport_error_kind(&error)
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(MegaError::Other(format!(
                "webhook delivery returned HTTP {}",
                status.as_u16()
            )));
        }
        Ok(())
    }
}

/// Coarse, URL-free description of a transport failure. The reqwest `Display`
/// impl embeds the request URL, which may itself be sensitive (e.g. a Slack
/// incoming-webhook URL), so callers must use this instead of `error.to_string()`.
pub(super) fn transport_error_kind(error: &reqwest::Error) -> &'static str {
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
    use std::{net::SocketAddr, sync::Arc};

    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured {
        body: Arc<Mutex<Option<serde_json::Value>>>,
        auth: Arc<Mutex<Option<String>>>,
    }

    async fn capture_handler(
        State(state): State<Captured>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> &'static str {
        *state.auth.lock().await = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        *state.body.lock().await = serde_json::from_slice(&body).ok();
        "ok"
    }

    async fn spawn_capture_server(state: Captured) -> SocketAddr {
        let app = Router::new()
            .route("/hook", post(capture_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn sample_message() -> OutboundMessage<'static> {
        OutboundMessage {
            username: "alice",
            event_type_code: "cl.comment.created",
            to: "alice@example.com",
            subject: "New comment",
            body_html: "<p>hello</p>",
            body_text: Some("hello"),
        }
    }

    #[tokio::test]
    async fn webhook_posts_json_with_bearer_token() {
        let state = Captured::default();
        let addr = spawn_capture_server(state.clone()).await;
        let url = format!("http://{addr}/hook");

        let channel =
            WebhookChannel::new(url, Some(SecretString::new("s3cret-token"))).expect("build");
        assert_eq!(channel.name(), "webhook");

        channel
            .deliver(&sample_message())
            .await
            .expect("delivery should succeed");

        let body = state.body.lock().await.clone().expect("body captured");
        assert_eq!(body["username"], "alice");
        assert_eq!(body["event_type"], "cl.comment.created");
        assert_eq!(body["subject"], "New comment");
        let auth = state.auth.lock().await.clone().expect("auth captured");
        assert_eq!(auth, "Bearer s3cret-token");
    }

    #[tokio::test]
    async fn webhook_error_on_non_success_status_has_no_token_or_url() {
        // Server that always 500s.
        async fn fail() -> (axum::http::StatusCode, &'static str) {
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
        }
        let app = Router::new().route("/hook", post(fail));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}/hook");

        let channel = WebhookChannel::new(url.clone(), Some(SecretString::new("s3cret-token")))
            .expect("build");
        let err = channel
            .deliver(&sample_message())
            .await
            .expect_err("non-success status should error");
        let message = err.to_string();
        assert!(message.contains("HTTP 500"));
        assert!(!message.contains("s3cret-token"));
        assert!(!message.contains(&url));
    }

    #[tokio::test]
    async fn webhook_transport_error_does_not_leak_url_or_token() {
        // Port 1 is not listening; connection fails fast. Proxies are disabled
        // explicitly: a host-level system proxy (which reqwest honors) would
        // answer for the dead port with its own HTTP 5xx and route this test
        // into the HTTP-status branch instead of the transport branch.
        let url = "http://127.0.0.1:1/hook".to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .expect("build no-proxy test client");
        let channel = WebhookChannel {
            client,
            url: url.clone(),
            token: Some(SecretString::new("s3cret-token")),
        };
        let err = channel
            .deliver(&sample_message())
            .await
            .expect_err("connection should fail");
        let message = err.to_string();
        assert!(message.contains("webhook delivery failed"));
        assert!(!message.contains("s3cret-token"));
        assert!(!message.contains(&url));
    }
}
