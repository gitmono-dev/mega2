//! Database migration module for the Jupiter application.
//!
//! This module provides database migration functionality using SeaORM's migration framework.
//! It contains all migration files and utilities for managing database schema changes.
//!
//! # Overview
//!
//! The migrator handles database schema evolution through versioned migration files.
//! Each migration is represented as a separate module and implements the `MigrationTrait`.
//!
//! # Migration Files
//!
//! - `m20250314_025943_init` - Initial database schema setup
//! - `m20250427_031332_add_mr_refs_tag` - Adds merge request reference tagging
//! - `m20250605_013340_alter_mega_mr_index` - Modifies merge request indexing
//! - `m20250610_000001_add_vault_storage` - Adds vault storage functionality
//! - `m20250613_033821_alter_user_id` - Alters user ID column definitions
//! - `m20250618_065050_add_label` - Adds label functionality to issues
//!
//! # Usage
//!
//! ```rust,ignore
//! use crate::jupiter::migrator::apply_migrations;
//!
//! // Apply pending migrations
//! apply_migrations(&db, false).await?;
//!
//! // Refresh all migrations (development only)
//! apply_migrations(&db, true).await?;
//! ```
use sea_orm_migration::{prelude::*, schema::big_integer};

mod m20250314_025943_init;
mod m20250427_031332_add_mr_refs_tag;
mod m20250605_013340_alter_mega_mr_index;
mod m20250610_000001_add_vault_storage;
mod m20250613_033821_alter_user_id;
mod m20250618_065050_add_label;
mod m20250628_025312_add_username_in_conversation;
mod m20250702_072055_add_item_assignees;
mod m20250710_073119_create_reactions;
mod m20250725_103004_add_note;
mod m20250804_151214_alter_builds_end_at;
mod m20250812_022434_alter_mega_mr;
mod m20250815_075653_remove_commit_id;
mod m20250819_025231_alter_builds;
mod m20250820_102133_gpgkey;
mod m20250821_083749_add_checks;
mod m20250828_092459_remove_gpg_table;
mod m20250828_092729_create_standalone_table;
mod m20250903_013904_create_task_table;
mod m20250903_071928_add_issue_refs;
mod m20250904_074945_modify_tasks_and_builds;
mod m20250904_120000_add_commit_auths;
mod m20250905_163011_add_mr_reviewer;
mod m20250910_153212_add_username_to_reviewer;
mod m20250930_024736_mr_to_cl;
mod m20251011_091944_tasks_mr_id_to_cl_id;
mod m20251012_071700_mr_to_cl_batch;
mod m20251021_073817_rename_mr_sync_to_cl_sync;
mod m20251026_065433_drop_user_table;
mod m20251027_062734_add_metadata_to_object;
mod m20251107_025431_add_cl_commits;
mod m20251109_073000_add_merge_queue;
mod m20251117_101804_add_commit_id_in_mega_tree;
mod m20251117_181240_add_system_required_field_for_reviewer;
mod m20251119_145041_add_draft_status;
mod m20251125_135032_add_draft_conv_type;
mod m20251128_000001_create_buck_session;
mod m20251203_013745_add_dynamic_sidebar;
mod m20251210_113942_remove_unique_constraint_from_order_index;
mod m20260106_070511_add_retry_time;
mod m20260106_070515_remove_relay_mq_lfs_raw_table;
mod m20260108_085945_remove_splited_in_lfs_objects;
mod m20260108_105158_remove_storage_type_enum;
mod m20260115_000000_create_targets_table;
mod m20260119_060233_add_mega_code_review;
mod m20260127_081517_create_build_triggers;
mod m20260128_080549_add_mega_code_review_anchor_and_position;
mod m20260130_065535_refactor_orion_module;
mod m20260208_012349_change_build_events;
mod m20260209_064016_remove_default_dynamic_sidebar;
mod m20260210_062050_create_target_state_history;
mod m20260216_013852_create_group_permission_tables;
mod m20260224_142019_create_target_build_status;
mod m20260224_230000_create_notification_center;
mod m20260228_100254_change_build_target_and_add_index_for_build_event_start_at;
include!("m20260302_082846_register.rs");
mod m20260304_013434_seed_cla_sign_check_config;
mod m20260306_121829_create_bots_related_table;
mod m20260308_191753_create_webhook;
mod m20260308_220000_add_base_branch_to_mega_cl;
mod m20260308_230000_normalize_webhook_event_types;
mod m20260316_120000_add_bot_tokens_token_hash_index;
mod m20260324_024559_add_notes;
mod m20260324_033322_fix_migration;
mod m20260327_034553_drop_legacy_tasks;
mod m20260413_033315_create_artifact_tables;
mod m20260529_013000_chat_foundations;
mod m20260529_023000_channel_chat;
mod m20260618_063000_add_preferred_locale_to_notification_settings;
mod m20260618_064500_create_email_job_attachments;
mod m20260619_120000_create_user_inbox_notifications;
mod m20260623_050000_chat_index_hardening;
mod m20260623_060000_attachment_soft_delete;
mod m20260630_000000_seed_notification_event_types;
mod m20260701_000000_fix_reaction_unique_nulls;
mod m20260731_000000_drop_chat_and_notes;
pub(crate) mod m20260731_000001_drop_email_jobs;
mod m20260815_000000_unique_mega_cl_link;
mod m20260815_000100_merge_queue_requester;
mod m20260831_000000_drop_mega_issue_tables;
mod m20260902_000100_add_oci_tables;
pub(crate) mod m20260905_000100_add_push_queue;
mod m20260905_000200_mega_cl_revision;
mod m20260905_000300_add_mega_ref_tombstones;
mod m20260905_000400_add_authz_outbox;
mod m20260905_000500_add_blob_paths;
mod m20260905_000600_mega_refs_path_pattern;
mod m20260910_000100_drop_merge_queue;
mod m20260912_000100_drop_orion_build_tables;
mod m20260913_000100_add_agent_capture_tables;
mod m20260916_000100_add_mst2_verified_object;
mod m20260917_000100_add_mst2_retention;
mod m20260917_000200_add_mst2_publication;
mod m20260918_000100_fix_mst2_publication_unique;
mod m20260918_000100_mega_tag_path;
mod m20260918_000200_drop_notification_settings_delivery_columns;
pub(crate) mod m20260919_000100_drop_user_inbox_notifications;
mod m20260919_000200_drop_dynamic_sidebar;
pub(crate) mod m20260919_000300_drop_reactions;
pub(crate) mod m20260919_000400_drop_mega_cl_reviewer;
pub(crate) mod m20260919_000500_delete_code_review_check_rows;
pub(crate) mod m20260919_000600_drop_mega_code_review;
pub(crate) mod m20260919_000700_drop_label_tables;
pub(crate) mod m20260919_000800_delete_cla_sign_check_rows;
pub(crate) mod m20260919_000900_drop_cla_status;
mod m20260921_000100_fix_git_tag_unique;
mod m20260923_000100_import_repo_cleanups;
mod m20260923_000200_canonicalize_import_repo_paths;
mod m20260925_000100_media_paging;
mod runner;
pub use m20260905_000100_add_push_queue::ensure_queue_control_seed;
pub use runner::apply_migrations;

