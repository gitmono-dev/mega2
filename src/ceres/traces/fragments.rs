//! Logical transcript fragments for keyword projection (MTR-16).
//!
//! Fragments are copies. The archived Git bytes are not rewritten. Each piece
//! is at most 16 KiB, ends on a UTF-8 boundary, and overlaps the next piece by
//! at least 256 bytes when the transcript is longer than one piece.

use sha2::{Digest, Sha256};

/// Largest UTF-8 byte length of one fragment body.
pub const MAX_FRAGMENT_BYTES: usize = 16 * 1024;
/// Minimum byte overlap between adjacent fragments.
pub const MIN_OVERLAP_BYTES: usize = 256;
/// First index version written by this projector. Later cards may rebuild it.
pub const FRAGMENT_INDEX_VERSION: i64 = 1;

/// One searchable slice of a logical transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFragment {
    pub body: String,
    pub ascii_fold: String,
    pub content_digest: String,
}

/// Lowercase ASCII letters. Every other character, including non-ASCII, stays.
pub fn ascii_fold(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii_uppercase() {
                ch.to_ascii_lowercase()
            } else {
                ch
            }
        })
        .collect()
}

/// Join manifest parts in declared order. The result is the logical byte stream.
pub fn reassemble_parts(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut logical = Vec::new();
    for part in parts {
        logical.extend_from_slice(part);
    }
    logical
}

/// Split a logical transcript into overlapping UTF-8 fragments.
///
/// Returns `None` when the bytes are not UTF-8. An empty transcript yields no
/// fragments. The body of each fragment is an exact slice of `bytes`.
pub fn split_utf8(bytes: &[u8]) -> Option<Vec<TextFragment>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() {
        return Some(Vec::new());
    }
    if text.len() <= MAX_FRAGMENT_BYTES {
        return Some(vec![fragment(text)]);
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + MAX_FRAGMENT_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            return None;
        }
        pieces.push(fragment(&text[start..end]));
        if end == text.len() {
            break;
        }
        let mut next = end - MIN_OVERLAP_BYTES;
        while next < end && !text.is_char_boundary(next) {
            next += 1;
        }
        if end - next < MIN_OVERLAP_BYTES {
            let mut back = next;
            while back > start && end - back < MIN_OVERLAP_BYTES {
                back -= 1;
                while back > start && !text.is_char_boundary(back) {
                    back -= 1;
                }
            }
            if back > start && end - back >= MIN_OVERLAP_BYTES {
                next = back;
            }
        }
        if next <= start {
            return None;
        }
        start = next;
    }
    Some(pieces)
}

fn fragment(text: &str) -> TextFragment {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    TextFragment {
        ascii_fold: ascii_fold(text),
        content_digest: hex::encode(hasher.finalize()),
        body: text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap_len(left: &str, right: &str) -> usize {
        let max = left.len().min(right.len());
        (MIN_OVERLAP_BYTES..=max)
            .rev()
            .find(|size| {
                left.is_char_boundary(left.len() - size)
                    && right.is_char_boundary(*size)
                    && left.ends_with(&right[..*size])
            })
            .unwrap_or(0)
    }

    #[test]
    fn fragment_projection_bounds_utf8_overlap_and_fold() {
        let mut text = String::new();
        while text.len() < 20_000 {
            text.push_str(&format!("{:08x}-中-", text.len()));
        }
        let pieces = split_utf8(text.as_bytes()).expect("utf-8 transcript");
        assert!(pieces.len() >= 2);
        let mut rebuilt = pieces[0].body.clone();
        for (index, piece) in pieces.iter().enumerate() {
            assert!(piece.body.len() <= MAX_FRAGMENT_BYTES);
            assert!(piece.body.contains('中') || piece.body.is_ascii());
            assert_eq!(piece.ascii_fold, ascii_fold(&piece.body));
            assert!(!piece.ascii_fold.chars().any(|ch| ch.is_ascii_uppercase()));
            if index > 0 {
                let overlap = overlap_len(&pieces[index - 1].body, &piece.body);
                assert!(overlap >= MIN_OVERLAP_BYTES, "overlap {overlap}");
                rebuilt.push_str(&piece.body[overlap..]);
            }
        }
        assert_eq!(rebuilt, text);
        let mut joined = b"alpha".to_vec();
        joined.extend_from_slice("中".as_bytes());
        assert_eq!(
            reassemble_parts(&[b"alpha".to_vec(), "中".as_bytes().to_vec()]),
            joined
        );
        assert!(split_utf8(&[0xff, 0xfe]).is_none());
    }
}
