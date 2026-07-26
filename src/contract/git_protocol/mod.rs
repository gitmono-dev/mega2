use std::str::FromStr;

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use serde::Deserialize;

use crate::{
    ceres::{api_service::state::ProtocolApiState, protocol::AuthContext},
    common::errors::ProtocolError,
    config::GitConfig,
    contract::policy::{context::CedarContext, util::SaturnEUid},
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
    let username = auth
        .username
        .as_deref()
        .ok_or_else(|| ProtocolError::Forbidden("push requires authentication".to_owned()))?;

    let repo_name = repo_path.to_str().ok_or_else(|| {
        ProtocolError::InvalidInput("repository path is not valid UTF-8".to_owned())
    })?;

    let entity_store = &state.entity_store;

    if entity_store.is_empty() {
        return Ok(());
    }

    let cedar_context = CedarContext::new(entity_store.clone())
        .map_err(|e| ProtocolError::Forbidden(format!("policy engine error: {e}")))?;

    let principal = SaturnEUid::from(EntityUid::from_type_name_and_id(
        EntityTypeName::from_str("User").map_err(|e| ProtocolError::Forbidden(e.to_string()))?,
        EntityId::new(username),
    ));

    let action = SaturnEUid::from(EntityUid::from_type_name_and_id(
        EntityTypeName::from_str("Action").map_err(|e| ProtocolError::Forbidden(e.to_string()))?,
        EntityId::new("pushRepo"),
    ));

    let resource = SaturnEUid::from(EntityUid::from_type_name_and_id(
        EntityTypeName::from_str("Repository")
            .map_err(|e| ProtocolError::Forbidden(e.to_string()))?,
        EntityId::new(repo_name),
    ));

    cedar_context
        .is_authorized(&principal, &action, &resource, Context::empty())
        .map_err(|e| ProtocolError::Forbidden(format!("push permission denied: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ceres::protocol::PushUserInfo;

    #[tokio::test]
    async fn anonymous_access_allowed_by_default() {
        let git_config = GitConfig {
            anonymous_access: true,
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
}
