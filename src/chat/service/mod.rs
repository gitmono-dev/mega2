//! Chat domain services (Slice 3).
//!
//! Orchestrates storage calls, business rules (latest pointer, membership updates,
//! soft delete recomputes, notifications internal state), and event emission.
//! No direct SeaORM in handlers.

pub mod channel_chat;
pub mod shared;

pub use channel_chat::ChannelChatService;
pub use shared::SharedChatService;
