### Task FIX-OX-02：既有 MST2 bounded snapshot fixture 基线修复（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 修复 snapshot_chunks_bounded_tests.rs 两条 rooted fixture 的构造时序并建立 snapshot_content_tests.rs options scaffold（含 InitialFact、chunk faults 与 lease 选项）；正向 body/fact 必须匹配，batch 用例保留原 413 预算与 404 后续路径拒绝。该 helper scaffold 由本卡唯一拥有，FIX-OX-05/09/12 只消费。
**Current evidence:** `src/api/router/snapshot_chunks_bounded_tests.rs:161,387` 两条测试；`src/api/router/snapshot_objects_bounded_tests.rs:353` 一条测试；诊断性全量运行已确认三条具名 fixture 失败，并见 20+ snapshot/retention 失败待归因；该进程因高噪声/资源占用由执行者 SIGINT 终止，exit=130，不构成 C 结果。
**Acceptance criteria:**
- [ ] AC-1：large-chunk 正向 fixture 在 rooted resolve 前确定与其模拟 body 一致的 size。
- [ ] AC-2：large-chunk 正向 fixture 在 rooted resolve 前确定与其模拟 body 一致的 digest。
- [ ] AC-3：large-chunk 路径不在 rooted certification 后篡改已认证 fact。
- [ ] AC-4：batch 用例的超出 live budget 请求仍在 body I/O 前返回 `413/LIMIT_EXCEEDED`。
- [ ] AC-5：batch 用例的后续无效路径仍在 body I/O 前返回 `404/PATH_NOT_FOUND`。
- [ ] AC-6：共享构造入口的选项只作用于具名 synthetic fixture，其他测试默认路径保持。
**Verification:**
- [ ] VER-1：`source .env.test && cargo test -p mega2 --lib mst2_large_chunk_uses_current_oid_strict_range_faults_cancel_retry_and_lease -- --test-threads=1`。
- [ ] VER-2：`source .env.test && cargo test -p mega2 --lib mst2_chunk_batch_live_budget_and_invalid_later_path_reject_before_body_io -- --test-threads=1`。
**Dependencies:** `FIX-OX-01`。
**Implementation write set:** `src/api/router/snapshot_chunks_bounded_tests.rs`、`src/api/router/snapshot_content_tests.rs`（共享 fixture 构造入口）、`docs/plan/plan-20260920.md`（本卡状态/证据）。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤销测试 fixture 子提交，保留生产 fail-closed）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=snapshot chunks verified fact fixture 一致性; recovery=撤销 fixture 子提交并保留生产失败保护; complete=yes; self-contained=yes; AC=6/8; VER=2/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-01; writeset=序列化于 FIX-OX-01; release=REL-OX-01 child; split-from=N/A; exception=N/A`。
