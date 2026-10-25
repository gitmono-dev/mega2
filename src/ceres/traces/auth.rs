//! Traces principal binding and target authorization.
//!
//! Static push tokens and Mono accounts use different credential namespaces.
//! The commit author is not a principal. Callers that present no credential
//! receive [`TracesAuthDecision::Unauthenticated`]. Every other rejection is
//! [`TracesAuthDecision::NotFound`], including a missing binding, a missing
//! account snapshot, and a target the caller cannot read or write.

use std::str::FromStr;

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait, QueryFilter,
    Statement, Value,
};

use crate::{
    MegaError,
    callisto::trace_credential_binding,
    config::PushTokenConfig,
    contract::{
        git_protocol::token_covers_repo,
        policy::{
            builder::EntitySnapshot, context::CedarContext, enforcement::Enforcement,
            entitystore::SharedEntityStore, resource::resolve_resource, util::SaturnEUid,
        },
    },
    jupiter::storage::user_storage::AuthenticatedAccessToken,
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
    MonoAccessToken {
        token: &'a AuthenticatedAccessToken,
        enforcement: Enforcement,
        snapshot: Option<&'a EntitySnapshot>,
    },
    #[cfg(test)]
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
    if let TracesCaller::MonoAccessToken { token, .. } = request.caller
        && !token.is_current(db, false).await?
    {
        return Ok(TracesAuthDecision::NotFound);
    }
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
        TracesCaller::MonoAccessToken {
            token,
            enforcement,
            snapshot,
        } => Some(Presented {
            namespace: CredentialNamespace::MonoAccount,
            credential_id: token.credential_id(),
            kind: PresentedKind::Account {
                username: token.username(),
                enforcement,
                snapshot,
            },
        }),
        #[cfg(test)]
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
        #[cfg(test)]
        TracesCaller::MonoAccount { .. } => None,
    }
}

/// Authority presented to the publication transaction.
///
/// Account write permission is read from [`PublishAuthority::policy`] inside
/// that transaction. A snapshot carried on [`TracesCaller`] is not the
/// publication decision.
#[derive(Clone, Copy)]
pub struct PublishAuthority<'a> {
    pub caller: TracesCaller<'a>,
    pub principal_id: &'a str,
    pub target_id: &'a str,
    pub authorized_path: &'a str,
    pub expected_generation: i64,
    pub policy: Option<&'a SharedEntityStore>,
}

/// Recheck credential, target, binding generation, and write permission on `conn`.
///
/// `conn` is the publication transaction. A retired credential, an inactive
/// target, a changed path, a changed generation, or a lost write grant rejects
/// the publication.
pub async fn recheck_publish_authority<C: ConnectionTrait>(
    conn: &C,
    stream_id: &str,
    authority: &PublishAuthority<'_>,
) -> Result<(), MegaError> {
    let Some((namespace, credential_id)) = credential_identity(authority.caller) else {
        return Err(publish_unauthenticated());
    };
    let stream = trace_stream_owner(conn, stream_id).await?;
    if stream.principal_id != authority.principal_id || stream.target_id != authority.target_id {
        return Err(publish_rejected());
    }
    let Some(bound_principal) = locked_active_principal(conn, namespace, credential_id).await?
    else {
        return Err(publish_rejected());
    };
    if bound_principal != stream.principal_id {
        return Err(publish_rejected());
    }
    if let TracesCaller::MonoAccessToken { token, .. } = authority.caller
        && !token.is_current(conn, true).await?
    {
        return Err(publish_rejected());
    }
    let target = locked_target(conn, &stream.target_id).await?;
    if !target.active || target.project_path != authority.authorized_path {
        return Err(publish_rejected());
    }
    let generation =
        crate::jupiter::service::trace_target_service::binding_generation(conn, &stream.target_id)
            .await?;
    if generation != authority.expected_generation {
        return Err(publish_rejected());
    }
    let live_policy = live_account_policy(authority)?;
    let presented = presented_for_publish(authority, live_policy.as_deref())?;
    if !target_allows(&presented, &target.project_path, TargetAccess::Write) {
        return Err(publish_rejected());
    }
    Ok(())
}

