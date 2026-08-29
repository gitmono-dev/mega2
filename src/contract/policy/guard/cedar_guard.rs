use std::{collections::HashMap, str::FromStr};

use axum::{
    extract::{FromRef, FromRequestParts, Request, State},
    middleware::Next,
    response::Response,
};
use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use http::StatusCode;
use once_cell::sync::Lazy;

use crate::{
    api::{
        MonoApiServiceState,
        oauth::{BotIdentity, OptionalSessionUser},
    },
    common::errors::{ApiError, MegaError},
    contract::policy::{
        ActionEnum,
        builder::EntitySnapshot,
        context::CedarContext,
        enforcement::{Enforcement, EnforcementDecision, decide},
        resource::resolve_resource,
        util::SaturnEUid,
    },
};

/// Guarded endpoints, keyed by `{method, path}`: prefix → lowercase HTTP
/// method → path pattern → action.
///
/// The method is part of the key because the same path serves different
/// actions per method — `GET /{link}/reviewers` only reads the reviewer list
/// while `POST`/`DELETE` on that same path change it. Keying on the path alone
/// collapsed those into one action, so one of them was always mapped wrong.
type EndPointConfig = HashMap<String, HashMap<String, HashMap<String, String>>>;

/// Mount point of the API router the guard protects (`http_server.rs` nests it
/// under this prefix).
const API_PREFIX: &str = "/api/v1";
static GURADED_ENDPOINTS: Lazy<EndPointConfig> = Lazy::new(|| {
    let endpoints_config_dict: &str = include_str!("guarded_endpoints.json");
    serde_json::from_str(endpoints_config_dict).unwrap_or_else(|e| {
        tracing::error!("Failed to read endpoints configuration for guard {:}", e);
        EndPointConfig::new()
    })
});

/// Resolve the guarded action for a `{method, path}` pair.
///
/// A path with no registered entry *for this method* is unprotected: an
/// unmapped operation must not inherit another method's action.
pub fn resolve_cl_action(method: &str, req_path: &str) -> Result<(ActionEnum, String), MegaError> {
    let cl_path_prefix = "/cl";
    // The guard is a route layer on the router nested under `/api/v1`
    // (`http_server.rs`), so a request arrives with that prefix while the
    // mapping is written relative to the router. Without stripping it, no
    // guarded path ever matched and every endpoint fell through as
    // unprotected.
    let req_path = req_path.strip_prefix(API_PREFIX).unwrap_or(req_path);
    // Avoid parsing request of non-CL endpoints
    if !req_path.starts_with(cl_path_prefix) {
        return Ok((ActionEnum::UnprotectedRequest, String::new()));
    }
    let path = req_path.trim_start_matches(cl_path_prefix);

    let cl_config = GURADED_ENDPOINTS.get(cl_path_prefix).ok_or_else(|| {
        MegaError::Other("No CL config found in guarded_endpoints.json".to_string())
    })?;

    let method = method.to_ascii_lowercase();
    let Some(method_config) = cl_config.get(&method) else {
        tracing::warn!("No matching CL action for {} {}", method, req_path);
        return Ok((ActionEnum::UnprotectedRequest, String::new()));
    };

    let Some((action, mr_link)) = match_operation(path, method_config) else {
        tracing::warn!("No matching CL action for {} {}", method, req_path);
        return Ok((ActionEnum::UnprotectedRequest, String::new()));
    };

    Ok((action, mr_link))
}

//TODO: Only match cl api paths for now, extend when need in the future
/// return (ActionEnum, mr_link)
fn match_operation(
    suffix: &str,
    patterns: &HashMap<String, String>,
) -> Option<(ActionEnum, String)> {
    let suffix = suffix.trim_matches('/');

    for (pattern, action) in patterns {
        let pattern_trimmed = pattern.trim_matches('/');

        if pattern_trimmed.contains("{link}") {
            let parts: Vec<&str> = pattern_trimmed.split("{link}").collect();
            if parts.len() == 2 {
                let prefix = parts[0].trim_matches('/');
                let op = parts[1].trim_matches('/');

                if (prefix.is_empty() || suffix.starts_with(prefix))
                    && (op.is_empty() || suffix.ends_with(op))
                {
                    // Bounds check: ensure suffix is long enough
                    let prefix_len = prefix.len();
                    let op_len = op.len();
                    if prefix_len + op_len > suffix.len() {
                        continue;
                    }

                    let start = if prefix.is_empty() { 0 } else { prefix_len };
                    let end = if op.is_empty() {
                        suffix.len()
                    } else {
                        suffix.len() - op_len
                    };

                    if start > end {
                        continue;
                    }

                    let mr_link = &suffix[start..end];

                    return Some((
                        ActionEnum::from(action.as_str()),
                        mr_link.trim_matches('/').to_string(),
                    ));
                }
            }
        } else if suffix == pattern_trimmed {
            return Some((ActionEnum::from(action.as_str()), String::new()));
        }
    }
    None
}

