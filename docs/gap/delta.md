# Delta / DeltaDB 对照分析：mega2 的 forge 侧应对

本文档以 Zed Industries 的 **Delta**（客户端 + DeltaDB 云同步中枢）为参照系，分析 mega2 作为 **Git 托管服务引擎（forge）** 在 AI agent 协作时代应当补齐、坚守与拒绝的能力，并给出分组建议与实施顺序。

> **关键依赖**：本文**不占用** `docs/plan/plan-long.md` 的 `PT-*` 编号空间，也不与 `docs/plan/plan-20260827.md` 的 `MC-*` 冲突；建议进入路线图必须按 `plan-long.md` §路线图维护 的规则重新立项（新候选须给出竞品 revision/path、mega2 缺口、价值、风险、依赖与最小切入点）。与 `plan-20260827.md`（CL 多 commit push 放开）存在**写集相交与正确性前置**关系，见 §6.3；与 `plan-long.md` 的 `PT-03`（Git 协议兼容性与 LFS 收尾）、`PT-05`/`PT-06`/`PT-07`/`PT-08`（ceres/bus 事件总线与 Orion 三件套）存在合并评估关系，见 ME-B1 与 ME-A5。

## 事实校准（2026-08-27）

**mega2 侧（一手，逐条源码复核）**

| 项 | 值 |
|---|---|
| checkout | `git HEAD = 0836675`（`fix(test): align LFS batch URLs with git-cli bridge networking`） |
| 版本 | `Cargo.toml` version `0.3.5` |
| 形态 | 单 package `mega2`（lib `mega2_core`），Rust 2024 |
| 核对范围 | `src/ceres/`、`src/jupiter/`、`src/callisto/`、`src/api/`、`src/contract/`、`src/orbit*/`、`docs/plan/`、`docs/refactoring/`、`docs/manuel/` |

**Delta 侧（二手转引 + 一手来源留档）**

Delta 闭源私测、DeltaDB 未开源，**本仓库内没有、也不可能有它的 checkout**。本文的 Delta 事实转引自 sibling 仓库 `libra` 的竞品分析文档 `docs/development/gap/delta.md`（160 行，2026-08-27 版，已过两轮外部评审）。文中形如 `delta.md:NN` 的锚点供持有该仓库者交叉核对；**不持有该仓库的读者请直接查附录 A 的一手来源**——本文所有承重结论都在正文内给出了引文，不依赖读者能打开 `delta.md`。

> **⚠️ 关于 `docs/plan/plan-20260827.md` 的引用纪律（必读）**：该文件在本文成稿时**未被 git 跟踪**（`git status` → `?? docs/plan/plan-20260827.md`）且**正在被并发编辑**——核对开始时其 Review log 为 R1–R11，成稿时已增至 **R1–R12**（`mtime` 四分钟内前进），八张卡的 `Lifecycle` 行号同期从 `345…` 漂到 `348…`。**因此本文对该文件的全部引用一律改用「卡号 / ADR 号 / 章节名 + 原文片段」锚定，不使用行号**；请用原文片段 `grep`，不要用行号定位。其它文件（`src/*.rs`、`plan-long.md` 等）的行号引用保留，成稿时已逐条抽查复核。

**编号约定**：本文用 `ME-A*`（与既有规划契合）/ `ME-B*`（Mega 之外的新方向）/ `ME-C*`（应坚守、不跟进）。编号一经引用不重编；被推翻的条目改标注为「已实现 / 已替代 / 不采纳」并保留理由，不删除。

**本文性质**：只读分析与建议，成稿过程未修改任何源码。经三方对抗式复核（事实核验 / 可行性与冲突核验 / Delta 侧核验）+ 一轮可行性复审后定稿，所有被证伪的论断已删除或改写，处置逐条见文末「修订记录」。**本文不宣称任何实现完成**——按 `docs/plan/README.md` 规则 6，落地时每个建议都必须先刷新源码锚点、再按任务卡验收。

---

## 0. 先说三个必须先纠正的前提

在给建议之前，有三条流传中的表述需要按源码校正，否则后面所有推论都会偏。

**① 「plan-20260827 已放开多 commit push」不成立。** 八张卡（MC-01/02/03/06/04/05/07/08）全部 `Lifecycle=pending`（本轮逐卡实测，八处 `**Lifecycle / Acceptance:** \`pending\` / （空）`），`"only single commit support in each push"` 一行未动（`src/ceres/pack/monorepo.rs:230`）。这是一份成熟度极高（Codex 评审 **R1–R12，截至本轮核对 2026-08-27 14:07，仍在增加**）但零落地的计划。所有涉及它的建议必须写成「协同/前置」，不能写成「在已有基础上扩展」。

**② 真正抹掉用户署名的不是 merge，是 Update Branch。** `update_branch`（`src/ceres/api_service/mono_api_service.rs:3190` 起）把整个 CL 压成**一个** `mega <admin@mega.org>` 署名、消息固定 `"update-branch: rebase"` 的 commit（`:3279` → `apply_changes_as_single_commit` `:2080`），并同时改写 CL 的 `from_hash` 与 `to_hash`（`:3282` `update_cl_hash`）。叠加 `merge_cl` 的 `from_hash == main head` 硬校验（`:2071-2073`），常态路径是：

```
用户 C1（作者=用户）
  → Update Branch → R1（作者=mega bot，msg="update-branch: rebase"）
  → merge         → M1（作者=mega bot，msg="cl merge generated commit"）
```

用户的 commit 在进 trunk 之前就已经不存在了。

**「Update Branch 是常态」这一表述需要按路径分档**：`merge_cl` 的硬校验比的是 `get_main_ref(&cl.path)`（`:2066` 附近）——**按 CL 自己的路径取 main ref**。对**根路径 CL**（`cl.path == "/"`，默认 clone URL 场景），任何并发合入都会推进它，「常态」精确；对**子路径 CL**，只有触及该子路径的合入才会推进它，因而是「触及即触发」而非无条件常态。这个 root/subpath 区分与 §2.3 断裂 C 是同一条轴，全文沿用。

这一点比 ADR-MC-01 描述的严重得多，且**它不在 plan-20260827 任何一张卡的写集里**（全文 grep `update_branch` / `apply_changes_as_single_commit` 零命中，已实测）。

**③ mega2 已有一套完整的、可审计的锚定评审子系统——但它的性质被普遍误述，能力也被高估。** 它不是「内容锚」，是「带校验的行号锚 + diff 偏移」；它相对 Delta 的真实优势比传闻窄得多，且其中一档恰恰是 Delta 更强。见 §2.5。

---

## 1. 定位：mega2 不是 Delta 的竞品，是 Delta 需求说明书的接收方

### 1.1 形态对位

Delta 是 **客户端 + 云后端**：一个 Rust 桌面应用（同一份代码编译到 WASM 跑网页版，`delta.md:43`）+ DeltaDB 同步中枢。mega2 是**服务端 / forge**：Git wire 协议、CL 协作面、授权、构建触发、产物存储。两者在产品坐标上不重叠。

但 delta.md §2 的七条技术要点里有一条决定了关系性质：**Delta 与 git 严格互补**（`delta.md:57`）——「项目必须是 git 仓库」「Commits stay in git」「托管 checkout 有两个 remote：`origin`（原上游）与 `local`（指回用户本机仓库）」。**Delta 从设计上承认 forge 是最终事实源。** 它不想取代 origin，它想在 origin 之上加一层。

所以正确的读法是：**Delta 是一份来自客户端侧的、非常具体的服务端需求说明书**，上面写满了「你没给我，所以我自己建了」。

### 1.2 三层存储对位

事实列只保留 delta.md 原文（`delta.md:75`）与 mega2 源码；产品特性推断一律移入判断列并标注。

| DeltaDB 层（delta.md:75 原文） | mega2 对位物 | 判断（含推断） |
|---|---|---|
| **Git 对象（Cloudflare R2）** | **两层拆分**：blob 字节 → `src/orbit/` 可插拔对象存储，key 形如 `<ns>/aa/bb/cc/<oid[6..]>`（`src/orbit_api/object_storage.rs:28-46`，`ObjectNamespace::Git → "git"` `:119`；<6 字符的 key 不分片）；commit/tree/tag/ref → Postgres 结构化行（`mega_commit` / `mega_tree` / `mega_tag` / `mega_refs`；`mega_blob` 存的是 blob **元数据** `blob_id/name/size/pack_id/file_path/commit_id`，字节仍在 orbit） | **mega2 更重也更强**。*（推断）* R2 侧对象图对 DeltaDB 而言大概率是不透明存储；mega2 把图结构化进关系库——查询、授权、diff、blame 全部因此可行，代价是写放大（一次改 N 文件的 push ≥ N 次 PUT + N 次 SELECT + N 次 UPDATE）。**须注意**：`delta.md:69` 记录 Delta 有跨文件 diff 搜索、`:54` 记录代码↔会话双向跳转，说明其上层能力不弱，「不透明」只能作为存储形态的推断，不能推出能力结论 |
| **线程/未提交编辑的 delta（Durable Objects/SQLite）** | **无对位物** | 这是 Delta 的独有资产，也是它最大的合规负债。*（推断）* DO 的单实例模型天然给出强一致与订阅面，delta.md 未记录 |
| **元数据（KV/D1）** | Postgres 单库，**65 个** callisto 实体（`ls src/callisto/*.rs` 去掉 `mod.rs`/`prelude.rs`/`sea_orm_active_enums.rs`）：CL、issue、评审线程、锚点、check、merge queue、webhook、bot、审计 | **mega2 远比 Delta 丰富** |

**第一层和第三层 mega2 都有且更强，唯一缺的是中间层。** 这不是巧合：git 从来没有「一个工作单元 + 挂在它上面的对话与未落盘编辑」这个容器，所以谁想做 agent 协作，谁就得自己发明一个。

但**这里有一个必须切开的分界线**：Delta 的中间层同时承载「过程」（对话、评审、agent 活动）和「未提交编辑」（commit 前的试错）。这两者的性质完全不同：

- 「过程」是协作资产，forge 应该保存；
- 「未提交编辑」是本地私产，**forge 不应该知道**。

Hacker News 对 Delta 批评最多的正是后者（`delta.md:104`：「commit 前的死胡同与半成品本属本地，Delta 默认全部序列化上云」），且 Delta 官方承认删除不完整（`delta.md:75`，"it does not yet remove already-synced copies from our servers"）。自建 forge 的核心卖点是数据主权，主动收集未提交编辑等于自毁卖点。

> **设计原则（贯穿全文）：forge 只在 push 之后知情。** mega2 要补的中间层是「push 之后、merge 之前」的过程记录，不是「编辑器里正在发生什么」。

### 1.3 真正的战略风险不是竞争，是脱媒

Delta 的 DeltaDB **按 git remote URL 给仓库做键**（`delta.md:75`）——"Organization members working from the same remote use the same stored repository data"，即租户边界由 remote URL 隐式划定。拆开看这句话对 forge 意味着：

1. **forge 的 remote URL 成了第三方数据库的 join key。** 同一个 mega2 实例的成员，他们的 agent 会话、未提交编辑、worktree 历史会**自动汇聚**到 DeltaDB 的同一条记录上。
2. **隐式租户边界由 forge 定义，但由第三方执行。** forge 既不知情也不可否决。
3. **forge 完全无可见性。** 无法枚举、无法审计、无法在员工离职时撤销、无法回答「我们的代码副本还有几份、在哪」。

这条推论链不依赖单一引语，基线内另有三条独立佐证：`delta.md:57`（托管 checkout 带 `local` remote，agent 可把分支直推用户仓库，**含当前检出分支**）；`delta.md:69`（接受 = 纯 git，**评审与接受动作发生在 Delta 内，forge 只看到一次成品 push**）；`delta.md:71`（分享线程即授予其挂载仓库的 worktree 历史访问，仓库级非线程级，**官方文档明示此扩权**）。

对私有部署的 mega2 而言，这是一个**结构性的影子数据面**，且对用户无感——用户只是装了个好用的客户端。

**风险的终局形态是脱媒（disintermediation）：forge 退化成一个哑的 git origin，而 identity、权限、评审、执行、决策链全部搬到伴生层。** 本文所有建议围绕同一个问题组织：**哪些能力必须留在 forge，才能让 forge 不被架空。**

反过来说，谈判地位其实很好：**只要 forge 提供了挂载点，伴生层就没有理由自己存**。forge 不提供挂载点，伴生层就只能自己存，然后影子数据面必然发生。

---

## 2. 核心张力：CL 模型 vs「聚合快照丢失决策链」

### 2.1 靶心

Nathan Sobo 的主张（`delta.md:47`）：*"Increasingly, the conversation that generates the code is becoming the true source of our software"*；*"Forcing every AI interaction through the commit-based workflow is like trying to have a conversation through a fax machine"*。核心是「对话才是真正的源」与「commit 工作流是错误的传输管道」。

**mega2 是这个批评的最大靶心**，因为 CL 是最大化聚合的形态：

- CL = `(from_hash → to_hash)` 聚合 diff（ADR-MC-02：「CL 语义不变。链式 push 后，CL 的 `from_hash` = 链 base 的 parent 基线、`to_hash` = 链 tip；评审者看到的永远是聚合 diff」）；
- CL merge **永远**在 `refs/heads/main` 上生成一个**全新的单父 commit**（ADR-MC-01 Decision：「用户 push 的中间 commit 永远不上 main，仅保留在 `refs/cl/<link>` 供审计与 UI 展示」），消息硬编码 `"cl merge generated commit"`（`mono_api_service.rs:2536`），作者恒为 `mega <admin@mega.org>`（`git-internal 0.8.7` 的 `Commit::from_tree_id` 硬编码）；
- 即便 MC-06 放开多 commit push，trunk 侧**零改动**（ADR-MC-01 Consequences：「trunk 线性历史与 commit 数量解耦；本计划 trunk 侧零改动」）。

Delta 说「软件诞生于 commit 之间」，mega2 说「trunk 上只留结论」。正面冲突。

### 2.2 判断：CL 模型提供了正确的容器，但「trunk 只留结论」的设计取舍把决策链 100% 押在 CL 侧，而 CL 侧今天有三处断裂

诚实的表述是三段：

**① 容器成立。** Delta 发明 thread，是因为 git 没有「一个工作单元 + 挂在它上面的对话」这个容器。mega2 已经有了，而且 `link` 已经是全仓通用的 join key（六处列名已逐一核对）：

| 表 | 列 | 挂什么 |
|---|---|---|
| `mega_cl` | `link` | CL 本体（工作单元） |
| `mega_code_review_thread` | `link` | 行级评审线程（带锚定与位置） |
| `mega_conversation` | `link` | 时间线会话（**15 种** `ConvTypeEnum`，已实测计数） |
| `check_result` | `cl_link` | 检查结果 |
| `merge_queue` | `cl_link` | 合并队列 |
| `mega_cl_reviewer` | `cl_link` | 评审人与批准 |

（`issue_cl_references` 也参与 CL 关联，但用的是通用 `source_id/target_id` + `reference_type`，不是 `link` 列，本表不列。）

**Delta 的 thread ≈ mega2 的 CL。** 缺的只有一件：一条 agent 会话记录，用同一个 `link` 挂上去。

**② 但「trunk 只留结论」是一个设计取舍，不是中立事实。** ADR-MC-01 的 Status 是 **`Accepted`（用户决策，2026-08-26）**，这是**已被采纳的设计取舍**，不是未接线的实现缺口。它把决策链的保全责任**全部**押在 CL 侧。按 Delta 的判准，mega2 今天在 trunk 上保留的决策链**比一个普通 PR forge 还少**——PR 至少保留作者 commit，而前提 ② 的常态路径连作者 commit 都没有。「trunk 存结论 / CL 存过程」的两层分离在架构上确实比把过程与结论塞进同一个复制数据结构更清晰，但这个清晰度只有在 CL 侧真的存住了过程时才兑现。

