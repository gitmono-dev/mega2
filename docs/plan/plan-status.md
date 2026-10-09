# 计划执行状况总表（plan-status.md）

> **本文件是全仓计划的单一执行状况视图**，按任务卡粒度汇总每一份计划的执行状态，并登记计划内的延后决策/实施项（`DEFER-*`）与跨计划依赖（`DEP-*`）。**任何执行计划的 Agent 在完成/推进一张任务卡时，必须在同一变更中同步更新本文件**；新建计划时必须在「计划一览」登记一行，并把本文件的更新义务写入新计划的「使用规则」或修订历史。
>
> **维护规则（强制）**
>
> 1. 每张卡的状态推进（`pending` → `in-progress` → `blocked` → `done`，`Acceptance` 随 ER-04 转移）都在「计划一览」的对应行更新，并附发布版本 / commit / 时间（`YYYY-MM-DD HH:MM:SS UTC`）。
> 2. 计划收口、拆卡、合并发布、新增 `DEFER-*`、`DEP-*` 状态变化，同步更新「延后与未决策项」与「跨计划依赖」两节。
> 3. 新建计划：在「计划一览」加一行（类别、状态、一句话进度），并在「未启动计划」或「实施中计划」小节落位。
> 4. 以「计划一览」表为权威，其余小节是它的展开视图；冲突时以任务卡自身 `Lifecycle / Acceptance` 与 `plan-long.md` 的日期索引交叉核对。
> 5. 状态快照时间见本文件头，格式为 `YYYY-MM-DD HH:MM:SS UTC`（24 小时制、UTC、精确到秒）。每次更新必须把快照时间改成这次写入时的 UTC 时钟时间，便于多个 Agent 区分先后。已经写下的纯日期记录保持原样，不补写时间。
>
> **当前快照：** 2026-10-09 17:35:18 UTC（计划模板 `v2.4`）。本文件首次建档自 [`docs/plan/README.md`](README.md) 的「文件列表」与各 `plan-*.md` 的「计划状态」行汇总；**结论：31 份日期计划中 30 份已完成/已收口/已落地，唯一仍未实现的是 [`plan-20260920.md`](plan-20260920.md)**。该计划目前处于 M0 Claude Code 复审阶段：R24 Claude Code 对计划 SHA `8109d374ee3420414542bd1d7a09f4302050a5000027c8e242186b76020089c6` 返回 literal `VERDICT: PASS`；审查报告列有 3 项 non-blocking P3。306 张卡仍 pending，M0 本地签名 checkpoint 与 clean clone 验证完成前不开始任务卡。计划登记 306 张卡且全部 `Lifecycle=pending / Acceptance=空`，无卡片已开工。M0 未通过前不开始实施；`DEFER-GS-08` 的两项 live 验收分别由 OX-24 与 OX-06 承接，缺凭证、`not-run` 或 `env-not-set` 均不构成完成。建档时同步修复的 README 数据落差与 `plan-20260911` 完成判据仍保持已关闭。

---

## 一、计划一览

状态列取值：`未启动` / `实施中` / `已收口` / `已排期`（设计计划，尚未执行）。mega2 无 libra 的 issues/ 目录计划体系，全部为日期计划（`plan-YYYYMMDD.md`）；`plan-long.md` 为长期路线图（非日期计划，单独列于表末）。

