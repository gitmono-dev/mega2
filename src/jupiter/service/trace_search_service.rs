//! Bounded keyword search over one readable traces stream (MTR-19).
//!
//! The service owns the search budget and the readiness gate. It never
//! reports "no match" for a stream it could not search: an unavailable index
//! and a timed-out query both surface as stable errors instead.

use crate::{
    api_model::traces::QueryCoverage,
    ceres::traces::query::LiteralQuery,
    common::errors::{MegaError, TracesError},
    jupiter::{
        service::trace_search_index_service::IndexState,
        storage::trace_search_storage::{
            LiteralSearchResult, SearchBudget, SearchPlan, bounded_literal_search, is_timeout_error,
        },
    },
};

/// The stable reason the search surface is not available.
pub const SEARCH_UNAVAILABLE: &str = "search_unavailable";

/// Whether this deployment's locale can extract trigrams from CJK text.
///
/// The answer is read from the live database because it depends on the
/// deployed locale, not on the index alone.
pub async fn cjk_index_supported(db: &sea_orm::DatabaseConnection) -> Result<bool, TracesError> {
    crate::jupiter::storage::trace_search_storage::locale_extracts_cjk_trigrams(db)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "traces CJK locale probe failed");
            TracesError::search_unavailable(SEARCH_UNAVAILABLE)
        })
}

/// One keyword search request.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// The stream the search is scoped to.
    pub stream_id: String,
    /// The literal query.
    pub query: LiteralQuery,
}

/// One keyword search answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    /// The matched fragments.
    pub result: LiteralSearchResult,
}

/// Search the one stream the caller may read.
///
/// The index state is read from the process catalog probe, never inferred
/// from accepted or indexed metadata watermarks.
pub async fn search_stream(
    db: &sea_orm::DatabaseConnection,
    index: &IndexState,
    request: &SearchRequest,
    budget: SearchBudget,
) -> Result<SearchOutcome, TracesError> {
    ensure_searchable(index)?;
    let result = bounded_literal_search(db, &request.query, &request.stream_id, budget)
        .await
        .map_err(map_search_error)?;
    Ok(SearchOutcome { result })
}

/// The coverage verdict of one search, taken at its pinned snapshot.
///
/// The scan state comes from the bounded search that actually ran, and the
/// index state comes from the snapshot the query pinned, so a later push or
/// index catch-up does not change the verdict.
pub fn coverage_of(
    snapshot: crate::ceres::traces::cursor::QuerySnapshot,
    result: &LiteralSearchResult,
) -> QueryCoverage {
    // A candidate cap that stopped the search means the scan did not read the
    // whole scope. A truncated bounded scan means the byte budget stopped it.
    let scan_complete = !result.truncated;
    QueryCoverage::take(snapshot, scan_complete)
}

/// The plan one query used, for the coverage matrix.
pub fn plan_name(plan: SearchPlan) -> &'static str {
    match plan {
        SearchPlan::IndexedTrigram => "indexed_trigram",
        SearchPlan::BoundedScan => "bounded_scan",
    }
}

fn map_search_error(error: MegaError) -> TracesError {
    if is_timeout_error(&error) {
        return TracesError::QueryTimeout;
    }
    match error {
        MegaError::Traces(traces) => traces,
        _ => TracesError::search_unavailable(SEARCH_UNAVAILABLE),
    }
}

/// The readiness gate. A ready index passes; anything else is unavailable.
fn ensure_searchable(index: &IndexState) -> Result<(), TracesError> {
    if matches!(index, IndexState::Ready) {
        return Ok(());
    }
    Err(TracesError::search_unavailable(SEARCH_UNAVAILABLE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_search_reports_the_unavailable_reason_before_touching_the_database() {
        let unavailable = IndexState::Unavailable(
            crate::jupiter::storage::trace_search_index_storage::CatalogFailure::Missing,
        );
        let error = ensure_searchable(&unavailable).expect_err("unavailable index");
        assert_eq!(error.code(), "search_unavailable");
        assert_eq!(error.http_status().as_u16(), 503);
        assert_eq!(
            error.public_message(),
            "search_unavailable:search_unavailable"
        );

        let ready = IndexState::Ready;
        assert!(ensure_searchable(&ready).is_ok());
    }

    #[test]
    fn literal_search_budget_reads_the_protocol_limits() {
        let budget = SearchBudget::from_protocol_limits();
        assert_eq!(
            budget.candidate_limit,
            crate::config::TracesProtocolLimits::CANDIDATE_FRAGMENTS_MAX
        );
        assert_eq!(
            budget.scan_bytes_max,
            crate::config::TracesProtocolLimits::SCAN_BYTES_MAX
        );
        assert_eq!(
            budget.timeout_ms,
            crate::config::TracesProtocolLimits::QUERY_TIMEOUT_MS
        );
        assert_eq!(budget.candidate_limit, 2_000);
        assert_eq!(budget.scan_bytes_max, 8_388_608);
        assert_eq!(budget.timeout_ms, 2_000);
    }

    #[test]
    fn literal_search_names_both_plans() {
        assert_eq!(plan_name(SearchPlan::IndexedTrigram), "indexed_trigram");
        assert_eq!(plan_name(SearchPlan::BoundedScan), "bounded_scan");
    }

    #[test]
    fn scan_coverage_reports_a_truncated_scan_as_incomplete() {
        use crate::ceres::traces::cursor::QuerySnapshot;

        let snapshot = QuerySnapshot::new(4, 4, 1);

        // A scan that ran to the end of the scope is complete.
        let complete = LiteralSearchResult {
            matches: Vec::new(),
            plan: SearchPlan::IndexedTrigram,
            scanned_bytes: 0,
            truncated: false,
        };
        let coverage = super::coverage_of(snapshot, &complete);
        assert!(coverage.scan_complete);
        assert!(coverage.coverage_complete);

        // A scan that stopped at its budget is not complete.
        let truncated = LiteralSearchResult {
            matches: Vec::new(),
            plan: SearchPlan::BoundedScan,
            scanned_bytes: 4096,
            truncated: true,
        };
        let coverage = super::coverage_of(snapshot, &truncated);
        assert!(!coverage.scan_complete);
        assert!(!coverage.coverage_complete);
        assert_eq!(
            coverage.incomplete_reason.as_deref(),
            Some(crate::api_model::traces::QueryCoverage::SCAN_INCOMPLETE)
        );
    }
}
