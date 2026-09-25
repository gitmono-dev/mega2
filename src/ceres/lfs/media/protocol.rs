//! FastCDC Media manifest wire contract (`fastcdc-v2020-32k`).
//!
//! Canonical ID is SHA-256 over the JSON encoding of
//! `(version, algorithm, hash_algorithm, media_oid, media_size, chunks)`.
//! `created_by` and `fallback_oid` are not identity inputs. Paging boundaries
//! do not participate in identity (P-01).

use serde::{Deserialize, Serialize};

use crate::ceres::lfs::{digest::LfsDigest, media::chunker};

pub const MANIFEST_VERSION: u32 = 1;
/// Media metadata envelope: single page / summary / status wrap (C-02).
/// Not the sum of all pages or the full LFS stream.
pub const MAX_ENVELOPE_SIZE: usize = 1_048_576;
/// Historical alias — means [`MAX_ENVELOPE_SIZE`] only.
pub const MAX_MANIFEST_SIZE: usize = MAX_ENVELOPE_SIZE;
pub const MAX_PAGE_ENTRIES: usize = 4096;
/// Compact canonical entries-array budget; remainder of the 1 MiB envelope is
/// reserved for fixed page wrap fields (P-01a).
pub const MAX_PAGE_ENTRIES_BYTES: usize = 960 * 1024;
pub const MAX_CREATED_BY_BYTES: usize = 4096;
pub const HASH_ALGORITHM: &str = "sha256";
pub const COMPRESSION_NONE: &str = "none";
pub const MANIFEST_PAGING: &str = "v1";

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

/// Paging summary (P-01 / P-02); not a full layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub version: u32,
    pub algorithm: String,
    pub hash_algorithm: String,
    pub oid: String,
    pub size: u64,
    pub chunk_count: u64,
    pub page_count: u32,
    pub manifest_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<CreatedBy>,
}

/// One immutable page of chunk entries (P-01a / P-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPage {
    pub page_no: u32,
    pub entries: Vec<ChunkEntry>,
}

/// `POST …/manifests` body returned to Libra (`PrepareResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub manifest_id: String,
    pub missing_chunks: Vec<String>,
}

/// `POST …/manifests/{id}/seal` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealResponse {
    pub manifest_id: String,
    pub seal_generation: i64,
    pub page_count: u32,
}

/// `GET …/manifests/{id}/missing` page (P-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingChunksResponse {
    pub hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `POST …/manifests/{id}/finalize` accepted body (P-04a, HTTP 202).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeAcceptedResponse {
    pub task_id: String,
    pub manifest_id: String,
    pub state: String,
    pub status_url: String,
}

/// `GET …/tasks/{task_id}` status body (P-04a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeTaskResponse {
    pub task_id: String,
    pub manifest_id: String,
    pub state: String,
    pub stage: String,
    pub bytes_verified: u64,
    pub pages_verified: i32,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Present when `state == "complete"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `GET …/manifests/by-media/{oid}` body (`ManifestResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub manifest_id: String,
    pub manifest: MediaManifest,
}

/// One page in a `GET …/finalized/{id}/pages` response (P-03 / MF-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizedPageItem {
    pub page_no: u32,
    /// Inclusive absolute byte offset of the first entry.
    pub offset_start: u64,
    /// Exclusive absolute end (`offset_start` + covered length).
    pub offset_end: u64,
    pub entries: Vec<ChunkEntry>,
}

