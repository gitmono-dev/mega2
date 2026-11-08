//! Response budget for the traces read surfaces (MTR-43).
//!
//! One helper covers the metadata, search and transcript responses. It never
//! truncates JSON bytes: an over-budget response is rejected with the stable
//! `response_budget` error, so the client always reads valid JSON.

use serde::Serialize;

use crate::{common::errors::TracesError, config::TracesProtocolLimits};

/// The budget one response is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseBudget {
    /// Largest number of bytes one JSON envelope may use.
    pub max_bytes: u64,
}

impl ResponseBudget {
    /// The fixed protocol budget.
    pub fn from_protocol_limits() -> Self {
        Self {
            max_bytes: TracesProtocolLimits::RESPONSE_BYTES_MAX,
        }
    }
}

/// How a body fits the byte budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFit {
    /// The whole body fits.
    Whole,
    /// Only the prefix fits, and the rest is reachable with the cursor.
    Partial { kept_bytes: u64 },
}

/// The result of fitting one response into the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetedBody {
    /// The bytes that fit.
    pub bytes: Vec<u8>,
    /// How the body was cut.
    pub fit: BodyFit,
}

impl BudgetedBody {
    /// Whether the whole body fit.
    pub fn is_whole(&self) -> bool {
        matches!(self.fit, BodyFit::Whole)
    }

    /// The continuation cursor for a partial body, when there is one.
    pub fn next_cursor(&self) -> Option<u64> {
        match self.fit {
            BodyFit::Whole => None,
            BodyFit::Partial { kept_bytes } => Some(kept_bytes),
        }
    }
}

/// Fit one body into the byte budget.
///
/// A body that does not fit is cut on a UTF-8 character boundary, and the
/// cursor is the byte offset the next request continues from. A body that
/// cannot fit even one character is rejected instead of being cut into
/// invalid text.
pub fn fit_body(text: &str, budget: &ResponseBudget) -> Result<BudgetedBody, TracesError> {
    let whole = text.as_bytes();
    if whole.len() as u64 <= budget.max_bytes {
        return Ok(BudgetedBody {
            bytes: whole.to_vec(),
            fit: BodyFit::Whole,
        });
    }
    let limit = budget.max_bytes as usize;
    let mut end = limit.min(whole.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return Err(TracesError::ResponseBudget);
    }
    Ok(BudgetedBody {
        bytes: whole[..end].to_vec(),
        fit: BodyFit::Partial {
            kept_bytes: end as u64,
        },
    })
}

/// Check that one envelope fits the budget.
///
/// The envelope is measured as the JSON the client reads, so a response that
/// would not fit is rejected whole. The JSON bytes are never truncated.
pub fn fit_envelope<T: Serialize>(
    value: &T,
    budget: &ResponseBudget,
) -> Result<Vec<u8>, TracesError> {
    let bytes = serde_json::to_vec(value).map_err(|_| TracesError::ResponseBudget)?;
    if bytes.len() as u64 > budget.max_bytes {
        return Err(TracesError::ResponseBudget);
    }
    Ok(bytes)
}

/// The default number of search results one page returns.
pub fn search_limit_default() -> u64 {
    TracesProtocolLimits::SEARCH_LIMIT_DEFAULT
}

/// The largest number of search results one page may return.
pub fn search_limit_max() -> u64 {
    TracesProtocolLimits::SEARCH_LIMIT_MAX
}

/// The default body size one transcript request reads.
pub fn body_bytes_default() -> u64 {
    TracesProtocolLimits::TEXT_BYTES_DEFAULT
}

/// The largest body size one transcript request may read.
pub fn body_bytes_max() -> u64 {
    TracesProtocolLimits::TEXT_BYTES_MAX
}

/// Admit a search page size.
pub fn admit_search_limit(limit: Option<u64>) -> Result<u64, TracesError> {
    match limit {
        None => Ok(search_limit_default()),
        Some(limit) if (1..=search_limit_max()).contains(&limit) => Ok(limit),
        Some(_) => Err(TracesError::InvalidParameter),
    }
}

/// Admit a body size.
pub fn admit_body_bytes(max_bytes: Option<u64>) -> Result<u64, TracesError> {
    match max_bytes {
        None => Ok(body_bytes_default()),
        Some(max_bytes) if (1..=body_bytes_max()).contains(&max_bytes) => Ok(max_bytes),
        Some(_) => Err(TracesError::InvalidParameter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_budget_reads_the_protocol_limits() {
        let budget = ResponseBudget::from_protocol_limits();
        assert_eq!(budget.max_bytes, TracesProtocolLimits::RESPONSE_BYTES_MAX);
        assert_eq!(budget.max_bytes, 1_048_576);
        assert_eq!(search_limit_default(), 20);
        assert_eq!(search_limit_max(), 100);
        assert_eq!(body_bytes_default(), 16_384);
        assert_eq!(body_bytes_max(), 131_072);
    }

    #[test]
    fn response_budget_admits_the_fixed_page_sizes() {
        assert_eq!(admit_search_limit(None).expect("default"), 20);
        assert_eq!(admit_search_limit(Some(1)).expect("min"), 1);
        assert_eq!(admit_search_limit(Some(100)).expect("max"), 100);
        assert!(admit_search_limit(Some(0)).is_err());
        assert!(admit_search_limit(Some(101)).is_err());

        assert_eq!(admit_body_bytes(None).expect("default"), 16_384);
        assert_eq!(admit_body_bytes(Some(131_072)).expect("max"), 131_072);
        assert!(admit_body_bytes(Some(0)).is_err());
        assert!(admit_body_bytes(Some(131_073)).is_err());
    }

    #[test]
    fn response_budget_cuts_a_body_on_a_character_boundary() {
        let budget = ResponseBudget { max_bytes: 5 };
        // The three-byte character does not fit in four bytes, so the cut
        // backs up to the boundary before it.
        let body = fit_body("ab雪cd", &ResponseBudget { max_bytes: 4 }).expect("body");
        assert_eq!(body.bytes, b"ab".to_vec());
        assert_eq!(body.fit, BodyFit::Partial { kept_bytes: 2 });
        assert_eq!(body.next_cursor(), Some(2));
        assert!(!body.is_whole());

        // The character fits exactly, so the whole body is kept.
        let exact = fit_body("ab雪cd", &budget).expect("body");
        assert_eq!(exact.bytes, "ab雪".as_bytes().to_vec());
        assert_eq!(exact.fit, BodyFit::Partial { kept_bytes: 5 });

        let whole = fit_body("abc", &budget).expect("body");
        assert!(whole.is_whole());
        assert_eq!(whole.next_cursor(), None);
    }

    #[test]
    fn response_budget_rejects_an_envelope_that_does_not_fit() {
        let budget = ResponseBudget { max_bytes: 8 };
        let error = fit_envelope(&"a".repeat(64), &budget).expect_err("budget");
        assert_eq!(error.code(), "response_budget");
        assert_eq!(error.http_status().as_u16(), 413);

        // The rejected envelope is never returned as cut JSON.
        let ok = fit_envelope(&"ab", &budget).expect("envelope");
        assert_eq!(ok, b"\"ab\"".to_vec());
    }
}