| 计划 | 类别 | 状态 | 一句话进度（卡片状态） |
|---|---|---|---|
| [`plan-20260727.md`](plan-20260727.md) | 集成测试基建（PT-01 承接） | **已完成** | IT-01..IT-13 与 GM-* 全部收口；承接 PT-01；完成判据 10/10 勾选 |
| [`plan-20260731.md`](plan-20260731.md) | 服务拆退（chat/notes 退场 + website 用户系统） | **已完成** | AU-* / RM-* / ITW-* / MN-* / DOC-01 / REL-01 全部落地；完成判据 12/12 勾选。`DEP-06` 已由 `plan-20260802.md` 关闭 |
| [`plan-20260802.md`](plan-20260802.md) | Website 内部产品邮件 API | **已完成** | WE-01..WE-07；tip `a52d703`；mega2 `0.2.1`。`DEP-06` 关闭；完成判据 10/10 勾选 |
| [`plan-20260803.md`](plan-20260803.md) | Git 使用场景测试补全（PT-04） | **已完成** | GM-01..GM-12 全部终态（GM-04/GM-R1/GM-10A 取消，GM-10B handoff）；GM-12 发布于 v0.2.11，完成度复审收口 v0.2.15；完成判据 7/7 勾选 |
| [`plan-20260812.md`](plan-20260812.md) | 用户体系统一（website 认证 × mega2 授权） | **已完成** | UN-01..UN-60；收口发布 **v0.2.66** / UN-07；REL-01→v0.2.22、REL-02→v0.2.65；UN-05 handoff 与 UN-06 手册已入库；完成判据 10/10 勾选 |
| [`plan-20260820.md`](plan-20260820.md) | RustyVault → libvault crate 迁移 | **已完成（2026-08-21）** | vendored `src/vault/` 删除 → `libvault` 0.3.0；VLT-S1 判定 **go**（shadow-unseal），VLT-S2 / DEFER-VLT-01 未触发；REL-VLT-RO{02→04→FIX-VLT-01→05} 发布 `v0.2.68`；完成判据 11/11 勾选 |
| [`plan-20260824.md`](plan-20260824.md) | Orbit 完全单体内联 | **已完成（2026-08-23）** | ORB-00..09 家族发布 REL-ORB-01 → ORB-09，收口 **v0.3.0** / `85cfdd4`。ORB-09 `complete（附例外）`（D 组 Config Validation 因 monoui 镜像构建外因红）；`DEFER-ORB-01..04`；完成判据 8/8 勾选 |
| [`plan-20260826.md`](plan-20260826.md) | Mega 同步（#2130→#2175） | **已完成（2026-08-26）** | SYNC-01..05 全部 `done/complete`；逐卡 patch bump 收口 **v0.3.5**；review R7 PASS；完成判据 8/8 勾选 |
| [`plan-20260827.md`](plan-20260827.md) | CL 多 commit push 放开 | **已完成（2026-08-29）** | MC-01..11 全部 done；REL-MC-02 经 MC-10 发布 **0.4.0** / `0e9a4e6`，REL-MC-01 经 MC-08 收口 **0.5.2** / `d2f7ee0`；Claude/Codex 每卡双 PASS；完成判据 10/10 勾选 |
| [`plan-20260901.md`](plan-20260901.md) | Mega / Libra FastCDC 与并发可靠性同步 | **已完成** | FastCDC Media family `0.9.0` + FC-08～FC-13 独立发布至 **v0.10.0** / tip `634904b`；LB-01 Libra `d1aafb23`；Codex 卡级 PASS；`DEFER-FC-01～06` 延后 |
| [`plan-20260902.md`](plan-20260902.md) | storage-only OCI Distribution 容器镜像仓库 | **已完成** | DR-01..DR-15；storage-only OCI `/v2`；文档 [`../refactoring/oci.md`](../refactoring/oci.md)；DR-15 tip `070783e` / v0.8.53；完成判据 10/10 勾选 |
| [`plan-20260903.md`](plan-20260903.md) | Monorepo 初始 Object-ID 配置 | **已完成** | HSH-01/HSH-02：`monorepo.object_format` SHA-1/SHA-256 bootstrap；BLAKE3 fail-closed；mega2 可交付部分由 [`plan-20260907.md`](plan-20260907.md) 关闭；完成判据 4/4 勾选 |
| [`plan-20260904.md`](plan-20260904.md) | Storage-only trunk 产品 API 写文件 | **已完成** | AW-01..05；trunk 产品 API 写经 push_auth + MonoWriteQueue；compose 可继承黑盒 `api_write_smoke_storage_only.sh`；收口 **v0.8.61**；完成判据 5/5 勾选 |
| [`plan-20260905.md`](plan-20260905.md) | Trunk 直推形态与 Monorepo 写入序列化 | **已完成（2026-09-09）** | TP-01..TP-23 全部 `done/complete`；`MonoWriteQueue` 全局写入序列化、后代 ref 续接与墓碑、合成 commit 归属与 provenance、`push_policy="trunk"` 直推、静态 token 推送认证；完成判据 10/10 勾选。trunk LFS 限制已由 [`plan-20260909.md`](plan-20260909.md) supersede |
| [`plan-20260906.md`](plan-20260906.md) | storage-only 形态 Git 协议黑盒 smoke | **已完成** | 一 case 一卡；compose 黑盒；SO-01..04 / SO-07..15 / SO-17..25；SO-05 `cancelled`；收口 SO-06；完成判据 5/5 勾选 |
| [`plan-20260907.md`](plan-20260907.md) | git-internal 0.9.0 / BLAKE3 采纳 | **已完成** | B3-01..B3-06：git-internal 0.9.0、显式 HashKind、Git/LFS 独立 hash domain、blake3 bootstrap + Libra/git-internal normal service；收口 **v0.8.67** / `144b624`；`DEFER-B3-01..03`、`DEFER-B3-LFS-01` 延后 |
| [`plan-20260908.md`](plan-20260908.md) | storage-only SSH 只读对齐 HTTP | **已完成** | SP-01..SP-05；SSH upload-pack 对齐 HTTP 读；`auth_none`/password-token；SP-04 `cancelled`→SP-01；`DEFER-SP-01..04`；Codex R18 PASS；完成判据 9/9 勾选 |
| [`plan-20260909.md`](plan-20260909.md) | storage-only LFS（`push_auth=none` / `token`） | **已完成** | LF-01..LF-04；Claude+Codex 每卡双 PASS；supersede `plan-20260905`「trunk LFS 不可用」；完成判据 10/10 勾选 |
| [`plan-20260910.md`](plan-20260910.md) | CL merge 强制走 MonoWriteQueue 并删 legacy 写者 | **已完成** | MW-01..MW-06；删除 `merge_writer` / Legacy processor / `merge_queue` 表；无存量迁移；完成判据 10/10 勾选 |
| [`plan-20260911.md`](plan-20260911.md) | storage-only Agent Capture 落库 | **已完成** | AC-00..AC-22（含 AC-04-GC / AC-08-R）：`[agent_capture]` 配置门、`agent_capture_*` 表、`ObjectNamespace::Agent`、独立 ingest token、`/api/v1/agent-capture` raw ingest/查询；libra 客户端 DEFER。**2026-10-08 一致性修复**：「完成判据」10 项与各卡 AC/Verification 子项共 300 项已全部勾选（`[x]`） |
| [`plan-20260912.md`](plan-20260912.md) | storage-only 提交后出站 webhook | **已收口** | WH-01..WH-15 全部 `done/complete`（WH-01..13 → v0.10.38、WH-14 → v0.10.39、WH-15 → v0.10.40）；六类来源 hook 全部落地；`DEFER-WH-01..04`、`DEFER-WH-05` 已关闭 |
| [`plan-20260913.md`](plan-20260913.md) | mega2 FastCDC Media 效果对齐 | **已完成** | MF-00..MF-08 全部 `done/complete`（MF-06 发布 `v0.40.10`；MF-05 真 interop 已发布 `v0.40.14` / `5bc365a`）；用户授权跳过双 review；`DEFER-MF-01` 延后。README「文件列表」本行已同步为已完成 |
| [`plan-20260916.md`](plan-20260916.md) | monorepo 路径 → GitHub 单向同步基础设施 | **已完成（2026-09-20）** | GS-01..GS-28（24 张活动卡）全部 `done/complete`；五张 spike 全 go；执行链路移交 [`plan-20260920.md`](plan-20260920.md)；`DEP-02` 已关闭 |
| [`plan-20260917.md`](plan-20260917.md) | storage-only 目录变更与标签 HTTP 补全 | **已完成** | LB-01..LB-07 全部 `done/complete`（LB-02→v0.10.41、LB-03→v0.10.42、LB-04→v0.10.43、LB-05→v0.10.44）；`DEFER-LB-01..11` 承接情况见计划正文 |
| [`plan-20260918.md`](plan-20260918.md) | 文件删移与 Tag 契约跟进 | **已完成** | FT-01..FT-09 全部 `done/complete`（FT-02/03→v0.10.45/v0.10.46，FT-04→v0.11.0，FT-05→v0.11.1，FT-06→v0.11.2，FT-07→v0.11.3）；关闭 60917 `DEFER-LB-01/02/03` 与 `DEFER-LB-11` 隔离面 |
| [`plan-20260919.md`](plan-20260919.md) | mega2 通知出站与协作 UI 拆除 | **已收口** | RM-01..RM-04 全部落地；crate 停在 `0.38.1`；跳过 Codex/Claude 双评（配额耗尽）；README「文件列表」本行已补登 |
| [`plan-20260920.md`](plan-20260920.md) | monorepo 路径 → GitHub 出站同步执行 | **M0 计划审查中，未开工** | 306 张卡全部 `pending / Acceptance=空`；R24 Claude Code 对计划 SHA `8109d374ee34…020089c6` 返回 literal `PASS`，报告记录 3 项 non-blocking P3。全部 306 张卡仍为 `pending / Acceptance=空`；先完成包含全部 WIP 与 review evidence 的本地签名 checkpoint、签名核验及 clean clone，再开始 FIX-OX-08。PASS 后先做全部 WIP checkpoint 与干净副本核验；仅 OX-284 在计划末尾 bump 一个 patch 并发布。**本仓唯一未实现的日期计划**
| [`plan-20260921.md`](plan-20260921.md) | Artifacts API 挂载 storage-only | **已落地** | AR-01（storage-only 挂载 + 写鉴权门控）、AR-02（进程级黑盒）、AR-03（README 收口 + 门禁）均已实现；fmt/clippy/test 全绿。README「文件列表」本行已补登 |
| [`plan-20260923.md`](plan-20260923.md) | 首次使用路径策略与 ImportRepo 生命周期修复 | **已完成** | FU-01..FU-23 与 FU-04A 全部 `done/complete`（v0.38.22..v0.40.12）；收口发布 **v0.40.12** / `4bf6cc2`；「计划完成门」v0.40.13 补跑通过（macOS `ld` 链接提示视为平台噪音）；完成判据 10/10 勾选 |
| [`plan-20261001.md`](plan-20261001.md) | storage-only 形态 OCI / Artifacts / Libra 客户端黑盒 smoke | **已完成** | BB-01..BB-92（77 张非延后任务卡）全部 `done/complete`；默认栈 OCI 12/12、Artifacts 14/14、Libra 24/24 逐案 PASS；`interop-smoke`、共享 runner、opt-in helper 与产品修复已交付；`DEFER-DR-06` 已关闭；最后发布 `v0.41.72`，Docker job `111617454080` success；完成判据 11/11 勾选 |
| [`plan-20261002.md`](plan-20261002.md) | 历史投影视图 P0（只读投影） | **已完成（2026-10-08）** | HP-01…HP-25、HP-27…HP-35 与 FIX-HP-01（36/36 卡）实现、本地验收、复审及 34 个版本提交/tag/人工 GitHub Release 已完成；HP-26 计划收口 `done/complete`；发布 v0.41.73..v0.42.25；六张卡历史 Docker D 组绿灯保留，其余按 `EX-HP-01` 延期；P1/P2 留在 PT-14 |
| [`plan-long.md`](plan-long.md) | 长期能力（Mega → mega2 完全移植路线图） | **当前（进行中）** | 长期路线图，非日期计划；自 PT-13 起可登记 mega2 原生能力；跨计划索引为本文件的展开依据 |

