use super::*;

#[async_trait::async_trait]
impl LogStorage for ObjectStoreAdapter {
    async fn append(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        key.validate()?;
        // Fast path for single writer/single thread (optimized here):
        // - No conditional writes, no retries, no cleanup (no concurrent write conflicts)
        // - Single manifest read (treat non-existent as empty), single segment write, single manifest overwrite

        let buf = Self::buffer_stream(data, MAX_APPEND_BYTES).await?;
        let total_len = buf.len() as u64;
        if total_len == 0 {
            return Ok(());
        }

        let mut manifest = self.load_manifest(key).await?;
        let mut current_offset = manifest.len;

        // If data exceeds MAX_SEGMENT_SIZE, split into multiple segments
        let mut remaining = buf.as_ref();
        while !remaining.is_empty() {
            let segment_size = remaining.len().min(MAX_SEGMENT_SIZE as usize);
            let segment_data = Bytes::copy_from_slice(&remaining[..segment_size]);
            remaining = &remaining[segment_size..];

            let segment_start = current_offset;
            let segment_end = current_offset + segment_size as u64;

            let seg_key = log_segment_key(key, segment_start, segment_end);
            let seg_storage_key = seg_key.key.clone();

            let stream_from_buf =
                stream::once(async move { Ok::<Bytes, std::io::Error>(segment_data) });
            self.put_stream(&seg_key, Box::pin(stream_from_buf), ObjectMeta::default())
                .await?;

            manifest.segments.push(LogSegmentMeta {
                offset: segment_start,
                len: segment_size as u64,
                key: seg_storage_key,
            });
            current_offset = segment_end;
        }

        manifest.len = current_offset;

        // Overwrite manifest (single writer doesn't need conditional write)
        let mkey = log_manifest_key(key);
        let path = mkey.to_object_store_path();
        let bytes = serde_json::to_vec(&manifest)?;
        let opts = PutOptions::from(PutMode::Overwrite);
        self.to_store()
            .put_opts(&path, PutPayload::from_bytes(Bytes::from(bytes)), opts)
            .await
            .map(|_| ())
            .map_err(IoOrbitError::from)?;
        Ok(())
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> OrbitResult<ObjectByteStream> {
        key.validate()?;
        let m = self.load_manifest(key).await?;
        m.validate()?;
        let want_end = offset.saturating_add(length).min(m.len);
        if offset >= want_end {
            let s = stream::once(async { Ok::<Bytes, std::io::Error>(Bytes::new()) });
            return Ok(Box::pin(s));
        }
        enforce_read_len(want_end - offset)?;
        let mut out = BytesMut::new();
        for seg in &m.segments {
            let seg_end = seg.offset.saturating_add(seg.len);
            let overlap_start = offset.max(seg.offset);
            let overlap_end = want_end.min(seg_end);
            if overlap_start >= overlap_end {
                continue;
            }
            let seg_key = ObjectKey {
                namespace: key.namespace,
                key: seg.key.clone(),
            };
            let (mut strm, _) = self
                .get_range_stream(
                    &seg_key,
                    overlap_start - seg.offset,
                    Some(overlap_end - seg.offset),
                )
                .await?;
            while let Some(chunk) = strm.next().await {
                let c = chunk.map_err(IoOrbitError::Io)?;
                out.extend_from_slice(&c);
            }
        }
        let bytes = out.freeze();
        let s = stream::once(async move { Ok::<Bytes, std::io::Error>(bytes) });
        Ok(Box::pin(s))
    }

    async fn read_lines_range(
        &self,
        key: &ObjectKey,
        start_line: u64,
        end_line: u64,
    ) -> OrbitResult<ObjectByteStream> {
        key.validate()?;
        // Empty or reversed range, return empty stream directly
        if start_line >= end_line {
            let s = stream::once(async { Ok::<Bytes, std::io::Error>(Bytes::new()) });
            return Ok(Box::pin(s));
        }

        let m = self.load_manifest(key).await?;
        m.validate()?;
        if m.len == 0 {
            let s = stream::once(async { Ok::<Bytes, std::io::Error>(Bytes::new()) });
            return Ok(Box::pin(s));
        }

        let mut out = BytesMut::new();
        let mut current_line: u64 = 0;
        let mut partial_line: BytesMut = BytesMut::new();

        'outer: for seg in &m.segments {
            let seg_key = ObjectKey {
                namespace: key.namespace,
                key: seg.key.clone(),
            };
            let (mut strm, _) = self.get_stream(&seg_key).await?;

            while let Some(chunk) = strm.next().await {
                let chunk = chunk.map_err(IoOrbitError::Io)?;

                // Process the chunk in slices split by '\n' to avoid
                // per-byte push overhead while preserving line semantics.
                for part in chunk.split_inclusive(|&b| b == b'\n') {
                    if part.is_empty() {
                        continue;
                    }

                    let ends_with_nl = part[part.len() - 1] == b'\n';
                    // Bound total buffered read memory (accumulated output plus
                    // the in-progress line, including this part) BEFORE growing it,
                    // so even a very long unterminated line cannot blow past the cap.
                    enforce_read_len((out.len() + partial_line.len() + part.len()) as u64)?;
                    partial_line.extend_from_slice(part);

                    if ends_with_nl {
                        // Complete line (including newline)
                        if current_line >= start_line && current_line < end_line {
                            out.extend_from_slice(&partial_line);
                        }
                        current_line += 1;
                        partial_line.clear();

                        if current_line >= end_line {
                            break 'outer;
                        }
                    }
                }
            }
        }

        // If the last line doesn't end with '\n', treat it as a line
        if !partial_line.is_empty() && current_line >= start_line && current_line < end_line {
            out.extend_from_slice(&partial_line);
        }

        let bytes = out.freeze();
        let s = stream::once(async move { Ok::<Bytes, std::io::Error>(bytes) });
        Ok(Box::pin(s))
    }

