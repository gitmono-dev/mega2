//! TP-15: `push_policy` / `max_push_commits` / `push_auth` parse, validate,
//! restart-required fields, and DB-backed startup checks ②③⑥.

use super::{
    AgentCaptureConfig, AgentCaptureIngestTokenConfig, DEFAULT_MAX_PUSH_COMMITS, GitConfig,
    GithubSyncBinding, GithubSyncConfig, PushAuth, PushPolicy, PushTokenConfig,
    StorageEventsConfig, reload::ConfigHandle, testing::isolated_config, token_path_authorizes,
    validate,
};
use crate::{
    callisto::sea_orm_active_enums::PushQueueKindEnum,
    jupiter::{
        service::push_queue_service::EnqueueRequest,
        storage::blob_path_index::BlobPathIndexMode,
        tests::{test_storage, test_storage_with_config},
    },
};

fn trunk_config(base: &std::path::Path) -> super::Config {
    let mut config = isolated_config(base.join("config"));
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::None);
    config.git.ssh_receive_pack = Some(false);
    config.cedar.enforcement = "off".to_string();
    config
}

fn valid_config() -> super::Config {
    isolated_config(std::env::temp_dir().join("mega2-config-tp15"))
}

#[test]
fn defaults_are_review_250_and_omitted_push_auth() {
    let config = valid_config();
    assert_eq!(config.monorepo.push_policy, PushPolicy::Review);
    assert_eq!(config.monorepo.max_push_commits, DEFAULT_MAX_PUSH_COMMITS);
    assert_eq!(config.git.push_auth, None);
    assert!(config.git.push_tokens.is_empty());
    config.validate().expect("default sample-equivalent config");
}

#[test]
fn trunk_with_explicit_push_auth_none_validates() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::None);
    config.git.ssh_receive_pack = Some(false);
    config.cedar.enforcement = "off".to_string();
    config
        .validate()
        .expect("trunk + push_auth=none + cedar off");
}

#[test]
fn trunk_plus_cedar_not_off_is_rejected() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::None);
    config.cedar.enforcement = "enforce".to_string();
    let err = config.validate().expect_err("①");
    assert!(err.to_string().contains("cedar.enforcement"), "{}", err);
}

#[test]
fn push_auth_token_requires_trunk() {
    let mut config = valid_config();
    config.git.push_auth = Some(PushAuth::Token);
    config.git.push_tokens = vec![PushTokenConfig {
        name: "ci".into(),
        token: "literal-for-tests".into(),
        paths: None,
    }];
    let err = config.validate().expect_err("④ token ⇒ trunk");
    assert!(err.to_string().contains("push_policy"), "{err}");
}

#[test]
fn storage_only_requires_explicit_ssh_receive_pack_false() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::None);
    config.cedar.enforcement = "off".to_string();
    let err = config
        .validate()
        .expect_err("storage-only must fail closed without ssh_receive_pack=false");
    assert!(err.to_string().contains("ssh_receive_pack"), "{err}");
}

#[test]
fn trunk_without_push_auth_is_rejected() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = None;
    let err = config.validate().expect_err("⑤");
    assert!(err.to_string().contains("push_auth"), "{err}");
}

#[test]
fn oci_enabled_requires_storage_only() {
    let mut review = valid_config();
    review.oci.enabled = true;
    let err = review
        .validate()
        .expect_err("[oci] enabled=true must fail closed without git.push_auth");
    assert!(err.to_string().contains("[oci]"), "{err}");

    let mut storage_only = trunk_config(std::env::temp_dir().as_path());
    storage_only.oci.enabled = true;
    storage_only
        .validate()
        .expect("[oci] enabled in storage-only");
}

#[test]
fn agent_capture_enabled_requires_storage_only() {
    let mut review = valid_config();
    review.agent_capture.enabled = true;
    review.agent_capture.ingest_tokens = vec![AgentCaptureIngestTokenConfig {
        name: "capture".into(),
        token: "literal-for-tests".into(),
        paths: None,
        tenant_id: None,
    }];
    let err = review
        .validate()
        .expect_err("[agent_capture] enabled=true must fail closed without git.push_auth");
    assert!(err.to_string().contains("[agent_capture]"), "{err}");

    let mut storage_only = trunk_config(std::env::temp_dir().as_path());
    storage_only.agent_capture.enabled = true;
    storage_only.agent_capture.ingest_tokens = vec![AgentCaptureIngestTokenConfig {
        name: "capture".into(),
        token: "literal-for-tests".into(),
        paths: None,
        tenant_id: None,
    }];
    storage_only
        .validate()
        .expect("[agent_capture] enabled in storage-only with ingest token");
}

