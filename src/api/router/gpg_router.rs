use axum::{Json, extract::State};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    api::{MonoApiServiceState, api_doc::GPG_TAG, oauth::model::LoginUser},
    callisto::gpg_key::Model,
    ceres::model::gpg::{GpgKey, NewGpgRequest, RemoveGpgRequest},
    common::errors::ApiError,
    contract::api::common::CommonResult,
};
pub fn routers() -> OpenApiRouter<MonoApiServiceState> {
    OpenApiRouter::new().nest(
        "/gpg",
        OpenApiRouter::new()
            .routes(routes!(add_gpg))
            .routes(routes!(remove_gpg))
            .routes(routes!(list_gpg)),
    )
}

#[utoipa::path(
    delete,
    path = "/remove",
    request_body = RemoveGpgRequest,
    responses(
        (status = 200, body = CommonResult<String>, content_type="application/json")
    ),
    tag = GPG_TAG
)]
async fn remove_gpg(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(req): Json<RemoveGpgRequest>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    // let uid = "exampleid".to_string();
    let uid = user.website_user_id.clone();
    state.gpg_stg().remove_gpg_key(uid, req.key_id).await?;
    Ok(Json(CommonResult::success(None)))
}

#[utoipa::path(
    post,
    path = "/add",
    request_body = NewGpgRequest,
    responses(
        (status = 200, body = CommonResult<String>, content_type="application/json")
    ),
    tag = GPG_TAG
)]
async fn add_gpg(
    user: LoginUser,
    state: State<MonoApiServiceState>,
    Json(req): Json<NewGpgRequest>,
) -> Result<Json<CommonResult<String>>, ApiError> {
    // let uid = "exampleid".to_string();
    let uid = user.website_user_id.clone();
    println!("Adding GPG key for user: {}", req.gpg_content.clone());
    state.gpg_stg().add_gpg_key(uid, req.gpg_content).await?;

    Ok(Json(CommonResult::success(None)))
}
#[utoipa::path(
    get,
    path = "/list",
    responses(
        (status = 200, body = CommonResult<Vec<GpgKey>>, content_type="application/json")
    ),
    tag = GPG_TAG
)]
async fn list_gpg(
    user: LoginUser,
    state: State<MonoApiServiceState>,
) -> Result<Json<CommonResult<Vec<GpgKey>>>, ApiError> {
    // let uid = "exampleid".to_string();
    let uid = user.website_user_id;
    let raw_keys = state.gpg_stg().list_user_gpg(uid.clone()).await;

    let res: Vec<GpgKey> = raw_keys
        .into_iter()
        .flatten()
        .map(|k: Model| GpgKey {
            user_id: uid.clone(),
            key_id: k.key_id,
            fingerprint: k.fingerprint,
            created_at: k.created_at.and_utc(),
            expires_at: k.expires_at.map(|dt| dt.and_utc()),
        })
        .collect();

    Ok(Json(CommonResult::success(Some(res))))
}

#[cfg(test)]
mod tests {
    use super::{LoginUser, routers};

    #[test]
    fn gpg_routes_are_registered() {
        let (_, api) = routers().split_for_parts();
        let paths: Vec<&str> = api.paths.paths.keys().map(String::as_str).collect();
        for expected in ["/gpg/add", "/gpg/remove", "/gpg/list"] {
            assert!(paths.contains(&expected), "missing route {expected}");
        }
    }

    #[test]
    fn gpg_identity_uses_website_user_id_json_field() {
        // Pins the AU-04 rename for this consumer: the external-id field the
        // gpg handlers read must serialize as `website_user_id`, and the
        // legacy Campsite name must be gone.
        let user = LoginUser {
            website_user_id: "website-user-1".to_string(),
            username: "u".to_string(),
            avatar_url: String::new(),
            email: "u@example.com".to_string(),
        };
        let value = serde_json::to_value(&user).expect("serialize LoginUser");
        assert!(value.get("website_user_id").is_some());
        assert!(value.get("campsite_user_id").is_none());
    }
}
