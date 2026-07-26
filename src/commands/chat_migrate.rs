use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use clap::{Arg, ArgMatches, Command};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait, PaginatorTrait};
use serde::Deserialize;

use crate::{
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    context::AppContext,
    jupiter::storage::base_storage::StorageConnector,
};

const REQUIRED_LEGACY_EXPORTS: [&str; 9] = [
    "attachments.json",
    "custom_reactions.json",
    "message_notifications.json",
    "message_thread_membership_updates.json",
    "message_thread_memberships.json",
    "message_threads.json",
    "messages.json",
    "open_graph_links.json",
    "reactions.json",
];

#[derive(Deserialize, Debug)]
pub(crate) struct MigrationMapping {
    user_mappings: HashMap<String, String>,
    org_membership_mappings: HashMap<String, String>,
}

pub fn cli() -> Command {
    Command::new("chat-migrate")
        .about("Migrate legacy chat tables from PlanetScale JSON exports to monoengine chat/channel tables")
        .arg(
            Arg::new("input-dir")
                .short('i')
                .long("input-dir")
                .required(true)
                .value_name("DIR")
                .help("Directory path containing the 9 legacy JSON exports (e.g. messages.json, message_threads.json)"),
        )
        .arg(
            Arg::new("user-mapping")
                .short('u')
                .long("user-mapping")
                .required(true)
                .value_name("FILE")
                .help("JSON file path containing user mappings (user_mappings and org_membership_mappings)"),
        )
}

fn get_i64(val: &serde_json::Value) -> Option<i64> {
    match val {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn get_i32(val: &serde_json::Value) -> Option<i32> {
    match val {
        serde_json::Value::Number(n) => n.as_i64().map(|i| i as i32),
        serde_json::Value::String(s) => s.parse::<i32>().ok(),
        _ => None,
    }
}

fn get_bool(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else {
                false
            }
        }
        serde_json::Value::String(s) => s == "true" || s == "1" || s == "yes",
        _ => false,
    }
}