**③ 而 CL 侧今天有三处断裂。** 所以**风险既在设计（押注集中）也在实现（押注未兑现）**。这不削弱后面的行动项——ME-A1/A2/A3 全部照旧成立——只是把定性摆正。

mega2 确实把「commit 之间」的东西结构化存进了 DB（评审讨论、行级线程、checks、reviewer 决策），保真度比 git 原生高得多；Delta 只是把它做成了交互体验。这一点仍然成立。

### 2.3 三处断裂（其中两处是计划未覆盖的新发现）

#### 断裂 A：`mega_cl_commits` 零接线（已知，GAP-05）

`ClStorage::save_cl_commits`（`src/jupiter/storage/cl_storage.rs:436`）全仓**零调用点**（已 grep 实测），无读取函数，无 API。表结构是 `{created_at, updated_at, cl_link, commit_sha, author_name, author_email, message}`，主键 `(cl_link, commit_sha)`，**无顺序列**。

**「无顺序列」不是未被登记的缺陷——它是 MC-04 明文写定的设计决策。** MC-04 的 Out of scope 第四条逐字写着「表加顺序列：**永久非目标**，理由 = 链序在读取时由 parent 拓扑重建（见 AC），无需 schema 变更」；AC 对应写着「`ClStorage::get_cl_commits(link)` 按 parent 拓扑序返回 `sha/author_name/author_email/message`（**表保持集合语义、不存顺序列**，排序依据写入代码注释）」。ME-A3(a) 是对该已决 AC 的**显式挑战**，见其条目。

#### 断裂 B（新发现，计划未覆盖）：`update_branch` 的**两条**写回路径都会破坏 MC-04 依赖的不变式，其中 no-op 路径**无条件断链**

**本小节在本版中被完整重写**——上一版只核了 `update_branch` 的主路径，遗漏了 no-op 路径，并据此得出了一个过强的证伪结论。已实测的完整事实是：`update_branch` 有**两条**写回路径。

**共同前置守卫**（`mono_api_service.rs:3215-3217`）：

```rust
if target_head == cl.from_hash {
    return Ok("Already up-to-date".to_string());
}
```

即函数体后续代码执行时，`target_head` **必然 ≠ 旧 `from_hash`**——`target_head` 是 main 上晚于该 CL 分叉点的某个 commit。

**判据**（`:3242-3253`）：

```rust
let old_blobs = self.get_commit_blobs(&cl.from_hash).await?;   // :3242
let new_blobs = self.get_commit_blobs(&cl.to_hash).await?;     // :3246
let cl_changed = self.cl_files_list(old_blobs, new_blobs).await?; // :3250
```

`cl_changed` 是 **CL 自身的变更集**（旧 base 树 vs CL tip 树）。为空 ⇒ `to_hash` 的树等于（旧）base 树。

**路径 ①（主路径，`cl_changed` 非空，`:3279-3282`）**：`apply_changes_as_single_commit(&cl, &cl_changed, &target_head)` 在 `target_head` 之上生成一条 bot commit `new_head`，随后 `update_cl_hash(cl, &target_head, &new_head)`。此路径下 `from_hash = target_head` 且 `new_head` 的 parent **就是** `target_head`——是一条长度为 1 的完好链，parent 拓扑重建照样工作。**它的问题不是断链，是链上只剩一条 bot 记录**：CL 的全部用户 commit 被压平，旧 commit 变成无 ref 引用的孤儿（`clean_dangling_commits` 是注释掉的 TODO，`:2544`）。

**路径 ②（no-op 路径，`cl_changed` 为空，`:3255-3257`）**：

```rust
if cl_changed.is_empty() {
    // No-op rebase: just advance base hash and log.
    stg.update_cl_hash(cl.clone(), &target_head, &cl.to_hash)   // :3257
```

**`from_hash` 前移到 `target_head`，而 `to_hash` 原样不动。** 结合上面的守卫：`target_head ≠ 旧 from_hash`，而 `cl.to_hash` 的祖先链只经过旧 `from_hash`——**`target_head` 不是 `cl.to_hash` 的祖先**。CL 进入一个 `from_hash` 与 `to_hash` **不在同一条 parent 链上**的状态。

**这条路径对 plan-20260827 的影响是无条件的、且不止 MC-04 一张卡**——计划里有三处「从 `to_hash` 沿 parent 链反走到 `from_hash`」的消费者：

| 卡 | 反走 parent 链做什么 | no-op 断链后的行为 |
|---|---|---|
| **MC-02** | `gpg_signature_checker` 逐 commit 验签 | AC 已含「`from_hash` 不在链上（断链/伪造 to）时 fail-closed 报错」→ **该 CL 的 gpg 门永久 fail-closed**（若该门启用/required），CL 变为不可合 |
| **MC-03** | 链式校验器（受 `MAX_CL_CHAIN_COMMITS` = 250 约束） | 反走不终止 → 触 250 上限报错，或走到根 |
| **MC-04** | 清单全链重建 | **AC 中没有「`from_hash` 不在链上」的 fail-closed 谓词**（只有「链中 commit 缺行时 fail-closed」，那是另一回事）→ 要么产出整仓历史，要么撞 250 上限 |

**顺带登记的计划内部不一致（本版新发现）**：MC-02 的 AC 显式写了「`from_hash` 不在链上（断链/伪造 to）时 fail-closed 报错」，**MC-04 的 AC 没有对应谓词**。计划作者已经预见到断链，但把它归因为「伪造 to」这种对抗场景，未预见到 `update_branch` 这条**一方代码路径**会合法地制造它，也未把该谓词同步到 MC-04。

**MC-04 的触发时机需订正**：MC-04 的重建挂在 push 管线里（AC 要求与 `update_or_create_cl` 同事务），而 `update_branch` 是 REST 调用，**不走 push 管线、不会当场触发重建**。实际序列是：

```
Update Branch（任一路径）→ 清单与新的 (from_hash, to_hash) 失配
                        → 该 CL 的下一次 push 命中 AC「CL 存在而清单缺失或与当前 to_hash 不符时幂等补齐清单」
                        → 路径①：先删后插，重建为「只有一条 bot commit」的单条记录
                        → 路径②：重建无法终止于 from（见上表）
```

实施方若按「去 `update_branch` 里找调用点」的思路排查会扑空；失配窗口在两次事件之间持续存在。

**这是设计级问题，不是实现瑕疵。** `update_branch` / `apply_changes_as_single_commit` 在 `plan-20260827.md` 全文零命中，不在任何一张卡的写集里。

**由路径 ② 推出的一条更严重的后果（强推理，待实测复现——已列入 ME-A1 的实测批次）**：

- no-op 后 `cl.from_hash == main head`，`merge_cl` 的硬校验（`:2071-2073`，比的是 `cl.from_hash != get_main_ref(&cl.path).ref_commit_hash`）**放行**；
- `merge_cl_unchecked` 取 `commit_model = get_commit_by_hash(&cl.to_hash)`（`:2517-2521`），用 `commit.tree_id` 走 `build_result_by_chain(path, update_chain, commit.tree_id)`（`:2535`）再 `apply_update_result(..., "cl merge generated commit", ...)`（`:2536`）；
- 而 `cl.to_hash` 的树 = **旧 base 树**（no-op 分支的成立条件）。

⇒ **空 diff 的 CL 经一次 no-op rebase 后 merge，会把旧 base 树写回 main，静默回退期间他人已合入的改动。** 根路径 CL 是整树回退；子路径 CL 是该子树回退。若断裂 C 同时成立（根路径 parent 命中 CL ref），main 的新 head 的 parent 会是 CL tip，**期间合入的 commit 一并从 main 的可达集脱落**。

**标注**：以上为纯代码推理，**未实测**。复现步骤见 ME-A1。

#### 断裂 C（新发现，会让在途计划的验收当场失败）：根路径 CL 的 merge commit parent 可能不是旧 main head

代码链（逐段核对）：

- `apply_update_result`（`mono_api_service.rs:2589-2597`）构造 `cl_refs = [refs/cl/<link>, MEGA_BRANCH_NAME]` 并调 `get_refs_for_paths_and_cls(&paths, cl_refs)`；
- `get_refs_for_paths_and_cls`（`src/jupiter/storage/mono_storage.rs:95-102`）带 `.order_by_asc(mega_refs::Column::RefName)`；
- `process_ref_updates`（`:609-645`）对每个 update 取 **`refs.iter().find(|r| r.path == update.path)`——第一个 path 匹配项**（`:618`）；
- `"refs/cl/…"` < `"refs/heads/main"`（`c` < `h`），所以同一 path 上同时存在两条 ref 时，`find` 命中的是 **CL ref**；
- 紧跟着 `:638-640`：`push_update(&p_ref.ref_name); if p_ref.ref_name.starts_with("refs/cl/") { push_update(MEGA_BRANCH_NAME); }`——这行的存在本身就证明「命中 CL ref」是预期路径之一。

对**根路径 CL**（`cl.path == "/"`，即默认 clone URL 场景）：`build_result_by_chain("/", [], T1)` 在 update_chain 为空时立即 break，只产出一条 `{path:"/"}`；而根路径 push 的 CL ref 也用 `&self.path` 建（`monorepo.rs:907` 起 `mega_refs::Model::new(&self.path, ref_name, …)`），因此 `refs/cl/<link>` 确实落在 `/`。两条 ref 同 path，`find` 必命中 CL ref——于是 `parent = CL ref 的 ref_commit_hash = CL tip`。结果是 `git log main` = `M1`（bot，与 C1 同 tree 因而**空 diff**）→ `C1`（用户 commit，署名完整）→ `M0`。

反证方向也核了：**子路径 CL** 的 `/` 那条 ref_update 只能匹配到 `refs/heads/main`（CL ref 的 path 是 `/services/foo`），所以根 main 上 ADR-MC-01 成立。

**两条补强证据（本版新增）**：

1. `merge_cl_unchecked` 的 `remove_none_cl_refs` **只在 `normalized_path != "/"` 时调用**（`:2538-2545`）——**根路径 CL 的 `refs/cl/<link>` 在 merge 之后不会被清理**。因此 `/` 上两条 ref 长期共存，「`refs.iter().find` 命中 CL ref」的条件在**后续每一次**根路径操作上都持续成立，而非只在首次。这提示 ME-A1 的实测必须包含「同一根路径连续两个 CL」的场景。
2. `process_ref_updates` 内 `ObjectHash::from_str(&p_ref.ref_commit_hash).unwrap()`（`:620`）是**生产路径 panic**，与 ME-A4 第 4 项同属 SB-01 域（消除生产路径残余 panic），此前未登记。

若断裂 C 成立，**ADR-MC-01 的描述在根路径场景下是错的**，且 MC-06 的 AC「CL merge 后 `refs/heads/main` **恰好新增一个单父新 commit**（ADR-MC-01）」会当场失败（实际新增两个，其中一个是空的）。

**已删除的旁证**：原版引 `tests/integration_git_cli.rs:1531-1534` 的注释作旁证，经复核不成立。该注释给出的理由是另一回事——「陈旧 clone 会把 parent commit 一并推上去，触发单 commit 规则」，对 parent 归属**中立**。推理链本身不依赖它，故只删旁证不改结论。

**顺带登记计划的一处内部矛盾**：ADR-MC-01 的 Consequences 写「**MC-03** 的端到端验收必须断言此不变式」，而该 AC 实际落在 **MC-06**（「CL merge 后 `refs/heads/main` 恰好新增一个单父新 commit」）。开工前应统一。

**这一条我标为「待复核（高优先）」**：纯代码推理，未实测。复核方法见 §7。

### 2.4 第二个约束：CL 的并发模型是「每人每路径一个」，而 agent 时代的并发是「每任务一个」

这是把「CL 即 thread」从漂亮类比拉回工程现实的一条。

**CL 身份 = `(path, username, status=Open)`**（`get_open_cl_by_path`，`src/jupiter/storage/cl_storage.rs:43` 起三条 filter），`update_or_create_cl`（`src/ceres/code_edit/model.rs:334`）命中即走 `update_existing_cl(cl.clone(), storage, &cl.from_hash, to_hash, username)`（`:347-348`）并**保留原 `from_hash`、只更新 `to_hash`**。一个用户在一个目录下**同时只能有一个 open CL**。推第二个特性分支不会新建 CL，而是把现有 CL 的 `to_hash` 覆盖掉。

对人类这个折中还能忍。**对 agent 完全不能忍**：一个 agent 并行跑三个任务，在同一路径下会互相覆盖 `to_hash`；如果多个 agent 共用一个 bot 身份（`bots.name` 唯一），冲突面更大。

这直接对上了 `DEFER-MC-01`（Gerrit 式「一 push N CL」/ stacked CL），其重启条件逐字写的是「产品提出 stacked diff 需求」。**agent 并行就是那个需求。** 这是一个明确的 revisit 钩子，应该被显式触发而不是等着。

### 2.5 锚定评审：一次必要的降调——三条成立、一条方向相反

**本节是对原版的整段重写。** 原版把一句未见于基线的话（*"comments attach to snapshots and fall out of date"*）拟制为「Zed 反 PR 的具体技术论点」，再宣布这句话对 mega2 不成立，从而得出全文头条结论——**靶子是自设的**。delta.md 记录的 Nathan Sobo 论点只有 `:47` 那两句，均不涉及评论过期。该引语已删除。

同时，mega2 的锚定机制被误述了。**准确表述是「带校验的行号锚 + diff 偏移，无内容检索兜底」，不是「内容锚」。**

**三件套（表结构逐字核对）：**

- `mega_code_review_anchor`（`src/callisto/mega_code_review_anchor.rs:10-25`）——`file_path` / `diff_side` / `anchor_commit_sha` / **`original_line_number: i32`**（`:18`）/ `normalized_content` / `normalized_hash` / `context_before(+_hash)` / `context_after(+_hash)`；
- `mega_code_review_position`（`:10-23`）——重锚后的当前位置，带 `confidence: i32` 和五态 `position_status`：`Exact | Shifted | PendingReanchor | Ambiguous | NotFound`；
- `mega_code_review_thread`——`link` + `thread_status: Open | Resolved`。

**分层重锚算法（`src/jupiter/service/code_review_service.rs:309-362`）：**

- **Tier 1**（`try_tier1_no_change`，`:383` 起）：**第一步就是 `let idx = anchor.original_line_number as isize - 1;`（`:387`）按绝对行号取下标**，越界即 `None`；随后才校验归一化内容哈希、再校验前后上下文哈希（显式防「文件里有相同行」的误锚）。函数文档自称 "absolute line number **No Change**" validation。即：**行号必须一模一样**，两个哈希只是防误锚的二次校验 → `Exact`, confidence 100；
- **Tier 2**（`try_tier2_diff_hunk_shift`，`:436` 起）：解析 unified diff hunk，按 hunk 内的增删逐行推算 offset；锚定行被删则不硬凑 → `Shifted`，confidence = 相似度 × 100。**同样不做内容检索**；
- **Tier 3**（`:352` `// Tier 3(todo): Full document coverage`）—— **未实现**。这才是唯一会「在文件里按内容找回来」的那一档。
- 落空即 `NotFound`，confidence 0（`:353-362`）。

**触发点**：`reanchor_code_review_threads`（定义 `monorepo.rs:1094`）全仓**唯一调用点**是 `monorepo.rs:957`，位于 `run_mono_post_push_pipeline`（`:920-957`）的**末尾**——中间还隔着 `traverses_tree_and_update_filepath()`（`:945`）与条件执行的 `trigger_build_and_check(...)`（`:948` 起）。

**范围**：不是「每次 push 重定位所有评论」。`:1100-1113` 先取 `changed_files ∩ files_with_threads` 得到 `affected_files`，只重锚受影响文件上的线程。

**对照表（Delta 列逐条回到 delta.md 原文）：**