    async fn append_concurrently(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        key.validate()?;
        // The local backend has no conditional-write (CAS) support, so this
        // method cannot be made safe under contention there (it would fall back
        // to an overwrite that can lose concurrent updates). Fail loudly instead
        // of silently risking data loss; single-writer local logs should use
        // `append`.
        if matches!(self.store, BackendStore::Local(_)) {
            return Err(IoOrbitError::Other(
                "append_concurrently is not supported on the local backend \
                 (no conditional-write/CAS support); use append() for \
                 single-writer local logs, or an S3/GCS backend for concurrent \
                 appends"
                    .to_string(),
            ));
        }
        const MAX_RETRIES: usize = 32;
        const BASE_DELAY_MS: u64 = 10;
        const MAX_DELAY_MS: u64 = 1000;

        let buf = Self::buffer_stream(data, MAX_APPEND_BYTES).await?;
        let total_len = buf.len() as u64;
        if total_len == 0 {
            return Ok(());
        }

        // If data exceeds MAX_SEGMENT_SIZE, split into multiple segments
        // For concurrent scenarios, we need to handle each segment in the retry loop
        let segments: Vec<Bytes> = if total_len > MAX_SEGMENT_SIZE {
            let mut segments = Vec::new();
            let mut remaining = buf.as_ref();
            while !remaining.is_empty() {
                let segment_size = remaining.len().min(MAX_SEGMENT_SIZE as usize);
                segments.push(Bytes::copy_from_slice(&remaining[..segment_size]));
                remaining = &remaining[segment_size..];
            }
            segments
        } else {
            vec![buf]
        };

        for attempt in 0..MAX_RETRIES {
            let (mut manifest, ver) = self.read_log_manifest_with_version(key).await?;
            let mut current_offset = manifest.len;
            let mut segment_keys = Vec::new();

            // Write all segments
            for segment_data in &segments {
                let segment_len = segment_data.len() as u64;
                let segment_start = current_offset;
                let segment_end = current_offset + segment_len;

                let seg_key = log_segment_key(key, segment_start, segment_end);
                let seg_storage_key = seg_key.key.clone();
                let seg_key_clone = seg_key.clone();
                segment_keys.push((seg_key_clone, seg_storage_key, segment_start, segment_len));

                let stream_from_buf = stream::once({
                    let segment_data = segment_data.clone();
                    async move { Ok::<Bytes, std::io::Error>(segment_data) }
                });
                self.put_stream(&seg_key, Box::pin(stream_from_buf), ObjectMeta::default())
                    .await?;

                current_offset = segment_end;
            }

            // Update manifest
            for (_, seg_storage_key, seg_start, seg_len) in &segment_keys {
                manifest.segments.push(LogSegmentMeta {
                    offset: *seg_start,
                    len: *seg_len,
                    key: seg_storage_key.clone(),
                });
            }
            manifest.len = current_offset;

            match self
                .write_log_manifest_conditional(key, &manifest, ver)
                .await
            {
                Ok(()) => return Ok(()),
                Err(IoOrbitError::WriteManifestPreconditionFailed) => {
                    // Concurrent write conflict: delete all newly written segments to avoid orphaned objects, then retry
                    for (seg_key, _, _, _) in &segment_keys {
                        let _ = self.delete(seg_key).await;
                    }

                    // Exponential backoff: delay = min(BASE_DELAY_MS * 2^attempt, MAX_DELAY_MS)
                    // Add small random jitter (time-based) to avoid thundering herd effect
                    if attempt < MAX_RETRIES - 1 {
                        let delay_ms =
                            (BASE_DELAY_MS * (1u64 << attempt.min(10))).min(MAX_DELAY_MS);
                        // Add 0-10ms jitter derived from current time, so different
                        // callers are less likely to pick the same delay.
                        let jitter_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.subsec_millis() as u64 % 11)
                            .unwrap_or(0);
                        tokio::time::sleep(Duration::from_millis(delay_ms + jitter_ms)).await;
                    }
                    continue;
                }
                Err(e) => {
                    // Delete all written segments
                    for (seg_key, _, _, _) in &segment_keys {
                        let _ = self.delete(seg_key).await;
                    }
                    return Err(e);
                }
            }
        }

        Err(IoOrbitError::Other(
            "log manifest precondition failed (retry exceeded)".to_string(),
        ))
    }

    async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest> {
        key.validate()?;
        let mkey = log_manifest_key(key);
        let path = mkey.to_object_store_path();

        let mut s = match self.to_store().get(&path).await {
            Ok(r) => r.into_stream(),
            Err(object_store::Error::NotFound { .. }) => {
                return Ok(LogManifest {
                    len: 0,
                    segments: Vec::new(),
                });
            }
            Err(e) => return Err(IoOrbitError::from(e)),
        };

        let mut buf = BytesMut::new();
        while let Some(chunk) = s.next().await {
            let c = chunk
                .map_err(std::io::Error::other)
                .map_err(IoOrbitError::Io)?;
            buf.extend_from_slice(&c);
        }
        let bytes = buf.freeze();
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        key.validate()?;
        let mkey = log_manifest_key(key);
        self.exists(&mkey).await
    }
}
