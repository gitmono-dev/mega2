pub mod error;
pub mod factory;

mod adapter;

pub use adapter::{ObjectStoreAdapter, head_result_to_exists};
