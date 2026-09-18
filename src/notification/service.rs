//! Notification channel registration and delivery fan-out.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    common::errors::MegaError,
    config::{DEFAULT_NOTIFICATION_DELIVERY_MODE, reload::ConfigHandle},
    jupiter::storage::notification_storage::NotificationStorage,
    notification::{
        channels::{
            CHANNEL_IN_APP, ConsoleChannel, InAppChannel, NotificationChannel, OutboundMessage,
        },
        website_mail::WebsiteMailClient,
    },
};

static ACTIVE: RwLock<Option<Arc<NotificationService>>> = RwLock::new(None);

/// Holds channels available to notification delivery and the global gate.
///
/// Event triggers call [`deliver_user_notification`], which fans out to the
/// registered in-app channel plus any optional Slack/webhook channels without
/// requiring a mailer or local outbox dispatcher.
pub struct NotificationService {
    channels: Vec<Arc<dyn NotificationChannel>>,
    website_mail: Option<Arc<WebsiteMailClient>>,
    /// When present, `enabled` is read live from the config snapshot
    /// (hot-reload safe).
    config_handle: Option<ConfigHandle>,
    enabled_fallback: AtomicBool,
}

impl NotificationService {
    pub fn new(
        stg: NotificationStorage,
        extra_channels: Vec<Arc<dyn NotificationChannel>>,
        website_mail: Option<Arc<WebsiteMailClient>>,
        config_handle: Option<ConfigHandle>,
        enabled: bool,
    ) -> Self {
        let mut channels: Vec<Arc<dyn NotificationChannel>> =
            Vec::with_capacity(extra_channels.len() + 1);
        channels.push(Arc::new(InAppChannel::new(stg)));
        channels.extend(extra_channels);
        Self {
            channels,
            website_mail,
            config_handle,
            enabled_fallback: AtomicBool::new(enabled),
        }
    }

    pub fn set_active(service: Option<Arc<Self>>) {
        match ACTIVE.write() {
            Ok(mut guard) => *guard = service,
            Err(poisoned) => {
                *poisoned.into_inner() = service;
            }
        }
    }

