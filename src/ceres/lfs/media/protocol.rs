//! FastCDC v1 Media manifest wire contract (Libra `fastcdc-v1` / Mega `bb3ef17`).
//!
//! Canonical ID is SHA-256 over the JSON encoding of
//! `(version, algorithm, hash_algorithm, media_oid, media_size, chunks)`.
//! `created_by` and `fallback_oid` are not identity inputs.

use serde::{Deserialize, Serialize};

use crate::ceres::lfs::{digest::LfsDigest, media::chunker};

pub const MANIFEST_VERSION: u32 = 1;
pub const MAX_MANIFEST_SIZE: usize = 10 * 1024 * 1024;
pub const MAX_CHUNKS: usize = 8192;
pub const HASH_ALGORITHM: &str = "sha256";
pub const COMPRESSION_NONE: &str = "none";

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is malformed: {0}")]
    Invalid(String),
    #[error("failed to (de)serialize manifest: {0}")]
    Serde(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub offset: u64,
    pub length: u64,
    pub chunk_hash: String,
    pub encoded_length: u64,
    pub compression: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedBy {
    pub client: String,
    pub version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaManifest {
    pub version: u32,
    pub algorithm: String,
    pub hash_algorithm: String,
    pub media_oid: String,
    pub media_size: u64,
    pub chunks: Vec<ChunkEntry>,
    pub created_by: CreatedBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_oid: Option<String>,
}

/// `POST …/manifests` body returned to Libra (`PrepareResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub manifest_id: String,
    pub missing_chunks: Vec<String>,
}

/// `GET …/manifests/by-media/{oid}` body (`ManifestResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub manifest_id: String,
    pub manifest: MediaManifest,
}

/// Capability document at `libra/media/v1/capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub version: String,
    pub chunked_lfs: bool,
    pub chunk_algorithms: Vec<String>,
    pub hash_algorithms: Vec<String>,
    pub max_chunk_size: u64,
    pub max_manifest_size: u64,
    pub supports_batch_exists: bool,
    pub supports_range_read: bool,
    pub supports_standard_lfs_fallback: bool,
}

