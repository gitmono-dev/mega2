//! Canonical GIN descriptor and catalog check for trace fragment search.
//!
//! The DDL runs on its own connection, outside a migration transaction.
//! Repair of an invalid index belongs to a later card.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use crate::common::errors::MegaError;

/// Reviewed `pg_trgm` version. Receipts are not a runtime source for this value.
pub const EXPECTED_PG_TRGM_EXTVERSION: &str = "1.6";
/// Physical index whose `_v1` suffix is index version 1.
pub const CANONICAL_INDEX_NAME: &str = "trace_fragment_ascii_fold_trgm_v1";
pub const CANONICAL_INDEX_VERSION: i64 = 1;
pub const TRUSTED_EXTENSION_SCHEMA: &str = "mega2_extensions";
pub const TRUSTED_OPCLASS: &str = "gin_trgm_ops";
pub const TRUSTED_SCHEMA_OWNER: &str = "mega2_traces_ext_owner";

/// Identity read from the live catalog for one ready index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceGinDescriptor {
    pub database_oid: i64,
    pub schema_oid: i64,
    pub table_oid: i64,
    pub index_oid: i64,
    pub owner: String,
    pub index_version: i64,
}

/// Eight independent catalog predicates. `Ready` means every one passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogFailure {
    Method,
    Opclass,
    Columns,
    IndexVersion,
    NotValid,
    NotReady,
    ExtensionVersion,
    SchemaOwner,
    Missing,
    ExtensionMissing,
    WrongNamespace,
    IndexPermissionDenied,
    BuildTimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogValidation {
    Ready(TraceGinDescriptor),
    Unavailable(CatalogFailure),
}

/// Schema-qualified concurrent DDL. `fragment_schema` must already be `quote_ident` output.
pub fn canonical_index_ddl(fragment_schema: &str) -> String {
    format!(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {CANONICAL_INDEX_NAME} \
         ON {fragment_schema}.trace_fragment \
         USING gin (ascii_fold {TRUSTED_EXTENSION_SCHEMA}.{TRUSTED_OPCLASS})"
    )
}

/// Build and run the canonical DDL on `conn`. The schema name comes from
/// `current_schema()`, not from an operator string.
pub async fn create_canonical_index(conn: &DatabaseConnection) -> Result<(), MegaError> {
    let schema = quoted_current_schema(conn).await?;
    conn.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        canonical_index_ddl(&schema),
    ))
    .await?;
    Ok(())
}

