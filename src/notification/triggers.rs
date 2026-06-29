use std::{
    collections::HashSet,
    sync::{LazyLock, RwLock},
};

use crate::{
    common::errors::MegaError,
    config::{
        Config, MailConfig,
        reload::{ConfigReloadReport, ConfigReloadSubscriber},
    },
    jupiter::storage::{
        cl_reviewer_storage::ClReviewerStorage, cl_storage::ClStorage, issue_storage::IssueStorage,
        notification_storage::NotificationStorage,
    },
    mail::template::{
        DEFAULT_MAIL_LOCALE, LocalizedMailTemplate, MailTemplate, MailTemplateKey,
        MailTemplateRegistry, load_localized_templates_from_dir,
    },
};
pub const EVENT_CL_COMMENT_CREATED: &str = "cl.comment.created";
pub const EVENT_CL_MERGED: &str = "cl.merged";
pub const EVENT_ISSUE_COMMENT_CREATED: &str = "issue.comment.created";
pub const EVENT_ISSUE_CLOSED: &str = "issue.closed";
pub const EVENT_ITEM_REFERENCED: &str = "item.referenced";
pub const EVENT_CHAT_MENTION_CREATED: &str = "chat.mention.created";
pub const EVENT_CHAT_REPLY_CREATED: &str = "chat.reply.created";

static NOTIFICATION_MAIL_TEMPLATE_REGISTRY: LazyLock<RwLock<MailTemplateRegistry>> =
    LazyLock::new(|| RwLock::new(default_notification_mail_template_registry()));

fn cl_comment_created_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_CL_COMMENT_CREATED)
}

fn cl_merged_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_CL_MERGED)
}

fn issue_comment_created_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_ISSUE_COMMENT_CREATED)
}

fn issue_closed_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_ISSUE_CLOSED)
}

fn item_referenced_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_ITEM_REFERENCED)
}

fn chat_mention_created_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_CHAT_MENTION_CREATED)
}

fn chat_reply_created_mail_template_key() -> MailTemplateKey {
    MailTemplateKey::new(EVENT_CHAT_REPLY_CREATED)
}

pub fn default_notification_mail_template_registry() -> MailTemplateRegistry {
    notification_mail_template_registry_with_default_locale(DEFAULT_MAIL_LOCALE)
}

pub fn notification_mail_template_registry_from_config(
    mail_config: &MailConfig,
) -> Result<MailTemplateRegistry, MegaError> {
    let mut registry = notification_mail_template_registry_with_default_locale(
        &mail_config.template_default_locale,
    );
    if let Some(template_dir) = &mail_config.template_dir {
        registry.append_templates(load_localized_templates_from_dir(template_dir)?);
    }

    Ok(registry)
}

pub fn configure_notification_mail_template_registry(
    registry: MailTemplateRegistry,
) -> Result<(), MegaError> {
    let mut configured = NOTIFICATION_MAIL_TEMPLATE_REGISTRY.write().map_err(|_| {
        MegaError::Other("notification mail template registry lock is poisoned".to_string())
    })?;
    *configured = registry;

    Ok(())
}

/// Config-reload subscriber that hot-swaps the notification mail template
/// registry when `mail.template_dir` / `mail.template_default_locale` change
/// (docs/mail.md phase 4).
///
/// The rebuild reads TOML templates from disk only — no vault is involved — so
/// it is safe in the synchronous reload pipeline. It is fail-closed: if the new
/// `template_dir` cannot be loaded, the apply returns an error and the reload
/// machinery rolls back (rebuilding from the previous config), leaving the
/// previous registry intact. apply and rollback share one rebuild path: apply
/// runs against the new config, rollback against the previous one.
pub fn config_reload_mail_template_subscriber() -> ConfigReloadSubscriber {
    ConfigReloadSubscriber::new(
        "mail_template_registry",
        apply_mail_template_registry_reload,
        apply_mail_template_registry_reload,
    )
}

