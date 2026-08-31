use sea_orm::entity::prelude::*;

use crate::callisto::item_labels::{Column, Entity};

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {
    MegaCl,
    Label,
}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        match self {
            Self::MegaCl => Entity::belongs_to(crate::callisto::mega_cl::Entity)
                .from(Column::ItemId)
                .to(crate::callisto::mega_cl::Column::Id)
                .into(),
            Self::Label => Entity::belongs_to(crate::callisto::label::Entity)
                .from(Column::LabelId)
                .to(crate::callisto::label::Column::Id)
                .into(),
        }
    }
}

impl Related<crate::callisto::label::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Label.def()
    }
}

impl Related<crate::callisto::mega_cl::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::MegaCl.def()
    }
}