| 轴 | Delta（delta.md 原文） | mega2（源码） | 方向 |
|---|---|---|---|
| **评论锚定机制** | 「评论跟随其所指文本」，**机制未公开**（`:70`）；同一底座上的**引用**是「锚定到 delta 而非行号」的字符级永久链接，"survive any code transformation"（`:54`） | 绝对行号取下标 + 内容哈希 + 双侧上下文哈希校验（Tier 1）；unified diff hunk 推 offset（Tier 2）；无内容检索兜底（Tier 3 = TODO） | **原理上 Delta 更强**（见下方说明） |
| **失败降级** | 无公开描述（`:70`） | 五态 + confidence | mega2 有，但这是**失配的证据**，不是优势 |
| **resolve 生命周期** | **无 resolve/reopen**（`:70` 明记） | `Open/Resolved` + `/code_review/{thread_id}/resolve` 与 `/code_review/{thread_id}/reopen`（`src/api/router/code_review_router.rs:25-26`，nest 前缀 `"/code_review"` `:19`） | **mega2 胜，硬差异** |
| **触发** | **未公开**（`:61`：文档未使用 "CRDT" 一词描述具体算法，该词仅出现于 Sequoia 公告；锚定机制、冲突收敛算法、存储增长均未公开） | 每次 receive-pack 的 post-push 管线末尾自动触发，范围 = 变更文件 ∩ 有线程文件 | mega2 可核查 |

> **关于「原理上 Delta 更强」的诚实限定**：delta.md 明确记录的身份锚（`:54`）是针对**引用/永久链接**说的；对**评论**，`:70` 逐字写的是「机制未公开」。因此「Delta 的评论也用 delta 身份锚」是从同一底座推出的**推断**，不是被记录的事实。但即便按最保守读法，也只能得出「未知」，得不出「mega2 更强」。身份锚（锚到不可变的 delta）在原理上不会过期，因此**不需要**降级态；行号/偏移锚必然需要——五态与 confidence 正是失配的证据。把「对方不需要的东西」记成「对方缺的东西」是错的。

**结论收窄为三条（各自标明轴）：**

1. **生命周期**：mega2 有 `Open/Resolved` 两态 + **resolve 与 reopen 两个端点**，Delta 明确没有（`delta.md:70`）——**成立，且是硬差异**；
2. **可审计性**：mega2 的重锚算法在源码里、有置信度与显式失配态，Delta 的评论锚定机制未公开（`delta.md:70`）——**开放性优势，不等于效果优势**；
3. **锚定原理**：Delta 的 delta 身份锚在原理上强于启发式重匹配；mega2 的等价物是「补 Tier 3 + 把锚绑到稳定 ID」——**这是 ME-A4 的目标，不是既有胜势**。

需要注意的是，delta.md 给 Libra 的建议 B2（锚定评论，`delta.md:30`）在 Libra 是「从零新建」，**在 mega2 是「已建成、待补完」**——这仍然是两个仓库最大的能力差，只是「已建成」的含义要按上面三条读。

要坐实第 1、2 条并向第 3 条推进，有四个洞必须先补（见 ME-A4）。

---

## 3. A 组：与既有规划契合，可优先推进

本组全部落在已排期或已交付的既有工作上，**不是新方向**。**例外**：ME-A2 的部分可选出口与 ME-A3(a) 需要 ADR revisit + 迁移，见其条目内的冲突面标注与序位说明。

### ME-A1 — 实测批次：根路径 CL 的 trunk parent 归属 + no-op rebase 的静默回退

**结论**：MC-06 / MC-04 开工前必须跑一个**两场景实测批次**；两条都成立则 ADR-MC-01、MC-06 的 AC 与 MC-04 的 AC 均需修订。

---

#### 场景 1（原有）：根路径 CL merge 后 main 的 parent 归属（§2.3 断裂 C）

**依据**：`process_ref_updates` 取首个 path 匹配 ref（`mono_api_service.rs:618`）+ `get_refs_for_paths_and_cls` 的 `order_by_asc(RefName)`（`mono_storage.rs:95-102`）+ `"refs/cl/" < "refs/heads/"` + 根路径 CL ref 的 path 就是 `/`（`monorepo.rs:907` 起）。`:638-640` 的 `if starts_with("refs/cl/") then push main` 分支证明命中 CL ref 是预期路径；`remove_none_cl_refs` 的根路径豁免（`:2538-2545`）证明两条 ref 长期共存。

**复现步骤**：

```
1. 对根路径（默认 clone URL）push 一个 commit C1 建 CL-1
2. 走正常 merge
3. git log main --format='%H %P %an <%ae> %s'
   → 若 main 新 head 的 parent 是 C1（用户 commit）而非旧 main head M0，断裂 C 成立
4. 重复一次（同一根路径的 CL-2），确认 refs/cl/<CL-1> 未被清理、命中条件持续成立
```

**成本**：一次真实 root push + merge + 一条 `git log`。极低。

---

#### 场景 2（**本版新增，高优先**）：no-op rebase 后的静默回退 —— **强推理，待实测复现**

**依据**：§2.3 断裂 B 路径 ②。`:3255-3257` 的 no-op 分支把 `from_hash` 前移到 `target_head` 而 `to_hash` 不动；`merge_cl` 守卫（`:2071-2073`）随即放行；`merge_cl_unchecked` 用 `cl.to_hash` 的树（`:2517-2521` → `:2535`）推进 main。

**复现步骤**：

```
0. 起本地栈（compose：postgres + mega2），准备两个用户 A / B
1. A 对路径 P push 一个改动 F 的 commit C1 → 建 CL-A（from=M0, to=C1）
2. A 再 push 一个把 F 改回原状的 commit C2 → CL-A 更新为 (from=M0, to=C2)
   ——此时 cl_files_list(blobs(M0), blobs(C2)) 为空，CL-A 是「空 diff CL」
   （替代做法：A 直接 push 一个 `git commit --allow-empty` 的 commit）
3. B 在同一路径 P 上正常提交并 merge 一个改动 G 的 CL-B
   → main 前进到 target_head，G 已在 trunk
4. A 对 CL-A 调用 Update Branch（REST）
   → 命中 :3255 no-op 分支：from_hash := target_head，to_hash 仍 = C2
   → 断言 A：mega_cl 行的 from_hash == main head，且 target_head 不在 C2 的祖先链上
5. A merge CL-A
   → 断言 B：merge_cl 的 from_hash 守卫放行（不报 "ref hash conflict"）
   → 断言 C（核心）：merge 后 `git show main:<G 所在文件>` —— B 的改动 G 是否还在？
     若消失 ⇒ 静默回退成立
   → 断言 D：`git log main --format='%H %P'` —— B 的 merge commit 是否仍在 main 可达集？
6. 附带断言 E（对 MC-02/MC-03/MC-04 的直接影响）：
   在步骤 4 之后、步骤 5 之前，对 CL-A 触发一次 push，
   观察「从 to_hash 反走 parent 链到 from_hash」的任一消费者（当前只有 gpg checker 的
   单点验证；MC-02/03/04 落地后为链式）是否终止。
```

**影响**：**高于断裂 B/C 任何一条。** 若成立，这是一条**数据丢失**级缺陷（trunk 上他人已合入的改动被静默回退），且**触发条件在真实并发下并不罕见**——「一个最终没改动任何文件的 CL」在 agent 场景里恰恰常见（agent 尝试后自行回滚、格式化后又还原、只改了被 `.gitignore` 忽略的文件）。

**成本**：一次本地栈 + 两个用户 + 六步。低。**这一步应当排在 ME-A1 场景 1 之前跑。**

---

**落点**：`docs/plan/plan-20260827.md` 的 ADR-MC-01、MC-06 的 AC「CL merge 后 `refs/heads/main` 恰好新增一个单父新 commit」、MC-04 的 AC 全组；`src/ceres/api_service/mono_api_service.rs:609-645`、`:3215-3257`、`:2504-2545`。顺带修正 ADR-MC-01 Consequences（写 MC-03）与 AC 实际落卡（MC-06）的归属矛盾。

**风险**：不复核就开工，MC-06 的端到端断言会在最常见的默认场景（根路径 clone）当场失败，且失败原因会被误读为新代码引入的回归；而场景 2 若成立却未被发现，MC-04 会把一个数据丢失路径当成「数据落库」交付出去。

### ME-A2 — 把 `update_branch` 纳入 MC-04 的写集，并**先决定 no-op 分支的语义**

**结论**：MC-04 落地前必须先决定 Update Branch **两条路径**下 `mega_cl_commits` 的语义，否则 MC-04 的幂等补齐在主路径上把真实 commit 清单重建成一条 bot 记录，在 no-op 路径上**根本无法终止**。

**依据**：§2.3 断裂 B（含两条路径的完整代码链）。`plan-20260827.md` 全文无 `update_branch` 命中（已实测）。

**机制订正**：抹除/失配不在 `update_branch` 当场发生，而在**下一次 push** 命中「CL 存在而清单缺失或与当前 `to_hash` 不符时，管线幂等补齐清单」这条 AC 时发生；中间存在一段「清单与 CL 哈希不一致」的窗口。

---

#### 决策 0（**必须先做，本版新增**）：no-op 分支要不要修

no-op 分支（`:3255-3257`）制造的是一个 `from_hash` 与 `to_hash` **不在同一条 parent 链上**的 CL 状态。这个状态破坏的是 ADR-MC-02 的隐含不变式（「CL = `(from_hash → to_hash)` 聚合 diff」默认 from 是 to 的祖先），而**计划里三张卡（MC-02/03/04）的核心算法都建立在这个不变式上**。

| 出口 | 内容 | 冲突面 |
|---|---|---|
| **0-A**（推荐先评估） | 修 no-op 分支，恢复不变式：no-op rebase 也在 `target_head` 之上生成一条空树变更的 commit（或直接复用主路径），使 `to_hash` 的 parent 始终经过 `from_hash` | 改 `mono_api_service.rs:3255-3268`（**不在任何一张卡的写集里**）；行为可见变化 = no-op Update Branch 会产生一条 commit；须新开 ADR 或并入 ADR-MC-02 的 Consequences |
| **0-B** | 保留 no-op 分支，但把 MC-02 已有的「`from_hash` 不在链上时 fail-closed」谓词**同步到 MC-04 与 MC-03**，并明确「断链 CL 的清单 = 空集 + 一条断链标记」 | 需修订 MC-04 的 AC（加一条谓词）；不需迁移；但意味着 no-op 之后 CL 的 gpg 门永久 fail-closed，产品面须接受 |
| **0-C** | 不处理 | MC-04 交付即含一条不终止/撞 250 上限的路径；§2.3 断裂 B 推出的静默回退（待 ME-A1 场景 2 实测）也随之保留 |

**决策 0 与下面的决策 1 正交，且必须先做**：不定义 no-op 语义，决策 1 的三个出口没有一个能定义完整。

---

#### 决策 1：主路径（压平）下 `mega_cl_commits` 的语义

**三个可选出口，逐项标注冲突面（供计划决策，本文不代拍）**：

| 出口 | 内容 | 与已决 AC / 完成判据的冲突 | 落地代价 | no-op 分支下的语义（决策 0 的下游） |
|---|---|---|---|---|
| **(a) append-only** | rebase 时**保留**旧清单并追加一条 rebase 事件，CL commit 清单变成 append-only 历史（最贴近「保留决策链」） | **直接推翻** MC-04 AC「事务内先按 link **删除**再批量插入（重复/更新 push 幂等，无重复行）」——append-only 与「先删后插」互斥；且需新表或新列（区分「历史段」与「当前段」），与「兼容与文档收口」章的整目录断言「`src/callisto/`、`src/jupiter/storage/` 与 `src/jupiter/migration/`：**N/A（复用既有实体与迁移，无新增）**」冲突，也与 MC-04 卡内「Migration and rollback: N/A：复用既有表与迁移，无新 schema」冲突 | **ADR revisit + schema 迁移 + AC 修订 + 重新过 Codex 评审** | 若决策 0 选 0-A：追加一条 no-op 事件；若选 0-B：追加一条断链标记段 |
| **(b) 两段式快照** | rebase 时**冻结**旧清单为快照，新清单另存 | 同 (a)：与「先删后插」的幂等语义冲突（「另存」落在该 AC 之外），且必然新增表/列，同样撞整目录 N/A 断言 | **同 (a)** | 同上，快照边界多一档 |
| **(c) 显式接受抹除** | 接受抹除并在 UI 上标注「本 CL 曾被 rebase，中间 commit 已不可见」 | **与现有 AC 相容**（先删后插即抹除），无迁移；**代价是放弃 ADR-MC-01「仅保留在 `refs/cl/<link>` 供审计与 UI 展示」的审计承诺**——因为 rebase 后 `refs/cl/<link>` 指向的也已是压平后的 bot commit，旧 commit 成孤儿（`clean_dangling_commits` 是 TODO，`:2544`） | 低：一条 UI 标注 + 一条注释 | 若决策 0 选 0-A：与主路径一致；若选 0-B：清单为空 + 断链标记 |

> **「不涉及新工程」的表述必须收窄**（本版修订）：**提出问题**（把 `update_branch` 纳入 MC-04 的写集与决策面）不涉及新工程，可以立即做；**决策 0 的 0-A/0-B 与决策 1 的 (a)/(b) 的落地都是新工程**，各自需要 ADR revisit / AC 修订 / 迁移 / 重新评审。ME-A2 进第一梯队的部分只有「摆到台面并做决策」，不含任何一条出口的实现。

**落点**：`src/ceres/api_service/mono_api_service.rs:3190-3300`（尤其 `:3255-3268` 与 `:3279-3282`）、`:2080-2301`；`docs/plan/plan-20260827.md` 的 MC-04 Write set、MC-04 AC 组、ADR-MC-05、ADR-MC-02 Consequences。

**这个决策直接决定 ME-A3(a) 挑战的「强度」（不决定其「是否触发」——触发是无条件的）**，见 ME-A3。

**风险**：不处理则 MC-04 交付即残缺；主路径的缺陷只在「有并发合入」的真实负载下才暴露，L1 测试抓不到；no-op 路径的缺陷更隐蔽——它需要「空 diff CL + 并发合入 + Update Branch」三者同时出现。

### ME-A3 — 两件独立的事：(a) 顺序列 / 链序可判定性（对已决 AC 的显式挑战，**无条件触发**）+ (b) CL ↔ trunk commit 的显式边（无冲突）

**本条把两件性质完全不同的事拆开。** 原版把它们捆成「顺带补两个字段，成本近零」，其中前一件实质上是在推翻一条经十余轮 Codex 评审固化的已决 AC。

---

#### (a) `mega_cl_commits` 的链序可判定性 —— 对 MC-04 已决 AC 的显式挑战，**触发条件无条件成立**

**已决状态（必须先承认）**：MC-04 Out of scope「表加顺序列：**永久非目标**，理由 = 链序在读取时由 parent 拓扑重建（见 AC），无需 schema 变更」；AC「`ClStorage::get_cl_commits(link)` 按 parent 拓扑序返回 …（**表保持集合语义、不存顺序列**，排序依据写入代码注释）」。这是被评审固化的决策，不是遗漏。

**挑战的靶点是那条 rationale，不是那条结论。** 「链序在读取时由 parent 拓扑重建」这句话依赖一个未写出的不变式：**`from_hash` 必在 `to_hash` 的祖先链上**。而 `update_branch` 的 no-op 分支（`:3255-3257`）**无条件打破该不变式**（§2.3 断裂 B 路径 ②）——`from_hash` 前移到 `target_head`，`to_hash` 不动，`target_head` 不是 `to_hash` 的祖先。此时「从 to 反走 parent 链到 from」不终止，**拓扑重建根本不成立，遑论定义链序**。

**因此 ADR revisit 的触发是无条件的，与 ME-A2 的任何选项都无关。**

**但诚实地说：revisit 的结论有两个出口，「加顺序列」只是其中之一。**

