use axum::{
    Json,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState,
        api_common::{self},
        api_doc::CL_TAG,
        oauth::{OptionalSessionUser, model::LoginUser},
    },
    callisto::sea_orm_active_enums::{ConvTypeEnum, MergeStatusEnum},
    ceres::model::{
        change_list::{
            CLDetailRes, ClCommitRes, ClFilesRes, Condition, FilesChangedPage, ListPayload,
            MergeBoxRes, MuiTreeNode, UpdateBranchStatusRes, UpdateClStatusPayload,
        },
        conversation::ContentPayload,
        issue::ItemRes,
    },
    common::errors::{ApiError, MegaError},
    contract::{
        api::common::{CommonPage, CommonResult, PageParams},
        policy::guard::cedar_guard::ANONYMOUS_PRINCIPAL_ID,
    },
    jupiter::service::{cl_service::CLService, webhook_service::WebhookEvent},
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/cl",
        OpenApiRouter::new()
            .routes(routes!(fetch_cl_list))
            .routes(routes!(cl_detail))
            .routes(routes!(merge))
            .routes(routes!(merge_box))
            .routes(routes!(merge_no_auth))
            .routes(routes!(close_cl))
            .routes(routes!(reopen_cl))
            .routes(routes!(cl_mui_tree))
            .routes(routes!(cl_files_changed_by_page))
            .routes(routes!(cl_files_list))
            .routes(routes!(save_comment))
            .routes(routes!(edit_title))
            .routes(routes!(update_cl_status))
            .routes(routes!(update_branch_status))
            .routes(routes!(update_branch))
            .routes(routes!(cl_commits))
            .routes(routes!(put_cl_ci_check))
            .routes(routes!(get_cl_ci_authz)),
    )
}

const CANONICAL_V2_VERSION: &str = "orion-mega-v2";
const MAX_CI_CALLBACK_BODY_BYTES: usize = 65_536;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CiCheckCallbackReq {
    pub commit_sha: String,
    pub head_revision: i64,
    pub task_id: String,
    pub attempt_id: String,
    pub ci_generation: i64,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub details_url: Option<String>,
    pub occurred_at: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CiCheckCallbackRes {
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub action: String,
    pub cl_link: String,
    pub head_revision: i64,
    pub ci_generation: i64,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CiCheckErrorRes {
    pub code: String,
    pub message: String,
}

fn ci_error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(CiCheckErrorRes {
            code: code.to_owned(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn extract_single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

pub(crate) fn compute_orion_ci_v2_signature(
    secret: &str,
    cl_link: &str,
    timestamp_ascii: &str,
    nonce: &str,
    body_sha256_hex: &str,
) -> String {
    let canonical = format!(
        "{CANONICAL_V2_VERSION}\nPUT\n/api/v1/cl/{cl_link}/checks/ci\n{timestamp_ascii}\n{nonce}\n\n{body_sha256_hex}"
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, canonical.as_bytes());
    format!("sha256={}", hex::encode(tag.as_ref()))
}

fn verify_orion_ci_v2_signature(
    secret: &str,
    cl_link: &str,
    timestamp_ascii: &str,
    nonce: &str,
    body_sha256_hex: &str,
    provided_header: &str,
) -> bool {
    let Some(provided_hex) = provided_header.strip_prefix("sha256=") else {
        return false;
    };
    if provided_hex.len() != 64
        || !provided_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Ok(provided_bytes) = hex::decode(provided_hex) else {
        return false;
    };
    let canonical = format!(
        "{CANONICAL_V2_VERSION}\nPUT\n/api/v1/cl/{cl_link}/checks/ci\n{timestamp_ascii}\n{nonce}\n\n{body_sha256_hex}"
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, canonical.as_bytes(), &provided_bytes).is_ok()
}

fn is_valid_commit_sha(sha: &str) -> bool {
    (sha.len() == 40 || sha.len() == 64)
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_valid_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn canonicalize_ci_status(raw: &str) -> Option<&'static str> {
    match raw {
        "pending" => Some("PENDING"),
        "running" => Some("RUNNING"),
        "passed" => Some("PASSED"),
        "failed" => Some("FAILED"),
        "cancelled" => Some("CANCELLED"),
        _ => None,
    }
}

/// Receive canonical V2 HMAC-signed CI check status callback from Orion2 (`CLB-13`).
#[utoipa::path(
    put,
    params(
        ("link", description = "CL link"),
    ),
    request_body = CiCheckCallbackReq,
    path = "/{link}/checks/ci",
    responses(
        (status = 200, body = CiCheckCallbackRes, content_type = "application/json"),
        (status = 400, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 401, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 404, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 409, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 503, body = CiCheckErrorRes, content_type = "application/json"),
    ),
    tag = CL_TAG
)]
async fn put_cl_ci_check(
    Path(link): Path<String>,
    State(state): State<MonoApiServiceState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ci_cfg = state.storage.config().ci_checks.clone();
    if !ci_cfg.enabled {
        return ci_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ci_checks_disabled",
            "CI check callbacks are disabled",
        );
    }
    let Some(secret) = ci_cfg.hmac_secret.as_ref().map(|s| s.expose_secret()) else {
        return ci_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ci_checks_disabled",
            "CI check callback HMAC secret is not configured",
        );
    };
    if secret.len() < crate::config::CI_CHECKS_MIN_HMAC_SECRET_BYTES {
        return ci_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ci_checks_disabled",
            "CI check callback HMAC secret is invalid",
        );
    }

    if body.len() > MAX_CI_CALLBACK_BODY_BYTES {
        return ci_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "CI callback payload exceeds maximum allowed size",
        );
    }
    if link.is_empty()
        || link.len() > 128
        || link.contains('/')
        || link.contains('\\')
        || link.contains("..")
        || link.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return ci_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid CL link",
        );
    }

    let Some(timestamp_str) = extract_single_header(&headers, "x-orion-timestamp") else {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "missing or duplicate X-Orion-Timestamp header",
        );
    };
    let Some(nonce) = extract_single_header(&headers, "x-orion-nonce") else {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "missing or duplicate X-Orion-Nonce header",
        );
    };
    let Some(signature_v2) = extract_single_header(&headers, "x-orion-signature-v2") else {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "missing or duplicate X-Orion-Signature-V2 header",
        );
    };
    if !is_valid_opaque_id(nonce) {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "invalid X-Orion-Nonce header",
        );
    }
    let Ok(timestamp) = timestamp_str.parse::<i64>() else {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "invalid X-Orion-Timestamp header",
        );
    };
    let now_utc = chrono::Utc::now();
    if now_utc.timestamp().abs_diff(timestamp) > ci_cfg.max_skew_secs {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "X-Orion-Timestamp outside allowed skew window",
        );
    }

    let body_sha256 = hex::encode(Sha256::digest(&body));
    if !verify_orion_ci_v2_signature(
        secret,
        &link,
        timestamp_str,
        nonce,
        &body_sha256,
        signature_v2,
    ) {
        return ci_error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "canonical V2 HMAC signature verification failed",
        );
    }

    let now_naive = now_utc.naive_utc();
    let parse_and_validate =
        || -> Result<crate::jupiter::storage::cl_storage::CiCheckCallbackValidated, String> {
            let req: CiCheckCallbackReq = serde_json::from_slice(&body)
                .map_err(|err| format!("invalid CI callback JSON payload: {err}"))?;

            if !is_valid_commit_sha(&req.commit_sha) {
                return Err("commit_sha must be 40 or 64 lowercase hex characters".to_owned());
            }
            if req.head_revision < 1 {
                return Err("head_revision must be >= 1".to_owned());
            }
            if !is_valid_opaque_id(&req.task_id) || !is_valid_opaque_id(&req.attempt_id) {
                return Err(
                    "task_id and attempt_id must be non-empty identifiers (<= 128 bytes)"
                        .to_owned(),
                );
            }
            if req.ci_generation <= 0 {
                return Err("ci_generation must be > 0".to_owned());
            }
            if chrono::DateTime::parse_from_rfc3339(&req.occurred_at).is_err() {
                return Err("occurred_at must be a valid RFC 3339 timestamp".to_owned());
            }
            let status_upper = canonicalize_ci_status(&req.status).ok_or_else(|| {
                "status must be one of: pending, running, passed, failed, cancelled".to_owned()
            })?;
            if req.summary.len() > 4096 {
                return Err("summary exceeds 4096 bytes".to_owned());
            }
            if let Some(ref details_url) = req.details_url {
                let valid_url = details_url.len() <= 2048
                    && url::Url::parse(details_url)
                        .ok()
                        .is_some_and(|u| matches!(u.scheme(), "http" | "https"));
                if !valid_url {
                    return Err(
                        "details_url must be a valid http or https URL (<= 2048 bytes)".to_owned(),
                    );
                }
            }
            Ok(
                crate::jupiter::storage::cl_storage::CiCheckCallbackValidated {
                    commit_sha: req.commit_sha,
                    head_revision: req.head_revision,
                    task_id: req.task_id,
                    attempt_id: req.attempt_id,
                    ci_generation: req.ci_generation,
                    status_upper: status_upper.to_owned(),
                    summary: req.summary,
                    details_url: req.details_url,
                },
            )
        };

    match state
        .cl_stg()
        .apply_ci_check_callback(
            crate::jupiter::storage::cl_storage::CiCheckCallbackInput {
                cl_link: link,
                nonce: nonce.to_owned(),
                body_sha256,
                nonce_retention_secs: ci_cfg.nonce_retention_secs,
                now: now_naive,
            },
            parse_and_validate,
        )
        .await
    {
        Ok(outcome) => {
            let applied = outcome.action == "updated";
            let reason = if applied {
                None
            } else {
                Some(outcome.action.to_owned())
            };
            (
                StatusCode::OK,
                Json(CiCheckCallbackRes {
                    applied,
                    reason,
                    action: outcome.action.to_owned(),
                    cl_link: outcome.cl_link,
                    head_revision: outcome.head_revision,
                    ci_generation: outcome.ci_generation,
                    status: outcome.status,
                }),
            )
                .into_response()
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::InvalidRequest(msg)) => {
            ci_error_response(StatusCode::BAD_REQUEST, "invalid_request", msg)
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::NonceConflict) => {
            ci_error_response(
                StatusCode::CONFLICT,
                "nonce_conflict",
                "X-Orion-Nonce already used for a different path or payload",
            )
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::ClNotFound) => {
            ci_error_response(StatusCode::NOT_FOUND, "cl_not_found", "CL not found")
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::ClNotOpen) => {
            ci_error_response(StatusCode::CONFLICT, "cl_not_open", "CL is not open")
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::StaleHeadRevision) => {
            ci_error_response(
                StatusCode::CONFLICT,
                "stale_head_revision",
                "callback head_revision does not match current CL head_revision",
            )
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::StaleCommit) => {
            ci_error_response(
                StatusCode::CONFLICT,
                "stale_commit",
                "callback commit_sha does not match current CL head commit",
            )
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::AlreadyTerminal) => {
            ci_error_response(
                StatusCode::CONFLICT,
                "already_terminal",
                "CI check is already in a different terminal state for this generation",
            )
        }
        Err(crate::jupiter::storage::cl_storage::CiCheckCallbackError::Internal(err)) => {
            ci_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("failed to persist CI check callback: {err}"),
            )
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CiAuthzRes {
    pub allowed: bool,
    pub action: String,
    pub cl_link: String,
    pub principal: String,
}

fn parse_ci_authz_action(raw_query: Option<&str>) -> Result<&'static str, &'static str> {
    let raw = raw_query.ok_or("missing required query parameter 'action'")?;
    let mut action: Option<String> = None;
    for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
        if k != "action" {
            return Err("unexpected query parameter; only 'action=read|write' is allowed");
        }
        if action.is_some() {
            return Err("duplicate 'action' query parameter");
        }
        action = Some(v.into_owned());
    }
    match action.as_deref() {
        Some("read") => Ok("read"),
        Some("write") => Ok("write"),
        Some(_) => Err("invalid 'action' query parameter; expected 'read' or 'write'"),
        None => Err("missing required query parameter 'action'"),
    }
}

