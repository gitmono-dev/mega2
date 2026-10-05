//! MST/2 snapshot domain (spec 02/03): fixed views over pinned Git trees.
//!
//! Serving remains native-only. The independent namespace index is an unwired
//! composition seam; identity, attestation and publication integration remain
//! separate gates. Fixed-source readers must never look up current refs.
pub mod chunks;
pub mod descriptor;
pub mod error;
pub mod frame_stream;
pub mod namespace;
pub mod pages;
pub mod publication;
pub mod resolver;
pub mod retention;
pub mod runtime;
pub mod view;