---

## 二、未启动的计划与卡

唯一列入本节的计划为 `plan-20260920`（本仓唯一未实现的日期计划）。其余 30 份日期计划均已达终态。

| 计划 | 全部待执行卡 | 开工前置条件 |
|---|---|---|
| [`plan-20260920.md`](plan-20260920.md) | 306 张卡（完整依赖顺序见计划「实施顺序」；均 `pending`） | `DEP-OX-01` 已满足，`DEP-01` 已交接；M0 开工门尚未通过。R24 对 plan SHA `8109d374ee34…020089c6` 返回 literal PASS；三项 non-blocking P3 见报告。全部当前未提交文件与 review evidence 将作为本地 checkpoint 提交，不 push；随后先验证 Signed-off-by/gpgsig 并完成 clean clone，才开始 FIX-OX-08。live OX-24/OX-06 各自必须实测；OX-284 是唯一末尾 patch 发布点

---

## 三、实施中的计划、待退役的历史方案与当前卡

mega2 无仍在实施中的日期计划；已实施/收口的计划卡状态见「计划一览」对应行的详细版本/commit 证据。下表列出已收口但留有明确 `DEFER-*` 或数据落差的计划卡，供后续核对。

### 3.1 plan-20260913（FastCDC Media 效果对齐）— 已完成

