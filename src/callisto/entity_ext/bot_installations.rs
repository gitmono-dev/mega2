use chrono::Utc;

use crate::{
    callisto::{
        bot_installations,
        entity_ext::generate_id,
        sea_orm_active_enums::{InstallationBotStatusEnum, InstallationTargetTypeEnum},
    },
    common::errors::MegaError,
};

impl bot_installations::Model {
    pub fn new(
        bot_id: i64,
        target_type: InstallationTargetTypeEnum,
        target_id: i64,
        status: InstallationBotStatusEnum,
        installed_by: i64,
    ) -> Result<Self, MegaError> {
        let now = Utc::now().into();

        Ok(Self {
            id: generate_id()?,
            bot_id,
            target_type,
            target_id,
            status,
            installed_by,
            installed_at: now,
        })
    }
}
