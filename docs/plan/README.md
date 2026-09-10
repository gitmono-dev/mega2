# 计划（Plan）

本目录存放 monoengine 的开发计划文档。所有新计划必须使用本目录下的 `plan-template.md` 模板，不得自创格式。

## 规则

1. **强制使用模板。** 新建任何计划必须从 `plan-template.md` 复制，替换 `<...>` 占位符，删除不适用的说明性文字。强制章节不得删除，不适用时写 `N/A` 并说明原因。
2. **命名约定。**
   - 日期计划：`plan-YYYYMMDD.md`（用于可执行的实现、迁移、重构或发布任务）。
   - 长期能力：`plan-long.md`（唯一一份，条目化管理长期路线图）。
3. **事实基线优先。** 每个计划必须以当前 checkout 的源码、测试、配置和文档为事实基线，历史计划或对 Mega 目标项目的历史描述只能作为线索。
4. **任务可执行。** 每个任务卡必须能交给 Agent 独立执行：范围明确、依赖明确、文件落点明确、验收标准明确、验证命令明确。
5. **三门验收。** 每个任务至少通过 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test ...` 指定用例。
6. **计划不等于实现。** 文档只规划任务，不宣称实现完成。落地时每个任务都必须先刷新源码锚点，再按任务卡验收。

## 文件列表

| 文件 | 类型 | 状态 |
|---|---|---|
| `plan-template.md` | 模板 | 基线 |
| `plan-long.md` | 长期能力 | 当前（2026-07-27 首版，Mega → monoengine 完全移植路线图） |
| `plan-20260727.md` | 日期计划 | 已完成（承接 PT-01 集成测试基建） |
| `plan-20260731.md` | 日期计划 | 已完成（Website 用户系统接入 + chat/notes 整栈退场 + next-app compose 同栈 IT + 本仓邮件退场改接 website；AU-* / RM-* / ITW-* / MN-* / DOC-01 / REL-01）。**DEP-06 已由 `plan-20260802.md` 关闭** |
| `plan-20260802.md` | 日期计划 | 已完成（Website 内部产品邮件 API；tip `a52d703`；DEP-06 关闭；monoengine `0.2.1`） |
| `plan-20260803.md` | 日期计划 | 已完成（Git 使用场景测试补全；承接 PT-04 / DEFER-IT-01/02/12；GM-01..GM-12 全部终态，GM-04/GM-R1/GM-10A 正式取消、GM-10B handoff 承接；GM-12 发布于 v0.2.11，完成度复审收口于 v0.2.15） |
| `plan-20260812.md` | 日期计划 | **已完成**（用户体系统一：website 认证 × monoengine 授权；收口发布 **v0.2.66** / UN-07；REL-01→v0.2.22、REL-02→v0.2.65；UN-05 handoff 与 UN-06 手册已入库） |
| `plan-20260820.md` | 日期计划 | **已完成（2026-08-21）**（vendored `src/vault/` 删除 → `libvault` 0.3.0；VLT-S1 判定 **go**（shadow-unseal），VLT-S2 / DEFER-VLT-01 未触发；REL-VLT-RO{02→04→FIX-VLT-01→05} 发布 `v0.2.68`；UN-31 八 AC 在集成层等价重建） |
| `plan-20260824.md` | 日期计划 | **已执行**（orbit 完全单体内联：`src/orbit_api` + `src/orbit`，移除 `ObjectStorageProvider`，单 package `monoengine`；家族发布 REL-ORB-01 → ORB-09） |
| `plan-20260826.md` | 日期计划 | Mega 同步 #2130→#2175（SYNC-01..05；状态以计划内任务卡为准） |
| `plan-20260827.md` | 日期计划 | **新建**（CL 多 commit push 放开：push 侧链式校验 + 拒绝 merge commit + 修 ref 配对，trunk 侧 CL merge 永远单父新 commit 不动；MC-01..04；设计经 Codex/Claude 双评审有条件批准；下游 = monoui `docs/plan/plan-20260827.md`） |
| `plan-20260901.md` | 日期计划 | **Claude Code Review PASS**（Mega `bb3ef17` FastCDC media transport，以及 receive-pack 状态安全、导入批处理、DB/Redis 并发和多实例 ID 改进；FC-01..14；R4 通过） |
| `plan-20260903.md` | 日期计划 | 进行中：Monorepo 初始 object-ID 配置（SHA-1/SHA-256 bootstrap；BLAKE3 接口保留且 fail-closed）。端到端 SHA-256 / BLAKE3 Git 协议支持不在本切片。 |
| `plan-20260905.md` | 日期计划 | **已完成**（Trunk 直推 / `MonoWriteQueue`：TP-01..TP-23。设计事实源 = [`../refactoring/trunk-push.md`](../refactoring/trunk-push.md)。文档概览条目在 [`../refactoring/README.md`](../refactoring/README.md) **§5a**，不是仓库根 `README.md`） |
| `plan-20260906.md` | 日期计划 | **新建**（一 case 一卡；compose 黑盒；测试客户端=git；ADR-SO-07..10；Codex R7 PASS；实现未开工） |
| `plan-20260909.md` | 日期计划 | **已完成**（LF-01..LF-04；Claude+Codex 每卡双 PASS；Storage-only LFS：`push_auth=none` / `token`；supersede plan-20260905「trunk LFS 不可用」） |
| `plan-20260908.md` | 日期计划 | **进行中**（SP-01..SP-05；SP-01/SP-02 已落地并 push；SP-04 cancelled→SP-01；SP-03 本卡索引；SP-05 待终态 release；Codex R18 PASS；每实现/release 卡 patch+1） |
| `plan-20260910.md` | 日期计划 | **已完成**（MW-01..MW-06；CL merge 强制 MonoWriteQueue；删除 `merge_writer` / Legacy processor / `merge_queue` 表；无存量迁移） |