/// Read the fragment table's namespace and the trusted extension catalog.
pub async fn validate_catalog(conn: &DatabaseConnection) -> Result<CatalogValidation, MegaError> {
    let any_extension = one_row(
        conn,
        "SELECT ns.nspname AS schema
         FROM pg_catalog.pg_extension ext
         JOIN pg_catalog.pg_namespace ns ON ns.oid = ext.extnamespace
         WHERE ext.extname = 'pg_trgm'",
    )
    .await?;
    let Some(any_extension) = any_extension else {
        return Ok(CatalogValidation::Unavailable(
            CatalogFailure::ExtensionMissing,
        ));
    };
    if text(&any_extension, "schema").as_deref() != Some(TRUSTED_EXTENSION_SCHEMA) {
        return Ok(CatalogValidation::Unavailable(
            CatalogFailure::WrongNamespace,
        ));
    }
    let extension = one_row(
        conn,
        "SELECT ext.extversion AS version,
                pg_catalog.pg_get_userbyid(ns.nspowner) AS owner
         FROM pg_catalog.pg_extension ext
         JOIN pg_catalog.pg_namespace ns ON ns.oid = ext.extnamespace
         WHERE ext.extname = 'pg_trgm'
           AND ns.nspname = 'mega2_extensions'",
    )
    .await?;
    let Some(extension) = extension else {
        return Ok(CatalogValidation::Unavailable(
            CatalogFailure::ExtensionVersion,
        ));
    };
    if text(&extension, "version").as_deref() != Some(EXPECTED_PG_TRGM_EXTVERSION) {
        return Ok(CatalogValidation::Unavailable(
            CatalogFailure::ExtensionVersion,
        ));
    }
    if text(&extension, "owner").as_deref() != Some(TRUSTED_SCHEMA_OWNER) {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::SchemaOwner));
    }

    let table = one_row(
        conn,
        "SELECT d.oid::bigint AS database_oid,
                n.oid::bigint AS schema_oid,
                c.oid::bigint AS table_oid
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_catalog.pg_database d ON d.datname = current_database()
         WHERE c.relname = 'trace_fragment'
           AND c.relkind = 'r'
           AND n.nspname = current_schema()",
    )
    .await?;
    let Some(table) = table else {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Missing));
    };

    let indexes = conn
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT i.relname AS index_name,
                    i.oid::bigint AS index_oid,
                    am.amname AS method,
                    pg_catalog.pg_get_indexdef(i.oid) AS indexdef,
                    (ix.indnkeyatts = 1 AND ix.indnatts = 1
                     AND ix.indpred IS NULL AND ix.indexprs IS NULL
                     AND NOT ix.indisunique
                     AND pg_catalog.pg_get_indexdef(i.oid) = pg_catalog.format(
                         'CREATE INDEX %I ON %I.%I USING gin (ascii_fold mega2_extensions.gin_trgm_ops)',
                         i.relname, n.nspname, c.relname
                     )) AS canonical_definition,
                    ix.indisvalid AS valid,
                    ix.indisready AS ready,
                    a.attname AS column_name,
                    oc.opcname AS opclass,
                    oc_ns.nspname AS opclass_schema
             FROM pg_catalog.pg_index ix
             JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
             JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_catalog.pg_am am ON am.oid = i.relam
             LEFT JOIN pg_catalog.pg_attribute a
               ON a.attrelid = ix.indrelid AND a.attnum = ix.indkey[0]
             LEFT JOIN pg_catalog.pg_opclass oc ON oc.oid = ix.indclass[0]
             LEFT JOIN pg_catalog.pg_namespace oc_ns ON oc_ns.oid = oc.opcnamespace
             WHERE c.relname = 'trace_fragment'
               AND n.nspname = current_schema()
               AND i.relname LIKE 'trace_fragment_ascii_fold_trgm_v%'
             ORDER BY i.relname"
                .to_owned(),
        ))
        .await?;
    if indexes.is_empty() {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Missing));
    }
    let canonical = indexes
        .iter()
        .find(|row| text(row, "index_name").as_deref() == Some(CANONICAL_INDEX_NAME));
    let Some(index) = canonical else {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::IndexVersion));
    };
    let Some(index_version) =
        index_version_from_name(text(index, "index_name").as_deref().unwrap_or_default())
    else {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::IndexVersion));
    };
    if index_version != CANONICAL_INDEX_VERSION {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::IndexVersion));
    }
    if text(index, "method").as_deref() != Some("gin") {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Method));
    }
    if text(index, "column_name").as_deref() != Some("ascii_fold") {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Columns));
    }
    if text(index, "opclass").as_deref() != Some(TRUSTED_OPCLASS)
        || text(index, "opclass_schema").as_deref() != Some(TRUSTED_EXTENSION_SCHEMA)
    {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Opclass));
    }
    if flag(index, "canonical_definition") != Some(true) {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::Columns));
    }
    if flag(index, "valid") != Some(true) {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::NotValid));
    }
    if flag(index, "ready") != Some(true) {
        return Ok(CatalogValidation::Unavailable(CatalogFailure::NotReady));
    }
    Ok(CatalogValidation::Ready(TraceGinDescriptor {
        database_oid: integer(&table, "database_oid")?,
        schema_oid: integer(&table, "schema_oid")?,
        table_oid: integer(&table, "table_oid")?,
        index_oid: integer(index, "index_oid")?,
        owner: text(&extension, "owner").unwrap_or_default(),
        index_version,
    }))
}