    pub fn active() -> Option<Arc<Self>> {
        match ACTIVE.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn channels(&self) -> &[Arc<dyn NotificationChannel>] {
        &self.channels
    }

    pub fn channel_for(&self, channel_name: &str) -> Option<Arc<dyn NotificationChannel>> {
        self.channels
            .iter()
            .find(|channel| channel.name() == channel_name)
            .map(Arc::clone)
    }

    pub fn console_channel() -> Arc<dyn NotificationChannel> {
        Arc::new(ConsoleChannel::new())
    }

    pub fn is_enabled(&self) -> bool {
        if let Some(handle) = &self.config_handle
            && let Ok(config) = handle.snapshot()
        {
            return config
                .notification
                .as_ref()
                .map(|cfg| cfg.enabled)
                .unwrap_or(true);
        }
        self.enabled_fallback.load(Ordering::Relaxed)
    }

    pub fn default_delivery_mode(&self) -> String {
        DEFAULT_NOTIFICATION_DELIVERY_MODE.to_string()
    }

    pub fn default_locale(&self) -> String {
        "en-US".to_string()
    }

    /// Keep notification-service lifetime tied to application shutdown.
    pub async fn start(self: Arc<Self>, shutdown: CancellationToken) {
        shutdown.cancelled().await;
        Self::set_active(None);
    }
}

/// Default delivery mode for new user settings rows (config-backed when active).
pub fn current_default_delivery_mode() -> String {
    NotificationService::active()
        .map(|service| service.default_delivery_mode())
        .unwrap_or_else(|| DEFAULT_NOTIFICATION_DELIVERY_MODE.to_string())
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Deliver a product notification: honor the global kill switch, user prefs,
/// then fan out to registered channels (in-app primary; Slack/webhook extras).
///
/// Product email is not initiated here. The leftover website-mail client stays
/// assembled until RM-WM; this function does not call it.
pub async fn deliver_user_notification(
    stg: &NotificationStorage,
    username: &str,
    event_type: &str,
    subject: &str,
    body_text: &str,
    _website_payload: serde_json::Value,
) -> Result<(), MegaError> {
    if let Some(service) = NotificationService::active()
        && !service.is_enabled()
    {
        return Ok(());
    }

    if !stg.should_send(username, event_type).await? {
        return Ok(());
    }

    let Some(settings) = stg.get_user_settings(username).await? else {
        return Ok(());
    };

    let body_html = format!("<p>{}</p>", escape_html(body_text));
    let message = OutboundMessage {
        username,
        event_type_code: event_type,
        to: &settings.email,
        subject,
        body_html: &body_html,
        body_text: Some(body_text),
    };

    if let Some(service) = NotificationService::active() {
        for channel in service.channels() {
            match channel.deliver(&message).await {
                Ok(()) => {}
                Err(error) if channel.name() == CHANNEL_IN_APP => return Err(error),
                Err(error) => {
                    warn!(
                        channel = channel.name(),
                        error = %error,
                        "notification channel delivery failed"
                    );
                }
            }
        }
        return Ok(());
    }

    // Unit tests / callers without an activated service still write in-app.
    stg.create_inbox_notification(username, event_type, subject, &body_html, Some(body_text))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        jupiter::{migration::apply_migrations, tests::test_db_connection},
        notification::{
            channels::{SlackChannel, WebhookChannel},
            testing::MockChannel,
        },
    };

    /// `NotificationService::set_active` writes a process-global handle, so the
    /// tests that install one must not overlap — `cargo test` runs them on
    /// parallel threads, and without this lock one test's `set_active(None)`
    /// tears down the service another test is mid-delivery on (FIX-02).
    static ACTIVE_SERVICE_LOCK: std::sync::LazyLock<Mutex<()>> =
        std::sync::LazyLock::new(|| Mutex::new(()));

    fn test_service(
        stg: NotificationStorage,
        extra: Vec<Arc<dyn NotificationChannel>>,
        enabled: bool,
    ) -> Arc<NotificationService> {
        Arc::new(NotificationService::new(stg, extra, None, None, enabled))
    }

    #[tokio::test]
    async fn service_starts_without_mail_and_registers_in_app() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db));
        let service = test_service(stg, Vec::new(), true);

        assert_eq!(service.channels().len(), 1);
        assert_eq!(service.channels()[0].name(), "in_app");
        assert!(service.channel_for("in_app").is_some());
        assert!(service.channel_for("email").is_none());
    }

    #[tokio::test]
    async fn service_registers_configured_extra_channels_without_mail() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let stg = NotificationStorage::new(Arc::new(db));
        let slack: Arc<dyn NotificationChannel> = Arc::new(
            SlackChannel::new(crate::config::secret::SecretString::new(
                "http://127.0.0.1:1/services/secret",
            ))
            .unwrap(),
        );
        let webhook: Arc<dyn NotificationChannel> =
            Arc::new(WebhookChannel::new("http://127.0.0.1:1/hook".to_string(), None).unwrap());
        let service = test_service(stg, vec![slack, webhook], true);

        assert_eq!(service.channels().len(), 3);
        assert_eq!(service.channels()[0].name(), "in_app");
        assert!(service.channel_for("slack").is_some());
        assert!(service.channel_for("webhook").is_some());
    }

    #[tokio::test]
    async fn deliver_fans_out_to_extra_channels_when_service_active() {
        let _active_guard = ACTIVE_SERVICE_LOCK.lock().await;
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let stg = NotificationStorage::new(Arc::new(db));
        stg.upsert_event_type("cl.comment.created", "cl", "test", false, true)
            .await
            .unwrap();
        stg.upsert_user_settings("alice", "alice@example.test")
            .await
            .unwrap();

        let mock = Arc::new(MockChannel::new("slack", true));
        let service = test_service(
            stg.clone(),
            vec![mock.clone() as Arc<dyn NotificationChannel>],
            true,
        );
        NotificationService::set_active(Some(Arc::clone(&service)));

        deliver_user_notification(
            &stg,
            "alice",
            "cl.comment.created",
            "subject",
            "hello <world>",
            serde_json::json!({
                "cl_link": "CL1",
                "actor_username": "bob",
                "comment_excerpt": "hello <world>",
            }),
        )
        .await
        .unwrap();

        let sent = mock.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].username, "alice");
        assert_eq!(sent[0].subject, "subject");
        NotificationService::set_active(None);
    }

    #[tokio::test]
    async fn configured_website_mail_is_not_invoked_from_deliver() {
        let _active_guard = ACTIVE_SERVICE_LOCK.lock().await;
        #[derive(Clone, Default)]
        struct CapturedRequest {
            authorization: String,
            idempotency_key: String,
            payload: Option<Value>,
        }

        async fn capture(
            State(captured): State<Arc<Mutex<CapturedRequest>>>,
            headers: HeaderMap,
            Json(payload): Json<Value>,
        ) -> StatusCode {
            let mut captured = captured.lock().await;
            captured.authorization = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            captured.idempotency_key = headers
                .get("idempotency-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            captured.payload = Some(payload);
            StatusCode::ACCEPTED
        }

        let captured = Arc::new(Mutex::new(CapturedRequest::default()));
        let app = Router::new()
            .route("/api/internal/notifications/email", post(capture))
            .with_state(Arc::clone(&captured));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let stg = NotificationStorage::new(Arc::new(db));
        stg.upsert_event_type("cl.comment.created", "cl", "test", false, true)
            .await
            .unwrap();
        stg.upsert_user_settings("alice", "alice@example.test")
            .await
            .unwrap();
        stg.set_delivery_mode("alice", "email").await.unwrap();
        stg.set_preferred_locale("alice", Some("zh-CN"))
            .await
            .unwrap();

        let mail_client = Arc::new(
            WebsiteMailClient::new(
                &format!("http://{address}"),
                crate::config::secret::SecretString::new("it-shared-bearer"),
            )
            .unwrap(),
        );
        let service = Arc::new(NotificationService::new(
            stg.clone(),
            Vec::new(),
            Some(mail_client),
            None,
            true,
        ));
        NotificationService::set_active(Some(service));

        let website_payload = serde_json::json!({
            "cl_link": "CL-123",
            "actor_username": "bob",
            "comment_excerpt": "Please review the latest change.",
        });
        deliver_user_notification(
            &stg,
            "alice",
            "cl.comment.created",
            "New comment",
            "review requested",
            website_payload.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            stg.list_inbox_notifications("alice", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        let captured = captured.lock().await;
        assert!(
            captured.payload.is_none(),
            "deliver must not POST website-mail after RM-02B"
        );
        assert!(captured.authorization.is_empty());
        assert!(captured.idempotency_key.is_empty());
        drop(captured);
        NotificationService::set_active(None);
        server.abort();
    }

    #[tokio::test]
    async fn global_disabled_skips_all_delivery() {
        let _active_guard = ACTIVE_SERVICE_LOCK.lock().await;
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let stg = NotificationStorage::new(Arc::new(db));
        stg.upsert_event_type("cl.comment.created", "cl", "test", false, true)
            .await
            .unwrap();
        stg.upsert_user_settings("alice", "alice@example.test")
            .await
            .unwrap();

        let mock = Arc::new(MockChannel::new("slack", true));
        let service = test_service(
            stg.clone(),
            vec![mock.clone() as Arc<dyn NotificationChannel>],
            false,
        );
        NotificationService::set_active(Some(service));

        deliver_user_notification(
            &stg,
            "alice",
            "cl.comment.created",
            "subject",
            "hello",
            serde_json::json!({
                "cl_link": "CL1",
                "actor_username": "bob",
                "comment_excerpt": "hello",
            }),
        )
        .await
        .unwrap();

        assert!(mock.take_sent().is_empty());
        assert!(
            stg.list_inbox_notifications("alice", 10)
                .await
                .unwrap()
                .is_empty()
        );
        NotificationService::set_active(None);
    }
}
