use axum::{
    Json,
    extract::{Path, State},
};
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
            AssigneeUpdatePayload, CLDetailRes, ClCommitRes, ClFilesRes, Condition,
            FilesChangedPage, ListPayload, MergeBoxRes, MuiTreeNode, UpdateBranchStatusRes,
            UpdateClStatusPayload,
        },
        conversation::ContentPayload,
        issue::ItemRes,
        label::LabelUpdatePayload,
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
            .routes(routes!(labels))
            .routes(routes!(assignees))
            .routes(routes!(edit_title))
            .routes(routes!(update_cl_status))
            .routes(routes!(update_branch_status))
            .routes(routes!(update_branch))
            .routes(routes!(cl_commits)),
    )
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
    let mut items: Vec<ItemRes> = items.into_iter().map(|m| m.into()).collect();

    // Backfill the aggregated Orion build status for this page (worst-wins
    // over the latest task per CL). One batch query, and a failure only
    // leaves the statuses empty — the list itself still returns.
    // Ported from mega@fae6823 `ceres/.../mono/cl_list.rs` (#2163).
    let links: Vec<String> = items.iter().map(|i| i.link.clone()).collect();
    match state.cl_stg().latest_build_status_by_cl_links(&links).await {
        Ok(statuses) => ItemRes::apply_build_statuses(&mut items, &statuses),
        Err(e) => {
            tracing::warn!("Failed to load CL build statuses for list: {e}");
        }
    }

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
    if let Ok(Some(cl_model)) = state.cl_stg().get_cl(&link).await {
        state
            .webhook_svc()
            .dispatch(WebhookEvent::ClUpdated, &cl_model);
    }
    Ok(Json(CommonResult::success(Some(new_head))))
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

    let res = match cl.status {
        MergeStatusEnum::Open => {
            let check_res: Vec<Condition> = state
                .cl_stg()
                .get_check_result(&link)
                .await?
                .into_iter()
                .map(|m| m.into())
                .collect();
            MergeBoxRes::from_condition(check_res)
        }
        MergeStatusEnum::Draft | MergeStatusEnum::Merged | MergeStatusEnum::Closed => MergeBoxRes {
            merge_requirements: None,
        },
    };
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
    let conv_type = if state
        .storage
        .reviewer_storage()
        .is_reviewer(&link, &user.username)
        .await?
    {
        // If user is the reviewer for this cl, then the comment if of type review
        ConvTypeEnum::Review
    } else {
        ConvTypeEnum::Comment
    };

    state
        .conv_stg()
        .add_conversation(
            &link,
            &user.username,
            Some(payload.content.clone()),
            conv_type,
        )
        .await?;

    // Enqueue notification emails for the CL author + reviewers (outbox; the
    // background dispatcher delivers them). Best-effort: a notification failure
    // must not fail the comment request. See docs/notification.md phase 0.
    let notif_stg = state.storage.notification_storage();
    let cl_stg = state.cl_stg();
    let reviewer_stg = state.storage.reviewer_storage();
    if let Err(e) = crate::notification::triggers::on_cl_comment_created(
        &notif_stg,
        &cl_stg,
        &reviewer_stg,
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

/// Update cl related labels
#[utoipa::path(
    post,
    path = "/labels",
    request_body = LabelUpdatePayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn labels(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(payload): Json<LabelUpdatePayload>,
) -> Result<Json<CommonResult<()>>, ApiError> {
    api_common::label_assignee::label_update(user, state, payload, String::from("cl")).await
}

/// Update CL related assignees
#[utoipa::path(
    post,
    path = "/assignees",
    request_body = AssigneeUpdatePayload,
    responses(
        (status = 200, body = CommonResult<String>, content_type = "application/json"),
        (status = 403, description = "Authorization denied for this Change List operation"),
    ),
    tag = CL_TAG
)]
async fn assignees(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(payload): Json<AssigneeUpdatePayload>,
) -> Result<Json<CommonResult<()>>, ApiError> {
    api_common::label_assignee::assignees_update(user, state, payload, String::from("cl")).await
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
mod mc05_tests {

    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use sea_orm::{EntityTrait, IntoActiveModel};
    use tempfile::TempDir;
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::routers;
    use crate::{
        api::{MonoApiServiceState, oauth::api_store::BrowserSessionStore},
        bellatrix::Bellatrix,
        callisto::{mega_cl_commits, mega_commit},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::test_storage,
        },
    };

    fn mc05_state(storage: Storage) -> MonoApiServiceState {
        MonoApiServiceState {
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
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
}