fn index_version_from_name(name: &str) -> Option<i64> {
    let suffix = name.rsplit("_v").next()?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

async fn quoted_current_schema(conn: &DatabaseConnection) -> Result<String, MegaError> {
    let row = one_row(conn, "SELECT quote_ident(current_schema()) AS schema")
        .await?
        .ok_or_else(|| MegaError::Other("current schema is missing".into()))?;
    text(&row, "schema").ok_or_else(|| MegaError::Other("current schema is missing".into()))
}

async fn one_row(
    conn: &DatabaseConnection,
    sql: &str,
) -> Result<Option<sea_orm::QueryResult>, MegaError> {
    Ok(conn
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await?)
}

fn text(row: &sea_orm::QueryResult, column: &str) -> Option<String> {
    row.try_get("", column).ok()
}

fn flag(row: &sea_orm::QueryResult, column: &str) -> Option<bool> {
    row.try_get("", column).ok()
}

fn integer(row: &sea_orm::QueryResult, column: &str) -> Result<i64, MegaError> {
    row.try_get("", column)
        .map_err(|error| MegaError::Other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::*;
    use crate::jupiter::tests::trace_search_catalog_db;

    async fn database() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = trace_search_catalog_db(directory.path()).await;
        // The cloned test database does not copy the server's shared extension.
        // Install it only here. Do not drop the shared `pg_trgm`.
        // The operator-provisioned role is cluster-wide. Do not create or alter
        // roles from a test; CREATE SCHEMA fails if the role is absent.
        exec(
            &db,
            "CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner",
        )
        .await;
        exec(
            &db,
            "CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6'",
        )
        .await;
        (directory, db)
    }

    async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    }

    #[tokio::test]
    async fn gin_catalog_ready_requires_every_predicate() {
        let (_dir, db) = database().await;
        create_canonical_index(&db).await.unwrap();
        match validate_catalog(&db).await.unwrap() {
            CatalogValidation::Ready(descriptor) => {
                assert_eq!(descriptor.index_version, CANONICAL_INDEX_VERSION);
                assert_eq!(descriptor.owner, TRUSTED_SCHEMA_OWNER);
                assert!(descriptor.index_oid > 0);
            }
            CatalogValidation::Unavailable(reason) => {
                panic!("canonical catalog was unavailable: {reason:?}")
            }
        }
    }

    #[tokio::test]
    async fn gin_catalog_method_rejects_btree() {
        let (_dir, db) = database().await;
        exec(
            &db,
            "CREATE INDEX trace_fragment_ascii_fold_trgm_v1 ON trace_fragment (ascii_fold)",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Method)
        );
    }

    #[tokio::test]
    async fn gin_catalog_opclass_rejects_other_ops() {
        let (_dir, db) = database().await;
        create_canonical_index(&db).await.unwrap();
        exec(
            &db,
            "UPDATE pg_catalog.pg_index
             SET indclass = (
                 SELECT oc.oid::text::oidvector
                 FROM pg_catalog.pg_opclass oc
                 JOIN pg_catalog.pg_namespace ns ON ns.oid = oc.opcnamespace
                 WHERE oc.opcname = 'text_ops' AND ns.nspname = 'pg_catalog'
                 LIMIT 1
             )
             WHERE indexrelid = (
                 SELECT i.oid FROM pg_catalog.pg_class i
                 JOIN pg_catalog.pg_namespace n ON n.oid = i.relnamespace
                 WHERE i.relname = 'trace_fragment_ascii_fold_trgm_v1'
                   AND n.nspname = current_schema()
             )",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Opclass)
        );
    }

    #[tokio::test]
    async fn gin_catalog_columns_rejects_other_column() {
        let (_dir, db) = database().await;
        exec(
            &db,
            "CREATE INDEX trace_fragment_ascii_fold_trgm_v1
             ON trace_fragment USING gin (body mega2_extensions.gin_trgm_ops)",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Columns)
        );
    }

    #[tokio::test]
    async fn gin_catalog_rejects_partial_index() {
        let (_dir, db) = database().await;
        exec(&db, "CREATE INDEX trace_fragment_ascii_fold_trgm_v1 ON trace_fragment USING gin (ascii_fold mega2_extensions.gin_trgm_ops) WHERE index_version = 1").await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Columns)
        );
    }

    #[tokio::test]
    async fn gin_catalog_rejects_multiple_columns() {
        let (_dir, db) = database().await;
        exec(&db, "CREATE INDEX trace_fragment_ascii_fold_trgm_v1 ON trace_fragment USING gin (ascii_fold mega2_extensions.gin_trgm_ops, body mega2_extensions.gin_trgm_ops)").await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::Columns)
        );
    }

    #[tokio::test]
    async fn gin_catalog_version_rejects_suffix() {
        let (_dir, db) = database().await;
        exec(
            &db,
            "CREATE INDEX trace_fragment_ascii_fold_trgm_v2
             ON trace_fragment USING gin (ascii_fold mega2_extensions.gin_trgm_ops)",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::IndexVersion)
        );
    }

    #[tokio::test]
    async fn gin_catalog_indisvalid_rejects_false() {
        let (_dir, db) = database().await;
        create_canonical_index(&db).await.unwrap();
        exec(
            &db,
            "UPDATE pg_catalog.pg_index SET indisvalid = false
             WHERE indexrelid = (
                 SELECT i.oid FROM pg_catalog.pg_class i
                 JOIN pg_catalog.pg_namespace n ON n.oid = i.relnamespace
                 WHERE i.relname = 'trace_fragment_ascii_fold_trgm_v1'
                   AND n.nspname = current_schema()
             )",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::NotValid)
        );
    }

    #[tokio::test]
    async fn gin_catalog_indisready_rejects_false() {
        let (_dir, db) = database().await;
        create_canonical_index(&db).await.unwrap();
        exec(
            &db,
            "UPDATE pg_catalog.pg_index SET indisready = false
             WHERE indexrelid = (
                 SELECT i.oid FROM pg_catalog.pg_class i
                 JOIN pg_catalog.pg_namespace n ON n.oid = i.relnamespace
                 WHERE i.relname = 'trace_fragment_ascii_fold_trgm_v1'
                   AND n.nspname = current_schema()
             )",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::NotReady)
        );
    }

    #[tokio::test]
    async fn gin_preconditions_missing_extension() {
        let directory = tempfile::tempdir().unwrap();
        let (db, _guard) = crate::jupiter::tests::test_database_without_extensions()
            .await
            .expect("template0 database");
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let _ = directory;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::ExtensionMissing)
        );
    }

    #[tokio::test]
    async fn gin_preconditions_wrong_namespace() {
        let (_dir, db) = database().await;
        exec(&db, "CREATE SCHEMA IF NOT EXISTS wrong_trgm").await;
        exec(
            &db,
            "UPDATE pg_catalog.pg_extension
             SET extnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = 'wrong_trgm')
             WHERE extname = 'pg_trgm'",
        )
        .await;
        assert_eq!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Unavailable(CatalogFailure::WrongNamespace)
        );
    }

    #[tokio::test]
    async fn gin_preconditions_permission_denied() {
        let (_dir, db) = database().await;
        let role = format!("gin_denied_{}", uuid::Uuid::new_v4().simple());
        exec(&db, &format!("CREATE ROLE {role} NOLOGIN")).await;
        exec(&db, &format!("GRANT {role} TO CURRENT_USER")).await;
        exec(&db, &format!("SET ROLE {role}")).await;
        let schema = quoted_current_schema(&db).await.unwrap();
        let error = db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                canonical_index_ddl(&schema),
            ))
            .await
            .unwrap_err();
        exec(&db, "RESET ROLE").await;
        exec(&db, &format!("DROP OWNED BY {role}")).await;
        exec(&db, &format!("DROP ROLE {role}")).await;
        assert!(format!("{error}").contains("permission denied"), "{error}");
    }

    #[tokio::test]
    async fn gin_trust_extension_version_requires_reviewed_constant() {
        let (_dir, db) = database().await;
        let row = super::one_row(
            &db,
            "SELECT extversion AS version FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'",
        )
        .await
        .unwrap()
        .expect("shared pg_trgm");
        assert_eq!(
            super::text(&row, "version").as_deref(),
            Some(EXPECTED_PG_TRGM_EXTVERSION)
        );
        assert_ne!(EXPECTED_PG_TRGM_EXTVERSION, "9.9");
        create_canonical_index(&db).await.unwrap();
        assert!(matches!(
            validate_catalog(&db).await.unwrap(),
            CatalogValidation::Ready(_)
        ));
    }

    #[tokio::test]
    async fn gin_trust_schema_owner_requires_traces_ext_owner() {
        let (_dir, db) = database().await;
        let row = super::one_row(
            &db,
            "SELECT pg_catalog.pg_get_userbyid(ns.nspowner) AS owner
             FROM pg_catalog.pg_namespace ns
             WHERE ns.nspname = 'mega2_extensions'",
        )
        .await
        .unwrap()
        .expect("shared extension schema");
        assert_eq!(
            super::text(&row, "owner").as_deref(),
            Some(TRUSTED_SCHEMA_OWNER)
        );
        create_canonical_index(&db).await.unwrap();
        match validate_catalog(&db).await.unwrap() {
            CatalogValidation::Ready(descriptor) => {
                assert_eq!(descriptor.owner, TRUSTED_SCHEMA_OWNER)
            }
            CatalogValidation::Unavailable(reason) => panic!("{reason:?}"),
        }
    }
}