MF-00..MF-08 全部 `done/complete`，`DEFER-MF-01` 与 Libra `DEP-FL-*` 跨仓依赖状态见「延后与未决策项」与「跨计划依赖」两节。**README「文件列表」本行已由本文件建档时同步为已完成。**

### 3.2 plan-20260911（Agent Capture 落库）— 一致性问题已修复

AC-00..AC-22 等全部 `Lifecycle=done / Acceptance=complete`（每卡字段逐一核对）。此前 `## 完成判据` 的 10 个复选框与各卡 AC/Verification 子项均为 `[ ]`；2026-10-08 已依据仓内实现证据（`src/`、`tests/integration_agent_capture.rs`、迁移 `m20260913_000100`、`docs/refactoring/agent-capture.md`）、Review log R16 Codex `PASS` 与版本面 bump（基线 `0.10.1` → 当前 `0.42.25`）完成勾选，使「任务卡状态、完成判据、本文件」三者一致。

### 3.3 plan-20260919 / plan-20260921 — 已补登进 README

两份计划均已收口/落地，本文件建档时已补登 README「文件列表」两行，并关闭此项落差。

---

## 四、当前执行指针（next action）

- **本仓当前唯一未开工日期计划：** [`plan-20260920.md`](plan-20260920.md)。`DEP-OX-01` 已满足，但 M0 计划门尚未通过；R24 Claude Code 对计划 SHA `8109d374ee3420414542bd1d7a09f4302050a5000027c8e242186b76020089c6` 返回 literal PASS；3 项 non-blocking P3 见报告。M0 checkpoint 含全部当前未提交文件与 review evidence，将只本地提交、不 push；签名核验和 clean clone 完成后才开始 FIX-OX-08。当前 306 张卡均 pending。
- **已收口但残留延后项的计划：** `plan-20260913`（`DEFER-MF-01`）、`plan-20260912`（`DEFER-WH-01..04`）、`plan-20260824`（`DEFER-ORB-01..04`）、`plan-20260902`（`DEFER-DR-01..06`，其中 `DEFER-DR-06` 已由 plan-20261001 关闭）、`plan-20260907`（`DEFER-B3-01..03`、`DEFER-B3-LFS-01`）、`plan-20260916`（`DEFER-GS-01..09`）等，见「五、延后决策或实施项」。
- **待修复数据落差：** 无（`plan-20260911` 完成判据与各卡子项已勾选；`plan-20260913` 与 `plan-20260919` / `plan-20260921` 的 README 落差已在本文件建档时同步修复）。

