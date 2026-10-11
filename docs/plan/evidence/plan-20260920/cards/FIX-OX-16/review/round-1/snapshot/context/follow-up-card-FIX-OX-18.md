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

### Task FIX-OX-23：96 次 metadata lookup 的 fixture lease 根因诊断（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 只改 `committed_metadata_requests_retire_owners_without_retiring_source_history` 的 test fixture 调用，通过 `Fixture::new_in_publication_mode_with_options(true, 0, &[], false, true, FixtureOptions { lease_seconds: Some(3600), ..FixtureOptions::default() })` 显式请求 3600 秒 lease；不改生产 lease expiry、SQL、anchor guard 或 shared fixture constructor。A 使用原 fixture 调用作对照，B 只改变此调用点。B 的诊断结果必须走唯一分支：anchor mismatch 500 被复现→允许该诊断测试按预期失败验收并解除 FIX-OX-19 根因阻塞；96 次 lookup 全部完成且测试所有断言通过→FIX-OX-19/FIX-OX-21 保持 blocked，先修订计划删除未证实的 production root；其它错误（含仍为 410、未到 96 或无关 SQL 错误）→FIX-OX-19/FIX-OX-21 保持 blocked，按 FIX-OX-04 AC-3/AC-4 增加新根因卡并复审。
**Current evidence:** 原 fixture 在 focused run 616.32s 遇到 410 `LEASE_EXPIRED`，在历史 anchor assertion 前失败；该耗时不是完整 96-request 时长。显式 3600 秒仅提供 test-only 诊断余量。
**Acceptance criteria:**
- [ ] AC-1：A/B 仅在该测试调用点不同；B 通过 `FixtureOptions.lease_seconds = Some(3600)` 设置 lease，生产文件与共享 fixture constructor 不变。
- [ ] AC-2：A 与 B 均保留原始退出码、`running/passed/failed` 摘要、首个非成功 response/status、错误类别及断言位置；B 结果归入 Description 的一个且仅一个分支。
- [ ] AC-3：若重现 anchor mismatch 500，B 必须以 exit=101 报告 `running 1 test`、`0 passed; 1 failed`，并保存显示 HTTP 500 与 PostgreSQL anchor/active-lease mismatch 的脱敏 response/error 证据；此精确预期失败可通过本诊断卡验收并解除 FIX-OX-19 的根因诊断阻塞。
- [ ] AC-4：若 96 次 lookup 全部完成且测试的全部断言通过，B 必须以 exit=0 报告 `running 1 test`、`1 passed; 0 failed`；不实施 FIX-OX-19/21，并先提交经 Claude 复审的计划修订。
- [ ] AC-5：任何其它结果均使 FIX-OX-23 本身及 FIX-OX-19/21 在 plan-status 中保持 blocked/not accepted，按 FIX-OX-04 AC-3/AC-4 建立新根因卡并经 Claude 复审；记录实际退出码与摘要，不把未声明失败当作通过。
**Verification:**
- [ ] VER-1：`source .env.test && RUST_LOG=error cargo test -p mega2 --lib api::router::snapshot_router::content::tests::rooted_metadata::reader_retention::committed_metadata_requests_retire_owners_without_retiring_source_history -- --exact --test-threads=1`；核原始退出码、lease 设置与 Description 的结果分支。
**Dependencies:** `FIX-OX-18`。
**Deliverables:** focused 原始输出与脱敏摘要，A/B、review 与精确本地提交。
**Implementation write set:** `src/api/router/snapshot_reader_retention_tests.rs`（仅该用例 fixture-options 调用点）、`docs/plan/plan-20260920.md`。
**Files likely touched:** 同 Implementation write set。
**Rollback mode:** `revert`（撤回 test-only lease 选项；不改生产 expiry 行为）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`。
**Release write set:** `N/A`
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=fixture lease precondition for the 96-lookup anchor diagnosis; recovery=撤回 test-only lease option 并保持 production fix blocked; complete=yes; self-contained=yes; AC=5/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-18; writeset=序列化于 FIX-OX-18; release=REL-OX-01 plan child; split-from=N/A; exception=N/A`。

### Task FIX-OX-19：reader retention 请求保持精确有效 anchor（implementation）