#[test]
fn agent_capture_enabled_requires_ingest_token() {
    let mut storage_only = trunk_config(std::env::temp_dir().as_path());
    storage_only.agent_capture.enabled = true;
    let err = storage_only
        .validate()
        .expect_err("[agent_capture] enabled=true requires ingest_tokens");
    assert!(err.to_string().contains("ingest_tokens"), "{err}");
}

#[test]
fn storage_events_known_fields() {
    assert!(
        validate::known_fields("")
            .expect("root schema")
            .contains(&"storage_events")
    );

    let from_default = StorageEventsConfig::default();
    assert!(!from_default.enabled);
    assert_eq!(from_default.installation_id, None);
    assert_eq!(from_default.max_in_flight, 16);
    assert_eq!(from_default.connect_timeout_seconds, 2);
    assert_eq!(from_default.request_timeout_seconds, 5);
    assert_eq!(from_default.shutdown_grace_seconds, 5);
    assert!(from_default.targets.is_empty());

    let parsed: StorageEventsConfig =
        toml::from_str("enabled = false").expect("partial table deserializes");
    assert!(!parsed.enabled);
    assert_eq!(parsed.max_in_flight, 16);
    assert_eq!(parsed.connect_timeout_seconds, 2);
    assert_eq!(parsed.request_timeout_seconds, 5);
    assert_eq!(parsed.shutdown_grace_seconds, 5);

    let value = toml::from_str::<toml::Value>(
        r#"
        base_dir = "/tmp"
        [database]
        db_url = "postgres://localhost:5432/mono"
        [monorepo]
        import_dir = "/third-party"
        admin = ["admin"]
        root_dirs = ["project"]
        [storage_events]
        unexpected = true
        "#,
    )
    .unwrap();
    let err = crate::config::validate::reject_unknown_fields(&value)
        .expect_err("unknown storage_events key must fail closed");
    assert!(err.to_string().contains("unexpected"), "{err}");
}

#[test]
fn agent_capture_quota_defaults() {
    let from_default = AgentCaptureConfig::default();
    assert!(!from_default.enabled);
    assert_eq!(from_default.tenant_id, "default");
    assert_eq!(from_default.deployment_id, "default");
    assert_eq!(from_default.max_blob_bytes, 16_777_216);
    assert_eq!(from_default.max_file_blobs_per_session, 20);
    assert_eq!(from_default.max_events_per_batch, 500);
    assert_eq!(from_default.max_event_bytes, 1_048_576);
    assert_eq!(from_default.lease_ttl_seconds, 900);
    assert!(from_default.ingest_tokens.is_empty());

    let parsed: AgentCaptureConfig =
        toml::from_str("tenant_id = \"default\"").expect("partial table deserializes");
    assert!(!parsed.enabled);
    assert_eq!(parsed.max_blob_bytes, 16_777_216);
    assert_eq!(parsed.max_file_blobs_per_session, 20);
    assert_eq!(parsed.max_events_per_batch, 500);
    assert_eq!(parsed.max_event_bytes, 1_048_576);
    assert_eq!(parsed.lease_ttl_seconds, 900);
}

#[test]
fn max_push_commits_zero_is_rejected() {
    let mut config = valid_config();
    config.monorepo.max_push_commits = 0;
    let err = config.validate().expect_err("max_push_commits > 0");
    assert!(err.to_string().contains("max_push_commits"), "{err}");
}

#[test]
fn push_auth_token_requires_at_least_one_token() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::Token);
    config.git.ssh_receive_pack = Some(false);
    config.cedar.enforcement = "off".to_string();
    let err = config.validate().expect_err("token table required");
    assert!(err.to_string().contains("push_tokens"), "{err}");
}

