### Task FIX-OX-12：raw-stream 慢速 fixture 租约（implementation）

**Task type:** `implementation`
**Lifecycle / Acceptance:** `in-progress` / 空
**Description:** 修复两个需完整读取真实 raw body 的 rooted fixture 使用默认 600s lease、导致慢速串行验证在 alias/腐坏断言前过期的问题；只延长测试 fixture，不改变生产路由、租约策略或负向断言。
**Current evidence:** 从提交 `1b9a2547b53a6c8e7a567ed0694321254c126810` 运行当前卡 A/B。A 源 SHA-256 `7def8e48e8d631bffd1466b55b9e64f44dcc00d43e64109588984db690f10f66`；VER-1 exit=101 / 617.14s，在完整 cold raw body 断言前 `LeaseExpired`；VER-2 exit=101 / 616.31s，在 missing/forged receipt 断言前 `LeaseExpired`。B 仅将两个测试的 rooted fixture 改为 `lease_seconds: Some(3600)`，源 SHA-256 `600d9ae359dd7315fc68ae671c9ff460ac84d3e6798a12e89f7bafd3e1711072`，A/B diff SHA-256 `5bf08a694c082c1300bae2048fc5be36319469002026c245da540438ed937fcb`。B VER-1 exit=0 / 884.03s（1 passed）；B VER-2 exit=0 / 920.25s（1 passed），完整覆盖 forged/missing receipt、实际存储损坏及修复后 raw body。`cargo +nightly fmt --all --check` exit=0。逐次命令、退出码、耗时和日志 SHA-256 见 `docs/plan/evidence/plan-20260920/cards/FIX-OX-12/`；B-VER-1 compiler stderr checkout 路径按 ER-11 脱敏。此证据仅为卡级 VER，不是 REL-OX-01 最终树全量 C。
**Acceptance criteria:**
- [x] AC-1：上述两个用例均在 rooted resolve 前显式采用 3600s lease，且保留完整 raw body 读取路径。
- [x] AC-2：cold raw 仍验证全量字节、digest/headers、source read 计数及 alias receipt reuse；forged/corrupt raw 仍验证 502、无重建与原有 body read 断言。
**Verification:**
- [x] VER-1：`source .env.test && cargo test -p mega2 --lib cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers -- --test-threads=1`。
- [x] VER-2：`source .env.test && cargo test -p mega2 --lib raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding -- --test-threads=1`。
**Dependencies:** `FIX-OX-05`。
**Implementation write set:** src/api/router/snapshot_raw_blob_tests.rs、docs/plan/plan-20260920.md（本卡证据）；FixtureOptions.lease_seconds 已由 FIX-OX-02 的共享 scaffold 提供，本卡仅消费。
**Release write set:** `N/A`（REL-OX-01 plan release child；版本面只由 OX-284 修改）
**Rollback mode:** `revert`（撤回 fixture 时长修订，保留 lease-expiry 失败证据）。
**Estimated scope:** `S`
**Version increment:** `N/A`
**Release boundary:** `plan release child of REL-OX-01`（本地 A/B、ER-05 PASS、精确本地提交；OX-284 前不 bump/push/tag/Release）
**C/D coverage from:** `OX-284；继承 D-OX-TAG`。
**Granularity:** `type=implementation; axis=raw-stream fixture 租约时长; recovery=撤回 fixture 修订并阻止发布; complete=yes; self-contained=yes; AC=2/8; VER=2/8; landing=0; prod-files=0; scope=S; deps=FIX-OX-05; writeset=序列化于 FIX-OX-05; release=REL-OX-01 child; split-from=N/A; exception=N/A`。
