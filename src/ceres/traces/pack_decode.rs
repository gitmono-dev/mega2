//! Bounded structural decoding for non-delta Git PACK streams.
//!
//! Only pack structure is validated here: magic, version, declared object
//! count, per-object headers, zlib payloads, the trailer digest and the exact
//! end of the stream. Object identity verification belongs to MTR-98, so this
//! module never derives a trusted OID. The hash kind is explicit and never read
//! from a thread-local default.

use std::{
    io::{self, BufRead, Read},
    sync::{Arc, Mutex},
};

use git_internal::{
    hash::HashKind,
    internal::{object::types::ObjectType, zlib::stream::inflate::ReadBoxed},
    utils::HashAlgorithm,
};
use thiserror::Error;

/// PACK stream signature.
pub const PACK_MAGIC: &[u8; 4] = b"PACK";
/// Largest chunk handed to [`PackSink::object_chunk`].
pub const MAX_SINK_CHUNK: usize = 64 * 1024;

/// A structural PACK rejection.
#[derive(Debug, Error)]
pub enum PackError {
    #[error("pack magic must be PACK")]
    Magic,
    #[error("pack version {0} is not supported")]
    Version(u32),
    #[error("pack object type {0} is not supported")]
    ObjectType(u8),
    #[error("delta objects are not supported")]
    Delta,
    #[error("pack is truncated")]
    Truncated,
    #[error("pack trailer does not match the decoded bytes")]
    Trailer,
    #[error("pack has trailing bytes after its trailer")]
    Trailing,
    #[error("pack object size overflows the platform width")]
    SizeOverflow,
    #[error("inflated object size {actual} differs from the declared {declared}")]
    Size { declared: usize, actual: usize },
    #[error("inflate failed: {0}")]
    Inflate(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<git_internal::errors::GitError> for PackError {
    fn from(error: git_internal::errors::GitError) -> Self {
        PackError::Inflate(error.to_string())
    }
}

/// Receives one decoded object at a time. The decoder never buffers a whole
/// pack, and each chunk is at most [`MAX_SINK_CHUNK`] bytes.
pub trait PackSink {
    fn object_header(&mut self, object_type: ObjectType, size: usize) -> Result<(), PackError>;
    fn object_chunk(&mut self, chunk: &[u8]) -> Result<(), PackError>;
    fn object_end(&mut self) -> Result<(), PackError>;
}

/// What a successful decode observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackSummary {
    pub version: u32,
    pub object_count: u32,
}

/// Decode a non-delta PACK stream into `sink`.
pub fn decode_pack<R, S>(input: R, kind: HashKind, sink: &mut S) -> Result<PackSummary, PackError>
where
    R: Read,
    S: PackSink,
{
    let mut reader = HashingReader::new(input, kind);
    let source = reader.source();
    let mut header = [0u8; 12];
    read_full(&mut reader, &mut header)?;
    if &header[0..4] != PACK_MAGIC {
        return Err(PackError::Magic);
    }
    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if version != 2 && version != 3 {
        return Err(PackError::Version(version));
    }
    let declared = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    for _ in 0..declared {
        decode_object(&mut reader, kind, sink, &source)?;
    }
    let mut trailer = vec![0u8; kind.size()];
    read_full_unhashed(&mut reader, &mut trailer)?;
    let mut extra = [0u8; 1];
    if reader.read_unhashed(&mut extra)? != 0 {
        return Err(PackError::Trailing);
    }
    let digest = reader.finish();
    if digest != trailer {
        return Err(PackError::Trailer);
    }
    Ok(PackSummary {
        version,
        object_count: declared,
    })
}

fn decode_object<R, S>(
    reader: &mut R,
    kind: HashKind,
    sink: &mut S,
    source: &SourceWatch,
) -> Result<(), PackError>
where
    R: BufRead,
    S: PackSink,
{
    let (pack_type, size) = read_type_and_size(reader)?;
    let object_type = match pack_type {
        1 => ObjectType::Commit,
        2 => ObjectType::Tree,
        3 => ObjectType::Blob,
        4 => ObjectType::Tag,
        6 | 7 => return Err(PackError::Delta),
        other => return Err(PackError::ObjectType(other)),
    };
    sink.object_header(object_type, size)?;
    let mut inflater = ReadBoxed::new_with_kind(&mut *reader, object_type, size, kind)?;
    let chunk = MAX_SINK_CHUNK.min(size.max(1));
    let mut buffer = vec![0u8; chunk];
    let mut inflated = 0usize;
    loop {
        match inflater.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                inflated += read;
                if inflated > size {
                    return Err(PackError::Size {
                        declared: size,
                        actual: inflated,
                    });
                }
                sink.object_chunk(&buffer[..read])?;
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(PackError::Truncated);
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                if let Some(kind) = source.error() {
                    return Err(source_error(kind));
                }
                return Err(PackError::Inflate(error.to_string()));
            }
            Err(error) => return Err(PackError::Io(error)),
        }
    }
    if inflated < size {
        // Distinguish a truncated source from a complete stream whose declared
        // size is larger than what it actually inflates to.
        return Err(if source.eof() {
            PackError::Truncated
        } else {
            PackError::Size {
                declared: size,
                actual: inflated,
            }
        });
    }
    sink.object_end()?;
    Ok(())
}

