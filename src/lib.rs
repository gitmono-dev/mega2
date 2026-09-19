#![allow(dead_code)]

//! `mega2-core` — the mega2 library.
//!
//! This crate contains all of mega2's logic, including the inlined
//! `orbit_api` contract and `orbit` object-storage implementation.

mod api;
mod api_model;
mod callisto;
pub use crate::callisto::*;
mod ceres;
mod cli;
mod commands;
mod common;
pub mod config;
mod context;
mod contract;
mod jupiter;
pub mod notification;
pub mod orbit;
pub mod orbit_api;
mod server;

/// Internal seam for the read-only ops assembly (UN-30).
///
/// The zero-side-effect proof drives the assembly from the bin IT target
/// without going through the CLI. `authz-audit` (UN-29) is the supported
/// operator surface; this module stays for the black-box before/after proof.
#[doc(hidden)]
pub mod readonly_ops {
    pub use crate::{
        commands::{LoadedConfigPaths, LoadedConfigSummary},
        context::ReadOnlyContext,
        jupiter::storage::ReadOnlyStorage,
    };
}

/// Internal seam for IT bridging promote (UN-35) until `authz-audit promote`
/// lands in UN-37. Not a supported API.
#[doc(hidden)]
pub mod authz_audit_ops {
    pub use crate::contract::policy::{
        baseline_promotion::{PromoteFence, PromoteRequest, content_digest, promote},
        secure_artifact::RestrictedRoot,
    };
}

// Public entry points for the thin `mega2` binary (composition root).
pub use cli::parse;
pub use common::errors::MegaError;

/// Internal seam for github_sync loopback ITs (plan-20260916 GS-07).
#[doc(hidden)]
pub mod github_sync {
    pub use crate::ceres::github_sync::{
        key::{GithubSyncKey, held},
        send_pack::{Advertisement, advertise},
        ssh::{SshError, SshSession, SshStage, connect},
    };

    pub fn install_openssh(openssh: &str) -> Result<GithubSyncKey, crate::MegaError> {
        crate::ceres::github_sync::key::install_openssh_for_it(openssh)
    }

    pub fn clear_held() {
        crate::ceres::github_sync::key::clear_held_for_it();
    }
}

/// Hidden re-exports for the `migrate_local_to_s3` auxiliary binary (ORB-06).
#[doc(hidden)]
pub mod orbit_bin_api {
    pub use crate::orbit::{
        error::{IoOrbitError, OrbitResult},
        factory::{ObjectStorageBackend, ObjectStorageConfig},
        head_result_to_exists,
    };
}
