//! Quarantine unpack for one traces ingest. Objects land under the staging
//! prefix until closure and OID checks pass.

use std::collections::HashSet;

use crate::{
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
    jupiter::storage::{
        object_storage::MegaObjectStorageWrapper,
        trace_object_storage::{self, get_raw},
    },
};

/// One inflated loose object. Delta entries are not a quarantine object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooseObject {
    pub object_type: String,
    pub declared_oid: String,
    pub payload: Vec<u8>,
}

/// Running unpack counters. They are the bytes already seen in this attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnpackBudget {
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
}

fn budget_exceeded() -> MegaError {
    MegaError::Traces(TracesError::ResponseBudget)
}

fn rejected() -> MegaError {
    MegaError::Traces(TracesError::InvalidParameter)
}

fn admit(budget: UnpackBudget) -> Result<(), MegaError> {
    if budget.compressed_bytes > TracesProtocolLimits::MAX_PACK_BYTES
        || budget.expanded_bytes > TracesProtocolLimits::MAX_DECODED_BYTES
    {
        return Err(budget_exceeded());
    }
    Ok(())
}

/// Verify OIDs and write the objects to ingest quarantine.
///
/// A budget overrun or a bad OID writes nothing further and does not publish.
pub async fn unpack_into_quarantine(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    mut budget: UnpackBudget,
    objects: &[LooseObject],
) -> Result<Vec<String>, MegaError> {
    admit(budget)?;
    let mut staged = Vec::with_capacity(objects.len());
    for object in objects {
        if is_delta(&object.object_type) {
            return Err(rejected());
        }
        let next = budget
            .expanded_bytes
            .checked_add(object.payload.len() as u64)
            .ok_or_else(budget_exceeded)?;
        budget.expanded_bytes = next;
        admit(budget)?;
        let oid = trace_object_storage::verify_declared_oid(
            hash_kind,
            &object.object_type,
            &object.payload,
            &object.declared_oid,
        )?;
        trace_object_storage::put_staging(store, ingest_id, hash_kind, &oid, &object.payload)
            .await?;
        staged.push(oid);
    }
    Ok(staged)
}

fn is_delta(object_type: &str) -> bool {
    matches!(
        object_type,
        "ref-delta" | "ofs-delta" | "hash-delta" | "offset-delta"
    )
}

/// Every OID that will become visible must already have bytes on this stream.
pub async fn require_durable_oids(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    oids: &[String],
) -> Result<(), MegaError> {
    for oid in oids {
        if get_raw(store, stream_id, hash_kind, oid).await?.is_none() {
            return Err(rejected());
        }
    }
    Ok(())
}

/// Every referenced OID must already be in this unpack or this stream.
/// Another stream, and the `git` namespace, are not bases.
pub async fn require_closure(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    objects: &[LooseObject],
) -> Result<(), MegaError> {
    let mut have = HashSet::new();
    for object in objects {
        have.insert(trace_object_storage::verify_declared_oid(
            hash_kind,
            &object.object_type,
            &object.payload,
            &object.declared_oid,
        )?);
    }
    for object in objects {
        for need in referenced_oids(hash_kind, &object.object_type, &object.payload)? {
            if have.contains(&need) {
                continue;
            }
            if get_raw(store, stream_id, hash_kind, &need).await?.is_some() {
                have.insert(need);
                continue;
            }
            return Err(rejected());
        }
    }
    Ok(())
}

fn referenced_oids(
    hash_kind: &str,
    object_type: &str,
    payload: &[u8],
) -> Result<Vec<String>, MegaError> {
    let oid_hex_len = match hash_kind {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(rejected()),
    };
    let hash_len = oid_hex_len / 2;
    match object_type {
        "blob" => Ok(Vec::new()),
        "commit" => header_oids(payload, &["tree", "parent"], oid_hex_len),
        "tag" => header_oids(payload, &["object"], oid_hex_len),
        "tree" => tree_oids(payload, hash_len),
        _ => Err(rejected()),
    }
}

fn header_oids(
    payload: &[u8],
    labels: &[&str],
    oid_hex_len: usize,
) -> Result<Vec<String>, MegaError> {
    let header_end = payload
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap_or(payload.len());
    let mut refs = Vec::new();
    for line in payload[..header_end].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, |byte| *byte == b' ');
        let label = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if !labels.iter().any(|candidate| candidate.as_bytes() == label) {
            continue;
        }
        if value.len() != oid_hex_len || !value.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(rejected());
        }
        refs.push(String::from_utf8_lossy(value).to_ascii_lowercase());
    }
    Ok(refs)
}

