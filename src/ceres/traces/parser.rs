//! Libra checkpoint layout parsing for published traces history (MTR-14).
//!
//! A traces commit tree keeps each capture at
//! `checkpoint/<id[..2]>/<id[2..]>/` with role files such as
//! `metadata.json` and `manifest.json`. This module reads only raw Git tree
//! and blob payloads that the caller already verified; it never writes and
//! never trusts an identity that metadata or manifest bytes claim on their
//! own. The checkpoint ID comes from the two path components, and the
//! checkpoint tree OID comes from the raw parent tree entry.
//!
//! A checkpoint whose role files are missing or disagree is reported as
//! [`CheckpointParse::Invalid`]; the archived objects stay untouched and
//! remain fetchable. Manifest resource integrity (MTR-51) and schema
//! interpretation (MTR-52) build on these locations.

use crate::{
    ceres::traces::ingest::{RawTreeEntry, RawTreeMode, verified_tree_entries},
    common::errors::MegaError,
    jupiter::service::trace_object_service::RawReadControl,
};

/// Root tree entry holding every checkpoint.
pub(crate) const CHECKPOINT_ROOT: &[u8] = b"checkpoint";
/// Role file names inside one checkpoint tree.
pub(crate) const METADATA_FILE: &[u8] = b"metadata.json";
pub(crate) const MANIFEST_FILE: &[u8] = b"manifest.json";
/// Upper bound on checkpoint subtrees read from one commit tree.
pub(crate) const MAX_CHECKPOINTS: usize = 100_000;
/// Largest checkpoint ID accepted from the layout.
pub(crate) const MAX_CHECKPOINT_ID_BYTES: usize = 256;

/// A checkpoint subtree located from raw tree entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckpointLocation {
    /// `<prefix><rest>` read from the two path components.
    pub(crate) checkpoint_id: String,
    /// OID of the `checkpoint/<prefix>/<rest>` tree.
    pub(crate) tree_oid: String,
}

/// Blob OIDs of the role files present in one checkpoint tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CheckpointRoles {
    pub(crate) metadata: Option<String>,
    pub(crate) manifest: Option<String>,
}

/// Why a located checkpoint could not be parsed. Stable token in
/// [`InvalidCheckpoint::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidCheckpoint {
    /// The two path components do not form a checkpoint ID.
    LayoutKey,
    /// A path component or role file has an unexpected Git mode.
    LayoutMode,
    MissingMetadata,
    MissingManifest,
    MetadataNotObject,
    ManifestNotObject,
    /// metadata.json or manifest.json names another checkpoint.
    IdMismatch,
    /// Transcript parts are not in the manifest's declared order.
    PartOrder,
    /// A declared resource path is not in the checkpoint tree.
    PartPath,
    /// The tree entry OID is not the OID named by the manifest.
    PartOid,
    /// A declared resource is not a Git blob.
    PartType,
    /// A part's payload length is not its manifest `byte_len`.
    PartLength,
    /// The logical resource length is not the manifest total.
    TotalLength,
    /// `content_hash.txt` does not match the covered bytes.
    ContentHash,
}

impl InvalidCheckpoint {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LayoutKey => "layout_key",
            Self::LayoutMode => "layout_mode",
            Self::MissingMetadata => "missing_metadata",
            Self::MissingManifest => "missing_manifest",
            Self::MetadataNotObject => "metadata_not_object",
            Self::ManifestNotObject => "manifest_not_object",
            Self::IdMismatch => "id_mismatch",
            Self::PartOrder => "part_order",
            Self::PartPath => "part_path",
            Self::PartOid => "part_oid",
            Self::PartType => "part_type",
            Self::PartLength => "part_length",
            Self::TotalLength => "total_length",
            Self::ContentHash => "content_hash",
        }
    }
}

/// One parsed checkpoint with the role blobs it was parsed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCheckpoint {
    pub(crate) location: CheckpointLocation,
    pub(crate) metadata_oid: String,
    pub(crate) manifest_oid: String,
}