fn source_error(kind: io::ErrorKind) -> PackError {
    match kind {
        io::ErrorKind::UnexpectedEof => PackError::Truncated,
        _ => PackError::Io(io::Error::from(kind)),
    }
}

fn read_type_and_size<R: BufRead>(reader: &mut R) -> Result<(u8, usize), PackError> {
    let mut byte = read_u8(reader)?;
    let pack_type = (byte >> 4) & 0x07;
    let mut size = u64::from(byte & 0x0f);
    let mut shift = 4u32;
    while byte & 0x80 != 0 {
        byte = read_u8(reader)?;
        let part = u64::from(byte & 0x7f);
        if shift >= 64 || part > (u64::MAX >> shift) {
            return Err(PackError::SizeOverflow);
        }
        size = size
            .checked_add(part << shift)
            .ok_or(PackError::SizeOverflow)?;
        shift += 7;
    }
    usize::try_from(size)
        .map_err(|_| PackError::SizeOverflow)
        .map(|size| (pack_type, size))
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, PackError> {
    let mut byte = [0u8; 1];
    read_full(reader, &mut byte)?;
    Ok(byte[0])
}

fn read_full<R: Read>(reader: &mut R, out: &mut [u8]) -> Result<(), PackError> {
    match reader.read_exact(out) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Err(PackError::Truncated),
        Err(error) => Err(PackError::Io(error)),
    }
}

fn read_full_unhashed<R: Read>(
    reader: &mut HashingReader<R>,
    out: &mut [u8],
) -> Result<(), PackError> {
    let mut filled = 0usize;
    while filled < out.len() {
        match reader.read_unhashed(&mut out[filled..]) {
            Ok(0) => return Err(PackError::Truncated),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(PackError::Truncated);
            }
            Err(error) => return Err(PackError::Io(error)),
        }
    }
    Ok(())
}

#[derive(Default)]
struct SourceState {
    eof: bool,
    error: Option<io::ErrorKind>,
}

#[derive(Clone, Default)]
struct SourceWatch(Arc<Mutex<SourceState>>);

impl SourceWatch {
    fn record_eof(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.eof = true;
        }
    }

    fn record_error(&self, kind: io::ErrorKind) {
        if let Ok(mut state) = self.0.lock() {
            state.error = Some(kind);
        }
    }

    fn record_success(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.error = None;
        }
    }

    fn error(&self) -> Option<io::ErrorKind> {
        self.0.lock().ok().and_then(|state| state.error)
    }

    fn eof(&self) -> bool {
        self.0.lock().map(|state| state.eof).unwrap_or(false)
    }
}

