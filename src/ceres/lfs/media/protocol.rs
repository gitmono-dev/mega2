//! Wire-compatible FastCDC Media manifest protocol.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::chunker;

pub const MAX_MANIFEST_SIZE: usize = 10 * 1024 * 1024;
pub const MAX_CHUNKS: usize = 8192;

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

#[derive(Debug, Error)]
pub enum MediaProtocolError {
    #[error("unsupported media manifest version")]
    UnsupportedVersion,
    #[error("unsupported media chunk algorithm")]
    UnsupportedAlgorithm,
    #[error("unsupported media hash algorithm")]
    UnsupportedHashAlgorithm,
    #[error("media object ID must be lowercase SHA-256 hex")]
    InvalidMediaOid,
    #[error("media manifest has too many chunks")]
    TooManyChunks,
    #[error("fallback object ID must equal the media object ID")]
    InvalidFallbackOid,
    #[error("chunk {index} does not begin at the expected offset")]
    NonContiguousChunk { index: usize },
    #[error("chunk {index} has zero length")]
    ZeroLengthChunk { index: usize },
    #[error("chunk {index} exceeds the maximum chunk size")]
    OversizedChunk { index: usize },
    #[error("chunk {index} hash must be lowercase SHA-256 hex")]
    InvalidChunkHash { index: usize },
    #[error("chunk {index} uses unsupported compression")]
    UnsupportedCompression { index: usize },
    #[error("chunk {index} encoded length differs from raw length")]
    EncodedLengthMismatch { index: usize },
    #[error("chunk {index} checksum is unsupported for fastcdc-v1")]
    UnsupportedChecksum { index: usize },
    #[error("chunk {index} causes a byte-offset overflow")]
    ChunkOffsetOverflow { index: usize },
    #[error("chunk lengths do not match media size")]
    MediaSizeMismatch,
    #[error("media manifest payload exceeds {MAX_MANIFEST_SIZE} bytes")]
    ManifestTooLarge,
    #[error("media manifest JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn parse_manifest(payload: &[u8]) -> Result<MediaManifest, MediaProtocolError> {
    if payload.len() > MAX_MANIFEST_SIZE {
        return Err(MediaProtocolError::ManifestTooLarge);
    }

    let manifest: MediaManifest = serde_json::from_slice(payload)?;
    manifest.validate()?;
    Ok(manifest)
}

impl MediaManifest {
    pub fn validate(&self) -> Result<(), MediaProtocolError> {
        if self.version != 1 {
            return Err(MediaProtocolError::UnsupportedVersion);
        }
        if self.algorithm != chunker::ALGORITHM {
            return Err(MediaProtocolError::UnsupportedAlgorithm);
        }
        if self.hash_algorithm != "sha256" {
            return Err(MediaProtocolError::UnsupportedHashAlgorithm);
        }
        if !valid_hash(&self.media_oid) {
            return Err(MediaProtocolError::InvalidMediaOid);
        }
        if self.chunks.len() > MAX_CHUNKS {
            return Err(MediaProtocolError::TooManyChunks);
        }
        if self
            .fallback_oid
            .as_ref()
            .is_some_and(|oid| oid != &self.media_oid)
        {
            return Err(MediaProtocolError::InvalidFallbackOid);
        }

        let mut offset = 0u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.offset != offset {
                return Err(MediaProtocolError::NonContiguousChunk { index });
            }
            if chunk.length == 0 {
                return Err(MediaProtocolError::ZeroLengthChunk { index });
            }
            if chunk.length > chunker::MAX_SIZE as u64 {
                return Err(MediaProtocolError::OversizedChunk { index });
            }
            if !valid_hash(&chunk.chunk_hash) {
                return Err(MediaProtocolError::InvalidChunkHash { index });
            }
            if chunk.compression != "none" {
                return Err(MediaProtocolError::UnsupportedCompression { index });
            }
            if chunk.encoded_length != chunk.length {
                return Err(MediaProtocolError::EncodedLengthMismatch { index });
            }
            if chunk.checksum.is_some() {
                return Err(MediaProtocolError::UnsupportedChecksum { index });
            }

            offset = offset
                .checked_add(chunk.length)
                .ok_or(MediaProtocolError::ChunkOffsetOverflow { index })?;
        }

