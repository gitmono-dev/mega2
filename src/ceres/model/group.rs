use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::callisto::{mega_group, mega_group_member, sea_orm_active_enums::PermissionEnum};

#[derive(Debug, Deserialize, ToSchema)]
pub struct EmptyListAdditional {}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateGroupRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateGroupRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupResponse {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddMembersRequest {
    pub usernames: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupMemberResponse {
    pub id: i64,
    pub group_id: i64,
    pub username: String,
    pub joined_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionValue {
    Read,
    Write,
    Admin,
}

impl PermissionValue {
    pub fn level(self) -> u8 {
        match self {
            PermissionValue::Read => 1,
            PermissionValue::Write => 2,
            PermissionValue::Admin => 3,
        }
    }

    pub fn satisfies(self, required: PermissionValue) -> bool {
        self.level() >= required.level()
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteGroupResponse {
    pub group_id: i64,
    pub deleted_members_count: u64,
    pub deleted_permissions_count: u64,
    pub deleted_groups_count: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RemoveMemberResponse {
    pub group_id: i64,
    pub username: String,
    pub removed: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserGroupsResponse {
    pub username: String,
    pub groups: Vec<GroupResponse>,
}

impl From<mega_group::Model> for GroupResponse {
    fn from(value: mega_group::Model) -> Self {
        Self {
            id: value.id,
            name: value.name,
            description: value.description,
            created_at: value.created_at.and_utc().timestamp(),
            updated_at: value.updated_at.and_utc().timestamp(),
        }
    }
}

impl From<mega_group_member::Model> for GroupMemberResponse {
    fn from(value: mega_group_member::Model) -> Self {
        Self {
            id: value.id,
            group_id: value.group_id,
            username: value.username,
            joined_at: value.joined_at.and_utc().timestamp(),
        }
    }
}

impl From<PermissionValue> for PermissionEnum {
    fn from(value: PermissionValue) -> Self {
        match value {
            PermissionValue::Read => PermissionEnum::Read,
            PermissionValue::Write => PermissionEnum::Write,
            PermissionValue::Admin => PermissionEnum::Admin,
        }
    }
}

impl From<PermissionEnum> for PermissionValue {
    fn from(value: PermissionEnum) -> Self {
        match value {
            PermissionEnum::Read => PermissionValue::Read,
            PermissionEnum::Write => PermissionValue::Write,
            PermissionEnum::Admin => PermissionValue::Admin,
        }
    }
}