| 出口 | 内容 | 前置 |
|---|---|---|
| **R-1（先评估）** | **恢复不变式**：按 ME-A2 决策 0-A 修 no-op 分支，使 from 始终是 to 的祖先。此时 AC「表保持集合语义、不存顺序列」的 rationale 重新成立，**本小节可以只改 rationale 的措辞（补上不变式的显式声明 + 一条守卫用例），不改 schema** | ME-A2 决策 0 选 0-A |
| **R-2** | **接受不变式可破**：则集合语义无法定义链序，须存顺序（顺序列或等价物），并补 MC-02 已有的「`from_hash` 不在链上 fail-closed」谓词到 MC-04 | ME-A2 决策 0 选 0-B/0-C |

**条件加强（ME-A2 决策 1 的下游）**：若决策 1 选 **(a) append-only** 或 **(b) 两段式快照**，表里会同时存在**多条互不相连的链**（rebase 前的旧链与 rebase 后的新链之间没有 parent 边，且 `clean_dangling_commits` 是 TODO，旧 commit 仍在 `mega_commit` 里）。此时即便不变式在每条链内部成立，「按 parent 拓扑序返回」在集合语义下**仍无法定义跨链次序**——这是 R-2 的第二个独立理由。若决策 1 选 **(c)**，表里永远只有一条链，本条加强不适用。

**流程要求**：走 ADR revisit（挑战 MC-04 Out of scope 与 AC 两处原文），需要 AC 修订 + 重新评审；R-2 另需迁移。

**成本**：**不是「近零」**——最好情况（R-1）是「AC rationale 修订 + 一条守卫用例 + 重新过评审」；最坏情况（R-2 + 决策 1 选 (a)/(b)）是「schema 迁移 + AC 组重写 + 重新过评审」。

**序位**：**排在 MC-04 收口之后**，且以 ME-A2 的决策 0 / 决策 1 结论为前置。不得与 MC-04「同批」。

---

#### (b) CL ↔ trunk commit 的显式边 —— 与任何已决条目无冲突

**结论**：`mega_cl` 没有 `merge_commit_id` 列，`mega_commit` 没有 `cl_link` 列，`merge_commit_id` 在 `plan-20260827.md` 与 `src/` 全仓**零命中**（已实测）。这是一个未被任何决策覆盖的真实缺口。

**依据**：

- trunk commit 无 trailer、无 notes、无 commit binding——`apply_update_result` 的 `save_mega_commits(commits, None)`（`mono_api_service.rs:2623`、`:2736`）不写 `commit_auths`（`commit_auths` 的唯一写路径在 `commit_binding_storage.rs`）；
- 唯一可用的 join 是 `mega_cl.from_hash == trunk_commit.parent ∧ status = Merged`——**一个可推断关系，不是被记录的关系**；
- 而 §2.3 断裂 C 恰恰说明这个可推断关系在根路径场景下就已经不可靠（parent 可能是 CL tip 而非旧 main head），断裂 B 的两条路径都会改写 `from_hash`，任何前滚修复、queue 语义变化或 from_hash 语义调整都会静默打断它。

**落点**：`src/jupiter/migration/`（新迁移，禁手写 schema SQL）；`src/callisto/mega_cl.rs`；`src/ceres/api_service/mono_api_service.rs:2504-2571`（merge 后回写 `merge_commit_id`）。
**风险**：低。加列不改现有读路径。ADR-MC-06 的钩子——「任何未来读取 `mega_tree`/`mega_blob.commit_id` 的功能必须先重审本 ADR」——本条不读那个字段，不触发。**但**：新增迁移与「兼容与文档收口」章的整目录 N/A 断言冲突，见 §6.3。
**序位**：与 (a) 相同，排在 MC-04 收口之后（写集与冲突面说明见 §6.3）。

---

**为什么这张卡重要**：Delta 全部叙事里最有说服力的一句是 *"From any line of code, find the conversation that produced it"*（`delta.md:54`）。forge 侧的版本是「从任一 CL 的任一 hunk 找到产出它的会话」，而 forge 有 Delta 没有的东西：**这个归因是服务端权威的、不可被客户端伪造的**。`mega_cl_commits`（CL 由哪些 commit 组成）× `commit_auths`（commit 由谁产出）× `merge_commit_id`（CL 落到 trunk 的哪一点）相乘就是原料。不需要 CRDT。

### ME-A4 — 补完锚定评审的四个洞

**结论**：mega2 的锚定评审在「生命周期」与「可审计性」两条轴上真实领先（§2.5），但四个洞会让它在 agent 场景下失效；其中第 3 项同时是把锚定原理向 Delta 水平推进的唯一路径。

**依据**：§2.5。

**落点与具体项（按严重程度排序）**：

1. **【最严重】修重锚 diff 的分页截断**（`src/ceres/pack/monorepo.rs:1179-1181`）。重锚喂给 Tier 2 的 diff 是 `mono_api_service.paged_content_diff(&cl_link, Pagination::default())`（`:1180`），而 `Pagination::default()` = `{page: 1, per_page: 20}`（`src/contract/api/common.rs:43-50`）。**即重锚只看得到 CL 的前 20 条 diff item**；第 21 个及以后被改动的文件上的线程拿不到任何 hunk → Tier 2 必失败 → 因 Tier 3 未实现，直接落 `NotFound`（`code_review_service.rs:353-362`）。
   **这是静默正确性缺陷，不是性能问题**：在 monorepo 里一个 CL 改动 >20 文件是常态，而「一次改一大片」恰恰是 agent 的典型行为。
2. **修 N×全量 diff**（同一调用点，`monorepo.rs:1180`）。`paged_content_diff` 在**每个线程**的 async 闭包体内各调一次（`:1157-1180`；同一文件上的多个线程各调一次，`blob_cache` 也是 per-thread 的），且这些 future 经 `stream::iter(...).buffer_unordered(get_recommended_batch_concurrency())`（`:1223`）**并发**执行。每次全量 diff 是两次 `get_commit_blobs` 全树遍历 + 相似度 rename 检测。并发放大的是**峰值内存**，不只是延迟。
   **第 1、2 项是同一处调用点，必须合并修**——一次改动可同时解决截断与重复计算（把 diff 提到循环外、一次性取全量而非首页）。
3. **补 Tier 3**（`src/jupiter/service/code_review_service.rs:352`）。大重构（函数整体移动、文件重命名）下评论落 `NotFound`——**而大重构恰恰是 agent 最常做的事**。这一项也是 §2.5 第 3 条轴（锚定原理）的实质内容：配合「把锚绑到稳定 ID」，才是向 Delta 的身份锚靠拢的路径。
   > **⚠️ 治理前置（本版新增）**：第 1/2/4 项是**缺陷修复**，`plan-long.md:83` 原则 4「先追平、后扩展」不拦；**第 3 项是在已移植模块（`src/jupiter/`）上叠加新能力**，必须先过原则 4 的确认——「已移植模块的 Mega 基线漂移（PT-02）优先于在其上叠加新能力；在漂移窗口上实施新 PT 前，必须先确认相关模块的 Mega 变更已被吸收或明确排除」。而 PT-02 当前正处于漂移窗口：`plan-long.md:69` 明确把 ceres 的 `application/transport/bus` 结构重构（Mega #2138/#2139/#2142）登记为 **DEFER-SYNC-02/03/05**（未吸收）。第 3 项立项文档必须显式给出「相关模块漂移已吸收或明确排除」的书面确认。
4. **修 `.expect("latest blob must exist")`**（`src/ceres/pack/monorepo.rs:1195`）。文件被删时 panic，在 receive-pack 管线里（`affected_files` = changed ∩ has-threads，被删文件同时满足两者）。这既是可用性缺陷也违反 SB-01（消除生产路径残余 panic）。
   **同域顺带项（本版新增）**：`process_ref_updates` 的 `ObjectHash::from_str(&p_ref.ref_commit_hash).unwrap()`（`mono_api_service.rs:620`）是同性质的生产路径 panic，应一并登记进 SB-01 域（但其落点与 MC-01/03/04/06 写集相交，须按 GC-01 排）。
5. **明确 trunk 推进后的重锚触发**（**已可结案**）。`reanchor_code_review_threads` 全仓唯一调用点是 `monorepo.rs:957`（CL push 路径），**CL 合并推进 trunk 后其它 CL 的评论不会随之重锚**——已实测确认，不再是待复核项。需评估补触发的频率成本（trunk 每次推进 × 全部 open CL 的受影响线程数）。

**关联**：SB-01（常驻门禁）；无 PT 归属，属既有能力补完（第 3 项除外，见其治理前置）。
**风险**：第 1、2 项落在 `monorepo.rs`，与 MC-01/03/04/06 的写集相交，须按 GC-01 核对收口状态后排期（见 §6.3）。第 3 项是新算法 + 原则 4 前置，需独立设计与测试。第 4 项纯修复，极低。

### ME-A5 — 在 PT-05/PT-06 的决策期登记 agent 执行为「第二个已知消费者」

**结论**：这不是「现在就建 agent 执行面」，而是一个**零成本的设计约束**——让 Orion 的任务模型不要长成 buck2 专用形状。

**依据**：

- PT-06（Orion Server 构建控制面）/ PT-07（构建执行 Agent）/ PT-08（Scheduler + QEMU VM 池）是 plan-long 里最大的整体缺口（`plan-long.md:117-119`），且**正好是一套完整的远程执行平面**：任务模型、调度、日志收集、产物管理、ws 控制通道、disk/repo 缓存管理——除 `buck_controller` 外全部通用。
- PT-08 的 QEMU VM 池是**天然的 agent 沙箱**，而沙箱正是 §5 里 mega2 唯一没有对位资产的一项。
- PT-06/07 目前卡在 PT-05（`ceres/bus` 通信形态决策；`plan-long.md:635` 明写「orion 三件套……实施前须先完成 PT-05 的通信形态决策」），而 PT-05 允许的结论之一是「明确不移植」。**这个决策现在有了第二个利益相关方。**

**落点**：`docs/plan/plan-long.md` PT-05 的「最小可验证第一阶段」与 PT-06 的任务模型定义段落——加一行「已知消费者：构建触发（现）、agent 执行（潜在，见 ME-B\*）」。
**必须规避的红线**：PT-05 的非目标写着「**不引入 Mega 未使用的通用事件框架**，只移植有真实消费者的事件面」（`plan-long.md:353`）。本条**不是**提议建通用事件总线，而是在既有决策里登记一个额外消费者，让接口形状留出余量。措辞必须精确，否则会被这条挡回。
**风险**：近零（设计期一句话）。**不做的风险**：mega2 把 Orion 建成构建专用，将来为 agent 再造一套平行的执行平面——**在同一个仓里犯两次同样的错**。

**已删除的分句**：原版写「Delta 因为没有服务端底子，云 runner 必须从零建」——与基线冲突。`delta.md:58` 明写 agent 单点执行可在「发消息者的机器**或 Delta 云机**」，`delta.md:81` 的 "Remote runtime 云常驻执行（进行中）" 是把它做成**常驻**而非从零建，`delta.md:75` 还记录了完整的 Cloudflare 服务端栈。本条论证不依赖对 Delta 的贬低，删掉更稳。

---

## 4. B 组：新立候选（**Mega 之外的新方向**）

### 4.0 为什么必须标注为「Mega 之外的新方向」

`plan-long.md` 是**排他性的 Mega 移植文档**：标题即「Mega → mega2 完全移植」；§路线图维护 明文规定「新候选移植项必须同时给出 **Mega revision/path**、mega2 代码/测试缺口、价值、风险、依赖和最小可验证切入点」（`:787`）。一个 Mega 里不存在的能力在结构上无法满足 PT 候选的准入条件。硬塞 PT-13 会破坏该文档自己的定义。

但 mega2 的自我定位是「移植**并扩展**」，且**已有成规模的非移植先例**：`plan-20260731`（chat/Notes 整栈删除 + 接入 monoui Better Auth）、`plan-20260820`（Vault 改 crates.io `libvault`，明确「不做 mega 的 `libvault-core` 迁移」）、`plan-20260824`（orbit 单体内联），以及**在途的 `plan-20260827` 自陈**：§与其它计划的关系 的 `plan-long.md` 行逐字写着「无直接对应 PT 项；本计划是 mega2 自有的协议能力增强 | **不触碰**」，且其「兼容与文档收口」章写着「`plan-long.md`：N/A（无对应 PT/SB 项）」。

**推荐落地形态**：走 `plan-20260827` 的先例——独立日期计划，在「与其它计划的关系」表里显式声明不申请 PT 编号、不触碰 plan-long。若确需长期登记，另开 `docs/plan/plan-long-ai.md` 用自有前缀（如 `AG-*`），并在 plan-long §不进入本长期移植计划 增一行指针。**不要修订 plan-long 的文档职责**——那份治理文档刚被多轮评审固化，改它会波及原则 3/4 与整张依赖图。

**另需规避原则 4「先追平、后扩展」**（`plan-long.md:83`）：新能力应强调**新增落点（新 module + 新表）**，避免声称要在已移植模块（ceres/jupiter/api）上叠加。

### ME-B1 — Bot 成为一等 Cedar principal（唯一的真瓶颈，最高优先）

**结论**：这是所有 agent 面的前置。全仓没有任何一处把非人主体建模为一等授权主体。

**本条删去了全部 Delta 侧论据。** 原版把 Delta roadmap 的「仓库权限接入」读成「Delta 卡在 bot 主体建模上」——`delta.md:81` 只说该项「进行中」，**没有任何关于它为何未完成、卡在哪里的信息**，该读法是纯猜测；且它与 ME-B4 对同一条 roadmap 项的定性（Delta 想**消费** forge 已有的仓库权限）互斥。Delta 是**客户端**，`delta.md:71` 明示共享线程里「人人可 steer」「作者归属直达模型」——它的主体模型就是人，没有把 agent 当独立 principal 的需求。「Delta 想消费 forge 权限」的论据归 **ME-B4 独占**；「先做完的一方定义赛道」的叙事一并删除。

以下依据全部是 mega2 自证，已逐条实测：

- `src/contract/policy/mega.cedarschema`（全文已读，`grep -c "Bot"` = **0**）只有 `entity UserGroup in [UserGroup]` 与 `entity User in [UserGroup]`，**没有 `entity Bot`**；四条 action 声明（`deleteRepo/viewRepo/forkRepo/pullRepo/pushRepo`、`createMergeRequest/editMergeRequest/deleteMergeRequest/approveMergeRequest`、`openIssue/assignIssue/deleteIssue/editIssue`、`addMaintainer/addAdmin`）的 `principal` **全部是 `[User]`**。`Bot::"<id>"` 不是那个类型。
- 官方说明（`src/api/un27_bot_authz.rs:1-9`）："A full bot authorization model (schema plus identity mapping) is deferred."
- 行为矩阵：`off` → 200 短路；`shadow` → 200 + would-deny；`enforce` → **403**。
- `un27_a_bot_does_not_inherit_a_same_named_users_permissions` 明确断言 principal 的类型是身份的一部分。
- **官方迁移建议是反向的**：`docs/manuel/authz.md:96` 逐字写着「Bot 对受保护 CL 面的操作须在 enforce 前迁移到**用户 token**；完整 Bot 授权模型归后续计划」——即架构性地要求 agent 冒充人。

**这与「决策链归因」直接冲突：你无法归因一个冒充人的主体。** 全仓唯一**被持久化审计**的人/机类型是 `ActorTypeEnum { Human, Bot }`（`sea_orm_active_enums.rs:8-13`），而它属于一张**从未写入过**的表（`log_audit` 在 `src/jupiter/storage/audit_storage.rs:33` 定义，全 `src/` **零调用点**，已 grep 确认）。
*（精度限定）*：请求路径上并非完全不区分——`cedar_guard.rs:343` 已能返回 `("Bot".to_string(), bot_id)`，另有 `TargetTypeEnum::Bot`。准确说法是「唯一被持久化审计的人/机类型，而它所属的表零写入」。

**已有地基（不需要重建）**：`bots` / `bot_tokens`（HMAC-SHA256 入库 + 过期 + 撤销）/ `bot_keys`（RSA-2048）/ `bot_installations`（installation 模型，照着 GitHub App 建模）；认证侧 `bot_` Bearer → `BotIdentity` 已接（`src/api/oauth/mod.rs:128-174`），且在 guard 里优先于 session（`cedar_guard.rs:343-345`）。

