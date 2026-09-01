//! Opt-in FastCDC Media session services.
//!
//! Media chunks remain private to an authenticated actor and repository until
//! a later finalize step publishes the standard LFS fallback.

pub mod chunker;
pub mod protocol;
pub(crate) mod scope;
pub(crate) mod service;

#[cfg(test)]
mod tests;
