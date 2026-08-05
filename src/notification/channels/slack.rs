use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use serde_json::json;

use super::{CHANNEL_SLACK, NotificationChannel, OutboundMessage, webhook::transport_error_kind};
use crate::{common::errors::MegaError, config::secret::SecretString};

/// Slack incoming-webhook channel (docs/notification.md phase 3).
///
/// POSTs a Slack-formatted message (`{"text": ...}`) to an incoming-webhook URL
/// resolved from a vault `SecretRef` after startup. A Slack incoming-webhook URL
/// embeds a secret token in its path, so the URL is held as a [`SecretString`]
/// and is never logged or returned in an error (transport failures are reported
/// as a coarse category, HTTP failures as the status code only). Redirects are
/// refused to avoid leaking the URL via a redirect target.
pub struct SlackChannel {
    client: reqwest::Client,
    webhook_url: SecretString,
}

impl SlackChannel {
    pub fn new(webhook_url: SecretString) -> Result<Self, MegaError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .build()
            .map_err(|_| MegaError::Other("failed to build slack HTTP client".to_string()))?;
        Ok(Self {
            client,
            webhook_url,
        })
    }
}

#[async_trait]
impl NotificationChannel for SlackChannel {
    fn name(&self) -> &'static str {
        CHANNEL_SLACK
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        let text = match message.body_text {
            Some(body) if !body.is_empty() => format!("*{}*\n{}", message.subject, body),
            _ => format!("*{}*", message.subject),
        };
        let payload = json!({ "text": text });
        let response = self
            .client
            .post(self.webhook_url.expose_secret())
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                MegaError::Other(format!(
                    "slack delivery failed: {}",
                    transport_error_kind(&error)
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(MegaError::Other(format!(
                "slack delivery returned HTTP {}",
                status.as_u16()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use axum::{Router, extract::State, routing::post};
    use tokio::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn slack_posts_text_payload_to_webhook_url() {
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/services/secret",
                post(
                    |State(state): State<Arc<Mutex<Option<serde_json::Value>>>>,
                     body: axum::body::Bytes| async move {
                        *state.lock().await = serde_json::from_slice(&body).ok();
                        "ok"
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = format!("http://{addr}/services/secret");
        let channel = SlackChannel::new(SecretString::new(url)).expect("build");
        assert_eq!(channel.name(), "slack");

        let message = OutboundMessage {
            username: "bob",
            event_type_code: "cl.merged",
            to: "bob@example.com",
            subject: "CL merged",
            body_html: "<p>merged</p>",
            body_text: Some("Your CL was merged"),
        };
        channel.deliver(&message).await.expect("delivery succeeds");

        let body = captured.lock().await.clone().expect("body captured");
        assert_eq!(body["text"], "*CL merged*\nYour CL was merged");
    }

    #[tokio::test]
    async fn slack_transport_error_does_not_leak_webhook_url() {
        // The webhook URL embeds the Slack secret; a transport failure must not
        // include it. Port 1 is not listening.
        let secret_url = "http://127.0.0.1:1/services/T000/B000/SECRETTOKEN".to_string();
        let channel = SlackChannel::new(SecretString::new(secret_url.clone())).expect("build");
        let message = OutboundMessage {
            username: "bob",
            event_type_code: "cl.merged",
            to: "bob@example.com",
            subject: "CL merged",
            body_html: "<p>merged</p>",
            body_text: None,
        };
        let err = channel.deliver(&message).await.expect_err("should fail");
        let rendered = err.to_string();
        assert!(rendered.contains("slack delivery failed"));
        assert!(!rendered.contains("SECRETTOKEN"));
        assert!(!rendered.contains(&secret_url));
    }
}
