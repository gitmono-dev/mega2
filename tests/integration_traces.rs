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

#[tokio::test]
async fn import_target_remove_blackbox_deactivates_matching_target() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    use sea_orm::{ActiveModelTrait, DatabaseBackend, Statement};
    use url::Url;

    let (traces, db) = traces_db::TracesDb::migrated().await;
    let database_name: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database() AS name".to_owned(),
        ))
        .await
        .expect("database name")
        .expect("row")
        .try_get("", "name")
        .expect("name");
    let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
        "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
    });
    let mut database_url = Url::parse(&admin).expect("admin url");
    database_url.set_path(&database_name);
    let database_url = database_url.to_string();
    let temp = tempfile::tempdir().expect("temp");
    let config_path = temp.path().join("config.toml");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        &config_path,
    )
    .expect("copy config");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let stdout_path = temp.path().join("service.out");
    let stderr_path = temp.path().join("service.err");
    let mut service = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["service", "http", "--host", "127.0.0.1", "-p"])
        .arg(port.to_string())
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ))
        .spawn()
        .expect("spawn service");
    let deadline = Instant::now() + Duration::from_secs(90);
    let ready = loop {
        if Instant::now() >= deadline {
            break false;
        }
        if service.try_wait().expect("poll service").is_some() {
            break false;
        }
        let count: i64 = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM mega_refs".to_owned(),
            ))
            .await
            .expect("count refs")
            .expect("row")
            .try_get("", "n")
            .expect("n");
        if count > 0 {
            break true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = service.kill();
    let _ = service.wait();
    assert!(
        ready,
        "service did not initialize\nstdout:\n{}\nstderr:\n{}",
        std::fs::read_to_string(&stdout_path).unwrap_or_default(),
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );

    let path = "/third-party/blackbox";
    let repo_id = 910_048_i64;
    let target_id = Uuid::new_v4().to_string();
    mega2_core::git_repo::ActiveModel {
        id: Set(repo_id),
        repo_path: Set(path.to_owned()),
        repo_name: Set("blackbox".to_owned()),
        created_at: Set(Utc::now().naive_utc()),
        updated_at: Set(Utc::now().naive_utc()),
    }
    .insert(&db)
    .await
    .expect("seed repo");
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("import".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(Some(repo_id.to_string())),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("seed target");

    let remove = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["import-repo", "remove", "--path", path, "--yes"])
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .output()
        .expect("run remove");
    let stdout = String::from_utf8_lossy(&remove.stdout);
    let stderr = String::from_utf8_lossy(&remove.stderr);
    assert!(
        remove.status.success(),
        "remove failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("removed {path}")),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let retired = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load")
        .expect("target");
    assert!(!retired.active);
    let generation: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(MAX((metadata->>'binding_generation')::bigint), 1) AS generation
             FROM audit_logs WHERE metadata->>'trace_target_id' = $1",
            [target_id.clone().into()],
        ))
        .await
        .expect("generation")
        .expect("row")
        .try_get("", "generation")
        .expect("generation");
    assert_eq!(generation, 2);
    let repo_left: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM git_repo WHERE id = $1",
            [repo_id.into()],
        ))
        .await
        .expect("repo count")
        .expect("row")
        .try_get("", "n")
        .expect("n");
    assert_eq!(repo_left, 0);

    let replacement = Uuid::new_v4().to_string();
    trace_target::ActiveModel {
        id: Set(replacement.clone()),
        target_kind: Set("import".to_owned()),
        project_path: Set(path.to_owned()),
        backing_import_repo_id: Set(Some("910049".to_owned())),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("replacement target");
    let again = Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["import-repo", "remove", "--path", path, "--yes"])
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .output()
        .expect("run continuation");
    let again_out = String::from_utf8_lossy(&again.stdout);
    let again_err = String::from_utf8_lossy(&again.stderr);
    assert!(
        again.status.success(),
        "second remove failed\nstdout:\n{again_out}\nstderr:\n{again_err}"
    );
    let kept = trace_target::Entity::find_by_id(&replacement)
        .one(&db)
        .await
        .expect("load replacement")
        .expect("replacement");
    assert!(kept.active);
    assert_eq!(kept.id, replacement);
    let _ = traces;
}

fn rebind_output(
    cwd: &std::path::Path,
    args: &[&str],
    env: &[(&str, Option<&str>)],
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"));
    command
        .current_dir(cwd)
        .args(args)
        .env("MEGA_LOG__PRINT_STD", "true")
        .env_remove("MEGA_CONFIG")
        .env_remove("MEGA_PROFILE");
    for (key, value) in env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    command.output().expect("spawn mega2")
}