---

## 四·零、数据落差（建档时发现的三处，均已修复）

| 计划 | 落差 | 建议动作 |
|---|---|---|
| （已修复）[`plan-20260911.md`](plan-20260911.md) | 历史落差：任务卡全部 `done/complete`，但「完成判据」10 项与各卡 AC/Verification 子项均未勾选 | 2026-10-08 已全部勾选（`[x]`，共 218 处行内子项 + 10 项完成判据 + 8 项文档收口）；`cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings` 绿；`m20260913_000100_add_agent_capture_tables` 6/6、`integration_agent_capture` 8/8 绿；实现、迁移、集成与文档证据均核实在仓 |
| （已修复）[`plan-20260913.md`](plan-20260913.md) | 历史落差：README「文件列表」原标「新建（设计稿，0 实现）/ MF-00..08 pending」，实际已全部 `done/complete` | 本文件建档时已将 README 行更新为已完成 |
| （已修复）[`plan-20260919.md`](plan-20260919.md) / [`plan-20260921.md`](plan-20260921.md) | 历史落差：未登记进 README「文件列表」表 | 本文件建档时已补登两行 |

> **全部关闭（2026-10-08）：** 上述三处数据落差均已修复，本文件无待修复项。后续任一 Agent 修改计划状态时须再次核对「任务卡状态、完成判据、本文件」三者一致。

