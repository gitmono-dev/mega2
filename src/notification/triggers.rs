use std::collections::HashSet;

use crate::{
    common::errors::MegaError,
    jupiter::storage::{
        cl_reviewer_storage::ClReviewerStorage, cl_storage::ClStorage,
        notification_storage::NotificationStorage,
    },
    mail::template::{
        DEFAULT_MAIL_LOCALE, LocalizedMailTemplate, MailTemplate, MailTemplateKey,
        MailTemplateRegistry,
    },
};
pub const EVENT_CL_COMMENT_CREATED: &str = "cl.comment.created";

const CL_COMMENT_CREATED_MAIL_TEMPLATE_KEY: MailTemplateKey =
    MailTemplateKey::new(EVENT_CL_COMMENT_CREATED);

const CL_COMMENT_CREATED_MAIL_TEMPLATE: MailTemplate<'static> = MailTemplate::new(
    "New comment on CL {{cl_link}}",
    "<p><b>{{actor_username}}</b> commented on <b>{{cl_link}}</b>:</p><p>{{comment_text}}</p>",
    Some("{{actor_username}} commented on {{cl_link}}: {{comment_text}}"),
);

const CL_COMMENT_CREATED_MAIL_TEMPLATE_ZH_CN: MailTemplate<'static> = MailTemplate::new(
    "CL {{cl_link}} 有新评论",
    "<p><b>{{actor_username}}</b> 评论了 <b>{{cl_link}}</b>：</p><p>{{comment_text}}</p>",
    Some("{{actor_username}} 评论了 {{cl_link}}：{{comment_text}}"),
);

const NOTIFICATION_MAIL_TEMPLATE_REGISTRY: MailTemplateRegistry<'static> =
    MailTemplateRegistry::new(
        DEFAULT_MAIL_LOCALE,
        &[
            LocalizedMailTemplate::new(
                CL_COMMENT_CREATED_MAIL_TEMPLATE_KEY,
                DEFAULT_MAIL_LOCALE,
                CL_COMMENT_CREATED_MAIL_TEMPLATE,
            ),
            LocalizedMailTemplate::new(
                CL_COMMENT_CREATED_MAIL_TEMPLATE_KEY,
                "zh-CN",
                CL_COMMENT_CREATED_MAIL_TEMPLATE_ZH_CN,
            ),
        ],
    );

/// Ensure the core event types exist in DB
///
/// currently does not seed event types in migrations
/// upsert the event type at first use.
async fn ensure_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(
        EVENT_CL_COMMENT_CREATED,
        "cl",
        "New comment on a Change List",
        false,
        true,
    )
    .await?;

    Ok(())
}

