//! Sequential TreeFrame emission with identity/zstd negotiation (spec 06).
//!
//! One [`FrameStream`] owns the stream_id and the consecutive sequence
//! counter for a single HTTP response. `encoding=zstd` compresses each
//! compressible frame but falls back to identity whenever compression does
//! not shrink the wire payload — the header flags tell the client which
//! encoding that frame actually used, so a mixed identity/zstd stream is
//! legal. END frames are always identity (spec 06 forbids compressing them).

use mst2_codec::treeframe::{ChunkPayload, EndPayload, MetaPayload, ObjectPayload};

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// Negotiated per-request encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Zstd,
}

impl Encoding {
    pub fn parse(s: &str) -> Result<Self, SnapshotError> {
        match s {
            "identity" | "" => Ok(Encoding::Identity),
            "zstd" => Ok(Encoding::Zstd),
            other => Err(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                format!("unsupported frame encoding: {other}"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Zstd => "zstd",
        }
    }
}

pub struct FrameStream {
    stream_id: u32,
    sequence: u64,
    encoding: Encoding,
}

impl FrameStream {
    pub fn new(stream_id: u32, encoding: Encoding) -> Self {
        FrameStream {
            stream_id,
            sequence: 0,
            encoding,
        }
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.sequence;
        self.sequence += 1;
        s
    }

    /// Pick the smaller of the identity and (when negotiated) zstd frame.
    fn choose(
        &self,
        identity: Vec<u8>,
        zstd: Option<Result<Vec<u8>, mst2_codec::CodecError>>,
    ) -> Vec<u8> {
        match zstd {
            Some(Ok(wire)) if self.encoding == Encoding::Zstd && wire.len() < identity.len() => {
                wire
            }
            // Compression failure or no gain: keep serving identity; a
            // compression problem never blocks a valid identity response.
            _ => identity,
        }
    }

    /// Emit an OBJECT frame, compressing when it shrinks the wire.
    pub fn object(&mut self, objects: Vec<([u8; 32], Vec<u8>)>) -> Result<Vec<u8>, SnapshotError> {
        let payload = ObjectPayload { objects };
        let seq = self.next_seq();
        let identity = payload
            .encode(self.stream_id, seq)
            .map_err(|e| codec_err("OBJECT", e))?;
        let zstd = if self.encoding == Encoding::Zstd {
            Some(payload.encode_zstd(self.stream_id, seq))
        } else {
            None
        };
        Ok(self.choose(identity, zstd))
    }

    /// Emit a META frame, compressing when it shrinks the wire.
    pub fn meta(&mut self, pages: Vec<([u8; 32], Vec<u8>)>) -> Result<Vec<u8>, SnapshotError> {
        let payload = MetaPayload { pages };
        let seq = self.next_seq();
        let identity = payload
            .encode(self.stream_id, seq)
            .map_err(|e| codec_err("META", e))?;
        let zstd = if self.encoding == Encoding::Zstd {
            Some(payload.encode_zstd(self.stream_id, seq))
        } else {
            None
        };
        Ok(self.choose(identity, zstd))
    }

    /// Emit a CHUNK frame, compressing when it shrinks the wire.
    pub fn chunk(
        &mut self,
        map_id: [u8; 32],
        file_content_id: [u8; 32],
        chunk_index: u64,
        chunk_bytes: Vec<u8>,
    ) -> Result<Vec<u8>, SnapshotError> {
        let payload = ChunkPayload {
            map_id,
            file_content_id,
            chunk_index,
            chunk_bytes,
        };
        let seq = self.next_seq();
        let identity = payload
            .encode(self.stream_id, seq)
            .map_err(|e| codec_err("CHUNK", e))?;
        let zstd = if self.encoding == Encoding::Zstd {
            Some(payload.encode_zstd(self.stream_id, seq))
        } else {
            None
        };
        Ok(self.choose(identity, zstd))
    }

    /// Terminating END frame — always identity (spec 06). Uses the next
    /// sequence so the whole stream stays consecutive.
    pub fn end(
        &mut self,
        request_item_count: u32,
        unique_unit_count: u32,
        logical_bytes: u64,
        request_body_sha256: [u8; 32],
    ) -> Vec<u8> {
        let seq = self.next_seq();
        EndPayload {
            request_item_count,
            unique_unit_count,
            logical_bytes,
            request_body_sha256,
        }
        .encode(self.stream_id, seq)
    }
}

fn codec_err(kind: &str, e: mst2_codec::CodecError) -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::Internal,
        format!("{kind} frame encode failed: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use mst2_codec::treeframe::HEADER_LEN;
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn compressed_service_frames_commit_to_the_raw_payload() {
        let object = vec![b'x'; 4096];
        let content_id: [u8; 32] = Sha256::digest(&object).into();
        let mut identity = FrameStream::new(13, Encoding::Identity);
        let mut compressed = FrameStream::new(13, Encoding::Zstd);
        let raw = identity.object(vec![(content_id, object.clone())]).unwrap();
        let wire = compressed.object(vec![(content_id, object)]).unwrap();
        let raw_digest: [u8; 32] = Sha256::digest(&raw[HEADER_LEN..]).into();
        let wire_digest: [u8; 32] = Sha256::digest(&wire[HEADER_LEN..]).into();
        assert_eq!(wire[7], 1);
        assert_eq!(wire[32..64], raw_digest);
        assert_ne!(wire[32..64], wire_digest);

        let mut legacy = wire.clone();
        legacy[32..64].copy_from_slice(&wire_digest);
        assert!(mst2_codec::treeframe::parse_frame(&legacy).is_err());
        let end = compressed.end(1, 1, 4096, [4; 32]);
        assert_eq!(end[7], 0);
        let mut stream = wire;
        stream.extend(end);
        assert_eq!(
            mst2_codec::treeframe::parse_stream(&stream).unwrap().len(),
            2
        );
    }
}
