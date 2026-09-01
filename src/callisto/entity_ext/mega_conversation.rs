use sea_orm::entity::prelude::*;

use crate::{
    callisto::{
        entity_ext::generate_id,
        mega_conversation::{self, Column, Entity},
        sea_orm_active_enums::ConvTypeEnum,
    },
    common::errors::MegaError,
};

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {
    MegaMr,
    Reactions,
}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        match self {
            Self::MegaMr => Entity::belongs_to(crate::callisto::mega_cl::Entity)
                .from(Column::Link)
                .to(crate::callisto::mega_cl::Column::Link)
                .into(),
            Self::Reactions => Entity::has_many(crate::reactions::Entity).into(),
        }
    }
}

impl Related<crate::callisto::mega_cl::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MegaMr.def()
    }
}

impl Related<crate::reactions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Reactions.def()
    }
    fn via() -> Option<RelationDef> {
        None
    }
}

impl mega_conversation::Model {
    pub fn new(
        link: &str,
        conv_type: ConvTypeEnum,
        comment: Option<String>,
        username: &str,
    ) -> Result<Self, MegaError> {
        let now = chrono::Utc::now().naive_utc();
        let resolved = if conv_type == ConvTypeEnum::Review {
            Some(false)
        } else {
            None
        };

        Ok(Self {
            id: generate_id()?,
            link: link.to_owned(),
            conv_type,
            comment,
            created_at: now,
            updated_at: now,
            username: username.to_owned(),
            resolved,
        })
    }
}
