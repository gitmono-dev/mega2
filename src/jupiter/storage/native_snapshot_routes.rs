//! Permanent namespace selection precedes the generic physical session proof.

use super::{
    ConnectionTrait, DatabaseTransaction, PostgresMetadataInstallRepository, SESSION_SQL,
    SnapshotError, integrity, internal, statement,
};

fn function(installer: &PostgresMetadataInstallRepository, name: &str) -> String {
    format!(
        "\"{}\".{name}",
        installer.captured_schema().replace('"', "\"\"")
    )
}

pub(super) async fn enter(
    txn: &DatabaseTransaction,
    installer: &PostgresMetadataInstallRepository,
) -> Result<(), SnapshotError> {
    installer.verify_primary_connection(txn).await?;
    txn.execute_raw(statement(
        &format!(
            "SELECT {}(current_schema())",
            function(installer, "mst2_route_enter")
        ),
        [],
    ))
    .await
    .map_err(internal)?;
    generic_path(txn, installer).await?;
    Ok(())
}

pub(super) async fn generic_path(
    txn: &DatabaseTransaction,
    installer: &PostgresMetadataInstallRepository,
) -> Result<(), SnapshotError> {
    let schema = installer.captured_schema().replace('"', "\"\"");
    txn.execute_unprepared(&format!(
        "SET LOCAL search_path=\"{schema}\",pg_catalog,pg_temp"
    ))
    .await
    .map_err(internal)?;
    Ok(())
}

pub(super) fn session_sql(installer: &PostgresMetadataInstallRepository) -> String {
    let schema = installer.captured_schema().replace('"', "\"\"");
    let mut sql = SESSION_SQL.to_owned();
    for table in [
        "mst2_metadata_storage_scope",
        "mst2_snapshot_context",
        "mst2_snapshot_lease",
        "mst2_retention_node",
        "mst2_retention_root",
        "mst2_metadata_prepare",
        "mst2_native_publication",
        "mst2_publication",
        "mst2_publication_outbox",
    ] {
        for join in ["FROM", "JOIN"] {
            sql = sql.replace(
                &format!("{join} {table} "),
                &format!("{join} \"{schema}\".{table} "),
            );
        }
    }
    sql
}

pub(super) async fn snapshot<C: ConnectionTrait>(
    connection: &C,
    installer: &PostgresMetadataInstallRepository,
    sid: &str,
) -> Result<(), SnapshotError> {
    let row = connection
        .query_one_raw(statement(
            &format!(
                "SELECT * FROM {}($1,current_schema())",
                function(installer, "mst2_route_select_snapshot")
            ),
            [sid.into()],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| integrity("snapshot storage route selection is missing"))?;
    let context: bool = row.try_get("", "context_present").map_err(internal)?;
    let route: bool = row.try_get("", "route_present").map_err(internal)?;
    let valid: bool = row.try_get("", "valid").map_err(internal)?;
    if context != route || context && !valid {
        return Err(integrity(
            "snapshot storage route conflicts with its immutable generic session",
        ));
    }
    Ok(())
}

pub(super) async fn lease<C: ConnectionTrait>(
    connection: &C,
    installer: &PostgresMetadataInstallRepository,
    lease_id: &str,
) -> Result<Option<String>, SnapshotError> {
    let row = connection
        .query_one_raw(statement(
            &format!(
                "SELECT * FROM {}($1,current_schema())",
                function(installer, "mst2_route_select_lease")
            ),
            [lease_id.into()],
        ))
        .await
        .map_err(internal)?
        .ok_or_else(|| integrity("lease storage route selection is missing"))?;
    let sid: Option<String> = row.try_get("", "actual_sid").map_err(internal)?;
    let route: bool = row.try_get("", "route_present").map_err(internal)?;
    let valid: bool = row.try_get("", "valid").map_err(internal)?;
    if sid.is_some() != route || sid.is_some() && !valid {
        return Err(integrity(
            "lease storage route conflicts with its exact generic incarnation",
        ));
    }
    Ok(sid)
}
