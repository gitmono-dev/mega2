#[path = "common/traces_db.rs"]
mod traces_db;

use std::path::PathBuf;

use chrono::Utc;
use mega2_core::{
    config::{
        Config,
        loader::{ConfigInput, ConfigLoader},
        reload::ConfigHandle,
    },
    trace_checkpoint, trace_credential_binding, trace_fragment, trace_ingest_attempt,
    trace_principal, trace_ref_update, trace_resource, trace_stream, trace_target,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, Set,
    Statement,
};
use uuid::Uuid;

async fn table_exists(db: &sea_orm::DatabaseConnection, table: &str) -> bool {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT to_regclass('public.{table}')::text AS table_name"),
        ))
        .await
        .expect("catalog query")
        .expect("catalog row");
    row.try_get::<Option<String>>("", "table_name")
        .expect("table_name")
        .is_some()
}

#[tokio::test]
async fn schema_identity_principal_persists() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("insert principal");
    let loaded = trace_principal::Entity::find_by_id(id.clone())
        .one(&db)
        .await
        .expect("load principal")
        .expect("principal row");
    assert_eq!(loaded.id, id);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_identity_namespaces_are_independent() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let token_owner = Uuid::new_v4().to_string();
    let account_owner = Uuid::new_v4().to_string();
    for id in [&token_owner, &account_owner] {
        trace_principal::ActiveModel {
            id: Set(id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("insert principal");
    }
    let credential_id = "cred-synthetic-beta";
    for (principal_id, namespace) in [
        (token_owner.as_str(), "static_push_token"),
        (account_owner.as_str(), "mono_account"),
    ] {
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            namespace: Set(namespace.to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert binding");
    }
    let rows = trace_credential_binding::Entity::find()
        .filter(trace_credential_binding::Column::CredentialId.eq(credential_id))
        .all(&db)
        .await
        .expect("load bindings");
    assert_eq!(rows.len(), 2);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_identity_path_rebuild_keeps_code_and_capture_tables() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let path = "/team/rebuild";
    let first = trace_target::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        target_kind: Set("native".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("first target");
    let mut inactive: trace_target::ActiveModel =
        trace_target::Entity::find_by_id(first.id.clone())
            .one(&db)
            .await
            .expect("load first")
            .expect("first row")
            .into();
    inactive.active = Set(false);
    inactive.update(&db).await.expect("deactivate");
    let rebuilt = trace_target::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        target_kind: Set("native".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("rebuilt target");
    assert_ne!(first.id, rebuilt.id);
    assert!(table_exists(&db, "git_repo").await);
    assert!(table_exists(&db, "agent_capture_session").await);
    assert!(table_exists(&db, "trace_principal").await);
    db.close().await.expect("close traces database");
}

async fn published_stream(db: &sea_orm::DatabaseConnection) -> String {
    let principal_id = Uuid::new_v4().to_string();
    let target_id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(principal_id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("principal");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set(format!("/pub/{target_id}")),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("target");
    trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id),
        target_id: Set(target_id),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(Uuid::new_v4().to_string()),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .expect("stream")
    .id
}

#[tokio::test]
async fn schema_publication_stream_natural_key() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let principal_id = Uuid::new_v4().to_string();
    let target_id = Uuid::new_v4().to_string();
    let epoch_id = Uuid::new_v4().to_string();
    trace_principal::ActiveModel {
        id: Set(principal_id.clone()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("principal");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set(format!("/pub/{target_id}")),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("target");
    let first = trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id.clone()),
        target_id: Set(target_id.clone()),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(epoch_id.clone()),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("stream");
    let duplicate = trace_stream::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        principal_id: Set(principal_id),
        target_id: Set(target_id),
        source_repo_id: Set("source-opaque".to_owned()),
        epoch_id: Set(epoch_id),
        hash_kind: Set("sha1".to_owned()),
        tip_oid: Set(None),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(duplicate.is_err(), "natural key is unique");
    let changed = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE trace_stream SET hash_kind = 'sha256' WHERE id = $1",
            [first.id.into()],
        ))
        .await;
    assert!(changed.is_err(), "hash_kind is immutable");
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_publication_ref_journal_unique() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let stream_id = published_stream(&db).await;
    trace_ingest_attempt::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        idempotency_key: Set("attempt-1".to_owned()),
        state: Set("published".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("attempt");
    trace_ref_update::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        ref_sequence: Set(1),
        idempotency_key: Set("journal-1".to_owned()),
        old_tip: Set(None),
        new_tip: Set("aabb".to_owned()),
        state: Set("pending".to_owned()),
        claim_until: Set(None),
        attempt_count: Set(0),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("journal");
    let same_sequence = trace_ref_update::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id),
        ref_sequence: Set(1),
        idempotency_key: Set("journal-2".to_owned()),
        old_tip: Set(None),
        new_tip: Set("ccdd".to_owned()),
        state: Set("pending".to_owned()),
        claim_until: Set(None),
        attempt_count: Set(0),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(same_sequence.is_err(), "ref_sequence is unique");
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_publication_keeps_code_and_capture_rows() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    db.execute_unprepared(
        "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
         VALUES (91004, '/code/it', 'it', now(), now())",
    )
    .await
    .expect("code row");
    db.execute_unprepared(
        "INSERT INTO agent_capture_session (
            deployment_id, tenant_id, repo_id, producer_id, session_kind,
            client_session_id, completeness, created_at, updated_at
         ) VALUES (
            'dep', 'tenant', 'repo', 'producer', 'external_capture',
            'client-mtr03-it', 'complete', now(), now()
         )",
    )
    .await
    .expect("capture row");
    assert!(table_exists(&db, "trace_stream").await);
    assert!(table_exists(&db, "trace_ref_update").await);
    let code = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM git_repo WHERE id = 91004".to_owned(),
        ))
        .await
        .expect("count code")
        .expect("code row");
    let capture = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM agent_capture_session WHERE client_session_id = 'client-mtr03-it'".to_owned(),
        ))
        .await
        .expect("count capture")
        .expect("capture row");
    assert_eq!(code.try_get::<i64>("", "n").expect("n"), 1);
    assert_eq!(capture.try_get::<i64>("", "n").expect("n"), 1);
    db.close().await.expect("close traces database");
}

