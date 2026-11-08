//! Literal keyword search over the stored fragment index (MTR-41, MTR-19).
//!
//! The query is always bound as a parameter and its `LIKE` pattern is
//! escaped, so a caller can never inject SQL or change the match semantics.

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, QueryResult, Statement, TransactionTrait,
};

use crate::{
    ceres::traces::query::{LiteralQuery, like_pattern},
    common::errors::MegaError,
    config::TracesProtocolLimits,
};

/// One matched fragment row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralMatch {
    pub fragment_id: String,
    pub stream_id: String,
    pub checkpoint_id: String,
    pub resource_id: String,
    pub content_digest: String,
    pub body: String,
    pub byte_start: i64,
    pub byte_end: i64,
}

/// How one candidate set was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPlan {
    /// The pattern has an extractable trigram, so the GIN index answers it.
    IndexedTrigram,
    /// The pattern has no extractable trigram, so a keyset scan was used.
    BoundedScan,
}

/// One bounded keyword search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralSearchResult {
    /// The matched fragments.
    pub matches: Vec<LiteralMatch>,
    /// Which plan produced the candidates.
    pub plan: SearchPlan,
    /// Bytes of fragment body the bounded scan read.
    pub scanned_bytes: u64,
    /// Whether the candidate cap stopped the search.
    pub truncated: bool,
}

/// The search budget for one query, taken from the protocol limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBudget {
    /// Largest number of candidate fragments one query may return.
    pub candidate_limit: u64,
    /// Largest number of fragment body bytes one query may read.
    pub scan_bytes_max: u64,
    /// Statement timeout in milliseconds.
    pub timeout_ms: u64,
}

impl SearchBudget {
    /// The fixed protocol budget.
    pub fn from_protocol_limits() -> Self {
        Self {
            candidate_limit: TracesProtocolLimits::CANDIDATE_FRAGMENTS_MAX,
            scan_bytes_max: TracesProtocolLimits::SCAN_BYTES_MAX,
            timeout_ms: TracesProtocolLimits::QUERY_TIMEOUT_MS,
        }
    }
}

/// The bound statement for a literal keyword query.
///
/// The pattern is escaped and the query itself is passed as a bound value, so
/// the statement text never carries caller input.
pub fn literal_search_statement(
    query: &LiteralQuery,
    stream_id: &str,
    candidate_limit: u64,
) -> Statement {
    let pattern = like_pattern(query);
    let sql = "SELECT f.id AS fragment_id, f.stream_id AS stream_id, \
               m.checkpoint_id AS checkpoint_id, m.resource_id AS resource_id, \
               f.content_digest AS content_digest, f.body AS body, \
               f.ascii_fold AS fold_body, \
               r.byte_start AS byte_start, r.byte_end AS byte_end \
               FROM trace_fragment f \
               JOIN trace_fragment_membership m ON m.fragment_id = f.id \
               JOIN trace_resource r ON r.id = m.resource_id \
               WHERE f.stream_id = $1 AND f.ascii_fold LIKE $2 ESCAPE E'\\\\' \
               ORDER BY f.content_digest, m.resource_id \
               LIMIT $3";
    Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        [stream_id.into(), pattern.into(), candidate_limit.into()],
    )
}

/// The bound statement for the keyset bounded scan.
///
/// The scan reads fragments in digest order and stops at the byte budget, so
/// it never walks the whole index for a pattern the trigram index cannot help
/// with.
pub fn bounded_scan_statement(
    stream_id: &str,
    pattern: &str,
    after_digest: &str,
    candidate_limit: u64,
) -> Statement {
    let sql = "SELECT f.id AS fragment_id, f.stream_id AS stream_id, \
               m.checkpoint_id AS checkpoint_id, m.resource_id AS resource_id, \
               f.content_digest AS content_digest, f.body AS body, \
               f.ascii_fold AS fold_body, \
               r.byte_start AS byte_start, r.byte_end AS byte_end, \
               octet_length(f.body) AS body_bytes \
               FROM trace_fragment f \
               JOIN trace_fragment_membership m ON m.fragment_id = f.id \
               JOIN trace_resource r ON r.id = m.resource_id \
               WHERE f.stream_id = $1 AND f.ascii_fold LIKE $2 ESCAPE E'\\\\' \
                 AND f.content_digest > $3 \
               ORDER BY f.content_digest, m.resource_id \
               LIMIT $4";
    Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        [
            stream_id.into(),
            pattern.into(),
            after_digest.into(),
            candidate_limit.into(),
        ],
    )
}

