# Mega2 历史投影（History Projection / Views）设计文档

本文档定义 mega2 从根历史确定性生成只读 Git 视图的契约。

> **关键依赖**：根链与物化提交的不变式依赖 [`trunk-push.md`](trunk-push.md)，HTTP / SSH 的分层与能力宣告依赖 [`protocol.md`](protocol.md)；协议、数据库和真实客户端的集成测试场景登记在 [`integration.md`](integration.md)。

> 状态：草案 v0.2（2026-10-02，Asia/Shanghai）。修订分三个阶段：先对照 mega2 代码改写；再经四个视角审查（代码事实、算法与 Josh 保真、内部一致性、trunk 不变式兼容）；最后经 Codex 五轮独立审计，每轮的结论都回到源码核实后才修订（附录 F）。v0.1 → v0.2 的变更见附录 C；v0.1 原文未入库。
> 入库与执行：本文于 2026-10-02 按 Codex 审计后的 v0.2 原样入库，作为 [`../plan/plan-20261002.md`](../plan/plan-20261002.md) 的设计契约。执行计划按本文的节号（§0–§8、附录 A–F）引用条款，节号冻结；按 [`general.md`](general.md) 补齐必需章节由该计划的 HP-01 承接，只增不改既有节号。实现偏离本文时，先修订本文与执行计划，再改代码。
> 依据代码版本：
> - mega2：`Cargo.toml` version `0.41.0`，Libra tip `367cae5`（2026-10-01），依赖 git-internal `0.10.2`
> - Josh：`josh-project/josh` master @ `e6dfb95e`（2026-09-30 UTC），MIT 许可；只移植算法与测试用例
> - 上游 Mega `aecc11a9` 只作背景参考。v0.1 第 0 节是对着上游写的，在 mega2 中多数已不成立，本版整节改写。
>
> 标注约定：
> - **【代码】**：直接读 mega2 或 Josh（`e6dfb95e`）源码得到的事实。涉及 Josh 时写明 Josh 的文件路径。
> - **【推断】**：根据代码推测，未经运行验证。
> - **【决策】**：本设计的选择。
>
> 代码引用写 mega2 的 `src/...` 路径。行号取 2026-10-01 的快照，写作"约 Lxxx"；行号对不上时以符号名为准。Josh 的历史投影规则记作 J1–J5（附录 E），风险记作 R1–R14（7.3）。

---

## 事实校准（2026-10-05）

本次对照 `01f51e4`、crate 版本 `0.41.72`，核对范围为执行计划「事实基线」表的源码、测试、配置及文档锚点，以及本次新增章节中的仓库相对 `file:line` 引用。设计正文中的「约 Lxxx」来自头部注明的旧快照，仍按符号名定位，不属于逐行已核对的引用；执行前依 [`../plan/plan-20261002.md`](../plan/plan-20261002.md) 的 ER-02 再刷新。与旧快照不同的事实和执行口径如下。

1. §4.1 把 `resolve_path_tree_hash_in_txn` 标在约 L1665–1692；现行实现覆盖 `src/jupiter/storage/mono_storage.rs:1665-1694`。缺行与真正没有路径目前同样返回 `None`，视图投影须按设计 §4.1 区分。
2. §0.4 与 §6.1 的 SSH 入口旧锚点 `ssh.rs` 约 L441–487 已漂移；`parse_ssh_exec_request` 现位于 `src/contract/git_protocol/ssh.rs:448-495`，`channel_eof` 位于 `src/contract/git_protocol/ssh.rs:392-430`。事实基线的旧区间 L148–162、L180–191、L223–244 对应的接线现分别位于 `src/contract/git_protocol/ssh.rs:150-164`、`src/contract/git_protocol/ssh.rs:182-194`、`src/contract/git_protocol/ssh.rs:227-248`。上传分轮处理新增了状态，但错误仍写裸文本，退出状态仍固定为 0；错误契约的改造仍由执行计划承接。
3. §6.1 的 HTTP upload-pack 缓冲旧锚点约 L247–252 已扩展：`src/contract/git_protocol/http.rs:247-260` 在输出 protocol_buf 后检查浅克隆分轮标记；事实基线的 v2 请求区间 L310–321 现为 `src/contract/git_protocol/http.rs:313-324`。这不改变视图请求必须先做预检的条件。
4. §0 第 13 条把 `smart.rs` 的浅克隆守卫标为约 L236–244，事实基线写 L236–254；现行守卫在 `src/ceres/protocol/smart.rs:252-270`。§6.3 的 v0 upload-pack 旧打包错误锚点约 L261–287 已漂移到 `src/ceres/protocol/smart.rs:271-318`；浅克隆分轮处理已加入，而普通 pack 错误仍被压成 `InvalidInput`。
5. §4.6 锁表把 L_V 写作 `(VIEW_LOCK_NS, view_key(F.pk))`；执行采用 §4.4 的 `key1 = VIEW_FILTER_LOCK_NS`、`key2 = hash32(filter_pk)`，并与根链单例锁分离。
6. §4.2 的启用前审计需要 Rust 函数 `rebuild_canonical_commit_bytes` 复算对象；Postgres 没有内置 sha1，本仓也没有安装 pgcrypto 的迁移，所以 P0 没有可执行的启用前审计。按 ADR-HP-08 暂由失败告警和停止计数定位首个失败点，完整审计由 `DEFER-HP-04` 的 `mega2 view status` 承接。
7. §3.3 的 `root_chain_halted` 按第 3 步的顺序求值：c 已在根链表中时只检查它是否为链尾；暂存表顶行是无父的 seq = 1 链尾时判为「连上」，谓词为假。
8. §7.5 P0 验收 4 与 §7.4「物化链回归」的启用侧按 `ADR-HP-11` 收窄为 `integration_git_cli_trunk_n1_identity_three_ff_and_no_cl_refs` 与 `integration_git_cli_trunk_n_gt1_squash_sideband_and_nff_align` 两例并加 I3 断言；关闭侧由每张发布卡的 C 组全量门承接。此处只记录执行口径，设计正文仍保留原验收文字。
9. §7.4 错误契约第 8 项的进程级用例以原始 TCP 发送畸形 chunked 请求体（chunk 长度行非法）；`Body::from_stream` 只适用于进程内构造，不用于该进程级场景。
10. §6.3 把 `RepoHandler` trait 标为 `src/ceres/pack/mod.rs` 约 L50–566；现行 trait 是 `src/ceres/pack/mod.rs:50-572`。执行计划事实基线的 `49-572` 包含了 trait 前一行，实施时按符号边界取证。
11. §0.1 把 `b0_reject_push` 标为约 L926–966，执行计划事实基线写到 L955；现行函数是 `src/jupiter/service/push_queue_service.rs:926-967`，末尾还检查单次链长，根路径与主分支的拒绝分支未变。
12. §6.1 的 SSH 错误写出点不能继续按事实基线的旧位置查找：`channel_eof` 在 `src/contract/git_protocol/ssh.rs:425` 固定发退出码 0，upload-pack 与 v2 错误的裸文本写出点见 `src/contract/git_protocol/ssh.rs:602`、`src/contract/git_protocol/ssh.rs:669`、`src/contract/git_protocol/ssh.rs:680`、`src/contract/git_protocol/ssh.rs:695`；此处的修正仅定位源码，不提前改变错误契约。
13. 执行计划「测试」行把 `GIT_CLI_UNAVAILABLE` 标在 `tests/common/git_cli.rs` 约 L20、runner 不可用分支标在约 L440–448；端口分配改造后，常量在 `tests/common/git_cli.rs:21`，该分支在 `tests/common/git_cli.rs:482-537`，仍会在未启用宿主 opt-in 且容器不可用时 panic。
14. 执行计划「文档」行的 `docs/refactoring/integration.md` 迁移覆盖锚点原为 L129–150；增加 compose 黑盒覆盖表后，迁移覆盖章节从 `docs/refactoring/integration.md:143-164` 开始，原行号不再定位该节。
15. §6.1 第 3 层的读者触发重新预热未标（P1），但 §3.3、§7.5 与 §7.4 的阶段描述指向 P1。按执行计划 HP-21 与 ADR-HP-08，P0 实现读者触发的准入；本条只校准阶段归属，不改设计正文的算法与错误契约。
16. 2026-10-08 的 HP-24 冻结运行在 §7.5 P0 验收 11(c)(d) 失败，执行计划按 ER-10 新增 FIX-HP-01。§3.3 与 §4.4 的批量写入、无预算单提交快路和末批终态是性能修复的执行契约；预算调用、根链不连续、投影停止及部分推进仍按原语义。冻结参数和阈值不变，修复发布后再重跑 HP-24。

## 当前实现状态速览表

本表只覆盖设计 §7.5 的 P0 内容；P1–P3 的组件由设计 §7.5 与长期路线图 PT-14 跟踪。

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
|---|---|---|
| Subdir / Prefix / Exclude / Compose / Nop / Empty、规范化与 filter_id | 仅草案 | `src/ceres/` 尚无视图过滤器模块；组合相交必须在注册时拒绝。 |
| 视图元数据与对象表（§3.1–§3.5） | 未实现 | `src/callisto/` 与 `src/jupiter/migration/` 尚无 `mega_view_*` 表。 |
| 线性投影与 view_tip（§4.1–§4.5） | 未实现 | `src/jupiter/service/` 尚无根链追赶；缺对象不能视作空路径。 |
| ViewRepo upload-pack（v0 / v2） | 未实现 | `src/ceres/pack/` 尚无 ViewRepo；能力宣告必须符合实现。 |
| `/.filter/` 与 `/.view/` URL、保留名及写拒绝 | 未实现 | `src/contract/git_protocol/path.rs` 尚未分派视图路径。 |
| want 归属校验 | 未实现 | `src/ceres/protocol/` 尚无视图 commit_map 闸门。 |
| 后台追赶、补偿、视图锁与根链锁 | 未实现 | `src/jupiter/service/` 尚无视图 worker；锁序影响并发正确性。 |
| 注册 API、上限与回收态重新预热 | 未实现 | `src/api/router/` 尚无视图注册接口；准入须跨副本原子化。 |
| `[views]` 配置 | 未实现 | `src/config/model.rs` 尚无 `ViewsConfig`；加载白名单需同步。 |
| sha1 限定 | 仅草案 | `src/config/validate.rs` 尚无视图启用时的 hash kind 启动门。 |

## 硬约束与不可违反的原则

以下约束归纳自 §1.2；任何偏离都须按执行计划的 ADR-HP-01 先更新契约再实现。

1. 投影只读根提交、tree 与 ref，派生对象只写视图表。否则会改动 trunk 的事实源，破坏现有 `/<path>.git` 语义。
2. P0 只服务 trunk、sha1 与线性根链。否则对象 ID 与父链的确定性前提不成立。
3. 视图 URL 只读，且不充当读 ACL。否则调用方会误把过滤结果当成授权边界，或通过视图写入绕过根写入序列。
4. 缺对象、根链分叉和投影前提失败必须停止发布。否则会把损坏误报为空视图，并覆盖已发布的映射。

## 现状与目标对比

下表对照 §0.6 的缺口与 §1.1 的目标。

| 维度 | 当前状态 | 目标状态 | 实现难度 |
|---|---|---|---|
| 历史范围 | 子路径只见首次物化后的提交 | 视图从根提交起按过滤器重建历史 | 复杂 |
| 目录组合 | `/<path>.git` 只对应单路径 | Prefix、Exclude 和不相交 Compose 可组合 | 复杂 |
| 重建 | 无持久根链与视图映射 | 相同根链和定义产生相同对象 ID | 复杂 |
| 读取 | Monorepo 与 ImportRepo 处理 Git 协议 | 独立只读 ViewRepo 支持 v0 / v2 clone 与 fetch | 复杂 |
| 配置与准入 | 无视图配置和注册入口 | 默认关闭并限制注册、并发及速率 | 中等 |

## 迁移步骤（分阶段）

以下阶段对应 §7.5 的 P0，任务依赖和发布窗口以执行计划为准。

**阶段 0 — 契约与索引**

1. 登记本设计、源码事实和长期能力 PT-14。
2. 固定执行计划的评审与文档链接门。

> **验收标准**：设计治理章节与 PT-14 索引可寻址，所有新增相对链接可解析。

**阶段 1 — 纯计算与配置**

1. 实现过滤器代数、tree 求值和字节级 commit 投影。
2. 增加默认关闭的配置与保留名门。

> **验收标准**：过滤器正反用例通过；关闭状态下既有 Git 路径回归通过。

**阶段 2 — 派生状态与准入**

1. 建视图表、根链与追赶事务，加入缺对象停止语义。
2. 接入跨副本准入、后台补偿和注册查询 API。

> **验收标准**：两次冷启动与并发 worker 产生相同 tip；超额注册无残留行。

**阶段 3 — 协议与完成门**

1. 接入视图 URL、ViewRepo、want 闸门和错误传输。
2. 执行真实 Git / Libra 客户端、基准及停服恢复场景。

> **验收标准**：v0 / v2、HTTP / SSH 的只读 clone 与 fetch 按 §7.5 通过，基准达到四项预算。

## 前置依赖矩阵

以下矩阵汇总 §8 的外部与内部依赖；具体同步点在执行计划的 DEP-HP 登记表中。

| 本文档的工作 | 对其他文档的依赖 | 类型 | 关键同步点 |
|---|---|---|---|
| 根链与写入事实 | [`trunk-push.md`](trunk-push.md) | 前置 | 根提交、C 段与 `main@/` 不变式先保持稳定。 |
| ViewRepo 协议 | [`protocol.md`](protocol.md) | 协同 | v0 / v2 能力表和错误应答同步。 |
| 真实客户端验收 | [`integration.md`](integration.md) | 后置 | 路由和存储就绪后加入 Git / SSH 场景索引。 |
| 跨计划 Libra smoke | [`../plan/plan-20261001.md`](../plan/plan-20261001.md) | 前置 | harness 可用且共享脚本无并行写入时执行。 |
| 长期范围 | [`../plan/plan-long.md`](../plan/plan-long.md) | 协同 | P0 收口后更新 PT-14，保留 P1–P3 缺口。 |

## 风险与约束

此处概括 §7.3，细节和恢复动作以该节及执行计划为准。

- **风险：根链分叉或缺对象被误判为空。** 影响：错误视图 tip 被发布且镜像获得虚假历史。缓解措施：可失败读取、停止追赶、告警与可复现的故障注入。
- **风险：准入竞争导致无界冷启动。** 影响：数据库和 worker 资源被占满。缓解措施：在短事务内原子检查总数、名额与速率。
- **风险：协议在宣告后才发现损坏。** 影响：客户端读到不完整 pack。缓解措施：在 ACK 与 pack 字节输出之前执行 want 和对象闭包预检。
- **约束：只读视图与默认关闭。** 理由：中间发布时派生状态尚未完整。违反后果：客户端可能访问未就绪历史或写入虚拟命名空间。
- **约束：锁序与单快照。** 理由：多个 worker 必须对同一根 tip 作一致判定。违反后果：死锁、重复提交或不确定的映射。

## 改进方案多维评估小结

| 维度 | 评估结论 |
|---|---|
| 合理性 | 8/10。当前不足：子路径历史不能回溯到根起点；改进方向：按根链提供确定性视图。 |
| 可行性 | 7/10。当前不足：根对象读取会混淆缺行和路径不存在；改进方向：先交付可失败读取接口。 |
| 完整性 | 7/10。当前不足：协议、准入和停服恢复尚无实现；改进方向：按阶段完成真实客户端矩阵。 |
| 安全性 | 8/10。当前不足：虚拟 URL 尚无早期写拒绝；改进方向：定位阶段先拒绝 receive-pack 与 LFS。 |
| 功能正确性 | 7/10。当前不足：线性投影规则只有设计；改进方向：移植正反用例并核对对象哈希。 |
| 可靠性 | 7/10。当前不足：缺对象可能被静默略过；改进方向：停止语义与故障恢复同事务验证。 |
| 兼容性 | 8/10。当前不足：共享 v2 零 tip 宣告有伪 ref；改进方向：独立修正并跑旧路径回归。 |
| 可扩展性 | 7/10。当前不足：没有跨副本准入与 worker 补偿；改进方向：事务配额和周期扫描。 |
| 合规性 | 8/10。当前不足：部分错误口径与已发布设计正文不同；改进方向：在事实校准和评审记录中持续登记。 |

## 小结

本设计给出从 trunk 根链生成只读视图历史的语义。P0 限定在线性根链与 sha1，使过滤器、对象 ID 和协议行为能够逐项验证。派生表和后台追赶维持根数据不变，异常时停止发布。执行计划负责逐卡验证并保留 P1–P3 的长期缺口。

## 预期收益

- 相同根链与过滤器在清表重建后得到逐字节相同的视图 tip。
- Git 与 Libra 客户端可从视图 URL 克隆完整前史并增量获取新提交。
- 根链、对象或协议前提失败时返回可诊断错误，不发布错误的空历史。

## 0. 背景：mega2 现状

### 0.1 部署形态与写入模型

1. **【代码】trunk 是默认形态，也是开源交付形态。** `PushPolicy` 的 `#[default]` 是 `Trunk`（`src/config/model.rs` 约 L405–414）。review 形态仍受支持，需用 `config/config-review.toml` 显式开启；在队列为空时，两种形态可以互相切换。trunk 启动时 fail-closed：
   - `Config::validate` 要求 `cedar.enforcement = off`、显式设置 `push_auth`、`ssh_receive_pack = false`（`src/config/validate.rs` 约 L615–638）；
   - `prepare_push_policy_startup` 要求没有未关闭的 CL（`src/jupiter/storage/mod.rs` 约 L367–377）。

   trunk 下不注册 CL 路由，`api_router::routers_for` 在 Trunk 下只挂 storage-only 路由集（`src/api/api_router.rs` 约 L55–90）；SSH 只读。
2. **【代码】服务接流后的根写入都经 MonoWriteQueue 串行；唯一的例外是接流前的 bootstrap。** `initialize_monorepo`（`src/jupiter/service/mono_service.rs` 约 L103）在事务内先取初始化专属 advisory 锁（约 L105），再写根 `main`（约 L126）。根提交由 `MegaModelConverter::init` 用 `Commit::from_tree_id(root_tree.id, vec![], …)` 构造，没有父提交（`src/jupiter/utils/converter.rs` 约 L727–732）；在线性根链上，它就是 seq=1 的提交（3.3）。bootstrap 对应 trunk-push.md 硬约束 2 和 I5（附录 A，约 L1600）中的第三类写入者，即"在服务接流前完成并持专属锁"。MonoWriteQueue 由 Postgres 的 `push_queue` 表和 `PushQueueService`（`src/jupiter/service/push_queue_service.rs`）组成，流程如下：
   - B0：准入；
   - B2.5：认领，同时写入 `expected_*` 根基线；
   - B3：在 `pg_advisory_xact_lock(MONO_WRITE_LOCK)` 下单事务执行，恰好做一次根 CAS；
   - C 段：`run_c_segment_index`（约 L731），在锁外执行，但会在回复客户端之前被 await。

   队列 kind 是 Postgres 枚举，只有 `push / merge / attach` 三种（`src/callisto/sea_orm_active_enums.rs` 约 L220–227）。`push_queue_active_push_path` 部分唯一索引只约束 `kind='push'`（ADR-TP-10）。ADR-TP-02/03（`docs/refactoring/trunk-push.md` L194、L201）否决了用 Redis/RedLock 做写入序列化，所以本设计不用 Redis 锁。
3. **【代码】trunk 拒绝推送到 `path="/"`（`b0_reject_push`，约 L926–955），并且只接受 `refs/heads/main`。** 因此所有写入都落在某个子路径 P 上。
4. **【代码】推送链由 `PushChain` 校验**（`src/ceres/pack/push_chain.rs`）：链必须连通，链内 merge 提交被拒绝（ADR-TP-17），链长上限为 `[monorepo].max_push_commits`。上游"一次只能推一个提交"的限制已在 MC-06 中移除。
   - N=1：客户端推上来的提交就是 `main@P`（ADR-TP-12、硬约束 5、不变式 I2；`push_queue_service.rs` 约 L2671）。
   - N>1：在 P 处压成一个 squash 提交，它的 message 列出全部被合并的提交（ADR-TP-15）。
5. **【代码】根层与祖先层是服务端合成的 roll-up 提交**（`mono_api_service.rs::apply_push_in_txn` 约 L3958；`src/ceres/pack/trunk_provenance.rs`）：
   - author 取推送链 tip 的 author（ADR-TP-13）；
   - committer 的姓名、邮箱取 tip；时间取 max(落地时刻, 上一根提交时间)，时区沿用前值或取 +0000（ADR-TP-14，约 L108–125）；
   - 剥掉客户端的 gpgsig，改嵌入服务端的 gpgsig（`src/contract/vault/server_signing.rs::embed_gpgsig` 约 L149）；
   - N>1 时，根层与祖先层用 compact message，只带 `Mono-Path` 与 `Mono-Squash-Commit: <id>` 指针（`trunk_provenance.rs::compact_message` 约 L182）；
   - 净零推送只推进 P，根不前进（ADR-TP-16），此时 B3 仍执行一次同值根 CAS（`mono_api_service.rs` 约 L4058–4099）。
6. **【代码】以下写入者仍用 git-internal 的 `Commit::from_tree_id`**，即作者为 `mega <admin@mega.org>`、时间取 `now()`、时区 `+0800`：
   - review 合并（`process_ref_updates`，约 L1035）；
   - attach/detach 的根提交；
   - Legacy 风格的后代续接；
   - trunk 下的产品写 API（edit/save、create/delete/move-entry），经 `land_api_tip_push` 以 N=1 落地；
   - 接流前的 bootstrap 根提交（`converter.rs` 约 L732，见 0.1-2）。

### 0.2 子路径历史：物化链与墓碑续接

7. **【代码】advertise `/<path>.git` 时会调用 `materialize::materialize_path_refs`**（`src/ceres/pack/materialize.rs` 约 L74）：
   - 已有 `main@P` 行时直接返回；
   - 没有时，在 `MONO_WRITE_LOCK` 短事务内校验根身份对，然后插入新行。父提交取墓碑记录的 `last_commit_hash`（`materialize_parents_in_txn`）；只有既没物化过、也没有墓碑的路径，才生成无父快照；
   - author/committer 取根 tip，message 剥掉签名头后重新成帧，提交写入 `mega_commit`。

   7a. **【代码】trunk 的 B3 也会物化祖先。** `apply_push_in_txn` 对 P 的每个非根祖先 upsert main 行；某个祖先还没有行时，直接用无父的合成提交新建（约 L4017–4049），这一步不查墓碑。所以一个路径的历史起点，是"首次被 advertise"和"首次有后代被推送"二者中较早的那次。【推断】祖先路径带墓碑时，这里可能绕过墓碑续接，已另立核实项。
8. **【代码】B3 推进后代 ref**（`mono_storage.rs::advance_descendant_refs_with` 约 L649）。推进用 Merkle 剪枝：子树没变的路径不前进；路径消失时写墓碑（`mega_ref_tombstones`）。`remove_none_cl_refs` 已没有生产调用方。
9. **【代码】不变式**（`docs/refactoring/trunk-push.md` 附录 A）：
   - I1：已物化路径的历史只增不改；
   - I2：内容保真，N=1 时还要求对象保真；
   - I2a：每个受影响的已物化 ref 恰好前进一个提交；
   - I3：`main@P.ref_tree_hash == resolve(root, P)`，强一致（ADR-TP-19 否决了惰性推进）；
   - I4：N>1 时完整 provenance 落在被推路径的 squash 提交上，其他层用指针引用；push_queue 每行锚定的提交链都必须可解析，引入 GC 时视为 GC root；
   - I5：队列完备性；
   - I6：根 roll-up 的顺序与 `push_queue.id` 一致。

   `main@P` 行是以下逻辑的输入：B3 的 NFF 闸门（约 L2530–2540）、TP-11 树哈希断言、reaper 的 `apply_i3`、巡检修复、产品写 API 选父提交、路径级 Tag、`blob_paths` 的 C 段索引。
10. **【推断】mega2 的子路径链在结构上已经是一种"存储式的 `:/P` 投影"。** I3 相当于 `filter_tree`；I2a 与净零剪枝相当于 J4（丢弃空变更）。它与确定性投影只差两点：
    - 提交身份：子路径链含客户端的原样对象，无法从根历史重算；
    - 历史起点：子路径链从首次物化开始，投影从仓库起点开始。

### 0.3 读侧

11. **【代码】REST 读接口缺省从 `main@/` 出发。** history、blame、latest-commit、tree、blob 都按路径遍历并过滤（如 `commit_ops.rs::resolve_start_commit`），不依赖子路径链；MST/2 快照也固定在 `main@/`。
    - 这些接口都带可选的 `refs` 查询参数（`src/api/router/preview_router.rs`）。参数缺省或为空时取 `main@/`；也可以传 tag 或提交 ID。其中 `resolve_start_commit`（`commit_ops.rs` 约 L261–311）接受 `main`/`master`、tag 和 7–40 位 SHA；tree 走 `get_root_tree`（`mono_api_service.rs` 约 L1719–1745），只接受 40 位 SHA 和 tag。
    - 按提交 ID 解析时，查的是整张 `mega_commit`，可能命中子路径坐标的提交（0.5-19），此时该提交的 tree 会被当作根树。【决策】6.6 的视图接口不沿用这种解析方式，`commit=` 只接受该视图的视图提交或根链上的提交。
    - ScorpioFS 只调用按路径的 REST（`/api/v1/tree?path=` 等），不固定 commit。
12. **【代码】读授权是全局的。** upload-pack 只看 `anonymous_access` 与是否登录（`src/contract/git_protocol/mod.rs` 约 L29–42）；storage-only 的 `/api/v1` 没有认证中间件（`src/server/http_server.rs` 约 L773–777），可以按哈希读任意 blob。

### 0.4 协议

13. **【代码】能力宣告。** v0 宣告 `multi_ack_detailed no-done shallow`（`src/ceres/protocol/smart.rs` 约 L57）；v2 宣告 `ls-refs` 与 `fetch=shallow filter`（`src/ceres/protocol/v2.rs` L22），其中 filter 只支持 `blob:none`。宣告是全局的，各 handler 再通过 `RepoHandler::supports_shallow_fetch` / `supports_filtered_fetch` 判断自己是否支持。这两个方法默认返回 false（`src/ceres/pack/mod.rs` 约 L276–300，ImportRepo 用的就是默认值），Monorepo 覆写为 true（`src/ceres/pack/monorepo.rs` 约 L253、L373）。客户端请求了 handler 不支持的能力时，返回明确的协议错误（`smart.rs` 约 L236–244，`v2.rs` 约 L192–205）。git 从 2.26 起默认走 v2；Libra 只走 v0。
14. **【代码】`Monorepo::incremental_pack` 对父提交逐个调用 `get_commit_by_hash`**，每个提交一次 DB 往返，父提交缺失时 `unwrap` 会 panic。它不做 want 校验：want 全部不是 commit 时，整批交给 `direct_object_pack` 按哈希下发；want 中混有 commit 时，非 commit 的 want 被静默忽略。
15. **【代码】URL 路由。**
    - HTTP 的 catch-all 路由进入 `src/contract/git_protocol/path.rs::parse_git_protocol_path`。`normalize_repo_path`（约 L106）只剥掉一个尾部 `.git`，空路径归一为 `/`。
    - 推送授权在构造 handler 之前就使用 URL 路径：`check_push_permission`（`http.rs` 约 L71/L391，`ssh.rs` 约 L163）。
    - `src/ceres/protocol/mod.rs::repo_handler_with_commands` 按 `import_dir` 前缀分派给 ImportRepo，其余交给 Monorepo。

    - LFS 不经过 `parse_git_protocol_path`。`http_server.rs` 约 L455 用 `MapRequestLayer` 全局安装 `rewrite_lfs_request_uri`（约 L888）。它把任意 `/<repo>/info/lfs/...` 改写为 `/info/lfs/...`，把 `<repo>` 存入 `LfsRepoContext`，再交给约 L821 nest 的 LFS 路由。trunk 下的 LFS 写闸门是 `lfs_router.rs::enforce_trunk_lfs_access`（约 L236–268）：`push_auth=none` 直接放行；`token` 用 `token_covers_repo(token, <repo>)` 判定。
    - v2 的 `info/refs` 不构造 handler。`git_info_refs` 在认证之后，对带 `Git-Protocol: version=2` 的 upload-pack 请求直接返回全局能力列表（`http.rs` 约 L75–86）；SSH 在 exec 阶段同样直接发送（`ssh.rs` 约 L180–191）。handler 要到随后的 `ls-refs` / `fetch` 才第一次构造（`v2.rs` 约 L64、L188）。`From<MegaError> for ProtocolError` 只把 `MaterializeAborted` 映射为 503，其余一律映射为 400（`src/common/errors/mod.rs` 约 L372–392）。
    - SSH 不经过 `path.rs`：它用 `ssh.rs::parse_ssh_exec_request`（约 L441）解析命令和路径，`git-lfs-authenticate` / `git-lfs-transfer` 也由它解析。

    因此 `/.view/x.git` 目前会被当成一个空的 Monorepo 路径：advertise 返回空 refs；如果 token 覆盖全仓，receive-pack 也会被放行进入 handler。`/.view/x.git/info/lfs/...` 会进入 LFS 路由，锁和 FastCDC 媒体键以 `/.view/x.git` 为命名空间写入。【推断】这不构成越权：能通过 LFS 写闸门的只有 `push_auth=none`、不限路径的 token、或授权路径为 `/` 的 token，这些凭证本来就能经 `/info/lfs` 写入同一个内容寻址的存储。
16. **【代码】不支持 push-options。** `RECEIVE_CAP_LIST` 中没有 `push-options`；`split_receive_pack_request`（`smart.rs` 约 L329）要求命令 flush 之后紧跟 PACK，push-options 段会被当作非法载荷。Libra 已支持 `-o`，但服务端没宣告该能力时会直接拒绝推送。
17. **【代码】推送授权。**
    - `push_auth=token`：按 token 的 `paths` 前缀做组件边界匹配（`apply_push_auth_gate`，`src/contract/git_protocol/mod.rs` 约 L88）。产品写 API 走 `api::api_write_auth::authorize_trunk_api_write(git, headers, path)`（约 L29），同样是逐路径授权。
    - `push_auth=none`：不做路径授权，直接以 anonymous 放行。
    - review 形态禁止设置 `push_auth`，推送只要求登录；`authorize_trunk_api_write` 在这种形态下恒返回 401。

### 0.5 对象与存储

18. **【代码】表与迁移。**
    - 实体在 `src/callisto/`，迁移在 `src/jupiter/migration/`：迁移只前进，写法是 raw DDL 加 `IF NOT EXISTS`，在 `Migrator::migrations()` 中注册。
    - 只支持 Postgres：`validate.rs` 拒绝其他 `db_type`。
    - git 对象 id 一律以 hex TEXT 存储。`mega_commit` 的列为 `id, commit_id UNIQUE, tree, parents_id JSON, author, committer, content TEXT, …`。`mega_tree.sub_trees` 是 BYTEA，存的是 `Tree::to_data()` 重新序列化后的字节，`tree_id` 却是接收时按原始字节算出的哈希。路径是：git-internal `decode.rs` 约 L1172 → `converter.rs::process_entry` 约 L138 → `into_mega_model` 约 L260–264。`Tree::from_bytes` 解析时把 `100664`、`100640` 归一为 `100644`（git-internal `tree.rs` 约 L95–125）；非 UTF-8 文件名先按 GBK 解码，再以 UTF-8 写回（约 L187–216）。`40000`、`100644`、`100755`、`120000`、`160000` 条目与合法 UTF-8 文件名逐字节不变。所以经 receive-pack 原样收下的客户端 tree，只要含前两类条目，就会出现 `hash(Tree, sub_trees) ≠ tree_id`。mega2 自己构造的 tree 由归一后的条目重新计算哈希，内容与 id 自洽，包括 B3 重建的祖先、产品写 API 和 bootstrap。

    18a. **【代码】L0 不保存 tree 的原始字节，现有打包会输出内容与 id 不符的 tree。**
    - receive-pack 不落盘 pack：`src/ceres/pack/mod.rs` 约 L115 的 `temp_pack_id` 恒为空串，回填 pack_id 的代码已注释（约 L195–213）；对象存储的 `Git` 命名空间只存 blob。`batch_save_model` 只插入、冲突不覆盖（`monorepo.rs` 约 L215），同一对象重推也修复不了坏行。
    - 现有 `/<path>.git` 的 clone/fetch 经 `Tree::from_mega_model` → `Entry::from(Tree)` 打包（git-internal `entry.rs` 约 L62–70），用的是归一后的字节配原 `tree_id`。客户端按收到的字节重算对象名，连通性检查随即失败。这是 L0 既有缺陷，视图只是继承。
    - 同类既有缺陷还有两处。其一，GBK 也解不开的文件名会让 `process_entry` 的 `unwrap` 在 spawn 出的保存任务中 panic，而 `receiver_handler` 对 `JoinError` 只记日志、不返回错误（约 L189–191），该批对象被静默丢弃。其二，默认的 `traverse` / `traverse_for_count` 把非 `Tree` 条目一律当 blob（约 L407、L454），遇到 `160000`（gitlink）就去对象存储取一个不存在的 key，打包失败。
    - commit 也有同类缺陷：author/committer 列是重新序列化的，个别客户端提交无法逐字节重建，打包时同样会输出内容与 id 不符的字节（4.2）。
19. **【代码】`mega_commit` 混存两种坐标的提交。** 除了根提交，它还存客户端推到子路径的提交（以该子树为根），以及物化与续接生成的提交。ImportRepo 的历史在 `git_*` 表里，不在这里。
20. **【代码】git-internal 0.10.2 的相关行为：**
    - `Commit::from_bytes` 把 committer 行之后的全部字节（gpgsig、gpgsig-sha256、mergetag、encoding 以及正文）原样放进 `message`，`to_data` 再原样写回（`commit.rs` 约 L279–323）；
    - `Tree::from_tree_items(_with_kind)` 不排序，且拒绝空条目列表；
    - `Commit::from_tree_id` 硬编码作者身份，并取 `now()` 与 `+0800`；`Commit::new` 依赖 thread-local 的 hash kind；
    - 按 kind 显式构造的 API 有：`Commit::new_with_kind`、`from_tree_id_with_kind`、`Tree::from_tree_items_with_kind`、`ObjectHash::from_type_and_data_for_kind`（`hash.rs` L418）。

    mega2 中可复用的工具：
    - `sort_git_tree_items`（`src/jupiter/utils/converter.rs` 约 L498）；
    - 不会 panic 的 `rebuild_canonical_commit_bytes(&mega_commit::Model) -> Result<String, MegaError>`（`src/ceres/merge_checker/gpg_signature_checker.rs` 约 L323）；
    - `is_signature_header`（`src/common/utils.rs` L98，覆盖 gpgsig 与 gpgsig-sha256）；
    - `split_commit_message`（`utils.rs` 约 L102；没有成帧空行的历史 message 整体视为正文）；
    - `tree_from_items_checked`（`mono_api_service.rs` 约 L819）只能参考它的重名检查，不能直接调用，因为它内部用的是依赖 thread-local kind 的 `Tree::from_tree_items`。
21. **【代码】批量读取。** `mono_storage.rs::get_commits_by_hashes` / `get_trees_by_hashes`（约 L1626 / L1696）都是单条 IN 查询，但 DB 出错时直接 `unwrap`。Redis 的 `GitObjectCache` 没有批量接口。
22. **【代码】对象格式。** `object_format` 是部署级的单一取值（sha1/sha256/blake3），修改需要重启。L0 写入者多处依赖 thread-local kind；`tests/` 中没有非 sha1 的 Git 端到端测试；github_sync 也要求 sha1。
23. **根历史的形状。**
    - 【代码】`/` 上的每个提交都只有一个父提交：trunk 的 roll-up、attach/detach 都以锁内的当前根为父，推送链也拒绝 merge。
    - 【代码】例外：review 形态合并根路径 CL 时，`process_ref_updates` 取该路径上按 ref_name 排序的第一个候选 ref 作父提交，`refs/cl/<link>` 排在 `refs/heads/main` 之前，于是父提交是 CL tip（约 L1019–1037）。结果是根首父链可能包含客户端的 CL 提交；CL 没有基于当前 `main@/` 时，旧 tip 还会离开首父链。
    - 【推断】存量数据是否线性、是否连续，在建链时校验（3.3）。
24. **【代码】路径策略。**
    - `path_policy::classify_creation_path`（约 L19）：创建路径时，首段必须在 `root_dirs` 中，路径不能落在 import 命名空间内。
    - `check_write_operands`（约 L157，ADR-FU-06）：写入不得改动 import_dir 本身及其下的内容；删除操作的对象不得是嵌套着 import_dir 的严格祖先。
    - GC-FU-04：import_dir 的严格祖先（包括 `/`）永远不物化 main 行；产品写入永远不落在 `/` 上。
    - trunk 下删除路径必须通过在其父路径推送来完成。

### 0.6 本设计在 mega2 中要解决的问题

v0.1 的核心动机是"子路径每合并一次就变成无关历史"，这一点在 mega2 中已由物化链加墓碑续接解决。剩下的缺口有四个：

- (a) 子路径**首次物化之前**的根历史，在 `/<path>.git` 上看不到；
- (b) 没有 Prefix / Exclude / Compose 这类组合视图：无法提供跨目录的 Agent 工作区，也无法通过"排除某目录"来裁剪上下文；
- (c) 没有一份能确定性重建、能按视图镜像的历史；
- (d) Agent 只能按单个路径取上下文。

本设计**新增**视图 URL 来补这些缺口，**不替换** `/<path>.git`。

---

## 1. 目标与非目标

