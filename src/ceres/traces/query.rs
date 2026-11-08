//! Literal keyword query definition for the traces search surface (MTR-41).
//!
//! A query is matched literally: there is no Unicode normalization, no regular
//! expression syntax, and no vector syntax. ASCII letters compare
//! case-insensitively through the same fold the fragment index stores; every
//! other character, including non-ASCII, compares byte for byte.

use crate::ceres::traces::fragments::ascii_fold;

/// Longest accepted query in UTF-8 bytes.
pub const QUERY_BYTES_MAX: u64 = 256;

/// One literal keyword query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralQuery {
    /// The query as the caller wrote it.
    pub raw: String,
    /// The ASCII-folded form the fragment index stores.
    pub folded: String,
}

impl LiteralQuery {
    /// Validate and fold one query.
    ///
    /// An empty query and a query longer than [`QUERY_BYTES_MAX`] are rejected.
    pub fn new(raw: &str) -> Result<Self, LiteralQueryError> {
        if raw.is_empty() {
            return Err(LiteralQueryError::Empty);
        }
        if raw.len() as u64 > QUERY_BYTES_MAX {
            return Err(LiteralQueryError::TooLong);
        }
        Ok(Self {
            raw: raw.to_owned(),
            folded: ascii_fold(raw),
        })
    }
}

/// Why a literal query was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralQueryError {
    /// The query was empty.
    Empty,
    /// The query exceeded the fixed byte budget.
    TooLong,
}

impl std::fmt::Display for LiteralQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Empty => "query is empty",
            Self::TooLong => "query exceeds the fixed byte budget",
        };
        f.write_str(text)
    }
}

/// Escape the LIKE metacharacters so the query matches literally.
///
/// `%`, `_` and `\` are the only characters a `LIKE` pattern treats specially.
/// The escape character itself is doubled.
pub fn escape_like_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Build the `LIKE` pattern for a folded literal query.
///
/// The pattern matches the folded column and anchors at both ends, so the
/// query only matches whole literal substrings.
pub fn like_pattern(query: &LiteralQuery) -> String {
    format!("%{}%", escape_like_literal(&query.folded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_query_folds_ascii_and_keeps_other_characters() {
        let query = LiteralQuery::new("MiXeD 雪").expect("query");
        assert_eq!(query.raw, "MiXeD 雪");
        assert_eq!(query.folded, "mixed 雪");
        assert!(LiteralQuery::new("").is_err());
        assert!(LiteralQuery::new(&"a".repeat(257)).is_err());
        assert!(LiteralQuery::new(&"a".repeat(256)).is_ok());
    }

    #[test]
    fn literal_query_escapes_like_metacharacters() {
        assert_eq!(escape_like_literal("100%"), "100\\%");
        assert_eq!(escape_like_literal("a_b"), "a\\_b");
        assert_eq!(escape_like_literal("a\\b"), "a\\\\b");
        assert_eq!(escape_like_literal("雪"), "雪");
        let query = LiteralQuery::new("A%_\\").expect("query");
        assert_eq!(like_pattern(&query), "%a\\%\\_\\\\%");
    }
}