/// Principal id used when a request carries no authenticated subject.
///
/// A reserved literal rather than a plausible username (ADR-UN-06 ⑤): the old
/// `"reader"` fallback collided with a real account named `reader`, which would
/// have handed every anonymous request that account's permissions. UN-14's
/// builder refuses to let ACL input occupy this name.
pub const ANONYMOUS_PRINCIPAL_ID: &str = "__anonymous__";

/// What the guard evaluates the request against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardResource {
    /// Repository path this request touches.
    Path(String),
    /// The mapping produced a CL link, but no such CL exists (or it could not
    /// be read). There is no resource to authorize against, so this is
    /// fail-closed under `enforce`.
    UnknownLink(String),
}

impl GuardResource {
    fn label(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::UnknownLink(link) => link,
        }
    }
}

/// Resolve the repository a guarded request acts on.
///
/// A mapped endpoint carrying a CL link resolves through `get_cl(link)` to the
/// CL's real path (UN-10's unique index makes that an equality probe). Mapped
/// endpoints without a link (`/cl/labels`, `/cl/assignees`) act on the monorepo
/// root. A storage failure is treated exactly like a missing CL: the guard must
/// not fall open just because the lookup failed.
async fn resolve_guard_resource(state: &MonoApiServiceState, link: &str) -> GuardResource {
    if link.is_empty() {
        return GuardResource::Path("/".to_owned());
    }
    match state.storage.cl_storage().get_cl(link).await {
        Ok(Some(cl)) => GuardResource::Path(cl.path),
        Ok(None) => GuardResource::UnknownLink(link.to_owned()),
        Err(error) => {
            tracing::warn!(
                event = "authz_resource_lookup_failed",
                link = %link,
                error = %error,
                "CL lookup failed; treating the resource as unresolvable (fail-closed)"
            );
            GuardResource::UnknownLink(link.to_owned())
        }
    }
}

/// Pure three-state guard decision (ADR-UN-01/05, UN-08).
///
/// The request path is normalized to the single monorepo root repository
/// (ADR-UN-05, UN-11); a missing root, an unresolvable CL link, or a policy
/// engine error all count as would-deny. `shadow` allows but records it.
pub fn decide_guard(
    enforcement: Enforcement,
    snapshot: &EntitySnapshot,
    principal_type: &str,
    principal_id: &str,
    action: &str,
    resource: &GuardResource,
) -> Result<(), ApiError> {
    let entity_store = snapshot.store();

    let would_deny = match resource {
        GuardResource::UnknownLink(_) => true,
        GuardResource::Path(path) => {
            let resolution = resolve_resource(path, entity_store);
            match resolution.resource() {
                None => true,
                Some(resource_euid) => match CedarContext::new(entity_store.clone()) {
                    Err(error) => {
                        tracing::error!(
                            event = "authz_policy_engine_error",
                            error = %error,
                            "policy engine unavailable; treating as would-deny"
                        );
                        true
                    }
                    Ok(cedar_context) => match guard_euids(principal_type, principal_id, action) {
                        None => true,
                        Some((principal, action_euid)) => cedar_context
                            .is_authorized(
                                &principal,
                                &action_euid,
                                resource_euid,
                                Context::empty(),
                            )
                            .is_err(),
                    },
                },
            }
        }
    };

    if enforcement.records_would_deny() && would_deny {
        tracing::warn!(
            event = "authz_would_deny",
            principal = %principal_id,
            principal_type = %principal_type,
            action = %action,
            resource = %resource.label(),
            "would-deny recorded: this request would be denied under enforce"
        );
    }

    match decide(enforcement, would_deny, entity_store.is_empty()) {
        EnforcementDecision::Allow => Ok(()),
        EnforcementDecision::Deny => Err(authorization_denied(
            principal_id,
            action,
            MegaError::Other("not authorized for this operation".to_owned()),
        )),
    }
}

