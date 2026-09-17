use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::contract::api::common::CommonPage;

#[derive(PartialEq, Eq, Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct TagInfo {
    pub name: String,
    pub tag_id: String,
    pub object_id: String,
    pub object_type: String,
    pub tagger: String,
    pub message: String,
    pub created_at: String,
}

/// Request to create a tag
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateTagRequest {
    /// Tag name
    pub name: String,
    /// Target commit SHA (optional, defaults to current HEAD)
    #[serde(alias = "target_commit")]
    pub target: Option<String>,
    /// Optional path context to indicate which repo or path this tag applies to
    pub path_context: Option<String>,
    /// Tagger name
    pub tagger_name: Option<String>,
    /// Tagger email
    pub tagger_email: Option<String>,
    /// Tag message (if provided creates annotated tag, otherwise creates lightweight tag)
    pub message: Option<String>,
}

/// Tag information response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TagResponse {
    /// Tag name
    pub name: String,
    /// Tag ID (SHA-1)
    pub tag_id: String,
    /// Pointed object ID
    pub object_id: String,
    /// Object type (commit/tag)
    pub object_type: String,
    /// Creator information
    pub tagger: String,
    /// Tag message
    pub message: String,
    /// Creation time
    pub created_at: String,
}

/// Tag list response (paged)
pub type TagListResponse = CommonPage<TagResponse>;

/// Query for `GET /tags/list` (plan-20260918 ADR-FT-02): three required keys.
/// Missing/illegal keys fail in the axum 0.8 `Query` extractor (HTTP 400).
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct TagListQuery {
    pub page: u64,
    pub per_page: u64,
    pub path: String,
}

/// Delete tag response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DeleteTagResponse {
    /// Deleted tag name
    pub deleted_tag: String,
    /// Operation message
    pub message: String,
}
