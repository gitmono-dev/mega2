//! Agent Capture session row. Server PK is `id` (`capture_id` in the HTTP API).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capture_session")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "Text")]
    pub deployment_id: String,
    #[sea_orm(column_type = "Text")]
    pub tenant_id: String,
    #[sea_orm(column_type = "Text")]
    pub repo_id: String,
    #[sea_orm(column_type = "Text")]
    pub producer_id: String,
    #[sea_orm(column_type = "Text")]
    pub session_kind: String,
    #[sea_orm(column_type = "Text")]
    pub client_session_id: String,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub ended_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "Text")]
    pub completeness: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub partial_reason: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub libra_repoid: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub cl_link: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    #[test]
    fn session_model_has_no_user_id() {
        let src = include_str!("agent_capture_session.rs");
        let field = format!("pub {}: ", "user_id");
        assert!(
            !src.contains(&field),
            "agent_capture_session entity must not declare that column"
        );
    }
}
