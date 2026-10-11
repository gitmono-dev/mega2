### Task FIX-OX-17：fragmented raw post-await revocation 触达 backend poll（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 调查四种 fragment/EOF raw revocation 同步；先证明根因属于 fixture/hook，不能预设为测试缺陷。若证据指向生产行为，卡保持 blocked，补具名实现卡并经 ER-05/计划复审后再继续。
**Current evidence（2026-10-11 修订）：** `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll` 在 checkpoint 全量与本轮首次 focused 均通过（exit=0 / 194.12s 与 231.48s）；该测试文件不在 checkpoint source diff 中，不能把通过归因于 checkpoint WIP。本卡执行 VER-1 的“三次连续 focused”后**未全通过**：base 源三次为 exit 0 / 101 / 101，两次失败均为 `src/api/router/snapshot_raw_blob_tests.rs:530:14` 的 `Elapsed(())`（即 `entered` 屏障超时）。因此按本文规定**不得用空 diff 关闭**，本卡转入“任一失败则继续调查”分支：证明生产撤销语义正确（四例均进入 held poll、撤销后 `LEASE_EXPIRED`、`tail=0`、`drops=1`、两项预算归零、prefix 与 case 一致），并把根因定位为测试侧固定 10 s 屏障余量不足（实测 `barrier_wait_ms = 6200 / 6141 / 9448 / 8817`，占预算 62%–94%；5 s 短屏障探针可复现 `entered_within_5s=false`）。交付：一个 test-only hunk，只把 `entered` 屏障改为 60 s、释放后窗口改为 30 s，断言不变、无生产文件改动；修复后三次连续 VER-1 为 exit 0 / 0 / 0（210.66 s / 204.86 s / 214.48 s）。AC-5 按原文不适用；AC-1/AC-3/AC-4 由 witness 断言，AC-2 的 prefix 由 witness 断言、`LEASE_EXPIRED` 终止由诊断支持（该路径无可观察 410）。原空 diff 收口条件保留在下方 AC-5 中，作为未触发的备选路径。
**Acceptance criteria:**
- [ ] AC-1：四个 case 均在 backend 实际 held 后才撤销 lease。
- [ ] AC-2：恢复后 body 以 `LEASE_EXPIRED` 终止且 delivered prefix 符合该 case。
- [ ] AC-3：不再 poll tail，source drop 恰一次。
- [ ] AC-4：response 与 scratch budget 全部归还。
- [ ] AC-5：若验证三次连续通过且 checkpoint/A-B 无本卡 source delta，则以空代码差异、无生产变更的 evidence-only 交付关闭；source/review/evidence 的精确本地提交仍须满足 ER-05 与本计划门。
**Verification:**
- [ ] VER-1：连续执行三次 `source .env.test && RUST_LOG=error cargo test -p mega2 --lib raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll -- --test-threads=1`；每次须原始退出码 0 且报告 `1 passed; 0 failed`。
**Dependencies:** `FIX-OX-16`。
**Deliverables:** N/A。
**Implementation write set:** `src/api/router/snapshot_raw_blob_tests.rs`；`docs/plan/plan-20260920.md`。
**Files likely touched:** 同 Implementation write set。
**Rollback mode:** `revert`（撤回同步修订并保持失败阻塞）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`。
**Release write set:** `N/A`
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=fragmented raw post-await revocation witness; recovery=撤回同步修订并保持发布 blocked; complete=yes; self-contained=yes; AC=5/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-16; writeset=序列化于 FIX-OX-16; release=REL-OX-01 plan child; split-from=N/A; exception=N/A`。

### Task FIX-OX-18：rooted HTTP 事务屏障覆盖真实请求路径（implementation）