#[test]
fn push_token_secret_ref_must_use_config_namespace() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::Token);
    config.git.ssh_receive_pack = Some(false);
    config.cedar.enforcement = "off".to_string();
    config.git.push_tokens = vec![PushTokenConfig {
        name: "ci".into(),
        token: "vault://secret/config/prod/mail/password#value".into(),
        paths: None,
    }];
    let err = config.validate().expect_err("wrong SecretRef namespace");
    assert!(err.to_string().contains("git.push_tokens"), "{err}");

    config.git.push_tokens[0].token =
        "vault://secret/config/prod/git/push_tokens/ci#value".to_string();
    config
        .validate()
        .expect("matching git/push_tokens/<name> SecretRef");
}

#[test]
fn token_paths_use_component_boundaries() {
    assert!(token_path_authorizes("/foo", "/foo"));
    assert!(token_path_authorizes("/foo", "/foo/bar"));
    assert!(
        !token_path_authorizes("/foo", "/foobar"),
        "/foo must not authorize /foobar"
    );
    assert!(token_path_authorizes("/", "/anything"));
}

#[test]
fn git_push_tokens_omitted_paths_mean_whole_repo() {
    let git: GitConfig = toml::from_str(
        r#"
        anonymous_access = true
        push_auth = "token"
        [[push_tokens]]
        name = "ci"
        token = "literal-for-tests"
        "#,
    )
    .expect("parse GitConfig");
    assert!(git.push_tokens[0].paths.is_none());
}

#[test]
fn push_token_file_placeholder_expands_through_config_load() {
    let _lock = super::testing::env_lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = dir.path().join("token");
    std::fs::write(&secret, "loaded-secret").expect("write secret");
    let mut content = super::template::config_init_template(dir.path());
    content = content.replace("# push_policy = \"review\"", "push_policy = \"trunk\"");
    content.push_str(&format!(
        r#"
[git]
push_auth = "token"
ssh_receive_pack = false
[[git.push_tokens]]
name = "ci"
token = "${{file:{}}}"
"#,
        secret.display()
    ));
    let loaded = super::Config::load_str(&content).expect("load");
    assert_eq!(loaded.git.push_tokens[0].token, "loaded-secret");
    loaded.validate().expect("expanded token must validate");
}

#[test]
fn token_path_must_start_with_slash() {
    let mut config = valid_config();
    config.monorepo.push_policy = PushPolicy::Trunk;
    config.git.push_auth = Some(PushAuth::Token);
    config.cedar.enforcement = "off".to_string();
    config.git.push_tokens = vec![PushTokenConfig {
        name: "ci".into(),
        token: "literal-for-tests".into(),
        paths: Some(vec!["project/foo".into()]),
    }];
    let err = config.validate().expect_err("component-boundary path");
    assert!(err.to_string().contains("must start with '/'"), "{err}");
}

#[test]
fn restart_required_fields_include_trunk_surface() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let mut current = isolated_config(temp_dir.path().join("current"));
    current.monorepo.push_policy = PushPolicy::Trunk;
    current.git.push_auth = Some(PushAuth::None);
    current.git.ssh_receive_pack = Some(false);
    current.cedar.enforcement = "off".to_string();
    let handle = ConfigHandle::new(current);
    let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
    candidate.monorepo.max_push_commits = 10;
    candidate.git.push_auth = Some(PushAuth::Token);
    candidate.git.push_tokens = vec![PushTokenConfig {
        name: "ci".into(),
        token: "x".into(),
        paths: None,
    }];
    let report = handle.reload(candidate).expect("reload");
    assert!(
        report
            .restart_required_fields
            .contains(&"monorepo.max_push_commits")
    );
    assert!(report.restart_required_fields.contains(&"git.push_auth"));
    assert!(report.restart_required_fields.contains(&"git.push_tokens"));
}

#[test]
fn ssh_receive_pack_change_is_restart_required() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let mut current = isolated_config(temp_dir.path().join("current"));
    current.git.ssh_receive_pack = Some(false);
    let handle = ConfigHandle::new(current);
    let mut candidate = handle.snapshot().expect("snapshot").as_ref().clone();
    candidate.git.ssh_receive_pack = None;
    let report = handle.reload(candidate).expect("reload");
    assert!(
        report
            .restart_required_fields
            .contains(&"git.ssh_receive_pack")
    );
}

