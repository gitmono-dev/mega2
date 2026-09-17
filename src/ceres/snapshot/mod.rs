//! MST/2 snapshot domain (spec 02/03): fixed views over pinned Git trees.
//!
//! Slice scope (T02-a): native monorepo views only — no bindings, imports or
//! release policy yet (T04/T05). Everything here resolves against fixed OIDs;
//! no code path in this module may read current refs.
pub mod chunks;
pub mod descriptor;
pub mod error;
pub mod frame_stream;
pub mod pages;
pub mod publication;
pub mod resolver;
pub mod runtime;
pub mod view;
