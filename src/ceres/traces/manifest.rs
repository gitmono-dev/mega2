//! Manifest resource integrity for one published checkpoint (MTR-51).
//!
//! MTR-14 has already accepted the checkpoint layout and the role blobs.
//! This module checks the resources that `manifest.json` declares: part
//! order, path, Git type, OID, `byte_len`, the logical total, and the
//! content hash. A failure marks that checkpoint invalid. The archived
//! objects are not removed. Schema support is MTR-52; a manifest with no
//! `entries` object stays a layout-only checkpoint.

use sha2::{Digest, Sha256};

use crate::{
    ceres::traces::{
        ingest::{RawTreeMode, verified_tree_entries},
        parser::InvalidCheckpoint,
    },
    common::errors::MegaError,
    jupiter::service::trace_object_service::{RawObjectType, RawReadControl},
};

/// Roles covered by `content_hash.txt`, in hash order. The transcript role
/// contributes the logical byte stream, not a particular chunking.
const CONTENT_HASH_COVERAGE: [&str; 4] = [
    "metadata",
    "lifecycle_events",
    "transcript",
    "redaction_report",
];

/// Read one verified object. A missing or wrong-typed object is an archive
/// integrity error, not a manifest mismatch: the caller already published it.
pub(crate) trait ManifestStore {
    fn load<'a>(
        &'a self,
        oid: &'a str,
        kind: RawObjectType,
    ) -> impl Future<Output = Result<Vec<u8>, MegaError>> + Send;
}