/// The shortest literal run the trigram index can use.
///
/// `pg_trgm` builds trigrams from three consecutive characters. A `LIKE`
/// pattern whose literal run is shorter than that cannot narrow the index:
/// the scan returns every row and drops them again in the recheck.
pub const TRIGRAM_MIN_RUN_CHARS: usize = 3;

/// Whether the pattern can use the trigram index.
///
/// The query is literal, so the whole folded query is one literal run. A run
/// of at least three characters yields trigrams; a shorter pattern degenerates
/// into a full index scan.
pub fn pattern_is_index_selective(query: &LiteralQuery) -> bool {
    query.folded.chars().count() >= TRIGRAM_MIN_RUN_CHARS
}

/// Whether the deployed locale can extract trigrams from CJK text.
///
/// The answer depends on the deployment locale, so it is read from the live
/// database instead of being assumed.
pub async fn locale_extracts_cjk_trigrams(db: &impl ConnectionTrait) -> Result<bool, MegaError> {
    let statement = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "SELECT cardinality(mega2_extensions.show_trgm($1)) > 0 AS has_cjk_trigram",
        ["中文測".into()],
    );
    let rows = db.query_all_raw(statement).await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    Ok(row.try_get::<bool>("", "has_cjk_trigram").unwrap_or(false))
}

/// Run one literal keyword search and return the matched fragments.
pub async fn literal_search(
    db: &DatabaseConnection,
    query: &LiteralQuery,
    stream_id: &str,
    candidate_limit: u64,
) -> Result<Vec<LiteralMatch>, MegaError> {
    let statement = literal_search_statement(query, stream_id, candidate_limit);
    let rows = db.query_all_raw(statement).await?;
    let mut matches = Vec::with_capacity(rows.len());
    for row in rows {
        matches.push(LiteralMatch {
            fragment_id: read_text(&row, "fragment_id")?,
            stream_id: read_text(&row, "stream_id")?,
            checkpoint_id: read_text(&row, "checkpoint_id")?,
            resource_id: read_text(&row, "resource_id")?,
            content_digest: read_text(&row, "content_digest")?,
            body: read_text(&row, "body")?,
            byte_start: read_i64(&row, "byte_start")?,
            byte_end: read_i64(&row, "byte_end")?,
        });
    }
    Ok(matches)
}

/// Whether the error is a PostgreSQL statement timeout or lock timeout.
pub fn is_timeout_error(error: &MegaError) -> bool {
    let sqlx_error = match error {
        MegaError::Db(sea_orm::DbErr::Query(sea_orm::RuntimeErr::SqlxError(sqlx_error)))
        | MegaError::Db(sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sqlx_error))) => {
            sqlx_error
        }
        _ => return false,
    };
    sqlx_error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| matches!(code.as_ref(), "57014" | "55P03"))
}

/// Run one bounded literal keyword search inside a statement timeout.
///
/// The statement timeout is applied on the same transaction that runs the
/// query, so a slow plan is cancelled by the server instead of by the
/// caller waiting forever.
pub async fn bounded_literal_search(
    db: &DatabaseConnection,
    query: &LiteralQuery,
    stream_id: &str,
    budget: SearchBudget,
) -> Result<LiteralSearchResult, MegaError> {
    let txn = db.begin().await?;
    let timeout = format!("{}ms", budget.timeout_ms);
    txn.execute_unprepared(&format!("SET LOCAL statement_timeout = '{timeout}'"))
        .await
        .map_err(MegaError::Db)?;

    let pattern = like_pattern(query);
    let result = if pattern_is_index_selective(query) {
        let statement = literal_search_statement(query, stream_id, budget.candidate_limit);
        let rows = txn.query_all_raw(statement).await?;
        // The index answers with candidates; the literal text still decides.
        let matches = read_matches(&rows, query)?;
        LiteralSearchResult {
            matches,
            plan: SearchPlan::IndexedTrigram,
            scanned_bytes: 0,
            truncated: rows.len() as u64 >= budget.candidate_limit,
        }
    } else {
        let (matches, scanned_bytes, truncated) =
            bounded_scan(&txn, query, stream_id, &pattern, budget).await?;
        LiteralSearchResult {
            matches,
            plan: SearchPlan::BoundedScan,
            scanned_bytes,
            truncated,
        }
    };
    txn.commit().await?;
    Ok(result)
}