fn apply_mail_template_registry_reload(
    config: &Config,
    report: &ConfigReloadReport,
) -> Result<(), MegaError> {
    let template_changed = report
        .applied_fields
        .iter()
        .any(|field| matches!(*field, "mail.template_dir" | "mail.template_default_locale"));
    if !template_changed {
        return Ok(());
    }

    if let Some(mail) = &config.mail {
        let registry = notification_mail_template_registry_from_config(mail)?;
        configure_notification_mail_template_registry(registry)?;
    }

    Ok(())
}

fn current_notification_mail_template_registry() -> Result<MailTemplateRegistry, MegaError> {
    let configured = NOTIFICATION_MAIL_TEMPLATE_REGISTRY.read().map_err(|_| {
        MegaError::Other("notification mail template registry lock is poisoned".to_string())
    })?;

    Ok(configured.clone())
}

fn notification_mail_template_registry_with_default_locale(
    default_locale: &str,
) -> MailTemplateRegistry {
    let key = cl_comment_created_mail_template_key();
    let merged_key = cl_merged_mail_template_key();
    let issue_key = issue_comment_created_mail_template_key();
    let issue_closed_key = issue_closed_mail_template_key();
    let reference_key = item_referenced_mail_template_key();
    let chat_mention_key = chat_mention_created_mail_template_key();
    let chat_reply_key = chat_reply_created_mail_template_key();
    MailTemplateRegistry::new(
        default_locale,
        vec![
            LocalizedMailTemplate::new(
                key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "New comment on CL {{cl_link}}",
                    "<p><b>{{actor_username}}</b> commented on <b>{{cl_link}}</b>:</p><p>{{comment_text}}</p>",
                    Some("{{actor_username}} commented on {{cl_link}}: {{comment_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                key,
                "zh-CN",
                MailTemplate::new(
                    "CL {{cl_link}} 有新评论",
                    "<p><b>{{actor_username}}</b> 评论了 <b>{{cl_link}}</b>：</p><p>{{comment_text}}</p>",
                    Some("{{actor_username}} 评论了 {{cl_link}}：{{comment_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                merged_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "CL {{cl_link}} was merged",
                    "<p><b>{{actor_username}}</b> merged <b>{{cl_link}}</b> ({{cl_title}}).</p>",
                    Some("{{actor_username}} merged {{cl_link}} ({{cl_title}})"),
                ),
            ),
            LocalizedMailTemplate::new(
                merged_key,
                "zh-CN",
                MailTemplate::new(
                    "CL {{cl_link}} 已合并",
                    "<p><b>{{actor_username}}</b> 合并了 <b>{{cl_link}}</b>（{{cl_title}}）。</p>",
                    Some("{{actor_username}} 合并了 {{cl_link}}（{{cl_title}}）"),
                ),
            ),
            LocalizedMailTemplate::new(
                issue_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "New comment on issue {{issue_title}}",
                    "<p><b>{{actor_username}}</b> commented on issue <b>{{issue_title}}</b>:</p><p>{{comment_text}}</p>",
                    Some("{{actor_username}} commented on issue {{issue_title}}: {{comment_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                issue_key,
                "zh-CN",
                MailTemplate::new(
                    "议题 {{issue_title}} 有新评论",
                    "<p><b>{{actor_username}}</b> 评论了议题 <b>{{issue_title}}</b>：</p><p>{{comment_text}}</p>",
                    Some("{{actor_username}} 评论了议题 {{issue_title}}：{{comment_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                issue_closed_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "Issue {{issue_title}} was closed",
                    "<p><b>{{actor_username}}</b> closed issue <b>{{issue_title}}</b>.</p>",
                    Some("{{actor_username}} closed issue {{issue_title}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                issue_closed_key,
                "zh-CN",
                MailTemplate::new(
                    "议题 {{issue_title}} 已关闭",
                    "<p><b>{{actor_username}}</b> 关闭了议题 <b>{{issue_title}}</b>。</p>",
                    Some("{{actor_username}} 关闭了议题 {{issue_title}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                reference_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "{{referenced_link}} was referenced",
                    "<p><b>{{actor_username}}</b> referenced <b>{{referenced_link}}</b> in {{source_link}}.</p>",
                    Some("{{actor_username}} referenced {{referenced_link}} in {{source_link}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                reference_key,
                "zh-CN",
                MailTemplate::new(
                    "{{referenced_link}} 被引用",
                    "<p><b>{{actor_username}}</b> 在 {{source_link}} 中引用了 <b>{{referenced_link}}</b>。</p>",
                    Some("{{actor_username}} 在 {{source_link}} 中引用了 {{referenced_link}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                chat_mention_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "{{actor_username}} mentioned you in {{channel_name}}",
                    "<p><b>{{actor_username}}</b> mentioned you in <b>{{channel_name}}</b>:</p><p>{{message_text}}</p>",
                    Some("{{actor_username}} mentioned you in {{channel_name}}: {{message_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                chat_mention_key,
                "zh-CN",
                MailTemplate::new(
                    "{{actor_username}} 在 {{channel_name}} 提到了你",
                    "<p><b>{{actor_username}}</b> 在 <b>{{channel_name}}</b> 中提到了你：</p><p>{{message_text}}</p>",
                    Some("{{actor_username}} 在 {{channel_name}} 提到了你：{{message_text}}"),
                ),
            ),
            LocalizedMailTemplate::new(
                chat_reply_key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "{{actor_username}} replied to your message in {{channel_name}}",
                    "<p><b>{{actor_username}}</b> replied to your message in <b>{{channel_name}}</b>:</p><p>{{message_text}}</p>",
                    Some(
                        "{{actor_username}} replied to your message in {{channel_name}}: {{message_text}}",
                    ),
                ),
            ),
            LocalizedMailTemplate::new(
                chat_reply_key,
                "zh-CN",
                MailTemplate::new(
                    "{{actor_username}} 在 {{channel_name}} 回复了你的消息",
                    "<p><b>{{actor_username}}</b> 在 <b>{{channel_name}}</b> 回复了你的消息：</p><p>{{message_text}}</p>",
                    Some("{{actor_username}} 在 {{channel_name}} 回复了你的消息：{{message_text}}"),
                ),
            ),
        ],
    )
}

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

async fn ensure_cl_merged_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(EVENT_CL_MERGED, "cl", "Change List was merged", false, true)
        .await?;

    Ok(())
}

async fn ensure_issue_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(
        EVENT_ISSUE_COMMENT_CREATED,
        "issue",
        "New comment on an Issue",
        false,
        true,
    )
    .await?;
    stg.upsert_event_type(EVENT_ISSUE_CLOSED, "issue", "Issue was closed", false, true)
        .await?;

    Ok(())
}

async fn ensure_reference_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(
        EVENT_ITEM_REFERENCED,
        "reference",
        "Your CL or Issue was referenced (mentioned)",
        false,
        true,
    )
    .await?;

    Ok(())
}

async fn ensure_chat_mention_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(
        EVENT_CHAT_MENTION_CREATED,
        "chat",
        "You were mentioned in a chat message",
        false,
        true,
    )
    .await?;

    Ok(())
}

async fn ensure_chat_reply_event_type_exists(stg: &NotificationStorage) -> Result<(), MegaError> {
    stg.upsert_event_type(
        EVENT_CHAT_REPLY_CREATED,
        "chat",
        "Your message received a reply",
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
    let registry = current_notification_mail_template_registry()?;
    on_cl_comment_created_with_registry(
        notif_stg,
        cl_stg,
        reviewer_stg,
        &registry,
        actor_username,
        cl_link,
        comment_text,
    )
    .await
}

pub async fn on_cl_comment_created_with_registry(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    reviewer_stg: &ClReviewerStorage,
    mail_templates: &MailTemplateRegistry,
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

        let mail = mail_templates.render(
            &cl_comment_created_mail_template_key(),
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

/// Trigger: a Change List is merged.
///
/// Notifies the CL author (excluding the merger), respecting user preferences
/// via `should_send`, and enqueues an email job for the background dispatcher.
pub async fn on_cl_merged(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    actor_username: &str,
    cl_link: &str,
) -> Result<(), MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_cl_merged_with_registry(notif_stg, cl_stg, &registry, actor_username, cl_link).await
}

pub async fn on_cl_merged_with_registry(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    cl_link: &str,
) -> Result<(), MegaError> {
    ensure_cl_merged_event_type_exists(notif_stg).await?;

    let cl = cl_stg
        .get_cl(cl_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("CL {cl_link} not found")))?;

    if cl.username == actor_username {
        return Ok(());
    }

    if !notif_stg.should_send(&cl.username, EVENT_CL_MERGED).await? {
        return Ok(());
    }

    let settings = match notif_stg.get_user_settings(&cl.username).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    let mail = mail_templates.render(
        &cl_merged_mail_template_key(),
        settings.preferred_locale.as_deref(),
        &[
            ("actor_username", actor_username),
            ("cl_link", cl_link),
            ("cl_title", &cl.title),
        ],
    )?;

    notif_stg
        .enqueue_email_job(
            &cl.username,
            &settings.email,
            EVENT_CL_MERGED,
            &mail.subject,
            &mail.html,
            mail.text.as_deref(),
        )
        .await?;

    Ok(())
}

/// Trigger: a new comment is created on an Issue.
///
/// Behavior mirrors [`on_cl_comment_created`]: notify the issue author (excluding
/// the actor), respecting user preferences via `should_send`, and enqueue an
/// email job for the background dispatcher. Additional recipients (assignees /
/// participants) are a future extension.
pub async fn on_issue_comment_created(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    issue_link: &str,
    comment_text: &str,
) -> Result<(), MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_issue_comment_created_with_registry(
        notif_stg,
        issue_stg,
        &registry,
        actor_username,
        issue_link,
        comment_text,
    )
    .await
}

pub async fn on_issue_comment_created_with_registry(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    issue_link: &str,
    comment_text: &str,
) -> Result<(), MegaError> {
    ensure_issue_event_type_exists(notif_stg).await?;

    let issue = issue_stg
        .get_issue(issue_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("Issue {issue_link} not found")))?;

    let mut recipients: HashSet<String> = HashSet::new();
    recipients.insert(issue.author);
    recipients.remove(actor_username);

    for username in recipients {
        if !notif_stg
            .should_send(&username, EVENT_ISSUE_COMMENT_CREATED)
            .await?
        {
            continue;
        }

        let settings = match notif_stg.get_user_settings(&username).await? {
            Some(s) => s,
            None => continue,
        };

        let mail = mail_templates.render(
            &issue_comment_created_mail_template_key(),
            settings.preferred_locale.as_deref(),
            &[
                ("actor_username", actor_username),
                ("issue_link", issue_link),
                ("issue_title", &issue.title),
                ("comment_text", comment_text),
            ],
        )?;

        notif_stg
            .enqueue_email_job(
                &username,
                &settings.email,
                EVENT_ISSUE_COMMENT_CREATED,
                &mail.subject,
                &mail.html,
                mail.text.as_deref(),
            )
            .await?;
    }

    Ok(())
}

/// Trigger: an issue was closed.
///
/// Notifies the issue author (excluding the actor who closed it), respecting
/// user preferences.
pub async fn on_issue_closed(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    issue_link: &str,
) -> Result<(), MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_issue_closed_with_registry(notif_stg, issue_stg, &registry, actor_username, issue_link).await
}

pub async fn on_issue_closed_with_registry(
    notif_stg: &NotificationStorage,
    issue_stg: &IssueStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    issue_link: &str,
) -> Result<(), MegaError> {
    ensure_issue_event_type_exists(notif_stg).await?;

    let issue = issue_stg
        .get_issue(issue_link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("Issue {issue_link} not found")))?;

    let mut recipients: HashSet<String> = HashSet::new();
    recipients.insert(issue.author);
    recipients.remove(actor_username);

    for username in recipients {
        if !notif_stg.should_send(&username, EVENT_ISSUE_CLOSED).await? {
            continue;
        }

        let settings = match notif_stg.get_user_settings(&username).await? {
            Some(s) => s,
            None => continue,
        };

        let mail = mail_templates.render(
            &issue_closed_mail_template_key(),
            settings.preferred_locale.as_deref(),
            &[
                ("actor_username", actor_username),
                ("issue_link", issue_link),
                ("issue_title", &issue.title),
            ],
        )?;

        notif_stg
            .enqueue_email_job(
                &username,
                &settings.email,
                EVENT_ISSUE_CLOSED,
                &mail.subject,
                &mail.html,
                mail.text.as_deref(),
            )
            .await?;
    }

    Ok(())
}

/// Trigger: an item (CL or Issue) is referenced / @mentioned from a comment.
///
/// Notifies the author of the referenced item (excluding the actor), respecting
/// user preferences. Resolves `referenced_link` as a CL first, then as an Issue.
/// Wired into `api_common::comment::check_comment_ref`, which is shared by the
/// CL and Issue comment paths, so this covers cross-references from both.
pub async fn on_item_referenced(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    issue_stg: &IssueStorage,
    actor_username: &str,
    source_link: &str,
    referenced_link: &str,
) -> Result<(), MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_item_referenced_with_registry(
        notif_stg,
        cl_stg,
        issue_stg,
        &registry,
        actor_username,
        source_link,
        referenced_link,
    )
    .await
}

pub async fn on_item_referenced_with_registry(
    notif_stg: &NotificationStorage,
    cl_stg: &ClStorage,
    issue_stg: &IssueStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    source_link: &str,
    referenced_link: &str,
) -> Result<(), MegaError> {
    // Resolve the referenced item's author: CL first, then Issue. Unknown links
    // are ignored (no notification).
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

    ensure_reference_event_type_exists(notif_stg).await?;

    if !notif_stg
        .should_send(&author, EVENT_ITEM_REFERENCED)
        .await?
    {
        return Ok(());
    }

    let settings = match notif_stg.get_user_settings(&author).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    let mail = mail_templates.render(
        &item_referenced_mail_template_key(),
        settings.preferred_locale.as_deref(),
        &[
            ("actor_username", actor_username),
            ("referenced_link", referenced_link),
            ("source_link", source_link),
        ],
    )?;

    notif_stg
        .enqueue_email_job(
            &author,
            &settings.email,
            EVENT_ITEM_REFERENCED,
            &mail.subject,
            &mail.html,
            mail.text.as_deref(),
        )
        .await?;

    Ok(())
}

/// Trigger: a user is @mentioned in a chat message.
///
/// Notifies each mentioned user (excluding the actor), respecting user
/// preferences and enqueuing an email job for the background dispatcher.
/// Returns the set of usernames for whom an email job was actually enqueued.
pub async fn on_chat_mention_created(
    notif_stg: &NotificationStorage,
    actor_username: &str,
    channel_name: &str,
    message_text: &str,
    mentioned_usernames: &[String],
) -> Result<HashSet<String>, MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_chat_mention_created_with_registry(
        notif_stg,
        &registry,
        actor_username,
        channel_name,
        message_text,
        mentioned_usernames,
    )
    .await
}

pub async fn on_chat_mention_created_with_registry(
    notif_stg: &NotificationStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    channel_name: &str,
    message_text: &str,
    mentioned_usernames: &[String],
) -> Result<HashSet<String>, MegaError> {
    ensure_chat_mention_event_type_exists(notif_stg).await?;

    let mut enqueued = HashSet::new();
    for username in mentioned_usernames {
        if username == actor_username {
            continue;
        }

        if !notif_stg
            .should_send(username, EVENT_CHAT_MENTION_CREATED)
            .await?
        {
            continue;
        }

        let settings = match notif_stg.get_user_settings(username).await? {
            Some(s) => s,
            None => continue,
        };

        let mail = match mail_templates.render(
            &chat_mention_created_mail_template_key(),
            settings.preferred_locale.as_deref(),
            &[
                ("actor_username", actor_username),
                ("channel_name", channel_name),
                ("message_text", message_text),
            ],
        ) {
            Ok(mail) => mail,
            Err(e) => {
                tracing::warn!(
                    username = %username,
                    error = %e,
                    "failed to render chat mention email; skipping recipient"
                );
                continue;
            }
        };

        if let Err(e) = notif_stg
            .enqueue_email_job(
                username,
                &settings.email,
                EVENT_CHAT_MENTION_CREATED,
                &mail.subject,
                &mail.html,
                mail.text.as_deref(),
            )
            .await
        {
            tracing::warn!(
                username = %username,
                error = %e,
                "failed to enqueue chat mention email; skipping recipient"
            );
            continue;
        }
        enqueued.insert(username.clone());
    }

    Ok(enqueued)
}

/// Trigger: a user receives a reply to their chat message.
///
/// Notifies the author of the parent message (excluding the actor), respecting
/// user preferences and enqueuing an email job for the background dispatcher.
pub async fn on_chat_reply_created(
    notif_stg: &NotificationStorage,
    actor_username: &str,
    channel_name: &str,
    message_text: &str,
    parent_author: &str,
) -> Result<(), MegaError> {
    let registry = current_notification_mail_template_registry()?;
    on_chat_reply_created_with_registry(
        notif_stg,
        &registry,
        actor_username,
        channel_name,
        message_text,
        parent_author,
    )
    .await
}

pub async fn on_chat_reply_created_with_registry(
    notif_stg: &NotificationStorage,
    mail_templates: &MailTemplateRegistry,
    actor_username: &str,
    channel_name: &str,
    message_text: &str,
    parent_author: &str,
) -> Result<(), MegaError> {
    if parent_author == actor_username {
        return Ok(());
    }

    ensure_chat_reply_event_type_exists(notif_stg).await?;

    if !notif_stg
        .should_send(parent_author, EVENT_CHAT_REPLY_CREATED)
        .await?
    {
        return Ok(());
    }

    let settings = match notif_stg.get_user_settings(parent_author).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    let mail = mail_templates.render(
        &chat_reply_created_mail_template_key(),
        settings.preferred_locale.as_deref(),
        &[
            ("actor_username", actor_username),
            ("channel_name", channel_name),
            ("message_text", message_text),
        ],
    )?;

    notif_stg
        .enqueue_email_job(
            parent_author,
            &settings.email,
            EVENT_CHAT_REPLY_CREATED,
            &mail.subject,
            &mail.html,
            mail.text.as_deref(),
        )
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{email_jobs, mega_cl, mega_cl_reviewer, mega_issue},
        jupiter::{
            migration::apply_migrations,
            storage::base_storage::{BaseStorage, StorageConnector},
            tests::test_db_connection,
        },
    };

    #[test]
    fn mail_template_reload_subscriber_hot_swaps_default_locale() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = crate::config::testing::isolated_config(temp_dir.path().join("base"));
        config.mail = Some(crate::config::MailConfig {
            template_default_locale: "zh-CN".to_string(),
            ..Default::default()
        });
        let report = ConfigReloadReport {
            applied_fields: vec!["mail.template_default_locale"],
            restart_required_fields: Vec::new(),
        };

        apply_mail_template_registry_reload(&config, &report)
            .expect("template registry reload should apply");

        let registry = current_notification_mail_template_registry().expect("registry");
        let rendered = registry
            .render(
                &cl_comment_created_mail_template_key(),
                None,
                &[
                    ("actor_username", "bob"),
                    ("cl_link", "L1"),
                    ("comment_text", "hi"),
                ],
            )
            .expect("render with hot-swapped default locale");
        assert!(
            rendered.subject.contains("有新评论"),
            "default locale should now resolve to the zh-CN template"
        );

        // Restore the global registry so other tests observe the default.
        configure_notification_mail_template_registry(
            default_notification_mail_template_registry(),
        )
        .expect("restore default registry");
    }

    #[tokio::test]
    async fn test_on_issue_comment_created_enqueues_job_for_issue_author() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let issue_stg = IssueStorage { base: base.clone() };

        let now = chrono::Utc::now().naive_utc();
        mega_issue::ActiveModel {
            id: Set(1),
            link: Set("ISSUE1".to_string()),
            title: Set("My Issue".to_string()),
            status: Set("open".to_string()),
            created_at: Set(now),
            updated_at: Set(now),
            closed_at: Set(None),
            author: Set("alice".to_string()),
        }
        .insert(&db)
        .await
        .unwrap();

        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        // bob comments on alice's issue.
        on_issue_comment_created(&notif, &issue_stg, "bob", "ISSUE1", "looks good")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "issue author should be notified");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "issue.comment.created");
        assert!(jobs[0].subject.contains("My Issue"));

        // The actor (bob) commenting does not notify himself even if he is the author.
        on_issue_comment_created(&notif, &issue_stg, "alice", "ISSUE1", "self comment")
            .await
            .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "actor should not be notified of own comment"
        );
    }

    #[tokio::test]
    async fn test_on_issue_closed_enqueues_job_for_issue_author() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let issue_stg = IssueStorage { base: base.clone() };

        let now = chrono::Utc::now().naive_utc();
        mega_issue::ActiveModel {
            id: Set(1),
            link: Set("ISSUE1".to_string()),
            title: Set("My Issue".to_string()),
            status: Set("open".to_string()),
            created_at: Set(now),
            updated_at: Set(now),
            closed_at: Set(None),
            author: Set("alice".to_string()),
        }
        .insert(&db)
        .await
        .unwrap();

        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        // bob closes alice's issue.
        on_issue_closed(&notif, &issue_stg, "bob", "ISSUE1")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "issue author should be notified");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "issue.closed");
        assert!(jobs[0].subject.contains("My Issue"));

        // The actor (alice) closing her own issue does not notify herself.
        on_issue_closed(&notif, &issue_stg, "alice", "ISSUE1")
            .await
            .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "actor should not be notified of own close"
        );
    }

    #[tokio::test]
    async fn test_on_chat_mention_created_enqueues_job_for_mentioned_user() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let notif = NotificationStorage::new(Arc::new(db.clone()));
        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        on_chat_mention_created(
            &notif,
            "bob",
            "general",
            "hi @alice, please review",
            &["alice".to_string()],
        )
        .await
        .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "mentioned user should be notified");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "chat.mention.created");
        assert!(jobs[0].subject.contains("mentioned you"));

        // The actor mentioning themselves should not enqueue a job.
        on_chat_mention_created(
            &notif,
            "alice",
            "general",
            "hi @alice",
            &["alice".to_string()],
        )
        .await
        .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "actor should not be notified of self-mention"
        );
    }

    #[tokio::test]
    async fn test_on_chat_reply_created_enqueues_job_for_parent_author() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let notif = NotificationStorage::new(Arc::new(db.clone()));
        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        on_chat_reply_created(&notif, "bob", "general", "thanks for the context", "alice")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "parent message author should be notified");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "chat.reply.created");
        assert!(jobs[0].subject.contains("replied"));

        // The actor replying to their own message should not enqueue a job.
        on_chat_reply_created(&notif, "alice", "general", "self reply", "alice")
            .await
            .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "actor should not be notified of own reply"
        );
    }

    #[tokio::test]
    async fn test_on_cl_merged_enqueues_job_for_author_excluding_merger() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };

        let now = chrono::Utc::now().naive_utc();
        mega_cl::ActiveModel {
            id: Set(1),
            link: Set("CL-MERGED".to_string()),
            title: Set("My CL".to_string()),
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

        // bob merges alice's CL.
        on_cl_merged(&notif, &cl_stg, "bob", "CL-MERGED")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "CL author should be notified of merge");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "cl.merged");
        assert!(jobs[0].subject.contains("CL-MERGED"));

        // The author merging their own CL does not notify themselves.
        on_cl_merged(&notif, &cl_stg, "alice", "CL-MERGED")
            .await
            .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "author merging own CL should not be notified"
        );
    }

    #[tokio::test]
    async fn test_on_item_referenced_notifies_referenced_cl_author() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let issue_stg = IssueStorage { base: base.clone() };

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
        notif
            .upsert_user_settings("alice", "alice@example.com")
            .await
            .unwrap();

        // bob references alice's CL1 from ISSUE9.
        on_item_referenced(&notif, &cl_stg, &issue_stg, "bob", "ISSUE9", "CL1")
            .await
            .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(jobs.len(), 1, "referenced CL author should be notified");
        assert_eq!(jobs[0].username, "alice");
        assert_eq!(jobs[0].event_type_code, "item.referenced");
        assert!(jobs[0].subject.contains("CL1"));

        // Unknown link notifies nobody; author referencing own item notifies nobody.
        on_item_referenced(&notif, &cl_stg, &issue_stg, "bob", "ISSUE9", "UNKNOWN")
            .await
            .unwrap();
        on_item_referenced(&notif, &cl_stg, &issue_stg, "alice", "ISSUE9", "CL1")
            .await
            .unwrap();
        let jobs_after = email_jobs::Entity::find().all(&db).await.unwrap();
        assert_eq!(
            jobs_after.len(),
            1,
            "no extra notifications for unknown/self reference"
        );
    }

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
    async fn test_on_cl_comment_created_skips_when_all_recipients_opt_out() {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();

        let base = BaseStorage::new(Arc::new(db.clone()));
        let notif = NotificationStorage::new(Arc::new(db.clone()));
        let cl_stg = ClStorage { base: base.clone() };
        let reviewer_stg = ClReviewerStorage { base: base.clone() };
        let now = chrono::Utc::now().naive_utc();

        mega_cl::ActiveModel {
            id: Set(10),
            link: Set("CL-opt-out".to_string()),
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

        for (id, username) in [(10, "bob"), (11, "carol")] {
            mega_cl_reviewer::ActiveModel {
                id: Set(id),
                cl_link: Set("CL-opt-out".to_string()),
                username: Set(username.to_string()),
                approved: Set(false),
                system_required: Set(false),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&db)
            .await
            .unwrap();
        }

        ensure_event_type_exists(&notif).await.unwrap();
        for username in ["alice", "bob", "carol"] {
            notif
                .upsert_user_settings(username, &format!("{username}@example.com"))
                .await
                .unwrap();
            notif
                .set_user_preference(username, EVENT_CL_COMMENT_CREATED, false)
                .await
                .unwrap();
        }

        on_cl_comment_created(
            &notif,
            &cl_stg,
            &reviewer_stg,
            "dave",
            "CL-opt-out",
            "hello",
        )
        .await
        .unwrap();

        let jobs = email_jobs::Entity::find().all(&db).await.unwrap();
        assert!(jobs.is_empty());
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
    async fn test_on_cl_comment_created_uses_supplied_mail_template_registry() {
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
            link: Set("CL-custom-template".to_string()),
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

        let template_key = MailTemplateKey::new(EVENT_CL_COMMENT_CREATED);
        let registry = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            vec![LocalizedMailTemplate::new(
                template_key,
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new(
                    "[custom] {{cl_link}}",
                    "<section>{{comment_text}}</section>",
                    Some("[custom] {{actor_username}}: {{comment_text}}"),
                ),
            )],
        );

        on_cl_comment_created_with_registry(
            &notif,
            &cl_stg,
            &reviewer_stg,
            &registry,
            "bob",
            "CL-custom-template",
            "<b>ship it</b>",
        )
        .await
        .unwrap();

        let job = email_jobs::Entity::find()
            .filter(email_jobs::Column::Username.eq("alice"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(job.subject, "[custom] CL-custom-template");
        assert_eq!(
            job.body_html,
            "<section>&lt;b&gt;ship it&lt;/b&gt;</section>"
        );
        assert_eq!(
            job.body_text.as_deref(),
            Some("[custom] bob: <b>ship it</b>")
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
