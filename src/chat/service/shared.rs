use std::{sync::LazyLock, time::Duration};

use chrono::Utc;
use regex::Regex;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::{attachment, open_graph_link, reactions},
    common::errors::MegaError,
    jupiter::storage::{
        attachment_storage::AttachmentStorage, base_storage::StorageConnector,
        channel_membership_storage::ChannelMembershipStorage,
        custom_reaction_storage::CustomReactionStorage, message_storage::MessageStorage,
        open_graph_storage::OpenGraphStorage, reaction_storage::ReactionStorage,
    },
};

#[derive(Clone)]
pub struct SharedChatService {
    pub attachment_storage: AttachmentStorage,
    pub reaction_storage: ReactionStorage,
    pub custom_reaction_storage: CustomReactionStorage,
    pub open_graph_storage: OpenGraphStorage,
    pub message_storage: MessageStorage,
    pub membership_storage: ChannelMembershipStorage,
}

impl SharedChatService {
    pub fn new(
        attachment_storage: AttachmentStorage,
        reaction_storage: ReactionStorage,
        custom_reaction_storage: CustomReactionStorage,
        open_graph_storage: OpenGraphStorage,
        message_storage: MessageStorage,
        membership_storage: ChannelMembershipStorage,
    ) -> Self {
        Self {
            attachment_storage,
            reaction_storage,
            custom_reaction_storage,
            open_graph_storage,
            message_storage,
            membership_storage,
        }
    }

    pub fn from_storage(storage: &crate::jupiter::storage::Storage) -> Self {
        Self::new(
            storage.attachment_storage(),
            storage.reaction_storage(),
            storage.custom_reaction_storage(),
            storage.open_graph_storage(),
            storage.message_storage(),
            storage.channel_membership_storage(),
        )
    }

    pub async fn create_reaction(
        &self,
        message_public_id: &str,
        username: String,
        content: Option<String>,
        custom_reaction_public_id: Option<String>,
    ) -> Result<reactions::Model, MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Message {} not found", message_public_id))
            })?;

        // Verify membership of caller in channel
        let mem = self
            .membership_storage
            .get_membership(msg.channel_id, &username)
            .await?;
        if mem.is_none() {
            // Return not found to prevent channel enumeration
            return Err(MegaError::NotFound(format!(
                "Message {} not found",
                message_public_id
            )));
        }

        let custom_reaction_id = if let Some(ref cr_pub) = custom_reaction_public_id {
            let cr = self
                .custom_reaction_storage
                .get_custom_reaction_by_public_id(cr_pub)
                .await?
                .ok_or_else(|| {
                    MegaError::NotFound(format!("Custom reaction {} not found", cr_pub))
                })?;
            Some(cr.id)
        } else {
            None
        };

        // Check if reaction already exists to prevent duplicate reaction insertion
        let existing = self
            .reaction_storage
            .get_reactions_by_subject("Message", msg.id)
            .await?;
        let duplicate = existing.iter().any(|r| {
            r.username == username
                && r.content == content
                && r.custom_reaction_id == custom_reaction_id
        });
        if duplicate {
            return Err(MegaError::Other("Reaction already exists".into()));
        }

        let public_id = crate::callisto::entity_ext::generate_public_id();
        let reaction = self
            .reaction_storage
            .create_reaction(
                public_id,
                content,
                "Message".to_string(),
                msg.id,
                username,
                custom_reaction_id,
            )
            .await?;

        Ok(reaction)
    }

    pub async fn delete_reaction(
        &self,
        reaction_public_id: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        let reaction = reactions::Entity::find()
            .filter(reactions::Column::PublicId.eq(reaction_public_id))
            .filter(reactions::Column::DiscardedAt.is_null())
            .one(self.reaction_storage.get_connection())
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Reaction {} not found", reaction_public_id))
            })?;

        if reaction.username != username {
            return Err(MegaError::Other("Only reaction owner can delete".into()));
        }

        // Verify the caller is still a current member of the channel that owns
        // the reacted message. A removed member must not be able to mutate
        // reactions even if they originally created them.
        if reaction.subject_type == "Message"
            && let Some(msg) = self
                .message_storage
                .get_message_by_id(reaction.subject_id)
                .await?
        {
            let mem = self
                .membership_storage
                .get_membership(msg.channel_id, username)
                .await?;
            if mem.is_none() {
                return Err(MegaError::NotFound(format!(
                    "Reaction {} not found",
                    reaction_public_id
                )));
            }
        }

        self.reaction_storage
            .soft_delete_reaction(reaction_public_id)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_attachment(
        &self,
        message_public_id: &str,
        username: &str,
        file_path: String,
        file_type: String,
        name: String,
        size: i64,
        position: i32,
    ) -> Result<attachment::Model, MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Message {} not found", message_public_id))
            })?;

        // Verify membership of caller in channel
        let mem = self
            .membership_storage
            .get_membership(msg.channel_id, username)
            .await?;
        if mem.is_none() {
            return Err(MegaError::NotFound(format!(
                "Message {} not found",
                message_public_id
            )));
        }

        let public_id = crate::callisto::entity_ext::generate_public_id();
        let att = self
            .attachment_storage
            .create_attachment(
                public_id,
                file_path,
                file_type,
                "Message".to_string(),
                msg.id,
                name,
                size,
                position,
            )
            .await?;

        Ok(att)
    }

    /// Fetch an Open Graph preview for `url` and cache it in
    /// `open_graph_links`. If a fresh cached entry exists, return it without
    /// making a network request. If fetching is disabled by configuration,
    /// return any existing cached entry (stale or not) or `None`.
    pub async fn fetch_or_refresh_open_graph_link(
        &self,
        url: &str,
        fetch_enabled: bool,
        timeout_ms: u64,
    ) -> Result<Option<open_graph_link::Model>, MegaError> {
        let cached = self
            .open_graph_storage
            .get_open_graph_link_by_url(url)
            .await?;
        let now = Utc::now().naive_utc();
        let is_fresh = cached.as_ref().is_some_and(|c| {
            let age = now.signed_duration_since(c.updated_at);
            age < OPEN_GRAPH_CACHE_TTL
        });
        if is_fresh {
            return Ok(cached);
        }
        if !fetch_enabled {
            return Ok(cached);
        }

        let fetched = match fetch_open_graph(url, timeout_ms).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "failed to fetch open graph link");
                return Ok(cached);
            }
        };

        let title = fetched.title.unwrap_or_default();
        let image_path = fetched.image.and_then(|h| resolve_url(url, &h));
        let favicon_path = fetched.favicon.and_then(|h| resolve_url(url, &h));
        let model = self
            .open_graph_storage
            .upsert_open_graph_link(url.to_string(), title, image_path, favicon_path)
            .await?;
        Ok(Some(model))
    }
}

