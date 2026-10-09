# Frozen plan card excerpt

Frozen plan SHA-256: `46d83eb6cf655ddbb96f80f960878c825691954e91b8c6f1515032768dbd9d74`

### Task FIX-OX-09：snapshot objects verified fact fixture 一致性（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 自 FIX-OX-02 拆出 snapshot_objects_bounded_tests.rs 单条 conflicting-sizes fixture 的调用点；共享构造入口与 InitialFact scaffold 由 FIX-OX-02 唯一拥有，本卡只消费并保留故意冲突的 verified fact，不改生产 fail-closed 逻辑。
**Current evidence:** `src/api/router/snapshot_objects_bounded_tests.rs:353`；诊断性全量运行确认该具名 fixture 失败，原进程 SIGINT exit=130 不能当作 C 证据。
**Acceptance criteria:**
- [ ] AC-1：本具名 synthetic fixture 通过共享构造入口在 rooted resolve 前注入 fact，保留 rooted lease 路径。
- [ ] AC-2：首轮故意设置与 body 不一致的 size，仍在 body I/O 前得到 `INTEGRITY_ERROR/502`。
- [ ] AC-3：恢复 size 后，两个不同 OID 的 body 各自接受验证。
- [ ] AC-4：恢复后错误 expected digest 仍得到 409。
- [ ] AC-5：消费 FIX-OX-02 已接受的共享构造入口时保持默认选项行为；本卡不修改该入口。
- [ ] AC-6：只改测试 fixture，生产 fail-closed 逻辑不动。
**Verification:**
- [ ] VER-1：`source .env.test && cargo test -p mega2 --lib conflicting_sizes_reject_before_io_and_distinct_oids_still_verify_each_body -- --test-threads=1`。
**Dependencies:** `FIX-OX-02`。
**Implementation write set:** src/api/router/snapshot_objects_bounded_tests.rs、docs/plan/plan-20260920.md（本卡状态/证据）；不写 snapshot_content_tests.rs。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回 fixture 子提交，保留生产 fail-closed）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=snapshot objects 负向 fact fixture 一致性; recovery=撤回 fixture 子提交并保留生产失败保护; complete=yes; self-contained=yes; AC=6/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-02; writeset=序列化于 FIX-OX-02; release=REL-OX-01 child; split-from=FIX-OX-02; exception=N/A`。

