use sea_orm::entity::prelude::*;

use crate::callisto::item_assignees::{Column, Entity};

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {
    MegaMr,
}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        match self {
            Self::MegaMr => Entity::belongs_to(crate::callisto::mega_cl::Entity)
                .from(Column::ItemId)
                .to(crate::callisto::mega_cl::Column::Id)
                .into(),
        }
    }
}

impl Related<crate::callisto::mega_cl::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MegaMr.def()
    }
}
