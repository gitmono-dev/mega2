use utoipa::OpenApi;

/// Swagger / OpenAPI tag constants for Mono HTTP APIs.
///
/// Keeping them in a dedicated module avoids mixing documentation concerns
/// with runtime server wiring.
pub const SYSTEM_COMMON: &str = "System Common";
pub const CODE_PREVIEW: &str = "Code Preview";
pub const TAG_MANAGE: &str = "Tag Management";
pub const CL_TAG: &str = "Change List";
pub const GPG_TAG: &str = "Gpg Key";
pub const LABEL_TAG: &str = "Label Management";
pub const CONV_TAG: &str = "Conversation and Comment";
pub const USER_TAG: &str = "User Management";
pub const REPO_TAG: &str = "Repo creation and synchronisation";
pub const MERGE_QUEUE_TAG: &str = "Merge Queue Management";
pub const PUSH_QUEUE_TAG: &str = "MonoWriteQueue Control";
pub const BUCK_TAG: &str = "Buck Upload API";
pub const LFS_TAG: &str = "Git LFS";
/// OpenAPI tag for OCI Distribution API (`/v2/...`).
pub const OCI_TAG: &str = "OCI Distribution";
/// OpenAPI tag for storage-only Agent Capture (`/api/v1/agent-capture`, including discovery).
pub const AGENT_CAPTURE_TAG: &str = "Agent Capture";
pub const CODE_REVIEW_TAG: &str = "Code Review";
pub const GROUP_PERMISSION_TAG: &str = "Group Permission Management";

/// Shared OpenAPI tag for automation / integration–related APIs.
pub const AUTOMATION_TAG: &str = "Automation & Integrations";
pub const BUILD_TRIGGER_TAG: &str = AUTOMATION_TAG;
pub const WEBHOOK_TAG: &str = AUTOMATION_TAG;
pub const BOT_TAG: &str = AUTOMATION_TAG;

/// OpenAPI tag for repo-scoped artifact HTTP APIs (`/api/v1/repos/.../artifacts/...`).
pub const ARTIFACTS_TAG: &str = "Repo Artifacts";

#[derive(OpenApi)]
#[openapi()]
pub struct ApiDoc;
