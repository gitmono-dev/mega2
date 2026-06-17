//! This is a Rust module that contains several adaptors to different cryptography libraries.
//! The crate::vault::crypto module utilize these adaptors to do the real crypto operations.
//!
//! Only one crypto adaptor can be used in one build. It's configured when building RustyVault.
//! An adaptor implements a set of methods that perform cryptography operations like encryption,
//! description, signing, verification and so on.

#[macro_use]
pub mod common;
#[cfg(not(any()))]
pub mod openssl_adaptor;
#[cfg(any())]
pub mod tongsuo_adaptor;
