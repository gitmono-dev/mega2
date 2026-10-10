### Task FIX-OX-13：PostgreSQL boolean 测试断言类型（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 修复三个测试把 PostgreSQL boolean 标量直接 `::bigint` 的无效断言投影；只改测试 SQL，保留 true/false 语义及生产 SQL。
**Current evidence:** `qualified_metadata_gc_tests.rs::admitted_orphan_gc_preserves_replay_identity_and_advances_exact_generation` 使用 `SELECT mst2_metadata_gc_enabled()::bigint`；`qualified_metadata_source_revision_tests.rs` 的两个 `current_source_revision_*` 测试使用 `mst2_route_source_tree_matches(...)::bigint`。串行全量日志均为 PostgreSQL `42846 cannot cast type boolean to bigint`，定位 `/tmp/mega2-baseline-audit.log`。
**Acceptance criteria:**
- [ ] AC-1：三个布尔投影改用 PostgreSQL 支持且语义等价的布尔读取或 `CASE` 映射，不改变 schema/function 返回类型。
- [ ] AC-2：GC enabled 与 source-tree match 的真值断言仍能区分 true/false，三项原业务断言保持不变。
**Verification:**
- [ ] VER-1：`source .env.test && cargo test -p mega2 --lib admitted_orphan_gc_preserves_replay_identity_and_advances_exact_generation -- --test-threads=1`。
- [ ] VER-2：`source .env.test && cargo test -p mega2 --lib current_source_revision_uses_captured_core_relations_under_temp_shadow -- --test-threads=1`。
- [ ] VER-3：`source .env.test && cargo test -p mega2 --lib current_source_revision_share_fence_orders_real_source_update_after_read -- --test-threads=1`。
**Dependencies:** `FIX-OX-12`。
**Implementation write set:** `src/jupiter/storage/qualified_metadata_gc_tests.rs`、`src/jupiter/storage/qualified_metadata_source_revision_tests.rs`、`docs/plan/plan-20260920.md`（本卡证据）。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回测试 SQL 变更并保留失败证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=PostgreSQL boolean 测试断言类型; recovery=撤回测试 SQL 修订并阻止发布; complete=yes; self-contained=yes; AC=2/8; VER=3/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-12; writeset=序列化于 FIX-OX-12; release=REL-OX-01 child; split-from=N/A; exception=N/A`。