fn tree_oids(payload: &[u8], hash_len: usize) -> Result<Vec<String>, MegaError> {
    let mut refs = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|byte| *byte == 0) else {
            return Err(rejected());
        };
        let hash_end = nul.checked_add(1 + hash_len).ok_or_else(rejected)?;
        if hash_end > rest.len() || nul == 0 || !rest[..nul].contains(&b' ') {
            return Err(rejected());
        }
        refs.push(encode_hex(&rest[nul + 1..hash_end]));
        rest = &rest[hash_end..];
    }
    Ok(refs)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use git_internal::{
        hash::{HashKind, ObjectHash},
        internal::object::types::ObjectType,
    };

    use super::*;
    use crate::jupiter::storage::{
        object_storage::mock_object_storage,
        trace_object_storage::{get_raw, get_staging, put_raw},
    };

    const STREAM_A: &str = "11111111-1111-4111-8111-111111111111";
    const STREAM_B: &str = "22222222-2222-4222-8222-222222222222";
    const INGEST: &str = "33333333-3333-4333-8333-333333333333";

    fn oid(kind: HashKind, object_type: ObjectType, payload: &[u8]) -> String {
        ObjectHash::from_type_and_data_for_kind(kind, object_type, payload)
            .expect("oid")
            .to_string()
    }

    fn loose(object_type: &str, declared_oid: &str, payload: &[u8]) -> LooseObject {
        LooseObject {
            object_type: object_type.to_owned(),
            declared_oid: declared_oid.to_owned(),
            payload: payload.to_vec(),
        }
    }

    #[tokio::test]
    async fn quarantine_recovery_stages_verified_objects_and_stops_on_budget() {
        let store = mock_object_storage();
        let payload = b"hello-traces";
        let good = oid(HashKind::Sha1, ObjectType::Blob, payload);
        let staged = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: TracesProtocolLimits::MAX_PACK_BYTES,
                expanded_bytes: 0,
            },
            &[loose("blob", &good, payload)],
        )
        .await
        .expect("stage");
        assert_eq!(staged, vec![good.clone()]);
        assert_eq!(
            get_staging(&store, INGEST, "sha1", &good)
                .await
                .expect("staging read"),
            Some(payload.to_vec())
        );
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", &good)
                .await
                .expect("durable read"),
            None
        );
        let key = trace_object_storage::staging_object_key(INGEST, "sha1", &good).expect("key");
        assert!(key.key.starts_with(&format!("v1/staging/{INGEST}/")));
        assert!(!key.key.contains(&format!("v1/{STREAM_A}/")));

        let over_pack = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: TracesProtocolLimits::MAX_PACK_BYTES + 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &good, payload)],
        )
        .await;
        assert!(over_pack.is_err(), "compressed bytes stop at 512MiB");
        let over_expanded = unpack_into_quarantine(
            &store,
            "44444444-4444-4444-8444-444444444444",
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: TracesProtocolLimits::MAX_DECODED_BYTES,
            },
            &[loose("blob", &good, payload)],
        )
        .await;
        assert!(over_expanded.is_err(), "expanded bytes stop at 1GiB");
        assert_eq!(
            get_staging(
                &store,
                "44444444-4444-4444-8444-444444444444",
                "sha1",
                &good
            )
            .await
            .expect("over-budget staging"),
            None
        );

        let bad = unpack_into_quarantine(
            &store,
            "55555555-5555-4555-8555-555555555555",
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &"ab".repeat(20), payload)],
        )
        .await;
        assert!(bad.is_err(), "declared sha1 must match the payload");

        let sha256_payload = b"sha256-body";
        let sha256 = oid(HashKind::Sha256, ObjectType::Blob, sha256_payload);
        unpack_into_quarantine(
            &store,
            INGEST,
            "sha256",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &sha256, sha256_payload)],
        )
        .await
        .expect("sha256 stage");
        assert_eq!(
            get_staging(&store, INGEST, "sha256", &sha256)
                .await
                .expect("sha256 staging"),
            Some(sha256_payload.to_vec())
        );

        let delta = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("ref-delta", &good, payload)],
        )
        .await;
        assert!(
            delta.is_err(),
            "a thin delta is not resolved from another scope"
        );
    }

    #[tokio::test]
    async fn quarantine_recovery_rejects_a_missing_or_cross_scope_base() {
        let store = mock_object_storage();
        let blob = b"base-blob";
        let blob_oid = oid(HashKind::Sha1, ObjectType::Blob, blob);
        let commit = format!("tree {blob_oid}\n\nmessage\n");
        let commit_oid = oid(HashKind::Sha1, ObjectType::Commit, commit.as_bytes());
        let objects = [loose("commit", &commit_oid, commit.as_bytes())];
        unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: commit.len() as u64,
                expanded_bytes: 0,
            },
            &objects,
        )
        .await
        .expect("stage commit");
        assert!(
            require_closure(&store, STREAM_A, "sha1", &objects)
                .await
                .is_err(),
            "the tree is not in this quarantine"
        );

        put_raw(&store, STREAM_B, "sha1", &blob_oid, blob)
            .await
            .expect("other stream");
        assert!(
            require_closure(&store, STREAM_A, "sha1", &objects)
                .await
                .is_err(),
            "another stream is not a base"
        );
        assert_eq!(
            get_raw(&store, STREAM_B, "sha1", &blob_oid)
                .await
                .expect("other read"),
            Some(blob.to_vec())
        );

        let closed = [
            loose("blob", &blob_oid, blob),
            loose("commit", &commit_oid, commit.as_bytes()),
        ];
        require_closure(&store, STREAM_A, "sha1", &closed)
            .await
            .expect("same quarantine closes the tree");
        put_raw(&store, STREAM_A, "sha1", &blob_oid, blob)
            .await
            .expect("same stream");
        require_closure(&store, STREAM_A, "sha1", &objects)
            .await
            .expect("this stream's archived blob closes the tree");
    }
}
