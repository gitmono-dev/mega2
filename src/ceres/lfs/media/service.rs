//! Media prepare / chunk upload / resume / paging (pre-finalize). Finalize is FC-06.

use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::{Bytes, BytesMut};
use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::{
    ceres::lfs::{
        digest::LfsDigest,
        media::{
            chunker,
            membership_cache::{CacheKey, MembershipCache, MembershipRecord},
            protocol::{
                ChunkEntry, FinalizedPageItem, FinalizedPagesResponse, MAX_ENVELOPE_SIZE,
                MAX_PAGE_ENTRIES, ManifestError, ManifestPage, ManifestSummary, MediaManifest,
                MissingChunksResponse, PrepareResponse, SealResponse, split_pages,
            },
            publication,
            scope::{MediaObjectKind, MediaScope, ScopeError, redact_storage_error},
        },
    },
    jupiter::{
        storage::{
            media_paging_storage::{
                ChunkIndexRow, MediaPagingError, MediaPagingStorage, STATE_FINALIZED,
                STATE_PENDING, STATE_SEALED,
            },
            object_storage::MegaObjectStorageWrapper,
        },
        utils::into_obj_stream::IntoObjectStream,
    },
    orbit_api::{
        error::IoOrbitError,
        object_storage::{ObjectKey, ObjectMeta},
    },
};

pub const PENDING_TTL_SECS: u64 = 24 * 60 * 60;
/// C-05: prepare / missing exists probes run with this concurrency bound.
pub const EXISTS_CONCURRENCY: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid media request: {0}")]
    Invalid(String),
    #[error("media object not found")]
    NotFound,
    #[error("media conflict: {0}")]
    Conflict(String),
    #[error("media object store error")]
    Storage,
    #[error("media I/O error")]
    Io(#[from] std::io::Error),
    #[error("media JSON error: {0}")]
    Json(String),
    /// Finalize queue full (P-04b); map to HTTP 429.
    #[error("media finalize queue full")]
    TooManyRequests,
}