fn live_account_policy(
    authority: &PublishAuthority<'_>,
) -> Result<Option<std::sync::Arc<EntitySnapshot>>, MegaError> {
    let account = match authority.caller {
        TracesCaller::MonoAccessToken { .. } => true,
        #[cfg(test)]
        TracesCaller::MonoAccount { .. } => true,
        TracesCaller::Anonymous | TracesCaller::StaticPushToken { .. } => false,
    };
    if !account {
        return Ok(None);
    }
    let store = authority.policy.ok_or_else(publish_rejected)?;
    if store.is_dirty() {
        return Err(publish_rejected());
    }
    Ok(Some(store.snapshot().ok_or_else(publish_rejected)?))
}

fn presented_for_publish<'a>(
    authority: &'a PublishAuthority<'a>,
    live_policy: Option<&'a EntitySnapshot>,
) -> Result<Presented<'a>, MegaError> {
    match authority.caller {
        TracesCaller::MonoAccessToken {
            token, enforcement, ..
        } => Ok(Presented {
            namespace: CredentialNamespace::MonoAccount,
            credential_id: token.credential_id(),
            kind: PresentedKind::Account {
                username: token.username(),
                enforcement,
                snapshot: Some(live_policy.ok_or_else(publish_rejected)?),
            },
        }),
        #[cfg(test)]
        TracesCaller::MonoAccount {
            credential_id,
            username,
            enforcement,
            ..
        } if !credential_id.is_empty() && !username.is_empty() => Ok(Presented {
            namespace: CredentialNamespace::MonoAccount,
            credential_id,
            kind: PresentedKind::Account {
                username,
                enforcement,
                snapshot: Some(live_policy.ok_or_else(publish_rejected)?),
            },
        }),
        #[cfg(test)]
        TracesCaller::MonoAccount { .. } => Err(publish_rejected()),
        TracesCaller::Anonymous | TracesCaller::StaticPushToken { .. } => {
            presented_credential(&TracesAuthRequest {
                caller: authority.caller,
                action: TracesAction::Update {
                    owner_principal_id: authority.principal_id,
                },
                canonical_target_path: authority.authorized_path,
                commit_author: None,
            })
            .ok_or_else(publish_rejected)
        }
    }
}

struct StreamOwner {
    principal_id: String,
    target_id: String,
}

struct LockedTarget {
    active: bool,
    project_path: String,
}

fn credential_identity(caller: TracesCaller<'_>) -> Option<(CredentialNamespace, &str)> {
    match caller {
        TracesCaller::Anonymous => None,
        TracesCaller::StaticPushToken { credential_id, .. } if !credential_id.is_empty() => {
            Some((CredentialNamespace::StaticPushToken, credential_id))
        }
        TracesCaller::StaticPushToken { .. } => None,
        TracesCaller::MonoAccessToken { token, .. } => {
            Some((CredentialNamespace::MonoAccount, token.credential_id()))
        }
        #[cfg(test)]
        TracesCaller::MonoAccount { credential_id, .. } if !credential_id.is_empty() => {
            Some((CredentialNamespace::MonoAccount, credential_id))
        }
        #[cfg(test)]
        TracesCaller::MonoAccount { .. } => None,
    }
}

async fn trace_stream_owner<C: ConnectionTrait>(
    conn: &C,
    stream_id: &str,
) -> Result<StreamOwner, MegaError> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT principal_id, target_id FROM trace_stream WHERE id = $1 FOR UPDATE",
            [Value::from(stream_id.to_owned())],
        ))
        .await?
        .ok_or_else(publish_rejected)?;
    Ok(StreamOwner {
        principal_id: row.try_get("", "principal_id")?,
        target_id: row.try_get("", "target_id")?,
    })
}

async fn locked_active_principal<C: ConnectionTrait>(
    conn: &C,
    namespace: CredentialNamespace,
    credential_id: &str,
) -> Result<Option<String>, MegaError> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT principal_id FROM trace_credential_binding
             WHERE namespace = $1 AND credential_id = $2 AND retired_at IS NULL
             FOR UPDATE",
            [
                Value::from(namespace.as_str()),
                Value::from(credential_id.to_owned()),
            ],
        ))
        .await?;
    Ok(match row {
        Some(row) => Some(row.try_get("", "principal_id")?),
        None => None,
    })
}

