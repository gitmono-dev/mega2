### Task FIX-OX-16：empty raw EOF revocation 用例触达 held source poll（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 调查零字节 raw source revocation 同步；先证明根因属于 fixture/hook，不能预设为测试缺陷。若证据指向生产行为，卡保持 blocked，补具名实现卡并经 ER-05/计划复审后再继续。
**Current evidence:** `empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success` 定向复现等待 `entered.notified()` 超时，exit=101，33.05s；未取得 backend poll 到达的确定性证据。
**Acceptance criteria:**
- [ ] AC-1：证明零字节 source 进入 held EOF poll。
- [ ] AC-2：等待期间撤销 lease，恢复后返回不可重试 `410 LEASE_EXPIRED`。
- [ ] AC-3：恢复后不再 poll backend EOF，source drop 恰一次。
- [ ] AC-4：response 与 scratch budget 全部归还，不产生空成功。
**Verification:**
- [ ] VER-1：`source .env.test && RUST_LOG=error cargo test -p mega2 --lib empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success -- --test-threads=1`。
**Dependencies:** `FIX-OX-15`。
**Deliverables:** N/A。
**Implementation write set:** `src/api/router/snapshot_raw_blob_tests.rs`；`docs/plan/plan-20260920.md`。
**Files likely touched:** 同 Implementation write set。
**Rollback mode:** `revert`（撤回同步修订并保留失败阻塞）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`。
**Release write set:** `N/A`
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=empty raw EOF revocation witness; recovery=撤回同步修订并保持最终 C blocked; complete=yes; self-contained=yes; AC=4/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-15; writeset=序列化于 FIX-OX-15; release=REL-OX-01 plan child; split-from=N/A; exception=N/A`。

### Task FIX-OX-17：fragmented raw post-await revocation 触达 backend poll（implementation）