fn get_str(val: &serde_json::Value) -> Option<String> {
    match val {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_dt(s: &str) -> chrono::NaiveDateTime {
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return dt;
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return dt;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.naive_utc();
    }
    chrono::Utc::now().naive_utc()
}

fn get_dt(val: &serde_json::Value) -> Option<chrono::NaiveDateTime> {
    match val {
        serde_json::Value::String(s) => Some(parse_dt(s)),
        serde_json::Value::Number(n) => {
            if let Some(secs) = n.as_i64() {
                chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.naive_utc())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn validate_legacy_export_set(input_dir: &Path) -> MegaResult {
    if !input_dir.is_dir() {
        return Err(MegaError::Other(format!(
            "Legacy chat export input path is not a directory: {}",
            input_dir.display()
        )));
    }

    let missing: Vec<_> = REQUIRED_LEGACY_EXPORTS
        .iter()
        .filter(|filename| !input_dir.join(filename).is_file())
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(MegaError::Other(format!(
            "Legacy chat export is incomplete: missing required file(s): {}",
            missing.join(", ")
        )));
    }

    Ok(())
}

fn read_legacy_json(input_dir: &Path, filename: &str) -> Result<Vec<serde_json::Value>, MegaError> {
    let path = input_dir.join(filename);
    let content = fs::read_to_string(&path).map_err(|e| {
        MegaError::Other(format!(
            "Failed to read legacy chat export {}: {}",
            path.display(),
            e
        ))
    })?;
    serde_json::from_str::<Vec<serde_json::Value>>(&content).map_err(|e| {
        MegaError::Other(format!(
            "Failed to parse legacy chat export {} as a JSON array: {}",
            path.display(),
            e
        ))
    })
}

fn resolve_username(
    mapping: &MigrationMapping,
    user_id: Option<i64>,
    membership_id: Option<i64>,
    context: &str,
) -> Result<String, MegaError> {
    if let Some(uname) = user_id.and_then(|uid| mapping.user_mappings.get(&uid.to_string())) {
        return Ok(uname.clone());
    }
    if let Some(uname) =
        membership_id.and_then(|mid| mapping.org_membership_mappings.get(&mid.to_string()))
    {
        return Ok(uname.clone());
    }

    let id_description = match (user_id, membership_id) {
        (Some(uid), Some(mid)) => format!("user_id={uid} or organization_membership_id={mid}"),
        (Some(uid), None) => format!("user_id={uid}"),
        (None, Some(mid)) => format!("organization_membership_id={mid}"),
        (None, None) => "missing user_id and organization_membership_id".to_string(),
    };
    Err(MegaError::Other(format!(
        "Missing chat migration user mapping for {id_description} at {context}"
    )))
}

fn validate_chat_migration_mappings(
    mapping: &MigrationMapping,
    custom_reactions: &[serde_json::Value],
    threads: &[serde_json::Value],
    memberships: &[serde_json::Value],
    membership_updates: &[serde_json::Value],
    messages: &[serde_json::Value],
    reactions: &[serde_json::Value],
) -> MegaResult {
    let imported_channel_ids: HashSet<i64> = threads
        .iter()
        .filter_map(|v| {
            let id = get_i64(&v["id"]).unwrap_or(0);
            let oauth_id = get_i64(&v["oauth_application_id"]);
            let integration_id = get_i64(&v["integration_id"]);
            (id != 0 && oauth_id.is_none() && integration_id.is_none()).then_some(id)
        })
        .collect();
    let imported_message_ids: HashSet<i64> = messages
        .iter()
        .filter_map(|v| {
            let id = get_i64(&v["id"]).unwrap_or(0);
            let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
            let oauth_id = get_i64(&v["oauth_application_id"]);
            let integration_id = get_i64(&v["integration_id"]);
            let call_id = get_i64(&v["call_id"]);
            (id != 0
                && imported_channel_ids.contains(&channel_id)
                && oauth_id.is_none()
                && integration_id.is_none()
                && call_id.is_none())
            .then_some(id)
        })
        .collect();

    for v in custom_reactions {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let name = get_str(&v["name"]).unwrap_or_default();
        if id == 0 || name.is_empty() {
            continue;
        }
        resolve_username(
            mapping,
            get_i64(&v["user_id"]),
            None,
            &format!("custom_reactions[{id}].user_id"),
        )?;
    }

    for v in threads {
        let id = get_i64(&v["id"]).unwrap_or(0);
        if !imported_channel_ids.contains(&id) {
            continue;
        }
        resolve_username(
            mapping,
            get_i64(&v["owner_id"]),
            None,
            &format!("message_threads[{id}].owner_id"),
        )?;
    }

    for v in memberships {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            continue;
        }
        resolve_username(
            mapping,
            get_i64(&v["user_id"]),
            get_i64(&v["organization_membership_id"]),
            &format!("message_thread_memberships[{id}]"),
        )?;
    }

    for v in membership_updates {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            continue;
        }
        resolve_username(
            mapping,
            get_i64(&v["actor_id"]),
            None,
            &format!("message_thread_membership_updates[{id}].actor_id"),
        )?;
        for field in ["added_usernames", "removed_usernames"] {
            if let Some(arr) = v[field].as_array() {
                for item in arr {
                    if let Some(user_id) = item.as_i64() {
                        resolve_username(
                            mapping,
                            Some(user_id),
                            None,
                            &format!("message_thread_membership_updates[{id}].{field}"),
                        )?;
                    } else if let Some(str_val) = item.as_str()
                        && let Ok(user_id) = str_val.parse::<i64>()
                    {
                        resolve_username(
                            mapping,
                            Some(user_id),
                            None,
                            &format!("message_thread_membership_updates[{id}].{field}"),
                        )?;
                    }
                }
            }
        }
    }

    for v in messages {
        let id = get_i64(&v["id"]).unwrap_or(0);
        if !imported_message_ids.contains(&id) {
            continue;
        }
        if let Some(sender_id) = get_i64(&v["sender_id"]) {
            resolve_username(
                mapping,
                Some(sender_id),
                None,
                &format!("messages[{id}].sender_id"),
            )?;
        }
    }

    for v in reactions {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let subject_type = get_str(&v["subject_type"]).unwrap_or_default();
        let subject_id = get_i64(&v["subject_id"]).unwrap_or(0);
        if id == 0 || subject_type != "Message" || !imported_message_ids.contains(&subject_id) {
            continue;
        }
        resolve_username(
            mapping,
            get_i64(&v["user_id"]),
            get_i64(&v["organization_membership_id"]),
            &format!("reactions[{id}]"),
        )?;
    }

    Ok(())
}

pub(crate) struct MigrationInput {
    pub mapping: MigrationMapping,
    pub custom_reactions: Vec<serde_json::Value>,
    pub open_graph_links: Vec<serde_json::Value>,
    pub threads: Vec<serde_json::Value>,
    pub memberships: Vec<serde_json::Value>,
    pub membership_updates: Vec<serde_json::Value>,
    pub messages: Vec<serde_json::Value>,
    pub notifications: Vec<serde_json::Value>,
    pub attachments: Vec<serde_json::Value>,
    pub reactions: Vec<serde_json::Value>,
}

pub(crate) struct MigrationReport {
    pub custom_reaction_in: usize,
    pub custom_reaction_out: usize,
    pub custom_reaction_skip: usize,
    pub custom_reaction_conflict: usize,
    pub og_in: usize,
    pub og_out: usize,
    pub og_skip: usize,
    pub channel_in: usize,
    pub channel_out: usize,
    pub channel_skip: usize,
    pub membership_in: usize,
    pub membership_out: usize,
    pub membership_skip: usize,
    pub membership_update_in: usize,
    pub membership_update_out: usize,
    pub membership_update_skip: usize,
    pub message_in: usize,
    pub message_out: usize,
    pub message_skip: usize,
    pub message_skip_oauth_integration: usize,
    pub message_skip_calls: usize,
    pub notif_in: usize,
    pub notif_out: usize,
    pub notif_skip: usize,
    pub attachment_in: usize,
    pub attachment_out: usize,
    pub attachment_skip: usize,
    pub reaction_in: usize,
    pub reaction_out: usize,
    pub reaction_skip: usize,
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config = require_config(ctx, "chat-migrate")?;

    let input_dir = args.get_one::<String>("input-dir").unwrap();
    let mapping_file = args.get_one::<String>("user-mapping").unwrap();
    let dir_path = Path::new(input_dir);
    validate_legacy_export_set(dir_path)?;

    let mapping_content = fs::read_to_string(mapping_file)
        .map_err(|e| MegaError::Other(format!("Failed to read user mapping file: {}", e)))?;
    let mapping: MigrationMapping = serde_json::from_str(&mapping_content)
        .map_err(|e| MegaError::Other(format!("Failed to parse user mapping JSON: {}", e)))?;

    let legacy_custom_reactions = read_legacy_json(dir_path, "custom_reactions.json")?;
    let legacy_og = read_legacy_json(dir_path, "open_graph_links.json")?;
    let legacy_threads = read_legacy_json(dir_path, "message_threads.json")?;
    let legacy_memberships = read_legacy_json(dir_path, "message_thread_memberships.json")?;
    let legacy_updates = read_legacy_json(dir_path, "message_thread_membership_updates.json")?;
    let legacy_messages = read_legacy_json(dir_path, "messages.json")?;
    let legacy_notifs = read_legacy_json(dir_path, "message_notifications.json")?;
    let legacy_attachments = read_legacy_json(dir_path, "attachments.json")?;
    let legacy_reactions = read_legacy_json(dir_path, "reactions.json")?;
    validate_chat_migration_mappings(
        &mapping,
        &legacy_custom_reactions,
        &legacy_threads,
        &legacy_memberships,
        &legacy_updates,
        &legacy_messages,
        &legacy_reactions,
    )?;

    let context = AppContext::new(config).await?;
    let mono_storage = context.storage.mono_storage();
    let conn = mono_storage.get_connection();

    let existing_channels = crate::callisto::channel::Entity::find()
        .count(conn)
        .await
        .unwrap_or(0);
    if existing_channels > 0 {
        return Err(MegaError::Other(format!(
            "Refusing to migrate: target database already has {existing_channels} channel(s). \
             The migration requires an empty database (it inserts with explicit legacy IDs and \
             is not idempotent). Please truncate chat tables before re-running."
        )));
    }

    println!(
        "Loaded user mappings: {} users, {} org memberships",
        mapping.user_mappings.len(),
        mapping.org_membership_mappings.len()
    );

    let input = MigrationInput {
        mapping,
        custom_reactions: legacy_custom_reactions,
        open_graph_links: legacy_og,
        threads: legacy_threads,
        memberships: legacy_memberships,
        membership_updates: legacy_updates,
        messages: legacy_messages,
        notifications: legacy_notifs,
        attachments: legacy_attachments,
        reactions: legacy_reactions,
    };

    run_migration(conn, &input).await
}

pub(crate) async fn run_migration(
    conn: &sea_orm::DatabaseConnection,
    input: &MigrationInput,
) -> MegaResult {
    let mapping = &input.mapping;
    let legacy_custom_reactions = &input.custom_reactions;
    let legacy_og = &input.open_graph_links;
    let legacy_threads = &input.threads;
    let legacy_memberships = &input.memberships;
    let legacy_updates = &input.membership_updates;
    let legacy_messages = &input.messages;
    let legacy_notifs = &input.notifications;
    let legacy_attachments = &input.attachments;
    let legacy_reactions = &input.reactions;

    println!("\n=== Starting Migration ===");

    // 1. custom_reactions
    let custom_reaction_in = legacy_custom_reactions.len();
    let mut custom_reaction_out = 0;
    let mut custom_reaction_skip = 0;
    let mut custom_reaction_conflict = 0;
    let mut imported_custom_reactions = HashSet::new();

    // Sort by created_at to keep earliest on conflict
    let mut custom_reaction_list = Vec::new();
    for v in legacy_custom_reactions {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        custom_reaction_list.push((id, created_at, v));
    }
    custom_reaction_list.sort_by_key(|k| k.1);

    let mut seen_names = HashSet::new();
    for (id, created_at, v) in custom_reaction_list {
        if id == 0 {
            custom_reaction_skip += 1;
            continue;
        }
        let name = get_str(&v["name"]).unwrap_or_default();
        if name.is_empty() {
            custom_reaction_skip += 1;
            continue;
        }
        let name_lower = name.to_lowercase();
        if seen_names.contains(&name_lower) {
            custom_reaction_conflict += 1;
            continue;
        }
        seen_names.insert(name_lower);

        let public_id = get_str(&v["public_id"])
            .unwrap_or_else(crate::callisto::entity_ext::generate_public_id);
        let file_path = get_str(&v["file_path"]).unwrap_or_default();
        let file_type = get_str(&v["file_type"]).unwrap_or_default();
        let user_id = get_i64(&v["user_id"]);
        let username = resolve_username(
            mapping,
            user_id,
            None,
            &format!("custom_reactions[{id}].user_id"),
        )?;
        let pack = get_str(&v["pack"]);
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::custom_reaction::ActiveModel {
            id: Set(id),
            public_id: Set(public_id),
            name: Set(name),
            file_path: Set(file_path),
            file_type: Set(file_type),
            username: Set(username),
            pack: Set(pack),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => {
                custom_reaction_out += 1;
                imported_custom_reactions.insert(id);
            }
            Err(e) => {
                println!("Error inserting custom_reaction {}: {}", id, e);
                custom_reaction_skip += 1;
            }
        }
    }

    // 2. open_graph_links
    let og_in = legacy_og.len();
    let mut og_out = 0;
    let mut og_skip = 0;

    let mut seen_urls = HashSet::new();
    for v in legacy_og {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let url = get_str(&v["url"]).unwrap_or_default();
        if id == 0 || url.is_empty() {
            og_skip += 1;
            continue;
        }
        if seen_urls.contains(&url) {
            og_skip += 1;
            continue;
        }
        seen_urls.insert(url.clone());

        let title = get_str(&v["title"]).unwrap_or_default();
        let image_path = get_str(&v["image_path"]);
        let favicon_path = get_str(&v["favicon_path"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::open_graph_link::ActiveModel {
            id: Set(id),
            url: Set(url),
            title: Set(title),
            image_path: Set(image_path),
            favicon_path: Set(favicon_path),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => og_out += 1,
            Err(e) => {
                println!("Error inserting open_graph_link {}: {}", id, e);
                og_skip += 1;
            }
        }
    }

    // 3. channels (from message_threads)
    let channel_in = legacy_threads.len();
    let mut channel_out = 0;
    let mut channel_skip = 0;
    let mut imported_channel_ids = HashSet::new();

    for v in legacy_threads {
        let id = get_i64(&v["id"]).unwrap_or(0);
        if id == 0 {
            channel_skip += 1;
            continue;
        }

        // Skip integration DM or app message
        let oauth_id = get_i64(&v["oauth_application_id"]);
        let integration_id = get_i64(&v["integration_id"]);
        if oauth_id.is_some() || integration_id.is_some() {
            channel_skip += 1;
            continue;
        }

        let public_id = get_str(&v["public_id"])
            .unwrap_or_else(crate::callisto::entity_ext::generate_public_id);
        let title = get_str(&v["title"]);
        let last_message_at =
            get_dt(&v["last_message_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let latest_message_id = get_i64(&v["latest_message_id"]);
        let members_count = get_i32(&v["members_count"]).unwrap_or(0);
        let image_path = get_str(&v["image_path"]);
        let group = get_bool(&v["group"]);
        let notification_forced_at = get_dt(&v["notification_forced_at"]);
        let owner_id = get_i64(&v["owner_id"]);
        let owner_username = resolve_username(
            mapping,
            owner_id,
            None,
            &format!("message_threads[{id}].owner_id"),
        )?;
        let discarded_at = get_dt(&v["discarded_at"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::channel::ActiveModel {
            id: Set(id),
            public_id: Set(public_id),
            title: Set(title),
            last_message_at: Set(last_message_at),
            latest_message_id: Set(latest_message_id),
            members_count: Set(members_count),
            image_path: Set(image_path),
            group: Set(group),
            notification_forced_at: Set(notification_forced_at),
            owner_username: Set(owner_username),
            discarded_at: Set(discarded_at),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => {
                channel_out += 1;
                imported_channel_ids.insert(id);
            }
            Err(e) => {
                println!("Error inserting channel {}: {}", id, e);
                channel_skip += 1;
            }
        }
    }

    // 4. channel_memberships (from message_thread_memberships)
    let membership_in = legacy_memberships.len();
    let mut membership_out = 0;
    let mut membership_skip = 0;
    let mut imported_membership_ids = HashSet::new();

    for v in legacy_memberships {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            membership_skip += 1;
            continue;
        }

        let user_id = get_i64(&v["user_id"]);
        let membership_id = get_i64(&v["organization_membership_id"]);
        let username = resolve_username(
            mapping,
            user_id,
            membership_id,
            &format!("message_thread_memberships[{id}]"),
        )?;

        let last_read_at =
            get_dt(&v["last_read_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let manually_marked_unread_at = get_dt(&v["manually_marked_unread_at"]);
        let notification_level = get_i32(&v["notification_level"]).unwrap_or(0);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::channel_membership::ActiveModel {
            id: Set(id),
            channel_id: Set(channel_id),
            username: Set(username),
            last_read_at: Set(last_read_at),
            manually_marked_unread_at: Set(manually_marked_unread_at),
            notification_level: Set(notification_level),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => {
                membership_out += 1;
                imported_membership_ids.insert(id);
            }
            Err(e) => {
                println!("Error inserting membership {}: {}", id, e);
                membership_skip += 1;
            }
        }
    }

    // 5. channel_membership_updates (from message_thread_membership_updates)
    let membership_update_in = legacy_updates.len();
    let mut membership_update_out = 0;
    let mut membership_update_skip = 0;

    for v in legacy_updates {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            membership_update_skip += 1;
            continue;
        }

        let actor_id = get_i64(&v["actor_id"]);
        let actor_username = resolve_username(
            mapping,
            actor_id,
            None,
            &format!("message_thread_membership_updates[{id}].actor_id"),
        )?;

        // Map added/removed usernames
        let map_names_json = |field: &str| -> Result<serde_json::Value, MegaError> {
            let mut resolved = Vec::new();
            if let Some(arr) = v[field].as_array() {
                for item in arr {
                    if let Some(id_val) = item.as_i64() {
                        resolved.push(resolve_username(
                            mapping,
                            Some(id_val),
                            None,
                            &format!("message_thread_membership_updates[{id}].{field}"),
                        )?);
                    } else if let Some(str_val) = item.as_str() {
                        if let Ok(id_val) = str_val.parse::<i64>() {
                            resolved.push(resolve_username(
                                mapping,
                                Some(id_val),
                                None,
                                &format!("message_thread_membership_updates[{id}].{field}"),
                            )?);
                        } else {
                            resolved.push(str_val.to_string());
                        }
                    }
                }
            }
            Ok(serde_json::Value::Array(
                resolved
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ))
        };

        let added = map_names_json("added_usernames")?;
        let removed = map_names_json("removed_usernames")?;
        let discarded_at = get_dt(&v["discarded_at"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::channel_membership_update::ActiveModel {
            id: Set(id),
            channel_id: Set(channel_id),
            actor_username: Set(actor_username),
            added_usernames: Set(added),
            removed_usernames: Set(removed),
            discarded_at: Set(discarded_at),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => membership_update_out += 1,
            Err(e) => {
                println!("Error inserting membership_update {}: {}", id, e);
                membership_update_skip += 1;
            }
        }
    }

    // 6. messages
    let message_in = legacy_messages.len();
    let mut message_out = 0;
    let mut message_skip = 0;
    let mut message_skip_oauth_integration = 0;
    let mut message_skip_calls = 0;
    let mut imported_message_ids = HashSet::new();

    for v in legacy_messages {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            message_skip += 1;
            continue;
        }

        // oauth_application_id / integration_id: skip
        let oauth_id = get_i64(&v["oauth_application_id"]);
        let integration_id = get_i64(&v["integration_id"]);
        if oauth_id.is_some() || integration_id.is_some() {
            message_skip += 1;
            message_skip_oauth_integration += 1;
            continue;
        }

        // call_id non-null: skip
        let call_id = get_i64(&v["call_id"]);
        if call_id.is_some() {
            message_skip += 1;
            message_skip_calls += 1;
            continue;
        }

        let sender_id = get_i64(&v["sender_id"]);
        let sender_username = if sender_id.is_some() {
            Some(resolve_username(
                mapping,
                sender_id,
                None,
                &format!("messages[{id}].sender_id"),
            )?)
        } else {
            None
        };

        // system_shared_post_id non-null: convert to system text
        let mut content = get_str(&v["content"]).unwrap_or_default();
        let post_id = get_i64(&v["system_shared_post_id"]);
        if post_id.is_some() {
            content = "[Shared Post]".to_string();
        }

        let public_id = get_str(&v["public_id"])
            .unwrap_or_else(crate::callisto::entity_ext::generate_public_id);
        let reply_to_id = get_i64(&v["reply_to_id"]);
        let unfurled_link = get_str(&v["unfurled_link"]);
        let discarded_at = get_dt(&v["discarded_at"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::message::ActiveModel {
            id: Set(id),
            channel_id: Set(channel_id),
            sender_username: Set(sender_username),
            content: Set(content),
            public_id: Set(public_id),
            reply_to_id: Set(reply_to_id),
            unfurled_link: Set(unfurled_link),
            discarded_at: Set(discarded_at),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => {
                message_out += 1;
                imported_message_ids.insert(id);
            }
            Err(e) => {
                println!("Error inserting message {}: {}", id, e);
                message_skip += 1;
            }
        }
    }

    // 7. message_notifications
    let notif_in = legacy_notifs.len();
    let mut notif_out = 0;
    let mut notif_skip = 0;

    for v in legacy_notifs {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let membership_id = get_i64(&v["message_thread_membership_id"]).unwrap_or(0);
        let message_id = get_i64(&v["message_id"]).unwrap_or(0);

        if id == 0
            || !imported_membership_ids.contains(&membership_id)
            || !imported_message_ids.contains(&message_id)
        {
            notif_skip += 1;
            continue;
        }

        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::message_notification::ActiveModel {
            id: Set(id),
            channel_membership_id: Set(membership_id),
            message_id: Set(message_id),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => notif_out += 1,
            Err(e) => {
                println!("Error inserting message_notification {}: {}", id, e);
                notif_skip += 1;
            }
        }
    }

    // 8. attachments
    let attachment_in = legacy_attachments.len();
    let mut attachment_out = 0;
    let mut attachment_skip = 0;

    for v in legacy_attachments {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let subject_type = get_str(&v["subject_type"]).unwrap_or_default();
        let subject_id = get_i64(&v["subject_id"]).unwrap_or(0);

        if id == 0 || subject_type != "Message" || !imported_message_ids.contains(&subject_id) {
            attachment_skip += 1;
            continue;
        }

        let public_id = get_str(&v["public_id"])
            .unwrap_or_else(crate::callisto::entity_ext::generate_public_id);
        let file_path = get_str(&v["file_path"]).unwrap_or_default();
        let file_type = get_str(&v["file_type"]).unwrap_or_default();
        let preview_file_path = get_str(&v["preview_file_path"]);
        let width = get_i32(&v["width"]);
        let height = get_i32(&v["height"]);
        let duration = get_i32(&v["duration"]);
        let position = get_i32(&v["position"]).unwrap_or(1);
        let name = get_str(&v["name"]).unwrap_or_default();
        let size = get_i64(&v["size"]).unwrap_or(0);
        let gallery_id = get_i64(&v["gallery_id"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::attachment::ActiveModel {
            id: Set(id),
            public_id: Set(public_id),
            file_path: Set(file_path),
            file_type: Set(file_type),
            subject_type: Set(subject_type),
            subject_id: Set(subject_id),
            preview_file_path: Set(preview_file_path),
            width: Set(width),
            height: Set(height),
            duration: Set(duration),
            position: Set(position),
            name: Set(name),
            size: Set(size),
            gallery_id: Set(gallery_id),
            discarded_at: Set(None),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => attachment_out += 1,
            Err(e) => {
                println!("Error inserting attachment {}: {}", id, e);
                attachment_skip += 1;
            }
        }
    }

    // 9. reactions
    let reaction_in = legacy_reactions.len();
    let mut reaction_out = 0;
    let mut reaction_skip = 0;

    for v in legacy_reactions {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let subject_type = get_str(&v["subject_type"]).unwrap_or_default();
        let subject_id = get_i64(&v["subject_id"]).unwrap_or(0);

        if id == 0 || subject_type != "Message" || !imported_message_ids.contains(&subject_id) {
            reaction_skip += 1;
            continue;
        }

        let user_id = get_i64(&v["user_id"]);
        let membership_id = get_i64(&v["organization_membership_id"]);
        let username =
            resolve_username(mapping, user_id, membership_id, &format!("reactions[{id}]"))?;

        let public_id = get_str(&v["public_id"])
            .unwrap_or_else(crate::callisto::entity_ext::generate_public_id);
        let content = get_str(&v["content"]);

        let custom_id_raw = get_i64(&v["custom_reaction_id"]);
        let custom_reaction_id =
            custom_id_raw.filter(|cid| imported_custom_reactions.contains(cid));

        let discarded_at = get_dt(&v["discarded_at"]);
        let created_at = get_dt(&v["created_at"]).unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let updated_at = get_dt(&v["updated_at"]).unwrap_or(created_at);

        let am = crate::callisto::reactions::ActiveModel {
            id: Set(id),
            public_id: Set(public_id),
            content: Set(content),
            subject_id: Set(subject_id),
            subject_type: Set(subject_type),
            username: Set(username),
            custom_reaction_id: Set(custom_reaction_id),
            discarded_at: Set(discarded_at),
            created_at: Set(created_at),
            updated_at: Set(updated_at),
        };

        match am.insert(conn).await {
            Ok(_) => reaction_out += 1,
            Err(e) => {
                println!("Error inserting reaction {}: {}", id, e);
                reaction_skip += 1;
            }
        }
    }

    println!("\n=== Migration Report ===");
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "Table", "Legacy", "Imported", "Skipped", "Conflicts"
    );
    println!("{}", "-".repeat(70));
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "custom_reactions",
        custom_reaction_in,
        custom_reaction_out,
        custom_reaction_skip,
        custom_reaction_conflict
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "open_graph_links", og_in, og_out, og_skip, 0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "channels", channel_in, channel_out, channel_skip, 0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "channel_memberships", membership_in, membership_out, membership_skip, 0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "channel_membership_updates",
        membership_update_in,
        membership_update_out,
        membership_update_skip,
        0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "messages", message_in, message_out, message_skip, 0
    );
    println!(
        "  (Skipped by app/oauth: {}, Skipped by calls: {})",
        message_skip_oauth_integration, message_skip_calls
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "message_notifications", notif_in, notif_out, notif_skip, 0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "attachments", attachment_in, attachment_out, attachment_skip, 0
    );
    println!(
        "{:<28} | {:<8} | {:<8} | {:<8} | {:<8}",
        "reactions", reaction_in, reaction_out, reaction_skip, 0
    );
    println!("{}", "-".repeat(70));

    // Verification: query actual DB row counts and compare with imported counts.
    println!("\n=== Verification (DB actual vs imported) ===");
    let db_counts = vec![
        (
            "custom_reactions",
            custom_reaction_out,
            crate::callisto::custom_reaction::Entity::find()
                .count(conn)
                .await
                .unwrap_or(0),
        ),
        (
            "channels",
            channel_out,
            crate::callisto::channel::Entity::find()
                .count(conn)
                .await
                .unwrap_or(0),
        ),
        (
            "messages",
            message_out,
            crate::callisto::message::Entity::find()
                .count(conn)
                .await
                .unwrap_or(0),
        ),
        (
            "attachments",
            attachment_out,
            crate::callisto::attachment::Entity::find()
                .count(conn)
                .await
                .unwrap_or(0),
        ),
        (
            "reactions",
            reaction_out,
            crate::callisto::reactions::Entity::find()
                .count(conn)
                .await
                .unwrap_or(0),
        ),
    ];
    let mut discrepancies = 0;
    for (table, imported, actual) in &db_counts {
        let status = if imported == actual { "OK" } else { "MISMATCH" };
        if imported != actual {
            discrepancies += 1;
        }
        println!(
            "{:<28} | imported={:<8} | db_actual={:<8} | {}",
            table, imported, actual, status
        );
    }
    if discrepancies > 0 {
        println!(
            "\nWARNING: {discrepancies} table(s) have count mismatches. Review the migration output for errors."
        );
    } else {
        println!("\nAll verified table counts match imported counts.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use sea_orm::EntityTrait;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        config::testing::{EnvVarGuard, TestConfigBuilder, env_lock},
        jupiter::{
            storage::{
                init::database_connection,
                object_storage::{
                    MegaObjectStorageWrapper, ObjectStorageProvider, mock_object_storage,
                    set_object_storage_provider,
                },
            },
            tests::{test_db_config, test_storage},
        },
    };

    struct TestObjectStorageProvider;

    #[async_trait::async_trait]
    impl ObjectStorageProvider for TestObjectStorageProvider {
        async fn build(
            &self,
            _cfg: &crate::config::ObjectStorageConfig,
        ) -> Result<MegaObjectStorageWrapper, MegaError> {
            Ok(mock_object_storage())
        }
    }

    fn ensure_test_object_storage_provider() {
        set_object_storage_provider(Arc::new(TestObjectStorageProvider));
    }

    fn write_empty_required_exports(input_dir: &Path) {
        for filename in REQUIRED_LEGACY_EXPORTS {
            fs::write(input_dir.join(filename), "[]").unwrap();
        }
    }

    fn write_sanitized_legacy_export_fixture(input_dir: &Path, mapping_file: &Path) {
        fs::write(
            mapping_file,
            r#"{
                "user_mappings": {
                    "1": "alice",
                    "2": "bob"
                },
                "org_membership_mappings": {
                    "11": "bob"
                }
            }"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("custom_reactions.json"),
            r#"[
                {
                    "id": 4001,
                    "public_id": "cr4001xxxxxx",
                    "name": "wave",
                    "file_path": "chat/custom-reactions/wave.png",
                    "file_type": "image/png",
                    "user_id": 1,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("open_graph_links.json"),
            r#"[
                {
                    "id": 6001,
                    "url": "https://example.invalid/sanitized-chat-export",
                    "title": "Sanitized Export",
                    "image_path": "chat/open-graph/image.png",
                    "favicon_path": "chat/open-graph/favicon.ico",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("message_threads.json"),
            r#"[
                {
                    "id": 1001,
                    "public_id": "thread1001xx",
                    "title": "Sanitized General",
                    "last_message_at": "2026-05-30 12:03:00",
                    "latest_message_id": 3003,
                    "members_count": 2,
                    "image_path": null,
                    "group": true,
                    "owner_id": 1,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:03:00"
                },
                {
                    "id": 1002,
                    "public_id": "thread1002xx",
                    "title": "Integration Thread",
                    "last_message_at": "2026-05-30 12:04:00",
                    "owner_id": 1,
                    "integration_id": 42,
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("message_thread_memberships.json"),
            r#"[
                {
                    "id": 2001,
                    "message_thread_id": 1001,
                    "user_id": 1,
                    "last_read_at": "2026-05-30 12:00:00",
                    "notification_level": 1,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 2002,
                    "message_thread_id": 1001,
                    "organization_membership_id": 11,
                    "last_read_at": "2026-05-30 12:00:00",
                    "notification_level": 0,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 2003,
                    "message_thread_id": 1002,
                    "user_id": 1,
                    "last_read_at": "2026-05-30 12:04:00",
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("message_thread_membership_updates.json"),
            r#"[
                {
                    "id": 7001,
                    "message_thread_id": 1001,
                    "actor_id": 1,
                    "added_usernames": [2],
                    "removed_usernames": ["1"],
                    "created_at": "2026-05-30 12:02:00",
                    "updated_at": "2026-05-30 12:02:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("messages.json"),
            r#"[
                {
                    "id": 3001,
                    "message_thread_id": 1001,
                    "sender_id": 1,
                    "content": "Hello from sanitized export",
                    "public_id": "msg3001xxxxx",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 3002,
                    "message_thread_id": 1001,
                    "sender_id": 2,
                    "content": "Reply from mapped user",
                    "public_id": "msg3002xxxxx",
                    "reply_to_id": 3001,
                    "created_at": "2026-05-30 12:01:00",
                    "updated_at": "2026-05-30 12:01:00"
                },
                {
                    "id": 3003,
                    "message_thread_id": 1001,
                    "sender_id": 1,
                    "content": "Shared a source post",
                    "public_id": "msg3003xxxxx",
                    "system_shared_post_id": 99,
                    "created_at": "2026-05-30 12:03:00",
                    "updated_at": "2026-05-30 12:03:00"
                },
                {
                    "id": 3004,
                    "message_thread_id": 1001,
                    "sender_id": 1,
                    "content": "Call system message",
                    "public_id": "msg3004xxxxx",
                    "call_id": 55,
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                },
                {
                    "id": 3005,
                    "message_thread_id": 1002,
                    "sender_id": 1,
                    "content": "Integration thread message",
                    "public_id": "msg3005xxxxx",
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("message_notifications.json"),
            r#"[
                {
                    "id": 8001,
                    "message_thread_membership_id": 2001,
                    "message_id": 3002,
                    "created_at": "2026-05-30 12:01:00",
                    "updated_at": "2026-05-30 12:01:00"
                },
                {
                    "id": 8002,
                    "message_thread_membership_id": 2003,
                    "message_id": 3005,
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("attachments.json"),
            r#"[
                {
                    "id": 9001,
                    "public_id": "att9001xxxxx",
                    "file_path": "chat/attachments/sanitized.png",
                    "file_type": "image/png",
                    "subject_type": "Message",
                    "subject_id": 3001,
                    "preview_file_path": null,
                    "width": 800,
                    "height": 600,
                    "duration": null,
                    "position": 1,
                    "name": "sanitized.png",
                    "size": 102400,
                    "gallery_id": null,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 9002,
                    "public_id": "att9002xxxxx",
                    "file_path": "chat/attachments/post.png",
                    "file_type": "image/png",
                    "subject_type": "Post",
                    "subject_id": 999,
                    "name": "post.png",
                    "size": 1024,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 9003,
                    "public_id": "att9003xxxxx",
                    "file_path": "chat/attachments/call.png",
                    "file_type": "image/png",
                    "subject_type": "Message",
                    "subject_id": 3004,
                    "name": "call.png",
                    "size": 1024,
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("reactions.json"),
            r#"[
                {
                    "id": 5001,
                    "public_id": "rx5001xxxxxx",
                    "subject_type": "Message",
                    "subject_id": 3001,
                    "user_id": 2,
                    "content": "clap",
                    "created_at": "2026-05-30 12:02:00",
                    "updated_at": "2026-05-30 12:02:00"
                },
                {
                    "id": 5002,
                    "public_id": "rx5002xxxxxx",
                    "subject_type": "Message",
                    "subject_id": 3002,
                    "organization_membership_id": 11,
                    "content": null,
                    "custom_reaction_id": 4001,
                    "created_at": "2026-05-30 12:03:00",
                    "updated_at": "2026-05-30 12:03:00"
                },
                {
                    "id": 5003,
                    "public_id": "rx5003xxxxxx",
                    "subject_type": "Post",
                    "subject_id": 999,
                    "user_id": 1,
                    "content": "heart",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 5004,
                    "public_id": "rx5004xxxxxx",
                    "subject_type": "Message",
                    "subject_id": 3004,
                    "user_id": 2,
                    "content": "fire",
                    "created_at": "2026-05-30 12:04:00",
                    "updated_at": "2026-05-30 12:04:00"
                }
            ]"#,
        )
        .unwrap();
    }

    #[test]
    fn chat_migrate_exec_imports_sanitized_legacy_export_fixture() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();
        let mapping_file = temp.path().join("mapping.json");
        write_sanitized_legacy_export_fixture(&input_dir, &mapping_file);

        let lock = env_lock();
        let base_dir = temp.path().join("mega-base");
        let cache_dir = temp.path().join("mega-cache");
        let _base_guard = EnvVarGuard::set(
            &lock,
            "MEGA_BASE_DIR",
            base_dir.to_str().expect("utf-8 base dir"),
        );
        let _cache_guard = EnvVarGuard::set(
            &lock,
            "MEGA_CACHE_DIR",
            cache_dir.to_str().expect("utf-8 cache dir"),
        );

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db_config = runtime.block_on(test_db_config(temp.path()));
        drop(runtime);

        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let config = TestConfigBuilder::new(temp.path().join("config"))
            .database_url(db_config.db_url)
            .redis_url(redis_url)
            .build();
        let query_config = config.clone();
        ensure_test_object_storage_provider();

        let args = cli()
            .try_get_matches_from([
                "chat-migrate",
                "--input-dir",
                input_dir.to_str().expect("utf-8 input dir"),
                "--user-mapping",
                mapping_file.to_str().expect("utf-8 mapping file"),
            ])
            .unwrap();

        exec(
            CommandContext {
                config: Some(config),
                ..Default::default()
            },
            &args,
        )
        .unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let conn = database_connection(&query_config.database).await.unwrap();

            assert_eq!(
                crate::callisto::custom_reaction::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::open_graph_link::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::channel::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::channel_membership::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                crate::callisto::channel_membership_update::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::message::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                3
            );
            assert_eq!(
                crate::callisto::message_notification::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::attachment::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(
                crate::callisto::reactions::Entity::find()
                    .count(&conn)
                    .await
                    .unwrap(),
                2
            );

            let shared_post = crate::callisto::message::Entity::find_by_id(3003)
                .one(&conn)
                .await
                .unwrap()
                .expect("shared-post message should import");
            assert_eq!(shared_post.content, "[Shared Post]");

            let reaction = crate::callisto::reactions::Entity::find_by_id(5002)
                .one(&conn)
                .await
                .unwrap()
                .expect("org-membership reaction should import");
            assert_eq!(reaction.username, "bob");
            assert_eq!(reaction.custom_reaction_id, Some(4001));
        });
    }

    #[tokio::test]
    async fn test_migration_lifecycle() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();

        let mapping_content = r#"{
            "user_mappings": {
                "1": "alice",
                "2": "bob"
            },
            "org_membership_mappings": {
                "10": "alice",
                "11": "bob"
            }
        }"#;
        fs::write(temp.path().join("mapping.json"), mapping_content).unwrap();

        fs::write(
            input_dir.join("message_threads.json"),
            r#"[
                {
                    "id": 101,
                    "public_id": "thread101xxx",
                    "title": "General",
                    "last_message_at": "2026-05-30 12:00:00",
                    "owner_id": 1,
                    "group": true,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("message_thread_memberships.json"),
            r#"[
                {
                    "id": 201,
                    "message_thread_id": 101,
                    "user_id": 1,
                    "last_read_at": "2026-05-30 12:00:00",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 202,
                    "message_thread_id": 101,
                    "user_id": 2,
                    "last_read_at": "2026-05-30 12:00:00",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("messages.json"),
            r#"[
                {
                    "id": 301,
                    "message_thread_id": 101,
                    "sender_id": 1,
                    "content": "Hello bob!",
                    "public_id": "msg301xxxxxx",
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                },
                {
                    "id": 302,
                    "message_thread_id": 101,
                    "sender_id": 2,
                    "content": "Hi alice!",
                    "public_id": "msg302xxxxxx",
                    "reply_to_id": 301,
                    "created_at": "2026-05-30 12:01:00",
                    "updated_at": "2026-05-30 12:01:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("custom_reactions.json"),
            r#"[
                {
                    "id": 401,
                    "public_id": "react401xxxx",
                    "name": "rocket",
                    "file_path": "path/to/rocket.png",
                    "file_type": "image/png",
                    "user_id": 1,
                    "created_at": "2026-05-30 12:00:00",
                    "updated_at": "2026-05-30 12:00:00"
                }
            ]"#,
        )
        .unwrap();

        fs::write(
            input_dir.join("reactions.json"),
            r#"[
                {
                    "id": 501,
                    "public_id": "rx501xxxxxxx",
                    "subject_type": "Message",
                    "subject_id": 301,
                    "user_id": 2,
                    "content": "🎉",
                    "created_at": "2026-05-30 12:02:00",
                    "updated_at": "2026-05-30 12:02:00"
                }
            ]"#,
        )
        .unwrap();

        let db_dir = tempdir().unwrap();
        let storage = test_storage(db_dir.path()).await;
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();

        let threads_data: Vec<serde_json::Value> = serde_json::from_str(
            &fs::read_to_string(input_dir.join("message_threads.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(threads_data.len(), 1);

        let t = &threads_data[0];
        let id = t["id"].as_i64().unwrap();
        let public_id = t["public_id"].as_str().unwrap().to_string();
        let title = t["title"].as_str().map(|s| s.to_string());
        let last_message_at = parse_dt(t["last_message_at"].as_str().unwrap());
        let owner_username = "alice".to_string();

        let am = crate::callisto::channel::ActiveModel {
            id: Set(id),
            public_id: Set(public_id),
            title: Set(title),
            last_message_at: Set(last_message_at),
            owner_username: Set(owner_username),
            created_at: Set(last_message_at),
            updated_at: Set(last_message_at),
            ..Default::default()
        };
        am.insert(conn).await.unwrap();

        let inserted = crate::callisto::channel::Entity::find()
            .all(conn)
            .await
            .unwrap();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].id, 101);
        assert_eq!(inserted[0].title.as_deref(), Some("General"));
    }

    #[tokio::test]
    async fn test_migration_e2e_full_fixture() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();

        let mapping = MigrationMapping {
            user_mappings: HashMap::from([
                ("1".to_string(), "alice".to_string()),
                ("2".to_string(), "bob".to_string()),
                ("3".to_string(), "carol".to_string()),
                ("4".to_string(), "dave".to_string()),
            ]),
            org_membership_mappings: HashMap::from([
                ("10".to_string(), "alice".to_string()),
                ("11".to_string(), "bob".to_string()),
            ]),
        };

        let legacy_custom_reactions = vec![
            serde_json::json!({
                "id": 401,
                "public_id": "cr401xxxxxxx",
                "name": "rocket",
                "file_path": "path/to/rocket.png",
                "file_type": "image/png",
                "user_id": 1,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 402,
                "public_id": "cr402xxxxxxx",
                "name": "Rocket",
                "file_path": "path/to/rocket2.png",
                "file_type": "image/png",
                "user_id": 2,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 403,
                "public_id": "cr403xxxxxxx",
                "name": "thumbsup",
                "file_path": "path/to/thumbsup.png",
                "file_type": "image/png",
                "user_id": 3,
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
        ];

        let legacy_og = vec![
            serde_json::json!({
                "id": 601,
                "url": "https://example.com/article",
                "title": "Example Article",
                "image_path": "path/to/og_image.png",
                "favicon_path": "path/to/favicon.ico",
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 602,
                "url": "https://example.com/article",
                "title": "Duplicate URL",
                "image_path": null,
                "favicon_path": null,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
        ];

        let legacy_threads = vec![
            serde_json::json!({
                "id": 101,
                "public_id": "thread101xxx",
                "title": "General",
                "last_message_at": "2026-05-30 12:00:00",
                "latest_message_id": 303,
                "members_count": 3,
                "image_path": null,
                "group": true,
                "owner_id": 1,
                "discarded_at": null,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 102,
                "public_id": "thread102xxx",
                "title": "DM alice-bob",
                "last_message_at": "2026-05-30 12:01:00",
                "owner_id": 1,
                "group": false,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 103,
                "public_id": "thread103xxx",
                "title": "Integration DM",
                "last_message_at": "2026-05-30 12:02:00",
                "owner_id": 1,
                "group": false,
                "integration_id": 42,
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 104,
                "public_id": "thread104xxx",
                "title": "OAuth App Thread",
                "last_message_at": "2026-05-30 12:03:00",
                "owner_id": 1,
                "group": false,
                "oauth_application_id": 7,
                "created_at": "2026-05-30 12:03:00",
                "updated_at": "2026-05-30 12:03:00"
            }),
        ];

        let legacy_memberships = vec![
            serde_json::json!({
                "id": 201,
                "message_thread_id": 101,
                "user_id": 1,
                "last_read_at": "2026-05-30 12:00:00",
                "notification_level": 1,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 202,
                "message_thread_id": 101,
                "user_id": 2,
                "last_read_at": "2026-05-30 12:00:00",
                "notification_level": 0,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 203,
                "message_thread_id": 101,
                "user_id": 3,
                "last_read_at": "2026-05-30 12:00:00",
                "notification_level": 0,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 204,
                "message_thread_id": 102,
                "user_id": 1,
                "last_read_at": "2026-05-30 12:01:00",
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 205,
                "message_thread_id": 102,
                "user_id": 2,
                "last_read_at": "2026-05-30 12:01:00",
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 206,
                "message_thread_id": 103,
                "user_id": 1,
                "last_read_at": "2026-05-30 12:02:00",
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 207,
                "message_thread_id": 104,
                "user_id": 1,
                "last_read_at": "2026-05-30 12:03:00",
                "created_at": "2026-05-30 12:03:00",
                "updated_at": "2026-05-30 12:03:00"
            }),
        ];

        let legacy_updates = vec![
            serde_json::json!({
                "id": 701,
                "message_thread_id": 101,
                "actor_id": 1,
                "added_usernames": [2, 3],
                "removed_usernames": [],
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 702,
                "message_thread_id": 101,
                "actor_id": 1,
                "added_usernames": [],
                "removed_usernames": [3],
                "created_at": "2026-05-30 12:05:00",
                "updated_at": "2026-05-30 12:05:00"
            }),
            serde_json::json!({
                "id": 703,
                "message_thread_id": 103,
                "actor_id": 1,
                "added_usernames": [2],
                "removed_usernames": [],
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
        ];

        let legacy_messages = vec![
            serde_json::json!({
                "id": 301,
                "message_thread_id": 101,
                "sender_id": 1,
                "content": "Hello everyone!",
                "public_id": "msg301xxxxxx",
                "reply_to_id": null,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 302,
                "message_thread_id": 101,
                "sender_id": 2,
                "content": "Hi alice!",
                "public_id": "msg302xxxxxx",
                "reply_to_id": 301,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 303,
                "message_thread_id": 101,
                "sender_id": 3,
                "content": "Hey folks",
                "public_id": "msg303xxxxxx",
                "reply_to_id": null,
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 304,
                "message_thread_id": 101,
                "sender_id": 1,
                "content": "Shared a post",
                "public_id": "msg304xxxxxx",
                "system_shared_post_id": 99,
                "created_at": "2026-05-30 12:03:00",
                "updated_at": "2026-05-30 12:03:00"
            }),
            serde_json::json!({
                "id": 305,
                "message_thread_id": 101,
                "sender_id": 1,
                "content": "Call started",
                "public_id": "msg305xxxxxx",
                "call_id": 55,
                "created_at": "2026-05-30 12:04:00",
                "updated_at": "2026-05-30 12:04:00"
            }),
            serde_json::json!({
                "id": 306,
                "message_thread_id": 101,
                "sender_id": 1,
                "content": "Integration message",
                "public_id": "msg306xxxxxx",
                "integration_id": 42,
                "created_at": "2026-05-30 12:05:00",
                "updated_at": "2026-05-30 12:05:00"
            }),
            serde_json::json!({
                "id": 307,
                "message_thread_id": 102,
                "sender_id": 1,
                "content": "DM from alice",
                "public_id": "msg307xxxxxx",
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 308,
                "message_thread_id": 102,
                "sender_id": 2,
                "content": "DM from bob",
                "public_id": "msg308xxxxxx",
                "reply_to_id": 307,
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 309,
                "message_thread_id": 103,
                "sender_id": 1,
                "content": "Integration DM message",
                "public_id": "msg309xxxxxx",
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 310,
                "message_thread_id": 104,
                "sender_id": 1,
                "content": "OAuth app message",
                "public_id": "msg310xxxxxx",
                "created_at": "2026-05-30 12:03:00",
                "updated_at": "2026-05-30 12:03:00"
            }),
        ];

        let legacy_notifs = vec![
            serde_json::json!({
                "id": 801,
                "message_thread_membership_id": 201,
                "message_id": 302,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 802,
                "message_thread_membership_id": 202,
                "message_id": 301,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 803,
                "message_thread_membership_id": 206,
                "message_id": 309,
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
        ];

        let legacy_attachments = vec![
            serde_json::json!({
                "id": 901,
                "public_id": "att901xxxxxx",
                "file_path": "chat/attachments/file1.png",
                "file_type": "image/png",
                "subject_type": "Message",
                "subject_id": 301,
                "preview_file_path": "chat/attachments/file1_thumb.png",
                "width": 800,
                "height": 600,
                "duration": null,
                "position": 1,
                "name": "screenshot.png",
                "size": 102400,
                "gallery_id": null,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
            serde_json::json!({
                "id": 902,
                "public_id": "att902xxxxxx",
                "file_path": "chat/attachments/file2.jpg",
                "file_type": "image/jpeg",
                "subject_type": "Message",
                "subject_id": 302,
                "preview_file_path": null,
                "width": 1024,
                "height": 768,
                "duration": null,
                "position": 1,
                "name": "photo.jpg",
                "size": 204800,
                "gallery_id": 1,
                "created_at": "2026-05-30 12:01:00",
                "updated_at": "2026-05-30 12:01:00"
            }),
            serde_json::json!({
                "id": 903,
                "public_id": "att903xxxxxx",
                "file_path": "chat/attachments/file3.pdf",
                "file_type": "application/pdf",
                "subject_type": "Message",
                "subject_id": 305,
                "preview_file_path": null,
                "width": null,
                "height": null,
                "duration": null,
                "position": 1,
                "name": "doc.pdf",
                "size": 51200,
                "gallery_id": null,
                "created_at": "2026-05-30 12:04:00",
                "updated_at": "2026-05-30 12:04:00"
            }),
            serde_json::json!({
                "id": 904,
                "public_id": "att904xxxxxx",
                "file_path": "chat/attachments/file4.png",
                "file_type": "image/png",
                "subject_type": "Post",
                "subject_id": 999,
                "preview_file_path": null,
                "width": 100,
                "height": 100,
                "duration": null,
                "position": 1,
                "name": "post_attachment.png",
                "size": 1024,
                "gallery_id": null,
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
        ];

        let legacy_reactions = vec![
            serde_json::json!({
                "id": 501,
                "public_id": "rx501xxxxxxx",
                "subject_type": "Message",
                "subject_id": 301,
                "user_id": 2,
                "content": "🎉",
                "created_at": "2026-05-30 12:02:00",
                "updated_at": "2026-05-30 12:02:00"
            }),
            serde_json::json!({
                "id": 502,
                "public_id": "rx502xxxxxxx",
                "subject_type": "Message",
                "subject_id": 301,
                "user_id": 3,
                "content": "👍",
                "created_at": "2026-05-30 12:03:00",
                "updated_at": "2026-05-30 12:03:00"
            }),
            serde_json::json!({
                "id": 503,
                "public_id": "rx503xxxxxxx",
                "subject_type": "Message",
                "subject_id": 302,
                "user_id": 1,
                "content": null,
                "custom_reaction_id": 401,
                "created_at": "2026-05-30 12:04:00",
                "updated_at": "2026-05-30 12:04:00"
            }),
            serde_json::json!({
                "id": 504,
                "public_id": "rx504xxxxxxx",
                "subject_type": "Message",
                "subject_id": 305,
                "user_id": 2,
                "content": "🔥",
                "created_at": "2026-05-30 12:05:00",
                "updated_at": "2026-05-30 12:05:00"
            }),
            serde_json::json!({
                "id": 505,
                "public_id": "rx505xxxxxxx",
                "subject_type": "Post",
                "subject_id": 999,
                "user_id": 1,
                "content": "❤️",
                "created_at": "2026-05-30 12:00:00",
                "updated_at": "2026-05-30 12:00:00"
            }),
        ];

        let input = MigrationInput {
            mapping,
            custom_reactions: legacy_custom_reactions,
            open_graph_links: legacy_og,
            threads: legacy_threads,
            memberships: legacy_memberships,
            membership_updates: legacy_updates,
            messages: legacy_messages,
            notifications: legacy_notifs,
            attachments: legacy_attachments,
            reactions: legacy_reactions,
        };

        let db_dir = tempdir().unwrap();
        let storage = test_storage(db_dir.path()).await;
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();

        run_migration(conn, &input).await.unwrap();

        let db_custom_reactions = crate::callisto::custom_reaction::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_custom_reactions, 2,
            "custom_reactions: rocket (401) imported, Rocket (402) skipped as lowercase conflict, thumbsup (403) imported"
        );

        let db_og = crate::callisto::open_graph_link::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_og, 1,
            "open_graph_links: first URL imported, duplicate URL skipped"
        );

        let db_channels = crate::callisto::channel::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_channels, 2,
            "channels: 101 (General) and 102 (DM) imported, 103 (integration) and 104 (oauth) skipped"
        );

        let db_memberships = crate::callisto::channel_membership::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_memberships, 5,
            "memberships: 201-205 imported (channels 101+102), 206-207 skipped (channels 103+104)"
        );

        let db_updates = crate::callisto::channel_membership_update::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_updates, 2,
            "membership_updates: 701+702 imported (channel 101), 703 skipped (channel 103)"
        );

        let db_messages = crate::callisto::message::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_messages, 6,
            "messages: 301-304 imported (channel 101), 307-308 imported (channel 102), 305 (call) skipped, 306 (integration) skipped, 309-310 skipped (channels 103+104)"
        );

        let db_notifs = crate::callisto::message_notification::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_notifs, 2,
            "message_notifications: 801+802 imported (valid membership+message), 803 skipped (membership 206 not imported)"
        );

        let db_attachments = crate::callisto::attachment::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_attachments, 2,
            "attachments: 901+902 imported (subject_type=Message, subject_id in imported messages), 903 skipped (subject_id 305 not imported), 904 skipped (subject_type=Post)"
        );

        let db_reactions = crate::callisto::reactions::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(
            db_reactions, 3,
            "reactions: 501+502+503 imported (subject_type=Message, subject_id in imported messages), 504 skipped (subject_id 305 not imported), 505 skipped (subject_type=Post)"
        );

        let channel_101 = crate::callisto::channel::Entity::find_by_id(101)
            .one(conn)
            .await
            .unwrap()
            .expect("channel 101 should exist");
        assert_eq!(channel_101.owner_username, "alice");
        assert_eq!(channel_101.title.as_deref(), Some("General"));

        let channel_102 = crate::callisto::channel::Entity::find_by_id(102)
            .one(conn)
            .await
            .unwrap()
            .expect("channel 102 should exist");
        assert_eq!(channel_102.owner_username, "alice");
        assert_eq!(channel_102.title.as_deref(), Some("DM alice-bob"));

        let msg_304 = crate::callisto::message::Entity::find_by_id(304)
            .one(conn)
            .await
            .unwrap()
            .expect("message 304 should exist");
        assert_eq!(msg_304.content, "[Shared Post]");

        let msg_301 = crate::callisto::message::Entity::find_by_id(301)
            .one(conn)
            .await
            .unwrap()
            .expect("message 301 should exist");
        assert_eq!(msg_301.sender_username, Some("alice".to_string()));

        let msg_302 = crate::callisto::message::Entity::find_by_id(302)
            .one(conn)
            .await
            .unwrap()
            .expect("message 302 should exist");
        assert_eq!(msg_302.reply_to_id, Some(301));

        let cr_401 = crate::callisto::custom_reaction::Entity::find_by_id(401)
            .one(conn)
            .await
            .unwrap()
            .expect("custom_reaction 401 should exist");
        assert_eq!(cr_401.username, "alice");
        assert_eq!(cr_401.name, "rocket");

        let cr_403 = crate::callisto::custom_reaction::Entity::find_by_id(403)
            .one(conn)
            .await
            .unwrap()
            .expect("custom_reaction 403 should exist");
        assert_eq!(cr_403.username, "carol");
        assert_eq!(cr_403.name, "thumbsup");

        let rx_503 = crate::callisto::reactions::Entity::find_by_id(503)
            .one(conn)
            .await
            .unwrap()
            .expect("reaction 503 should exist");
        assert_eq!(rx_503.custom_reaction_id, Some(401));
        assert_eq!(rx_503.username, "alice");

        let att_901 = crate::callisto::attachment::Entity::find_by_id(901)
            .one(conn)
            .await
            .unwrap()
            .expect("attachment 901 should exist");
        assert_eq!(att_901.subject_id, 301);
        assert_eq!(att_901.name, "screenshot.png");
        assert_eq!(att_901.size, 102400);

        let notif_801 = crate::callisto::message_notification::Entity::find_by_id(801)
            .one(conn)
            .await
            .unwrap()
            .expect("notification 801 should exist");
        assert_eq!(notif_801.message_id, 302);
        assert_eq!(notif_801.channel_membership_id, 201);

        let update_701 = crate::callisto::channel_membership_update::Entity::find_by_id(701)
            .one(conn)
            .await
            .unwrap()
            .expect("update 701 should exist");
        assert_eq!(update_701.actor_username, "alice");
        assert_eq!(update_701.channel_id, 101);
    }

    #[tokio::test]
    async fn empty_db_guard_detects_existing_data() {
        let temp = tempdir().unwrap();
        let storage = test_storage(temp.path()).await;
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();

        // Fresh DB: no channels — guard should pass (count == 0).
        let count = crate::callisto::channel::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert_eq!(count, 0, "fresh DB should have zero channels");

        // Insert a channel — guard should now detect existing data.
        let now = chrono::Utc::now().naive_utc();
        let am = crate::callisto::channel::ActiveModel {
            id: Set(1),
            public_id: Set("guardch0001".to_string()),
            title: Set(Some("Test".to_string())),
            last_message_at: Set(now),
            owner_username: Set("alice".to_string()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        am.insert(conn).await.unwrap();

        let count = crate::callisto::channel::Entity::find()
            .count(conn)
            .await
            .unwrap();
        assert!(count > 0, "guard should detect existing channel data");
    }

    #[test]
    fn validate_legacy_export_set_requires_all_nine_files() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();
        write_empty_required_exports(&input_dir);
        fs::remove_file(input_dir.join("messages.json")).unwrap();

        let err = validate_legacy_export_set(&input_dir).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing required file(s): messages.json"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_legacy_export_set_accepts_complete_empty_export() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();
        write_empty_required_exports(&input_dir);

        validate_legacy_export_set(&input_dir).unwrap();
    }

    #[test]
    fn read_legacy_json_rejects_non_array_export() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("messages.json"), r#"{"id": 1}"#).unwrap();

        let err = read_legacy_json(temp.path(), "messages.json").unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to parse legacy chat export")
                && err.to_string().contains("as a JSON array"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_username_rejects_unmapped_user_id() {
        let mapping = MigrationMapping {
            user_mappings: HashMap::from([("1".to_string(), "alice".to_string())]),
            org_membership_mappings: HashMap::new(),
        };

        let err = resolve_username(&mapping, Some(2), None, "messages[301].sender_id").unwrap_err();
        assert!(
            err.to_string().contains("user_id=2")
                && err.to_string().contains("messages[301].sender_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_username_uses_org_membership_mapping_when_user_mapping_absent() {
        let mapping = MigrationMapping {
            user_mappings: HashMap::new(),
            org_membership_mappings: HashMap::from([("10".to_string(), "alice".to_string())]),
        };

        let username = resolve_username(&mapping, None, Some(10), "memberships[201]").unwrap();
        assert_eq!(username, "alice");
    }

    #[test]
    fn validate_chat_migration_mappings_rejects_unmapped_update_member_id() {
        let mapping = MigrationMapping {
            user_mappings: HashMap::from([("1".to_string(), "alice".to_string())]),
            org_membership_mappings: HashMap::new(),
        };
        let membership_updates = vec![serde_json::json!({
            "id": 701,
            "message_thread_id": 101,
            "actor_id": 1,
            "added_usernames": ["2"]
        })];
        let threads = vec![serde_json::json!({
            "id": 101,
            "owner_id": 1
        })];

        let err = validate_chat_migration_mappings(
            &mapping,
            &[],
            &threads,
            &[],
            &membership_updates,
            &[],
            &[],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("user_id=2")
                && err
                    .to_string()
                    .contains("message_thread_membership_updates[701].added_usernames"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_chat_migration_mappings_ignores_rows_outside_import_scope() {
        let mapping = MigrationMapping {
            user_mappings: HashMap::from([("1".to_string(), "alice".to_string())]),
            org_membership_mappings: HashMap::new(),
        };
        let skipped_threads = vec![serde_json::json!({
            "id": 101,
            "owner_id": 999,
            "oauth_application_id": 42
        })];
        let skipped_messages = vec![serde_json::json!({
            "id": 301,
            "message_thread_id": 101,
            "sender_id": 999
        })];

        validate_chat_migration_mappings(
            &mapping,
            &[],
            &skipped_threads,
            &[],
            &[],
            &skipped_messages,
            &[],
        )
        .unwrap();
    }
}
