use sea_orm::entity::prelude::*;

use crate::{
    callisto::{
        entity_ext::generate_id,
        mega_code_review_thread::{self, Column, Entity},
        sea_orm_active_enums::ThreadStatusEnum,
    },
    common::errors::MegaError,
};

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {
    MegaMr,
    Comment,
    Anchor,
}

impl RelationTrait for Relation {
    fn def(&self) -> sea_orm::RelationDef {
        match self {
            Self::MegaMr => Entity::belongs_to(crate::callisto::mega_cl::Entity)
                .from(Column::Link)
                .to(crate::callisto::mega_cl::Column::Link)
                .into(),
            Self::Comment => Entity::has_many(crate::mega_code_review_comment::Entity).into(),
            Self::Anchor => Entity::has_one(crate::mega_code_review_anchor::Entity).into(),
        }
    }
}

impl Related<crate::callisto::mega_cl::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MegaMr.def()
    }
}

impl mega_code_review_thread::Model {
    pub fn new(link: &str, thread_status: ThreadStatusEnum) -> Result<Self, MegaError> {
        let now = chrono::Utc::now().naive_utc();

        Ok(Self {
            id: generate_id()?,
            link: link.to_owned(),
            thread_status,
            created_at: now,
            updated_at: now,
        })
    }
}
