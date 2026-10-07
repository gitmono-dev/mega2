use std::{fmt, str::FromStr, sync::Arc};

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    ceres::{
        api_service::state::ProtocolApiState, protocol::AuthContext,
        view::filter::recheck_definition,
    },
    common::errors::{ProtocolError, ViewUnavailableReason},
    config::{Config, GitConfig, PushAuth, PushTokenConfig, token_path_authorizes},
    contract::{
        git_protocol::path::ViewLocator,
        policy::{
            context::CedarContext,
            enforcement::{Enforcement, EnforcementDecision, decide},
            resource::resolve_resource,
            util::SaturnEUid,
        },
    },
    jupiter::storage::view_admission::{AdmitLimits, AdmitOutcome, AdmitRequest},
};

pub mod http;
pub mod path;
pub mod ssh;

#[derive(Deserialize, Debug)]
pub struct InfoRefsParams {
    pub service: Option<String>,
    pub refspec: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ResolvedView {
    pub(crate) filter_pk: i64,
    pub(crate) filter_id: String,
    pub(crate) config: Arc<Config>,
}

impl fmt::Debug for ResolvedView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedView")
            .field("filter_pk", &self.filter_pk)
            .field("filter_id", &self.filter_id)
            .finish()
    }
}

fn corrupt_view(filter_pk: i64, filter_id: &str, failed: &str) -> ProtocolError {
    tracing::error!(filter_pk, filter_id, failed, "view definition corrupt");
    ProtocolError::ViewUnavailable {
        filter_id: filter_id.to_owned(),
        reason: ViewUnavailableReason::DefinitionCorrupt,
    }
}

pub(crate) async fn resolve_view_target(
    state: &ProtocolApiState,
    locator: &ViewLocator,
) -> Result<ResolvedView, ProtocolError> {
    let config = state.storage.config();
    let view_storage = state.storage.view_storage();
    let not_found = || ProtocolError::NotFound("view not found".to_owned());
    let filter_pk = match locator {
        ViewLocator::Named { name, version } => {
            let version = version
                .map(|version| i32::try_from(version).map_err(|_| not_found()))
                .transpose()?;
            view_storage
                .find_view_by_name(name, version)
                .await?
                .ok_or_else(not_found)?
                .filter_pk
        }
        ViewLocator::FilterId(filter_id) => {
            view_storage
                .get_filter_by_filter_id(filter_id)
                .await?
                .ok_or_else(not_found)?
                .id
        }
    };
    let definition = view_storage
        .view_definition_state(filter_pk)
        .await?
        .ok_or_else(not_found)?;
    let filter_id = definition.filter_id;
    if definition.algo_version != 1 {
        return Err(corrupt_view(filter_pk, &filter_id, "algo_version"));
    }
    let expected_format = config.monorepo.object_hash_kind()?.as_str();
    if definition.object_format != expected_format {
        return Err(corrupt_view(filter_pk, &filter_id, "object_format"));
    }
    if let Err(error) = recheck_definition(&definition.canonical_spec, &filter_id) {
        let failed = match error.failed {
            crate::ceres::view::filter::RecheckFailure::RoundTrip => "canonical_spec",
            crate::ceres::view::filter::RecheckFailure::FilterIdMismatch => "filter_id",
        };
        return Err(corrupt_view(filter_pk, &filter_id, failed));
    }
    if definition.ready_seq.is_none()
        && definition.warming_since.is_none()
        && definition.projected_seq == 0
    {
        match view_storage
            .admit(
                AdmitRequest::rewarm(filter_pk),
                AdmitLimits::from(&config.views),
            )
            .await
        {
            Ok(AdmitOutcome::Admitted { .. }) => {
                state.storage.view_signal().notify_worker();
                tracing::info!(filter_id, "view rewarming admitted");
            }
            Ok(AdmitOutcome::Idempotent { .. }) => {
                tracing::debug!(filter_id, "view already rewarming");
            }
            Ok(AdmitOutcome::Rejected { reason, .. }) => {
                tracing::info!(filter_id, ?reason, "view rewarming rejected");
            }
            Err(error) => {
                tracing::warn!(filter_id, %error, "view rewarming failed");
            }
        }
    }
    if definition.halted {
        return Err(ProtocolError::ViewUnavailable {
            filter_id,
            reason: ViewUnavailableReason::RootChainHalted,
        });
    }
    if definition.ready_seq.is_none() {
        return Err(ProtocolError::ViewUnavailable {
            filter_id,
            reason: ViewUnavailableReason::WarmingUp,
        });
    }
    Ok(ResolvedView {
        filter_pk,
        filter_id,
        config,
    })
}

pub async fn check_upload_pack_access(
    git_config: &GitConfig,
    auth: &AuthContext,
) -> Result<(), ProtocolError> {
    if git_config.anonymous_access {
        return Ok(());
    }
    if auth.username.is_none() {
        return Err(ProtocolError::Forbidden(
            "anonymous clone/fetch is disabled; authentication required".to_owned(),
        ));
    }
    Ok(())
}

