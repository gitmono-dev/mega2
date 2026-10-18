//! Traces principal binding and target authorization.
//!
//! Static push tokens and Mono accounts use different credential namespaces.
//! The commit author is not a principal. Callers that present no credential
//! receive [`TracesAuthDecision::Unauthenticated`]. Every other rejection is
//! [`TracesAuthDecision::NotFound`], including a missing binding, a missing
//! account snapshot, and a target the caller cannot read or write.

use std::str::FromStr;

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::{
    MegaError,
    callisto::trace_credential_binding,
    config::PushTokenConfig,
    contract::{
        git_protocol::token_covers_repo,
        policy::{
            builder::EntitySnapshot, context::CedarContext, enforcement::Enforcement,
            resource::resolve_resource, util::SaturnEUid,
        },
    },
};

/// Credential namespace stored on `trace_credential_binding.namespace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialNamespace {
    /// Named static push token. The credential id is the token name.
    StaticPushToken,
    /// Mono account mapping. The credential id is not the Cedar username.
    MonoAccount,
}

impl CredentialNamespace {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticPushToken => "static_push_token",
            Self::MonoAccount => "mono_account",
        }
    }
}

/// Who is asking. Anonymous is the absence of a credential, including SSH
/// none-auth. It is never treated as a code-read grant.
#[derive(Debug, Clone, Copy)]
pub enum TracesCaller<'a> {
    Anonymous,
    StaticPushToken {
        credential_id: &'a str,
        token: &'a PushTokenConfig,
    },
    MonoAccount {
        credential_id: &'a str,
        username: &'a str,
        enforcement: Enforcement,
        snapshot: Option<&'a EntitySnapshot>,
    },
}

/// Read uses the target read gate. Create and update use the write gate.
/// Update also requires the bound principal to be the stream owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracesAction<'a> {
    Read,
    /// First create binds the current principal as owner.
    Create,
    Update {
        owner_principal_id: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetAccess {
    Read,
    Write,
}

/// One traces authorization outcome.
///
/// `Allow` carries the stable principal from the active binding. `NotFound`
/// covers unknown and unauthorized callers so the two cannot be distinguished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracesAuthDecision {
    Allow { principal_id: String },
    Unauthenticated,
    NotFound,
}

/// Inputs for [`authorize_traces`]. `commit_author` is accepted and ignored.
#[derive(Debug, Clone, Copy)]
pub struct TracesAuthRequest<'a> {
    pub caller: TracesCaller<'a>,
    pub action: TracesAction<'a>,
    pub canonical_target_path: &'a str,
    pub commit_author: Option<&'a str>,
}

struct Presented<'a> {
    namespace: CredentialNamespace,
    credential_id: &'a str,
    kind: PresentedKind<'a>,
}

enum PresentedKind<'a> {
    Token {
        token: &'a PushTokenConfig,
    },
    Account {
        username: &'a str,
        enforcement: Enforcement,
        snapshot: Option<&'a EntitySnapshot>,
    },
}

/// Resolve a traces caller to a stable principal and a target decision.
///
/// The active binding is loaded from `trace_credential_binding` in the
/// caller's namespace only. A retired row is not a binding. Static tokens
/// then use [`token_covers_repo`] on `canonical_target_path`. Mono accounts
/// use `viewRepo` for read and `pushRepo` for write against the root
/// repository snapshot. `enforcement = off` does not consume a snapshot, and
/// a shadow allow does not override a Cedar denial; both fail closed here.
pub async fn authorize_traces(
    db: &DatabaseConnection,
    request: &TracesAuthRequest<'_>,
) -> Result<TracesAuthDecision, MegaError> {
    tracing::debug!(
        commit_author_ignored = request.commit_author.is_some(),
        "traces authorization ignores commit author"
    );
    let Some(presented) = presented_credential(request) else {
        return Ok(TracesAuthDecision::Unauthenticated);
    };
    let Some(principal_id) =
        active_principal(db, presented.namespace, presented.credential_id).await?
    else {
        return Ok(TracesAuthDecision::NotFound);
    };
    let access = match request.action {
        TracesAction::Read => TargetAccess::Read,
        TracesAction::Create | TracesAction::Update { .. } => TargetAccess::Write,
    };
    let owner_matches = match request.action {
        TracesAction::Update { owner_principal_id } => principal_id == owner_principal_id,
        TracesAction::Read | TracesAction::Create => true,
    };
    if owner_matches && target_allows(&presented, request.canonical_target_path, access) {
        Ok(TracesAuthDecision::Allow { principal_id })
    } else {
        Ok(TracesAuthDecision::NotFound)
    }
}