/// Primary key `BIGINT` (not DB auto-increment); the application assigns `id` (e.g. `idgenerator::IdInstance::next_id`).
fn pk_bigint<T: IntoIden>(name: T) -> ColumnDef {
    big_integer(name).primary_key().take()
}

/// The main migrator struct that implements the migration trait.
///
/// This struct is responsible for managing all database migrations in the correct order.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20250314_025943_init::Migration),
            Box::new(m20250427_031332_add_mr_refs_tag::Migration),
            Box::new(m20250605_013340_alter_mega_mr_index::Migration),
            Box::new(m20250610_000001_add_vault_storage::Migration),
            Box::new(m20250613_033821_alter_user_id::Migration),
            Box::new(m20250618_065050_add_label::Migration),
            Box::new(m20250628_025312_add_username_in_conversation::Migration),
            Box::new(m20250702_072055_add_item_assignees::Migration),
            Box::new(m20250710_073119_create_reactions::Migration),
            Box::new(m20250725_103004_add_note::Migration),
            Box::new(m20250804_151214_alter_builds_end_at::Migration),
            Box::new(m20250812_022434_alter_mega_mr::Migration),
            Box::new(m20250815_075653_remove_commit_id::Migration),
            Box::new(m20250819_025231_alter_builds::Migration),
            Box::new(m20250820_102133_gpgkey::Migration),
            Box::new(m20250821_083749_add_checks::Migration),
            Box::new(m20250828_092459_remove_gpg_table::Migration),
            Box::new(m20250828_092729_create_standalone_table::Migration),
            Box::new(m20250903_013904_create_task_table::Migration),
            Box::new(m20250903_071928_add_issue_refs::Migration),
            Box::new(m20250904_074945_modify_tasks_and_builds::Migration),
            Box::new(m20250904_120000_add_commit_auths::Migration),
            Box::new(m20250905_163011_add_mr_reviewer::Migration),
            Box::new(m20250910_153212_add_username_to_reviewer::Migration),
            Box::new(m20250930_024736_mr_to_cl::Migration),
            Box::new(m20251011_091944_tasks_mr_id_to_cl_id::Migration),
            Box::new(m20251012_071700_mr_to_cl_batch::Migration),
            Box::new(m20251021_073817_rename_mr_sync_to_cl_sync::Migration),
            Box::new(m20251026_065433_drop_user_table::Migration),
            Box::new(m20251027_062734_add_metadata_to_object::Migration),
            Box::new(m20251107_025431_add_cl_commits::Migration),
            Box::new(m20251109_073000_add_merge_queue::Migration),
            Box::new(m20251117_101804_add_commit_id_in_mega_tree::Migration),
            Box::new(m20251117_181240_add_system_required_field_for_reviewer::Migration),
            Box::new(m20251119_145041_add_draft_status::Migration),
            Box::new(m20251125_135032_add_draft_conv_type::Migration),
            Box::new(m20251128_000001_create_buck_session::Migration),
            Box::new(m20251203_013745_add_dynamic_sidebar::Migration),
            Box::new(m20251210_113942_remove_unique_constraint_from_order_index::Migration),
            Box::new(m20260106_070511_add_retry_time::Migration),
            Box::new(m20260106_070515_remove_relay_mq_lfs_raw_table::Migration),
            Box::new(m20260108_085945_remove_splited_in_lfs_objects::Migration),
            Box::new(m20260108_105158_remove_storage_type_enum::Migration),
            Box::new(m20260115_000000_create_targets_table::Migration),
            Box::new(m20260119_060233_add_mega_code_review::Migration),
            Box::new(m20260127_081517_create_build_triggers::Migration),
            Box::new(m20260128_080549_add_mega_code_review_anchor_and_position::Migration),
            Box::new(m20260130_065535_refactor_orion_module::Migration),
            Box::new(m20260208_012349_change_build_events::Migration),
            Box::new(m20260209_064016_remove_default_dynamic_sidebar::Migration),
            Box::new(m20260210_062050_create_target_state_history::Migration),
            Box::new(m20260216_013852_create_group_permission_tables::Migration),
            Box::new(m20260224_142019_create_target_build_status::Migration),
            Box::new(m20260224_230000_create_notification_center::Migration),
            Box::new(m20260228_100254_change_build_target_and_add_index_for_build_event_start_at::Migration),
            cla_status_create_migration(),
            Box::new(m20260304_013434_seed_cla_sign_check_config::Migration),
            Box::new(m20260306_121829_create_bots_related_table::Migration),
            Box::new(m20260308_191753_create_webhook::Migration),
            Box::new(m20260308_220000_add_base_branch_to_mega_cl::Migration),
            Box::new(m20260308_230000_normalize_webhook_event_types::Migration),
            Box::new(m20260316_120000_add_bot_tokens_token_hash_index::Migration),
            Box::new(m20260324_024559_add_notes::Migration),
            Box::new(m20260324_033322_fix_migration::Migration),
            Box::new(m20260327_034553_drop_legacy_tasks::Migration),
            Box::new(m20260413_033315_create_artifact_tables::Migration),
            Box::new(m20260529_013000_chat_foundations::Migration),
            Box::new(m20260529_023000_channel_chat::Migration),
            Box::new(m20260618_063000_add_preferred_locale_to_notification_settings::Migration),
            Box::new(m20260618_064500_create_email_job_attachments::Migration),
            Box::new(m20260619_120000_create_user_inbox_notifications::Migration),
            Box::new(m20260623_050000_chat_index_hardening::Migration),
            Box::new(m20260623_060000_attachment_soft_delete::Migration),
            Box::new(m20260630_000000_seed_notification_event_types::Migration),
            Box::new(m20260701_000000_fix_reaction_unique_nulls::Migration),
            Box::new(m20260731_000000_drop_chat_and_notes::Migration),
            Box::new(m20260731_000001_drop_email_jobs::Migration),
            Box::new(m20260815_000000_unique_mega_cl_link::Migration),
            Box::new(m20260815_000100_merge_queue_requester::Migration),
            Box::new(m20260831_000000_drop_mega_issue_tables::Migration),
            Box::new(m20260905_000100_add_push_queue::Migration),
            Box::new(m20260905_000200_mega_cl_revision::Migration),
            Box::new(m20260905_000300_add_mega_ref_tombstones::Migration),
            Box::new(m20260905_000400_add_authz_outbox::Migration),
            Box::new(m20260905_000500_add_blob_paths::Migration),
            Box::new(m20260905_000600_mega_refs_path_pattern::Migration),
            Box::new(m20260910_000100_drop_merge_queue::Migration),
            Box::new(m20260902_000100_add_oci_tables::Migration),
            Box::new(m20260912_000100_drop_orion_build_tables::Migration),
            Box::new(m20260913_000100_add_agent_capture_tables::Migration),
            Box::new(m20260916_000100_add_mst2_verified_object::Migration),
            Box::new(m20260917_000100_add_mst2_retention::Migration),
            Box::new(m20260917_000200_add_mst2_publication::Migration),
            Box::new(m20260918_000100_mega_tag_path::Migration),
            Box::new(m20260918_000100_fix_mst2_publication_unique::Migration),
            Box::new(m20260918_000200_drop_notification_settings_delivery_columns::Migration),
            Box::new(m20260919_000100_drop_user_inbox_notifications::Migration),
            Box::new(m20260919_000200_drop_dynamic_sidebar::Migration),
            Box::new(m20260919_000300_drop_reactions::Migration),
            Box::new(m20260919_000400_drop_mega_cl_reviewer::Migration),
            Box::new(m20260919_000500_delete_code_review_check_rows::Migration),
            Box::new(m20260919_000600_drop_mega_code_review::Migration),
            Box::new(m20260919_000700_drop_label_tables::Migration),
            Box::new(m20260919_000800_delete_cla_sign_check_rows::Migration),
            Box::new(m20260919_000900_drop_cla_status::Migration),
            Box::new(m20260921_000100_fix_git_tag_unique::Migration),
            Box::new(m20260923_000100_import_repo_cleanups::Migration),
            Box::new(m20260923_000200_canonicalize_import_repo_paths::Migration),
            Box::new(m20260925_000100_media_paging::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
    use sea_orm_migration::prelude::*;

    use super::{
        Migrator, m20260923_000100_import_repo_cleanups,
        m20260923_000200_canonicalize_import_repo_paths::{
            self, AliasRewrite, canonicalize_import_repo_paths,
        },
    };
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    fn migration_names() -> Vec<String> {
        Migrator::migrations()
            .iter()
            .map(|m| m.name().to_owned())
            .collect()
    }

    async fn scalar_bool(db: &sea_orm::DatabaseConnection, sql: &str) -> bool {
        db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "v")
            .unwrap()
    }

    #[tokio::test]
    async fn import_repo_cleanups_migration_applies() {
        let names = migration_names();
        let at = names
            .iter()
            .position(|name| name == "m20260923_000100_import_repo_cleanups")
            .expect("registered");
        assert_eq!(
            names[at - 1],
            "m20260921_000100_fix_git_tag_unique",
            "appended after the migrations it was written against"
        );

        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        assert!(
            scalar_bool(
                &db,
                "SELECT to_regclass('import_repo_cleanups') IS NOT NULL AS v"
            )
            .await
        );
        let index: String = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT indexdef FROM pg_indexes WHERE schemaname = current_schema() \
                 AND tablename = 'import_repo_cleanups' \
                 AND indexname = 'idx_import_repo_cleanups_path_state_id'",
            ))
            .await
            .unwrap()
            .expect("the (path, state, id) index exists")
            .try_get("", "indexdef")
            .unwrap();
        assert!(index.ends_with("(path, state, id)"), "{index}");
        assert!(
            db.execute_unprepared(
                "INSERT INTO import_repo_cleanups (id, path, repo_id, state, requester) \
                 VALUES (1, '/third-party/x', 1, 'pending', 'anonymous')"
            )
            .await
            .is_err(),
            "state is only detached or swept"
        );

        // Re-running `up` over the existing table and index is a no-op.
        let manager = SchemaManager::new(&db);
        m20260923_000100_import_repo_cleanups::Migration
            .up(&manager)
            .await
            .unwrap();

        // `down` keeps a ledger that still has rows, including one whose
        // insert is still in flight on another connection when `down` starts.
        let exists = "SELECT to_regclass('import_repo_cleanups') IS NOT NULL AS v";
        let insert = "INSERT INTO import_repo_cleanups (id, path, repo_id, state, requester) \
                      VALUES (1, '/third-party/x', 1, 'detached', 'anonymous')";
        let txn = db.begin().await.unwrap();
        txn.execute_unprepared(insert).await.unwrap();
        let down_db = db.clone();
        let down = tokio::spawn(async move {
            m20260923_000100_import_repo_cleanups::Migration
                .down(&SchemaManager::new(&down_db))
                .await
        });
        // Commit only once `down` is queued on the table lock.
        let mut waiting = false;
        for _ in 0..200 {
            let row = txn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM pg_locks \
                     WHERE relation = 'import_repo_cleanups'::regclass AND NOT granted \
                     AND database = (SELECT oid FROM pg_database WHERE datname = current_database())) \
                     AS v",
                ))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                waiting = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(waiting, "down waits for the open insert");
        txn.commit().await.unwrap();
        assert!(
            down.await.unwrap().is_err(),
            "down sees the committed row and refuses"
        );
        assert!(scalar_bool(&db, exists).await);
        assert!(
            scalar_bool(
                &db,
                "SELECT EXISTS (SELECT 1 FROM import_repo_cleanups) AS v"
            )
            .await
        );

        // An empty ledger is dropped, and `up` recreates it.
        db.execute_unprepared("DELETE FROM import_repo_cleanups")
            .await
            .unwrap();
        m20260923_000100_import_repo_cleanups::Migration
            .down(&manager)
            .await
            .unwrap();
        assert!(!scalar_bool(&db, exists).await);
        m20260923_000100_import_repo_cleanups::Migration
            .down(&manager)
            .await
            .unwrap();
        m20260923_000100_import_repo_cleanups::Migration
            .up(&manager)
            .await
            .unwrap();
        assert!(scalar_bool(&db, exists).await);
    }

    async fn alias_db() -> sea_orm::DatabaseConnection {
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        db
    }

    async fn insert_repo(db: &sea_orm::DatabaseConnection, id: i64, path: &str) {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
             VALUES ($1, $2, 'r', now(), now())",
            [id.into(), path.into()],
        ))
        .await
        .unwrap();
    }

    async fn repo_paths(db: &sea_orm::DatabaseConnection) -> Vec<(i64, String)> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id, repo_path FROM git_repo ORDER BY id",
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get("", "id").unwrap(),
                row.try_get("", "repo_path").unwrap(),
            )
        })
        .collect()
    }

    async fn run_alias_migration(db: &sea_orm::DatabaseConnection) {
        let txn = db.begin().await.unwrap();
        m20260923_000200_canonicalize_import_repo_paths::Migration
            .up(&SchemaManager::new(&txn))
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    async fn rewrite_aliases(db: &sea_orm::DatabaseConnection) -> AliasRewrite {
        let txn = db.begin().await.unwrap();
        let summary = canonicalize_import_repo_paths(&txn).await.unwrap();
        txn.commit().await.unwrap();
        summary
    }

    #[tokio::test]
    async fn import_repo_alias_rows_canonicalized() {
        let names = migration_names();
        assert_eq!(
            &names[names.len() - 3..],
            &[
                "m20260923_000100_import_repo_cleanups".to_string(),
                "m20260923_000200_canonicalize_import_repo_paths".to_string(),
                "m20260925_000100_media_paging".to_string(),
            ],
            "media paging registered last, after the FU-14/16 migrations"
        );

        let db = alias_db().await;
        insert_repo(&db, 1, "/third-party//a").await;
        insert_repo(&db, 2, "/third-party/b/").await;
        insert_repo(&db, 3, "/third-party/./c").await;
        insert_repo(&db, 4, "/third-party/d").await;
        db.execute_unprepared(
            "INSERT INTO import_refs \
             (id, repo_id, ref_name, ref_git_id, ref_type, default_branch, created_at, updated_at) \
             VALUES (100, 1, 'refs/heads/main', 'aaaa', 'branch', true, now(), now())",
        )
        .await
        .unwrap();

        let summary = rewrite_aliases(&db).await;
        assert_eq!(
            summary,
            AliasRewrite {
                rewritten: 3,
                collisions: 0,
                invalid: 0,
            }
        );
        assert_eq!(
            repo_paths(&db).await,
            vec![
                (1, "/third-party/a".to_owned()),
                (2, "/third-party/b".to_owned()),
                (3, "/third-party/c".to_owned()),
                (4, "/third-party/d".to_owned()),
            ]
        );
        assert!(
            scalar_bool(
                &db,
                "SELECT EXISTS (SELECT 1 FROM import_refs WHERE id = 100 AND repo_id = 1) AS v"
            )
            .await,
            "refs stay with the same repo_id"
        );
    }

    #[tokio::test]
    async fn import_repo_alias_collision_left_untouched() {
        use std::{
            io::Write,
            sync::{Arc, Mutex},
        };

        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(buffer.clone()))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        // Thread-local: the rewrite runs on this test's single-threaded runtime.
        let _capture = tracing::subscriber::set_default(subscriber);
        // Only this migration's events: sqlx's slow-statement WARNs land on
        // the same thread under load.
        let target = format!(
            "WARN {}:",
            module_path!().replace(
                "::tests",
                "::m20260923_000200_canonicalize_import_repo_paths"
            )
        );
        let warnings = || -> Vec<String> {
            String::from_utf8(buffer.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter(|line| line.contains(&target))
                .map(str::to_owned)
                .collect()
        };

        let db = alias_db().await;
        insert_repo(&db, 10, "/third-party/x").await;
        insert_repo(&db, 11, "/third-party//x").await;
        let summary = rewrite_aliases(&db).await;
        assert_eq!(
            summary,
            AliasRewrite {
                rewritten: 0,
                collisions: 1,
                invalid: 0,
            }
        );
        assert_eq!(
            repo_paths(&db).await,
            vec![
                (10, "/third-party/x".to_owned()),
                (11, "/third-party//x".to_owned()),
            ]
        );
        // One warning per collision, naming only the repo_id and the canonical
        // path, never the stored alias (GC-FU-05).
        let logged = warnings();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(
            logged[0].contains("repo_id=11") && logged[0].contains("canonical=/third-party/x"),
            "{logged:?}"
        );
        assert!(!logged[0].contains("//x"), "{logged:?}");

        // Two aliases of a free canonical path: the older one takes it.
        insert_repo(&db, 13, "/third-party//y").await;
        insert_repo(&db, 14, "/third-party/y/").await;
        let summary = rewrite_aliases(&db).await;
        assert_eq!((summary.rewritten, summary.collisions), (1, 2));
        let paths = repo_paths(&db).await;
        assert!(
            paths.contains(&(13, "/third-party/y".to_owned())),
            "{paths:?}"
        );
        assert!(
            paths.contains(&(14, "/third-party/y/".to_owned())),
            "{paths:?}"
        );

        // Paths with no canonical form stay as they are, counted apart, and
        // the warning names only the repo_id.
        insert_repo(&db, 15, "/third-party/../z").await;
        insert_repo(&db, 16, "/third-party\\w").await;
        let summary = rewrite_aliases(&db).await;
        assert_eq!(
            (summary.rewritten, summary.collisions, summary.invalid),
            (0, 2, 2)
        );
        let paths = repo_paths(&db).await;
        assert!(
            paths.contains(&(15, "/third-party/../z".to_owned())),
            "{paths:?}"
        );
        assert!(
            paths.contains(&(16, "/third-party\\w".to_owned())),
            "{paths:?}"
        );
        let logged = warnings();
        for id in ["repo_id=15", "repo_id=16"] {
            assert!(logged.iter().any(|line| line.contains(id)), "{logged:?}");
        }
        assert!(
            logged.iter().all(|line| !line.contains("//")
                && !line.contains("/third-party/y/")
                && !line.contains("../z")
                && !line.contains("\\w")),
            "{logged:?}"
        );

        // An alias whose insert is still open on another connection when the
        // rewrite starts is not missed: the rewrite waits for it.
        let txn = db.begin().await.unwrap();
        txn.execute_unprepared(
            "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
             VALUES (17, '/third-party//late', 'r', now(), now())",
        )
        .await
        .unwrap();
        let rewrite_db = db.clone();
        let rewrite = tokio::spawn(async move { rewrite_aliases(&rewrite_db).await });
        let mut waiting = false;
        for _ in 0..200 {
            let row = txn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM pg_locks \
                     WHERE relation = 'git_repo'::regclass AND NOT granted \
                     AND database = (SELECT oid FROM pg_database WHERE datname = current_database())) \
                     AS v",
                ))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                waiting = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(waiting, "the rewrite waits for the open insert");
        txn.commit().await.unwrap();
        assert_eq!(rewrite.await.unwrap().rewritten, 1);
        assert!(
            repo_paths(&db)
                .await
                .contains(&(17, "/third-party/late".to_owned()))
        );
    }

    #[tokio::test]
    async fn import_repo_alias_migration_idempotent() {
        let db = alias_db().await;
        insert_repo(&db, 1, "/third-party//a/").await;
        insert_repo(&db, 2, "/third-party/b").await;
        run_alias_migration(&db).await;
        let snapshot = "SELECT string_agg(id || '=' || repo_path || '@' || updated_at, ',' ORDER BY id) \
                        AS v FROM git_repo";
        let read = |db: &sea_orm::DatabaseConnection| {
            let db = db.clone();
            async move {
                db.query_one_raw(Statement::from_string(DbBackend::Postgres, snapshot))
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get::<String>("", "v")
                    .unwrap()
            }
        };
        let first = read(&db).await;
        run_alias_migration(&db).await;
        assert_eq!(read(&db).await, first, "a second run changes nothing");
        assert_eq!(rewrite_aliases(&db).await, AliasRewrite::default());
        assert_eq!(
            repo_paths(&db).await,
            vec![
                (1, "/third-party/a".to_owned()),
                (2, "/third-party/b".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn import_repo_alias_non_default_import_dir() {
        use crate::jupiter::storage::{
            base_storage::{BaseStorage, StorageConnector},
            git_db_storage::GitDbStorage,
        };

        let db = alias_db().await;
        insert_repo(&db, 21, "/vendor//lib/").await;
        run_alias_migration(&db).await;
        let git_db = GitDbStorage {
            base: BaseStorage::new(std::sync::Arc::new(db)),
        };
        let found = git_db
            .find_git_repo_exact_match("/vendor/lib")
            .await
            .unwrap()
            .expect("canonical lookup hits the rewritten row");
        assert_eq!(found.id, 21);
    }
}