#[tokio::test]
async fn startup_rejects_trunk_with_open_cl() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = test_storage_with_config(temp.path(), trunk_config(temp.path())).await;
    storage
        .cl_storage()
        .new_cl(
            "/project/x",
            "CL-TP15-OPEN",
            "open",
            "main",
            &"a".repeat(40),
            &"b".repeat(40),
            "tester",
        )
        .await
        .unwrap();
    let err = storage
        .prepare_push_policy_startup()
        .await
        .expect_err("② open CL");
    assert!(err.to_string().contains("open change list"), "{err}");
}

#[tokio::test]
async fn startup_rejects_policy_change_with_non_terminal_rows() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = test_storage_with_config(temp.path(), trunk_config(temp.path())).await;
    storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Merge,
            operation_id: "CL-TP15-Q".into(),
            path: "/queued".into(),
            old_id: "0".repeat(40),
            new_id: "1".repeat(40),
            requester: None,
            payload: serde_json::json!({}),
            ref_name: None,
            is_delete: false,
        })
        .await
        .unwrap();
    let err = storage
        .prepare_push_policy_startup()
        .await
        .expect_err("③ non-terminal");
    assert!(err.to_string().contains("non-terminal"), "{err}");
    assert_eq!(
        storage
            .push_queue_storage()
            .get_control()
            .await
            .unwrap()
            .last_policy,
        "review"
    );
}

#[tokio::test]
async fn unchanged_review_policy_does_not_reset_watermarks() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = test_storage(temp.path()).await;
    let blob = "d".repeat(40);
    storage
        .mono_storage()
        .upsert_blob_path(&blob, "/keep.txt", BlobPathIndexMode::Queue { push_id: 9 })
        .await
        .unwrap();
    storage.prepare_push_policy_startup().await.unwrap();
    assert_eq!(
        storage.mono_storage().list_blob_paths().await.unwrap()[0].indexed_push_id,
        Some(9)
    );
    assert_eq!(
        storage
            .push_queue_storage()
            .get_control()
            .await
            .unwrap()
            .last_policy,
        "review"
    );
}

#[tokio::test]
async fn review_to_trunk_null_rows_can_be_queue_overwritten() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = test_storage_with_config(temp.path(), trunk_config(temp.path())).await;
    let blob = "e".repeat(40);
    let path = "/r2t.txt";
    storage
        .mono_storage()
        .upsert_blob_path(blob.as_str(), path, BlobPathIndexMode::Review)
        .await
        .unwrap();
    storage.prepare_push_policy_startup().await.unwrap();
    assert_eq!(
        storage
            .push_queue_storage()
            .get_control()
            .await
            .unwrap()
            .last_policy,
        "trunk"
    );
    assert!(
        storage
            .mono_storage()
            .upsert_blob_path(blob.as_str(), path, BlobPathIndexMode::Queue { push_id: 4 })
            .await
            .unwrap(),
        "review→trunk: NULL rows can be queue-overwritten"
    );
    assert_eq!(
        storage.mono_storage().list_blob_paths().await.unwrap()[0].indexed_push_id,
        Some(4)
    );
}

#[tokio::test]
async fn trunk_to_review_reset_lets_review_update_all_rows() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = test_storage(temp.path()).await;
    storage
        .push_queue_storage()
        .set_last_policy("trunk")
        .await
        .unwrap();
    let blob = "f".repeat(40);
    let path = "/t2r.txt";
    storage
        .mono_storage()
        .upsert_blob_path(blob.as_str(), path, BlobPathIndexMode::Queue { push_id: 8 })
        .await
        .unwrap();
    assert!(
        !storage
            .mono_storage()
            .upsert_blob_path(blob.as_str(), path, BlobPathIndexMode::Review)
            .await
            .unwrap(),
        "review must not punch through a trunk watermark before reset"
    );
    storage.prepare_push_policy_startup().await.unwrap();
    assert!(
        storage.mono_storage().list_blob_paths().await.unwrap()[0]
            .indexed_push_id
            .is_none()
    );
    assert!(
        storage
            .mono_storage()
            .upsert_blob_path(blob.as_str(), path, BlobPathIndexMode::Review)
            .await
            .unwrap(),
        "trunk→review: after reset, review can update all rows"
    );
    assert_eq!(
        storage
            .push_queue_storage()
            .get_control()
            .await
            .unwrap()
            .last_policy,
        "review"
    );
}

