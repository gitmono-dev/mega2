//! Literal keyword search over the stored fragment index (MTR-41).
//!
//! The query is always bound as a parameter and its `LIKE` pattern is
//! escaped, so a caller can never inject SQL or change the match semantics.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, QueryResult, Statement};

use crate::{
    ceres::traces::query::{LiteralQuery, like_pattern},
    common::errors::MegaError,
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
}
