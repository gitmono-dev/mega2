use crate::chat::domain::{ChatCapability, ChatMigrationSlice, MIGRATION_SLICES};

#[derive(Clone, Debug, Default)]
pub struct ChatEngine;

impl ChatEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn migration_slices(&self) -> &'static [ChatMigrationSlice] {
        MIGRATION_SLICES
    }

    pub fn has_capability(&self, capability: &ChatCapability) -> bool {
        self.migration_slices()
            .iter()
            .any(|slice| &slice.capability == capability)
    }
}
