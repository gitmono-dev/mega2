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
struct MigrationMapping {
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

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config = require_config(ctx, "chat-migrate")?;

    let input_dir = args.get_one::<String>("input-dir").unwrap();
    let mapping_file = args.get_one::<String>("user-mapping").unwrap();
    let dir_path = Path::new(input_dir);
    validate_legacy_export_set(dir_path)?;

    // 1. Read user mapping. Do this before AppContext startup so malformed
    // migration inputs fail before connecting to long-lived services.
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

    // Empty-DB guard: refuse to run if chat tables already contain data.
    // The migration inserts with explicit legacy IDs and is not idempotent;
    // re-running on a non-empty database would cause primary-key conflicts.
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

    println!("\n=== Starting Migration ===");

    // 1. custom_reactions
    let custom_reaction_in = legacy_custom_reactions.len();
    let mut custom_reaction_out = 0;
    let mut custom_reaction_skip = 0;
    let mut custom_reaction_conflict = 0;
    let mut imported_custom_reactions = HashSet::new();

    // Sort by created_at to keep earliest on conflict
    let mut custom_reaction_list = Vec::new();
    for v in &legacy_custom_reactions {
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
            &mapping,
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
    for v in &legacy_og {
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

    for v in &legacy_threads {
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
            &mapping,
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

    for v in &legacy_memberships {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            membership_skip += 1;
            continue;
        }

        let user_id = get_i64(&v["user_id"]);
        let membership_id = get_i64(&v["organization_membership_id"]);
        let username = resolve_username(
            &mapping,
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

    for v in &legacy_updates {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let channel_id = get_i64(&v["message_thread_id"]).unwrap_or(0);
        if id == 0 || !imported_channel_ids.contains(&channel_id) {
            membership_update_skip += 1;
            continue;
        }

        let actor_id = get_i64(&v["actor_id"]);
        let actor_username = resolve_username(
            &mapping,
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
                            &mapping,
                            Some(id_val),
                            None,
                            &format!("message_thread_membership_updates[{id}].{field}"),
                        )?);
                    } else if let Some(str_val) = item.as_str() {
                        if let Ok(id_val) = str_val.parse::<i64>() {
                            resolved.push(resolve_username(
                                &mapping,
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

    for v in &legacy_messages {
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
                &mapping,
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

    for v in &legacy_notifs {
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

    for v in &legacy_attachments {
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

    for v in &legacy_reactions {
        let id = get_i64(&v["id"]).unwrap_or(0);
        let subject_type = get_str(&v["subject_type"]).unwrap_or_default();
        let subject_id = get_i64(&v["subject_id"]).unwrap_or(0);

        if id == 0 || subject_type != "Message" || !imported_message_ids.contains(&subject_id) {
            reaction_skip += 1;
            continue;
        }

        let user_id = get_i64(&v["user_id"]);
        let membership_id = get_i64(&v["organization_membership_id"]);
        let username = resolve_username(
            &mapping,
            user_id,
            membership_id,
            &format!("reactions[{id}]"),
        )?;

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
    use std::fs;

    use sea_orm::EntityTrait;
    use tempfile::tempdir;

    use super::*;
    use crate::jupiter::tests::test_storage;

    fn write_empty_required_exports(input_dir: &Path) {
        for filename in REQUIRED_LEGACY_EXPORTS {
            fs::write(input_dir.join(filename), "[]").unwrap();
        }
    }

    #[tokio::test]
    async fn test_migration_lifecycle() {
        let temp = tempdir().unwrap();
        let input_dir = temp.path().join("exports");
        fs::create_dir(&input_dir).unwrap();

        // Write user mappings
        let mapping_file = temp.path().join("mapping.json");
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
        fs::write(&mapping_file, mapping_content).unwrap();

        // Write legacy message_threads.json
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

        // Write legacy message_thread_memberships.json
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

        // Write legacy messages.json
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

        // Write custom_reactions.json
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

        // Write reactions.json
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

        // Now run the migration logic manually by mimicking the command exec
        let db_dir = tempdir().unwrap();
        let storage = test_storage(db_dir.path()).await;
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();

        // Read thread
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

        // Query database to ensure channel is inserted
        let inserted = crate::callisto::channel::Entity::find()
            .all(conn)
            .await
            .unwrap();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].id, 101);
        assert_eq!(inserted[0].title.as_deref(), Some("General"));
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
