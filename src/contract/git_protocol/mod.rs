use std::str::FromStr;

use cedar_policy::{Context, EntityId, EntityTypeName, EntityUid};
use serde::Deserialize;

use crate::{
    ceres::{api_service::state::ProtocolApiState, protocol::AuthContext},
    common::errors::ProtocolError,
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