---

## 五、延后决策或实施项（DEFER-* 汇总）

按计划分组列出仍在延后的项（关闭项不列出）。状态：`延后` / `已关闭` / `由其它计划承接`。

### 5.1 已收口计划的 DEFER-* 残留

| 计划 | DEFER ID | 内容 | 状态 |
|---|---|---|---|
| plan-20260824 | DEFER-ORB-01..04 | orbit 内联范围外项（monoui 镜像外因、配置等） | 延后 |
| plan-20260902 | DEFER-DR-01..06 | OCI 范围外/presigned 直传确认等（`DEFER-DR-06` 已由 plan-20261001 关闭） | 延后（DEFER-DR-06 已关闭） |
| plan-20260907 | DEFER-B3-01..03、DEFER-B3-LFS-01 | BLAKE3 wire/pack/runtime、LFS BLAKE3 业务面 | 延后 |
| plan-20260912 | DEFER-WH-01..04、DEFER-WH-05 | presigned 直传确认、media 静态 token 认证等（DEFER-WH-05 已关闭） | 延后（DEFER-WH-05 已关闭） |
| plan-20260916 | DEFER-GS-01..09 | 出站同步范围外项（入站 `DEFER-GS-02`、真实 live `DEFER-GS-08` 等） | 延后（部分由 plan-20260920 承接） |
| plan-20260917 | DEFER-LB-01..11 | 目录/标签 HTTP 范围外项 | 部分由 plan-20260918 承接 |
| plan-20260918 | DEFER-FT-01、DEFER-FT-02 | 文件删移范围外项 | 延后 |
| plan-20260923 | DEFER-FU-01..50 | ImportRepo 范围外项 | 延后 |
| plan-20261002 | DEFER-HP-01..21 | 历史投影 P1/P2/P3（shallow、tag 投影、REST 读接口、`mega2 view`、回收、sha256/blake3 等） | 延后（P1/P2 留在 PT-14） |
| plan-20261001 | DEFER-BB-01..12 | 黑盒范围外项（Docker-in-Docker 等） | 延后 |

### 5.2 具体承接与在途 DEFER（重要项）

- `DEFER-GS-08`（真实 GitHub live）：已纳入 `plan-20260920` 的 OX-24（独立新仓首推）与 OX-06（新仓同 run 首推+增推）两项 live 门；任一卡仅在正向协议结果、人工清理、原 case 复核及证据归档完成后关闭。缺凭证或 `not-run` / `env-not-set` 不得记完成。
- `DEFER-GS-02`（入站同步）：独立日期计划。
- `DEFER-MF-01`（plan-20260913）：FastCDC interop 范围外项，受 Libra `DEP-FL-*` 约束。
- `DEFER-TP-05` / `DEFER-TP-01..05`（plan-20260905）：trunk 直推范围外项。