#[tokio::test]
async fn schema_projection_checkpoint_resource_and_fragment() {
    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let stream_id = published_stream(&db).await;
    let checkpoint = trace_checkpoint::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_key: Set("ckpt-public".to_owned()),
        original_tree: Set("tree-public".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("checkpoint");
    let duplicate = trace_checkpoint::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_key: Set("ckpt-public".to_owned()),
        original_tree: Set("tree-other".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await;
    assert!(duplicate.is_err(), "checkpoint key is unique in the stream");
    let changed = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE trace_checkpoint SET original_tree = 'tree-mutated' WHERE id = $1",
            [checkpoint.id.clone().into()],
        ))
        .await;
    assert!(changed.is_err(), "original_tree is immutable");
    let resource = trace_resource::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        checkpoint_id: Set(checkpoint.id.clone()),
        ordinal: Set(4),
        byte_start: Set(32),
        byte_end: Set(64),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("resource");
    let fragment = trace_fragment::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        stream_id: Set(stream_id.clone()),
        content_digest: Set("digest-public".to_owned()),
        index_version: Set(1),
        body: Set("Hello".to_owned()),
        ascii_fold: Set("hello".to_owned()),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("fragment");
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO trace_fragment_membership (
            id, fragment_id, stream_id, checkpoint_id, resource_id, created_at
         ) VALUES ($1, $2, $3, $4, $5, now())",
        [
            Uuid::new_v4().to_string().into(),
            fragment.id.clone().into(),
            stream_id.into(),
            checkpoint.id.into(),
            resource.id.clone().into(),
        ],
    ))
    .await
    .expect("membership");
    let loaded = trace_resource::Entity::find_by_id(resource.id)
        .one(&db)
        .await
        .expect("load resource")
        .expect("resource row");
    assert_eq!(
        (loaded.ordinal, loaded.byte_start, loaded.byte_end),
        (4, 32, 64)
    );
    let text = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT index_version, ascii_fold FROM trace_fragment WHERE id = $1",
            [fragment.id.into()],
        ))
        .await
        .expect("fragment sql")
        .expect("fragment row");
    assert_eq!(
        text.try_get::<i64>("", "index_version").expect("version"),
        1
    );
    assert_eq!(
        text.try_get::<String>("", "ascii_fold").expect("fold"),
        "hello"
    );
    let gin = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM pg_indexes \
             WHERE tablename = 'trace_fragment' AND indexdef ILIKE '%gin%'"
                .to_owned(),
        ))
        .await
        .expect("gin count")
        .expect("gin row");
    assert_eq!(gin.try_get::<i64>("", "n").expect("n"), 0);
    db.close().await.expect("close traces database");
}