fn with_no_store_cache_control(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    resp
}

/// Fail-closed enforced Cedar authorization decision endpoint for CL CI read/write operations
/// (`CLB-14`, `ADR-CLB-05`).
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
        ("action" = String, Query, description = "CI operation scope: 'read' (viewRepo) or 'write' (editMergeRequest)"),
    ),
    path = "/{link}/ci-authz",
    responses(
        (status = 200, body = CiAuthzRes, content_type = "application/json"),
        (status = 400, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 401, description = "Login required"),
        (status = 403, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 404, body = CiCheckErrorRes, content_type = "application/json"),
        (status = 503, body = CiCheckErrorRes, content_type = "application/json"),
    ),
    tag = CL_TAG
)]
async fn get_cl_ci_authz(
    user: LoginUser,
    Path(link): Path<String>,
    RawQuery(raw_query): RawQuery,
    State(state): State<MonoApiServiceState>,
) -> Response {
    let resp = evaluate_cl_ci_authz(user, link, raw_query, state).await;
    with_no_store_cache_control(resp)
}

async fn evaluate_cl_ci_authz(
    user: LoginUser,
    link: String,
    raw_query: Option<String>,
    state: MonoApiServiceState,
) -> Response {
    let action = match parse_ci_authz_action(raw_query.as_deref()) {
        Ok(a) => a,
        Err(msg) => return ci_error_response(StatusCode::BAD_REQUEST, "invalid_request", msg),
    };

    let enforcement = crate::contract::policy::enforcement::Enforcement::parse(
        &state.storage.config().cedar.enforcement,
    );
    if enforcement != Some(crate::contract::policy::enforcement::Enforcement::Enforce) {
        return ci_error_response(
            StatusCode::FORBIDDEN,
            "ci_authz_policy_not_enforced",
            "Cedar authorization enforcement must be set to 'enforce' for CI operations",
        );
    }

    let Some(snapshot) = state.entity_store.snapshot() else {
        return ci_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ci_authz_unavailable",
            "Cedar authorization entity store snapshot is unavailable",
        );
    };

    let cl = match state.cl_stg().get_cl(&link).await {
        Ok(Some(cl)) => cl,
        Ok(None) => {
            return ci_error_response(StatusCode::NOT_FOUND, "cl_not_found", "CL not found");
        }
        Err(err) => {
            tracing::error!(
                cl_link = %link,
                error = %err,
                "failed to load CL for CI authorization"
            );
            return ci_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to load CL for authorization",
            );
        }
    };

    let cedar_action = match action {
        "read" => "viewRepo",
        "write" => "editMergeRequest",
        _ => unreachable!("parse_ci_authz_action restricts to read|write"),
    };

    match crate::contract::policy::guard::cedar_guard::decide_guard(
        crate::contract::policy::enforcement::Enforcement::Enforce,
        &snapshot,
        "User",
        &user.username,
        cedar_action,
        &crate::contract::policy::guard::cedar_guard::GuardResource::Path(cl.path),
    ) {
        Ok(()) => (
            StatusCode::OK,
            Json(CiAuthzRes {
                allowed: true,
                action: action.to_owned(),
                cl_link: link,
                principal: user.username,
            }),
        )
            .into_response(),
        Err(_) => ci_error_response(
            StatusCode::FORBIDDEN,
            "ci_authz_denied",
            "authorization denied for this CL CI operation",
        ),
    }
}

/// List a Change List's commits (DEP-01 frozen contract; plan-20260827
/// MC-05). Chain order, commit metadata only.
///
/// Authorization is mapped by the Cedar guard in MC-07 and ships atomically
/// with it (REL-MC-01 / MC-08). The mandatory session extractor mirrors the
/// sibling `cl_detail` (also `viewRepo`): an anonymous request is a 401 from
/// the extractor (UN-23), while a known-but-unauthorized principal is the
/// guard's 403 under enforce.
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/commits",
    responses(
        (status = 200, body = CommonResult<Vec<ClCommitRes>>, content_type = "application/json"),
        (status = 401, description = "Login required"),
        (status = 404, description = "Change List not found"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn cl_commits(
    _user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<ClCommitRes>>>, ApiError> {
    // DEP-01 ④: unknown link → 404. The read function answers an unknown link
    // with an empty listing too, so existence is established first (the CL row
    // is the authority) instead of returning a bare empty list.
    state
        .cl_stg()
        .get_cl(&link)
        .await?
        .ok_or_else(|| MegaError::NotFound(format!("CL {link} not found")))?;
    let rows = state.cl_stg().get_cl_commits(&link).await?;
    let res: Vec<ClCommitRes> = rows.into_iter().map(Into::into).collect();
    Ok(Json(CommonResult::success(Some(res))))
}

/// Reopen Change List
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/reopen",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn reopen_cl(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.cl_stg().get_cl(&link).await?;
    let model = res.ok_or(MegaError::Other("Not Found".to_string()))?;

    if model.status == MergeStatusEnum::Closed {
        let link = model.link.clone();
        state.cl_stg().reopen_cl(model.clone()).await?;
        state
            .conv_stg()
            .add_conversation(
                &link,
                &user.username,
                Some(format!("{} reopen this", user.username)),
                ConvTypeEnum::Reopen,
            )
            .await
            .unwrap();
        let updated_model = state
            .cl_stg()
            .get_cl(&link)
            .await?
            .ok_or(MegaError::Other("Not Found".to_string()))?;
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClReopened, &updated_model);
    }
    Ok(Json(CommonResult::success(None)))
}

/// Close Change List
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/close",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn close_cl(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.cl_stg().get_cl(&link).await?;
    let model = res.ok_or(MegaError::Other("Not Found".to_string()))?;

    if matches!(model.status, MergeStatusEnum::Open | MergeStatusEnum::Draft) {
        let link = model.link.clone();
        state.cl_stg().close_cl(model.clone()).await?;
        state
            .conv_stg()
            .add_conversation(
                &link,
                &user.username,
                Some(format!("{} closed this", user.username)),
                ConvTypeEnum::Closed,
            )
            .await?;
        let updated_model = state
            .cl_stg()
            .get_cl(&link)
            .await?
            .ok_or(MegaError::Other("Not Found".to_string()))?;
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClClosed, &updated_model);
    }
    Ok(Json(CommonResult::success(None)))
}

