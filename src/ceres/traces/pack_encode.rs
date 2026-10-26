//! Pull-based PACK encoding of the authorized stream's original object bytes.

use bytes::Bytes;
use git_internal::{hash::HashKind, internal::object::utils::compress_zlib, utils::HashAlgorithm};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
    jupiter::service::{
        trace_fetch_service::TraceFetchStream, trace_object_service::RawObjectType,
    },
};

fn integrity() -> MegaError {
    TracesError::ObjectIntegrity.into()
}

struct RawPackEncoder {
    header: Option<Bytes>,
    digest: Option<HashAlgorithm>,
    remaining: u32,
}

impl RawPackEncoder {
    fn new(kind: HashKind, count: u64) -> Result<Self, MegaError> {
        if !matches!(kind, HashKind::Sha1 | HashKind::Sha256) {
            return Err(integrity());
        }
        let count =
            u32::try_from(count).map_err(|_| MegaError::Traces(TracesError::ResponseBudget))?;
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(b"PACK");
        header.extend_from_slice(&2u32.to_be_bytes());
        header.extend_from_slice(&count.to_be_bytes());
        let mut digest = HashAlgorithm::new_for_kind(kind);
        digest.update(&header);
        Ok(Self {
            header: Some(Bytes::from(header)),
            digest: Some(digest),
            remaining: count,
        })
    }

    fn header(&mut self) -> Result<Bytes, MegaError> {
        if self.digest.is_none() {
            return Err(integrity());
        }
        match self.header.take() {
            Some(header) => Ok(header),
            None => {
                self.abort();
                Err(integrity())
            }
        }
    }

    fn object(&mut self, kind: RawObjectType, payload: &[u8]) -> Result<Bytes, MegaError> {
        let result = self.object_inner(kind, payload);
        if result.is_err() {
            self.abort();
        }
        result
    }

