//! ServingDescriptor construction via the shared `mst2-codec` (spec 03).

use mst2_codec::descriptor::ServingDescriptor;
use uuid::Uuid;

use crate::{
    ceres::snapshot::{
        error::{SnapshotError, SnapshotErrorCode},
        view::SnapshotView,
    },
    config::Mst2Config,
};

/// A codec-built descriptor plus its derived snapshot id, ready to serve.
#[derive(Debug, Clone)]
pub struct BuiltDescriptor {
    /// Canonical byte-level descriptor (snapshot_id is derived from these).
    pub descriptor: ServingDescriptor,
    /// JSON form per spec 03 §2 (instance_id as UUID string).
    pub instance_id: String,
    pub snapshot_id: String,
    pub metadata_root: String,
}

/// Build the descriptor for `view` limited to `scope` (which must be a
/// directory of the view). `metadata_root` is the MTP2 page id of the scope
/// root directory (built from the fixed tree by the caller).
pub fn build(
    cfg: &Mst2Config,
    view: &SnapshotView,
    scope: &str,
    metadata_root: [u8; 32],
) -> Result<BuiltDescriptor, SnapshotError> {
    let instance_str = cfg.instance_uuid.as_deref().ok_or_else(|| {
        SnapshotError::new(
            SnapshotErrorCode::Internal,
            "[mst2].instance_uuid is required when mst2 is enabled",
        )
    })?;
    let uuid = Uuid::parse_str(instance_str).map_err(|e| {
        SnapshotError::new(
            SnapshotErrorCode::Internal,
            format!("[mst2].instance_uuid is not a valid UUID: {e}"),
        )
    })?;
    // view_id "sha256:<hex>" -> 32 bytes for the descriptor digest field.
    let view_hex = view.view_id.strip_prefix("sha256:").ok_or_else(|| {
        SnapshotError::new(SnapshotErrorCode::Internal, "view_id must be sha256:<hex>")
    })?;
    let mut view_digest = [0u8; 32];
    for i in 0..32 {
        view_digest[i] = u8::from_str_radix(&view_hex[2 * i..2 * i + 2], 16).map_err(|e| {
            SnapshotError::new(SnapshotErrorCode::Internal, format!("view_id hex: {e}"))
        })?;
    }
    let descriptor = ServingDescriptor {
        instance_uuid: *uuid.as_bytes(),
        namespace_view_id: view_digest,
        scope: scope.to_string(),
        metadata_root,
    };
    let snapshot_id = descriptor.snapshot_id().map_err(|e| {
        SnapshotError::new(
            SnapshotErrorCode::Internal,
            format!("descriptor encode failed: {e}"),
        )
    })?;
    Ok(BuiltDescriptor {
        descriptor,
        instance_id: instance_str.to_string(),
        snapshot_id: format!("sha256:{}", crate::ceres::snapshot::view::hex(&snapshot_id)),
        metadata_root: format!(
            "sha256:{}",
            crate::ceres::snapshot::view::hex(&metadata_root)
        ),
    })
}