/// Approve Change List
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/merge",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
        (status = 503, description = "Authorization is unavailable; the merge is not decidable right now and may be retried"),
    ),
    tag = CL_TAG
)]
async fn merge(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.cl_stg().get_cl(&link).await?;
    let model = res.ok_or(MegaError::Other("Not Found".to_string()))?;

    if model.status == MergeStatusEnum::Draft {
        return Err(ApiError::from(MegaError::Other(
            "CL is not ready for review".to_owned(),
        )));
    }

    if model.status == MergeStatusEnum::Open {
        state
            .monorepo()
            // A normal merge is authorized as, and executed by, the same
            // logged-in user (ADR-UN-06 ④).
            .merge_cl(&user.username, &user.username, model.clone())
            .await?;
        let updated_model = state
            .cl_stg()
            .get_cl(&link)
            .await?
            .ok_or(MegaError::Other("Not Found".to_string()))?;
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClMerged, &updated_model);

        // Best-effort CL-merged notification to the CL author (outbox; delivered
        // by the background dispatcher). A failure must not fail the merge
        // request. See docs/notification.md phase 0.
        let notif_stg = state.storage.notification_storage();
        let cl_stg = state.cl_stg();
        if let Err(e) =
            crate::notification::triggers::on_cl_merged(&notif_stg, &cl_stg, &user.username, &link)
                .await
        {
            tracing::warn!(
                error = %e,
                cl_link = %link,
                "failed to enqueue CL merged notification"
            );
        }
    }
    Ok(Json(CommonResult::success(None)))
}

/// Merge a Change List without an authenticated session.
///
/// "No auth" here means no *authentication* is required, not that authorization
/// is skipped (UN-24): an anonymous caller is authorized as the reserved
/// anonymous principal, which under `enforce` the guard rejects with 403.
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/merge-no-auth",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
        (status = 503, description = "Authorization is unavailable; the merge is not decidable right now and may be retried"),
    ),
    tag = CL_TAG
)]
async fn merge_no_auth(
    Path(link): Path<String>,
    // UN-24: the subject is optional, not absent. `OptionalSessionUser` never
    // rejects, so an anonymous call still reaches the handler — and is then
    // authorized as the reserved anonymous principal, which under `enforce`
    // the guard has already turned into a 403.
    OptionalSessionUser(requester): OptionalSessionUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.cl_stg().get_cl(&link).await?;
    let model = res.ok_or(MegaError::Other("CL Not Found".to_string()))?;

    if model.status != MergeStatusEnum::Open {
        return Err(ApiError::from(MegaError::Other(format!(
            "CL is not in Open status, current status: {:?}",
            model.status
        ))));
    }

    // No *authentication* required — which is not the same as no
    // authorization. An anonymous caller is authorized as the reserved
    // anonymous principal (never as a name a real account could hold), and the
    // merge is recorded as executed by `system`.
    let authz_principal = requester
        .as_ref()
        .map(|user| user.username.as_str())
        .unwrap_or(ANONYMOUS_PRINCIPAL_ID);
    let execution_actor = requester
        .as_ref()
        .map(|user| user.username.as_str())
        .unwrap_or("system");
    state
        .monorepo()
        .merge_cl(authz_principal, execution_actor, model.clone())
        .await?;
    let updated_model = state
        .cl_stg()
        .get_cl(&link)
        .await?
        .ok_or(MegaError::Other("CL Not Found".to_string()))?;
    state
        .webhook_svc()
        .dispatch(WebhookEvent::ClMerged, &updated_model);

    Ok(Json(CommonResult::success(Some(
        "Merge completed successfully".to_string(),
    ))))
}

/// Fetch CL list
#[utoipa::path(
    post,
    path = "/list",
    request_body = PageParams<ListPayload>,
    responses(
        (status = 200, body = CommonResult<CommonPage<ItemRes>>, content_type = "application/json")
    ),
    tag = CL_TAG
)]
async fn fetch_cl_list(
    state: State<MonoApiServiceState>,
    Json(json): Json<PageParams<ListPayload>>,
) -> Result<Json<CommonResult<CommonPage<ItemRes>>>, ApiError> {
    let (items, total) = state
        .cl_stg()
        .get_cl_list(json.additional.into(), json.pagination)
        .await?;
    let items: Vec<ItemRes> = items.into_iter().map(|m| m.into()).collect();

    let res = CommonPage { items, total };
    Ok(Json(CommonResult::success(Some(res))))
}