pub async fn check_push_permission(
    state: &ProtocolApiState,
    auth: &AuthContext,
    repo_path: &std::path::Path,
) -> Result<(), ProtocolError> {
    let config = state.storage.config();
    apply_push_auth_gate(&config.git, auth, repo_path)?;

    let repo_name = repo_path.to_str().ok_or_else(|| {
        ProtocolError::InvalidInput("repository path is not valid UTF-8".to_owned())
    })?;

    let enforcement = Enforcement::parse(&config.cedar.enforcement)
        .ok_or_else(|| ProtocolError::Forbidden("invalid cedar.enforcement".to_owned()))?;

    // `off`: no build, no consume (ADR-UN-01). Token path authorization
    // already ran above; this early return must not skip it.
    if !enforcement.builds() {
        return Ok(());
    }

    let username = auth
        .username
        .as_deref()
        .ok_or_else(|| ProtocolError::Forbidden("push requires authentication".to_owned()))?;

    let snapshot = state
        .entity_store
        .snapshot()
        .ok_or_else(|| ProtocolError::Forbidden("authorization store not built".to_owned()))?;

    decide_push(enforcement, &snapshot, username, repo_name)
}

/// Receive-pack HTTP endpoints skip the auth challenge only when
/// `push_auth = "none"` is explicit. Omitted `push_auth` keeps the OAuth chain.
pub(crate) fn receive_pack_requires_http_auth(git: &GitConfig) -> bool {
    git.push_auth != Some(PushAuth::None)
}