/// `GET …/finalized/{manifest_id}/pages` body (P-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizedPagesResponse {
    pub manifest_id: String,
    pub pages: Vec<FinalizedPageItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Capability document at `libra/media/v1/capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub version: String,
    pub chunked_lfs: bool,
    pub chunk_algorithms: Vec<String>,
    pub hash_algorithms: Vec<String>,
    pub max_chunk_size: u64,
    /// Envelope limit (1 MiB). Kept for older clients that still read this key.
    pub max_manifest_size: u64,
    pub supports_batch_exists: bool,
    pub supports_range_read: bool,
    pub supports_standard_lfs_fallback: bool,
    /// Shared-table fields (C-01 table / MF-02).
    pub batch_exists: bool,
    pub range_read: bool,
    pub standard_lfs_fallback: bool,
    pub supports_manifest_id_read: bool,
    pub manifest_paging: String,
    pub max_page_entries: u64,
    pub max_page_bytes: u64,
}

impl Capabilities {
    pub fn v1() -> Self {
        Self {
            version: "1".to_string(),
            chunked_lfs: true,
            chunk_algorithms: vec![chunker::ALGORITHM.to_string()],
            hash_algorithms: vec![HASH_ALGORITHM.to_string()],
            max_chunk_size: chunker::MAX_SIZE as u64,
            max_manifest_size: MAX_ENVELOPE_SIZE as u64,
            supports_batch_exists: true,
            supports_range_read: false,
            supports_standard_lfs_fallback: true,
            batch_exists: true,
            range_read: false,
            standard_lfs_fallback: true,
            supports_manifest_id_read: true,
            manifest_paging: MANIFEST_PAGING.to_string(),
            max_page_entries: MAX_PAGE_ENTRIES as u64,
            max_page_bytes: MAX_ENVELOPE_SIZE as u64,
        }
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Compact canonical encoding of a chunk-entry slice (identity / P-01a).
pub fn canonical_entries_bytes(entries: &[ChunkEntry]) -> Result<Vec<u8>, ManifestError> {
    serde_json::to_vec(entries).map_err(|e| ManifestError::Serde(e.to_string()))
}

/// Longest prefix length under the P-01a entry-count and byte budgets.
pub fn longest_page_prefix(chunks: &[ChunkEntry]) -> Result<usize, ManifestError> {
    if chunks.is_empty() {
        return Ok(0);
    }
    let max = chunks.len().min(MAX_PAGE_ENTRIES);
    let mut lo = 1usize;
    let mut hi = max;
    let mut best = 0usize;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let bytes = canonical_entries_bytes(&chunks[..mid])?;
        if bytes.len() <= MAX_PAGE_ENTRIES_BYTES {
            best = mid;
            lo = mid + 1;
        } else if mid == 1 {
            return Err(ManifestError::Invalid(
                "single chunk entry exceeds page byte budget".into(),
            ));
        } else {
            hi = mid - 1;
        }
    }
    if best == 0 {
        return Err(ManifestError::Invalid(
            "unable to form a non-empty page under P-01a budgets".into(),
        ));
    }
    Ok(best)
}

/// Deterministic P-01a page split. Empty input → zero pages; every non-final
/// page is the longest legal prefix of the remainder; final page is non-empty.
pub fn split_pages(chunks: &[ChunkEntry]) -> Result<Vec<Vec<ChunkEntry>>, ManifestError> {
    if chunks.is_empty() {
        return Ok(Vec::new());
    }
    let mut pages = Vec::new();
    let mut rest = chunks;
    while !rest.is_empty() {
        let n = longest_page_prefix(rest)?;
        pages.push(rest[..n].to_vec());
        rest = &rest[n..];
    }
    Ok(pages)
}

impl ManifestPage {
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_ENVELOPE_SIZE {
            return Err(ManifestError::Invalid(
                "page envelope exceeds size limit".to_string(),
            ));
        }
        let page: ManifestPage =
            serde_json::from_str(text).map_err(|e| ManifestError::Serde(e.to_string()))?;
        page.validate()?;
        Ok(page)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.entries.is_empty() {
            return Err(ManifestError::Invalid(
                "page entries must be non-empty".into(),
            ));
        }
        if self.entries.len() > MAX_PAGE_ENTRIES {
            return Err(ManifestError::Invalid(
                "page exceeds max_page_entries".into(),
            ));
        }
        let bytes = canonical_entries_bytes(&self.entries)?;
        if bytes.len() > MAX_PAGE_ENTRIES_BYTES {
            return Err(ManifestError::Invalid(
                "page entries exceed compact byte budget".into(),
            ));
        }
        for (i, c) in self.entries.iter().enumerate() {
            validate_chunk_fields(i, c, /*is_tail_unknown*/ true)?;
        }
        Ok(())
    }
}

impl ManifestSummary {
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_ENVELOPE_SIZE {
            return Err(ManifestError::Invalid(
                "summary envelope exceeds size limit".to_string(),
            ));
        }
        let summary: ManifestSummary =
            serde_json::from_str(text).map_err(|e| ManifestError::Serde(e.to_string()))?;
        summary.validate()?;
        Ok(summary)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
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
                "unsupported hash algorithm '{}' (oid must be sha256)",
                self.hash_algorithm
            )));
        }
        if !is_sha256_hex(&self.oid) || !is_sha256_hex(&self.manifest_id) {
            return Err(ManifestError::Invalid(
                "oid and manifest_id must be 64 lowercase-hex characters".into(),
            ));
        }
        if let Some(cb) = &self.created_by {
            validate_created_by(cb)?;
        }
        Ok(())
    }
}

fn validate_created_by(cb: &CreatedBy) -> Result<(), ManifestError> {
    let bytes = serde_json::to_vec(cb).map_err(|e| ManifestError::Serde(e.to_string()))?;
    if bytes.len() > MAX_CREATED_BY_BYTES {
        return Err(ManifestError::Invalid(
            "created_by exceeds 4096 bytes".into(),
        ));
    }
    Ok(())
}

/// `is_tail_unknown`: when true (page-local), only enforce 1..=MAX (tail-safe).
fn validate_chunk_fields(
    i: usize,
    c: &ChunkEntry,
    is_tail_or_unknown: bool,
) -> Result<(), ManifestError> {
    if c.checksum.is_some() {
        return Err(ManifestError::Invalid(format!(
            "chunk {i} has unsupported checksum"
        )));
    }
    let max = chunker::MAX_SIZE as u64;
    let min = chunker::MIN_SIZE as u64;
    if is_tail_or_unknown {
        if c.length == 0 || c.length > max {
            return Err(ManifestError::Invalid(format!(
                "chunk {i} has invalid length"
            )));
        }
    } else if c.length < min || c.length > max {
        return Err(ManifestError::Invalid(format!(
            "chunk {i} has invalid length (non-tail must be {min}..={max})"
        )));
    }
    if !is_sha256_hex(&c.chunk_hash) {
        return Err(ManifestError::Invalid(format!(
            "chunk {i} chunk_hash must be 64 lowercase-hex characters"
        )));
    }
    if c.compression != COMPRESSION_NONE {
        return Err(ManifestError::Invalid(format!(
            "chunk {i} has unsupported compression '{}' (supports only 'none')",
            c.compression
        )));
    }
    if c.encoded_length != c.length {
        return Err(ManifestError::Invalid(format!(
            "chunk {i} encoded_length {} must equal length {}",
            c.encoded_length, c.length
        )));
    }
    Ok(())
}

impl MediaManifest {
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_ENVELOPE_SIZE {
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

    pub fn summary(&self) -> Result<ManifestSummary, ManifestError> {
        let pages = split_pages(&self.chunks)?;
        Ok(ManifestSummary {
            version: self.version,
            algorithm: self.algorithm.clone(),
            hash_algorithm: self.hash_algorithm.clone(),
            oid: self.media_oid.clone(),
            size: self.media_size,
            chunk_count: self.chunks.len() as u64,
            page_count: pages.len() as u32,
            manifest_id: self.id()?,
            created_by: Some(self.created_by.clone()),
        })
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self
            .fallback_oid
            .as_ref()
            .is_some_and(|oid| oid != &self.media_oid)
        {
            return Err(ManifestError::Invalid(
                "mismatched fallback_oid".to_string(),
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
        validate_created_by(&self.created_by)?;

        if self.chunks.is_empty() {
            if self.media_size != 0 {
                return Err(ManifestError::Invalid(
                    "empty chunk list requires media_size 0".into(),
                ));
            }
            return Ok(());
        }

        let n = self.chunks.len();
        let mut expected_offset = 0u64;
        for (i, c) in self.chunks.iter().enumerate() {
            let is_tail = i + 1 == n;
            validate_chunk_fields(i, c, is_tail)?;
            if c.offset != expected_offset {
                return Err(ManifestError::Invalid(format!(
                    "chunk {i} offset {} breaks contiguity (expected {expected_offset})",
                    c.offset
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
        "09ae74a7f69da0bbd2b3df12d8f4bb85413b8ba34ec5de123f68141219ac3b96";

    fn sample() -> MediaManifest {
        // Single-chunk file: the sole chunk is the tail (1..=MAX).
        MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: "a".repeat(64),
            media_size: 10,
            chunks: vec![ChunkEntry {
                offset: 0,
                length: 10,
                chunk_hash: "b".repeat(64),
                encoded_length: 10,
                compression: "none".to_string(),
                checksum: None,
            }],
            created_by: CreatedBy {
                client: "libra".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string(), "sha256".to_string()],
            },
            fallback_oid: None,
        }
    }

    fn entry(offset: u64, length: u64, hash_byte: u8) -> ChunkEntry {
        ChunkEntry {
            offset,
            length,
            chunk_hash: format!("{hash_byte:064x}"),
            encoded_length: length,
            compression: "none".to_string(),
            checksum: None,
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
        assert_eq!(m.algorithm, chunker::ALGORITHM);
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
        assert!(split_pages(&m.chunks).unwrap().is_empty());
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
            m.media_size = length;
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
    fn rejects_non_tail_below_min() {
        let mut m = sample();
        m.chunks = vec![
            entry(0, (chunker::MIN_SIZE - 1) as u64, 1),
            entry((chunker::MIN_SIZE - 1) as u64, 10, 2),
        ];
        m.media_size = (chunker::MIN_SIZE - 1) as u64 + 10;
        assert!(m.validate().is_err());
        m.chunks[0].length = chunker::MIN_SIZE as u64;
        m.chunks[0].encoded_length = chunker::MIN_SIZE as u64;
        m.chunks[1].offset = chunker::MIN_SIZE as u64;
        m.media_size = chunker::MIN_SIZE as u64 + 10;
        assert!(m.validate().is_ok());
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
        m.chunks[0].offset = 99;
        assert!(m.validate().is_err());
        let mut m = sample();
        m.media_size = 999;
        assert!(m.validate().is_err());
    }

    #[test]
    fn prepare_and_capability_payloads_match_shared_table() {
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
        assert_eq!(v["chunk_algorithms"][0], chunker::ALGORITHM);
        assert_eq!(v["hash_algorithms"][0], "sha256");
        assert_eq!(v["max_chunk_size"], chunker::MAX_SIZE as u64);
        assert_eq!(v["max_manifest_size"], MAX_ENVELOPE_SIZE as u64);
        assert_eq!(v["supports_batch_exists"], serde_json::Value::Bool(true));
        assert_eq!(v["supports_range_read"], serde_json::Value::Bool(false));
        assert_eq!(
            v["supports_standard_lfs_fallback"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(v["batch_exists"], serde_json::Value::Bool(true));
        assert_eq!(v["range_read"], serde_json::Value::Bool(false));
        assert_eq!(v["standard_lfs_fallback"], serde_json::Value::Bool(true));
        assert_eq!(
            v["supports_manifest_id_read"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(v["manifest_paging"], MANIFEST_PAGING);
        assert_eq!(v["max_page_entries"], MAX_PAGE_ENTRIES as u64);
        assert_eq!(v["max_page_bytes"], MAX_ENVELOPE_SIZE as u64);
    }

    #[test]
    fn p01a_split_by_entry_count() {
        let len = chunker::MIN_SIZE as u64;
        let chunks: Vec<_> = (0..4097)
            .map(|i| entry(i as u64 * len, len, (i % 200) as u8))
            .collect();
        let pages = split_pages(&chunks).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), 4096);
        assert_eq!(pages[1].len(), 1);
        // Re-splitting concatenated pages is identity.
        let flat: Vec<_> = pages.iter().flatten().cloned().collect();
        assert_eq!(flat, chunks);
        let again = split_pages(&flat).unwrap();
        assert_eq!(again, pages);
    }

    #[test]
    fn p01a_empty_and_single_page() {
        assert!(split_pages(&[]).unwrap().is_empty());
        let one = vec![entry(0, 10, 1)];
        let pages = split_pages(&one).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], one);
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