/// A reader that hashes exactly the bytes consumed through [`Read`] or
/// [`BufRead::consume`]. `read_unhashed` is reserved for the trailer and the
/// end-of-stream check.
struct HashingReader<R> {
    inner: R,
    hasher: Option<HashAlgorithm>,
    buffer: Vec<u8>,
    position: usize,
    source: SourceWatch,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R, kind: HashKind) -> Self {
        Self {
            inner,
            hasher: Some(HashAlgorithm::new_for_kind(kind)),
            buffer: Vec::new(),
            position: 0,
            source: SourceWatch::default(),
        }
    }

    fn source(&self) -> SourceWatch {
        self.source.clone()
    }

    fn refill(&mut self) -> io::Result<()> {
        if self.position == self.buffer.len() {
            self.position = 0;
            self.buffer.resize(8192, 0);
            match self.inner.read(&mut self.buffer) {
                Ok(0) => {
                    self.source.record_eof();
                    self.buffer.clear();
                }
                Ok(read) => {
                    self.source.record_success();
                    self.buffer.truncate(read);
                }
                Err(error) => {
                    self.source.record_error(error.kind());
                    self.buffer.clear();
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn read_unhashed(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0usize;
        while filled < out.len() && self.position < self.buffer.len() {
            out[filled] = self.buffer[self.position];
            self.position += 1;
            filled += 1;
        }
        if filled < out.len() {
            match self.inner.read(&mut out[filled..]) {
                Ok(0) => self.source.record_eof(),
                Ok(read) => {
                    self.source.record_success();
                    filled += read;
                }
                Err(error) => {
                    self.source.record_error(error.kind());
                    return Err(error);
                }
            }
        }
        if self.position == self.buffer.len() {
            self.position = 0;
            self.buffer.clear();
        }
        Ok(filled)
    }

    fn finish(mut self) -> Vec<u8> {
        self.hasher
            .take()
            .map(HashAlgorithm::finalize)
            .unwrap_or_default()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let read = self.read_unhashed(out)?;
        if read > 0
            && let Some(hasher) = self.hasher.as_mut()
        {
            hasher.update(&out[..read]);
        }
        Ok(read)
    }
}

impl<R: Read> BufRead for HashingReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.refill()?;
        Ok(&self.buffer[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        let amount = amount.min(self.buffer.len().saturating_sub(self.position));
        if amount > 0 {
            if let Some(hasher) = self.hasher.as_mut() {
                hasher.update(&self.buffer[self.position..self.position + amount]);
            }
            self.position += amount;
        }
        if self.position == self.buffer.len() {
            self.position = 0;
            self.buffer.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        path::Path,
        process::{Command, Stdio},
    };

    use super::*;

    type ObjectBytes = Vec<(ObjectType, Vec<u8>)>;
    type Decoded = (PackSummary, ObjectBytes);

    #[derive(Default)]
    struct CollectSink {
        entries: Vec<(ObjectType, Vec<u8>)>,
        current: Vec<u8>,
        current_type: Option<ObjectType>,
    }

    impl PackSink for CollectSink {
        fn object_header(
            &mut self,
            object_type: ObjectType,
            _size: usize,
        ) -> Result<(), PackError> {
            self.current_type = Some(object_type);
            self.current.clear();
            Ok(())
        }

        fn object_chunk(&mut self, chunk: &[u8]) -> Result<(), PackError> {
            self.current.extend_from_slice(chunk);
            Ok(())
        }

        fn object_end(&mut self) -> Result<(), PackError> {
            self.entries
                .push((self.current_type.expect("header"), self.current.clone()));
            Ok(())
        }
    }

    fn git(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        child
            .stdin
            .take()
            .expect("git stdin")
            .write_all(input)
            .expect("write git stdin");
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn object_name(object_type: ObjectType) -> &'static str {
        match object_type {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Commit => "commit",
            ObjectType::Tag => "tag",
            _ => panic!("git pack object type {object_type:?} is unsupported"),
        }
    }

    /// Build a real PACK with `git pack-objects` and return the expected
    /// `(type, bytes)` objects in pack order plus the raw pack bytes.
    fn real_pack(kind: HashKind, deltas: bool) -> (ObjectBytes, Vec<u8>) {
        real_pack_with(kind, deltas, false)
    }

    fn real_pack_with(kind: HashKind, deltas: bool, ofs_delta: bool) -> (ObjectBytes, Vec<u8>) {
        let dir = tempfile::tempdir().expect("temp repo");
        let format = match kind {
            HashKind::Sha1 => "sha1",
            HashKind::Sha256 => "sha256",
            HashKind::Blake3 => panic!("git does not support blake3 packs"),
        };
        git(dir.path(), &["init", "-q", "--object-format", format], b"");
        let objects: Vec<(ObjectType, Vec<u8>)> = if deltas {
            let base: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
            let mut next = base.clone();
            for byte in next.iter_mut().take(1000) {
                *byte = byte.wrapping_add(7);
            }
            vec![
                (ObjectType::Blob, base),
                (ObjectType::Blob, next),
                (ObjectType::Blob, b"tail\n".to_vec()),
            ]
        } else {
            let tree_bytes: Vec<u8> = Vec::new();
            let tree = String::from_utf8(git(
                dir.path(),
                &["hash-object", "-w", "-t", "tree", "--stdin"],
                &tree_bytes,
            ))
            .expect("tree oid")
            .trim()
            .to_owned();
            let commit = format!(
                "tree {tree}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \nfake-signature\n -----END PGP SIGNATURE-----\n\ncommit with gpgsig\n"
            );
            vec![
                (ObjectType::Blob, b"plain\n".to_vec()),
                (ObjectType::Blob, vec![0xff, 0xfe, 0x00, b'\n']),
                (ObjectType::Tree, tree_bytes),
                (ObjectType::Commit, commit.into_bytes()),
            ]
        };
        let mut written = Vec::new();
        for (object_type, bytes) in &objects {
            let oid = git(
                dir.path(),
                &[
                    "hash-object",
                    "-w",
                    "-t",
                    object_name(*object_type),
                    "--stdin",
                ],
                bytes,
            );
            written.push(String::from_utf8(oid).expect("oid").trim().to_owned());
        }
        let input = format!("{}\n", written.join("\n"));
        let window = if deltas { "10" } else { "0" };
        let depth = if deltas { "10" } else { "0" };
        let mut args = vec![
            "-c".to_owned(),
            format!("pack.window={window}"),
            "-c".to_owned(),
            format!("pack.depth={depth}"),
            "pack-objects".to_owned(),
            "--stdout".to_owned(),
        ];
        if ofs_delta {
            args.push("--delta-base-offset".to_owned());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let pack = git(dir.path(), &args, input.as_bytes());
        (objects, pack)
    }

    fn decode(kind: HashKind, pack: &[u8]) -> Result<Decoded, PackError> {
        let mut sink = CollectSink::default();
        let summary = decode_pack(pack, kind, &mut sink)?;
        Ok((summary, sink.entries))
    }

    #[test]
    fn pack_structure_valid_decodes_real_git_packs() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let (expected, pack) = real_pack(kind, false);
            let (summary, decoded) = decode(kind, &pack).expect("valid pack");
            assert_eq!(summary.version, 2);
            assert_eq!(summary.object_count as usize, expected.len());
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn pack_structure_valid_header_is_rejected_for_bad_magic() {
        let mut sink = CollectSink::default();
        let pack = b"XXXX\x00\x00\x00\x02\x00\x00\x00\x00".to_vec();
        assert!(matches!(
            decode_pack(&pack[..], HashKind::Sha1, &mut sink),
            Err(PackError::Magic)
        ));
    }
    #[test]
    fn pack_structure_reject_mutations_and_deltas() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let (_, pack) = real_pack(kind, false);
            let header_len = 12;
            let trailer_len = kind.size();

            let mut bad_magic = pack.clone();
            bad_magic[0] = b'X';
            assert!(matches!(decode(kind, &bad_magic), Err(PackError::Magic)));

            let mut bad_version = pack.clone();
            bad_version[4..8].copy_from_slice(&9u32.to_be_bytes());
            assert!(matches!(
                decode(kind, &bad_version),
                Err(PackError::Version(9))
            ));

            let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
            let mut bad_count = pack.clone();
            bad_count[8..12].copy_from_slice(&(count + 1).to_be_bytes());
            assert!(decode(kind, &bad_count).is_err());
            let mut short_count = pack.clone();
            short_count[8..12].copy_from_slice(&count.saturating_sub(1).to_be_bytes());
            assert!(decode(kind, &short_count).is_err());

            let mut bad_trailer = pack.clone();
            let last = bad_trailer.len() - 1;
            bad_trailer[last] ^= 0xff;
            assert!(matches!(
                decode(kind, &bad_trailer),
                Err(PackError::Trailer)
            ));

            let truncated = &pack[..pack.len() - trailer_len];
            assert!(matches!(decode(kind, truncated), Err(PackError::Truncated)));

            let mut trailing = pack.clone();
            trailing.push(0);
            assert!(matches!(decode(kind, &trailing), Err(PackError::Trailing)));

            assert!(decode(kind, &pack[header_len..]).is_err());
        }
    }

    /// Walk one real pack and return the raw pack object types it contains.
    fn pack_object_types(pack: &[u8], kind: HashKind) -> Vec<u8> {
        let mut reader = std::io::Cursor::new(pack);
        let mut header = [0u8; 12];
        reader.read_exact(&mut header).expect("pack header");
        let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        let mut types = Vec::new();
        for _ in 0..count {
            let mut byte = [0u8; 1];
            reader.read_exact(&mut byte).expect("object header");
            let pack_type = (byte[0] >> 4) & 0x07;
            let mut size = usize::from(byte[0] & 0x0f);
            let mut shift = 4u32;
            while byte[0] & 0x80 != 0 {
                reader.read_exact(&mut byte).expect("object size");
                size |= usize::from(byte[0] & 0x7f) << shift;
                shift += 7;
            }
            types.push(pack_type);
            let object_type = match pack_type {
                1 => ObjectType::Commit,
                2 => ObjectType::Tree,
                3 => ObjectType::Blob,
                4 => ObjectType::Tag,
                _ => ObjectType::Blob,
            };
            if pack_type == 6 {
                // OFS_DELTA carries a little-endian offset with continuation bits.
                let mut offset = [0u8; 1];
                reader.read_exact(&mut offset).expect("ofs delta offset");
                while offset[0] & 0x80 != 0 {
                    reader.read_exact(&mut offset).expect("ofs delta offset");
                }
            } else if pack_type == 7 {
                let mut base = vec![0u8; kind.size()];
                reader.read_exact(&mut base).expect("ref delta base");
            }
            let mut inflater = if pack_type == 6 || pack_type == 7 {
                ReadBoxed::new_for_delta_with_kind(&mut reader, kind)
            } else {
                ReadBoxed::new_with_kind(&mut reader, object_type, size, kind).expect("inflater")
            };
            let mut discarded = Vec::new();
            inflater
                .read_to_end(&mut discarded)
                .expect("inflate pack object");
        }
        types
    }

    #[test]
    fn pack_structure_reject_both_real_git_delta_kinds() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let (_, ref_delta_pack) = real_pack(kind, true);
            let types = pack_object_types(&ref_delta_pack, kind);
            assert!(
                types.contains(&7),
                "git must produce a REF_DELTA (type 7) object: {types:?}"
            );
            assert!(matches!(
                decode(kind, &ref_delta_pack),
                Err(PackError::Delta)
            ));

            let (_, ofs_delta_pack) = real_pack_with(kind, true, true);
            let types = pack_object_types(&ofs_delta_pack, kind);
            assert!(
                types.contains(&6),
                "git must produce an OFS_DELTA (type 6) object: {types:?}"
            );
            assert!(matches!(
                decode(kind, &ofs_delta_pack),
                Err(PackError::Delta)
            ));
        }
    }

    #[test]
    fn pack_structure_reject_object_size_overflow() {
        let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x01".to_vec();
        pack.push(0xb0);
        pack.extend([0xffu8; 12]);
        assert!(matches!(
            decode(HashKind::Sha1, &pack),
            Err(PackError::SizeOverflow)
        ));
    }
    #[test]
    fn pack_structure_reject_payload_truncation_and_corrupt_zlib() {
        let (_, pack) = real_pack(HashKind::Sha1, false);
        // Drop the last payload byte together with the trailer: the stream ends
        // inside an object instead of exactly after it.
        let cut = pack.len() - HashKind::Sha1.size() - 1;
        assert!(matches!(
            decode(HashKind::Sha1, &pack[..cut]),
            Err(PackError::Truncated)
        ));

        // A synthetic pack whose only object carries an invalid deflate stream.
        let mut corrupt = b"PACK\x00\x00\x00\x02\x00\x00\x00\x01".to_vec();
        corrupt.push(0x31);
        corrupt.extend([0xff, 0xff, 0xff, 0xff]);
        corrupt.extend([0u8; HashKind::Sha1.size()]);
        assert!(matches!(
            decode(HashKind::Sha1, &corrupt),
            Err(PackError::Inflate(_))
        ));
    }
    #[test]
    fn pack_structure_reject_declared_size_larger_than_stream() {
        // A complete zlib stream for "a" while the object header declares 2
        // bytes: the stream is intact, so this is a size mismatch, not
        // truncation.
        let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x01".to_vec();
        pack.push(0x32);
        pack.extend([0x78, 0x9c, 0x4b, 0x04, 0x00, 0x00, 0x62, 0x00, 0x62]);
        pack.extend([0u8; HashKind::Sha1.size()]);
        let outcome = decode(HashKind::Sha1, &pack);
        assert!(
            matches!(
                outcome,
                Err(PackError::Size {
                    declared: 2,
                    actual: 1
                })
            ),
            "unexpected outcome: {outcome:?}"
        );
    }
    #[test]
    fn pack_structure_reject_short_stream_without_trailer() {
        // The header declares 100 bytes while the complete stream inflates one
        // byte and the input ends immediately: the inflater hits source EOF.
        let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x01".to_vec();
        pack.push(0xb4);
        pack.push(0x06);
        pack.extend([0x78, 0x9c, 0x4b, 0x04, 0x00, 0x00, 0x62, 0x00, 0x62]);
        assert!(matches!(
            decode(HashKind::Sha1, &pack),
            Err(PackError::Truncated)
        ));
    }
}
