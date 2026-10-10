### Task FIX-OX-15：same-source 并发测试安装 PostgreSQL map repository（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 修正 same-source actual HTTP 并发用例 fixture，使被测请求经过 Postgres chunk-map receipt 安装门；不改生产并发行为。
**Current evidence:** `same_source_cold_actual_http_callers_share_one_full_pass_and_each_recheck_their_receipt` 未安装 `PostgresChunkMapRepository`，等待 `receipt_write_holds` 超时；相邻取消/重试用例显式安装。聚焦 exit=101，36.03s。
**Acceptance criteria:**
- [ ] AC-1：held leader 前安装测试 PostgreSQL repository 与 budget。
- [ ] AC-2：receipt-write gate 实际触发后才创建并发 callers。
- [ ] AC-3：同源 callers 共享一次完整 proof，且逐个重核 receipt。
- [ ] AC-4：异源 storage 的 receipt 故障仍 fail closed。
**Verification:**
- [ ] VER-1：`source .env.test && RUST_LOG=error cargo test -p mega2 --lib same_source_cold_actual_http_callers_share_one_full_pass_and_each_recheck_their_receipt -- --test-threads=1`。
**Dependencies:** `FIX-OX-14`。
**Deliverables:** N/A。
**Implementation write set:** `src/api/router/snapshot_persisted_chunk_map_tests.rs`；`docs/plan/plan-20260920.md`。
**Files likely touched:** 同 Implementation write set。
**Rollback mode:** `revert`（撤回 fixture wiring 并保留失败证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`。
**Release write set:** `N/A`
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=same-source HTTP fixture repository wiring; recovery=撤回 fixture 修订并保留失败证据; complete=yes; self-contained=yes; AC=4/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-14; writeset=序列化于 FIX-OX-14; release=REL-OX-01 plan child; split-from=N/A; exception=N/A`。
