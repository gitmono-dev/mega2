use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    common::errors::MegaError,
    jupiter::storage::{
        cl_reviewer_storage::ClReviewerStorage, cl_storage::ClStorage,
        notification_storage::NotificationStorage,
    },
};

pub const EVENT_CL_COMMENT_CREATED: &str = "cl.comment.created";
pub const EVENT_CL_MERGED: &str = "cl.merged";
pub const EVENT_ITEM_REFERENCED: &str = "item.referenced";

const COMMENT_EXCERPT_MAX_CHARS: usize = 500;

/// Stand-ins for business values that are legitimately blank.
///
/// The website product templates reject any required payload field that is
/// blank after `trim()` with `422 invalid_payload`
/// (`docs/refactoring/website-mail.md` §2.1 field rules). A whitespace-only
/// comment body, or a CL/Issue saved with an empty title, would therefore make
/// mega2 emit a request that can only ever be rejected — and because
/// `service::deliver_user_notification` only `warn!`s on failure, the email
/// would be dropped silently while the in-app notification still appeared.
/// Substituting a placeholder keeps the recipient informed that the event
/// happened, minus an excerpt that carried no information anyway.
const EMPTY_COMMENT_PLACEHOLDER: &str = "(no comment text)";
const EMPTY_TITLE_PLACEHOLDER: &str = "(untitled)";

/// Trim using the *website's* notion of whitespace, not Rust's.
///
/// `str::trim` follows Unicode `White_Space`, which excludes U+FEFF; JavaScript's
/// `String.prototype.trim` strips it. A value that is nothing but a BOM
/// therefore looks non-blank here and blank there, so it would sail past the
/// guards below and still be rejected `422 invalid_payload` — reopening exactly
/// the silent-drop hole those guards exist to close.
fn trim_like_javascript(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// Return `value` trimmed, or `placeholder` when it is blank.
///
/// Only used for payload fields the website marks required; optional fields
/// keep their real (possibly empty) value.
fn required_field(value: &str, placeholder: &'static str) -> String {
    let trimmed = trim_like_javascript(value);
    if trimmed.is_empty() {
        placeholder.to_owned()
    } else {
        trimmed.to_owned()
    }
}

async fn ensure_event_type(
    stg: &NotificationStorage,
    code: &str,
    category: &str,
    description: &str,
) -> Result<(), MegaError> {
    stg.upsert_event_type(code, category, description, false, true)
        .await?;
    Ok(())
}

fn comment_excerpt(text: &str) -> String {
    let trimmed = trim_like_javascript(text);
    if trimmed.is_empty() {
        return EMPTY_COMMENT_PLACEHOLDER.to_owned();
    }
    let mut excerpt = trimmed
        .chars()
        .take(COMMENT_EXCERPT_MAX_CHARS)
        .collect::<String>();
    if trimmed.chars().count() > COMMENT_EXCERPT_MAX_CHARS {
        excerpt.push('…');
    }
    excerpt
}

async fn deliver_event(
    stg: &NotificationStorage,
    username: &str,
    event_type: &str,
    subject: &str,
    body_text: &str,
    website_payload: Value,
) -> Result<(), MegaError> {
    crate::notification::service::deliver_user_notification(
        stg,
        username,
        event_type,
        subject,
        body_text,
        website_payload,
    )
    .await
}

/// Trigger: a new comment is created on a Change List.
pub async fn on_cl_comment_created(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    reviewer_stg: &ClReviewerStorage,
    actor_username: &str,
    cl_link: &str,
    comment_text: &str,
) -> Result<(), MegaError> {
    ensure_event_type(
        notif_stg,
        EVENT_CL_COMMENT_CREATED,
        "cl",
        "New comment on a Change List",
    )
    .await?;
    let cl = cl_stg
        .get_cl(cl_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("CL {cl_link} not found")))?;
    let mut recipients = HashSet::from([cl.username]);
    recipients.extend(
        reviewer_stg
            .list_reviewers(cl_link)
            .await?
            .into_iter()
            .map(|reviewer| reviewer.username),
    );
    recipients.remove(actor_username);

    let excerpt = comment_excerpt(comment_text);
    let payload = json!({
        "cl_link": cl_link,
        "actor_username": actor_username,
        "comment_excerpt": excerpt,
    });

    for username in recipients {
        deliver_event(
            notif_stg,
            &username,
            EVENT_CL_COMMENT_CREATED,
            &format!("New comment on CL {cl_link}"),
            &format!("{actor_username} commented: {comment_text}"),
            payload.clone(),
        )
        .await?;
    }
    Ok(())
}

