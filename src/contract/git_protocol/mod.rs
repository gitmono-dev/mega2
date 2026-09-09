use std::str::FromStr;

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    ceres::{api_service::state::ProtocolApiState, protocol::AuthContext},
    common::errors::ProtocolError,
    config::{GitConfig, PushAuth, PushTokenConfig, token_path_authorizes},
    contract::policy::{
        context::CedarContext,
        enforcement::{Enforcement, EnforcementDecision, decide},
        resource::resolve_resource,
        util::SaturnEUid,
    },
};

pub mod http;
pub mod path;
pub mod ssh;

#[derive(Deserialize, Debug)]
pub struct InfoRefsParams {
    pub service: Option<String>,
    pub refspec: Option<String>,
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

fn token_covers_repo(token: &PushTokenConfig, repo_path: &str) -> bool {
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

    use super::*;
    use crate::{
        ceres::{
            api_service::{cache::GitObjectCache, state::ProtocolApiState},
            protocol::PushUserInfo,
        },
        contract::{
            git_protocol::ssh::SshServer,
            policy::{
                builder::build_from_json,
                entitystore::{SharedEntityStore, generate_entity},
            },
        },
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
}