### 1.1 目标
- **G1 确定性派生。** L0 定义为从 `mega_refs(path="/", ref_name="refs/heads/main")` 可达的根提交 DAG。P0–P1 只支持这个 DAG **线性且连续**的情形：首父链就是全部历史，每个新根 tip 的首父都是上一个根 tip。这一前提在建链和每次扩展时都会校验，不满足就停止（R11）。这条首父链下称"根链"。P2 可以按附录 E 支持非线性根链，但那时只做只读投影；P2 的 view_push 仍只在线性根链上运行，遇到多父提交时 fail-closed（5.2 第 2 步）。视图历史是 L0 的确定性函数：派生数据可以清空，重建后哈希逐字节一致。签名和落地时间一旦写入 L0 就固定了，不影响确定性。
- **G2 视图有完整历史。** 视图从仓库起点开始，父子关系与根链一致。作者、提交者和说明取自对应的根提交，签名头按 4.2 剥离。
- **G3 增量。** 根链新增 k 个提交后，更新一个视图的开销是 O(k × 每个提交的 tree 工作量)，与历史长度无关。冷启动的串行数据库往返次数是 O(H/B × 深度)，B 为批大小（默认 1000），不能出现"每个提交一次往返"。
- **G4 双向（P2）。** trunk 形态下，可以向一个"可推送视图"（2.2）推送一条线性提交链。服务端在 MonoWriteQueue 内把它反向映射后落地。**接受**推送后客户端需要按 ADR-TP-18 对齐（fetch + reset）；不承诺 Josh 式的往返恒等。
- **G5 客户端。** 标准 git（v0/v2）、Libra（v0）、ScorpioFS（REST）和 Agent 用同一个视图标识访问同一个视图。

### 1.2 非目标与硬约束
- **不替换 `/<path>.git`。** 它的 advertise/fetch/push 继续由 Monorepo 加物化链应答。`main@P`、墓碑、I1–I6、ADR-TP-12…20 都不变。
- **只支持 trunk 形态。** 在 review 形态下设置 `[views].enabled = true` 时，`Config::validate` 拒绝启动。review 形态下的只读视图、鉴权模型和视图 CL 放到 P3 评估（附录 D）。
- 不修改 push/merge/attach 现有的根提交构造规则（ADR-TP-13/14/15、服务端签名）。P2 的 view_push 沿用 ADR-TP-13/14 与服务端签名；它的 message 和 trailer 是 ADR-TP-15 的一个新分支（5.2 第 6、7 步），在 trunk-push.md 中登记（第 8 节）。
- 不引入 josh-proxy、sled 或 gix，只移植算法与测试。
- 投影生成的视图提交和视图专有 tree，不写入 `mega_commit` / `mega_tree` / `mega_refs`。P2 推送时，pack 中的 tree 在 A 段写入 `mega_tree`（5.1，内容寻址）；落地时 unapply 新生成的 tree 属于 L0，在 B3 写入 `mega_tree`（5.2）。客户端推送的视图提交写入 `mega_view_pushed_commit`（3.6），不写入 `mega_commit`。
- **视图不是读安全边界**（0.3-12）。want 校验只用于保证正确性和裁剪上下文。
- 视图的源路径不能落在 `import_dir` 之下，因为 ImportRepo 的历史不在根历史里。可推送视图另有更严的约束（2.2）。
- 不支持 merge 推送（ADR-TP-17）、新分支和孤儿链。
- P0–P1 不支持 Josh 的 `:workspace`、`:stored`、`:starlark`、`:rev`、`:hook`、regex 替换和 insert；glob 放到 P2。
- canonical 形式为 `:nop` 或 `:empty` 的过滤器拒绝注册：整个仓库请直接用 `/` 或现有 URL。Josh 中 nop 的恒等语义不移植。`src_paths` 为空的过滤器同样拒绝（2.2）。
- P0–P1 只支持 `object_format = "sha1"`。sha256/blake3 要等 L0 写入者完成 hash kind 显式化之后再支持（P2）。
- 不提供 Josh 风格的 `repo.git:<spec>.git` URL。

### 1.3 与物化链的关系【决策】

| | `/<path>.git`（物化链） | 视图 `:/<path>`（`/.filter/<id>.git`） |
|---|---|---|
| 历史起点 | 首次物化（advertise，或某个后代的首次推送）；advertise 路径可从墓碑续接 | 仓库起点 |
| N=1 推送后的 tip | 客户端提交本身 | 根 roll-up 的投影（committer 时间、签名都不同） |
| 能否从 L0 重算 | 不能（含客户端原样对象） | 能 |
| 写入方式 | Monorepo，`kind=push`，N=1 无需对齐 | P2：`kind=view_push`，推送后需要对齐 |
| 主要用途 | Agent 日常推送 | 完整前史、组合视图、镜像、上下文裁剪 |

同一内容在这两个 URL 下的提交哈希不同。客户端不得把两者当作同一个 remote 混用，Libra 和 Agent 只能选其一（用户文档会写明）。"用投影给首次物化补上前史"会改变 L0，留到 P2 另立 ADR（附录 D）。

---

## 2. 过滤器模型（P0 子集）

### 2.1 语法（Josh 文本语法的子集，便于移植测试）【决策】
```
filter   := chain
chain    := op+                         # 并列即串联（Chain），从左到右依次应用
op       := ":/" PATH                   # Subdir：取子树，并把它提升为根
          | ":prefix=" PATH             # Prefix：把整棵树挂到 PATH 下
          | ":exclude[" sel ("," sel)* "]"   # Exclude：从当前树中减去 sel 选中的部分
          | ":[" filter ("," filter)* "]"    # Compose：并集
          | ":nop" | ":empty"
sel      := "::" PATH                   # 选中 PATH 处的条目，不论类型（blob/symlink/tree/gitlink）
          | "::" PATH "/"               # 只选中 PATH 处的 tree
PATH     := BARE | QUOTED              # 编码规则见下文“PATH 编码”
BARE     := (安全字符 | "/")+
QUOTED   := '"' (允许字符 | "/" | '\"')+ '"'   # 引号包住整个 PATH；唯一的转义是 \"
```
- 与 Josh 的对应【代码】：`Op::Subdir / Prefix / Exclude / Compose / Chain / Nop / Empty`（`josh-filter/src/op.rs`）。在 `josh-filter/src/flang/parse.rs` 中，`::p/` 解析为 `subdir(p).prefix(p)`，`::f` 解析为 `Op::File(f, f)`。`Op::File` 取 f 处的条目时不区分类型（`josh-core/src/filter/mod.rs` 约 L1160–1175），所以 `:exclude[::secret]` 也会隐藏目录 `secret/`，本设计与之保持一致。
- v1 **没有** meta 选项（如 `:~(...)`），签名处理是固定规则（4.2）。
- Mega 路径 `/project/foo` 写作 `:/project/foo`。