---

## 六、跨计划依赖（DEP-* 现行生效项）

### 6.1 在途 / 生效的跨计划依赖

| DEP-ID | 类型 | 内容 | 现状 |
|---|---|---|---|
| DEP-OX-01 | 跨计划前置（incoming） | `plan-20260920` 依赖 `plan-20260916` GS-10 `Acceptance=complete`（五章节落盘） | 已满足（60916 已收口）；`plan-20260920` 尚未开工 |
| DEP-OX-02 | 环境前置（incoming） | `plan-20260920` OX-24/OX-06 的 runner token、namespace kind/name、`MEGA_TEST_GITHUB_EXPECTED_OPERATOR_LOGIN` 与具名操作员独立权限 | `.env.test` 变量存在性曾被核验，权限与当日可用性仍须 live 卡开工前复核；值不进入本台账 | 缺凭证只阻塞相应 live 卡，不阻塞无关本地卡；`env-not-set` 不能通过 live 或计划完成门 |
| DEP-01（60916→60920） | 跨计划移交（outgoing） | 60916 交付出站同步基础设施与五域冻结 | 已交接；60920 整体承接 |
| DEP-02 | 跨计划移交 | 修改 `docs/plan/plan-template.md`（GC-12 / ER-07 改回 Libra） | 已关闭（60916 收口时） |
| DEP-MF-01（60913） | 跨计划前置 | mega2 FastCDC 真 interop 依赖 Libra FL-04 | 已满足（两条真实 interop 已执行通过，pin `8c870c4` / VER 2/2） |
| DEP-BB-*（61001） | 跨计划前置/移交 | OCI/Artifacts/Libra 黑盒与 `mega2 browser`（Libra 侧） | 已满足 |
| DEP-HP-05（61002） | 跨计划前置 | 历史投影 L0 对象保真修复移交 | 已交付（`../libra-backend`）；`DEFER-HP-20` 承接后续核对 |

### 6.2 历史跨计划依赖（已完成关闭，不再阻塞）

- `DEP-06`（plan-20260731 → plan-20260802）：Website 邮件投递移交，已关闭。
- `DEP-04`（plan-20260731 → PT-12）：网站用户系统移交，已入库。
- `DEP-02`（plan-20260916）：模板 GC-12 / ER-07 改回 Libra，已关闭。
- `DEP-ORB-*`、`DEP-DR-*`、`DEP-B3-*`、`DEP-FC-*`、`DEP-MW-*`、`DEP-SP-*`、`DEP-LF-*` 等均属对应计划内部/收口前依赖，已随各计划收口满足。

---

## 七、完成计划清单（已收口）

日期计划：`plan-20260727`、`plan-20260731`、`plan-20260802`、`plan-20260803`、`plan-20260812`、`plan-20260820`、`plan-20260824`、`plan-20260826`、`plan-20260827`、`plan-20260901`、`plan-20260902`、`plan-20260903`、`plan-20260904`、`plan-20260905`、`plan-20260906`、`plan-20260907`、`plan-20260908`、`plan-20260909`、`plan-20260910`、`plan-20260911`、`plan-20260912`、`plan-20260913`、`plan-20260916`、`plan-20260917`、`plan-20260918`、`plan-20260919`、`plan-20260921`、`plan-20260923`、`plan-20261001`、`plan-20261002`。

> 说明：本清单含全部已达终态的日期计划。唯一未完成者为 [`plan-20260920.md`](plan-20260920.md)：306 张卡目前全部 `pending`，M0 尚待 Claude Code 对新冻结 SHA 返回 literal `VERDICT: PASS`；`DEFER-GS-08` 由 OX-24 与 OX-06 两项 live 卡承接。`plan-20260913`、`plan-20260919`、`plan-20260921` 于本文件建档时已补登/同步进 README「文件列表」。