/// Token lookup and path prefix authorization, independent of Cedar.
///
/// Commit author/committer are not inputs: authentication identity is
/// `auth.username` (token name under `push_auth=token`).
fn apply_push_auth_gate(
    git: &GitConfig,
    auth: &AuthContext,
    repo_path: &std::path::Path,
) -> Result<(), ProtocolError> {
    let repo_name = repo_path.to_str().ok_or_else(|| {
        ProtocolError::InvalidInput("repository path is not valid UTF-8".to_owned())
    })?;

    match git.push_auth {
        Some(PushAuth::None) => Ok(()),
        Some(PushAuth::Token) => {
            let username = auth.username.as_deref().ok_or_else(|| {
                ProtocolError::Forbidden("push requires authentication".to_owned())
            })?;
            let token = git
                .push_tokens
                .iter()
                .find(|candidate| candidate.name == username)
                .ok_or_else(|| {
                    ProtocolError::Forbidden("push requires authentication".to_owned())
                })?;
            if token_covers_repo(token, repo_name) {
                Ok(())
            } else {
                Err(ProtocolError::Forbidden(format!(
                    "token is not authorized for path {repo_name}"
                )))
            }
        }
        None => {
            if auth.username.is_none() {
                Err(ProtocolError::Forbidden(
                    "push requires authentication".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Whether a static push token authorizes `repo_path` (component-boundary
/// prefixes via [`token_path_authorizes`]). Omitted/empty `paths` = whole repo.
/// Shared by receive-pack and trunk LFS write gates (ADR-LF-01 / GC-LF-02).
pub(crate) fn token_covers_repo(token: &PushTokenConfig, repo_path: &str) -> bool {
    match &token.paths {
        None => true,
        Some(paths) if paths.is_empty() => true,
        Some(paths) => paths
            .iter()
            .any(|authorized| token_path_authorizes(authorized, repo_path)),
    }
}

/// Constant-time scan of `[[git.push_tokens]]`. Digests are compared so
/// secret length is not leaked by an early return; every configured token is
/// hashed and compared even after a match.
pub(crate) fn lookup_push_token<'a>(
    tokens: &'a [PushTokenConfig],
    presented: &str,
) -> Option<&'a PushTokenConfig> {
    let presented_digest = sha256_bytes(presented);
    let mut matched = None;
    for token in tokens {
        let stored_digest = sha256_bytes(&token.token);
        if ct_eq_bytes(&presented_digest, &stored_digest) && matched.is_none() {
            matched = Some(token);
        }
    }
    matched
}

fn sha256_bytes(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pure three-state push decision (ADR-UN-01/05, UN-11). The request path is
/// normalized to the single monorepo root repository; a missing root is
/// fail-closed. `shadow` allows but records would-deny.
pub fn decide_push(
    enforcement: Enforcement,
    snapshot: &crate::contract::policy::builder::EntitySnapshot,
    username: &str,
    repo_name: &str,
) -> Result<(), ProtocolError> {
    let entity_store = snapshot.store();

    // Normalize the request path to the single monorepo root repository
    // (ADR-UN-05, UN-11). A missing root is fail-closed.
    let resolution = resolve_resource(repo_name, entity_store);
    let would_deny = if let Some(resource) = resolution.resource() {
        let cedar_context = CedarContext::new(entity_store.clone())
            .map_err(|e| ProtocolError::Forbidden(format!("policy engine error: {e}")))?;
        let principal = SaturnEUid::from(EntityUid::from_type_name_and_id(
            EntityTypeName::from_str("User")
                .map_err(|e| ProtocolError::Forbidden(e.to_string()))?,
            EntityId::new(username),
        ));
        let action = SaturnEUid::from(EntityUid::from_type_name_and_id(
            EntityTypeName::from_str("Action")
                .map_err(|e| ProtocolError::Forbidden(e.to_string()))?,
            EntityId::new("pushRepo"),
        ));
        cedar_context
            .is_authorized(&principal, &action, resource, Context::empty())
            .is_err()
    } else {
        true
    };

    if enforcement.records_would_deny() && would_deny {
        tracing::warn!(
            event = "authz_would_deny",
            principal = %username,
            action = "pushRepo",
            resource = %repo_name,
            "push would be denied under enforce"
        );
    }

    match decide(enforcement, would_deny, entity_store.is_empty()) {
        EnforcementDecision::Allow => Ok(()),
        EnforcementDecision::Deny => Err(ProtocolError::Forbidden(
            "push permission denied".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use chrono::Utc;
    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, DbBackend, EntityTrait, Set, Statement,
        TransactionTrait, Value,
    };

    use super::*;
    use crate::{
        callisto::{mega_view, mega_view_filter},
        ceres::{
            api_service::{cache::GitObjectCache, state::ProtocolApiState},
            protocol::PushUserInfo,
            view::filter::parse_for_registration,
        },
        contract::{
            git_protocol::ssh::SshServer,
            policy::{
                builder::build_from_json,
                entitystore::{SharedEntityStore, generate_entity},
            },
        },
        jupiter::storage::base_storage::StorageConnector,
    };

    fn un02_snapshot(admins: &[&str]) -> crate::contract::policy::builder::EntitySnapshot {
        let admins = admins.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let json = generate_entity(&admins, "/").expect("generate");
        build_from_json(&json).expect("build")
    }

    /// Run `f` under a capturing tracing subscriber and return the formatted
    /// output, so structured log fields can be asserted.
    fn capture_tracing<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        use std::{io::Write, sync::Mutex};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buf.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf8")
    }

    #[test]
    fn un02_push_gate_off_allows_without_build() {
        // `off`: no build, no consume -> always allow (ADR-UN-01).
        let snapshot = un02_snapshot(&["admin"]);
        assert!(decide_push(Enforcement::Off, &snapshot, "nobody", "/project").is_ok());
    }

    #[test]
    fn un02_push_gate_shadow_allows_but_records_would_deny() {
        // `shadow`: allows but records would-deny for a non-admin principal.
        let snapshot = un02_snapshot(&["admin"]);
        let out = capture_tracing(|| {
            assert!(decide_push(Enforcement::Shadow, &snapshot, "nobody", "/project").is_ok());
        });
        // Structured would-deny fields (event/principal/action/resource).
        assert!(
            out.contains("authz_would_deny"),
            "missing event field: {out}"
        );
        assert!(out.contains("nobody"), "missing principal field: {out}");
        assert!(out.contains("pushRepo"), "missing action field: {out}");
        assert!(out.contains("/project"), "missing resource field: {out}");
    }

    #[test]
    fn un02_push_gate_enforce_denies_unauthorized() {
        let snapshot = un02_snapshot(&["admin"]);
        let err = decide_push(Enforcement::Enforce, &snapshot, "nobody", "/project")
            .expect_err("non-admin push should be denied under enforce");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
    }

    #[test]
    fn un02_push_gate_enforce_allows_admin() {
        let snapshot = un02_snapshot(&["admin"]);
        assert!(decide_push(Enforcement::Enforce, &snapshot, "admin", "/project").is_ok());
    }

    #[test]
    fn un02_push_gate_enforce_denies_missing_root() {
        // Store without the root repository -> fail-closed (ADR-UN-05).
        let json = generate_entity(&["admin".to_string()], "other").expect("generate");
        let snapshot = build_from_json(&json).expect("build");
        let err = decide_push(Enforcement::Enforce, &snapshot, "admin", "/other")
            .expect_err("missing root repository should deny under enforce");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
    }

    #[tokio::test]
    async fn un02_object_identity_shared_across_context_storage_http() {
        let dir = tempfile::tempdir().unwrap();
        let mut storage = crate::jupiter::tests::test_storage(dir.path()).await;

        // `ctx_entity_store` is the `AppContext.entity_store` field (unique
        // owner, ADR-UN-02). `AppContext::new` injects the same `Arc` into
        // `Storage`, the HTTP state, and the SSH state.
        let ctx_entity_store = Arc::new(SharedEntityStore::new());
        storage.set_entity_store(ctx_entity_store.clone());

        // Storage leg: `entity_store()` returns the injected `Arc`.
        assert!(Arc::ptr_eq(&ctx_entity_store, &storage.entity_store()));

        // HTTP leg: build a real `ProtocolApiState` exactly as the HTTP server
        // does (`src/server/http_server.rs`), from `ctx.entity_store.clone()`.
        let http_state = ProtocolApiState {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: crate::jupiter::redis::init_connection(&crate::config::RedisConfig {
                    url: std::env::var("MEGA_REDIS__URL")
                        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string()),
                })
                .await
                .expect("redis connection"),
                prefix: "test".to_string(),
            }),
            entity_store: ctx_entity_store.clone(),
        };
        assert!(Arc::ptr_eq(&ctx_entity_store, &http_state.entity_store));
        assert!(Arc::ptr_eq(
            &storage.entity_store(),
            &http_state.entity_store
        ));

        // SSH leg (UN-03): build a real `SshServer` exactly as the SSH server
        // does (`src/server/ssh_server.rs`), from `ctx.entity_store.clone()`.
        let ssh_server = SshServer {
            clients: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            state: ProtocolApiState {
                storage: storage.clone(),
                git_object_cache: http_state.git_object_cache.clone(),
                entity_store: ctx_entity_store.clone(),
            },
            id: 0,
            channels: HashMap::new(),
            v2_channels: HashMap::new(),
            authenticated_user: None,
        };
        assert!(Arc::ptr_eq(
            &ctx_entity_store,
            &ssh_server.state.entity_store
        ));
        assert!(Arc::ptr_eq(
            &storage.entity_store(),
            &ssh_server.state.entity_store
        ));
    }

    #[tokio::test]
    async fn anonymous_access_allowed_by_default() {
        let git_config = GitConfig {
            anonymous_access: true,
            ..Default::default()
        };
        let auth = AuthContext {
            username: None,
            authenticated_user: None,
        };
        check_upload_pack_access(&git_config, &auth)
            .await
            .expect("anonymous access should be allowed when config permits it");
    }

    #[tokio::test]
    async fn anonymous_access_denied_when_disabled_and_no_auth() {
        let git_config = GitConfig {
            anonymous_access: false,
            ..Default::default()
        };
        let auth = AuthContext {
            username: None,
            authenticated_user: None,
        };
        let err = check_upload_pack_access(&git_config, &auth)
            .await
            .expect_err("anonymous access should be denied when config forbids it");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
        assert!(
            err.to_string()
                .contains("anonymous clone/fetch is disabled")
        );
    }

    #[tokio::test]
    async fn authenticated_access_allowed_when_anonymous_disabled() {
        let git_config = GitConfig {
            anonymous_access: false,
            ..Default::default()
        };
        let auth = AuthContext {
            username: Some("alice".to_string()),
            authenticated_user: Some(PushUserInfo {
                username: "alice".to_string(),
            }),
        };
        check_upload_pack_access(&git_config, &auth)
            .await
            .expect("authenticated user should be allowed when anonymous access is disabled");
    }

    fn token_config(name: &str, secret: &str, paths: Option<Vec<String>>) -> PushTokenConfig {
        PushTokenConfig {
            name: name.to_owned(),
            token: secret.to_owned(),
            paths,
        }
    }

    fn token_auth(name: &str) -> AuthContext {
        AuthContext {
            username: Some(name.to_owned()),
            authenticated_user: Some(PushUserInfo {
                username: name.to_owned(),
            }),
        }
    }

    #[test]
    fn lookup_push_token_matches_by_constant_time_digest() {
        let tokens = vec![
            token_config("alpha", "secret-a", None),
            token_config("ci", "secret-ci", Some(vec!["/project/foo".to_owned()])),
        ];
        let hit = lookup_push_token(&tokens, "secret-ci").expect("hit");
        assert_eq!(hit.name, "ci");
        assert!(lookup_push_token(&tokens, "secret-missing").is_none());
        assert!(lookup_push_token(&tokens, "secret-c").is_none());
    }

    #[test]
    fn ssh_receive_pack_enabled_only_for_review_oauth_form() {
        assert!(GitConfig::default().ssh_receive_pack_enabled());

        let review_disabled = GitConfig {
            ssh_receive_pack: Some(false),
            ..Default::default()
        };
        assert!(!review_disabled.ssh_receive_pack_enabled());

        let token = GitConfig {
            push_auth: Some(PushAuth::Token),
            ssh_receive_pack: Some(false),
            ..Default::default()
        };
        assert!(!token.ssh_receive_pack_enabled());

        let none = GitConfig {
            push_auth: Some(PushAuth::None),
            ssh_receive_pack: Some(false),
            ..Default::default()
        };
        assert!(!none.ssh_receive_pack_enabled());

        let storage_only_even_if_true = GitConfig {
            push_auth: Some(PushAuth::Token),
            ssh_receive_pack: Some(true),
            ..Default::default()
        };
        assert!(!storage_only_even_if_true.ssh_receive_pack_enabled());
    }

    #[test]
    fn receive_pack_requires_auth_except_explicit_none() {
        let omitted = GitConfig::default();
        assert!(receive_pack_requires_http_auth(&omitted));

        let token = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_config("ci", "s", None)],
            ..Default::default()
        };
        assert!(receive_pack_requires_http_auth(&token));

        let none = GitConfig {
            push_auth: Some(PushAuth::None),
            ..Default::default()
        };
        assert!(!receive_pack_requires_http_auth(&none));
    }

    #[test]
    fn omitted_push_auth_still_requires_username() {
        let git = GitConfig::default();
        let err = apply_push_auth_gate(
            &git,
            &AuthContext {
                username: None,
                authenticated_user: None,
            },
            std::path::Path::new("/"),
        )
        .expect_err("omitted push_auth keeps the OAuth username requirement");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
    }

    #[test]
    fn none_push_auth_skips_username() {
        let git = GitConfig {
            push_auth: Some(PushAuth::None),
            ..Default::default()
        };
        apply_push_auth_gate(
            &git,
            &AuthContext {
                username: None,
                authenticated_user: None,
            },
            std::path::Path::new("/project/foo"),
        )
        .expect("explicit none bypasses the username gate");
    }

    #[test]
    fn token_paths_use_component_boundaries() {
        let git = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_config(
                "ci",
                "secret",
                Some(vec!["/project/foo".to_owned()]),
            )],
            ..Default::default()
        };
        let auth = token_auth("ci");
        apply_push_auth_gate(&git, &auth, std::path::Path::new("/project/foo"))
            .expect("exact path is authorized");
        apply_push_auth_gate(&git, &auth, std::path::Path::new("/project/foo/bar"))
            .expect("child path is authorized");
        let err = apply_push_auth_gate(&git, &auth, std::path::Path::new("/project/foobar"))
            .expect_err("/project/foo must not authorize /project/foobar");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
        let err = apply_push_auth_gate(&git, &auth, std::path::Path::new("/other"))
            .expect_err("path outside token.paths is denied");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
    }

    #[test]
    fn token_auth_uses_token_name_not_commit_author() {
        let git = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_config("ci", "secret", None)],
            ..Default::default()
        };
        // AuthContext.username is the token name. Commit author is not an
        // input to this gate, so a mismatched author cannot change the result.
        apply_push_auth_gate(&git, &token_auth("ci"), std::path::Path::new("/anything"))
            .expect("token name authorizes regardless of commit author");
        let err = apply_push_auth_gate(&git, &token_auth("alice"), std::path::Path::new("/"))
            .expect_err("unknown token name is not authenticated");
        assert!(matches!(err, ProtocolError::Forbidden(_)));
    }

    #[test]
    fn omitted_or_empty_token_paths_mean_whole_repo() {
        let omitted = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_config("ci", "secret", None)],
            ..Default::default()
        };
        apply_push_auth_gate(
            &omitted,
            &token_auth("ci"),
            std::path::Path::new("/anywhere"),
        )
        .expect("omitted paths authorize the whole repo");

        let empty = GitConfig {
            push_auth: Some(PushAuth::Token),
            push_tokens: vec![token_config("ci", "secret", Some(Vec::new()))],
            ..Default::default()
        };
        apply_push_auth_gate(&empty, &token_auth("ci"), std::path::Path::new("/anywhere"))
            .expect("empty paths authorize the whole repo");
    }

    async fn resolver_state() -> (tempfile::TempDir, ProtocolApiState) {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let connection = crate::jupiter::redis::init_connection(&crate::config::RedisConfig {
            url: std::env::var("MEGA_REDIS__URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned()),
        })
        .await
        .unwrap();
        let state = ProtocolApiState {
            entity_store: storage.entity_store(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "hp20-resolve".to_owned(),
            }),
            storage,
        };
        (temp, state)
    }

    async fn insert_resolver_filter(state: &ProtocolApiState, pk: i64, spec: &str) -> String {
        let canonical = parse_for_registration(spec).unwrap();
        let filter_id = canonical.filter_id.clone();
        mega_view_filter::ActiveModel {
            id: Set(pk),
            filter_id: Set(filter_id.clone()),
            canonical_spec: Set(canonical.canonical_text),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!([])),
            push_enabled: Set(false),
            projected_seq: Set(0),
            ready_seq: Set(Some(0)),
            warming_since: Set(None),
            last_access_at: Set(None),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(state.storage.view_storage().get_connection())
        .await
        .unwrap();
        filter_id
    }

    async fn insert_resolver_name(
        state: &ProtocolApiState,
        id: i64,
        name: &str,
        version: i32,
        filter_pk: i64,
    ) {
        mega_view::ActiveModel {
            id: Set(id),
            name: Set(name.to_owned()),
            version: Set(version),
            filter_pk: Set(filter_pk),
            created_by: Set("hp20".to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(state.storage.view_storage().get_connection())
        .await
        .unwrap();
    }

    fn assert_view_not_found(result: Result<ResolvedView, ProtocolError>) {
        assert!(
            matches!(result, Err(ProtocolError::NotFound(message)) if message == "view not found")
        );
    }

    #[tokio::test]
    async fn resolve_view_locator_targets() {
        let (_temp, state) = resolver_state().await;
        let first_id = insert_resolver_filter(&state, 101, ":/project/first").await;
        let second_id = insert_resolver_filter(&state, 102, ":/project/second").await;
        insert_resolver_name(&state, 201, "hp20/test", 1, 101).await;
        insert_resolver_name(&state, 202, "hp20/test", 2, 102).await;
        let latest = resolve_view_target(
            &state,
            &ViewLocator::Named {
                name: "hp20/test".to_owned(),
                version: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            (latest.filter_pk, latest.filter_id.as_str()),
            (102, second_id.as_str())
        );
        let earlier = resolve_view_target(
            &state,
            &ViewLocator::Named {
                name: "hp20/test".to_owned(),
                version: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            (earlier.filter_pk, earlier.filter_id.as_str()),
            (101, first_id.as_str())
        );
        let direct = resolve_view_target(&state, &ViewLocator::FilterId(first_id.clone()))
            .await
            .unwrap();
        assert_eq!(
            (direct.filter_pk, direct.filter_id.as_str()),
            (101, first_id.as_str())
        );
        assert!(!format!("{latest:?}").contains("config"));
        for locator in [
            ViewLocator::Named {
                name: "not-registered".to_owned(),
                version: None,
            },
            ViewLocator::Named {
                name: "hp20/test".to_owned(),
                version: Some(3),
            },
            ViewLocator::Named {
                name: "hp20/test".to_owned(),
                version: Some(u32::MAX),
            },
            ViewLocator::FilterId("f".repeat(64)),
        ] {
            assert_view_not_found(resolve_view_target(&state, &locator).await);
        }
    }

    #[tokio::test]
    async fn resolve_view_definition_corrupt() {
        use std::{io::Write, sync::Mutex};

        use tracing_subscriber::fmt::MakeWriter;
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl MakeWriter<'_> for Writer {
            type Writer = Writer;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }
        let (_temp, state) = resolver_state().await;
        let good_id = insert_resolver_filter(&state, 300, ":/project/good").await;
        assert!(
            resolve_view_target(&state, &ViewLocator::FilterId(good_id))
                .await
                .is_ok()
        );
        for (index, failed) in [
            "algo_version",
            "object_format",
            "canonical_spec",
            "filter_id",
        ]
        .iter()
        .enumerate()
        {
            let pk = 301 + index as i64;
            let spec = format!(":/project/corrupt-{index}");
            let canonical = parse_for_registration(&spec).unwrap();
            let mut filter_id = canonical.filter_id.clone();
            let mut canonical_spec = canonical.canonical_text;
            let mut algo_version = 1;
            let mut object_format = "sha1".to_owned();
            match *failed {
                "algo_version" => algo_version = 2,
                "object_format" => object_format = "sha256".to_owned(),
                "canonical_spec" => canonical_spec.push('/'),
                "filter_id" => {
                    filter_id.replace_range(..1, if filter_id.starts_with('0') { "1" } else { "0" })
                }
                _ => unreachable!(),
            }
            mega_view_filter::ActiveModel {
                id: Set(pk),
                filter_id: Set(filter_id.clone()),
                canonical_spec: Set(canonical_spec),
                algo_version: Set(algo_version),
                object_format: Set(object_format),
                src_paths: Set(serde_json::json!([])),
                push_enabled: Set(false),
                projected_seq: Set(0),
                ready_seq: Set(Some(0)),
                warming_since: Set(None),
                last_access_at: Set(None),
                created_at: Set(Utc::now().naive_utc()),
            }
            .insert(state.storage.view_storage().get_connection())
            .await
            .unwrap();
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::fmt()
                .with_writer(Writer(bytes.clone()))
                .finish();
            let guard = tracing::subscriber::set_default(subscriber);
            let result =
                resolve_view_target(&state, &ViewLocator::FilterId(filter_id.clone())).await;
            drop(guard);
            assert!(
                matches!(result, Err(ProtocolError::ViewUnavailable { filter_id: actual, reason: ViewUnavailableReason::DefinitionCorrupt }) if actual == filter_id)
            );
            let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert_eq!(log.matches("view definition corrupt").count(), 1, "{log}");
            assert!(log.contains(&filter_id), "{log}");
            assert!(log.contains(&format!("failed=\"{failed}\"")), "{log}");
        }
    }

    async fn rewarm_state() -> (tempfile::TempDir, ProtocolApiState) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.cedar.enforcement = "off".to_owned();
        config.git.push_auth = Some(PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.views.enabled = true;
        let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
        let connection = crate::jupiter::redis::init_connection(&crate::config::RedisConfig {
            url: std::env::var("MEGA_REDIS__URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned()),
        })
        .await
        .unwrap();
        let state = ProtocolApiState {
            entity_store: storage.entity_store(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "hp21-rewarm".to_owned(),
            }),
            storage,
        };
        (temp, state)
    }

    async fn rewarm_filter(
        state: &ProtocolApiState,
        pk: i64,
        spec: &str,
        state_kind: &str,
    ) -> String {
        let canonical = parse_for_registration(spec).unwrap();
        let filter_id = canonical.filter_id.clone();
        let (projected_seq, ready_seq, warming_since) = match state_kind {
            "ready" => (5, Some(5), None),
            "warming" => (0, None, Some(Utc::now().naive_utc())),
            "recycled" => (0, None, None),
            _ => panic!("unexpected filter state"),
        };
        mega_view_filter::ActiveModel {
            id: Set(pk),
            filter_id: Set(filter_id.clone()),
            canonical_spec: Set(canonical.canonical_text),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!([format!(
                "/{}",
                spec.trim_start_matches(":/")
            )])),
            push_enabled: Set(false),
            projected_seq: Set(projected_seq),
            ready_seq: Set(ready_seq),
            warming_since: Set(warming_since),
            last_access_at: Set(None),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(state.storage.view_storage().get_connection())
        .await
        .unwrap();
        filter_id
    }

    async fn rewarm_rows(state: &ProtocolApiState) -> Vec<Vec<String>> {
        let views = state.storage.view_storage();
        let db = views.get_connection();
        let mut tables = Vec::new();
        for table in ["mega_view_filter", "mega_view", "mega_view_register_log"] {
            let rows = db
                .query_all_raw(Statement::from_string(
                    DbBackend::Postgres,
                    format!("SELECT to_jsonb(t)::text AS row FROM {table} t ORDER BY id"),
                ))
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get("", "row").unwrap())
                .collect();
            tables.push(rows);
        }
        tables
    }

    fn assert_rewarm_unavailable(
        result: Result<ResolvedView, ProtocolError>,
        expected_id: &str,
        expected_reason: ViewUnavailableReason,
    ) {
        assert!(
            matches!(result, Err(ProtocolError::ViewUnavailable { filter_id, reason }) if filter_id == expected_id && reason == expected_reason)
        );
    }

    async fn assert_rewarm_signal(state: &ProtocolApiState, expected: bool) {
        let signaled = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            state.storage.view_signal().notified(),
        )
        .await
        .is_ok();
        assert_eq!(signaled, expected);
    }

    async fn capture_rewarm_tracing<F: std::future::Future>(future: F) -> (F::Output, String) {
        use std::{io::Write, sync::Mutex};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl MakeWriter<'_> for Writer {
            type Writer = Writer;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(Writer(bytes.clone()))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let result = future.await;
        drop(guard);
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        (result, output)
    }

    fn assert_only_warming_changed(before: &str, after: &str) {
        let mut before: serde_json::Value = serde_json::from_str(before).unwrap();
        let mut after: serde_json::Value = serde_json::from_str(after).unwrap();
        assert_eq!(
            before.as_object_mut().unwrap().remove("warming_since"),
            Some(serde_json::Value::Null)
        );
        assert!(
            after
                .as_object_mut()
                .unwrap()
                .remove("warming_since")
                .is_some_and(|value| !value.is_null())
        );
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn rewarm_admitted_marks_warming_and_signals() {
        let (_temp, state) = rewarm_state().await;
        let id = rewarm_filter(&state, 1001, ":/a", "recycled").await;
        let views = state.storage.view_storage();
        let db = views.get_connection();
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO mega_view_register_log (requester, created_at) VALUES ($1, now() - interval '2 hours')",
            [Value::from("anonymous")],
        ))
        .await
        .unwrap();
        let before = rewarm_rows(&state).await;
        assert_rewarm_unavailable(
            resolve_view_target(&state, &ViewLocator::FilterId(id.clone())).await,
            &id,
            ViewUnavailableReason::WarmingUp,
        );
        let after = rewarm_rows(&state).await;
        assert_only_warming_changed(&before[0][0], &after[0][0]);
        assert_eq!(after[1..], before[1..]);
        let filter = mega_view_filter::Entity::find_by_id(1001)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert!(filter.warming_since.is_some());
        assert_eq!(filter.projected_seq, 0);
        assert_eq!(filter.ready_seq, None);
        assert_eq!(filter.last_access_at, None);
        assert_rewarm_signal(&state, true).await;
        assert_rewarm_unavailable(
            resolve_view_target(&state, &ViewLocator::FilterId(id.clone())).await,
            &id,
            ViewUnavailableReason::WarmingUp,
        );
        assert_eq!(after, rewarm_rows(&state).await);
        assert_rewarm_signal(&state, false).await;

        let (_halt_temp, halted) = rewarm_state().await;
        let halted_id = rewarm_filter(&halted, 1002, ":/b", "recycled").await;
        halted
            .storage
            .view_storage()
            .get_connection()
            .execute_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("INSERT INTO mega_view_root_chain_scan (pos, commit_id, tree_id, parent_count, first_parent) VALUES (1, '{}', '{}', 2, '{}')", "a".repeat(40), "b".repeat(40), "c".repeat(40)),
            ))
            .await
            .unwrap();
        assert_rewarm_unavailable(
            resolve_view_target(&halted, &ViewLocator::FilterId(halted_id.clone())).await,
            &halted_id,
            ViewUnavailableReason::RootChainHalted,
        );
        let filter = mega_view_filter::Entity::find_by_id(1002)
            .one(halted.storage.view_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert!(filter.warming_since.is_some());
        assert_rewarm_signal(&halted, true).await;
    }

    async fn rewarm_waiter(holder: &sea_orm::DatabaseTransaction) -> Option<i32> {
        for _ in 0..1000 {
            let row = holder
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
                     AND classid = $1::int4::oid \
                     AND objid = hashtext(current_schema() || ':' || 'register')::oid \
                     AND objsubid = 2",
                    [Value::from(
                        crate::jupiter::storage::view_storage::VIEW_LOCK_NS,
                    )],
                ))
                .await
                .unwrap();
            if let Some(row) = row {
                return Some(row.try_get("", "pid").unwrap());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    #[tokio::test]
    async fn rewarm_not_admitted_still_unavailable() {
        for (limit, active_kind) in [("max_filters", "ready"), ("cold_slots", "warming")] {
            let (_temp, state) = rewarm_state().await;
            let mut config = state.storage.config().as_ref().clone();
            if limit == "max_filters" {
                config.views.max_filters = 1;
            } else {
                config.views.max_concurrent_cold_starts = 1;
            }
            state.storage.config_handle().reload(config).unwrap();
            rewarm_filter(&state, 1100, ":/a", active_kind).await;
            let id = rewarm_filter(&state, 1101, ":/b", "recycled").await;
            let before = rewarm_rows(&state).await;
            let (result, output) = capture_rewarm_tracing(resolve_view_target(
                &state,
                &ViewLocator::FilterId(id.clone()),
            ))
            .await;
            assert_rewarm_unavailable(result, &id, ViewUnavailableReason::WarmingUp);
            assert!(output.contains(&id), "{output}");
            assert!(output.contains("view rewarming rejected"), "{output}");
            let reason = if limit == "max_filters" {
                "MaxFilters"
            } else {
                "ColdStartSlots"
            };
            assert!(output.contains(reason), "{output}");
            assert_eq!(before, rewarm_rows(&state).await);
            assert_rewarm_signal(&state, false).await;
        }

        let (_temp, state) = rewarm_state().await;
        let id = rewarm_filter(&state, 1201, ":/a", "recycled").await;
        let before = rewarm_rows(&state).await;
        let views = state.storage.view_storage();
        let db = views.get_connection();
        let holder = db.begin().await.unwrap();
        crate::jupiter::storage::view_storage::acquire_view_lock(
            &holder,
            crate::jupiter::storage::view_storage::ViewLock::Register,
            crate::jupiter::storage::view_storage::ViewLockMode::Blocking,
        )
        .await
        .unwrap();
        let locator = ViewLocator::FilterId(id.clone());
        let ((result, observed), output) = capture_rewarm_tracing(async {
            tokio::join!(resolve_view_target(&state, &locator), async {
                let pid = rewarm_waiter(&holder).await;
                if let Some(pid) = pid {
                    let row = holder
                        .query_one_raw(Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "SELECT pg_cancel_backend($1) AS cancelled",
                            [Value::from(pid)],
                        ))
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(row.try_get::<bool>("", "cancelled").unwrap());
                }
                holder.rollback().await.unwrap();
                pid.is_some()
            })
        })
        .await;
        assert!(observed, "admission did not wait for register lock");
        assert_rewarm_unavailable(result, &id, ViewUnavailableReason::WarmingUp);
        assert!(
            output.lines().any(|line| line.contains("WARN")
                && line.contains("view rewarming failed")
                && line.contains(&id)),
            "{output}"
        );
        assert_eq!(before, rewarm_rows(&state).await);
        assert_rewarm_signal(&state, false).await;

        let (_temp, state) = rewarm_state().await;
        let id = rewarm_filter(&state, 1301, ":/a", "recycled").await;
        let before = rewarm_rows(&state).await;
        let views = state.storage.view_storage();
        let db = views.get_connection();
        let holder = db.begin().await.unwrap();
        crate::jupiter::storage::view_storage::acquire_view_lock(
            &holder,
            crate::jupiter::storage::view_storage::ViewLock::Register,
            crate::jupiter::storage::view_storage::ViewLockMode::Blocking,
        )
        .await
        .unwrap();
        let locator = ViewLocator::FilterId(id.clone());
        let (result, written) = tokio::join!(resolve_view_target(&state, &locator), async {
            let pid = rewarm_waiter(&holder).await;
            if pid.is_some() {
                let row = holder
                    .query_one_raw(Statement::from_string(
                        DbBackend::Postgres,
                        "UPDATE mega_view_filter SET warming_since = now() WHERE id = 1301 RETURNING warming_since",
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                let written: chrono::NaiveDateTime = row.try_get("", "warming_since").unwrap();
                holder.commit().await.unwrap();
                Some(written)
            } else {
                holder.rollback().await.unwrap();
                None
            }
        });
        let written = written.expect("admission did not wait for register lock");
        assert_rewarm_unavailable(result, &id, ViewUnavailableReason::WarmingUp);
        let after = rewarm_rows(&state).await;
        assert_only_warming_changed(&before[0][0], &after[0][0]);
        assert_eq!(after[1..], before[1..]);
        let filter = mega_view_filter::Entity::find_by_id(1301)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(filter.warming_since, Some(written));
        assert_eq!(filter.ready_seq, None);
        assert_eq!(filter.projected_seq, 0);
        assert_rewarm_signal(&state, false).await;
    }

    #[tokio::test]
    async fn rewarm_limits_follow_reload() {
        let (_temp, state) = rewarm_state().await;
        let mut config = state.storage.config().as_ref().clone();
        config.views.max_concurrent_cold_starts = 1;
        state.storage.config_handle().reload(config).unwrap();
        rewarm_filter(&state, 1400, ":/a", "warming").await;
        let x = rewarm_filter(&state, 1401, ":/b", "recycled").await;
        let y = rewarm_filter(&state, 1402, ":/c", "recycled").await;

        assert_rewarm_unavailable(
            resolve_view_target(&state, &ViewLocator::FilterId(x.clone())).await,
            &x,
            ViewUnavailableReason::WarmingUp,
        );
        let views = state.storage.view_storage();
        let db = views.get_connection();
        assert!(
            mega_view_filter::Entity::find_by_id(1401)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .warming_since
                .is_none()
        );
        assert_rewarm_signal(&state, false).await;

        let mut config = state.storage.config().as_ref().clone();
        config.views.max_concurrent_cold_starts = 2;
        let report = state.storage.config_handle().reload(config).unwrap();
        assert!(
            report
                .applied_fields
                .contains(&"views.max_concurrent_cold_starts")
        );
        assert_rewarm_unavailable(
            resolve_view_target(&state, &ViewLocator::FilterId(x.clone())).await,
            &x,
            ViewUnavailableReason::WarmingUp,
        );
        assert!(
            mega_view_filter::Entity::find_by_id(1401)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .warming_since
                .is_some()
        );
        assert_rewarm_signal(&state, true).await;

        let mut config = state.storage.config().as_ref().clone();
        config.views.max_concurrent_cold_starts = 10;
        config.views.max_filters = 2;
        let report = state.storage.config_handle().reload(config).unwrap();
        assert!(report.applied_fields.contains(&"views.max_filters"));
        assert_rewarm_unavailable(
            resolve_view_target(&state, &ViewLocator::FilterId(y.clone())).await,
            &y,
            ViewUnavailableReason::WarmingUp,
        );
        assert!(
            mega_view_filter::Entity::find_by_id(1402)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .warming_since
                .is_none()
        );
        assert_rewarm_signal(&state, false).await;
    }

    #[tokio::test]
    async fn rewarm_skipped_for_corrupt_definition() {
        let (_temp, state) = rewarm_state().await;
        let views = state.storage.view_storage();
        let db = views.get_connection();
        for (index, failed) in [
            "canonical_spec",
            "algo_version",
            "object_format",
            "filter_id",
        ]
        .iter()
        .enumerate()
        {
            let pk = 1500 + index as i64;
            let spec = format!(":/{}", (b'a' + index as u8) as char);
            let mut id = rewarm_filter(&state, pk, &spec, "recycled").await;
            match *failed {
                "canonical_spec" => {
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE mega_view_filter SET canonical_spec = $1 WHERE id = $2",
                        [Value::from(format!("{spec}/")), Value::from(pk)],
                    ))
                    .await
                    .unwrap();
                }
                "algo_version" => {
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE mega_view_filter SET algo_version = 2 WHERE id = $1",
                        [Value::from(pk)],
                    ))
                    .await
                    .unwrap();
                }
                "object_format" => {
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE mega_view_filter SET object_format = 'sha256' WHERE id = $1",
                        [Value::from(pk)],
                    ))
                    .await
                    .unwrap();
                }
                "filter_id" => {
                    id.replace_range(..1, if id.starts_with('0') { "1" } else { "0" });
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE mega_view_filter SET filter_id = $1 WHERE id = $2",
                        [Value::from(id.clone()), Value::from(pk)],
                    ))
                    .await
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let before = rewarm_rows(&state).await;
            assert_rewarm_unavailable(
                resolve_view_target(&state, &ViewLocator::FilterId(id.clone())).await,
                &id,
                ViewUnavailableReason::DefinitionCorrupt,
            );
            assert_eq!(before, rewarm_rows(&state).await);
            assert_rewarm_signal(&state, false).await;
        }
    }
}
