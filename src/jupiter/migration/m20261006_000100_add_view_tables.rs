//! HP-09: persistence tables for history-projection views.
//!
//! This migration deliberately has no foreign keys. The view tables include
//! persistent definitions, rebuildable projections, and a rate-limit ledger.
//! Their writers are introduced by later history-projection task cards.

use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_filter (
                id bigint NOT NULL,
                filter_id text NOT NULL,
                canonical_spec text NOT NULL,
                algo_version smallint NOT NULL,
                object_format text NOT NULL,
                src_paths jsonb NOT NULL,
                push_enabled boolean NOT NULL,
                projected_seq bigint NOT NULL DEFAULT 0,
                ready_seq bigint,
                warming_since timestamp,
                last_access_at timestamp,
                created_at timestamp NOT NULL,
                PRIMARY KEY (id),
                UNIQUE (filter_id)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view (
                id bigint NOT NULL,
                name text NOT NULL,
                version integer NOT NULL,
                filter_pk bigint NOT NULL,
                created_by text NOT NULL,
                created_at timestamp NOT NULL,
                PRIMARY KEY (id),
                UNIQUE (name, version)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_mega_view_filter_pk ON mega_view (filter_pk)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_root_chain (
                seq bigint NOT NULL,
                commit_id text NOT NULL,
                tree_id text NOT NULL,
                PRIMARY KEY (seq),
                UNIQUE (commit_id)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_root_chain_scan (
                pos bigint NOT NULL,
                commit_id text NOT NULL,
                tree_id text NOT NULL,
                parent_count smallint NOT NULL,
                first_parent text,
                PRIMARY KEY (pos)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_commit_map (
                filter_pk bigint NOT NULL,
                seq_from bigint NOT NULL,
                view_commit text,
                view_tree text NOT NULL,
                PRIMARY KEY (filter_pk, seq_from),
                UNIQUE (filter_pk, view_commit)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_object (
                object_id text NOT NULL,
                kind smallint NOT NULL,
                data bytea NOT NULL,
                created_at timestamp NOT NULL,
                gc_marked_at timestamp,
                PRIMARY KEY (object_id)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_object_ref (
                filter_pk bigint NOT NULL,
                object_id text NOT NULL,
                PRIMARY KEY (filter_pk, object_id)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_mega_view_object_ref_object_id \
             ON mega_view_object_ref (object_id)",
        )
        .await?;

        // Keep this final: the migration rollback test injects a same-named
        // view, letting CREATE TABLE be a no-op and this index creation fail.
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS mega_view_register_log (
                id bigserial NOT NULL,
                requester text NOT NULL,
                created_at timestamp NOT NULL DEFAULT now(),
                PRIMARY KEY (id)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_mega_view_register_log_requester_created_at \
             ON mega_view_register_log (requester, created_at)",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