**PATH 编码【决策】（冻结进 v1，见 2.4）**
- **字符集。** 过滤器文本是 UTF-8 字符串。API 的 `filter_spec` 是 JSON 字符串：先解码 JSON 层的转义，本节规则作用在解码后的文本上，两层互不相干。不支持非 UTF-8 字节，也不需要支持。【代码】L0 的 tree 条目名是 `String`（git-internal `tree.rs` 约 L161）；非 UTF-8 名字在解析时先按 GBK 转成 UTF-8，转不了就报错（约 L187–216）；`mega_refs.path` 是 TEXT（`src/callisto/mega_refs.rs` 约 L11）。所以 L0 中可寻址的路径都是 UTF-8。原名是 GBK 字节的目录，按解码后的 UTF-8 名书写。
- **段规则。** 段非空；不是 `.` 或 `..`；不含 `/`、`\`、NUL 及其他控制字符（`char::is_control`）；首尾字符都不是空白（`char::is_whitespace`）。不做 Unicode 规范化（NFC/NFD），也不做大小写折叠，按 UTF-8 字节逐段比较。
  - 依据【代码】：`strict_creation_path_input` 拒绝 NUL、`\` 与控制字符（`src/ceres/pack/path_policy.rs` 约 L49–69）；`canonicalize_mono_ref_path` 拒绝 `\` 与 `..` 段，并去掉整串的首尾空白（`src/common/utils.rs` 约 L238–265）；`normalize_token_path` 把 `\` 改写为 `/`，也去掉首尾空白（`src/config/model.rs` 约 L1196–1209）。
  - 如果允许 `\`，授权会错位：源路径 `/a\b` 在 `token_path_authorizes`（约 L1187）中被当作 `/a/b`，被授权的路径和实际写入的路径不是同一个。如果允许段首尾有空白，这个段落在路径末尾时会被 trim 掉；而 2.3 第 2、3 条会拼接和拆分路径，段的位置会变。按段禁止首尾空白之后，任何拼接结果都仍然合法。
  - 由此，任何源路径 p（写成 `/…`）都满足 `canonicalize_mono_ref_path(p) == p` 与 `normalize_token_path(p) == p`，并能通过 `strict_creation_path_input`。
  - 代价：名字含 `\`、控制字符或首尾空白的条目无法被单独指名，既不能 Subdir 到它，也不能单独 exclude 它。它们仍随父目录整体出现在视图中。
- **安全字符。** ASCII 的 `A–Z`、`a–z`、`0–9`、`.`、`_`、`-`，以及所有非 ASCII 且不是空白的字符。
- **引号。**
  - PATH 全部由安全字符和 `/` 组成时可以不加引号，否则必须用双引号包住**整个** PATH。只给部分段加引号是语法错误：`:/web/"@types"` 应写成 `:/"web/@types"`。
  - 引号内唯一的转义是 `\"`，表示 `"`。其他任何 `\` 都是语法错误，因为段内本来就不允许 `\`。引号内的 `/` 仍是分隔符，不能转义。
  - 未加引号的 PATH 中出现安全字符以外的字符时，按语法错误处理并提示加引号；不采用“先宽松接受、再规范化”的做法。这样 `*`、`?`、`~`、`(`、`=` 等字符就保留下来了：P2 引入 glob 或新算子时，v1 的 canonical 文本不会被重新解释。
- **首尾 `/`。** `:/` 与 `:prefix=` 的参数先去掉引号，再去掉首尾的 `/`（2.3 第 1 条）。选择器不做这项容忍：tree 标记 `/` 必须写在引号之外。`::"a b"/` 合法；`::"a b/"` 末段为空，被拒。
- **空白。** 输入只在以下位置容忍 ASCII 空白（空格、`\t`、`\n`、`\r`）：整串首尾、`[` 之后、`,` 前后、`]` 之前。其他位置出现空白都是语法错误。
- **canonical 打印。**
  - 各算子分别写作 `:/P`、`:prefix=P`、`:exclude[s,…]`、`:[f,…]`、`:nop`、`:empty`，Chain 直接拼接。不输出任何空白，`,` 后面不加空格。
  - P 的各段用 `/` 连接，不带首尾 `/`。全部是安全字符时不加引号；否则整体加引号，其中的 `"` 写作 `\"`。选择器写作 `::P` 或 `::P/`。
  - Compose 成员与 Exclude 选择器各自按 canonical 文本的 UTF-8 字节序排序，即 Rust `str` 的 `Ord`。2.3 第 5、6 条所说的排序都指这个顺序。
- **往返要求。**
  - 对任意 canonical AST，`parse(print(ast)) == ast`。
  - 对任意可接受的输入 x，`print(canonicalize(parse(x)))` 是 `print∘canonicalize∘parse` 的不动点。
  - 加载过滤器定义时（6.1 第 3 层，以及 worker 首次加载），要求 `print(canonicalize(parse(canonical_spec))) == canonical_spec`，并且按 2.4 重算的哈希等于 `filter_id`。任一项不成立，都按 3.1 的防御性校验拒绝服务并告警。这样，实现升级如果改变了打印或规范化的结果，会立即暴露，而不是静默地产生第二个 filter_id。
- **与 Josh 的关系【推断】。** 本轮没有核对 Josh 源码中的引号细节，可能与上述规则不同。移植 Josh 用例时，含引号的输入按本节规则改写。
- **黄金向量。** 下表中的 filter_id 已按 2.4 的公式实际算出。

  | 输入 | canonical 文本 | filter_id |
  |---|---|---|
  | `:/project/foo/`；`:/"project/foo"` | `:/project/foo` | `8fc638cef18f06b23a788f7eb21c085cb4b7aa40974f29010e2e7e60f045834f` |
  | `:prefix="a,b"` | `:prefix="a,b"` | `f238ead9656130686004bc34d7c61ddc8fe2796386c7df1a8302f33fd6bb6f6b` |
  | `:/"x]y"` | `:/"x]y"` | `728494f84405f553e3e4406b2effd9414944dd0446eb44c038b9b36a6265ded5` |
  | `:/"say \"hi\""` | `:/"say \"hi\""` | `a4fd2c31643a45a650264530a24c3e7e7e23051b532e91153cfd9edc022d80ed` |
  | `:/a:exclude[ ::b , ::"c,d"/ ]` | `:/a:exclude[::"c,d"/,::b]` | `37f8ba0aef0540241ae5ebb80ea21c34c2487f511cfd047b9189dec7d00544a2` |
  | `:[:/b:prefix=y, :/a:prefix=x]` | `:[:/a:prefix=x,:/b:prefix=y]` | `4e77701b623b885b03e6e6a766f6eb7af6d163f169a5fa6d0fa5ecf71309f3d1` |
  | `:[:/a:prefix=a,:/B:prefix=b]` | `:[:/B:prefix=b,:/a:prefix=a]`（按字节序，`B` 排在 `a` 之前） | `f9ed38b3cc202c93c54af197b4433547900cbc7be559a0df2ca103b8b05c8a8b` |
  | `:/中文/目录`；`:/"中文/目录"` | `:/中文/目录` | `aa95db5b3ad2f600dd183a2f96a62b1743e27cb7ec2809b7526ae4e86a07ff6e` |
  | `:/"my dir"` | `:/"my dir"` | `f60323fbf08cc5a9533db2ca50f337cd2ac5205006c43712055a21d8b5986dc4` |

  以下输入应被拒绝：
  - `:/a\b`、`:/"a\b"`：含 `\`；
  - `:/"a "`、`:/" a"`：段首尾有空白；
  - `:/a/../b`、`:/a//b`、`:/`、`:/""`：`..` 段、空段、空 PATH；
  - `:/my dir`、`:/a=b`：未加引号却含非安全字符；
  - `:/web/"@types"`：只给部分段加了引号；
  - `:/"a`：引号未闭合；
  - 引号内含制表符或其他控制字符；
  - `::"a b/"`：选择器末段为空。

### 2.2 语义约束【决策】
以下约束和 `src_paths(F)` 都针对 2.3 规范化之后的 canonical 形式计算。
- **Compose 成员的源路径和输出路径都必须两两不相交。** 按路径段、在组件边界上判定；一个成员的源路径是另一个成员源路径的祖先，也算相交。
  - 依据【代码】：Josh `josh-core/src/filter/tree.rs::compose` 中的 taken 是在**输入侧**去重；`josh-filter/src/opt/prefix_sort.rs` 只在源和目标都不重叠时，才允许重排成员。
  - 只约束输出不相交时的反例：`:[:/a:prefix=x, :/a/b:prefix=y]`。它的正向结果与 Josh 不同；反向时由成员顺序决定哪一份修改生效，另一份会被静默丢弃。
  - 这条判定是**保守的**，只看路径前缀。`:[:/a:exclude[::b/]:prefix=x, :/a/b:prefix=y]` 这类实际上不重叠的写法也会被拒（R8）。
- **源路径集合 `src_paths(F)`【决策】。** 注册时对 canonical 形式静态计算，不读任何树。
  - **记号。** 路径是段序列，`ε` 表示根，存储和展示时写作 `/`。路径 q 的“区域”指 q 处的条目本身（不论类型，也包括“不存在”这一状态）及其全部后代。`a ≼ b` 表示 a 等于 b，或在组件边界上是 b 的祖先。
  - **回拉 `pull(F, q)`。** 输出树中区域 q 的内容只取决于输入树中 `pull(F, q)` 各区域的内容。对路径集合 Q，`pull(F, Q) = ⋃_{q∈Q} pull(F, q)`。各算子的规则如下：

    | 算子 | `pull(op, q)` |
    |---|---|
    | `:nop` | `{q}` |
    | `:empty` | `∅` |
    | `:/p` | `{p/q}`；q = ε 时为 `{p}` |
    | `:prefix=p` | q = p/r（q = p 时 r = ε）：`{r}`；q 是 p 的严格祖先（含 ε）：`{ε}`；二者互不为前缀：`∅` |
    | `:exclude[S]` | `{q}`。选择器是**排除目标，不是源路径**，也不用来缩小结果 |
    | Compose[f₁…fₙ] | `⋃ pull(fᵢ, q)` |
    | Chain[f₁…fₙ] | `pull(f₁, pull(f₂, … pull(fₙ, q)))`，从最后一个算子往前回拉 |

  - **定义。** `src_paths(F) = min(pull(F, {ε}))`。`min` 去掉在集合内另有祖先（≼）的元素，结果是两两不相交的最小前缀集。3.1 的 `src_paths` 列存为 JSON 字符串数组，元素写成 `mega_refs.path` 的形式（`/a/b`，根为 `/`），按 UTF-8 字节序排序。
  - **输出路径。** 本节第一条所说成员的“输出路径”取 `dst_paths(F) = src_paths(invert(F))`（2.5）。两项不相交检查都在该 Compose 节点自己的输入坐标和输出坐标中逐成员进行。
  - **例。**
    - `:exclude[::secret]` → `/`，**不是** `/secret`；`:/a:exclude[::b/]` → `/a`；
    - `:prefix=x` → `/`；`:/a:prefix=x` → `/a`；
    - `:/a:[:/b:prefix=x,:/c:prefix=y]` → `/a/b`、`/a/c`；
    - `:[:/a:prefix=x,:/b:prefix=y]:/x` → `/a`；
    - `:prefix=x:[:/w:prefix=b,:/z:prefix=a]` → `∅`。2.3 的规则不完备，这个过滤器的 canonical 形式不是 `:empty`。
  - **保守且正确。**
    - 读：只要两棵输入树在 `src_paths(F)` 的各区域上相同，`filter_tree(F, ·)` 的结果就相同。逐算子归纳可证。
    - 写：逐算子对照可知，invert(F) 的输入区域 q 只影响输出中 `pull(F, q)` 的各区域。Subdir 与 Prefix 互换、Chain 倒序之后，正好对上表中的规则。2.6 的 `lifted` 和 `covered` 都是 invert(F) 的输出，所以 `T_mono_new` 与 `T_base` 的差异只落在 `src_paths(F)` 的区域内。写入时连带改写祖先 tree、新建祖先目录，不算区域外写入，这一点与 `kind=push` 相同。
    - 因此，5.2 B0 与 6.5 按 src_paths 逐路径授权、5.2 第 7 步的受影响集合 A、第 9 步的 C 段索引，覆盖了 view_push 能改动的全部 mono 路径。src_paths 只会偏大，例如 `:exclude[::a]:/a` 得到 `/a`，而这个视图恒为空。偏大的后果只是授权更严、候选更多。
  - **空集与回退。**
    - `src_paths(F) = ∅` 时按 `:empty` 拒绝注册。否则 6.5 与 5.2 B0 的“逐个源路径授权”一次都不会执行：`push_auth=token` 下不带凭据也能注册，并触发一次 O(H) 冷启动。【代码】凭据只在 `authorize_trunk_api_write` 中检查（`src/api/api_write_auth.rs` 约 L29–50）。
    - 以后新增的算子必须同时给出 pull 规则。给不出时按 `pull(op, q) = {ε}` 处理：src_paths 于是为 `/`，视图不可推送，注册需要覆盖 `/` 的 token。
  - 每个源路径都不能落在 `import_dir` 之下，首段不能是保留名 `.view` / `.filter`（6.1）。src_paths 为 `/` 的只读视图（如 `:exclude[::secret]`）可以注册，但需要覆盖 `/` 的 token（6.5）。
- **Exclude 的参数只能是 `::` 选择器。**
- **可推送视图（P2）的额外约束。** 每个源路径都必须满足：
  - 不等于 `/`；
  - 不是 import_dir 本身，也不是它的祖先或后代；
  - 首段属于 `root_dirs`。

  不满足的视图只能只读注册，`mega_view_filter.push_enabled = false`。这样做是为了让 view_push 的写入面不超过 `kind=push`：不改根级文件，不绕过 `root_dirs` 白名单，不碰 ImportRepo 的命名空间（0.5-24）。

### 2.3 规范化（canonical form）【决策】
按顺序反复应用以下规则，直到结果不再变化。**每条规则都必须语义可靠**；规则集不要求完备。
1. **路径。** 按段比较。`:/` 和 `:prefix=` 的参数在解析时容忍首尾 `/` 并将其去掉。选择器 `::p/` 末尾的 `/` 是语法记号，不参与规范化。
2. **合并同类。** `:/a:/b` → `:/a/b`；`:prefix=a:prefix=b` → `:prefix=b/a`（先挂到 a 下再挂到 b 下，等于挂到 b/a 下）。依据【代码】：Josh `opt/simplify.rs`。
3. **Prefix 后接 Subdir**，即 `:prefix=p:/q`，按段比较（依据【代码】：Josh `opt/step.rs`）：
   - q == p → `:nop`
   - p = q/r → `:prefix=r`（v0.1 中 `:prefix=x/y:/x → :prefix=y` 就是这种情形）
   - q = p/r → `:/r`
   - 两者互不为前缀 → `:empty`
4. **空与恒等。** 本条的可靠性依赖 4.1 的空树规范：P0 中没有任何算子能从空树生成内容。
   - Chain 中去掉 `:nop`；Chain 中只要出现 `:empty`，整条链就是 `:empty`。
   - Compose 中去掉 `:empty` 成员。去掉后没有成员，结果是 `:empty`；只剩一个成员，退化为该成员。
   - Compose 中的 `:nop` 成员**不能**去掉：它是恒等映射，会与其他成员的源路径相交，由 2.2 拒绝。
5. **Compose 展开与排序。** 嵌套 Compose 展开。重复成员必然源路径相交，由 2.2 拒绝。剩下的成员按 canonical 文本的 UTF-8 字节序（2.1）排序；在 2.2 的约束下，成员顺序与语义无关。
6. **Exclude 选择器。** 去重，并按 canonical 文本的 UTF-8 字节序（2.1）排序；选择器列表为空时，去掉这个 Exclude。

**不完备之处。** v1 不做 Exclude 与 Subdir/Prefix 之间的交换。例如 `:/a:exclude[::b/]` 与 `:exclude[::a/b/]:/a` 语义相同，规范化后的文本却不同，所以"语义相同"不保证"filter_id 相同"。在附录 E 启用之前（根链线性），这只会带来重复缓存和重复冷启动，因为视图提交的哈希只取决于复合 tree 函数和第 4 节的规则，与 filter_id 无关：语义等价的两个过滤器投影出的提交逐字节相同。启用附录 E 之后这一性质不再成立，到时 filter_id 要编码 Chain 的结构，算法版本也要升级。

### 2.4 filter_id【决策】
```
filter_id = SHA-256( "mega-view-filter/v1\n" || canonical_text )
# canonical_text：2.1 canonical 打印结果的 UTF-8 字节，不含末尾换行；展示为 64 位小写 hex
```
- `v1` 同时冻结以下全部规则：过滤器文本的字节契约（2.1 的段规则、安全字符集、引号与转义、canonical 打印与 UTF-8 字节序排序，以及 2.1 的黄金向量）、2.3 的规范化规则集、签名头剥离范围（4.2）、tree 排序、EMPTY_TREE 与空子树剪枝（4.1 空树规范）、线性投影规则（4.3），以及 L0 tree 的解析与序列化行为（4.1 条目复制）。其中任何一项改变，都必须升到 `v2`，旧视图保持不变；或者强制重建受影响的视图并发布公告。前两项一旦改变，同一输入在升级前后会得到不同的 filter_id：已有的 `/.filter/<id>.git` 克隆需要重新 clone（R3），2.1 的加载复核也会把旧定义判为损坏。
- 存储时用整数代理键 `mega_view_filter.id`（下称 `filter_pk`），其他表只存代理键。与存 65 B 的 hex 文本相比，每行约省 57 B。
- `sha2` 与 `hex` 已经是直接依赖。
- 不需要与 Josh 的 filter id 兼容（Josh 的 id 是过滤器结构序列化成 git tree 后的 OID），只保持文本语法兼容。

### 2.5 逆运算（依据【代码】：`josh-filter/src/opt/invert.rs::invert`）

| 过滤器 F | invert(F) |
|---|---|
| `:/p` | `:prefix=p` |
| `:prefix=p` | `:/p` |
| `:nop` / `:empty` | 不变 |
| Chain[a, b, …, z] | Chain[inv z, …, inv b, inv a] |
| Compose[a, b, …] | Compose[inv a, inv b, …] |
| `:exclude[S]` | `:exclude[S]`（**选择器不改写**） |

**【代码】** Josh 的实现是 `Op::Exclude(f) => Op::Exclude(invert(f))`，`::` 选择器本身是自逆的。Chain 求逆时整条链倒序，Exclude 自然落在视图坐标系里。

例：`F = :/a:exclude[::b/]`，则 `invert(F) = :exclude[::b/]:prefix=a`。

v0.1 写的是"S 中的路径要加上或去掉前缀"，按它会得到 `:exclude[::a/b/]:prefix=a`。这个 exclude 在视图坐标系里匹配不到任何东西，lifted 就不会剔除 `b/`，推送者因此能写进被隐藏的 `a/b`。这是安全缺陷，本版已更正。

### 2.6 tree 级逆向 unapply（依据【代码】：`josh-core/src/filter/mod.rs::unapply`；P2 使用）
给定过滤器 F、视图侧的新树 `T_v` 和 mono 侧的基准树 `T_base`：
```
inv        = invert(F)
covered    = apply(inv, apply(F, T_base))     # 基准树中被 F 覆盖的部分（mono 坐标）
stripped   = subtract(T_base, covered)
lifted     = apply(inv, T_v)
T_mono_new = overlay(lifted, stripped)        # overlay 冲突时 lifted 胜出
```
- `subtract` 按路径删除（Josh `tree.rs::subtract_inner`）：名字出现在第二个参数中就删除；blob 不比较 oid；tree 递归处理；结果为空时整个条目删除。所以视图侧的删除能传播到 mono 侧。
- `subtract` 与 `overlay` 遇到 tree id 相同的条目时整条处理，不再递归。
- 以上运算都遵守 4.1 的空树规范。
- P0 子集中的过滤器都能整体求逆。

### 2.7 反向安全校验（P2 推送必做）【决策】
单靠 unapply 公式保护不了视图之外的内容。B3 在锁内，针对锁内读到的根树 `T_base = tree(R)` 做以下三项校验，任一项失败就拒绝推送。
1. **视图外不变。** 令 `outside(T) = subtract(T, apply(inv, apply(F, T)))`，要求 `outside(T_mono_new) == outside(T_base)`。
   - 这一项挡住"同名文件覆盖被隐藏的目录"：`:exclude[::h/]` 只匹配 tree。推送者在 h 处放一个文件，overlay 就会用这个 blob 覆盖整个被隐藏的 `h/`，而且正向重投影的结果仍然等于 `T_v`，第 2 项发现不了。
   - **授权面复核（纵深防御）。** 令 `X = :exclude[::s₁,…,::s_k]`，sᵢ 取遍 `src_paths(F)`。另外要求 `filter_tree(X, T_mono_new) == filter_tree(X, T_base)`。按 2.2 的论证，这一项恒成立；如果不成立，说明 src_paths 的推导或 unapply 的实现有缺陷，此时 fail-closed、告警并拒绝推送。有了这一项，授权范围不再只依赖静态推导的正确性。代价是两侧各重写 O(k × 深度) 个脊柱 tree。
2. **重投影一致。** 要求 `apply(F, T_mono_new) == T_v`；不一致时经 sideband 列出落在视图值域之外的路径。
   - 这一项挡住"视图树中含有 F 值域之外的内容"，例如在 Compose 视图根上新建的文件，或放在被 exclude 位置下的文件。这些内容会被 `lifted` 静默丢掉。
3. **路径策略等价。** 令 D 为 `tree(R)` 与 `T_mono_new` 之间变更路径的集合：
   - D 中任何路径都必须通过 `path_policy::check_write_operands`（ADR-FU-06）；
   - 源路径本身不能被删除，也不能变成非 tree：trunk 规定删除必须在父路径推送；
   - 源路径在 `tree(R)` 中不存在、在 `T_mono_new` 中出现时，先执行 `classify_creation_path`，即 root_dirs 白名单与创建策略（0.5-24）。

---

## 3. 数据模型

**持久数据与派生数据【决策】：**
- **持久定义**，不得清空：3.1 中的定义列（`id, filter_id, canonical_spec, algo_version, object_format, src_paths, push_enabled, created_at`）、3.2 整表、3.6 整表。
- **派生缓存**，可以清空重建：3.1 中的状态列（`projected_seq, ready_seq, last_access_at, warming_since`），以及 3.3、3.4、3.5 的全部内容。清空后 `projected_seq` 归零，`ready_seq` 与 `warming_since` 置为 NULL；`last_access_at` 保持原值（4.6"闲置回收"）。
- **运行态**：`mega_view_register_log`（6.5），只用于注册速率计数，可以随时清空，清空的唯一后果是重置各 token 的速率窗口。
- **实现位置。**
  - 实体放在 `src/callisto/<table>.rs`。
  - 迁移放在 `src/jupiter/migration/`，只前进，写 raw DDL 加 `IF NOT EXISTS`，在 `Migrator::migrations()` 中注册。测试 `import_repo_alias_rows_canonicalized` 断言了最后三个迁移名，需同步更新。
  - 存储包装写在 `src/jupiter/storage/view_storage.rs`，由 `Storage` 聚合。
- **哈希列的类型。** 沿用 mega2 惯例存 hex TEXT，与 `mega_commit.commit_id` 同型，关联查询时不用转换。

### 3.1 `mega_view_filter`：过滤器定义
| 列 | 类型 | 说明 |
|---|---|---|
| id | BIGINT **PK** | `generate_id()`；即 `filter_pk` |
| filter_id | TEXT UNIQUE | 2.4 的 hex 哈希 |
| canonical_spec | TEXT NOT NULL | 规范化文本 |
| algo_version | SMALLINT NOT NULL | =1 |
| object_format | TEXT NOT NULL | 注册时的部署取值；与当前部署不一致时拒绝服务（防御性校验） |
| src_paths | JSONB NOT NULL | 2.2 静态计算出的源路径集合 |
| push_enabled | BOOL NOT NULL | 是否满足 2.2 可推送视图的约束 |
| projected_seq | BIGINT NOT NULL DEFAULT 0 | 投影水位：根链上已投影到的 seq（4.4） |
| ready_seq | BIGINT NULL | 首次追平时写入，前提是根链已覆盖同一事务内读到的 `main@/`；NULL 表示"未就绪"（4.4） |
| warming_since | TIMESTAMP NULL | 冷启动名额（6.5）。准入成功时写入 `now()`；视图首次就绪的那个 catch_up 批事务内清空（4.4）；回收与 rebuild 时清空（4.6）。非 NULL 即占用一个名额 |
| last_access_at | TIMESTAMP NULL | 最近一次访问时刻，供 P1 的闲置回收使用。P0 不写入，恒为 NULL。写入来源、节流、NULL 语义与回收复查见 4.6"闲置回收（P1）" |
| created_at | TIMESTAMP | |

### 3.2 `mega_view`：命名视图
| 列 | 类型 | 说明 |
|---|---|---|
| id | BIGINT PK | |
| name | TEXT NOT NULL | 段规则与 PATH 相同（2.1），但不接受引号，段内只允许 `A–Z a–z 0–9 . _ -`，末段不得以 `.git` 结尾；例如 `agent/task-1234`。视图名会出现在 URL 中（6.1）：非 ASCII 字符和 `"` 会被百分号编码，形成第二种文本表示；URL 尾部的 `.git` 可以省略，以 `.git` 结尾的名字会有两种解读。`@` 不在允许的字符中，专用于分隔版本号。 |
| version | INT NOT NULL | 从 1 开始递增 |
| filter_pk | BIGINT → mega_view_filter.id | |
| created_by | TEXT | 认证身份（storage-only 下为 token 名） |
| created_at | TIMESTAMP | |
| UNIQUE(name, version)；INDEX(filter_pk) | | |

视图定义不可变：修改定义就是新建一个 version，对应一个新的 filter_id。版本切换的后果见 6.1。

### 3.3 `mega_view_root_chain`：根链序号（所有视图共享）【决策】
| 列 | 类型 | 说明 |
|---|---|---|
| seq | BIGINT **PK** | 根链的第一个提交为 1 |
| commit_id | TEXT UNIQUE | |
| tree_id | TEXT NOT NULL | 冗余存储，避免投影时再读一次提交 |

回走暂存表 **`mega_view_root_chain_scan`**（派生缓存，与根链表一起清空重建）：

| 列 | 类型 | 说明 |
|---|---|---|
| pos | BIGINT **PK** | 回走序号。1 是本次扫描的起点 h0，即起扫时读到的 `main@/`；沿首父链向回递增 |
| commit_id | TEXT NOT NULL | |
| tree_id | TEXT NOT NULL | |
| parent_count | SMALLINT NOT NULL | 用来区分无父与多父 |
| first_parent | TEXT NULL | 无父时为 NULL |

这张表保存已经回走、还没接入根链的提交，既是续扫状态，也是"不连续"结论的证据。它只在持有根链锁的事务内写入。

- **用途。** 记录 `main@/` 的首父链。在 G1 的线性、连续前提下，seq 就是代数：对链上任意 a、b，`is_ancestor(a, b) ⇔ seq(a) ≤ seq(b)`。这张表取代了 v0.1 计划给 `mega_commit` 加的 `generation` 列，不改动热表。I6 保证 `push_queue.id` 与根 roll-up 同序，可以用来交叉校验。
- **定位。** 它是派生缓存：不写根树，也不写路径 ref，所以不是 I5 意义上的写入者。
- **连续性与线性校验【决策】。** 根链表的行必须满足：
  - `seq = 1` 的提交没有父提交。【代码】初始化根提交以空父列表构造（`src/jupiter/utils/converter.rs::MegaModelConverter::init` 约 L732），由 `initialize_monorepo` 在初始化专属锁下写入（0.1-2）。
  - `seq > 1` 的提交恰有一个父提交，且它等于 `seq − 1` 行的 `commit_id`。

  由此，根链表是**首父闭包**的：表中任一提交的全部祖先都在表中，且 seq 更小。下面的追加算法依据这一性质判定祖先关系。回走时要区分三种情形：`parents_id` 为空，表示到达根；首父 id 在 `mega_commit` 中查不到，表示数据缺失，不能当作根；父提交多于一个，表示非线性（R11）。插入时如果遇到主键冲突，要核对已有行的 `commit_id` 与本次相同。
- **追加算法 `extend_root_chain(budget)`。** 后台 worker（C 段信号与补偿任务）、advertise 的同步追赶和 B0 的滞后预检共用同一个实现。结果是以下三者之一："已追平"、"未追上"、"不连续（原因）"。
  1. **取锁。** 每一步都是一个短事务，开头取 `pg_try_advisory_xact_lock(VIEW_LOCK_NS, ROOT_CHAIN_KEY)`，锁键方案见 4.4。根链表与暂存表都只在这把锁下写入，所以同一事务内的成员检查与链尾读取是一致的。
     - 后台 worker 抢锁失败时直接返回，不留标记。持锁者读到 h0 之后才落地的根提交，最迟由下一次周期补偿（4.4 触发方式第 2 条）接入，额外滞后不超过一个 `worker_interval_secs`。
     - advertise 与 B0 抢锁失败时，用带 `lock_timeout` 的阻塞锁等待，超时按"未追上"处理。
  2. **起扫。**
     - 暂存表为空时，在事务内读取 `main@/`，记为 h0。h0 等于链尾就返回"已追平"，否则写入 pos=1 的行。
     - 暂存表为空、调用方不设预算、链尾存在、h0 恰有一个父提交且该父提交等于当前链尾时，可在这笔持锁事务内直接以 `seq(链尾)+1` 接入 h0，随后提交、清除断链告警并返回"已追平"。h0 与链尾都在取锁之后的同一事务内读取。这条单提交快路同时核对父链与链尾，不根据无条件的 `max(seq)+1` 猜测接入位置；插入的 `SELECT` 同时检查链尾提交行仍在 `mega_commit`，并使用 `ON CONFLICT DO NOTHING`。因父行缺失或唯一键冲突使影响行数为 0 时，在同一事务内继续写 pos=1 的暂存行，由原算法判定并保留暂存证据，不能把不一致的行视为成功。带预算的调用保持原有回走与接入计数，不走快路；暂存表非空、多父、缺父、回滚及分叉仍走原算法。
     - 暂存表非空时，从 pos 最大的行接着扫，不重读 `main@/`。扫描期间根的前进，留给下一次扫描。
  3. **判定。** 取暂存表中 pos 最大的行 c，即回走到的最早提交：
     - c 在根链表中，且等于链尾：**连上**，转第 5 步。
     - c 在根链表中，但不是链尾：**不连续**。c 就是 h0 时，属于根被回滚；否则属于分叉，即 h0 的首父链在 c 处离开了根链，例如回滚后又有新推送，或者根路径 CL 的合并让旧 tip 离开了首父链（0.5-23）。
     - c 无父：根链表为空时，c 是冷启动的起点，**连上**；根链表非空时，h0 属于一段与根链无关的历史（例如根 ref 被替换），**不连续**。
     - c 有多个父提交：**不连续**（非线性，R11）。
     - c 的首父在 `mega_commit` 中查不到：**不连续**（数据缺失）。
     - 其余情形，即 c 不在表中、恰有一个父提交：未终止，转第 4 步。
  4. **分批回走。** 从 c 的首父起，用一条递归 CTE 沿 `mega_commit.parents_id->>0` 往回取，至多 `views.batch_size` 个提交。遇到第一个"在根链表中、无父、多父或首父缺失"的提交就停下，这个提交也写入暂存表，然后回到第 3 步。
     - 同一批暂存行以每条至多 1000 行的批量 SQL 写入，按 pos 顺序保留 `ON CONFLICT DO NOTHING` 的原有幂等语义；每行 5 个参数，因此每条最多 5000 个参数，低于 Postgres 的 65535 上限。即使内部调用绕过配置层的 `views.batch_size ≤ 10000` 校验，分块仍保证参数上限。
     - 预算按本次调用处理的提交数计算，回走与第 5 步的接入合计。advertise 与 B0 的预算取 `views.max_append_walk`。
     - 预算用尽就返回"未追上"。这只表示本次的工作量用完了，**不是**不连续判定：暂存行保留，由下一次调用或后台 worker 接着扫。
     - 后台 worker 不设预算，一批接一批扫到终止，或者扫到进程退出为止。
  5. **接入。** 按 pos 从大到小（即 seq 递增）把暂存行插入根链表。每段不超过 10 000 行，每段一个事务。根链锁是事务级锁（第 1 步），段与段之间会释放，期间进程可能退出，其他调用方也可能取得锁，所以每个段事务都自成一体，不沿用上一段的判定：
     - **重新判定。** 段事务按第 1 步取根链锁，重读链尾，取暂存表中 pos 最大的行作为锚点 c，在本事务内重做第 3 步。只有结论为"连上"才继续，其余结论按第 3 步处理。"连上"本身就保证待插入的第一行的首父等于链尾；冷启动时，它保证根链表为空。
     - **插入。** 设 c 的 pos 为 P。
       - 一段内的行以每条至多 1000 行的批量 SQL 写入，每行 3 个参数。部分行因唯一键冲突未插入时，按原有规则核对已存在行的 seq 与 commit_id；不一致仍为"不连续（RowConflict）"，整段事务回滚。段至多 10000 行，冲突核对的 2 × 10000 个参数也低于 Postgres 上限。
       - 接在链尾之后时（c 就是链尾），本段从 pos = P − 1 起向下插入，pos = p 的行得到 seq = seq(c) + (P − p)。
       - 冷启动时，本段从 c 自身起插入，pos = p 的行得到 seq = P − p + 1，c 得到 seq = 1。
     - **换锚点。** 设本段插入的最小 pos 为 p_min。在**同一事务内**删除暂存表中 pos > p_min 的全部行，包括原锚点 c（冷启动时 c 已在本段接入，同样删除），只留下 pos = p_min 的行，即本段的新链尾；p_min = 1 时清空暂存表。插入与删除必须一起提交，不得拆成两个事务。

     **锚点不变式【决策】。** 每个段事务提交后，暂存表要么为空，要么 pos 最大的行恰好是根链表的链尾，其余行都是尚未接入、pos 更小的提交。预算在段间用尽时，直接返回"未追上"，由这条不变式保证下一次调用可以续接。

     **续扫为何不会误判。** 根链表与暂存表只在根链锁下由本算法写入，段事务又把"接入一段"与"锚点换成新链尾"原子地提交。因此之后任何取得根链锁的调用方（进程重启后的本副本、其他副本的 worker、阻塞等锁的 advertise 与 B0）在第 3 步看到的 pos 最大的行只有三种：
       - 回走前沿：转第 4 步；
       - 第 4 步停下的终止提交：结论与上次相同；
       - 当前链尾：判为"连上"，从 pos 更小的行接着接入。

     所以"在根链表中但不是链尾"仍然只表示真实的回滚或分叉，不会因为段间退出或段间换手而出现。不持根链锁的读者（5.2 第 2 步的单条 SQL、4.4 的 catch_up）按快照读根链表，只会看到某个段事务提交之前或之后的状态。两种状态都满足首父闭包，回走遇到的第一个表内提交都是当时的链尾，所以也不会误判。

     全部接入后返回"已追平"。如果这时 `main@/` 又前进了，下一次调用从新的 h0 开始扫描。
  6. **只按 seq 递增插入。** 禁止不核对首父与链尾就用 `max(seq)+1` 追加单个提交；第 2 步的快路满足首父闭包并持同一把根链锁。
- **调用方如何处理"未追上"。** 只读且已就绪的视图，照常广告已投影到的最新 tip，并记录滞后（4.4）；可推送视图的 advertise 与 B0 返回可重试错误。三者都发 4.4 的进程内信号，唤醒本副本的后台 worker 从暂存表接着扫描（worker 不受预算限制）。
- **为什么不用走回根就能判定祖先关系。** 根链表是首父闭包的，所以从 h0 回走遇到的第一个表内提交 c，就是 h0 与链尾的最近公共祖先：c 等于链尾，当且仅当链尾是 h0 的祖先。回走长度就是 h0 到这个公共祖先的距离。
  - 合法滞后时，它等于需要接入的提交数，这部分工作本来就要做；
  - 回滚或分叉时，它等于分叉之后新增的根提交数，与历史长度无关；
  - 只有"h0 属于无关历史"这一种情形会一直走到 h0 的根，代价是一次 O(depth(h0)) 的分批回走；结论随即由暂存行固定下来，不会重复付出。
- **并发。** 多个 worker 与同步调用方共享同一份暂存状态，而且只在根链锁下推进，所以不会出现"两个 worker 各持一份列表、分别读自不同的 `main@/`"的情形。段与段之间换手时结果仍然正确，这由第 5 步的锚点不变式保证。
- **不连续之后。** 第 3 步得出的任一"不连续"结论，以及"连续性与线性校验"中的任一项不满足，都说明 G1 的前提被破坏。
  - 一旦发生，停止根链与全部视图的追赶，视图返回 503 并告警（R11）。
  - 暂存表不清空：pos 最大的行就是证据。之后每次调用都在第 3 步用 O(1) 次查询复现同一结论，进程重启后、其他副本上也一致。不持根链锁的读者用谓词 `root_chain_halted` 判定是否停追。取暂存表中 pos 最大的行 c，满足以下任一条件即为真：c 在根链表中但不是链尾；c 无父且根链表非空；c 的 `parent_count > 1`；c 的首父不在 `mega_commit` 中。这个谓词可以并入读者的单条 SQL 求值（4.6）。
  - 不得自动从新的 tip 重建，去覆盖已经发布的映射。
  - 运维确认原因后，有两种恢复方式：
    - `main@/` 已恢复为链尾的后代：清空暂存表即可，下一次调用按当前的 `main@/` 重新判定；
    - 否则执行 rebuild。P1 为 `mega2 view rebuild`，按 4.6 的全局 rebuild 执行；P0 为运维步骤，必须在所有副本停服后执行：清空 3.3–3.5 的派生表，并把 3.1 的状态列置为回收态；之后由再次注册（P0）或读者访问（6.1 第 3 层）按 6.5 准入重新预热。rebuild 会改变已发布的视图历史，需要公告（R3）。
- **冷启动的取数方式。** 冷启动就是根链表为空时的扫描，走同一套第 2–5 步：每批一条递归 CTE，结果暂存在表中，内存占用 O(batch_size)，中途重启可以续扫。【推断：吞吐需要实测】

### 3.4 `mega_view_commit_map`：游程式正向映射
| 列 | 类型 | 说明 |
|---|---|---|
| filter_pk | BIGINT | |
| seq_from | BIGINT | 本段起点（根链 seq） |
| view_commit | TEXT NULL | NULL 表示本段投影为空，视图尚无提交 |
| view_tree | TEXT NOT NULL | 空段为 EMPTY_TREE |
| **PK (filter_pk, seq_from)** | | |
| UNIQUE (filter_pk, view_commit) | | 供 want 校验与 REST 反查；NULL 不参与唯一性判断 |

- **语义。** 根链上 seq ∈ [seq_from, 下一行的 seq_from) 的提交都投影为 `view_commit`。被丢弃的提交不单独存行，只延长所在的区间。查询 `map(F, s)` 就是取 `seq_from ≤ s` 的最大一行；没有这样的行时，`map(F, s) := (NULL, EMPTY_TREE)`。
- **行数。** 等于该视图的 NEW 提交数，再加至多一个起始空段。v0.1 的全量映射和 P2 稀疏方案都由此取代。

### 3.5 `mega_view_object` 与 `mega_view_object_ref`：投影生成的对象
| 列 | 类型 | 说明 |
|---|---|---|
| object_id | TEXT **PK** | |
| kind | SMALLINT | 1=commit、2=tree |
| data | BYTEA | **原始 git 对象内容**（不含对象头），保证哈希可复算 |
| created_at | TIMESTAMP | |
| gc_marked_at | TIMESTAMP NULL | 清扫标记：清扫首次发现该对象没有引用行的时刻。catch_up 为它写引用行时，在同一批事务内清空（4.4）；清扫发现它有引用行时也清空（4.6） |

`mega_view_object_ref` 的列为 `(filter_pk, object_id)`，**PK** 即为这两列；另建 INDEX(object_id)，供清扫按对象判定有无引用。

- **只存投影生成的新对象**：所有视图提交，以及 Prefix/Exclude/Compose 新建的"脊柱" tree，也就是被重写的祖先 tree。纯 Subdir 视图的 tree 都是 `mega_tree` 中已有的子树，不写入这里；blob 永远不会被合成。
- **引用不变式。** 视图 F 的任何视图提交，其 tree 闭包中凡是属于 `mega_view_object` 的对象（包括 EMPTY_TREE），都必须有一行 `(F.pk, object_id)` 引用。
  - 这类对象只有脊柱 tree，数量是 O(深度 × 成员数)。
  - catch_up 每写出一个 NEW 视图提交，就为它的提交对象和脊柱 tree 批量 upsert 引用行，不论这些对象是新建的、memo 命中的还是已经存在的。memo 只缓存计算结果，不能代替引用行。
- **GC。** 回收一个视图只删除它的 commit_map 行与引用行，不直接删除对象。对象由清扫任务按 4.6 先标记、后删除，宽限期从 `gc_marked_at` 起算，不从 `created_at` 起算。【代码】`artifacts_gc` 只能作为"无引用 + 宽限期"的先例：它的宽限期基于每次登记都会刷新的 `last_seen_at`，引用复查与删行分两步执行，中间没有互斥（`src/jupiter/service/artifact_service.rs` 约 L860–921，`src/jupiter/storage/artifact_storage.rs` 约 L278–314）。本表会被 catch_up 并发补写引用行，不能照搬。
- **读取。** 视图对象优先，其余 tree 回落到 `mega_tree`。
- **可选优化【推断】。** 视图提交的字节可以不持久化：由 `root_chain[seq_from]`、`view_tree` 和上一段的 `view_commit` 经 `rewrite_commit` 按需重算，`mega_view_object` 只存 tree。是否采用由 P1 实测决定（7.2）。

### 3.6 `mega_view_pushed_commit`：客户端推送的视图提交（P2，持久）
| 列 | 类型 | 说明 |
|---|---|---|
| filter_pk | BIGINT NOT NULL → mega_view_filter.id | 经由哪个视图收到 |
| object_id | TEXT NOT NULL | 客户端推上来的视图提交 id（视图坐标） |
| data | BYTEA NOT NULL | 原始提交字节（不含对象头）。直接取 pack 解码结果，不经 `Commit::from_bytes` / `to_data` 往返 |
| last_seen_at | TIMESTAMP NOT NULL | A 段每次写入（含主键冲突）都刷新 |
| **PK (filter_pk, object_id)**；INDEX(object_id) | | |

- **为什么用复合主键【决策】。** 同一个提交对象可能经由多个视图推送进来。
  - 语义等价而 filter_id 不同的过滤器，在线性根链上会投影出逐字节相同的视图提交（2.3）。
  - 不等价的过滤器也可能如此：只要两者的差异部分在历史上始终为空（例如 `:/a` 与 `:/a:exclude[::x]`，而 `a/x` 从未出现），投影就逐字节相同。
  - 客户端在同一个 old_v 上做出的同一个提交，可以先后推给这两个视图。如果只以 object_id 为主键，第二个视图在 A 段写入时，要么主键冲突，要么沿用第一行的 filter_pk；无论哪种，按视图查找（5.2 B0 的 `CommitSource`、6.6 的 `commits/{id}`）都会落空。
- **写入。** A 段按 `ON CONFLICT (filter_pk, object_id) DO UPDATE SET last_seen_at = now()` 写入，`data` 不变。object_id 由 pack 解码时按原始字节计算，因此同一 object_id 在不同视图下的 data 必然相同，不必另做字节比对。
- **读取。** ViewRepo 的 `CommitSource` 和 6.6 都按 `(F.pk, id)` 查询；GC 这类只按 id 的扫描走 `INDEX(object_id)`。
- **内容寻址，不按 `push_queue.id` 归属。** reaper 终态化一行后，B1 会复制 payload 新建一行；新行的 filter_pk 不变，仍能找到这些对象。
- **GC root。** 所有 `view_push` 行（Queued、Running、Done，以及还可重试的 Failed）的 payload 中，`chain` 与 `new_v` 引用的提交以 `(该行的 filter_pk, id)` 作为 GC root（I4）。
  - `old_v` 是投影生成的视图提交（5.1），不在本表中。B3 与 PushChain 都只比较它的 id，不读它的对象。old_v 在途时所依赖的派生状态，按 4.6 保护；落地之后，由投影的确定性保证它可以重算。
  - 落地成功后的提交永久保留，因为根提交 message 中的枚举引用了这些 id，并由 `Mono-View` 指明所属视图（5.2 第 6 步）。
  - 残留行由一条 DELETE 回收：`last_seen_at < now() − gc_grace_secs` 与"不被任何同 filter_pk 的 GC root 行引用"两个条件写在同一条语句中。【推断】A 段刷新 `last_seen_at` 时持有该行的行锁，并发的 DELETE 会等它提交，再按新的 `last_seen_at` 重新求值，从而跳过该行。所以"残留行已过宽限期、客户端又重推同一对象、B0 还没入队"这个窗口不会造成误删；`DO NOTHING` 加 `created_at` 的写法挡不住这个窗口。
- **派生缓存的 rebuild 和 GC 都不触碰这张表。**

### 3.7 不建的表（相对 v0.1）
- `mega_view_tree_cache`：P0 不建持久化 tree 缓存。根链上每个提交都会改变根树，以根树为键的备忘几乎不会命中；`commit_map.view_tree` 已经够用。中间层的复用交给进程内有界 memo，不新增依赖；如需 LRU 类依赖，按 AGENTS.md 论证。P2 视实测再决定。
- `mega_view_ref`：视图 tip 由 `projected_seq` 加 commit_map 给出（4.5）。
- `mega_view_push_map`：P2 推送的幂等复用 `push_queue` 现有的收养/重放机制（5.2）。

### 3.8 对现有表的改动
- P0–P1：**不改**现有表。
- P2：
  - `push_queue.kind` 枚举新增 `view_push`（`ALTER TYPE … ADD VALUE`）；
  - `push_queue` 新增可空列 `filter_pk`，并建部分唯一索引：`WHERE kind = 'view_push' AND status IN ('Queued','Running')`。同一视图同一时刻只允许一个推送在途，与 ADR-TP-10 的理由相同。不能用 `path` 作键，因为 LCP 会被无关的视图共享；
  - payload 加入 `filter_pk / old_v / new_v / chain`。
  - **迁移拆分**：`ALTER TYPE push_queue_kind_enum ADD VALUE IF NOT EXISTS 'view_push'` 单独放在一个迁移里；`filter_pk` 列和引用 `'view_push'` 的部分唯一索引放在后续迁移中。原因是 Postgres 不允许在同一事务内使用新增的枚举值，而 sea-orm-migration 在 Postgres 上默认每个迁移一个事务。
- 任何阶段都不改 `mega_commit`、`mega_refs`、`mega_cl` 和 `mega_cl_commits`。

---

## 4. 正向投影算法

### 4.1 tree 过滤 `filter_tree(F, T)`
```
fn filter_tree(F, T) -> Result<TreeId, MissingObject>:
    if let Some(r) = memo.get(F.node, T): return Ok(r)             # 进程内有界 memo；只缓存成功结果
    r = match F.op:
        Nop        => T
        Empty      => EMPTY_TREE
        Subdir(p)  => match lookup_path(T, p)? { Tree(id) => id, Absent | NotTree => EMPTY_TREE }
        Prefix(p)  => if T == EMPTY_TREE { EMPTY_TREE } else { wrap(T, p) }   # 新建 |p| 个脊柱 tree；不读 T
        Exclude(S) => subtract_paths(T, S)?         # 只重写 S 中各路径的祖先 tree
        Compose(fs)=> overlay_disjoint([filter_tree(f, T)? for f in fs])?
        Chain(fs)  => try_fold(fs, T, filter_tree)?
    return Ok(r)
```
**空树规范（冻结进 v1）【决策】**，与 Josh 一致（被重写层的兄弟空树条目除外，见 §7.3 R8）【代码】：Josh `tree.rs::replace_child_inner` 中有 `remove = oid == null || oid == empty_id()`，`subtract_inner` 在结果为空时删除整个条目。规则如下：
- 任何新建的 tree 都不写入指向 EMPTY_TREE 的条目。某一层在删除或替换之后没有条目了，该层的结果就是 EMPTY_TREE，同时在父层删掉对应条目，逐级向上传递。
- `Prefix(p)` 作用于 EMPTY_TREE 时，结果是 EMPTY_TREE。
- `Exclude(S)` 删掉命中的条目后，按上一条剪掉被删空的祖先目录；根被删空时返回 EMPTY_TREE。
- `Compose` 的所有成员都是 EMPTY_TREE 时，结果是 EMPTY_TREE。
- 2.6 中的 `lifted`、`stripped`、`T_mono_new` 同样遵守本规范。

**缺对象不等于路径不存在【决策】。**
- **读取原语。** `filter_tree`、`lookup_path`、`subtract_paths`、`overlay_disjoint` 与 4.3 的 `is_empty_root`，都只经由 `read_tree(id) -> Result<Tree, MissingObject>` 读 tree：先查 `mega_view_object`，其余查 `mega_tree`。行不存在、字节无法解析，都返回 `MissingObject { tree_id }`，告警中注明属于哪一种。解析用 `ObjectHash::from_hex_for_kind(kind, tree_id)?` 加 `<Tree as ObjectTrait>::from_bytes`：它出错时返回 Err，按 id 的 kind 切分条目（git-internal `tree.rs` 约 L408–411）。不用 `Tree::from_mega_model`，它会 panic，且依赖 thread-local kind（`converter.rs` 约 L857–863）。
- **读取原语的细化（HP-04）。** 「未预取」是除「行不存在」「字节无法解析」外的第三种 `MissingObject` 原因，表示 TreeSource 没有加载该 id；读取同步且不访问数据库（ADR-HP-02），它不表示库中没有该行，不属于本节所说的缺对象，不进入缺失集合，调用方不得据此按 §4.4 把水位停在 s − 1。`read_tree(EMPTY_TREE)` 不经 TreeSource，直接返回空条目的 tree，即使没有对应行也不返回 `MissingObject`。本批缺失集合中的 id 是「行不存在」；id 长度与 kind 不符、`from_bytes` 失败或解析出的 tree 含同名条目（只按名字判定，不论 mode）都是「字节无法解析」，所有读取共用该解析函数，所以 `lookup_path` 与各算子对同名条目的结论相同。
- **只有两种情形算路径不存在**，按 EMPTY_TREE 处理：路径上某一层的 tree 中确实没有该名字的条目；条目存在，但不是 tree（blob、symlink、gitlink）。条目存在且 mode 为 tree、它指向的 tree 却读不到时，返回 `MissingObject`，不得返回 EMPTY_TREE。根树 T 本身读不到，或者 Exclude、Compose 需要重写的祖先 tree 读不到，同样返回 `MissingObject`。`lookup_path` 因此返回 `Result<Tree(id) | Absent | NotTree, MissingObject>`。
- **只检查计算需要读取的 tree。** 原样复用、不需要读取的子树在投影时不检查，与下文“原样复用的 L0 tree”一致。这类子树包括 Subdir 的结果 tree、Prefix 包住的 T，以及 Exclude/Compose 没有触及的子树；它们缺失时，由 6.3 在打包时报错。视图提交的哈希只依赖这些子树的 id，所以确定性不受影响。memo 不缓存 `MissingObject`。
- **批量读取。** 本节要求新增的可失败批量读取，只在查询本身失败时返回 `Err`。查不到的 id 由调用方按“请求集减返回集”得出，存储层不判错。4.4 的按层预取把这些 id 记入本批的缺失集合，不让整批失败；`read_tree` 命中缺失集合时返回 `MissingObject`。6.3 的 ViewRepo 读取发现缺失集合非空即报错。
- 【代码】现有读取的语义不能沿用：`resolve_path_tree_hash_in_txn` 对“条目不存在”“不是 tree”“子 tree 行查不到”一律返回 `None`（`mono_storage.rs` 约 L1665–1692，查不到子 tree 行在约 L1684–1689）；`get_trees_by_hashes` 用 `is_in` 查询，静默略去查不到的 id（约 L1696–1705）；默认 `traverse_for_count` 只遍历查到的行（`pack/mod.rs` 约 L415–419）。`push_queue_service.rs::import_leaf_in_txn`（约 L235–272）区分 `Absent`、`NotDirectory` 与 “tree … not found” 错误，可作先例。
- 【代码】缺行确有现实来源：保存任务 panic 时，`receiver_handler` 只记日志，照常返回（`pack/mod.rs` 约 L189–191），第 8 节已把它列入 L0 修复任务。【推断】把缺对象当作路径不存在，会把损坏发布成空视图，或者让提交被 J4 丢弃。缺失的行日后被同内容的推送补上之后，清表重建会得出另一份历史，违反 G1。

**实现要点：**
- **读取。** 按层批量读：同一批提交的同一层只发一次查询。需要为 `get_commits_by_hashes` 与 `get_trees_by_hashes` 新增可失败的版本（返回 `Result`，不 `unwrap`），供 4.4 和 6.3 使用。
- **建树。** 先用 `sort_git_tree_items` 排序，再用 `Tree::from_tree_items_with_kind(kind, …)` 构造，并做重名检查（参照 `tree_from_items_checked`）。不调用依赖 thread-local kind 的 `Tree::from_tree_items`。
- **EMPTY_TREE。** 用 `ObjectHash::from_type_and_data_for_kind(kind, Tree, b"")` 算出（sha1 下为 `4b825dc6…`）。客户端需要它时写入 `mega_view_object`，并补上引用行。不得用 `ObjectHash::default()`（全零）。
- **条目复制【推断】。** Prefix/Exclude/Compose 新建 tree 时，子条目的 (mode, name, id) 取自解析后的 mono tree。这些条目的 mode 和非 UTF-8 文件名已被归一（0.5-18），所以新建的脊柱 tree 可能与 git 原始条目不同。脊柱 tree 的哈希由归一后的字节算出，内容与 id 自洽。只要 L0 tree 的解析与序列化行为不变，结果就是确定的；该行为有任何改动，都按 2.4 视为算法变更。
- **原样复用的 L0 tree【决策】。** 视图 tree 闭包中未被重写的子树，直接引用 `mega_tree` 的 `tree_id`，其中可能有 0.5-18a 所说的不自洽行。这类子树包括 Subdir 的结果 tree、Prefix 包住的 T，以及 Exclude/Compose 没有触及的子树。
  - 投影不校验闭包，照常推进，视图提交哈希与 L0 一致。
  - 不自洽只在打包时处理（6.3）。这不改变任何视图提交的哈希，所以不属于 2.4 所说的算法变更。

### 4.2 确定性提交构造 `rewrite_commit`（字节级）
对照 `josh-core/src/history.rs::rewrite_commit`【代码】：以原提交的字节为基础，只替换 tree 和 parents。
```
fn rewrite_commit(row: mega_commit::Model, tree: TreeId, parents: [ViewCommitId]) -> (ViewCommitId, Bytes):
    bytes = rebuild_canonical_commit_bytes(row)?        # 不 panic，不依赖 thread-local kind
    require hash(kind, Commit, bytes) == row.commit_id  # 前提校验：失败即报错，不静默产出
    bytes = replace_tree_and_parents(bytes, tree, parents)
    bytes = strip_signature_headers(bytes)              # 只删 gpgsig / gpgsig-sha256 头块
    id    = hash(kind, Commit, bytes)                   # ObjectHash::from_type_and_data_for_kind
    return (id, bytes)                                  # 纯计算，不写表
```
- **计算与持久化分离。** `rewrite_commit` 与 `filter_tree` 都是纯计算。视图提交、脊柱 tree 及其引用行，只由 4.4 的 catch_up 在批末统一写入（满足 3.5 的不变式）。5.2 第 2 步和第 10 步只使用计算结果，不写表。下文伪码中 `rewrite_commit(F, c, t, ps)` 是 `rewrite_commit(row_of(c), t, ps).id` 的简写。
- **前提校验失败【决策】。**
  - 【代码】`commit_id` 是接收时按原始字节算出的（git-internal `decode.rs` 约 L1172），`content` 原样保存，`author` / `committer` 列却是 `Signature::to_data` 重新序列化的结果（`converter.rs` 约 L199–203）。git-internal 0.10.2 的 `Signature::from_data` 能接受、`to_data` 却写不回原样的形态有三类：
    - 时间戳带前导零或 `+` 号：按 `usize` 解析（`signature.rs` 约 L188–190），按整数写回（约 L230）；
    - 空名字与 `<` 之间只有一个空格（`author <e> …`）：解析为空名（约 L169–175），写回时变成两个空格（约 L221–223）；
    - tree / parent 行的 hex 含大写字母：`from_hex_for_kind` 用 `hex::decode` 接受（`hash.rs` 约 L522），写回为小写。
    时区按原文保存；扩展头与正文都在 `content` 中。这两部分都不受影响。【推断】标准 git 不会写出这三类形态，只有第三方工具或手工构造的提交才会。
  - 【代码】mega2 自己构造的提交，id 都是对同一结构的 `to_data()` 求哈希（`Commit::new`，git-internal `commit.rs` 约 L119–138），从列重建出的字节必然一致。这包括：bootstrap、attach/detach 与 review 合并所用的 `Commit::from_tree_id`；trunk roll-up 所用的 `trunk_provenance::synthesize`（约 L127）；服务端签名后经 `Commit::new` 重建的提交（`server_signing.rs::sign_with_identities` 约 L216–246）。
  - 【推断】因此，只在 trunk 下运行过的部署，根链上全是这类提交，校验不会失败。客户端提交只有两条途径能进入根链：一是 review 形态下合并根路径 CL 时父提交取到 CL tip，CL 链随之进入首父链（0.5-23），切换到 trunk 后这段历史仍留在根链上；二是遗留数据或人工写库。
  - 【代码】这是 L0 既有缺陷，视图只是继承。Monorepo 打包提交时走 `Commit::from_mega_model` → `Entry::from(Commit)`，用 `to_data()` 的字节配原 `commit_id`（`monorepo.rs` 约 L269–290，git-internal `entry.rs` 约 L51–59）。所以根路径的 clone 遇到这类提交时，客户端的连通性检查已经会失败；trunk 下以 N=1 落为 `main@P` 的同类客户端提交，同样会让 `/<P>.git` 的 clone 失败。
  - **保留校验，不用重建字节代替。** 只凭列无法区分“语义等价的归一化”与“旧版本写入时丢掉的信息”。而且，只要从不发布依赖重建字节的视图提交，日后 L0 修复了该提交，已发布的视图历史也不会变。
  - **失败语义。** 只有需要为该提交写出视图提交时（`project_commit` 调用 `rewrite_commit`）才会触发校验，所以只阻塞在该提交处产生 NEW 提交的视图；在该处被 J4 丢弃的视图照常追赶。设失败提交的 seq 为 s：
    - catch_up 照常写入本批中 seq < s 的段、对象与引用行，水位推进到 s − 1，然后退出循环，记错误并告警（含 filter_id、s、commit_id），不得原地重试。之后每次追赶都从 s 开始，立即得出同一结论，代价为一批的预取，即 O(深度) 条查询，与 4.4 中缺对象的情形相同。这与“批的前提”不成立时整批不写不同，因为失败点之前的结果是确定的。
    - 未就绪的视图保持未就绪，返回 503，与 R11 一样不会自动恢复。协议侧仍按“未就绪”应答，原因只在告警与日志中给出，P0 不为此新增状态列。已就绪的只读视图照常广告 s − 1 处的 tip，滞后指标持续增长。可推送视图（P2）的 advertise 与 5.2 第 2 步的补算都 fail-closed。
    - 清表 rebuild 不会改变这一结论。L0 修复使该提交的重建字节与 `commit_id` 一致后，追赶自动从 s 继续。
  - **审计。** 对根链上每个提交复算 `hash(kind, Commit, rebuild_canonical_commit_bytes(row)) == commit_id`，列出不一致的 `(seq, commit_id)`。该审计在 P1 并入 `mega2 view status`。有 review 形态历史的部署，启用视图前先跑这项审计（写入 `docs/deploy-trunk.md`）。P0 只靠上面的告警给出首个失败点。
  - **P0 不新增原始字节列。** trunk 下新进入根链的提交都由服务端构造；存量客户端提交的原始字节已经丢失（receive-pack 不落盘 pack，0.5-18a），新增列也救不回来。L0 侧的修复另立任务（第 8 节）。
- **签名【决策】。** v1 固定删除 `gpgsig` 与 `gpgsig-sha256` 头块。
  - 原因：mega2 trunk 的根提交几乎都带服务端签名，而签名不覆盖视图提交的字节，保留下来会让客户端显示签名无效。
  - 来源证明改由"视图提交 → 根提交区间"的映射承担，通过 REST 暴露（6.6）。
  - 这与 Josh 有两处有意的差异，都已冻结进 v1，并记入 R8：Josh 默认保留签名（`GpgsigMode::Preserve`）；Josh 的 remove 模式只删 `gpgsig`，不删 `gpgsig-sha256`（`history.rs` 约 L252–256）。含 `gpgsig-sha256` 的用例是 mega2 自有的用例，不以 Josh 为基准。
- **`strip_signature_headers` 是新写的函数，复用 `is_signature_header`。**
  - 头区以第一个空行为界。消息结束前一直没有遇到空行时，视为没有扩展头，字节原样保留。这与 `split_commit_message` 的 `whole()` 语义一致，用来覆盖 ADR-FU-02 之前没有成帧的历史 message，例如 `create new directory demo`。
  - 现有实现不能直接拿来用：`split_commit_message(..).body` 会丢掉 encoding、mergetag 等全部扩展头；`extract_from_commit_content` 只认 `gpgsig `，还会 trim 并补换行。
  - 对服务端签名的根提交，删掉 `embed_gpgsig` 加入的头块之后，剩下的正好是 `canonical_commit_payload`（`server_signing.rs`），即规范成帧的字节。
- **其余头部原样保留**：author（含时间）、committer（trunk 下为落地时间，时区为 +0000 或沿用前值）、encoding、mergetag 和 message。
  - N>1 落地的根提交，message 是 compact 形态，带 `Mono-Squash-Commit: <id>`，视图里照样显示。被引用的 squash 提交仍在 `main@P` 物化链上（本设计不删物化链），所以 I4 的指针仍然可以解析，REST 可以据此展开（6.6）。
- **不调用的构造函数。** 不调用 `Commit::from_tree_id`（硬编码身份、取 `now()` 和 `+0800`、依赖 thread-local kind），也不调用 `Commit::new`（依赖 thread-local kind）。rewrite_commit 在字节层面完成，用不到它们；确实需要按 kind 构造对象时，用 `Commit::new_with_kind`。
- **根提交作者的可读性（R7）。** review 合并、attach/detach 和 trunk 产品写 API 的根提交，作者仍是 `mega <admin@mega.org>`，视图会如实显示。改进属于 L0 写入者的独立 ADR，不是投影的前提：
  - 产品写 API 改用认证身份作 author；
  - review 合并需要重审硬约束 8，以及 `process_ref_updates` 中 trunk-push.md 的 GAP-07 与 ADR-MC-01 的耦合（`mono_api_service.rs` 约 L1020–1035：父提交的选取依赖 CL ref 的命名分歧）。

### 4.3 线性投影规则（P0）
根链是线性的，每个提交至多一个父提交。`prev` 表示上一个已投影的状态 `(view_commit | NULL, view_tree)`，初值为 `map(F, s0)`。
```
fn project_commit(F, c /* seq = s */, parent_tree, prev) -> Result<Option<Segment>, ProjectError>:
    t = filter_tree(F, c.tree)?                                # 下文 is_empty_root、rewrite_commit 的调用同样带 ?
    if prev.view_commit is NULL:                               # 视图尚无提交
        if s == 1 && is_empty_root(c.tree):                    # Josh：无父且根树（递归）为空 → 照写无父提交
            return Some(Segment(1, rewrite_commit(F, c, t, []), t))
        if t == EMPTY_TREE:
            return if s == 1 { Some(Segment(1, NULL, EMPTY_TREE)) } else { None }
        return Some(Segment(s, rewrite_commit(F, c, t, []), t))
    (vp, vt) = prev
    if c.tree == parent_tree:                                  # 原提交本身是空变更：J3 all_diffs_empty，保留
        return Some(Segment(s, rewrite_commit(F, c, t, [vp]), t))
    if t == vt: return None                                    # J4：丢弃，区间延长
    return Some(Segment(s, rewrite_commit(F, c, t, [vp]), t))  # 含 t == EMPTY_TREE（视图被清空）
```
- `is_empty_root(T)` 的定义：T == EMPTY_TREE，或者 T 的全部条目都是 tree，且每个条目都递归满足 `is_empty_root`（Josh `history.rs::is_empty_root`）。递归读取经由 4.1 的 `read_tree`，缺对象时返回错误，不按非空或空处理。`project_commit` 返回 `Result<Option<Segment>, ProjectError>`，ProjectError 是以下三者之一：4.2 前提校验失败；4.1 `MissingObject`；根链行的 `mega_commit` 行缺失。最后一种只在 `rewrite_commit` 需要读该行时触发，在该处被 J4 丢弃的视图不受影响。
- **线性投影的实现细化（HP-06）。** `project_commit` 返回 `Result<Option<Segment>, ProjectFailure>`，其中 `ProjectFailure::Data(ProjectError)` 承载前提校验失败、`MissingObject` 与根链提交行缺失，`ProjectError` 保持这三个变体不变；`ProjectFailure::Internal` 只承载调用方违反前提时的过滤器不变式违例，不对应数据状态，并由调用方整批回滚、水位不变；`MissingObject` 原样保留含未预取在内的读取原因，由调用方按原因分派；`Segment` 带视图提交的字节，供批末写对象；R8 检测在视图尚无提交且复合结果不写提交时逐级读取，超出按层预取范围的读取由调用方补预取后重算；中间级只含目录、子 tree 逐提交变化、复合结果为 EMPTY_TREE（如 `:/a:exclude[::b]` 且 `a/` 只含 `b/`）的罕见形态构成 §4.4「往返次数」的明示例外，可能使往返次数随批内提交增加，由 HP-24 计量。
- 在单父情形下，以上规则与 Josh 的 `create_filtered_commit2` / `select_parent_commits` 一致【代码：`history.rs` 约 L697–721、L899–911】。
- 【推断】在 mega2 中，trunk 根提交的 tree 总是与父提交不同（ADR-TP-16：净零推送不推进根），所以 `all_diffs_empty` 分支实际上不会触发；mega2 的初始化提交也不是空树。这两条规则仍然保留，用于与 Josh 对齐，并由测试覆盖。
- **顶层 Chain。** Josh 按 `flatten_chain` 逐级做历史过滤（`filter/mod.rs::apply_to_commit2`）。在线性历史上，这与"先求复合 tree 函数，再套一次上述规则"等价，所以 P0 用复合 tree 函数。例外：L0 tree 中含有指向空树的条目时，逐级过滤的中间级可能触发空根规则，复合函数复现不了这种情形。这一点列入 R8，投影时检测到就告警。非线性情形见附录 E。

### 4.4 冷启动与增量 `catch_up`
```
fn catch_up(F):
    loop:
        txn:
            if !acquire(L_V(F), mode) || !acquire_shared(L_G, mode):   # mode：后台 worker 与只读视图的同步追赶用试锁；可推送视图的 advertise 用带 lock_timeout 的阻塞锁（4.6）
                return                                             # 另一个 worker 在处理；不留标记，由信号或周期补偿重试
            if F.ready_seq is NULL && F.warming_since is NULL: return   # 未经 6.5 准入（如回收后尚未重新预热），不投影
            s0    = F.projected_seq (re-read in txn)
            tip   = root_chain.max_seq()
            if s0 >= tip:
                if !mark_ready_if_covered(F, tip):
                    if caller is not worker { notify_worker() }    # 只由同步调用方唤醒 worker；worker 自身依赖下一次周期补偿，避免保存的许可造成空转
                return
            rows  = root_chain.range(max(s0,1) ..= min(s0+B, tip))  # 1 条 SQL；第 s0 行只提供父树
            require seqs(rows) == [max(s0,1) ..= min(s0+B, tip)]     # 连续且含 s0+1；否则 fail-closed，不推进水位
            commits = get_commits_by_hashes_fallible(rows)          # 1 条 SQL；读不到的行记入本批缺失集合
            prefetch trees level by level                           # 每层 1 条 SQL；读不到的 id 记入本批缺失集合
            prev  = map(F, s0)                                       # 无行时 (NULL, EMPTY_TREE)
            stop  = None
            for c in rows where seq > s0:                            # tree_of(0) := EMPTY_TREE（seq=1 时不会被用到）
                match project_commit(F, c, tree_of(seq-1), prev):
                    Ok(Some(seg)) => segs.push(seg); prev = seg
                    Ok(None)      => {}
                    Err(e)        => stop = Some((seq, e)); break      # 4.2 前提校验失败、4.1 缺对象或提交行缺失
            insert segs, objects, refs ON CONFLICT DO NOTHING        # 只含 seq < s 的结果；按主键排序
            clear gc_marked_at of objects referenced by refs         # 与写引用行同事务，持 L_G 共享锁，与清扫互斥（3.5）
            last  = if let Some((s, _)) = stop { s − 1 } else { rows.last.seq }
            F.projected_seq = GREATEST(F.projected_seq, last)
            if let Some(st) = stop: alert(st); return                 # 本事务照常提交；不原地重试
            if last == tip:                                          # 与推进水位同一事务
                if mark_ready_if_covered(F, tip): return Ready
                return MainNotCovered

fn mark_ready_if_covered(F, tip) -> bool:                           # 只在持 L_V(F) 与 L_G 共享锁的批事务内调用
    root = read main@/
    if root_chain.commit_at(tip) != root.commit: return false       # 比较 seq = tip 那一行，不重读链尾
    if F.ready_seq is NULL: F.ready_seq = tip; F.warming_since = NULL   # 同一事务内释放冷启动名额
    return true
```
- **往返次数。** 每批 O(1) 条提交查询，再加 O(深度) 条按层的 tree 查询；不存在"每个提交一次往返"。
- **末批终态（FIX-HP-01）。** 最后一批在持有 L_V(F) 与 L_G 共享锁的同一事务内，已写入 `projected_seq` 并执行 `mark_ready_if_covered`。该事务提交成功后可直接返回 Ready 或 MainNotCovered，不再打开一笔无内容的终止检查事务。若该事务提交失败，仍返回错误而不是终态；未到 tip 的批、停止和部分推进仍按原循环处理。这里不跳过就绪校验，也不提前释放锁。只读视图的同步追赶收到 MainNotCovered 时会按 4.6 唤醒 worker；直接返回终态使这个唤醒早于原先的空终止事务，行为仍可重试。
- **可失败批量读取的语句数（HP-29）。** 一层有 k 个待查 tree id、其中 r 个不在 `mega_view_object` 中时，先查询 `mega_view_object`、再查询剩余 `mega_tree`，至多执行 ⌈k/1000⌉ + ⌈r/1000⌉ 条语句；每条最多 1000 个 id。伪代码的「每层 1 条 SQL」和 4.1 的「同一层只发一次查询」按此分块规则理解：一层 id 不超过 1000 时至多两条。每层语句数只取决于该层 id 数，与批内提交数无关，因此 O(深度) 不变。提交的 `get_commits_by_hashes_fallible` 也按 1000 分块：请求 m 个不同提交 id 时执行 ⌈m/1000⌉ 条；若按伪代码传入完整 rows，m 至多 B + 1，默认 B = 1000 且 s0 ≥ 1 的满批为 1001 个 id、两条查询，B ≤ 10 000 时至多 11 条。第 s0 行只提供父 tree 时可从 `mega_view_root_chain.tree_id` 取得它，不读取该提交则 m ≤ B；提交读取的「1 条 SQL」按此理解，仍为每批 O(1)。
- **批的前提【决策】。** `s0 < tip` 时，本批必须恰好取到 seq 从 `max(s0,1)` 到 `min(s0+B, tip)` 的连续行，因此至少包含 `seq = s0+1` 这一行，水位每批至少前进 1。
  - 取不到这些行，说明派生状态或配置已经损坏，原因可能是根链表出现空洞，或者 B < 1 绕过了 6.9 的校验。
  - 此时本批不写任何行、不推进水位，记错误、告警并退出循环，不得原地重试。
  - 没有这条前提时，B = 0 且 s0 ≥ 1 会使 rows 只含第 s0 行，水位不变，循环空转；s0 = 0 时 rows 为空，`rows.last` 没有定义。
  - 4.2 的前提校验失败时，按 4.2 的失败语义处理：写入 seq < s 的部分，水位推进到 s − 1。
  - `filter_tree` 或 `is_empty_root` 返回 `MissingObject`（4.1），或者 `project_commit` 需要为本批某个根链行调用 `rewrite_commit`、该行的 `mega_commit` 行却读不到时，设该提交的 seq 为 s。处理方式与 4.2 的失败语义相同：照常写入本批中 seq < s 的段、对象与引用行，水位推进到 s − 1；随后退出循环，记错误并告警，告警中含 filter_id、s、commit_id 与缺失的对象 id；不得原地重试。未就绪的视图保持 503，不释放名额（6.5）；已就绪的只读视图照常广告 s − 1 处的 tip；可推送视图按 5.2 第 2 步处理。与 4.2 相同，之后每次追赶都从 s 重新读取，代价为 O(深度) 条查询；缺失的行被补上之后（例如同一内容再次被推送），追赶自动从 s 继续。
  - 查询本身失败（连接中断、超时等）不属于上一条：整批事务回滚，不写任何行，水位不变，由下一次触发重做。
- **就绪与水位同事务【决策】。** 首次就绪在把水位推进到 tip 的那个批事务内写入，与 3.1、6.5 所说的‘视图首次就绪的那个 catch_up 批事务’一致，只有该批读到的 `main@/` 已经超出根链时，才留给之后 s0 ≥ tip 分支的事务置就绪；6.9 的候选谓词保证它在一个周期内被选中。如果放在不同的事务里，最后一批提交之后、置就绪之前，只要发生以下任一情形，视图就会停在‘水位已到链尾、`ready_seq` 仍为 NULL’的状态：进程退出，该事务遇到瞬时数据库错误，或者后台 worker 对 L_G 试锁失败后返回（4.6）。此后入口要到下一个补偿周期才不再返回 503，名额也多占一个周期。判定是否覆盖 `main@/` 时，与根链中 seq = tip 那一行的 `commit_id` 比较，不重读链尾。【推断】READ COMMITTED 下，`max_seq()` 与链尾是两条语句，各取一次快照；其间 `extend_root_chain` 可能接入新行，重读链尾会把落后的水位误判为已覆盖 `main@/`。
- **锁【决策】。** 视图锁和根链锁都用两个整数作键：
  - `VIEW_LOCK_NS` 与 `VIEW_FILTER_LOCK_NS` 互不相同，也与 `MONO_WRITE_LOCK` 及初始化锁的 key1 都不同；
  - 单例锁（根链锁、L_G、L_R）用 `key1 = VIEW_LOCK_NS`，`key2` 分别取 `ROOT_CHAIN_KEY`、`OBJECT_GC_KEY`、`REGISTER_KEY`。视图锁改用独立的 `key1 = VIEW_FILTER_LOCK_NS`，`key2 = hash32(filter_pk)`（`hash32(x) = ((x as u64) ^ ((x as u64) >> 32)) as u32 as i32`，即 64 位 id 的高低 32 位异或折叠），与单例锁不会撞键；
  - `cfg(test)` 下，key2 改为 `hashtext(current_schema() || ':' || d)`，其中 d 是 filter_pk 的十进制文本，或者 `'root_chain'`、`'object_gc'`、`'register'` 之一。这样并行的测试 schema 之间互不争锁，同一 schema 内根链锁与各视图锁也仍然互不相同。`mono_write_lock_sql` 只有一把锁，可以整体替换 key2；这里有多把锁，不能照搬。

  投影写入只发生在同时持有视图锁和 L_G 共享锁的 worker 事务中；回收、清扫与 rebuild 的删除按 4.6 执行；B3 等写入判定路径对这些表只读（5.2）。对象维护锁 L_G、各类事务的取锁方式和全局锁顺序，见 4.6。投影是确定的，重复计算无害：写入都是 `ON CONFLICT DO NOTHING`，水位只增不减。抢锁失败时直接返回，不设进程内标记。【推断】这样做不影响正确性：B3 的写入判定只用锁内算出的 `tip_at_R`（4.5）；advertise 与 B0 的同步路径或者带 `lock_timeout` 阻塞等锁，或者在追不上时按 3.3、4.4 广告已投影的 tip、返回可重试错误，都不依赖 worker 何时被唤醒。活性只由周期补偿保证，前提是周期任务的候选谓词覆盖所有未完成的活跃视图（6.9 `worker_interval_secs`）。代价是：持锁者最后一次读取链尾之后落地的提交，在本副本上最多多等一个 `worker_interval_secs`。原来的 dirty 握手也给不出更强的保证：持锁者检查标记之后、提交并释放锁之前，竞争者仍可能置位后离开；而且标记只在进程内可见，跨副本无效。如果实测需要更低的延迟，再加回在释放锁之后复查的版本。
- **触发方式：**
  1. `run_c_segment_index` 改为按 kind 分派：push、merge、attach 保持现有的 `index_blob_paths_c_segment(path)`；view_push 走 5.2 第 9 步的按源路径索引。四种 kind 都在这个挂点的末尾发一个进程内信号。信号本身不读写任何视图表，也不等待投影完成；信号或 worker 失败只记日志和指标，绝不影响已提交轮次的返回；
  2. 周期补偿任务在每个副本上都运行。这一项必不可少，因为信号只能唤醒执行了 B3 的那个副本。它先扩展根链（3.3）。结果为"不连续"时，本轮不执行任何 catch_up（3.3 停止追赶）；否则再从活跃视图（`ready_seq` 或 `warming_since` 非空）中挑出水位落后于链尾、或尚未就绪（`ready_seq` 为 NULL）的视图，执行 catch_up。候选谓词见 6.9；
  3. advertise 时做有界同步追赶（见下文"读时一致性"）；
  4. 注册或重新预热通过 6.5 的准入之后，发一个进程内信号。
- **后台任务。** 按 `src/server/http_server.rs` 中 `spawn_*_task` 的模式实现：受配置开关控制、订阅热加载、返回 JoinHandle。不放进 `AppContext::new` 中无条件启动的任务。进程内信号使用会保存许可的原语（例如 `tokio::sync::Notify::notify_one`）：worker 执行一轮期间到达的信号不会丢失，本轮结束后立即再执行一轮。
- **读时一致性【决策】：**
  - **未就绪。** `ready_seq` 为 NULL 表示视图从未追平过，例如刚注册、正在冷启动或正在 rebuild。此时视图的每个协议入口都返回可重试错误：HTTP 返回 503 加 `Retry-After`；SSH 按 6.1 错误契约写入一行 `ERR view <filter_id> unavailable: warming up`，并以退出码 75 结束通道。
    - 检查分两处（6.1）。入口的 `resolve_view_target` 在首次宣告之前检查，覆盖 v0 的 ref 广告和 v2 的能力宣告；`ViewRepo::refs_with_head_hash`、`check_wants_and_ready` 与 `prepare_pack`（6.3）在自己的读取中再查一次（其中 `check_wants_and_ready` 逐轮执行，覆盖 SSH 同一个 exec 内的多轮 v2 协商），覆盖两次请求之间状态发生变化的情形。【代码】只在 handler 中检查不够，因为 v2 的 `info/refs` 不构造 handler（0.4-15）。
    - 两处都返回类型化的 `MegaError::ViewUnavailable`（HTTP 503），不得借用 `InvalidInput`（400），也不得经 `GitError` 传出。
    - 任何一处都不得以 capabilities-only 的空仓库或旧 tip 应答，与 ADR-TP-20 的"绝不把未就绪伪装成空仓库"一致。
    - 根链按 3.3 停止追赶时，同样这样处理。
  - **只读视图已就绪。** advertise 前先按 3.3 把根链扩展到 `main@/`，再做有界的同步视图追赶，最多 `views.sync_catch_up_commits` 个提交（默认 64）。追不平就广告已投影到的最新 tip，并记录滞后指标。根链扩展返回"未追上"时，跳过同步视图追赶，直接广告已投影到的最新 tip；返回"不连续"时返回 503。
  - **可推送视图（P2）。** 客户端会在视图 tip 上叠加提交，这正是 ADR-TP-19 判定"不能最终一致"的第二条判据。所以可推送视图的 advertise 必须先扩展根链，再把视图追到 `seq(main@/)`；必要时阻塞等待根链锁和视图锁，超时返回 503。这也保证了 view_push 之后的 fetch 能读到自己的写入，这是 ADR-TP-18 中 fetch + reset 的前提。根链扩展返回"未追上"时，同样返回 503 加 `Retry-After`。

### 4.5 视图 tip
- **持久 tip `view_tip(F)`（下文也称 persisted_tip）。** `view_tip(F) = map(F, F.projected_seq)`，滞后量 `lag = seq(main@/) − projected_seq`。
  - 如果 `main@/` 还没进入根链，`lag` 等于根链尚未覆盖的提交数加上 `(root_chain.max_seq − projected_seq)`，视为未追平。
  - 只有在 `projected_seq ≥ seq(main@/)` 时，`view_tip` 才是当前 `main@/` 的投影。
  - 它只供读侧使用：ref 广告（6.2）、REST（6.6）、GitHub 镜像（6.8）和滞后指标。B0 的滞后预检读的是 `projected_seq`，属于准入，不是写入判定。
- **锁内 tip `tip_at_R(F)`【决策】。** 这是 view_push 唯一的写入判定输入，由 B3 在 `MONO_WRITE_LOCK` 内、针对锁内读到的根 R 计算（5.2 第 2 步）：
  - **求 seq(R)。** 在 B3 事务内读出 `p0 = F.projected_seq`。R 在根链表中时，`seq(R)` 取表中的值；不在表中时，`seq(R)` 等于链尾的 seq 加上锁内回溯得到的提交数。`seq(R) < p0`、回溯不满足 3.3 第 3 步、或回溯途中遇到多父提交时，一律 fail-closed。
  - **非线性闸门（附录 E 启用后）【决策】。** 上一条只检查锁内回溯到的提交，也就是还没进根链表的那部分。附录 E 启用后，多父提交会作为普通行接入根链表，补算集合 K 中 seq ≤ `root_chain.max_seq` 的部分又直接取自根链表，所以 merge 一旦入表，回溯就再也遇不到它，有三种情形：
    - R 就是该 merge，投影已追平到它：K 为空，tip_at_R 直接等于持久 tip；
    - R 是之后的 trunk roll-up：回溯只经过单父提交；
    - `p0` 落在 merge 之前：K 中的 merge 被按线性规则以 `tree_of(seq − 1)` 补算，结果与 catch_up 按附录 E 规则持久化的映射不同，NFF 的输入随之失真。
    因此，B3 在第 2 步的同一条 SQL 中另读根链的非线性标记 `EXISTS (SELECT 1 FROM mega_view_root_chain WHERE parent_count > 1)`，经部分索引 `WHERE parent_count > 1` 判定，代价 O(1)。结果为真时，不论 R 是否在表中、K 是否为空，都在补算、第 3 步断言与 NFF 之前以不可重试错误拒绝，提示根历史非线性、view_push 已停用；只读投影不受影响。`parent_count` 列与该部分索引由附录 E 的迁移加入。P0–P1 中，3.3 第 3 步把多父提交判为不连续、从不接入，根链表恒为线性，tip_at_R 也只在 P2 的 B3 中计算，所以 P0–P1 不加这一列、不建索引，根链与 catch_up 的语句不变。
  - **补算。** 令 K 为根链上 seq 从 `p0 + 1` 到 `seq(R)` 的提交：seq ≤ `root_chain.max_seq` 的部分取自根链表，其余取自锁内回溯。从 `prev = map(F, p0)` 出发，对 K 按 seq 递增依次调用 `project_commit(F, k, tree_of(seq − 1), prev)`（4.3；`tree_of(0) := EMPTY_TREE`），返回 `Some(seg)` 时令 `prev = seg`。最后得到的 `prev`，即 `(view_commit | NULL, view_tree)`，就是 `tip_at_R(F)`。
  - **性质。** 它只在内存中计算，不写表。由 4.3、4.4 的确定性，它与 catch_up 投影到 seq(R) 之后持久化的 `map(F, seq(R))` 逐字节相同；`p0 = seq(R)` 时，它就是 `view_tip(F)`。
- **写入判定不得读 `view_tip(F)`。** C 段只发信号，投影是异步的（4.4），所以每次普通推送落地之后，都有一段 `projected_seq < seq(R)` 的窗口。窗口内，根可能已经改动了视图内的内容，而 `view_tip(F)` 仍等于客户端的 old_v。拿它做 NFF 会放行陈旧推送，覆盖已提交的内容，这正是 ADR-TP-19 所说的失效链（trunk-push.md 约 L326–343）。2.7 的三项校验只保护视图之外的内容，挡不住这种覆盖。advertise 是否新鲜，只影响客户端拿到的 old_v 是不是最新，不承担写入安全。
- "视图为空"专指 tip 为 NULL 段，与"未就绪"（`ready_seq` 为 NULL）严格区分。tip 是一个空树提交（视图被清空）时，照常广告这个提交。
- P2 不使用 `unapply_base`：推送基准恒为锁内的 R（5.2）。非线性情形下的反向约束记在附录 E，留待另立 ADR。

### 4.6 派生状态的并发、回收与 rebuild【决策】
**锁。**

| 锁 | 键 | 持有者 |
|---|---|---|
| L_M：`MONO_WRITE_LOCK` | 现有 | B3、物化插入、ADR-TP-20 的对账与巡检批次、reaper（试锁） |
| L_C：根链锁 | `(VIEW_LOCK_NS, ROOT_CHAIN_KEY)` | `extend_root_chain`、全局 rebuild |
| L_V(F)：视图锁 | `(VIEW_FILTER_LOCK_NS, hash32(F.pk))` | catch_up(F)、回收与单视图 rebuild |
| L_G：对象维护锁（新增，分共享与排他） | `(VIEW_LOCK_NS, OBJECT_GC_KEY)`；`cfg(test)` 下 d 取 `'object_gc'`（4.4） | 共享：catch_up 批事务、回收；排他：清扫、全局 rebuild |
| L_R：注册准入锁 | `(VIEW_LOCK_NS, REGISTER_KEY)`；`cfg(test)` 下 d 取 `'register'`（4.4） | 6.5 的注册准入事务与重新预热准入事务 |

- **顺序。** 全局顺序为 L_M → L_C → L_V → L_G。同一事务需要持多把锁时，只能按这个顺序获取。另有两条硬约束：
  - L_M 不与 L_C、L_V、L_G 出现在同一事务中。B3 本来就不取根链锁和视图锁（5.2 第 2 步）；视图侧的维护事务也一律不取 L_M，GC、回收与 rebuild 都不得借 L_M 做栅栏。否则持 L_M 的事务会排在一个 catch_up 批次之后，拖住全局写锁。
  - L_R 只在准入短事务中单独持有，不与 L_M、L_C、L_V、L_G 出现在同一事务中。
  - 持有 `queue_control` 准入行锁时（B1，`src/jupiter/storage/push_queue_storage.rs` 约 L129–162），不得等待 L_C 或 L_V。B0 的根链扩展必须在 B1 的准入事务之外完成。
- **各事务的取锁方式：**
  - B3：只持 L_M，对 `mega_view_*` 只读。
  - `extend_root_chain`：只持 L_C。advertise 先执行根链事务，再执行 catch_up 事务，两者不嵌套；B0 只执行根链事务。
  - catch_up 的每个批事务：先取 L_V(F)，再取 L_G 共享锁，一直持有到提交，覆盖读水位、读根链和写入的全过程。后台 worker 与只读视图的同步追赶对两把锁都用试锁，失败时直接返回，由下一次信号或周期补偿重试；可推送视图的 advertise 用带 `lock_timeout` 的阻塞锁（4.4）。
  - 回收、单视图 rebuild：L_V(F) → L_G 共享。
  - 清扫：只持 L_G 排他。
  - 全局 rebuild：L_C → L_G 排他。
- **死锁检查。**
  - 等待边只有三类：持 L_V 等 L_G（catch_up、回收）；持 L_C 等 L_G（全局 rebuild）；不持锁时等任一把锁。持 L_G 排他的清扫与全局 rebuild 不再等待 L_V；持 L_C 的 `extend_root_chain` 不等待其他锁；持 L_M 的 B3 不等待任何视图侧的锁，也不写 `mega_view_*`，因此不会在唯一索引上等待 worker 尚未提交的插入（5.2 第 2 步）。持 L_R 的准入事务只可能等待 `mega_view_filter` 的行锁；这类行锁由释放名额的 catch_up 和清列的回收持有，而它们都不等待 L_R，所以等待图仍然无环。
  - 行级锁：catch_up 之间按主键排序插入（4.4）；回收、全局 rebuild 与 catch_up 由 L_V 和 L_G 互斥，不会以不同顺序删改同一批行。
  - 删除一律用 DELETE，不用 TRUNCATE。TRUNCATE 要取 ACCESS EXCLUSIVE 表锁，持 L_M 读这些表的 B3 会因此排队。
  - 结论：等待图无环，不会死锁。
- **清扫。** 分批执行，每批一个事务，持 L_G 排他，按 `object_id` 游标处理至多 `gc_batch_limit` 行：
  - 没有引用行的对象：置 `gc_marked_at = statement_timestamp()`，已经置过的不改。标记写入 `statement_timestamp()` 而不是 `now()`：清扫事务可能在 L_G 排他锁上排队，`now()` 是事务开始的时刻，可能早于清扫实际看到无引用状态的时刻，宽限期会因此提前起算；
  - 有引用行的对象：清空 `gc_marked_at`；
  - `gc_marked_at < now() − gc_grace_secs` 且仍然没有引用行的对象：删除。

  写引用行的 catch_up 事务都持 L_G 共享锁直到提交，并在同一事务内清空所引用对象的 `gc_marked_at`，所以清扫判定的"无引用"不会被并发事务推翻，重新被引用的对象也不会带着旧标记。清扫之后再运行的 catch_up，用 `ON CONFLICT DO NOTHING` 重新插入对象，字节由确定性投影给出，与原对象逐字节相同。不使用外键，原因有两点：并发插入的引用会让清扫整批报外键冲突；外键还会给 `mega_view_object_ref` 的每次插入增加一次索引探查和 KEY SHARE 行锁。
- **视图删除即回收。** 3.1、3.2 的定义行永不物理删除，因为 3.6、`push_queue.filter_pk` 和根提交中的 `Mono-View` 都引用它们。按 `last_access_at` 的自动回收和 `mega2 view gc <filter_id>`（即"删除视图"：按 filter 回收，不提供 HTTP 删除接口）都只回收派生状态。回收在一个事务内完成（L_V(F) → L_G 共享）：
  1. 如果 `push_queue` 中有 `kind = 'view_push' AND filter_pk = F.pk AND status IN ('Queued','Running')` 的行，本次跳过，CLI 返回 busy。这条查询正好命中 3.8 的部分唯一索引。
  2. 置 `ready_seq = NULL`、`projected_seq = 0`、`warming_since = NULL`。
  3. 删除 F 的 commit_map 行与引用行，对象留给清扫。

  回收后，视图按“未就绪”应答（503）。再次被访问时，按 6.5 的准入重新预热；准入被拒（`max_filters` 或名额已满）时，本次访问仍返回 503 加 `Retry-After`，读者无法借访问绕过名额。单视图 rebuild 就是回收之后立即按 6.5 准入预热：它是运维命令，不计速率；名额不足时 CLI 返回 busy。
- **闲置回收（P1）【决策】。** 按 `last_access_at` 的自动回收沿用上面的回收事务，另加以下规则。
  - **写入来源。** 只有表明视图正在被使用的成功请求才刷新这一列：
    - 6.1 第 3 层解析成功且视图就绪的协议请求，包括 HTTP 的 `info/refs`、`ls-refs`、`fetch` 与 v0 `git-upload-pack`，以及 SSH 的每个 exec；
    - 6.6 REST 读接口的成功请求；
    - 6.5 准入事务第 6 步新建过滤器行或重新预热时，在同一事务内置 `last_access_at = now()`。否则刚预热完的视图会在下一轮立即被判为闲置。命中已有过滤器的幂等注册，按一次访问刷新；
    - P2：通过 B0 的 view_push；视图 GitHub binding 的镜像读取。

    不刷新的请求：`GET /api/v1/views*` 的元数据查询（监控轮询不应让视图常驻）；worker 与补偿任务；`mega2 view status`；应答为 404、429、503 的请求。
  - **节流。** 刷新语句为 `UPDATE mega_view_filter SET last_access_at = now() WHERE id = $1 AND (last_access_at IS NULL OR last_access_at < now() − T)`，T 是代码常量 3600 秒。各副本另在进程内记下每个 filter_pk 上次刷新成功的时刻，T 之内不再发语句，因此每个视图在每个副本上每小时至多一条 UPDATE。
    - 刷新在应答路径之外异步执行，尽力而为：失败只记日志与指标，不影响请求结果，请求也不等待 catch_up 或回收持有的行锁。
    - 刷新语句不取任何 advisory 锁，只在语句执行期间持本行的行锁，所以上面"死锁检查"的等待图仍然无环。
  - **NULL 语义。** NULL 表示没有访问记录，不参与闲置回收；这类视图只能由 `mega2 view gc` 回收。P1 上线的迁移把现存行中的 NULL 置为迁移时刻，P0 期间注册的视图从 P1 上线起计时，不会一上线就被判为闲置。回收与 rebuild 都不修改这一列。
  - **候选与复查。** 闲置阈值为 `idle_recycle_secs`（6.9），下界为 2T。
    - 周期任务先按 `ready_seq IS NOT NULL AND last_access_at < now() − idle_recycle_secs` 选出候选。正在预热的视图（`warming_since` 非空）不是候选，否则会白白浪费一次冷启动。
    - 对每个候选单独执行上面的回收事务，其中第 2 步改为带复查的单条语句：`UPDATE mega_view_filter SET ready_seq = NULL, projected_seq = 0, warming_since = NULL WHERE id = F.pk AND ready_seq IS NOT NULL AND last_access_at < now() − idle_recycle_secs`。命中 0 行即放弃本次回收，不删除任何行。
    - `mega2 view gc` 不做闲置复查，第 1 步照常执行。
  - 【推断】复查与状态改写在同一条语句中取得行锁。并发的刷新先提交时，复查不成立，回收放弃。刷新后提交时，它要等回收提交之后才写入，只是给已回收的行留下一个新的访问时刻；那次请求随后会在 `prepare_pack` 的复查中得到 503，或者已经在发包，由 `gc_grace_secs` 保护（6.3"发包与回收交错"）。
- **全局 rebuild（P1 `mega2 view rebuild`）。** 在一个事务内依次持 L_C、L_G 排他：清空根链表与暂存表、全部 commit_map 与引用行，重置所有视图的状态列（含 `warming_since`；被重置的视图再次被访问时按 6.5 准入预热）；对象留给清扫。L_G 排他保证此刻没有 catch_up 批事务在按旧根链写入。全局 rebuild 不检查在途行：在途的 B3 要么读到 rebuild 之前的一致快照，照常执行；要么读到 `ready_seq` 为 NULL，返回可重试错误（见下条）。
- **读者的单快照规则。** 【代码】mega2 没有显式设置事务隔离级别（`src/` 中没有 `IsolationLevel`，也没有 `default_transaction_isolation`），B3 按 Postgres 默认的 READ COMMITTED 运行，每条语句各取一次快照。catch_up 只追加，分句读取不会出错；回收与 rebuild 会删行，如果水位读自回收之前、映射读自回收之后，5.2 第 3 步就会误报派生状态损坏。因此：
  - B3 用一条 SQL 读出：F 的 `ready_seq` 与 `projected_seq`、`root_chain_halted`（3.3）、`map(F, projected_seq)`、old_v 所在段及下一段的 `seq_from`、第 3 步断言要用的 `root_chain[p]` 与 `root_chain[old_v 段的 seq_from]`、补算要用的根链行，根链链尾与回溯（5.2 第 2 步已要求后两项同快照），以及附录 E 启用后的根链非线性标记（4.5）。`ready_seq` 为 NULL 时返回可重试错误。
  - `ViewRepo::refs_with_head_hash` 与 6.6 的接口，同样用一条 SQL 读出就绪状态、`root_chain_halted` 与 tip，不把正在回收的视图当作空仓库广告（4.4）。
  - `check_wants_and_ready`（6.3）用一条 SQL 读出 filter 行、`root_chain_halted`（3.3）与各 want 的 `seq_from`。如果分句读取，回收中的视图会被误报为 `not our ref`（不可重试）。
  - `prepare_pack`（6.3）同样用一条 SQL 读出五项：就绪状态与水位、`root_chain_halted`（3.3）、各 want 是否仍命中 `(filter_pk, view_commit)`、seq 区间内的视图提交 id。回收在这条 SQL 之前提交时，返回 `ViewUnavailable`；want 不再命中也按此处理，不报 `not our ref`。如果分句读取，可能读到空区间，发出缺提交的 pack。之后的打包只读对象，不再读 commit_map。对象的保护见 6.3"发包与回收交错"。
- **在途 view_push 依赖什么。**
  - 【代码】B3 与 PushChain 都不读 old_v 的提交对象。PushChain 只用 id 比较链上最老提交的首父与 base（`src/ceres/pack/push_chain.rs` 约 L384、L484）；NFF 只比较 id。
  - 真正需要读取的，是 tree(new_v) 闭包中客户端没有随 pack 发送的子树。【代码】receive-pack 宣告 `no-thin`（`src/ceres/protocol/smart.rs` 约 L46），pack 内没有引用外部对象的 delta；但 old_v 中未改动的子树照样不随 pack 发送，其中的脊柱 tree 只存在于 `mega_view_object` 中。
  - 保护规则有三条：回收跳过有在途行的视图；对象只有被标记且超过宽限期之后才删除；B3 读取 tree(new_v) 闭包时缺对象，以可重试错误结束，并提示 fetch 后重推，不判为派生状态损坏。
  - 可重试的 Failed 行不阻止回收。重推之前视图会重新预热，确定性投影会原样恢复 old_v 及其脊柱 tree；如果期间 rebuild 改变了历史，NFF 会以 "fetch first" 拒绝。

---

## 5. 视图推送（P2，trunk 形态）

### 5.1 范围
- **适用范围。** 只支持 trunk 形态、HTTP、`refs/heads/main`，且视图的 `push_enabled = true`（2.2）。SSH 在 storage-only 下只读。
- **链的形状。** 推送链必须线性、不含 merge（ADR-TP-17），长度不超过 `max_push_commits`。
  - `old_v` 必须是该视图 commit_map 中的提交。
  - **`old_v` 为 ZERO 的推送一律拒绝。** 它等价于孤儿链：视图为空时，先通过 `/<path>.git` 或路径开通建立源路径。
  - 不支持新分支和推送选项。
- **推送暂存（A 段，锁外）：**
  - pack 中的 blob 写入 `mega_blob` 和对象存储（内容寻址，与 L0 共享）；
  - pack 中的 tree 写入 `mega_tree`。它们按内容寻址；推送被拒时留下的残留，与 `kind=push` 现在的情况同类；
  - pack 中的 commit 以 `(F.pk, object_id)` 为键写入 `mega_view_pushed_commit`（3.6），data 取 pack 解码出的原始字节。

### 5.2 落地流程：新 queue kind `view_push`

**B0 与 B1（准入与幂等解析，锁外）**
1. **静态准入（B0，先于幂等解析）。** 这些检查只依赖请求本身和视图定义，重放时同样必须通过，授权尤其不能被回放绕过。【代码】这与 `b0_reject_push`（`push_queue_service.rs` 约 L926–966）只判定请求本身的做法一致。
   - **视图定义。** 视图存在，`push_enabled = true`，`algo_version` 受支持，`object_format = sha1`。
   - **授权。**
     - `push_auth=token` 时，token 的 paths 必须覆盖 `src_paths(F)` 中的**每一个**源路径（按组件边界匹配）。
     - `push_auth=none` 时，默认拒绝 view_push（`views.allow_anonymous_push = false`）；显式打开后不做路径授权，部署文档要写明风险。
   - **推送链校验。** 规则与 `PushChain` 相同：连通、无 merge、tip 匹配、链长受限。实现上要把 `PushChain` 读取提交的来源抽象成一个 trait（例如 `CommitSource`）：Monorepo 用 mega_commit 实现；ViewRepo 先按 `(F.pk, id)` 查 `mega_view_pushed_commit`，再查 commit_map 中的视图提交。校验规则不变，`push_chain.rs` 的测试一并移植。
2. **幂等解析（B1，先于任何状态性拒绝）。** 以 `(kind=view_push, path=LCP, operation_id)` 做一次只读查询：
   - **命中 Done：** 回放 `landed_commit_id`。report-status 返回 ok，sideband 给出根提交；视图已投影到该提交时，附上对应的视图提交，否则提示稍后 fetch 后 reset。回放不再做下面的状态预检。
   - **命中 Queued/Running：** 收养该行，沿用它持久化的 payload；就绪状态和补算上限由 B3 第 2 步在锁内检查。

   【代码】现有 B1 的 Done 回放在暂停、硬停、容量已满时都可用（`push_queue_storage.rs` 约 L394）；trunk-push.md 1.11 的"判定次序"要求幂等解析先于任何可能拒绝的预检。现有 `enqueue_atomic` 只有一条路径，即"条件 INSERT 加零行分类"（约 L130–430），没有"先分类、后准入"的挂点。实现时，在它之前加一次与 `classify_zero_row` 条件相同的只读查询。
3. **状态预检（B0 的滞后部分，只对新轮次执行）。** 只有 B1 三态都未命中时才执行，包括 Failed 或无继任的 Cancelled 之后、按 1.11 复制载荷的重试。检查项：
   - 视图已就绪（`ready_seq` 不为 NULL）；
   - 按 3.3 扩展根链，返回"未追上"时以可重试错误拒绝；
   - 滞后量超过 `views.max_in_lock_catch_up` 时以可重试错误拒绝。

   通过后执行 B1 的条件 INSERT。预检与 INSERT 之间如果状态发生变化，由 B3 第 2 步兜底；INSERT 时如果发现同指纹的行已经出现，按 B1 的零行分类回放或收养。根链扩展必须在 B1 的准入事务（持有 `queue_control` 行锁）之外完成（4.6）。
4. **入队。** 写入 `kind=view_push`、`path = LCP(src_paths(F))`、`filter_pk = F.pk`，payload 为 `{filter_pk, old_v, new_v, chain}`。
   - 由 2.2 可知，可推送视图的 LCP 可能是 `/`，例如跨一级目录的 Compose。`path` 只是队列坐标，不赋予"被推路径"语义。
   - `operation_id = "<filter_id>:<old_v>→<new_v>"`。同一指纹的重试走 B1 收养。指纹不同时，3.8 的部分唯一索引会拒绝并发推送，并提示客户端 fetch。
   - `enqueue_atomic` 要按索引名把这次唯一冲突映射为新的拒绝原因（例如 `ActiveViewPushConflict`）。【代码】现在只有 `push_queue_active_push_path` 会被映射为拒绝（约 L238）；其余唯一冲突会回查同指纹行，查不到就返回 "unique conflict without classifiable row"（约 L268），正常的并发拒绝会因此被报成系统错误。

**B3（在 `MONO_WRITE_LOCK` 内，单事务）**
1. **读根。** 读出锁内的根 R（commit 与 tree），与 B2.5 的基线比对，沿用现有逻辑。
2. **只读补算，得到 `tip_at_R(F)`（4.5）。** 读取已提交的根链与 commit_map，从 `map(F, projected_seq)` 出发，在内存中把 F 的投影补算到 seq(R)。
   - **线性与连续。** R 还没进根链表时，在内存中沿首父链回溯补齐；只要不满足 3.3 的连续性或线性要求，就 fail-closed。启用附录 E 之后，多父提交可能已经作为普通行进入根链表，回溯遇不到它，所以另按 4.5 的非线性闸门，在下文‘同快照读取’的同一条 SQL 中读根链表的非线性标记，结果为真就拒绝。这项检查先于补算、第 3 步断言与 NFF。根链一旦出现多父提交，不论它是否已入表、投影是否已追平到它，view_push 一律 fail-closed，直到另立 ADR 定义非线性情形下的反向规则。
   - **锁内回溯的判定。** 与 3.3 第 3 步相同：遇到的第一个根链表内提交必须是链尾。B3 不取根链锁，所以回溯和读链尾放在同一条 SQL 中，在同一个快照内完成，以免与后台的并发接入交错而误判。回溯超过 `views.max_in_lock_catch_up` 时，只返回可重试错误，不判为不连续。
   - **同快照读取。** 第 2、3 步用到的派生状态都在一条 SQL 中读出，读取范围和理由见 4.6。`ready_seq` 为 NULL 时，以可重试错误结束。缺对象的处理见下一条。
   - **缺对象。** 第 2 步的补算、第 3 步的断言与第 4 步的 unapply 中，任何一次 tree 读取返回 `MissingObject`（4.1），都以可重试错误结束本轮：不判为第 3 步的派生状态损坏，不按 NFF 以 fetch first 拒绝，也不当作路径不存在。缺的如果是投影该提交时需要读取的根一侧 tree（4.1），catch_up 会停在同一个提交处（4.4），这与 4.2 失败时"5.2 第 2 步的补算 fail-closed"一致。缺的如果是 tree(R) 闭包中投影不读取的子树（第 4 步 unapply 与 2.7 第 3 项比较时会读到的视图内部子树），catch_up 照常推进，该视图打包时在 6.3 报缺对象。两种情形本轮都另行告警；缺的如果是 tree(new_v) 一侧的对象，按 4.6 提示 fetch 后重推。第 7 步解析续接候选时缺对象，按第 7 步第 2 条回滚本轮。第 10 步在内存中计算 projected tip 时缺对象，只在 sideband 中省略该 tip、提示稍后 fetch，不改变已落地的结果。
   - **不写表。** B3 事务**不写**任何 `mega_view_*` 表，也不取视图锁。原因是：这些表的唯一键与后台 worker 的未提交插入冲突时，Postgres 会让本事务等对方结束，全局写锁就会被拖住。
   - **补算上限。** 补算量超过 `views.max_in_lock_catch_up` 时，本轮以可重试错误结束。队列不做服务端重试（ADR-TP-04），客户端收到 ng 后重推。
3. **断言先于 NFF。**
   - **断言。** 在已持久化的水位处复核派生状态。令 `p = min(projected_seq, seq(R))`，要求 `filter_tree(F, tree(root_chain[p])) == map(F, p).view_tree`；如果 old_v 所在段的起点 `seq_from ≤ p`，还要求 `filter_tree(F, tree(root_chain[seq_from]))` 等于该段的 `view_tree`。任一项不等，都说明已持久化的派生状态已损坏：fail-closed，告警并拒绝。内存补算出的那一段由构造保证一致，复核它没有意义，所以复核对象是已持久化的段。
   - **NFF。** **只有 `tip_at_R(F).view_commit == old_v` 才继续第 4 步**；不等时以 "fetch first" 拒绝，并附上 ADR-TP-18 风格的对齐提示。old_v 不为 ZERO（5.1），所以 tip 为 NULL 段时必然拒绝。NFF 不读 `view_tip(F)`（4.5）。
   - **为什么只改视图外时不需要 rebase。** 根只在树变化时前进（ADR-TP-16），视图外的变更又会被 J4 丢弃。所以根只改了视图外的文件时，`tip_at_R(F)` 仍等于 old_v，推送照常接受。
4. **unapply。** 计算 `T_new = unapply_tree(F, tree(new_v), tree(R))`（2.6），并执行 2.7 的三项校验。
5. **净零（`T_new == tree(R)`）。**
   - 照常执行一次同值根 CAS，保证根更新的唯一性，tripwire 不缺席；
   - 不产生根提交，也不产生任何续接提交；
   - 该行终态为 Done，`landed_commit_id = R.commit`；
   - C 段照常执行第 9 步（见第 9 步"净零轮次同样执行"）；
   - report-status 返回 ok，sideband 写明"未落地，视图 tip 仍为 <old_v>，请 fetch 后 reset"。

   这偏离了 ADR-TP-16 的"被推路径仍记录历史"，因为视图没有可以记录历史的路径；需要在 trunk-push.md 中登记为 view_push 的例外。客户端推上来的链只保留在 push_queue 行与 `mega_view_pushed_commit` 中，二者都是 GC root。
6. **落地根提交。** 用 `trunk_provenance` 合成根提交 m：
   - **身份。** author 取 new_v 的 author；committer 的姓名、邮箱取 new_v，时间按 ADR-TP-14 计算；用服务端签名；parent 为 R。
   - **message。** N=1 时取 new_v 的 body；N>1 时完整列出视图链上的提交，这份完整枚举落在根提交 m 上。
   - **trailer。** 写 `Mono-View: <filter_id>`、`Mono-View-Range: <old_v>..<new_v>` 和 `Mono-Squash-Count`。不写 `Mono-Path` / `Mono-Squash-Range`：这两个 trailer 的语义是 L0 路径坐标，而视图提交不在 mega_commit 中。枚举中的 id 是视图坐标，可以通过 `GET /api/v1/views/{filter_id}/commits/{id}` 解析（6.6）。
   - **写 tree。** B3 只写入 unapply 在锁内新生成的 O(深度 × 成员数) 个 tree，写入前按 L0 规则（`sort_git_tree_items` + `from_tree_items_with_kind`）构造并校验哈希。lifted 中原样复用的 tree 已在 A 段写入。
   - 最后做根 CAS，写入 `main@/`。
7. **维持物化链：只续接，不创建。**
   - **受影响集合 A。** A 由三部分组成：src_paths(F) 中各源路径已物化的严格祖先（不含 `/`）、各源路径自身，以及它们已物化的后代。`/` 由第 6 步的根 CAS 覆盖。
   - **续接规则【决策】。** 按路径深度升序处理 A 中已物化的 `main` 行。对每一行 p，令 `new = resolve(T_new, p)`，`old = resolve(tree(R), p)`：
     1. `new == old`：本轮该子树没有变化。跳过该行，并把 p 加入剪枝集合，其后代一律跳过；由 Merkle 性质，后代子树也没有变化。
     2. `new` 所在路径不存在：写墓碑。2.7 第 3 项保证源路径及其祖先不会消失，所以只有源路径的后代会走到这一条。解析途中如果缺少 tree 对象，就 fail-closed、回滚本轮，不按"路径消失"处理。
     3. `new == ref_tree_hash`：只跳过该行本身，**不**加入剪枝集合。
     4. 其余情形：合成续接提交，parent 为该行的旧 tip，tree 为 `new`。如果同时 `ref_tree_hash ≠ old`，说明该行在本轮之前就已陈旧，续接顺带修正了它；此时计入指标 `view_push_stale_healed`，并记一条 warn。
   - **续接提交的 message。** N=1 时取 m 的 body。N>1 时用 view_push 专用的 compact 形态：写 `Mono-View: <filter_id>`、`Mono-Squash-Commit: <m>` 和 `Mono-Squash-Count`，不写 `Mono-Path` / `Mono-Squash-Range`，正文也不写 "Squashed on the pushed path"。所以必须先合成 m，再合成各层的续接提交。现有的 `trunk_provenance::compact_message` 固定写入 L0 路径坐标的 trailer，不能直接复用；为此给 `DescendantCommitStyle` 新增 `ViewPush{plan, sign}` 变体。
   - **保证范围【决策】。**
     - 本轮之前满足 I3 的行，提交后仍满足 I3；本轮子树有变化的陈旧行，由续接修正。
     - 本轮子树没有变化的陈旧行保持原状，不归 view_push 负责。这类行只可能来自绕过 I5 的写入或升级前的遗留，因为 ADR-TP-20 第 1 项的插入校验已经排除了正常竞态。它们由 ADR-TP-20 的第 2 项（push、merge、产品写 API 在消费 `main@P` 之前做 TP-11 断言与墓碑修复）和第 3 项（`mono_write_audit.rs` 持锁分批巡检）处理。
     - I2（被推路径的 tip 就是客户端提交）不适用于 view_push；I2a 在 view_push 下没有"被推路径"这个例外，所有层都以树是否变化为条件。
   - **不创建任何未物化的行，也不调用 `apply_push_in_txn`。** 原因：
     - LCP 为 `/` 时它会失败：`/` 层先命中 `clean == path_p` 分支；
     - 它会无条件 upsert `main@LCP` 和无父的祖先行，既不查墓碑，也不遵守 GC-FU-04。

     `build_result_by_chain` 虽然是纯函数，这里同样用不到。
   - **不复用 `advance_descendant_refs_with` 的剪枝，也不对 `/` 调用它。**
     - 【代码】它先用一条 `LIKE '<path>/%'` 查询，取出 path 之下全部已物化的 main 行，在内存中按深度排序（`mono_storage.rs::descendant_main_refs` 约 L594–602，`list_descendant_main_refs_on` 约 L604–625）。如果以 `/` 调用，锁内开销会随全仓物化行数 M 增长，与视图的写入面无关；`max_in_lock_catch_up` 管不到这一部分。
     - 【代码】只要新旧树在某一层的子树相同，`resolve_descendant_tree` 就返回 `Unchanged`（约 L771–778）。子树有变化、但新哈希恰好等于该行的 `ref_tree_hash` 时，它把该行加入 `unchanged` 集合（约 L683–685），约 L669 随后跳过该行的全部后代。新 tree 对象缺失时，它返回 `Delete`（约 L762–764）。
     - 【推断】第二种剪枝的后果是：如果某个祖先行已经陈旧，而本轮的新子树恰好等于它的陈旧哈希，原本一致的后代行就会被漏掉，view_push 自己制造出 I3 违例。
     - 因此 view_push 使用按 A 过滤候选的专用变体，执行上述四条规则，提交样式仍走 `DescendantCommitStyle::ViewPush`。
   - **候选的取法（P2 必需）。**
     - 各源路径 s 已物化的严格祖先（不含 `/`）及 s 自身：用一条 `path IN (…)` 查询取出。多个源路径共享的祖先只处理一次，以满足 I2a。
     - 各源路径 s 已物化的后代：每个 s 一次 `LIKE 's/%'` 查询。2.2 要求源路径两两不相交，所以各次查询的候选互不重叠。
     - Merkle 剪枝用的旧树一律取 `resolve(tree(R), ·)`，不取 ref 行上的 `ref_tree_hash`。
   - **锁内成本。**
     - 候选行数为 |A|。候选查询共 1 + k 次，k 是源路径数，注册时就已固定。续接提交数和签名次数只取决于子树实际有变化的行。
     - 这与 `kind=push` 属于同一类成本：`kind=push` 只对被推路径 P 调用一次后代续接（`push_queue_service.rs` 约 L2769），祖先在 `apply_push_in_txn` 中逐个处理（`mono_api_service.rs` 约 L4002–4050）。
     - trunk-push.md 对 B3 成本上界的论证（约 L985），以及持锁告警与 watchdog（约 L988–992），对 view_push 照样适用。
     - 不按候选数设硬上限：候选数不会因重试而减少，硬上限会让大视图永远无法推送，`kind=push` 也没有这种上限。改为观测：新增指标 `view_push_b3_candidates`（即 |A|）和 `view_push_b3_lock_ms`；持锁时间过长时，按 trunk-push.md 的 `stuck_timeout` 告警处理。
   - **不做 TP-11 断言。** view_push 的 NFF 比较的是视图 tip，不以任何 `main@P` 作为闸门输入，所以锁内不对 A 逐行做 TP-11 断言，也不会因为 A 中的存量陈旧行拒绝本轮推送。逐行断言会让推送因为与它无关的派生异常而失败，还会把 O(|A|) 次树解析搬进全局锁。
8. **配套写入与出站事件（与 `kind=push` 对齐，按事务边界分两段）：**
   - **同事务内**（第 7 步与 `mark_done_if_running_in_txn` 之后、`txn.commit()` 之前）。这里只放必须与根 CAS 一起原子提交的记录：
     - 启用 `mst2.publication_enabled` 时，调用 `record_publication_in_txn(txn, operation_id, normalize_namespace(LCP), R, m, "view_push")`（`mono_storage.rs` 约 L1778）。【代码】该函数在同一事务内写入发布回执、`mst2_namespace_seq` 序号和 `mst2_publication_outbox` 行；同一 operation_id 重放时返回原序号。`kind=push` 的调用点在 `push_queue_service.rs` 约 L2797–2810，早于约 L2812 的 `txn.commit()`。
     - github-sync 的 Q1 outbox（`docs/refactoring/github-sync.md` Q1；plan-20260920 尚未实现）：变更集取第 7 步中实际续接（advance）或写墓碑删除（delete）的 `main@P` 行，并且只取 binding 按路径精确命中的行，在同一事务内插入。view_push 不创建行，所以没有 upsert 类变更；子树未变而跳过的行和根 `/` 不进入变更集。
   - **事务提交成功之后、第 9 步 C 段之前**，尝试发出一次存储事件：沿用 `repo.push`，或者在 storage-events.md 中新增 `view.push` 并写进固定的 wire schema。
     - 事件快照只取本轮的行和结果。builder 失败时只调用 `record_invalid_event`；`try_emit` 返回任何非 accepted 的结果，都不改变已提交的落地结果。
     - 【代码】这是 storage-events.md「来源适配：`repo.push`」（约 L51）中的现有契约：`push_queue_service.rs` 约 L2812 提交事务，约 L2813–2843 构造事件并调用 `try_emit`，约 L2844 才进入 `run_c_segment_index`。
     - 如果在事务内发出事件，回滚的轮次和 fencing 失败的轮次（`mark_done_if_running_in_txn` 命中 0 行）也会对外宣告"已落地"；接收方收到事件后回查，还可能读不到尚未提交的变更。
   - **选型约束（P2 定案）。** 【代码】`EventType` 是封闭的六值枚举（`src/jupiter/service/storage_event.rs` 约 L15–22），新增 `view.push` 属于 wire schema 变更。【推断】如果沿用 `repo.push`，会有两个问题：
     - `scope.repo_path` 只能填 LCP。按 storage-events.md 的路径过滤规则（约 L77），订阅某个源路径或其子路径的 target 收不到这个事件；LCP 为 `/` 时，只有过滤项为 `/` 的 target 能收到。
     - `old_oid` 与 `requested_oid` 是视图坐标下的 old_v / new_v，`landed_oid` 却是根提交 m，与 `repo.push` 现有字段的坐标语义不一致。
   - **不落地的轮次**（第 5 步的净零、B1 收养或重放、CAS 或 fencing 失败、回滚、fail-closed）不调用 `record_publication_in_txn`，不写 Q1 outbox，也不发事件。【代码】`kind=push` 的 n=0 分支（`push_queue_service.rs` 约 L2463–2521）同样既不写发布回执，也不发 `repo.push`。
9. **C 段**（`run_c_segment_index` 的 view_push 分支，见 4.4）。对每个源路径 s：
   - **索引。** 在 C 段执行时读取**当前**的 `main@/`，调用 `index_tree_blob_paths(resolve(当前根树, s), s, Queue{push_id})`（`blob_path_index.rs` 约 L167）。用当前根而不是固定的 tree(m)，是为了不让延迟执行的 C 段删掉后续推送已写入的出现对。
   - **跳过条件【决策】。** 只有当某个更晚的 Done 行（`id > 本行 id`）**实际索引的前缀覆盖 s** 时，才跳过本步。覆盖是指该前缀等于 s，或者在组件边界上是 s 的祖先（含 `/`）。
     - 各 kind 实际索引的前缀：push、merge、attach 是该行的 `path`，因为 `index_blob_paths_c_segment` 读的是 `main@path`；view_push 是其 `filter_pk` 对应的 `src_paths`，**不是**该行的 `path`，因为 LCP 只是队列坐标，第 9 步并不索引 LCP。
     - 更晚 Done 行的前缀如果只是 s 的后代，或只与 s 部分相交，就不跳过，因为它没有覆盖 s 之下其余路径的出现对。
     - 【代码】reconciliation 只删除传入前缀之下的行（`blob_path_index.rs` 约 L313–325），而每个任务读的都是执行时的当前树，所以覆盖关系可以传递：被跳过的任务，由 id 最大的覆盖任务代为索引。
   - **共享谓词同步修改。** 【代码】现有的 `same_path_has_later_done` 只按 `path` 相等匹配，不区分 kind（约 L197–209）。引入 view_push 后，如果某个更晚的 view_push 行的 LCP 恰好等于某个 push、merge 或 attach 的路径，这些轮次的 C 段也会被跳过，而那个 view_push 只索引了它自己的源路径。所以要把它改为上面的覆盖谓词（例如 `later_done_covers(s, push_id)`），四种 kind 共用。最小实现可以只在 push、merge、attach 行上匹配"相等或祖先"，并排除 `kind = 'view_push'` 的行，即永远不因 view_push 行而跳过，代价只是一次重复索引。
   - **净零轮次同样执行本步。** 这与 `kind=push` 的 n=0 分支一致：`push_queue_service.rs` 约 L2522 在净零的 Done 之后照常调用 `run_c_segment_index`。否则，一个净零的 Done 行会被当作覆盖者，使更早的轮次跳过索引，而它自己什么也没有索引。
   - **跳过只是优化。** 【代码】周期补偿任务每 60 秒按当前根全量重扫一次（`DEFAULT_COMPENSATE_INTERVAL`，`context/mod.rs` 约 L264）。所以跳过判断出错只会推迟收敛，不影响任何写入判定（ADR-TP-11）；谓词宁可保守。
   - s 在当前根树中不存在时（该源路径从未被创建），直接跳过。2.7 第 3 项禁止 view_push 删除源路径，所以这里不会遇到"被本次推送删除"的情形。
   - 不调用 `index_blob_paths_c_segment(LCP)`，因为它读的是 `main@LCP`：LCP 未物化时，它什么也不索引；LCP 为 `/` 时，它会在请求路径上遍历整个仓库。
   - 完成后，在同一挂点发出视图投影的追赶信号。
10. **返回。** 客户端收到 ok，并从 sideband 得知"视图已由服务端写为 <projected tip>，请 fetch 后 reset"（ADR-TP-18）。这里的 projected tip 是 `project_commit(F, m, tree(R), tip_at_R(F))` 得到的视图提交，在内存中计算；此时持久的 `view_tip(F)` 还没有前进，不能拿来填写。投影出的视图提交 ≠ new_v，因为 committer 时间、签名以及 N>1 时的 message 都不同。

### 5.3 相对 v0.1 删除的内容
- **不再逐提交反向映射，也不保留视图链的拓扑。** N>1 时压成一个根提交，与 trunk 的现有语义一致。因此 v0.1 的拓扑式 `unapply_filter`、`find_new_branch_base`、`mega_view_push_map` 和 merge 推送规则都不再需要。Josh 的对应语义记在附录 E。
- **不支持 `-o base= / create / allow_orphans / cl=`，所以 P2 不需要 push-options。** 以后如果需要，必须同时修改 `split_receive_pack_request`、`Capability` 枚举、按 handler 区分的能力宣告和 B3 payload（0.4-16）。
- **不走 CL。** `mega_cl` 和 `mega_cl_commits` 不加列，也不需要 `ViewUnapply` 合并策略；mega2 中本来就没有 `merge_strategy.rs`。

### 5.4 拒绝规则
| 情形 | 判定 | 处理 |
|---|---|---|
| 期间根只改了视图之外的内容（含投影尚未追上的情形） | `tip_at_R(F) == old_v` | 接受，基于锁内的 R 落地 |
| 根改了视图之内的内容，客户端未 fetch（含投影滞后、`view_tip(F)` 仍等于 old_v 的情形） | `tip_at_R(F) ≠ old_v` | 拒绝：fetch first |
| 派生状态与根树不一致 | 第 3 步断言失败 | fail-closed，告警 |
| 根链含多父提交（附录 E 启用后；不论 merge 是否已入表、投影是否已追平到它） | 第 2 步非线性闸门（4.5） | 拒绝，不可重试；只读投影照常 |
| 视图未就绪；新轮次的滞后量超过上限，或根链未追上 | 6.1 第 3 层 / B0 状态预检 / 第 2 步 | 可重试错误；同指纹的回放与收养不受滞后限制 |
| 推送内容落在视图值域之外 | 2.7 第 2 项 | 拒绝，并列出路径 |
| 推送会改动视图之外的内容（含同名文件覆盖被隐藏目录） | 2.7 第 1 项 | 拒绝 |
| 推送改动 import 命名空间、删除源路径，或新建 root_dirs 之外的路径 | 2.7 第 3 项 | 拒绝 |
| 链内 merge、孤儿链、新分支、old_v 为 ZERO | PushChain 规则 / 5.1 | 拒绝（ADR-TP-17） |
| 视图不可推送；token 没有覆盖全部源路径；匿名推送未开启 | B0 | 拒绝 |
| 同一视图已有在途推送 | 部分唯一索引 | 同指纹：收养；不同指纹：以 `ActiveViewPushConflict` 拒绝，提示 fetch |
| 净零 | `T_new == tree(R)` | 同值 CAS；不产生根提交；提示对齐 |
| 同一 (old_v, new_v) 重复推送 | B1 先于状态预检：Done 回放，Queued/Running 收养 | 返回首次的结果；视图未就绪或根链停追期间，6.1 第 3 层先返回 503，就绪后重推即可回放 |
| 服务端签名 key 不可用 | B3 签名失败 | 整轮失败（继承 trunk 现状） |

### 5.5 新增 kind 的配套改动
- **穷尽匹配。** 补齐 `PushQueueKindEnum` 的所有穷尽匹配：`execute_b3` 的分派、`run_c_segment_index` 的分派、`push_queue_storage` 中的 kind 字符串、metrics；同时补齐 `DescendantCommitStyle` 新增的 `ViewPush` 变体（5.2 第 7 步）。现有的 `b0_reject_push` 对非 push 的 kind 直接放行，所以要新增 `b0_reject_view_push` 与 `ViewPushExecContext`；失败类型新增 `ViewPushFailure`，或明确复用 `PushFailure`。
- **reaper。** 为 `push_queue_reaper::apply_i3`（约 L263）增加 view_push 分支：按 5.2 第 7 步的规则重算 A 集合，对其中已物化的行做 I3 校验与墓碑修复；A 中没有已物化的行时跳过，不告警。是否处理与 LCP 是否为 `/`、是否已物化无关。B3 是单事务，崩溃时不会留下 view_push 专有的半成品。
- **其余。** MST/2 发布回执、存储事件和 github-sync 的 outbox 见 5.2 第 8 步；文档修订见第 8 节。

---

## 6. 协议与 API

### 6.1 URL 与路由【决策】
| URL | 含义 |
|---|---|
| `/<path>.git` | **不变**：Monorepo 加物化链 |
| `/.filter/<filter_id_hex>.git` | 按 filter_id 访问 |
| `/.view/<name>.git` | 命名视图的最新版本；只供人工浏览 |
| `/.view/<name>@<version>.git` | 命名视图的固定版本；Agent 和自动化必须用这种形式 |

- **入口分层【决策】。** 视图 URL 的判定分为三层。HTTP、SSH 与 LFS 共用这三层，不在各个 handler 中各自实现。
  1. **定位符**（同步，纯字符串，不查库）。`path.rs` 新增 `classify_repo_locator(raw) -> RepoLocator`，其中 `RepoLocator::{Path(PathBuf), View(ViewLocator)}`，`ViewLocator::{Named{name, version: Option<u32>}, FilterId(String)}`。
     - **识别规则**：去掉前导 `/` 后，首段为 `.view` 或 `.filter` 就进入视图分支。SSH 的 scp 式路径不带前导 `/`，同样适用。
     - **语法错误一律 404**，不回落为 `Path`。错误指以下情形：`.filter/` 之后不是单段 64 位小写 hex；`.view/` 之后的名字不满足 3.2 的视图名规则；`@` 之后不是正整数。尾部 `.git` 可有可无，与 `normalize_repo_path` 一致。
     - **为什么只产出未解析的定位符**：`filter_pk` 和"最新版本"都要查表，而 `parse_git_protocol_path` 是只接收 method 与 URL 的同步函数（`path.rs` 约 L48）。【代码】同文件的 `is_disallowed_root_repo_path` 是按首段做同步判定的先例。
     - **调用方有三处**：`parse_git_protocol_path`（HTTP，返回值携带 `RepoLocator`）；`ssh.rs::parse_ssh_exec_request`（约 L441，SSH 有自己的解析器，不经过 `path.rs`）；`http_server.rs::rewrite_lfs_request_uri`（约 L888，判定 LFS 请求的仓库前缀）。
  2. **按服务与配置拒绝**（同步，不查库，早于认证）。HTTP 在 `handle_smart_protocol`（`http_server.rs` 约 L960）解析出端点与 `service` 参数之后执行；SSH 在 `exec_request` 解析完命令之后执行。对 `View` 定位符：
     - `[views].enabled = false` 时，所有服务都返回 404，不回落到 Monorepo；
     - P0–P1 中 receive-pack 一律拒绝。HTTP 的 `info/refs?service=git-receive-pack` 与 `POST …/git-receive-pack` 返回 403；SSH `git-receive-pack` 没有状态码，按本节‘错误契约’写入一行 `ERR view URLs are read-only`，以退出码 1 结束通道。这一步早于 `receive_pack_requires_http_auth`、`git_http_auth`、`check_push_permission`（0.4-15）与 `ssh_receive_pack_enabled` 检查（`ssh.rs` 约 L149），也早于读取请求体。【代码】现有的 `ssh_receive_pack_enabled` 拒绝（`ssh.rs` 约 L148–162）把消息写到 stderr；Libra 不回显 SSH 的 stderr（Libra `ssh_client.rs` 约 L1715–1733），所以视图路径不沿用它，改用 ERR。git 的推送握手同样见到 ERR 即以 `remote error:` 退出（`transport.c` 约 L352–356）。
     - LFS 一律拒绝：`rewrite_lfs_request_uri` 对视图前缀不改写 URI，请求落入 `/{*path}` catch-all（`any()`，约 L753），由 `parse_git_protocol_path` 以 NotFound 返回 404。它是同步的 `MapRequestLayer`，不能直接返回响应，而这是唯一能同时覆盖 `lfs_router` 与 `lfs_media` 全部处理函数的位置。SSH 的 `git-lfs-authenticate` 与 `git-lfs-transfer` 对视图路径发送一行 `ERR view not found`、退出码 1，即错误契约「不存在」行的 SSH 形式，与 HTTP LFS 的 404 同类。本条与 `enabled` 无关，因为保留名始终生效。
  3. **视图解析**（异步，查库）。在 `src/contract/git_protocol/mod.rs` 新增 `resolve_view_target(state, &ViewLocator) -> Result<ResolvedView, ProtocolError>`，与 `check_upload_pack_access` 并列。
     - **调用顺序**：都在 `check_upload_pack_access` 之后调用，并且早于读取请求体和任何宣告。具体位置：HTTP `git_upload_pack` 中，位于 `check_upload_pack_access`（`http.rs` 约 L238）与 `collect_body_data`（约 L239）之间，先解析视图、再读请求体；`git_info_refs` 中，位于认证分支之后、v2 能力宣告的短路（约 L75）之前；SSH `exec_request` 中，位于 `check_upload_pack_access`（`ssh.rs` 约 L172）之后，早于 v2 短路（约 L180）与 `git_info_refs`（约 L193）。【代码】SSH 不需要额外处理：exec 被拒时不登记通道状态，之后到达的数据由 `data` 直接丢弃（`ssh.rs` 约 L349–355），不会缓冲。【推断】这个顺序不改变最坏情况下的内存占用，因为 `GIT_HTTP_MAX_BODY_BYTES`（4 GiB，`http.rs` 约 L173）对所有 upload-pack 路径一视同仁。它的作用是让未知、被禁用或未就绪的视图稳定地得到 404/503，而不是先缓冲请求体，再因读取失败得到 400。
     - **它依次做以下几件事**：
       - 按名称加版本查 `mega_view`（省略版本时取最大的 version），或按 filter_id 查 `mega_view_filter`，得到 `filter_pk`；查不到时返回 404；
       - `algo_version` 不受支持，或者 `object_format` 与当前部署不一致时，按 3.1 拒绝服务并告警；
       - 按 2.1 复核 canonical_spec 能否往返、filter_id 重算是否一致，任一不成立就按 3.1 拒绝服务并告警。这类防御性拒绝在错误契约中按"暂不可用"应答，reason 写作 `definition corrupt`；
       - 视图处于回收态（`ready_seq`、`warming_since` 均为 NULL 且 `projected_seq = 0`）时，按 6.5 执行读者触发的重新预热准入（第 1、2、4、5、6 步），无论准入成败，本次都返回 503；
       - （P1）视图就绪时，按 4.6 节流刷新 `last_access_at`；
       - `ready_seq` 为 NULL，或者 `root_chain_halted`（3.3）为真时，返回 503 加 `Retry-After`。SSH 按本节"错误契约"写入一行 ERR，并以退出码 75 结束通道；现有 SSH 错误路径写的是裸文本 `error: …`（`ssh.rs` 约 L584），不能用作视图状态的协议应答。
     - **结果的去处**：解析结果挂在 `SmartSession` 新增的 `view: Option<ResolvedView>` 字段上。`repo_path` 保留，`SmartSession::new` / `from_state` 的签名不变，现有约 30 处构造点不受影响。`repo_handler_with_commands` 先看 `view`：有值就构造 `ViewRepo`，否则沿用 import_dir 与 Monorepo 两个分支。
- **认证先于解析。** `anonymous_access = false` 时，未认证的请求先被 `check_upload_pack_access` 拒绝，既不会触发查库，也不能借 404/503 探测视图是否存在。receive-pack 的拒绝（HTTP 403；SSH 为一行 ERR 加退出码 1）在第 2 层按定位符直接给出，不需要查库。
- **逐请求解析。** HTTP 是无状态的：v2 的 `GET info/refs`、`POST ls-refs`、`POST fetch` 各自重新解析；SSH 每个 exec 解析一次。就绪状态可能在两次请求之间变化，所以 ViewRepo 还要再检查一次（4.4、6.3）。【代码】现有 `From<MegaError> for ProtocolError` 只把 `MaterializeAborted` 映射为 503，其余一律映射为 400（`src/common/errors/mod.rs` 约 L372–392），因此要新增 `MegaError::ViewUnavailable`、`MegaError::ViewPackRejected` 与对应的 `ProtocolError` 变体，前者映射为 503 加 `Retry-After`，后者映射为 200 加一行 ERR（见下条"错误契约"）。
- **错误契约【决策】。** 视图 URL 上的协议错误分四类，HTTP、SSH 与 v0、v2 统一按下表应答。错误以类型化变体传递，不靠字符串匹配：`MegaError::ViewUnavailable { filter_id, reason }` → `ProtocolError::ViewUnavailable`；`MegaError::ViewPackRejected(msg)` → `ProtocolError::PackRejected(msg)`；不存在与只读沿用 `ProtocolError::NotFound` / `Forbidden`。

| 类别 | 触发点 | HTTP | SSH（v0 / v2） | git 的表现 | Libra 的表现 |
|---|---|---|---|---|---|
| 不存在：定位符语法错、未知视图、`enabled=false` | 6.1 第 1–3 层 | 404 | 一行 `ERR view not found`，退出码 1 | `repository '…' not found`；SSH 为 `remote error: view not found` | `status code: 404`；SSH 为 `SSH exited with status 1` |
| 只读：P0–P1 的 receive-pack | 6.1 第 2 层 | 403 | 一行 `ERR view URLs are read-only`，退出码 1 | `returned error: 403`；SSH 为 `remote error: view URLs are read-only` | `status code: 403`；SSH 为 `status 1` |
| 暂不可用：未就绪、回收中、根链停追 | 6.1 第 3 层；`refs_with_head_hash`、`check_wants_and_ready`、`prepare_pack` 复查 | 503 加 `Retry-After`（正整数秒） | 一行 `ERR view <filter_id> unavailable: <reason>`（reason 为 `warming up` 或 `root chain halted`），退出码 75（EX_TEMPFAIL） | GET 为 `returned error: 503`，POST 为 `RPC failed; HTTP 503`；SSH 为 `remote error: view <filter_id> unavailable` | GET 按 `Retry-After` 有界重试后报 `HTTP 503`；POST 为 `status code: 503`；SSH 为 `status 75` |
| 打包预检失败：want 不属于本视图、L0 tree 不自洽、闭包缺 tree 对象 | want 不属于本视图：`check_wants_and_ready`，每轮都查；L0 tree 不自洽与缺对象：`prepare_pack`，只在发包轮（6.3） | 200，`Content-Type: application/x-git-upload-pack-result`，响应体只有一行 ERR | 宣告之后只有一行 ERR，退出码 1 | `remote error: <msg>` | `remote reported an error: <msg>` |
| 其余协议错误（能力守卫、请求格式等） | 共享协议代码 | 沿用现状（400） | 沿用现状（裸文本，属既有缺陷，另立任务） | — | — |

  - **HTTP 的预检失败为什么用 200 加 ERR。** 【代码】git 不显示 POST 失败的响应体，只报 `RPC failed; HTTP <code>`（git `remote-curl.c` 约 L872）；GET 失败时也只显示 `text/plain` 响应体（同文件约 L370–379），而 `ProtocolError` 的响应体是 `CommonResult` JSON（`src/common/errors/mod.rs` 约 L381–399）。能把 tree_id 交到客户端的，只有 200 响应里的 ERR 包：git 的 v0、v2 读取器都带 `PACKET_READ_DIE_ON_ERR_PACKET`（`fetch-pack.c` 约 L374、L1731；`pkt-line.c` 约 L508），见到 ERR 即以 `remote error: <msg>` 退出；Libra 在 PACK 之前见到 `ERR ` 即报 `remote reported an error`（Libra `src/command/fetch.rs` 约 L3158–3163）。git 自己的 upload-pack 对 `not our ref` 也是这样应答。状态类错误发生在任何字节写出之前，仍用状态码；响应体格式不变，判定以状态码为准。
  - **SSH 为什么用 ERR 加退出码，不用 stderr。** SSH 没有状态码。【代码】git 在 SSH 握手（`transport.c` 约 L352–356）和 fetch 读取中见到 ERR 即退出，并显示原文。Libra 不回显 SSH 的 stderr（Libra `ssh_client.rs` 约 L1715–1733，"SSH diagnostics withheld"），只报退出码（约 L1467–1479、L1536–1548）；它的宣告解析没有 ERR 分支（`src/internal/protocol/mod.rs` 约 L185–300），但能识别数据阶段的 ERR。所以 ERR 为 git 的所有阶段和 Libra 的数据阶段提供原文，退出码让 Libra 在宣告阶段也能判定类别（75 可重试，1 不可重试）。【推断】Libra 在宣告读到 EOF 后，只等 ssh 子进程 100 ms 来取退出码（`ssh_client.rs` 约 L385），所以 exit-status 必须在 eof/close 之前发出。
  - **SSH 的发送顺序。** exec 阶段：`channel_success` → 一行 ERR → `exit_status_request(码)` → `eof` → `close`，不登记通道状态。【代码】必须先 `channel_success`：`ssh.rs` 约 L150–152 的注释说明，CHANNEL_FAILURE 只会让 OpenSSH 报 "exec request failed" 并丢弃载荷。数据阶段（v0 upload-pack、v2 `ls-refs` / `fetch`）：写出一行 ERR 后不再写任何字节，并在通道状态上记下退出码；`channel_eof`（约 L418，现为固定的 0）按这个码发出 exit-status 后关闭通道。这些路径替换 `ssh.rs` 约 L584、L654、L669 的裸文本 `error: …`（git 对这种文本只会报 `bad line length character`），只对上表前四类生效。
  - **预检先于任何字节。** 【代码】HTTP 的 `protocol_buf` 只在 handler 返回 Ok 之后才进入响应流（`http.rs` 约 L252、L317–321）；SSH 也是返回 Ok 之后才写出（`ssh.rs` 约 L590、L678–682）。所以只要两个预检都在对应的写缓冲或打包方法之前完成即可：`check_wants_and_ready` 早于本轮任何 ACK、NAK 或 acknowledgments，失败时缓冲为空；v0 的 `prepare_pack` 同样早于 ACK 行（`smart.rs` 约 L273 起）；v2 ready 轮的 `prepare_pack` 晚于 acknowledgments 段（`v2.rs` 约 L246），失败时丢弃该段。应答中只有那一行 ERR 或 503。
- **P2。** 可推送视图的 receive-pack 在第 2 层放行，经第 3 层解析后，由 5.2 的 B0 按 `src_paths(F)` 授权。视图 URL 的路径永远不交给 `check_push_permission` 或 `token_covers_repo`，因为对 `/.view/...` 做路径前缀匹配没有意义。
- **保留名。**
  - 新增常量 `VIEW_URL_RESERVED_NAMES = [".view", ".filter"]`。不并入 `INIT_ROOT_RESERVED_NAMES`，因为 validate.rs 的测试要求后者中的每个名字都由 `init_trees` 实际创建。
  - `validate_monorepo_path_shape`（`validate.rs` 约 L500）拒绝 `root_dirs` 与 `import_dir` 的首段使用这两个名字；`check_write_operands` 与 `classify_creation_path` 拒绝写入这两个名字之下的路径。
  - 保留名与 `[views].enabled` 无关，始终生效。
  - 升级影响：`root_dirs` 已含这两个名字的部署会启动失败。`[views].enabled = true` 时，如果根树中已有这两个顶层条目，或 `mega_refs` 中有它们之下的行，启动 fail-closed，并给出迁移指引。
- **版本切换的后果。** `/.view/<name>.git` 总是解析到最新版本。新建一个 version 后，这个 URL 的历史与旧 version 无关（filter_id 不同，提交哈希一般全部不同），已有克隆再 fetch 时会遇到非快进更新。Libra 检测到 filter_id 变化时，提示用户重新 clone（R3）。

### 6.2 ref 广告
- **P0。**
  - `HEAD → refs/heads/main`，`refs/heads/main = view_tip(F)`；v2 的 `ls-refs` 同理（`symref-target:refs/heads/main`）。
  - 只有 tip 为 NULL 段时，才按空仓库应答，此时 `refs_with_head_hash` 返回 (ZERO_ID, [])：
    - v0：沿用现有的 `capabilities^{}` 伪 ref（`smart.rs` 约 L99–100）。这是 v0 规范中空仓库承载能力列表的形式。
    - v2：`ls-refs` 只返回 flush，不输出任何 ref 行。【代码】现有 `handle_v2_ls_refs` 在 head 为零 ID 时仍会写出一行 `<zero> capabilities^{}`（`v2.rs` 约 L72–88）。v2 的能力在 `info/refs` 中宣告，没有伪 ref 的约定；mega2 也没有宣告 `ls-refs=unborn`，所以同样不输出 `unborn HEAD` 行。
    - 【决策】P0 修改这一分支：head 为零 ID 时不写 HEAD 行。这是对共享代码的协议修正，`/<path>.git` 不存在的路径在 v2 下的输出也随之改变（第 8 节登记）。现有测试都没有断言这一行。【推断】在现行输出下，git 的 v2 clone 多半仍按空仓库处理，因为伪 ref 匹配不到 `refs/heads/*`；但 `git ls-remote` 会列出这一行。未经实测。
  - 视图未知、被禁用或未就绪时，在首次宣告就分别返回 404、404、503（SSH 按 6.1 错误契约返回一行 ERR，退出码依次为 1、1、75），v0 与 v2 一致（6.1 第 2、3 层；4.4）。
- **Tag（P1）。** mega2 的 tag 是路径级的（`mega_tag.path`）。视图 tag 的选取规则在 P1 定案，例如只取 `/` 上、目标在根链上的 tag，并按 `map(F, seq(target))` 投影。

### 6.3 fetch 协商
- **ViewRepo 实现 `RepoHandler`**（`src/ceres/pack/mod.rs` 约 L50–566）。【代码】trait 中没有默认实现的方法共 17 个，ViewRepo 必须全部实现；其余方法沿用默认实现。P0–P1 的 receive-pack 已在入口被拒绝（HTTP 403，SSH 为 ERR；6.1 第 2 层），表中接收侧的方法不可达，实现它们只是为了通过编译，并作纵深防御。

| 方法（trait 中约 Lxxx） | 默认实现 | 现有调用方 | ViewRepo 的处理（P0–P1） |
|---|---|---|---|
| `is_monorepo`（L51） | 无 | receive-pack 守卫（`smart.rs` 约 L515） | 返回 true。P0–P1 不可达；P2 沿用 monorepo 守卫：只接受 `refs/heads/main`，拒绝 tag 与删除 main |
| `object_hash_kind`（L54） | 无 | `unpack_stream`（receive-pack） | 返回部署的 hash kind；视图的 `object_format` 是否一致，由 6.1 第 3 层校验 |
| `refs_with_head_hash`（L97） | 无 | v0 `git_info_refs`（`smart.rs` 约 L97）、v2 `ls-refs`（`v2.rs` 约 L67） | 先重读本视图的 filter 行：`ready_seq` 为 NULL 或根链停追时，返回 `MegaError::ViewUnavailable`，不得返回 (ZERO, [])。否则按 4.4 做有界同步追赶后返回 `view_tip`：NULL 段返回 (ZERO_ID, [])；其余返回 HEAD = tip，以及一条 `default_branch = true` 的 `refs/heads/main` |
| `check_wants_and_ready`（新增，返回 `Result<(), MegaError>`，默认 `Ok(())`） | 有 | 每个带 want 的请求都调用，早于本轮任何 ACK、NAK 或 acknowledgments。`smart.rs` 在能力守卫（约 L236–254）之后、`if have.is_empty()`（约 L256）之前调用；`v2.rs` 在能力守卫（约 L192–221）之后、`if !done`（约 L234）之前调用，因此也早于“无 ACK 即结束本轮”的返回（约 L247–255）。v0 中 want 为空的分支（约 L230–234）只回 NAK，不调用 | 用一条 SQL 读出本视图的 filter 行，以及各 want 在 commit_map 中的 `seq_from`。未就绪或根链停追时，返回 `ViewUnavailable`。任一 want 不是本视图的视图提交，或 `seq_from > projected_seq`（晚于当前 tip）时，返回 `ViewPackRejected("upload-pack: not our ref <oid>")`。不计数、不读 tree，开销只与 want 数有关 |
| `prepare_pack`（新增，返回 `Result<(), MegaError>`，默认 `Ok(())`） | 有 | 只在本轮确定发包时调用。`smart.rs` 中在 v0 的两个发包分支开头调用，早于任何打包方法与 ACK 行：have 为空的分支在 `shallow_pack` / `full_pack` 之前调用；带 `multi_ack_detailed` 的分支在 `check_commit_exist` 循环之前调用，没有共同提交时也在 NAK 之后发包（约 L282–292）。have 非空而请求未带 `multi_ack_detailed` 的分支不发包，不调用本方法。`v2.rs` 中在“无 ACK 即结束本轮”的返回之后、`let pack_data`（约 L259）之前调用。P1 的 shallow / filtered 分支同样先调用 | 先用一条 SQL 读出就绪状态与水位、want 是否仍命中 `(filter_pk, view_commit)`、seq 区间内的视图提交 id（4.6 单快照规则）。未就绪、根链停追，或 want 已不在映射中（两次调用之间被回收）时，返回 `ViewUnavailable`。然后计数，并在计数中完成 L0 tree 校验与缺对象检查。计数结果（提交序列、待发对象 id 与总数；对象 id 按是否取自 `mega_view_object` 分为两组）留在本次请求的 ViewRepo 实例中。【代码】两个方法都必须单独设，并返回 `MegaError`：打包方法返回 `GitError`（`pack/mod.rs` 约 L258–264），`From<MegaError> for GitError` 会把错误压成 `CustomError(String)`（`errors/mod.rs` 约 L165–182），`smart.rs` 约 L267、L285 与 `v2.rs` 约 L280、L286 再一律映射为 `InvalidInput`（400），类型在途中丢失 |
| `full_pack`（L258） | 无 | v0/v2 中 have 为空的请求 | 只按 `prepare_pack` 留下的计数结果编码，tree 与 blob 按层批量读取，不再做校验。pack 头写出之后的读取失败按现状截断流，不在 6.1 错误契约的范围内 |
| `incremental_pack`（L260） | 无 | v0/v2 中 have 非空的请求；默认 `filtered_pack` 也回落到这里 | 要求同上。have 只认视图链上的提交，链外的 have 忽略。不复用 Monorepo 的逐父查询与 `unwrap` |
| `get_trees_by_hashes`（L302） | 无 | 默认的 `traverse*` | 先查 `mega_view_object`（kind = tree），其余回落到 `mega_tree`。使用 4.1 的可失败批量读取，缺对象时报错。`prepare_pack` 的计数要读遍待发闭包中的 tree，缺对象在这一步发现，返回 `ViewPackRejected("view <filter_id> pack aborted: tree <tree_id> is missing")`，按 6.1 错误契约的"打包预检失败"应答并告警，视图保持就绪 |
| `get_blobs_by_hashes`（L304） | 无 | 默认 `traverse`、ViewRepo 自己的打包 | 沿用 Monorepo 的 `storage.git_service.get_objects_stream(hashes)`（`monorepo.rs` 约 L633）；投影从不合成 blob |
| `get_blob_metadata_by_hashes`（L309） | 无 | 同上 | 沿用 `mono_storage().get_mega_blobs_by_hashes`（`monorepo.rs` 约 L640） |
| `check_commit_exist`（L316） | 无 | 协商时对 have 回 ACK（`smart.rs` 约 L275，`v2.rs` 约 L237）；receive-pack 的无包命令 | 只查 commit_map 的 `(filter_pk, view_commit)`，**不**查全局 `mega_commit`（0.5-19）。查询出错时返回 false，即不 ACK，代价只是多发一些对象 |
| `check_object_exist`（L321） | 无 | receive-pack 的无包 tag 命令 | 返回 false（返回值是 bool，没有错误通道）；不可达 |
| `check_default_branch`（L323） | 无 | receive-pack | 返回 true，与 Monorepo 相同；不可达 |
| `finalize_receive_pack`、`save_entry`、`check_entry`、`update_refs`、`update_pack_id`、`traverses_tree_and_update_filepath` | 无 | receive-pack；`traverses_*` 只在 Monorepo / ImportRepo 自己的 finalize 内部调用 | 返回只读错误（`check_entry` 返回 `GitError::CustomError`）。P2 的 view_push 也不用 `traverses_tree_and_update_filepath`，它的 C 段按源路径建索引（5.2 第 9 步） |
| `receiver_handler`、`unpack_stream` | 有 | receive-pack | 沿用默认：第一个条目就会被 `check_entry` 拒绝 |
| `save_entry_concurrency`、`receive_pack_extra_timings_ms`、`sync_commands_after_unpack`、`receive_pack_notice`、`bind_tip_after_receive` | 有 | receive-pack | 沿用默认，P2 另定。`bind_tip_after_receive` 默认为 true，而客户端推送的 new_v 不在 `mega_commit` 中，绑定语义要在第 5 节单独定义 |
| `supports_shallow_fetch`、`supports_filtered_fetch` | 有，返回 false | 协议层的能力守卫 | 沿用默认（见下一条"能力"）；P1 覆写 |
| `shallow_pack`、`filtered_pack` | 有，静默回落到 full/incremental | depth / filter 请求 | 沿用默认：`supports_*` 为 false 时协议层会先行拒绝，静默回落的分支不可达；P1 与 `supports_*` 一起覆写 |
| `traverse`、`traverse_for_count`、`traverse_trees_only*` | 有 | Monorepo 的打包 | 保留默认实现，ViewRepo 的打包不调用：它们每遍历一棵 tree 就发一次子 tree 查询，不满足"按层批量"的要求；对 gitlink 的处理也不对（0.5-18a） |
- **能力（P0）。** 沿用现有的"能力诚实"模式：ViewRepo 不覆写 `supports_shallow_fetch` / `supports_filtered_fetch`，二者默认返回 false。客户端请求 depth 或 filter 时，`smart.rs` / `v2.rs` 已有的守卫会返回明确的错误，普通的 v0/v2 clone 不受影响。
  - P1 实现 shallow（按视图链的深度计算）与 `filter=blob:none`。届时可以顺带把 `UPLOAD_CAP_LIST` / `V2_CAPABILITIES` 改成按 handler 取值，对应 `v2.rs` L18–21 中"只宣告已实现的能力"这条约束。
- **want 校验。** want 必须是该视图的视图提交（命中 `(filter_pk, view_commit)` 索引），并且不晚于当前 tip。这项校验用于保证正确性和裁剪上下文，不是安全边界（0.3-12）。不满足时，由 `check_wants_and_ready`返回 `ViewPackRejected`，按 6.1 错误契约应答一行 `ERR upload-pack: not our ref <oid>`，措辞与 git 自身的 upload-pack 一致。这项校验逐轮执行，不能只放在发包之前。【代码】v2 的协商轮（请求不带 `done`）没有 ACK 时，`handle_v2_fetch` 写完 `acknowledgments` 与 `NAK` 就返回（`v2.rs` 约 L234–255），走不到 `let pack_data`（约 L259）。如果只在发包前校验，非法 want 会先收到正常的 NAK，直到带 `done` 的那一轮才得到 ERR。【推断】git 的 upload-pack 在解析请求参数时就校验 want，早于 acknowledgments；本轮没有核对 git 源码。对象计数与 L0 tree 校验开销大，而且只对最终的 have 集合有意义，所以仍只在发包前执行（`prepare_pack`）。
- **打包：**
  - 视图链是线性的，按 seq 区间一次读出全部视图提交；
  - have 侧的排除，沿视图链对相邻视图提交逐个做 tree diff，不对 have tree 做全量遍历；
  - tree 与 blob 批量读取，每批不超过 1000；
  - 不复用 `incremental_pack` 的逐父查询和 `unwrap`；
  - **L0 tree 校验【决策】。** 凡是取自 `mega_tree`、不在 `mega_view_object` 中的 tree，都要在 `prepare_pack` 的对象计数阶段校验 `ObjectHash::from_type_and_data_for_kind(kind, Tree, sub_trees) == tree_id`。
    - 不相等时返回 `ViewPackRejected("view <filter_id> pack aborted: tree <tree_id> does not match its stored entries")`，按 6.1 错误契约应答：HTTP 为 200 加一行 ERR，SSH 为一行 ERR 加退出码 1。应答中没有 ACK、packfile 段与 pack 字节。同时记日志，并计入指标 `view_pack_tree_mismatch_total`。
    - 视图保持就绪，不返回 503，因为这不是可重试状态。mega2 没有原始 tree 字节可供恢复（0.5-18a），P0 不做恢复。
    - 校验只是对已经读出的字节多算一次 SHA-1，不增加查询。`mega_view_object` 中的 tree 由投影按原始字节写入，不需要校验。
  - **gitlink【决策】。** `160000` 条目指向外部仓库的提交，ViewRepo 不计数、不入包，也不读对象存储（0.5-18a）。
- **发包与回收交错（P1）【决策】。** P0 没有在线的回收与清扫（P0 的回收与 rebuild 都在停服后执行，3.3、6.5），本条从 P1 起生效。
  - 【代码】HTTP 在 handler 返回之后才消费 pack 流（`src/contract/git_protocol/http.rs` 约 L247–281）；SSH 也在 `git_upload_pack` 返回之后才逐块写出（`ssh.rs` 约 L579–592）。`src/` 中没有超时层，响应时长没有上限。读者不持 L_V，也不持 L_G（4.6），所以在 `prepare_pack` 之后、发包过程中，视图都可能被回收或全局 rebuild。
  - 回收与 rebuild 立即删除的只有 commit_map 行与引用行（4.6）。`mega_view_object` 中的对象要先被清扫标记，再过 `gc_grace_secs` 才删除；`mega_tree` 中的 tree 与 blob 从不被视图 GC 删除。保护分两层：
    - commit_map 只在 `prepare_pack` 的那条 SQL 中读取，打包方法不再读它；
    - 打包方法先读出并写出取自 `mega_view_object` 的对象（视图提交与脊柱 tree），再处理 `mega_tree` 中的 tree 与 blob。依赖宽限期的读取因此集中在发包开头，不会随 blob 的传输时长拉长。
  - 【推断】回收如果提交于 `prepare_pack` 的 SQL 之后，清扫的标记只会更晚（4.6 清扫写入 `statement_timestamp()`）。所以从这条 SQL 起算，在 `gc_grace_secs` 之内完成的对象读取一定成功。
  - 读取超出这段时长、期间视图又恰好被回收并经历标记与删除两轮清扫的请求，按本节 `full_pack` 行"pack 头写出之后的读取失败"截断，同时记日志并计入指标 `view_pack_object_missing_total`。客户端重试时得到 503，视图按 6.5 准入重新预热。
  - P1 不加读租约。持有快照或 advisory 锁的租约，要在整个发包期间占住一个连接和一个事务，长事务还会拖住 VACUUM；单建租约表则需要心跳和过期清理。而它要避免的只是一次可重试的失败。指标持续非零时再评估。

### 6.4 Libra
- clone/fetch：Libra 走 v0，P0 就能用；P2 的推送也不需要 `-o`。Libra 可以在本地配置中记录 filter_id，供诊断使用。
- 【代码】Libra 能判定的视图错误：HTTP 靠状态码，其中 `info/refs` 的 503/429 会按 `Retry-After` 有界重试（Libra `https_client.rs` 约 L337–347）；数据阶段的 ERR 报 `remote reported an error`；SSH 宣告阶段的 ERR 目前只能经退出码判定，因为宣告解析没有 ERR 分支（`src/internal/protocol/mod.rs` 约 L185–300），stderr 也不回显。
- Libra 的 `sparse-view` 是客户端的只读显示过滤器，使用 gitignore 语法。它与服务端视图互补，二者不共享 filter_id。

### 6.5 视图注册（P0）
- **接口：**
  - `POST /api/v1/views {filter_spec, name?}`：返回 `{filter_id, name, version, ready}`。支持 `wait=true`，阻塞到 ready 或超时为止。
  - `GET /api/v1/views/{filter_id}`：返回 canonical 文本、`src_paths`、`push_enabled`、ready 状态、投影水位与滞后量。滞后量按 §4.5 计算；`main@/` 在 `views.max_append_walk` 个提交之内不能按 §3.3 第 3 步连上链尾时返回 null。
  - `GET /api/v1/views?name=<name>&version=<v>`：按名字查询，省略 version 时取最新版本，返回 filter_id。视图名可以含 `/`，所以名字只出现在查询参数中，不进入路径参数。
- **`[views].enabled = false` 时【决策】。** 本节的三个接口与 6.6 的 REST 读接口（P1）都不挂载，请求由路由层直接返回 404，与 6.1 第 2 层对视图 URL 的应答一致。【代码】先例是 Agent Capture：`mount_agent_capture` 按配置决定是否把 `agent_capture_router` 并入 `storage_only_routers_with`（`src/server/http_server.rs` 约 L769–777、L845–847；`src/api/api_router.rs` 约 L72–90）。关闭时的 404 由测试 `disabled_storage_only_does_not_register_agent_capture` 断言（约 L1583）。OpenAPI 文档也按同一开关取舍（`storage_only_openapi_doc` 约 L849、`trunk_openapi_doc` 约 L870）。不采用 MST/2 在 handler 内判定的做法（`snapshot_router.rs::ensure_enabled` 约 L306），因为那样会以"未就绪"应答，与视图 URL 的 404 不一致。由此：
  - 拒绝早于鉴权、请求体解析和任何查库，不写过滤器行、视图行或速率行，也不占冷启动名额；
  - `enabled` 是重启类字段（6.9），路由在启动时按它装配，与第 2 层每次请求从快照读出的值始终相同；
  - 由读者访问触发的重新预热（4.6）发生在 6.1 第 3 层，而第 2 层已先返回 404，所以关闭期间同样不会占用名额；
  - 关闭前已占名额的过滤器（`warming_since` 非空）保持原状，重新启用后由补偿任务接着预热（4.4 触发方式第 2 条）。
- **校验**（注册时全部完成）：
  - 规范化；
  - 2.2 的不相交约束与源路径约束；
  - 拒绝 canonical 形式为 `:nop` / `:empty` 的过滤器，以及 `src_paths(F)` 为空的过滤器；
  - 要求 `object_format = sha1`；
  - 计算 `push_enabled`。
- **鉴权：**
  - 对 `src_paths(F)` 中的每一个源路径调用 `api::api_write_auth::authorize_trunk_api_write`，全部通过才放行，与 5.2 B0 一致；
  - `push_auth=none` 时，默认关闭注册（`views.allow_anonymous_register = false`）。因为每次注册都会触发一次 O(H) 的冷启动，并持久化最多 H 段映射，与产品写 API 那种有界开销不可类比；
- **过滤器规模上限【决策】。** 以下上限都是代码常量，不进配置；超限返回 400，并指明超了哪一项。它们只约束注册：worker 与 6.1 第 3 层加载已有定义时不复查，所以日后调低常量不会让已注册的视图失效。
  - `filter_spec` 经 JSON 解码后不超过 16 KiB（UTF-8 字节）；该路由另设 `DefaultBodyLimit::max(64 KiB)`。【代码】storage-only 的 `/api/v1` 没有单独的请求体上限，沿用 axum 默认的 2 MiB；`snapshot_router.rs` 约 L62、L76 以 `JSON_REQUEST_LIMIT = 131_072` 收紧，是先例。
  - `:[` 的嵌套层数不超过 16。解析器在递归下降时计数，超限立即报错，早于规范化与 pull 计算。【推断】不设此限时，2 MiB 的 `:[:[:[…` 会让递归实现在 tokio 工作线程上栈溢出，进程直接中止。
  - 规范化后，全部 Compose 的成员总数不超过 64，Exclude 选择器总数不超过 256。注册期以 `k = 64` 有界物化全过滤器与每个 Compose 成员的最小源路径集：活动集合超过 k 时立即以 K 超限拒绝（`actual = 65` 是下界），不为求最终 `|src_paths(F)|` 而继续展开。完成这一步后，对每个 Compose 节点的逆过滤器成员以同一 k 有界物化输出路径；任一活动集合超限同样以 K 拒绝。两个坐标的受限物化都早于相交检查，先检查输出路径相交，再检查源路径相交。于是每个获准过滤器都满足 `|src_paths(F)| ≤ k`，但某些最终会归约到 k 以内的输入也会被保守拒绝。k 决定三处开销：逐源路径授权的次数（本节与 5.2 B0）、2.7 授权面复核的选择器数、P2 锁内的 1 + k 次候选查询（5.2 第 7 步）。成员数决定每个提交 O(深度 × 成员数) 的脊柱 tree（3.5）。
  - 这组数值在 P0 基准中复核：用顶到上限的过滤器测单提交投影耗时与脊柱 tree 数，结果写入 7.2；超出预算就下调常量。
- **解析栈安全上限（HP-02）。** `parse` 与 §2.1「往返要求」第 3 项的加载复核受栈安全上限 `PARSE_MAX_NESTING = 64` 约束：`:[` 嵌套超过它的定义行复核为 `DefinitionCorrupt { failed: RoundTrip }`，不会让 worker 或协议处理线程栈溢出；它不是本节规模上限，不在注册时施加，经注册写入的定义嵌套不超过当时的 `REGISTER_MAX_NESTING`（当前 16），复核不会因它失败；它只能调高，且不得低于历次发布的 `REGISTER_MAX_NESTING` 最大值（当前 16），调低会让已注册的定义失效，与本节「日后调低常量不会让已注册的视图失效」相悖，只能按 §2.4 升到 `v2` 或强制重建受影响的视图。
- **注册准入【决策】。** 跨副本的计数与预留放在同一个 Postgres 短事务中，用 L_R（4.6）串行化。处理顺序为：规范化与 2.2 校验 → 逐源路径鉴权 → 准入事务。鉴权在准入之前，因此未通过鉴权的请求不会消耗任何 token 的配额。
  - **准入事务：**
    1. 执行 `pg_advisory_xact_lock(VIEW_LOCK_NS, REGISTER_KEY)`。READ COMMITTED 下此后每条语句各取一次快照，能看到此前所有已提交的准入。
    2. 按 filter_id 查 `mega_view_filter`，判断本次是否需要冷启动。需要的情形只有两种：行不存在；行已被回收，即 `ready_seq` 与 `warming_since` 都为 NULL 且 `projected_seq = 0`。P0 中这种状态只由运维的停服清理与 rebuild 产生，P1 起回收也会产生；本分支在 P0 就要实现。过滤器已就绪或正在预热时不需要名额；只为已有过滤器新建 version 的命名注册也不需要。
    3. **速率。** 本次会新建过滤器行、新建视图 version 或重新预热时，计入速率：执行 `SELECT count(*) FROM mega_view_register_log WHERE requester = $1 AND created_at > now() − 3600 s`，结果达到 `register_rate_per_token` 即拒绝。requester 取 `authorize_trunk_api_write` 的返回值，即 token 名（validate 保证唯一，`validate.rs` 约 L549–561）；`push_auth=none` 下，所有匿名注册共用 `anonymous` 这一个桶。命中已有定义、什么都不新建的幂等注册不计数。
    4. **总数。** 需要冷启动时，统计 `warming_since IS NOT NULL OR projected_seq > 0` 的行数，达到 `max_filters` 即拒绝。这里统计的是占有派生状态的过滤器，不是定义行：定义行永不删除（4.6），按行数计，P1 回收之后名额就永远收不回来。
    5. **名额。** 需要冷启动时，统计 `warming_since IS NOT NULL` 的行数，达到 `max_concurrent_cold_starts` 即拒绝。
    6. **写入。** 插入新过滤器行（`warming_since = now()`，`ON CONFLICT (filter_id) DO NOTHING`），或者对已回收的行置 `warming_since = now()`（P1 起，在同一语句中置 `last_access_at = now()`，见 4.6）；需要时插入 `mega_view` 新 version；计入速率时，插入一行 `mega_view_register_log`，并删除本 requester 窗口外的旧行。提交后发 4.4 的进程内信号。
    由读者访问触发的重新预热（4.6）只执行第 1、2、4、5、6 步，不计速率。
  - **拒绝应答。** 第 3、4、5 步任一项不满足时，事务回滚，不写任何行，返回 429 加 `Retry-After`。速率超限时，取窗口内最早一行还要多久离开窗口，向上取整到秒，上限 3600 秒；其余情形固定为 30 秒。【代码】`ApiError` 不携带响应头，注册接口要自行构造响应，参照 `lfs_media.rs::map_media_error`（约 L79–85）。
  - **名额的释放与崩溃回收。** 名额记在数据库状态中，不是进程持有的租约，所以不需要心跳或 TTL。
    - 释放只有两处：视图首次就绪的那个 catch_up 批事务内清空 `warming_since`（4.4）；回收与 rebuild 时清空（4.6）。
    - 处理冷启动的副本崩溃时，名额与 `projected_seq` 都已持久化。任一副本的补偿任务都把 `warming_since` 非空的过滤器视为活跃视图，从已提交的水位接着做（4.4 触发方式第 2 条）。水位已到链尾、尚未就绪的过滤器同样是补偿候选（6.9），由下一次 catch_up 置就绪并释放名额。
    - fail-closed 停下的冷启动不释放名额，包括 4.2 前提校验失败、4.1 缺对象、4.4 批前提不成立、3.3 不连续四种情形；其中缺对象在缺失的行补上之后会自动续追，不需要运维清理。系统告警，指标 `view_cold_start_slots_in_use` 持续反映占用。P0 由运维在停服后清掉该过滤器的派生行，并把它的状态列置为回收态；P1 起由 `mega2 view gc` 完成同一操作。代价是：这类故障累积到上限时，新注册会持续得到 429，直到运维介入。这与“出错即停、不自动覆盖”的取向一致。
    - 释放名额的事务不取 L_R，最多让并发的准入多算一个名额，结果偏保守。
  - **只有经过准入的过滤器才会被投影。** catch_up 在批事务内重读本行，`ready_seq` 与 `warming_since` 都为 NULL 时直接返回（4.4）；4.6 的重新预热也走本准入。所以 `max_filters` 与名额同样约束由读者访问触发的预热，不只约束注册。
  - **为什么不用现有设施。**【代码】mega2 没有跨副本的限流设施：
    - Buck 上传的 429 来自进程内信号量（`buck_service.rs::try_acquire_upload_permits` 约 L289–311，`common/errors/api.rs` 约 L169），多副本时上限随副本数放大；
    - LFS 媒体 finalize 先计数、后判定（`ceres/lfs/media/finalize.rs` 约 L164–171），两步之间不持锁，【推断】并发时可能越过上限；
    - 原子准入的先例是 B1 的 `queue_control … FOR UPDATE` 加条件 INSERT（`push_queue_storage.rs` 约 L145–203）。本设计沿用其中“串行化、计数、写入在同一事务内”的做法，但改用 advisory 锁：控制行需要迁移播种，还要处理行被删除的情形（`m20260905_000100_add_push_queue.rs` 约 L17–34），advisory 锁没有这些负担；
    - Redis 在启动时必连（`src/context/mod.rs` 约 L203–204），可以用来计数，但本设计不用它：Redis 计数与 `max_filters`、名额检查不在同一事务内，Redis 已经扣减而 Postgres 回滚时，配额会白白消耗；这也不符合 0.1-2 不把视图状态交给 Redis 的取向。注册本来就是低频操作（每次都可能触发 O(H) 冷启动），每次多几条 SQL 的开销可以忽略。
  - **`mega_view_register_log`**（运行态，见第 3 节）：列为 `id BIGSERIAL PK, requester TEXT NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT now()`，建 INDEX(requester, created_at)。行数不超过“token 数 × 每小时速率”。
  - 各上限取自请求开始时的配置快照，可以热加载（6.9）。调低 `max_filters` 不驱逐已有视图，调低名额不中断进行中的冷启动，都只影响之后的准入。各副本的配置分别热加载，短时间内取值可能不同；每次准入都在 L_R 下按发起副本当时的取值一次判定完毕。

### 6.6 REST 读接口（P1）与 ScorpioFS
- **接口：**
  - `GET /api/v1/views/{filter_id}/refs`
  - `GET /api/v1/views/{filter_id}/tree?commit=<view_commit|root_commit>&path=`
  - `GET /api/v1/views/{filter_id}/commits/{id}`：对投影生成的视图提交，返回对应的根提交区间，可用于来源追溯和展开 `Mono-Squash-Commit`；对客户端推送的视图提交（3.6），返回该提交及其落地记录。

  实现上直接复用 `filter_tree` 与 commit_map，挂入 storage-only 路由集。与 6.5 的接口按同一开关挂载，`enabled = false` 时同样返回 404。
- **ScorpioFS。** ScorpioFS 现在只按路径调用、不固定 commit，要支持视图挂载就得改造它的客户端：挂载参数加上视图标识，请求带上 commit。单 Subdir 的只读挂载可以继续用现有的路径 API。Antares 用到的 `/api/v1/cl/{link}/files-list` 在 trunk 路由集中本来就不存在，与本设计无关。
- **MST/2。** 快照描述符的 `namespace_view_id` 目前是 SHA-256(`mega.mst2.namespaceview\0` ‖ commit_oid)，只锁定 commit 身份（`src/ceres/snapshot/view.rs`）。spec 02 的 binding/release 组合会扩展它的覆盖范围；视图快照可以在那一层把 `filter_id@view_commit` 纳入摘要（P2 评估）。

### 6.7 Agent 工作区
- 任务创建方调用 `POST /api/v1/views` 拿到 `{filter_id, name, version}`。任务记录保存 `view_name@version`，以及当时的根提交或视图提交，保证任务可复现。Agent 只能使用 `@version` 形式的 URL。
- Agent Capture 现有的数据模型里没有 commit 或视图字段：session 只有 `repo_id / libra_repoid / cl_link`，checkpoint 只有自由格式的 JSON metadata。P1 需要约定 checkpoint metadata 的 schema，或者新增列；同时修订 `docs/refactoring/agent-capture.md` 的 ingest 契约，以及 `libra.md` 中的 LandingRecord（AC-LB-10 目前以 path commit / root roll-up 为对象）。
- 视图也是一种上下文裁剪手段：Agent 只 clone 任务需要的路径。
- 不做 Josh workspace 那种"过滤器随提交变化"的语义。

### 6.8 GitHub 镜像
- 现有的 github-sync 设计（`docs/refactoring/github-sync.md`；执行计划 `plan-20260920` 尚未实现）按路径 binding 镜像 `main@P` 物化链。本设计保留物化链，所以该设计的读侧不受影响。但 P2 的 view_push 是一个新的 B3 写入者，plan-20260920 的 outbox 变更集必须覆盖它（5.2 第 8 步）。
- 新增"视图 binding"：镜像源是 `view_tip(F)` 的投影历史，与路径 binding 并列；同一个 GitHub 仓库只能绑定其中一种。是否这样做要在 P1 定案（在 plan-20260920 的 OX 卡开工前写进该计划），实现放在 P2。
- github_sync 要求 `object_format = sha1`，与 1.2 一致。

### 6.9 配置
新增 `[views]` 配置节：

| 字段 | 取值范围 | 缺省 | 热加载 | 说明 |
|---|---|---|---|---|
| `enabled` | bool | false | 重启（两个方向） | 为 true 时，要求 `push_policy = "trunk"` 且 `object_format = "sha1"`，否则 validate 拒绝启动。是否 spawn worker、保留名的启动 fail-closed 检查（6.1），都只在启动时决定 |
| `worker_interval_secs` | ≥ 1 | 10 | 热 | 补偿任务的周期。为 0 时 `tokio::time::interval` 会 panic；任务内再按 `spawn_artifact_gc_task` 的先例取 `.max(1)`（`src/server/http_server.rs` 约 L292–293）。每次 tick 先探测一次根链，再用一条 SQL 取出活跃视图中满足 `projected_seq < root_chain.max_seq OR ready_seq IS NULL` 的视图，只对这些视图 catch_up。后一个条件不能省：只按滞后挑选时，水位已到链尾而尚未就绪的视图要等根再次前进才会被选中。【推断】例如最后一批提交时，`main@/` 已前进到根链尚未接入的提交，随后又被回退到链尾；根链扩展直接返回‘已追平’，水位也不再落后。满足后一条件的活跃视图，`warming_since` 必然非空，数量不超过 `max_concurrent_cold_starts`，所以每个周期的额外开销有界。它也是抢锁失败时额外滞后的上界（4.4）。 |
| `batch_size` | 1..=10 000 | 1000 | 热 | 4.4 每批的根链行数 B，也是 3.3 每条回走 SQL 的行数上限。上界的依据：`get_commits_by_hashes` 用 `is_in` 逐值绑定参数（`mono_storage.rs` 约 L1626），而 Postgres 单条语句至多 65 535 个绑定参数。按层读 tree 时，一层的 id 数可能超过 B，所以可失败的批量读取在内部仍要分块 |
| `max_append_walk` | 1..=100 000 | 1000 | 热 | 3.3：advertise 与 B0 单次调用最多处理的根链提交数，回走与接入合计。用尽只返回“未追上”，不是不连续判定；后台 worker 不受此限 |
| `sync_catch_up_commits` | 0..=`batch_size` | 64 | 热 | 4.4：只读视图 advertise 时同步追赶的提交数上限。0 表示不做同步追赶，直接广告已投影的 tip；可推送视图不受此限 |
| `max_in_lock_catch_up` | 1..=10 000 | 256 | 热 | P2：B0 预检与 B3 锁内补算的上限（5.2）。若为 0，只要投影有一点滞后，view_push 就会被拒。缺省值在 P2 基准中复核 |
| `max_filters` | ≥ 1 | 100 | 热 | 占有派生状态的过滤器数上限（6.5 第 4 步），不按定义行计数。缺省值对应 7.2 示例中的 V = 100 |
| `max_concurrent_cold_starts` | ≥ 1 | 2 | 热 | `warming_since` 非空的过滤器数上限（6.5 第 5 步）。设为 0 会让所有需要冷启动的注册都得到 429；要停用注册，用 `enabled` 或鉴权开关，不要把它设为 0 |
| `register_rate_per_token` | ≥ 1 | 10 | 热 | 每个 requester 在 3600 秒滑动窗口内产生新定义的注册次数上限（6.5 第 3 步）。窗口长度是代码常量 |
| `allow_anonymous_register`、`allow_anonymous_push` | bool | false | 重启 | 只在 `push_auth=none` 下起作用。`git.push_auth`、`git.push_tokens` 都是重启生效（`src/config/reload.rs::collect_git_restart_fields` 约 L676–691），这两项同属鉴权姿态，归入同类，避免不重启就改变匿名写入面 |
| `gc_grace_secs` | ≥ 3600 | 86 400 | 热 | P1。宽限期从 `mega_view_object.gc_marked_at` 起算（4.6），也用于 3.6 残留行的 `last_seen_at`。缺省值与 `artifacts_gc.grace_secs` 相同；后者没有下界，这里不照搬。下界取正数，因为宽限期是三类在途读取唯一的保护：upload-pack 在 `prepare_pack` 之后读取视图对象（6.3"发包与回收交错"）；在途 view_push 的 B3 读取 tree(new_v) 闭包（4.6）；3.6 中 A 段写入之后、B1 入队之前的残留行。取 0 时，回收之后的第二轮清扫就会删掉仍在发送的对象，3.6 的 DELETE 也会删掉 A 段刚写入、尚未入队的提交。3600 秒是保守的下界，并不保证发包能在这段时间内完成 |
| `gc_batch_limit` | ≥ 1 | 1000 | 热 | P1。清扫每批事务处理的对象数，下界与 `artifacts_gc.batch_limit` 一致。每批事务都持 L_G 排他锁，期间 catch_up 批事务要让路，所以不宜过大 |

P1 的清扫周期，以及闲置阈值 `idle_recycle_secs`（下界为 2T = 7200 秒，T 见 4.6"闲置回收（P1）"），在 P1 定案时连同缺省值与热加载类别一并补入本表。

实现要求：
- 在 `Config` 中加带 `#[serde(default)]` 的 `views: ViewsConfig` 字段；`ViewsConfig` 的每个字段都用 `#[serde(default = "…")]`，并且 `impl Default` 与上表的缺省值一致（`ArtifactGcConfig` 先例，`src/config/model.rs` 约 L488–521）。这样，只写了部分字段的 `[views]` 也能加载，未写的字段取表中缺省值；
- 在 `validate.rs::known_fields` 的根列表和本节的字段列表中登记。加载时会严格拒绝未知字段，现有的 `mst2` 就是漏登记的反例；
- 按上表在 `Config::validate` 中校验取值范围，写法仿照 `validate_artifact_gc_config`（`src/config/validate.rs` 约 L1328–1340）。热加载会先对候选配置执行 `validate`（`src/config/reload.rs` 约 L125），所以越界的热改同样被拒；
- 更新样例 `config/config.toml`，写出全部字段及其缺省值，并用注释标明各字段的热加载类别；同步更新双语文档 `docs/configuration(.zh).md`。

**热加载【决策】。**
- 【代码】`ConfigHandle::reload`（`src/config/reload.rs` 约 L120–161）先对候选配置执行 `validate`，再以当前快照的克隆作为 `next`，只把 apply 函数显式复制的字段写入 `next`。重启类字段只进入 `restart_required_fields`，不写入快照；如果只有重启类字段变化，快照不发布。这里没有兜底分类：apply 与 collect 函数都没列出的字段，热改时会被静默忽略，报告中也不出现。`mst2`、`oci`、`agent_capture`、`cedar` 目前就是这种情况。所以 `[views]` 的每个字段都必须在新函数 `apply_views_changes` 中显式归类，并在 `reload()` 中与 `apply_artifact_gc_changes` 并列调用。
- 归类规则：`enabled` 和两个 `allow_anonymous_*`，不论朝哪个方向改，都只报告为重启。其余字段在任何情况下都热应用，与 `enabled` 同时改动时也一样。这与 `artifacts_gc` 不同：`artifacts_gc` 在 `enable` 由 false 变 true 时把全部字段都列为重启（约 L394–421），因为它的开关可以热改，任务却只在启动时 spawn；本节的 `enabled` 本身就是重启类，不需要这个特例。
- 各方如何感知改动：
  - **worker**：沿用 `spawn_artifact_gc_task` 的模式，只在启动时 `enabled = true` 才 spawn，并注册 `ConfigReloadSubscriber`。`applied_fields` 中出现 `views.` 前缀的字段时，订阅者把新的 `ViewsConfig` 送进 watch 通道（`src/server/http_server.rs` 约 L158–180、L276–318）。`worker_interval_secs` 变化时重建 ticker。`batch_size`、`gc_*` 在每个批事务开始时读当前值，不改变正在执行的批。
  - **注册门与读入口**（6.1 第 2、3 层、`ViewRepo`、6.5、6.6）：每次请求开头调用一次 `state.storage.config()` 取快照，整个请求只用这一份，与现有协议代码读 `git` 配置的方式相同（`http.rs` 约 L58、L238；`src/contract/git_protocol/mod.rs` 约 L49）。`Storage::config()`（`src/jupiter/storage/mod.rs` 约 L355–359）返回热加载后的快照；`Storage.config` 字段是启动时的值，视图代码不得读取。整个请求只取一次快照，`sync_catch_up_commits ≤ batch_size` 这类跨字段约束才能在一次调用内成立。
  - **B0 与 B3（P2）**：各自在开头读快照，`max_in_lock_catch_up` 以 B3 锁内的判定为准。
  - **`enabled`**：第 2 层每次请求也从快照中读取它。由于它是重启类，热加载永远不会把新值写进快照，所以在重启之前，入口的行为与 worker 是否在运行始终一致。6.5、6.6 的 HTTP 接口按启动时的取值决定是否挂载，与第 2 层的判定等价。
- 多副本：每个副本各自轮询自己的配置文件（`ConfigReloadWatcher::poll_once`，约 L218–226），改动会在各副本上先后生效。全局上限按发起准入的那个副本的取值判定（6.5），短时间的不一致可以接受。
- 调低不回溯：调低 `max_filters` 不驱逐已有视图；调低名额不中断进行中的冷启动；调低 `batch_size` 从下一批开始生效。
- 测试（放在 `src/config/reload.rs` 的测试模块，仿照约 L1197–1283 的 `artifacts_gc` 用例）：
  - 每个热字段单独改动时，只出现在 `applied_fields`，快照随之更新；
  - `enabled` 的两个方向、两个 `allow_anonymous_*` 改动时，只出现在 `restart_required_fields`，快照保持旧值；与热字段同时改动时，热字段照常应用；
  - 逐字段穷举：把 `ViewsConfig` 的每个字段单独改动一次，断言它恰好出现在两张列表之一，防止新增的字段被静默忽略；
  - `sync_catch_up_commits > batch_size` 的候选配置被拒，快照不变；
  - 订阅者测试仿照 `artifact_gc_subscriber_updates_control_from_reload`（`src/server/http_server.rs` 约 L1140）。

---

## 7. 性能、规模、风险、测试与分期

### 7.1 已知事实
- **mega2【代码】：**
  - 根链线性，每轮 B3 至多产生一个根提交；净零推送不产生根提交（I6、ADR-TP-16）。
  - `incremental_pack` 每处理一个父提交就往返一次数据库。
  - 读一个 tree 等于读一行 `mega_tree`，再做一次 `from_bytes` 解析；Redis 缓存逐个对象读写。
  - 批量写入的块大小为 `BATCH_CHUNK_SIZE = 1000`，带死锁重试（`base_storage.rs`）。
- **Josh【代码】：** `walk2` 用 `known()` 剪枝；`history.rs` 中有一个单槽的"最近父提交树"缓存，专门优化线性历史。

### 7.2 规模估算（纯算术推导，P0 实测见本节末条）
- **根链表。** H 行，每行约 8 + 41 + 41 B，加上 Postgres 元组头、行指针和对齐约 30 B；再算上主键与 `commit_id` 唯一索引，约 250 B/行。H=10⁶ 时约 250 MB。回走暂存表只在扫描期间有行：冷启动时峰值约 H 行，与根链表同一量级，接入后删除。
- **commit_map。** 每个视图的行数 = 该视图的 NEW 提交数 ≤ H。按 hex 文本 id 计，堆约 136 B/行；两个索引合计约 140 B（叶页填充率按 70%）。合计约 280–300 B/行。
- **mega_view_object + mega_view_object_ref。** 每个 NEW 视图提交对应：
  - 一个提交对象：约 0.3–1 KB，加主键索引约 86 B；
  - 一行引用：堆约 84 B，加主键索引约 97 B。

  合计约 0.65–1.35 KB。Prefix/Exclude/Compose 视图每个提交还有 O(深度 × 成员数) 个脊柱 tree 及其引用行。
- **示例**（H=10⁶，V=100，每个视图的 NEW 提交平均占 5%，即 5×10⁶ 个 NEW 提交）：派生表合计约 4.7–8.3 GB，主要来自对象表。最坏情况是每个视图都覆盖几乎全部改动，此时 NEW 提交达 10⁸ 个，约 95–165 GB。如果采用 3.5 的"不持久化视图提交字节"优化，提交对象这部分可以基本消除。
- **单个新根提交对一个 Subdir 视图的开销。** 按层批量读 d 次 tree（d 为路径深度），最多新增一行映射。
- **冷启动。** 共 H/B 批，每批 O(1) 次提交查询，加 O(深度) 次批量 tree 读取。吞吐在 P0 基准中实测。
- **P0 基准实测（HP-24）。** 运行 `source .env.test && cargo run --release --example view_bench -- --params examples/view_bench.params.json`；参数冻结于 `7acc2b569475c137f905496438027fa174b002f0`，数据是 git.git v0.99.5 的 `3857284f7b892f855edffc5b9c196a0dd74b1b7d` 原始 pack（SHA-256 `812723cd7e7c540101e3cb297a7dc4f14f18f407cd68d76452e97321e2992003`）。计入判定的运行是 `target/tmp/view_bench/run-20261007T213147Z.json`，被测 HEAD `d2ac0beb0effa5fa1744d0c4cf1d8865ad39a5a7`，接缝与 example 的 SHA-256 见该证据 `env.code`。环境：Apple M4 Max 16 核、128 GiB、USB SSD、macOS 27.0.1、Postgres 18.6。H 三档为 307 / 615 / 1231，B=1000；每组 5 次预热、50 次样本；以下时间单位为 µs，大小单位为字节。
  | H | 根链冷启动耗时 / SQL | `:/Documentation` 冷启动耗时 / SQL / commits/s | `:exclude[::t/]` 冷启动耗时 / SQL / commits/s | 根链 / commit_map / object / object_ref 大小 |
  |---:|---:|---:|---:|---:|
  | 307 | 60082 / 19 | 34300 / 14 / 8950 | 53589 / 17 / 5729 | 139264 / 172032 / 262144 / 147456 |
  | 615 | 41728 / 19 | 31397 / 17 / 19588 | 75839 / 17 / 8109 | 221184 / 253952 / 1466368 / 294912 |
  | 1231 | 77556 / 27 | 93874 / 31 / 13113 | 217212 / 34 / 5667 | 368640 / 442368 / 5545984 / 704512 |

  单提交增量每格为 `p50 / p95 / SQL`，SQL 是该组 50 个样本共同的语句数。视图 0 为 `:/Documentation`，视图 1 为 `:exclude[::t/]`。
  | H | 视图 | in_view | outside | t_only |
  |---:|---:|---:|---:|---:|
  | 307 | 0 | 23443 / 31431 / 23 | 22242 / 43777 / 19 | 21036 / 37593 / 19 |
  | 307 | 1 | 24617 / 36936 / 23 | 21687 / 42749 / 19 | 20525 / 38388 / 19 |
  | 615 | 0 | 22700 / 38126 / 23 | 21902 / 39527 / 19 | 21402 / 41113 / 19 |
  | 615 | 1 | 22974 / 40108 / 23 | 22496 / 44565 / 19 | 20740 / 41269 / 19 |
  | 1231 | 0 | 24739 / 39020 / 23 | 26077 / 41710 / 19 | 24760 / 38769 / 19 |
  | 1231 | 1 | 24171 / 38809 / 23 | 27341 / 39856 / 19 | 23692 / 42684 / 19 |

  判据 (a) PASS（跨档逐样本 SQL 相等）；(b) PASS（最大 p95 比 1.24145 ≤ 1.5）；(c) PASS（最大投影批均耗时比 0.88864 ≤ 1）；(d) PASS（每视图最多 61 ≤ 100 条 SQL）。顶格过滤器上限 Compose 64、Exclude 256、k=64；冷启动 950462 µs / 24 SQL / 0 个脊柱 tree；增量 p50=23893 µs、p95=27295 µs ≤ 1000000 µs，样本均为 25 SQL、1 个脊柱 tree。H=307 时 `Documentation/` 尚未出现，顶格源路径在该历史中也未命中；这些数据只支持冻结夹具下的 P0 判据，不外推至更长历史或真实顶格命中负载。

### 7.3 风险
| 风险 | 说明 | 缓解 |
|---|---|---|
| R1 哈希不确定 | tree 排序、EMPTY_TREE 与空子树剪枝、hash kind、签名头剥离范围、L0 tree 存储的归一化行为 | 把 2.4 列出的规则冻结进 v1；做清表重建和双 worker 并发测试 |
| R2 L0 往返失真 | commit：git-internal 原样保留扩展头与正文，但 author/committer 列是重新序列化的；补零或带 `+` 的时间戳、单空格空名、大写 hex 无法从列重建，前提校验失败即报错（4.2）。服务端构造的提交不受影响，只在 trunk 下运行过的根链上没有这类提交。tree：`sub_trees` 是归一后的字节，`tree_id` 保留原始哈希；含 `100664`/`100640` 或 GBK 文件名的推送 tree 会出现 `hash(sub_trees) ≠ tree_id`，L0 也没有原始字节可以恢复（0.5-18、18a）。这是 L0 既有缺陷，`/<path>.git` 的 clone/fetch 同样会失败 | commit：往返测试分两组。mergetag、encoding、gpgsig-sha256、双空格空名和未成帧的历史 message 应通过；补零时间戳、带 `+` 的时间戳、单空格空名和大写 hex 应在前提校验处失败，该视图停在 s − 1 并告警（4.2）。根治另立任务（第 8 节）。tree：投影照常推进；ViewRepo 打包前逐个校验取自 `mega_tree` 的 tree，不一致时本次 upload-pack 明确失败并告警（6.3）。根治要靠 L0 在接收时拒绝非规范 tree，另立任务（第 8 节） |
| R3 同一内容两套哈希 | `/<path>.git` 与 `:/<path>` 视图的提交 id 不同；命名视图新建版本后，不带版本号的 URL 历史会换成另一套 | 用户文档写明；Libra 在检测到 filter_id 变化时提示；Agent 必须用 `@version`；不为 `/<path>.git` 提供隐式视图 |
| R4 视图被误当成读边界 | mega2 的读接口是全局开放的 | 文档写明；需要读隔离时，另立路径级读 ACL 计划 |
| R5 投影滞后 | 只读视图是最终一致的 | 未就绪返回 503；有界同步追赶；补偿任务；指标 `view_projection_lag_commits` |
| R6 表膨胀与 GC | 见 7.2 | 游程映射；注册上限；按 `last_access_at` 加引用不变式回收，带宽限期；可选不持久化提交字节 |
| R7 根提交作者可读性差 | 产品写 API、review 合并、attach 的作者仍是 mega 身份 | 为 L0 写入者另立 ADR，不阻塞本设计 |
| R8 与 Josh 的语义差异 | Compose 不相交判定偏保守；规范化不完备；签名默认删除，且同时删除 gpgsig-sha256；不移植 nop 恒等；L0 含空子树条目时，中间级空根规则无法复现；被重写的祖先 tree 不保留指向 EMPTY_TREE 的兄弟条目，Josh（`tree.rs::seed_entries`）原样保留 | 在文档中写明；对应的 Josh 用例改成负向测试，或者不移植 |
| R9 越权写入（P2） | Exclude 逆运算、同名覆盖、值域外内容、import 命名空间、根级文件、root_dirs 白名单、源路径删除 | 2.5 的更正；可推送视图的约束；2.7 的三项校验；按每个源路径授权；默认不允许匿名推送 |
| R10 推送后客户端分叉（P2） | 视图推送不能往返恒等 | sideband 对齐提示（ADR-TP-18）；可推送视图的 advertise 读到自己的写入；Libra 侧提示 reset |
| R11 根链非线性或不连续 | 遗留数据、review 形态的根路径 CL 合并、根 ref 被人工回滚 | 只支持 trunk；建链和扩展时都校验，一旦发现就停下、返回 503 并告警；附录 E 留到 P2。合法的长滞后不算不连续：回走预算用尽只返回"未追上"，由暂存表续扫（3.3） |
| R12 锁内成本（P2） | B3 要在内存中补算到 seq(R)，并续接 A 中已物化的行 | B0 预检滞后量；锁内只读；补算量超过上限就返回可重试错误；续接候选按 A 过滤（5.2 第 7 步），成本与 `kind=push` 同类，用指标观测 |
| R13 vault 依赖（P2） | `view_push` 落地需要服务端签名 key | 沿用 trunk 现状 |
| R14 匿名注册导致负载失控 | `push_auth=none` 时注册会触发 O(H) 冷启动 | 默认关闭匿名注册；限制过滤器规模；注册与重新预热都在 L_R 下原子准入，`max_filters`、冷启动名额与按 token 计的速率在同一事务内判定（6.5）。 |

### 7.4 测试计划
**夹具。** `/` 历史无法通过推送或导入构造出来：N>1 的推送会被 squash，merge 会被拒，ImportRepo 不进入根历史。所以夹具在测试 schema 中直接写库：
- 用 git-internal 的 `*_with_kind` API 构造 Tree/Commit，经 `save_mega_trees` / `save_mega_commits` 写入，再做一次根 CAS（仿照 `monorepo.rs` 测试中的夹具）；
- 配合 `test_db_connection` 与 `apply_migrations`，schema 守卫要绑定到 `_schema`；
- 只能在测试中这样写。在线实例上绕过队列会违反 I5。

**比对口径：** 期望的 `git log --graph` 与 tree 输出。

**正向（P0，线性），移植自 Josh `tests/filter/`：**
- `prefix.t`、`subtree_prefix.t`、`deleted_dir.t`、`moved_dir.t`、`empty_head.t`；
- `exclude_compose.t`：移植前两段；`:exclude[sub1=:/sub3]` 那段改成"注册时语法被拒"的负向测试，因为 P0 中 Exclude 的参数只能是 `::` 选择器；
- `gpgsig.t`：只取 remove 语义，并补充 mega2 自有的 gpgsig-sha256 用例；
- `filter_id.t`：只取其中的规范化规则；
- `compose_shadow_dir_same_name.t`：改成"注册时被拒"的负向测试。

依赖 merge 拓扑或孤儿提交的用例（`prune_trivial_merge.t`、`initial_merge_elided_parents.t`、`empty_orphan.t`）留到 P2。不使用 `#[ignore]` 或 skip（AGENTS.md）。

**协议（P0），移植自 Josh `tests/proxy/`：** `clone_subtree.t`、`clone_subsubtree.t`、`clone_prefix.t` 中的 clone/fetch 部分。在 compose 的 git-cli 容器内，以视图 URL 做黑盒比对。

**规范化与空树：**
- 逐条验证规范化规则是可靠的：在随机 tree 上，规范化前后的过滤结果相同；
- 验证空树规范：`:empty:prefix=a` 规范化前后的结果一致；
- 逐字节断言 2.1 的黄金向量（canonical 文本与 filter_id），并逐条断言应拒绝的输入；在随机 AST 上断言 `parse(print(ast)) == ast`，在随机输入上断言 `print∘canonicalize∘parse` 的不动点性质。
- 以下情形应被接受（P2）：在 Compose 视图中删掉某个成员目录下的子目录，被删空的祖先目录随之剪除；在 Exclude 视图中删掉"只剩隐藏内容的目录"下最后一个可见文件。
- 在 Compose 视图中删掉整个成员目录等于删除源路径，应被 2.7 第 3 项拒绝，并提示在父路径推送（P2）。

**src_paths：**
- 逐条断言 2.2 中的例子及各例的 push_enabled（`:exclude[::secret]` 的 src_paths 为 `/`，push_enabled = false）。
- （P0）注册 `:exclude[::secret]` 时，paths 只含 `/secret` 或只含 `/project` 的 token 得 403，只有覆盖 `/` 的 token 能通过。`:prefix=x:[:/w:prefix=b,:/z:prefix=a]` 的注册被拒，不带凭据时同样被拒。
- （P0）性质测试：随机生成 P0 过滤器 F，以及两棵只在 src_paths(F) 区域之外有差异的树，`filter_tree` 的结果相同。
- （P2）对 `:/a:exclude[::b/]` 视图，paths 只含 `/a/b` 的 token 在 B0 被拒。随机取 F、T_base、T_v，unapply 的结果都能通过 2.7 第 1 项的授权面复核。

**逆运算与 unapply（P2）：**
- Josh 的 `reverse_hide.t`、`reverse_hide_edit.t`、`reverse_hide_edit_missing_change.t`、`reverse_non_ff.t`、`reverse_split.t` 中 tree 级的部分；`stored_reverse_exclude.t` 中的 exclude 语义。`reverse_glob.t` 随 glob 一起做；`reverse_rev_trivial_merge.t` 不移植。
- 2.5 的反例：对 `:/a:exclude[::b/]` 视图推送，不能写进 `a/b`。
- 2.7 的反例：同名文件覆盖被隐藏的目录；在 Compose 根上新建文件；写入 import 命名空间；删除源路径。

**协议（P2）：**
- Josh `tests/proxy/` 中 `push_subtree.t`、`push_prefix.t`、`push_subdir_prefix.t`、`push_error.t` 的线性部分，期望的 tip 改为投影 tip 加对齐提示；
- `push_new_branch.t`、`push_new_orphan_branch.t` 改成负向用例。

**mega2 专有：**
- 确定性：两次冷启动、清表重建、两个 worker 并发，得到的哈希都一致。
- 增量：插入一个视图外的根提交，视图 tip 不变，commit_map 不新增行；插入一个视图内的根提交，恰好新增一行。
- 根链：
  - 乱序到达的 C 段信号不会导致编号错误；
  - 合法的长滞后能分批追平，不被判为不连续，例如关闭 views 一段时间后再打开，其间根链前进超过 `max_append_walk`；
  - 分批回走中途重启进程，从暂存表续扫的结果与一次扫完相同；
  - 分段接入中途换手：待接入的行数超过一段的上限（段上限是内部参数，测试中调小，不新增配置项）。首段事务提交后，分别模拟两种情形：进程退出后重启；另一个 worker 或阻塞等锁的 advertise 在段间取得根链锁。两种情形都判为"连上"并续接，不判为不连续，最终的根链与一次接完逐行相同。冷启动（根链表为空）与非冷启动（链尾已存在）各覆盖一次，并断言每个段事务提交后，暂存表要么为空，要么 pos 最大的行等于链尾；
  - 两个 worker 读到不同的 `main@/`、交替推进时，结果正确，不会误判；
  - 以下五种情形都判为不连续，结论在重启后和其他副本上保持：回滚；回滚后再推进（分叉）；根 ref 换成无关历史；多父提交；首父缺失；
  - `main@/` 恢复后，清空暂存表即可重新判定；
  - seq=1 的 bootstrap 提交无父，能被正常接入。
  - 抢锁失败不丢进度：持锁者最后一次读取链尾之后，再追加一个根提交；竞争的 worker 抢锁失败后返回。断言在一个 `worker_interval_secs`（测试中调小）之内追平。信号在 worker 执行一轮期间到达时，本轮结束后立即再执行一轮。
- 往返前提校验：按 R2 的两组用例分别断言通过与失败。失败组再断言三点：本批中 seq < s 的段已写入、水位为 s − 1；清表重建后停在同一处；在该提交处被 J4 丢弃的视图照常追平。
- 缺对象（4.1）：夹具中根树含 `a/` 条目，但删去它所指的 tree 行。断言 `:/a/b` 视图停在 s − 1 并告警，不产出空树提交，也不按 J4 丢弃；冷启动中的视图保持 503，并占用名额；补回该行后，追赶自动从 s 继续，最终哈希与从未缺行时清表重建的结果逐字节相同。对照组：根树中确实没有 `a/` 条目，或者 `a` 是文件时，投影为 EMPTY_TREE。批大小取 B = 1 与 B = 1000 时，停下的位置与已写入的段都相同。（P2）B3 补算遇到同一缺行时返回可重试错误，不触发第 3 步的损坏告警。
- L0 tree 不自洽：
  - 夹具按 receive-pack 的真实路径（`process_entry` + `convert_to_mega_model`）写入两个 tree：一个含 `100664` 条目，一个含 GBK 文件名，二者都满足 `hash(sub_trees) ≠ tree_id`。它们分别作为 Subdir 的结果 tree 和 Prefix 包住的 T 进入视图。
  - 断言：投影照常推进，清表重建后哈希不变；错误契约第 6 项全部成立；闭包不含这类 tree 的视图 clone 正常。
  - 另建一个含 `160000` 条目的视图：clone 成功，pack 中没有 gitlink 的目标对象。
- 物化链回归：启用视图后，`/<path>.git` 的 advertise、push 以及 I1–I3 相关测试全部不变。
- 保留名：`.view` / `.filter` 不能被创建为目录，`root_dirs` 拒绝这两个名字。
- URL 与写入：
  - 错误契约第 1–3 项；
  - 错误契约第 4 项；
  - 视图 URL 下的 LFS 一律返回 404。分别在 `push_auth=none` 和持有不限路径 token 两种配置下测试，覆盖：`POST …/info/lfs/objects/batch`（upload 与 download）、`PUT …/info/lfs/objects/<oid>`、`…/info/lfs/locks` 的创建、列表与 verify；启用 fastcdc 时还包括媒体接口。SSH 的 `git-lfs-authenticate '/.view/<name>.git' upload` 被拒绝；
  - 已就绪但投影为空的视图（tip 为 NULL 段）：`protocol.version=0` 的 clone 和默认 v2 的 clone 都得到空仓库，v2 的 `ls-refs` 应答只有 flush；`/<path>.git` 不存在的路径加一个同样的 v2 回归用例；
  - 错误契约第 5 项。

**错误契约（P0）。** 每个用例同时断言服务端的原始应答和客户端的诊断。原始应答：HTTP 在 cargo 黑盒中直接发请求，取状态码、`Retry-After` 与响应体字节；SSH 在 git-cli 容器内执行 `ssh … "git-upload-pack '<url>'" </dev/null`（v2 加 `-o SetEnv=GIT_PROTOCOL=version=2`），取 stdout 字节与退出码。客户端诊断：git-cli 的退出码与 stderr。
 1. 未知视图、`enabled=false`：v0、v2 的 `GET info/refs?service=git-upload-pack` 返回 404，`git ls-remote` 的 stderr 含 `not found`；SSH v0、v2 的 stdout 恰为一个 pkt-line `ERR view not found`，退出码 1，git 的 stderr 含 `remote error: view not found`。
 2. 未就绪：v0、v2 的 GET 返回 503，`Retry-After` 为正整数，git 的 stderr 含 `returned error: 503`；SSH v0、v2 的 stdout 恰为一个以 `ERR view <filter_id> unavailable:` 开头的 pkt-line，退出码 75，git 的 stderr 含 `remote error: view <filter_id> unavailable`。冷启动、回收中、根链停追三种情形各跑一遍。
 3. 宣告之后转为未就绪（GET 成功后，测试把 `ready_seq` 置为 NULL）：v2 的 `POST command=ls-refs`、`POST command=fetch`（带 `done` 与不带 `done` 的 NAK 轮各一次） 与 v0 的 `POST git-upload-pack` 都返回 503 加 `Retry-After`，不是 400，也不是 200 加空 ref 列表。SSH 在同一个 exec 中宣告之后再发 want，stdout 在宣告之后只多出一个 ERR pkt-line，退出码 75。SSH 另覆盖 v2 多轮：同一个 exec 中先完成一轮 NAK 协商，再把 `ready_seq` 置为 NULL；下一轮仍不带 `done`、have 不命中时，stdout 只多出一个 `ERR view <filter_id> unavailable:` pkt-line，不含 `acknowledgments`，退出码 75。
 4. receive-pack：`GET info/refs?service=git-receive-pack` 与 `POST git-receive-pack` 返回 403；POST 用读取即报错的请求体发送时仍返回 403，证明拒绝早于读取请求体；SSH `git-receive-pack` 的 stdout 恰为一个 pkt-line `ERR view URLs are read-only`，退出码 1；`git push` 经 HTTP 时 stderr 含 `returned error: 403`，经 SSH 时 stderr 含 `remote error: view URLs are read-only`。
 5. want 不属于本视图（取 Monorepo 中存在、但不在本视图 commit_map 中的提交），覆盖五种请求：v0 have 为空；v0 have 非空；v2 带 `done`；v2 不带 `done` 且 have 全不在视图链上（NAK 轮）；v2 不带 `done` 且 have 命中（ready 轮）。原始 POST 都返回 200，响应体恰为一个 pkt-line `ERR upload-pack: not our ref <oid>`，不含 `acknowledgments`、`NAK`、`ACK`、`packfile` 与 `PACK`；SSH 在宣告之后只有这一个 pkt-line，退出码 1。对照组：want 合法的 NAK 轮照常返回 `acknowledgments` 加 `NAK`，以 flush 结束。
 6. L0 tree 不自洽，覆盖四种请求：v0 have 为空；v0 have 非空且会被 ACK；v2 带 `done`；v2 不带 `done` 但 have 命中（ready 轮）。HTTP 返回 200，`Content-Type` 为 `application/x-git-upload-pack-result`，响应体恰为一个 ERR pkt-line，内容含该 tree_id，不含 `ACK`、`NAK`、`acknowledgments`、`packfile` 与 `PACK`；SSH 在宣告之后只有这一个 pkt-line，退出码 1；每次请求都使 `view_pack_tree_mismatch_total` 加一；随后的 GET info/refs 仍返回 200。git-cli 经 HTTP 与 SSH 的 v0、v2 clone 都退出非零，stderr 含 `remote error:` 与该 tree_id，不含 `did not send all necessary objects`。另加 v2 不带 `done`、have 全不在视图链上的 NAK 轮：照常返回 `acknowledgments` 加 `NAK`，`view_pack_tree_mismatch_total` 不变，以此证明 NAK 轮不计数；同一客户端随后带 `done` 的那一轮得到上述 ERR。
 7. Libra（随第 8 节的视图 Libra smoke 卡执行）：HTTP clone 未就绪视图时，报错含 `HTTP 503`；HTTP 与 SSH clone 含不自洽 L0 tree 的视图时，报错含 `remote reported an error:` 与该 tree_id；SSH clone 未就绪视图时，报错含 `status 75`。Libra 的宣告解析补上 ERR 分支后，SSH 用例改为断言 ERR 原文。
 8. 解析先于读取请求体：用读取即报错的请求体（cargo 测试中由 `Body::from_stream` 产出一个 `Err`），分别 POST 未知视图与未就绪视图的 `git-upload-pack`，断言返回 404 与 503（带 `Retry-After`），而不是 `collect_body_data` 返回的 400（failed to read upload-pack body）。

**注册准入（P0）：**
  - 用两个连接模拟两个副本。在活跃过滤器数为 `max_filters − 1`、且名额只剩一个时，并发注册两个不同的新过滤器：恰好一个成功，另一个得到 429，且没有留下过滤器行、视图行或速率行。同一个新过滤器并发注册两次：只建一行，只占一个名额；
  - 同一 token 在窗口内第 `register_rate_per_token + 1` 次产生新定义的注册得到 429，`Retry-After` 等于最早一行离开窗口的剩余时间向上取整到秒，不超过 3600 秒；幂等的重复注册不计数；不同 token 互不影响；
  - 冷启动进行中丢弃处理它的 worker，当前批不提交；另一个 worker 实例在同一 schema 上续做到就绪，名额在就绪的同一事务内释放；
  - 冷启动的最后一批：如果根链已覆盖 `main@/`，`projected_seq`、`ready_seq` 与 `warming_since` 在同一事务内更新，提交后不存在‘水位到链尾、`ready_seq` 为 NULL’的中间状态。如果最后一批时 `main@/` 已前进到根链之外，则不置就绪；根链扩展后，视图续追到就绪。另在测试中直接写出以下状态：`projected_seq` 等于链尾、`ready_seq` 为 NULL、`warming_since` 非空、`main@/` 等于链尾，此后不再推进根。只运行另一个 worker 实例的补偿任务，断言视图在一个周期内就绪并释放名额。
  - 4.2 前提校验失败的冷启动仍占用名额，并能由指标观测到；
  - `enabled = false` 时，`POST /api/v1/views`（含 `wait=true`）、`GET /api/v1/views/{filter_id}` 与 `GET /api/v1/views?name=` 都返回 404。分别在不带凭据和持有覆盖 `/` 的 token 两种情况下测试；`mega_view_filter`、`mega_view`、`mega_view_register_log` 都没有新行；`/api/openapi.json` 中没有 `/api/v1/views` 路径。
  - 超过 16 KiB、嵌套 17 层、成员 65 个、k = 65 的 filter_spec 各自得到 400；嵌套层数远超上限的输入同样以 400 结束，进程不崩溃；
  - （P1）视图回收后被读者访问：名额已满时该次访问返回 503；名额空出后，访问会触发预热。
- 能力：经 HTTP 请求 depth 或 filter 时返回 400，git 报 `RPC failed; HTTP 400`（SSH 下的同类错误仍是裸文本，见 6.1 错误契约末行）；git 默认的 v2 clone 成功，`protocol.version=0` 的 clone 也成功。
- P2：
  - 5.4 表中每一行各一个用例；
  - 同一推送重复执行是幂等的；
  - 能看到对齐 sideband；
  - `view_push` 之后物化链的 I1/I2a 成立；推送前满足 I3 的行，推送后仍然满足 I3；I4 的锚点可以解析。另加三个陈旧行用例：
    - 在本轮未变化的子树中预置一条陈旧行：view_push 之后它保持原状，随后被巡检写墓碑修复；
    - 在本轮有变化的子树中预置一条陈旧行：它被续接修正，`view_push_stale_healed` 加一；
    - 祖先行已陈旧，且本轮新子树恰好等于它的陈旧哈希：其下一致的后代行仍被续接。
  - LCP 为 `/` 和 LCP 未物化这两种情形都要覆盖。LCP 为 `/`、且仓库中有大量与视图无关的已物化行时，B3 读出的续接候选数等于 |A|（用指标或查询计数断言），视图外的行一律不前进。
  - NFF 以 `tip_at_R` 为准：
    - 不启动投影 worker，使 `projected_seq < seq(R)`。先用普通 push 改动视图内的文件，此时 `view_tip(F)` 仍等于客户端的 old_v；再提交基于 old_v 的 view_push，断言以 fetch first 拒绝，根树中该文件不变。
    - 对照组：滞后期间只改视图外的文件，view_push 被接受。
    - 恢复投影后，断言持久化的 `map(F, seq(R))` 与 B3 算出的 `tip_at_R(F)` 逐字节相同。
  - 非线性闸门（附录 E 启用后）：夹具在根历史中放入 merge 提交 M，并让它接入根链表。以下三种情形各推一次基于当前视图 tip 的 view_push：
    - (a) R = M，且 `projected_seq = seq(M)`，K 为空；
    - (b) M 之后另有一个单父根提交 R，回溯只经过 R；
    - (c) `projected_seq < seq(M)`，K 中含表内的 M。
    三种情形都在 NFF 之前以非线性原因拒绝，根树不变；同一视图的 v0、v2 clone 与 fetch 照常成功。
  - 幂等与滞后：视图滞后超过 `max_in_lock_catch_up` 时，三类重推分别处理：已 Done 的同指纹重推，回放首次结果；处于 Queued/Running 的同指纹重推，被收养；新指纹的推送，得到可重试错误。同一视图上不同指纹的并发推送，得到 fetch 提示，而不是系统错误。
  - C 段跳过：
    - 先一轮 view_push 改动了 `/a/c`，后一轮 Done 只索引 `/a/b`：先一轮照常索引。
    - 后一轮是祖先路径 `/a` 上的 push：先一轮跳过。
    - 更晚的 view_push 的 LCP 等于某个 push 的路径、但其源路径不覆盖该路径：那个 push 的 C 段不跳过。
    - 净零 view_push 之后，出现对与根树一致。
  - 回收或全局 rebuild 与在途 view_push 交错：B3 要么返回可重试错误，要么照常落地，不触发 5.2 第 3 步的损坏告警；视图预热完成后重推，落地成功。
  - A 段重推已过宽限期的残留提交，并与残留行回收交错：断言 B3 能读到整条链。
- 回收与清扫（P1）：
  - 清扫标记之后、删除之前，catch_up 为同一对象补写引用行：断言对象不被删除，也不出现"有引用行、无对象"的状态；
  - 回收与 catch_up 并发：断言回收之后，该视图不残留映射行与引用行；
  - 全局 rebuild 与 catch_up 并发：断言 rebuild 之后，没有按旧根链写入的映射行；
  - 正在回收的视图，在 v0、v2 首次宣告时都返回 503，而不是空仓库。
  - 回收与 `prepare_pack` 交错：回收在那条 SQL 之前提交时，返回 503，不返回 `not our ref`，也不发出缺提交的 pack；在其后提交时，本次 pack 完整送达，客户端的连通性检查通过；
  - 超出宽限期的截断：测试直接以参数调用清扫（宽限期取 0，不经配置校验），在 `prepare_pack` 与打包方法之间注入回收和两轮清扫。断言流被截断、`view_pack_object_missing_total` 加一、随后的请求得到 503；
  - `gc_grace_secs < 3600` 的候选配置被 validate 拒绝，热加载后快照不变。
  - 闲置回收复查：候选选出之后、回收事务开始之前，用另一个连接刷新 `last_access_at`。断言回收事务的条件改写命中 0 行，映射行与引用行都保留；
  - 刷新节流：同一副本在 T 之内对同一视图的多次访问，只发一条 UPDATE（用 `set_metric_callback` 计数）；元数据 GET 不刷新；
  - `last_access_at` 为 NULL 的就绪视图不被闲置回收；正在预热的视图不是候选；重新预热的准入写入了 `last_access_at`，视图就绪后不会在下一轮被立即回收。

**IT。** 黑盒 git 比对在 compose 的 git-cli 容器内运行（`tests/common/git_cli.rs`），不用宿主机上的 git。

HP-18 的 `integration_git_ssh` 视图用例沿用该 target 既有 storage-only 用例的宿主回环写法：宿主 `ssh` 与 git 经 `127.0.0.1` 连接。请求与应答字节与客户端执行位置无关；宿主 git 只断言 `not found`、`returned error: 403`、`remote error: view not found`、`remote error: view URLs are read-only` 这些稳定子串。这是上文「错误契约」原始 SSH 应答与本段容器内 git 比对规则在 HP-18 上的执行位置例外。

**基准【决策】。** 基准作为显式门运行（独立的 bin，或带理由的 opt-in 门），不进入 `cargo test --all`，也不新增 criterion 依赖，计时用 `std::time::Instant`。按以下协议执行，判据见 7.5 P0 验收 11。
- **开工前冻结。** 下列各项写入任务卡的 `Performance budget` 字段（`docs/plan/plan-template.md` 约 L549），在 ER-03 开工复核时一并确认（约 L185）。开工后改动其中任何一项，都要先修订任务卡，不得在拿到数据之后再调。
  - 数据：git.git 的源提交 id 和取得方式（本地镜像路径，或归档文件的 sha256）。取该提交的首父链，按 7.4 夹具的写库方式线性化写入，链长记为 H_max；
  - 历史长度分三档：H ∈ {⌊H_max/4⌋, ⌊H_max/2⌋, H_max}，都取同一条链的前缀。每档单独建库，因为探针接到前缀末端之后，就不能再续接 git.git 后面的提交，否则根链不连续（R11）；
  - `[views]` 参数，其中 `batch_size` B 默认为 1000；
  - 视图：`:/Documentation` 与 `:exclude[::t/]`；
  - 探针提交：一组固定的根提交，每档都接在前缀末端，每个探针改动的路径与内容逐条写明。至少包括三类：只改视图内文件；只改两个视图之外的文件；只改 `t/` 下的文件（对 `:exclude[::t/]` 而言是 J4 丢弃）；
  - 次数：每档、每个视图、每类探针先预热 w 次，再计量 n 次（建议 w = 5、n = 50）；
  - 环境：CPU 型号与核数、内存、存储类型、操作系统、Postgres 版本及参数来源（例如 `docker/docker-compose.test.yml` 的默认值），以及 Redis 缓存是否启用（建议关闭，免得语句数随缓存状态变化）。基准独占运行，同一台机器上不同时跑其他 cargo test 或基准；
  - 阈值 k_H、k_B、c_cold，含义见 7.5 P0 验收 11。
- **计量口径。**
  - 基准直接调用根链扩展（3.3）与 `catch_up`（4.4），不启动补偿任务，也不经过信号；
  - 单提交增量：视图先追平到探针的父提交，再写入一个探针根提交并 CAS `main@/`。计时从调用根链扩展开始，到该视图的 `catch_up` 事务提交为止，不含客户端 fetch；
  - 冷启动：每档先从空的根链表构建根链，单独计时；再让每个视图从空的派生表投影到 `ready_seq` 非空，分别计时；
  - SQL 语句数：用 sea-orm 的 `set_metric_callback` 计数，每执行一条语句回调一次【代码：sea-orm 2.0.2 `src/metric.rs`；mega2 已在 `src/jupiter/tests.rs` 约 L85 使用这个回调】。连接如果取自 `test_db_connection`，计数闭包必须同时持有原闭包捕获的 schema 守卫，因为再次调用 `set_metric_callback` 会替换原闭包，守卫随之释放，schema 就被删掉了。
- **证据。** 原始样本（逐次的耗时与语句数）和汇总（p50、p95、语句数、冷启动两段的耗时与吞吐）写入 `target/tmp/` 下的 JSON 文件。任务卡的 Verification 记录运行命令、冻结参数与汇总。每次运行都要记录，不得挑选。

### 7.5 分期计划与验收标准

**P0：只读投影（仅 trunk）**
- 内容：
  - 第 2 节的过滤器：Subdir / Prefix / Exclude / 源路径与输出路径都不相交的 Compose / Nop / Empty；规范化与 filter_id；
  - 3.1–3.5 的表；
  - 4.1–4.4 的线性投影，以及 4.5 的 view_tip；
  - ViewRepo 的 upload-pack（v0 + v2，遵守能力诚实）；
  - `/.filter/`、`/.view/` 两种 URL，保留名，早期拒绝写入；
  - want 校验；
  - 后台追赶、补偿任务、视图锁与根链锁；
  - 视图注册 API（6.5），含各项上限，以及对回收态过滤器的重新注册预热；
  - `[views]` 配置；
  - 只支持 sha1。
- 验收：
  1. 线性正向的 Josh 移植用例与协议用例全部通过；重叠 Compose 的用例以负向测试形式存在。
  2. 清表重建、两次冷启动、两个 worker 并发之后，所有视图 tip 的哈希逐字节相同。
  3. 过滤后 tree 不变的 trunk 推送之后，视图 tip 不变，映射不新增行。用例覆盖三种情形：推送路径落在全部源路径之外；推送只改动视图内被 exclude 的子路径（J4 丢弃）；被推路径 tree 不变的净零推送（ADR-TP-16：根不前进，根链不追加，`mono_api_service.rs` 约 L4058–4064 复用旧根提交）。过滤后 tree 改变的 trunk 推送（N=1 与 N>1 各一例）之后，视图恰好前进一个提交，`git fetch`（默认走 v2）增量成功。
  4. 启用视图前后，`/<path>.git` 的现有测试全部不变，包括 integration_git_cli 中 I1 与对齐相关的用例。
  5. 注册接口拒绝以下五类过滤器：Compose 重叠的、源路径在 import_dir 下的、`:nop` / `:empty`、src_paths 为空的、首段为保留名的。注册上限：7.4 的并发准入测试通过；超出规模上限的 filter_spec 得到 400；超过 `max_filters`、冷启动名额或速率时得到 429 加 `Retry-After`，且不写入任何行。
  6. `.view` / `.filter` 不能被创建。未知视图与 `enabled=false` 时，`info/refs`（v0 与 v2）返回 404，SSH（v0 与 v2）在首次宣告即返回一行 `ERR view not found` 与退出码 1。视图 URL 上的 receive-pack 经 HTTP 返回 403，经 SSH 返回一行 `ERR view URLs are read-only` 与退出码 1。`/.view/…git/info/lfs/` 下的 batch、对象上传和锁接口都返回 404（未知视图与 receive-pack 见 7.4 错误契约第 1、4 项；LFS 见 7.4"URL 与写入"第三条）。`enabled=false` 时，6.5 的注册与查询接口返回 404，且不写入任何行（7.4 注册准入）。
  7. want 指向不属于该视图的提交时被拒，v2 不带 `done` 的协商轮同样直接得到 ERR，不先返回 NAK（7.4 错误契约第 5 项）；请求 depth 或 filter 时返回明确的错误；`git -c protocol.version=0 clone` 与 Libra clone 视图 URL 都成功；已就绪但投影为空的视图，v0 与 v2 的 clone 都得到空仓库，v2 的 `ls-refs` 中没有伪 ref；闭包中含不自洽 L0 tree 的视图，经 HTTP 与 SSH 的 v0、v2 clone 都以 `remote error:` 结束，消息含 tree_id，服务端应答中没有 ACK、packfile 段与 pack 字节（7.4 错误契约第 5、6 项）；含 gitlink 的视图 clone 成功（6.3）。
  8. 冷启动未完成时，v0 与 v2（HTTP 与 SSH）在首次宣告即得到可重试错误：HTTP 为 503 加 `Retry-After`，SSH 为一行 `ERR view <filter_id> unavailable:` 加退出码 75。宣告之后转为未就绪时，`ls-refs`、`fetch` 与 v0 upload-pack 的 POST 返回 503，而不是 400。任何情形都不会得到空仓库或旧 tip（7.4 错误契约第 2、3 项）。
  9. `[views]` 中的未知字段被 validate 拒绝；在 review 形态下，或在非 sha1 部署下设置 `enabled=true`，都会拒绝启动。`[views]` 只写部分字段时，其余字段取 6.9 表中的缺省值。热改热加载类字段之后，下一次注册、下一次 advertise 和 worker 的下一批都使用新值，reload 报告列出这些字段；改动 `enabled` 或 `allow_anonymous_*` 只报告需要重启，行为不变。
  10. 根链：分段冷启动未完成时，新注册的视图不会被标为就绪；人工回滚 `main@/` 和根路径 CL 合并造成的不连续，都能被检测出来并返回 503；根链滞后超过 `max_append_walk` 时，视图在后台追平后照常服务，不进入 503。分段接入时，进程在两段之间重启，或者两段之间根链锁被其他调用方取得，都不被判为不连续。
  11. 基准按 7.4 冻结的协议运行，以下四项全部满足；任一项不满足，验收即不通过。
     - (a) 增量与 H 无关（语句数）：同一视图、同一探针在三档 H 下，单提交增量执行的 SQL 语句数逐一相等。这是主判据，与硬件无关；
     - (b) 增量与 H 无关（耗时）：同一视图、同一类探针在三档 H 下的 p95，最大值与最小值之比 ≤ k_H（建议 1.5）；
     - (c) 单提交增量不比冷启动的一批更贵：H_max 档单提交增量的 p95 ≤ k_B × 该视图冷启动投影段的平均每批耗时，平均每批耗时 = 投影段总耗时 ÷ ⌈H_max/B⌉（建议 k_B = 1）；
     - (d) 冷启动没有逐提交往返（G3）：H_max 档冷启动（根链构建加视图投影）的 SQL 语句总数 ≤ c_cold × ⌈H_max/B⌉（建议 c_cold = 50）。如果出现逐提交往返，每批至少要执行 B 条语句，远超这个值。
     冷启动吞吐（每秒提交数）只记录，不设阈值，作为附录 D 第 1 项的输入。
     【推断】原判据"与冷启动时单个提交的平均耗时处于同一数量级"删去。按 4.4，一次增量与冷启动的一批往返结构相同（1 条根链范围查询、1 条提交查询、每层 1 条 tree 查询，再加写入），一批只是行数最多多出 B 倍。冷启动摊到每个提交的耗时约为每批耗时的 1/B，B = 1000 时，单提交增量预计比它高两到三个数量级，原判据会因设计本身而失败。所以 (c) 改为与一批比较。
  12. 提交门禁全部通过。口径以 AGENTS.md 的「Required Checks Before Submitting Code Changes」与「Verification Checklist」为准，任务卡按 `docs/plan/plan-template.md` ER-04 的 C 组执行（约 L186、L254）。具体为：`cargo +nightly fmt --all --check` 无 diff；`cargo clippy --all-targets --all-features -- -D warnings` 零警告零错误；`source .env.test && cargo test --all` 全部通过；`cargo build` 与 `cargo build --tests` 均零错误零警告。跑全量测试之前，按 `docs/refactoring/test-infra.md`「fixture 生命周期」第 5 条（约 L72–82），以 `--profile git` 启动 compose `git-cli`；验收运行不得设置 `MEGA2_IT_ALLOW_HOST_GIT=1`。【代码】runner 不可用时，`integration_git_cli` 以 `git-cli runner unavailable` 直接 panic（`tests/common/git_cli.rs` 约 L20、L440–448），所以全量测试通过就证明 runner 已就绪，不另设证据项。基准（验收 11）不在这些门之内。

**P1：读侧完善**
| 内容 | 验收 |
|---|---|
| shallow 与 `filter=blob:none` | `--depth` 与 `--filter=blob:none` 的 clone/fetch 在视图 URL 上成功，并且视图链深度正确 |
| tag 投影 | 选取规则定案后，Josh 的 tag 相关用例或 mega2 自有用例通过 |
| REST 读接口（6.6） | refs/tree/commits 三个接口的结果与 git 侧一致；`commits/{id}` 能解析出根提交区间 |
| `mega2 view status\|rebuild\|gc` | rebuild 之后哈希不变；status 能显示滞后量 |
| GC | `mega2 view gc` 之后，它独占的对象在宽限期过后被回收，与其他视图共享的对象不受影响；7.4"回收与清扫"中的并发用例全部通过 |
| 不持久化视图提交字节（可选） | 按需重算的结果与持久化时逐字节一致 |
| Agent Capture 视图字段 | 契约文档修订完成，ingest 端到端测试通过 |
| 视图 GitHub binding 定案 | plan-20260920 写入相应决策 |
| ScorpioFS 视图挂载（跨仓） | ScorpioFS 侧验收另行确定 |

**P2：写入与扩展**
- 内容：
  - `view_push`（第 5 节）；
  - 非线性根链（附录 E）；
  - sha256/blake3，前提是 L0 写入者先完成 hash kind 显式化；
  - 物化前史 ADR（附录 D）；
  - 视图 GitHub binding 的实现；
  - MST/2 视图快照；
  - glob。
- 验收：
  - 5.4 表中每一行都有对应测试；
  - `view_push` 之后物化链的 I1/I2a 测试通过；推送前满足 I3 的行，推送后仍满足 I3（含 7.4 的三个陈旧行用例）；view_push 行的 I4 锚点可以解析；
  - LCP 为 `/` 时，续接候选数等于 |A|；以 `tip_at_R` 为准的 NFF 用例通过；
  - 同一推送重复执行是幂等的，对齐提示可见；
  - 附录 E 涉及的 Josh 用例通过：`prune_trivial_merge.t`、`initial_merge_elided_parents.t`、`empty_orphan.t`；根链非线性时，只读投影可用，view_push fail-closed，含 merge 已入表、投影已追平到它的情形（7.4 非线性闸门三例）；
  - sha256 与 blake3 视图的清表重建确定性测试通过；
  - 物化前史 ADR 有定案记录；若采纳，首次物化的路径的第一个提交以投影 tip 为父，且 I1 回归测试通过；
  - 视图 GitHub binding：同一个 GitHub 仓库不能同时绑定路径与视图；镜像出的历史与 `view_tip` 逐字节一致；
  - MST/2 视图快照：摘要包含 `filter_id@view_commit`，并有确定性测试；
  - glob：`reverse_glob.t` 与 Josh 的 glob 正向用例通过。

**P3：评估项**
- review 形态下的视图，包括视图 CL；Josh workspace 语义；树内视图定义 `/.mega/views/*.view`。

---

## 8. 与现有文档和计划的协调（实施时同步修改）
- **`docs/refactoring/trunk-push.md`。** P0–P1 不改其中任何不变式。P2 引入 `view_push` 时需要：
  - 把硬约束 2 与 I5 的写入者清单加上 `view_push`（TP-23 审计）；
  - 写明 I2 不适用于 `view_push`；I2a 在 `view_push` 下没有"被推路径"的例外；
  - 写明 I4 与 ADR-TP-15 在 `view_push` 下的形态：完整枚举落在根提交 m 上，各层用 `Mono-Squash-Commit` 指向 m，`view_push` 行锚定的视图链是 GC root；
  - 登记净零时对 ADR-TP-16 的例外（5.2 第 5 步）；
  - 补充 reaper 对 `view_push` 的 I3 语义；
  - 收窄 ADR-TP-20 理由中"后代方向不需要额外断言……续接语义本身就是自愈"的适用范围：它只对本轮子树有变化的行成立。【推断】`kind=push` 调用 `advance_descendant_refs_with` 时（`push_queue_service.rs` 约 L2761），同样受约 L669、L683 两处剪枝的限制，登记为既有限制，由 TP-11 断言与巡检兜底；
  - 写明共享谓词 `same_path_has_later_done` 改为覆盖谓词（5.2 第 9 步），四种 kind 共用。
- **`docs/refactoring/storage-events.md` 与 MST/2 发布规范**：写入 `view_push` 的事件与回执语义（5.2 第 8 步）。
- **`docs/refactoring/github-sync.md`**：在 OX 卡开工前，写入两条规则：视图 binding 与路径 binding 二选一；outbox 覆盖 `view_push`（6.8）。
- **`docs/refactoring/agent-capture.md` 与 `libra.md`**：P1 定义 checkpoint metadata 中的视图字段，以及 LandingRecord 所引用的对象（6.7）。
- **双语用户文档**：
  - `docs/architecture(.zh).md`：在第 6 节的协议面表格中加入视图 URL；
  - `docs/user-guide(.zh).md`：说明 `/<path>.git` 与视图 URL 的区别，以及版本与对齐规则；
  - `docs/configuration(.zh).md`：加入 `[views]` 配置节；
  - `docs/deploy-trunk.md`：加入视图运维。
- **trunk-push 阶段 6 的"物化 TTL + 墓碑"（DEFER-TP-04）**：它针对的是物化链在 B3 中的成本，与本设计相互独立。本设计不依赖它，也不取代它。
- **plan-20261001（Libra 黑盒 smoke）**：它的断言对象是 `/<path>.git`，本设计不改这个 URL 的语义，所以无需调整。视图 URL 的 Libra smoke 另立任务卡，内容包括：clone/fetch 成功；7.4 错误契约第 7 项；Libra 侧的宣告 ERR 分支，即 `parse_discovered_references`（Libra `src/internal/protocol/mod.rs` 约 L185）遇到 `ERR ` 包时返回带原文的错误，SSH 的 `read_advertisement`（`ssh_client.rs` 约 L842）读到 `ERR ` 包即停止、不再等待 flush。该分支落地之前，SSH 宣告阶段的视图错误在 Libra 上只能按退出码判定：75 为可重试，1 为不可重试。
- **v2 `ls-refs` 的零 tip 分支（P0）**：head 为零 ID 时不再输出 `capabilities^{}` 伪 ref（6.2）。这是对共享代码的协议修正，`/<path>.git` 不存在的路径在 v2 下的输出也随之改变；`docs/refactoring/protocol.md` 同步登记。
- **协议错误契约（P0）**：6.1 的错误契约，以及新增的 `RepoHandler::check_wants_and_ready`、`RepoHandler::prepare_pack` 及其调用位置，登记到 `docs/refactoring/protocol.md`：前者每轮都调用，早于任何 ACK/NAK；后者只在发包前调用；两者默认 `Ok(())`，Monorepo 与 ImportRepo 的行为不变。SSH 的 `channel_eof` 改为按通道状态发 exit-status，非视图路径的默认值仍为 0，行为不变。
- **测试契约文档（P0 起，随各阶段的用例增补）。** 三份文档分工不同，各按自己的口径登记，不互相复制：
  - `docs/refactoring/protocol.md` 的「场景覆盖表」是 Git 用户场景唯一的完整矩阵（约 L634）。为视图 URL 增加以下几行：clone、fetch、ls-remote、receive-pack 拒绝、LFS 404、未知或已禁用的视图（404 或 ERR）、未就绪（503 或 ERR）、已就绪的空视图。各列按该表的口径填写 `cargo:<exact_fn>`，不得用 clone/fetch 冒充其他行。P1 的 shallow 与 `filter=blob:none`、P2 的推送，到对应阶段实施时再加行。同一文档 Capability Truth Table 中 `fetch=shallow filter` 一行（约 L786）补注：ViewRepo 在 P0 不覆写 `supports_*`，请求 depth 或 filter 时同样返回明确错误（6.3）。
  - `docs/refactoring/integration.md` 只登记 cargo target 索引，并回链上面的矩阵（约 L34–36）。在「Active integration targets」中登记视图用例所在的 target 与 filter，例如 `integration_git_cli` filter `view`、`integration_git_ssh` filter `view`，写明用途和前置条件：PostgreSQL、Redis、`--profile git`。DB 模块测试（根链、catch_up、确定性、回收并发）参照该文档各迁移覆盖小节的写法，单列一节，写明模块路径与覆盖要点。基准不是集成测试，不进这张表，它的运行入口与冻结参数记在任务卡里（7.4）。
  - `docs/refactoring/test-infra.md` 是测试基建的事实源，管测试层次、fixture 生命周期、compose 服务登记与客户端版本 pin（约 L3–8）。本设计不新增 compose 服务：HTTP 黑盒沿用 git-cli runner，SSH 沿用 cargo-native self-start（约 L333–347），所以 P0 不改这份文档。视图 URL 下的 LFS 404 断言直接发 HTTP 请求，不经过宿主 git-lfs，以免落入该文档对宿主 git-lfs 版本的例外（约 L117）。如果基准或后续阶段需要新服务，先按该文档的 checklist 登记并评审（约 L87–106）。
- **L0 对象保真（另立任务，P0 不依赖）**：
  - receive-pack 在保存前拒绝 `to_data(from_bytes(raw)) ≠ raw` 的 tree 与 commit。tree 指含 `100664`、`100640` 或 GBK 文件名的 tree；commit 指 4.2 列出的三类形态。`receiver_handler` 把保存任务的 panic 当作失败返回；
  - 存量坏行分别用 `hash(sub_trees) ≠ tree_id` 与 `hash(rebuild_canonical_commit_bytes(row)) ≠ commit_id` 扫描找出，单独处置。【推断】commit 多数可以按 4.2 的形态枚举候选原始字节（补零位数、单空格空名、全大写 hex），以哈希命中为准恢复；枚举不中的只能人工处置。恢复后的字节存放在哪里、Monorepo 打包怎样改用它们，由该任务决定。根链上的提交恢复后，被阻塞的视图自动续追（4.2）；
  - 默认 `traverse` 对 gitlink 的处理一并修正；
  - P2 的 view_push 在 A 段写入的 tree 走同一条路径（5.1），同样受这一缺陷影响。
- **落地方式**：按 `docs/plan/README.md` 的模板拆分任务卡，在 `plan-long.md` 中新建 PT 编号。

---

## 附录 A：Josh 源码索引（本设计移植或参照的部分）
| 功能 | 位置 |
|---|---|
| 过滤器 AST | `josh-filter/src/op.rs::Op` |
| 求逆 | `josh-filter/src/opt/invert.rs::invert` |
| 解析（含 `::` 选择器） | `josh-filter/src/flang/parse.rs::parse`、`filter_presub` |
| 化简 | `josh-filter/src/opt/simplify.rs`、`opt/step.rs`、`opt/prefix_sort.rs`、`opt/mod.rs::optimize` |
| tree 过滤 | `josh-core/src/filter/mod.rs::apply_impl` |
| tree 逆向 | `josh-core/src/filter/mod.rs::unapply` |
| tree 运算（含空树规范） | `josh-core/src/filter/tree.rs::{subtract, subtract_inner, overlay, compose, insert_oid, replace_child_inner, get_path_entry}` |
| 单提交投影（Chain 逐级处理） | `josh-core/src/filter/mod.rs::apply_to_commit2` |
| 历史遍历 | `josh-core/src/history.rs::walk2` |
| 提交构造与丢弃规则 | `history.rs::{rewrite_commit, create_filtered_commit2, select_parent_commits, drop_commit, is_empty_root}` |
| 反向映射 | `history.rs::{unapply_filter, find_unapply_base, find_new_branch_base, find_oldest_similar_commit, find_original}` |
| 缓存与提交图 | `josh-core/src/cache/{transaction.rs, sled.rs, backend.rs::HistoryGraphHint, history_graph.rs::parents_share_root}` |

## 附录 B：mega2 代码索引（本设计涉及的部分）
| 功能 | 位置 |
|---|---|
| 写入队列（B0–B4、C 段） | `src/jupiter/service/push_queue_service.rs`（`b0_reject_push`、`execute_b3`、`run_c_segment_index`）、`src/jupiter/service/push_queue_reaper.rs::apply_i3`、`src/jupiter/storage/push_queue_storage.rs`（`MONO_WRITE_LOCK`）、`src/callisto/push_queue.rs`、`sea_orm_active_enums.rs`（kind 枚举）、迁移 `m20260905_000100_add_push_queue.rs`（`push_queue_active_push_path`） |
| 推送链 | `src/ceres/pack/push_chain.rs` |
| 根提交构造与签名 | `src/ceres/pack/trunk_provenance.rs`、`src/contract/vault/server_signing.rs`、`src/ceres/api_service/mono_api_service.rs::apply_push_in_txn` |
| 物化与墓碑 | `src/ceres/pack/materialize.rs`、`src/jupiter/storage/mono_storage.rs::{advance_descendant_refs_with, materialize_parents_in_txn}`、`src/callisto/mega_ref_tombstones.rs` |
| 产品写 API | `src/ceres/pack/api_tip_lander.rs::land_api_tip_push`、`src/api/api_write_auth.rs::authorize_trunk_api_write` |
| 协议 | `src/ceres/protocol/{smart.rs, v2.rs, mod.rs}`、`src/ceres/pack/mod.rs::RepoHandler`（`supports_shallow_fetch` / `supports_filtered_fetch`）、`src/ceres/pack/monorepo.rs` |
| URL 与授权 | `src/contract/git_protocol/{path.rs, http.rs, ssh.rs, mod.rs}`、`src/server/http_server.rs`（含 `rewrite_lfs_request_uri`）、`src/api/router/lfs_router.rs::enforce_trunk_lfs_access`、`src/common/errors/mod.rs`（`From<MegaError> for ProtocolError`） |
| 路径策略 | `src/ceres/pack/path_policy.rs::{classify_creation_path, check_write_operands}`、`src/config/validate.rs::validate_monorepo_path_shape` |
| 对象工具 | `src/jupiter/utils/converter.rs::sort_git_tree_items`、`src/ceres/merge_checker/gpg_signature_checker.rs::rebuild_canonical_commit_bytes`、`src/common/utils.rs::{is_signature_header, split_commit_message}`；git-internal `ObjectHash::from_type_and_data_for_kind`、`Commit::new_with_kind`、`Tree::from_tree_items_with_kind` |
| 批量读取 | `src/jupiter/storage/mono_storage.rs::{get_commits_by_hashes, get_trees_by_hashes}` |
| 索引、发布与事件 | `src/jupiter/storage/blob_path_index.rs::{index_tree_blob_paths, index_blob_paths_c_segment}`、`mono_storage.rs::record_publication_in_txn` |
| 迁移与实体 | `src/jupiter/migration/mod.rs`、`src/callisto/` |
| 配置 | `src/config/{model.rs, validate.rs, reload.rs}`、`config/config.toml` |
| 后台任务先例 | `src/server/http_server.rs::spawn_artifact_gc_task`、`src/jupiter/storage/blob_path_index.rs`（补偿任务） |
| 不变式与 ADR | `docs/refactoring/trunk-push.md`（硬约束、ADR-TP-02…20、附录 A） |

## 附录 C：v0.1 → v0.2 变更记录
| # | v0.1 | v0.2 | 理由 |
|---|---|---|---|
| 1 | 第 0 节基于上游 Mega | 改写为 mega2 现状 | 上游的多数事实在 mega2 已不成立（单提交限制、删除子路径 ref、只支持 v0 等） |
| 2 | §3.8 P1 迁移：废弃子路径链，`mega_refs` 只保留 `/` | **删除** | 与 I1/I2/I3、ADR-TP-12/16/19、I4 冲突；会改写所有客户端的历史，并让每次 N=1 推送都需要 reset |
| 3 | §6.1 `/<path>.git` 改由隐式视图应答 | **删除**；`/<path>.git` 不变（1.3） | 同上 |
| 4 | 反向推送以 CL 为载体（ViewUnapply、mega_cl 新列） | 在 trunk 下新增队列 kind `view_push`；review 形态下的视图移到 P3 | 交付形态 trunk 没有 CL；mega2 也没有 `merge_strategy.rs` |
| 5 | 多提交、含 merge 的推送，拓扑式反向映射 | 线性链，压成一个根提交 | ADR-TP-17；与 trunk 的 N>1 语义一致 |
| 6 | §5.6 去掉单提交限制 | 删除 | MC-06 已完成 |
| 7 | §2.5 Exclude 逆运算改写选择器路径 | 更正：选择器不改写 | 原写法允许写入被隐藏的路径，是安全缺陷 |
| 8 | §2.2 Compose 只约束输出路径不相交 | 源路径与输出路径都不相交（保守判定） | 源路径重叠时结果与 Josh 不同，反向会静默丢掉修改 |
| 9 | gpgsig 选项进入 filter_id，但语法中未定义 | v1 固定剥离 gpgsig 与 gpgsig-sha256，没有 meta 选项 | 消除内部矛盾；mega2 的根提交几乎都带服务端签名 |
| 10 | 冷启动按批 BFS | 根链表 + 按 seq 分批 | 线性历史上的 BFS 会退化为 O(H) 次串行往返 |
| 11 | commit_map 全量存储，P2 再改为稀疏存储 | P0 起采用游程映射 | `/` 线性时游程映射是精确的，行数降到 NEW 提交数 |
| 12 | 新增 `mega_commit.generation` 列 | 改用 `mega_view_root_chain`，并定义追加算法和连续性校验 | 不改热表；C 段乱序、review 形态根 CL 合并都可能破坏连续性 |
| 13 | Redis 锁；`MonoReceivePackFinalized` 事件 | 按视图的 advisory 锁（双整数键，测试中按 schema 隔离）；C 段只发信号，配合补偿任务 | ADR-TP-03；mega2 中没有该事件；C 段处在请求路径上 |
| 14 | 只支持 v0，v2/shallow/filter 放到 P2 | ViewRepo 支持 v0 + v2，沿用现有的能力诚实模式 | git 默认走 v2；mega2 已有 `supports_*` 先例 |
| 15 | 把 want 校验当作安全边界 | 定位为正确性与上下文裁剪 | mega2 的读接口是全局开放的 |
| 16 | 推送选项 `-o base=/create/allow_orphans/cl=` | P2 不需要 | 不支持新分支与 CL；mega2 的解析器也要另行改造 |
| 17 | 用 `mega_view_object.filter_id` 做 GC | 改为引用表，并规定引用不变式 | 不同视图会产生相同对象；memo 命中时也要补引用 |
| 18 | `mega_view_ref`、`mega_view_tree_cache`、`mega_view_push_map` | 都不建 | 分别由 projected_seq、commit_map.view_tree 与进程内 memo、push_queue 的幂等机制替代 |
| 19 | §4.3 修改 `apply_update_result` 中合并提交的作者 | 移出本设计（R7 另立 ADR） | mega2 中没有对应的同名函数；trunk 的 roll-up 已带真实作者 |
| 20 | 容量估算 20–30 GB | 重算，并计入对象表 | 原估算漏了行开销和对象表，而且基于全量映射 |
| 21 | Josh 测试中的重叠用例标为 skip；用 git 导入来构造历史 | 改为负向测试；用直接写库的夹具构造历史；补回 proxy 协议用例 | AGENTS.md 禁止 skip；推送和导入都构造不出 `/` 的历史 |
| 22 | BYTEA 主键 | hex TEXT + 整数代理键 | 沿用 mega2 的惯例 |
| 23 | hash_kind 只考虑 sha1/sha256 | 补上 blake3；P0–P1 只支持 sha1 | L0 写入者依赖 thread-local kind |
| 24 | 没有推送安全校验 | 新增 2.7 的三项校验，以及可推送视图的约束 | 同名覆盖、值域外内容、import 命名空间和根级写入，都会静默丢数据或越权 |
| 25 | §2.3 规则 4 无条件去掉 `:nop` | Compose 中的 `:nop` 不能去；规则 3 扩成四种情形；Chain 中的 `:empty` 吸收整条链；明确声明规则集不完备 | 每条规则都必须语义可靠 |
| 26 | 正向投影规则 R1–R5 适用于任意 DAG | P0 只处理线性历史；merge 规则（改称 J1–J5）移到附录 E；补上"顶层 Chain 逐级处理"的更正；J5 是否去重改为待定 | 根链是线性的；复合 tree 函数在 merge 边界上不等价 |
| 27 | 没有定义空树规范 | 冻结空树规范（4.1） | 否则规则 4 不可靠，投影结果偏离 Josh，P2 也会误拒合法推送 |
| 28 | `::f` 只匹配 blob（v0.2 初稿） | 与 Josh 一致，不论条目类型 | Josh 的 `Op::File` 不区分类型 |
| 29 | 只有 Josh 风格的 URL 是可选项 | 不提供 | 与 mega2 的路径归一化冲突，收益也小 |
| 30 | GitHub 按视图镜像"P0 起可用" | P1 定案，P2 实现；与路径 binding 二选一；outbox 覆盖 `view_push` | 与 plan-20260920 协调 |
| 31 | 3.1 的 `invertible` 列，3.2 的 `source` / `owner` 列 | 删除；新增 `push_enabled` / `ready_seq` / `projected_seq` / `warming_since` / `last_access_at` | 不再有隐式视图；需要区分"就绪"与"为空" |
| 32 | 没有说明视图的就绪状态 | 未就绪返回 503，严格区别于"视图为空" | 绝不把未就绪伪装成空仓库（ADR-TP-20） |
| 33 | 没有说明保留名、404、写入拒绝、LFS 和配置 | 补全（6.1、6.9） | 视图 URL 目前会落入 Monorepo |
| 34 | P0 验收中的"视图越权测试" | 删除；改为 want 正确性测试 | 第 15 条的推论 |
| 35 | 没有说明推送产生的 provenance 如何保存 | 新增 `mega_view_pushed_commit`，作为 GC root；新增 `Mono-View*` trailer | I4 |
| 36 | 没有区分持久数据与派生数据 | 在第 3 节导语中划分 | rebuild/gc 不能删掉定义与 provenance |

## 附录 D：待 ADR 的选项
1. **物化前史（P2）。** 路径首次物化且没有墓碑时，让物化出的第一个提交以 `:/P` 视图在上一个根提交处的投影 tip 为父。
   - 效果：`/<path>.git` 也能看到完整历史。
   - 代价与约束：
     - 投影提交会成为 L0 对象，必须写入 `mega_commit`；
     - 物化路径（ADR-TP-20 以及 7a 中的 B3 祖先物化）必须在 `MONO_WRITE_LOCK` 下完成该路径的投影，冷启动的成本落在 advertise 或推送上；
     - 这些提交的算法版本永久冻结；
     - 已物化的路径不受影响（I1）。
   - 是否采纳，等 P0 基准数据出来后再决定。
2. **review 形态的视图（P3）。** 需要解决以下问题：
   - 根链连续性：根路径 CL 合并时，父提交是 CL tip（0.5-23）；
   - 鉴权模型：`authorize_trunk_api_write` 在 review 形态下恒返回 401，review 形态也没有路径级的推送授权；
   - 视图 CL：
     - `ClSyncChecker` 以 `main@cl.path` 为基线，路径不存在时会因 `expect` 而 panic；
     - GPG 校验要求链上的每个提交都已签名，反向生成的提交天然通不过；
     - `fetch_or_new_cl_link` 按 (path, username) 查找 open CL，视图 CL 会和同路径的普通 CL 混到一起。

## 附录 E：非线性根链的投影规则（P2）
只有当根链出现多父提交时，才启用本附录（R11）。规则移植自 Josh【代码：`history.rs::create_filtered_commit2`、`select_parent_commits`】，并带有以下更正。
- **J1 initial merge。** 去掉 NULL 之后，如果过滤后的父提交多于一个、其中至少一个投影为空树、并且它们在视图历史中没有共同的根：还存在投影非空的父提交时，去掉全部投影为空树的父提交；全部为空时，一个都不去掉。判定"没有共同的根"需要每个视图提交的"可达根集合"摘要（Josh `cache/history_graph.rs::parents_share_root`），数据模型要相应扩展。
- **J2 平凡 merge 剪枝。** 前提：去掉 NULL 并经过 J1 之后，过滤后的父提交仍多于一个。此时若 `fpt[0].tree == t`，且 `tree(c.parents[0]) != c.tree`，就丢到 `fpt[0]`。这里 `fpt[0]` 指第一个存活的过滤后父提交，不一定是原第一父提交的投影。这条规则需要读取每个原父提交的 tree。
- **J3 select_parent_commits。** 当 `affects || all_diffs_empty` 时，保留全部过滤后的父提交。
- **J4 丢弃。** sel 为空、并且不属于"无父且 `is_empty_root(c.tree)`"的情形时：
  - 有过滤后的父提交，就映射到第一个父提交；
  - 否则投影为空树时，映射为 NULL；
  - 否则写出一个无父提交。
- **J5 合并父提交去重。** Josh 不去重，在某些 evil merge 下会写出 `[X, X]`。是否去重，在启用本附录时定案，并冻结进算法版本。
- **顶层 Chain 必须逐级过滤。** Josh 的 `apply_to_commit2` 对 `flatten_chain` 的每一级分别套用 J 规则，中间某一级为 NULL 时直接中止。在 merge 边界上，这与"复合 tree 函数套一次 J 规则"的结果不同。
  - 反例：`F = :/a:exclude[::c/]`。P1 侧 `a/` 下只有 `c/`；P2 侧 `a/` 下有 `d`；merge 提交 M 取 P1 的 `a/`，并带视图外的改动。
  - 逐级过滤：M 在第一级被丢到 A(P1)，第二级随之为 NULL。
  - 复合函数：M 只有一个非 NULL 的过滤后父提交 F(P2)，`t = EMPTY ≠ tree(F(P2))`，会写出一个空树提交。
  - 启用本附录前，要先用 Josh 实跑，把这个反例固化成测试。
  - 启用本附录后，filter_id 必须编码 Chain 的结构（2.3），算法版本也要升级。
- **根提交表与映射【决策，草案】。** 代数（generation）不能充当 3.3 的主键，也不能用来定位祖先：共同祖先 O 的两条分支 A、B 代数相同，而合并提交需要同时投影 A 与 B。启用本附录时：
  - **两列分开。** 根提交表收录 `main@/` 可达的全部提交，不只首父链。`seq` 仍是主键，含义改为唯一的拓扑序号，接入时保证每个提交的全部父提交 seq 都更小。另外新增四项：`generation`（= max(父提交的 generation) + 1，不唯一，只用于剪枝）；首父链标记；父边 `(seq, pos, parent_seq)`，pos 为父提交序位；`parent_count`（照抄暂存表的同名列，无父为 0），另建部分索引 `WHERE parent_count > 1`，供 B3 的非线性闸门（4.5）以 O(1) 判定根链是否含多父提交。
  - **祖先判定。** 只剩单向性质：a 是 b 的祖先 ⇒ seq(a) < seq(b) 且 generation(a) < generation(b)。3.3 中的 `is_ancestor ⇔ seq ≤` 与“首父闭包”只在线性区段成立；判定祖先必须沿父边回走，并用 generation 剪枝。3.3 的回走改为沿全部父边，多父本身不再判为不连续，但仍要求旧链尾是 h0 的首父祖先，否则首父链标记会失效。4.3–4.5 中以 `seq − 1` 指代父提交的地方（`tree_of(seq − 1)`、prev、seq(R) 的回溯），在非线性区段一律改为按父边取。
  - **映射。** commit_map 的主键与查找规则不变，`map(F, s)` 仍取 `seq_from ≤ s` 的最大一行。写入规则加一条：游程只沿“c 恰有一个父提交，且 seq(c) = seq(父) + 1”的边延续；其余提交（merge、无父提交、与父提交 seq 不相邻的侧枝提交）即使映射值不变，也单独存一行。这样查找规则在非线性区段仍然成立。单独存的行会重复 `view_commit`，因此 `UNIQUE (filter_pk, view_commit)` 改为部分唯一索引，只约束写出该视图提交的那一行，用新增的布尔列标记。
  - **视图 tip。** 取 seq ≤ projected_seq 的最新一个首父链提交的映射，不能直接取 `map(F, projected_seq)`。拓扑序上第 projected_seq 个提交可能在侧枝上，广告它之后，tip 会非快进。
  - **迁移。** 现有根链行原样有效：generation = seq，首父链标记为真，父边为 `(seq, 0, seq − 1)`（seq = 1 无父边）；`parent_count` 为 1（seq = 1 为 0），部分索引因此为空。现有 commit_map 行也满足上述写入规则。只需补列与父边，不需要重建。
- **反向（P2 不使用）。** P2 的推送基准恒为锁内的 R（4.5）；根链一旦出现多父提交，view_push 就 fail-closed（4.5 非线性闸门、5.2 第 2 步）；多父提交已入表时也是如此。所以 P2 不需要 `unapply_base`。以下只是另立 ADR 时必须满足的约束：
  - `unapply_base(F, vp, M)` 取 M 的祖先中映射为 vp 的最新提交，与 Josh `find_unapply_base` 一致。可以先经 commit_map 缩小候选范围，但每个候选都必须沿父边验证确实是 M 的祖先。不得用 `root_chain[b − 1]` 这类序号运算定位，因为拓扑序上相邻的提交未必是 M 的祖先；
  - 新分支：用 `find_new_branch_base` 加 `find_oldest_similar_commit`，取最旧的那个；
  - new == old：用 `find_original`；
  - 平局时，按 Josh `GenerationFrontier` 的插入顺序决定。

## 附录 F：Codex 五轮审计记录（2026-10-02）
审计方式：每轮用 `codex exec`（只读沙箱，gpt-6-sol，reasoning xhigh）对当前快照独立审计，每轮侧重不同；每条结论都由 Claude 代理回到 mega2、git-internal 0.10.2、Libra 源码逐条核实，然后才修订。每轮的提示词都附上前几轮的处理记录，避免重复报告。没有运行构建或测试。

| 轮次 | 侧重 | Codex 报告 | 核实结果 | 主要修订 |
|---|---|---|---|---|
| R1 | 全面：代码事实、P0 可实现性、内部一致性 | 14 条（high 7、medium 7） | 全部采纳：12 条成立，2 条部分成立；5 条下调严重度 | 3.3 根链追加改为"回走暂存表 + 三态结果 + 首父闭包"，seq=1 无父；0.5-18/18a 记录 L0 tree 字节不保真（L0 既有缺陷），6.3 打包时校验；6.1 入口分三层（定位符 → 按服务与配置拒绝 → 异步解析），覆盖 LFS、v2 info/refs 与 SSH；新增 ViewUnavailable（503）；6.3 列出 RepoHandler 17 个方法；5.2 第 8 步事件移到提交之后；3.6 改为复合主键；6.9 取值范围 |
| R2 | 并发、事务、锁、崩溃恢复、I1–I6 | 7 条（high 3、medium 4） | 全部采纳：5 条成立，2 条部分成立 | 3.3 段事务内重新判定、同一事务内换锚点，并定义锚点不变式；4.5 区分 view_tip（只供读侧）与 tip_at_R（锁内写入判定）；新增 4.6（锁表 L_M/L_C/L_V/L_G、全局锁顺序、先标记后删除的清扫、回收与 rebuild、单快照读取）；5.2 第 7 步定出四条续接规则与保证范围，按 A 过滤候选；B0 拆成静态准入 → B1 幂等解析 → 状态预检；C 段跳过条件改为前缀覆盖 |
| R3 | 算法与确定性 | 4 条（high 2、medium 2） | 全部采纳：3 条成立，1 条部分成立；均定为 medium | 4.2 写明前提校验失败的语义（三类无法往返的形态，停在 s−1）；附录 E 拆开 generation 与唯一拓扑序号；2.2 用 pull 回拉给出 src_paths 的精确定义，空集拒绝注册；2.1 定义 PATH 编码与 canonical 字节契约，附黄金向量 |
| R4 | 协议、API、鉴权、运维、测试 | 7 条（high 1、medium 5、low 1） | 全部采纳：4 条成立，3 条部分成立 | 6.1 新增错误契约（HTTP 状态码、200 加 ERR、SSH 用 ERR 加退出码 75/1，并说明 git 与 Libra 各自的表现）；新增 prepare_pack 钩子（GitError 会抹掉错误类型）；6.5 注册准入（在 L_R 下的 Postgres 短事务内处理速率、max_filters、冷启动名额，过滤器规模设上限）；6.9 给出缺省值与热加载类别；7.4/7.5 基准协议与可客观判定的验收、构建门禁 |
| R5 | 终审：回归、端到端、不变式 | 8 条（blocker 2、high 1、medium 5） | 全部采纳：6 条成立，2 条部分成立；两个 blocker 下调为 high | 就绪与推进水位放在同一事务；缺对象不再当作路径不存在（MissingObject）；非线性闸门；预检拆成 check_wants_and_ready 与 prepare_pack；enabled=false 时接口返回 404；gc_grace_secs 下界设为 3600；闲置回收规则；去掉 dirty 握手 |
| 复核 | Claude 对 R5 修订做两路回归核查 | — | 18 项，其中 2 项重复（medium 5、low 13），全部修正 | gc_marked_at 由 catch_up 在同一事务内清空；root_chain_halted 谓词；6.1 第 3 层职责补齐；P0 回收态可经重新注册离开；只由同步调用方唤醒 worker，避免空转；锁键分开命名空间；"删除视图"统一为 `mega2 view gc <filter_id>` |

**没有采纳的 Codex 建议（及理由）：**
- R1-F2：Codex 建议"优先保存原始 tree 字节"。存量对象的原始字节已经丢失（pack 不落盘），改在打包时校验，并另立 L0 修复任务。
- R2-F7：Codex 建议对锁内续接候选数设硬上限。重试不会让候选数变少，硬上限会让大视图永远无法推送，改为按 A 过滤并用指标观测。
- R3-F1：Codex 建议 P0 新增原始提交字节列。trunk 根链上的提交都由服务端规范构造，存量字节也无从恢复，改为 fail-closed 加审计，并另立任务。
- R5-F6：Codex 建议给发包加读租约。读租约要长时间占用连接与事务，而它只避免一次可重试的失败，改为宽限期下界加"先发视图对象"的发包顺序。

**审计后的结论：**
- P0（只读投影，仅 trunk、sha1）的端到端规定已经闭合，可以进入任务拆卡：注册与准入 → 冷启动与增量 → 就绪 → advertise/fetch → 错误应答 → 停服回收。
- 拆卡时，以下数值都标为待基准复核，先在任务卡中冻结：冷启动吞吐、过滤器规模上限、`max_concurrent_cold_starts` 等缺省值。
- P2（view_push）与附录 E 仍是草案，开工前要按第 8 节修订 trunk-push.md 的相关不变式与 ADR。
- 两项 L0 既有缺陷另立任务，P0 不依赖它们：tree/commit 字节保真；保存任务 panic 时对象被静默丢弃（第 8 节）。