fn presented_credential<'a>(request: &TracesAuthRequest<'a>) -> Option<Presented<'a>> {
    match request.caller {
        TracesCaller::Anonymous => None,
        TracesCaller::StaticPushToken {
            credential_id,
            token,
        } if !credential_id.is_empty() => Some(Presented {
            namespace: CredentialNamespace::StaticPushToken,
            credential_id,
            kind: PresentedKind::Token { token },
        }),
        TracesCaller::StaticPushToken { .. } => None,
        TracesCaller::MonoAccount {
            credential_id,
            username,
            enforcement,
            snapshot,
        } if !credential_id.is_empty() && !username.is_empty() => Some(Presented {
            namespace: CredentialNamespace::MonoAccount,
            credential_id,
            kind: PresentedKind::Account {
                username,
                enforcement,
                snapshot,
            },
        }),
        TracesCaller::MonoAccount { .. } => None,
    }
}

async fn active_principal(
    db: &DatabaseConnection,
    namespace: CredentialNamespace,
    credential_id: &str,
) -> Result<Option<String>, MegaError> {
    let row = trace_credential_binding::Entity::find()
        .filter(trace_credential_binding::Column::Namespace.eq(namespace.as_str()))
        .filter(trace_credential_binding::Column::CredentialId.eq(credential_id))
        .filter(trace_credential_binding::Column::RetiredAt.is_null())
        .one(db)
        .await?;
    Ok(row.map(|row| row.principal_id))
}

fn target_allows(
    presented: &Presented<'_>,
    canonical_target_path: &str,
    access: TargetAccess,
) -> bool {
    match presented.kind {
        PresentedKind::Token { token } => {
            token.name == presented.credential_id && token_covers_repo(token, canonical_target_path)
        }
        PresentedKind::Account {
            username,
            enforcement,
            snapshot,
        } => account_allows(
            username,
            enforcement,
            snapshot,
            canonical_target_path,
            access,
        ),
    }
}

fn account_allows(
    username: &str,
    enforcement: Enforcement,
    snapshot: Option<&EntitySnapshot>,
    canonical_target_path: &str,
    access: TargetAccess,
) -> bool {
    // `off` does not build or consume a snapshot. Shadow records a would-deny
    // for code and still allows it; traces stay on the Cedar decision.
    if !enforcement.builds() {
        return false;
    }
    let Some(snapshot) = snapshot else {
        return false;
    };
    let action = match access {
        TargetAccess::Read => "viewRepo",
        TargetAccess::Write => "pushRepo",
    };
    cedar_allows(snapshot, username, canonical_target_path, action)
}

