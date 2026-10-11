### Task FIX-OX-17：fragmented raw post-await revocation 触达 backend poll（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `pending` / 空
**Description:** 调查四种 fragment/EOF raw revocation 同步；先证明根因属于 fixture/hook，不能预设为测试缺陷。若证据指向生产行为，卡保持 blocked，补具名实现卡并经 ER-05/计划复审后再继续。
**Current evidence:** `raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll` 在 checkpoint 全量与本轮 focused 均通过；focused exit=0 / 194.12s。该测试文件不在 checkpoint source diff 中，不能将通过归因于 checkpoint WIP。若三次重复 focused 均通过且 A/B 确认没有本卡 source hunk，则允许按“无代码差异”验收并提交本卡 review/verification evidence；若任一失败则继续调查，不得用空 diff 关闭失败。
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
