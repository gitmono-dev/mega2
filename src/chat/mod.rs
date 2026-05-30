//! Chat/channel engine domain boundary.
//!
//! This module owns the migrated chat runtime inside monoengine. Keep storage,
//! service, and API wiring behind this boundary instead of spreading channel
//! concepts through existing Git hosting modules.

pub mod domain;
pub mod engine;
pub mod service;

pub use domain::{
    ChatCapability, ChatEntityKind, ChatEntityRef, ChatEvents, ChatMigrationSlice, NoopChatEvents,
};
pub use engine::ChatEngine;