fn cedar_allows(
    snapshot: &EntitySnapshot,
    username: &str,
    canonical_target_path: &str,
    action_name: &str,
) -> bool {
    let store = snapshot.store();
    let resolution = resolve_resource(canonical_target_path, store);
    let Some(resource) = resolution.resource() else {
        return false;
    };
    let Ok(cedar) = CedarContext::new(store.clone()) else {
        return false;
    };
    let (Ok(user_type), Ok(action_type)) = (
        EntityTypeName::from_str("User"),
        EntityTypeName::from_str("Action"),
    ) else {
        return false;
    };
    let principal = SaturnEUid::from(EntityUid::from_type_name_and_id(
        user_type,
        EntityId::new(username),
    ));
    let action = SaturnEUid::from(EntityUid::from_type_name_and_id(
        action_type,
        EntityId::new(action_name),
    ));
    cedar
        .is_authorized(&principal, &action, resource, Context::empty())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, Set};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::trace_principal,
        ceres::protocol::AuthContext,
        config::GitConfig,
        contract::{
            git_protocol::{check_upload_pack_access, decide_push},
            policy::{builder::build_from_json, entitystore::generate_entity},
        },
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn migrated_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (temp_dir, db)
    }

    async fn principal(db: &sea_orm::DatabaseConnection) -> String {
        let id = Uuid::new_v4().to_string();
        trace_principal::ActiveModel {
            id: Set(id.clone()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(db)
        .await
        .expect("insert principal");
        id
    }

    async fn bind(
        db: &sea_orm::DatabaseConnection,
        principal_id: &str,
        namespace: CredentialNamespace,
        credential_id: &str,
    ) {
        trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.to_owned()),
            namespace: Set(namespace.as_str().to_owned()),
            credential_id: Set(credential_id.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
            retired_at: Set(None),
        }
        .insert(db)
        .await
        .expect("insert binding");
    }

    fn token(name: &str, paths: &[&str]) -> PushTokenConfig {
        PushTokenConfig {
            name: name.to_owned(),
            token: format!("synthetic-secret-{name}"),
            paths: Some(paths.iter().map(|path| (*path).to_owned()).collect()),
        }
    }

    fn snapshot(users: &[(&str, &str)]) -> EntitySnapshot {
        let mut value: serde_json::Value =
            serde_json::from_str(&generate_entity(&[], "/").expect("entities")).expect("json");
        let users_map = value["users"].as_object_mut().expect("users");
        for (name, group) in users {
            users_map.insert(
                format!("User::\"{name}\""),
                serde_json::json!({
                    "euid": format!("User::\"{name}\""),
                    "parents": [format!("UserGroup::\"{group}\"")]
                }),
            );
        }
        build_from_json(&value.to_string()).expect("snapshot")
    }

    #[tokio::test]
    async fn auth_scope_token_namespace_does_not_impersonate_account() {
        let (_temp, db) = migrated_db().await;
        let credential_id = "shared-synthetic";
        let token_principal = principal(&db).await;
        let account_principal = principal(&db).await;
        bind(
            &db,
            &token_principal,
            CredentialNamespace::StaticPushToken,
            credential_id,
        )
        .await;
        bind(
            &db,
            &account_principal,
            CredentialNamespace::MonoAccount,
            credential_id,
        )
        .await;

        let token = token(credential_id, &["/project/foo"]);
        let account_snapshot = snapshot(&[("admin", "admin")]);
        let token_decision = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id,
                    token: &token,
                },
                action: TracesAction::Create,
                canonical_target_path: "/project/foo",
                commit_author: Some(&account_principal),
            },
        )
        .await
        .expect("token decision");
        let account_decision = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id,
                    username: "admin",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&account_snapshot),
                },
                action: TracesAction::Create,
                canonical_target_path: "/project/foo",
                commit_author: Some("admin"),
            },
        )
        .await
        .expect("account decision");

        assert_ne!(token_principal, account_principal);
        assert_eq!(
            token_decision,
            TracesAuthDecision::Allow {
                principal_id: token_principal,
            }
        );
        assert_eq!(
            account_decision,
            TracesAuthDecision::Allow {
                principal_id: account_principal,
            }
        );

        let secret_as_id = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: &token.token,
                    token: &token,
                },
                action: TracesAction::Create,
                canonical_target_path: "/project/foo",
                commit_author: None,
            },
        )
        .await
        .expect("secret is not the credential id");
        assert_eq!(secret_as_id, TracesAuthDecision::NotFound);
    }

    #[tokio::test]
    async fn auth_scope_anonymous_traces_rejected_while_code_read_remains() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        let git = GitConfig {
            anonymous_access: true,
            ..GitConfig::default()
        };
        let auth = AuthContext {
            username: None,
            authenticated_user: None,
        };
        assert!(
            check_upload_pack_access(&git, &auth).await.is_ok(),
            "ordinary anonymous code read stays allowed"
        );

        for action in [
            TracesAction::Read,
            TracesAction::Create,
            TracesAction::Update {
                owner_principal_id: "11111111-1111-4111-8111-111111111111",
            },
        ] {
            let decision = authorize_traces(
                &db,
                &TracesAuthRequest {
                    caller: TracesCaller::Anonymous,
                    action,
                    canonical_target_path: "/project/foo",
                    commit_author: Some("admin"),
                },
            )
            .await
            .expect("anonymous traces decision");
            assert_eq!(decision, TracesAuthDecision::Unauthenticated);
        }
    }

    #[tokio::test]
    async fn auth_scope_unbound_credential_keeps_code_operation() {
        let (_temp, db) = migrated_db().await;
        let path = "/project/foo";
        let token = token("hook", &[path]);
        assert!(
            token_covers_repo(&token, path),
            "unbound token still covers the code path"
        );
        let token_traces = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "hook",
                    token: &token,
                },
                action: TracesAction::Create,
                canonical_target_path: path,
                commit_author: Some("hook"),
            },
        )
        .await
        .expect("unbound token traces");
        assert_eq!(token_traces, TracesAuthDecision::NotFound);

        let account_snapshot = snapshot(&[("admin", "admin"), ("nobody", "reader")]);
        assert!(
            decide_push(Enforcement::Enforce, &account_snapshot, "admin", path).is_ok(),
            "unbound account still passes the code push gate"
        );
        let account_traces = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "admin",
                    username: "admin",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&account_snapshot),
                },
                action: TracesAction::Read,
                canonical_target_path: path,
                commit_author: Some("admin"),
            },
        )
        .await
        .expect("unbound account traces");
        assert_eq!(account_traces, TracesAuthDecision::NotFound);

        assert!(
            decide_push(Enforcement::Off, &account_snapshot, "admin", path).is_ok(),
            "code push stays allowed when enforcement is off"
        );
        assert!(
            decide_push(Enforcement::Shadow, &account_snapshot, "outsider", path).is_ok(),
            "shadow still allows a code push that Cedar would deny"
        );
    }

    #[tokio::test]
    async fn auth_scope_only_owner_can_update() {
        let (_temp, db) = migrated_db().await;
        let path = "/project/foo";
        let owner_id = principal(&db).await;
        let other_id = principal(&db).await;
        bind(
            &db,
            &owner_id,
            CredentialNamespace::StaticPushToken,
            "owner-token",
        )
        .await;
        bind(
            &db,
            &other_id,
            CredentialNamespace::StaticPushToken,
            "other-token",
        )
        .await;
        let owner_token = token("owner-token", &[path]);
        let other_token = token("other-token", &[path]);

        let other_update = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "other-token",
                    token: &other_token,
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: Some(&owner_id),
            },
        )
        .await
        .expect("other update");
        assert_eq!(other_update, TracesAuthDecision::NotFound);

        let owner_update = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "owner-token",
                    token: &owner_token,
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: Some("not-the-owner"),
            },
        )
        .await
        .expect("owner update");
        assert_eq!(
            owner_update,
            TracesAuthDecision::Allow {
                principal_id: owner_id,
            }
        );

        let other_read = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "other-token",
                    token: &other_token,
                },
                action: TracesAction::Read,
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("other read");
        assert_eq!(
            other_read,
            TracesAuthDecision::Allow {
                principal_id: other_id,
            }
        );
    }

    #[tokio::test]
    async fn auth_scope_owner_requires_target_write() {
        let (_temp, db) = migrated_db().await;
        let path = "/project/foo";
        let owner_id = principal(&db).await;
        bind(
            &db,
            &owner_id,
            CredentialNamespace::StaticPushToken,
            "owner-token",
        )
        .await;
        bind(
            &db,
            &owner_id,
            CredentialNamespace::MonoAccount,
            "owner-account",
        )
        .await;

        let narrow_token = token("owner-token", &["/other"]);
        let narrow = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "owner-token",
                    token: &narrow_token,
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("token outside the target");
        assert_eq!(narrow, TracesAuthDecision::NotFound);
        assert!(
            !token_covers_repo(&narrow_token, path),
            "the code path gate agrees the token does not cover the target"
        );

        let covering_token = token("owner-token", &[path]);
        let covered = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::StaticPushToken {
                    credential_id: "owner-token",
                    token: &covering_token,
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("token covering the target");
        assert_eq!(
            covered,
            TracesAuthDecision::Allow {
                principal_id: owner_id.clone(),
            }
        );

        let roles = snapshot(&[
            ("owner-user", "reader"),
            ("admin", "admin"),
            ("nobody", "reader"),
        ]);
        let reader = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "owner-user",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: Some("owner-user"),
            },
        )
        .await
        .expect("reader owner");
        assert_eq!(reader, TracesAuthDecision::NotFound);
        assert!(
            decide_push(Enforcement::Enforce, &roles, "owner-user", path).is_err(),
            "the code push gate also denies a reader"
        );

        let reader_read = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "owner-user",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Read,
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("reader read");
        assert_eq!(
            reader_read,
            TracesAuthDecision::Allow {
                principal_id: owner_id.clone(),
            }
        );

        let admin = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "admin",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: Some("somebody-else"),
            },
        )
        .await
        .expect("admin owner");
        assert_eq!(
            admin,
            TracesAuthDecision::Allow {
                principal_id: owner_id.clone(),
            }
        );

        let missing_snapshot = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "admin",
                    enforcement: Enforcement::Enforce,
                    snapshot: None,
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("missing snapshot");
        assert_eq!(missing_snapshot, TracesAuthDecision::NotFound);

        let off = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "admin",
                    enforcement: Enforcement::Off,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("enforcement off");
        assert_eq!(off, TracesAuthDecision::NotFound);
        assert!(decide_push(Enforcement::Off, &roles, "admin", path).is_ok());

        let shadow = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "outsider",
                    enforcement: Enforcement::Shadow,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Read,
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("shadow would-deny");
        assert_eq!(shadow, TracesAuthDecision::NotFound);
        assert!(decide_push(Enforcement::Shadow, &roles, "outsider", path).is_ok());

        retire_account_binding(&db, "owner-account").await;
        let retired = authorize_traces(
            &db,
            &TracesAuthRequest {
                caller: TracesCaller::MonoAccount {
                    credential_id: "owner-account",
                    username: "admin",
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&roles),
                },
                action: TracesAction::Update {
                    owner_principal_id: &owner_id,
                },
                canonical_target_path: path,
                commit_author: None,
            },
        )
        .await
        .expect("retired binding");
        assert_eq!(retired, TracesAuthDecision::NotFound);
    }

    async fn retire_account_binding(db: &sea_orm::DatabaseConnection, credential_id: &str) {
        let row = trace_credential_binding::Entity::find()
            .filter(trace_credential_binding::Column::CredentialId.eq(credential_id))
            .filter(
                trace_credential_binding::Column::Namespace
                    .eq(CredentialNamespace::MonoAccount.as_str()),
            )
            .one(db)
            .await
            .expect("load binding")
            .expect("binding row");
        let mut active: trace_credential_binding::ActiveModel = row.into();
        active.retired_at = Set(Some(Utc::now().fixed_offset()));
        active.update(db).await.expect("retire binding");
    }
}
