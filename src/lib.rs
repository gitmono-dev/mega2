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

/// Internal seam for the ImportRepo detach IT (plan-20260923 FU-16): detach
/// without the sweep, which neither product entry (POST
/// /api/v1/import-repo/remove, FU-20; `mega2 import-repo remove`, FU-21) can
/// do. Not a supported API.
#[doc(hidden)]
pub mod import_repo_ops {
    pub use crate::ceres::pack::import_repo::detach_for_integration_test;
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

/// Internal assembly seam for the opt-in history projection benchmark (HP-24).
/// Not a supported API.
#[doc(hidden)]
pub mod view_bench_ops {
    use std::{path::Path, sync::Arc};

    use git_internal::{
        hash::ObjectHash,
        internal::object::{commit::Commit, tree::Tree},
    };
    use sea_orm::{DatabaseConnection, DatabaseTransaction};

    use crate::{
        MegaError,
        config::{Config, DbConfig, testing::isolated_config},
        jupiter::{
            service::{
                view_metrics::ViewMetrics,
                view_projection_service::{CatchUpOutcome, ViewProjectionService},
            },
            storage::{Storage, object_storage::mock_object_storage},
        },
    };
    pub use crate::{
        ceres::view::filter::{
            CanonicalFilter, Filter, parse_for_registration,
            validate::{
                REGISTER_SCALE_LIMITS, RegistrationCheck, ScaleLimits, validate_for_registration,
            },
        },
        common::utils::generate_id,
        jupiter::{
            storage::{
                init::database_connection, view_root_chain::RootChainOutcome,
                view_storage::ViewLockMode,
            },
            utils::converter::sort_git_tree_items,
        },
    };

    pub struct BenchHarness {
        storage: Storage,
        projection: ViewProjectionService,
    }

    impl BenchHarness {
        pub async fn new(
            connection: Arc<DatabaseConnection>,
            batch_size: usize,
            base_dir: &Path,
        ) -> Result<Self, MegaError> {
            let mut config = isolated_config(base_dir);
            config.views.batch_size = u64::try_from(batch_size)
                .map_err(|_| MegaError::Other("benchmark batch size is too large".into()))?;
            let storage =
                Storage::new_with_connection(Arc::new(config), connection, mock_object_storage())
                    .await?;
            let projection = ViewProjectionService::new(storage.clone(), ViewMetrics::default());
            Ok(Self {
                storage,
                projection,
            })
        }

        pub fn config(&self) -> Arc<Config> {
            self.storage.config()
        }

        pub async fn extend_root_chain(
            &self,
            batch_size: usize,
        ) -> Result<RootChainOutcome, MegaError> {
            self.storage
                .view_storage()
                .extend_root_chain(None, batch_size, ViewLockMode::Try)
                .await
        }

        pub async fn catch_up(&self, filter_pk: i64) -> Result<bool, MegaError> {
            Ok(matches!(
                self.projection.catch_up(filter_pk).await?,
                CatchUpOutcome::Ready
            ))
        }

        pub async fn save_commits(&self, commits: Vec<Commit>) -> Result<(), MegaError> {
            self.storage
                .mono_storage()
                .save_mega_commits(commits, None)
                .await
        }

        pub async fn save_trees(
            &self,
            trees: Vec<Tree>,
            commit_id: ObjectHash,
        ) -> Result<(), MegaError> {
            self.storage
                .mono_storage()
                .save_mega_trees(trees, commit_id, None)
                .await
        }

        pub async fn insert_root_ref(
            &self,
            transaction: &DatabaseTransaction,
            model: crate::mega_refs::Model,
        ) -> Result<(), MegaError> {
            self.storage
                .mono_storage()
                .insert_ref_if_not_exists_in_txn(transaction, model)
                .await?;
            Ok(())
        }

        pub async fn cas_root_ref(
            &self,
            transaction: &DatabaseTransaction,
            expected_commit: &str,
            expected_tree: &str,
            next_commit: &str,
            next_tree: &str,
        ) -> Result<bool, MegaError> {
            self.storage
                .mono_storage()
                .cas_update_root_main_ref_in_txn(
                    transaction,
                    Some(expected_commit),
                    Some(expected_tree),
                    next_commit,
                    next_tree,
                )
                .await
        }
    }

    pub fn db_config(db_url: String) -> DbConfig {
        DbConfig {
            db_url,
            ..DbConfig::default()
        }
    }
}

// Public entry points for the thin `mega2` binary (composition root).
pub use cli::parse;
pub use common::errors::MegaError;

/// Internal seam for github_sync loopback ITs (plan-20260916 GS-07).
#[doc(hidden)]
pub mod github_sync {
    pub use crate::ceres::github_sync::{
        key::{GithubSyncKey, held},
        send_pack::{
            Advertisement, PACK_WRITE_WINDOW, ReceivePack, WriteStats, advertise, begin,
            build_command_frame,
        },
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