/// Check declared resources in `manifest`.
///
/// `Ok(Ok(()))` means the declaration holds, or the manifest does not declare
/// `entries`. `Ok(Err(_))` is a resource mismatch and the caller marks the
/// checkpoint invalid. `Err` is an unreadable archive or a deadline.
pub(crate) async fn verify_declared_resources(
    manifest: &[u8],
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<(), InvalidCheckpoint>, MegaError> {
    control.check()?;
    let value: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(manifest) {
        Ok(value) if value.is_object() => value,
        _ => return Ok(Err(InvalidCheckpoint::ContentHash)),
    };
    let Some(entries) = value.get("entries") else {
        return Ok(Ok(()));
    };
    let Some(entries) = entries.as_object() else {
        return Ok(Err(InvalidCheckpoint::PartPath));
    };
    if entries.is_empty() {
        return Ok(Err(InvalidCheckpoint::PartPath));
    }

    let mut covered = std::collections::HashMap::<String, Vec<u8>>::new();
    for role in CONTENT_HASH_COVERAGE {
        let Some(entry) = entries.get(role) else {
            return Ok(Err(InvalidCheckpoint::ContentHash));
        };
        let bytes =
            match role_bytes(role, entry, checkpoint_tree, hash_bytes, control, store).await? {
                Ok(bytes) => bytes,
                Err(reason) => return Ok(Err(reason)),
            };
        covered.insert(role.to_owned(), bytes);
    }
    for (role, entry) in entries {
        if CONTENT_HASH_COVERAGE.contains(&role.as_str()) {
            continue;
        }
        if let Err(reason) =
            role_bytes(role, entry, checkpoint_tree, hash_bytes, control, store).await?
        {
            return Ok(Err(reason));
        }
    }
    match content_hash(
        &value,
        &covered,
        checkpoint_tree,
        hash_bytes,
        control,
        store,
    )
    .await?
    {
        Ok(()) => {}
        Err(reason) => return Ok(Err(reason)),
    }
    reasoning_artifacts(&value, checkpoint_tree, hash_bytes, control, store).await
}

fn covered_digest(sections: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for section in sections {
        hasher.update(section);
    }
    hex::encode(hasher.finalize())
}

async fn role_bytes(
    role: &str,
    entry: &serde_json::Value,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<Vec<u8>, InvalidCheckpoint>, MegaError> {
    let Some(entry) = entry.as_object() else {
        return Ok(Err(InvalidCheckpoint::PartPath));
    };
    if role == "transcript" {
        return transcript_bytes(entry, checkpoint_tree, hash_bytes, control, store).await;
    }
    if entry.contains_key("parts")
        || entry.get("chunked").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return Ok(Err(InvalidCheckpoint::PartOrder));
    }
    one_blob(entry, checkpoint_tree, hash_bytes, control, store).await
}

async fn transcript_bytes(
    entry: &serde_json::Map<String, serde_json::Value>,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<Vec<u8>, InvalidCheckpoint>, MegaError> {
    let chunked = match entry.get("chunked") {
        None => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(_) => return Ok(Err(InvalidCheckpoint::PartOrder)),
    };
    let has_oid = entry.contains_key("oid");
    let parts = entry.get("parts");
    if chunked {
        if has_oid || parts.is_none() {
            return Ok(Err(InvalidCheckpoint::PartOrder));
        }
        let Some(parts) = parts.and_then(serde_json::Value::as_array) else {
            return Ok(Err(InvalidCheckpoint::PartOrder));
        };
        if parts.len() < 2 {
            return Ok(Err(InvalidCheckpoint::PartOrder));
        }
        let mut logical = Vec::new();
        let mut declared_total = 0u64;
        for (index, part) in parts.iter().enumerate() {
            let Some(part) = part.as_object() else {
                return Ok(Err(InvalidCheckpoint::PartPath));
            };
            let Some(name) = part.get("path").and_then(serde_json::Value::as_str) else {
                return Ok(Err(InvalidCheckpoint::PartPath));
            };
            let suffix = name
                .rsplit('/')
                .next()
                .and_then(|file| file.rsplit_once('.'));
            let Some((_, digits)) = suffix else {
                return Ok(Err(InvalidCheckpoint::PartOrder));
            };
            if digits.len() != 3
                || !digits.bytes().all(|byte| byte.is_ascii_digit())
                || digits.parse::<usize>().ok() != Some(index + 1)
            {
                return Ok(Err(InvalidCheckpoint::PartOrder));
            }
            let blob = match one_blob(part, checkpoint_tree, hash_bytes, control, store).await? {
                Ok(blob) => blob,
                Err(reason) => return Ok(Err(reason)),
            };
            declared_total = match declared_total.checked_add(blob.len() as u64) {
                Some(total) => total,
                None => return Ok(Err(InvalidCheckpoint::TotalLength)),
            };
            logical.extend_from_slice(&blob);
        }
        let Some(total) = json_u64(entry.get("byte_len")) else {
            return Ok(Err(InvalidCheckpoint::TotalLength));
        };
        if total != declared_total || logical.len() as u64 != total {
            return Ok(Err(InvalidCheckpoint::TotalLength));
        }
        return Ok(Ok(logical));
    }
    if parts.is_some() || !has_oid {
        return Ok(Err(InvalidCheckpoint::PartOrder));
    }
    one_blob(entry, checkpoint_tree, hash_bytes, control, store).await
}

async fn one_blob(
    entry: &serde_json::Map<String, serde_json::Value>,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<Vec<u8>, InvalidCheckpoint>, MegaError> {
    let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
        return Ok(Err(InvalidCheckpoint::PartPath));
    };
    let Some(declared_oid) = json_oid(entry.get("oid"), hash_bytes) else {
        return Ok(Err(InvalidCheckpoint::PartOid));
    };
    let Some(declared_len) = json_u64(entry.get("byte_len")) else {
        return Ok(Err(InvalidCheckpoint::PartLength));
    };
    let resolved = match resolve_blob(checkpoint_tree, path, hash_bytes, control, store).await? {
        Ok(resolved) => resolved,
        Err(reason) => return Ok(Err(reason)),
    };
    if resolved.oid != declared_oid {
        return Ok(Err(InvalidCheckpoint::PartOid));
    }
    if resolved.bytes.len() as u64 != declared_len {
        return Ok(Err(InvalidCheckpoint::PartLength));
    }
    Ok(Ok(resolved.bytes))
}

struct ResolvedBlob {
    oid: String,
    bytes: Vec<u8>,
}

async fn resolve_blob(
    root: &str,
    path: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<ResolvedBlob, InvalidCheckpoint>, MegaError> {
    let components = match path_components(path) {
        Some(components) => components,
        None => return Ok(Err(InvalidCheckpoint::PartPath)),
    };
    let mut oid = root.to_owned();
    for (index, component) in components.iter().enumerate() {
        control.checkpoint().await?;
        let tree = store.load(&oid, RawObjectType::Tree).await?;
        let entries = verified_tree_entries(&tree, hash_bytes, control)?;
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.name == component.as_bytes())
        else {
            return Ok(Err(InvalidCheckpoint::PartPath));
        };
        let last = index + 1 == components.len();
        if last {
            if entry.mode != RawTreeMode::Blob {
                return Ok(Err(InvalidCheckpoint::PartType));
            }
            let oid = entry.oid.clone();
            let bytes = store.load(&oid, RawObjectType::Blob).await?;
            return Ok(Ok(ResolvedBlob { oid, bytes }));
        }
        if entry.mode != RawTreeMode::Tree {
            return Ok(Err(InvalidCheckpoint::PartType));
        }
        oid = entry.oid.clone();
    }
    Ok(Err(InvalidCheckpoint::PartPath))
}

fn path_components(path: &str) -> Option<Vec<&str>> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        components.push(component);
    }
    (!components.is_empty()).then_some(components)
}

