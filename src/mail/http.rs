//! HTTP-based `Mailer` implementation.
//!
//! Posts a JSON payload to a configurable endpoint. Intended for generic
//! webhook-style providers (e.g. SendGrid, AWS SES HTTP, or a custom relay).
//! Attachments are base64-encoded inline.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{MailAttachment, Mailer};
use crate::{common::errors::MegaError, config::MailConfig};

/// JSON payload sent by the HTTP mail provider.
#[derive(Debug, Serialize, Deserialize)]
struct HttpMailPayload {
    to: String,
    subject: String,
    html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    attachments: Vec<HttpMailAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HttpMailAttachment {
    filename: String,
    content_type: String,
    content: String,
}

/// Mailer that delivers by POSTing JSON to `http_url`.
#[derive(Clone, Debug)]
pub struct HttpMailer {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    enabled: bool,
}

impl HttpMailer {
    pub fn new(cfg: &MailConfig) -> Result<Self, MegaError> {
        let url = cfg
            .http_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                MegaError::Other("mail.http_url is required for HTTP provider".to_string())
            })?
            .to_owned();

        let parsed = reqwest::Url::parse(&url)
            .map_err(|e| MegaError::Other(format!("mail.http_url is not a valid URL: {e}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(MegaError::Other(format!(
                "mail.http_url scheme must be http or https, got {}",
                parsed.scheme()
            )));
        }

        let timeout = Duration::from_secs(cfg.http_timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| MegaError::Other(format!("failed to build http mail client: {e}")))?;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        for (name, value) in &cfg.http_headers {
            let header_name = header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                MegaError::Other(format!("invalid mail.http_headers key '{name}': {e}"))
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|e| {
                MegaError::Other(format!("invalid mail.http_headers value for '{name}': {e}"))
            })?;
            headers.insert(header_name, header_value);
        }

        Ok(Self {
            client,
            url,
            headers,
            enabled: cfg.enabled,
        })
    }
}

#[async_trait]
impl Mailer for HttpMailer {
    async fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
    ) -> Result<(), MegaError> {
        self.send_html_with_attachments(to, subject, html, text, &[])
            .await
    }

    async fn send_html_with_attachments(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
        attachments: &[MailAttachment],
    ) -> Result<(), MegaError> {
        if !self.enabled {
            return Ok(());
        }

        let payload = HttpMailPayload {
            to: to.to_owned(),
            subject: subject.to_owned(),
            html: html.to_owned(),
            text: text.map(str::to_owned),
            attachments: attachments
                .iter()
                .map(|a| HttpMailAttachment {
                    filename: a.filename.clone(),
                    content_type: a.content_type.clone(),
                    content: base64::engine::general_purpose::STANDARD.encode(&a.content),
                })
                .collect(),
        };

        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|e| MegaError::Other(format!("http mail request failed: {e}")))?;

        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable response>".to_string());
            Err(MegaError::Other(format!(
                "http mail provider returned {status}: {body}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, net::SocketAddr};

    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::{MailConfig, MailProvider};

    #[derive(Clone)]
    struct TestAppState {
        tx: mpsc::Sender<HttpMailPayload>,
    }

    async fn capture_handler(
        State(state): State<TestAppState>,
        Json(payload): Json<HttpMailPayload>,
    ) -> StatusCode {
        let _ = state.tx.send(payload).await;
        StatusCode::OK
    }

    async fn start_capture_server() -> (SocketAddr, mpsc::Receiver<HttpMailPayload>) {
        let (tx, rx) = mpsc::channel(4);
        let app = Router::new()
            .route("/mail", post(capture_handler))
            .with_state(TestAppState { tx });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        (addr, rx)
    }

    #[tokio::test]
    async fn http_mailer_posts_json_payload() {
        let (addr, mut rx) = start_capture_server().await;
        let cfg = MailConfig {
            enabled: true,
            provider: MailProvider::Http,
            http_url: Some(format!("http://{addr}/mail")),
            http_headers: {
                let mut h = HashMap::new();
                h.insert("X-Custom-Header".to_string(), "custom-value".to_string());
                h
            },
            ..Default::default()
        };

        let mailer = HttpMailer::new(&cfg).unwrap();
        mailer
            .send_html("to@example.com", "Subject", "<p>Hi</p>", Some("Hi"))
            .await
            .unwrap();

        let payload = rx.recv().await.expect("server should receive payload");
        assert_eq!(payload.to, "to@example.com");
        assert_eq!(payload.subject, "Subject");
        assert_eq!(payload.html, "<p>Hi</p>");
        assert_eq!(payload.text, Some("Hi".to_string()));
    }

    #[tokio::test]
    async fn http_mailer_posts_attachment_as_base64() {
        let (addr, mut rx) = start_capture_server().await;
        let cfg = MailConfig {
            enabled: true,
            provider: MailProvider::Http,
            http_url: Some(format!("http://{addr}/mail")),
            ..Default::default()
        };

        let mailer = HttpMailer::new(&cfg).unwrap();
        let attachment = MailAttachment::new("file.txt", "text/plain", b"hello");
        mailer
            .send_html_with_attachments(
                "to@example.com",
                "Subject",
                "<p>Hi</p>",
                None,
                std::slice::from_ref(&attachment),
            )
            .await
            .unwrap();

        let payload = rx.recv().await.expect("server should receive payload");
        assert_eq!(payload.attachments.len(), 1);
        assert_eq!(payload.attachments[0].filename, "file.txt");
        assert_eq!(
            payload.attachments[0].content,
            base64::engine::general_purpose::STANDARD.encode(b"hello")
        );
    }

    #[tokio::test]
    async fn http_mailer_returns_error_on_non_2xx() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/mail", post(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });

        let cfg = MailConfig {
            enabled: true,
            provider: MailProvider::Http,
            http_url: Some(format!("http://{addr}/mail")),
            ..Default::default()
        };

        let mailer = HttpMailer::new(&cfg).unwrap();
        let err = mailer
            .send_html("to@example.com", "Subj", "body", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("503"));
    }

    #[test]
    fn http_mailer_requires_url() {
        let cfg = MailConfig {
            enabled: true,
            provider: MailProvider::Http,
            ..Default::default()
        };
        let err = HttpMailer::new(&cfg).unwrap_err();
        assert!(err.to_string().contains("mail.http_url is required"));
    }
}
