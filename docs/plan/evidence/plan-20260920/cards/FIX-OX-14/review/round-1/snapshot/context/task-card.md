### Task FIX-OX-14：METADATA_NOT_READY 可重试断言（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 对齐实际 rooted HTTP stale-file-fact 测试与统一错误映射：`METADATA_NOT_READY` 的 `retryable` 为 true；保留 HEAD 读取与零 body-read 的原行为断言。
**Current evidence:** `snapshot_rooted_metadata_tests.rs::actual_q_body_callers_recheck_only_returned_current_file_facts` 在串行全量基线报告 `retryable=true, expected=false`；`snapshot_router.rs:507-512` 将 `MetadataNotReady` 统一映射为 `retryable=true`。当前工作树已改该布尔期望，定向 1/1 PASS、35.93s；尚未 review/精确提交。
**Acceptance criteria:**
- [ ] AC-1：非当前 verified file fact 的 GET 仍返回 503 `METADATA_NOT_READY` 且 `retryable=true`，不读取 body。
- [ ] AC-2：独立 HEAD `/empty` 仍返回 200 和零 content size；该修订不改变错误映射或生产逻辑。
**Verification:**
- [ ] VER-1：`source .env.test && cargo test -p mega2 --lib actual_q_body_callers_recheck_only_returned_current_file_facts -- --test-threads=1`。
**Dependencies:** `FIX-OX-13`。
**Implementation write set:** `src/api/router/snapshot_rooted_metadata_tests.rs`、`docs/plan/plan-20260920.md`（本卡证据）。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回错误 expectation 并保留 baseline 失败证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=METADATA_NOT_READY retryable test expectation; recovery=撤回测试 expectation 修订并阻止发布; complete=yes; self-contained=yes; AC=2/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-13; writeset=序列化于 FIX-OX-13; release=REL-OX-01 child; split-from=N/A; exception=N/A`。
