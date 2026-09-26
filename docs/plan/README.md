# 计划（Plan）

本目录存放 mega2 的开发计划文档。所有新计划必须使用本目录下的 `plan-template.md`（中文规范原文）或 `plan-template.en.md`（English contributor edition）模板，不得自创格式。两份结构相同；门禁或字段冲突时以中文原文为准，并开 Issue 同步英文副本。

## 规则

1. **强制使用模板。** 新建任何计划必须从 `plan-template.md` 复制，替换 `<...>` 占位符，删除不适用的说明性文字。强制章节不得删除，不适用时写 `N/A` 并说明原因。
2. **命名约定。**
   - 日期计划：`plan-YYYYMMDD.md`（用于可执行的实现、迁移、重构或发布任务）。
   - 长期能力：`plan-long.md`（唯一一份，条目化管理长期路线图）。
3. **事实基线优先。** 每个计划必须以当前 checkout 的源码、测试、配置和文档为事实基线，历史计划或对 Mega 目标项目的历史描述只能作为线索。
4. **任务可执行。** 每个任务卡必须能交给 Agent 独立执行：范围明确、依赖明确、文件落点明确、验收标准明确、验证命令明确。
5. **三门验收。** 每个任务至少通过 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test ...` 指定用例。
6. **计划不等于实现。** 文档只规划任务，不宣称实现完成。落地时每个任务都必须先刷新源码锚点，再按任务卡验收。
7. **索引与状态必须同步。** 新建计划成稿即登记进下方「文件列表」；收口时同步三处并保持一致：① 计划内每张卡的 `Lifecycle / Acceptance`、② 计划的「完成判据」勾选、③ 本表状态列。三者不一致就是待修复的数据落差（历史实例见 `plan-20260824.md` 2026-09-15 回填记录）。

## 文件列表

| 文件 | 类型 | 状态 |
|---|---|---|
| `plan-template.md` | 模板（中文规范原文） | 基线 |
| `plan-template.en.md` | 模板（English contributor edition） | 与中文原文同结构；冲突以中文为准 |
| `plan-long.md` | 长期能力 | 当前（2026-07-27 首版 Mega 移植路线图；2026-09-16 起含 mega2 原生 **PT-13** 统一推送密文） |
| `plan-20260727.md` | 日期计划 | 已完成（承接 PT-01 集成测试基建） |
| `plan-20260731.md` | 日期计划 | 已完成（Website 用户系统接入 + chat/notes 整栈退场 + next-app compose 同栈 IT + 本仓邮件退场改接 website；AU-* / RM-* / ITW-* / MN-* / DOC-01 / REL-01）。**DEP-06 已由 `plan-20260802.md` 关闭** |
| `plan-20260802.md` | 日期计划 | 已完成（Website 内部产品邮件 API；tip `a52d703`；DEP-06 关闭；mega2 `0.2.1`） |
| `plan-20260803.md` | 日期计划 | 已完成（Git 使用场景测试补全；承接 PT-04 / DEFER-IT-01/02/12；GM-01..GM-12 全部终态，GM-04/GM-R1/GM-10A 正式取消、GM-10B handoff 承接；GM-12 发布于 v0.2.11，完成度复审收口于 v0.2.15） |
| `plan-20260812.md` | 日期计划 | **已完成**（用户体系统一：website 认证 × mega2 授权；收口发布 **v0.2.66** / UN-07；REL-01→v0.2.22、REL-02→v0.2.65；UN-05 handoff 与 UN-06 手册已入库） |
| `plan-20260820.md` | 日期计划 | **已完成（2026-08-21）**（vendored `src/vault/` 删除 → `libvault` 0.3.0；VLT-S1 判定 **go**（shadow-unseal），VLT-S2 / DEFER-VLT-01 未触发；REL-VLT-RO{02→04→FIX-VLT-01→05} 发布 `v0.2.68`；UN-31 八 AC 在集成层等价重建） |
| `plan-20260824.md` | 日期计划 | **已完成（2026-08-23）**（orbit 完全单体内联：`src/orbit_api` + `src/orbit`，移除 `ObjectStorageProvider`，单 package `mega2`；ORB-00..09 家族发布 REL-ORB-01 → ORB-09，收口 **v0.3.0** / `85cfdd4`（+ `0e07d3d`、`ca2b9b3`）。ORB-09 为 `complete（附例外）`：D 组 Config Validation 因 monoui 镜像构建外因红。`DEFER-ORB-01..04`；任务卡状态于 2026-09-15 回填） |
| `plan-20260826.md` | 日期计划 | **已完成（2026-08-26）**（Mega 同步 #2130→#2175；SYNC-01..05 全部 done/complete；逐卡 patch bump 收口 **v0.3.5**；review R7 PASS） |
| `plan-20260827.md` | 日期计划 | **已完成（2026-08-29）**（CL 多 commit push 放开：push 侧链式校验 + 拒绝 merge commit + 修 ref 配对，trunk 侧 CL merge 永远单父新 commit 不动；MC-01..11 全部 done，REL-MC-02 经 MC-10 发布 **0.4.0** / `0e9a4e6`，REL-MC-01 经 MC-08 收口 **0.5.2** / `d2f7ee0`；Claude/Codex 每卡双 PASS；下游 = monoui `docs/plan/plan-20260827.md`。任务卡勾选与计划级清单于 2026-09-15 回填，含三处后续 supersede 注记：MC-02 门控接线点随 `plan-20260910` 迁移、MC-06 AC⑧ 被 `plan-20260901` FC-08 改为 mixed report-status、版本面 parity 因 `plan-20260824` 单 package 化退化） |
| `plan-20260901.md` | 日期计划 | **已完成**（FastCDC Media family `0.9.0` + FC-08～FC-13 独立发布至 **v0.10.0** / tip `634904b`；LB-01 Libra `d1aafb23`；Codex 卡级 PASS。DEFER-FC-01～06 仍延后） |
| `plan-20260903.md` | 日期计划 | **已完成**（HSH-01/HSH-02：`monorepo.object_format` SHA-1/SHA-256 bootstrap；BLAKE3 fail-closed；DEFER wire/pack/BLAKE3 runtime。mega2 可交付部分由 [`plan-20260907.md`](plan-20260907.md) 关闭） |
| `plan-20260907.md` | 日期计划 | **已完成**（B3-01..B3-06：git-internal 0.9.0、显式 HashKind、Git/LFS 独立 hash domain、blake3 bootstrap + Libra/git-internal normal service；收口 **v0.8.67**。不宣称标准 Git BLAKE3 互通；LFS BLAKE3 业务面 `DEFER-B3-LFS-01`） |
| `plan-20260904.md` | 日期计划 | **已完成**（AW-01..05；trunk 产品 API 写经 push_auth + MonoWriteQueue；compose 可继承黑盒 `api_write_smoke_storage_only.sh`；收口 **v0.8.61** / tip 以 AW-05 提交为准；Codex 卡级 PASS） |
| `plan-20260905.md` | 日期计划 | **已完成（2026-09-09）**（TP-01..TP-23 全部 done/complete；`MonoWriteQueue` 全局写入序列化、后代 ref 续接与墓碑、合成 commit 归属与 provenance、`push_policy="trunk"` 直推、静态 token 推送认证；事实源 `../refactoring/trunk-push.md`；review R6–R29 PASS；TP-21 文档收口；`DEFER-TP-01..05`。trunk LFS 限制已由 [`plan-20260909.md`](plan-20260909.md) supersede） |
| `plan-20260906.md` | 日期计划 | **已完成**（一 case 一卡；compose 黑盒；测试客户端=git；SO-01..SO-04 / SO-07..15 / SO-17..25；SO-05 cancelled；收口 SO-06） |
| `plan-20260909.md` | 日期计划 | **已完成**（LF-01..LF-04；Claude+Codex 每卡双 PASS；Storage-only LFS：`push_auth=none` / `token`；supersede plan-20260905「trunk LFS 不可用」） |
| `plan-20260908.md` | 日期计划 | **已完成**（SP-01..SP-05；SSH upload-pack 对齐 HTTP 读；`auth_none`/password-token；SP-04 cancelled→SP-01；DEFER-SP-01..04；Codex R18 PASS） |
| `plan-20260910.md` | 日期计划 | **已完成**（MW-01..MW-06；CL merge 强制 MonoWriteQueue；删除 `merge_writer` / Legacy processor / `merge_queue` 表；无存量迁移） |
| `plan-20260902.md` | 日期计划 | **已完成**（DR-01..DR-15；storage-only OCI `/v2`；文档 [`../refactoring/oci.md`](../refactoring/oci.md)；DR-15 tip `070783e` / v0.8.53；完成判据文档收口见 tip） |
| [`plan-20260911.md`](plan-20260911.md) | 日期计划 | **已完成**（storage-only Agent Capture：`[agent_capture]` 配置门、`agent_capture_*` 表、`ObjectNamespace::Agent`、独立 ingest token、`/api/v1/agent-capture` raw ingest/查询；review 形态不挂载；契约正文 [`../refactoring/agent-capture.md`](../refactoring/agent-capture.md)；进程 IT `integration_agent_capture`。libra 客户端 DEFER） |
| [`plan-20260912.md`](plan-20260912.md) | 日期计划 | **已完成**（storage-only 提交后出站 webhook：运维静态配置、committed-write emitter，有界、仅元数据、带 HMAC。**WH-01..WH-15 全部 done/complete**：`[storage_events]` 配置表面、HTTPS HMAC 运输、DNS 地址钉住、事件投影与静态过滤、有界 emitter 运行时、CLI/service 清理尾段、启动 secret 绑定，以及六类来源 hook（`repo.push` / `oci.manifest.published` / `lfs.object.uploaded` / `lfs.media.finalized` / `agent_capture.events.committed` / `agent_capture.checkpoint.committed`）。WH-07/WH-08 经 `DEP-WH-01` 依赖 [`plan-20260911.md`](plan-20260911.md) 的 AC-15，已满足。**已收口**：2026-09-16 完成度复核追加的 `WH-14`（`shutdown_grace_seconds` 范围门与 `oci_repositories` 规范名门，v0.10.39）与 `WH-15`（`installation_id` 记录字段与 drop 记账，v0.10.40）均已于 2026-09-17 发布并取得 `docker` job 绿灯，契约正文 [`../refactoring/storage-events.md`](../refactoring/storage-events.md)。永久排除 webhook CRUD / 入站 webhook / outbox / 持久 delivery 状态 / retry worker；`DEFER-WH-01`（presigned 直传确认）与 `DEFER-WH-02`（media 静态 token 认证）仍延后） |
| [`plan-20260913.md`](plan-20260913.md) | 日期计划 | **新建（设计稿，0 实现）**（FastCDC Media 效果对齐：细粒度 CDC、历史布局、缺块上传、固定 manifest 读取、无容量硬上限的分页协议；MF-00..MF-08 全部 `pending`）。与 `../../../libra` 的 `plan-20260913.md` 为**对偶双仓计划**，共享表与 C-01..C-08 逐字一致；跨仓 DAG `MF-06 → FL-06 → FL-03 → FL-04 → MF-05 → FL-05`，MF-05 真 interop 经 `DEP-MF-01` 依赖 Libra FL-04，当前 blocked |
| [`plan-20260916.md`](plan-20260916.md) | 日期计划 | **已完成（2026-09-20）**（24 张活动卡全部 `done`/`complete`；五张 spike 全 go；执行链路移交 [`plan-20260920.md`](plan-20260920.md)。`DEP-02` 已关闭：模板 GC-12 / ER-07 改回 Libra） |
| [`plan-20260920.md`](plan-20260920.md) | 日期计划 | **新建（设计稿，0 实现）**（承接 60916 `DEP-01`：出站 worker / 游标 / reported pack / operator 表面 / `vault_create_once`。OX-01..05 全部 `pending`。不改直播 clone/fetch ACK） |
| [`plan-20260923.md`](plan-20260923.md) | 日期计划 | **收口中**（GitHub issues #25–#29：首次使用路径策略与开通、ImportRepo 增量推送与清理、合成 commit 签名头与帧；FU-01..FU-21 与 FU-04A 已发布（v0.38.22..v0.40.11，其中 v0.40.10 属 `plan-20260913`），FU-22 与 FU-23 随收口发布 v0.40.12（待评审、C 组与 D 组）；计划评审 Codex R22 PASS） |
| [`plan-20260917.md`](plan-20260917.md) | 日期计划 | **已完成**（补全 Libra `mega2 browser` 所需产品 HTTP。**LB-01..LB-07 全部 done/complete**：契约页（LB-01）、`POST /api/v1/delete-entry`（LB-02，v0.10.41）、`POST /api/v1/move-entry`（LB-03，v0.10.42）、storage-only 挂 `tag_router` + trunk 写鉴权（LB-04，v0.10.43）、compose 黑盒 case（LB-05，v0.10.44）、契约页「Libra pin」节（LB-06）、索引登记（LB-07）。不改 `GET /tree` / `POST /create-entry` 字段；不把 list tags 改成 GET；get/delete 无 path 选择器（handler 固定以 `/` 分发，服务层按 tag 名全局查找）；README 产品写列表由用户维护（DEFER-LB-08）） |
| [`plan-20260918.md`](plan-20260918.md) | 日期计划 | **已完成**（文件删移与 Tag 契约跟进。**FT-01..FT-09 全部 done/complete**：契约冻结（FT-01）、`is_directory` 文件删移（FT-02/03 → v0.10.45/v0.10.46；各自 docker 因 #13 path-dep 红，前滚 FIX 由 v0.11.0 证明）、GET `/tags/list` + POST 405（FT-04，v0.11.0）、`mega_tag.path`（FT-05，v0.11.1）、`(path, name)` 隔离（FT-06，v0.11.2）、compose 黑盒（FT-07，v0.11.3，docker job `35276552098`）、Libra pin 重钉（FT-08，`61679e5`）、索引与 60917 DEFER 改指（FT-09）。关闭 60917 `DEFER-LB-01/02/03` 与 `DEFER-LB-11` 隔离面；不关闭 `DEFER-LB-11` 目标归属（`DEFER-FT-02`）与 `DEFER-LB-04..10`。不新增 PT。不改 `../libra/**`） |