    fn object_inner(&mut self, kind: RawObjectType, payload: &[u8]) -> Result<Bytes, MegaError> {
        if self.header.is_some() || self.remaining == 0 {
            return Err(integrity());
        }
        let digest = self.digest.as_mut().ok_or_else(integrity)?;
        if payload.len() as u64 > TracesProtocolLimits::MAX_OBJECT_BYTES {
            return Err(TracesError::ResponseBudget.into());
        }
        let tag = match kind {
            RawObjectType::Commit => 1,
            RawObjectType::Tree => 2,
            RawObjectType::Blob => 3,
            RawObjectType::Tag => 4,
        };
        let compressed = compress_zlib(payload).map_err(|_| integrity())?;
        let mut bytes = Vec::with_capacity(compressed.len().saturating_add(16));
        let mut size = payload.len();
        let mut byte = (tag << 4) | (size & 0x0f) as u8;
        size >>= 4;
        loop {
            bytes.push(byte | if size == 0 { 0 } else { 0x80 });
            if size == 0 {
                break;
            }
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        bytes.extend_from_slice(&compressed);
        digest.update(&bytes);
        self.remaining -= 1;
        Ok(Bytes::from(bytes))
    }

    fn finish(&mut self) -> Result<Bytes, MegaError> {
        if self.header.is_some() || self.remaining != 0 {
            self.abort();
            return Err(integrity());
        }
        Ok(Bytes::from(
            self.digest.take().ok_or_else(integrity)?.finalize(),
        ))
    }

    fn abort(&mut self) {
        self.digest.take();
    }
}

pub(crate) struct RawPackStream {
    // Rust drops fields in declaration order: close the spool before its slot.
    source: TraceFetchStream,
    encoder: RawPackEncoder,
    header_sent: bool,
    finished: bool,
    _permit: OwnedSemaphorePermit,
}

impl RawPackStream {
    pub(crate) fn new(
        source: TraceFetchStream,
        kind: HashKind,
        permit: OwnedSemaphorePermit,
    ) -> Result<Self, MegaError> {
        let encoder = match RawPackEncoder::new(kind, source.object_count()) {
            Ok(encoder) => encoder,
            Err(error) => {
                drop(source);
                drop(permit);
                return Err(error);
            }
        };
        Ok(Self {
            source,
            encoder,
            header_sent: false,
            finished: false,
            _permit: permit,
        })
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Bytes>, MegaError> {
        if self.finished {
            return Ok(None);
        }
        if !self.header_sent {
            self.header_sent = true;
            return self.encoder.header().map(Some);
        }
        let result = match self.source.next().await {
            Ok(Some(object)) => self.encoder.object(object.object_type, &object.payload),
            Ok(None) => {
                self.finished = true;
                self.encoder.finish()
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.encoder.abort();
            self.finished = true;
        }
        result.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use git_internal::internal::object::types::ObjectType;
    use sha2::Digest;

    use super::*;
    use crate::ceres::traces::pack_decode::{PackError, PackSink, decode_pack};

    #[derive(Default)]
    struct Objects {
        values: Vec<(ObjectType, Vec<u8>)>,
        expected: usize,
    }

    impl PackSink for Objects {
        fn object_header(&mut self, kind: ObjectType, size: usize) -> Result<(), PackError> {
            self.values.push((kind, Vec::new()));
            self.expected = size;
            Ok(())
        }

        fn object_chunk(&mut self, chunk: &[u8]) -> Result<(), PackError> {
            self.values
                .last_mut()
                .expect("object header")
                .1
                .extend_from_slice(chunk);
            Ok(())
        }

        fn object_end(&mut self) -> Result<(), PackError> {
            assert_eq!(self.values.last().expect("object").1.len(), self.expected);
            Ok(())
        }
    }

    fn checksum(kind: HashKind, pack: &[u8]) {
        let split = pack.len() - kind.size();
        let actual = match kind {
            HashKind::Sha1 => sha1::Sha1::digest(&pack[..split]).to_vec(),
            HashKind::Sha256 => sha2::Sha256::digest(&pack[..split]).to_vec(),
            _ => panic!("test hash kind"),
        };
        assert_eq!(&pack[split..], actual);
    }

    #[test]
    fn raw_pack_preserves_types_and_exact_payloads_for_both_hashes() {
        for hash in [HashKind::Sha1, HashKind::Sha256] {
            let objects = [
                (RawObjectType::Commit, ObjectType::Commit,
                 b"tree 0000000000000000000000000000000000000000\nauthor A <a@b> 7 +0545\ncommitter C <c@d> 8 -0330\nx-extra original\n continuation\n\nmessage without final newline".to_vec()),
                (RawObjectType::Tree, ObjectType::Tree,
                 [b"100644 odd\xffname\0".as_slice(), &vec![7; hash.size()]].concat()),
                (RawObjectType::Blob, ObjectType::Blob, (0..131_073).map(|i| (i % 251) as u8).collect()),
                (RawObjectType::Tag, ObjectType::Tag, b"object retained\ntype commit\ntag original\n\nraw\0message\xff".to_vec()),
                (RawObjectType::Blob, ObjectType::Blob, Vec::new()),
            ];
            let mut encoder = RawPackEncoder::new(hash, objects.len() as u64).expect("encoder");
            let mut pack = encoder.header().expect("header").to_vec();
            for (kind, _, payload) in &objects {
                pack.extend_from_slice(&encoder.object(*kind, payload).expect("raw object"));
            }
            pack.extend_from_slice(&encoder.finish().expect("complete checksum"));
            assert_eq!(&pack[..12], b"PACK\0\0\0\x02\0\0\0\x05");
            checksum(hash, &pack);
            let mut decoded = Objects::default();
            let summary =
                decode_pack(pack.as_slice(), hash, &mut decoded).expect("independent decoder");
            assert_eq!(summary.object_count, 5);
            assert_eq!(decoded.values.len(), objects.len());
            for ((kind, bytes), (_, expected_kind, expected_bytes)) in
                decoded.values.iter().zip(&objects)
            {
                assert_eq!(kind, expected_kind);
                assert_eq!(
                    bytes, expected_bytes,
                    "original payload must not be reserialized"
                );
            }
            assert!(encoder.finish().is_err(), "no duplicate trailer");
        }
    }

    #[test]
    fn raw_pack_zero_count_is_valid_and_count_overflow_is_rejected() {
        for hash in [HashKind::Sha1, HashKind::Sha256] {
            let mut empty = RawPackEncoder::new(hash, 0).expect("empty pack");
            let mut bytes = empty.header().expect("header").to_vec();
            bytes.extend_from_slice(&empty.finish().expect("empty checksum"));
            checksum(hash, &bytes);
            let summary = decode_pack(bytes.as_slice(), hash, &mut Objects::default())
                .expect("valid empty pack");
            assert_eq!(summary.object_count, 0);
            let mut maximum =
                RawPackEncoder::new(hash, u64::from(u32::MAX)).expect("wire count bound");
            assert_eq!(
                &maximum.header().expect("header")[8..],
                &u32::MAX.to_be_bytes()
            );
            assert!(matches!(
                RawPackEncoder::new(hash, u64::from(u32::MAX) + 1),
                Err(MegaError::Traces(TracesError::ResponseBudget))
            ));
        }
        assert!(RawPackEncoder::new(HashKind::Blake3, 0).is_err());
    }

    #[test]
    fn raw_pack_missing_extra_or_failed_objects_cannot_finish() {
        for hash in [HashKind::Sha1, HashKind::Sha256] {
            let mut missing = RawPackEncoder::new(hash, 2).expect("encoder");
            let mut bytes = missing.header().expect("header").to_vec();
            bytes.extend_from_slice(
                &missing
                    .object(RawObjectType::Blob, b"original")
                    .expect("first object"),
            );
            assert!(missing.finish().is_err());
            assert!(decode_pack(bytes.as_slice(), hash, &mut Objects::default()).is_err());
            assert!(missing.object(RawObjectType::Blob, b"late").is_err());
            assert!(missing.finish().is_err());

            let mut extra = RawPackEncoder::new(hash, 0).expect("encoder");
            extra.header().expect("header");
            assert!(extra.object(RawObjectType::Blob, b"extra").is_err());
            assert!(extra.finish().is_err());

            let mut interrupted = RawPackEncoder::new(hash, 1).expect("encoder");
            interrupted.header().expect("header");
            interrupted
                .object(RawObjectType::Blob, b"bytes")
                .expect("object");
            interrupted.abort();
            assert!(
                interrupted.finish().is_err(),
                "a source failure cannot emit a checksum"
            );
        }
    }

    #[test]
    fn raw_pack_rejects_objects_before_header_and_duplicate_headers() {
        let mut missing = RawPackEncoder::new(HashKind::Sha1, 1).expect("encoder");
        assert!(missing.object(RawObjectType::Blob, b"bytes").is_err());
        assert!(missing.header().is_err());
        let mut repeated = RawPackEncoder::new(HashKind::Sha256, 0).expect("encoder");
        repeated.header().expect("header");
        assert!(repeated.header().is_err());
        assert!(repeated.finish().is_err());
    }
}