fn rebind_text(output: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn rebind_target_cli_help_lists_parent_and_subcommand() {
    let cwd = tempfile::tempdir().expect("cwd");
    let parent = rebind_output(cwd.path(), &["traces", "--help"], &[]);
    let (stdout, stderr) = rebind_text(&parent);
    assert!(
        parent.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("rebind-target"), "{stdout}");
    let child = rebind_output(cwd.path(), &["traces", "rebind-target", "--help"], &[]);
    let (stdout, stderr) = rebind_text(&child);
    assert!(
        child.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("--target"), "{stdout}");
    assert!(stdout.contains("--path"), "{stdout}");
}

#[test]
fn rebind_target_cli_refuses_unnamed_or_missing_config() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let base = root.path().join("base");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&base).expect("base");
    let missing = rebind_output(
        &cwd,
        &[
            "traces",
            "rebind-target",
            "--target",
            "00000000-0000-0000-0000-000000000001",
            "--path",
            "/project",
        ],
        &[("MEGA_BASE_DIR", Some(base.to_str().expect("utf-8")))],
    );
    let (stdout, stderr) = rebind_text(&missing);
    assert!(
        !missing.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("will not generate a default"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!base.join("etc/config.toml").exists());
    assert!(!cwd.join("config/config.toml").exists());

    let discovered = cwd.join("config");
    std::fs::create_dir_all(&discovered).expect("config dir");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        discovered.join("config.toml"),
    )
    .expect("copy cwd config");
    let cwd_config = rebind_output(
        &cwd,
        &[
            "traces",
            "rebind-target",
            "--target",
            "00000000-0000-0000-0000-000000000001",
            "--path",
            "/project",
        ],
        &[("MEGA_BASE_DIR", Some(base.to_str().expect("utf-8")))],
    );
    let (stdout, stderr) = rebind_text(&cwd_config);
    assert!(
        !cwd_config.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("was not named") && stderr.contains("source: cwd"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[tokio::test]
async fn rebind_target_cli_rebinds_existing_target_and_rejects_bad_paths() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    use sea_orm::{ActiveModelTrait, DatabaseBackend, Statement};
    use url::Url;

    let (_traces, db) = traces_db::TracesDb::migrated().await;
    let database_name: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT current_database() AS name".to_owned(),
        ))
        .await
        .expect("database name")
        .expect("row")
        .try_get("", "name")
        .expect("name");
    let admin = std::env::var("MEGA_DATABASE__DB_URL").unwrap_or_else(|_| {
        "postgres://mega2:mega2_test_password@127.0.0.1:15432/mega2".to_owned()
    });
    let mut database_url = Url::parse(&admin).expect("admin url");
    database_url.set_path(&database_name);
    let database_url = database_url.to_string();
    let temp = tempfile::tempdir().expect("temp");
    let config_path = temp.path().join("config.toml");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        &config_path,
    )
    .expect("copy config");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let stdout_path = temp.path().join("service.out");
    let stderr_path = temp.path().join("service.err");
    let mut service = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"))
        .arg("--config")
        .arg(&config_path)
        .args(["service", "http", "--host", "127.0.0.1", "-p"])
        .arg(port.to_string())
        .env("MEGA_DATABASE__DB_URL", &database_url)
        .env("MEGA_DATABASE__DB_TYPE", "postgres")
        .env("MEGA_BASE_DIR", temp.path())
        .env("MEGA_CACHE_DIR", temp.path().join("cache"))
        .env("MEGA_LOG__PRINT_STD", "true")
        .env_remove("MEGA_CONFIG")
        .stdout(Stdio::from(
            std::fs::File::create(&stdout_path).expect("stdout"),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr"),
        ))
        .spawn()
        .expect("spawn service");
    let deadline = Instant::now() + Duration::from_secs(90);
    let ready = loop {
        if Instant::now() >= deadline || service.try_wait().expect("poll service").is_some() {
            break false;
        }
        let count: i64 = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM mega_refs".to_owned(),
            ))
            .await
            .expect("count refs")
            .expect("row")
            .try_get("", "n")
            .expect("n");
        if count > 0 {
            break true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = service.kill();
    let _ = service.wait();
    assert!(
        ready,
        "service did not initialize\nstdout:\n{}\nstderr:\n{}",
        std::fs::read_to_string(&stdout_path).unwrap_or_default(),
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );

    let target_id = Uuid::new_v4().to_string();
    trace_target::ActiveModel {
        id: Set(target_id.clone()),
        target_kind: Set("native".to_owned()),
        project_path: Set("/project".to_owned()),
        backing_import_repo_id: Set(None),
        active: Set(true),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(&db)
    .await
    .expect("seed target");

    let config = config_path.to_str().expect("utf-8 config");
    let base = temp.path().to_str().expect("utf-8 base");
    let run = |args: &[&str], mega_config: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mega2"));
        command
            .current_dir(temp.path())
            .env("MEGA_DATABASE__DB_URL", &database_url)
            .env("MEGA_DATABASE__DB_TYPE", "postgres")
            .env("MEGA_BASE_DIR", base)
            .env("MEGA_CACHE_DIR", temp.path().join("cache"))
            .env("MEGA_LOG__PRINT_STD", "true")
            .env_remove("MEGA_PROFILE");
        if mega_config {
            command.env("MEGA_CONFIG", config);
        } else {
            command.env_remove("MEGA_CONFIG");
            command.arg("--config").arg(config);
        }
        command.args(args).output().expect("run rebind")
    };

    let moved = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/third-party",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&moved);
    assert!(
        moved.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("rebound {target_id} /third-party")),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");
    assert_eq!(row.id, target_id);
    assert!(row.active);

    let slash = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/third-party/",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&slash);
    assert!(
        !slash.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not a canonical project path"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after slash")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");

    let missing_path = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/project/missing",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&missing_path);
    assert!(
        !missing_path.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not an existing directory"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after missing path")
        .expect("target");
    assert_eq!(row.project_path, "/third-party");

    let unknown = "00000000-0000-0000-0000-000000000099";
    let missing_target = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            unknown,
            "--path",
            "/project",
        ],
        false,
    );
    let (stdout, stderr) = rebind_text(&missing_target);
    assert!(
        !missing_target.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("target does not exist"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        trace_target::Entity::find_by_id(unknown)
            .one(&db)
            .await
            .expect("lookup unknown")
            .is_none()
    );

    let via_env = run(
        &[
            "traces",
            "rebind-target",
            "--target",
            &target_id,
            "--path",
            "/project",
        ],
        true,
    );
    let (stdout, stderr) = rebind_text(&via_env);
    assert!(
        via_env.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let row = trace_target::Entity::find_by_id(&target_id)
        .one(&db)
        .await
        .expect("load after env rebind")
        .expect("target");
    assert_eq!(row.id, target_id);
    assert_eq!(row.project_path, "/project");
    assert!(row.active);
}