**内容**：

1. `mega.cedarschema` 增 `entity Bot`；action principal 扩为 `[User, Bot]`；
2. 打通 `bot_installations` → Cedar entity store 的身份映射；
3. **引入 on-behalf-of 语义**：agent 代表 user 行事时，有效权限 = agent 授予范围 ∩ user 权限（**取交集，不是并集**）；
4. 接线 `check_bot_permission`（`src/ceres/api_service/bot_ops.rs:19`，**全仓唯一定义、零调用点**）；
5. 补 bot 创建 API（`bot_router.rs` 的 `routers()` 恰 8 条路由——`install_bot` / `list_installed_bot` / `change_installation_status` / `uninstall_bot` / `create_bot_token` / `list_bot_tokens` / `revoke_bot_token` / `revoke_all_bot_tokens`——**没有 create**，bot 目前只能直接写库产生）。

**与路径级 ACL 的关系**：ADR-UN-05 把任意路径的 resource 归一到 `Repository::"/"`（`src/contract/policy/resource.rs:22`，函数签名就是 `pub fn normalize_resource(_path: &str)`，路径被完全丢弃），路径级 ACL 明确延后。对人类这个折中能忍；**对 agent 完全不能忍**——委托给 agent 的权限必须是最小化的、路径限定的。**agent 授权是路径级 ACL 的第一个真实消费者**。

现成的半成品：`mega_resource_permission`（`{resource_type, resource_id: String, group_id, permission}`）的形状**天然就是路径级 ACL**，只是 `ResourceTypeEnum` 目前只有 `Note` 一个取值而 `notes` 表已被删除，且全仓唯一引用是 `group_storage.rs:123-127` 的级联删除。另一个先例：`reviewer_parser`（`src/contract/policy/reviewer_parser.rs`）已经在生产中跑「路径前缀 → 人员列表」的仓内声明（`<dir>/.cedar/policies.cedar`）——**证明这种粒度在这个 monorepo 模型下是可行且已被接受的**，只是被用在评审而非授权上。

**落点**：`src/contract/policy/mega.cedarschema`、`src/contract/policy/resource.rs`、`src/contract/policy/guard/cedar_guard.rs`、`src/ceres/api_service/bot_ops.rs`、`src/api/router/bot_router.rs`。
**关联（依据订正）**：与 PT-03 的「repo/path 级 push ACL」同域，**应合并评估**。**依据必须显式指向 `plan-long.md:275`（PT-03 §目标范围）与 `:289`（§完成判据），而非 `:114` 的表格行**——表格行声称该能力「已由 plan-20260812 交付」，与 plan-20260812 自己的非目标（`:28`：路径级 ACL「归 DEFER-UN-01 的后续设计」）直接冲突，按表格行读会被判为「已交付项不需要合并评估」。这处漂移已补进 ME-B6。
**风险**：改 Cedar schema 是授权面的核心变更。**但落地路径现成**：`Enforcement::{Off, Shadow, Enforce}`（`src/contract/policy/enforcement.rs`）意味着可以先 shadow 观察 would-deny 再切 enforce。**这是 mega2 已有的、Delta 完全没有的工程资产**（`delta.md:74` 记录 Delta 自认无权限系统）。
**开工前必须确认的一件事**：生产实例当前跑在哪一档。`default_cedar_enforcement()` 返回 `"off"`（`src/config/model.rs:999-1001`），若生产是 `off` 或 `shadow`，那么**今天的 bot token 事实上不受 Cedar 约束**。在把 mega2 宣传为「有权限系统的 agent forge」之前必须先核实，否则叙事会被一句话戳破。

### ME-B2 — agent 会话归档面（挂 CL `link`，只读 / 幂等 / 脱敏）

**结论**：给 CL 补上 Delta 的中间层——但只补「过程」，不补「未提交编辑」。
**依据**：§1.2、§2.2。ACP（Agent Client Protocol）是 Zed 2025-08 开放的 JSON-RPC 标准，兼容清单含 Claude Code、Codex CLI、Gemini CLI、Cursor、goose（`delta.md:84`）——**agent 会话记录会以某种形式产生，问题只是它落在谁家的库里**。

**关键区分**：**forge 不应该实现 ACP。** ACP 是「编辑器 ↔ agent」协议，两端都在用户机器上，forge 不是这条链路上的任何一端。forge 的对应物是**接收 ACP 会话的产物**：

```
[Claude Code / Codex / Cursor] --ACP--> [本地客户端] --HTTPS--> [mega2 摄取端点]
                                                                     ↓
                                                            挂到 CL 的 link 上
```

**设计要点（每条都是硬约束）**：

1. **只读归档语义**——摄取的是既成事实的记录，不是可 steer 的活会话；
2. **必须过脱敏**（SB-02 纪律）。agent transcript 极易夹带密钥；Delta 在这点上明确是弱的（`delta.md:75`「仅匹配已知值，不扫描任意文件」，且 Background Mode 的 8 MiB 终端输出**先存原始字节**，`delta.md:68`）。mega2 有 `src/config/redaction.rs` / `src/notification/redact.rs` 的既有纪律可复用；
3. **幂等**（同一会话重复上报不产生重复记录）；
4. **只在 push 之后**——不接受未提交编辑；
5. 授权走 ME-B1。

**落点**：新 module（避开原则 4 的「不在已移植模块上叠加」）+ 新表 + 新 router；挂载键复用 `mega_cl.link`。`mega_conversation` 的 `ConvTypeEnum` 是开放枚举（**15** 个值，加类型是加迁移不是改架构），但**会话正文不应塞进 `mega_conversation.comment`**——那张表是给人读的时间线。
**已被低估的现成原语**：`mega_webhook.path_filter`（`src/callisto/mega_webhook.rs:15`）。在 monorepo 里，一个只负责 `//services/foo` 的 agent 应该只收到它那一片的事件——这是订阅侧的最小权限，**Delta 完全没有对应物**（它是单仓库心智）。

**必须显式处理的治理红线**：`plan-long.md:751` 写着「**本仓 Campsite 风格 chat / Notes**：源系统是 campsite Rails 而非 Mega；产品面已由 `plan-20260731.md` 整栈退场（能力留在 **website**），不占 PT 编号、不再作为本仓在维护基础能力」；`plan-20260731.md:30` 的非目标写着「不移植 website AI chat（`apps/next-app/app/api/chat`）」。**如果新方案看起来像「把 chat 加回来」，会直接撞上这条。** 立项文档必须在第一段说清：这是挂在 CL 上的**只读归档记录**，不是产品化的会话界面；会话 UI 归 monoui（原则 11 的前端影响评估同样适用）。

**风险**：中。存储增长（agent transcript 体积远大于人类评论）、脱敏漏网、与 chat 退场决策的观感冲突。
**存储路线提示**：`ObjectNamespace`（`src/orbit_api/object_storage.rs:106` 起）是**可追加的**（`test_namespace_string_values_are_stable` 明确写了「New namespaces may only be appended」，追加是合法路径），`LogStorage` 已有 append-only + CAS manifest 语义。但**不要直接拿 `LogStorage` 做多写者事件流**：`append` 是全量读改写 manifest 且 `PutMode::Overwrite` 无 CAS（`src/orbit/adapter/log.rs:5-64`），`append_concurrently` 的 32 次 CAS 重试（`:205`）在高频多写者下会活锁，且 manifest 随 segment 数线性膨胀。若要用，必须按写者/会话分片 log key。

### ME-B3 — agent 归因（provenance）

**结论**：把「哪个 agent / 哪个模型 / 哪次会话」写进 commit → 身份的链，并让它在 CL 面可查。
**依据**：`commit_auths`（`src/callisto/commit_auths.rs`：`{id, commit_sha, matched_username: Option<String>, is_anonymous, matched_at, created_at}`）——**commit → 身份绑定的表已经存在**，且 SSH publickey → commit binding 有「不得匿名降级」的既定安全约束。缺的是 agent 维度。
**落点**：`src/callisto/commit_auths.rs` + 新迁移；CL 面只读展示。与 ME-A3(b) 相乘得到 hunk 级归因。
**术语警告（重要）**：「provenance」在本仓**已被占用两次**——① `plan-20260812` UN-34 的「配置来源溯源」（`LoadedConfigSummary { source, profile_name, paths }`，已交付并写进 `docs/refactoring/contract.md:350`）；② `mega-git-fixtures-audit.md` 的「测试夹具来源/许可」。「audit」也被 `authz-audit` CLI（UN-26/29/30/32/35..60）大量占用；「session」在本仓 = **浏览器会话 / Better Auth cookie 内省**。**新立项必须用新术语或显式限定，否则会造成检索与评审的持续混淆。**
**风险**：低。加维度不改现有判定。
**依赖**：ME-A3(b)（CL ↔ trunk 显式边）；对外释放价值需 ME-B1。

### ME-B4 — 对外委托授权面（installation token + 权限查询端点）

**结论**：Delta roadmap「进行中」的**仓库权限接入**（`delta.md:81`："Repository-based access"——*"Use your repository's existing permissions to control access to shared Delta threads"*，即把共享线程的访问控制**委托给仓库既有权限**）本质是一封委托授权申请书；forge 应该提供这个接口，否则伴生层只能自己建一套。**这是 delta.md 明确记录的、Delta 对 forge 的显式依赖点**，也是全文唯一可以拿 Delta roadmap 立论的地方。

**依据**：§1.3、§4.0。Delta 的分享有一个官方自认的扩权缺陷——**分享一个线程即授予其挂载仓库的 worktree 历史访问（仓库级，非线程级）**（`delta.md:71`，官方文档明示）。

**关于「根源」的限定**：原版归因为「Delta 的权限模型只有『线程』和『仓库』两级，没有中间层」——**无据**。`delta.md:71` 记录的 Delta 访问模型实际是**三档**（仅邀请／全组织／任何有链接者）+ owner 独占改权限 + 收紧不驱逐既有参与者；delta.md 只记录了扩权**现象**，未记录其**成因**。准确表述：**delta.md 只记录了该扩权的存在（官方文档明示），未记录成因。** 本条的设计约束不依赖对成因的猜测。

**forge 侧的对位事实**：forge 有完整的资源层级（组织 / 仓库 / 路径 / CL / 线程），只是 Cedar 侧还没用起来（ADR-UN-05 归一到 `Repository::"/"`）。
**内容**：一个第三方可调用的授权查询接口，语义是「代表用户 U、在安装范围 S 内、对资源 R 的有效权限」。
**复用**：`bot_installations`（installation 模型已在）、`bot_tokens`（可撤销可过期）、Cedar 三态（可 shadow 灰度）、`audit_logs`（可审计，但需先接线——见风险）。
**关键设计约束**：**授权范围必须是路径级或 CL 级，不能是仓库级**——否则就重蹈 Delta 的覆辙，而 forge 本来有能力做对。
**落点**：`src/api/router/`（新 router）、`src/contract/policy/guard/`。
**依赖**：**ME-B1 是硬前提。** 不做 ME-B1，本条无从谈起。
**风险**：中高。这是把授权判定暴露给第三方，误设计即数据泄露。必须先有 `audit_logs` 的实际写入（目前 `log_audit` 零调用），否则「谁在什么时候查了什么」无法回答。

### ME-B5 — 订阅面：webhook 投递 outbox 化 + 事件类型扩展 + SSE

**结论**：mega2 的实时推送能力目前是**零**，这会让任何 agent 集成退化成轮询。

**依据（已系统核查）**：

- **SSE / WebSocket server / LISTEN-NOTIFY**：`grep -rn "Sse\b\|text/event-stream\|WebSocketUpgrade\|on_upgrade" src/` → **0 命中**。全仓唯一 WS 相关物是 `src/contract/api/buck2/ws.rs` 的**类型定义**（服务端在未移植的 Orion）；
- **webhook 是 fire-and-forget**：`src/jupiter/service/webhook_service.rs:119` 直接 `tokio::spawn`，进程崩溃即事件永久丢失。`mega_webhook_delivery` 的字段是 `{id, webhook_id, event_type, payload, response_status, response_body, success, attempt, error_message, created_at}`——**无 `status`、无 `next_attempt_at`**，是投递**记录**而非待投递队列；
- **webhook 事件类型只有 7 个，全是 CL 域**（`WebhookEventTypeEnum`）：`cl.created` / `cl.updated` / `cl.merged` / `cl.closed` / `cl.reopened` / `cl.comment.created` / `all`。**无 issue.\*、无 build.\*、无 review.\*、无 push.\***；
- **行内代码评论不发通知也不发 webhook**（`code_review_router.rs` 中通知/webhook 调用 0 处）——即 mega2 最完整的那套锚定评审能力，对外部完全不可见。

---

#### 「是否已有 outbox」的当场结案 + **与 `plan-long.md` 五处 outbox 口径的对齐（本版新增）**

**代码侧结案**：`grep -rn "outbox" src/` 只有 6 处，全是**注释**（`notification/service.rs:29`、`channels/inapp.rs:12`、`channels/mod.rs:32`、`cl_router.rs:191`、`cl_router.rs:564`、`issue_router.rs:224`）；`grep -rn "Dispatcher" src/` 里唯一的实体是 `BuildDispatcher`（`src/ceres/build_trigger/dispatcher.rs:12`）。**通知面没有 outbox 表、没有 dispatcher worker。**

**三条 router 注释还有一处此前未登记的问题（本版新增）**：`cl_router.rs:191`、`cl_router.rs:564`、`issue_router.rs:224` 三条注释都说「delivered by the background dispatcher」并指向 **`docs/notification.md`**——**该路径的文件不存在**（实际事实源是 `docs/refactoring/notification.md`）。即这三条注释同时有两处失真：指称一个不存在的 dispatcher，和一个不存在的文档路径。

**事实源侧**：`docs/refactoring/notification.md:15-20` 逐字写着 mega2 已无「`SmtpMailer`, `lettre`, `EmailDispatcher`, or `email_jobs` outbox」，且「The website is the sole owner of templates, delivery providers, **retries**, and SMTP configuration」。

**`plan-long.md` 现存的 5 处 outbox 表述，逐条对齐**：

| 位置 | 原文要点 | 与本条的关系 | ME-B5 必须做的动作 |
|---|---|---|---|
| `:177`（SB-03 当前风险） | 「多进程 outbox claim 竞争矩阵……未建立」 | 指的是**测试形态**，不指称具体实现 | 无冲突。新建 webhook outbox 后，此条获得第一个真实被测对象 |
| `:193`（PT-01 移植问题） | 「后续 PT 将引入……多实例 outbox 竞争……等新形态负载」 | 同上，前瞻性表述 | 无冲突 |
| `:200`（PT-01 目标范围） | 「多实例 outbox claim 竞争基线用例进入 CI 可运行形态」——而 **PT-01 已标「已完成」**（`:112`），其状态描述里对应的成果是「**并发 dispatcher 基线**」 | **口径漂移**：PT-01 交付的「并发 dispatcher 基线」实际对象只可能是 `BuildDispatcher`（全仓唯一 dispatcher），不是通知/webhook outbox。即完成判据的文字与实际交付物不同指 | ME-B6 补第五处漂移登记；ME-B5 立项时**不得**援引 `:200` 声称「基线已有」 |
| `:495`（PT-09 非目标） | 「不回流 Mega 的旧 notification/email 实现（原则 8）；**不在本仓重建 SMTP outbox**」 | **这是唯一有实质拦截力的一条** | ME-B5 必须在立项第一段**显式区分**：本条建的是 **webhook / SSE 投递 outbox**（通道是 HTTP POST 与 SSE，不含 SMTP、不含邮件模板、不含 `email_jobs`），与 `:495` 禁止的 **SMTP outbox** 是两件事；并引 `docs/refactoring/notification.md`「retries 归 website」佐证本条不侵入邮件域。否则会被这条非目标原样挡回 |
| `:741`（§性能） | 「通知 outbox 的批次/并发限流默认值不因新渠道退化」 | **失去指称对象**：本仓当前没有通知 outbox，该约束无处落地 | ME-B6 补第五处漂移登记；ME-B5 可顺势为该约束提供真实落点（新 outbox 的批次/并发默认值即为该条的第一个基线） |

