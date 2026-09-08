//! TP-06: MonoWriteQueue control plane (trunk-push.md §1.7).

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{
        MonoApiServiceState, api_common::group_permission::ensure_admin, api_doc::PUSH_QUEUE_TAG,
        oauth::model::LoginUser,
    },
    common::errors::ApiError,
    contract::api::common::CommonResult,
    jupiter::service::push_queue_service::{
        CancelQueuedOutcome, QueueControlSnapshot, QueueMetricsSnapshot,
    },
};

pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/push-queue",
        OpenApiRouter::new()
            .routes(routes!(snapshot))
            .routes(routes!(metrics))
            .routes(routes!(pause))
            .routes(routes!(resume))
            .routes(routes!(clear_hard_stop))
            .routes(routes!(cancel)),
    )
}

#[derive(Serialize, ToSchema)]
struct CancelResponse {
    cancelled: bool,
    message: String,
}

#[utoipa::path(
    get,
    path = "/snapshot",
    responses((status = 200, body = CommonResult<QueueControlSnapshot>)),
    tag = PUSH_QUEUE_TAG
)]
async fn snapshot(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueControlSnapshot>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let snap = state.storage.push_queue_service.control_snapshot().await?;
    Ok(Json(CommonResult::success(Some(snap))))
}

#[utoipa::path(
    get,
    path = "/metrics",
    responses((status = 200, body = CommonResult<QueueMetricsSnapshot>)),
    tag = PUSH_QUEUE_TAG
)]
async fn metrics(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueMetricsSnapshot>>, ApiError> {
    ensure_admin(&state, &user).await?;
    let snap = state.storage.push_queue_service.metrics_snapshot().await?;
    Ok(Json(CommonResult::success(Some(snap))))
}

#[utoipa::path(
    post,
    path = "/pause",
    responses((status = 200, body = CommonResult<QueueControlSnapshot>)),
    tag = PUSH_QUEUE_TAG
)]
async fn pause(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueControlSnapshot>>, ApiError> {
    ensure_admin(&state, &user).await?;
    state.storage.push_queue_service.pause().await?;
    let snap = state.storage.push_queue_service.control_snapshot().await?;
    Ok(Json(CommonResult::success(Some(snap))))
}

#[utoipa::path(
    post,
    path = "/resume",
    responses((status = 200, body = CommonResult<QueueControlSnapshot>)),
    tag = PUSH_QUEUE_TAG
)]
async fn resume(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueControlSnapshot>>, ApiError> {
    ensure_admin(&state, &user).await?;
    state.storage.push_queue_service.resume().await?;
    let snap = state.storage.push_queue_service.control_snapshot().await?;
    Ok(Json(CommonResult::success(Some(snap))))
}

#[utoipa::path(
    post,
    path = "/clear-hard-stop",
    responses((status = 200, body = CommonResult<QueueControlSnapshot>)),
    tag = PUSH_QUEUE_TAG
)]
async fn clear_hard_stop(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
) -> Result<Json<CommonResult<QueueControlSnapshot>>, ApiError> {
    ensure_admin(&state, &user).await?;
    state
        .storage
        .push_queue_service
        .clear_hard_stop(&user.username)
        .await?;
    let snap = state.storage.push_queue_service.control_snapshot().await?;
    Ok(Json(CommonResult::success(Some(snap))))
}