/// Trigger: a Change List is merged.
pub async fn on_cl_merged(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    actor_username: &str,
    cl_link: &str,
) -> Result<(), MegaError> {
    ensure_event_type(notif_stg, EVENT_CL_MERGED, "cl", "Change List was merged").await?;
    let cl = cl_stg
        .get_cl(cl_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("CL {cl_link} not found")))?;
    if cl.username != actor_username {
        deliver_event(
            notif_stg,
            &cl.username,
            EVENT_CL_MERGED,
            &format!("CL {cl_link} was merged"),
            &format!("{actor_username} merged {}.", cl.title),
            json!({
                "cl_link": cl_link,
                "actor_username": actor_username,
                "cl_title": required_field(&cl.title, EMPTY_TITLE_PLACEHOLDER),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Trigger: an item (CL) is referenced from a comment.
pub async fn on_item_referenced(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    actor_username: &str,
    source_link: &str,
    referenced_link: &str,
) -> Result<(), MegaError> {
    let Some(cl) = cl_stg.get_cl(referenced_link).await? else {
        return Ok(());
    };
    let author = cl.username;
    if author == actor_username {
        return Ok(());
    }

    ensure_event_type(
        notif_stg,
        EVENT_ITEM_REFERENCED,
        "reference",
        "Your CL was referenced (mentioned)",
    )
    .await?;
    deliver_event(
        notif_stg,
        &author,
        EVENT_ITEM_REFERENCED,
        &format!("{referenced_link} was referenced"),
        &format!("{actor_username} referenced it in {source_link}."),
        json!({
            "referenced_link": referenced_link,
            "source_link": source_link,
            "actor_username": actor_username,
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::{ActiveModelTrait, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{mega_cl, sea_orm_active_enums::MergeStatusEnum},
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    #[tokio::test]
    async fn email_delivery_mode_writes_in_app_notification() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let reviewer_stg = ClReviewerStorage { base };
        let now = chrono::Utc::now().naive_utc();
        mega_cl::ActiveModel {
            id: Set(1),
            link: Set("CL1".to_string()),
            title: Set("Example".to_string()),
            merge_date: Set(None),
            status: Set(MergeStatusEnum::Open),
            path: Set("/".to_string()),
            from_hash: Set("a".to_string()),
            to_hash: Set("b".to_string()),
            created_at: Set(now),
            updated_at: Set(now),
            username: Set("alice".to_string()),
            base_branch: Set("main".to_string()),
            revision: Set(0),
        }
        .insert(&db)
        .await
        .unwrap();
        notif.upsert_user_settings("alice").await.unwrap();

        on_cl_comment_created(&notif, &cl_stg, &reviewer_stg, "bob", "CL1", "<review>")
            .await
            .unwrap();

        let inbox = notif.list_inbox_notifications("alice", 10).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].event_type_code, EVENT_CL_COMMENT_CREATED);
        assert!(inbox[0].body_html.contains("&lt;review&gt;"));
    }

    #[test]
    fn comment_excerpt_substitutes_a_placeholder_for_blank_input() {
        // website rejects a blank required payload field with 422
        // invalid_payload, and service.rs only warns — so a whitespace-only
        // comment would silently drop the email while the in-app notification
        // still appeared. See docs/refactoring/website-mail.md §2.1.
        for blank in ["", "   ", "\n\t "] {
            let excerpt = comment_excerpt(blank);
            assert_eq!(excerpt, EMPTY_COMMENT_PLACEHOLDER);
            assert!(!excerpt.trim().is_empty());
        }
    }

    #[test]
    fn comment_excerpt_still_trims_and_truncates_real_input() {
        assert_eq!(comment_excerpt("  hello  "), "hello");
        let long = "x".repeat(COMMENT_EXCERPT_MAX_CHARS + 10);
        let excerpt = comment_excerpt(&long);
        assert!(excerpt.ends_with('…'));
        assert_eq!(excerpt.chars().count(), COMMENT_EXCERPT_MAX_CHARS + 1);
    }

    #[test]
    fn blank_detection_matches_the_websites_javascript_trim() {
        // U+FEFF is not Unicode White_Space, so `str::trim` keeps it while the
        // website's `String.prototype.trim` strips it. A BOM-only comment (easy
        // to produce by pasting from a BOM-prefixed file) would otherwise reach
        // the website as a "non-blank" required field and come back 422.
        assert_eq!(comment_excerpt("\u{FEFF}"), EMPTY_COMMENT_PLACEHOLDER);
        assert_eq!(comment_excerpt(" \u{FEFF}\t"), EMPTY_COMMENT_PLACEHOLDER);
        assert_eq!(
            required_field("\u{FEFF}", EMPTY_TITLE_PLACEHOLDER),
            EMPTY_TITLE_PLACEHOLDER
        );
        // A BOM must still be stripped from the edges of real content, again
        // matching the website (which sees the trimmed value as non-blank).
        assert_eq!(comment_excerpt("\u{FEFF}hello\u{FEFF}"), "hello");
    }

    #[test]
    fn required_field_substitutes_only_for_blank_values() {
        assert_eq!(
            required_field("   ", EMPTY_TITLE_PLACEHOLDER),
            EMPTY_TITLE_PLACEHOLDER
        );
        assert_eq!(
            required_field("", EMPTY_TITLE_PLACEHOLDER),
            EMPTY_TITLE_PLACEHOLDER
        );
        assert_eq!(
            required_field("  Real title  ", EMPTY_TITLE_PLACEHOLDER),
            "Real title"
        );
    }

    #[tokio::test]
    async fn disabled_preference_skips_in_app_notification() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let notif = NotificationStorage::new(Arc::new(db));
        ensure_event_type(
            &notif,
            EVENT_CL_COMMENT_CREATED,
            "cl",
            "New comment on a Change List",
        )
        .await
        .unwrap();
        notif.upsert_user_settings("alice").await.unwrap();
        notif
            .set_user_preference("alice", EVENT_CL_COMMENT_CREATED, false)
            .await
            .unwrap();

        deliver_event(
            &notif,
            "alice",
            EVENT_CL_COMMENT_CREATED,
            "New comment",
            "body",
            json!({ "cl_link": "CL1", "actor_username": "bob", "comment_excerpt": "body" }),
        )
        .await
        .unwrap();

        assert!(
            notif
                .list_inbox_notifications("alice", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