→ **结论**：ME-B5 仍按「**新建 webhook 投递 outbox**」推进，但立项文档必须带上上表的逐条对齐，并顺带清理三处陈旧注释（含其不存在的 `docs/notification.md` 路径）——P3 级。

---

**内容**：

1. 给 `mega_webhook_delivery` 加 `status` / `next_attempt_at`，配一个 `interval` worker（`http_server.rs:199-230` 已有现成的 spawn + watch 关停模式），把 fire-and-forget 变成可恢复投递；
2. 扩事件类型至 `review.thread.created` / `review.thread.resolved` / `check.*`；
3. **SSE 优先于 WebSocket**（`axum::response::Sse`）——更契合现有「只出不进」的通知模型，且能直接复用 outbox worker 做扇出。

**落点**：`src/jupiter/service/webhook_service.rs`、`src/callisto/mega_webhook_delivery.rs` + 新迁移、`src/api/router/`。
**风险**：中。需先修两个多实例前置门槛：snowflake `worker_id(1)` **硬编码**（`src/jupiter/utils/id_generator.rs:21`，多实例部署会 ID 冲突）与连接池 `max_lifetime(Duration::from_secs(8))`（`src/jupiter/storage/init.rs:112`，高频写会不断触发重连）。

### ME-B6 — 治理动作：补 `docs/refactoring/README.md` 索引 + 处理 plan-long 的**五处**漂移

**本条已大幅收缩，并降级为 P3 级文档补漏。** 原版称 `docs/refactoring/augmentcode.md` 是「完整的、**被遗忘的**孤儿文档」「**从未进入任何日期计划**」，且警告其「地基表已被三轮重构作废」——**三条全部为假或为复述**：

- `plan-20260731.md:134`（2026-08-06 完成度复审）条目 ⑥ 即「**DOC-01：修复 `augmentcode.md` 悬空 chat.md 引用并加现状注记**」，并把该文件列入写集；
- `plan-20260731.md:1614` 是一整段专门针对该文件的执行后注记；
- `plan-20260731.md:1645` 为容纳它改写了 VER-2 守卫命令；
- **「地基表已作废」的警告本身就写在文件里**——`augmentcode.md:5` 的现状注记逐条点名了同样那三轮重构，并写明「阅读地基表时请以现行源码为准」。原版是在复述该文件已有的注记。

**真正成立的只有一条**：`docs/refactoring/README.md` 全文对 `augmentcode.md` **零命中**（`grep -c` = 0，已确认）。

**收缩后的内容**：

1. 给 `docs/refactoring/README.md` 补一条 `augmentcode.md` 索引条目，注明其为 2026-06-21 历史快照、地基表以现行源码为准（指向 `augmentcode.md:5` 的既有注记，不重复其内容）。

**该文件的素材价值仍然成立，可作为新立项的参考**（不是本条的行动项）：26912 字节，撰写 2026-06-21，入库 2026-08-05；P0 建议九张 `agent_*` 表（`agent_experts` / `agent_sessions` / `agent_runs` / `agent_steps` / `agent_capabilities` / `agent_trigger_specs` / `agent_artifacts` / `agent_memories` / `agent_secrets`，`:184-192`）；六阶段落地顺序（**阶段 0 冻结边界与 schema，不接模型、不引 LLM 依赖**，`:381`）；一份「不建议优先做的事」反模式清单（`:459`：不把 SDK 塞进业务 router、不把 AI review 写成旁路评论系统而应复用 `code_review`、不让 build trigger 承担所有自动化事件、Context Engine 未有权限过滤前不接组织级知识、不把 secret 当环境变量）；9 项 MVP 清单（`:472`）。其核心主张与「先接一个 LLM API」相反：「应先把已有协作、事件、权限、密钥、工件和通知模块**收束为 Agent 平台的领域边界**，再逐步接入模型和远程执行」——**与本文 §1.3 的判断一致**。

**同时应处理的 plan-long 五处漂移**（任何要求改 plan-long 的建议都会撞上它们）：

1. §实施顺序（`:633`）仍写「当前执行任务：`plan-20260820.md`」，实际已到 `plan-20260827.md`；
2. §日期计划索引表（**表头 `:757`，表体 `:759-764`**，本版行号订正——原写 `:756-765` 不准）只列到 `plan-20260820`，**缺 `plan-20260824` / `plan-20260826` / `plan-20260827` 三份**；
3. PT-02 表格行（`:113`）的「关联日期计划 = 无」与现状描述（「同步基线停在 Mega #2129，Mega 已到 #2169」）均未反映 `plan-20260826` 的 #2130→#2175 收口——而同一份文档 `:69` 自己记录了该收口。*（限定，本版引用订正）*：`plan-20260826` 的自陈在 **`:69`**（「……campsite 身份模型（#2165–#2170）、ceres application/transport/bus 结构重构（#2138/#2139/#2142）、locks/verify 400 分类登记为 DEFER-SYNC-02/03/05，**不改变 PT 状态**」），**不是** `:74`——`:74` 的「本次结论」段是上一轮（2026-08-11）的结论，其「不改变任何 PT 状态或优先级」指的是 Mega #2165..#2169。故这是**索引登记层面的判断题**，不是 PT 状态判定错误，应如此标注；
4. **PT-03 自相矛盾**：表格行 `:114` 声称「**repo/path 级 push ACL / Cedar 三态 push 门**已由 `plan-20260812.md`（REL-01 UN-02/UN-03/UN-13）交付」，而 §目标范围 `:275` 与 §完成判据 `:289` 仍把 repo/path 级 push ACL 列为未完成；`plan-20260812.md:28`（§非目标）逐字写着「**路径级（per-path/per-subrepo）ACL**：本计划采用单 monorepo 资源归一模型（ADR-UN-05）……归 DEFER-UN-01 的后续设计」。即真正交付的是 **push 门本身**，不是路径级 ACL。**ME-B1 的「与 PT-03 合并评估」依据的是 §目标范围 `:275`，不是表格行 `:114`**，必须在立项文档里显式声明；
5. **【本版新增】outbox 口径失去指称对象**：PT-01 §目标范围 `:200` 的「多实例 **outbox** claim 竞争基线用例」与 §性能 `:741` 的「通知 **outbox** 的批次/并发限流默认值不因新渠道退化」，在邮件投递整体迁出本仓之后已无对应实现——本仓无通知 outbox 表、无通知 dispatcher（全仓唯一 dispatcher 是 `BuildDispatcher`），而 PT-01 已标「已完成」，其交付物描述为「并发 dispatcher 基线」。应改写为指向真实对象（build dispatcher 竞争 / 未来的 webhook 投递 outbox），或与 PT-09 的「多实例通知竞争矩阵」合并表述。**不处理则 ME-B5 会同时被两种读法夹击**：一种读法认为 outbox 已有（`:200` 已完成），另一种读法认为 outbox 被禁（`:495`）。

**落点**：`docs/refactoring/README.md`、`docs/plan/plan-long.md`。
**优先级**：**P3 级文档补漏**，不列入第二梯队。
**风险**：低，纯文档。第 4 项若不处理，ME-B1 的关联评估会在评审中被表格行挡回；第 5 项若不处理，ME-B5 的立项会在 outbox 口径上被反复往返。

---

## 5. C 组：应坚守 / 不跟进

本组既是设计边界也是叙事资产。**Delta 的每一条安全空白，mega2 都有对位资产——除了沙箱。**

| Delta 空白（官方自认，`delta.md:74`/`:75`） | mega2 对位 |
|---|---|
| **无权限系统**（agent 自主调用含破坏性工具，无审批） | Cedar 三态 fail-closed，admin 单源（UN-04），匿名保留字 `User::"__anonymous__"`（修复过一个真实漏洞：旧 fallback 是字面量 `"reader"`，会与真实叫 reader 的账户碰撞，`cedar_guard.rs:138-143` 注释逐字印证） |
| **无沙箱**（对所在设备无限制访问） | **同样没有**（agent 执行面整体不存在）→ 见 ME-C5 |
| **无 worktree 信任审查**（共享内容加载时可能自动执行代码） | `merge_checker` 五门（`cl_sync` / `cla_sign` / `code_review` / `commit_message` / `gpg_signature`）+ `path_check_configs` 的 `required` 标志 |
| 云优先 / 删除不完整 | **自建部署即答案**——数据在你自己的 Postgres + 你自己的 orbit 后端 |
| 遥测与 Sentry 崩溃上报**无退出开关** | 无外发遥测；SB-02 脱敏纪律 + 活进程脱敏门禁 |
| 密钥脱敏只匹配已知值 | `SecretRef` / `SecretString` + `libvault` 0.3.0；「DB 凭据永不进 vault」的引导循环硬约束 |

**这是 mega2 目前最锋利、最不需要新工程的差异化。** 但必须诚实：沙箱确实没有——这不是「我们更安全」，是「我们还没进这个场」。

---

**ME-C1 — 不做 CRDT 实时复制 worktree / 键击级捕获。**
理由：与 forge 的「服务端权威事实源 + 可审计」定位正面冲突；CL 模型（ADR-MC-01/02）建立在「聚合 diff + 单父 commit」之上，嫁接 CRDT 等于推翻 trunk 不变式。§2.2 的两层方案（trunk 存结论 / CL 存过程）已取其大部分价值，且不需要 CRDT。

**ME-C2 — 不做实时多人编辑与「人人可 steer」。**
理由不是做不到，而是与主体归属语义正面冲突：`merge_queue.requester`（UN-18）、`commit_auths.matched_username` 都在回答「这归谁」。如果三个人和两个 agent 同时 steer 一个会话，产出的 commit 归属谁？Delta 没有答案（`delta.md:71` 靠「作者归属直达模型」绕过去），**forge 不能没有答案**。正确的 forge 形态是**异步、只读、可脱敏的分享**（CL 的会话记录 + 锚定评论 + 检查结果，按 Cedar 判定可见性）+ 明确的主体归属。

**ME-C3 — 不做未提交编辑上云。**
见 §1.2 的设计原则。这是 Delta 被批评最多的一点（`delta.md:104`），也是自建 forge 的核心卖点所在。

**ME-C4 — 不实现 ACP，不做客户端。**
ACP 是编辑器↔agent 协议，两端都在用户机器上，forge 不在这条链路上。forge 做摄取面（ME-B2），不做协议端点。

**ME-C5 — 不照抄 `.agents/prepare` 式的仓内可执行引导脚本。**

**引用订正**：原版称 delta.md「已将其列为**风险清单第一条**」——两处偏差。① `delta.md:74` 记录的 Delta 官方 Agentic Safety 三项自认顺序是**无权限系统 → 无沙箱 → 无 worktree 信任审查**（「共享内容加载时可能自动执行代码」是**第三条**的括注）；② `.agents/prepare` 在 delta.md 中从未被列为风险条目——它记在 `delta.md:59`（技术要点第 7 条，中性描述）；「风险清单第一条」这个说法出自 `delta.md:136`，那是 **delta.md 写给 Libra 的 B7 建议**中的一句修辞，不是 Delta 的风险清单。

**准确表述**：`delta.md:74` 记录 Delta 官方自认无权限系统、无沙箱、无 worktree 信任审查（共享内容加载时可能自动执行代码）；`delta.md:59` 记录 `.agents/prepare` 是仓库根部的可执行脚本，Delta 在每个新建托管 checkout 里、agent 开工前执行（装依赖等），失败不致命。

**对 forge 而言这个模式更危险**：仓库内容触发**服务端**代码执行 = 教科书级 RCE 面。

**forge 已经有正确的模式**：`path_check_configs`（`{path, check_type_code, enabled, required}`）—— **检查器的类型是服务端枚举（`CheckTypeEnum`，8 个固定取值：`gpg_signature` / `branch_protection` / `commit_message` / `cl_sync` / `merge_conflict` / `ci_status` / `code_review` / `cla_sign`），仓库只能配置启用与否**。把这个心智延伸到 agent 环境准备就得到正确设计：**仓库声明它需要什么环境（依赖、工具链、服务），服务端按白名单 provision；仓库不能声明「运行这段任意代码」。**

这是 forge 相对伴生层的**结构性优势**：伴生层跑在用户机器上，它「只能」相信仓库；forge 跑在自己机器上，它**必须**不相信仓库——而这个约束逼出了更好的设计。

配套硬约束：**agent 执行面一旦要建，必须从第一天就带沙箱与审批**（吸取 Delta 的教训：`delta.md:74` 开门见山承认安全项全在 roadmap，`delta.md:82` 的 Sandboxing 排在 GA）。PT-08 的 QEMU VM 池是天然载体；若 PT-08 最终「不采纳 QEMU」，则 agent 执行面必须另找隔离方案，**不能裸跑**。

**ME-C6 — 不照抄仓库级的线程分享授权。**
即 Delta 的已知扩权缺陷（`delta.md:71`：分享线程 = 授予仓库级 worktree 历史访问，官方文档明示）。forge 有能力做对（路径级 / CL 级），不应照抄。见 ME-B4 的设计约束。

**ME-C7 — agent 会话面不得长成 chat 产品。**
本仓刚把 Campsite 风格 chat / Notes 整栈删除并写进 plan-long 的「不进入」清单（`plan-long.md:751`：「产品面已由 `plan-20260731.md` 整栈退场（**能力留在 website**），不占 PT 编号」——引用时须逐字，原文用的是「website」而非「monoui `apps/next-app`」，二者在本仓语境下大体同指，但这条要写进立项文档第一段，措辞会被评审逐字比对）；`plan-20260731.md:30` 的非目标也写着「不移植 website AI chat（`apps/next-app/app/api/chat`）」。会话 UI 归 monoui（原则 11，`plan-long.md:90`）。mega2 侧只提供**挂在 CL `link` 上的只读归档记录 + 查询 API**。

**ME-C8 — 影子数据面治理（叙事/治理条目，非工程）。**
把「第三方伴生层按 remote URL 汇聚本实例数据」（`delta.md:75`）列为显式风险条目。技术抓手是现成的：`mega_webhook` 的 `path_filter` 与 delivery 记录、`audit_logs` schema——forge 至少应能回答「谁在什么时候用什么 token 拉走了什么」。**这是现有能力的叙事化，不是新工程。** 但前提是 `log_audit` 先接线（目前零调用点）。

---

## 6. 优先级、依赖与实施顺序

### 6.1 依赖图

```
ME-A1（实测批次：① no-op 静默回退 ② root parent 归属）──┐
ME-A2（update_branch 纳入写集：决策 0 no-op 语义 + 决策 1 压平语义）┤
                                                        │
                                                        └→ plan-20260827
                                                           MC-02/MC-03/MC-04/MC-06 可正确落地
                                          MC-04 收口 ──┬→ ME-A3(a) 链序可判定性
                                                        │   （无条件触发的 ADR revisit；
                                                        │    出口 R-1/R-2 由 ME-A2 决策 0 定）
                                                        └→ ME-A3(b) CL↔trunk 边（无冲突）→ ME-B3
                                                                                              ↑
ME-B1（Bot 成为 Cedar principal）─────────────────────────────────────────────────────────────┤
      │                                                                                       │
      ├→ ME-B4（对外委托授权面）                                                              │
      └→ ME-B2（会话摄取面）←──────────────────────────────────────────────────────────────────┘
                │
                └→ ME-B5（订阅面：新建 webhook 投递 outbox + SSE）

ME-A4（补完锚定评审四洞）── 第 1/2/4 项与 monorepo.rs 写集相交，须按 GC-01 排；
                            第 3 项另需先过 plan-long 原则 4 的漂移确认
ME-A5（Orion 登记第二消费者）── 随 PT-05/PT-06 决策期
ME-B6（README 索引 + plan-long 五处漂移）── 随时可做，P3
ME-C*  ── 边界与叙事，随时可写
```

