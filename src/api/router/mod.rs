pub mod admin_router;
pub mod artifacts_router;
pub mod bot_router;
pub mod buck_router;
pub mod build_trigger_router;
pub mod cl_router;
pub mod code_review_router;
pub mod commit_router;
pub mod conv_router;
pub mod gpg_router;
pub mod group_router;
pub mod label_router;
#[cfg(feature = "fastcdc")]
pub mod lfs_media;
pub mod lfs_router;
pub mod merge_queue_router;
pub mod oci_router;
pub mod preview_router;
pub mod push_queue_router;
pub mod repo_router;
pub mod reviewer_router;
pub mod tag_router;
pub mod user_router;
pub mod webhook_router;

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;
    use utoipa_axum::router::OpenApiRouter;

    use crate::{
        api::{api_doc::ApiDoc, api_router, router::lfs_router},
        config::PushPolicy,
    };

    fn api_v1_paths(policy: PushPolicy) -> Vec<String> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .nest("/api/v1", api_router::routers_for(policy))
            .split_for_parts()
            .1
            .paths
            .paths
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn trunk_openapi_includes_writes_omits_cl_issue_reviewer() {
        let paths = api_v1_paths(PushPolicy::Trunk);
        assert!(
            paths
                .iter()
                .any(|p| p.contains("/blob") || p.contains("/tree")),
            "trunk OpenAPI must keep readonly preview: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("create-entry")),
            "trunk OpenAPI must include create-entry: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/edit/save")),
            "trunk OpenAPI must include /edit/save: {paths:?}"
        );
        for needle in ["/cl", "/issue", "reviewer"] {
            assert!(
                paths.iter().all(|p| !p.contains(needle)),
                "trunk OpenAPI must not include {needle}: {paths:?}"
            );
        }
    }

    #[test]
    fn review_openapi_keeps_cl_reviewer_and_preview_writes() {
        let paths = api_v1_paths(PushPolicy::Review);
        assert!(
            paths.iter().any(|p| p.contains("/cl")),
            "review OpenAPI must include /cl: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("reviewer")),
            "review OpenAPI must include reviewer: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("create-entry")),
            "review OpenAPI must include create-entry: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/edit/save")),
            "review OpenAPI must include /edit/save: {paths:?}"
        );
    }

    #[test]
    fn trunk_http_assembly_merges_lfs_as_does_review() {
        let trunk = OpenApiRouter::with_openapi(ApiDoc::openapi())
            .merge(lfs_router::routers())
            .nest("/api/v1", api_router::routers_for(PushPolicy::Trunk))
            .split_for_parts()
            .1;
        let trunk_paths: Vec<String> = trunk.paths.paths.keys().cloned().collect();
        assert!(
            trunk_paths.iter().any(|p| p.contains("/lfs")),
            "trunk must register LFS: {trunk_paths:?}"
        );

        let review = OpenApiRouter::with_openapi(ApiDoc::openapi())
            .merge(lfs_router::routers())
            .nest("/api/v1", api_router::routers_for(PushPolicy::Review))
            .split_for_parts()
            .1;
        let review_paths: Vec<String> = review.paths.paths.keys().cloned().collect();
        assert!(
            review_paths.iter().any(|p| p.contains("/lfs")),
            "review HTTP must still register LFS: {review_paths:?}"
        );
    }
}
