//! Whole-file raw delivery from one current-OID stream, authenticated in chunks.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use mst2_codec::chunkmap::{CHUNK_SIZE, CHUNKS_PER_PAGE};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    REQUEST_HEADERS, content, ensure_enabled, internal, mst2_error_response, revalidate_access,
    revalidate_request,
};
use crate::{
    api::MonoApiServiceState,
    ceres::snapshot::{
        content_budget::{
            MemoryBudget, MemoryLease, RANGE_WORK_BYTES, range_budget, response_budget,
        },
        error::{SnapshotError, SnapshotErrorCode},
        pages::hex_of,
        runtime::SnapshotContext,
        view::validate_scope_relative_path,
    },
    jupiter::storage::{
        Storage,
        native_chunk_map::{AuthenticatedChunkPage, PersistedChunkMap},
    },
    orbit_api::object_storage::ObjectByteStream,
};

const PRODUCER_ITEM_MAX: usize = 8 * 1024 * 1024;

#[derive(Deserialize, Debug)]
pub(super) struct BlobQuery {
    path: String,
    #[serde(default)]
    expected_digest: Option<String>,
}

pub(super) struct BlobBudgets {
    pub(super) scratch: Arc<MemoryBudget>,
    pub(super) response: Arc<MemoryBudget>,
}

impl Default for BlobBudgets {
    fn default() -> Self {
        Self {
            scratch: range_budget().clone(),
            response: response_budget().clone(),
        }
    }
}

pub(super) async fn blob(
    state: State<MonoApiServiceState>,
    path: AxumPath<String>,
    query: Query<BlobQuery>,
    headers: HeaderMap,
) -> Response {
    match blob_with_budgets(state, path, query, headers, BlobBudgets::default()).await {
        Ok(response) | Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
pub(super) async fn blob_with_budgets(
    state: State<MonoApiServiceState>,
    AxumPath(snapshot_id): AxumPath<String>,
    Query(query): Query<BlobQuery>,
    headers: HeaderMap,
    budgets: BlobBudgets,
) -> Result<Response, Response> {
    ensure_enabled(&state).map_err(mst2_error_response)?;
    let context = super::request_context(&state, &snapshot_id).map_err(mst2_error_response)?;
    validate_scope_relative_path(&query.path).map_err(mst2_error_response)?;
    if headers.contains_key("range") {
        return Err(mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::RangeNotSupported,
            "raw blob reads are whole-file; use chunks for ranges",
        )));
    }
    let handler = state
        .api_handler(std::path::Path::new("/"))
        .await
        .map_err(internal)?;
    let file = content::resolve_snapshot_file_metadata(
        &state,
        &context,
        &query.path,
        query.expected_digest.as_deref(),
    )
    .await?;
    let scratch = budgets
        .scratch
        .reserve(if file.size == 0 { 0 } else { RANGE_WORK_BYTES })
        .map_err(mst2_error_response)?;
    let first_credit = budgets
        .response
        .reserve(file.size.min(CHUNK_SIZE as u64) as usize)
        .map_err(mst2_error_response)?;
    revalidate_request(&state, &context)
        .await
        .map_err(mst2_error_response)?;
    let map = if file.size == 0 {
        if file.digest != <[u8; 32]>::from(Sha256::digest([])) {
            return Err(mst2_error_response(integrity(
                "empty raw source has a nonempty verified digest",
            )));
        }
        None
    } else {
        Some(content::project_resolved(handler.as_ref(), &file).await?)
    };
    let storage = handler.get_context();
    let page = if let Some(map) = map.as_ref() {
        Some(
            storage
                .chunk_maps()
                .await
                .map_err(mst2_error_response)?
                .selected_page(map, 0)
                .await
                .map_err(mst2_error_response)?,
        )
    } else {
        None
    };
    if let Some(map) = map.as_ref() {
        map.ensure_live().await.map_err(mst2_error_response)?;
    }
    let open = handler.get_raw_blob_stream_with_meta(&file.oid);
    let opened = if let Some(map) = map.as_ref() {
        map.await_backend(open).await.map_err(mst2_error_response)?
    } else {
        open.await
    };
    let (input, meta) =
        opened.map_err(|error| mst2_error_response(content::content_read_error(error)))?;
    if let Some(map) = map.as_ref() {
        map.ensure_live().await.map_err(mst2_error_response)?;
    }
    if u64::try_from(meta.size).ok() != Some(file.size) {
        return Err(mst2_error_response(integrity(
            "raw source physical size disagrees with its fixed verified fact",
        )));
    }
    let headers = REQUEST_HEADERS.try_with(Clone::clone).map_err(|_| {
        mst2_error_response(SnapshotError::new(
            SnapshotErrorCode::Unauthenticated,
            "request authentication context missing",
        ))
    })?;
    revalidate_access(&state, &context, &headers)
        .await
        .map_err(mst2_error_response)?;
    let digest = file.digest;
    let size = file.size;
    let kind = content::fs_kind_str(file.fs_kind);
    let mut reader = RawBlobReader {
        state: state.0,
        context,
        headers,
        storage,
        map,
        page,
        input: Some(input),
        pending: Bytes::new(),
        pending_offset: 0,
        received: 0,
        index: 0,
        size,
        digest,
        hash: Sha256::new(),
        first_credit: Some(first_credit),
        response_budget: budgets.response,
        _scratch: scratch,
    };
    let body = if size == 0 {
        reader.require_eof().await.map_err(mst2_error_response)?;
        reader.validate().await.map_err(mst2_error_response)?;
        Body::empty()
    } else {
        let stream = futures::stream::try_unfold(reader, |mut reader| async move {
            let Some(bytes) = reader.next_chunk().await? else {
                return Ok::<_, SnapshotError>(None);
            };
            Ok(Some((bytes, reader)))
        });
        Body::from_stream(stream)
    };
    Response::builder()
        .header("etag", format!("\"sha256:{}\"", hex_of(&digest)))
        .header("content-length", size.to_string())
        .header("x-mega-content-size", size.to_string())
        .header("x-mega-fs-kind", kind)
        .header("cache-control", "private, no-cache, no-transform")
        .header("vary", "Authorization, Accept")
        .body(body)
        .map_err(|_| mst2_error_response(internal("raw blob response build failed")))
}