### 6.2 建议顺序

**第一梯队（立即，且是在途计划的正确性前置）**

`ME-A1`（先跑场景 2 no-op 静默回退，再跑场景 1 root parent） → `ME-A2`（做决策 0 与决策 1，**只做决策，不做实现**）。

> **口径收窄（本版修订）**：这两条中，**「提出问题 + 跑实测 + 做决策」不涉及新工程**，可立即插入 `plan-20260827` 开工前；**ME-A2 决策 0 的 0-A/0-B 与决策 1 的 (a)/(b) 一旦被选中，其落地都是新工程**（ADR revisit + AC 修订 + 可能的迁移 + 重新评审），须另行排期，不得算进第一梯队的成本。

**不做则 MC-02/MC-03/MC-04/MC-06 交付即残缺，且主路径缺陷只在真实并发负载下暴露，no-op 路径缺陷更隐蔽。**

**第二梯队（按 GC-01 核对 `monorepo.rs` 收口状态后排）**
`ME-A4` 第 1+2 项（同一调用点，合并修：分页截断 + N×diff）与第 4 项（`expect` panic，可顺带 `process_ref_updates:620` 的 `unwrap`）。第 1 项是静默正确性缺陷，优先级高于其余。第 3 项（Tier 3）是新算法 + 原则 4 前置，独立排期。

**第三梯队（MC-04 收口之后）**
`ME-A3(b)`（CL↔trunk 显式边，无冲突，但需新迁移——见 §6.3）；`ME-A3(a)`（链序可判定性）**无条件启动 ADR revisit**，其出口（R-1 改 rationale / R-2 存顺序）由 ME-A2 决策 0 的结论决定。

**第四梯队（新方向的唯一瓶颈，需独立日期计划）**
`ME-B1`。**这是本文唯一的真瓶颈**：ME-B4 / ME-B2 硬依赖它，ME-B3 的价值也要靠它才能对外释放。
建议与 PT-03 §目标范围 `:275` 的「repo/path 级 push ACL」合并评估（依据不是表格行 `:114`）。落地路径：shadow 观察 → enforce 切换。

**第五梯队（ME-B1 之后）**
`ME-B2`（会话摄取）→ `ME-B3`（归因）→ `ME-B4`（委托授权）→ `ME-B5`（订阅面）。

**随节奏走（不独立排期）**
`ME-A5` 在 PT-05 通信形态决策与 PT-06 任务模型定义期插入一句约束。**成本近零，收益是避免为 agent 再造一套平行执行平面。**
`ME-B6` P3 级，随手做（第 5 项漂移建议与 ME-B5 立项同批处理）。

### 6.3 写集冲突提醒

`plan-20260827` 在 §与其它计划的关系 的 `plan-20260826.md` 行已声明写集相交状态下**不得并发**（原文：「其 SYNC-03/05 与本计划 MC-04 都可能触 `src/jupiter/storage/cl_storage.rs`｜开工时按 GC-01 核对其收口状态；若仍在跑，补顺序边（MC-04 排在其后），**不得在写集相交状态下并发**」）。八张卡的 Implementation write set 按卡号登记（不用行号）：

| 卡 | Implementation write set |
|---|---|
| MC-01 | `src/ceres/pack/monorepo.rs`、`src/ceres/pack/push_chain.rs`(新) 等 |
| MC-02 | `src/ceres/merge_checker/` |
| MC-03 | `src/ceres/pack/monorepo.rs`、`src/ceres/pack/push_chain.rs`、`src/ceres/protocol/mod.rs` |
| MC-06 | `src/ceres/pack/monorepo.rs` 等 |
| **MC-04** | **`src/jupiter/storage/cl_storage.rs`（含事务化 API）、`src/ceres/pack/monorepo.rs`（管线挂点）、`src/ceres/code_edit/model.rs`（CL 更新的事务编排点）** |
| MC-05 | `src/api/router/cl_router.rs`、`src/ceres/model/change_list.rs` |
| MC-07 | `src/contract/policy/guard/` |
| MC-08 | N/A（发布点） |

**本文建议触及的文件，逐条对照（本版补全）**：

| 文件 | 谁触及 | 状态 |
|---|---|---|
| **`src/jupiter/storage/cl_storage.rs`** | **ME-A3(a)**（改 `save_cl_commits` / `get_cl_commits`：无论出口是 R-1 的守卫用例还是 R-2 的顺序存储，都必然要改这两个函数） | **⚠️ 本版新增登记。该文件明确在 MC-04 的 Implementation write set 里，也在与 `plan-20260826` 的并发禁令行里。此前 §6.3 只把它列在「已声明的冲突面」而未与 ME-A3 挂钩，读者会误以为 ME-A3 只碰未声明文件。ME-A3 与 MC-04 是文件级直接相交，必须串行** |
| `src/ceres/pack/monorepo.rs` | ME-A4 第 1/2/4 项（`:1179-1195`） | 与 MC-01/03/04/06 直接相交，须按 GC-01 核对收口后排 |
| `src/callisto/mega_cl_commits.rs`、`src/callisto/mega_cl.rs`、`src/jupiter/migration/` | ME-A3(a) 的 R-2 出口、ME-A3(b) | 见下方「更硬的约束轴」 |
| `src/ceres/api_service/mono_api_service.rs` | ME-A3(b)（`:2504-2571` 回写 `merge_commit_id`）、ME-A2 的决策 0-A（`:3255-3268`）、ME-A4 顺带项（`:620`） | **不在任何一张卡的写集里**（仅作为 Current evidence 被引用），但它是 MC-06 端到端验收断言的对象，改动须与 MC-06 协同 |

**比「不在写集文本里」更硬的约束轴（本版新增引用）**：`plan-20260827` 的 **「兼容与文档收口」章**逐字写着

> `- [ ] `src/callisto/`、`src/jupiter/storage/` 与 `src/jupiter/migration/`：N/A（复用既有实体与迁移，无新增）。`

这是一条**计划级的整目录断言**，且「完成判据」章要求「`docs/monorepo.md`、`docs/refactoring/protocol.md`、`docs/errors.md` 同步完成（或显式 N/A）」并要求「代码 review 最终结论 `PASS`」——即该收口章必须被满足。MC-04 卡内另写「Migration and rollback: N/A：复用既有表与迁移，无新 schema」。

> *（对上一轮复核意见的精度订正）*：该整目录 N/A 断言位于**「兼容与文档收口」**章，**不是「完成判据」**章。复核意见与任务书都称其为「计划完成判据」，实测不准；但其约束力不因此减弱——完成判据要求该收口章成立。

**这条断言的后果**：**任何为 `mega_cl_commits` 或 `mega_cl` 新增列的动作（ME-A3(a) 的 R-2 出口、ME-A3(b) 的 `merge_commit_id`）都会推翻它。** 因此这两项**必须另开计划**（或作为 `plan-20260827` 的正式修订走 G-09 记录 + 重新评审），不能作为 MC-04 的「顺带项」塞进去。这比「不在写集文本里」是更硬的拦截。

**结论**：ME-A3(a)/(b) 与 ME-A4 第 1/2/4 项均**不得与 MC-04/MC-06「同批」**；ME-A3 整体后移到 MC-04 收口之后，且其迁移动作须另开计划。

### 6.4 三门与模板