const OPEN_GRAPH_CACHE_TTL: chrono::Duration = chrono::Duration::hours(24);

struct FetchedOpenGraph {
    title: Option<String>,
    image: Option<String>,
    favicon: Option<String>,
}

async fn fetch_open_graph(url: &str, timeout_ms: u64) -> Result<FetchedOpenGraph, MegaError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .user_agent("monoengine-open-graph/1.0")
        .build()
        .map_err(|e| MegaError::Other(format!("failed to build http client: {e}")))?;

    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| MegaError::Other(format!("open graph fetch failed: {e}")))?
        .text()
        .await
        .map_err(|e| MegaError::Other(format!("open graph fetch body read failed: {e}")))?;

    // Avoid parsing multi-megabyte pages for a handful of meta tags.
    let body: String = body.chars().take(1_000_000).collect();

    let title = extract_meta_property(&body, "og:title").or_else(|| extract_title_tag(&body));
    let image = extract_meta_property(&body, "og:image");
    let favicon = extract_link_rel(&body, "icon");

    Ok(FetchedOpenGraph {
        title,
        image,
        favicon,
    })
}

fn extract_meta_property(html: &str, property: &str) -> Option<String> {
    for caps in META_PROPERTY_FIRST.captures_iter(html) {
        if caps[1].eq_ignore_ascii_case(property) {
            return Some(caps[2].to_string());
        }
    }
    for caps in META_CONTENT_FIRST.captures_iter(html) {
        if caps[2].eq_ignore_ascii_case(property) {
            return Some(caps[1].to_string());
        }
    }
    None
}

fn extract_link_rel(html: &str, rel: &str) -> Option<String> {
    for caps in LINK_REL_FIRST.captures_iter(html) {
        if caps[1].to_lowercase().contains(rel) {
            return Some(caps[2].to_string());
        }
    }
    for caps in LINK_HREF_FIRST.captures_iter(html) {
        if caps[2].to_lowercase().contains(rel) {
            return Some(caps[1].to_string());
        }
    }
    None
}

fn extract_title_tag(html: &str) -> Option<String> {
    TITLE_TAG
        .captures(html)
        .map(|caps| caps[1].trim().to_string())
}

fn resolve_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let base_url = reqwest::Url::parse(base).ok()?;
    base_url.join(href).ok().map(|u| u.to_string())
}