        if offset != self.media_size {
            return Err(MediaProtocolError::MediaSizeMismatch);
        }

        Ok(())
    }

    /// Returns the canonical v1 manifest identifier.
    pub fn id(&self) -> Result<String, MediaProtocolError> {
        self.validate()?;
        let canonical = serde_json::to_vec(&(
            self.version,
            &self.algorithm,
            &self.hash_algorithm,
            &self.media_oid,
            self.media_size,
            &self.chunks,
        ))?;
        Ok(sha256_hex(&canonical))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub manifest_id: String,
    pub missing_chunks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub manifest_id: String,
    pub manifest: MediaManifest,
}

pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "version": "1",
        "chunked_lfs": true,
        "chunk_algorithms": [chunker::ALGORITHM],
        "hash_algorithms": ["sha256"],
        "max_chunk_size": chunker::MAX_SIZE,
        "max_manifest_size": MAX_MANIFEST_SIZE,
        "supports_batch_exists": true,
        "supports_range_read": false,
        "supports_standard_lfs_fallback": true,
        "scope": "authenticated-user-and-repository"
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEGA_V1_MANIFEST_JSON: &str = concat!(
        "{\"version\":1,\"algorithm\":\"fastcdc-v1\",\"hash_algorithm\":\"sha256\",\"media_oid\":\"",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "\",\"media_size\":3,\"chunks\":[{\"offset\":0,\"length\":3,\"chunk_hash\":\"",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "\",\"encoded_length\":3,\"compression\":\"none\"}],\"created_by\":{\"client\":\"mega\",\"version\":\"bb3ef17\",\"capabilities\":[\"fastcdc-v1\"]},\"fallback_oid\":\"",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "\"}",
    );
    const MEGA_V1_MANIFEST_ID: &str =
        "b62523d099b04d52dcf7b55cce8f09bd08d07569d21ed999522ae1a14de5bdac";

    fn sample_manifest() -> MediaManifest {
        MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.into(),
            hash_algorithm: "sha256".into(),
            media_oid: "a".repeat(64),
            media_size: 3,
            chunks: vec![ChunkEntry {
                offset: 0,
                length: 3,
                chunk_hash: "b".repeat(64),
                encoded_length: 3,
                compression: "none".into(),
                checksum: None,
            }],
            created_by: CreatedBy {
                client: "monoengine".into(),
                version: "test".into(),
                capabilities: vec![chunker::ALGORITHM.into()],
            },
            fallback_oid: None,
        }
    }

    #[test]
    fn canonical_id_is_stable() {
        let manifest = sample_manifest();
        let id = manifest.id().unwrap();

        let mut provenance_changed = manifest.clone();
        provenance_changed.created_by = CreatedBy {
            client: "another-client".into(),
            version: "newer".into(),
            capabilities: vec!["other".into()],
        };
        provenance_changed.fallback_oid = Some(manifest.media_oid.clone());
        assert_eq!(provenance_changed.id().unwrap(), id);

        let mut protocol_changed = manifest;
        protocol_changed.chunks[0].chunk_hash = "c".repeat(64);
        assert_ne!(protocol_changed.id().unwrap(), id);
    }

    #[test]
    fn mega_pinned_manifest_vector_has_expected_canonical_id() {
        let manifest = parse_manifest(MEGA_V1_MANIFEST_JSON.as_bytes()).unwrap();

        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            MEGA_V1_MANIFEST_JSON
        );
        assert_eq!(manifest.id().unwrap(), MEGA_V1_MANIFEST_ID);
    }

    #[test]
    fn validates_manifest_contract() {
        let manifest = sample_manifest();
        assert!(manifest.validate().is_ok());
        assert!(valid_hash(&manifest.media_oid));
        assert!(!valid_hash(&"A".repeat(64)));
        assert!(!valid_hash("not-a-hash"));

        let mut invalid = manifest.clone();
        invalid.version = 2;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::UnsupportedVersion)
        ));

        let mut invalid = manifest.clone();
        invalid.algorithm = "fastcdc-v2".into();
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::UnsupportedAlgorithm)
        ));

        let mut invalid = manifest.clone();
        invalid.hash_algorithm = "sha1".into();
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::UnsupportedHashAlgorithm)
        ));

        let mut invalid = manifest.clone();
        invalid.media_oid = "invalid".into();
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::InvalidMediaOid)
        ));

        let mut invalid = manifest.clone();
        invalid.fallback_oid = Some("c".repeat(64));
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::InvalidFallbackOid)
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].offset = 1;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::NonContiguousChunk { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].length = 0;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::ZeroLengthChunk { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].length = chunker::MAX_SIZE as u64 + 1;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::OversizedChunk { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].chunk_hash = "invalid".into();
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::InvalidChunkHash { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].compression = "gzip".into();
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::UnsupportedCompression { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].encoded_length = 2;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::EncodedLengthMismatch { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.chunks[0].checksum = Some("checksum".into());
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::UnsupportedChecksum { index: 0 })
        ));

        let mut invalid = manifest.clone();
        invalid.media_size = 2;
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::MediaSizeMismatch)
        ));

        let mut invalid = manifest;
        invalid.chunks = vec![invalid.chunks[0].clone(); MAX_CHUNKS + 1];
        assert!(matches!(
            invalid.validate(),
            Err(MediaProtocolError::TooManyChunks)
        ));
    }

    #[test]
    fn parses_bounded_manifest_payloads() {
        let manifest = sample_manifest();
        let payload = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(parse_manifest(&payload).unwrap(), manifest);

        let oversized = vec![b' '; MAX_MANIFEST_SIZE + 1];
        assert!(matches!(
            parse_manifest(&oversized),
            Err(MediaProtocolError::ManifestTooLarge)
        ));
    }

    #[test]
    fn response_and_capability_payloads_match_v1_wire_contract() {
        let manifest = sample_manifest();
        let manifest_id = "d".repeat(64);
        let prepare = PrepareResponse {
            manifest_id: manifest_id.clone(),
            missing_chunks: vec![manifest.chunks[0].chunk_hash.clone()],
        };
        assert_eq!(
            serde_json::to_value(&prepare).unwrap(),
            serde_json::json!({
                "manifest_id": prepare.manifest_id,
                "missing_chunks": prepare.missing_chunks,
            })
        );
        assert_eq!(
            serde_json::to_value(ManifestResponse {
                manifest_id,
                manifest,
            })
            .unwrap(),
            serde_json::json!({
                "manifest_id": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "manifest": {
                    "version": 1,
                    "algorithm": "fastcdc-v1",
                    "hash_algorithm": "sha256",
                    "media_oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "media_size": 3,
                    "chunks": [{
                        "offset": 0,
                        "length": 3,
                        "chunk_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "encoded_length": 3,
                        "compression": "none",
                    }],
                    "created_by": {
                        "client": "monoengine",
                        "version": "test",
                        "capabilities": ["fastcdc-v1"],
                    },
                },
            })
        );
        assert_eq!(
            capabilities(),
            serde_json::json!({
                "version": "1",
                "chunked_lfs": true,
                "chunk_algorithms": [chunker::ALGORITHM],
                "hash_algorithms": ["sha256"],
                "max_chunk_size": chunker::MAX_SIZE,
                "max_manifest_size": MAX_MANIFEST_SIZE,
                "supports_batch_exists": true,
                "supports_range_read": false,
                "supports_standard_lfs_fallback": true,
                "scope": "authenticated-user-and-repository"
            })
        );
    }
}