/// Decision for a request that arrives before any snapshot exists.
///
/// Nothing can be authorized, so this is a would-deny: recorded under `shadow`
/// with the same event and fields as every other one, denied under `enforce`.
fn decide_without_snapshot(
    enforcement: Enforcement,
    principal_type: &str,
    principal_id: &str,
    action: &str,
    resource: &GuardResource,
) -> Result<(), ApiError> {
    if enforcement.records_would_deny() {
        tracing::warn!(
            event = "authz_would_deny",
            principal = %principal_id,
            principal_type = %principal_type,
            action = %action,
            resource = %resource.label(),
            reason = "store_not_built",
            "would-deny recorded: this request would be denied under enforce"
        );
    }

    match decide(enforcement, true, true) {
        EnforcementDecision::Allow => Ok(()),
        EnforcementDecision::Deny => Err(authorization_denied(
            principal_id,
            action,
            MegaError::Other("authorization store not built".to_owned()),
        )),
    }
}

/// Build the Cedar principal/action ids, or `None` when either is not a valid
/// entity id (which the caller treats as would-deny rather than as an allow).
fn guard_euids(
    principal_type: &str,
    principal_id: &str,
    action: &str,
) -> Option<(SaturnEUid, SaturnEUid)> {
    let principal_type = EntityTypeName::from_str(principal_type).ok()?;
    let principal_id = EntityId::from_str(principal_id).ok()?;
    let action_type = EntityTypeName::from_str("Action").ok()?;
    let action_id = EntityId::from_str(action).ok()?;
    Some((
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            principal_type,
            principal_id,
        )),
        SaturnEUid::from(EntityUid::from_type_name_and_id(action_type, action_id)),
    ))
}

/// An authorization denial is **403, not 401** (UN-23).
///
/// The two mean different things to a client: 401 says "authenticate", 403 says
/// "you are known and still may not do this". The guard has already resolved a
/// principal by the time it evaluates policy, so 401 would be a lie — and the
/// protected operations declare 403 in their OpenAPI responses.
fn authorization_denied(principal_id: &str, action: &str, error: MegaError) -> ApiError {
    tracing::debug!("Authorization failed for {}: {}", principal_id, action);
    ApiError::with_status(
        StatusCode::FORBIDDEN,
        MegaError::Other(format!("Guard Authorization failed: {}", error)),
    )
}

/// The guard's authorization principal (UN-22).
///
/// Bot identity, decided by the caller, takes precedence — its authorization
/// semantics are UN-27's axis. Otherwise the subject comes from the
/// request-scoped resolution, so the guard and the downstream handler
/// extractors always agree: they used to resolve independently, and could
/// observe different subjects for one request if the session expired in
/// between. An anonymous request is a principal, not a rejection — whether it
/// is allowed is the policy's decision, not this function's.
pub(crate) async fn guard_principal<S>(
    parts: &mut http::request::Parts,
    state: &S,
    bot: Option<String>,
) -> (String, String)
where
    crate::api::oauth::api_store::BrowserSessionStore: FromRef<S>,
    S: Send + Sync,
{
    if let Some(bot_id) = bot {
        return ("Bot".to_string(), bot_id);
    }

    let OptionalSessionUser(user) = OptionalSessionUser::from_request_parts(parts, state)
        .await
        .unwrap_or(OptionalSessionUser(None));
    match user {
        Some(user) => ("User".to_string(), user.username),
        None => ("User".to_string(), ANONYMOUS_PRINCIPAL_ID.to_string()),
    }
}