async fn locked_target<C: ConnectionTrait>(
    conn: &C,
    target_id: &str,
) -> Result<LockedTarget, MegaError> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT active, project_path FROM trace_target WHERE id = $1 FOR UPDATE",
            [Value::from(target_id.to_owned())],
        ))
        .await?
        .ok_or_else(publish_rejected)?;
    Ok(LockedTarget {
        active: row.try_get("", "active")?,
        project_path: row.try_get("", "project_path")?,
    })
}

/// The typed rejection shared by every publication recheck, including the
/// first-scope identity/path checks in the publish service.
pub fn publish_rejection() -> MegaError {
    publish_rejected()
}

fn publish_rejected() -> MegaError {
    MegaError::Traces(crate::common::errors::TracesError::UnknownScope)
}

fn publish_unauthenticated() -> MegaError {
    MegaError::Traces(crate::common::errors::TracesError::Unauthenticated)
}

async fn active_principal<C: ConnectionTrait>(
    db: &C,
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
        MegaError,
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

    fn policy_json(users: &[(&str, &str)]) -> String {
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
        value.to_string()
    }

    fn snapshot(users: &[(&str, &str)]) -> EntitySnapshot {
        build_from_json(&policy_json(users)).expect("snapshot")
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

    struct PublishFixture {
        stream_id: String,
        principal_id: String,
        target_id: String,
        path: String,
        credential_id: String,
    }

    async fn publish_fixture(db: &sea_orm::DatabaseConnection) -> PublishFixture {
        let principal_id = principal(db).await;
        let target_id = Uuid::new_v4().to_string();
        let path = format!("/team/{target_id}");
        let credential_id = format!("publish-{target_id}");
        let now = Utc::now().fixed_offset();
        crate::callisto::trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(path.clone()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        bind(
            db,
            &principal_id,
            CredentialNamespace::StaticPushToken,
            &credential_id,
        )
        .await;
        let stream_id = crate::callisto::trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("stream")
        .id;
        PublishFixture {
            stream_id,
            principal_id,
            target_id,
            path,
            credential_id,
        }
    }

    fn publish_token(fixture: &PublishFixture, paths: &[&str]) -> PushTokenConfig {
        token(&fixture.credential_id, paths)
    }

    fn publish_authority<'a>(
        fixture: &'a PublishFixture,
        token: &'a PushTokenConfig,
    ) -> PublishAuthority<'a> {
        PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id: &fixture.credential_id,
                token,
            },
            principal_id: &fixture.principal_id,
            target_id: &fixture.target_id,
            authorized_path: &fixture.path,
            expected_generation: 1,
            policy: None,
        }
    }

    fn assert_rejected(err: MegaError) {
        assert!(
            matches!(
                err,
                MegaError::Traces(crate::common::errors::TracesError::UnknownScope)
            ),
            "publication rejection is unknown_scope, got {err:?}"
        );
    }

    #[tokio::test]
    async fn publish_authority_allows_a_current_credential_and_write_grant() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let token = publish_token(&fixture, &[&fixture.path]);
        let authority = publish_authority(&fixture, &token);
        recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect("current credential, active target, generation 1, and write grant");
    }

    #[tokio::test]
    async fn publish_authority_rejects_a_retired_credential() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let row = trace_credential_binding::Entity::find()
            .filter(trace_credential_binding::Column::CredentialId.eq(&fixture.credential_id))
            .one(&db)
            .await
            .expect("binding query")
            .expect("binding");
        let mut binding: trace_credential_binding::ActiveModel = row.into();
        binding.retired_at = Set(Some(Utc::now().fixed_offset()));
        binding.update(&db).await.expect("retire");
        let token = publish_token(&fixture, &[&fixture.path]);
        let authority = publish_authority(&fixture, &token);
        let err = recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect_err("retired credential");
        assert_rejected(err);
    }

    #[tokio::test]
    async fn publish_authority_rejects_an_inactive_target() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let row = crate::callisto::trace_target::Entity::find_by_id(&fixture.target_id)
            .one(&db)
            .await
            .expect("target query")
            .expect("target");
        let mut target: crate::callisto::trace_target::ActiveModel = row.into();
        target.active = Set(false);
        target.update(&db).await.expect("deactivate");
        let token = publish_token(&fixture, &[&fixture.path]);
        let authority = publish_authority(&fixture, &token);
        let err = recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect_err("inactive target");
        assert_rejected(err);
    }

    #[tokio::test]
    async fn publish_authority_rejects_a_rebound_path() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let row = crate::callisto::trace_target::Entity::find_by_id(&fixture.target_id)
            .one(&db)
            .await
            .expect("target query")
            .expect("target");
        let mut target: crate::callisto::trace_target::ActiveModel = row.into();
        target.project_path = Set(format!("/other/{}", fixture.target_id));
        target.update(&db).await.expect("rebind");
        let token = publish_token(
            &fixture,
            &[&fixture.path, &format!("/other/{}", fixture.target_id)],
        );
        let authority = publish_authority(&fixture, &token);
        let err = recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect_err("rebound path");
        assert_rejected(err);
    }

    #[tokio::test]
    async fn publish_authority_rejects_a_stale_generation() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        crate::callisto::audit_logs::ActiveModel {
            id: Set(910_037),
            actor_id: Set(0),
            actor_type: Set(crate::callisto::sea_orm_active_enums::ActorTypeEnum::Human),
            action: Set(crate::callisto::sea_orm_active_enums::AuditActionEnum::Update),
            target_type: Set(crate::callisto::sea_orm_active_enums::TargetTypeEnum::Repository),
            target_id: Set(0),
            metadata: Set(Some(serde_json::json!({
                "trace_target_id": fixture.target_id,
                "binding_generation": 2
            }))),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("generation audit");
        let token = publish_token(&fixture, &[&fixture.path]);
        let stale = publish_authority(&fixture, &token);
        let err = recheck_publish_authority(&db, &fixture.stream_id, &stale)
            .await
            .expect_err("stale generation");
        assert_rejected(err);
        let mut current = publish_authority(&fixture, &token);
        current.expected_generation = 2;
        recheck_publish_authority(&db, &fixture.stream_id, &current)
            .await
            .expect("generation 2 matches the detach audit");
    }

    #[tokio::test]
    async fn publish_authority_rejects_a_token_outside_the_target_path() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let token = publish_token(&fixture, &["/elsewhere"]);
        let authority = publish_authority(&fixture, &token);
        let err = recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect_err("token does not cover the current path");
        assert_rejected(err);
    }

    #[tokio::test]
    async fn publish_authority_rejects_an_anonymous_caller() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let authority = PublishAuthority {
            caller: TracesCaller::Anonymous,
            principal_id: &fixture.principal_id,
            target_id: &fixture.target_id,
            authorized_path: &fixture.path,
            expected_generation: 1,
            policy: None,
        };
        let err = recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect_err("anonymous caller");
        assert!(
            matches!(
                err,
                MegaError::Traces(crate::common::errors::TracesError::Unauthenticated)
            ),
            "a missing credential is unauthenticated, got {err:?}"
        );
    }

    #[tokio::test]
    async fn publish_authority_rechecks_the_live_account_policy() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let account_id = format!("account-{}", fixture.target_id);
        bind(
            &db,
            &fixture.principal_id,
            CredentialNamespace::MonoAccount,
            &account_id,
        )
        .await;
        let store = SharedEntityStore::new();
        store
            .swap(&policy_json(&[("publisher", "admin")]))
            .expect("admin policy");
        let stale = snapshot(&[("publisher", "reader")]);
        let authority = PublishAuthority {
            caller: TracesCaller::MonoAccount {
                credential_id: &account_id,
                username: "publisher",
                enforcement: Enforcement::Enforce,
                snapshot: Some(&stale),
            },
            principal_id: &fixture.principal_id,
            target_id: &fixture.target_id,
            authorized_path: &fixture.path,
            expected_generation: 1,
            policy: Some(&store),
        };
        recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .expect("the live admin snapshot grants write even when the caller snapshot does not");

        let mut off = authority;
        off.caller = TracesCaller::MonoAccount {
            credential_id: &account_id,
            username: "publisher",
            enforcement: Enforcement::Off,
            snapshot: Some(&stale),
        };
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &off)
                .await
                .expect_err("off does not grant traces write"),
        );
        let mut shadow = authority;
        shadow.caller = TracesCaller::MonoAccount {
            credential_id: &account_id,
            username: "publisher",
            enforcement: Enforcement::Shadow,
            snapshot: Some(&stale),
        };
        recheck_publish_authority(&db, &fixture.stream_id, &shadow)
            .await
            .expect("shadow follows the live Cedar allow");

        store
            .swap(&policy_json(&[("publisher", "reader")]))
            .expect("reader policy");
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .expect_err("the same authority now sees the reader snapshot"),
        );
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &shadow)
                .await
                .expect_err("shadow does not turn a Cedar denial into a traces write"),
        );

        store
            .swap(&policy_json(&[("publisher", "admin")]))
            .expect("restore admin policy");
        store.mark_dirty();
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .expect_err("a dirty policy store fails closed"),
        );
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
    #[tokio::test]
    async fn first_scope_authority_rejects_new_target_retired_credential_and_generation_drift() {
        use sea_orm::TransactionTrait;

        use crate::callisto::{trace_stream, trace_target};

        let (_temp, db) = migrated_db().await;
        let principal_id = principal(&db).await;
        let credential_id = "first-scope-operator";
        bind(
            &db,
            &principal_id,
            CredentialNamespace::StaticPushToken,
            credential_id,
        )
        .await;
        let target_id = Uuid::new_v4().to_string();
        let now = Utc::now().fixed_offset();
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set("/project/foo".to_owned()),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        let stream_id = Uuid::new_v4().to_string();
        trace_stream::ActiveModel {
            id: Set(stream_id.clone()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream");

        let token = token(credential_id, &["/project/foo"]);
        let authority = PublishAuthority {
            caller: TracesCaller::StaticPushToken {
                credential_id,
                token: &token,
            },
            principal_id: &principal_id,
            target_id: &target_id,
            authorized_path: "/project/foo",
            expected_generation: 1,
            policy: None,
        };
        let txn = db.begin().await.expect("txn");
        recheck_publish_authority(&txn, &stream_id, &authority)
            .await
            .expect("bound target is accepted");
        txn.rollback().await.expect("rollback");

        let stranger = Uuid::new_v4().to_string();
        let stranger_authority = PublishAuthority {
            target_id: &stranger,
            ..authority
        };
        let txn = db.begin().await.expect("txn");
        assert!(
            recheck_publish_authority(&txn, &stream_id, &stranger_authority)
                .await
                .is_err()
        );
        txn.rollback().await.expect("rollback");

        let stale_generation = PublishAuthority {
            expected_generation: 2,
            ..authority
        };
        let txn = db.begin().await.expect("txn");
        assert!(
            recheck_publish_authority(&txn, &stream_id, &stale_generation)
                .await
                .is_err()
        );
        txn.rollback().await.expect("rollback");

        let moved_path = PublishAuthority {
            authorized_path: "/project/bar",
            ..authority
        };
        let txn = db.begin().await.expect("txn");
        assert!(
            recheck_publish_authority(&txn, &stream_id, &moved_path)
                .await
                .is_err()
        );
        txn.rollback().await.expect("rollback");

        db.execute_unprepared(&format!(
            "UPDATE trace_credential_binding SET retired_at = now() \
             WHERE namespace = 'static_push_token' AND credential_id = '{credential_id}'"
        ))
        .await
        .expect("retire credential");
        let txn = db.begin().await.expect("txn");
        assert!(
            recheck_publish_authority(&txn, &stream_id, &authority)
                .await
                .is_err()
        );
        txn.rollback().await.expect("rollback");
    }
    async fn authenticated_token(
        db: &DatabaseConnection,
        id: i64,
        username: &str,
    ) -> AuthenticatedAccessToken {
        use crate::jupiter::storage::{
            base_storage::{BaseStorage, StorageConnector},
            user_storage::UserStorage,
        };
        let secret = format!("synthetic-mono-token-{id}");
        crate::callisto::access_token::ActiveModel {
            id: Set(id),
            username: Set(username.into()),
            token: Set(secret.clone()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .unwrap();
        UserStorage {
            base: BaseStorage::new(std::sync::Arc::new(db.clone())),
        }
        .authenticate_traces_access_token(&secret)
        .await
        .unwrap()
        .unwrap()
    }

    fn mono_authority<'a>(
        fixture: &'a PublishFixture,
        token: &'a AuthenticatedAccessToken,
        policy: &'a SharedEntityStore,
    ) -> PublishAuthority<'a> {
        PublishAuthority {
            caller: TracesCaller::MonoAccessToken {
                token,
                enforcement: Enforcement::Enforce,
                snapshot: None,
            },
            principal_id: &fixture.principal_id,
            target_id: &fixture.target_id,
            authorized_path: &fixture.path,
            expected_generation: 1,
            policy: Some(policy),
        }
    }

    #[tokio::test]
    async fn mono_token_rotation_keeps_explicit_principal_and_namespace() {
        let (_temp, db) = migrated_db().await;
        let owner = principal(&db).await;
        let other = principal(&db).await;
        let first = authenticated_token(&db, 7001, "publisher").await;
        let second = authenticated_token(&db, 7002, "publisher").await;
        bind(
            &db,
            &other,
            CredentialNamespace::StaticPushToken,
            first.credential_id(),
        )
        .await;
        bind(
            &db,
            &owner,
            CredentialNamespace::MonoAccount,
            first.credential_id(),
        )
        .await;
        let policy = snapshot(&[("publisher", "admin")]);
        let request = |token| TracesAuthRequest {
            caller: TracesCaller::MonoAccessToken {
                token,
                enforcement: Enforcement::Enforce,
                snapshot: Some(&policy),
            },
            action: TracesAction::Read,
            canonical_target_path: "/team/app",
            commit_author: Some("another-user"),
        };
        assert_eq!(
            authorize_traces(&db, &request(&first)).await.unwrap(),
            TracesAuthDecision::Allow {
                principal_id: owner.clone()
            }
        );
        assert_eq!(
            authorize_traces(&db, &request(&second)).await.unwrap(),
            TracesAuthDecision::NotFound
        );
        bind(
            &db,
            &owner,
            CredentialNamespace::MonoAccount,
            second.credential_id(),
        )
        .await;
        trace_credential_binding::Entity::update_many()
            .col_expr(
                trace_credential_binding::Column::RetiredAt,
                sea_orm::sea_query::Expr::value(Utc::now().fixed_offset()),
            )
            .filter(trace_credential_binding::Column::Namespace.eq("mono_account"))
            .filter(trace_credential_binding::Column::CredentialId.eq(first.credential_id()))
            .exec(&db)
            .await
            .unwrap();
        assert_eq!(
            authorize_traces(&db, &request(&first)).await.unwrap(),
            TracesAuthDecision::NotFound
        );
        assert_eq!(
            authorize_traces(&db, &request(&second)).await.unwrap(),
            TracesAuthDecision::Allow {
                principal_id: owner
            }
        );
    }

    #[tokio::test]
    async fn mono_token_publication_rejects_deleted_or_replaced_source() {
        use crate::callisto::access_token;
        for mutation in 0..3 {
            let (_temp, db) = migrated_db().await;
            let fixture = publish_fixture(&db).await;
            let token = authenticated_token(&db, 7100 + mutation, "publisher").await;
            bind(
                &db,
                &fixture.principal_id,
                CredentialNamespace::MonoAccount,
                token.credential_id(),
            )
            .await;
            let store = SharedEntityStore::new();
            store.swap(&policy_json(&[("publisher", "admin")])).unwrap();
            let authority = mono_authority(&fixture, &token, &store);
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap();
            match mutation {
                0 => {
                    access_token::Entity::delete_by_id(token.id())
                        .exec(&db)
                        .await
                        .unwrap();
                }
                1 => {
                    access_token::Entity::update_many()
                        .col_expr(
                            access_token::Column::Token,
                            sea_orm::sea_query::Expr::value("synthetic-replaced-secret"),
                        )
                        .filter(access_token::Column::Id.eq(token.id()))
                        .exec(&db)
                        .await
                        .unwrap();
                }
                _ => {
                    access_token::Entity::update_many()
                        .col_expr(
                            access_token::Column::Username,
                            sea_orm::sea_query::Expr::value("other"),
                        )
                        .filter(access_token::Column::Id.eq(token.id()))
                        .exec(&db)
                        .await
                        .unwrap();
                }
            }
            assert_rejected(
                recheck_publish_authority(&db, &fixture.stream_id, &authority)
                    .await
                    .unwrap_err(),
            );
            let policy = store.snapshot().unwrap();
            let request = TracesAuthRequest {
                caller: TracesCaller::MonoAccessToken {
                    token: &token,
                    enforcement: Enforcement::Enforce,
                    snapshot: Some(&policy),
                },
                action: TracesAction::Read,
                canonical_target_path: &fixture.path,
                commit_author: None,
            };
            assert_eq!(
                authorize_traces(&db, &request).await.unwrap(),
                TracesAuthDecision::NotFound
            );
        }
    }

    #[tokio::test]
    async fn mono_token_publication_uses_live_policy_and_binding() {
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let token = authenticated_token(&db, 7201, "publisher").await;
        bind(
            &db,
            &fixture.principal_id,
            CredentialNamespace::MonoAccount,
            token.credential_id(),
        )
        .await;
        let store = SharedEntityStore::new();
        store.swap(&policy_json(&[("publisher", "admin")])).unwrap();
        let stale = store.snapshot().unwrap();
        let mut authority = mono_authority(&fixture, &token, &store);
        authority.caller = TracesCaller::MonoAccessToken {
            token: &token,
            enforcement: Enforcement::Enforce,
            snapshot: Some(&stale),
        };
        recheck_publish_authority(&db, &fixture.stream_id, &authority)
            .await
            .unwrap();
        store
            .swap(&policy_json(&[("publisher", "reader")]))
            .unwrap();
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
        authority.caller = TracesCaller::MonoAccessToken {
            token: &token,
            enforcement: Enforcement::Shadow,
            snapshot: Some(&stale),
        };
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
        store.swap(&policy_json(&[("publisher", "admin")])).unwrap();
        authority.caller = TracesCaller::MonoAccessToken {
            token: &token,
            enforcement: Enforcement::Off,
            snapshot: Some(&stale),
        };
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
        authority.caller = TracesCaller::MonoAccessToken {
            token: &token,
            enforcement: Enforcement::Enforce,
            snapshot: Some(&stale),
        };
        store.mark_dirty();
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
        store.swap(&policy_json(&[("publisher", "admin")])).unwrap();
        trace_credential_binding::Entity::update_many()
            .col_expr(
                trace_credential_binding::Column::RetiredAt,
                sea_orm::sea_query::Expr::value(Utc::now().fixed_offset()),
            )
            .filter(trace_credential_binding::Column::Namespace.eq("mono_account"))
            .filter(trace_credential_binding::Column::CredentialId.eq(token.credential_id()))
            .exec(&db)
            .await
            .unwrap();
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn mono_token_publication_holds_source_lock_until_commit() {
        use std::time::Duration;

        use sea_orm::TransactionTrait;
        let (_temp, db) = migrated_db().await;
        let fixture = publish_fixture(&db).await;
        let token = authenticated_token(&db, 7301, "publisher").await;
        bind(
            &db,
            &fixture.principal_id,
            CredentialNamespace::MonoAccount,
            token.credential_id(),
        )
        .await;
        let store = SharedEntityStore::new();
        store.swap(&policy_json(&[("publisher", "admin")])).unwrap();
        let authority = mono_authority(&fixture, &token, &store);
        let publication = db.begin().await.unwrap();
        let publisher: i32 = publication
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_backend_pid() AS pid",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "pid")
            .unwrap();
        recheck_publish_authority(&publication, &fixture.stream_id, &authority)
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let delete = async {
            let deleting = db.begin().await.unwrap();
            let deleter: i32 = deleting
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT pg_backend_pid() AS pid",
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get("", "pid")
                .unwrap();
            tx.send(deleter).unwrap();
            let result = deleting
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "DELETE FROM access_token WHERE id = $1",
                    [token.id().into()],
                ))
                .await
                .unwrap();
            deleting.commit().await.unwrap();
            result.rows_affected()
        };
        let observe_and_release = async {
            let deleter = rx.await.unwrap();
            let blocked = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let row = db
                        .query_one_raw(Statement::from_sql_and_values(
                            DatabaseBackend::Postgres,
                            "SELECT $1::integer = ANY(pg_blocking_pids($2::integer)) AS blocked",
                            [publisher.into(), deleter.into()],
                        ))
                        .await
                        .unwrap()
                        .unwrap();
                    if row.try_get::<bool>("", "blocked").unwrap() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            publication.commit().await.unwrap();
            blocked
        };
        let (deleted, blocked) = tokio::join!(delete, observe_and_release);
        assert!(
            blocked.is_ok(),
            "the real deletion must wait on the publication backend"
        );
        assert_eq!(deleted, 1);
        assert_rejected(
            recheck_publish_authority(&db, &fixture.stream_id, &authority)
                .await
                .unwrap_err(),
        );
    }
}