/// The keyset bounded scan.
///
/// The scan walks fragments in digest order, reads one page at a time, and
/// stops at the byte budget or the candidate cap. A pattern the trigram index
/// cannot narrow never turns into an unbounded walk over the whole index.
async fn bounded_scan(
    txn: &sea_orm::DatabaseTransaction,
    query: &LiteralQuery,
    stream_id: &str,
    pattern: &str,
    budget: SearchBudget,
) -> Result<(Vec<LiteralMatch>, u64, bool), MegaError> {
    let page_limit = budget.candidate_limit.min(256);
    let mut after_digest = String::new();
    let mut matches: Vec<LiteralMatch> = Vec::new();
    let mut scanned_bytes: u64 = 0;
    let mut truncated = false;

    loop {
        let statement = bounded_scan_statement(stream_id, pattern, &after_digest, page_limit);
        let rows = txn.query_all_raw(statement).await?;
        if rows.is_empty() {
            break;
        }
        let page = read_matches(&rows, query)?;
        let mut last_digest = after_digest.clone();
        for candidate in &page {
            last_digest = candidate.content_digest.clone();
            let body_bytes = candidate.body.len() as u64;
            if scanned_bytes.saturating_add(body_bytes) > budget.scan_bytes_max {
                truncated = true;
                break;
            }
            scanned_bytes = scanned_bytes.saturating_add(body_bytes);
            matches.push(candidate.clone());
            if matches.len() as u64 >= budget.candidate_limit {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
        if (rows.len() as u64) < page_limit {
            break;
        }
        after_digest = last_digest;
    }
    Ok((matches, scanned_bytes, truncated))
}

fn read_matches(
    rows: &[QueryResult],
    query: &LiteralQuery,
) -> Result<Vec<LiteralMatch>, MegaError> {
    let mut matches = Vec::with_capacity(rows.len());
    for row in rows {
        let fold_body = read_text(row, "fold_body")?;
        // The candidate must contain the literal query text. The index only
        // narrows the search; this check decides the match.
        if !fold_body.contains(&query.folded) {
            continue;
        }
        matches.push(LiteralMatch {
            fragment_id: read_text(row, "fragment_id")?,
            stream_id: read_text(row, "stream_id")?,
            checkpoint_id: read_text(row, "checkpoint_id")?,
            resource_id: read_text(row, "resource_id")?,
            content_digest: read_text(row, "content_digest")?,
            body: read_text(row, "body")?,
            byte_start: read_i64(row, "byte_start")?,
            byte_end: read_i64(row, "byte_end")?,
        });
    }
    Ok(matches)
}

fn read_text(row: &QueryResult, column: &str) -> Result<String, MegaError> {
    row.try_get::<String>("", column)
        .map_err(|err| MegaError::Other(format!("literal search column {column}: {err}")))
}

fn read_i64(row: &QueryResult, column: &str) -> Result<i64, MegaError> {
    row.try_get::<i64>("", column)
        .map_err(|err| MegaError::Other(format!("literal search column {column}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrated test database with the reviewed `pg_trgm` extension.
    ///
    /// The cloned test database does not copy the server's shared extension,
    /// so it is installed here. The shared `pg_trgm` is never dropped.
    async fn search_test_db(temp_dir: &std::path::Path) -> DatabaseConnection {
        let db = crate::jupiter::tests::test_db_connection(temp_dir).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .expect("migrations");
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner",
            "CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6'",
        ] {
            db.execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                sql.to_owned(),
            ))
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        db
    }

    #[tokio::test]
    async fn literal_query_binding_matches_literal_text_in_a_real_database() {
        use sea_orm::{ActiveModelTrait, Set};
        use uuid::Uuid;

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = crate::jupiter::tests::test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .expect("migrations");
        let owner = Uuid::new_v4().to_string();
        let target = Uuid::new_v4().to_string();
        let stream = Uuid::new_v4().to_string();
        let checkpoint = Uuid::new_v4().to_string();
        let resource = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        crate::callisto::trace_principal::ActiveModel {
            id: Set(owner.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        crate::callisto::trace_target::ActiveModel {
            id: Set(target.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/project/{target}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        crate::callisto::trace_stream::ActiveModel {
            id: Set(stream.clone()),
            principal_id: Set(owner),
            target_id: Set(target),
            source_repo_id: Set("search-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream");
        crate::callisto::trace_checkpoint::ActiveModel {
            id: Set(checkpoint.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_key: Set("search-checkpoint".to_owned()),
            original_tree: Set("a".repeat(40)),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("checkpoint");
        crate::callisto::trace_resource::ActiveModel {
            id: Set(resource.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_id: Set(checkpoint.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(11),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("resource");
        for (body, folded) in [
            ("100% pure", "100% pure"),
            ("100X pure", "100x pure"),
            ("a_b pure", "a_b pure"),
        ] {
            let fragment = Uuid::new_v4().to_string();
            crate::callisto::trace_fragment::ActiveModel {
                id: Set(fragment.clone()),
                stream_id: Set(stream.clone()),
                content_digest: Set(Uuid::new_v4().to_string()),
                index_version: Set(1),
                body: Set(body.to_owned()),
                ascii_fold: Set(folded.to_owned()),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("fragment");
            crate::callisto::trace_fragment::membership::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                fragment_id: Set(fragment),
                stream_id: Set(stream.clone()),
                checkpoint_id: Set(checkpoint.clone()),
                resource_id: Set(resource.clone()),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("membership");
        }

        // ASCII letters compare case-insensitively, so `100X` matches `100x pure`.
        let folded_case = LiteralQuery::new("100X").expect("query");
        let matches = literal_search(&db, &folded_case, &stream, 10)
            .await
            .expect("literal search");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].body, "100X pure");

        // A percent sign in the query is literal, never a wildcard.
        let percent = LiteralQuery::new("100%").expect("query");
        let matches = literal_search(&db, &percent, &stream, 10)
            .await
            .expect("literal search");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].body, "100% pure");
        assert!(
            !matches.iter().any(|row| row.body == "100X pure"),
            "the percent sign must not act as a wildcard: {matches:?}"
        );

        // An underscore in the query is literal, never a single-character wildcard.
        let underscore = LiteralQuery::new("a_b").expect("query");
        let matches = literal_search(&db, &underscore, &stream, 10)
            .await
            .expect("literal search");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].body, "a_b pure");

        let other = LiteralQuery::new("100%").expect("query");
        let matches = literal_search(&db, &other, &Uuid::new_v4().to_string(), 10)
            .await
            .expect("literal search");
        assert!(matches.is_empty(), "{matches:?}");
    }

    #[test]
    fn literal_query_binding_uses_parameters_only() {
        let query = LiteralQuery::new("a%b_c").expect("query");
        let statement = literal_search_statement(&query, "stream", 20);
        let sql = statement.sql.to_string();
        assert!(!sql.contains("a%b_c"), "{sql}");
        assert!(
            sql.contains("$1") && sql.contains("$2") && sql.contains("$3"),
            "{sql}"
        );
        assert!(sql.contains("ESCAPE"), "{sql}");
    }

    #[tokio::test]
    async fn literal_search_sql_uses_the_gin_index_and_falls_back_to_a_bounded_scan() {
        use sea_orm::{ActiveModelTrait, Set};
        use uuid::Uuid;

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = search_test_db(temp_dir.path()).await;
        let owner = Uuid::new_v4().to_string();
        let target = Uuid::new_v4().to_string();
        let stream = Uuid::new_v4().to_string();
        let checkpoint = Uuid::new_v4().to_string();
        let resource = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        crate::callisto::trace_principal::ActiveModel {
            id: Set(owner.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        crate::callisto::trace_target::ActiveModel {
            id: Set(target.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/project/{target}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        crate::callisto::trace_stream::ActiveModel {
            id: Set(stream.clone()),
            principal_id: Set(owner),
            target_id: Set(target),
            source_repo_id: Set("search-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream");
        crate::callisto::trace_checkpoint::ActiveModel {
            id: Set(checkpoint.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_key: Set("search-checkpoint".to_owned()),
            original_tree: Set("a".repeat(40)),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("checkpoint");
        crate::callisto::trace_resource::ActiveModel {
            id: Set(resource.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_id: Set(checkpoint.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(11),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("resource");
        for (body, folded) in [
            ("100% pure", "100% pure"),
            ("100X pure", "100x pure"),
            ("a_b pure", "a_b pure"),
            ("hello world", "hello world"),
        ] {
            let fragment = Uuid::new_v4().to_string();
            crate::callisto::trace_fragment::ActiveModel {
                id: Set(fragment.clone()),
                stream_id: Set(stream.clone()),
                content_digest: Set(Uuid::new_v4().to_string()),
                index_version: Set(1),
                body: Set(body.to_owned()),
                ascii_fold: Set(folded.to_owned()),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("fragment");
            crate::callisto::trace_fragment::membership::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                fragment_id: Set(fragment),
                stream_id: Set(stream.clone()),
                checkpoint_id: Set(checkpoint.clone()),
                resource_id: Set(resource.clone()),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("membership");
        }

        let budget = SearchBudget::from_protocol_limits();

        // A pattern with a three-character literal run uses the GIN candidate
        // plan.
        let query = LiteralQuery::new("hello").expect("query");
        assert!(pattern_is_index_selective(&query));
        let result = bounded_literal_search(&db, &query, &stream, budget)
            .await
            .expect("indexed search");
        assert_eq!(result.plan, SearchPlan::IndexedTrigram);
        assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
        assert_eq!(result.matches[0].body, "hello world");

        // A three-character literal run is selective, even with an escaped
        // metacharacter, because the escape keeps the run literal.
        let query = LiteralQuery::new("a_b").expect("query");
        assert!(pattern_is_index_selective(&query));
        let result = bounded_literal_search(&db, &query, &stream, budget)
            .await
            .expect("bounded search");
        assert_eq!(result.plan, SearchPlan::IndexedTrigram);
        assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
        assert_eq!(result.matches[0].body, "a_b pure");

        // A literal run shorter than three characters cannot narrow the
        // index, so the search falls back to the bounded scan.
        let query = LiteralQuery::new("a_").expect("query");
        assert!(!pattern_is_index_selective(&query));
        let result = bounded_literal_search(&db, &query, &stream, budget)
            .await
            .expect("bounded search");
        assert_eq!(result.plan, SearchPlan::BoundedScan);
        assert_eq!(result.matches.len(), 1, "{:?}", result.matches);
        assert_eq!(result.matches[0].body, "a_b pure");
        assert!(result.scanned_bytes <= budget.scan_bytes_max);

        // The bounded scan reads at most the configured byte budget.
        let small = SearchBudget {
            scan_bytes_max: 0,
            ..budget
        };
        let query = LiteralQuery::new("a_").expect("query");
        let result = bounded_literal_search(&db, &query, &stream, small)
            .await
            .expect("bounded search");
        assert_eq!(result.plan, SearchPlan::BoundedScan);
        assert!(result.matches.is_empty(), "{:?}", result.matches);
        assert!(result.truncated);
        assert_eq!(result.scanned_bytes, 0);
    }

    #[tokio::test]
    async fn literal_search_sql_reports_the_timeout_and_bounds_the_candidates() {
        use sea_orm::{ActiveModelTrait, Set};
        use uuid::Uuid;

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = search_test_db(temp_dir.path()).await;
        let owner = Uuid::new_v4().to_string();
        let target = Uuid::new_v4().to_string();
        let stream = Uuid::new_v4().to_string();
        let checkpoint = Uuid::new_v4().to_string();
        let resource = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        crate::callisto::trace_principal::ActiveModel {
            id: Set(owner.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        crate::callisto::trace_target::ActiveModel {
            id: Set(target.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/project/{target}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        crate::callisto::trace_stream::ActiveModel {
            id: Set(stream.clone()),
            principal_id: Set(owner),
            target_id: Set(target),
            source_repo_id: Set("search-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream");
        crate::callisto::trace_checkpoint::ActiveModel {
            id: Set(checkpoint.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_key: Set("search-checkpoint".to_owned()),
            original_tree: Set("a".repeat(40)),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("checkpoint");
        crate::callisto::trace_resource::ActiveModel {
            id: Set(resource.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_id: Set(checkpoint.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(11),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("resource");
        for index in 0..5 {
            let fragment = Uuid::new_v4().to_string();
            crate::callisto::trace_fragment::ActiveModel {
                id: Set(fragment.clone()),
                stream_id: Set(stream.clone()),
                content_digest: Set(Uuid::new_v4().to_string()),
                index_version: Set(1),
                body: Set(format!("needle body {index}")),
                ascii_fold: Set(format!("needle body {index}")),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("fragment");
            crate::callisto::trace_fragment::membership::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                fragment_id: Set(fragment),
                stream_id: Set(stream.clone()),
                checkpoint_id: Set(checkpoint.clone()),
                resource_id: Set(resource.clone()),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .expect("membership");
        }

        // The candidate cap stops the search at the requested number.
        let budget = SearchBudget {
            candidate_limit: 2,
            ..SearchBudget::from_protocol_limits()
        };
        let query = LiteralQuery::new("needle").expect("query");
        let result = bounded_literal_search(&db, &query, &stream, budget)
            .await
            .expect("capped search");
        assert_eq!(result.plan, SearchPlan::IndexedTrigram);
        assert_eq!(result.matches.len(), 2, "{:?}", result.matches);

        // A statement timeout is reported as a timeout, not as "no match".
        // The same mechanism the search uses is exercised here: the timeout
        // is set on the transaction and the server cancels the statement.
        let txn = db.begin().await.expect("transaction");
        txn.execute_unprepared("SET LOCAL statement_timeout = '1ms'")
            .await
            .expect("statement timeout");
        let error = txn
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_sleep(5)",
            ))
            .await
            .expect_err("timeout");
        assert!(is_timeout_error(&MegaError::Db(error)), "timeout error");
        txn.rollback().await.expect("rollback");
    }
}
