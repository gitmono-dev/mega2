|---|---|
| `same_source_cold_actual_http_callers_share_one_full_pass_and_each_recheck_their_receipt` | receipt-write gate 未触发；FIX-OX-15 |
| `cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers` | 600s fixture lease 到期；FIX-OX-12 |
| `empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success` | entered barrier 超时；FIX-OX-16 |
| `raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding` | lease 到期，目标分支未到达；FIX-OX-12 |
| `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll` | held backend entered barrier 超时；FIX-OX-17 |
| `actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes` | 4s source-fact admission barrier 超时；FIX-OX-18 |
| `actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content` | 4s reader admission barrier 超时；FIX-OX-18 |
| `actual_q_body_callers_recheck_only_returned_current_file_facts` | retryable 期望与契约不符；FIX-OX-14 |
| `committed_metadata_requests_retire_owners_without_retiring_source_history` | 先由 FIX-OX-23 延长 test-only lease 并诊断；只有重现 anchor 与 active lease operation 不匹配的 500 才由 FIX-OX-19 修复 |
| `http_resolve_captured_before_commit_never_mixes_new_sequence_with_old_descriptor` | 暂时 handoff 冲突映射为 500；FIX-OX-20 |
| `admitted_orphan_gc_preserves_replay_identity_and_advances_exact_generation` | PostgreSQL boolean→bigint cast；FIX-OX-13 |
| `current_source_revision_uses_captured_core_relations_under_temp_shadow` | 同一测试 SQL cast；FIX-OX-13 |
| `current_source_revision_share_fence_orders_real_source_update_after_read` | 同一测试 SQL cast；FIX-OX-13 |

**Checkpoint-inclusive evidence (2026-10-10):** 在 checkpoint 执行副本 HEAD `c10776d9e84d2301e953d673fe0e0c70dad1fb1a`、计划文件 SHA-256 `46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74` 上执行 VER-1，exit=101；`2448 passed; 13 failed; 3 ignored; 0 measured; 0 filtered out; 18457.34s`。环境：Darwin 27.0.0 arm64、rustc 1.99.0、cargo 1.99.0；加载 `.env.test`，`RUST_LOG=error`，`--test-threads=1`。完整原始日志 `/tmp/mega2-baseline-audit.twOQQk.log`（SHA-256 `9670b9443028a0cce77ef2b2e00fd6a643067edbab488d6735f4e27381b2c3ba`，319425 行）；VER-2 精确 `rg -n` 检索 exit=0、136 条匹配（输出 SHA-256 `e3c665ce644cc1a86f53dce0b3025dfc892ca6ba1aab1b895de0e85ebdb91042`，仅临时保留）；不提交原始日志或检索输出，仅在本表保留脱敏摘要。
| checkpoint 执行结果（精确测试名） | 全量结果与 focused 定向结果 | 当前具名 owner |
|---|---|---|
| `same_source_cold_actual_http_callers_share_one_full_pass_and_each_recheck_their_receipt` | 全量失败；focused exit=101 / 35.98s，真实 HTTP callers 未到达 same-source install gate（`Elapsed(())`）。 | FIX-OX-15 |
| `cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers` | 全量失败；focused exit=101 / 614.68s，`LeaseExpired`。 | FIX-OX-12 |
| `empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success` | 全量失败；focused exit=101 / 32.00s，EOF source-poll entered barrier 为 `Elapsed(())`。 | FIX-OX-16 |
| `raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding` | 全量失败；focused exit=101 / 616.69s，`LeaseExpired`，目标 receipt/corruption 分支被租约过期遮蔽。 | FIX-OX-12 |
| `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll` | 全量通过；focused exit=0 / 194.12s。历史 barrier 超时本轮未复现；该测试文件不在 checkpoint source diff 中，不能把通过归因于 checkpoint WIP。 | FIX-OX-17（历史输入；focused 通过后仍按计划完成验收） |
| `actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes` | 全量失败；focused exit=101 / 24.19s，4s source-fact admission barrier 为 `Elapsed(())`。 | FIX-OX-18 |
| `actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content` | 全量失败；focused 两次分别 exit=0 / 25.26s、exit=101 / 26.17s（4s reader admission barrier `Elapsed(())`），表现间歇；未据单次通过宣告关闭。 | FIX-OX-18 |
| `actual_q_body_callers_recheck_only_returned_current_file_facts` | 全量通过；focused exit=0 / 31.67s。checkpoint WIP 将 retryable 期望改为 `true`，归 FIX-OX-14。 | FIX-OX-14（checkpoint 后结果改变） |
| `committed_metadata_requests_retire_owners_without_retiring_source_history` | 全量失败；focused exit=101 / 616.32s，返回 410 `LEASE_EXPIRED`，未复现历史 anchor mismatch 500。由 FIX-OX-23 先做 test-only 3600s lease 诊断；只有复现 500 才启动 FIX-OX-19，不能把本轮 410 当成 anchor 根因已修复。 | FIX-OX-23；条件分支 FIX-OX-19 |
| `stale_actual_reader_cannot_read_or_finish_a_reissued_uuid`（本轮全量新增失败） | 全量在 4s reader admission barrier 失败；focused 两次均 exit=0 / 25.58s、26.57s，属于全量顺序下的间歇性 hook 未触达。 | FIX-OX-18（先证明 test hook 与真实请求路径的关系；语义断言仍由后续 reader-retention 验收覆盖） |
| `rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish`（本轮全量新增失败） | 全量失败；focused exit=101 / 26.00s，4s reader admission barrier 为 `Elapsed(())`。 | FIX-OX-18 |
| `http_resolve_captured_before_commit_never_mixes_new_sequence_with_old_descriptor` | 全量失败；focused exit=101 / 28.57s，实际 500、预期 503，错误为暂时 qualified-handoff 冲突。 | FIX-OX-20 |
| `admitted_orphan_gc_preserves_replay_identity_and_advances_exact_generation` | 全量失败；focused exit=101 / 10.23s，PostgreSQL `42846: cannot cast type boolean to bigint`。 | FIX-OX-13 |
