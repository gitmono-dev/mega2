//! `monoengine` — the thin binary (composition root).
//!
//! This crate wires the concrete object-storage implementation (the heavy
//! `orbit` crate that pulls `object_store` + cloud SDKs) into `monoengine-core`
//! via [`set_object_storage_provider`], installs the global allocator, then
//! dispatches the CLI through [`parse`]. `monoengine-core` itself depends only
//! on the `orbit-api` contract crate; this binary is the single point in the
//! whole product that names `orbit::` (the implementation crate). See
//! `docs/refactoring/orbit.md`.

use std::sync::Arc;

use monoengine_core::{MegaError, ObjectStorageProvider, parse, set_object_storage_provider};
use orbit_api::factory::{MegaObjectStorageWrapper, ObjectStorageConfig};

#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Constructs object storage through the heavy `orbit` implementation crate.
///
/// This is the only `orbit::factory::ObjectStorageFactory::build` call site in
/// the product; every other object-storage interaction goes through the
/// `orbit-api` traits/config carried by `monoengine-core`.
struct OrbitObjectStorageProvider;

#[async_trait::async_trait]
impl ObjectStorageProvider for OrbitObjectStorageProvider {
    async fn build(
        &self,
        cfg: &ObjectStorageConfig,
    ) -> Result<MegaObjectStorageWrapper, MegaError> {
        orbit::factory::ObjectStorageFactory::build(cfg)
            .await
            .map_err(Into::into)
    }
}

fn main() {
    set_object_storage_provider(Arc::new(OrbitObjectStorageProvider));

    let result = parse(None);

    // If there was an error, print it.
    if let Err(e) = result {
        e.print();
        std::process::exit(1);
    }
}