#[test]
fn github_sync_shape_and_defaults() {
    let defaulted = GithubSyncConfig::default();
    assert!(!defaulted.enabled);
    assert!(defaulted.ssh_host.is_empty());
    assert!(defaulted.ssh_user.is_empty());
    assert!(defaulted.ssh_host_key.is_empty());
    assert!(defaulted.ssh_key_ref.is_empty());
    assert!(defaulted.bindings.is_empty());

    let parsed: GithubSyncConfig = toml::from_str("").expect("empty table loads");
    assert_eq!(parsed, GithubSyncConfig::default());

    let with_binding: GithubSyncConfig = toml::from_str(
        r#"
enabled = false
ssh_host = "github.com"
ssh_user = "git"
ssh_host_key = "ssh-ed25519 AAAA"
ssh_key_ref = "secret/github-sync"
[[bindings]]
id = "core"
path = "/project/core"
remote = "git@github.com:example/core.git"
"#,
    )
    .expect("explicit schema loads");
    assert_eq!(
        with_binding,
        GithubSyncConfig {
            enabled: false,
            ssh_host: "github.com".into(),
            ssh_user: "git".into(),
            ssh_host_key: "ssh-ed25519 AAAA".into(),
            ssh_key_ref: "secret/github-sync".into(),
            bindings: vec![GithubSyncBinding {
                id: "core".into(),
                path: "/project/core".into(),
                remote: "git@github.com:example/core.git".into(),
            }],
        }
    );

    let cfg = isolated_config(std::env::temp_dir().join("mega2-gs03-github-sync"));
    assert_eq!(cfg.github_sync, GithubSyncConfig::default());
    let _enabled: bool = cfg.github_sync.enabled;
    let _host: String = cfg.github_sync.ssh_host;
    let _user: String = cfg.github_sync.ssh_user;
    let _host_key: String = cfg.github_sync.ssh_host_key;
    let _key_ref: String = cfg.github_sync.ssh_key_ref;
    let _bindings: Vec<GithubSyncBinding> = cfg.github_sync.bindings;
}

#[test]
fn github_sync_rejects_unknown_key() {
    assert!(
        validate::known_fields("")
            .expect("root schema")
            .contains(&"github_sync")
    );
    assert_eq!(
        validate::known_fields("github_sync").expect("section schema"),
        [
            "enabled",
            "ssh_host",
            "ssh_user",
            "ssh_host_key",
            "ssh_key_ref",
            "bindings",
        ]
    );
    assert_eq!(
        validate::known_fields("github_sync.bindings").expect("binding schema"),
        ["id", "path", "remote"]
    );

    let value = toml::from_str::<toml::Value>(
        r#"
        base_dir = "/tmp"
        [database]
        db_url = "postgres://localhost:5432/mono"
        [monorepo]
        import_dir = "/third-party"
        admin = ["admin"]
        root_dirs = ["project"]
        [github_sync]
        unexpected = true
        "#,
    )
    .unwrap();
    let err = crate::config::validate::reject_unknown_fields(&value)
        .expect_err("unknown github_sync key must fail closed");
    assert!(err.to_string().contains("unexpected"), "{err}");

    let binding_value = toml::from_str::<toml::Value>(
        r#"
        base_dir = "/tmp"
        [database]
        db_url = "postgres://localhost:5432/mono"
        [monorepo]
        import_dir = "/third-party"
        admin = ["admin"]
        root_dirs = ["project"]
        [[github_sync.bindings]]
        unexpected = true
        "#,
    )
    .unwrap();
    let binding_err = crate::config::validate::reject_unknown_fields(&binding_value)
        .expect_err("unknown github_sync.bindings key must fail closed");
    assert!(
        binding_err.to_string().contains("unexpected"),
        "{binding_err}"
    );
}