const TRACE_SERVER_ID: &str = "00000000-0000-4000-8000-000000000050";

fn load_profile(path: &str) -> Config {
    let loaded = ConfigLoader::new(ConfigInput {
        cli_path: Some(PathBuf::from(path)),
        ..ConfigInput::default()
    })
    .load()
    .expect("resolve profile");
    let body = std::fs::read_to_string(&loaded.path).expect("read profile");
    // Sample storage-only mounts a deploy secret. Substitute only that
    // placeholder so the shipped `[traces]` table still loads through Config.
    let rendered = substitute_file_secrets(&body);
    Config::load_str(&rendered).unwrap_or_else(|err| panic!("load {path}: {err}"))
}

fn substitute_file_secrets(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    const OPEN: &str = "${file:";
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str("fixture-token");
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn sample_with_traces(replacement: &str) -> String {
    let body = std::fs::read_to_string("config/config.toml").expect("sample");
    let marker = "[traces]\nenabled = false\n";
    assert!(body.contains(marker), "sample traces block");
    body.replacen(marker, replacement, 1)
}

#[tokio::test]
async fn trace_config_model_profiles_stay_disabled() {
    for path in [
        "config/config.toml",
        "config/config-review.toml",
        "config/config-storage-only.toml",
        "config/config-storage-only.none.toml",
    ] {
        let config = load_profile(path);
        assert!(!config.traces.enabled, "{path}");
        assert!(config.traces.server_id.is_none(), "{path}");
        assert!(config.traces.scope.is_none(), "{path}");
    }
}

#[tokio::test]
async fn trace_config_model_explicit_enable_keeps_configured_server_id() {
    let rendered = sample_with_traces(&format!(
        "[traces]\nenabled = true\nserver_id = \"{TRACE_SERVER_ID}\"\nscope = \"fixture-a\"\n"
    ));
    let first = Config::load_str(&rendered).expect("first load");
    let second = Config::load_str(&rendered).expect("second load");
    first.validate().expect("explicit enable validates");
    second.validate().expect("second load validates");
    assert!(first.traces.enabled);
    assert_eq!(first.traces.server_id.as_deref(), Some(TRACE_SERVER_ID));
    assert_eq!(first.traces.scope.as_deref(), Some("fixture-a"));
    assert_eq!(first.traces.server_id, second.traces.server_id);
    assert_eq!(first.traces.scope, second.traces.scope);
}

#[tokio::test]
async fn trace_config_model_enabled_without_server_id_is_rejected() {
    let rendered = sample_with_traces("[traces]\nenabled = true\n");
    let config = Config::load_str(&rendered).expect("toml parses");
    let error = config.validate().expect_err("enabled requires server_id");
    let text = error.to_string();
    assert!(
        text.contains("traces.server_id"),
        "rejection names server_id: {text}"
    );
}

#[tokio::test]
async fn trace_config_model_rejects_budget_override() {
    let rendered = sample_with_traces("[traces]\nenabled = false\nmax_object_bytes = 1\n");
    let error = Config::load_str(&rendered).expect_err("budget key is unknown");
    let text = error.to_string();
    assert!(
        text.contains("max_object_bytes") || text.contains("unknown"),
        "unknown budget field: {text}"
    );
}

#[tokio::test]
async fn trace_config_model_reload_classifies_traces() {
    let current = load_profile("config/config.toml");
    let handle = ConfigHandle::new(current);
    let before = handle.snapshot().expect("snapshot");
    let mut candidate = before.as_ref().clone();
    candidate.traces.enabled = true;
    candidate.traces.server_id = Some(TRACE_SERVER_ID.to_owned());
    candidate.traces.scope = Some("fixture-a".to_owned());
    let report = handle.reload(candidate).expect("reload");
    assert!(report.restart_required_fields.contains(&"traces.enabled"));
    assert!(report.restart_required_fields.contains(&"traces.server_id"));
    assert!(report.restart_required_fields.contains(&"traces.scope"));
    let after = handle.snapshot().expect("snapshot after reload");
    assert!(!after.traces.enabled);
    assert!(after.traces.server_id.is_none());
    assert!(after.traces.scope.is_none());
}
