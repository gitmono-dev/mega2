//! MST/2 snapshot domain (spec 02/03): fixed views over pinned Git trees.
//!
//! Serving remains native-only. The independent namespace index is an unwired
//! composition seam; identity, attestation and publication integration remain
//! separate gates. Fixed-source readers must never look up current refs.
pub(crate) mod chunk_map_gate;
pub(crate) mod chunk_map_index;
pub mod chunks;
pub(crate) mod content_budget;
pub mod descriptor;
pub mod error;
pub mod frame_stream;
pub(crate) mod metadata_install;
pub mod namespace;
pub mod pages;
pub(crate) mod projection_observation;
pub(crate) mod projection_writer;
pub mod publication;
pub mod resolver;
pub mod retention;
pub mod retention_dag;
pub(crate) mod rooted_metadata_install;
pub(crate) mod rooted_metadata_projection;
pub mod runtime;
pub mod view;