#[derive(Clone)]
pub struct MediaService {
    store: MegaObjectStorageWrapper,
    paging: MediaPagingStorage,
    membership: MembershipCache,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingSession {
    pub(crate) created_at_unix: u64,
    pub(crate) manifest: MediaManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MissingCursorV1 {
    v: u8,
    scope: String,
    manifest_id: String,
    seal_generation: i64,
    index: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PagesCursorV1 {
    v: u8,
    scope: String,
    manifest_id: String,
    page_no: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_end: Option<u64>,
}

impl MediaService {
    pub fn new(store: MegaObjectStorageWrapper, paging: MediaPagingStorage) -> Self {
        Self {
            store,
            paging,
            membership: MembershipCache::shared(),
        }
    }

    /// Test helper: isolated cache capacity (does not touch the process shared cache).
    #[cfg(test)]
    pub fn new_with_cache(
        store: MegaObjectStorageWrapper,
        paging: MediaPagingStorage,
        membership: MembershipCache,
    ) -> Self {
        Self {
            store,
            paging,
            membership,
        }
    }

    pub fn paging(&self) -> &MediaPagingStorage {
        &self.paging
    }

    pub fn membership_cache(&self) -> &MembershipCache {
        &self.membership
    }

    pub async fn prepare(
        &self,
        scope: &MediaScope,
        manifest: MediaManifest,
    ) -> Result<PrepareResponse, MediaError> {
        self.prepare_at(scope, manifest, unix_now()).await
    }

    pub async fn upload_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        body: Bytes,
    ) -> Result<(), MediaError> {
        self.upload_chunk_at(scope, manifest_id, chunk_hash, body, unix_now())
            .await
    }

    pub async fn get_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
    ) -> Result<Bytes, MediaError> {
        self.get_chunk_at(scope, manifest_id, chunk_hash, unix_now())
            .await
    }

    /// `GET …/finalized/{manifest_id}` → summary (immutable layout).
    pub async fn finalized_summary(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<ManifestSummary, MediaError> {
        let identity = publication::load_immutable_by_id(self, scope, manifest_id).await?;
        identity.manifest.summary().map_err(map_manifest)
    }

    /// `GET …/finalized/{manifest_id}/pages` — sequential or covering (P-03).
    pub async fn finalized_pages(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        cursor: Option<&str>,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> Result<FinalizedPagesResponse, MediaError> {
        self.require_finalized_session(scope, manifest_id).await?;
        let digest = scope.digest();

        let (mut page_no, cover) = match (cursor, offset, length) {
            (Some(raw), _, _) => {
                let cur = decode_pages_cursor(raw)?;
                if cur.scope != digest || cur.manifest_id != manifest_id {
                    return Err(MediaError::Invalid(
                        "pages cursor is expired or unauthorized".into(),
                    ));
                }
                (
                    cur.page_no,
                    cur.cover_start
                        .zip(cur.cover_end)
                        .map(|(s, e)| (s, e.saturating_sub(s))),
                )
            }
            (None, Some(off), Some(len)) => {
                let end = off
                    .checked_add(len)
                    .ok_or_else(|| MediaError::Invalid("range overflow".into()))?;
                let entries = self
                    .paging
                    .entries_covering_range(&digest, manifest_id, off, len, MAX_PAGE_ENTRIES as u64)
                    .await
                    .map_err(map_paging)?;
                let first = entries
                    .first()
                    .map(|e| e.page_no as u32)
                    .unwrap_or(u32::MAX);
                (first, Some((off, end.saturating_sub(off))))
            }
            (None, None, None) => (0u32, None),
            _ => {
                return Err(MediaError::Invalid(
                    "pages query requires cursor, or both offset and length, or neither".into(),
                ));
            }
        };

        if page_no == u32::MAX {
            return Ok(FinalizedPagesResponse {
                manifest_id: manifest_id.to_owned(),
                pages: Vec::new(),
                next_cursor: None,
            });
        }

        let session = self
            .paging
            .get_session(&digest, manifest_id)
            .await
            .map_err(map_paging)?;
        if page_no as i32 >= session.page_count {
            return Ok(FinalizedPagesResponse {
                manifest_id: manifest_id.to_owned(),
                pages: Vec::new(),
                next_cursor: None,
            });
        }

        // Covering mode: advance page_no until the page intersects [start, end).
        if let Some((start, len)) = cover {
            let end = start.saturating_add(len);
            while (page_no as i32) < session.page_count {
                let item = self
                    .load_finalized_page_item(scope, manifest_id, page_no)
                    .await?;
                if item.offset_end > start && item.offset_start < end {
                    let next = page_no.saturating_add(1);
                    let next_cursor = if (next as i32) < session.page_count {
                        // Peek whether more covering pages remain.
                        let more = self
                            .page_intersects_range(scope, manifest_id, next, start, end)
                            .await?;
                        if more {
                            Some(encode_pages_cursor(&PagesCursorV1 {
                                v: 1,
                                scope: digest.clone(),
                                manifest_id: manifest_id.to_owned(),
                                page_no: next,
                                cover_start: Some(start),
                                cover_end: Some(end),
                            })?)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let resp = FinalizedPagesResponse {
                        manifest_id: manifest_id.to_owned(),
                        pages: vec![item],
                        next_cursor,
                    };
                    ensure_envelope(&resp)?;
                    return Ok(resp);
                }
                page_no = page_no.saturating_add(1);
            }
            return Ok(FinalizedPagesResponse {
                manifest_id: manifest_id.to_owned(),
                pages: Vec::new(),
                next_cursor: None,
            });
        }

        // Sequential: one page per response.
        let item = self
            .load_finalized_page_item(scope, manifest_id, page_no)
            .await?;
        let next = page_no.saturating_add(1);
        let next_cursor = if (next as i32) < session.page_count {
            Some(encode_pages_cursor(&PagesCursorV1 {
                v: 1,
                scope: digest,
                manifest_id: manifest_id.to_owned(),
                page_no: next,
                cover_start: None,
                cover_end: None,
            })?)
        } else {
            None
        };
        let resp = FinalizedPagesResponse {
            manifest_id: manifest_id.to_owned(),
            pages: vec![item],
            next_cursor,
        };
        ensure_envelope(&resp)?;
        Ok(resp)
    }

    /// `GET …/finalized/{manifest_id}/chunks/{hash}` — fixed-layout membership.
    pub async fn finalized_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
    ) -> Result<Bytes, MediaError> {
        self.require_finalized_session(scope, manifest_id).await?;
        let members = self.membership_for_finalized(scope, manifest_id).await?;
        if !members.contains(chunk_hash) {
            return Err(MediaError::NotFound);
        }
        let expected_len = members.length_of(chunk_hash);
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        let bytes = self.read_existing_chunk(&key).await?;
        if let Some(len) = expected_len
            && bytes.len() as u64 != len
        {
            return Err(MediaError::Conflict(
                "stored chunk length does not match the finalized layout".to_string(),
            ));
        }
        if LfsDigest::sha256_of(&bytes).hex() != chunk_hash {
            return Err(MediaError::Conflict(
                "stored chunk content does not match the declared hash".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub async fn put_page(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        page: ManifestPage,
    ) -> Result<(), MediaError> {
        self.put_page_at(scope, manifest_id, page, unix_now()).await
    }

    pub async fn seal(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<SealResponse, MediaError> {
        self.seal_at(scope, manifest_id, unix_now()).await
    }

    pub async fn missing_chunks_page(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingChunksResponse, MediaError> {
        self.missing_chunks_page_at(scope, manifest_id, cursor, unix_now())
            .await
    }

    pub(crate) async fn prepare_at(
        &self,
        scope: &MediaScope,
        mut manifest: MediaManifest,
        now: u64,
    ) -> Result<PrepareResponse, MediaError> {
        manifest.validate().map_err(map_manifest)?;
        manifest.fallback_oid = Some(manifest.media_oid.clone());
        let manifest_id = manifest.id().map_err(map_manifest)?;
        let pages = split_pages(&manifest.chunks).map_err(map_manifest)?;
        let page_count = i32::try_from(pages.len())
            .map_err(|_| MediaError::Invalid("page_count exceeds i32".into()))?;

        let created_by = serde_json::to_string(&manifest.created_by)
            .map_err(|e| MediaError::Json(e.to_string()))?;
        let expires_at =
            Utc::now().fixed_offset() + ChronoDuration::seconds(PENDING_TTL_SECS as i64);
        self.paging
            .upsert_pending_session(
                &scope.digest(),
                &manifest_id,
                &manifest.algorithm,
                &manifest.media_oid,
                manifest.media_size,
                manifest.chunks.len() as u64,
                page_count,
                Some(&created_by),
                expires_at,
            )
            .await
            .map_err(map_paging)?;

        let pending_key = scope_key(scope, MediaObjectKind::Pending, &manifest_id)?;
        let created_at_unix = match self.load_pending(&pending_key).await {
            Ok(existing) if !is_expired(existing.created_at_unix, now) => existing.created_at_unix,
            Ok(_) | Err(MediaError::NotFound) => now,
            Err(e) => return Err(e),
        };
        let session = PendingSession {
            created_at_unix,
            manifest: manifest.clone(),
        };
        let payload = encode_pending(&session)?;
        self.put_metadata(&pending_key, payload).await?;
        let missing_chunks = self.missing_chunks_unique(scope, &manifest.chunks).await?;
        Ok(PrepareResponse {
            manifest_id,
            missing_chunks,
        })
    }

    pub(crate) async fn put_page_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        page: ManifestPage,
        now: u64,
    ) -> Result<(), MediaError> {
        page.validate().map_err(map_manifest)?;
        let _ = self.require_active_pending(scope, manifest_id, now).await?;
        let session = self
            .paging
            .get_session(&scope.digest(), manifest_id)
            .await
            .map_err(map_paging)?;
        if session.state != STATE_PENDING {
            return Err(MediaError::Conflict(format!(
                "cannot put page on session in state {}",
                session.state
            )));
        }
        if page.page_no as i32 >= session.page_count {
            return Err(MediaError::Invalid(
                "page_no out of range for session".into(),
            ));
        }

        let rows: Vec<ChunkIndexRow> = page
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| ChunkIndexRow {
                page_no: page.page_no as i32,
                ordinal: i as i32,
                offset: e.offset,
                length: e.length,
                chunk_hash: e.chunk_hash.clone(),
            })
            .collect();
        self.paging
            .put_page_entries(&scope.digest(), manifest_id, page.page_no as i32, &rows)
            .await
            .map_err(map_paging)?;

        let page_id = page_object_id(manifest_id, page.page_no);
        let key = scope_key(scope, MediaObjectKind::Page, &page_id)?;
        let body = serde_json::to_vec(&page).map_err(|e| MediaError::Json(e.to_string()))?;
        if body.len() > MAX_ENVELOPE_SIZE {
            return Err(MediaError::Invalid(
                "page envelope exceeds size limit".into(),
            ));
        }
        self.put_metadata(&key, Bytes::from(body)).await
    }

    pub(crate) async fn seal_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        now: u64,
    ) -> Result<SealResponse, MediaError> {
        let pending = self.require_active_pending(scope, manifest_id, now).await?;
        let expected = split_pages(&pending.manifest.chunks).map_err(map_manifest)?;
        let digest = scope.digest();
        let session = self
            .paging
            .get_session(&digest, manifest_id)
            .await
            .map_err(map_paging)?;
        if expected.len() as i32 != session.page_count {
            return Err(MediaError::Conflict(
                "session page_count does not match P-01a split".into(),
            ));
        }

        for (page_no, entries) in expected.iter().enumerate() {
            let page_id = page_object_id(manifest_id, page_no as u32);
            let key = scope_key(scope, MediaObjectKind::Page, &page_id)?;
            let bytes = self.read_bytes(&key, MAX_ENVELOPE_SIZE).await?;
            let stored: ManifestPage =
                serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))?;
            if stored.page_no != page_no as u32 || stored.entries != *entries {
                return Err(MediaError::Conflict(
                    "uploaded page does not match P-01a canonical split".into(),
                ));
            }
        }

        // Empty layout: no pages to upload; seal is still valid.
        let sealed = self
            .paging
            .seal_session(&digest, manifest_id)
            .await
            .map_err(map_paging)?;
        Ok(SealResponse {
            manifest_id: manifest_id.to_owned(),
            seal_generation: sealed.seal_generation,
            page_count: sealed.page_count as u32,
        })
    }

    pub(crate) async fn missing_chunks_page_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        cursor: Option<&str>,
        _now: u64,
    ) -> Result<MissingChunksResponse, MediaError> {
        let digest = scope.digest();
        let session = self
            .paging
            .get_session(&digest, manifest_id)
            .await
            .map_err(map_paging)?;
        if session.state != STATE_SEALED && session.state != STATE_FINALIZED {
            return Err(MediaError::Invalid(
                "missing cursor requires a sealed session".into(),
            ));
        }

        let start = match cursor {
            None => 0u64,
            Some(raw) => {
                let cur = decode_missing_cursor(raw)?;
                if cur.scope != digest
                    || cur.manifest_id != manifest_id
                    || cur.seal_generation != session.seal_generation
                {
                    return Err(MediaError::Invalid(
                        "missing cursor is expired or unauthorized".into(),
                    ));
                }
                cur.index
            }
        };

        let entries = self
            .paging
            .list_entries(&digest, manifest_id)
            .await
            .map_err(map_paging)?;
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for e in &entries {
            if seen.insert(e.chunk_hash.as_str()) {
                unique.push(e.chunk_hash.clone());
            }
        }

        let slice_start = usize::try_from(start).unwrap_or(usize::MAX);
        if slice_start > unique.len() {
            return Err(MediaError::Invalid(
                "missing cursor index out of range".into(),
            ));
        }
        let remaining = &unique[slice_start..];
        // Probe a window large enough to fill one missing page after filtering
        // for absences; cap by MAX_PAGE_ENTRIES.
        let probe_end = remaining.len().min(MAX_PAGE_ENTRIES);
        let probe = &remaining[..probe_end];
        let flags = self.exists_many(scope, probe).await?;
        let mut hashes = Vec::new();
        for (hash, exists) in probe.iter().zip(flags.iter()) {
            if !exists {
                hashes.push(hash.clone());
            }
        }

        let next_index = start + probe_end as u64;
        let next_cursor = if next_index >= unique.len() as u64 {
            None
        } else {
            Some(encode_missing_cursor(&MissingCursorV1 {
                v: 1,
                scope: digest,
                manifest_id: manifest_id.to_owned(),
                seal_generation: session.seal_generation,
                index: next_index,
            })?)
        };

        let resp = MissingChunksResponse {
            hashes,
            next_cursor,
        };
        let encoded = serde_json::to_vec(&resp).map_err(|e| MediaError::Json(e.to_string()))?;
        if encoded.len() > MAX_ENVELOPE_SIZE {
            return Err(MediaError::Invalid(
                "missing response exceeds envelope".into(),
            ));
        }
        Ok(resp)
    }

    pub(crate) async fn upload_chunk_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        body: Bytes,
        now: u64,
    ) -> Result<(), MediaError> {
        let members = self.membership_for_pending(scope, manifest_id, now).await?;
        let declared_len = members.length_of(chunk_hash).ok_or_else(|| {
            MediaError::Invalid("chunk is not declared by the pending manifest".to_string())
        })?;
        if body.len() as u64 != declared_len {
            return Err(MediaError::Invalid(
                "chunk length does not match the pending manifest".to_string(),
            ));
        }
        if body.len() > chunker::MAX_SIZE {
            return Err(MediaError::Invalid(format!(
                "chunk exceeds {} bytes",
                chunker::MAX_SIZE
            )));
        }
        let actual = LfsDigest::sha256_of(&body).hex().to_owned();
        if actual != chunk_hash {
            return Err(MediaError::Invalid(
                "chunk SHA-256 does not match the declared hash".to_string(),
            ));
        }
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        if self.exists(&key).await? {
            let existing = self.read_existing_chunk(&key).await?;
            if LfsDigest::sha256_of(&existing).hex() != chunk_hash {
                return Err(MediaError::Conflict(
                    "existing chunk content does not match the declared hash".to_string(),
                ));
            }
            return Ok(());
        }
        self.put_bytes(&key, body).await
    }

    pub(crate) async fn get_chunk_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        now: u64,
    ) -> Result<Bytes, MediaError> {
        let members = self.membership_for_pending(scope, manifest_id, now).await?;
        if !members.contains(chunk_hash) {
            return Err(MediaError::NotFound);
        }
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        let bytes = self.read_existing_chunk(&key).await?;
        if LfsDigest::sha256_of(&bytes).hex() != chunk_hash {
            return Err(MediaError::Conflict(
                "stored chunk content does not match the declared hash".to_string(),
            ));
        }
        Ok(bytes)
    }

    /// Scoped chunk read for finalize (MF-03): membership comes from the sealed
    /// entry index — does **not** reload the pending envelope.
    pub(crate) async fn read_chunk_for_entry(
        &self,
        scope: &MediaScope,
        chunk_hash: &str,
        expected_length: u64,
    ) -> Result<Bytes, MediaError> {
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        let bytes = self.read_existing_chunk(&key).await?;
        if bytes.len() as u64 != expected_length {
            return Err(MediaError::Invalid(
                "chunk length mismatch during finalize".to_string(),
            ));
        }
        if LfsDigest::sha256_of(&bytes).hex() != chunk_hash {
            return Err(MediaError::Invalid(
                "chunk hash mismatch during finalize".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) async fn require_active_pending(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        now: u64,
    ) -> Result<PendingSession, MediaError> {
        let key = scope_key(scope, MediaObjectKind::Pending, manifest_id)?;
        let session = self.load_pending(&key).await?;
        if is_expired(session.created_at_unix, now) {
            return Err(MediaError::Invalid("media session expired".to_string()));
        }
        Ok(session)
    }

    /// Require a sealed (or already finalized) paging session for finalize.
    pub(crate) async fn require_sealed_session(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<crate::callisto::media_session::Model, MediaError> {
        let session = self
            .paging
            .get_session(&scope.digest(), manifest_id)
            .await
            .map_err(map_paging)?;
        if session.state != STATE_SEALED && session.state != STATE_FINALIZED {
            return Err(MediaError::Invalid(
                "finalize requires a sealed session".to_string(),
            ));
        }
        Ok(session)
    }

    async fn require_finalized_session(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<crate::callisto::media_session::Model, MediaError> {
        let session = self
            .paging
            .get_session(&scope.digest(), manifest_id)
            .await
            .map_err(map_paging)?;
        if session.state != STATE_FINALIZED {
            return Err(MediaError::NotFound);
        }
        Ok(session)
    }

    /// Pending membership with TTL enforced on every hit (AC / R-MF-04).
    async fn membership_for_pending(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        now: u64,
    ) -> Result<MembershipRecord, MediaError> {
        let key = CacheKey {
            scope_digest: scope.digest(),
            manifest_id: manifest_id.to_owned(),
        };
        if let Some(cached) = self.membership.get(&key)
            && let Some(created) = cached.pending_created_at
        {
            if is_expired(created, now) {
                return Err(MediaError::Invalid("media session expired".to_string()));
            }
            return Ok(cached);
        }
        let session = self.require_active_pending(scope, manifest_id, now).await?;
        let record = MembershipRecord::from_chunks(
            session
                .manifest
                .chunks
                .iter()
                .map(|c| (c.chunk_hash.as_str(), c.length)),
            Some(session.created_at_unix),
        );
        self.membership.insert(key, record.clone());
        Ok(record)
    }

    async fn membership_for_finalized(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<MembershipRecord, MediaError> {
        let key = CacheKey {
            scope_digest: scope.digest(),
            manifest_id: manifest_id.to_owned(),
        };
        if let Some(cached) = self.membership.get(&key)
            && cached.pending_created_at.is_none()
        {
            return Ok(cached);
        }
        let entries = self
            .paging
            .list_entries(&scope.digest(), manifest_id)
            .await
            .map_err(map_paging)?;
        let record = MembershipRecord::from_chunks(
            entries
                .iter()
                .map(|e| (e.chunk_hash.as_str(), e.length as u64)),
            None,
        );
        self.membership.insert(key, record.clone());
        Ok(record)
    }

    async fn load_finalized_page_item(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        page_no: u32,
    ) -> Result<FinalizedPageItem, MediaError> {
        let page_id = page_object_id(manifest_id, page_no);
        let key = scope_key(scope, MediaObjectKind::Page, &page_id)?;
        let bytes = self.read_bytes(&key, MAX_ENVELOPE_SIZE).await?;
        let page: ManifestPage =
            serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))?;
        if page.page_no != page_no {
            return Err(MediaError::Conflict(
                "stored page_no does not match requested page".into(),
            ));
        }
        page.validate().map_err(map_manifest)?;
        let (offset_start, offset_end) = page_byte_range(&page.entries)?;
        Ok(FinalizedPageItem {
            page_no,
            offset_start,
            offset_end,
            entries: page.entries,
        })
    }

    async fn page_intersects_range(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        page_no: u32,
        start: u64,
        end: u64,
    ) -> Result<bool, MediaError> {
        let session = self
            .paging
            .get_session(&scope.digest(), manifest_id)
            .await
            .map_err(map_paging)?;
        if page_no as i32 >= session.page_count {
            return Ok(false);
        }
        let item = self
            .load_finalized_page_item(scope, manifest_id, page_no)
            .await?;
        Ok(item.offset_end > start && item.offset_start < end)
    }

    /// Ordered unique missing hashes (C-03 / batch_exists). Exists probes run
    /// with concurrency ≤ [`EXISTS_CONCURRENCY`].
    async fn missing_chunks_unique(
        &self,
        scope: &MediaScope,
        chunks: &[ChunkEntry],
    ) -> Result<Vec<String>, MediaError> {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for chunk in chunks {
            if seen.insert(chunk.chunk_hash.as_str()) {
                unique.push(chunk.chunk_hash.clone());
            }
        }
        let flags = self.exists_many(scope, &unique).await?;
        Ok(unique
            .into_iter()
            .zip(flags)
            .filter_map(|(h, exists)| if exists { None } else { Some(h) })
            .collect())
    }

    async fn exists_many(
        &self,
        scope: &MediaScope,
        hashes: &[String],
    ) -> Result<Vec<bool>, MediaError> {
        let scope = scope.clone();
        let store = self.clone();
        stream::iter(hashes.iter().cloned())
            .map(|hash| {
                let scope = scope.clone();
                let store = store.clone();
                async move {
                    let key = scope_key(&scope, MediaObjectKind::Chunk, &hash)?;
                    store.exists(&key).await
                }
            })
            .buffered(EXISTS_CONCURRENCY)
            .try_collect()
            .await
    }

    async fn load_pending(&self, key: &ObjectKey) -> Result<PendingSession, MediaError> {
        let bytes = self.read_bytes(key, MAX_ENVELOPE_SIZE).await?;
        serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))
    }

    async fn read_existing_chunk(&self, key: &ObjectKey) -> Result<Bytes, MediaError> {
        match self.read_bytes(key, chunker::MAX_SIZE).await {
            Ok(bytes) => Ok(bytes),
            Err(MediaError::Invalid(_)) => Err(MediaError::Conflict(
                "existing chunk content does not match the declared hash".to_string(),
            )),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn exists(&self, key: &ObjectKey) -> Result<bool, MediaError> {
        self.store.inner.exists(key).await.map_err(map_store)
    }

    pub(crate) async fn put_bytes(&self, key: &ObjectKey, bytes: Bytes) -> Result<(), MediaError> {
        let meta = ObjectMeta {
            size: bytes.len() as i64,
            ..ObjectMeta::default()
        };
        self.store
            .inner
            .put_stream_bounded(key, bytes.into_stream(), meta)
            .await
            .map_err(map_store)
    }

    /// ≤1 MiB complete-object metadata PUT (pages, pending, finalized, by-media).
    pub(crate) async fn put_metadata(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
    ) -> Result<(), MediaError> {
        let meta = ObjectMeta {
            size: bytes.len() as i64,
            ..ObjectMeta::default()
        };
        self.store
            .inner
            .put_metadata_atomic(key, bytes, meta)
            .await
            .map_err(map_store)
    }

    pub(crate) async fn read_bytes(
        &self,
        key: &ObjectKey,
        max: usize,
    ) -> Result<Bytes, MediaError> {
        let (mut stream, meta) = self.store.inner.get_stream(key).await.map_err(map_store)?;
        if meta.size > max as i64 {
            return Err(MediaError::Invalid(
                "stored media object exceeds size limit".to_string(),
            ));
        }
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.try_next().await? {
            let projected = buf.len().saturating_add(chunk.len());
            if projected > max {
                return Err(MediaError::Invalid(
                    "stored media object exceeds size limit".to_string(),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    pub(crate) fn object_store(&self) -> &MegaObjectStorageWrapper {
        &self.store
    }
}

#[cfg(test)]
impl MediaService {
    pub(crate) async fn overwrite_chunk_for_test(
        &self,
        scope: &MediaScope,
        chunk_hash: &str,
        body: Bytes,
    ) -> Result<(), MediaError> {
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        self.put_bytes(&key, body).await
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_expired(created_at_unix: u64, now: u64) -> bool {
    now.saturating_sub(created_at_unix) >= PENDING_TTL_SECS
}

fn encode_pending(session: &PendingSession) -> Result<Bytes, MediaError> {
    let bytes = serde_json::to_vec(session).map_err(|e| MediaError::Json(e.to_string()))?;
    if bytes.len() > MAX_ENVELOPE_SIZE {
        return Err(MediaError::Invalid(
            "pending manifest exceeds size limit".to_string(),
        ));
    }
    Ok(Bytes::from(bytes))
}

fn page_object_id(manifest_id: &str, page_no: u32) -> String {
    format!("{manifest_id}-p{page_no}")
}

fn encode_missing_cursor(cur: &MissingCursorV1) -> Result<String, MediaError> {
    let bytes = serde_json::to_vec(cur).map_err(|e| MediaError::Json(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(&bytes))
}

fn decode_missing_cursor(raw: &str) -> Result<MissingCursorV1, MediaError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw.as_bytes())
        .map_err(|_| MediaError::Invalid("invalid missing cursor".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| MediaError::Invalid("invalid missing cursor".into()))
}

fn encode_pages_cursor(cur: &PagesCursorV1) -> Result<String, MediaError> {
    let bytes = serde_json::to_vec(cur).map_err(|e| MediaError::Json(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(&bytes))
}

fn decode_pages_cursor(raw: &str) -> Result<PagesCursorV1, MediaError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw.as_bytes())
        .map_err(|_| MediaError::Invalid("invalid pages cursor".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| MediaError::Invalid("invalid pages cursor".into()))
}

fn page_byte_range(entries: &[ChunkEntry]) -> Result<(u64, u64), MediaError> {
    let Some(first) = entries.first() else {
        return Ok((0, 0));
    };
    let last = entries.last().expect("non-empty");
    let end = last
        .offset
        .checked_add(last.length)
        .ok_or_else(|| MediaError::Invalid("page offset overflow".into()))?;
    Ok((first.offset, end))
}

fn ensure_envelope<T: Serialize>(value: &T) -> Result<(), MediaError> {
    let encoded = serde_json::to_vec(value).map_err(|e| MediaError::Json(e.to_string()))?;
    if encoded.len() > MAX_ENVELOPE_SIZE {
        return Err(MediaError::Invalid(
            "pages response exceeds envelope".into(),
        ));
    }
    Ok(())
}

pub(crate) fn scope_key(
    scope: &MediaScope,
    kind: MediaObjectKind,
    object_id: &str,
) -> Result<ObjectKey, MediaError> {
    scope.object_key(kind, object_id).map_err(map_scope)
}

fn map_manifest(err: ManifestError) -> MediaError {
    match err {
        ManifestError::Invalid(msg) => MediaError::Invalid(msg),
        ManifestError::Serde(msg) => MediaError::Json(msg),
    }
}

fn map_scope(err: ScopeError) -> MediaError {
    match err {
        ScopeError::Actor | ScopeError::Repository | ScopeError::ObjectId => {
            MediaError::Invalid("invalid media scope or object id".to_string())
        }
    }
}

fn map_paging(err: MediaPagingError) -> MediaError {
    match err {
        MediaPagingError::NotFound => MediaError::NotFound,
        MediaPagingError::Conflict(msg) => MediaError::Conflict(msg),
        MediaPagingError::Storage(_) => MediaError::Storage,
        MediaPagingError::StaleLease => MediaError::Conflict("stale media lease".into()),
    }
}

pub(crate) fn map_store(err: IoOrbitError) -> MediaError {
    let _ = redact_storage_error(&err);
    if err.is_not_found() {
        MediaError::NotFound
    } else {
        MediaError::Storage
    }
}