pub async fn cedar_guard(
    State(state): State<MonoApiServiceState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let request_path = req.uri().path().to_owned();
    tracing::debug!("Processing request: {}", request_path);

    let request_method = req.method().as_str().to_owned();
    let (action, link) = resolve_cl_action(&request_method, &request_path).map_err(|e| {
        tracing::error!("Failed to resolve CL action: {}", e);
        ApiError::with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            MegaError::Other("Failed to resolve CL action".to_string()),
        )
    })?;
    tracing::debug!("Resolved action: {:?}, link: {}", action, link);

    // Skip authorization for unprotected requests
    if action.eq(&ActionEnum::UnprotectedRequest) {
        tracing::debug!("Unprotected request for path: {}", request_path);
        return Ok(next.run(req).await);
    }

    let enforcement =
        Enforcement::parse(&state.storage.config().cedar.enforcement).ok_or_else(|| {
            ApiError::with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                MegaError::Other("invalid cedar.enforcement".to_owned()),
            )
        })?;

    // `off`: no build, no consume — behavior identical to before (ADR-UN-01).
    if !enforcement.builds() {
        return Ok(next.run(req).await);
    }

    let (mut parts, body) = req.into_parts();

    let bot = BotIdentity::from_request_parts(&mut parts, &state)
        .await
        .ok()
        .map(|bot| bot.bot.id.to_string());
    let (principal_type, principal_id) = guard_principal(&mut parts, &state, bot).await;

    let resource = resolve_guard_resource(&state, &link).await;

    // A store that was never built cannot authorize anything. It takes the
    // same path as any other would-deny — one event shape, one set of fields —
    // so shadow-mode log processing does not need a special case.
    match state.entity_store.snapshot() {
        Some(snapshot) => decide_guard(
            enforcement,
            &snapshot,
            &principal_type,
            &principal_id,
            &action.to_string(),
            &resource,
        )?,
        None => decide_without_snapshot(
            enforcement,
            &principal_type,
            &principal_id,
            &action.to_string(),
            &resource,
        )?,
    }

    let req = Request::from_parts(parts, body);
    let response = next.run(req).await;

    if response.status().is_client_error() {
        tracing::error!(
            status = %response.status(),
            path = %request_path,
            "Downstream returned a 4xx error"
        );
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http::Request;

    use super::*;
    use crate::{
        api::oauth::{
            ResolvedSessionPrincipal,
            api_store::{BrowserSessionStore, CountingSessionStore},
            model::LoginUser,
        },
        common::errors::MegaError,
    };

    fn login_user(name: &str) -> LoginUser {
        LoginUser {
            username: name.to_string(),
            ..Default::default()
        }
    }

    fn counting_state(
        answers: Vec<Result<Option<LoginUser>, MegaError>>,
    ) -> (BrowserSessionStore, CountingSessionStore) {
        let store = CountingSessionStore::new(answers);
        (BrowserSessionStore::Counting(store.clone()), store)
    }

    fn parts() -> http::request::Parts {
        Request::builder()
            .uri("/cl/ABC123/merge")
            .body(Body::empty())
            .unwrap()
            .into_parts()
            .0
    }

    #[tokio::test]
    async fn un22_guard_principal_reads_the_session_subject_once() {
        // The store answers with a different user on a second call, so reading
        // the cached resolution is the only way to get a stable answer.
        let (state, counter) = counting_state(vec![
            Ok(Some(login_user("session-user"))),
            Ok(Some(login_user("drifted-user"))),
        ]);
        let mut parts = parts();

        let first = guard_principal(&mut parts, &state, None).await;
        assert_eq!(
            first,
            ("User".to_string(), "session-user".to_string()),
            "the guard's principal is the session subject"
        );
        assert_eq!(counter.call_count(), 1);

        // A later consumer on the same request sees the guard's answer.
        let second = guard_principal(&mut parts, &state, None).await;
        assert_eq!(second, first, "the resolution is reused, not repeated");
        assert_eq!(
            counter.call_count(),
            1,
            "the session store must not be consulted twice for one request"
        );
    }

    #[tokio::test]
    async fn un22_guard_principal_reuses_an_already_resolved_extension() {
        // Whoever resolved first wins; the guard must not go behind its back.
        let (state, counter) = counting_state(vec![Ok(Some(login_user("store-user")))]);
        let mut parts = parts();
        parts
            .extensions
            .insert(ResolvedSessionPrincipal(Some(login_user(
                "resolved-earlier",
            ))));

        let principal = guard_principal(&mut parts, &state, None).await;
        assert_eq!(
            principal,
            ("User".to_string(), "resolved-earlier".to_string())
        );
        assert_eq!(
            counter.call_count(),
            0,
            "an already resolved request must not hit the session store at all"
        );
    }

    #[tokio::test]
    async fn un22_guard_principal_is_anonymous_without_a_session() {
        let (state, counter) = counting_state(vec![Ok(None)]);
        let mut parts = parts();

        let principal = guard_principal(&mut parts, &state, None).await;
        assert_eq!(
            principal,
            ("User".to_string(), ANONYMOUS_PRINCIPAL_ID.to_string()),
            "an anonymous request is a principal, not a rejection"
        );
        assert_eq!(counter.call_count(), 1);
    }

    #[tokio::test]
    async fn un22_guard_principal_normalizes_a_session_store_failure_to_anonymous() {
        let (state, counter) = counting_state(vec![Err(MegaError::Other("store down".into()))]);
        let mut parts = parts();

        let principal = guard_principal(&mut parts, &state, None).await;
        assert_eq!(
            principal,
            ("User".to_string(), ANONYMOUS_PRINCIPAL_ID.to_string())
        );
        assert_eq!(counter.call_count(), 1);
    }

    // ---- UN-27: bot principals under the three states ----

    /// A bot is not a principal type the schema knows: every action's principal
    /// is `User` (`mega.cedarschema`). Evaluating a `Bot::"…"` principal
    /// therefore cannot produce an allow, and the guard treats it as would-deny
    /// rather than as an error to swallow. Bots using protected CL endpoints
    /// must move to a user token before `enforce` is switched on; the full bot
    /// authorization model (schema plus identity mapping) is deferred.
    #[test]
    fn un27_a_bot_principal_is_denied_under_enforce() {
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "Bot",
                "42",
                "viewRepo",
                &root_resource(),
            )
            .is_err(),
            "the schema has no Bot principal, so enforce must refuse rather than \
             silently allow"
        );
    }

    #[test]
    fn un27_a_bot_principal_still_passes_under_shadow() {
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Shadow,
                &snapshot,
                "Bot",
                "42",
                "viewRepo",
                &root_resource(),
            )
            .is_ok(),
            "shadow records the would-deny but must not change what runs — which \
             is what gives bot owners a window to migrate"
        );
    }

    // On the shadow would-deny record for bots: the emitting callsite is
    // `decide_guard`'s, already asserted by UN-08's shadow-log gate and its
    // structured-field script. Capturing it a second time from here proved
    // order-fragile — tracing caches a callsite's interest the first time it is
    // evaluated, so whichever test in this binary installs a subscriber first
    // decides whether later ones can observe it. A test that passes alone and
    // fails in a full run is worse than the division of labor.

    #[test]
    fn un27_a_bot_principal_is_unaffected_under_off() {
        // `off` never reaches evaluation at all; asserted here so the
        // three-state story for bots is complete in one place.
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Off,
                &snapshot,
                "Bot",
                "42",
                "viewRepo",
                &root_resource(),
            )
            .is_ok()
        );
    }

    /// Even an ACL that names a user `42` must not lend its permissions to
    /// `Bot::"42"` — the principal *type* is part of the identity.
    #[test]
    fn un27_a_bot_does_not_inherit_a_same_named_users_permissions() {
        let snapshot = snapshot_with_admin("42");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "42",
                "deleteRepo",
                &root_resource(),
            )
            .is_ok(),
            "the user named 42 is an admin"
        );
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "Bot",
                "42",
                "deleteRepo",
                &root_resource(),
            )
            .is_err(),
            "the bot with id 42 is not that user"
        );
    }

    #[tokio::test]
    async fn un22_guard_principal_keeps_bot_precedence_without_a_session_lookup() {
        let (state, counter) = counting_state(vec![Ok(Some(login_user("session-user")))]);
        let mut parts = parts();

        let principal = guard_principal(&mut parts, &state, Some("42".to_string())).await;
        assert_eq!(
            principal,
            ("Bot".to_string(), "42".to_string()),
            "bot identity keeps its existing precedence"
        );
        assert_eq!(
            counter.call_count(),
            0,
            "a bot request must not resolve a browser session"
        );
    }

    /// The fixed `{method, path, action}` matrix (UN-23). It is the card's
    /// deliverable: every registered `/cl` operation appears exactly once, so a
    /// route added or renamed without updating the mapping shows up here.
    const UN23_MATRIX: &[(&str, &str, ActionEnum)] = &[
        ("POST", "/cl/list", ActionEnum::UnprotectedRequest),
        ("POST", "/cl/labels", ActionEnum::EditMergeRequest),
        ("POST", "/cl/assignees", ActionEnum::EditMergeRequest),
        ("POST", "/cl/ABC123/reopen", ActionEnum::EditMergeRequest),
        ("POST", "/cl/ABC123/close", ActionEnum::EditMergeRequest),
        ("POST", "/cl/ABC123/merge", ActionEnum::ApproveMergeRequest),
        // UN-24 brought this entry point under authorization; it used to be
        // unmapped and hardcoded a "system" actor.
        (
            "POST",
            "/cl/ABC123/merge-no-auth",
            ActionEnum::ApproveMergeRequest,
        ),
        ("POST", "/cl/ABC123/comment", ActionEnum::EditMergeRequest),
        ("POST", "/cl/ABC123/title", ActionEnum::EditMergeRequest),
        ("POST", "/cl/ABC123/status", ActionEnum::EditMergeRequest),
        (
            "POST",
            "/cl/ABC123/update-branch",
            ActionEnum::EditMergeRequest,
        ),
        ("POST", "/cl/ABC123/files-changed", ActionEnum::ViewRepo),
        ("POST", "/cl/ABC123/reviewers", ActionEnum::EditMergeRequest),
        (
            "POST",
            "/cl/ABC123/reviewer/approve",
            ActionEnum::ApproveMergeRequest,
        ),
        (
            "POST",
            "/cl/ABC123/review/resolve",
            ActionEnum::EditMergeRequest,
        ),
        ("GET", "/cl/ABC123/detail", ActionEnum::ViewRepo),
        // MC-07: the CL commits listing endpoint (GET /cl/{link}/commits).
        ("GET", "/cl/ABC123/commits", ActionEnum::ViewRepo),
        ("GET", "/cl/ABC123/mui-tree", ActionEnum::ViewRepo),
        ("GET", "/cl/ABC123/files-list", ActionEnum::ViewRepo),
        ("GET", "/cl/ABC123/merge-box", ActionEnum::ViewRepo),
        ("GET", "/cl/ABC123/update-status", ActionEnum::ViewRepo),
        // Semantic fix (UN-23): reading the reviewer list is a read, while
        // POST/DELETE on the same path change it. Keying on the path alone
        // could only ever get one of the three right.
        ("GET", "/cl/ABC123/reviewers", ActionEnum::ViewRepo),
        (
            "DELETE",
            "/cl/ABC123/reviewers",
            ActionEnum::EditMergeRequest,
        ),
    ];

    #[test]
    fn un23_method_path_matrix_resolves_every_registered_operation() {
        for (method, path, expected) in UN23_MATRIX {
            let (action, _link) = resolve_cl_action(method, path).expect("resolver must not error");
            assert_eq!(
                action, *expected,
                "{method} {path} must resolve to {expected:?}"
            );
        }
    }

    #[test]
    fn un23_the_same_path_resolves_differently_per_method() {
        let read = resolve_cl_action("GET", "/cl/ABC123/reviewers").unwrap().0;
        let write = resolve_cl_action("POST", "/cl/ABC123/reviewers").unwrap().0;
        let remove = resolve_cl_action("DELETE", "/cl/ABC123/reviewers")
            .unwrap()
            .0;

        assert_eq!(read, ActionEnum::ViewRepo);
        assert_eq!(write, ActionEnum::EditMergeRequest);
        assert_eq!(remove, ActionEnum::EditMergeRequest);
        assert_ne!(
            read, write,
            "reading and changing the reviewer list are different actions"
        );
    }

    #[test]
    fn un23_an_unmapped_method_does_not_inherit_another_methods_action() {
        // `/{link}/merge` is only registered for POST. A PUT to the same path
        // must not pick up `approveMergeRequest` from the POST entry.
        let (action, _) = resolve_cl_action("PUT", "/cl/ABC123/merge").unwrap();
        assert_eq!(action, ActionEnum::UnprotectedRequest);
    }

    #[test]
    fn un23_the_method_key_is_case_insensitive() {
        let lower = resolve_cl_action("post", "/cl/ABC123/merge").unwrap().0;
        let upper = resolve_cl_action("POST", "/cl/ABC123/merge").unwrap().0;
        assert_eq!(lower, ActionEnum::ApproveMergeRequest);
        assert_eq!(lower, upper);
    }

    #[test]
    fn un23_the_cl_link_is_extracted_alongside_the_action() {
        let (_, link) = resolve_cl_action("POST", "/cl/ABC123/merge").unwrap();
        assert_eq!(link, "ABC123");
        let (_, link) = resolve_cl_action("POST", "/cl/ABC123/reviewer/approve").unwrap();
        assert_eq!(link, "ABC123");
    }

    /// An authorization denial must be 403, matching what the protected
    /// operations declare in OpenAPI. 401 would tell a known principal to log
    /// in again, which is not the problem.
    #[tokio::test]
    async fn un23_an_authorization_denial_is_403_not_401() {
        use axum::response::IntoResponse;

        let response = authorization_denied(
            "someone",
            "approveMergeRequest",
            MegaError::Other("denied".to_string()),
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "401 is for authentication failures, not authorization denials"
        );
    }

    /// The guard sits on the router nested under `/api/v1`, so this is the
    /// shape a real request actually has. Without stripping the prefix the
    /// resolver matched nothing and every endpoint fell through unprotected.
    #[test]
    fn un08_the_api_prefix_is_stripped_before_matching() {
        let (action, link) = resolve_cl_action("GET", "/api/v1/cl/ABC123/detail").unwrap();
        assert_eq!(action, ActionEnum::ViewRepo);
        assert_eq!(link, "ABC123");

        let (action, _) = resolve_cl_action("POST", "/api/v1/cl/ABC123/merge").unwrap();
        assert_eq!(action, ActionEnum::ApproveMergeRequest);

        // Non-CL paths under the same prefix stay unprotected.
        let (action, _) = resolve_cl_action("POST", "/api/v1/merge-queue/add").unwrap();
        assert_eq!(action, ActionEnum::UnprotectedRequest);
    }

    #[test]
    fn un23_non_cl_paths_stay_unprotected() {
        let (action, _) = resolve_cl_action("POST", "/merge-queue/add").unwrap();
        assert_eq!(action, ActionEnum::UnprotectedRequest);
    }

    /// UN-24: `merge-no-auth` is under authorization now. Its name is a
    /// leftover from a local-testing shortcut — it no longer means the request
    /// skips authorization, only that it needs no *authenticated session*.
    #[test]
    fn un24_merge_no_auth_is_mapped_like_merge() {
        let (action, link) = resolve_cl_action("POST", "/cl/ABC123/merge-no-auth").unwrap();
        assert_eq!(action, ActionEnum::ApproveMergeRequest);
        assert_eq!(link, "ABC123");
        assert_eq!(
            action,
            resolve_cl_action("POST", "/cl/ABC123/merge").unwrap().0,
            "both merge entry points require the same action"
        );
    }

    // ---- UN-08: three-state guard evaluation ----

    fn snapshot_with_admin(admin: &str) -> EntitySnapshot {
        let json = crate::contract::policy::entitystore::generate_entity(&[admin.to_string()], "/")
            .expect("generate authz json");
        crate::contract::policy::builder::build_from_json(&json).expect("build snapshot")
    }

    fn empty_snapshot() -> EntitySnapshot {
        crate::contract::policy::builder::build_from_json(
            r#"{"users":{},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#,
        )
        .expect("build empty snapshot")
    }

    fn root_resource() -> GuardResource {
        GuardResource::Path("/".to_owned())
    }

    #[test]
    fn un08_shadow_allows_a_request_that_enforce_would_deny() {
        let snapshot = snapshot_with_admin("admin-user");
        // `outsider` is in no group, so `deleteRepo` is not permitted.
        let shadow = decide_guard(
            Enforcement::Shadow,
            &snapshot,
            "User",
            "outsider",
            "deleteRepo",
            &root_resource(),
        );
        assert!(shadow.is_ok(), "shadow never changes the allow decision");

        let enforced = decide_guard(
            Enforcement::Enforce,
            &snapshot,
            "User",
            "outsider",
            "deleteRepo",
            &root_resource(),
        );
        assert!(
            enforced.is_err(),
            "the same request must be denied under enforce"
        );
    }

    #[test]
    fn un08_enforce_allows_an_authorized_principal() {
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "admin-user",
                "deleteRepo",
                &root_resource(),
            )
            .is_ok(),
            "an admin keeps its admin-only action under enforce"
        );
    }

    #[tokio::test]
    async fn un08_an_enforced_denial_is_403() {
        use axum::response::IntoResponse;

        let snapshot = snapshot_with_admin("admin-user");
        let error = decide_guard(
            Enforcement::Enforce,
            &snapshot,
            "User",
            "outsider",
            "deleteRepo",
            &root_resource(),
        )
        .expect_err("must deny");
        assert_eq!(error.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn un08_an_unknown_cl_link_is_fail_closed() {
        let snapshot = snapshot_with_admin("admin-user");
        let unknown = GuardResource::UnknownLink("NOSUCHCL".to_owned());

        assert!(
            decide_guard(
                Enforcement::Shadow,
                &snapshot,
                "User",
                "admin-user",
                "viewRepo",
                &unknown,
            )
            .is_ok(),
            "shadow records but still allows"
        );
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "admin-user",
                "viewRepo",
                &unknown,
            )
            .is_err(),
            "an unresolvable resource must be denied under enforce, even for an admin"
        );
    }

    #[test]
    fn un08_an_empty_store_denies_under_enforce() {
        let snapshot = empty_snapshot();
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "admin-user",
                "viewRepo",
                &root_resource(),
            )
            .is_err(),
            "enforce + empty store = deny (fail-closed, ADR-UN-01)"
        );
        assert!(
            decide_guard(
                Enforcement::Shadow,
                &snapshot,
                "User",
                "admin-user",
                "viewRepo",
                &root_resource(),
            )
            .is_ok()
        );
    }

    #[test]
    fn un08_a_missing_root_repository_is_fail_closed() {
        // Users but no repository entity: nothing to authorize against.
        let json = serde_json::json!({
            "users": { "User::\"solo\"": { "euid": "User::\"solo\"", "parents": [] } },
            "repos": {},
            "user_groups": {},
            "merge_requests": {},
            "issues": {}
        })
        .to_string();
        let snapshot = crate::contract::policy::builder::build_from_json(&json)
            .expect("build snapshot without a root repo");

        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "solo",
                "viewRepo",
                &root_resource(),
            )
            .is_err(),
            "a missing root repository denies under enforce"
        );
    }

    #[test]
    fn un08_the_anonymous_principal_is_the_reserved_literal() {
        assert_eq!(
            ANONYMOUS_PRINCIPAL_ID, "__anonymous__",
            "the anonymous fallback must be a reserved literal, not a name a real \
             account could take (ADR-UN-06)"
        );

        // An ACL that grants `reader` admin must not thereby grant anonymous
        // requests anything.
        let snapshot = snapshot_with_admin("reader");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                ANONYMOUS_PRINCIPAL_ID,
                "deleteRepo",
                &root_resource(),
            )
            .is_err(),
            "the anonymous principal must not inherit an account's permissions"
        );
    }

    #[test]
    fn un08_a_principal_type_outside_the_schema_is_would_deny() {
        // `Bot` is not a principal type in the schema; UN-27 owns bot
        // authorization semantics. Until then it must fail closed, not open.
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "Bot",
                "42",
                "viewRepo",
                &root_resource(),
            )
            .is_err()
        );
    }

    #[test]
    fn un08_any_repository_path_normalizes_to_the_single_root() {
        // ADR-UN-05: a CL under /project inherits the root repository's ACL.
        let snapshot = snapshot_with_admin("admin-user");
        assert!(
            decide_guard(
                Enforcement::Enforce,
                &snapshot,
                "User",
                "admin-user",
                "deleteRepo",
                &GuardResource::Path("/project/sub".to_owned()),
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn test_match_operation() {
        let patterns: HashMap<String, String> = HashMap::from([
            (
                "/{link}/approve".to_string(),
                "approveMergeRequest".to_string(),
            ),
            ("/{link}/close".to_string(), "editMergeRequest".to_string()),
            (
                "/{link}/review/delete".to_string(),
                "editMergeRequest".to_string(),
            ),
        ]);

        let suffix = "/my-cl-link/approve";
        let result = match_operation(suffix, &patterns);
        assert_eq!(
            result,
            Some((ActionEnum::ApproveMergeRequest, "my-cl-link".to_string()))
        );

        let suffix = "/another-cl-link/close";
        let result = match_operation(suffix, &patterns);
        assert_eq!(
            result,
            Some((ActionEnum::EditMergeRequest, "another-cl-link".to_string()))
        );

        let suffix = "/no-match-link/delete";
        let result = match_operation(suffix, &patterns);
        assert_eq!(result, None);

        let suffix = "/path/subpath/review/delete";
        let result = match_operation(suffix, &patterns);
        assert_eq!(
            result,
            Some((ActionEnum::EditMergeRequest, "path/subpath".to_string()))
        );
    }
}
