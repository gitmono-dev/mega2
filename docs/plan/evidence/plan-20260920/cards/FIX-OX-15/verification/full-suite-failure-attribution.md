# FIX-OX-15 repository-wide failure attribution

## cargo test --all

- Command: source .env.test && cargo test --all
- Exit code: 101.
- Captured individual statuses: 1841 passed and 34 failed; 3 tests had a long-running notice but no terminal status.
- The captured output has no final libtest result line and no per-test failure-detail blocks. The individual causes of the failed statuses and the reason the run ended before a summary are not established by this evidence.
- Compared with the earlier FIX-OX-04 failure inventory (13 names; tested HEAD c10776d9e84d2301e953d673fe0e0c70dad1fb1a), 6 failed names overlap and 28 are not in that inventory. This comparison identifies recurrence only; it does not establish root cause.
- The three tests still running when this command exited were rerun individually with --test-threads=1 and all passed. This is consistent with whole-suite scheduling or resource contention but does not prove that explanation and does not attribute the other failed statuses.
- This command is an incomplete failed run, not a repository-wide test pass. OX-284 final C still requires the plan's final full-suite gate.

### Reported failures by test area

| Area | Count |
|---|---:|
| bounded_chunks | 1 |
| bounded_objects | 8 |
| chunk_map_retention | 5 |
| other snapshot content tests | 2 |
| persisted_chunk_maps | 4 |
| raw_blob | 5 |
| rooted_metadata | 9 |

### Failed test names

- api::router::snapshot_router::content::tests::bounded_objects::cancelling_actual_object_request_drops_held_stream_and_retry_succeeds
- api::router::snapshot_router::content::tests::bounded_objects::lease_revoked_during_object_load_cannot_deliver_verified_frames
- api::router::snapshot_router::content::tests::bounded_objects::a_later_object_stream_failure_keeps_earlier_verified_data_unpublished
- api::router::snapshot_router::content::tests::bounded_objects::conflicting_sizes_reject_before_io_and_distinct_oids_still_verify_each_body
- api::router::snapshot_router::content::tests::bounded_objects::oversized_item_and_later_invalid_path_reject_entire_batch_before_body_io
- api::router::snapshot_router::content::tests::chunk_map_retention::completion_during_real_inventory_keeps_backing_credit_after_pending_install_disappears
- api::router::snapshot_router::content::tests::chunk_map_retention::actual_warm_body_owner_cancels_never_returning_open_and_next_without_backend_release
- api::router::snapshot_router::content::tests::bounded_objects::cap_boundaries_and_empty_object_keep_exact_raw_bytes_and_end_counts
- api::router::snapshot_router::content::tests::chunk_map_retention::late_cancelled_create_is_reserved_and_old_key_replay_preserves_new_generation
- api::router::snapshot_router::content::tests::chunk_map_retention::held_raw_and_exact_range_do_not_resume_after_actual_reader_expiry
- api::router::snapshot_router::content::tests::bounded_objects::unique_batch_over_eight_mib_rejects_before_any_body_io
- api::router::snapshot_router::content::tests::bounded_chunks::mst2_large_chunk_uses_current_oid_strict_range_faults_cancel_retry_and_lease
- api::router::snapshot_router::content::tests::persisted_chunk_maps::different_current_sources_enter_cold_installations_independently
- api::router::snapshot_router::content::tests::persisted_chunk_maps::failed_digest_failed_body_and_cancelled_cold_producer_allow_the_joined_current_source_to_retry
- api::router::snapshot_router::content::tests::persisted_chunk_maps::failed_or_cancelled_leader_and_cancelled_waiter_leave_actual_http_retry_capacity
- api::router::snapshot_router::content::tests::bounded_objects::every_alias_is_admitted_and_exact_oid_body_is_loaded_once
- api::router::snapshot_router::content::tests::raw_blob::empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success
- api::router::snapshot_router::content::tests::mst2_fixed_head_uses_verified_facts_without_body_reads_and_preserves_raw_bytes
- api::router::snapshot_router::content::tests::persisted_chunk_maps::receipt_orphan_failure_and_cancelled_install_release_owned_credit_and_replay_atomically
- api::router::snapshot_router::content::tests::mst2_fixed_warm_map_and_leaf_aliases_skip_body_reads_and_chunks_use_current_ranges
- api::router::snapshot_router::content::tests::chunk_map_retention::json_chunk_and_raw_last_transport_clones_block_actual_collection
- api::router::snapshot_router::content::tests::raw_blob::raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll
- api::router::snapshot_router::content::tests::rooted_metadata::actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes
- api::router::snapshot_router::content::tests::rooted_metadata::actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content
- api::router::snapshot_router::content::tests::raw_blob::raw_drop_cancel_and_revocation_during_held_io_drop_producer_and_all_owned_credit
- api::router::snapshot_router::content::tests::rooted_metadata::actual_q_lease_http_and_direct_handoff_retry_source_lock_without_partial_durable_mutation
- api::router::snapshot_router::content::tests::rooted_metadata::reader_retention::stale_actual_reader_cannot_read_or_finish_a_reissued_uuid
- api::router::snapshot_router::content::tests::rooted_metadata::reader_retention::upgrade::current_reader_retention_migration_verifies_fresh_family_without_rewriting_history
- api::router::snapshot_router::content::tests::rooted_metadata::reader_retention::upgrade::native_runtime::captured_cc90_interrupted_before_reader_migration_resumes_without_reader_ddl
- api::router::snapshot_router::content::tests::rooted_metadata::rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish
- api::router::snapshot_router::content::tests::raw_blob::raw_physical_size_and_current_fact_fail_before_body_and_path_errors_keep_formal_statuses
- api::router::snapshot_router::content::tests::raw_blob::warm_raw_corruption_growth_truncation_and_late_error_never_yield_the_last_bytes
- api::router::snapshot_router::content::tests::rooted_metadata::reader_retention::committed_metadata_requests_retire_owners_without_retiring_source_history
- api::router::snapshot_router::content::tests::rooted_metadata::actual_q_body_callers_preserve_unchanged_source_revision_and_ignore_temp_shadow

### Tests without terminal status in the full run

- api::router::snapshot_router::content::tests::raw_blob::cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers
- api::router::snapshot_router::content::tests::raw_blob::raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding
- api::router::snapshot_router::content::tests::rooted_metadata::rooted_wide_directory_windows_and_lookup_survive_rebuild_with_valid_proofs

### Serial follow-up runs

- api::router::snapshot_router::content::tests::raw_blob::cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers: exit 0; 1 passed; 877.83s; exact output and exit evidence are under verification/diagnostic/full-suite-tail/.
- api::router::snapshot_router::content::tests::raw_blob::raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding: exit 0; 1 passed; 895.07s; exact output and exit evidence are under verification/diagnostic/full-suite-tail/.
- api::router::snapshot_router::content::tests::rooted_metadata::rooted_wide_directory_windows_and_lookup_survive_rebuild_with_valid_proofs: exit 0; 1 passed; 187.48s; exact output and exit evidence are under verification/diagnostic/full-suite-tail/.
