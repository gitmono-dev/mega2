use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    common::errors::MegaError,
    jupiter::storage::{
        cl_reviewer_storage::ClReviewerStorage, cl_storage::ClStorage, issue_storage::IssueStorage,
        notification_storage::NotificationStorage,
    },
};

pub const EVENT_CL_COMMENT_CREATED: &str = "cl.comment.created";
pub const EVENT_CL_MERGED: &str = "cl.merged";
pub const EVENT_ISSUE_COMMENT_CREATED: &str = "issue.comment.created";
pub const EVENT_ISSUE_CLOSED: &str = "issue.closed";
pub const EVENT_ITEM_REFERENCED: &str = "item.referenced";

const COMMENT_EXCERPT_MAX_CHARS: usize = 500;

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
    let trimmed = text.trim();
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
                "cl_title": cl.title,
            }),
        )
        .await?;
    }
    Ok(())
}

/// Trigger: a new comment is created on an Issue.
pub async fn on_issue_comment_created(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    issue_link: &str,
    comment_text: &str,
) -> Result<(), MegaError> {
    ensure_event_type(
        notif_stg,
        EVENT_ISSUE_COMMENT_CREATED,
        "issue",
        "New comment on an Issue",
    )
    .await?;
    let issue = issue_stg
        .get_issue(issue_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("Issue {issue_link} not found")))?;
    if issue.author != actor_username {
        let excerpt = comment_excerpt(comment_text);
        deliver_event(
            notif_stg,
            &issue.author,
            EVENT_ISSUE_COMMENT_CREATED,
            &format!("New comment on issue {}", issue.title),
            &format!("{actor_username} commented: {comment_text}"),
            json!({
                "issue_link": issue_link,
                "issue_title": issue.title,
                "actor_username": actor_username,
                "comment_excerpt": excerpt,
            }),
        )
        .await?;
    }
    Ok(())
}

/// Trigger: an issue was closed.
pub async fn on_issue_closed(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    issue_link: &str,
) -> Result<(), MegaError> {
    ensure_event_type(notif_stg, EVENT_ISSUE_CLOSED, "issue", "Issue was closed").await?;
    let issue = issue_stg
        .get_issue(issue_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("Issue {issue_link} not found")))?;
    if issue.author != actor_username {
        deliver_event(
            notif_stg,
            &issue.author,
            EVENT_ISSUE_CLOSED,
            &format!("Issue {} was closed", issue.title),
            &format!("{actor_username} closed {issue_link}."),
            json!({
                "issue_link": issue_link,
                "issue_title": issue.title,
                "actor_username": actor_username,
            }),
        )
        .await?;
    }
    Ok(())
}

/// Trigger: an item (CL or Issue) is referenced from a comment.
pub async fn on_item_referenced(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    source_link: &str,
    referenced_link: &str,
) -> Result<(), MegaError> {
    let author = if let Some(cl) = cl_stg.get_cl(referenced_link).await? {
        cl.username
    } else if let Some(issue) = issue_stg.get_issue(referenced_link).await? {
        issue.author
    } else {
        return Ok(());
    };
    if author == actor_username {
        return Ok(());
    }

    ensure_event_type(
        notif_stg,
        EVENT_ITEM_REFERENCED,
        "reference",
        "Your CL or Issue was referenced (mentioned)",
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
        }
        .insert(&db)
        .await
        .unwrap();
        notif
            .upsert_user_settings("alice", "alice@example.test")
            .await
            .unwrap();
        notif.set_delivery_mode("alice", "email").await.unwrap();

        on_cl_comment_created(&notif, &cl_stg, &reviewer_stg, "bob", "CL1", "<review>")
            .await
            .unwrap();

        let inbox = notif.list_inbox_notifications("alice", 10).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].event_type_code, EVENT_CL_COMMENT_CREATED);
        assert!(inbox[0].body_html.contains("&lt;review&gt;"));
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
        notif
            .upsert_user_settings("alice", "alice@example.test")
            .await
            .unwrap();
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
