use sea_orm::entity::prelude::*;

use crate::callisto::{
    entity_ext::generate_id,
    mega_conversation::{self, Column, Entity},
    sea_orm_active_enums::ConvTypeEnum,
};

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {
    MegaMr,
}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        match self {
            Self::MegaMr => Entity::belongs_to(crate::callisto::mega_cl::Entity)
                .from(Column::Link)
                .to(crate::callisto::mega_cl::Column::Link)
                .into(),
        }
    }
}

impl Related<crate::callisto::mega_cl::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MegaMr.def()
    }
}

impl mega_conversation::Model {
    pub fn new(
        link: &str,
        conv_type: ConvTypeEnum,
        comment: Option<String>,
        username: &str,
    ) -> Self {
        let now = chrono::Utc::now().naive_utc();
        let resolved = if conv_type == ConvTypeEnum::Review {
            Some(false)
        } else {
            None
        };

        Self {
            id: generate_id(),
            link: link.to_owned(),
            conv_type,
            comment,
            created_at: now,
            updated_at: now,
            username: username.to_owned(),
            resolved,
        }
    }
}