impl Capabilities {
    pub fn v1() -> Self {
        Self {
            version: "1".to_string(),
            chunked_lfs: true,
            chunk_algorithms: vec![chunker::ALGORITHM.to_string()],
            hash_algorithms: vec![HASH_ALGORITHM.to_string()],
            max_chunk_size: chunker::MAX_SIZE as u64,
            max_manifest_size: MAX_MANIFEST_SIZE as u64,
            supports_batch_exists: true,
            supports_range_read: false,
            supports_standard_lfs_fallback: true,
        }
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl MediaManifest {
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_SIZE {
            return Err(ManifestError::Invalid(
                "manifest exceeds size limit".to_string(),
            ));
        }
        let manifest: MediaManifest =
            serde_json::from_str(text).map_err(|e| ManifestError::Serde(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_json(&self) -> Result<String, ManifestError> {
        serde_json::to_string_pretty(self).map_err(|e| ManifestError::Serde(e.to_string()))
    }

    pub fn id(&self) -> Result<String, ManifestError> {
        self.validate()?;
        let bytes = serde_json::to_vec(&(
            self.version,
            &self.algorithm,
            &self.hash_algorithm,
            &self.media_oid,
            self.media_size,
            &self.chunks,
        ))
        .map_err(|error| ManifestError::Serde(error.to_string()))?;
        Ok(LfsDigest::sha256_of(&bytes).hex().to_owned())
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.chunks.len() > MAX_CHUNKS {
            return Err(ManifestError::Invalid("too many chunks".to_string()));
        }
        if self
            .fallback_oid
            .as_ref()
            .is_some_and(|oid| oid != &self.media_oid)
        {
            return Err(ManifestError::Invalid(
                "too many chunks or mismatched fallback_oid".to_string(),
            ));
        }
        if let Some(oid) = &self.fallback_oid
            && !is_sha256_hex(oid)
        {
            return Err(ManifestError::Invalid(
                "fallback_oid must be exactly 64 lowercase-hex characters".to_string(),
            ));
        }
        if self.version != MANIFEST_VERSION {
            return Err(ManifestError::Invalid(format!(
                "unsupported manifest version {} (this binary supports {MANIFEST_VERSION})",
                self.version
            )));
        }
        if self.algorithm != chunker::ALGORITHM {
            return Err(ManifestError::Invalid(format!(
                "unsupported chunk algorithm '{}' (expected '{}')",
                self.algorithm,
                chunker::ALGORITHM
            )));
        }
        if self.hash_algorithm != HASH_ALGORITHM {
            return Err(ManifestError::Invalid(format!(
                "unsupported hash algorithm '{}' (media_oid must be sha256)",
                self.hash_algorithm
            )));
        }
        if !is_sha256_hex(&self.media_oid) {
            return Err(ManifestError::Invalid(
                "media_oid must be exactly 64 lowercase-hex characters".to_string(),
            ));
        }
        let mut expected_offset = 0u64;
        for (i, c) in self.chunks.iter().enumerate() {
            if c.length == 0 || c.length > chunker::MAX_SIZE as u64 || c.checksum.is_some() {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} has invalid length or unsupported checksum"
                )));
            }
            if c.offset != expected_offset {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} offset {} breaks contiguity (expected {expected_offset})",
                    c.offset
                )));
            }
            if !is_sha256_hex(&c.chunk_hash) {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} chunk_hash must be 64 lowercase-hex characters"
                )));
            }
            if c.compression != COMPRESSION_NONE {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} has unsupported compression '{}' (v1 supports only 'none')",
                    c.compression
                )));
            }
            if c.encoded_length != c.length {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} encoded_length {} must equal length {} for uncompressed v1 chunks",
                    c.encoded_length, c.length
                )));
            }
            expected_offset = expected_offset
                .checked_add(c.length)
                .ok_or_else(|| ManifestError::Invalid("chunk offset overflow".into()))?;
        }
        if expected_offset != self.media_size {
            return Err(ManifestError::Invalid(format!(
                "chunk lengths sum to {expected_offset} but media_size is {}",
                self.media_size
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_V1: &str = include_str!("fixtures/valid_v1.json");
    const EMPTY: &str = include_str!("fixtures/empty.json");
    const INVALID_VERSION: &str = include_str!("fixtures/invalid_version.json");
    const INVALID_FALLBACK: &str = include_str!("fixtures/invalid_fallback.json");

    /// SHA-256 of `fixtures/valid_v1.json` bytes (pinned; not a sibling path).
    const VALID_V1_FILE_SHA256: &str =
        "20226243095e92274b3683f4c09bcd12ae35d245b073b38296a2a895c13b8c9d";

    fn sample() -> MediaManifest {
        MediaManifest {
            version: 1,
            algorithm: "fastcdc-v1".to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: "a".repeat(64),
            media_size: 10,
            chunks: vec![
                ChunkEntry {
                    offset: 0,
                    length: 6,
                    chunk_hash: "b".repeat(64),
                    encoded_length: 6,
                    compression: "none".to_string(),
                    checksum: None,
                },
                ChunkEntry {
                    offset: 6,
                    length: 4,
                    chunk_hash: "c".repeat(64),
                    encoded_length: 4,
                    compression: "none".to_string(),
                    checksum: None,
                },
            ],
            created_by: CreatedBy {
                client: "libra".to_string(),
                version: "0".to_string(),
                capabilities: vec!["fastcdc-v1".to_string(), "sha256".to_string()],
            },
            fallback_oid: None,
        }
    }

    #[test]
    fn canonical_id_is_stable() {
        let mut m = sample();
        let id = m.id().unwrap();
        m.created_by.client = "other-client".to_string();
        m.created_by.version = "9.9.9".to_string();
        m.fallback_oid = Some(m.media_oid.clone());
        assert_eq!(id, m.id().unwrap());
        assert_eq!(id.len(), 64);
        assert!(is_sha256_hex(&id));
    }

    #[test]
    fn fixture_valid_v1_round_trip_and_id() {
        let m = MediaManifest::from_json(VALID_V1).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.algorithm, "fastcdc-v1");
        assert_eq!(m.hash_algorithm, "sha256");
        assert_eq!(m.media_size, 10);
        let id = m.id().unwrap();
        let again = MediaManifest::from_json(&m.to_json().unwrap()).unwrap();
        assert_eq!(id, again.id().unwrap());
        let resp = ManifestResponse {
            manifest_id: id.clone(),
            manifest: m,
        };
        let encoded = serde_json::to_value(&resp).unwrap();
        assert_eq!(encoded["manifest_id"], id);
        assert!(encoded.get("manifest").is_some());
    }

    #[test]
    fn fixture_empty_file_is_valid() {
        let m = MediaManifest::from_json(EMPTY).unwrap();
        assert_eq!(m.media_size, 0);
        assert!(m.chunks.is_empty());
        assert!(is_sha256_hex(&m.id().unwrap()));
    }

    #[test]
    fn fixture_invalid_version_and_fallback() {
        assert!(MediaManifest::from_json(INVALID_VERSION).is_err());
        assert!(MediaManifest::from_json(INVALID_FALLBACK).is_err());
    }

    #[test]
    fn rejects_zero_oversize_chunks_uppercase_hash_and_compression() {
        for length in [0, u64::MAX, chunker::MAX_SIZE as u64 + 1] {
            let mut m = sample();
            m.chunks[0].length = length;
            m.chunks[0].encoded_length = length;
            assert!(m.validate().is_err());
        }
        let mut m = sample();
        m.media_oid = "A".repeat(64);
        assert!(m.validate().is_err());
        let mut m = sample();
        m.chunks[0].compression = "gzip".to_string();
        assert!(m.validate().is_err());
        let mut m = sample();
        m.chunks[0].encoded_length = 5;
        assert!(m.validate().is_err());
    }

    #[test]
    fn rejects_bad_version_algo_size_and_contiguity() {
        let mut m = sample();
        m.version = 2;
        assert!(m.validate().is_err());
        let mut m = sample();
        m.algorithm = "fastcdc-v2".to_string();
        assert!(m.validate().is_err());
        let mut m = sample();
        m.chunks[1].offset = 99;
        assert!(m.validate().is_err());
        let mut m = sample();
        m.media_size = 999;
        assert!(m.validate().is_err());
    }

    #[test]
    fn prepare_and_capability_payloads_match_libra_fields() {
        let prepare = PrepareResponse {
            manifest_id: "d".repeat(64),
            missing_chunks: vec!["e".repeat(64)],
        };
        let v = serde_json::to_value(&prepare).unwrap();
        assert_eq!(v["manifest_id"], "d".repeat(64));
        assert_eq!(v["missing_chunks"][0], "e".repeat(64));
        let caps = Capabilities::v1();
        let v = serde_json::to_value(&caps).unwrap();
        assert_eq!(v["version"], "1");
        assert_eq!(v["chunked_lfs"], serde_json::Value::Bool(true));
        assert_eq!(v["chunk_algorithms"][0], "fastcdc-v1");
        assert_eq!(v["hash_algorithms"][0], "sha256");
        assert_eq!(v["max_chunk_size"], chunker::MAX_SIZE as u64);
        assert_eq!(v["max_manifest_size"], MAX_MANIFEST_SIZE as u64);
        assert_eq!(v["supports_batch_exists"], serde_json::Value::Bool(true));
        assert_eq!(v["supports_range_read"], serde_json::Value::Bool(false));
        assert_eq!(
            v["supports_standard_lfs_fallback"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn fixture_valid_v1_file_hash_is_pinned() {
        let digest = LfsDigest::sha256_of(VALID_V1.as_bytes());
        assert_eq!(
            digest.hex(),
            VALID_V1_FILE_SHA256,
            "update VALID_V1_FILE_SHA256 when the fixture bytes change"
        );
    }
}
