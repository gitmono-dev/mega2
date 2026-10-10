### Task FIX-OX-05：alias 租约覆盖慢速 rooted 读取（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `in-progress` / `locally-accepted`
**Description:** 修复 `every_alias_is_admitted_and_exact_oid_body_is_loaded_once` 的 fixture 租约时长不足，使 128 alias 的真实 rooted 读取在单线程下仍于有效租约内验证原 409 契约；不改生产路由错误映射。
**Current evidence:** 原 HEAD 单线程默认 600s lease 的历史聚焦用例约 676s，第二请求实际 503、预期 409；历史临时日志未作为仓库证据保留。当前执行副本以 HEAD `b69827d7727c6e6d5124c05d311e195085bf781f` 进行 A/B：A 反向屏蔽本卡 fixture hunk 后 exit=101 / 654.71s，第二请求实际 503、预期 409；B 保留 rooted fixture 的 3600s lease，最终原始副本 exit=0 / 775.63s（1 passed），仍验证完整 128 alias body 读取与第二请求 409。另一次 B 首试在 462.78s 报首请求 503，响应体未保存；随后只增强临时副本失败诊断的 B 重跑 exit=0 / 771.23s，未插桩 B 最终重跑再次通过。逐次退出码、摘要和日志 SHA-256 见 `docs/plan/evidence/plan-20260920/cards/FIX-OX-05/`；临时副本路径按 ER-11 脱敏。该证据是卡级 VER，不是 REL-OX-01 最终树全量 C。
**Acceptance criteria:**
- [x] AC-1：fixture 在 rooted resolve 前显式设置覆盖本用例最坏读取时间的 lease，不能通过删除 alias 或跳过 body 验证缩短路径。
- [x] AC-2：第二请求仍断言原 409；单线程定向复现通过。诊断输出增强由后续 FIX-OX-06 单独验收，不作为本卡前置。
**Verification:**
- [x] VER-1：`source .env.test && cargo test -p mega2 --lib every_alias_is_admitted_and_exact_oid_body_is_loaded_once -- --test-threads=1`。
**Dependencies:** `FIX-OX-04`。
**Implementation write set:** src/api/router/snapshot_objects_bounded_tests.rs、docs/plan/plan-20260920.md（本卡证据）；snapshot_content_tests.rs 的共享 lease scaffold 由 FIX-OX-02 唯一拥有。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回 fixture 时长修订并保留失败证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=alias fixture 租约时长; recovery=撤回 fixture 修订并阻止发布; complete=yes; self-contained=yes; AC=2/8; VER=1/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-04; writeset=序列化于 FIX-OX-04; release=REL-OX-01 child; split-from=N/A; exception=N/A`。