async fn content_hash(
    manifest: &serde_json::Value,
    covered: &std::collections::HashMap<String, Vec<u8>>,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<(), InvalidCheckpoint>, MegaError> {
    let Some(descriptor) = manifest
        .get("content_hash")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    };
    if descriptor
        .get("algorithm")
        .and_then(serde_json::Value::as_str)
        != Some("sha256")
    {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    }
    if descriptor.get("path").and_then(serde_json::Value::as_str) != Some("content_hash.txt") {
        return Ok(Err(InvalidCheckpoint::PartPath));
    }
    let Some(coverage) = descriptor
        .get("coverage")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    };
    let coverage_ok = coverage.len() == CONTENT_HASH_COVERAGE.len()
        && coverage
            .iter()
            .zip(CONTENT_HASH_COVERAGE)
            .all(|(value, role)| value.as_str() == Some(role));
    if !coverage_ok {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    }
    let sections: Vec<&[u8]> = CONTENT_HASH_COVERAGE
        .iter()
        .filter_map(|role| covered.get(*role).map(Vec::as_slice))
        .collect();
    if sections.len() != CONTENT_HASH_COVERAGE.len() {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    }
    let expected = covered_digest(&sections);
    let file = match covered_file(manifest, checkpoint_tree, hash_bytes, control, store).await? {
        Ok(file) => file,
        Err(reason) => return Ok(Err(reason)),
    };
    let actual = match std::str::from_utf8(&file) {
        Ok(text) => parse_content_hash(text),
        Err(_) => None,
    };
    if actual.as_deref() != Some(expected.as_str()) {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    }
    Ok(Ok(()))
}

async fn covered_file(
    manifest: &serde_json::Value,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<Vec<u8>, InvalidCheckpoint>, MegaError> {
    let Some(entry) = manifest
        .pointer("/entries/content_hash")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Err(InvalidCheckpoint::ContentHash));
    };
    if entry.get("path").and_then(serde_json::Value::as_str) != Some("content_hash.txt") {
        return Ok(Err(InvalidCheckpoint::PartPath));
    }
    one_blob(entry, checkpoint_tree, hash_bytes, control, store).await
}

fn parse_content_hash(text: &str) -> Option<String> {
    let hex = text.trim().strip_prefix("sha256:").unwrap_or(text.trim());
    let normalized = hex.to_ascii_lowercase();
    (normalized.len() == 64 && normalized.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(normalized)
}

async fn reasoning_artifacts(
    manifest: &serde_json::Value,
    checkpoint_tree: &str,
    hash_bytes: usize,
    control: &impl RawReadControl,
    store: &impl ManifestStore,
) -> Result<Result<(), InvalidCheckpoint>, MegaError> {
    let Some(artifacts) = manifest.get("reasoning_artifacts") else {
        return Ok(Ok(()));
    };
    let Some(artifacts) = artifacts.as_array() else {
        return Ok(Err(InvalidCheckpoint::PartPath));
    };
    for artifact in artifacts {
        let Some(artifact) = artifact.as_object() else {
            return Ok(Err(InvalidCheckpoint::PartPath));
        };
        let Some(expected) = artifact.get("sha256").and_then(serde_json::Value::as_str) else {
            return Ok(Err(InvalidCheckpoint::ContentHash));
        };
        let expected = expected.to_ascii_lowercase();
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(Err(InvalidCheckpoint::ContentHash));
        }
        let bytes = match one_blob(artifact, checkpoint_tree, hash_bytes, control, store).await? {
            Ok(bytes) => bytes,
            Err(reason) => return Ok(Err(reason)),
        };
        if covered_digest(&[&bytes]) != expected {
            return Ok(Err(InvalidCheckpoint::ContentHash));
        }
    }
    Ok(Ok(()))
}

fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(serde_json::Value::as_u64)
}

fn json_oid(value: Option<&serde_json::Value>, hash_bytes: usize) -> Option<String> {
    let text = value.and_then(serde_json::Value::as_str)?;
    if text.len() != hash_bytes * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, future::Future};

    use super::*;

    struct Store {
        objects: HashMap<String, (RawObjectType, Vec<u8>)>,
    }

    impl ManifestStore for Store {
        async fn load(&self, oid: &str, kind: RawObjectType) -> Result<Vec<u8>, MegaError> {
            match self.objects.get(oid) {
                Some((got, bytes))
                    if std::mem::discriminant(got) == std::mem::discriminant(&kind) =>
                {
                    Ok(bytes.clone())
                }
                _ => Err(crate::common::errors::TracesError::ObjectIntegrity.into()),
            }
        }
    }

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
            future: impl Future<Output = Result<T, MegaError>> + Send,
        ) -> Result<T, MegaError> {
            future.await
        }
    }

    fn oid(byte: u8) -> String {
        hex::encode([byte; 20])
    }

    fn tree(entries: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| {
            let key = |entry: &(&str, &str, &str)| {
                let mut name = entry.1.as_bytes().to_vec();
                name.push(if entry.0 == "40000" { b'/' } else { 0 });
                name
            };
            key(left).cmp(&key(right))
        });
        let mut bytes = Vec::new();
        for (mode, name, entry_oid) in entries {
            bytes.extend_from_slice(mode.as_bytes());
            bytes.push(b' ');
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&hex::decode(entry_oid).unwrap());
        }
        bytes
    }

    struct Sample {
        store: Store,
        root: String,
        manifest: serde_json::Value,
    }

    fn sample() -> Sample {
        let metadata = b"{\"agent_kind\":\"claude_code\"}";
        let events = b"{}\n";
        let part_a = b"alpha\n";
        let part_b = b"beta\n";
        let report = b"{}";
        let mut logical = part_a.to_vec();
        logical.extend_from_slice(part_b);
        let digest = covered_digest(&[metadata, events, &logical, report]);
        let hash_file = format!("sha256:{digest}");
        let ids = [
            oid(1),
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            oid(6),
            oid(7),
            oid(8),
        ];
        let events_tree = tree(&[("100644", "lifecycle.jsonl", &ids[1])]);
        let transcript_tree = tree(&[
            ("100644", "claude_code.jsonl.001", &ids[2]),
            ("100644", "claude_code.jsonl.002", &ids[3]),
        ]);
        let root_id = oid(9);
        let root = tree(&[
            ("100644", "content_hash.txt", &ids[5]),
            ("40000", "events", &ids[6]),
            ("100644", "metadata.json", &ids[0]),
            ("100644", "redaction_report.json", &ids[4]),
            ("40000", "transcript", &ids[7]),
        ]);
        let mut objects = HashMap::new();
        for (id, bytes) in [
            (&ids[0], metadata.as_slice()),
            (&ids[1], events.as_slice()),
            (&ids[2], part_a.as_slice()),
            (&ids[3], part_b.as_slice()),
            (&ids[4], report.as_slice()),
            (&ids[5], hash_file.as_bytes()),
        ] {
            objects.insert(id.clone(), (RawObjectType::Blob, bytes.to_vec()));
        }
        objects.insert(ids[6].clone(), (RawObjectType::Tree, events_tree));
        objects.insert(ids[7].clone(), (RawObjectType::Tree, transcript_tree));
        objects.insert(root_id.clone(), (RawObjectType::Tree, root));
        let manifest = serde_json::json!({
            "schema_version": 1,
            "content_hash": {
                "algorithm": "sha256",
                "coverage": ["metadata", "lifecycle_events", "transcript", "redaction_report"],
                "path": "content_hash.txt"
            },
            "entries": {
                "metadata": blob_entry("metadata.json", &ids[0], metadata.len()),
                "lifecycle_events": blob_entry("events/lifecycle.jsonl", &ids[1], events.len()),
                "transcript": {
                    "path": "transcript/claude_code.jsonl",
                    "byte_len": part_a.len() + part_b.len(),
                    "chunked": true,
                    "parts": [
                        part_entry("transcript/claude_code.jsonl.001", &ids[2], part_a.len()),
                        part_entry("transcript/claude_code.jsonl.002", &ids[3], part_b.len())
                    ]
                },
                "redaction_report": blob_entry("redaction_report.json", &ids[4], report.len()),
                "content_hash": blob_entry("content_hash.txt", &ids[5], hash_file.len())
            }
        });
        Sample {
            store: Store { objects },
            root: root_id,
            manifest,
        }
    }

    fn blob_entry(path: &str, oid: &str, len: usize) -> serde_json::Value {
        serde_json::json!({"path": path, "oid": oid, "byte_len": len})
    }

    fn part_entry(path: &str, oid: &str, len: usize) -> serde_json::Value {
        blob_entry(path, oid, len)
    }

    async fn check(sample: &Sample) -> Result<(), InvalidCheckpoint> {
        verify_declared_resources(
            &serde_json::to_vec(&sample.manifest).unwrap(),
            &sample.root,
            20,
            &Unbounded,
            &sample.store,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn resource_integrity_accepts_ordered_parts_and_hash() {
        let sample = sample();
        assert_eq!(check(&sample).await, Ok(()));
        let undeclared = serde_json::json!({"checkpoint_id": "abcd"});
        assert_eq!(
            verify_declared_resources(
                &serde_json::to_vec(&undeclared).unwrap(),
                &sample.root,
                20,
                &Unbounded,
                &sample.store,
            )
            .await
            .unwrap(),
            Ok(())
        );
    }

    #[tokio::test]
    async fn resource_integrity_part_order() {
        let mut sample = sample();
        let parts = sample.manifest["entries"]["transcript"]["parts"]
            .as_array_mut()
            .unwrap();
        parts.swap(0, 1);
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartOrder));
    }

    #[tokio::test]
    async fn resource_integrity_part_path() {
        let mut sample = sample();
        sample.manifest["entries"]["transcript"]["parts"][0]["path"] =
            serde_json::json!("transcript/missing.jsonl.001");
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartPath));
    }

    #[tokio::test]
    async fn resource_integrity_part_oid() {
        let mut sample = sample();
        sample.manifest["entries"]["metadata"]["oid"] = serde_json::json!(oid(0x11));
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartOid));
    }

    #[tokio::test]
    async fn resource_integrity_part_type() {
        let mut sample = sample();
        let transcript = tree(&[
            ("40000", "claude_code.jsonl.001", &oid(7)),
            ("100644", "claude_code.jsonl.002", &oid(4)),
        ]);
        sample
            .store
            .objects
            .insert(oid(8), (RawObjectType::Tree, transcript));
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartType));
    }

    #[tokio::test]
    async fn resource_integrity_part_length() {
        let mut sample = sample();
        sample.manifest["entries"]["metadata"]["byte_len"] = serde_json::json!(1);
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartLength));
    }

    #[tokio::test]
    async fn resource_integrity_total_length() {
        let mut sample = sample();
        let total = sample.manifest["entries"]["transcript"]["byte_len"]
            .as_u64()
            .unwrap();
        sample.manifest["entries"]["transcript"]["byte_len"] = serde_json::json!(total + 1);
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::TotalLength));
    }

    #[tokio::test]
    async fn resource_integrity_content_hash() {
        let mut sample = sample();
        let file_oid = sample.manifest["entries"]["content_hash"]["oid"]
            .as_str()
            .unwrap()
            .to_owned();
        sample.store.objects.get_mut(&file_oid).unwrap().1 =
            b"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec();
        sample.manifest["entries"]["content_hash"]["byte_len"] = serde_json::json!(71);
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::ContentHash));
    }

    #[tokio::test]
    async fn resource_integrity_failure_marks_invalid_without_a_schema_gate() {
        let mut sample = sample();
        sample.manifest["schema_version"] = serde_json::json!(99);
        assert_eq!(
            check(&sample).await,
            Ok(()),
            "an unknown schema stays verifiable; MTR-52 owns unsupported"
        );
        sample.manifest["entries"]["redaction_report"]["path"] = serde_json::json!("no/such.json");
        assert_eq!(check(&sample).await, Err(InvalidCheckpoint::PartPath));
        assert_eq!(InvalidCheckpoint::ContentHash.as_str(), "content_hash");
    }
}