/// Result for one checkpoint location. `Invalid` keeps the location so the
/// projection can report it; nothing is removed from the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckpointParse {
    Parsed(ParsedCheckpoint),
    Invalid {
        /// Raw path key (`<prefix>/<rest>` with non-UTF-8 bytes hex-escaped).
        key: String,
        tree_oid: String,
        reason: InvalidCheckpoint,
    },
}

impl CheckpointParse {
    pub(crate) fn tree_oid(&self) -> &str {
        match self {
            Self::Parsed(parsed) => &parsed.location.tree_oid,
            Self::Invalid { tree_oid, .. } => tree_oid,
        }
    }
}

/// OID of the `checkpoint` tree in a commit root, when present.
///
/// A non-tree `checkpoint` entry is not a checkpoint root; the commit then
/// carries no checkpoint.
pub(crate) fn checkpoint_root(
    root_tree: &[u8],
    hash_bytes: usize,
    control: &impl RawReadControl,
) -> Result<Option<String>, MegaError> {
    Ok(verified_tree_entries(root_tree, hash_bytes, control)?
        .into_iter()
        .find(|entry| entry.name == CHECKPOINT_ROOT && entry.mode == RawTreeMode::Tree)
        .map(|entry| entry.oid))
}

/// Prefix subtrees of the checkpoint root, in raw tree order. Non-tree
/// entries are returned so the caller can report them as layout errors.
pub(crate) fn checkpoint_children<'a>(
    tree: &'a [u8],
    hash_bytes: usize,
    control: &impl RawReadControl,
) -> Result<Vec<RawTreeEntry<'a>>, MegaError> {
    verified_tree_entries(tree, hash_bytes, control)
}

/// Checkpoint ID for `checkpoint/<prefix>/<rest>`.
///
/// Libra writes `prefix = id[..2]` and `rest = id[2..]`, so the ID is their
/// concatenation. The prefix must be exactly two printable ASCII bytes and
/// the whole ID printable ASCII without `/`, at most
/// [`MAX_CHECKPOINT_ID_BYTES`] bytes.
pub(crate) fn checkpoint_id(prefix: &[u8], rest: &[u8]) -> Result<String, InvalidCheckpoint> {
    let printable = |bytes: &[u8]| {
        bytes
            .iter()
            .all(|byte| byte.is_ascii_graphic() && *byte != b'/' && *byte != b'\\')
    };
    if prefix.len() != 2
        || rest.is_empty()
        || prefix.len() + rest.len() > MAX_CHECKPOINT_ID_BYTES
        || !printable(prefix)
        || !printable(rest)
    {
        return Err(InvalidCheckpoint::LayoutKey);
    }
    let mut id = String::with_capacity(prefix.len() + rest.len());
    id.push_str(std::str::from_utf8(prefix).map_err(|_| InvalidCheckpoint::LayoutKey)?);
    id.push_str(std::str::from_utf8(rest).map_err(|_| InvalidCheckpoint::LayoutKey)?);
    Ok(id)
}

/// Display key for a checkpoint location that is not a valid ID.
pub(crate) fn raw_key(prefix: &[u8], rest: &[u8]) -> String {
    let escape = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('%') => text.to_owned(),
        _ => bytes.iter().map(|byte| format!("%{byte:02x}")).collect(),
    };
    format!("{}/{}", escape(prefix), escape(rest))
}

/// Role file blobs of one checkpoint tree. A role name with a non-blob mode
/// is a layout error.
pub(crate) fn checkpoint_roles(
    tree: &[u8],
    hash_bytes: usize,
    control: &impl RawReadControl,
) -> Result<Result<CheckpointRoles, InvalidCheckpoint>, MegaError> {
    let mut roles = CheckpointRoles::default();
    for entry in verified_tree_entries(tree, hash_bytes, control)? {
        let slot = if entry.name == METADATA_FILE {
            &mut roles.metadata
        } else if entry.name == MANIFEST_FILE {
            &mut roles.manifest
        } else {
            continue;
        };
        if entry.mode != RawTreeMode::Blob {
            return Ok(Err(InvalidCheckpoint::LayoutMode));
        }
        *slot = Some(entry.oid);
    }
    Ok(Ok(roles))
}