static META_PROPERTY_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<meta\s+[^>]*property=["']([^"']+)["'][^>]*content=["']([^"']*)["'][^>]*>"#)
        .unwrap()
});
static META_CONTENT_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<meta\s+[^>]*content=["']([^"']*)["'][^>]*property=["']([^"']+)["'][^>]*>"#)
        .unwrap()
});
static LINK_REL_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<link\s+[^>]*rel=["']([^"']+)["'][^>]*href=["']([^"']*)["'][^>]*>"#).unwrap()
});
static LINK_HREF_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<link\s+[^>]*href=["']([^"']*)["'][^>]*rel=["']([^"']+)["'][^>]*>"#).unwrap()
});
static TITLE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<title>([^<]*)</title>"#).unwrap());

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{Router, response::Html, routing::get};
    use tokio::net::TcpListener;

    use super::*;
    use crate::{chat::service::channel_chat::ChannelChatService, jupiter::tests::test_storage};

    #[tokio::test]
    async fn delete_reaction_rejects_removed_member() {
        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;

        let channel_svc = ChannelChatService::from_storage(&storage);
        let shared_svc = SharedChatService::from_storage(&storage);

        // alice creates a channel with bob as a member and sends a message.
        let (ch, first_msg) = channel_svc
            .create_channel(
                Some("Reaction Test".to_string()),
                None,
                "alice".to_string(),
                vec!["bob".to_string()],
                true,
                Some("hello".to_string()),
                None,
            )
            .await
            .expect("create channel");
        let msg = first_msg.expect("initial message");

        // bob reacts to alice's message.
        let reaction = shared_svc
            .create_reaction(
                &msg.public_id,
                "bob".to_string(),
                Some("+1".to_string()),
                None,
            )
            .await
            .expect("create reaction");

        // alice removes bob from the channel.
        channel_svc
            .remove_members(&ch.public_id, "alice", vec!["bob".to_string()])
            .await
            .expect("remove bob");

        // bob tries to delete his reaction — must be rejected (NotFound).
        let result = shared_svc.delete_reaction(&reaction.public_id, "bob").await;
        assert!(
            matches!(result, Err(MegaError::NotFound(_))),
            "removed member should not be able to delete reaction"
        );

        // alice (still a member) cannot delete bob's reaction (not owner).
        let result = shared_svc
            .delete_reaction(&reaction.public_id, "alice")
            .await;
        assert!(
            matches!(result, Err(MegaError::Other(_))),
            "non-owner member should not be able to delete reaction"
        );
    }

    async fn start_og_server(body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let app = Router::new().route("/", get(move || async move { Html(body) }));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve failed");
        });
        addr
    }

    #[tokio::test]
    async fn fetch_open_graph_link_parses_and_caches_preview() {
        let body = r#"<!doctype html>
<html>
<head>
<title>Page Title</title>
<meta property="og:title" content="OG Title">
<meta property="og:image" content="/image.png">
<link rel="icon" href="/favicon.ico">
</head>
<body>hi</body>
</html>"#;
        let addr = start_og_server(body).await;
        let url = format!("http://{}/", addr);

        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let shared_svc = SharedChatService::from_storage(&storage);

        let preview = shared_svc
            .fetch_or_refresh_open_graph_link(&url, true, 5000)
            .await
            .expect("fetch should succeed")
            .expect("preview should be returned");
        assert_eq!(preview.title, "OG Title");
        assert_eq!(
            preview.image_path,
            Some(format!("http://{}/image.png", addr))
        );
        assert_eq!(
            preview.favicon_path,
            Some(format!("http://{}/favicon.ico", addr))
        );

        // A second fetch should return the cached row without fetching again.
        let cached = shared_svc
            .fetch_or_refresh_open_graph_link(&url, true, 5000)
            .await
            .expect("cached fetch should succeed")
            .expect("cached preview should exist");
        assert_eq!(cached.id, preview.id);
    }

    #[tokio::test]
    async fn fetch_open_graph_disabled_returns_cached_or_none() {
        let body = r#"<html><head><title>T</title></head><body></body></html>"#;
        let addr = start_og_server(body).await;
        let url = format!("http://{}/", addr);

        let temp = tempfile::tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let shared_svc = SharedChatService::from_storage(&storage);

        let none = shared_svc
            .fetch_or_refresh_open_graph_link(&url, false, 5000)
            .await
            .expect("fetch should succeed");
        assert!(none.is_none());

        // Pre-seed the cache and confirm disabled mode still returns it.
        shared_svc
            .open_graph_storage
            .upsert_open_graph_link(url.clone(), "Cached".to_string(), None, None)
            .await
            .expect("upsert");
        let cached = shared_svc
            .fetch_or_refresh_open_graph_link(&url, false, 5000)
            .await
            .expect("fetch should succeed")
            .expect("cached entry should be returned");
        assert_eq!(cached.title, "Cached");
    }
}