#[utoipa::path(
    post,
    path = "/cancel/{id}",
    params(("id" = i64, Path, description = "push_queue row id")),
    responses((status = 200, body = CommonResult<CancelResponse>)),
    tag = PUSH_QUEUE_TAG
)]
async fn cancel(
    user: LoginUser,
    State(state): State<MonoApiServiceState>,
    Path(id): Path<i64>,
) -> Result<Json<CommonResult<CancelResponse>>, ApiError> {
    ensure_admin(&state, &user).await?;
    match state.storage.push_queue_service.cancel_queued(id).await? {
        CancelQueuedOutcome::Cancelled => Ok(Json(CommonResult::success(Some(CancelResponse {
            cancelled: true,
            message: "cancelled".into(),
        })))),
        CancelQueuedOutcome::NotFound => Err(ApiError::not_found(anyhow::anyhow!(
            "push_queue row {id} not found"
        ))),
        CancelQueuedOutcome::NotQueued { status } => Err(ApiError::bad_request(anyhow::anyhow!(
            "cancel only applies to Queued rows; current status is {status:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tower::ServiceExt;
    use utoipa_axum::router::OpenApiRouter;

    use super::routers;
    use crate::{
        api::router::merge_queue_router,
        callisto::{
            mega_refs,
            sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
        },
        common::utils::MEGA_BRANCH_NAME,
        config::PushPolicy,
        jupiter::{
            migration::apply_migrations,
            service::push_queue_service::{
                CancelQueuedOutcome, EnqueueRequest, ExecuteOutcome, ExecuteRequest,
            },
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                push_queue_storage::{ClaimOutcome, EnqueueOutcome},
            },
            tests::test_db_connection,
        },
    };

    #[test]
    fn tp06_openapi_registers_control_and_maps_retired_paths() {
        let (_, api) = OpenApiRouter::new()
            .merge(routers())
            .merge(merge_queue_router::routers())
            .split_for_parts();
        let paths: Vec<&str> = api.paths.paths.keys().map(String::as_str).collect();
        for expected in [
            "/push-queue/snapshot",
            "/push-queue/metrics",
            "/push-queue/pause",
            "/push-queue/resume",
            "/push-queue/clear-hard-stop",
            "/push-queue/cancel/{id}",
            "/merge-queue/list",
            "/merge-queue/stats",
            "/merge-queue/remove/{cl_link}",
            "/merge-queue/cancel-all",
        ] {
            assert!(paths.contains(&expected), "missing route {expected}");
        }
    }

    async fn service() -> (
        tempfile::TempDir,
        crate::jupiter::service::push_queue_service::PushQueueService,
    ) {
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(std::sync::Arc::new(db));
        let svc = crate::jupiter::service::push_queue_service::PushQueueService::new(
            base,
            PushPolicy::Trunk,
        )
        .with_timeouts(Duration::from_millis(400), Duration::from_millis(20));
        (temp, svc)
    }

    async fn enqueue_merge(
        svc: &crate::jupiter::service::push_queue_service::PushQueueService,
        op: &str,
    ) -> i64 {
        let outcome = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: op.into(),
                path: format!("/{op}"),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: Some("alice".into()),
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        let EnqueueOutcome::Inserted { id } = outcome else {
            panic!("insert {op}");
        };
        id
    }

    async fn seed_root(svc: &crate::jupiter::service::push_queue_service::PushQueueService) {
        svc.mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/",
                    MEGA_BRANCH_NAME.to_owned(),
                    "a".repeat(40),
                    "b".repeat(40),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tp06_snapshot_reports_depth_head_running_path_kind_requester_wait() {
        let (_t, svc) = service().await;
        let id = enqueue_merge(&svc, "CL-TP06-SNAP").await;
        let snap = svc.control_snapshot().await.unwrap();
        assert_eq!(snap.depth, 1);
        let head = snap.head.expect("head");
        assert_eq!(head.id, id);
        assert_eq!(head.path, "/CL-TP06-SNAP");
        assert_eq!(head.kind, "Merge");
        assert_eq!(head.requester.as_deref(), Some("alice"));
        assert_eq!(head.status, "Queued");
        assert!(head.wait_ms < 60_000);
        assert!(snap.running.is_none());
    }

    #[tokio::test]
    async fn tp06_pause_rejects_new_enqueue_but_queued_row_still_runs() {
        let (_t, svc) = service().await;
        seed_root(&svc).await;
        let id = enqueue_merge(&svc, "CL-TP06-PAUSE").await;
        svc.pause().await.unwrap();
        let snap = svc.control_snapshot().await.unwrap();
        assert!(snap.paused);
        let rejected = svc
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Merge,
                operation_id: "CL-TP06-PAUSE2".into(),
                path: "/CL-TP06-PAUSE2".into(),
                old_id: "0".repeat(40),
                new_id: "1".repeat(40),
                requester: None,
                payload: json!({}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap();
        assert!(matches!(rejected, EnqueueOutcome::Rejected { .. }));
        assert_eq!(
            svc.storage().claim_for_execution(id).await.unwrap(),
            ClaimOutcome::Claimed
        );
        let outcome = svc
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Done { .. }));
        svc.resume().await.unwrap();
        assert!(!svc.control_snapshot().await.unwrap().paused);
    }

    #[tokio::test]
    async fn tp06_resume_does_not_clear_hard_stop() {
        let (_t, svc) = service().await;
        svc.storage()
            .set_control_flags(Some(true), Some(true), None)
            .await
            .unwrap();
        svc.resume().await.unwrap();
        let snap = svc.control_snapshot().await.unwrap();
        assert!(!snap.paused);
        assert!(snap.hard_stopped);
        svc.clear_hard_stop("ops").await.unwrap();
        let snap = svc.control_snapshot().await.unwrap();
        assert!(!snap.hard_stopped);
    }

    #[tokio::test]
    async fn tp06_cancel_only_queued_loses_to_claim() {
        let (_t, svc) = service().await;
        let head = enqueue_merge(&svc, "CL-TP06-C1").await;
        let queued = enqueue_merge(&svc, "CL-TP06-C2").await;
        assert_eq!(
            svc.storage().claim_for_execution(head).await.unwrap(),
            ClaimOutcome::Claimed
        );
        assert_eq!(
            svc.cancel_queued(queued).await.unwrap(),
            CancelQueuedOutcome::Cancelled
        );
        let run = svc.cancel_queued(head).await.unwrap();
        assert!(matches!(
            run,
            CancelQueuedOutcome::NotQueued {
                status: PushQueueStatusEnum::Running
            }
        ));
    }

    #[tokio::test]
    async fn tp06_metrics_include_counters_and_depth() {
        let (_t, svc) = service().await;
        let _id = enqueue_merge(&svc, "CL-TP06-M").await;
        svc.metrics()
            .claim_lost
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        svc.metrics()
            .cas_assert_failures
            .fetch_add(0, std::sync::atomic::Ordering::Relaxed);
        let m = svc.metrics_snapshot().await.unwrap();
        assert_eq!(m.depth, 1);
        assert_eq!(m.claim_lost, 1);
        assert_eq!(m.cas_assert_failures, 0);
        assert_eq!(m.lock_timeout_alarms, 0);
    }

    #[tokio::test]
    async fn tp06_gone_handlers_return_410() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };

        let (router, _api) = OpenApiRouter::new()
            .merge(merge_queue_router::routers())
            .split_for_parts();
        let router = router.with_state(dummy_state().await);
        let remove = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/merge-queue/remove/CL-X")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(remove.status(), StatusCode::GONE);
        let cancel_all = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/merge-queue/cancel-all")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel_all.status(), StatusCode::GONE);
    }

    async fn dummy_state() -> crate::api::MonoApiServiceState {
        use std::sync::Arc;

        use crate::{
            api::oauth::api_store::BrowserSessionStore, bellatrix::Bellatrix,
            ceres::api_service::cache::GitObjectCache,
            contract::policy::entitystore::SharedEntityStore,
        };

        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        crate::api::MonoApiServiceState {
            session_store: BrowserSessionStore::Counting(
                crate::api::oauth::api_store::CountingSessionStore::new(vec![Ok(None)]),
            ),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "tp06-test".to_string(),
            }),
            listen_addr: "http://127.0.0.1:0".to_string(),
            entity_store: Arc::new(SharedEntityStore::new()),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            storage,
        }
    }
}