/// Trigger: a new comment is created on a Change List
///
/// Behavior:
/// - recipients: CL author + all reviewers
/// - exclude actor
/// - respect user preferences via `should_send`
/// - enqueue email job (outbox) and let the background dispatcher deliver it
pub async fn on_cl_comment_created(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    reviewer_stg: &ClReviewerStorage,
    actor_username: &str,
    cl_link: &str,
    comment_text: &str,
) -> Result<(), MegaError> {
    ensure_event_type_exists(notif_stg).await?;

    let cl: crate::callisto::mega_cl::Model = cl_stg
        .get_cl(cl_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("CL {cl_link} not found")))?;

    let reviewers = reviewer_stg.list_reviewers(cl_link).await?;

    let mut recipients: HashSet<String> = HashSet::new();
    recipients.insert(cl.username);
    for r in reviewers {
        recipients.insert(r.username);
    }
    recipients.remove(actor_username);

    for username in recipients {
        // should_send returns false if user settings are missing or globally disabled
        if !notif_stg
            .should_send(&username, EVENT_CL_COMMENT_CREATED)
            .await?
        {
            continue;
        }

        let settings = match notif_stg.get_user_settings(&username).await? {
            Some(s) => s,
            None => continue,
        };

        let mail = NOTIFICATION_MAIL_TEMPLATE_REGISTRY.render(
            CL_COMMENT_CREATED_MAIL_TEMPLATE_KEY,
            settings.preferred_locale.as_deref(),
            &[
                ("actor_username", actor_username),
                ("cl_link", cl_link),
                ("comment_text", comment_text),
            ],
        )?;

        notif_stg
            .enqueue_email_job(
                &username,
                &settings.email,
                EVENT_CL_COMMENT_CREATED,
                &mail.subject,
                &mail.html,
                mail.text.as_deref(),
            )
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{email_jobs, mega_cl, mega_cl_reviewer},
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    #[tokio::test]
    async fn test_on_cl_comment_created_enqueues_jobs_for_author_and_reviewers() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let reviewer_stg = ClReviewerStorage { base: base.clone() };

        // Create CL (author = alice)
        let now = chrono::Utc::now().naive_utc();
        mega_cl::ActiveModel {
            id: Set(1),
            link: Set("CL1".to_string()),
            title: Set("t".to_string()),
            merge_date: Set(None),
            status: Set(crate::callisto::sea_orm_active_enums::MergeStatusEnum::Open),
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

        // Create reviewer bob
        mega_cl_reviewer::ActiveModel {
            id: Set(1),
            cl_link: Set("CL1".to_string()),
            username: Set("bob".to_string()),
            approved: Set(false),
            system_required: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        // Only notify users who have settings rows
        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();
        notif
            .upsert_user_settings("bob", "bob@example.com")
            .await
            .unwrap();
        notif
            .set_preferred_locale("bob", Some("zh-CN"))
            .await
            .unwrap();
        notif
            .upsert_user_settings("carol", "carol@example.com")
            .await
            .unwrap();

        // SUppose the actor is carol, should notify alice and bob but not carol
        on_cl_comment_created(&notif, &cl_stg, &reviewer_stg, "carol", "CL1", "hello")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 2);

        let alice_job = email_jobs::Entity::find()
            .filter(email_jobs::Column::Username.eq("alice"))
            .one(&db)
            .await
            .unwrap();
        let alice_job = alice_job.unwrap();
        assert_eq!(alice_job.subject, "New comment on CL CL1");

        let bob_job = email_jobs::Entity::find()
            .filter(email_jobs::Column::Username.eq("bob"))
            .one(&db)
            .await
            .unwrap();
        let bob_job = bob_job.unwrap();
        assert_eq!(bob_job.subject, "CL CL1 有新评论");
        assert_eq!(
            bob_job.body_text.as_deref(),
            Some("carol 评论了 CL1：hello")
        );
    }

    #[tokio::test]
    async fn test_on_cl_comment_created_renders_mail_template_with_html_escaping() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let reviewer_stg = ClReviewerStorage { base: base.clone() };
        let now = chrono::Utc::now().naive_utc();

        mega_cl::ActiveModel {
            id: Set(1),
            link: Set("CL-template".to_string()),
            title: Set("t".to_string()),
            merge_date: Set(None),
            status: Set(crate::callisto::sea_orm_active_enums::MergeStatusEnum::Open),
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
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        on_cl_comment_created(
            &notif,
            &cl_stg,
            &reviewer_stg,
            "bob",
            "CL-template",
            r#"<script>alert("x")</script> & done"#,
        )
        .await
        .unwrap();

        let job = email_jobs::Entity::find()
            .filter(email_jobs::Column::Username.eq("alice"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(job.subject, "New comment on CL CL-template");
        assert!(
            job.body_html
                .contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; done")
        );
        assert!(!job.body_html.contains(r#"<script>alert("x")</script>"#));
        assert_eq!(
            job.body_text.as_deref(),
            Some(r#"bob commented on CL-template: <script>alert("x")</script> & done"#)
        );
    }

    #[tokio::test]
    async fn test_on_cl_comment_created_respects_should_send() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let reviewer_stg = ClReviewerStorage { base: base.clone() };
        let now = chrono::Utc::now().naive_utc();

        mega_cl::ActiveModel {
            id: Set(1),
            link: Set("CL2".to_string()),
            title: Set("t".to_string()),
            merge_date: Set(None),
            status: Set(crate::callisto::sea_orm_active_enums::MergeStatusEnum::Open),
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
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();
        // disable globally
        notif.set_global_enabled("alice", false).await.unwrap();

        on_cl_comment_created(&notif, &cl_stg, &reviewer_stg, "bob", "CL2", "hello")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 0);
    }
}