/// Get change list details
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/detail",
    responses(
        (status = 200, body = CommonResult<CLDetailRes>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn cl_detail(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<CLDetailRes>>, ApiError> {
    let cl_service: CLService = state.storage.cl_service.clone();
    let cl_details: CLDetailRes = cl_service
        .get_cl_details(&link, user.username)
        .await?
        .into();
    Ok(Json(CommonResult::success(Some(cl_details))))
}

#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/mui-tree",
    responses(
        (status = 200, body = CommonResult<Vec<MuiTreeNode>>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn cl_mui_tree(
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<MuiTreeNode>>>, ApiError> {
    let files = state
        .monorepo()
        .get_sorted_changed_file_list(&link, None)
        .await?;
    let mui_trees = build_forest(files);
    Ok(Json(CommonResult::success(Some(mui_trees))))
}

/// Get Change List file changed list in Pagination
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/files-changed",
    request_body = PageParams<String>,
    responses(
        (status = 200, body = CommonResult<FilesChangedPage>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn cl_files_changed_by_page(
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
    Json(json): Json<PageParams<String>>,
) -> Result<Json<CommonResult<FilesChangedPage>>, ApiError> {
    let (items, total) = state
        .monorepo()
        .paged_content_diff_for_cl(&link, json.pagination)
        .await?;
    let res = CommonResult::success(Some(FilesChangedPage {
        page: CommonPage { total, items },
    }));
    Ok(Json(res))
}

/// Get Change List file list
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/files-list",
    responses(
        (status = 200, body = CommonResult<Vec<ClFilesRes>>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn cl_files_list(
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<ClFilesRes>>>, ApiError> {
    let cl = state
        .cl_stg()
        .get_cl(&link)
        .await?
        .ok_or(MegaError::Other("CL Not Found".to_string()))?;

    let stg = state.monorepo();
    let old_files = stg.get_commit_blobs(&cl.from_hash).await?;
    let new_files = stg.get_commit_blobs(&cl.to_hash).await?;
    let cl_diff_files = stg.cl_files_list(old_files, new_files.clone()).await?; // TODO

    let res = cl_diff_files
        .into_iter()
        .map(|m| {
            let item: ClFilesRes = m.into();
            item
        })
        .collect::<Vec<ClFilesRes>>();
    Ok(Json(CommonResult::success(Some(res))))
}

/// Get Update Branch status
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/update-status",
    responses(
        (status = 200, body = CommonResult<UpdateBranchStatusRes>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn update_branch_status(
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<UpdateBranchStatusRes>>, ApiError> {
    let res = state.monorepo().update_branch_status(&link).await?;
    Ok(Json(CommonResult::success(Some(res))))
}

/// Update Branch for Change List
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/update-branch",
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn update_branch(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let new_head = state
        .monorepo()
        .update_branch(&user.username, &link)
        .await?;
    Ok(Json(CommonResult::success(Some(new_head))))
}

fn build_merge_box_res(
    cl: &crate::callisto::mega_cl::Model,
    rows: Vec<crate::callisto::check_result::Model>,
    ci_checks_enabled: bool,
) -> MergeBoxRes {
    use crate::{
        callisto::sea_orm_active_enums::CheckTypeEnum,
        ceres::merge_checker::{CheckType, ConditionResult},
    };

    if cl.status != MergeStatusEnum::Open {
        return MergeBoxRes {
            merge_requirements: None,
        };
    }

    if !ci_checks_enabled {
        let check_res: Vec<Condition> = rows
            .into_iter()
            .filter_map(|m| Condition::try_from(m).ok())
            .collect();
        return MergeBoxRes::from_condition(check_res);
    }

    let mut conditions = Vec::with_capacity(rows.len() + 1);
    let mut ci_row: Option<crate::callisto::check_result::Model> = None;
    for row in rows {
        if row.check_type_code == CheckTypeEnum::CiStatus {
            ci_row = Some(row);
        } else if let Ok(cond) = Condition::try_from(row) {
            conditions.push(cond);
        }
    }

    let ci_condition = match ci_row {
        Some(row) if row.head_revision == cl.head_revision && row.commit_id == cl.to_hash => {
            match Condition::try_from(row) {
                Ok(cond) => cond,
                Err(()) => Condition {
                    condition_type: CheckType::CiStatus,
                    display_name: CheckType::CiStatus.display_name().to_owned(),
                    description: CheckType::CiStatus.description().to_owned(),
                    message: "Invalid CI check row".to_owned(),
                    result: ConditionResult::FAILED,
                    commit_id: Some(cl.to_hash.clone()),
                    head_revision: Some(cl.head_revision),
                    ci_task_id: None,
                    ci_attempt_id: None,
                    ci_generation: None,
                    details_url: None,
                    raw_status: None,
                },
            }
        }
        stale_or_missing => {
            let is_invalidated = stale_or_missing
                .as_ref()
                .is_some_and(|r| r.status == "INVALIDATED");
            let message = if is_invalidated {
                "CI check invalidated by CL head update; awaiting new run".to_owned()
            } else if stale_or_missing.is_some() {
                "CI check is for an older CL revision; awaiting check for current head".to_owned()
            } else {
                "Awaiting CI check for current CL head".to_owned()
            };
            Condition {
                condition_type: CheckType::CiStatus,
                display_name: CheckType::CiStatus.display_name().to_owned(),
                description: CheckType::CiStatus.description().to_owned(),
                message,
                result: ConditionResult::PENDING,
                commit_id: Some(cl.to_hash.clone()),
                head_revision: Some(cl.head_revision),
                ci_task_id: None,
                ci_attempt_id: None,
                ci_generation: stale_or_missing
                    .as_ref()
                    .and_then(|r| (r.ci_generation > 0).then_some(r.ci_generation)),
                details_url: None,
                raw_status: Some(if is_invalidated {
                    "INVALIDATED".to_owned()
                } else {
                    "PENDING".to_owned()
                }),
            }
        }
    };
    conditions.push(ci_condition);

    MergeBoxRes::from_condition(conditions)
}

/// Get Merge Box to check merge status
#[utoipa::path(
    get,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/merge-box",
    responses(
        (status = 200, body = CommonResult<MergeBoxRes>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn merge_box(
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<MergeBoxRes>>, ApiError> {
    let cl = state
        .cl_stg()
        .get_cl(&link)
        .await?
        .ok_or(MegaError::Other("CL Not Found".to_string()))?;

    let rows = if cl.status == MergeStatusEnum::Open {
        state.cl_stg().get_check_result(&link).await?
    } else {
        Vec::new()
    };
    let res = build_merge_box_res(&cl, rows, state.storage.config().ci_checks.enabled);
    Ok(Json(CommonResult::success(Some(res))))
}

/// Add new comment on Change List
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/comment",
    request_body = ContentPayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn save_comment(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<ContentPayload>,
) -> Result<Json<CommonResult<()>>, ApiError> {
    state
        .conv_stg()
        .add_conversation(
            &link,
            &user.username,
            Some(payload.content.clone()),
            ConvTypeEnum::Comment,
        )
        .await?;

    // Enqueue notification emails for the CL author (outbox; the
    // background dispatcher delivers them). Best-effort: a notification failure
    // must not fail the comment request. See docs/notification.md phase 0.
    let notif_stg = state.storage.notification_storage();
    let cl_stg = state.cl_stg();
    if let Err(e) = crate::notification::triggers::on_cl_comment_created(
        &notif_stg,
        &cl_stg,
        &user.username,
        &link,
        &payload.content,
    )
    .await
    {
        tracing::warn!(cl = %link, error = %e, "failed to enqueue CL comment notifications");
    }

    if let Ok(Some(cl_model)) = state.cl_stg().get_cl(&link).await {
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClCommentCreated, &cl_model);
    }

    api_common::comment::check_comment_ref(user, state, &payload.content, &link).await
}

/// Edit CL title
#[utoipa::path(
    post,
    params(
        ("link", description = "A string ID representing a Change List"),
    ),
    path = "/{link}/title",
    request_body = ContentPayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn edit_title(
    _: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<ContentPayload>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    state.cl_stg().edit_title(&link, &payload.content).await?;
    if let Ok(Some(cl_model)) = state.cl_stg().get_cl(&link).await {
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClUpdated, &cl_model);
    }
    Ok(Json(CommonResult::success(None)))
}

/// Update CL status (Draft or Open)
#[utoipa::path(
    post,
    params(
        ("link", description = "CL link"),
    ),
    path = "/{link}/status",
    request_body = UpdateClStatusPayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn update_cl_status(
    user: LoginUser,
    Path(link): Path<String>,
    state: State<MonoApiServiceState>,
    Json(payload): Json<UpdateClStatusPayload>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    let res = state.cl_stg().get_cl(&link).await?;
    let model = res.ok_or(MegaError::Other("Not Found".to_string()))?;

    let new_status = match payload.status.to_lowercase().as_str() {
        "draft" => MergeStatusEnum::Draft,
        "open" => MergeStatusEnum::Open,
        _ => {
            return Err(ApiError::from(MegaError::Other(
                "Invalid status. Only 'draft' and 'open' are supported".to_string(),
            )));
        }
    };

    // Only allow Draft ↔ Open transitions
    match (&model.status, &new_status) {
        (MergeStatusEnum::Draft, MergeStatusEnum::Open) => {
            state
                .cl_stg()
                .update_cl_status(model.clone(), new_status.clone())
                .await?;
            state
                .conv_stg()
                .add_conversation(
                    &link,
                    &user.username,
                    Some(format!("{} marked this as ready for review", user.username)),
                    ConvTypeEnum::Review,
                )
                .await?;
            let updated_model = state
                .cl_stg()
                .get_cl(&link)
                .await?
                .ok_or(MegaError::Other("Not Found".to_string()))?;
            state
                .webhook_svc()
                .dispatch(WebhookEvent::ClCreated, &updated_model);
        }
        (MergeStatusEnum::Open, MergeStatusEnum::Draft) => {
            state
                .cl_stg()
                .update_cl_status(model.clone(), new_status.clone())
                .await?;
            state
                .conv_stg()
                .add_conversation(
                    &link,
                    &user.username,
                    Some(format!("{} marked this as draft", user.username)),
                    ConvTypeEnum::Draft,
                )
                .await?;
            let updated_model = state
                .cl_stg()
                .get_cl(&link)
                .await?
                .ok_or(MegaError::Other("Not Found".to_string()))?;
            state
                .webhook_svc()
                .dispatch(WebhookEvent::ClUpdated, &updated_model);
        }
        _ => {
            return Err(ApiError::from(MegaError::Other(
                "Invalid status transition. Only Draft ↔ Open is allowed".to_string(),
            )));
        }
    }

    Ok(Json(CommonResult::success(None)))
}

fn build_forest(paths: Vec<String>) -> Vec<MuiTreeNode> {
    let mut roots: Vec<MuiTreeNode> = Vec::new();

    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.is_empty() {
            continue;
        }
        let root_label = parts[0];
        if let Some(existing_root) = roots.iter_mut().find(|r| r.label == root_label) {
            let mut buf = existing_root.path.clone();
            existing_root.insert_path(&parts[1..], &mut buf);
        } else {
            let mut buf = String::new();
            buf.push('/');
            buf.push_str(parts[0]);
            let mut new_root = MuiTreeNode::new(root_label, &buf);
            new_root.insert_path(&parts[1..], &mut buf);
            roots.push(new_root);
        }
    }

    roots
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use git_internal::DiffItem;

    use crate::api::router::cl_router::build_forest;

    fn extract_files_with_status(diff_output: &str) -> HashMap<String, String> {
        let mut files = HashMap::new();

        let chunks: Vec<&str> = diff_output.split("diff --git ").collect();

        for chunk in chunks.iter().skip(1) {
            let lines: Vec<&str> = chunk.split_whitespace().collect();
            if lines.len() >= 2 {
                let current_file = lines[0].trim_start_matches("a/").to_string();
                files.insert(current_file.clone(), "modified".to_string()); // 默认状态为修改
                if chunk.contains("new file mode") {
                    files.insert(current_file, "new".to_string());
                } else if chunk.contains("deleted file mode") {
                    files.insert(current_file, "deleted".to_string());
                }
            }
        }
        files
    }

    #[test]
    fn test_parse_diff_result_to_filelist() {
        let diff_output = r#"
        diff --git a/ceres/src/api_service/mono_api_service.rs b/ceres/src/api_service/mono_api_service.rs
        new file mode 100644
        index 0000000..561296a1
        @@ -1,0 +1,595 @@
        fn main() {
            println!("Hello, world!");
        }
        diff --git a/ceres/src/lib.rs b/ceres/src/lib.rs
        index 1234567..89abcdef 100644
        --- a/ceres/src/lib.rs
        +++ b/ceres/src/lib.rs
        @@ -10,7 +10,8 @@
        diff --git a/ceres/src/removed.rs b/ceres/src/removed.rs
        deleted file mode 100644
        "#;
        let files_with_status = extract_files_with_status(diff_output);
        println!("Files with status:");
        for (file, status) in &files_with_status {
            println!("{file} ({status})");
        }

        let mut expected = HashMap::new();
        expected.insert(
            "ceres/src/api_service/mono_api_service.rs".to_string(),
            "new".to_string(),
        );
        expected.insert("ceres/src/lib.rs".to_string(), "modified".to_string());
        expected.insert("ceres/src/removed.rs".to_string(), "deleted".to_string());

        assert_eq!(files_with_status, expected);
    }

    #[test]
    fn test_files_changed_tree() {
        let paths = vec![
            String::from("crates-pro/crates_pro/src/bin/bin_analyze.rs"),
            String::from("crates-pro/images/analysis-tool-worker.Dockerfile"),
            String::from("crates-pro/images/crates-pro.Dockerfile"),
            String::from("another-root/foo/bar.txt"),
        ];

        let forest = build_forest(paths);
        println!("{}", serde_json::to_string_pretty(&forest).unwrap());
    }

    #[test]
    fn test_cl_files_changed_logic() {
        // Test the core logic of cl_files_changed function
        // This tests the data transformation logic without needing the full state

        let sample_diff_output = r#"diff --git a/src/main.rs b/src/main.rs
            new file mode 100644
            index 0000000..abc1234
            --- /dev/null
            +++ b/src/main.rs
            @@ -0,0 +1,5 @@
            +fn main() {
            +    println!("Hello, world!");
            +}
            diff --git a/src/lib.rs b/src/lib.rs
            index def5678..ghi9012 100644
            --- a/src/lib.rs
            +++ b/src/lib.rs
            @@ -1,3 +1,4 @@
            +// Added a comment
            pub fn add(left: usize, right: usize) -> usize {
                left + right
            }
            diff --git a/README.md b/README.md
            deleted file mode 100644
            index 1234567..0000000"#;

        // Test extract_files_with_status
        let diff_files = extract_files_with_status(sample_diff_output);

        assert_eq!(diff_files.len(), 3);
        assert_eq!(diff_files.get("src/main.rs"), Some(&"new".to_string()));
        assert_eq!(diff_files.get("src/lib.rs"), Some(&"modified".to_string()));
        assert_eq!(diff_files.get("README.md"), Some(&"deleted".to_string()));

        // Test path extraction and tree building
        let mut paths = vec![];
        for (path, _) in diff_files {
            paths.push(path);
        }

        let mui_trees = build_forest(paths);

        // Verify the tree structure
        assert!(!mui_trees.is_empty());

        // Check that we have the expected root nodes
        let root_labels: Vec<&str> = mui_trees.iter().map(|tree| tree.label.as_str()).collect();
        assert!(root_labels.contains(&"src"));
        assert!(root_labels.contains(&"README.md"));

        let content = [DiffItem {
            data: sample_diff_output.to_string(),
            path: "diff_output.txt".to_string(),
        }];

        assert!(!mui_trees.is_empty());
        assert_eq!(content.first().unwrap().data, sample_diff_output);
    }

    #[test]
    fn test_extract_files_with_status_edge_cases() {
        // Test with empty diff output
        let empty_diff = "";
        let result = extract_files_with_status(empty_diff);
        assert!(result.is_empty());

        // Test with malformed diff output
        let malformed_diff = "not a valid diff output";
        let result = extract_files_with_status(malformed_diff);
        assert!(result.is_empty());

        // Test with diff containing only additions
        let additions_only = r#"diff --git a/new_file.txt b/new_file.txt
new file mode 100644
index 0000000..1234567
--- /dev/null
+++ b/new_file.txt"#;

        let result = extract_files_with_status(additions_only);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("new_file.txt"), Some(&"new".to_string()));

        // Test with diff containing only deletions
        let deletions_only = r#"diff --git a/old_file.txt b/old_file.txt
deleted file mode 100644
index 1234567..0000000"#;

        let result = extract_files_with_status(deletions_only);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("old_file.txt"), Some(&"deleted".to_string()));
    }

    #[test]
    fn test_build_forest_edge_cases() {
        // Test with empty paths
        let empty_paths = vec![];
        let result = build_forest(empty_paths);
        assert!(result.is_empty());

        // Test with single file
        let single_file = vec!["single_file.txt".to_string()];
        let result = build_forest(single_file);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "single_file.txt");

        // Test with deeply nested paths
        let nested_paths = vec![
            "a/b/c/d/e/file.txt".to_string(),
            "a/b/different.txt".to_string(),
            "a/another.txt".to_string(),
        ];
        let result = build_forest(nested_paths);
        assert_eq!(result.len(), 1); // Should have one root "a"
        assert_eq!(result[0].label, "a");

        // The tree should have nested structure
        let root = &result[0];
        assert!(root.children.is_some());
        let children = root.children.as_ref().unwrap();
        assert!(children.iter().any(|child| child.label == "b"));
        assert!(children.iter().any(|child| child.label == "another.txt"));
    }
}

// MC-05 endpoint tests live in a sibling module: they need async runtimes and
// DB-backed state, unlike the pure-logic cases above.
#[cfg(test)]
mod cl_commits_tests {

    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::{compute_orion_ci_v2_signature, routers};
    use crate::{
        api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
        callisto::{mega_cl, mega_cl_commits, mega_commit, sea_orm_active_enums::MergeStatusEnum},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::test_storage,
        },
    };

    fn mc05_state(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            entity_store: Arc::new(SharedEntityStore::new()),
            git_object_cache: Arc::new(crate::ceres::api_service::cache::GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "mc05-test".to_string(),
            }),
            listen_addr: "http://127.0.0.1:0".to_string(),
            // MC-07: the handler takes a mandatory session (401 otherwise);
            // these handler-level tests authenticate as a fixed user.
            session_store: BrowserSessionStore::Fixed(
                crate::api::oauth::api_store::FixedUserSessionStore {
                    user: crate::api::oauth::model::LoginUser {
                        username: "mc05-user".to_string(),
                        ..Default::default()
                    },
                },
            ),
            storage,
        }
    }

    fn mc05_sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn mc05_commit_row(n: u64, parents: &[u64]) -> mega_commit::Model {
        mega_commit::Model {
            id: crate::callisto::entity_ext::generate_id(),
            commit_id: mc05_sha(n),
            tree: mc05_sha(900_000),
            parents_id: serde_json::json!(parents.iter().map(|p| mc05_sha(*p)).collect::<Vec<_>>()),
            author: Some("author MC05 User <mc05@example.invalid> 1750000000 +0000".to_string()),
            committer: Some(
                "committer MC05 User <mc05@example.invalid> 1750000000 +0000".to_string(),
            ),
            content: Some(format!("mc05 message {n}")),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    /// Seed a CL at `(from, to)` plus the listing rows for `listing_members`
    /// (chain order is rebuilt read-side; the write order here is irrelevant).
    async fn mc05_seed(storage: &Storage, link: &str, from: u64, to: u64, listing_members: &[u64]) {
        let cl_stg = storage.cl_storage();
        cl_stg
            .new_cl_model(
                "/",
                link,
                "mc05 test cl",
                "main",
                &mc05_sha(from),
                &mc05_sha(to),
                "mc05-user",
            )
            .await
            .expect("seed CL row");
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();
        let mut commit_rows = vec![mc05_commit_row(from, &[])];
        commit_rows.extend((from + 1..=to).map(|i| mc05_commit_row(i, &[i - 1])));
        mega_commit::Entity::insert_many(
            commit_rows
                .into_iter()
                .map(|m| m.into_active_model())
                .collect::<Vec<_>>(),
        )
        .exec(conn)
        .await
        .expect("insert commits");
        if listing_members.is_empty() {
            return;
        }
        let now = chrono::Utc::now().naive_utc();
        let listing: Vec<mega_cl_commits::ActiveModel> = listing_members
            .iter()
            .map(|&n| {
                let row = mc05_commit_row(n, &[n - 1]);
                mega_cl_commits::ActiveModel {
                    cl_link: sea_orm::Set(link.to_string()),
                    commit_sha: sea_orm::Set(row.commit_id),
                    author_name: sea_orm::Set("MC05 User".to_string()),
                    author_email: sea_orm::Set("mc05@example.invalid".to_string()),
                    message: sea_orm::Set(row.content.unwrap_or_default()),
                    created_at: sea_orm::Set(now),
                    updated_at: sea_orm::Set(now),
                }
            })
            .collect();
        mega_cl_commits::Entity::insert_many(listing)
            .exec(conn)
            .await
            .expect("seed listing");
    }

    async fn mc05_get(state: MonoApiServiceState, link: &str) -> (axum::http::StatusCode, String) {
        let (router, _api) = OpenApiRouter::new().merge(routers()).split_for_parts();
        let resp = router
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/cl/{link}/commits"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// AC①②: the route is registered under the `/cl` nest and its 200 response
    /// advertises the DEP-01 contract schema (the four snake_case fields).
    #[test]
    fn cl_commits_route_is_registered_with_contract_schema() {
        let (_router, api) = routers().split_for_parts();
        let path_item = api
            .paths
            .paths
            .get("/cl/{link}/commits")
            .expect("the /cl/{link}/commits path must be registered");
        let operation = path_item.get.as_ref().expect("GET operation");
        let success = operation
            .responses
            .responses
            .get("200")
            .expect("200 response documented");
        let content_ref = serde_json::to_value(success).expect("response to json");
        assert!(
            content_ref.to_string().contains("ClCommitRes"),
            "the 200 response must reference the ClCommitRes schema: {content_ref}"
        );
        let components = serde_json::to_value(api.components).expect("components to json");
        let schema = &components["schemas"]["ClCommitRes"];
        for field in ["sha", "message", "author_name", "author_email"] {
            assert!(
                schema["properties"][field].is_string() || schema["properties"][field].is_object(),
                "ClCommitRes must declare `{field}`: {schema}"
            );
        }
    }

    /// AC④⑤: a CL with a listing returns the contract fields in chain order
    /// (oldest first), wrapped in CommonResult.
    #[tokio::test]
    async fn cl_commits_returns_contract_fields_in_chain_order() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        mc05_seed(&storage, "CLMC05A1", 100, 103, &[101, 102, 103]).await;

        let (status, body) = mc05_get(mc05_state(storage), "CLMC05A1").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(json["req_result"], true);
        let data = json["data"].as_array().expect("data is an array");
        let shas: Vec<&str> = data.iter().map(|c| c["sha"].as_str().unwrap()).collect();
        assert_eq!(
            shas,
            vec![mc05_sha(101), mc05_sha(102), mc05_sha(103)],
            "commits must arrive oldest-first in chain order"
        );
        let first = &data[0];
        assert_eq!(first["message"], "mc05 message 101");
        assert_eq!(first["author_name"], "MC05 User");
        assert_eq!(first["author_email"], "mc05@example.invalid");
    }

    /// AC⑤: a CL without listing data returns an empty list, not an error.
    #[tokio::test]
    async fn cl_commits_empty_listing_returns_empty_list() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        mc05_seed(&storage, "CLMC05E1", 200, 201, &[]).await;

        let (status, body) = mc05_get(mc05_state(storage), "CLMC05E1").await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(json["req_result"], true);
        assert_eq!(json["data"].as_array().expect("data array").len(), 0);
    }

    /// AC⑥ (DEP-01 ④): an unknown link is a 404, not an empty list.
    #[tokio::test]
    async fn cl_commits_unknown_link_returns_404() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;

        let (status, body) = mc05_get(mc05_state(storage), "CLMC05NO").await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(json["req_result"], false);
    }

    const CI_TEST_SECRET: &str = "clb13-test-hmac-secret-at-least-32-bytes-long";

    async fn ci_test_storage(temp: &TempDir, enabled: bool) -> Storage {
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.ci_checks.enabled = enabled;
        config.ci_checks.hmac_secret =
            Some(crate::config::secret::SecretString::new(CI_TEST_SECRET));
        config.ci_checks.max_skew_secs = 300;
        config.ci_checks.nonce_retention_secs = 604_800;
        crate::jupiter::tests::test_storage_with_config(temp.path(), config).await
    }

    async fn seed_ci_cl(
        storage: &Storage,
        link: &str,
        to_hash: &str,
        head_revision: i64,
        status: MergeStatusEnum,
    ) {
        let now = chrono::Utc::now().naive_utc();
        let mut model = mega_cl::Model::new(
            "/project".to_owned(),
            format!("CI CL {link}"),
            link.to_owned(),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            to_hash.to_owned(),
            "alice".to_owned(),
        );
        model.status = status;
        model.head_revision = head_revision;
        model.created_at = now;
        model.updated_at = now;
        model
            .into_active_model()
            .insert(storage.cl_storage().get_connection())
            .await
            .expect("seed mega_cl");
    }

    async fn put_ci_signed(
        state: MonoApiServiceState,
        target_link: &str,
        sign_link: &str,
        secret: &str,
        timestamp: i64,
        nonce: &str,
        raw_body: &str,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let ts_str = timestamp.to_string();
        let body_sha256 = hex::encode(Sha256::digest(raw_body.as_bytes()));
        let sig = compute_orion_ci_v2_signature(secret, sign_link, &ts_str, nonce, &body_sha256);
        let (router, _api) = OpenApiRouter::new().merge(routers()).split_for_parts();
        let resp = router
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/cl/{target_link}/checks/ci"))
                    .header("content-type", "application/json")
                    .header("x-orion-timestamp", ts_str)
                    .header("x-orion-nonce", nonce)
                    .header("x-orion-signature-v2", sig)
                    .body(Body::from(raw_body.to_owned()))
                    .unwrap(),
            )
            .await
            .expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json response");
        (status, json)
    }

    fn ci_payload(
        commit_sha: &str,
        head_revision: i64,
        ci_generation: i64,
        status: &str,
    ) -> String {
        serde_json::json!({
            "commit_sha": commit_sha,
            "head_revision": head_revision,
            "task_id": "task_clb13_01",
            "attempt_id": "att_clb13_01",
            "ci_generation": ci_generation,
            "status": status,
            "summary": format!("status {status}"),
            "details_url": "http://127.0.0.1:3000/workspace/cls/CL1/checks",
            "occurred_at": "2026-11-24T12:00:00Z"
        })
        .to_string()
    }

    #[tokio::test]
    async fn cl_ci_check_disabled_returns_503_and_invalid_signatures_return_401() {
        let temp = TempDir::new().expect("temp");
        let storage_off = ci_test_storage(&temp, false).await;
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let now_ts = chrono::Utc::now().timestamp();
        let body = ci_payload(sha_a, 1, 1, "pending");

        // 503 when disabled
        let (status, json) = put_ci_signed(
            mc05_state(storage_off),
            "CLCI001",
            "CLCI001",
            CI_TEST_SECRET,
            now_ts,
            "nonce-disabled",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "ci_checks_disabled");

        // Enabled: test 401 cases (wrong secret, expired skew, cross-CL path substitution)
        let temp2 = TempDir::new().expect("temp2");
        let storage = ci_test_storage(&temp2, true).await;
        seed_ci_cl(&storage, "CLCI001", sha_a, 1, MergeStatusEnum::Open).await;
        seed_ci_cl(&storage, "CLCI002", sha_a, 1, MergeStatusEnum::Open).await;

        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI001",
            "CLCI001",
            "wrong-secret-0123456789abcdef0123456789abcdef",
            now_ts,
            "nonce-bad-sig",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "invalid_signature");

        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI001",
            "CLCI001",
            CI_TEST_SECRET,
            now_ts - 301,
            "nonce-expired",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "invalid_signature");

        // Cross-CL path substitution: signed for CLCI001, sent to CLCI002 -> 401
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI002",
            "CLCI001",
            CI_TEST_SECRET,
            now_ts,
            "nonce-path-swap",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "invalid_signature");
    }

    #[tokio::test]
    async fn cl_ci_check_nonce_replay_conflict_aba_and_monotonic_state_machine() {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        let temp = TempDir::new().expect("temp");
        let storage = ci_test_storage(&temp, true).await;
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sha_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        seed_ci_cl(&storage, "CLCI100", sha_a, 1, MergeStatusEnum::Open).await;
        seed_ci_cl(&storage, "CLCI_CLOSED", sha_a, 1, MergeStatusEnum::Closed).await;
        let now_ts = chrono::Utc::now().timestamp();

        // 404 cl_not_found
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CL_MISSING",
            "CL_MISSING",
            CI_TEST_SECRET,
            now_ts,
            "nonce-404",
            &ci_payload(sha_a, 1, 1, "pending"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "cl_not_found");

        // 409 cl_not_open
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI_CLOSED",
            "CLCI_CLOSED",
            CI_TEST_SECRET,
            now_ts,
            "nonce-closed",
            &ci_payload(sha_a, 1, 1, "pending"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "cl_not_open");

        // 409 stale_commit (right head_revision, wrong commit_sha)
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-stale-commit",
            &ci_payload(sha_b, 1, 1, "pending"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "stale_commit");

        // 1) Initial PENDING (rev=1, gen=1) -> 200 updated
        let p_pending = ci_payload(sha_a, 1, 1, "pending");
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-1",
            &p_pending,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["applied"], true);
        assert!(json.get("reason").is_none());
        assert_eq!(json["action"], "updated");
        assert_eq!(json["status"], "PENDING");

        // Exact replay of nonce-1 -> 200 replayed
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-1",
            &p_pending,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["applied"], false);
        assert_eq!(json["reason"], "replayed");
        assert_eq!(json["action"], "replayed");

        // Same nonce-1 with different body (even malformed JSON!) -> 409 nonce_conflict
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-1",
            "{not-valid-json",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["code"], "nonce_conflict");

        // 2) PENDING -> RUNNING (gen=1) -> 200 updated
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-2",
            &ci_payload(sha_a, 1, 1, "running"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["action"], "updated");
        assert_eq!(json["status"], "RUNNING");

        // Regression RUNNING -> PENDING (same gen=1) -> 200 { applied: false, reason: "stale_status_transition" }, zero overwrite
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-3",
            &ci_payload(sha_a, 1, 1, "pending"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["applied"], false);
        assert_eq!(json["reason"], "stale_status_transition");
        assert_eq!(json["action"], "stale_status_transition");
        assert_eq!(json["status"], "RUNNING");

        // 3) RUNNING -> PASSED (gen=1) -> 200 updated
        let p_passed = ci_payload(sha_a, 1, 1, "passed");
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-4",
            &p_passed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["applied"], true);
        assert_eq!(json["action"], "updated");
        assert_eq!(json["status"], "PASSED");

        // Regression PASSED -> RUNNING (same gen=1) -> 200 { applied: false, reason: "stale_status_transition" }, zero overwrite
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-5",
            &ci_payload(sha_a, 1, 1, "running"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["applied"], false);
        assert_eq!(json["reason"], "stale_status_transition");
        assert_eq!(json["action"], "stale_status_transition");
        assert_eq!(json["status"], "PASSED");

        // Different terminal state PASSED -> FAILED (same gen=1) -> 409 already_terminal
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-6",
            &ci_payload(sha_a, 1, 1, "failed"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "already_terminal");

        // Higher generation gen=2 advances PASSED -> FAILED, then lower gen=1 is stale_generation
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-7",
            &ci_payload(sha_a, 1, 2, "failed"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["action"], "updated");
        assert_eq!(json["ci_generation"], 2);
        assert_eq!(json["status"], "FAILED");

        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-8",
            &ci_payload(sha_a, 1, 1, "passed"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["applied"], false);
        assert_eq!(json["reason"], "stale_generation");
        assert_eq!(json["action"], "stale_generation");
        assert_eq!(json["ci_generation"], 2);
        assert_eq!(json["status"], "FAILED");

        // 4) Simulate nonce expiry (delete nonce-7) + ack-loss redrive:
        //    (a) head unchanged -> 200 unchanged, zero overwrite
        crate::callisto::ci_callback_nonces::Entity::delete_by_id("nonce-7".to_owned())
            .exec(storage.cl_storage().get_connection())
            .await
            .expect("delete nonce-7");
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-7",
            &ci_payload(sha_a, 1, 2, "failed"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["action"], "unchanged");
        assert_eq!(json["status"], "FAILED");

        //    (b) Simulate A(rev=1) -> B(rev=2) -> A(rev=3): commit_sha is STILL sha_a,
        //        but head_revision is now 3! After nonce deletion, rev=1 callback MUST be rejected
        //        with permanent 409 stale_head_revision (zero overwrite).
        let cl_row = mega_cl::Entity::find()
            .filter(mega_cl::Column::Link.eq("CLCI100"))
            .one(storage.cl_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        let mut active_cl = cl_row.into_active_model();
        active_cl.head_revision = sea_orm::Set(3);
        active_cl.to_hash = sea_orm::Set(sha_a.to_owned());
        active_cl
            .update(storage.cl_storage().get_connection())
            .await
            .unwrap();

        crate::callisto::ci_callback_nonces::Entity::delete_by_id("nonce-7".to_owned())
            .exec(storage.cl_storage().get_connection())
            .await
            .expect("delete nonce-7 again");

        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-7",
            &ci_payload(sha_a, 1, 2, "failed"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json["code"], "stale_head_revision");

        //    (c) Valid callback for the new head_revision = 3 supersedes the old rev=1 row even
        //        with the same/lower ci_generation and without prior INVALIDATED status.
        let (status, json) = put_ci_signed(
            mc05_state(storage.clone()),
            "CLCI100",
            "CLCI100",
            CI_TEST_SECRET,
            now_ts,
            "nonce-new-head-rev3",
            &ci_payload(sha_a, 3, 1, "pending"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["action"], "updated");
        assert_eq!(json["head_revision"], 3);
        assert_eq!(json["ci_generation"], 1);
        assert_eq!(json["status"], "PENDING");
        let persisted = storage
            .cl_storage()
            .get_check_result("CLCI100")
            .await
            .expect("get check_result");
        let ci_row = persisted
            .into_iter()
            .find(|r| {
                r.check_type_code == crate::callisto::sea_orm_active_enums::CheckTypeEnum::CiStatus
            })
            .expect("ci_status row");
        assert_eq!(ci_row.head_revision, 3);
        assert_eq!(ci_row.ci_generation, 1);
        assert_eq!(ci_row.status, "PENDING");
    }

    #[tokio::test]
    async fn cl_ci_check_concurrent_same_nonce_serializes_without_deadlock() {
        let temp = TempDir::new().expect("temp");
        let storage = ci_test_storage(&temp, true).await;
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        seed_ci_cl(&storage, "CLCONC1", sha_a, 1, MergeStatusEnum::Open).await;
        seed_ci_cl(&storage, "CLCONC2", sha_a, 1, MergeStatusEnum::Open).await;
        let now_ts = chrono::Utc::now().timestamp();

        // 1. Concurrent conflicting requests sharing the exact same nonce (even across different CLs):
        //    advisory lock serializes them so strictly one succeeds (200) and the other gets 409 nonce_conflict.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);
        let s1 = storage.clone();
        let s2 = storage.clone();
        let body1 = ci_payload(sha_a, 1, 1, "pending");
        let body2 = ci_payload(sha_a, 1, 1, "running");

        let t1 = tokio::spawn(async move {
            b1.wait().await;
            put_ci_signed(
                mc05_state(s1),
                "CLCONC1",
                "CLCONC1",
                CI_TEST_SECRET,
                now_ts,
                "shared-concurrent-nonce",
                &body1,
            )
            .await
        });
        let t2 = tokio::spawn(async move {
            b2.wait().await;
            put_ci_signed(
                mc05_state(s2),
                "CLCONC2",
                "CLCONC2",
                CI_TEST_SECRET,
                now_ts,
                "shared-concurrent-nonce",
                &body2,
            )
            .await
        });
        let (r1, r2) = tokio::join!(t1, t2);
        let (st1, j1) = r1.expect("task 1");
        let (st2, j2) = r2.expect("task 2");
        let outcomes = [(st1, j1), (st2, j2)];
        assert_eq!(
            outcomes
                .iter()
                .filter(|(s, _)| *s == StatusCode::OK)
                .count(),
            1,
            "expected exactly one 200 winner: {outcomes:?}"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|(s, j)| *s == StatusCode::CONFLICT && j["code"] == "nonce_conflict")
                .count(),
            1,
            "expected exactly one 409 nonce_conflict loser: {outcomes:?}"
        );

        // 2. Concurrent callbacks reclaiming DIFFERENT expired nonces across different CLs must
        //    both succeed 200 without deadlocking on expired-nonce cleanup.
        let expired_at = chrono::Utc::now().naive_utc() - chrono::TimeDelta::seconds(60);
        for (n, link) in [("exp-nonce-1", "CLCONC1"), ("exp-nonce-2", "CLCONC2")] {
            crate::callisto::ci_callback_nonces::ActiveModel {
                nonce: sea_orm::Set(n.to_owned()),
                cl_link: sea_orm::Set(link.to_owned()),
                body_sha256: sea_orm::Set("0".repeat(64)),
                expires_at: sea_orm::Set(expired_at),
            }
            .insert(storage.cl_storage().get_connection())
            .await
            .expect("seed expired nonce");
        }
        let barrier2 = Arc::new(tokio::sync::Barrier::new(2));
        let b3 = Arc::clone(&barrier2);
        let b4 = Arc::clone(&barrier2);
        let s3 = storage.clone();
        let s4 = storage.clone();
        let body3 = ci_payload(sha_a, 1, 2, "passed");
        let body4 = ci_payload(sha_a, 1, 2, "passed");
        let t3 = tokio::spawn(async move {
            b3.wait().await;
            put_ci_signed(
                mc05_state(s3),
                "CLCONC1",
                "CLCONC1",
                CI_TEST_SECRET,
                now_ts,
                "exp-nonce-1",
                &body3,
            )
            .await
        });
        let t4 = tokio::spawn(async move {
            b4.wait().await;
            put_ci_signed(
                mc05_state(s4),
                "CLCONC2",
                "CLCONC2",
                CI_TEST_SECRET,
                now_ts,
                "exp-nonce-2",
                &body4,
            )
            .await
        });
        let (r3, r4) = tokio::join!(t3, t4);
        assert_eq!(r3.unwrap().0, StatusCode::OK);
        assert_eq!(r4.unwrap().0, StatusCode::OK);
    }

    fn ci_authz_state(
        storage: Storage,
        entity_store: Arc<SharedEntityStore>,
        username: Option<&str>,
    ) -> MonoApiServiceState {
        let mut st = mc05_state(storage);
        st.entity_store = entity_store;
        st.session_store = match username {
            Some(name) => {
                BrowserSessionStore::Fixed(crate::api::oauth::api_store::FixedUserSessionStore {
                    user: crate::api::oauth::model::LoginUser {
                        website_user_id: format!("uid-{name}"),
                        username: name.to_owned(),
                        avatar_url: String::new(),
                        email: format!("{name}@example.invalid"),
                    },
                })
            }
            None => BrowserSessionStore::Counting(
                crate::api::oauth::api_store::CountingSessionStore::new(vec![Ok(None)]),
            ),
        };
        st
    }

    fn private_repo_entity_store() -> Arc<SharedEntityStore> {
        let store = Arc::new(SharedEntityStore::new());
        let entity_json = serde_json::json!({
            "users": {
                "User::\"admin-user\"": {
                    "euid": "User::\"admin-user\"",
                    "parents": ["UserGroup::\"admin\""]
                },
                "User::\"maintainer-user\"": {
                    "euid": "User::\"maintainer-user\"",
                    "parents": ["UserGroup::\"maintainer\""]
                },
                "User::\"reader-user\"": {
                    "euid": "User::\"reader-user\"",
                    "parents": ["UserGroup::\"reader\""]
                },
                "User::\"outsider-user\"": {
                    "euid": "User::\"outsider-user\"",
                    "parents": []
                }
            },
            "repos": {
                "Repository::\"/\"": {
                    "euid": "Repository::\"/\"",
                    "is_private": true,
                    "admins": "UserGroup::\"admin\"",
                    "maintainers": "UserGroup::\"maintainer\"",
                    "readers": "UserGroup::\"reader\"",
                    "parents": []
                }
            },
            "user_groups": {
                "UserGroup::\"admin\"": {
                    "euid": "UserGroup::\"admin\"",
                    "parents": ["UserGroup::\"maintainer\""]
                },
                "UserGroup::\"maintainer\"": {
                    "euid": "UserGroup::\"maintainer\"",
                    "parents": ["UserGroup::\"reader\""]
                },
                "UserGroup::\"reader\"": {
                    "euid": "UserGroup::\"reader\"",
                    "parents": []
                }
            },
            "merge_requests": {},
            "issues": {}
        })
        .to_string();
        store.ensure(&entity_json).expect("build private snapshot");
        store
    }

    async fn get_ci_authz_req(
        state: MonoApiServiceState,
        uri: &str,
    ) -> (StatusCode, Option<String>, serde_json::Value) {
        let (v1_router, _api) = OpenApiRouter::new().merge(routers()).split_for_parts();
        let guarded_v1 =
            v1_router
                .with_state(state.clone())
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::contract::policy::guard::cedar_guard::cedar_guard,
                ));
        let app = axum::Router::new().nest("/api/v1", guarded_v1);
        let full_uri = format!("/api/v1{uri}");
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&full_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router responds");
        let status = resp.status();
        let cache_control = resp
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }));
        (status, cache_control, json)
    }

    #[tokio::test]
    async fn cl_ci_authz_enforces_login_mode_snapshot_and_read_write_separation() {
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        // 1. Cedar mode = off -> 403 ci_authz_policy_not_enforced (even for admin)
        let temp_off = TempDir::new().expect("temp off");
        let mut cfg_off = crate::config::testing::isolated_config(temp_off.path().join("cfg"));
        cfg_off.cedar.enforcement = "off".to_owned();
        let stg_off =
            crate::jupiter::tests::test_storage_with_config(temp_off.path(), cfg_off).await;
        seed_ci_cl(&stg_off, "CLAUTHZ1", sha_a, 1, MergeStatusEnum::Open).await;
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(stg_off, private_repo_entity_store(), Some("admin-user")),
            "/cl/CLAUTHZ1/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["code"], "ci_authz_policy_not_enforced");

        // 2. Cedar mode = shadow -> 403 ci_authz_policy_not_enforced (even for admin)
        let temp_shadow = TempDir::new().expect("temp shadow");
        let mut cfg_shadow =
            crate::config::testing::isolated_config(temp_shadow.path().join("cfg"));
        cfg_shadow.cedar.enforcement = "shadow".to_owned();
        let stg_shadow =
            crate::jupiter::tests::test_storage_with_config(temp_shadow.path(), cfg_shadow).await;
        seed_ci_cl(&stg_shadow, "CLAUTHZ1", sha_a, 1, MergeStatusEnum::Open).await;
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(stg_shadow, private_repo_entity_store(), Some("admin-user")),
            "/cl/CLAUTHZ1/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["code"], "ci_authz_policy_not_enforced");

        // 3. Cedar mode = enforce (exercising full cedar_guard middleware -> handler path)
        let temp_enf = TempDir::new().expect("temp enforce");
        let mut cfg_enf = crate::config::testing::isolated_config(temp_enf.path().join("cfg"));
        cfg_enf.cedar.enforcement = "enforce".to_owned();
        let stg_enf =
            crate::jupiter::tests::test_storage_with_config(temp_enf.path(), cfg_enf).await;
        seed_ci_cl(&stg_enf, "CLAUTHZ1", sha_a, 1, MergeStatusEnum::Open).await;

        // 3a. Anonymous -> 401
        let (status, _, _) = get_ci_authz_req(
            ci_authz_state(stg_enf.clone(), private_repo_entity_store(), None),
            "/cl/CLAUTHZ1/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // 3b. Missing / invalid / duplicate / extra query params -> 400
        for bad_uri in [
            "/cl/CLAUTHZ1/ci-authz",
            "/cl/CLAUTHZ1/ci-authz?action=delete",
            "/cl/CLAUTHZ1/ci-authz?action=read&action=write",
            "/cl/CLAUTHZ1/ci-authz?action=read&extra=1",
        ] {
            let (status, cc, json) = get_ci_authz_req(
                ci_authz_state(
                    stg_enf.clone(),
                    private_repo_entity_store(),
                    Some("admin-user"),
                ),
                bad_uri,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_uri} -> {json}");
            assert_eq!(cc.as_deref(), Some("no-store"));
            assert_eq!(json["code"], "invalid_request");
        }

        // 3c. Entity store snapshot unavailable -> 503 ci_authz_unavailable
        let empty_unbuilt_store = Arc::new(SharedEntityStore::new());
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(stg_enf.clone(), empty_unbuilt_store, Some("admin-user")),
            "/cl/CLAUTHZ1/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["code"], "ci_authz_unavailable");

        // 3d. Unknown CL link -> 404 cl_not_found
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(
                stg_enf.clone(),
                private_repo_entity_store(),
                Some("admin-user"),
            ),
            "/cl/CL_UNKNOWN/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["code"], "cl_not_found");

        // 3e. Read-only user: read=200, write=403 ci_authz_denied
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(
                stg_enf.clone(),
                private_repo_entity_store(),
                Some("reader-user"),
            ),
            "/cl/CLAUTHZ1/ci-authz?action=read",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["allowed"], true);
        assert_eq!(json["action"], "read");
        assert_eq!(json["cl_link"], "CLAUTHZ1");
        assert_eq!(json["principal"], "reader-user");

        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(
                stg_enf.clone(),
                private_repo_entity_store(),
                Some("reader-user"),
            ),
            "/cl/CLAUTHZ1/ci-authz?action=write",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["code"], "ci_authz_denied");

        // 3f. Maintainer user: read=200, write=200
        let (status, cc, json) = get_ci_authz_req(
            ci_authz_state(
                stg_enf.clone(),
                private_repo_entity_store(),
                Some("maintainer-user"),
            ),
            "/cl/CLAUTHZ1/ci-authz?action=write",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(cc.as_deref(), Some("no-store"));
        assert_eq!(json["allowed"], true);
        assert_eq!(json["action"], "write");
        assert_eq!(json["principal"], "maintainer-user");

        // 3g. Unauthorized member (outsider): read=403, write=403
        for act in ["read", "write"] {
            let (status, cc, json) = get_ci_authz_req(
                ci_authz_state(
                    stg_enf.clone(),
                    private_repo_entity_store(),
                    Some("outsider-user"),
                ),
                &format!("/cl/CLAUTHZ1/ci-authz?action={act}"),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{act} -> {json}");
            assert_eq!(cc.as_deref(), Some("no-store"));
            assert_eq!(json["code"], "ci_authz_denied");
        }
    }

    #[test]
    fn ci_merge_gate_merge_box_synthesizes_pending_when_enabled() {
        use super::build_merge_box_res;
        use crate::{
            callisto::{check_result, mega_cl, sea_orm_active_enums::CheckTypeEnum},
            ceres::{
                merge_checker::{CheckType, ConditionResult},
                model::change_list::RequirementsState,
            },
        };

        let now = chrono::Utc::now().naive_utc();
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sha_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let cl = mega_cl::Model {
            id: 1,
            link: "CLMBOX1".to_owned(),
            title: "merge box test".to_owned(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/project/demo".to_owned(),
            from_hash: "0000000000000000000000000000000000000001".to_owned(),
            to_hash: sha_a.to_owned(),
            created_at: now,
            updated_at: now,
            username: "alice".to_owned(),
            base_branch: "main".to_owned(),
            revision: 2,
            head_revision: 3,
        };

        // 1. ci_checks.enabled == false: missing CiStatus does not block mergeability.
        let disabled = build_merge_box_res(&cl, vec![], false);
        let req = disabled.merge_requirements.unwrap();
        assert_eq!(req.state, RequirementsState::MERGEABLE);
        assert!(req.conditions.is_empty());

        // 2. ci_checks.enabled == true, missing CiStatus -> synthesized PENDING + UNMERGEABLE.
        let missing = build_merge_box_res(&cl, vec![], true);
        let req = missing.merge_requirements.unwrap();
        assert_eq!(req.state, RequirementsState::UNMERGEABLE);
        assert_eq!(req.conditions.len(), 1);
        assert_eq!(req.conditions[0].condition_type, CheckType::CiStatus);
        assert_eq!(req.conditions[0].result, ConditionResult::PENDING);
        assert_eq!(req.conditions[0].head_revision, Some(3));
        assert_eq!(req.conditions[0].commit_id.as_deref(), Some(sha_a));

        // 3. Stale head_revision (A->B->A: same commit_id sha_a, old head_revision=1) -> synthesized PENDING + UNMERGEABLE.
        let stale_rev = check_result::Model {
            id: 10,
            created_at: now,
            updated_at: now,
            path: cl.path.clone(),
            cl_link: cl.link.clone(),
            commit_id: sha_a.to_owned(),
            check_type_code: CheckTypeEnum::CiStatus,
            status: "PASSED".to_owned(),
            message: "old green".to_owned(),
            head_revision: 1,
            ci_generation: 4,
            ci_task_id: Some("tsk_old".to_owned()),
            ci_attempt_id: Some("att_old".to_owned()),
            details_url: None,
        };
        let stale_rev_box = build_merge_box_res(&cl, vec![stale_rev], true);
        let req = stale_rev_box.merge_requirements.unwrap();
        assert_eq!(req.state, RequirementsState::UNMERGEABLE);
        assert_eq!(req.conditions.len(), 1);
        assert_eq!(req.conditions[0].result, ConditionResult::PENDING);
        assert_eq!(req.conditions[0].raw_status.as_deref(), Some("PENDING"));

        // 4. Stale commit_id (head_revision matches 3, commit_id is sha_b) -> synthesized PENDING + UNMERGEABLE.
        let stale_commit = check_result::Model {
            id: 11,
            created_at: now,
            updated_at: now,
            path: cl.path.clone(),
            cl_link: cl.link.clone(),
            commit_id: sha_b.to_owned(),
            check_type_code: CheckTypeEnum::CiStatus,
            status: "PASSED".to_owned(),
            message: "wrong commit".to_owned(),
            head_revision: 3,
            ci_generation: 5,
            ci_task_id: None,
            ci_attempt_id: None,
            details_url: None,
        };
        let stale_commit_box = build_merge_box_res(&cl, vec![stale_commit], true);
        let req = stale_commit_box.merge_requirements.unwrap();
        assert_eq!(req.state, RequirementsState::UNMERGEABLE);
        assert_eq!(req.conditions[0].result, ConditionResult::PENDING);

        // 5. Exact match PASSED -> MERGEABLE.
        let exact_pass = check_result::Model {
            id: 12,
            created_at: now,
            updated_at: now,
            path: cl.path.clone(),
            cl_link: cl.link.clone(),
            commit_id: sha_a.to_owned(),
            check_type_code: CheckTypeEnum::CiStatus,
            status: "PASSED".to_owned(),
            message: "green".to_owned(),
            head_revision: 3,
            ci_generation: 6,
            ci_task_id: Some("tsk_3".to_owned()),
            ci_attempt_id: Some("att_3".to_owned()),
            details_url: Some("https://ci.example/tasks/tsk_3".to_owned()),
        };
        let passed_box = build_merge_box_res(&cl, vec![exact_pass], true);
        let req = passed_box.merge_requirements.unwrap();
        assert_eq!(req.state, RequirementsState::MERGEABLE);
        assert_eq!(req.conditions.len(), 1);
        assert_eq!(req.conditions[0].result, ConditionResult::PASSED);
        assert_eq!(req.conditions[0].ci_task_id.as_deref(), Some("tsk_3"));
    }
}
