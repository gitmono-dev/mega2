//! UN-39: baseline pointer codec and version-file naming.
//!
//! The write primitives live in [`secure_artifact`] (UN-32). This module freezes
//! the pointer JSON shape and the content-addressed version-file rules that
//! promotion (UN-35) and recovery (UN-40) will call.

use serde::{Deserialize, Serialize};

use crate::contract::policy::{
    secure_artifact::{
        ArtifactError, ArtifactResult, BASELINES_DIR, POINTER_NAME, RestrictedRoot,
        read_baseline_file, replace_pointer, write_baseline_version,
    },
    secure_sweep::MaintenanceLock,
};

/// Frozen pointer schema version.
pub const POINTER_SCHEMA_VERSION: u32 = 1;

/// Canonical baseline pointer document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselinePointer {
    pub schema_version: u32,
    pub digest: String,
}

impl BaselinePointer {
    pub fn new(digest: impl Into<String>) -> ArtifactResult<Self> {
        let digest = digest.into();
        validate_artifact_digest(&digest)?;
        Ok(Self {
            schema_version: POINTER_SCHEMA_VERSION,
            digest,
        })
    }
}

/// Canonical pointer bytes: compact JSON, fixed key order, no trailing newline.
pub fn encode_pointer(pointer: &BaselinePointer) -> ArtifactResult<Vec<u8>> {
    if pointer.schema_version != POINTER_SCHEMA_VERSION {
        return Err(ArtifactError::PointerInvalid {
            reason: format!(
                "unsupported schema_version {}; only {POINTER_SCHEMA_VERSION} is accepted",
                pointer.schema_version
            ),
        });
    }
    validate_artifact_digest(&pointer.digest)?;
    // Manual encode keeps key order and spacing under this card's control.
    Ok(format!(
        r#"{{"schema_version":{},"digest":"{}"}}"#,
        pointer.schema_version, pointer.digest
    )
    .into_bytes())
}

/// Strict parse of pointer bytes.
pub fn parse_pointer(bytes: &[u8]) -> ArtifactResult<BaselinePointer> {
    let pointer: BaselinePointer =
        serde_json::from_slice(bytes).map_err(|source| ArtifactError::PointerInvalid {
            reason: format!("pointer JSON rejected: {source}"),
        })?;
    if pointer.schema_version != POINTER_SCHEMA_VERSION {
        return Err(ArtifactError::PointerInvalid {
            reason: format!(
                "unsupported schema_version {}; only {POINTER_SCHEMA_VERSION} is accepted",
                pointer.schema_version
            ),
        });
    }
    validate_artifact_digest(&pointer.digest)?;
    Ok(pointer)
}

/// `sha256:` + 64 lowercase hex digits.
pub fn validate_artifact_digest(digest: &str) -> ArtifactResult<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(ArtifactError::PointerInvalid {
            reason: "digest must use the `sha256:` prefix".into(),
        });
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ArtifactError::PointerInvalid {
            reason: "digest must be `sha256:` followed by 64 lowercase hex characters".into(),
        });
    }
    Ok(())
}

/// `sha256:<64hex>` → `<64hex>.json`.
pub fn version_file_name(digest: &str) -> ArtifactResult<String> {
    validate_artifact_digest(digest)?;
    let hex = digest.strip_prefix("sha256:").expect("validated");
    Ok(format!("{hex}.json"))
}

/// Read and strictly parse `baselines/current.json`, or `None` if absent.
///
/// Symlinks and non-regular files fail closed via the UN-32 read path.
pub fn read_current_pointer(root: &RestrictedRoot) -> ArtifactResult<Option<BaselinePointer>> {
    let Some(bytes) = crate::contract::policy::secure_artifact::read_pointer(root)? else {
        return Ok(None);
    };
    Ok(Some(parse_pointer(&bytes)?))
}

/// Replace the pointer under the held maintenance lock (temp + atomic rename).
pub fn write_current_pointer(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    digest: &str,
) -> ArtifactResult<()> {
    lock.assert_guards(root)?;
    let pointer = BaselinePointer::new(digest)?;
    let bytes = encode_pointer(&pointer)?;
    replace_pointer(root, &bytes)
}

/// Read an immutable version file by digest, or `None` if absent.
pub fn read_version_for_digest(
    root: &RestrictedRoot,
    digest: &str,
) -> ArtifactResult<Option<Vec<u8>>> {
    let name = version_file_name(digest)?;
    read_baseline_file(root, &name)
}

/// Write an immutable version file named for `digest`.
///
/// Same digest with identical bytes is idempotent; differing bytes fail closed.
pub fn write_version_for_digest(
    root: &RestrictedRoot,
    digest: &str,
    contents: &[u8],
) -> ArtifactResult<()> {
    let name = version_file_name(digest)?;
    write_baseline_version(root, &name, contents)
}

/// Convenience: path display fragment for errors / tests.
pub fn pointer_path_fragment() -> &'static str {
    POINTER_NAME
}

/// Directory fragment holding pointer and version files.
pub fn baselines_dir_fragment() -> &'static str {
    BASELINES_DIR
}
