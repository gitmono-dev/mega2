use chrono::Utc;

use crate::{
    callisto::{bot_keys, entity_ext::generate_id},
    common::errors::MegaError,
};

impl bot_keys::Model {
    pub fn new(bot_id: i64, private_key: String, public_key: String) -> Result<Self, MegaError> {
        let now = Utc::now().into();

        Ok(Self {
            id: generate_id()?,
            bot_id,
            private_key,
            public_key,
            created_at: now,
        })
    }
}
