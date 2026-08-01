use anyhow::anyhow;
use http::StatusCode;

use crate::{
    api::{MonoApiServiceState, oauth::model::LoginUser},
    common::errors::ApiError,
};

pub async fn ensure_admin(state: &MonoApiServiceState, user: &LoginUser) -> Result<(), ApiError> {
    if state.monorepo().check_is_admin(&user.username).await? {
        return Ok(());
    }

    tracing::warn!(
        actor = %user.username,
        "admin check failed: access forbidden"
    );

    Err(ApiError::with_status(
        StatusCode::FORBIDDEN,
        anyhow!("Admin access required"),
    ))
}
