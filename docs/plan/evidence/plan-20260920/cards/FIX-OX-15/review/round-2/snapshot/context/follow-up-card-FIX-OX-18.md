### Task FIX-OX-18：rooted HTTP 事务屏障覆盖真实请求路径（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 调查 actual-Q HTTP 与 rooted reader 生命周期测试屏障和真实请求执行路径的注入关系；不得预设为测试缺陷。若重复 focused run 不能稳定触达 hook，或证据指向生产行为，本卡保持 blocked，先按 fail-closed amendment 增加具名修复卡并经 Claude 复审。全量执行顺序下的复现由 FIX-OX-03 VER-3 的完整串行 readiness 覆盖。
**Current evidence:** `actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes`、`actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content`、`stale_actual_reader_cannot_read_or_finish_a_reissued_uuid`、`rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish` 均在 4 秒 admission barrier 处失败于 checkpoint 全量运行；focused 重跑有通过与失败，说明屏障未到达具有间歇性。相关用例均通过 `with_rooted_reader_barriers` / `with_rooted_source_fact_barriers` 包裹真实 `Router::oneshot` 请求，hook 位于 `qualified_metadata_reader.rs`；当前证据尚不能把原因归为 hook 缺陷或生产行为。
**Acceptance criteria:**
- [ ] AC-1：held-current-fact 用例到达 admission 后才验证写事务等待。
- [ ] AC-2：source-mutation 用例在 admission 后改源并证明旧内容不返回。
- [ ] AC-3：两请求均结束 reader operation 并清理 REQUEST/READER anchors。
- [ ] AC-4：test hook 不泄漏到其它测试。
- [ ] AC-5：`stale_actual_reader_cannot_read_or_finish_a_reissued_uuid` 的三次连续 focused run 均到达真实 HTTP reader admission，并各自报告 `1 passed; 0 failed`。
- [ ] AC-6：`rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish` 的三次连续 focused run 均到达真实 HTTP reader admission，并各自报告 `1 passed; 0 failed`。
**Verification:**
- [ ] VER-1：连续执行三次 `source .env.test && RUST_LOG=error cargo test -p mega2 --lib actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes -- --test-threads=1`；每次均须原始退出码 0、`running 1 test` 且 `1 passed; 0 failed`，并到达真实 HTTP admission 后验证写事务等待。
- [ ] VER-2：连续执行三次 `source .env.test && RUST_LOG=error cargo test -p mega2 --lib actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content -- --test-threads=1`；每次均须原始退出码 0 且报告 `1 passed; 0 failed`。
- [ ] VER-3：连续执行三次 `source .env.test && RUST_LOG=error cargo test -p mega2 --lib rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish -- --test-threads=1`；每次均须原始退出码 0 且报告 `1 passed; 0 failed`。
- [ ] VER-4：连续执行三次 `source .env.test && RUST_LOG=error cargo test -p mega2 --lib stale_actual_reader_cannot_read_or_finish_a_reissued_uuid -- --test-threads=1`；每次均须原始退出码 0 且报告 `1 passed; 0 failed`。
**Dependencies:** `FIX-OX-17`。
**Deliverables:** N/A。
**Implementation write set:** `src/jupiter/storage/qualified_metadata_reader.rs`、`src/api/router/snapshot_rooted_metadata_tests.rs`、`src/api/router/snapshot_reader_retention_tests.rs`、`docs/plan/plan-20260920.md`。
**Files likely touched:** 同 Implementation write set。
**Rollback mode:** `revert`（撤回 test-only hook 并保持竞态验证 blocked）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`。
**Release write set:** `N/A`
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=rooted HTTP test barrier propagation; recovery=撤回 test-only hook 并保持竞态验证 blocked; complete=yes; self-contained=yes; AC=6/8; VER=4/8; landing=1; prod-files=1; scope=S; deps=FIX-OX-17; writeset=序列化于 FIX-OX-17; release=REL-OX-01 plan child; split-from=N/A; exception=N/A`。