所有实施必须过：`cargo +nightly fmt --all --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `source .env.test && cargo test --all`，加 `cargo build` + `cargo build --tests` 零错误零警告。计划文档走 `plan-template.md` v2（`G-*` 粒度规则、`ER-*`/`GC-*` 全局约束、A–D 四组验收门）。禁手写 schema SQL（走 `src/jupiter/migration/`）；新 HTTP API 同步 utoipa/OpenAPI + `docs/errors.md`；**原则 11 强制**（`plan-long.md:90`）：任何新增或变更后端公开行为的计划必须包含对 monoui `apps/next-app` 与 Mega moon 两侧前端的影响评估，下游 UI 改动归 monoui 仓（先例：`plan-20260827` DEP-01）。**原则 4 强制**（`plan-long.md:83`）：凡在已移植模块（ceres/jupiter/api/callisto）上**叠加新能力**的条目（ME-A3(a) R-2、ME-A3(b)、ME-A4 第 3 项、ME-B3、ME-B5），立项文档须先给出「相关模块 Mega 漂移已吸收或明确排除」的书面确认——注意 PT-02 当前处于漂移窗口（DEFER-SYNC-02/03/05，`plan-long.md:69`）。

---

## 7. 待复核清单

| # | 项 | 复核方法 | 影响 |
|---|---|---|---|
| **1** | **no-op rebase 后的静默回退**（§2.3 断裂 B 路径 ②） | **ME-A1 场景 2 的六步复现**（空 diff CL + 并发合入 + Update Branch + merge），断言 A–E | **最高**。若成立即**数据丢失**级缺陷；同时决定 ME-A2 决策 0 的出口与 MC-02/03/04 的 AC 是否需补断链谓词 |
| 2 | **根路径 CL merge 后 main 的 parent 归属**（§2.3 断裂 C） | ME-A1 场景 1：一次真实 root push + merge，`git log main --format='%H %P %an <%ae> %s'`；并跑「同一根路径连续两个 CL」确认 `refs/cl/*` 未被清理 | **高**。ADR-MC-01 的正确性、MC-06 的 AC「恰好新增一个单父新 commit」能否通过 |
| 3 | 生产实例的 `[cedar].enforcement` 实际档位 | 部署侧确认。默认值是 `"off"`（`src/config/model.rs:999-1001`） | **高**。决定「mega2 有权限系统」这一叙事今天是否成立 |
| 4 | `gpg_signature` 门在目标部署里是否 enabled/required | 查 `path_check_configs` 实际配置 | 中。决定断裂 B 路径 ② 的「CL 被 gpg 门永久 fail-closed」是否是真实可见后果 |
| 5 | `mega_tree` / `mega_blob.commit_id` 是否真的无读取点（ADR-MC-06 的前提） | 全仓 grep 读路径。ADR-MC-06 自陈「无读取点」并留了 revisit 钩子；本文未独立复核 | 低。仅在触碰该字段时相关 |
| 6 | `plan-20260827` 引用的 `target/tmp/plan-review-codex-r*.md` 评审记录 | 在 `target/`（gitignore）下，本文未读。复盘 R1–R12 结论需另取 | 低 |
| 7 | walgit 对比文档 | `plan-20260827` 引用（`DEFER-MC-04` 的来源），但**不在仓库内** | 低。仅在处理 DEFER-MC-04 时相关 |

**已结案、从待复核清单移除的两项**：

- ~~trunk 推进后其它 CL 的评论是否重锚~~ → **已实测结案**：`reanchor_code_review_threads` 全仓唯一调用点是 `monorepo.rs:957`（CL push 路径），trunk 推进**不触发**其它 CL 的重锚。结论并入 ME-A4 第 5 项。
- ~~通知面是否已有 outbox~~ → **已实测结案**：`src/` 内 `outbox` 全部 6 处命中均为注释，无 outbox 表、无 dispatcher worker（唯一 dispatcher 是 `BuildDispatcher`）。三处 router 注释还指向一个**不存在的** `docs/notification.md` 路径。ME-B5 按「新建 webhook 投递 outbox」推进，并须与 `plan-long` 五处 outbox 口径逐条对齐（ME-B5 表格）。

**本文自身的边界**：未运行任何测试、未启动服务、未修改任何文件；全部 mega2 侧结论基于源码与计划文档的静态阅读，关键断言已逐条给出 file:line 且在本轮重新实测。`plan-20260827.md` 是未跟踪的活文档，本文对其一律用卡号/章节名 + 原文片段锚定。Delta 侧事实全部转引自 `delta.md`（160 行），未独立复核 delta.dev 页面；DeltaDB 开源后应按 `delta.md:144`（§6.3 C2）的快照规则重新核对。

---

## 8. 一页总结

**定位**：Delta 不是竞品，是一份客户端侧写给服务端的需求说明书。它的三层存储里，第一层（对象）和第三层（元数据）mega2 都有且更强，**唯一缺的是中间层**——但只该补其中的「过程」，不该补「未提交编辑」。真正的风险是脱媒：forge 退化成哑 origin，而 DeltaDB **按 remote URL 给仓库做键**（`delta.md:75`）意味着这个影子数据面的边界由 forge 定义、却由第三方执行。

**核心张力的判断（摆正，不和稀泥）**：CL 模型提供了 Delta 需要而 git 没有的**容器**——`link` 已被六张表共用，这部分论证成立。但「trunk 只留结论」是 **ADR-MC-01 已采纳的设计取舍**（Status `Accepted`，用户决策），它把决策链的保全责任 **100% 押在 CL 侧**；而 CL 侧今天有三处断裂，其中两处（`update_branch` 抹平/断链、根路径 parent 归属）**不在任何计划的覆盖范围内**。所以**风险既在设计（押注集中）也在实现（押注未兑现）**。按 Delta 的判准，mega2 今天在 trunk 上保留的决策链比一个普通 PR forge 还少。此外，CL 的并发模型是「每人每路径一个」，与 agent 时代的「每任务一个」不匹配——这正是 `DEFER-MC-01` 等待的那个重启条件。

**本版最重的新发现——`update_branch` 的 no-op 分支**：`mono_api_service.rs:3255-3257` 在 `cl_changed` 为空时把 `from_hash` 前移到 `target_head` 而 `to_hash` 原样不动；配合函数开头 `target_head == cl.from_hash` 的守卫，这**无条件**制造出一个 `from` 不在 `to` 祖先链上的 CL。后果有三层：① plan-20260827 里三处「从 to 反走 parent 链到 from」的算法（MC-02 gpg / MC-03 校验器 / MC-04 清单重建）全部落进未定义行为——其中 **MC-02 的 AC 已含「`from_hash` 不在链上时 fail-closed」谓词，而 MC-04 的 AC 没有**，这是计划自身的一处不一致；② MC-04 那条「表加顺序列是永久非目标，理由 = 链序在读取时由 parent 拓扑重建」的 rationale 依赖一个从未写出的不变式，而该不变式被这条一方代码路径无条件打破——**ADR revisit 的触发因此是无条件的**（revisit 的出口可以是「修 no-op 恢复不变式」而非「加顺序列」）；③ **强推理、待实测复现**：no-op 后 `from_hash == main head` 使 `merge_cl` 守卫放行，而 `merge_cl_unchecked` 用 `cl.to_hash` 的**旧 base 树**推进 main，会**静默回退期间他人已合入的改动**。这一条已作为最高优先项写进 ME-A1 的实测批次，附六步复现。

**关于锚定评审的诚实结论（原版头条已撤回）**：原版拿一句不在基线内的话（"comments attach to snapshots and fall out of date"）当作 Zed 的论点再宣布它不成立，是自设靶子，已删除。真实情况是：mega2 的锚**不是内容锚**，是**带校验的行号锚 + diff 偏移，无内容检索兜底**（Tier 1 第一步就按 `original_line_number` 取下标，`code_review_service.rs:387`；Tier 3 是 TODO）。因此三条结论各归其位——① **生命周期**：`Open/Resolved` + **resolve 与 reopen 两个端点**，Delta 明确没有，**硬差异**；② **可审计性**：算法在源码里、有置信度与显式失配态，Delta 机制未公开——**开放性优势，不等于效果优势**；③ **锚定原理**：Delta 锚到不可变 delta 的身份锚在原理上强于启发式重匹配，五态降级恰是失配的证据而非优势，mega2 的等价物是「补 Tier 3 + 锚绑稳定 ID」——**那是 ME-A4 的目标，不是既有胜势**。而且这套机制还有四个洞，其中最严重的是：**重锚喂给 Tier 2 的 diff 被 `Pagination::default()`（per_page=20）截断**（`monorepo.rs:1179-1181`），CL 改动超 20 文件时第 21 个起的线程静默落 `NotFound`——正是 agent 的典型工作形态。

**唯一的真瓶颈**：`mega.cedarschema` 里没有 `entity Bot`（`grep -c "Bot"` = 0），四条 action 声明的 principal 全是 `[User]`。全仓唯一被持久化审计的人/机类型属于一张从未写入过的表（`log_audit` 零调用）；官方迁移建议是让 agent 改用**用户 token**（`docs/manuel/authz.md:96`），即架构性地要求 agent 冒充人——**而你无法归因一个冒充人的主体**。这个判断完全由 mega2 自证支撑，不依赖任何对 Delta roadmap 的解读。

---

## 修订记录

### 第一轮（2026-08-27，三方对抗式复核后的修订）

原建议经事实核验（1×P1 + 13×P2）、可行性与冲突核验（1×P0 + 3×P1 + 5×P2）、Delta 侧核验（2×P0 + 4×P1 + 5×P2）后修订。要点保留如下（详细逐条见第二轮末的「第一轮处置索引」）：

1. **§2.5 与 §8 整段重写**——删除自设靶子引语 `"comments attach to snapshots and fall out of date"`；订正锚定机制为「带校验的行号锚 + diff 偏移」；结论收窄为三条并各自标明轴；补入「Delta 的评论锚定机制未公开，身份锚记录的是引用/永久链接」的诚实限定。
2. **§2.2 与 §8 定性摆正**——ADR-MC-01 Status = `Accepted`，改写为「风险既在设计也在实现」。
3. **ME-A3 拆分为 (a)/(b) 并后移**——承认 MC-04 的 Out of scope 与 AC 原文，成本从「近零」改为「迁移 + AC 修订 + 重新评审」，序位移到 MC-04 收口之后。
4. **ME-B6 大幅收缩降级 P3**——删除「被遗忘/孤儿/从未进入日期计划/地基表已作废」四项不实表述。
5. **ME-A4 补入分页截断缺陷并置于首位**；**ME-B6 补入 PT-03 自相矛盾**。
6. **论据删除三处**——ME-B1 全部 Delta 侧论据、`integration_git_cli.rs` 旁证、ME-A5 的「Delta 云 runner 从零建」。
7. **事实与引用订正约二十条**（评审轮次、行号、`ConvTypeEnum`=15、callisto=65、对象 key 分片、`mega_blob` 为元数据、root/subpath 分档、重锚触发点与范围、outbox 当场结案等）。

### 第二轮（2026-08-27，可行性复审 FAIL 后的最终修订）

复审提出 2×P1 + 6×P2。**每条均由本人重新实测源码与文档后落笔，未照单全收**；其中三条对复审自身的引用做了精度订正。

**P1-1（本轮修订新引入的过强论断）— 全部接受，并扩大处置范围**

第一轮修订在 ME-A3(a) 里写「证伪复核员给出的反例……**AC 在单链场景下是对的**」，并据此把 ME-A3(a) 降为「仅当 ME-A2 选 (a)/(b) 才启动」。**实测确认这是错的**：`update_branch` 有两条写回路径，第一轮只核了主路径。no-op 路径（`mono_api_service.rs:3255-3257`）在 `cl_changed.is_empty()` 时把 `from_hash` 前移到 `target_head` 而 `to_hash` 不动，配合 `:3215-3217` 的 `target_head == cl.from_hash` 守卫，**无条件**制造断链。处置：

- ① **ME-A2 的可选语义面重构为「决策 0（no-op 语义）+ 决策 1（压平语义）」两层**，决策 0 给出 0-A/0-B/0-C 三个出口并逐项标注冲突面与代价；决策 1 的三选一表格新增「no-op 分支下的语义」一列，使每个组合都有定义。
- ② **ME-A3(a) 的触发条件改为无条件成立**，理由写明白：被打破的是「`from` 必在 `to` 祖先链上」这一未写出的不变式，而 AC 的 rationale（「链序在读取时由 parent 拓扑重建」）恰好依赖它。**但同时给出一条复审未提的诚实限定**：revisit 的**触发**无条件，revisit 的**结论**有两个出口——R-1「修 no-op 恢复不变式，只改 rationale 措辞 + 加守卫用例」与 R-2「接受不变式可破，存顺序 + 补 fail-closed 谓词」；把「无条件触发」直接等同于「必须加顺序列」是过度推论，因为断链状态下根本没有链可供排序。ME-A2 决策 1 选 (a)/(b) 时的多链问题作为 R-2 的**第二个独立理由**保留。
- ③ 「AC 在单链场景下是对的」这句无限定断言已**删除**；主路径的分析结论（长度为 1 的完好链）作为「路径 ①」保留在 §2.3 断裂 B 内，但不再据以推出任何关于 AC 的普遍结论。
- ④ **静默回退推论已作为最高优先项写进 ME-A1 的实测批次**（新增「场景 2」，含六步可执行复现与五条断言），并升为 §7 待复核清单第 1 行。标注为「强推理，待实测复现」。完整代码链已在 §2.3 断裂 B 内逐段给出：no-op → `merge_cl` 守卫（`:2071-2073`）放行 → `merge_cl_unchecked` 取 `cl.to_hash` 的 commit（`:2517-2521`）→ `build_result_by_chain(..., commit.tree_id)`（`:2535`）→ `apply_update_result(..., "cl merge generated commit", ...)`（`:2536`）。
- ⑤ **本人额外实测发现的两条（复审未提）**：（i）**MC-02 的 AC 已含「`from_hash` 不在链上（断链/伪造 to）时 fail-closed 报错」，而 MC-04 的 AC 没有对应谓词**——计划作者预见了断链但归因为对抗场景，未把谓词同步到 MC-04；已在 §2.3 断裂 B 内以表格登记，并作为 ME-A2 决策 0-B 的具体内容。（ii）断链影响的不止 MC-04，还有 **MC-03**（链校验器，撞 `MAX_CL_CHAIN_COMMITS`=250）与 **MC-02**（gpg 门永久 fail-closed，使该 CL 不可合）——已加入 §7 第 4 行「gpg 门是否 enabled/required」的待复核。

**P1-2（第一轮 P0 的同类缺陷未从 ME-A2 清除）— 全部接受**

实测确认：MC-04 的 AC「事务内先按 link **删除**再批量插入（重复/更新 push 幂等，无重复行）」与 append-only 互斥；「兼容与文档收口」章的整目录 N/A 断言与 MC-04 卡内「Migration and rollback: N/A：复用既有表与迁移，无新 schema」都与 (a)/(b) 的新表/新列冲突。处置：ME-A2 的选项表已改为**逐项标注冲突面 + 落地代价**的四列表格；「不涉及新工程」在 ME-A2 与 §6.2 第一梯队两处均已收窄为「**提出问题 + 跑实测 + 做决策**不涉及新工程；决策 0 的 0-A/0-B 与决策 1 的 (a)/(b) 的落地是新工程」。

> *（精度订正）*：复审称该整目录 N/A 断言在「计划完成判据（`:1008`）」，**实测其位于「兼容与文档收口」章**（完成判据是另一章，要求文档收口成立与 review PASS）。约束力不减，但引用须改。本文按实测表述。

**P2-3（活文档行号失准）— 接受，并自行重测**

实测：`docs/plan/plan-20260827.md` 未被 git 跟踪（`?? docs/plan/plan-20260827.md`），本轮核对期间（14:07）Review log 已是 **R1–R12**（复审于 14:04 时为 R1–R11），八卡 `Lifecycle` 行号已漂到 `348/434/506/581/654/739/811/880`。处置：

- 全文对该文件的**所有行号引用已删除**，改为「卡号 / ADR 号 / 章节名 + 原文片段」锚定（例：「MC-04 的 AC『事务内先按 link 删除再批量插入』」）；§6.3 的写集表按卡号重排。
- 「九轮评审」改为「**R1–R12，截至本轮核对 2026-08-27 14:07，仍在增加**」——**未采用复审给的「R1–R11 十一轮」**，因为本轮实测已到 R12；同时在文档开头的事实基线里加了独立的引用纪律警示框，注明该文件未跟踪、并发编辑、行号不可依赖。
- 其它文件的行号引用**保留并逐条抽查**：`monorepo.rs:230/907/920/945/957/1094/1180/1195/1223`、`common.rs:43-50`、`code_review_service.rs:352/383/387/436`、`mono_storage.rs:95-102`、`mono_api_service.rs:618/620/638-640/2058/2071-2073/2504/2517-2521/2535/2536/2538-2545/2544/2589-2597/2623/2736/3190/3215-3217/3242-3253/3255-3257/3279-3282`、`cl_storage.rs:43/436`、`code_edit/model.rs:334/347-348`、`id_generator.rs:21`、`init.rs:112`、`config/model.rs:999-1001`、`resource.rs:22`、`object_storage.rs:28-46/106/119`、`bot_ops.rs:19`、`audit_storage.rs:33`、`webhook_service.rs:119`、`mega_webhook.rs:15`、`authz.md:96`、`plan-long.md:69/83/90/112/113/114/117-119/177/193/200/275/289/353/495/633/635/741/751/757/759-764/787`、`plan-20260731.md:30/134/1614/1645`、`plan-20260812.md:28`、`augmentcode.md:5`、`delta.md:47/54/57/70/71/74/75/81` —— **全部本轮重新实测通过**，两处订正见下。

**P2-4（写集清单不完整）— 全部接受**

`src/jupiter/storage/cl_storage.rs` 已在 §6.3 明确与 **ME-A3(a) 挂钩**并标为「文件级直接相交，必须串行」；同时引入「兼容与文档收口」章的整目录 N/A 断言作为**比文件级相交更硬的约束轴**，据此写明 ME-A3(a) R-2 出口与 ME-A3(b) 的迁移**必须另开计划**（或作为 `plan-20260827` 的正式修订走 G-09 + 重新评审）。§6.3 整节改为表格，避免「三处未声明文件」的误读。

**P2-5（outbox 口径未对齐 / ME-B6 漂移清单不全）— 全部接受，并有额外发现**

自行 `grep plan-long.md` 并逐条读了 5 处命中（`:177/:193/:200/:495/:741`），ME-B5 内新增一张五行对齐表，逐条给出「与本条的关系 + 必须做的动作」。关键处置：

- `:495`（PT-09 非目标「不在本仓重建 **SMTP** outbox」）是唯一有实质拦截力的一条，ME-B5 立项第一段必须显式区分「webhook / SSE 投递 outbox」与「SMTP outbox」，并引 `docs/refactoring/notification.md:15-20`（mega2 已无 `email_jobs` outbox；retries 归 website）佐证；
- `:200` 与 `:741` 已失去指称对象，作为**第五处 plan-long 漂移**补进 ME-B6，并说明不处理会让 ME-B5 被「已有 outbox」与「outbox 被禁」两种读法夹击；
- **额外发现（复审未提）**：`cl_router.rs:191`、`cl_router.rs:564`、`issue_router.rs:224` 三条 outbox 注释同时指向一个**不存在的文件路径 `docs/notification.md`**（实际是 `docs/refactoring/notification.md`），已并入 P3 注释清理项。

**P2-6（原则 4 未覆盖 ME-A4 第 3 项）— 接受**

已在 ME-A4 第 3 项下加治理前置块，逐字引 `plan-long.md:83` 原则 4，并指出 PT-02 当前处于漂移窗口（`plan-long.md:69` 的 DEFER-SYNC-02/03/05）；§6.4 补入「原则 4 强制」一段，点名适用条目（ME-A3(a) R-2、ME-A3(b)、ME-A4 第 3 项、ME-B3、ME-B5）。

**P2-7（两处引用/归属失准）— 全部接受并实测**

- `plan-20260826` 自陈的出处由 `plan-long.md:74` 订正为 **`:69`**（实测 `:74` 是 2026-08-11 那一轮的「本次结论」段，指的是 #2165..#2169）；ME-B6 第 3 项已改。
- 日期计划索引表行号由 `:756-765` 订正为**表头 `:757`、表体 `:759-764`**（实测），实质判断「表体止于 `plan-20260820`，缺三份」属实，保留。

**P2-8（断裂 C 补强证据未登记）— 全部接受**

- `remove_none_cl_refs` 的根路径豁免（`mono_api_service.rs:2538-2545`）已作为断裂 C 的第一条补强证据登记，并据此在 ME-A1 场景 1 加了「同一根路径连续两个 CL」的实测要求；
- `process_ref_updates` 的 `ObjectHash::from_str(...).unwrap()`（`:620`）已作为 SB-01 域的同性质生产路径 panic 登记进 ME-A4 第 4 项的顺带项与 §6.2 第二梯队。

**未改动、按复审「复核通过」清单原样保留的内容**

§0 三前提（① 按 R1–R12 更新轮次与卡号锚定，其余不动）、§1 全节（三层对位表、脱媒论证）、§2.3 断裂 A / 断裂 C 的代码链、§2.4、§2.5 整节、ME-A1 场景 1、ME-A3(b)、ME-A4 第 1/2/4/5 项、ME-A5、§4.0 治理判断、ME-B1 的 mega2 侧全部自证依据、ME-B2、ME-B3、ME-B4、ME-B5 主体、ME-B6 收缩后的四处漂移、§5 全部 ME-C1..C8、§6.4 三门与模板、§7 已结案两项与「本文自身的边界」。编号 ME-A\*/ME-B\*/ME-C\* 未重编。

### 仍待复核（见 §7，按优先级）

1. **no-op rebase 后的静默回退**（最高，ME-A1 场景 2 六步复现；若成立为数据丢失级缺陷）；
2. **根路径 CL merge 后 main 的 parent 归属**（高，ME-A1 场景 1；影响 ADR-MC-01 与 MC-06 AC）；
3. 生产实例的 `[cedar].enforcement` 实际档位（高，默认 `"off"`，决定「有权限系统」叙事今天是否成立）；
4. `gpg_signature` 门在目标部署里是否 enabled/required（中，决定断链后「CL 永久 fail-closed」是否为真实可见后果）；
5. ADR-MC-06 前提（`mega_tree`/`mega_blob.commit_id` 无读取点）本文未独立复核；
6. `target/tmp/plan-review-codex-r1..r12.md` 评审记录未读（在 gitignore 下）；
7. walgit 对比文档不在仓库内。

---

## 附录 A：Delta 侧一手来源

本文的 Delta 事实经由 sibling 仓库 `libra` 的 `docs/development/gap/delta.md` 转引，其一手来源如下（抓取日 2026-08-27）。不持有该仓库的读者可直接核对这些来源。

**delta.dev 官方文档（全部 20 页）**：首页；`/roadmap`；`/docs/getting-started`；`/docs/whats-in-the-latest`；`/docs/concepts/delta-and-git`；`/docs/concepts/worktrees-and-machines`；`/docs/agents/threads`；`/docs/agents/terminals`；`/docs/agents/review-and-sync`；`/docs/agents/comments`；`/docs/agents/skills`；`/docs/agents/models-and-providers`；`/docs/collaboration/collaborate-thread`；`/docs/configuration/settings`；`/docs/account/plans-and-pricing`；`/docs/privacy-and-security/{data-storage,privacy,security,agentic-safety}`；`/docs/troubleshooting`。

**Zed 官方公告**：Introducing Delta（Nathan Sobo，2026-08-12）；Software Is Made Between Commits（Nathan Sobo，2026-06-11）；`zed.dev/deltadb` 落地页；Sequoia Backs Zed's Vision for Collaborative Coding（2025-08-20 —— CRDT 表述与商业模式的最明确来源）。

**第三方**：MindStudio 上手评测（2026-08-20）；byteiota 分析（2026-08-13 —— ACP 协议、agent 兼容清单、Hacker News 反馈汇总）。

**复核纪律（无公开仓库时的快照规则）**：Delta 无本地 checkout，`plan-long.md` 按 revision 增量复核的惯例不适用。后续每次复核须记录：① 产品版本（nightly build 号，本次为 `0.1.1-nightly`）；② 抓取日期；③ 复核的页面/公告清单相对上次的增删；④ 能力变更摘要（对照本文 §1、§2 逐项 diff）。DeltaDB 官方承诺开源（"build it, open-source it, and offer an optional paid service"），届时应转为标准 revision 审计。