/// Parse one located checkpoint from its role blobs.
///
/// metadata.json and manifest.json must be JSON objects. A `checkpoint_id`
/// field, when present, must equal the ID read from the layout.
pub(crate) fn parse_checkpoint(
    location: CheckpointLocation,
    roles: &CheckpointRoles,
    metadata: &[u8],
    manifest: &[u8],
) -> CheckpointParse {
    let invalid = |reason| CheckpointParse::Invalid {
        key: location.checkpoint_id.clone(),
        tree_oid: location.tree_oid.clone(),
        reason,
    };
    let (Some(metadata_oid), Some(manifest_oid)) = (&roles.metadata, &roles.manifest) else {
        return invalid(if roles.metadata.is_none() {
            InvalidCheckpoint::MissingMetadata
        } else {
            InvalidCheckpoint::MissingManifest
        });
    };
    let object = |bytes: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .filter(serde_json::Value::is_object)
    };
    let Some(metadata_value) = object(metadata) else {
        return invalid(InvalidCheckpoint::MetadataNotObject);
    };
    let Some(manifest_value) = object(manifest) else {
        return invalid(InvalidCheckpoint::ManifestNotObject);
    };
    for value in [&metadata_value, &manifest_value] {
        match value.get("checkpoint_id") {
            None => {}
            Some(serde_json::Value::String(claimed)) if *claimed == location.checkpoint_id => {}
            Some(_) => return invalid(InvalidCheckpoint::IdMismatch),
        }
    }
    CheckpointParse::Parsed(ParsedCheckpoint {
        metadata_oid: metadata_oid.clone(),
        manifest_oid: manifest_oid.clone(),
        location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Unbounded;

    impl RawReadControl for Unbounded {
        fn check(&self) -> Result<(), MegaError> {
            Ok(())
        }

        async fn checkpoint(&self) -> Result<(), MegaError> {
            Ok(())
        }

        async fn run<T: Send>(
            &self,
            future: impl std::future::Future<Output = Result<T, MegaError>> + Send,
        ) -> Result<T, MegaError> {
            future.await
        }
    }

    fn tree(entries: &[(&str, &[u8], &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (mode, name, oid) in entries {
            bytes.extend_from_slice(mode.as_bytes());
            bytes.push(b' ');
            bytes.extend_from_slice(name);
            bytes.push(0);
            bytes.extend_from_slice(&hex::decode(oid).unwrap());
        }
        bytes
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn checkpoint_parse_layout_matrix() {
        let control = Unbounded;
        let root = tree(&[
            ("100644", b"README", A),
            ("40000", b"checkpoint", B),
            ("40000", b"checkpoints", C),
        ]);
        assert_eq!(
            checkpoint_root(&root, 20, &control).unwrap().as_deref(),
            Some(B)
        );
        let blob_root = tree(&[("100644", b"checkpoint", B)]);
        assert_eq!(checkpoint_root(&blob_root, 20, &control).unwrap(), None);
        assert_eq!(
            checkpoint_root(&tree(&[("100644", b"x", A)]), 20, &control).unwrap(),
            None
        );

        let id = "1c6cc65e-f8bd-409a-8f0e-69fda086a5d0";
        assert_eq!(checkpoint_id(b"1c", &id.as_bytes()[2..]).unwrap(), id);
        assert_eq!(
            checkpoint_id(b"a", b"bcd"),
            Err(InvalidCheckpoint::LayoutKey)
        );
        assert_eq!(checkpoint_id(b"ab", b""), Err(InvalidCheckpoint::LayoutKey));
        assert_eq!(
            checkpoint_id(b"ab", b"c d"),
            Err(InvalidCheckpoint::LayoutKey)
        );
        assert_eq!(
            checkpoint_id(b"\xffa", b"cd"),
            Err(InvalidCheckpoint::LayoutKey)
        );
        assert_eq!(
            checkpoint_id(b"ab", &[b'c'; MAX_CHECKPOINT_ID_BYTES]),
            Err(InvalidCheckpoint::LayoutKey)
        );
        assert_eq!(raw_key(b"\xffa", b"cd"), "%ff%61/cd");
        assert_eq!(raw_key(b"ab", b"cd"), "ab/cd");

        let leaf = tree(&[
            ("100644", b"content_hash.txt", A),
            ("100644", b"manifest.json", B),
            ("100644", b"metadata.json", C),
            ("40000", b"transcript", A),
        ]);
        let roles = checkpoint_roles(&leaf, 20, &control).unwrap().unwrap();
        assert_eq!(roles.metadata.as_deref(), Some(C));
        assert_eq!(roles.manifest.as_deref(), Some(B));
        let bad_mode = tree(&[("40000", b"metadata.json", C)]);
        assert_eq!(
            checkpoint_roles(&bad_mode, 20, &control).unwrap(),
            Err(InvalidCheckpoint::LayoutMode)
        );
        assert!(
            checkpoint_roles(b"not a tree", 20, &control).is_err(),
            "a malformed raw tree is an integrity error"
        );
    }

    #[test]
    fn checkpoint_parse_role_matrix() {
        let location = CheckpointLocation {
            checkpoint_id: "abcd".to_owned(),
            tree_oid: A.to_owned(),
        };
        let roles = CheckpointRoles {
            metadata: Some(B.to_owned()),
            manifest: Some(C.to_owned()),
        };
        let parsed = parse_checkpoint(
            location.clone(),
            &roles,
            br#"{"checkpoint_id":"abcd","schema_version":2}"#,
            br#"{"checkpoint_id":"abcd","schema_version":1}"#,
        );
        assert_eq!(
            parsed,
            CheckpointParse::Parsed(ParsedCheckpoint {
                location: location.clone(),
                metadata_oid: B.to_owned(),
                manifest_oid: C.to_owned(),
            })
        );
        assert!(matches!(
            parse_checkpoint(location.clone(), &roles, b"{}", b"{}"),
            CheckpointParse::Parsed(_)
        ));
        for (metadata, manifest, reason) in [
            (&b"[]"[..], &b"{}"[..], InvalidCheckpoint::MetadataNotObject),
            (b"{", b"{}", InvalidCheckpoint::MetadataNotObject),
            (b"{}", b"\xff", InvalidCheckpoint::ManifestNotObject),
            (
                br#"{"checkpoint_id":"other"}"#,
                b"{}",
                InvalidCheckpoint::IdMismatch,
            ),
            (
                b"{}",
                br#"{"checkpoint_id":7}"#,
                InvalidCheckpoint::IdMismatch,
            ),
        ] {
            assert_eq!(
                parse_checkpoint(location.clone(), &roles, metadata, manifest),
                CheckpointParse::Invalid {
                    key: "abcd".to_owned(),
                    tree_oid: A.to_owned(),
                    reason,
                }
            );
        }
        let missing = CheckpointRoles {
            metadata: None,
            manifest: Some(C.to_owned()),
        };
        let result = parse_checkpoint(location.clone(), &missing, b"", b"{}");
        assert_eq!(result.tree_oid(), A);
        assert!(matches!(
            result,
            CheckpointParse::Invalid {
                reason: InvalidCheckpoint::MissingMetadata,
                ..
            }
        ));
        let no_manifest = CheckpointRoles {
            metadata: Some(B.to_owned()),
            manifest: None,
        };
        assert!(matches!(
            parse_checkpoint(location, &no_manifest, b"{}", b""),
            CheckpointParse::Invalid {
                reason: InvalidCheckpoint::MissingManifest,
                ..
            }
        ));
        assert_eq!(InvalidCheckpoint::IdMismatch.as_str(), "id_mismatch");
    }
}