struct RawBlobReader {
    state: MonoApiServiceState,
    context: SnapshotContext,
    headers: HeaderMap,
    storage: Storage,
    map: Option<Arc<PersistedChunkMap>>,
    page: Option<Arc<AuthenticatedChunkPage>>,
    input: Option<ObjectByteStream>,
    pending: Bytes,
    pending_offset: usize,
    received: u64,
    index: u64,
    size: u64,
    digest: [u8; 32],
    hash: Sha256,
    first_credit: Option<MemoryLease>,
    response_budget: Arc<MemoryBudget>,
    _scratch: MemoryLease,
}

struct RawBlobChunk {
    bytes: Vec<u8>,
    _credit: MemoryLease,
    _map: Arc<PersistedChunkMap>,
}

impl AsRef<[u8]> for RawBlobChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl RawBlobReader {
    async fn validate(&mut self) -> Result<(), SnapshotError> {
        revalidate_access(&self.state, &self.context, &self.headers).await?;
        if let Some(map) = self.map.as_ref() {
            map.ensure_live().await?;
        }
        Ok(())
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>, SnapshotError> {
        self.validate().await?;
        let map = self
            .map
            .as_ref()
            .ok_or_else(|| integrity("nonempty raw source has no authenticated map"))?;
        if self.index == map.map.chunk_count {
            return Ok(None);
        }
        let length = map
            .map
            .chunk_len(self.index)
            .map_err(|_| integrity("raw source chunk index disagrees with its map"))?
            as usize;
        let page_index = self.index / CHUNKS_PER_PAGE as u64;
        if self.page.as_ref().map(|p| p.leaf.page_index) != Some(page_index) {
            drop(self.page.take());
            self.page = Some(
                self.storage
                    .chunk_maps()
                    .await?
                    .selected_page(map, page_index)
                    .await?,
            );
            self.validate().await?;
        }
        let credit = if let Some(credit) = self.first_credit.take() {
            credit
        } else {
            self.response_budget.reserve(length)?
        };
        let mut raw = Vec::new();
        raw.try_reserve_exact(length).map_err(|_| {
            SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "raw chunk allocation could not be admitted",
            )
        })?;
        let mut empty_parts = 0;
        while raw.len() < length {
            if self.pending_offset == self.pending.len() {
                self.pending = Bytes::new();
                self.pending_offset = 0;
                let Some(part) = self.poll_source().await? else {
                    return Err(integrity("raw source is truncated before its final chunk"));
                };
                if part.len() as u64 > self.size - self.received {
                    return Err(integrity("raw source exceeds its fixed verified size"));
                }
                if part.len() > PRODUCER_ITEM_MAX {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::TemporaryUnavailable,
                        "raw producer item exceeds its consumer processing limit",
                    ));
                }
                self.received += part.len() as u64;
                if part.is_empty() {
                    empty_parts += 1;
                    if empty_parts == 32 {
                        tokio::task::yield_now().await;
                        self.validate().await?;
                        empty_parts = 0;
                    }
                    continue;
                }
                self.pending = part;
            }
            let amount = (length - raw.len()).min(self.pending.len() - self.pending_offset);
            let end = self.pending_offset + amount;
            let bytes = &self.pending[self.pending_offset..end];
            self.hash.update(bytes);
            raw.extend_from_slice(bytes);
            self.pending_offset = end;
        }
        let map = self
            .map
            .as_ref()
            .ok_or_else(|| integrity("raw source authenticated map disappeared"))?;
        self.page
            .as_ref()
            .ok_or_else(|| integrity("raw source authenticated page disappeared"))?
            .verify_chunk(&map.map, self.index, &raw)?;
        self.index += 1;
        if self.index == map.map.chunk_count {
            self.require_eof().await?;
            let actual: [u8; 32] = self.hash.clone().finalize().into();
            if actual != self.digest {
                return Err(integrity(
                    "raw source whole-file digest disagrees with its fixed fact",
                ));
            }
        }
        self.validate().await?;
        if let Some(map) = self.map.as_ref() {
            map.record_progress().await?;
        }
        if raw.capacity() > credit.bytes {
            return Err(integrity("raw chunk allocation exceeds its owned credit"));
        }
        Ok(Some(Bytes::from_owner(RawBlobChunk {
            bytes: raw,
            _credit: credit,
            _map: self
                .map
                .as_ref()
                .cloned()
                .ok_or_else(|| integrity("raw source authenticated map disappeared"))?,
        })))
    }

    async fn poll_source(&mut self) -> Result<Option<Bytes>, SnapshotError> {
        self.validate().await?;
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| integrity("raw source stream was already closed"))?;
        let result = if let Some(map) = self.map.as_ref() {
            map.await_backend(input.next()).await?
        } else {
            input.next().await
        };
        self.validate().await?;
        let part = result.transpose().map_err(|error| {
            tracing::warn!(%error, "fixed-view raw stream failed");
            SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "fixed-view raw stream failed",
            )
        })?;
        if part.as_ref().is_some_and(|bytes| !bytes.is_empty())
            && let Some(map) = self.map.as_ref()
        {
            map.record_progress().await?;
        }
        Ok(part)
    }

    async fn require_eof(&mut self) -> Result<(), SnapshotError> {
        if self.received != self.size || self.pending_offset != self.pending.len() {
            return Err(integrity(
                "raw source final length disagrees with its fixed fact",
            ));
        }
        let mut empty_parts = 0;
        while let Some(part) = self.poll_source().await? {
            if !part.is_empty() {
                return Err(integrity(
                    "raw source has trailing bytes after its fixed size",
                ));
            }
            empty_parts += 1;
            if empty_parts == 32 {
                tokio::task::yield_now().await;
                self.validate().await?;
                empty_parts = 0;
            }
        }
        drop(self.input.take());
        self.pending = Bytes::new();
        Ok(())
    }
}

fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
