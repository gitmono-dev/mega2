# Trunk 直推形态（storage-only）与 Monorepo 写入序列化

本文档记录 `mega2` 在**不接入用户系统、不使用 Issue 与 Change List** 的部署形态下，Git 推送如何直接落入 `main`，以及为支撑该形态必须先行修复的 Monorepo 写入路径缺陷。

> **治理规范**：本文档遵循 **`general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **产品规则入口**：公开分支、Tag 和 ImportRepo 路径规则见[使用指南](../user-guide.zh.md)。本文记录 `push_policy = "trunk"` 下的写入设计与不变式；`review` / `trunk` 的配置差异见 [`../deploy-trunk.md`](../deploy-trunk.md)。

> **用户文档同步记录（TP-21，2026-09-09）**：N 分流推送与客户端对齐规则已摘要到[使用指南](../user-guide.zh.md)；不变式 I1–I6（含 I2a）、墓碑续接和路径索引一致性契约以本文为技术参考。部署形态（trunk / storage-only / `push_auth=none` / LFS / SSH）见 [`../deploy-trunk.md`](../deploy-trunk.md)。

> **协议实现锚点**：**[`protocol.md`](./protocol.md)**（receive-pack 分层、pkt-line、认证上下文）。**权限模型**：Cedar 判定与 `contract::policy` 见 **[`contract.md`](./contract.md)**。

> **集成测试指引**：验收路径落在 `tests/integration_git_cli.rs` 与 `scripts/git_protocol_smoke.sh`，测试栈与矩阵见 **[`integration.md`](./integration.md)** / **[`test-infra.md`](./test-infra.md)**。

## 需求前提（部署形态）

> **Libra 协同（2026-09-06）**：[`libra.md`](libra.md) REQ-LB-03/06 依赖本文阶段 1–3 的根写入序列化与 provenance，复用同一 root writer，不另建 Agent 写入队列。ADR-TP-10 同路径一项在队**仅约束 `kind='push'`**；多个任务的 CL 落地为 merge 行，由全序与锁内重查保证正确性，不新增或放宽 merge 的入队约束。证据重查、landing mapping 与 ref 更新须在同一事务边界设计。本文 storage-only trunk 形态不被强制接入 CL／website／人类批准，净零无新根 commit 的语义继续成立。此项仅登记协同需求，不表示本文功能已实现或修改既有 ADR。

本计划服务于一类明确的部署：使用方只需要 monorepo 的**存储与分发**能力，不接入 mega2 的用户系统，不使用 Issue 与 Change List，因而不走评审。

**主要使用方是 Agent**。Agent 以「每产生一个 commit 就推送一次」的方式使用 monorepo，因此**单 commit 推送（N = 1）是常态路径，多 commit 推送（N > 1）是例外路径**。这一假设决定了下文对两条路径的不同处理。

该形态有三条不可让步的要求：

1. **`main` 的历史不能退化**——N = 1（Agent 常态路径）时 commit 的作者、时间、message **逐字段保真**（对象级）；N > 1 的 squash 不承诺逐字段保真，以**内容保真**（不变式 I2：tree 精确等于客户端 tip）与 **provenance 完整**（ADR-TP-13/14/15：真实作者署名、Co-authored-by、完整枚举、服务端签名）替代。
2. **N = 1 时客户端 commit 原样落地；N > 1 时由 mega2 自动合并为一个 commit 进入 `main`**。Agent 把若干 commit 攒在一起推送时，不应把这些中间状态带进 `main`。
3. **保持 trunk-based development 的分支模型**——唯一公开分支 `main`。

补充前提：monorepo 体量大，**根路径 clone 不可行**，使用方通过子路径 clone 或虚拟文件系统挂载消费。因此本计划的全部设计以**子路径推送**为唯一形态；根路径推送虽在协议上成立（`src/contract/git_protocol/path.rs:104` 把空前缀归一为 `/`），但不作为该形态的假设。

> 本仓代码中不存在虚拟文件系统客户端的引用，上述挂载消费方属于部署前提，不是本仓的代码事实。

## 事实校准（2026-09-04；条目 18 于 2026-09-06 追加）

> 本文档中的代码引用已对照当前 `src/` 逐条核对。

1. **Monorepo 的分支推送无条件进入 CL 管线，删除类更新除外**。实际调用路径：分支命令经 `finalize_receive_pack`（`monorepo.rs:205`）→ `persist_mono_branch_cl_mega_refs_transaction`（`:863-877`）→ `apply_cl_mega_ref_for_push_command`（`:913`）落地为 `refs/cl/<id>`；`run_mono_post_push_pipeline`（`:975`）随后调用 `update_or_create_cl`。`Monorepo::update_refs`（`:676`）只在 tag 命令时被 `smart.rs:450` 调用（对 Tag 返回拒绝），其分支命令分支实际不可达。落地不检查 ref 名，**推送 `refs/heads/main` 同样进入 CL 管线**，`main` 只能经合并路径推进。两个例外：删除类命令（`CommandType::Delete` 或 `new_id == ZERO_ID`）在 `:916-939` 直接走删除分支，不产生 CL——其中 `main` 的删除被 UN-16 拒绝（`:920-931`），其余分支的删除直接生效。因此**review 形态下的非删除分支更新只写 `refs/cl/*` 行，不触碰根树与任何 `main` 行**（删除类例外直接删目标分支 ref，不经队列，UN-16 仅护 `main`）；根树的推进只发生在 CL merge 与 ImportRepo attach 上。

2. **`[monorepo]` 配置面没有任何直推相关字段**。`MonoConfig`（`src/config/model.rs:141`）只有 `import_dir`、`admin`、`root_dirs`、`object_format`、`rename`。全仓不存在 `direct_push` / `skip_cl` / `require_cl` 语义的配置项。

3. **Monorepo 的根树写入当前没有任何序列化**。`RedLock`（`src/ceres/protocol/mod.rs:202`，TTL 30000ms，键 `git:receive-pack:lock:monorepo-root`）**只接在 ImportRepo 的 attach 路径**（`src/ceres/pack/import_repo.rs:513`）。Monorepo push 与 CL merge 路径不持有任何跨进程锁。

4. **根树更新在推送与合并路径上是无版本检查的盲写**。`search_tree_for_update`（`src/ceres/api_service/tree_ops.rs:263`）在 `:271` 读取 `get_root_tree(None)` 取快照后逐级下行；`batch_update_by_path_concurrent`（`src/jupiter/storage/mono_storage.rs:287`）对 `mega_refs` 执行无条件 `UPDATE`，且不在事务内。**例外是 attach 路径**：`attach_to_monorepo_parent_in_txn`（`mono_storage.rs:453-483`）对根 ref 携带 `(ref_commit_hash, ref_tree_hash)` 双条件 CAS，`rows_affected == 0` 时返回 `MegaError::StaleMonorepoRootRef`，调用方 `import_repo.rs:507-511` 以 `MAX_ATTACH_ATTEMPTS = 64` 重试。盲写缺陷因此覆盖 push/merge 路径而非全部写入者；且该重试循环在 attach 被队列吸收后**必须删除**（见 1.9）——队列下陈旧根读取不可能发生，残留的重试会静默吞掉 ADR-TP-09 依赖的绕过信号。另注：`batch_update_by_path_concurrent` 对 `mega_refs` 中**不存在的行静默跳过**（`ref_map.get(...)` 无 else 分支，不插入也不报错），且各 `UPDATE` 经 `FuturesUnordered` 在连接池上并发执行，无法加入外部事务（见事实校准 16）。

5. **现有队列是 Postgres 的，Redis 侧没有队列**。`merge_queue` 表（`src/callisto/merge_queue.rs`，迁移 `m20251109_073000_add_merge_queue.rs`、`m20260815_000100_merge_queue_requester.rs`）配套 storage/service/router/DTO 四层。Redis 侧只有 `init_connection`（`src/jupiter/redis/mod.rs`，101 行）与 `RedLock`（`src/jupiter/redis/lock.rs`，792 行）；`RedLock` 是互斥锁，不具备 FIFO、持久化、位置查询与暂停能力。

6. **现有 merge queue 有三个互斥缺陷**：
   - `try_start_processor`（`src/jupiter/service/merge_queue_service.rs:169`）用 `AtomicBool::compare_exchange`，**仅进程内互斥**；驱动方 `ensure_merge_processor_running`（`src/ceres/api_service/mono_api_service.rs:4342`）在每个实例各起一个 processor。
   - `get_next_waiting_item`（`src/jupiter/storage/merge_queue_storage.rs:178`）是裸 `SELECT ... ORDER BY position LIMIT 1`，**无 `FOR UPDATE SKIP LOCKED`、无原子 claim**，两个 processor 可取到同一项。
   - `position` 取毫秒时间戳（`merge_queue_storage.rs:83`），非单调序列；同毫秒入队顺序不确定。

   此外其执行语义有两处必须在吸收时显式保留：冲突失败**重排队到队尾**而非标失败（`mono_api_service.rs:4419-4429`，`QueueFailureTypeEnum::Conflict` → `move_item_to_tail`）；执行期重查 CL 存在性、授权、冲突与 GPG 门（`mono_api_service.rs:4465-4603`，GPG 在 `:4599`）。

7. **子路径的 `main` ref 是懒生成的无父 commit**。`refs_with_head_hash`（`src/ceres/pack/monorepo.rs:121`）在路径无 `main` ref 时，用 `Commit::new(author, committer, tree.id, vec![], &message)` 合成一个 **parents 为空**的 commit 并落库。

8. **`remove_none_cl_refs` 有三个缺陷**（`src/jupiter/storage/mono_storage.rs:78`，唯一调用点 `src/ceres/api_service/mono_api_service.rs:2619`）：
   - 语义为**删除**后代 ref，而非续接；
   - `Column::Path.starts_with(path)` 是字符串前缀，**无组件边界**，`/project/foo` 会命中 `/project/foobar`；
   - sea-orm 2.0.2 的 `starts_with`（`sea-orm-2.0.2/src/entity/column.rs:315-321`）是 `format!("{}%", s)` 后接 `like`，**不转义 LIKE 元字符**，路径中的 `_` 与 `%` 会被当作通配符。

9. **合成 commit 的归属是硬编码的**。`process_ref_updates`（`src/ceres/api_service/mono_api_service.rs:623`）在 `:646` 用 `Commit::from_tree_id`，而 git-internal 0.8.7（`src/internal/object/commit.rs:104-128`）把 author 与 committer 写死为 `mega <admin@mega.org>`；message 由调用方传入常量（merge 路径为 `"cl merge generated commit"`，`mono_api_service.rs:2614`）。

10. **祖先方向已具备续接语义**。`process_ref_updates` 为每个路径生成的 commit 以**该路径自身的当前 tip** 为 parent（`mono_api_service.rs:646`），并在 CL ref 上额外推进 `main`（`:668`）。缺失的只是后代方向。

11. **推送链已有校验与上限**。`validate_incoming_push`（`monorepo.rs:1153`）执行 ADR-MC-04 单分支准入与 MC-03 `PushChain::validate`（`src/ceres/pack/push_chain.rs`：拒绝 merge commit、拓扑连续、tip 匹配）；链长上限 `MAX_CL_CHAIN_COMMITS = 250`（`src/ceres/merge_checker/mod.rs:27`）。`PushChain::ordered_commits` 的顺序是 **tip 在前**。

12. **`mega_refs` 只有一个索引**：唯一索引 `uniq_mref_path (path, ref_name)`（`src/jupiter/migration/m20250314_025943_init.rs:268-277`）。表列为 `path`（text）、`ref_name`（text）、`ref_commit_hash`、`ref_tree_hash`、`is_cl`、时间戳。

13. **合并侧门控分散在合并入口与 processor 中，都不在推送路径上**。`enforce_acl_change_authorization`（UN-19）在 `merge_cl_unchecked` 入口内（`mono_api_service.rs:2592` 附近，`merge_cl_unchecked` 自身 `:2580` 起）；GPG 门 `ensure_gpg_check_passed` 定义于 `:2135`，调用点在 `:2124`（直接合并入口）与 `:4599`（merge queue processor 的执行期重查）——**不在 `merge_cl_unchecked` 函数体内**，而是分布在其调用方。两处都不在推送路径上。

14. **`import_dir` 属重启生效字段**。`src/config/reload.rs:697` 将其登记为 `restart_required_fields`；`src/config/validate.rs:522` 校验其非空。

15. **存在两条无锁的惰性物化路径，都是 `mega_refs` 的写入者**。`refs_with_head_hash`（`src/ceres/pack/monorepo.rs:121`，经 `SmartSession::git_info_refs` 于 `src/ceres/protocol/smart.rs:76-82` 触达，即每次 advertise/clone/ls-remote）与 `create_repo_commit`（`src/ceres/code_edit/utils.rs:362-442`）都在路径无 `main` 行时从**各自读取的根树快照**合成无父 commit，并经 `mega_head_hash_with_txn`（`mono_storage.rs`，独立事务）落库——无 advisory lock、无 CAS、无版本检查。它们不改写根树，但写入的路径 ref 派生自根树快照，与 B3 并发时会使 `ref_tree_hash` 落后于根树（见 ADR-TP-20）。

16. **ref 批量更新原语无法加入外部事务，但单 ref 原语已可**。`batch_update_by_path_concurrent`（`mono_storage.rs:287`）在连接池连接上经 `FuturesUnordered` 逐条执行 `UPDATE`，不接受 `DatabaseTransaction` 参数；单 ref 的 `save_refs(..., txn)`（`mono_storage.rs:65-73`）与 `update_ref(..., txn)`（`:188-199`）已支持事务——缺的是**批量与 `apply_update_result` 调用链**的事务化变体。B3 的「单事务」硬要求需要补齐这些变体（阶段 1 交付物）。

17. **无用户系统的静态 token 认证需要改造现有认证链**。HTTP 启动有**两道** OAuth 门槛：`start_http()` 经 `require_oauth_for_http_service`（`src/server/http_server.rs:424-425`，实现在 `src/config/validate.rs:162-169`）在进入 `app()` 之前校验；`app()` 自身再取一次 OAuth 配置（`http_server.rs:627-634`）。git HTTP 的 token 认证经 `login_user_from_mono_access_token` 走 `UserStorage`（`src/contract/git_protocol/http.rs:105-149`）；`check_push_permission`（`src/contract/git_protocol/mod.rs:43-62`）**在 `cedar.enforcement = off` 时仍要求非空 username** 才放行；SSH 路径要求已认证用户（`src/contract/git_protocol/ssh.rs:135-157`）。阶段 5 的静态 token 模型必须逐一改造这些挂点（含两道启动门槛），不能只加配置项。

18. **阶段 1.1 写入者审计（TP-23，2026-09-06）已完成并机器可核对**。穷举 `rg -n "save_refs|update_ref|mega_head_hash_with_txn|batch_update" src/` 全部命中，并对 `mega_refs` 直接写原语做超集核对后，按硬约束 2 三分法 + 范围外登记：
   - **queue-serial（改写根树）**：CL merge `merge_cl_unchecked`→`apply_update_result`→`batch_update_by_path_concurrent`（`mono_api_service.rs:2582/2614/2651/2694`）；含分支命令的 ImportRepo attach（`import_repo.rs:450`→`:577`→`mono_storage.rs:453`）；trunk 推送落地为 planned（阶段 4，现状无代码路径）。附属：`remove_none_cl_refs`（`:2619`）。
   - **adr-tp-20（写路径 `main`）**：`Monorepo::refs_with_head_hash`（`monorepo.rs:121`，经 `:140` `get_all_refs("/", true)`）与 `create_repo_commit`（`code_edit/utils.rs:362/377`）。二者按根上**全部** `is_cl=false` 行（含 tags）复制到路径——路径 `main` 属 ADR-TP-20；路径 tag 复制为范围外副作用（审计清单 W-OOS-09）。
   - **bootstrap-lock**：`initialize_monorepo`（`mono_service.rs:103`）在 `:105` 持 `acquire_monorepo_initialization_lock`（`:63-70`）；根身份由 `converter.rs:727-746` 构造（`path="/"` + `MEGA_BRANCH_NAME`），经 `:126` ActiveModel insert 落库——**不经** `save_refs`。常态服务路径：`commands/service/mod.rs:51` → `context/mod.rs:281` → 再绑 HTTP/SSH（`multi.rs:53-60`）；one-shot `service init` 走 `bootstrap_monorepo`（`commands/service/init.rs:19-22`）后退出、不接流。
   - **范围外**：CL ref（`on_edit.rs:47`、`monorepo.rs:913-971`、`mono_api_service.rs:3392/3446`、**Buck** `buck_service.rs:928-938`→`save_or_update_cl_ref_in_txn`）、tag 创建/删除（`:1927/1978`、`:1356/1373`）、惰性物化/merge 清理的路径 tag 副作用、纯删除 attach、`git_db` 自身 refs。`remove_none_cl_refs`（`mono_storage.rs:78-84`，调用于 `mono_api_service.rs:2619`）在 `apply_update_result` **之后另连接**删除全部非 CL 后代（含路径 tags）——**非同事务**，崩溃可留下根已前进而路径行陈旧。
   - **结论**：无清单外生产 `main`/根树写入者；完整条目见 [`trunk-push-writers-audit.md`](./trunk-push-writers-audit.md)。硬约束 2 清单与本条一致，无需升级修订。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
| --- | --- | --- |
| Monorepo 推送落地 | 已激活 | 无条件进 CL 管线，删除类更新除外（`monorepo.rs:676/913/916-939/975`）；无直推路径；review 形态下推送不触碰根树 |
| 根树写入序列化 | **未实现** | push/merge 路径无锁盲写（事实校准 4）；attach 有 CAS + 64 次重试（同上），无全序 |
| 路径 ref 惰性物化 | 已激活 | 两条无锁物化路径从根快照写 `mega_refs`（事实校准 15），与 B3 并发时产生陈旧 ref |
| 队列基础设施 | 部分完成 | `merge_queue` 四层齐备，但互斥/claim/序号三处缺陷（事实校准 6）；仅覆盖 CL 合并，不覆盖推送与 ImportRepo attach |
| ref 批量更新原语 | 部分可用 | `batch_update_by_path_concurrent` 静默跳过缺失行、无法加入事务（事实校准 4/16） |
| 后代 ref 处理 | 已激活（语义错误） | `remove_none_cl_refs` 删除后代 ref（事实校准 8），导致 unrelated history |
| 祖先 ref 处理 | 已激活 | 已具备续接语义（事实校准 10） |
| 合成 commit 归属 | 已激活（归属丢失） | 硬编码 `mega <admin@mega.org>`（事实校准 9） |
| 子路径 ref 物化 | 已激活 | 懒生成无父 commit（事实校准 7/15）；无墓碑，删除后不可续接 |
| `push_policy` 配置 | **未实现** | `MonoConfig` 无该字段（事实校准 2） |
| 无用户系统的推送认证 | **未实现** | 启动强依赖 OAuth、token 认证走 UserStorage、push 权限检查要求 username（事实校准 17）；无静态 token 模型 |
| Redis 队列 | **未实现** | Redis 侧只有连接与 RedLock（事实校准 5） |

## 硬约束与不可违反的原则

1. **每一次落地（trunk 推送、CL merge、ImportRepo attach）都必然改写根树，因此写入序列化是语义必然，不是性能取舍**。子路径的改动要出现在根树中，必须重算从该路径到 `/` 的整条树脊并推进 `/` 的 `main`（review 形态的推送只写 `refs/cl/*`，不落地，见硬约束 2）。所有推送在 `/` 上的冲突率是 100%，乐观并发（CAS + 重试）在此负载上退化为忙等。唯一的例外是净零变更推送（ADR-TP-16）：根树内容不变，根与祖先不前进，被推路径仍需记录历史——该例外由显式判定产生，不是对序列化的豁免。违反后果：并发推送互相覆盖根树，路径 ref 与根树给出两个互相矛盾的视图，且无报错。

2. **队列的闸门粒度是「根树与路径 ref 的全部写入者」，不是「推送」**。已知写入者按类划分（与事实校准 18 / [`trunk-push-writers-audit.md`](./trunk-push-writers-audit.md) 对齐；锚点 2026-09-06 刷新）：
   - **改写根树的**：CL merge `merge_cl_unchecked`（`mono_api_service.rs:2582`，经 `:2614` 调 `apply_update_result`；定义于 `:2651`，生产调用方仅此）→`batch_update_by_path_concurrent`（`:2694`）、**含分支命令的** ImportRepo attach（`import_repo.rs:450`→`:577`→`mono_storage.rs:453`；纯删除式 attach——无非零分支命令时只删 ImportRepo 自身 refs 即返回，`import_repo.rs:450-475`，不触碰根树——**不入队**，登记为范围外）、trunk 形态下的推送落地（阶段 4 引入）。review 形态下的推送**不在其中**——`finalize_receive_pack`（`monorepo.rs:205`）在 review 形态只写 `refs/cl/*` 行，不触碰根树与 `main` 行。
   - **写路径 ref 的**：两条惰性物化路径（事实校准 15）——advertise/clone 上的 `refs_with_head_hash`（`monorepo.rs:121`）与 code_edit 的 `create_repo_commit`（`utils.rs:362-442`）。它们不改写根树，但写入的 ref 派生自根树快照，必须由 ADR-TP-20 的机制覆盖。实现上对根 `get_all_refs("/", true)` 的**全部**非 CL 行（含 tags）做路径复制——I3 相关的是路径 `main`；路径 tag 复制登记为范围外副作用（事实校准 18 / 审计清单 W-OOS-09）。
   - **队列外的**：bootstrap 初始化 `initialize_monorepo`（`mono_service.rs:103`）于 `:105` 持有专属 advisory lock（`acquire_monorepo_initialization_lock`，`:63-70`）；根身份由 `converter.rs:727-746` 构造，经 `converter.refs.insert`（`:126`）写根 `main`。常态服务在接流前完成（`commands/service/mod.rs:51` → `context/mod.rs:281` → `multi.rs:53-60`）；one-shot `service init` 走 `bootstrap_monorepo`（`commands/service/init.rs:19-22`）后退出。登记在审计清单中但不入队。
   
   违反后果：漏掉任一写入者，序列化形同虚设，且缺陷会以「视图不一致」的形式静默出现。

   **审计范围限定**：本约束与 1.1 的清单**主范围**覆盖 `refs/heads/main` 行的写入者——它们是根树的派生视图，参与写入判定链（I3）。`mega_refs` 还承载另外两类行的写入者：CL ref 行（如 `code_edit/on_edit.rs:47`）与 tag ref 行（`mono_api_service.rs:1927-1978`）——它们不是根树的派生视图、不参与任何写入判定：CL ref 行的一致性由 CL 管线自身的门控（`ClSyncChecker` 等）负责；**显式 tag API**（创建/删除，`mono_api_service.rs:1927-1978` / `:1356/1373`）独立管理根路径 tags，与 `ClSyncChecker` 无关——两者都**不在本队列的覆盖范围**，审计清单中登记为「范围外」以免混淆。**例外须同时登记**：惰性物化（`get_all_refs("/", true)`）会把根上非 CL 行（含 tags）复制到路径，merge 的 `remove_none_cl_refs` 会非同事务地删除路径上全部非 CL 后代（含 tags）——这些是真实生产副作用（审计清单 W-OOS-09/09a），不得因「tag API 独立」措辞而被后续队列改造省略。

3. **子路径推送必然在祖先方向产生合成 commit**。路径 `P` 处 commit 的 tree 是 `P` 的子树，根 commit 的 tree 是根树；二者形状不同，客户端 commit 在物理上无法直接作为根 commit。「N 个 commit 在 trunk 上合并成 1 个」是这一事实的推论，不是产品选择。

4. **已物化路径的历史只增不改（不变式 I1）**。任何写入之后，该路径的旧 tip 必须仍是新 tip 的祖先。违反后果：所有该路径的 clone 者收到 `refusing to merge unrelated histories`，本地工作无法推回，只能弃库重来。

5. **单 commit 推送不得在被推路径上合成 commit**。N = 1 时 `main@P` 恒等于客户端推送的 `cmd.new_id`。这是 Agent 场景的常态路径（需求前提），违反后果是每一次推送都强制客户端重新对齐，且原始 GPG 签名不再挂在任何可见历史上。N > 1 时被推路径按 ADR-TP-12 合并为一个 commit，属显式设计而非违反本条。

5a. **内容保真优先于对象保真**。无论是否合并，`main@P` 的 tip 的 tree 必须精确等于客户端推送 tip 的 tree（不变式 I2）。合并只折叠历史，不得改变任何一个字节的内容。违反后果：服务端落地的内容与客户端推送的内容不一致，是最难察觉的一类数据损坏。

6. **`push_policy = "trunk"` 与 `cedar.enforcement != "off"` 互斥**。ACL 自提权检查（UN-19）挂在合并入口（事实校准 13），trunk 形态绕过合并入口即绕过该检查。启动期 fail-closed 拒绝，比在推送路径上复制半套授权检查更可控。

7. **文件路径索引重建必须在临界区之外**。`traverses_tree_and_update_filepath`（`monorepo.rs:704`）从 tip 的树递归遍历重建索引，其成本与子树规模成正比。违反后果：大子树推送会长时间独占全局写入闸门。

8. **不改变 review 形态的既有语义**。阶段 1–3 是对现有写入路径的缺陷修复：CL 创建、检查、合并的**判定、门控与拒绝面**不得改变，既有测试用例的期望值不得修改。两类显式豁免（均为登记在案、验收覆盖的变化，不属语义漂移）：**缺陷修复**——阶段 2 的后代 ref 续接（含 merge 分支的切换，见 2.8）与其验收标准所列行为变化是本计划要交付的修复本身；**队列生命周期变化**——merge 的背压（深度/暂停拒绝与 503）、`wait_timeout`、异步转同步的执行模型（1.9 2a）是引入统一队列的必然伴随物，其运维面语义由 1.9 如实登记。阶段 4 之后的形态差异只由 `push_policy` 一个开关决定。

## 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
| --- | --- | --- | --- |
| 根树写入并发控制 | 无锁盲写 | 全局 FIFO 队列 + 事务级 advisory lock + CAS 断言 | 复杂 |
| 队列覆盖范围 | 仅 CL 合并，进程内互斥 | 全部根树写入者，跨实例互斥、原子 claim、单调序号 | 中等 |
| 后代 ref | 删除后重新懒生成（历史断裂） | 续接推进；路径消失时落墓碑 | 中等 |
| 合成 commit 归属 | `mega <admin@mega.org>` + 常量 message | 真实作者 + 完整 provenance trailer + 服务端签名 | 中等 |
| 被推路径的历史 | 合并后叠加一个合成 commit | N = 1 恒等于客户端 tip（零改写）；N > 1 合并为一个 | 中等 |
| 推送落地形态 | 只有 CL 管线 | `push_policy` 二选一：`review` / `trunk` | 中等 |
| 无用户系统的推送认证 | 依赖 mono access token | 静态 token + 路径前缀授权；认证身份为 token 名，commit 署名仅作 provenance | 中等 |
| 文件索引重建位置 | 临界区内 | 事务提交后异步，带序号保护 | 简单 |

## 写入模型（贯穿各阶段的统一规则）

`mega_refs` 是 `(path, ref_name)` 二维表。一次落在路径 `P` 的写入，按下列规则影响 `refs/heads/main` 行（含被推路径尚无已物化行的创建情形）：

| 类别 | 规则 | 新子树来源 |
| --- | --- | --- |
| `R == P`（被推路径），**P 无已物化 `main` 行且 `old_id = ZERO_ID`** | **创建语义**：INSERT `main@P = cmd.new_id`（N = 1 时即客户端 tip；N > 1 时为 squash commit），parent 按 N 分流规则取值 | 同被推路径行 |
| `R == P`（被推路径），**N = 1** | `ref = cmd.new_id`，**不合成**（常态路径） | 客户端推送的 `chain.tip.tree_id` |
| `R == P`（被推路径），**N > 1** | 合成 squash commit，`parent = R 的旧 tip` | 同上（tree 不变，只折叠历史） |
| `R` 是 `P` 的祖先（含 `/`） | 合成 commit，`parent = R 的旧 tip` | `build_result_by_chain` 逐层重算的输出（`mono_api_service.rs:570`） |
| `R` 是 `P` 的后代 | 合成 commit，`parent = R 的旧 tip` | 从 `chain.tip.tree_id` 向下解析 |

另有一条拒绝规则：**`P` 无已物化 `main` 行且 `old_id != ZERO_ID`** 的推送在 B0 拒绝（客户端声称的基线在服务端无据可查）。`P` 无行而 `old_id = ZERO_ID` 的创建语义必须显式实现——现状 `batch_update_by_path_concurrent` 对不存在的行**静默跳过**（事实校准 4/16），若不补创建分支，一次指向未物化路径的推送会在根树与祖先前进后**悄悄丢失 `main@P`**，客户端下次 advertise 时懒合成一个无父 commit，`refusing to merge unrelated histories` 在第一次向新路径推送时就复现。该缺陷的修复（upsert 或显式报错）列为阶段 1 交付物。

**创建语义按 N 分流**：`P` 无行时不存在「旧 tip」；合法创建的 `old_id = ZERO_ID` 而 `new_id` 为有效 commit，故 N = 0 在创建场景不可达（N = 0 当且仅当 `old_id == new_id`，见上）。N = 1 时 `main@P` = `cmd.new_id`（客户端 commit 本身，`landed_commit_id` = `new_id`）；N > 1 时落地 **parentless squash commit**（（`parent = vec![]`，I1 对空历史为空真；`new_id` 保留客户端 tip 不变，`landed_commit_id` = squash id，provenance/GC 锚点相应分开）。前置校验统一：`cmd.new_id` 必须是服务端可解析的 commit；「真创建」经 1.10 交付物 6 的**树项插入原语**沿路径补齐缺失组件（支持多级），ref 行经交付物 2 的 upsert 原语插入（`mono_storage.rs:287-333` 的静默跳过）——两条腿都齐，创建才可执行。provenance 相应调整：创建情形**省略 `Mono-Squash-Range`**（`ZERO_ID..tip` 不是合法的 Git revision range，也无基线可作端点），N 条原始 commit 的清单由 message 逐条枚举与 `push_queue` 行共同承担，链的 GC root 是该行的 `new_id`（= 链 tip，N 条原始 commit 全部是它的新对象，由它可达；`operation_id` 只是数据库操作指纹，不承载对象身份，不作为 GC root）——I4 的区间语义注明「创建情形以 tip 锚定全链」。

**墓碑优先于创建**：`P` 无行但存在墓碑时，创建语义**不得**生效——直接 INSERT 会让新 tip 与墓碑记录的历史断开（I1 破损）。B0 拒绝这类推送并提示：先经 advertise 从墓碑续接重新物化，`fetch && reset` 对齐后基于续接 tip 正常推送。

**孤儿链拒绝具有黏性**（[`plan-20260923.md`](../plan/plan-20260923.md) ADR-FU-07，issue #25 E3/E4）：`old_id = ZERO_ID` 的推送，若其第一父链沿 pack 内容走到无父根——新 tip 时途中没有可作 fork 基线的已知 commit；已知 tip 时只要 pack 链走到根即是，哪怕该历史已被其它 ref 引用（例如把另一路径的 clone 原样推到新路径，此前经 `validate` 以 `push chain is broken` 拒绝；因此孤儿拒绝不能证明历史未被引用）——`PushChain::resolve` 返回孤儿拒绝 `Can not init directory under monorepo directory!`；首推失败后对象已入库，原样重试时整条链变为「已知」——此前已知 tip 分支把根当作 fork 基线，重试漂移为 `push chain is broken: base X is not on the first-parent chain of tip X`。现在不论 tip 新旧都返回同一孤儿拒绝（类型化为 `MegaError::OrphanChain`，显示文本仍为 `Other error: Can not init directory under monorepo directory!`；`push_chain::orphan_chain_error` / `is_orphan_chain_error`）；trunk Noop 桥的 `PushChain::from_known_tip` 在 `old_id = ZERO_ID` 走到根时同样返回它。`old_id ≠ ZERO_ID` 的拒绝文案与对齐提示不变。trunk 形态下，创建路径上的孤儿拒绝映射为 `MONO_PATH_UNINITIALIZED`（见下）。

**创建的路径策略**（[`plan-20260923.md`](../plan/plan-20260923.md) ADR-FU-03 / ADR-FU-04，issue #29、#25 E1/E2）：trunk 下 `old_id = ZERO_ID`、`P` 无 `main` 行、无墓碑且在根树中不可解析为目录的推送是一次**创建**。准入门 `Monorepo::validate_incoming_push` 先调用唯一的分类函数 `path_policy::classify_creation_path`：

- `P` 不在任何 `root_dirs` 根之下 → `MONO_PATH_NOT_ALLOWED`，列出允许的根。不论历史是孤儿还是 fork 型都拒绝，关闭此前未文档化的「fork 型创建新顶层」。（`import_dir` 之下的路径由协议分派给 ImportRepo，不经过这道门。）
- `P` 在允许的根下且链为孤儿 → `MONO_PATH_UNINITIALIZED`，点名 `mega2 path provision` 与 `POST /api/v1/path/provision`；首推与原样重试同一文本。
- `P` 在允许的根下且为 fork 型（链经 pack 到达已知 commit）→ 沿用上表的创建语义。

已有 `main` 行、已能在根树中解析（例如尚未物化的一级根）或存在墓碑的路径不做分类，拒绝与创建规则不变，孤儿推送到这类路径仍得到孤儿拒绝。`root_dirs` 因此是「创建」白名单：新增一级根要改配置并重启，之后只有 fork 型推送能创建它——孤儿推送到新根得到 `MONO_PATH_UNINITIALIZED`，但开通 API 不在 `/` 落地，照提示开通会得到点名 `/` 的 `MONO_PATH_NOT_ALLOWED`（`DEFER-FU-14`）。B3 在锁内对真创建分支（无行且根树不可解析）再分类一次，作为纵深防御，覆盖准入与落地之间根树变化的竞态；这一拒绝经队列失败消息返回，`finalize_trunk_push` 把它还原为类型化的路径策略错误，`ng` 行同样以码开头。

已有行的写入走同一条合成规则——`parent` 取该 ref 自身的旧 tip，`tree` 取该层的新子树——差别仅在新子树从哪里来。两个例外：N = 1 的被推路径（客户端 commit 的 tree 恰好就是该层所需的 tree，无需合成，直接 fast-forward）与被推路径无已物化行的创建语义（上表首行）。祖先方向的合成已经实现（事实校准 10），后代方向是阶段 2 的内容，被推路径的 N > 1 分支是阶段 4 的内容。

**N 的定义**：N = 从 `cmd.new_id` 沿第一父链回溯到 `cmd.old_id`（**客户端声明的基线**）的步长——**只由客户端意图决定，服务端已知性完全不参与**（被拒重试的对象都已持久化，按已知性计数会把 N>1 重试错算成 N=0 而 fast-forward，违反 ADR-TP-12；`fork_base` 作为对象边界单独记录，不混入 N）。N 在 B0 一次算出、随描述符持久化（1.3），重试不重算。N 只决定写入模型的 squash 分流，**不携带落地与否的语义**。

**N = 0 当且仅当 `old_id == new_id`**（退化重推：客户端声明把 ref 置于其现值）。空 pack 与对象已知的非空 pack **不是** N=0——N 锚定客户端基线段长、随描述符持久化，重试不重算（否则被拒重试会退化为 fast-forward，违反 ADR-TP-12）。N=0 轮次照常入队，由 B3 权威闸门裁决：tip 吻合 → 无变更落地（行 `Done`）；不符 → non-fast-forward 拒绝。B3 的分流因此覆盖 N ≥ 0，N 只决定被推路径的落地形态。

**「受影响的层」**：某一层的新子树 hash 不等于其现有 `ref_tree_hash`，该层即为受影响。被推路径是唯一例外——它以 tip 是否变化为条件，因为历史改变了就必须记录，即便树没变。下文所有关于「每层前进一个 commit」的表述，一律限定在受影响的层上。

**推进条件按层区分**：被推路径以「tip 是否变化」为条件，祖先与后代以「树是否变化」为条件。一次净零变更的推送（例如末位 commit 撤销了首位 commit 的改动）只推进 `main@P`，根树与所有祖先纹丝不动，不会产生 tree 与 parent 相同的空 commit。

## 决策记录（ADR-TP）

本节记录本计划中已定的架构决策。每条给出决策、理由、被否方案与影响。任何实现偏离都必须重新评审（`general.md` 硬约束条款）。

### 队列约束

**ADR-TP-01：全局单写入者，不做子树并行**

- **决策**：任意时刻至多一个操作处于 B 段；不按子树划分并行写入域。
- **理由**：每一次推送都要重算从被推路径到 `/` 的整条树脊并推进 `/` 的 `main`，所有推送在根节点上完全冲突。按子树划分并行域无法避开这个共同节点。
- **Alternatives considered**：乐观 CAS + 重试（拒绝——冲突率 100%，重试退化为忙等，且失败语义与真正的 non-fast-forward 混淆）；按子树分片加锁（拒绝——所有分片仍在 `/` 上汇合，分片只增加死锁面）。
- **影响**：写入吞吐上界由 B 段时长决定。扩展路径是阶段 6 的 group commit（批量摊薄），不是放弃序列化。

**ADR-TP-02：队列基座选 Postgres，不选 Redis**

- **决策**：`push_queue` 表 + Postgres 锁原语；Redis 侧不新增队列设施。
- **理由**：B 段本身是 Postgres 事务，锁与数据同源，不存在锁存储与数据存储之间的裂缝；队列的暂停、排空、位置查询与事后审计需要持久化行；`merge_queue` 已提供同形状的四层实现可供泛化。
- **Alternatives considered**：Redis list / stream 做 FIFO（拒绝——队列状态仍须落 Postgres 供查询与审计，形成两份真相）；复用现有 `RedLock`（拒绝——见 ADR-TP-03）。
- **影响**：Redis 仍承担 git object cache 与既有 RedLock 用途（vault 签名 key 首次初始化等），不再承担写入序列化职责。

**ADR-TP-03：互斥用事务级 advisory lock，不用会话级 advisory lock，也不用 RedLock**

- **决策**：`pg_advisory_xact_lock(MONO_WRITE_LOCK)`，在 B3 事务内获取，随提交或回滚自动释放。
- **理由**：无 TTL，不会在长事务中途失效；连接断开自动释放，进程崩溃不留死锁，且事务同时回滚，不存在部分状态。
- **Alternatives considered**：会话级 `pg_advisory_lock`（拒绝——要求 acquire 与 release 落在同一条连接上，在连接池下需要 pin 连接，增加泄漏面）；`RedLock`（拒绝——固定 TTL 30000ms，续期任务停顿时会在事务写入中途丢锁；且锁与数据跨系统，裂缝不可根除）。
- **影响**：现有 ImportRepo attach 路径的 `RedLock`（`import_repo.rs:513`）退场，改由队列闸门覆盖。

**ADR-TP-04：推送同步阻塞等待轮次，队列不是异步作业队列**

- **决策**：receive-pack 连接在 B2 阻塞等待自己的轮次，B3 执行完成后才返回 report-status，返回的是真实结果。
- **理由**：Git 的 report-status 是推送成败的唯一告知渠道。若入队即返回成功，客户端会认为 `main` 已推进而实际尚未执行，后续失败无处告知。
- **Alternatives considered**：入队即返回 accepted，异步执行（拒绝——破坏 Git 推送语义，客户端无法得知最终结果）。
- **影响**：等待期间连接保持占用，因此必须有背压（ADR-TP-08）。排队期间的实时进度反馈属阶段 6 可选项。

**ADR-TP-05：闸门粒度是「根树写入者」，`merge_queue` 被吸收而非并存**

- **决策**：`MonoWriteQueue` 覆盖全部根树写入者；现有 `merge_queue` 泛化为其一部分，不保留为独立队列。
- **理由**：两个队列各管一部分写入者等于没有全序；`merge_queue` 现有的进程内互斥、非原子 claim 与时间戳序号（事实校准 6）均无法提供跨实例的全序。
- **Alternatives considered**：保留 `merge_queue` 只管 CL 合并、新建队列只管推送（拒绝——两条路径都改根树，无全序）；仅给 `merge_queue` 打补丁而不扩大覆盖（拒绝——推送与 ImportRepo attach 仍在闸门外）。
- **影响**：UN-17（legacy 行的执行判定）、UN-18（requester）、UN-25（authz freeze）的语义须逐条对齐迁移，作为阶段 1 的评审门；`merge_queue` 的 HTTP 表面需保留或明确迁移路径。**全部 merge 入口（直连 `/merge`、`/merge-no-auth`、`/merge-queue/add`）统一入队**（1.9 2a）——直连调用在队列外执行 `merge_cl_unchecked` 是未覆盖的根树写入者，破坏 ADR-TP-09 的前提。**吸收是语义保持的重放，不是换一张表**——现 processor 的执行期行为必须逐条映射：冲突失败重排队到队尾（`mono_api_service.rs:4419-4429`）在新模型中表现为「关闭当前队列行（`Cancelled`，`failure_type=Conflict`）并为同一 CL **重新入队一个新行**（新 id 落到队尾）」，而不是原地改 `position`——`bigserial` id 是权威序号，不可改写（ADR-TP-06）；CL 存在性、UN-19 授权、冲突与 GPG 门（`:4465-4603`，GPG `:4599`）在 B3 执行期内原样重查。attach 被吸收后，其 `MAX_ATTACH_ATTEMPTS = 64` 的 CAS 重试循环**必须删除**（事实校准 4）——队列下陈旧根读取不可能发生，残留的重试会把 ADR-TP-09 依赖的绕过信号静默吞掉。

**ADR-TP-06：FIFO 序号取 `bigserial`，不取时间戳**

- **决策**：`push_queue.id` 为 `bigserial`，充当**全局队列操作**的权威序号（push、merge、attach 三类行共用同一 id 序列）；trunk commit 按它保序，但不与它一一对应。
- **理由**：单调、无并列、不受时钟回拨影响；trunk 上 roll-up 的先后顺序须与该序号保序（不变式 I6）。**保序不等于双射**：净零推送（ADR-TP-16）照常入队并占用一个 id，却不在根上产生 roll-up，因此 id 序列存在空洞。空洞不影响保序——剩余的 roll-up 仍严格随 id 递增——但它意味着 id 不是 trunk commit 的序号，措辞上不可混用。
- **Alternatives considered**：沿用 `merge_queue` 的毫秒时间戳 `position`（拒绝——同毫秒入队并列，顺序不确定，时钟回拨会插队）。
- **影响**：`position` 若保留则降级为展示用的排名快照，不参与排序判定。空洞的来源不限于净零推送——B1/B4 事务中止与崩溃同样消耗 `bigserial` 值；**已提交的 id 保持 FIFO 序**，空洞一般性允许（I6 的保序不受影响）。

**ADR-TP-07：轮次失败不冻结队列**

- **决策**：B4 将该项标 `Failed` 并立即让出轮次，队列继续处理后续项。
- **理由**：单次推送失败（non-fast-forward、链校验不通过）是常规事件，不是系统故障；沿用 `plan-20260827` 对 `MergeFailure` 不 freeze 的既有先例，保持运维心智一致。
- **Alternatives considered**：失败即暂停队列待人工介入（拒绝——把常规拒绝升级为服务中断）。
- **影响**：需要独立的失败率指标与告警阈值，避免持续失败被淹没。冻结仅保留给 UN-25 的 authz 场景。

**ADR-TP-08：背压用队列深度上限加等待超时，超出直接拒绝推送**

- **决策**：入队前检查深度上限，超出则拒绝且不入队；`wait_timeout` 只约束**调用方的 B2 等待**——到期即放弃（连接侧拒绝/503），**不取消共享行**（多收养者下取消会误杀他人，孤儿行的清理由 heartbeat reaper 兜底）；已认领进入 B3 的轮次不受其截断，调用方等待真实终态（见 1.9 2a 与 B2）。
- **理由**：ADR-TP-04 决定了排队占用连接，无界队列会耗尽连接与 receive-pack 并发额度。Git 客户端对推送被拒有成熟的处理方式（重推），对连接挂死没有。
- **Alternatives considered**：无界排队（拒绝——连接耗尽）；排队溢出转异步（拒绝——违反 ADR-TP-04）。
- **影响**：上限与超时须可配；拒绝信息必须可诊断并提示重试。

**ADR-TP-09：根 ref CAS 是断言，失败不重试**

- **决策**：B3 的根 ref 更新携带旧值条件；影响行数为 0 时回滚并 fail-closed 告警，不进入重试。
- **理由**：队列成立时该条件恒应满足。失败只可能意味着存在绕过 `MonoWriteQueue` 的写入者，属缺陷而非并发；重试会掩盖缺陷并可能把错误状态写入。
- **Alternatives considered**：CAS 失败后重试（拒绝——那是 ADR-TP-01 已否决的乐观并发模型，且会掩盖闸门缺口）。
- **影响**：把「队列是否覆盖全部写入者」从代码审查事项变为运行时可检测性质（不变式 I5）；须配备断言失败计数指标与一条故意绕过队列的负向测试。**CAS 是 tripwire，不是完备性证明**：一个在队列事务快照建立**之前**完成提交的未登记写入者可以不被它发现——完备性的第一依据是 1.1 的写入者审计（人工穷举 + 统一入口收敛），CAS 只是审计遗漏时的 fail-closed 兜底，二者缺一不可，措辞上不得把「CAS 恒成功」等同于「不存在未登记写入者」。

**ADR-TP-10：同一路径同时只允许一项在队**

- **决策**：`push_queue` 上建 `status in ('Queued','Running')` 的路径唯一部分索引，**仅约束 `kind = 'push'` 的行**；同路径第二次推送入队直接拒绝。merge 与 attach 行不适用该索引——它们的正确性由队列本身的全序与 B3 锁内重查保证（merge 的落地 parent 按现状取 `refs/cl/<link>` 行的 tip、命名分歧时落 main head（GAP-07），执行时点重读，天然吸收并发推进，见 1.9），同路径排队的 merge 也不会「必然失败」。
- **理由**：同路径的第二项**推送**在 B3 的权威 non-fast-forward 闸门上几乎必然失败，占位只是浪费队列深度与等待时间（保守序列化：N=0 的合法 no-op 重推理论上可在前者落地后成功，但该情形已被指纹回放与入队收养覆盖，不值得为其放开排队额度）；该论证由推送语义导出，对 merge/attach 不成立（Claude R1 #2：merge 落地在现状下本就取当前 tip 为 parent，无陈旧基线问题；其既有入口预检与执行期冲突重查原样保留，见 1.9 第 3 条）。
- **Alternatives considered**：允许同路径多项排队（拒绝——对 push 而言确定会失败的项占用背压额度）；索引覆盖全部 kind（**已否决**——会使今天能成功的并发 merge 在入队即被拒，违反硬约束 8）。
- **影响**：并发推送同一路径时，第二个客户端立即得到明确拒绝而非排队后失败。

**ADR-TP-11：文件路径索引重建移出临界区，接受最终一致**

- **决策**：`traverses_tree_and_update_filepath` 与 authz notify 在 B3 事务提交后执行；索引任务带 `push_queue.id` 保护，序号较小者发现该路径已被更大序号索引过即跳过。
- **理由**：索引重建成本与子树规模成正比，留在临界区会让大子树推送长时间独占全局写入闸门（硬约束 7）。
- **Alternatives considered**：保留在临界区内（拒绝——吞吐不可预测）；改为增量索引后留在临界区内（拒绝——增量正确性依赖树 diff，复杂度与收益不匹配，可作为后续独立议题）。
- **影响**：Web 浏览与挂载消费方的路径查询在推送提交后短暂落后；须提供索引补偿任务；用户文档应说明该索引的最终一致性。序号保护需要新的持久化状态——现状 `mega_blob` 只有 `file_path` 一列（`src/callisto/mega_blob.rs:6-22`），`update_blob_filepath`（`mono_storage.rs:335-352`）是无条件覆盖。交付物：文件路径索引改造为**出现对表** `blob_paths(blob_id, path, indexed_push_id)`（现状 `mega_blob` 每 blob 单 `file_path` 列，`callisto/mega_blob.rs:6-22`，无法表达同一 blob 的多路径出现，而 `traverses_tree_and_update_filepath`（`monorepo.rs:1051-1114`）会遇到；按 `(blob_id, path)` 出现对做 CAS 更新与**精确删除**——删除感知的 reconciliation 只清「本子树内上一代存在、本代不存在」的出现对，不影响该 blob 在其他路径的引用；`mega_blob.file_path` 降级为展示用最新路径或废弃。**已知限制（如实声明，不夸大）**：出现对模型仍允许旧任务把已删除的 `(blob_id, path)` 重插（删除本身没有代际水位，旧任务既无行可比对也无删除标记）——「永不复活」**不被本计划承诺**；收敛是最终一致的：补偿任务周期重扫会把重插行再次清除。持久化删除水位/墓碑列为后续独立议题。配「重插后补偿收敛」回归（而非「永不复活」断言）。**水位必须按行而非按任务**：一次推送的索引任务会写整个子树的行（`traverses_tree_and_update_filepath` 自 tip 递归），若只按触发路径记水位，`/a`（id 5）的滞后任务可以覆盖 `/a/b` 已被 id 6 索引过的行——受保护路径上没有更大的 id，逐行 CAS 是唯一能挡住嵌套覆盖的粒度。**行级 CAS 挡不住删除复活**：索引任务只更新新树中仍存在的 blob（`monorepo.rs:1051-1114`），被新推送删除的 blob 没有新行可写水位，旧任务可以把它的旧路径重新写回。交付物补**删除感知的按代 reconciliation**：索引任务以「本次 tip 子树的全集」为界，除 CAS 更新仍存在的行外，必须清除「上一代索引中存在而新子树中不存在」的 file_path 行（子树范围 diff 清理），使删除与更新以同一代际收敛。补偿任务仍保留，作为 CAS 与 reconciliation 之外的收敛兜底。**版本来源按形态分离（含隔离规则；对 review 形态这是一处登记在案的索引行为变化**——队列轮次打上水位后，其后 review 推送的索引不再无条件覆盖这些行；该索引不在写入判定链上（ADR-TP-11 的原有论据），变化属有意设计，与硬约束 8 的两类豁免并列）：`indexed_push_id` 水位只由队列路径（trunk push 的 C 段、merge/attach 轮次）写入；review 形态的推送不入队，其索引仍走现状同步调用（`run_mono_post_push_pipeline` → `traverses_tree_and_update_filepath`，`monorepo.rs:1012`）。**隔离规则**：`indexed_push_id` 可空；review 更新携带 `WHERE indexed_push_id IS NULL`（只写从未被队列索引过的行，不得覆盖队列已索引行）；队列更新携带 `WHERE indexed_push_id IS NULL OR indexed_push_id < $id`（可覆盖 review 行与更旧队列行）——否则 review 推送的无版本更新会击穿行级 CAS。

### 多 commit 推送

**ADR-TP-12：按 N 分流——N = 1 原样落地，N > 1 在被推路径合并为一个**

- **决策**：`N = 1` 时 `main@P = cmd.new_id`，客户端 commit 对象原样落地，不合成；`N > 1` 时 `main@P` 前进**一个**合成的 squash commit（`parent` = 该路径旧 tip，`tree` = `chain.tip.tree_id`）。祖先与根在两种情况下均只前进一个 roll-up（树未变的层不前进，见 ADR-TP-16）。
- **理由**：主要使用方是 Agent，其常态是每 commit 一推（需求前提），因此 N = 1 是热路径，必须零成本——保住对象 hash 与原始 GPG 签名，且客户端推送后无需任何重新对齐。N > 1 是 Agent 攒批推送的例外，那些中间状态不应进入 `main`；合并为一个使 `main` 的每一步都对应一次完整的推送意图。中间 commit 的 tree 是被推路径的子树，与根 commit 所需的根树形状不同，本就无法逐一嫁接到根（硬约束 3）。
- **Alternatives considered**：N > 1 时也全量保留（拒绝——Agent 的中间状态进入 `main`，与需求前提第 2 条相悖，且 `main@P` 与祖先的粒度长期不一致）；无论 N 一律 squash（拒绝——热路径 N = 1 白白损失对象 hash 与签名，且强制每次推送后重新对齐，收益为零）；逐 commit 重放为 N 个各层合成 commit（拒绝——N 倍树重算成本，hash 仍与客户端不同，不换来任何保真收益）。
- **影响**：`main@P` 与祖先的历史粒度**恒定一致**——每次推送在每个受影响的层上恰好前进一个 commit（N = 1 时该 commit 就是客户端对象本身；净零推送时受影响的层只有 `P`，见 ADR-TP-16）。原有的「两个 bisect 粒度」随之消失，bisect 在任一层的最小步长都是一次推送。N > 1 时客户端本地历史与服务端发散，处理方式见 ADR-TP-18。根上只出现推送原子态这一性质不变，并在本文档中记录。

**ADR-TP-13：roll-up 的作者身份取链上 tip，其余作者进 `Co-authored-by`**

- **决策**：author 与 committer 身份取 `chain.tip` 的对应字段；链上其余不同作者以 `Co-authored-by:` trailer 逐一列出。
- **理由**：tip 是该批工作的完成点；`Co-authored-by` 是 Git 生态既有的多作者表达，工具链可识别。
- **Alternatives considered**：取链上第一个 commit 的 author（拒绝——与「谁完成了这批工作」的语义不符）；取推送者身份（拒绝——trunk 形态下不接入用户系统，推送者身份只有 token 名，不是可归属的自然人）。
- **影响**：归属信息不因合并而丢失；单 commit 推送时退化为原样搬运。

**ADR-TP-14：roll-up 的 committer date 取落地时刻，author date 取原值**

- **决策**：committer date = `max(该轮次的执行时刻, 前一 trunk commit 的 committer date)`——max 规则在同一 B3 锁内取前值，墙钟回拨不会让根上时间倒流（单调性与 I6 同源）；author date = tip 的原值。
- **理由**：trunk 时间线的顺序必须与 `push_queue.id` 保序（不变式 I6）。保留原始 committer date 会在有人推送本地积压 commit 时使根上的 `git log` 出现时间倒流，与队列全序矛盾；仅取「执行时刻」在 NTP 回拨下同样可能倒流，max 规则以一个额外的时间戳读取换取无条件的单调不减。author date 不参与默认排序，保留原值不产生该问题。
- **Alternatives considered**：两个时间戳都原样搬运（拒绝——根上时间倒流）；两个都取落地时刻（拒绝——丢失「工作何时完成」这一信息）。
- **影响**：若链上 author date 跨度较大，补 `Mono-Author-Date-Range` trailer 记录区间。

**ADR-TP-15：squash commit 的 message 必须完整列出全部被合并 commit，不截断**

- **决策**：N > 1 时**被推路径**的 squash commit，其 message 必须包含一段说明合并行为的自然语言，以及**全部 N 条**被合并 commit 的逐条枚举（commit id、author 署名、author date、subject），不设条数上限、不截断。祖先与后代的合成 commit **不重复枚举**，改带 `Mono-Squash-Commit` 指向被推路径那一个。`Mono-Commits` trailer 移除（与正文枚举重复）；`Mono-Squash-Range` 与 `Mono-Squash-Count` 保留为机器可读锚点。provenance 的持久权威记录是 `push_queue` 行的 `(old_id, new_id)` 区间加对象库中的 commit 对象。
- **理由**：合并是服务端单方面对客户端历史做出的改写，使用方必须能够知道这次行为发生了、合并了什么。把这件事放在 message 里是唯一无需额外工具、无需查文档、`git log` / `git show` 直接可见的告知方式。一旦允许截断，「被截掉的那部分」恰恰是使用方最需要而最不容易找回的信息；而 provenance 的完整性押在 message 体积阈值上，阈值一调整就失效。ADR-TP-12 改为 N > 1 时被推路径也合并之后，原始 N 个 commit 不再被任何 ref 引用，message 与 `push_queue` 行是仅有的两条线索，二者都不能残缺。
- **Alternatives considered**：正文枚举封顶 K 条、超出以剩余数收尾（**已否决**——违背「让用户知道这次行为」的目的，且完整性依赖可调阈值）；只写 `Mono-Squash-Range` 不逐条枚举（拒绝——读者必须另行遍历对象库才知道合并了什么，`git show` 看不出所以然）；把每个被合并 commit 的完整 message 正文也复制进来（拒绝——体积随正文长度无界增长，而 subject 加署名已足以判断内容，完整正文可经 range 取回）；新增 `push_commits` 列表表（拒绝——`push_queue` 的区间已足够还原，除非出现按 commit 维度查询的真实需求）。
- **影响**：message 体积由 `max_push_commits`（ADR-TP-17）直接决定——按每条约 100 字节估算，默认上限 250 对应约 25 KB message，调高链长上限会成比例放大。**每次推送只产生一份**，不随已物化层数放大（这是「祖先不重复枚举」那半条决策的作用；若各层都带，体积会是枚举 × 层数，而层数随只读操作单调增长）。**该放大属于已接受的代价**：mega2 的定位就是超大仓库处理，一个 25 KB 量级的 commit message 相对于它承载的对象规模可以忽略，而 provenance 的完整性不可替代。Git 对 message 长度无实际限制，`git log --oneline` 只取 subject 行，日常浏览不受影响；受影响的只是 `git show` 的输出长度。不变式 I4 相应改写。若后续引入 commit GC，必须把 `push_queue` 各行的 `(old_id, new_id)` 区间所锚定的 commit 链视为 GC root——`clean_dangling_commits`（`mono_api_service.rs` 中的 TODO 注释态）在实现前必须先满足这一条。

**ADR-TP-15a：逐 commit 的 GPG 签名验证不作为受支持的能力**

- **决策**：mega2 **不承诺**「原始 commit 的 GPG 签名始终可从 ref 上验证」。N > 1 时被推路径的 squash commit 由服务端以自己的 GPG 密钥签名（沿用 MC-09 的 `ServerSigningContext`）；原始逐 commit 签名不再挂在任何 ref 上。N = 1 时客户端签名原样保留，但这是零改写路径的自然结果，**不是一项可被依赖的保证**，部署方不得据此设计签名验证流程。
- **理由**：签名覆盖 commit 对象全文，合并必然产生新对象，逐 commit 签名在合并语义下不可能保留。与其提供一个「只在 N = 1 时成立」的条件性保证——它会诱导部署方去约束使用方的推送方式，把一个服务端的实现约束转嫁成使用方的操作纪律——不如明确不支持。使用方需要知道的是「这次合并包含了什么」，那由 ADR-TP-15 的完整 message 回答；需要知道「这个 commit 是谁落的」，那由服务端签名回答。
- **Alternatives considered**：约束使用方只走 N = 1 推送以保住逐 commit 签名（**已否决**——把服务端约束转嫁给使用方，且与 Agent 攒批推送的现实用法冲突）；在 squash commit 中嵌入原始签名块（拒绝——签名与其覆盖的对象不匹配，是伪造可验证性，比不提供更糟）。
- **影响**：风险与约束的「约束 2」按此改写。签名验证的对象是服务端密钥，验证的语义是「这次落地由该 mega2 实例执行」，而不是「这些内容由某个作者签署」。



**ADR-TP-16：净零变更的推送只推进被推路径**

- **决策**：推进条件按层区分——被推路径以 tip 是否变化为条件，祖先与后代以树是否变化为条件。
- **理由**：N 个 commit 净变更为零时（例如末位撤销首位），根树内容确实未变，插入 tree 与 parent 相同的合成 commit 只会污染 trunk 时间线；而路径层的历史确实变了，必须记录。
- **Alternatives considered**：无条件在所有层推进（拒绝——产生语义为空的 trunk commit）；净零时整体拒绝推送（拒绝——客户端的 commit 是合法历史，无理由拒收）。
- **影响**：与阶段 2 后代推进的「子树未变则跳过」是同一条谓词，实现上共用判定。

**ADR-TP-17：拒绝链内 merge commit；链长上限改为可配**

- **决策**：沿用 MC-03 对 merge commit 的拒绝。review 形态**保留** `MAX_CL_CHAIN_COMMITS = 250` 常量不动（其唯一含义由 ADR-MC-07 定义——CL 的 `(from_hash → to_hash)` 累积范围上界，GPG checker 只是纵深防御，见 `src/ceres/merge_checker/mod.rs:20-27` 的注释）；trunk 形态引入独立配置 `[monorepo].max_push_commits`，默认 250，**只作用于 trunk 推送路径的 B0 校验**。
- **理由**：线性链是 trunk-based development 的前提，也使枚举顺序确定（`ordered_commits` 为 tip 在前，枚举时反转为拓扑升序）。trunk 推送不再经过 CL 的逐 commit 验签链路，需要一个独立于 ADR-MC-07 语义的上限；但上限仍然必需，且比原来更吃重——ADR-TP-15 要求 message 完整枚举不截断，**`max_push_commits` 于是直接决定 message 体积上界**，二者是一组联动配置，调整其一必须同时评估另一项。将其做成 trunk-only 是硬约束 8 的要求：让 ADR-MC-07 的 CL 不变量在 review 形态下变成可配置项，是阶段 1–3 禁止的可观察行为变化。
- **Alternatives considered**：trunk 形态下取消上限（**已否决**——B 段时长失去上界，全局写入闸门的可预测性随之丧失）；为压缩 message 体积而调低上限（**已否决**——message 放大是已接受的代价，见下）；trunk 形态下接受 merge commit（拒绝——枚举顺序不再确定，且与唯一公开分支的产品规则相悖）；直接把 `MAX_CL_CHAIN_COMMITS` 改为可配（**已否决**——见决策段，违反硬约束 8）。
- **影响**：首次导入大量历史的场景需要显式调高该配置，或分批推送。**兑现「可调高」需要参数化现有校验**——`PushChain::resolve` 与 `validate` 的链长上界现硬编码引用 `MAX_CL_CHAIN_COMMITS`（`push_chain.rs:182-205, 408-447`），trunk 路径必须将其改为入参（review 路径传常量、trunk 路径传配置值），列入阶段 3/4 交付物；未参数化前该配置不得宣称可调。**该配置的取值依据是 B 段时长与推送批量的实际需要，message 体积不参与该决策**——mega2 面向超大仓库，25 KB 量级的 message 相对其承载的对象规模可以忽略，完整 provenance 的价值高于体积。

**ADR-TP-18：N > 1 推送后客户端必须重新对齐，服务端负责把这一点说清楚**

- **决策**：N > 1 的推送被接受后，`main@P` 是服务端合成的 squash commit，与客户端本地 tip 不同。客户端下一次推送的 `old_id` 将不等于服务端 tip，会被 non-fast-forward 闸门拒绝。约定的对齐动作是 `git fetch && git reset --hard origin/main`；服务端在两处主动告知：推送成功时经 sideband 返回合成后的 commit id，以及在 non-fast-forward 拒绝信息中直接给出该对齐命令。
- **理由**：Git 的 receive-pack 没有「服务端把 ref 落到另一个值」的协商机制——客户端会把远程跟踪引用乐观地更新为自己推送的值，发散要到下一次 fetch 才暴露。与其让使用方撞上一次莫名其妙的拒绝，不如在推送成功的当次就告知，并让拒绝信息可直接照做。对 Agent 而言这是一条可脚本化的固定动作，且在 N = 1 的常态路径上是 no-op（服务端 tip 就等于本地 tip），因此「每次推送后执行对齐」可以无条件写进 Agent 的工作流。
- **Alternatives considered**：服务端记住「上次接受的客户端 tip」并容忍以它为基的后续推送（拒绝——服务端要长期维护一份与自身历史平行的客户端谱系，客户端历史与服务端永久发散且越差越远，排障成本远高于一条 reset）；N > 1 时拒绝推送并要求客户端自己先 squash（拒绝——把服务端能自动完成的事推给每一个使用方，且与需求前提第 2 条相悖）。
- **影响**：Agent 侧工作流固定为「commit → push → fetch + reset」。该约定须写入[使用指南](../user-guide.zh.md)与部署文档；non-fast-forward 拒绝信息的措辞属阶段 4 的验收项。

**ADR-TP-19：后代 ref 恒与 B3 同事务推进；I3 保持强一致，不分层**

- **决策**：后代 ref 的推进始终在 B3 的同一事务内完成（阶段 2.7）。不变式 I3 保持强一致，**不**拆成「根树与被推路径强一致 + 其余最终一致」。惰性后代推进（原阶段 6 候选项）**否决**。
- **理由**：I3 约束的不是一份展示视图，而是**参与写入判定的状态**。B3 的 non-fast-forward 闸门比对的正是 `main@P` 的 `ref_commit_hash`，闸门的正确性直接依赖该行是最新的。一旦后代 ref 允许滞后，下述失效链成立且**无任何报错**：

  ```
  1. /project/foo 在 T1，子树 S1；Agent 本地也是 T1
  2. 有人在 /project 推送，往 /project/foo 里加了一个文件 → 根树该处变为 S2
  3. 惰性：main@/project/foo 仍是 T1/S1，只打陈旧标记
  4. Agent 未 fetch 直接推送，old_id = T1；闸门读到 T1 → 相等 → 放行
  5. B3 把基于 S1 算出的新子树嫁接进根树
     → 第 2 步加入的文件从根树消失，静默丢失
  ```

  在同事务推进下，第 3 步即把该行推进到 T2/S2，第 4 步闸门看到 T2 ≠ T1 按 non-fast-forward 拒绝，Agent 重新对齐后再推——正确。要让惰性方案安全，**闸门本身也必须先补齐再比对**，而闸门位于 B3 事务内、持着全局写锁，补齐意味着把树解析与 commit 合成搬回临界区——这恰恰是惰性方案想省掉的工作，收益被自身要求抵消。

  第二重理由是覆盖面：git 广告路径（`monorepo.rs:121`）之外，直接读取路径级 `main@P` 的地方至少还有十处——`code_edit/utils.rs:64/:128/:351/:365`、`api/router/tag_router.rs:74`、`merge_checker/cl_sync_checker.rs:50`、`mono_api_service.rs:1280/:2115/:3274/:3307/:4055/:4637`。惰性方案要求每一处都先补齐再读，漏一处即是一个静默的错误读取点，且新增读点时没有任何机制提醒。

- **Alternatives considered**：
  - **惰性后代推进**（否决——上述闸门失效链与覆盖面问题；曾作为阶段 6 候选项，本 ADR 撤销之）。
  - **分层 I3**（根树与被推路径强一致、其余最终一致）（否决——分层的前提是「其余」不参与写入判定，而后代 ref 恰恰是闸门的比对对象；ADR-TP-11 对 file_path 索引接受最终一致是成立的，因为那份数据不在任何写入判定链上，也没有客户端在其上叠加提交，二者不可类比）。
  - **物化 TTL + 墓碑**（**采纳**为阶段 6 候选项，见阶段 6 第 4 项——它命中同一个成本来源，却不触碰任何不变式）。
- **影响**：阶段 2.7 的同事务硬要求保持不变。B3 中后代处理的成本以「`P` 下已物化的后代数」为界，并被 Merkle hash 比对剪枝（子树未变即跳过）。该成本的真实增长来源是**物化只增不减**（一次 `ls-remote` 即可造成，见写入模型一节），应由阶段 6 的物化 TTL 处理，而不是靠推迟工作。

**ADR-TP-20：惰性物化不入队，由「插入时根快照校验 + B3 树哈希断言与修复」双层覆盖**

- **决策**：两条惰性物化路径（事实校准 15，advertise/clone 上的 `refs_with_head_hash` 与 code_edit 的 `create_repo_commit`）**不进入 `MonoWriteQueue`**——它们在读路径上，入队会让每次冷 `ls-remote`/`clone` 排在全部写入之后。取而代之的两层保护：
  1. **物化插入校验根快照新鲜度**：树遍历在锁外完成，但遍历必须记录其出发的根 ref **身份对**（`ref_commit_hash` 与 `ref_tree_hash`，与 B3/reaper 的判定同构——tree-only 的绕过不会被 commit hash 比对漏过）；随后的「墓碑检查 + ref 行插入」在一个持有 `pg_advisory_xact_lock(MONO_WRITE_LOCK)` 的事务内完成（该事务只做几次行读写，毫秒级），事务内**重读根 ref 并与遍历出发时的身份对比对**——一致才插入（携带 `WHERE NOT EXISTS` 守卫），不一致说明遍历基于已被推进的旧根，**本次插入放弃**。**「放弃」必须有协议层的落点，不能静默**：现状 `git_info_refs`/v2 把 `(ZERO_ID, 空 refs)` 编码为 capabilities-only（`smart.rs:76-88`、`v2.rs:67-87`），客户端会把远端当空仓库且不会自动重试。交付物：物化入口（`refs_with_head_hash`/`create_repo_commit`）在放弃插入时执行**服务端有界重试**（重新走一遍「遍历 → 校验 → 插入」循环，默认 K=2，间隔退避）；重试**必须绕过 `heads_exist` 快路径**（`monorepo.rs:129-135` 与 `utils.rs:367-375` 在行存在时直接返回该行，不重走校验会让放弃-重试原地拿回同一陈旧行）；重试仍失败则向调用方返回可诊断错误，**绝不因放弃插入而以空 refs 降级**（注意区分：路径根本不在根树中时返回 `(ZERO_ID, 空 refs)` 是正确答案——既有行为，且为 2.5「路径不在根树 → 跳过物化」所要求；本条禁的是**放弃插入被伪装成空仓库**）。错误传播需要配套改造：`RepoHandler::refs_with_head_hash` 现为不可失败签名（`src/ceres/pack/mod.rs:90`），需改为可失败，两个实现（`monorepo.rs` 与 `import_repo.rs:84-94`）及其调用点（含 `import_repo.rs:392` 的 `.await.0` 用法）同步修改，并在 `smart.rs`/`v2.rs` 传播；`ProtocolError` 现把 `MegaError` 映射为 HTTP 400（`src/common/errors/mod.rs:200-216`），advertise 失败需映射为 5xx——使失败对用户与运维可见，而非被伪装成空仓库。带 `NOT EXISTS` 守卫的插入发生在持锁事务内，不存在与 B3 的行级交叉。**插入失去竞态的返回语义**：`NOT EXISTS` 命中既有行（如一次净零推送在根 hash 不变的情况下推进了 `main@P`）时，物化**必须重读持久化行并返回它**，与自身预计算不一致时回到重试循环——把自己的预计算 ref 返回给调用方会把陈旧视图暴露给客户端。配回归。
  2. **B3 闸门追加树哈希断言与修复（push 与 merge 分支同谓词；只覆盖 `refs/heads/main` 行——两条物化路径经 `get_all_refs("/", true)` 也会为 tag ref 生成行，tag 行不参与写入判定、由 tag API 独立管理，其出现属既有行为，登记在审计清单「范围外」，不在墓碑/断言覆盖之列）**：B3 **消费或覆写** `main@P` 行的分支（push 的基线比对、merge 覆写 `main@P` 前的行状态）除比对 `main@P.ref_commit_hash` 外，**追加断言 `main@P.ref_tree_hash == resolve(root_in_txn, P)`**——从锁内根树解析 `P` 的子树哈希，与 ref 行记录的 tree 比对。不等即拒绝该轮次，修复经 **SAVEPOINT 同事务**完成（撤数据写、写墓碑、删行、标终态并清除意图）。崩溃恢复**不依赖任何修复意图**：`expected_*` 基线在 B2.5 认领事务内原子落库，统一规则是 **reaper 对任何待终态化的 `Running` 行先比对根基线（不一致 ⇒ 绕过，硬停），再做该行 `path` 的 I3 校验，陈旧即就地墓碑修复，再终态化**（见 1.6），使全部崩溃窗口闭合；即便遗漏，检测仍可重入——下一次推送再次断言拒绝，且周期性不变式巡检（第 3 项）必然扫到；修复是幂等的，不会双写墓碑。**修复必须续接而不能单纯删除重建**——若只删行，下次物化会合成无父 commit，陈旧 tip 不再是任何新 tip 的祖先，I1 断裂。客户端随后重新 advertise，从墓碑续接物化出与当前根树一致的行，再推/再合成功。merge 分支同样需要断言的理由见 1.9 第 4 条；**落地 parent 的选取（`refs/cl/<link>` 优先，GAP-07 现状）不受断言影响**——CL ref 行不是根树的派生视图，不参与该断言。
  3. **残余窗口的兜底：对账与巡检（不做夸大声明）**：物化校验读与插入提交之间若恰有**绕过队列的**写入者推进根，插入的行相对新根陈旧——下一次 B3 的根 CAS **不必然**发现它（B3 读到的是绕过者写入后的根，CAS 恒成功；只有恰好在被推路径或其祖先上推送时树哈希断言才会检出）。这类「绕过队列」属 I5 的 fail-closed 事件域，不能靠单点断言穷尽，兜底是三层的：**启用前对账**——开关开启时把存量 `refs/heads/main` 行与当前根树逐一比对，陈旧行转墓碑；**周期性不变式巡检**——后台任务按同一谓词全量比对并修复（复用墓碑续接），频率与告警阈值可配；**断言与巡检的指标计数**。**对账与巡检的「比对 + 修复」都必须在持有 `MONO_WRITE_LOCK` 的事务内分批执行**——否则巡检扫描到根 R0、B3 随后提交 R1，巡检会依据 R0 的比对结果把 R1 下已经合法的 ref 行误删/误墓碑；持锁使比对与修复相对 B3 原子，分批避免长时间独占闸门。I3 的强一致表述因此显式附带前提：**全部根写入者遵守锁纪律**（即 I5 的审计前提）——绕过者是 I5 的失效事件，不是 I3 的正常输入。
- **理由**：物化从**各自读取的根树快照**合成 ref 行，与 B3 并发时会写入一个落后于根树的 `ref_tree_hash`，且不触碰根 ref——ADR-TP-09 的 CAS 断言发现不了它，I5 保持绿色而 I3 已被打破。更糟的是它复现 ADR-TP-19 论证过的那条失效链：陈旧物化的 tip 被客户端当作推送基线，闸门只比 commit hash 就会放行，B3 把基于陈旧子树算出的结果嫁接进根树，祖先推送的改动**静默消失**。仅靠闸门断言还不够——若只拒绝不修复，`refs_with_head_hash` 见行即返（`monorepo.rs:129-135` 的 heads_exist 分支），陈旧行会永远占据该路径，之后每一次推送都被拒绝，形成死局；删除重建（修复）与插入时校验（预防）二者缺一不可。后代方向不需要额外断言——物化的后代行若陈旧，阶段 2 的续接语义（解析新子树、parent 取旧 tip）本身就是自愈。
- **Alternatives considered**：物化也走队列（拒绝——冷读路径被全部写入串行阻塞，`ls-remote` 的延迟不可接受）；只加插入校验、不加闸门断言（拒绝——升级前遗留的陈旧行、或未来新增的物化代码点都绕得过插入校验，闸门是判定链上的最后防线）；只加闸门断言、不加插入校验（拒绝——每次竞态都变成一次被拒推送 + 一次重建，把可预防的问题变成可观测的损耗，且拒绝信息对 Agent 是一次无谓的重试）；删除 I3 的强一致（拒绝——见 ADR-TP-19，那不是取舍而是失效）。
- **影响**：`refs_with_head_hash` 与 `create_repo_commit` 进入 1.1 的写入者审计清单但标注为「队列外、ADR-TP-20 覆盖」；物化签名增加「遍历出发根 hash」的传递；B3 的闸门从单字段比对升级为双字段断言加修复；阶段 1/4 的验收标准相应增加物化竞态与陈旧行修复的回归用例。**I3 保持强一致、不分层**：物化提交的行恒与提交时点的根树一致（插入校验保证），B3 推进的行恒与提交后的根树一致（同事务续接保证），任何提交边界上不存在既非旧值亦非新值的中间态——「不入队」只是说物化不占轮次，不是它豁免一致性。
- **物化 commit 消息**：物化 commit 消息 = 根 commit 正文，不复制签名头。物化 commit 是服务端合成的未签名 commit，消息取遍历出发根 commit 的**正文**（`split_commit_message` 剥离 `gpgsig` / `gpgsig-sha256` 等额外头），并经 `format_commit_msg(body, None)` 以头/体空行成帧（`src/ceres/pack/materialize.rs` 的 `persist_walked_refs`）；否则根 commit 的服务端签名头会原样落进物化 commit，形成对其不可验证的签名。ImportRepo attach 根 commit 同理取被导入 tip 正文的首个非空行（`push_queue_service.rs` 的 `attach_root_message`）。只影响此后新物化的 commit，已物化的历史不改写（[`plan-20260923.md`](../plan/plan-20260923.md) FU-02，issue #28）。

## 迁移步骤（分阶段）

阶段编号：1 – 6（阶段 6 为可选优化）。阶段 1–3 不引入新形态，是对现有写入路径的缺陷修复，**可独立验收**；但**阶段 1 不可独立上线服务生产流量**——其 merge/push 分支沿用的删除式后代处理（`remove_none_cl_refs`）违反不变式 I1（无墓碑删除历史），生产部署以阶段 2 落地为前置条件（阶段 2 起后代续接使 I1 成立）。

---

### 阶段 1 — `MonoWriteQueue`：根树写入的全局序列化

**1.1 写入者审计（交付物）**

穷举并登记全部根树与路径 ref 的写入者，形成清单并在代码中以统一入口收敛。已知写入者按硬约束 2 的三类划分登记；其中惰性物化两条路径（事实校准 15）标注为「队列外、ADR-TP-20 覆盖」，bootstrap（事实校准 15/硬约束 2）标注为「服务前、专属锁」。审计结论必须写回本文档的事实校准部分。

**状态（2026-09-06 / TP-23）**：机器可核对清单见 [`trunk-push-writers-audit.md`](./trunk-push-writers-audit.md)；结论已回写事实校准 18。统一入口收敛属后续实现卡（TP-04/07/08），本交付物覆盖「穷举登记 + 回写」文档面。

**1.2 推送生命周期分段**

| 段 | 内容 | 并发性 |
| --- | --- | --- |
| A | negotiation、pack decode、`save_entry` 写对象与 blob | 并发 |

A 段写入的对象库内容**不在 B3 事务之内**——被拒推送留下的不可达对象是现状即有的垃圾（`monorepo.rs:248-257` 已记录），B3 的原子性承诺只覆盖元数据（ref/commit/tree 行与队列状态），不覆盖对象库回收（对象 GC 的既有议题）。
| B | 准入校验 + 树嫁接 + ref 更新，**单事务** | 独占（队列） |
| C | `traverses_tree_and_update_filepath`、UN-16 authz notify | 并发（事务提交后）* |

C 段移出临界区（硬约束 7）后，文件路径索引变为最终一致；索引保护按 ADR-TP-11 的行级水位实现——索引行带 `indexed_push_id` 列，更新携带 `WHERE indexed_push_id < $id` 的 CAS（一次任务写整个子树的行，只按触发路径记水位挡不住嵌套覆盖）。

\* **例外——authz notify 的屏障与单调发布（`cedar.enforcement != off` 时）**：`notify.rs:27-73` 在提交后**异步重建共享 Cedar snapshot**，若下一轮 B3 的 UN-19 授权重查读到旧 snapshot，授权决策会用陈旧的 `/.mega_cedar.json` 判定；且两个 C 段 notify 并发完成时可能**乱序发布**——旧快照后到会覆盖新快照。enforce 模式下 notify 不参与 C 段并发，交付三件套：
  1. **持久化 outbox**：B3 在同一事务内写 notify outbox 行（`version = push_queue.id`），提交即登记待重建；崩溃后未发布的 outbox 行由补偿任务重放（重建以**最新根**为准，天然幂等）。
  2. **单调 CAS 发布**：快照发布带持久化版本列，`UPDATE ... SET published_version = $v WHERE published_version < $v`——乱序完成者发布被拒，发布序恒等于 push_queue.id 序；当前实现为内存级 best-effort（`notify.rs:35-93`、`entitystore.rs:148-219`），需改造为持久化版本。
  3. **读取屏障（按实例自校）**：`SharedEntityStore` 是进程本地的内存快照（`entitystore.rs:148-220`，每 context 各自初始化），全局持久化的 `published_version` 在实例 A 推进时，实例 B 的陈旧本地快照不会自行更新——屏障必须是**每个 B3 实例的授权重查前自校**：读 DB 权威版本，与本进程内存快照版本比对，落后即**本进程重建快照**（从最新根）再判定，禁止以「本进程快照看起来是新的」代替持久化版本比对；重建期间 fail-closed 等待（带超时与告警）。授权读取不允许滞后。

`cedar.enforcement = off`（含 trunk 形态）时快照不构建不被消费，无屏障需求。**队列外的触发点**（review 形态删除路径的 best-effort notify，`monorepo.rs:903-907`）没有 `push_queue.id` 可作版本——它们不参与单调版本比较，统一改走 dirty 标记 + 补偿重建（以最新根重建，无版本语义，最终一致即可：删除不落地，不存在静默丢失面）。配乱序发布与 notify 前崩溃的回归测试。

**1.3 三层结构与操作判别**

| 层 | 机制 | 回答的问题 |
| --- | --- | --- |
| 顺序 | `push_queue` 表（`bigserial`） | 先后、公平、可观测、可管理 |
| 互斥 | `pg_advisory_xact_lock(MONO_WRITE_LOCK)` | 任意时刻只有一个执行中 |
| 正确性 | 根 ref CAS 断言 + B3 树哈希断言 | 有没有写入者绕过了前两层 |

**队列是序列化闸门与生命周期台账，不是作业执行器**。队列行携带 `kind`（`push|merge|attach`）、公共坐标（`path`/`old_id`/`new_id`/`requester`）与**最小必要载荷 `payload`**——执行所需的确定性输入，使任一收养者（1.11）无需原连接的内存状态即可执行：
- push：**推送描述符** `{commits, fork_base, n}`——链上 commit id 列表（拓扑序）、fork/base 点与 N。不可省略：A 段落库后对象全部在库，对象存在性**无法**区分 N=1 与 N>1、也无法定位 fork 点（`PushChain` 需要 `new_commit_ids` 全集，`push_chain.rs:82-85/262-288`）；描述符在 B0 由客户端命令算出、B1 入队时持久化，B3 push 分支只读它，不重新从对象库推断。**构造规则**：
  - **`n` 只由客户端基线决定**：`n` = 从 `cmd.new_id` 沿第一父链回溯到 `cmd.old_id` 的步长（`old_id` 必须在链上，否则 MC-03 拓扑校验拒绝；`old_id = ZERO_ID` 的创建推送回溯到链根，`n` = 全链长；`old_id == new_id` → `n = 0`，**这是 N=0 的唯一来历**）。**服务端已知性不参与 `n`**——被拒重试的全部对象都已持久化，按已知性计数会把 N>1 的重试错算成 N=0（违反 ADR-TP-12）。
  - **`fork_base` 是独立的对象/校验边界**：第一个服务端已知 commit（对象复用与验证的参照），不参与 `n` 的计算——二者职责分离，混用即回到「已知性计数」的错误。
  - 环/断链/merge commit 由 MC-03 拓扑校验拒绝；超过 `max_push_commits` → B0 拒绝。
  - 空 pack 且 `old_id != new_id`：对象全部已持久化，`n` 按基线距离计算（可 > 0），B3 N 分支据此快进落地。
- merge：`{cl_link}`（CL 状态仍在库中可查，无需快照）。
- attach：`{repo 上下文, 完整命令描述符}`——**分支 create/update/delete 的完整列表**（`import_repo.rs:450-475/550-573` 应用的是完整快照，仅存仓库上下文无法在崩溃后重放分支级变更）。操作标识为**确定性导出**而非调用方令牌：attach 由 receive-pack 自动触发（`protocol/mod.rs:175-216` 构造 `ImportRepo`，`RefCommand` 无请求 id，`import_refs.rs:50-59`；git 重试也无法携带令牌），故 `operation_id = hash(仓库标识 ‖ 规范化命令描述符)`——git 重试发送相同命令序列，导出值自然一致，收养/回放由此闭合。

执行逻辑仍由调用方在 B3 内按 kind 分派（ADR-TP-04 的同步模型）；载荷的持久化使崩溃后的两种恢复路径成为可能：行仍活跃（`Queued/Running`）时重试**收养**（1.11）；行已被 reaper 终态化（`Failed`，无继任）时重试**以原行持久化载荷新建队列行**——描述符完全可执行，无需原连接。这是 ADR-TP-04「同步阻塞、真实结果」的直接推论。

选择 Postgres 而非 Redis 的依据：B 段本身是 Postgres 事务，锁与数据同源，不存在锁存储与数据存储之间的裂缝；`pg_advisory_xact_lock` 无 TTL，随事务提交/回滚与连接断开自动释放，不会像固定 TTL 的 RedLock 那样在长事务中途失效（事实校准 3）；队列的暂停、排空、位置查询与事后审计需要持久化行；`merge_queue` 已提供同形状的四层实现可供泛化（事实校准 5）。选择事务级而非会话级 advisory lock 的依据：会话级要求 acquire 与 release 落在同一条连接上，在连接池下需要 pin 连接。

**1.4 表结构**

`push_queue` 是**新实体**，不复用现有 `merge_queue` 的枚举——`QueueStatusEnum` 的现有状态机（`Waiting/Testing/Merging/Merged/Failed`，`src/callisto/sea_orm_active_enums.rs:228-239`）描述的是 CL 工作流，与本队列的生命周期（排队/执行/终态）不同构，改挂新状态会改变既有列的语义；`QueueFailureTypeEnum` 也缺少本设计所需的 `QueueBypassDetected`/`PushFailure`/`AttachFailure` 变体。1.8 所说「可复用」指四层形状（storage/service/router/DTO），枚举与实体全新建、走独立迁移：

```sql
create type push_queue_status_enum as enum ('Queued','Running','Done','Failed','Cancelled');
create type push_queue_kind_enum   as enum ('push','merge','attach');
create type push_queue_failure_enum as enum
  ('PushFailure','MergeFailure','AttachFailure','Conflict','QueueBypassDetected',
   'WaitTimeout','ClaimLost','SystemError');
create type push_queue_pending_enum as enum ('requeue_conflict');

push_queue(
  id            bigserial primary key,        -- 全局队列操作序（push/merge/attach 共享，
                                                -- ADR-TP-06：trunk commit 按它保序但非一一对应）
  kind          push_queue_kind_enum not null,-- 操作判别：B3 按此分派
  operation_id  text not null,                -- 稳定操作指纹：push=「old_id→new_id」；
                                                -- merge=cl.link；attach=hash(仓库标识 ‖ 规范化
                                                -- 命令描述符)（内容寻址，重试稳定，1.11）
  path          text not null,
  old_id        text not null,                -- push: cmd.old_id；merge: 信息性（无闸门）；attach: 入队时点根快照（仅诊断用）
  new_id        text not null,                -- push: cmd.new_id；merge: cl.to_hash（语义锚点，见 1.9）；attach: 仓库内容 commit（非根依赖）
  payload        jsonb not null,              -- 最小必要执行载荷（1.3，NOT NULL：reaper 恢复与
                                              -- 崩溃后重排队依赖它必然在）：push=推送描述符
                                              -- {commits, fork_base, n}；merge={cl_link}；
                                              -- attach={repo 上下文, 完整 create/update/delete
                                              -- 命令描述符}。收养执行依据它，不依赖原连接
  landed_commit_id text,                      -- 实际落地 commit：push 行 N=1 时=cmd.new_id、
                                              -- N>1 时=squash commit id；merge/attach 行由 B3 回填。
                                              -- 响应丢失后的结果回放以此为准（1.11）
  status        push_queue_status_enum not null,
  requester     text,                         -- trunk 形态下为 token 名（阶段 5 落地前为 commit committer email）
  failure_type  push_queue_failure_enum,
  error_message text,
  heartbeat_at  timestamptz not null,         -- 等待者存活证明，孤儿 Queued 行的唯一出口
  superseded_by bigint,                       -- 冲突重排的继任行 id（同步调用方的跟随链接，见 B4）
  expected_commit_hash text,                  -- **B2.5 认领事务内落库的根身份对基线**
  expected_tree_hash  text,                   -- （缺失行以 NULL 哨兵参与比对；
                                              --  reaper 对每行 Running 比对之，
                                              --  判定谓词与 CAS 一致）
  pending_action push_queue_pending_enum,     -- 持久化意图（**仅 requeue_conflict**|null）：
                                              -- 根绕过判定不经过它——reaper 对每行 Running
                                              -- 都比对 expected_*（B2.5 落库的基线）；
                                              -- 仅冲突重排写意图（SAVEPOINT 前持久化，崩溃后由 reaper
                                              -- 幂等补做，见 B4/1.6）；陈旧行修复不写意图（expected_* + 通用 I3 修复）。
                                              -- 意图**不携带额外载荷**：修复/重排的全部输入都可从本行
                                              -- 自身列（path/operation_id/payload/superseded_by）+
                                              -- reaper 持锁时的现读（陈旧行 tip/tree、queue_control）重建，
                                              -- 幂等守卫见 B4（状态条件 + superseded_by IS NULL）
  enqueued_at, started_at, finished_at, updated_at
);

create table queue_control(
  id        int primary key default 1 check (id = 1),  -- 单行表
  paused    boolean not null default false,            -- 维护性暂停：drain-only（挡新入队，在队项照常执行）
  hard_stopped boolean not null default false,           -- fail-closed 硬停（QueueBypassDetected 专用）：
                                                         -- B2.5 认领前与 B3 开始时检查，命中即弃权
  last_policy text not null default 'review',            -- 上次运行的 push_policy（4.1 第 3 条的
                                                         -- 形态切换校验依据；启动验证通过后更新）
  max_depth int not null,
  updated_at timestamptz not null
);
-- 迁移/启动时播种唯一行（ON CONFLICT DO NOTHING），
-- 否则 B1 的 FOR UPDATE 无行可锁、准入序列化失效。

-- 同一路径同时只允许一项「推送」在队：第二项必然 non-fast-forward 失败。
-- 仅约束 kind='push'（ADR-TP-10）：merge/attach 的正确性由全序与 B3 锁内重查保证。
create unique index push_queue_active_push_path
  on push_queue(path) where status in ('Queued','Running') and kind = 'push';

-- 同一操作（按 kind + path + operation_id 判别）的 Queued/Running/Done 行至多一个：
-- B1 的条件 INSERT（NOT EXISTS 三态查重）是权威判定，此索引是并发兜底。
-- merge 冲突重排不受影响：原行已终态（Cancelled/Conflict），不在三态之列。
create unique index push_queue_operation_states
  on push_queue(kind, path, operation_id)
  where status in ('Queued','Running','Done');
```

**实现口径（TP-01）:** `push_queue.id` 由 `sea_query` 渲染为 Postgres IDENTITY 列而非 `bigserial` 字面写法——两者同取自序列，ADR-TP-06 的序号保序性质不变。`queue_control.max_depth` 无默认值，播种取 64（本节未规定初值，由 TP-01 选定）。`enqueued_at` / `updated_at` 取 `DEFAULT now()`，使 1.5 B1 INSERT 列清单（不含二者）可执行。

**1.5 算法**

```
B0. 早期拒绝（不入队；全部为非权威预检，权威判定在 B1/B3；无任何 no-op 快路径——空/全已知推送照常入队，见交付物 10）
    - review 形态：push 不入队（只写 refs/cl/*，不触碰根树），走既有 CL 管线
    - ADR-MC-04 单分支准入；ref_type != Tag
    - **P = "/" 拒绝（仅 push kind）**：子路径推送是本计划的唯一形态
      （需求前提）；根路径推送会让「根 CAS」与「P 行落地」指向同一行，
      破坏「每轮至多一次根写入」的唯一性（1.5/ADR-TP-09）。
      **merge 与 attach 的 `P=/` 允许**：根 merge 是现状支持的操作
      （`mono_api_service.rs:2602-2619`，2.2 的后代查询为其保留自我
      排除）；attach 以根为目标是合法的根行更新（其 CAS 即根写入，
      ADR-TP-20 的祖先检查排除 `/`）——按 kind 分别声明，不搞一刀切
    - trunk 形态追加：ref_name == MEGA_BRANCH_NAME，否则拒绝——
      primary_branch_command（push_chain.rs:469-482）现状接受任意非 tag
      分支，trunk 的「唯一公开分支 main」必须在 B0 算法化落实
    - **push 重试幂等解析（先于 N=0 判定，权威次序见 B1②）**：
      指纹查重覆盖三态——命中 `Done` → 按 landed_commit_id 回放 report-status ok；
      命中 `Queued/Running` → **收养**（B1 的统一规则，见 1.11 收养规则：
      reaper 补做的继任行必须可被同指纹重试接管）；
      已成功的 N > 1 推送重试时链上对象全部已知——若让 N=0 早退先行，
      重试将绕过幂等解析，与 1.11 不一致
    - **无任何 no-op 快路径**（ADR-MC-05 的语义由 B3 权威实现）：
      B0 读 ref 是无锁的，快路径会在并发 B3 推进 tip 后虚报成功、且绕过
      路径排除与树哈希修复。空 pack 与已知对象 pack 一律照常校验入队
      （指纹 Done 回放拦截重复推送），B3 按 N（客户端基线锚定）给出
      权威结局——见 B3 的 N=0/N≥1 分支。
      **非空 pack 即使链上对象全部已知也照常走 MC-03 校验与入队**——
      `PushChain::resolve` 对已知 tip 显式返回 Chain 并重验（`push_chain.rs:104-111/160-163/275-288`），
      把「对象已知」当 no-op 会让一次 A 段落库后被拒的推送在重试时虚报成功；
      其最终裁决由指纹 Done 回放（曾成功）或 B3 权威闸门（未成功）给出
    - MC-03 PushChain::validate（无 merge commit、拓扑连续、链长 <= 上限；
      trunk 形态用 max_push_commits，review 形态沿用 MAX_CL_CHAIN_COMMITS）
    - 乐观 non-fast-forward 预检：cmd.old_id == main@P.tip（**非权威且不拒绝**——
      仅作遥测/排序提示；拒绝权威在 B3。无锁的 B0 拒绝会在 B3 修复陈旧
      物化行之前放逐客户端，违背「断言先于 NFF」的次序，ADR-TP-20。
      **仅当 `main@P` 行存在**——无行走下面的创建分支判定，不做 NFF 预检）
    - P 无已物化 main 行：
        - 存在墓碑 → 拒绝（路径有历史，基线无据可查）。提示语必须
          如实分两步：**先在父路径推送重建该目录**（目录不在当前根树时
          advertise 什么也物化不出来——续接物化的前提是路径在根树中
         可解析，否则从墓碑物化会违反 I3）；目录重建后 advertise 会
          从墓碑续接物化，fetch 对齐后再推送（ADR-TP-20/2.5）
        - old_id != ZERO_ID → 拒绝（基线无据可查，见写入模型）
        - old_id == ZERO_ID 且无墓碑 → 创建语义（写入模型）
    - **trunk 形态的删除策略**：删除类命令（CommandType::Delete 或
      new_id == ZERO_ID）在 trunk 形态下一律拒绝——main 的删除沿用 UN-16
      的拒绝语义；其余 ref 的删除同样拒绝并提示：移除内容的正确途径是
      「在父路径推送一个删除该子目录的 commit」（后代路径走墓碑），
      而非删 ref。现状的删除分支挂在 apply_cl_mega_ref_for_push_command
      内（monorepo.rs:919-939），trunk 直推不经过它，闸门必须在 B0 显式补上
    - 队列 paused / 深度超限 → 预拒绝（提示重试）；同路径已有**不同 operation_id** 的 push 在队 → 预拒绝（ADR-TP-10；同指纹重试不在此列，由 B1 收养，见 1.11）

B1. 入队（单事务，权威准入；查重 + 门槛 + 插入为**单语句**原子操作）
    BEGIN
      -- ① 准入锁先行：queue_control 单行 FOR UPDATE 把全部入队
      --    （含 B4 的冲突重入队）串行化，bigserial id 在锁内分配，
      --    id 序 = 提交序 = FIFO 序（ADR-TP-06/I6）。
      --    没有这道锁，先到的入队事务未提交、后到者先拿到更大 id
      --    并提交，B2.5 会把后者当队首——FIFO 被破坏。
      SELECT paused, max_depth FROM queue_control FOR UPDATE

      -- ② 单语句入队（无 check-then-insert 竞态；参考现有形状
      --    merge_queue_storage.rs:56-103；queue_control 以 CTE 读入，
      --    使 paused/hard_stopped/max_depth 与 INSERT 同语句原子）：
      WITH ctrl AS (SELECT paused, hard_stopped, max_depth
                    FROM queue_control WHERE id = 1 FOR UPDATE)
      INSERT INTO push_queue(kind, operation_id, path, old_id, new_id,
                             requester, payload, status, heartbeat_at)
      SELECT <values> WHERE
          (SELECT NOT paused FROM ctrl)                       -- 维护性暂停门槛（drain-only：
                                                              -- 仍允许 Done 回放，见下）
      AND (SELECT NOT hard_stopped FROM ctrl)                 -- fail-closed 硬停门槛：
                                                              -- bypass 调查期间不接受新入队
                                                              -- （Done 回放不受限，回放永远可用）
      AND (SELECT count(*) FROM push_queue
             WHERE status IN ('Queued','Running')) < max_depth -- 容量门槛
      AND NOT EXISTS (                                         -- 三态查重
            SELECT 1 FROM push_queue
             WHERE kind = ? AND path = ? AND operation_id = ?
               AND status IN ('Queued','Running','Done'))
      RETURNING id

      影响 1 行 → COMMIT，取得新 id，进 B2
      影响 0 行 → 回查分类（同一事务内只读判定，随后 ROLLBACK）：
        三态命中 Done             → 按 1.11 回放 landed_commit_id（含暂停/已满时：
                                    回放永远可用，见 1.11 判定次序）
        三态命中 Queued/Running   → **收养**：COMMIT 后以既有行 id 进 B2
                                    （1.11 收养规则——同指纹重试必须能接管
                                    reaper 补做的继任行，拒绝会使继任行
                                    无人执行）
        命中 Failed/Cancelled(无继任) → 三态不含它们，正常 INSERT 新行，
                                    载荷照抄原行（持久化描述符使新行
                                    完全可执行——reaper 终态化后的重试
                                    走此路径，1.3/1.6）
        paused / hard_stopped / 容量超限 → 拒绝（提示重试；硬停期间被拒的
                                    新请求在 clear 后重试，语义与背压一致；
                                    已在队的行保留排队意图——B2.5/B3 的硬停
                                    检查令其弃权但不终态，clear 后重新竞争）
      **唯一索引冲突的并发路径（线性化点说明）**：B1 的 `queue_control
      FOR UPDATE` 使并发入队完全串行——严格按此实现时「双双通过
      `NOT EXISTS`」不会发生（后者在锁内能看到前者已提交/未提交的行），
      串行化点就是本准入锁。若实现退化为无锁并行（如分片准入），冲突
      恢复路径必须存在：INSERT 唯一冲突 → ROLLBACK → **重读分类**
      （胜者已提交 `Done` → 回放；`Queued/Running` → 收养；查无 → 胜者
      回滚 → 重试本入队，至多 K 次）。同 path 不同 operation_id 的第二个
      push 命中 `push_queue_active_push_path` → 拒绝（ADR-TP-10）。
      路径唯一索引冲突（同 path 不同 operation_id 的第二个 push）→ 拒绝
      （ADR-TP-10：这是不同操作的同路径排队，不是收养）
      -- 同路径并发 push 由 push_queue_active_push_path 唯一索引兜底：
      -- 冲突 → ROLLBACK，拒绝（B0 的预检只是省这次失败的开销）
    COMMIT

B2. 等待轮次
    loop:
      本项被 Cancelled：
        有 superseded_by（冲突重排的继任行）→ 跟随继任：以继任 id 回到 B2
        无 superseded_by（真取消）           → 拒绝该推送
      本项被 Failed               → 拒绝该推送
      queue_control.hard_stopped  → **保持等待**（不退出）：B2.5 会弃权并
                                    把行退回 Queued，等待 clear-hard-stop 后
                                    自动重新竞争；hard_stopped 期间 heartbeat
                                    清理冻结（孤儿行不清理，由人工 clear 后
                                    按常规心跳规则处理）
      UPDATE push_queue SET heartbeat_at=now() WHERE id=?   -- 存活证明
      等待超过 wait_timeout：
        **放弃而非取消**——多收养者共享同一行（1.11），任一超时者
        取消行会误杀仍在等待的其他收养者。超时者退出（连接侧 503/拒绝），
        行保持原状态：其他收养者的心跳继续；全部收养者离开后由
        heartbeat reaper 兜底清理。响应丢失由 Done 回放（1.11）兜底
      连接断开 → 本收养者退出（不改行状态；其他收养者继续心跳）
      sleep(poll_interval, 带抖动)
      -- B2 的「无 Running 且我是最小 Queued id」只是进入 B2.5 的
      -- 非权威观察；权威判定在 B2.5 的原子认领里完成

B2.5 认领（**独立事务，先于 B3 提交**；单条语句原子重验 + 认领 + **预期根快照原子落库**——认领事务内读根身份对并与认领同事务持久化 `expected_*`。此后不存在其他合法根写入者：任何 B3 都要求 `NOT EXISTS Running`，故认领之后的根变化只能来自队列外写入者，`expected_*` 是 reaper/B3 判定绕过的可靠基线，封住「标记持久化前崩溃」的窗口）
    -- 认领前先查 hard_stopped（fail-closed：bypass 后在队项不得执行）
    IF SELECT hard_stopped FROM queue_control WHERE id=1:
      → 弃权：行**回 Queued**（独立条件重置事务，排队意图保留），
        调用方的 B2 循环继续等待（clear-hard-stop 后自动重新竞争），
        不拒绝、不终态
    -- 准入/认领串行化：先取 queue_control FOR UPDATE（与 B1 同一行锁），
    -- 使并发认领互相串行——READ COMMITTED 下两个并发认领可各自通过
    -- NOT EXISTS 谓词（行锁不序列化谓词），各自置 Running，产生并发 B3，
    -- 破坏 I6；队列行锁是唯一可靠的串行化点。
    SELECT paused, hard_stopped FROM queue_control WHERE id=1 FOR UPDATE
    -- 认领**先行**（先于基线读取——认领的 NOT EXISTS Running 谓词在
    -- 上述锁内求值，认领成功后不可能再有其他 B3 存在或启动，其后的根
    -- 读取吸收了全部已提交合法写入；先读后认领则可能把前序 B3 的提交
    -- 算成绕过）：
    -- 认领的原子谓词内含 hard_stopped = false（锁内再查一次，封住
    -- 「硬停恰在预查与加锁之间置位」的窗口）：
    UPDATE push_queue SET status=Running, started_at=now(), heartbeat_at=now()
      WHERE id=? AND status='Queued'
        AND (SELECT NOT hard_stopped FROM queue_control WHERE id=1)
        AND NOT EXISTS (SELECT 1 FROM push_queue WHERE status='Running')
        AND id = (SELECT min(id) FROM push_queue WHERE status='Queued')
    -- 影响行数 = 0（含硬停置位所致）→ 回 B2 按硬停语义处理
    -- 同事务内读根身份对并落基线（**认领后、提交前**；此后到本轮终态，
    -- 根的任何变化都只能来自队列外写入者）：
    root0 = get_main_ref("/")            -- 可能 NULL（行缺失）
    UPDATE push_queue SET expected_commit_hash=root0.commit,
                          expected_tree_hash=root0.tree
      WHERE id=?
    -- 三条件在同一语句内判定，Postgres 行级锁保证原子性：
    --   已有 Running（先我一步认领）        → 0 行，回 B2
    --   我不是最小 Queued id（队首被跳过？）→ 0 行，回 B2
    --   status 已非 Queued（reaper/cancel） → 0 行，按 B2 重判
    影响行数 = 0 → 回到 B2 重判
    COMMIT                          -- 认领必须先落盘，见下方「为什么认领要独立提交」

B3. 执行轮次（单事务；B3 首句是 fencing，见下）
    BEGIN
      pg_advisory_xact_lock(MONO_WRITE_LOCK)

      -- fencing：认领（B2.5）与取锁之间，reaper 可能已持锁把本行标 Failed。
      -- 持锁后重读本行，非 Running 即弃权。
      IF SELECT status FROM push_queue WHERE id=? != 'Running':
        → ROLLBACK；向调用方返回可重试失败（ClaimLost），不动任何数据

      -- fail-closed 硬停检查（持锁、事务内，先于一切业务读写）：
      IF SELECT hard_stopped FROM queue_control WHERE id=1:
        → ROLLBACK（数据写入全部撤销）；
          随后**独立提交的条件重置事务**：UPDATE push_queue
            SET status='Queued', started_at=NULL
            WHERE id=? AND status='Running'    -- 行回队列，不终态
          NOTIFY；返回 HardStopped
      -- B2.5 已原子持久化 expected_*（认领时点的根身份对）；此处比对其
      -- 与锁内现读根——不一致即 B2.5 之后出现队列外写入者 → 走 CAS 失败
      -- 同款硬停路径（预期根标记已在认领时落库，无「CAS 已执行而意图
      -- 未落库」窗口）
        -- 「B3 内直接改回 Queued」不可持久化：B2.5 已独立提交 Running，
        -- B3 回滚会连同重置一起撤销，行停留 Running 且可能被 reaper
        -- 标 Failed——排队意图丢失。独立重置事务的崩溃语义：
        -- 崩溃前提交 → 行回 Queued 等待人工 clear；
        -- 崩溃在重置前 → 行停留 Running，reaper 在 hard_stopped 置位
        --   期间的清理动作是**条件重置回 Queued**（而非 Failed），
        --   排队意图同样保留

      -- 权威准入：排队期间状态可能已变，必须在锁内重查（按 kind 分派）
      root = get_main_ref_in_txn("/")

      -- **预期根基线比对（全 kind 通用，独立可执行分支）**：expected_*
      -- 在 B2.5 认领事务内落库；此处比对锁内现读根与基线
      -- （`IS NOT DISTINCT FROM`，缺失行 NULL 哨兵）：
      IF root 与 expected_* 任一分量不一致:
        → **不 ROLLBACK**（先回滚会给 B1 留出准入窗口）——SAVEPOINT 回退
          业务写入，**同一事务内**置 hard_stopped + 行 Failed
          (QueueBypassDetected, pending_action=NULL) + NOTIFY，持锁提交；
          不做任何树/ref 工作；无此分支则绕过写入会被并入本轮根、
          CAS 反而成功，静默吞掉绕过
      -- merge/attach 的根 CAS 与 push 同受此覆盖（R31 #1）

      kind = push（trunk 形态）:
        -- 树结果与分流一律以行上持久化的推送描述符为准（1.3）：
        -- N 锚定客户端基线（new_id→old_id 段长），服务端已知性不参与，
        -- 重试不重算；
        -- 不从对象库已知性推断（被拒重试的全部对象都已持久化）
        IF n == 0（**当且仅当 cmd.old_id == cmd.new_id**——退化重推；
          -- 空 pack / 对象已知不构成 N=0：N 锚定客户端基线段长
          -- （写入模型），随描述符持久化，重试不重算）:
          -- N=0 是**真实落地路径**，不是 no-op 快捷方式：
          -- 闸门照常（non-ff + 树哈希断言）。old_id == new_id 意味着
          -- 客户端声明「把 ref 置于其现值」：
          --   new_id == tip          → 无变更，行 Done（landed_commit_id = tip）
          --   其余（tip 与声明不符）  → B4 拒绝（non-fast-forward 语义）
          -- 曾成功者的重推在 B1 即被指纹 Done 回放拦截，不到此处
        row  = get_main_ref_in_txn(P)
        -- 权威 commit 校验（I1/I2 的落地判据，逐一显式）：
        --   行存在  → 必须满足 ref_commit_hash == cmd.old_id（N ≥ 1 的
        --             非快进即拒绝；树哈希断言已先行）
        --   行缺失  → cmd.old_id == ZERO_ID 才可创建（否则拒绝）；
        --             注意交付物 2 之前 batch_update 对缺失行静默跳过，
        --             创建必须走 upsert 原语
        --   N = 0   → 必须满足 cmd.new_id == 当前 tip（否则 NFF 拒绝）
        IF row 存在:
          -- 树哈希断言**先于** non-fast-forward 比对：陈旧物化行无论
          -- 客户端基线新旧都先被拒绝并修复（NFF 在前会让持非陈旧 tip
          -- 的客户端绕过修复，「下一次推送修复陈旧物化」的承诺失效）
          IF row.ref_tree_hash != resolve(root_tree, P) → 拒绝（陈旧物化，
            ADR-TP-20；SAVEPOINT 同事务修复：撤数据写、写墓碑、删行、
            标 Failed，原子提交；SAVEPOINT 前崩溃由 reaper 对每行 Running
            的 expected_* 比对 + 通用 I3 修复兜底）
        ELSE:
          -- 创建语义的权威前提（B0 的墓碑预检非权威：B0 与 B3 之间
          -- 可能发生一次 ADR-TP-20 修复，墓碑是锁内新出现的）
          IF 墓碑存在(path, 'refs/heads/main') → 拒绝（走 B4；路径有历史，
            先 advertise 从墓碑续接物化、对齐后重推）
          IF cmd.old_id != ZERO_ID → 拒绝
          ELSE → 创建语义落地（写入模型，按 N 分流；N = 0 不可达：
            old_id = ZERO_ID 而有效 new_id ≠ ZERO_ID）:
            N = 1 → main@P = cmd.new_id（landed_commit_id = new_id）
            N >  1 → parentless squash commit（landed_commit_id = squash id）
            两者均经 non-ff（ZERO_ID）与树哈希断言双重校验：
            new_id 的 tree 必须等于 resolve(root_tree, P) 或路径不在根树
        -- B0 判定「无行」与 B3 锁内重读之间，物化可能已插入合成行
        -- （路径在根树中可解析但此前未物化）。此时创建推送的
        -- old_id=ZERO_ID 与合成 tip 不匹配：按可诊断拒绝处理——
        -- 「路径已在等待期间物化，请 fetch 后基于物化 tip 重推」，
        -- 不落入通用 non-fast-forward 文案；盲目落地会以客户端
        -- 无基线的历史顶掉物化视图，破坏 I1。真创建（路径不在
        -- 根树中）不可能是物化的产物，无此竞态
        chain_trees = search_tree_for_update(parent(P))
        result      = build_result_by_chain(P, chain_trees, chain.tip.tree_id)
        P      → N == 1: (cmd.new_id, chain.tip.tree_id)   -- 不合成，fast-forward
                  N >  1: 合成 squash commit(parent = P.tip, tree = chain.tip.tree_id)
        祖先 A → 合成 commit(parent = A.tip, tree = A 的新树)
        后代 D → 阶段 1：remove_none_cl_refs 的**事务内变体**（交付物 5，
                 与 merge 分支同一原语）；阶段 2 起切换为
                 advance_descendant_refs（2.8 同时覆盖 merge 与 push 分支）
        -- 预期根标记已在上方通用路径持久化（全 kind 通用）
        -- 根 ref 更新 = 本轮唯一一次根写入：
        --   树变化的轮次：推进写；净零轮次：同值写（tripwire 不断，
        --   见「根更新的唯一性」）；双条件 CAS 与 attach 一致
        UPDATE mega_refs SET ... WHERE path='/' AND ref_name='refs/heads/main'
          AND ref_commit_hash = root.ref_commit_hash
          AND ref_tree_hash   = root.ref_tree_hash            -- CAS 断言
        -- CAS = 1 → 轮次正常收尾（expected_* 保留作台账字段，无待清意图）

      kind = merge（review 形态）:
        -- 无 old_id 闸门：落地 parent 按现状取 refs/cl/<link> 行的 tip
        -- （命名分歧时落 main head，GAP-07 保留）；根树在锁内重读，
        -- 陈旧性已被消除
        执行期门控重查（**全部既有门控原样保留**，含 CL 状态 Closed/Draft 门，`mono_api_service.rs:4419-4455`）：CL 存在性、UN-19、冲突（冲突 → 队尾重排，见 ADR-TP-05）、
        GPG（ensure_gpg_check_passed，mono_api_service.rs:4599 的现状等价物）
        -- 顺序：树哈希断言（数据一致性前置）先于业务门控重查；
        -- CL 行更新带版本 CAS（WHERE to_hash = 读时值），与并发 rebase 串行化
        执行既有 merge 落地（merge_cl_unchecked 的落库部分）；其中：
          - **main@P 缺失 → 拒绝**（沿用现状语义：`merge_cl` 入口拒绝
            缺失 ref，`mono_api_service.rs:2113-2118`；执行期重查
            `:4634-4647` 同）。merge 不使用交付物 2 的 upsert——
            upsert 只属于 push 创建语义，merge 落地以「基线存在」为前提
          - **树哈希断言（与 push 分支同一谓词，对象是 main@P 行）**：
            merge 会**覆写** `main@P`，覆写前断言
            main@P.ref_tree_hash == resolve(root_tree, P)，不等 → 拒绝 +
            ADR-TP-20 墓碑修复（否则覆写会把与根树断开的历史写入主干，
            且旧 tip 的祖先链断裂）。**落地 parent 的选取保持现状**——
            `process_ref_updates` 优先取 `refs/cl/<link>` 行的 tip、
            命名分歧时落 main head（`mono_api_service.rs:623-636`，
            GAP-07 现状保留）；CL ref 行不是根树的派生视图，
            不参与该断言。修复后 CL 保持 open，重新物化后重试即可
          - **锁内重算 TreeUpdateResult**：现状 `merge_cl_unchecked` 在应用前
            预计算 `search_tree_for_update`/`build_result_by_chain`
            （`mono_api_service.rs:2582-2614`）；B3 中该预计算结果可能
            基于排队前的旧根，直接沿用会覆盖等待期间的 intervening
            写入。merge 分支必须在锁内从当前根**重算**全部树结果后再落地
          - 后代处理：阶段 1 用 remove_none_cl_refs 的**事务内变体**
            （1.10 交付物 5——现状函数自建连接，不入 B3 事务会破坏
            单事务原子性；变体同时修复 LIKE 两处缺陷），阶段 2 切换为
            advance_descendant_refs（2.8）
          - 根 ref 的更新从 apply_update_result 的无条件 UPDATE 改走
            事务内 CAS 原语（每轮至多一次根写入，见下方「根更新的唯一性」）；
            CL 状态与 conversation 写入一并穿入同一事务（交付物 6），
            杜绝「根已落地、CL 状态可重试」的二次合并窗口

      kind = attach:
        -- operation_id 为内容寻址指纹（hash(仓库标识 ‖ 命令描述符)）。
        -- 仅含分支命令的 attach 到达本分支（纯删除式 attach 不入队）。
        -- **目标路径、其严格非根祖先、或其任何已物化后代 → 拒绝**：
        -- attach 只更新根 ref 与对象，不更新 `main@P`、祖先 main 行、
        -- 也不推进/墓碑化后代 main 行（`attach_to_monorepo_parent_in_txn`
        -- 仅 trees/commits/root）——对已物化目标/祖先/后代 attach 会使
        -- 相应物化行与根树分叉（I3 破损：后代可能被 attach 的树变更
        -- 改写或删除而 ref 行纹丝不动）。后代检查 = 存在 `path LIKE 'P/%'`
        -- 且 `ref_name = main` 且 `is_cl = false` 的行（2.2 同款谓词 +
        -- 自我排除）。
        -- **`main@/` 例外**：bootstrap 恒物化根行（`mono_service.rs:103-154`），
        -- 且 attach 本就合法更新根行（其 CAS 即根写入），故 `/` 不在检查之列；
        -- `P = "/"` 的 attach **允许**（B0 的 `P=/` 拒绝仅属 push kind，
        -- 见 B0 按 kind 的声明）。
        -- 报错可诊断：「目标路径或其祖先已物化，内容变更请经 CL 管线
        -- （review）或先解除物化」。目标与严格非根祖先全程无 main 行才是
        -- attach 的正常前提（reaper 的 I3 语义按此定义，1.6）
        -- 准备（根快照读取、树拼装、根 commit 合成）必须在锁内、
        -- 即本分支内进行：队列等待期间根可能已被前序轮次推进，
        -- 入队时预计算的快照一律作废（1.9）
        以队列表中的稳定标识（attach 请求 id）定位仓库上下文，
        锁内重读根快照 → 拼装合并树 → 合成根 commit →
        attach_to_monorepo_parent_in_txn 落库——其根 ref 更新本就携带
        (ref_commit_hash, ref_tree_hash) 双条件 CAS（事实校准 4），
        即本分支唯一的根写入；重试循环已随吸收删除（ADR-TP-05）

      落 tree 对象、落合成 commit 对象

      -- （push 分支的根 CAS 已在分支内完成；merge/attach 各自在分支内
      --   完成唯一一次根写入。B3 不存在第二个根更新点。）

      IF 本轮根 CAS 影响行数 = 0（任一分支）:
        → 不重试（预期根标记已在 CAS 前持久化，见上方分支内注释）。
           比对 expected 身份对与当前根（`IS NOT DISTINCT FROM` 的
           null 安全比较：commit hash 与 tree hash **任一分量**不等
           即绕过——tree-only 的绕过不会被 commit hash 比对漏过；
           根行缺失以 NULL 哨兵参与比对）→ B3 内 SAVEPOINT：
              ROLLBACK TO SAVEPOINT（撤全部数据写入），同事务内
              UPDATE queue_control SET hard_stopped=true
              UPDATE push_queue SET status=Failed,
                     failure_type=QueueBypassDetected, error_message=...,
                     pending_action=NULL
                WHERE id=? AND status='Running'
              -- 命中 0 行（行已被 reaper 处理）→ 仍置 hard_stopped 并
              -- fail-closed 告警（终态写一律带状态条件，0 行不可静默）
              COMMIT（无数据写入、只有 hard_stopped + Failed 终态）
           并 fail-closed 告警：存在绕过 MonoWriteQueue 的根树写入路径
        -- 崩溃语义（无未判定窗口）：意图提交先于根 CAS 持久化——
        --   意图提交后、SAVEPOINT 提交前崩溃 → reaper 依据 expected_root
        --   判定（根被推进 ⇒ 确认绕过，hard_stopped + Failed；根未动 ⇒
        --   无绕过，普通 Failed），幂等补做（1.6）；
        --   意图提交前崩溃必然发生在 CAS 之前，无绕过发生，普通 Failed
        --   即正确
      其余 ref 更新（经 1.1 交付的事务内顺序更新变体，事实校准 16）
      UPDATE push_queue SET status=Done, landed_commit_id=?, finished_at=now()
        WHERE id=? AND status='Running'
      -- landed_commit_id 与 Done 在同一条件更新内写入（1.4/1.11 的
      -- 回放权威依据）：push 行 = N>1 时的 squash id（N=1 时即 cmd.new_id）、
      -- merge 行 = 落地的合成 commit id、attach 行 = 落地的根 commit id
      IF 影响行数 = 0 → ROLLBACK（理论不可达：fencing 之后本行必为 Running；
        可达即缺陷，按 fail-closed 处理并告警）
      NOTIFY mono_write_queue
    COMMIT

B4. 失败
    -- 两段式仅用于冲突重排（requeue_conflict 是 `pending_action` 的
    -- 唯一取值）：先持久化重排意图，再 SAVEPOINT 完成。SAVEPOINT 在
    -- 任何可能出错的业务写之前建立；语句错误会中止整个事务，届时退回
    -- 「独立状态事务」路径（意图已先行持久化，reaper 依据意图幂等收尾）。
    -- 陈旧行修复不使用意图（其崩溃窗口由 reaper 对每行 Running 的
    -- `expected_*` 比对 + 通用 I3 修复兜底，见 1.6/ADR-TP-20）。
    -- 任一崩溃窗口都有确定的恢复者：
    --   意图提交前崩溃 → 行 Running、无意图，reaper 标 Failed
    --                     （Conflict 的重排由调用方重试重建）
    --   意图提交后崩溃 → 行 Running、意图在，reaper **先幂等完成重排
    --                     （创建继任行）再终态化**
    failure_type = Conflict（merge 冲突重排）:
      独立短事务：UPDATE push_queue SET pending_action='requeue_conflict'
        WHERE id=? AND status='Running'
      B3 内 SAVEPOINT（撤数据写）+ 准入锁（queue_control FOR UPDATE）:
        UPDATE push_queue SET status=Cancelled, failure_type=Conflict,
               finished_at=now(), pending_action=NULL
          WHERE id=? AND status='Running' AND pending_action='requeue_conflict'
        INSERT push_queue(..., payload=旧行.payload, status=Queued, heartbeat_at=now())
          RETURNING id → successor_id        -- id 在准入锁内分配，落队尾；
                                             -- 载荷随行继承（收养者执行依据）。
                                             -- 本插入是同事务的**内部替代操作**
                                             -- （旧行同时关闭），豁免外部的
                                             -- hard_stopped/depth 门槛（B3 能执行
                                             -- 即证明非硬停；净深度不变），不受
                                             -- 外部 gate 拒绝——若创建仍失败，
                                             -- 重排意图已持久化，reaper 幂等补做
        UPDATE push_queue SET superseded_by = successor_id WHERE id = 旧行 id
      COMMIT
      -- 调用方（同步在线）读出 successor_id 后回到 B2 继续等待/执行
      -- 继任行；没有这道交接，后台 processor 已退役（1.3/ADR-TP-04），
      -- 继任行将永远无人执行。reaper 补做时同样写 superseded_by，
      -- 幂等性由「旧行 Cancelled 且 superseded_by IS NULL 才插继任」守卫
    其他 failure_type:
      SAVEPOINT（撤数据写）+ 标 Failed（含 CAS 失败的 hard_stopped，见 B3；
        **每一次终态迁移都置 `pending_action=NULL`**——崩溃残留的意图
        由 reaper 按前述规则补做，不得残留在终态行上）
    NOTIFY mono_write_queue
    COMMIT（无数据写入，只有终态/重排/修复）
    不冻结队列（沿用 plan-20260827 对 MergeFailure 不 freeze 的先例）
```

**根更新的唯一性**：每个**抵达根写入点的轮次**对根 ref 的 CAS 断言写**恰好执行一次**（在 B3 早期闸门处被拒的轮次走 B4，不执行根写入，也无此断言）——push 分支在根树受影响时执行推进写；**净零推送（根树不变）执行同值 CAS 写**（`SET ref_commit_hash=原值 WHERE ref_commit_hash=原值`，Postgres 计 1 行并持行锁，tripwire 不因净零而缺席——绕过队列的写入者恰可在 B3 读根与提交之间推进根，跳过断言就放过它）；merge 与 attach 分支各执行一次且是分支内唯一一次。一律携带 CAS 条件：push/merge 走事务内 CAS 原语，attach 沿用 `attach_to_monorepo_parent_in_txn` 的既有双条件 CAS。**根 commit 的前进与断言写是两件事**：前者由「该轮的根树是否变化」决定（ADR-TP-16 同一谓词），后者在抵达根写入点的轮次上恒在。B3 中**不存在**第二个根更新点：若把通用根 CAS 放在分支之后对 merge/attach 再执行一次，前一次写入已推进 `ref_commit_hash`，第二次必然命中 0 行并误报 `QueueBypassDetected`。三种 kind 共用同一条 CAS 断言语义，tripwire 对全部成功轮次生效。

**为什么认领要独立提交（B2.5）**：若把 `status=Running` 的写入放进 B3 的业务事务，执行中崩溃会连同它一起回滚，队列行退回 `Queued` 且 `started_at` 仍为 NULL——既不是可识别的崩溃残留，也无法被按 `started_at` 判定的 reaper 捞到，而它又永远是最小 `Queued` id，会让此后所有推送在 B2 等不到轮次、全部超时取消。**认领与执行必须是两个事务**：认领先落盘，崩溃后残留物才是可识别的 `Running` 行。这同时落实了 ADR-TP-05 对 `merge_queue` 提出的「原子 claim 写回」要求——B2.5 的 `WHERE status='Queued'` 条件更新就是那个原子 claim。

CAS 在本设计中的角色是**断言/tripwire**而非并发控制：队列成立时它恒应成功，失败即证明存在绕过队列的写入者，正确反应是 fail-closed 告警。它是写入者审计（1.1）之外的运行时兜底，**不是完备性证明**——完备性第一依据是审计本身（ADR-TP-09）。

**CAS 失败是 ADR-TP-07 的例外**。ADR-TP-07 说「轮次失败不冻结队列」，针对的是 non-fast-forward、链校验不通过这类**常规拒绝**——它们不影响后续轮次的正确性。CAS 失败不同：它意味着有写入者在队列之外改根树，序列化保证已经失效，继续处理后续轮次只会在一个正被他人变动的根上叠加更多写入。因此 CAS 失败必须**同时**做三件事：把该行标 `Failed`（终态明确，不留给 reaper 去猜）、自动置位 `hard_stopped`（fail-closed 硬停，B1/B2/B2.5/B3 检查；与维护性 `paused` 的 drain-only 语义分离）、发出告警。三者的崩溃恢复不依赖修复意图：**预期根基线（`expected_commit_hash`/`expected_tree_hash`）在 B2.5 认领事务内原子落库**，B3 与 reaper 各自比对基线与现读根（不一致 ⇒ 绕过 ⇒ 硬停 + `QueueBypassDetected`），reaper 终态化前统一执行该行 path 的 I3 修复（1.6）。恢复需要人工确认绕过路径已封堵后 `clear-hard-stop`。

**1.11 幂等与重试解析**

队列不跨进程恢复执行（1.3），调用方在响应丢失后重试是常态路径（HTTP 客户端超时重试、git 客户端整包重推），因此每类操作必须有明确的**重试解析规则**，防止「已提交、响应丢失、重试再执行一次」造成二次落地：

- **稳定操作标识 `operation_id`**：push 用**推送指纹** `old_id → new_id`（同一路径上「从哪个基线推到哪个 tip」唯一确定一次推送意图）——**不能只用 `cmd.new_id`**：同一 tip 可经不同基线提交（先 A→C 的 N = 2 推送落地后，再提交 B→C），两次是不同操作，仅按 `new_id` 查重会把第二次误判为重试而绕过 non-fast-forward 校验；按指纹查重时第二次正常排队并被权威闸门拒绝（tip 已非 B）。merge 用 **`cl.link`**——不能用 `cl.to_hash`：两个不同 CL 可以共享同一 `to_hash`（`mega_cl.rs` 的 `from_hash`/`to_hash` 是区间端点，不含 CL 身份）。`cl.link` 标识的是**合并意图**而非某个版本：CL 被 rebase 后 `to_hash` 变更，`operation_id` 不变，B3 执行时按 CL 的**当时** `to_hash` 落地（与「merge 落地 parent 取执行时点」一致）；重试解析按 `landed_commit_id` 回放，不受 rebase 影响。attach 用**内容寻址指纹** `hash(仓库标识 ‖ 规范化命令描述符)`——attach 由 receive-pack 自动触发，`RefCommand` 无请求 id、git 重试也不携带令牌，内容寻址是唯一重试稳定的来源；落地 commit 依赖执行时点的根，由 `landed_commit_id` 记录。
- **判定次序**：push 的幂等解析（B1 的条件 INSERT 查 `Done` 行）**先于**乐观 non-fast-forward 预检——N > 1 的重试在服务端落地的是 squash commit，`cmd.new_id != main@P.tip` 恒成立，只有查 `Done` 行才能在 non-fast-forward 拒绝之前拦截它。**不存在任何 B0 快路径，B0 也不做任何拒绝性 NFF 判定**（1.5：无 no-op 早退；无锁的 B0 判定会绕过 Done/活跃操作解析与 ADR-TP-20 修复，乐观 NFF 预检降级为遥测）——`Done` 回放（B1）与 B3 的 N=0 分支是仅有的两条权威路径，**B3 是唯一的 NFF 权威**。merge/attach 同样走 B1 的条件 INSERT。
- **活跃行的收养规则（Queued/Running 命中的唯一动作）**：三态查重命中 `Queued/Running` 时，B1 一律**收养**——以既有行 id 进入 B2 等待/认领循环、接管心跳刷新。收养是必须支持的路径：冲突重排的继任行在原调用方崩溃后没有执行者，同指纹重试（merge 的 HTTP 重试、push 的整包重推）收养继任行才能让它被执行；多个收养者并发时 B2.5 的原子认领保证只有一个执行者，其余回到等待。**已知损失（自主重试语义的丧失）**：reaper 补做冲突重排后若原调用方已死亡且无重试到来，继任行没有任何执行者，最终由 heartbeat 清理取消——调用方必须重试才能完成该 merge（这是 caller-owned 模型的固有代价，ADR-TP-04 的同步模型以此换取真实结果；如需自主重试须引入持久 worker，见 1.9 2a 的退役决定）。push 的收养是安全的——落地面 `(path, old_id, new_id)` 完全由队列行与对象库决定（对象在 A 段已持久化），不依赖原连接的内存状态。**终态命中（`Failed`/无继任的 `Cancelled`）不在三态之列**：B1 正常插入新行并照抄原行 `payload`——持久化描述符使新行完全可执行，这是 reaper 终态化后重试的恢复路径（1.3/1.6）。B1 的状态机因此是三分支：`Done` → 回放；`Queued/Running` → 收养；未命中（含终态命中）→ 插入（同路径不同指纹的第二个 push 另被 ADR-TP-10 的路径索引拒绝）。
- **查重与插入必须同一语句**：`INSERT ... WHERE NOT EXISTS (... status IN ('Queued','Running','Done'))` 三态原子判定——先查后插的两步写法存在终态竞态（重试在原行 `Running` 时查重未命中 `Done`，被活跃索引挡下；原行提交 `Done` 后重试的 INSERT 不再与活跃索引冲突，重复入队）。三态条件 + `push_queue_operation_states` 唯一索引（`Queued/Running/Done` 三态上的 `(kind, path, operation_id)` 唯一）双层兜底：条件 INSERT 是权威判定，索引是并发保险。命中 `Queued/Running` → **收养**（以既有行 id 进 B2，见收养规则）；命中 `Done` → 回放。
- **结果回放**：命中已完成行时**短路返回成功**并回放 `landed_commit_id`（注：回放的 tip 此后可能已被更新的推送超越——回放 commit 恒为现 tip 的祖先，对 git 语义无害；调用方需要最新态时应另行 fetch）（push 行 N = 1 时为 `cmd.new_id`、N > 1 时为 squash commit id；merge/attach 行由 B3 回填）。回放内容：push 返回 report-status ok（附「已落地」提示）；merge 返回当前 `main@P` 与 CL 终态；attach 返回已 attach 的路径状态。不产生新队列行、不进入 B3。
- **B3 内兜底**：merge 执行期门控重查的「CL 存在性」天然拦截已合并 CL 的重放（CL 已 Merged → 按 `landed_commit_id` 回放幂等成功而非错误）；attach 执行前按 `operation_id` 查重。**merge 与 rebase 的双向串行化（单调 revision）**：仅比对 `to_hash` 挡不住反序竞态——rebase 读到 `Open` 后被排队的 merge 先落地 `Merged`，陈旧 rebase 随后仍能把行改回 `Open`（现状两侧都是无条件更新，`cl_storage.rs:374-379/404-415`）。交付物：`mega_cl` 增加**单调 revision 列**（每次变更 +1），merge 与 rebase 的行更新一律携带全量 CAS（`WHERE id=? AND status='Open' AND to_hash=<读时值> AND revision=<读时值>`），命中 0 行即 `ClaimLost`（回滚弃权，调用方重试）——merge 提交后的陈旧 rebase 被拒，rebase 之后的 merge 按新 `to_hash` 执行。配反序回归（先 merge 落地、后陈旧 rebase）。
- **不可达的负例**：同 `operation_id` 且**内容不同**的重试——push 不存在（指纹含 `old_id`，基线不同即指纹不同，正常排队并交由权威闸门裁决）；merge 的 rebase 是同一意图的版本演进，执行语义已覆盖（按当时 `to_hash` 落地 + 版本 CAS 串行化，重放按 `landed_commit_id`）；attach 的指纹随命令描述符变化（不同变更集 = 不同操作），描述符不变则命令不变。不同操作自然产生不同 id，正常排队。

> **验收标准**（并入阶段 1/4 的验收清单）：见阶段 1 验收的「幂等重试回归」条目。

**1.6 崩溃与卡死**

崩溃可能发生在三个窗口，各自留下不同的残留物，必须分别有清理路径。B1 的 INSERT 与 B2.5 的认领都是独立提交的，因此「队列行的状态」始终如实反映进程走到了哪一步。

| 情形 | 残留物 | 清理路径 |
| --- | --- | --- |
| B3 执行中进程崩溃 | 业务事务回滚（无部分状态）；advisory lock 随连接释放；队列行停在 **`Running`**（B2.5 已独立提交） | reaper 持锁事务内将该行标 `Failed`（见下，**立即**执行，无需等待 `stuck_timeout`）；数据面已由回滚保证干净；调用方重试按 1.11 以持久化载荷**新建队列行**（收养仅适用于仍活跃的行） |
| B2 等待中进程崩溃 | 队列行停在 **`Queued`**，`heartbeat_at` 停止推进；**它是最小 Queued id，会阻塞其后所有推送** | reaper 将 `heartbeat_at` 早于 `now() - heartbeat_timeout` 的 `Queued` 行标 `Cancelled` |
| 客户端排队中断线（进程存活） | 该收养者停止心跳，行状态不变（多收养者共享行，单个离开不得杀行） | 收养者检测到断连即**自行退出**（不改行状态）；其他收养者的心跳继续；**全部**收养者离开后心跳停止，由 heartbeat reaper 兜底清理（孤儿 `Queued` 行的唯一出口，见下） |
| B1 与 B2 之间崩溃 | 同「B2 等待中崩溃」 | 同上 |
| 认领与取锁之间被 reaper 越过 | 本行被标 `Failed`，而工作进程仍会尝试执行 | B3 首句 fencing：持锁后重读本行，非 `Running` 即弃权回滚（ClaimLost），不动任何数据 |
| 队列 paused | 无 | 允许排空：不接新项，已在队项照常执行 |

**heartbeat 是孤儿 `Queued` 行的唯一出口**。B2 的等待循环每轮刷新 `heartbeat_at`（见 B2 伪代码），因此一个没有活跃等待者的 `Queued` 行必然停止心跳。没有这条机制，B2 的「无 Running 且我是最小 Queued id」规则会让单个孤儿行永久占住队首，之后每一次推送都等到超时取消——队列硬死锁，只能人工 `cancel`。`heartbeat_timeout` 应显著大于 `poll_interval` 且小于 `wait_timeout`。

**reaper 的正确形态：以锁为互斥，立即清理**。B3 全程持有 `pg_advisory_xact_lock(MONO_WRITE_LOCK)`，这条性质本身就给出了无竞案的判定：

1. **reaper 在自己的事务里 `pg_try_advisory_xact_lock(MONO_WRITE_LOCK)`**。拿到锁 ⇒ 任何 B3 都不在执行（advisory xact lock 是排他的）⇒ 此刻可见的每一行 `Running` 都没有活跃的执行者，**当场处理**（对 `started_at` 极新（秒级宽限，可配）的行跳过一轮——B2.5 提交后排队等锁的活轮次不必吃一次无谓的 ClaimLost；宽限是观测友好性设计，不参与正确性判定）：**对每一行 `Running` 行（无论有无意图）先比对根基线**：当前根身份对（commit hash 与 tree hash，缺失行以 NULL 哨兵）与行上 `expected_*`（**B2.5 认领事务内落库的基线**——认领后无其他合法根写入者，任何不一致都指向队列外写入）任一分量不等 ⇒ 确认绕过——**绕过判定优先于一切意图补做**：`requeue_conflict` 意图被丢弃（重排由调用方重试重建），**先置 `hard_stopped`（止住后续轮次），再做该行 path 的 I3 校验/修复，最后终态 `Failed(QueueBypassDetected)`**——三步在 reaper 持锁事务内按序完成。全等 ⇒ 无绕过——**仍执行该行 `path` 的 I3 校验与墓碑修复**（根相等不代表 path 层一致：净零推送可在根不变时推进 `main@P`；B3 在检出前崩溃时，若跳过校验，陈旧行将无人修复、`refs_with_head_hash` 会持续返回它，I3 破损），完成后普通 `Failed`；`requeue_conflict` 按前述补做，**继任行 INSERT 同样取 `queue_control FOR UPDATE` 准入锁**（reaper 持有的 `MONO_WRITE_LOCK` 与 B1 准入锁互不排斥，无准入锁的插入会让并发 B1 拿到更大 id 先提交，破坏 I6 的 min(id) 认领序）。**reaper 对任何待终态化的 `Running` 行（无论有无意图、无论根比对结果）都先做一次 I3 校验**（语义按 kind）：push/merge 行的 `path` 解析当前根树子树哈希与 ref 行比对，陈旧则就地墓碑修复；**attach 行的 `path` 无 `main@P` 行属正常（`main@/` 恒存在且不参与此校验——attach 合法更新根行）**——attach 只写根 ref 与对象，不物化路径 ref（`attach_to_monorepo_parent_in_txn` 仅 trees/commits/root ref，分支 ref 在 Git DB，`import_repo.rs:552-570`），校验跳过；**attach 行的 path 存在 `main@P` 行** → 异常残留（B3 已拒绝已物化目标的 attach，出现即告警登记，按陈旧行处理）；**push 行的 `path` 无 `main@P` 行**：`old_id = ZERO_ID` 且无墓碑 → 创建轮次在写 ref 前崩溃的正常中间态，跳过 I3 修复、按普通失败终态化（硬停期间回 `Queued`）；有墓碑 → 该行本应被 B3 拒绝，属异常残留，登记告警；**merge 行无 `main@P` 行** → 按缺失拒绝语义登记（不修复）（reaper 持锁，成本为每行一次 resolve，不在 B3 热路径）——这是陈旧行修复的统一兜底，覆盖「意图提交后、SAVEPOINT 提交前崩溃」「意图提交前崩溃（该情形无陈旧行产生，校验幂等通过）」全部窗口。随后（写墓碑 + 删行 / 创建继任行 + `superseded_by`，幂等守卫见 B4）；重排意图的补做把旧行终态定为 **`Cancelled`（`failure_type=Conflict`）并写 `superseded_by`**——B2 的跟随逻辑按 `superseded_by` 工作，`Failed` 终态的旧行不可跟随，会使继任行对重试者不可见；其余意图补做后行终态 `Failed`。无意图的行直接标 `Failed`——**例外（仅适用于硬停置位**之前**已在 `Running` 的行）**：`hard_stopped` 置位后被 reap 的存量 `Running` 行**条件重置回 `Queued`**（硬停不是轮次失败，排队意图保留，等待人工 clear）；而**本次 reap 运行新检出的基线失配行不受此例外保护**——其结局是终态 `Failed(QueueBypassDetected)` + `hard_stopped`（硬停由它触发，重置回 Queued 会让绕过源继续排队）。拿不到锁 ⇒ 锁被占用——**持有者不一定是轮次**（物化插入与 ADR-TP-20 的对账/巡检批次也持锁），因此跳过与告警都以「存在 `Running` 行」为门：有 `Running` 行 ⇒ 可能是轮次被锁竞争阻塞，本轮不 reap 并按持有时长告警；无 `Running` 行而锁被占用 ⇒ 物化/巡检持有，正常让行，不告警（`Queued` 行的心跳清理不受影响；`hard_stopped` 置位期间心跳清理**整体冻结**——孤儿行保留排队意图，等待人工 clear，避免把被硬停暂停的等待者误判为孤儿）。试锁是非阻塞的、随 reaper 自己的短事务立即释放，不会阻塞等待中的 B3。**活性取舍（ADR-TP-03）**：reaper 可在 B2.5 提交后、B3 取锁前的间隙 reap 活轮次，工作进程随后 fencing 弃权（ClaimLost）——安全由 fencing 保证，代价是一次可重试的客户端失败；调用方按 1.11 收养/新建重试，`ClaimLost` 指标观测其频率。这个构造使 reap 与 B3 **互斥由数据库保证**，不存在「reaper 读 pg_locks 之后、写状态之前轮次才上线」的 TOCTOU 窗口——先查后写的只读探测不可用，`pg_try_advisory_lock`（会话级）会把锁留在连接上同样不可用。**重排意图补做是 reaper 的职责而非可选项**：冲突重排采用「先持久化意图、再 SAVEPOINT 完成」，意图提交后、SAVEPOINT 提交前的崩溃把完成职责交给 reaper——没有它，冲突重排的继任行会丢失（陈旧行修复不依赖意图，由 `expected_*` 比对 + 通用 I3 修复兜底）。
2. **崩溃恢复不再依赖 `stuck_timeout` 拖时**。旧设计中崩溃的 `Running` 行要等 `stuck_timeout`（必须大于 B3 最坏时长，可能是分钟级）才能被安全清理，期间所有等待者被 `wait_timeout < stuck_timeout` 的大量取消波及。新设计下锁空闲即证明持有者已死，清理是即时的；`stuck_timeout` 降级为**告警阈值**——锁被持续持有超过它说明存在病态慢轮次，触发告警与（可选的）人工介入，不参与正确性判定。B3 的成本上界见 ADR-TP-11（索引重建已移出临界区）与 ADR-TP-17（链长上限）。
3. **工作进程侧的 fencing**（B3 首句）：认领（B2.5）与取锁之间存在窗口——reaper 可在窗口内持锁把本行标 `Failed`。工作进程持锁后重读本行，非 `Running` 即弃权回滚并向调用方返回可重试失败（`ClaimLost`）。没有这一步，reaper 标完 `Failed` 后工作进程仍会照常执行 B3 并改写根树，随后 `status=Done` 的条件更新命中 0 行——数据面已变更而队列行终态为 `Failed`，恰是必须避免的静默矛盾。

**卡死轮次的升级路径（watchdog / runbook）**：试锁拿不到 ⇒ 有轮次在跑 ⇒ 若该轮次**活着但卡死**（事务悬挂在慢存储、死锁等待等），锁会被无限期持有，队列停摆——仅靠告警不够，必须有升级动作：

1. **告警与观测**：锁被持续持有超过 `stuck_timeout` 触发告警，指标附 `pg_stat_activity` 中持锁会话的 pid、事务起点与当前查询快照，供人工判断「慢」还是「死」。
2. **升级程序（runbook，人工授权执行）**：确认持锁会话即为停滞轮次后，`pg_terminate_backend(pid)` 终止之——事务整体回滚（B3 单事务保证无部分状态），advisory lock 随连接释放，reaper 下一轮把该 `Running` 行标 `Failed`，队列恢复。终止对象必须经 `pg_locks`/`pg_stat_activity` 交叉核对确系 MONO_WRITE_LOCK 的持有者，防止误杀。
3. **可选的自动 watchdog**：部署可显式开启自动终止（默认关闭）——条件为「`MONO_WRITE_LOCK` 被持续持有超过 `stuck_timeout × N` 且存在非终态 `Running` 行」（`started_at` 写于认领时刻而非 B3 开始，不能与后端事务起点精确匹配，不作匹配条件），仍属 fail-closed 动作，触发即告警。

配验收：注入持锁睡眠的假死 B3 → 告警触发；执行升级程序后队列恢复，ref/tree 无部分状态，该行终态 `Failed`。

另外，B3 的 `status=Done` 与 B4 的 `status=Failed` 都应带 `WHERE status='Running'` 条件，使被 reaper 抢先改过状态的行不会被静默覆盖回去；fencing 之后该条件命中 0 行属理论不可达，一旦出现即按缺陷 fail-closed 处理并告警，而不是继续提交。

**1.7 控制面**

沿用 `merge_queue_router` 的形状：队列查询（深度、head、running 项、各项 path/kind/requester/等待时长）、`pause` / `resume`（维护窗口，清 `paused`）、
**存量端点的逐一映射（吸收后不得出现指向死状态的端点）**——`status`/`list`/`stats` → `push_queue` 等价查询（路径/响应形状保留，数据源换表）；`cancel` → 本节 `cancel`（仅 `Queued`）；`retry` → **重新入队并同步等待执行**（caller-owned 模型下重试者即执行者：以原行载荷新建行、幂等键不变，随后走 B2/B3 直至终态并返回真实结果——不允许「已接受未执行」的返回，那会制造无执行者的孤儿行）；`remove` → 废弃（410，登记迁移说明——队列行的删除破坏台账完整性，不留此口）；`cancel-all` → **410 + 迁移说明**（批量取消破坏逐行台账与先到先得语义，逐项 `cancel` 保留）；UN-25 freeze 的挂点随 B3 执行期重查迁移。每个端点配映射后行为一致的验收。
**`clear-hard-stop`**（独立运维操作，清 `hard_stopped` 并留审计日志——与维护性 resume 分离，防止把 bypass 硬停当维护窗口顺手清掉）、单项 `cancel`。**`cancel` 只允许作用于 `Queued` 行**（条件更新 `WHERE status='Queued'`，与认领竞争时先到先得、败者可见地失败）；`Running` 行不可 cancel——它已在临界区内执行，取消它会与 B3 的 `Done` 写入竞态并留下「数据已提交而行状态为 `Cancelled`」的矛盾；需要中断执行中轮次时走 `pause` + 等待排空或运维干预。指标：队列深度、等待时长 P50/P99、轮次时长 P50/P99（作为 `stuck_timeout` 告警阈值的标定依据）、失败率、**CAS 断言失败计数（应恒为 0）**、**fencing 弃权（ClaimLost）计数**（安全弃权，非零提示 reaper 与慢认领竞争，观测值）、**锁持续持有超过 `stuck_timeout` 的告警**。

**1.8 与现有 merge queue 的关系**

`merge_queue` 被 `MonoWriteQueue` **吸收**，不并存——两个队列各管一部分根树写入者等于没有全序。可复用的是**四层形状**（storage/service/router/DTO）与既有语义资产：`requester`（UN-18）、freeze 语义（UN-25）、位置查询与统计 API、HTTP 表面。**不复用**其枚举——`QueueStatusEnum`（`Waiting/Testing/Merging/Merged/Failed`）是 CL 工作流状态机，与本队列生命周期不同构，本队列为新实体配新枚举（`push_queue_status_enum` / `push_queue_kind_enum` / `push_queue_failure_enum`，见 1.4），走独立迁移，不动既有列；UN-25 的 freeze 契约（`un25_freeze.rs`）现以 `Failed + SystemError` 表达，新枚举保留 `SystemError` 变体并在迁移时逐条映射。**不可复用**其互斥（`AtomicBool` → advisory lock）、claim（裸 SELECT → `WHERE status='Queued'` 条件写回）与序号（毫秒时间戳 → `bigserial`）实现。冲突重排的语义映射见 ADR-TP-05 影响段（关行重入队，不改 id）。

**Sunset（[`plan-20260910.md`](../plan/plan-20260910.md)）：** 下列「存量行迁移与滚动部署」及 `merge_writer` 双写者开关已废止。CL merge 只经 MonoWriteQueue；无该配置键；不吸收 `merge_queue` 行（表已 DROP）。下文保留为阶段 1 设计时的历史叙述。

**存量行迁移与滚动部署**：切换步骤必须显式设计，防止新旧两套执行器混跑——(1) 切换前**排空**：置 `merge_queue` 不再接受新入队，存量 `Waiting/Testing/Merging` 行由旧 processor 执行至终态；(2) 终态后一次性数据迁移（`Waiting/Testing → Queued`、`Merging → Failed(MergeFailure, interrupted)` 或等其终态后迁移、`Merged/Failed` 按映射归档）；(3) **单写者切换**：以配置开关（重启生效）在旧 processor 与 `MonoWriteQueue` 之间二选一，配置校验拒绝两者同时启用；(4) 混合版本部署窗口内，旧实例的 processor 必须因开关关闭而不再启动。配一条「开关双开拒绝启动」的校验测试。

**1.9 非推送写入者到队列模型的映射**

ADR-TP-04 的等待载体是 receive-pack 连接，但硬约束 2 列出的写入者里 `merge_cl_unchecked`（HTTP 请求）与 ImportRepo attach（管理操作）没有这个载体。三类写入者以 `kind` 列判别，字段与等待方式如下，这张表是阶段 1 的交付物之一：

| 维度 | MonoRepo push（**仅 trunk 形态**，`kind='push'`） | CL merge（review 形态，`kind='merge'`） | ImportRepo attach（`kind='attach'`） |
| --- | --- | --- | --- |
| 是否入队 | trunk 形态入队；**review 形态不入队**（只写 `refs/cl/*`，不触碰根树与 `main` 行，见事实校准 1） | 入队 | **含分支命令时入队**；纯删除式 attach（无非零分支命令，`import_repo.rs:450-475` 只删 ImportRepo 自身 refs）不入队、登记为范围外 |
| 等待载体 | receive-pack 连接在 B2 阻塞 | HTTP 请求 handler 在 B2 阻塞 | 调用方任务在 B2 阻塞 |
| `operation_id` | 指纹 `old_id→new_id`（同 tip 不同基线是不同操作，见 1.11） | `cl.link`（不用 `to_hash`——不同 CL 可共享区间端点） | **确定性导出**：`hash(仓库标识 ‖ 规范化命令描述符)`——attach 由 receive-pack 自动触发、git 重试不带令牌，内容寻址是唯一重试稳定的来源（1.3） |
| `path` | 被推路径 `P` | CL 的 `path`（`mega_cl.path`） | attach 的目标路径 |
| `old_id` | `cmd.old_id`（B3 权威闸门比对项） | **信息性记录**（入队时点的 tip 快照），**不参与闸门**——merge 落地 parent 按现状取 `refs/cl/<link>` 行的 tip（命名分歧时落 main head，GAP-07），执行时点重读，天然吸收并发推进 | 入队时点的根快照 hash，**仅诊断用**——attach 的根依赖准备全部在 B3 锁内重做（见下） |
| `new_id` | `cmd.new_id` | **入队时点** `cl.to_hash` 的快照（信息性；执行按**当时** `to_hash`，rebase 后两者可不同——落地 commit 以 `landed_commit_id` 及其 parent 记录） | 仓库内容 commit（确定性，非根依赖） |
| 根依赖准备的时机 | B3 锁内（本就如此） | B3 锁内（本就如此） | **B3 锁内重做**——现状 attach 在入队/事务前预计算根快照与根 commit（`import_repo.rs:518-550`），排队期间根可能被前序轮次推进，预计算值一律作废；否则其 CAS 会对合法前序轮次误报 stale，与「删除重试循环」（ADR-TP-05）矛盾 |
| `landed_commit_id` | `cmd.new_id` | 落地的合成 commit id（B3 回填） | 落地的根 commit id（B3 回填） |
| `requester` | **token 名**（阶段 5 落地后；落地前过渡期取 commit committer email 并注明未经认证） | 合并请求的 subject（UN-18 现状） | 管理操作的 subject |
| `failure_type` | `PushFailure` | `MergeFailure`（冲突重排用 `Conflict` 关行重入队，见 ADR-TP-05） | `AttachFailure` |
| 失败语义 | 不冻结（ADR-TP-07） | 不冻结；冲突 → 队尾重排 | 不冻结；`MAX_ATTACH_ATTEMPTS` 重试循环**删除**（ADR-TP-05） |
| `heartbeat_at` | 等待循环刷新 | 同左 | 同左 |

HTTP 请求的等待需要一个上界与客户端可读的超时响应——`wait_timeout` 到期时返回 503 加可诊断信息，语义与推送被拒一致。UN-25 的 authz freeze 是独立于本表的路径：它冻结的是**具体队列项**，与 ADR-TP-07「轮次失败不冻结队列」不冲突，迁移时须逐条核对而不是整体套用。

**CL merge 的 `new_id` 是语义锚点，不是落地的 tip。** 这是三类写入者里唯一需要单独说明的一条。核实 `merge_cl_unchecked`（`mono_api_service.rs:2612-2616`，执行时读取当时的 `cl.to_hash`）与 `process_ref_updates`（`:646`）：merge 落在 `main@P` 的是一个**新合成的 commit**（`Commit::from_tree_id(合并后的子树, parent, "cl merge generated commit")`），**执行时点**的 `cl.to_hash` 只是它的 `parent`——在 CL ref 名与 CL row 的 link 一致时如此；在 GAP-07 的命名分歧下 parent 会落到 main 的 head（见 `:623-636` 的注释与 ADR-MC-01 耦合）。因此行上的 `new_id`（入队时点快照）与实际 parent 可因 rebase 而不同，落地面以 `landed_commit_id` 及其 parent 为准：

- **B3 对 merge 是第三种形态**：既不是 `N == 1` 的直接落地，也不是 `N > 1` 的 squash，而是「合成 commit，`parent` = 执行时点的 CL tip 或 main head，`tree` = 合并后的子树」。merge 走的是既有的 `apply_update_result` 路径，阶段 1 只把它纳入队列闸门，**不改变它的落地形态**。
- **I4 的区间语义对 merge 行需要限定**：merge 行落地的 commit **不在** `(old_id, new_id]` 区间内（它是 `new_id` 的子代）。用该区间重建「这次写入写了什么」对 merge 行会得出错误结论；merge 行的落地对象须由 `push_queue.finished_at` 时点的 `main@P` 反查，或在实现时为 merge 行额外记录落地 commit id。
- **GC root 对 merge 行仍以 `new_id` 为准**：落地 commit 由 `main@P` 直接可达，本就不需要显式 root；真正会随 CL 生命周期失去引用的是 CL 链（`new_id` 及其祖先），那正是 `new_id` 该被当作 root 的原因。

**关于硬约束 8：merge 行没有 non-fast-forward 闸门，阶段 1 对 merge 的全部行为变化如下，须如实陈述**。merge 的落地 parent 按现状取 `refs/cl/<link>` 行的 tip（命名分歧时落 main head，GAP-07 保留），根树在锁内重读，因此**不存在**「陈旧基线拒绝」：一条 merge 不会因为等待期间该路径 tip 被推进而被拒。阶段 1 给 merge 带来的真实行为变化只有四类，前三类是运维面而非语义面：

1. **背压**：队列深度超限或 `pause` 期间，merge 请求被拒（现状：merge 排队无上限）。这是背压设计的显式选择（ADR-TP-08），不是静默语义变化。
2. **全序重排**：merge 与其他写入者（attach、trunk push）进入同一 FIFO，两条 merge 的相对顺序仍由入队序决定，与现状 `position` 序一致；不同 kind 之间的交错顺序与现状可能不同——现状没有任何跨写入者的全序可言，无从比较。
2a. **执行模型由异步转同步（登记在案的契约变化）**：现状 merge 是「入队即返回 + 后台 processor 异步执行」（`add_to_merge_queue` 立即返回，`mono_api_service.rs:3724-3755`；`ensure_merge_processor_running` 起后台 processor，`:4343-4386`）。吸收后 **全部 merge 入口同步化**——包括直连路由 `/merge` 与 `/merge-no-auth`（`cl_router.rs:218-223/279-304`）：它们现状直接调 `merge_cl`（`:2107-2128`）在队列外执行 `merge_cl_unchecked`，是不入队的根树写入者，与排队的操作竞态；改造后走「既有入口预检（`cl.from_hash != main` 拒绝，现状保留）→ B1 入队 → 同步等待 B2/B3」。`/merge-queue/add`（`merge_queue_router.rs:43-55`）同理**同步化**——它现状入队即返回、依赖被退役的后台 processor，同步化后其 `Queued` 行不会成为孤儿；响应形状保留，语义变为「已执行完毕」（选定方案，不做 410 废弃备选）。后台 processor 退役——没有在线的调用方就没有 B3 的执行主体。1.8 所说「HTTP 表面保留」指路由与请求/响应形状；执行语义的异步转同步必须在迁移文档中显式声明。`wait_timeout` 只约束 **B2 等待（Queued 状态）**：到期仍未取得轮次 → 503；一旦认领进入 B3（Running），调用方等待的是真实终态，不受 `wait_timeout` 截断——执行时长上界由 B3 的成本上界（ADR-TP-11/17）与 `stuck_timeout` 告警保障，响应丢失由 1.11 的回放兜底。
3. **执行期门控原样重查**（CL 存在性、UN-19、冲突、GPG，事实校准 6），冲突重排为关行重入队。所有门控的判定结果与现状一致。**既有入口预检原样保留**：`merge_cl` 在入口处拒绝 `cl.from_hash != main`（`mono_api_service.rs:2107-2126`，`/merge` 与 `/merge-no-auth` 经 `cl_router.rs:203-223/270-304` 调用）——这是现状行为，硬约束 8 要求不动；「merge 无 push 式 old_id 闸门」指队列**不加新的**闸门，不是移除既有入口预检。排队等待期间的基线漂移由执行期冲突重查处理（Conflict → 重排，非终态）。**其中冲突重查对「tip 推进」敏感**：`check_merge_conflicts`（`mono_api_service.rs:4626-4658`）比对 `cl.from_hash` 与当前 `main@path`，tip 被并发推进且 CL 基线陈旧时返回 `Conflict` → 队尾重排——这与现状完全一致（现状同样重查并重排，`:4419-4429`），重排不是终态失败，CL 最终照常合并。
4. **陈旧物化 main 行的 merge 被拒（缺陷修复，ADR-TP-20；与 CL 基线漂移是两回事）**：merge 会**覆写** `main@P` 行，覆写一条 `ref_tree_hash` 落后于根树的陈旧**物化**行会把陈旧 tip 永久烘进主干历史——那正是事实校准 4 静默损坏的变体。B3 对覆写前的 `main@P` 行执行树哈希断言（先于业务门控重查），检出即拒绝并修复（墓碑续接），CL 保持 open。**修复后重试不承诺直接成功**：重物化产生的新 tip 与 CL 的 `from_hash` 基线的关系由既有冲突重查裁决——基线陈旧 → `Conflict` → 队尾重排或要求 rebase（与任何其他 merge 相同的常规门控，不再被陈旧行阻断）。现状下这类 merge「成功」落地，但落地结果带有陈旧前史，不属「能成功且正确」的范畴。落地 parent 的选取（`refs/cl/<link>` 优先，GAP-07 现状）不受影响。

推论：**没有任何在现状下正确且落地方正确的 merge 会因门控被终态拒绝**；会被拒的只有被运维面（背压/pause）拒绝的请求、依据陈旧物化 main 行（缺陷状态）的请求，以及按现状语义重排（Conflict → 队尾，非终态）的冲突。该陈述取代本节此前「给 merge 补 non-fast-forward 闸门」的提法——那个闸门只属于 `kind='push'`。

**1.10 阶段 1 代码交付物清单**

除队列本身外，以下现有代码缺陷在阶段 1 一并修复，否则 B3 的硬要求无法落地：

1. **事务内 ref 更新变体**：`batch_update_by_path_concurrent` 经 `FuturesUnordered` 在连接池上并发执行，无法加入 B3 事务（事实校准 16）。新增接受 `&DatabaseTransaction` 的**顺序**更新变体（并发在单事务内没有意义），B3 全部 ref 更新改走它。
2. **缺失行静默跳过修复**：同函数对 `mega_refs` 中不存在的 `(path, ref_name)` 行静默跳过（事实校准 4）。新变体**必须提供事务内 upsert/create 原语**——服务于 push 的创建语义（写入模型）插入 `main@P`；merge 不使用它（缺失行按现状拒绝，见 1.5 merge 分支）。禁止静默吞掉。
3. **attach 重试循环删除**：`import_repo.rs:507-511` 的 `MAX_ATTACH_ATTEMPTS` 循环随吸收删除（ADR-TP-05）。
4. **`escape_like` 助手**：`src/` 中不存在 LIKE 转义助手（sea-orm 提供 `LikeExpr` 但无转义器）。新增 `escape_like`（转义 `%`、`_`、`\`），配 `%`/`_`/`\` 三元单测——阶段 2 的候选集查询与 ADR-TP-20 的墓碑查询都依赖它，这是事实校准 8 第三处缺陷的唯一修复点，一个静默的 off-by-one 就会在新函数里复刻 `remove_none_cl_refs` 的 bug。
5. **`remove_none_cl_refs` 的事务内变体**：现状函数自建连接（`mono_storage.rs:77-84`），在 B3 事务外执行会使 merge 落地分裂为两个提交窗口——根已提交而后代 ref 仍陈旧，崩溃即留下 I3 破损状态，阶段 1 的「B3 单事务」承诺随之落空。阶段 1 为 merge 分支提供接受 `&DatabaseTransaction` 的变体（**同时修复 LIKE 无组件边界与不转义元字符两处缺陷**，事实校准 8），阶段 2 以 `advance_descendant_refs` 取代之。
6. **树项插入原语（创建语义的前置）**：`build_result_by_chain`（`mono_api_service.rs:570-616`）经 `update_tree_hash`（`:551-563`）只按名称**定位**既有 item，缺失即报 `Tree item '<name>' not found`，无法**插入**新条目；`search_tree_for_update`（`tree_ops.rs:263-298`）对缺失组件硬报 `Path 'X' not exist, please create path first!`。创建语义（写入模型「真创建」）需要一个 **insert-or-replace 变体**：沿路径插入缺失的 `TreeItem`（`TreeItemMode::Tree`，遵守 `Tree::from_tree_items` 的排序约定），支持多级创建（`/a/b/c` 中 `/a/b` 缺失时逐级补齐——**缺失祖先的插入是该原语的显式设计行为**，不属于「禁止静默跳过」的范畴：后者约束的是 ref 批量更新对既有更新意图的静默丢弃；本原语的插入与交付物 2 的 ref 行 upsert 都是显式创建动作）。B3 创建分支与 `search_tree_for_update` 的调用点改用该变体。配单级与多级创建回归。
7. **`apply_update_result` 全量事务化（含 merge 的全部副作用）**：现状的数据面写入分散在多个自管连接上——ref 更新走连接池（`mono_api_service.rs:2693-2696`），commit 与 tree 的保存各自独立（`:2701`/`:2717`）。B3 的「单事务」要求 **ref、commit、tree 三类写入全部穿入同一 `&DatabaseTransaction`**（对象先落、ref 后落，同一提交点原子可见），这是交付物 1 的推广，不止于 ref 更新变体。**merge 的 CL 状态与 conversation 写入同样在 `apply_update_result` 之后发生（`:2625-2635`），必须一并纳入 B3 同一事务**——否则数据落地后、状态写入前的一次失败会留下「根已推进而 CL 仍可重试」的状态，重试造成二次合并；三者同库（sea-orm/Postgres），无跨存储障碍。若实现中发现某副作用确在异构存储，该副作用改走以 `landed_commit_id` 为幂等键的补偿状态机，并在评审中单独登记。
8. **墓碑机制的最小交付（ADR-TP-20 的前置）**：`mega_ref_tombstones` 表迁移（2.5 的表结构）、修复路径（B3 内 SAVEPOINT 同事务：撤数据写、写墓碑、删行、标终态，原子提交，无崩溃间隙）与两条物化路径的**续接集成**（`refs_with_head_hash`/`create_repo_commit` 懒生成前查墓碑、命中则以 `last_commit_hash` 为 parent、复活后删墓碑行）——树哈希断言的修复在阶段 1 就需要完整的「写墓碑 → 续接物化」闭环，不能等到阶段 2。阶段 2 在其上补齐后代删除分支与复活语义的完整验收。
9. **测试专用 `push_policy` 开关**：配置项读取与推送落地路径的 kind 分派（B0/B1/B3 push 分支），使阶段 1 的 push 用例可运行；**不含**阶段 4 的协议层接线（`finalize_receive_pack` 条件化、CL 管线关闭、OpenAPI 表面、trunk 启动校验）——那些留在阶段 4。
10. **trunk 形态的 receive-pack 桥接**：现状 `build_push_chain` 对空链/全已知链返回 `None` 并以 `Noop` 短路退出（`monorepo.rs:1191-1247`，`PushChain::resolve` 的 Chain 在此被丢弃；`validate_incoming_push` 在 `:1174-1179` 随即成功返回）——trunk 形态要求这些推送照常入队（B3 的 N=0 分支给出权威结局，含空 pack 且 `old_id != new_id` 的情形）。交付：trunk 形态下该短路改为**构造持久化描述符（N/commits/fork_base）并继续走 B1/B3**；review 形态保留 `Noop` 现状（硬约束 8）。配空 pack 与全已知对象两种入队测试（断言 B1 行出现、B3 执行）。

> **验收标准**（阶段 1 以 merge 与 attach 为主要执行体；push 用例在 1.10 交付物 9 的**测试专用 `push_policy` 开关**下运行——该开关只接通推送落地路径的配置读取与 B3 分支，不含阶段 4 的协议层接线/CL 关闭/OpenAPI 表面）：
> - ✅ 20 个**互不嵌套**不同路径的并发 CL merge 全部成功；根上 roll-up 的 parent 链顺序逐一等于 `push_queue.id` 顺序；根树包含全部 20 处改动
> - ✅ 伴随用例（阶段 1 的预期失败形态）：互嵌路径（`/a` 与 `/a/b`）的两次 merge 串行执行——`/a` 的 merge 经阶段 1 仍沿用的 `remove_none_cl_refs` 删除 `main@/a/b`，随后 `/a/b` 的 merge 被 1.5 的「缺失 main 行 → 拒绝」拒绝（可诊断）；**续接成功形态在阶段 2.8 切换后验收**（阶段 2 验收的 merge 触发回归）
> - ✅ 注入 B3 中段延迟的探针，确认任意时刻至多一项处于 `Running`
> - ✅ 执行中 `kill -9`：ref 与 tree 均未变更，队列行停在 `Running`（认领已独立提交）；advisory lock 随连接释放，reaper 下一轮持锁即把它标 `Failed`（无需等 `stuck_timeout`），下一项正常上场
> - ✅ **B2 等待中 `kill -9`**：队列行停在 `Queued` 且心跳停止；`heartbeat_timeout` 后被 reaper 标 `Cancelled`；期间与之后的轮次均能正常取得执行权（孤儿队首死锁的回归锁定）
> - ✅ **fencing 回归**：注入「认领提交后、取锁前」延迟的探针 + 同窗口触发 reaper，B3 弃权回滚（ClaimLost），根树与 ref 无任何变更，该行终态 `Failed`
> - ✅ **慢轮次不被误伤**：注入一次时长超过 `stuck_timeout` 的 B3，reaper 因锁被持有而跳过 `Running` 行，该行最终为 `Done`；锁释放后被 reaper 扫过亦不被改动（终态幂等）；告警指标计数
> - ✅ CL merge、ImportRepo attach 两类写入者并发时全部经同一队列串行，`push_queue` 中两类行的字段按 1.9 的表取值；merge 行的 `old_id` 为信息性、不影响落地
> - ✅ merge 冲突注入：该行关闭为 `Conflict` 并在队尾出现同 CL 的新行，id 严格递增；最终合并成功（ADR-TP-05 的重排映射）
> - ✅ 根更新唯一性：抵达根写入点的成功轮次**恰好执行一次根 CAS 断言写**（早期闸门拒绝的轮次走 B4、无根写入）；根 commit 仅在根树变化的轮次前进，净零轮次同值写不前进（配净零用例）；merge/attach 轮次不触发第二个根更新点（对 `apply_update_result` 重构后根更新走 CAS 原语的直接断言）
> - ✅ CAS 失败的 fail-closed：注入绕过写入者使根 CAS 命中 0 行 → SAVEPOINT 同事务完成数据回滚 + `hard_stopped` 置位 + 行终态 `Failed(QueueBypassDetected)`（判定经 `expected_*` 基线）；**两个崩溃窗口各有归属**——CAS 执行前崩溃（行仍 Running）由 reaper 的 `expected_*` 比对兜住（绕过根已被写入 → 硬停）；CAS 失败后、终态提交前崩溃由 reaper 同一比对兜住。两窗口均配交错探针
> - ✅ `cancel` 规则：仅 `Queued` 行可被取消（`WHERE status='Queued'` 条件更新）；对 `Running` 行的 cancel 请求可见地失败；取消与认领并发时恰好一方生效
> - ✅ CL merge 行：入队时点 `new_id` 等于当时 `cl.to_hash`，而 `main@P` 的落地 tip 是以**执行时点** `to_hash` 为 parent 的合成 commit，二者不相等（锁定 1.9 的语义锚点说明）；rebase 场景下 `landed_commit_id` 的 parent 等于执行时点 `to_hash` 而非 `new_id`
> - ✅ CAS 断言失败后队列自动 `hard_stopped`，该行终态为 `Failed` 且 `failure_type=QueueBypassDetected`（判定依据：B2.5 认领时落库的 `expected_*` 基线与现读根比对，无独立 CAS 意图）；`hard_stopped` 置位期间 B2/B2.5/B3 全部弃权（在队项不执行）；`clear-hard-stop` 前不接受任何轮次，`resume` 不清除它
> - ✅ **三 kind 的 CAS 意图覆盖回归**：push/merge/attach 各注入「CAS 失败后、意图清除前崩溃」→ reaper 依据 `expected_*` 身份对判定并硬停（意图为全 kind 通用前置，B3）（1.5）
> - ✅ **attach 的 reaper I3 语义回归**：attach 轮次崩溃 → reaper 终态化时对其 path 跳过 I3 校验（无 `main@P` 行属正常），不误报陈旧、不误修（1.6）
> - ✅ **reaper 通用 I3 修复回归**：注入「B3 在陈旧行检出前崩溃」且根未动 → reaper 终态化时对该行 path 执行 I3 校验并完成墓碑修复，`refs_with_head_hash` 不再返回陈旧行（1.6）
> - ✅ **认领时基线回归**：注入「B2.5 认领提交后、B3 取锁前队列外根写入」→ reaper/B3 依据认领时 `expected_*` 判定不一致 → `hard_stopped` + `QueueBypassDetected`（预标记崩溃窗口闭合，1.5/1.6）
> - ✅ 故意保留一条绕过队列的写入路径时，CAS 断言触发 fail-closed（该用例同时是不变式 I5 的活文档）
> - ✅ 队列深度上限 + 1 的请求被拒且错误可诊断（B1 原子判定：并发到达时恰拒第 `max_depth + 1` 个）；`pause` 后被拒、`resume` 后恢复；`hard_stopped` 置位期间新入队被拒而 `Done` 回放不受限，`clear-hard-stop` 后恢复
> - ✅ 同一路径并发两次推送入队，第二次被 `push_queue_active_push_path` 拒绝
> - ✅ 队列外物化竞态回归（ADR-TP-20）：物化事务在锁外基于旧根遍历后挂起，B3 提交新根，物化随后持锁插入——根快照校验检出不一致，**放弃插入**；该路径的下一次 advertise 基于新根重新物化，产出正确行
> - ✅ 陈旧行修复回归（ADR-TP-20）：手工注入一条 `ref_tree_hash` 落后于根树的 `main@P` 行，下一次推送被树哈希断言拒绝，**B3 内 SAVEPOINT 同事务**写入墓碑（`last_commit_hash` = 陈旧 tip）并删除该行（撤数据写与修复原子提交，无崩溃间隙）；紧随的 advertise 从墓碑续接物化，新行 parent 等于陈旧 tip（I1 保持，无 unrelated history），再推送成功（拒绝-修复-续接-重推闭环，无死局）
> - ✅ 幂等重试回归：同一 `operation_id` 的 push/merge 轮次提交后丢弃响应再重试——push 命中 `Done` 行按 `landed_commit_id` 回放 report-status ok（**含 N > 1 情形**：`main@P` 已是 squash id，重试不被 non-fast-forward 误拒），merge 同理回放；均不产生第二个落地 commit、不产生新队列行（1.11）
> - ✅ 创建语义回归：向全新路径 `old_id = ZERO_ID` 推送——N = 1 时 `main@P` 精确等于客户端 tip；N = 2 时落地 parentless squash commit，message 完整枚举 2 条且**无 `Mono-Squash-Range`**，`push_queue` 行的 `new_id`（链 tip）可解析回全部原始 commit（写入模型/I4）
> - ✅ **多级创建回归**：`/a/b/c` 三级均不存在时推送 `/a/b/c` → 树项插入原语逐级补齐（根树含完整链路），`main@/a/b/c` 落地为客户端 tip；单级创建（父级已存在）同验（交付物 6）
> - ✅ 墓碑拒绝回归：路径存在墓碑（有历史）时 `old_id = ZERO_ID` 的直推被 B0 拒绝且提示先 advertise 重新物化；重新物化后续接 tip 是墓碑 `last_commit_hash` 的直接后代（I1）
> - ✅ 净零轮次的断言在场：注入绕过队列的根写入者于「B3 读根后、提交前」提交 → 净零推送的同值 CAS 命中 0 行，fail-closed（tripwire 不因净零缺席）
> - ✅ **认领原子性回归**：注入「B2 观察到无 Running 后、B2.5 之前有前序项完成认领」的交错探针，B2.5 的原子条件更新命中 0 行并回 B2，任意时刻至多一项 `Running`（I6）
> - ✅ **双认领者并发回归**：两个认领者对各自 Queued 行同时发起 B2.5（READ COMMITTED）→ `queue_control` 行锁串行化，至多一项 `Running`，另一者 0 行回 B2（1.5）
> - ✅ attach 排队回归：attach 入队后、执行前有前序轮次推进根 → B3 锁内重做根依赖准备后正常落地，CAS 不误报 stale；`MAX_ATTACH_ATTEMPTS` 循环不存在
> - ✅ merge 分支后代删除处于 B3 事务内（交付物 5）：注入 merge 落地中段 `kill -9`，根与后代 ref 同回滚，无「根已提交、后代陈旧」状态
> - ✅ 数据面全量事务化（交付物 6）：注入 commit/tree 保存中段 `kill -9`，对象与 ref 同回滚，**无孤儿元数据与 ref 状态**（A 段已落对象库的 unpacked 对象按现状成为不可达垃圾——`monorepo.rs:248-257` 已记录该现状——由对象 GC 处理，不属 B3 元数据原子性范畴）
> - ✅ merge 副作用原子性（交付物 6）：注入 CL 状态/conversation 写入前 `kill -9` → 全量回滚，根未推进、CL 仍 open，重试恰好合并一次（无二次合并）
> - ✅ 活跃操作收养回归：同一 `operation_id` 的 merge 在原行 `Queued`/`Running` 期间重试 → **收养**既有行（B2 等待/认领，多收养者恰一执行者）；原行终态后重试走 `Done` 回放（1.11）
> - ✅ **终态竞态回归**：注入「重试的查重与原行提交 `Done` 并发」的交错探针 → 条件 INSERT 三态判定命中 `Done`，回放而非重复入队（1.11）
> - ✅ **B0–B3 墓碑竞态回归**：注入「B0 预检时无墓碑、B3 锁内判定前一次 ADR-TP-20 修复写入墓碑」的交错探针 → B3 锁内墓碑重查拒绝该创建推送，I1 不破（写入模型）
> - ✅ **创建推送的物化竞态回归**：路径在根树中可解析但未物化，物化在 B0 判「无行」后、B3 锁内重读前插入合成行 → 创建推送得到可诊断拒绝（「路径已在等待期间物化，请 fetch 后重推」）而非通用 non-fast-forward 文案；fetch 对齐后重推成功
> - ✅ **推送指纹回归**：先推 A→C（N = 2，落地 squash S），再推 B→C（同 tip 不同基线）→ 指纹不同，正常排队并被 non-fast-forward 闸门拒绝（不误判为 A→C 的重试）（1.11）
> - ✅ N 计数回归：N 按 `new_id→old_id` 客户端基线段长计数、随描述符持久化——曾被拒的 N=2 推送重试仍走 squash 分支（不因对象已知而退化为 fast-forward，ADR-TP-12）；曾成功者被指纹 `Done` 回放拦截；空 pack（`old_id != new_id`）按 N ≥ 1 正常分流，`old_id == new_id` 的退化重推由 B3 判 no-op 落地（landed_commit_id = tip）
> - ✅ Conflict 重排的 B4 分支回归：冲突行终态 `Cancelled(Conflict)` 与队尾新行在同一事务内出现，新 id 严格递增，三态唯一索引不拦截（B4/ADR-TP-05）
> - ✅ merge 断言先于门控回归：陈旧 `main@P` 行 + CL 基线同时陈旧 → 树哈希断言先拒绝（而非冲突重查先行）；修复后重试进入常规门控，按 `from_hash` 基线陈旧走 Conflict 重排（1.9 第 4 条）
> - ✅ 物化放弃的协议落点回归：注入持续根推进使物化重试耗尽 → advertise 返回可诊断错误而非空 refs/ capabilities-only（ADR-TP-20 第 1 项）
> - ✅ 推送描述符收养回归：push 轮次执行中崩溃 → 重试结局按行状态二分：行仍 `Running` → **收养**（凭持久化描述符执行 B3）；行已被 reaper 终态化 → 以载荷**新建队列行**重推。两者落地结果均与原轮次应得结果一致（N=1 与 N>1 各一例，验证不依赖对象存在性推断）（1.3/1.11/1.6）
> - ✅ 后代查询自我排除回归：在根路径 `/` 落地 merge/推送 → `main@/` 行自身不被当作后代续接，parent 链无自指（2.2）
> - ✅ merge 与 rebase 串行化回归：merge 执行中并发 rebase CL → 状态更新的版本 CAS 命中 0 行，本轮 ClaimLost 弃权，CL 保持 rebase 后状态，重试成功（1.11）
> - ✅ **直连入口入队回归**：`/merge`、`/merge-no-auth`、`/merge-queue/add` 与排队的 push/attach 并发 → 三者的根写入全部经同一队列串行（B1 行可见、无队列外落地）；入口预检行为与现状一致（1.9 2a）
> - ✅ **同指纹并发入队回归**：并发同指纹请求在准入锁下串行 → 恰一个插入成功，另一者收养或回放；无锁退化实现下冲突恢复路径同样可达一致（含胜者回滚的重试路径）（1.5/1.11）
> - ✅ **attach 已物化目标回归**：对已物化路径发起 attach → B3 拒绝且报错可诊断；纯删除式与未物化目标 attach 照常（1.5/1.9）
> - ✅ **attach 祖先/后代物化回归**：对祖先 main 已物化（仅 `/` 除外）、或存在已物化后代 `P/D` 的目标发起 attach → 拒绝且报错可诊断；`P=/` 的 attach 在**无已物化后代**时允许（kind 声明，B0/B3）（1.5）
> - ✅ **B4 修复/重排的崩溃间隙回归**：注入「意图持久化后、SAVEPOINT 提交前 `kill -9`」→ 行停留 `Running` 且 `pending_action` 在，reaper **幂等补做**（墓碑/继任行）后终态化；注入「意图持久化前 `kill -9`」→ 行标 `Failed`、无残缺修复，检测由下次推送与巡检重入（B4/1.6）
> - ✅ trunk 形态 CL 变更面关闭回归：`/create-entry`、`/edit/save` 在 trunk 形态下不注册（或返回可诊断错误），连续操作后 `mega_cl` 行数与 CL ref 行数均为零（4.3）
> - ✅ **授权快照屏障回归**（enforce 模式）：注入「notify 提交后、snapshot 重建完成前」的交错 → 下一轮 B3 的 UN-19 重查等待水位追平后才读快照，不产生陈旧授权判定；`off` 模式无此屏障且行为不变（1.2）
> - ✅ **索引删除收敛回归**：推送 A（id 5）索引后、推送 B（id 6）删除某 blob，A 的滞后任务可能重插该行（已知限制）→ 补偿任务重扫后清除，最终收敛一致（ADR-TP-11：无「永不复活」承诺，删除水位为后续议题）
> - ✅ trunk 分支名准入回归：trunk 形态推送 `refs/heads/dev` → B0 拒绝（唯一公开分支 main）；review 形态行为不变
> - ✅ **FIFO 准入序回归**：并发入队探针断言「id 分配序 = 提交序 = 执行序」——注入「先到事务未提交、后到者先拿大 id」的交错，B2.5 不把后者当队首（B1 准入锁，ADR-TP-06/I6）；深度门槛恰好容纳 `max_depth` 项
> - ✅ **超时认领竞态回归**：wait_timeout 到期 → 调用方放弃（连接拒绝/503），共享行状态不变；仍在心跳的其他收养者继续并正常执行（B2/1.11）
> - ✅ **冲突重排交接回归**：merge Conflict → 旧行 `superseded_by` 指向继任行，调用方自动跟随继任并最终合并成功；继任行不因后台 processor 退役而无人执行（B4/1.3）
> - ✅ **提交后丢响应/崩溃的重排收养回归**：reaper 补做重排后原调用方已不在 → 同指纹重试**收养**继任行（B2 等待/认领），最终合并成功且仅合并一次；多收养者并发时恰一个执行（1.11 收养规则）
> - ✅ **reaper 补做的可跟随性回归**：reaper 补做重排后旧行终态为 `Cancelled(Conflict)` 且 `superseded_by` 可跟随（非 `Failed`），重试者不失去继任行（1.6）
> - ✅ merge 陈旧物化行回归：注入陈旧 `main@P` 行后发起 merge → 树哈希断言拒绝 + 墓碑修复；重新物化后同一 CL 重试进入常规门控，历史中无陈旧 tip；落地 parent 仍按现状取 `refs/cl/<link>` tip（GAP-07 行为不变）（1.9 第 4 条）
> - ✅ merge 冲突重排回归：CL 基线陈旧（tip 已被推进）时执行 → `Conflict` → 队尾重排（非终态失败），重排后成功合并（1.9 第 3 条，与现状一致）
> - ✅ trunk 删除策略回归：trunk 形态下推送 main 删除被拒（UN-16 等价语义）；其余 ref 删除被拒并提示走父路径删除 commit；review 形态的删除行为不变（B0）
> - ✅ 对账与巡检回归：注入一条与根树不一致的存量 main 行 → 启用前对账将其转墓碑；周期巡检在同一谓词上修复后续注入的行（ADR-TP-20 第 3 项）
> - ✅ UN-25 freeze 映射回归：注入 authz freeze 场景，行终态 `Failed` 且 `failure_type=SystemError`，冻结语义与现状契约一致（1.8）
> - ✅ `queue_control` 迁移含单行播种（重复迁移不报错，B1 的 FOR UPDATE 有行可锁）（交付物配套）
> - ✅ 1.10 交付物 1–6 各有直接断言的单元测试（事务内更新、缺失行 upsert/创建、重试循环不存在、`escape_like` 三元转义、事务内后代删除、树项插入）
> - ✅ `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、指定测试用例全绿

---

### 阶段 2 — `advance_descendant_refs`：后代 ref 续接

**2.1 要解决的故障链**

```
1. A clone /project/foo  → 懒生成无父 commit X，落库为 main@/project/foo
2. A 本地提交            → X → a1 → a2
3. B 在 /project 推送（或合入 CL）
4. remove_none_cl_refs("/project") 删除 main@/project/foo
5. A 执行 git pull
6. 服务端重新懒生成无父 commit Y（Y != X）
7. A 拿到的远端历史与本地无共同祖先
   → fatal: refusing to merge unrelated histories
```

在 review 形态下该故障被 CL 合并的低频掩盖；在 trunk 形态下，「在父路径推送」是创建新子目录的主要途径（向未物化路径的首次推送是另一条，见写入模型创建语义），第 3 步是日常操作。

**2.2 候选集查询（同时修复 LIKE 两处缺陷）**

新增函数，不修改 `remove_none_cl_refs` 的签名以免影响 review 路径的既有调用：

```rust
/// 被推路径 P 下的已物化 main ref。
/// 前缀必须带尾斜线（组件边界），并转义 LIKE 元字符。
/// `escape_like` 为阶段 1 交付的新助手（见 1.10 第 4 项），src/ 中现无此函数。
async fn descendant_main_refs(&self, p: &str, txn: &DatabaseTransaction)
    -> Result<Vec<mega_refs::Model>, MegaError>
{
    let prefix  = format!("{}/", p.trim_end_matches('/'));
    let pattern = format!("{}%", escape_like(&prefix));   // 转义 % _ \

    mega_refs::Entity::find()
        .filter(Expr::col(mega_refs::Column::Path)
                    .like(LikeExpr::new(pattern).escape('\\')))
        .filter(Expr::col(mega_refs::Column::Path).ne(p))   // 自我排除：
        // p="/" 时 pattern "/%" 会命中根行自身（现有 remove_none_cl_refs
        // 显式排除请求路径，mono_storage.rs:79-82；根路径 merge 受支持，
        // mono_api_service.rs:2602-2619），漏掉排除会把被推路径自己
        // 当后代续接，制造 parent 自指
        .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME))
        .filter(mega_refs::Column::IsCl.eq(false))
        .all(txn)
        .await
}
```

`is_cl = true` 的 ref 保持不动（review 形态下是后代路径上的 open CL，其基线陈旧由 `ClSyncChecker` 负责），与现况一致。**tag ref 的行为变化（登记在案）**：现状 `remove_none_cl_refs` 删除一切非 CL ref（含 tag API 写入的 tag 行，`mono_api_service.rs:1927-1978`），新查询按 `ref_name = main` 过滤——**tag 被保留是刻意选择**（tag 不参与写入判定，删除它们是现状缺陷的一部分）；配「后代路径上的 tag 行在推送前后不变」回归。

**2.3 子树解析与四种结果**

对每个候选 `D`，取相对路径 `R = D - P`，从 `chain.tip.tree_id` 向下解析：

| 结果 | 动作 |
| --- | --- |
| 解析到 tree hash 且等于 `D.ref_tree_hash` | 跳过（Merkle 性质：hash 相同即整棵子树相同） |
| 解析到 tree hash 且不等 | 续接：合成 commit(parent = `D.ref_commit_hash`, tree = 新 hash)，更新该行 |
| 路径不存在（目录被删） | 落墓碑并删除 ref |
| 存在但 `mode` 不是 `Tree`（目录被文件取代） | 同上 |

第四种必须显式判 `TreeItemMode::Tree`；否则会把 blob hash 写进 `ref_tree_hash`，故障延迟到下一次该路径被 clone 时才在 `get_tree_by_hash` 上暴露。

**2.4 剪枝与记忆化**

- **剪枝**：下行途中一旦某层 tree hash 等于旧树同层 hash，该候选及其更深候选全部判定为未变更，立即中止下行。
- **记忆化**：`HashMap<ObjectHash, Arc<Tree>>` 缓存本轮读过的树；候选按深度升序处理，使浅层结论覆盖深层候选。

成本由**已物化的后代路径数**决定（只有被 clone 或挂载过的路径才会物化），而非由变更规模决定——这是临界区内需要的可预测上界。

**2.5 墓碑与路径复活**

```sql
mega_ref_tombstones(
  path             text not null,
  ref_name         text not null,
  last_commit_hash text not null,
  last_tree_hash   text not null,
  deleted_at       timestamp not null,
  primary key (path, ref_name)
)
```

`refs_with_head_hash`（`monorepo.rs:121`）与 `create_repo_commit`（`utils.rs:362-442`）两条物化路径在懒生成前先查墓碑：命中且**该路径在当前根树中可解析**时，以 `last_commit_hash` 为 parent 物化（续接 commit 的 tree 必须等于 `resolve(root, P)`，I3 才成立）；路径不在当前根树（目录尚未重建）→ **跳过物化**（保持未物化状态，绝不从墓碑凭空造行）；复活（续接物化成功）后删除墓碑行。墓碑检查与**该次物化的全部 ref 行**（一物化可能为多个根 ref 各生成一行，`monorepo.rs:142-198` 的逐行调用须合并进持锁事务）按 ADR-TP-20 持 `MONO_WRITE_LOCK` 短锁并在同一事务完成。**交付时点**：墓碑表迁移、修复路径与续接集成由阶段 1 先行交付（1.10 交付物 8，树哈希断言的修复闭环依赖它）；阶段 2 在其上补齐后代删除分支的落墓碑动作与复活语义的完整验收。这是仅有的两个集成点。

**升级前的历史删除回填（存量数据）**：现状的删除（`remove_none_cl_refs`）不留任何痕迹，「曾物化后被删」的路径与「从未物化」的路径在数据上不可区分——若不处理，升级后向这类路径的创建会以无父 commit 落地，升级前 clone 过该路径的旧客户端将遭遇一次 unrelated history（I1 破损）。阶段 2 迁移提供两条路径，部署时二选一并显式记录：(a) **尽力回填**：遍历对象库中引用过候选路径的 commit（按 message/树遍历的启发式），取最新 tip 回填墓碑——不保证完备，只缩小风险面；(b) **fail-closed（默认）**：不做回填，部署文档显式声明该一次性断裂风险，要求运维在升级前从审计/运维记录整理删除清单并手工回填。配回归：删除 → 升级 → 重建 → 旧客户端推送（(a) 续接成功；(b) 断裂被文档声明覆盖）。

**权衡**：Git 不追踪目录身份，同名路径删除后重建，续接会把两段语义无关的历史串联。取舍依据是二者的失败代价不对称——续接的代价是 `git log` 中多一段前史，不续接的代价是该路径所有 clone 者的本地工作作废（2.1 的故障链原样复现，只是触发更罕见）。

**2.6 索引**

在非 C collation 的 Postgres 上，`LIKE 'prefix%'` 不会走 `uniq_mref_path`（事实校准 12）。补一个贴合候选集过滤条件的部分索引：

```sql
CREATE INDEX mega_refs_path_pattern
  ON mega_refs (path text_pattern_ops)
  WHERE ref_name = 'refs/heads/main' AND is_cl = false;
```

**2.7 事务位置**

必须与阶段 1 的 B3 同一事务提交。若在事务外，崩溃窗口内会出现「根树已更新、后代 ref 仍指向旧树」，违反不变式 I3。读者在 read committed 下只能看到提交前或提交后的完整状态，fetch 与 B3 无需额外读锁。

**这是硬要求，不接受最终一致的替代方案**——后代 ref 是 B3 的 non-fast-forward 闸门的比对对象，滞后会让闸门放行一个基于陈旧子树的推送，静默覆盖掉祖先推送的改动。完整论证与被否方案见 **ADR-TP-19**。

**2.8 merge 分支的后代切换**

阶段 2 的续接逻辑必须同时接管 **merge 分支**的后代处理：`merge_cl_unchecked` 与 B3 merge/push 均调用 `advance_descendant_refs`（同一 B3 事务内，候选集来源从推送链 tip 的 tree 换为合并后子树的 tree，其余四种结果的处理完全一致）。这是**登记在案的行为变化**（硬约束 8 的「缺陷修复除外」）：2.1 的故障链在 review 形态下正是由 merge 触发的（B 在 `/project` 合入 CL → 旧 `remove_none_cl_refs("/project")` 删除 `main@/project/foo`），只修 push 分支不修 merge 分支等于没修——review 形态才是该故障的主战场。**已落地（TP-14）**：`remove_none_cl_refs` 不再出现在 merge/push 落地面；legacy `merge_cl_unchecked` 将路径/根写入与 `advance_descendant_refs` 放在同一事务（CL 状态与 conversation 仍在提交后写入）。

> **验收标准**：
> - ✅ 断裂回归：A clone `/project/foo` 并本地提交 → B 在 `/project` 推送 → A `git pull` 成功并可继续推送（当前实现下此用例必失败）
> - ✅ 无辜删除回归：推送 `/project/foo` 后，`/project/foobar` 的 ref 的 commit hash 与 tree hash 均未变化
> - ✅ LIKE 通配回归：推送 `/project/my_lib` 后，`/project/myXlib` 的 ref 未被触碰
> - ✅ 后代子树未变时 ref 的 commit hash 不变（不产生空 commit）
> - ✅ 后代子树变更时新 tip 的 parent 精确等于旧 tip
> - ✅ 目录被删 → ref 删除且墓碑写入；同名目录复活后 clone，新 base 的 parent 等于墓碑记录的 `last_commit_hash`
> - ✅ 目录被同名文件取代时走删除分支，不写入 blob hash
> - ✅ `is_cl = true` 的后代 ref 在推送前后完全不变
> - ✅ 后代路径上的 tag ref 行在推送前后完全不变（2.2 的 `ref_name = main` 过滤，登记在案的行为变化）
> - ✅ `/a`、`/a/b`、`/a/b/c` 三条 ref 同时物化时推送 `/a`，三条各自续接到各自旧 tip
> - ✅ merge 触发路径的断裂回归（2.8）：A clone `/project/foo` 并本地提交 → B 向 `/project` 合入 CL → `merge` 分支走 `advance_descendant_refs`，`remove_none_cl_refs` 不再删除 `main@/project/foo` → A `git pull` 成功（review 形态下登记在案的行为变化，本条的失败形态即 2.1）
> - ✅ 互嵌路径 merge 续接回归（自阶段 1 移入）：`/a` 与 `/a/b` 两次 merge 串行执行，后者落地 parent 等于前者落地后的 tip（阶段 1 的预期失败在此转为成功）
> - ✅ 三项通用门禁（fmt / clippy / 指定测试）全绿

---

### 阶段 3 — Roll-up commit 的归属与 provenance

**适用范围（硬约束 8）**：本阶段的归属与 provenance 规则**只作用于 trunk 形态下新引入的合成 commit**——trunk push 的 squash commit、祖先 roll-up、后代续接 commit。review 形态的既有落地形态不动：CL merge 在 `main@P` 落下的合成 commit 沿用现状（`Commit::from_tree_id` 硬编码 `mega <admin@mega.org>`、常量 message，事实校准 9），这正是硬约束 8 要求阶段 1–3 不改变可观察行为的原因。阶段 2 的后代续接 commit 是新引入的 commit 类：阶段 3 落地前暂用现状归属（与既有 `process_ref_updates` 行为一致），阶段 3 落地后即升级为本节规则——两个阶段靠近落地为宜（前置依赖矩阵）。

**3.1 构造规则**

改走 `Commit::new(author, committer, tree, parents, message)`，不使用 `Commit::from_tree_id`（事实校准 9）：

| 字段 | 取值 | 依据 |
| --- | --- | --- |
| author 身份 | 链上 tip commit 的 author | 完成该批工作的主体 |
| 其余作者 | `Co-authored-by:` 逐一列出 | 一次推送可含多人的 commit |
| author date | tip 的 author date，原值搬运 | 不影响 trunk 排序 |
| **committer date** | **max(落地时刻, 前一 trunk commit 的 committer date)** | trunk 时间线的顺序必须与 `push_queue.id` 保序（I6）；仅取执行时刻在墙钟回拨下仍会倒流，max 规则保证无条件单调不减 |
| committer 身份 | tip 的 committer | |
| signature | 服务端签名（沿用 MC-09 的 `ServerSigningContext`） | 原始逐 commit 签名无法穿过合并 |

**3.2 message 与 trailer**

链长为 1 时退化为 tip 的 message **正文**原样（`gpgsig` 等额外头剥离，见下文），零损失。链长 N > 1 时（`ordered_commits` 是 tip 在前，枚举时反转为拓扑升序）：

```
Squash 3 commits at /project/foo

This commit was created by mega2. The push carried 3 commits, which
were squashed into this single commit. The original commits are listed
below in topological order; their objects remain retrievable from the
object store via Mono-Squash-Range (omitted for creation pushes without
a baseline — traversal starts from the persisted tip recorded in the
push queue row).

  a1b2c3d4e5f6  2026-09-04 10:12:03 +0800  Alice <alice@example.com>
      feat: add parser
  e4f5g6h7i8j9  2026-09-04 10:31:44 +0800  Alice <alice@example.com>
      fix: handle empty input
  i7j8k9l0m1n2  2026-09-04 11:02:17 +0800  Bob <bob@example.com>
      test: parser edge cases

Mono-Path: /project/foo
Mono-Ref-Path: /project/foo
Mono-Squash-Range: 9f8e7d6..i7j8k9l0m1n2
Mono-Squash-Count: 3
Co-authored-by: Bob <bob@example.com>
```

**完整枚举只出现在被推路径的 squash commit 上。** 祖先与后代的合成 commit 带紧凑形式，用 `Mono-Squash-Commit` 指向那一个：

```
Land 3 commits at /project/foo

Squashed on the pushed path; see Mono-Squash-Commit for the full listing.

Mono-Path: /project/foo
Mono-Ref-Path: /
Mono-Squash-Commit: <被推路径 squash commit 的 id>
Mono-Squash-Range: 9f8e7d6..i7j8k9l0m1n2
Mono-Squash-Count: 3
```

N = 1 时不存在 squash，各层 roll-up 的 message 一律取客户端 commit 的 message **正文**原样，无 `Mono-Squash-*` trailer。「正文」指 commit 对象中头/体空行之后的字节：git-internal 把 `gpgsig` / `gpgsig-sha256` 等额外头放在 `Commit.message` 里，合成层 commit 必须先按 Git 头文法剥离这些头（`src/common/utils.rs` 的 `split_commit_message`），否则客户端签名块会落进合成 commit 的正文、`git log --oneline` 显示为 `gpgsig -----BEGIN PGP SIGNATURE-----`（issue #28；[`plan-20260923.md`](../plan/plan-20260923.md) FU-01）。合成层 commit 只带服务端签名；客户端签名只保留在被推路径逐字落地的 commit 上。squash 清单中的逐条主题同样取正文首个非空行。

这条规则同时限定了体积：**一次推送只产生一份完整枚举**，与已物化祖先/后代的层数无关。若各层都带完整枚举，体积会是「枚举 × 层数」，而层数随 `ls-remote` 这类只读操作单调增长（见写入模型一节对物化的说明），代价将不可控。`Mono-Squash-Commit` 是**不可变的 commit id**（被推路径 squash commit 的对象 id）——不是「当前 tip」这类活引用表述：后续推送推进 `main@P` 后，该 id 仍精确指向那份完整清单，`git show <id>` 一跳可达。

三个要点：

1. **说明段落是必需的，不是装饰**。合并是服务端单方面做出的行为，客户端在推送成功回执之外看不到它。message 的第一段必须用自然语言说清「这是 mega2 合并的，原始有几个 commit，去哪里找」，使任何一个 `git show` 的读者不必查文档就明白发生了什么。
2. **逐条枚举完整，不截断**（ADR-TP-15），且**每次推送只有一份**——落在被推路径的 squash commit 上。每条给出 commit id、author 署名与 author date、subject，足以让读者判断这次推送包含了什么、由谁在什么时候写的。各 commit 的完整 message 正文不复制进来，经 `Mono-Squash-Range` 到对象库取。
3. **`Mono-Commits` trailer 移除**。正文的完整枚举已经承载了同样的信息，再列一份全 id 只是把 message 体积翻倍。机器可读的锚点由 `Mono-Squash-Range` 与 `Mono-Squash-Count` 承担，权威记录仍是 `push_queue` 行。

若 N 个 commit 的 author date 跨度较大，补 `Mono-Author-Date-Range: <最早>..<最晚>`。

**3.3 按 N 分流的分层结果**

| 层 | N = 1（Agent 常态） | N > 1（攒批推送） |
| --- | --- | --- |
| `main@P` | 客户端 commit 原样落地，hash / 作者 / message / **签名**逐字不变（+1） | 一个合成的 squash commit，`tree` = `chain.tip.tree_id`（+1） |
| `main@A`（祖先，含 `/`） | 一个 roll-up；树未变（净零推送）则不动（+1 或 0） | 同左 |
| `main@D`（后代） | 一个续接 commit；子树未变则不动（+1 或 0） | 同左 |

**每次推送在每个受影响的层上恰好前进一个 commit**，与 N 无关——这是 ADR-TP-12 分流之后的核心性质。N 只决定被推路径那一个 commit 是客户端对象本身，还是服务端合成的 squash commit。

「受影响」的判定见写入模型一节：祖先与后代以树是否变化为条件，被推路径以 tip 是否变化为条件。**净零变更的推送（ADR-TP-16）只有被推路径受影响**，祖先与根前进 0 个——这不是对上述性质的例外，而是「受影响的层集合」在该情形下退化为单元素集。

中间 commit 的树从不被嫁接进任何一层——各层都只从 `chain.tip.tree_id` 采样一次。阶段 2 的后代推进同样只读该值，因此推送 1 个与推送 N 个的后代处理成本完全相同。

由此得到三条性质，需在本文档明确并逐条配套测试：

- **各层历史粒度一致**。任一层的最小步长都是一次推送，bisect 不存在两个粒度。
- **只出现推送原子态**。N 个 commit 的中间状态从不在任何层的树上出现；若中途 commit 引入的问题已由 tip 修复，`main` 与挂载消费方从未见过该问题状态。
- **签名一律由服务端承担（限 trunk 形态与新增合成 commit）**。trunk 推送产生的全部合成 commit（squash、祖先 roll-up、后代续接）都以服务端 GPG 密钥签名；逐 commit 的原始签名验证**不作为受支持的能力**（ADR-TP-15a）。review 形态既有落地形态不动——CL merge 落地的合成 commit 保持现状签名形态（本阶段适用范围已限定，见阶段 3 开头）。N = 1 时客户端签名原样保留是零改写的自然结果，不构成保证。取而代之的知情机制是 squash commit 的完整 message：说明段落加全部 N 条 commit 的逐条枚举，不截断（ADR-TP-15）。

**3.4 链长上限**

review 形态保留 `MAX_CL_CHAIN_COMMITS = 250` 常量不动——其唯一含义由 ADR-MC-07 定义（CL 的 `(from_hash → to_hash)` 累积范围上界），GPG checker 只是纵深防御（`src/ceres/merge_checker/mod.rs:20-27`），该语义在 review 形态下继续成立。trunk 形态引入独立配置 `[monorepo].max_push_commits`（默认 250，trunk-only，见 ADR-TP-17）。

由于 3.2 的 message 必须完整枚举、不得截断，**该配置同时是 message 体积的上界**：按每条约 100 字节估算，250 对应约 25 KB。该体积是**每次推送一份**，落在被推路径的 squash commit 上；祖先与后代只带指针，不随已物化层数放大。

**message 放大是已接受的代价，不作为调低上限的理由。** mega2 面向的就是超大仓库，25 KB 量级的 commit message 相对于其承载的对象规模可以忽略，而完整 provenance 是 ADR-TP-15 与 ADR-TP-15a 共同的落点——签名保真既然已明确不支持，message 就是使用方仅有的知情渠道，不能为体积让步。Git 对 message 长度无实际限制，`git log --oneline` 只取 subject 行，日常浏览不受影响；受影响的只是 `git show` 的输出长度。因此 `max_push_commits` 的取值应当按 B 段时长与推送批量的实际需要来定，message 体积不参与该决策。

> **验收标准**：
> - ✅ 单 commit 推送：`main@P` 精确等于客户端 commit id，且对象**逐字节**与客户端所推一致（签名字节随之保留，但断言的是字节相等，不是签名可验证性——ADR-TP-15a）；根 roll-up 的 message 与该 commit 逐字相同
> - ✅ 三 commit 推送：`main@P` 是合成的 squash commit，其 `tree` 精确等于客户端 tip 的 `tree`（不变式 I2），`parent` 等于推送前的 `main@P`
> - ✅ 三 commit 推送：squash commit 与根 roll-up 的 author 等于客户端 author；正文三条枚举的 id 均可解析回原 commit 对象，author 与 author date 与原对象逐字相同
> - ✅ 多作者链：其余作者出现在 `Co-authored-by:`
> - ✅ 根上连续两次推送的 committer date 单调不减，且与 `push_queue.id` 保序（I6）
> - ✅ 枚举完整性：N 取 3、50、`max_push_commits` 上限三档，squash commit 的 message 逐条列出**全部** N 条（id / author / author date / subject），条数等于 `Mono-Squash-Count`，无截断标记
> - ✅ 枚举顺序：正文清单严格按拓扑升序（父先于子）排列，与 `ordered_commits` 的 tip 在前顺序相反——以一条多 commit 链的 message 断言，不依赖人工目检
> - ✅ message 首段含说明合并行为的自然语言，`git show` 可直接读懂发生了什么
> - ✅ `Mono-Commits` trailer 不再出现；有基线的推送行 `Mono-Squash-Range` 精确，据 `push_queue` 行的 `(old_id, new_id)` 可还原完整清单（创建情形按 I4 省略 Range，由 message 枚举与 tip 锚定）
> - ✅ 全部**新增**合成 commit（squash / roll-up / 后代续接）均带服务端 GPG 签名且可验证；review 形态 CL merge 的落地 commit 签名形态与落地前一致（适用范围限定，见 3.3）
> - ✅ review 形态回归：trunk 规则落地后，CL merge 落地的合成 commit 的 author/committer/message 与落地前逐字节一致（硬约束 8 的直接断言）
> - ✅ 净零变更推送：`main@P` 前进，根与祖先 ref 的 commit hash 均不变
> - ✅ 受影响层的步长一致：无论 N 为何，一次**有树变更**的推送后 `main@P`、`main@A`、`/` 各自恰好前进一个 commit（与上一条净零用例互为补集，二者共同覆盖 I2a）
> - ✅ 三项通用门禁全绿

---

### 阶段 4 — `push_policy = "trunk"` 形态接线

**4.1 配置面**

```toml
[monorepo]
push_policy = "review"          # "review"（默认，CL 管线）| "trunk"（直推）
```

登记为 `restart_required_fields`（与 `import_dir` 同，`config/reload.rs:697`）。启动期 fail-closed 校验六条：

1. `push_policy = "trunk"` 且 `cedar.enforcement != "off"` → 拒绝启动（硬约束 6）。
2. `push_policy = "trunk"` 且库中存在 open CL → 拒绝启动，要求先收口。
3. `push_policy` 与上次运行不同且 `push_queue` 存在非终态行（`Queued/Running`）→ 拒绝启动——切换前排空或人工取消。`kind` 已随行持久化、不因形态切换重解释；真正的风险是**新形态执行旧形态的非终态操作**（如 review 下遗留的 `kind=merge` 行在 trunk 下被收养执行），故非终态行必须清零后切换；两个切换方向都校验，配双向重启测试。
4. `push_auth ∈ {"token","none"}` 且 `push_policy != "trunk"` → 拒绝启动（静态 token 是 storage-only 形态的认证模型，review 形态的身份模型未定义，见阶段 5）。
5. 反向同样强制：`push_policy = "trunk"` 且 `push_auth` 缺省 → 拒绝启动——trunk 的无用户系统前提要求显式选择 token/none，缺省的 OAuth/UserStorage 链在 trunk 下语义未定义（阶段 5）。
6. **形态切换的索引水位重置（必须置 NULL，不得置 0）**：两种形态的索引规则不同（trunk 写/比对 `indexed_push_id` 水位；review 只写 `indexed_push_id IS NULL` 的行）——trunk → review 的切换会使既有水位行永久不符合 review 的更新条件。`indexed_push_id` 为可空列；切换运维步骤执行 `UPDATE ... SET indexed_push_id = NULL`（随迁移/维护执行），使新形态规则从干净状态生效；两个切换方向各配方向测试（review → trunk：NULL 行可被队列索引覆盖；trunk → review：重置后全部行可被 review 更新）。校验随第 3 条的重启流程执行。

**4.2 推送路径改造**

| 位置 | review 形态 | trunk 形态 |
| --- | --- | --- |
| `monorepo.rs:205` `finalize_receive_pack` | `persist_mono_branch_cl_mega_refs_transaction`（写 `refs/cl/*`，`:863-877`，经 `apply_cl_mega_ref_for_push_command`）+ `run_mono_post_push_pipeline` | **按 `push_policy` 条件化**：跳过 CL ref 持久化与 CL post-push 管线——`finalize_receive_pack` 是分支 ref 的唯一变更点（`smart.rs:450` 只对 tag 调 `update_refs`，`:676` 的分支分支不可达），trunk 形态下它的唯一动作是以 `kind='push'` 入队并执行 B3 |
| `monorepo.rs:975` post-push | `update_or_create_cl` | 随 `finalize_receive_pack` 一并跳过 |
| non-fast-forward 闸门 | 不适用（push 不入队） | B3 锁内权威重查 + 树哈希断言（阶段 1 的 B3） |

**验收增加**：trunk 形态下连续推送后**新增** `refs/cl/*` 行数为零（CL 持久化确实被跳过）；存量已合并/关闭的 `refs/cl/*` 行保留为档案但 **advertise 过滤**——trunk 形态的 `refs_with_head_hash` 不返回 CL refs（否则历史 CL ref 会继续被广告，与「唯一公开分支 main」相悖）。

**4.2a 认证前置（自阶段 5 前移）**：trunk 形态必须显式选择静态 token 认证（阶段 5 的配置与实现随本阶段交付——`push_auth` 配置、静态 token 认证器、`check_push_permission` 改造、双端点旁路、无 OAuth 启动；`push_policy=trunk` 且 `push_auth` 缺省 → 拒绝启动，见 4.1 第 5 条）。阶段 5 保留多 token 管理与运维强化。

**4.3 关闭与保留清单**

| 关闭 | 保留 |
| --- | --- |
| CL 创建（`update_or_create_cl`）、**code_edit 的 CL 变更写路由**（`preview_router.rs` 的 `/create-entry` 与 `/edit/save`——其 handler 经 `find_or_create_cl_for_edit` 创建/变更 CL，trunk 形态一并关闭；只读 preview 路由保留） | 对象存储、pack 收发、**LFS HTTP**（批/锁鉴权对齐 `push_auth`；已由 [`plan-20260909.md`](../plan/plan-20260909.md) ADR-LF-01 开启，**supersede** 本阶段曾关闭 LFS 的决策。历史评审记录见修订史 Codex R58 #2） |
| `CheckerRegistry` 全部 checker | `traverses_tree_and_update_filepath`（C 段） |
| merge queue、conversation | UN-16 的写入侧钩子（见下） |
| code review 线程重锚、CLA | MC-03 链校验与链长上限 |
| `commit_auths` 绑定（无用户系统） | ADR-MC-04 单分支准入 |
| CL / issue / reviewer router | Git 客户端禁 tag（见[使用指南](../user-guide.zh.md)） |

router 按 `push_policy` **条件挂载**：trunk 形态下不注册 CL / issue / reviewer router，使 OpenAPI 表面如实反映部署形态，而不是保留一组恒返回空的端点。

**关于保留清单里的 UN-16**：UN-16 是一张卡，同时覆盖授权快照的重建触发（`src/contract/policy/notify.rs`）与「拒绝删除主干 ref」（`monorepo.rs:920-931`）——后者存在的理由正是前者以主干的 `/.mega_cedar.json` 为唯一真相源。硬约束 6 强制 trunk 形态下 `cedar.enforcement = off`，此时快照不构建也不被消费，**因此 trunk 形态下保留 UN-16 的实际作用只有两条**：一是继续拒绝删除主干 ref（与授权无关，本身就是 monorepo 的结构性保护），二是保持写入路径上的 notify 钩子完整，使日后开启 cedar 不必回头补写入侧。**trunk 形态不因保留 UN-16 而具备任何授权保护**，这一点必须在部署文档中明说，避免读者从「保留」二字推断出不存在的安全性。

> **验收标准**：
> - ✅ Agent 常态路径：clone `/project/foo`、单个 commit、推送 → `ls-remote` 的 `main@/project/foo` 精确等于客户端 commit hash；作者/时间/message/签名逐字不变；随后 `git fetch && git reset --hard origin/main` 为 no-op，本地 tip 不变
> - ✅ 连续三轮「单 commit → 推送 → 对齐」全部为 fast-forward，无一次被拒
> - ✅ 攒批路径：三个 commit 一次推送 → `main@/project/foo` 是合成的 squash commit，其 `tree` 等于客户端 tip 的 `tree`，`parent` 等于推送前的 tip；`git fetch && git reset --hard origin/main` 之后本地工作树与推送前逐字节一致
> - ✅ 攒批推送后不做对齐即再次推送 → 被 non-fast-forward 拒绝，且拒绝信息中直接给出对齐命令（ADR-TP-18）
> - ✅ 两条路径（有树变更）推送后 `/` 均恰好前进一个 commit；净零推送时 `/` 不前进（ADR-TP-16）
> - ✅ 陈旧基线推送被拒，远端未变，错误信息可直接照做
> - ✅ **断言先于 NFF 回归**：陈旧物化行 + 客户端持非陈旧 tip → 树哈希断言先拒绝并修复（而非 NFF 拒绝跳过修复）；下一轮推送正常（ADR-TP-20/1.5）
> - ✅ 20 个**互不嵌套**不同路径的并发推送全部成功；根 roll-up 的 parent 链顺序逐一等于 `push_queue.id` 顺序（阶段 1 验收的 push 版，本阶段在 trunk 形态下补齐）
> - ✅ 互嵌路径回归：`/a` 物化后向 `/a` 推送的同时 `/a/b` 的陈旧物化行存在 → B3 树哈希断言拒绝陈旧基线（ADR-TP-20）
> - ✅ 向未物化路径的首次推送：`old_id = ZERO_ID` 时按创建语义落地 `main@P`，客户端随后的 fetch/对齐为 no-op；`old_id != ZERO_ID` 时被拒且报错可诊断（写入模型）
> - ✅ `push_policy=trunk` + `cedar.enforcement=enforce` 启动失败且信息可诊断
- ✅ `push_policy=trunk` + `push_auth` 缺省 → 启动失败（4.1 第 5 条）；`push_auth=token` 下静态 token 推送可用（4.2a 前移的认证交付）
> - ✅ `push_policy=trunk` + 存在 open CL 启动失败
> - ✅ trunk 形态下 CL / issue / reviewer 端点不在 OpenAPI 中登记
> - ✅ `push_policy=review` 下所有既有 CL 测试用例行为不变（硬约束 8）
> - ✅ trunk 形态下连续推送后 `refs/cl/*` 行数为零（CL 持久化确实被跳过，而非仅不再创建 CL）；`finalize_receive_pack` 的 policy 条件化生效（4.2）
> - ✅ merge 的 TreeUpdateResult 锁内重算回归：merge 排队期间注入 intervening attach/merge 推进根 → 锁内重算后落地， intervening 写入完好保留（不被旧预计算结果覆盖）
> - ✅ **B0 活跃重试回归**：原轮次 `Queued/Running` 期间的同指纹重试 → **收养**（B1 统一规则），不因 N=0 早退回 ok 而掩盖原轮次可能的失败；收养后原轮次失败则重试者得到真实失败结果
> - ✅ 三项通用门禁全绿

---

### 阶段 5 — 多 token 运维与认证强化（基础认证已于 4.2a 随阶段 4 交付）

```toml
[git]
anonymous_access = true        # 读，沿用现有语义
# push_auth 缺省 = 现有认证链（OAuth/UserStorage），仅与 review 形态组合；
# "token" | "none" 是显式选择的 storage-only 模式（none 仅限受控内网），
# 且启动校验强制 push_auth ∈ {token, none} ⇒ push_policy = "trunk"——
# review 形态 + 静态 token 是不受支持的组合（评审管线的身份模型未定义）
push_auth = "token"

[[git.push_tokens]]
name  = "team-foo"
token = "${file:/run/secrets/team_foo_token}"
paths = ["/project/foo"]       # 前缀授权；省略表示全库
```

**范围**：基础静态 token 认证（配置、认证器、权限判定改造、双端点旁路、无 OAuth 启动）已于 **4.2a 随阶段 4 交付**，本阶段不重复列举；阶段 5 交付多 token 运维与强化。**认证身份与 provenance 分离**：`requester` 等一切认证判定只使用 **token 名**——commit 的 author/committer 字段是客户端可任意伪造的自声明元数据，不构成认证身份，只作为 provenance 记录保留（1.9 表的 push 行据此取值）。token 只回答「该客户端能否写这棵子树」；「这批工作是谁写的」由 commit 署名回答，二者不可混用。凭据经 SecretRef / 文件挂载注入，遵循 `config.md` 的既有机制，不在配置文件中内联明文。

**现状认证链的改造点（事实校准 17；改造已随 4.2a 于阶段 4 交付，本节保留为规格与验收依据）**——静态 token 模型不是加一段配置就能生效，以下挂点必须逐一改造：

1. **HTTP 启动（两道门槛）与运行时形态**：`start_http()` 的 `require_oauth_for_http_service`（`http_server.rs:424-425`，实现在 `config/validate.rs:162-169`）与 `app()` 内的 OAuth 取用（`http_server.rs:627-634`，含无条件构造的 `WebsiteSessionStore`，`:627-650`）都无条件要求 OAuth 配置。`push_auth` 配置存在时两条路径都改为条件化——storage-only 部署采用**协议专用 router 集**（git 协议路由 + 只读 API 子集），不注册 OAuth 依赖的 Web API 路由（`/auth/*`、website 会话路由等）；会话存储以**匿名/no-op 实现**替代 `WebsiteSessionStore`（`BrowserSessionStore` 现仅有 Website 变体，`api/oauth/api_store.rs:4-14`，需补 no-op 变体）。配启动测试（无 OAuth + 有 push_auth 配置 → 启动成功）。
2. **git HTTP 认证**：`git_http_auth`（`git_protocol/http.rs:105-149`）经 `login_user_from_mono_access_token` 走 `UserStorage`。`push_auth = "token"` 时替换为静态 token 查找：对 `[[git.push_tokens]]` 表做常量时间比对，命中后 `set_authenticated_user(token.name)`。
3. **推送权限判定与端点旁路**：token 的路径前缀授权在 `check_push_permission` 内**先于且独立于** Cedar enforcement 执行——现状该函数在 `cedar.enforcement = off` 时早退（`git_protocol/mod.rs:43-62`），而 trunk 形态强制 off，只提供 username 的实现会意外绕过路径限制；token 模式的路径匹配必须发生在该早退之前。此外它还要求非空 username，且这只是**第二道**门——`git_info_refs` 与 `git_receive_pack` 在其之前就要求 `git_http_auth() == true`（`git_protocol/http.rs:60-65` 与 `:354-365`），`push_auth = "none"` 必须在这**两个端点**都加模式感知旁路（跳过认证要求，读端点沿用匿名语义），只改 `check_push_permission` 不生效；`push_auth = "token"` 时两处保留 token 校验。`push_auth = "token"` 时 username 由 token 名满足，随后执行**前缀匹配授权**（token 的 `paths` 覆盖被推路径）。匹配必须按**路径组件边界**判定——规范化两侧路径后要求 `被推路径 == 授权路径` 或 `被推路径` 以 `授权路径 + '/'` 开头，纯字符串前缀匹配会让 `/foo` 误授权 `/foobar`；配负向测试。`push_auth = "none"` 时显式跳过 username 要求——该分支必须以显式配置为前提，缺省不生效。
4. **SSH 边界**：`ssh.rs:135-157` 要求已认证用户，静态 token 模型不提供 SSH 凭据体系。**阶段 5 的推送认证只交付 git-over-HTTP**；storage-only 形态下 SSH 端口不监听 receive-pack（配置上显式禁用并在部署文档声明），避免留下一条「无凭据要求」的隐性推送通道。
5. **`push_auth = "none"` 的运行前提**：仅当部署方能保证网络边界（受控内网/回环/Unix socket 前置）时允许；启动日志与部署文档必须显式警告该形态不提供任何推送者身份。

> **验收标准**：
> - ✅ 无 token 的推送被拒，返回带 `WWW-Authenticate` 的 401（限 `push_auth = "token"` 模式；`none` 模式按旁路语义放行）
> - ✅ token 的 `paths` 之外的路径推送被拒
> - ✅ 组件边界负向测试：token 授权 `/project/foo` 时，对 `/project/foobar` 的推送被拒（纯字符串前缀不构成授权）
> - ✅ `push_auth = "none"` 必须显式配置，缺省不生效
> - ✅ `push_queue.requester` 记录 **token 名**（认证身份）；commit 的 author/committer 字段可被伪造的元数据不参与任何认证判定（构造 author 与 token 不一致的推送，行为不受影响）
> - ✅ `push_auth = "token"` 且未配置 OAuth 时 HTTP 服务可启动；`queue_control` 单行播种存在（B1 准入锁可用）；`git_object_cache` 与 git 协议路由正常
> - ✅ storage-only 形态下 SSH 不暴露 receive-pack；显式配置缺失时启动失败
> - ✅ 三项通用门禁全绿

---

### 阶段 6 — 可选优化（需实测压力证据）

1. **Group commit**：一次轮次从队列取出连续 K 项，对根树做一次树脊重算，发出 K 个 roll-up（parent 依次串接，顺序即 `push_queue.id` 顺序）。摊薄树遍历与事务开销，全序、原子性与「每次推送一个 roll-up」的语义完全不变。阶段 1 的轮次函数应以 `Vec<QueueItem>` 为入参预留该扩展。
2. **`LISTEN/NOTIFY` 唤醒**：取代阶段 1 的短轮询。
3. **实时排队进度**：`receive_pack_notice`（`monorepo.rs:106`）当前是一次性的，实时进度需要在等待期间持续写 sideband channel 2，属协议层改造。
4. **物化 TTL + 墓碑回收**：给已物化的路径 ref 记最后访问时间，超过 TTL 未被读取则删除并落墓碑；下次有人访问时从墓碑续接重新物化。

   **要解决的成本**：B3 中后代处理与祖先 roll-up 的成本都以「已物化路径数」为界，而**物化只增不减**——一次 `git ls-remote` 就能永久物化一条路径（见写入模型一节），此后该路径下的每一次推送都要为它多做一份工作，即便再也没人读它。几十个 Agent 各自一次性 clone 过不同子路径、之后只有少数活跃的仓库，会为一堆死路径反复付钱。

   **为什么用回收而不是推迟**：阶段 2 的墓碑表已经提供了安全删除一条路径 ref 所需的全部零件——再次物化时从墓碑续接，不变式 I1（历史只增不改）不破。因此这条路只需一个时间戳列加一个清理任务，**不触碰任何不变式**，也不需要幂等合成、陈旧标记传播或对全部读点的审计。它把成本从「推迟」变成「消除」：死路径根本不存在，不必为它付钱。原先列在此处的「后代 ref 惰性推进」已被 ADR-TP-19 否决——它推迟工作却让 non-fast-forward 闸门失去正确性依据。

   **待定项**：TTL 取值、最后访问时间的记录点（至少覆盖 git 广告路径与 API 读点）、墓碑表的容量上界与二次回收策略。

> **验收标准**：各项独立评估；无实测压力证据时不启动。

### 阶段 7 — 出站提交事件（plan-20260912 / WH-03，已交付）

storage-only 部署可在 B3 提交后发出一份有界、仅元数据、带 HMAC 的 `repo.push` 事件（契约与传输见 [`storage-events.md`](storage-events.md)）：

- **唯一插入点**：`b3_execute_push_inner` 的真实 `n > 0` 落地路径，在 `txn.commit()` 成功后、C-segment 索引前。协议 receive-pack 与产品 API 写（`land_api_tip_push`）共用同一提交点，API 侧不再另挂钩子。
- **明确排除**：`n = 0` 净零轮次（`old_id == new_id`）、Done 行 replay、attach/merge 轮次、CAS/fencing 失败（`QueueBypassDetected`/`ClaimLost`）与一切回滚路径都不发事件；后代 ref 续接不逐个发事件。
- **快照数据**：`push_id`（队列行 id）、`operation_id`、`ref_name`、`old_oid`、`requested_oid`、`landed_oid`，全部取自本轮行与落地结果，不在发送时回查最新状态。
- **事件身份**：`sha256("repo.push\0" + installation_id + "\0" + canonical_repo_path + "\0" + operation_id + "\0" + landed_commit_id)` 前 16 字节按 UUID v5 布局编码；队列 i64 id 不参与身份。同输入同 ID，跨安装/仓库/操作/落地 commit 区分。
- **失败语义**：构造或投递失败只记 drop，不改变已提交推送的结果（best-effort，不承诺 exactly-once/有序/durability）。
- **测试**：`push_queue_service::tests::storage_event_commit_matrix`（真实 PG 的 Done/replay/n=0/ClaimLost/CAS 矩阵）、`api_tip_lander::tests::storage_event_api_commit`（API 写共用提交点）、`tests/integration_storage_events_git.rs`（进程级真实 push + review 形态回归）。

## 前置依赖矩阵

| 本文档的工作 | 依赖 | 类型 | 关键同步点 |
| --- | --- | --- | --- |
| 阶段 1 写入者审计 | `contract.md`（Git 协议边界）、`protocol.md`（receive-pack 分层） | 协同 | 审计结论若改变协议层调用点，需同步更新 `protocol.md` |
| 阶段 1 队列泛化 | 现有 `merge_queue` 四层实现 | 前置 | `merge_queue` 被吸收后，UN-17 / UN-18 / UN-25 的语义须逐条对齐迁移 |
| 阶段 2 | 阶段 1（不变式 I1 的归纳证明依赖全序） | 前置 | 代码可并行开发，正确性论证依赖阶段 1 落地 |
| 阶段 3 | MC-09 `ServerSigningContext` | 前置 | 服务端签名能力已存在，本阶段只新增调用点 |
| 阶段 3 | 阶段 2 | 协同 | 阶段 3 的归属规则只作用于 trunk 形态与新增的续接 commit；阶段 2 先落地时续接 commit 暂用现状归属，阶段 3 落地后升级，二者靠近落地为宜 |
| 阶段 4 | 阶段 1 / 2 / 3 全部完成 | 前置 | trunk 形态的正确性完全建立在三者之上 |
| 阶段 4 | `../user-guide.zh.md` | 协同 | `push_policy` 的形态差异须同步到用户指南 |
| 阶段 4 | Cedar 判定（`contract.md`） | 前置 | 互斥校验依赖 `cedar.enforcement` 的既有三态语义 |
| 阶段 5 | 阶段 4（4.2a 已交付基础静态 token 认证） | 前置 | 阶段 5 为多 token 运维与强化，不承担基础认证 |
| 阶段 5 | `config.md` 的 SecretRef 与文件挂载机制 | 前置 | token 凭据不得内联明文 |
| 全阶段 | `integration.md` / `test-infra.md` | 协同 | 新增 e2e 用例须登记进测试矩阵 |

## 风险与约束

- **风险 1：写入者审计遗漏**
  - 影响：序列化出现缺口，根树与路径 ref 给出不一致视图，且无显式报错。
  - 缓解措施：CAS 断言把遗漏转化为运行时 fail-closed 事件（阶段 1 B3）；配套一条「故意绕过队列」的负向测试作为该断言的活文档；指标中登记断言失败计数并告警。

- **风险 2：全局队列成为吞吐瓶颈**
  - 影响：推送延迟随并发上升；**读路径同样耦合**——ADR-TP-20 让冷 advertise/clone 的物化插入持 `MONO_WRITE_LOCK`，读延迟上界受 B3 时长影响，读流量与写闸门竞争（reaper 试锁未中因此成为常态而非异常）。
  - 缓解措施：临界区只保留树脊读取、内存拼树与一次写事务，A 段与 C 段均在临界区外；阶段 6 的 group commit 作为已设计好的扩展路径，轮次函数在阶段 1 即以批量入参预留。

- **风险 3：`merge_queue` 吸收过程中语义漂移**
  - 影响：UN-17（legacy 行的执行判定）、UN-18（requester）、UN-25（authz freeze）的既有行为被改变。
  - 缓解措施：逐条列出三者的现行语义与迁移后对应物，作为阶段 1 的评审门；review 形态的既有测试用例全部保留且不得修改期望值。

- **风险 4：墓碑续接串联语义无关的历史**
  - 影响：同名路径删除后重建时，`git log` 中出现与当前内容无关的前史。
  - 缓解措施：在[使用指南](../user-guide.zh.md)中说明该语义；提供墓碑清理的运维入口，允许在明确知情时切断续接。

- **风险 5：C 段异步化导致索引落后**
  - 影响：文件路径索引在推送提交后短暂落后，Web 浏览与挂载消费方的路径查询可能读到旧值。
  - 缓解措施：索引行带 `indexed_push_id` 行级水位，旧序号的更新被 `WHERE indexed_push_id < $id` 的 CAS 挡住（ADR-TP-11——按行而非按任务，嵌套路径的交错任务才不会被旧数据覆盖）；提供索引补偿任务；在本文档中声明该最终一致性。

- **风险 6：惰性物化与 B3 的竞态**
  - 影响：物化写入落后于根树的路径 ref，后续推送以它为基线时静默丢失祖先推送的改动（ADR-TP-19 失效链经物化路径复现）。
  - 缓解措施：物化插入持 `MONO_WRITE_LOCK` 短锁并校验根快照新鲜度（不一致即放弃插入，ADR-TP-20）；B3 的 push/merge 闸门追加 `ref_tree_hash == resolve(root, P)` 树哈希断言，检出即拒绝，B3 内以 SAVEPOINT 同事务写墓碑并删行（自愈，不死局，无崩溃间隙）；阶段 1/4 各配一条回归用例。

- **约束 1：trunk 形态不提供评审与授权门控**
  - 理由：CL 管线是 ACL 自提权检查、GPG 门控、CLA、code review 的唯一挂载点（事实校准 13）。
  - 影响：违反硬约束 6 强行同时启用 Cedar enforce 与 trunk，等于让 `enforce` 声称的保护在推送路径上不存在。

- **约束 2：逐 commit 的 GPG 签名验证不受支持（ADR-TP-15a）**
  - 理由：签名覆盖 commit 对象全文，合并必然产生新对象。提供一个只在 N = 1 时成立的条件性保证，会诱导部署方去约束使用方的推送方式，把服务端的实现约束转嫁成使用方的操作纪律。
  - 影响：所有合成 commit（祖先 roll-up、后代续接、N > 1 的被推路径 squash）一律由服务端 GPG 密钥签名，验证语义是「这次落地由该 mega2 实例执行」，而非「这些内容由某个作者签署」。N = 1 时客户端签名原样保留，但**不作为可依赖的保证**，部署方不得据此设计验签流程。使用方对「这次合并包含了什么」的知情权由 ADR-TP-15 的完整 message 承担。

- **约束 3：形态切换不可逆地依赖库状态**
  - 理由：`review` → `trunk` 要求无 open CL；`trunk` → `review` 后既有的 roll-up 历史不会重建为 CL。
  - 影响：形态应在部署规划阶段确定，不作为可反复切换的运行时开关。

## 改进方案多维评估小结

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | **高（9/10）**。序列化被论证为语义必然而非性能取舍（硬约束 1），「合并成一个」被论证为树形状的推论而非产品选择（硬约束 3）；阶段 1–3 独立于新形态成立，本身即现有缺陷的修复。不足：`merge_queue` 吸收范围需在审计后才能完全确定。 |
| **可行性** | **中高（8/10）**。全部写入者类别均已定位（硬约束 2：根树写入者、路径 ref 物化者、队列外 bootstrap）；队列可由 `merge_queue` 四层泛化而来；`build_result_by_chain` / `apply_update_result` / `ServerSigningContext` 均为现成原语。不足：阶段 1 触及所有写入路径，回归面宽，需要成套负向测试先行；B3 依赖的事务内 ref 更新原语需新增（事实校准 16）。 |
| **完整性** | **中高（8/10）**。覆盖顺序、互斥、正确性三层，覆盖祖先/被推路径/后代三类 ref，覆盖崩溃、卡死、背压与控制面。不足：阶段 6 的四项优化仅给出方向与前提，未展开设计。 |
| **安全性** | **中高（7.5/10）**。以启动期互斥（硬约束 6）替代在推送路径上复制授权检查，边界清晰且 fail-closed；CAS 断言提供覆盖度的运行时自检。不足：trunk 形态本身放弃了评审门控，安全性依赖部署边界（内网、token 前缀授权）而非引擎内判定，这一点必须在部署文档中显式声明。 |
| **功能正确性** | **高（9/10）**。不变式 I1–I6（含 I2a）全部可测；I1 有基于队列全序的归纳证明；I2 拆为内容保真与 N = 1 的对象保真两档，二者都可直接断言；每个已知缺陷都配有对应的回归用例。不足：墓碑续接的语义取舍无法用测试判定优劣，只能靠文档声明。 |
| **可靠性** | **中高（8/10）**。B3 单事务保证崩溃后无部分状态；advisory lock 随连接释放，不留死锁；reaper 清帐并幂等补做持久化意图（墓碑修复/冲突重排/硬停判定），含孤儿行的 I3 顺带校验。不足：C 段最终一致性引入了新的补偿路径，需要独立的巡检。 |
| **兼容性** | **中高（8/10）**。`push_policy` 默认 `review`，既有部署行为不变；阶段 1–3 不改变 CL 的可观察语义；Agent 的常态路径（N = 1）零改写，客户端无需适配。不足：N > 1 的推送会使客户端本地历史与服务端发散，需要一条约定的对齐动作（ADR-TP-18），这是本方案唯一要求使用方配合的地方；`merge_queue` 被吸收后其 HTTP 表面需保留或明确迁移。 |
| **可扩展性** | **中高（8/10）**。group commit 的扩展点在阶段 1 即以批量入参预留；已物化路径数带来的成本增长由阶段 6 的物化 TTL 回收处理，且该方案不触碰任何不变式（ADR-TP-19 否决了推迟工作的惰性方案）。不足：全局单写入者是该数据模型的固有上界，横向扩展只能靠批量摊薄。 |

## 小结

Monorepo 的每一次落地（trunk 推送、CL merge、attach）都必须改写根树，所有落地在 `/` 上完全冲突，因此写入必须序列化——这是数据模型的语义必然。本计划以 `MonoWriteQueue`（顺序）+ 事务级 advisory lock（互斥）+ 根 ref CAS（正确性自检）三层结构收拢全部根树写入者，在其上把后代 ref 从「删除后重新懒生成」改为「续接推进」以保证历史只增不改，把合成 commit 的归属从硬编码改为真实作者加 provenance trailer，最终由 `push_policy = "trunk"` 一个开关开启不接入用户系统、不使用 Change List 的存储形态。该形态面向 Agent 的「每 commit 一推」用法：单 commit 推送时客户端对象原样落地，多 commit 推送时由 mega2 自动合并为一个 commit，两种路径下每个受影响的层都恰好前进一个 commit，分支模型仍是唯一公开分支 `main` 的 trunk-based development。

## 预期收益

- Monorepo 根树写入具备全序与原子性，并发推送不再互相覆盖；根树视图与路径视图恒一致。
- 已物化路径的历史只增不改，`refusing to merge unrelated histories` 这一类故障在结构上不可能出现。
- 根与祖先层的历史带真实作者、真实时间与可回溯到原始 commit 的 provenance，可读且可审计。
- Agent 的常态路径（每 commit 一推）零改写：被推路径保留客户端 commit 的原始对象，`git log` 与原生 Git 无差别，推送后无需重新对齐。（原始签名字节随对象一并保留，但这是零改写的副产物，**不是可依赖的保证**——见 ADR-TP-15a。）
- Agent 攒批推送时中间状态不进入 `main`：自动合并为一个 commit，`main` 的每一步都对应一次完整的推送意图，各层历史粒度一致。
- 队列具备深度、等待时长、失败率与断言失败计数等可观测指标，以及暂停/排空/取消的运维控制面。
- 仅需存储能力的部署可在不引入用户系统、Issue 与 Change List 的前提下使用 monorepo。

## 附录 A：不变式

在用户指南与本文档中分别记录用户可见规则和技术约束，并逐条配套测试：

- **I1 历史只增不改**：对任一已物化路径 `R`，任何写入之后，`R` 的旧 tip 仍是新 tip 的祖先。**成立时点自阶段 2 起**：阶段 1 的删除式后代处理（`remove_none_cl_refs`，无墓碑）违反本条，故阶段 1 产物不部署生产（见迁移步骤开头）；阶段 2 的续接 + 墓碑使其成立。
- **I2 内容保真**：`main@P` 的 tip 的 `tree` 恒等于客户端推送 tip 的 `tree`。其中 N = 1 时更强——tip 本身恒等于客户端推送的 commit id（对象保真）。
- **I2a 步长一致**：一次推送后，每个**受影响的**已物化 ref 恰好前进一个 commit。「受影响」= 该层新子树 hash 与现有 `ref_tree_hash` 不同（被推路径例外，以 tip 是否变化为条件）。未受影响的层前进零个——净零推送时祖先与根、以及子树未变的后代都属此类。任一层的历史粒度都是「一次推送」。
- **I3 视图一致**：对每个已物化路径 `P`，`main@P.ref_tree_hash` 恒等于从当前根树沿 `P` 解析出的子树 hash。根树是唯一真相，路径 ref 是它的物化视图，二者不得分叉。**本条是强一致且不分层**（ADR-TP-19）——它是 B3 的 non-fast-forward 闸门的正确性前提，属写入判定链，不可降级为最终一致；ADR-TP-11 对 file_path 索引接受最终一致不构成先例，那份数据不参与写入判定。强一致由两个保证合成：B3 推进的行与提交后的根树一致（后代同事务续接，ADR-TP-19），队列外物化提交的行与提交时点的根树一致（插入时根快照校验，ADR-TP-20）；任一提交边界上不存在陈旧或中间态的行，陈旧行一旦被检出即经 B3 内 SAVEPOINT 同事务转为墓碑并由续接重建（ADR-TP-20）。**本条的前提是全部根写入者遵守锁纪律（I5）**：绕过队列的写入者造成的陈旧行不在 I3 的正常输入之内，由启用前对账与周期性不变式巡检兜底（ADR-TP-20 第 3 项），检出即转 I5 的 fail-closed 事件。
- **I4 provenance 完整**：每次 N > 1 的推送恰好产生一份完整枚举，落在**被推路径**的 squash commit 上，逐条列出全部被合并 commit，条数等于 `Mono-Squash-Count`，不截断；祖先与后代的合成 commit 以 `Mono-Squash-Commit` 指向它，该指针必须可解析；**区间语义只对有基线的推送行成立**——创建情形（`P` 无已物化行）无基线可作 range 端点，省略 `Mono-Squash-Range`，N 条原始 commit 由 message 枚举与 `new_id`（链 tip）锚定；CL merge 行的落地 commit 是**执行时点** `cl.to_hash` 的子代（rebase 会改变 `to_hash`，可能不同于入队时点的 `new_id` 快照，见 1.9）、不在 `(old_id, new_id]` 内（见 1.9），对它须按 `landed_commit_id` 反查；merge 行**保留双锚点**——`landed_commit_id` 锚定落地对象，执行时点的 `to_hash`（= 落地 commit 的 parent）锚定 CL 链及其祖先，两个锚点在 GC 中同等保留（仅用其一都会丢另一半：只留 `landed_commit_id` 丢 CL 链、只留 `to_hash` 丢落地 commit 本身）；`push_queue` 每一行锚定的 commit 链全部可解析（推送行按区间或 tip 锚定，merge/attach 行按 `landed_commit_id`）；**墓碑行同样锚定历史**——`last_commit_hash`/`last_tree_hash` 是续接的基点，未来 GC 中必须与队列锚点同等保留。若后续引入 commit GC，必须将上述锚点全部视为 GC root（`operation_id` 是数据库操作指纹，不承载对象身份，不在其列）。
- **I5 队列完备性**：全部根树与路径 ref 写入者（硬约束 2 的清单）要么经 `MonoWriteQueue` 串行，要么被 ADR-TP-20 的机制显式覆盖，**要么在服务接流前完成并持专属锁**（bootstrap，硬约束 2 的第三类）——三类与硬约束 2 的清单一一对应。**I3 与本条共享同一前提：全部写入者遵守锁纪律**。在此前提下根 ref CAS 断言恒不失败；一旦失败即存在清单之外的写入者，属 fail-closed 事件。CAS 是兜底 tripwire 而非完备性证明（一个先于队列事务快照完成的未登记写入者可不被它发现，陈旧的物化行亦然），完备性由三层共同承担：写入者审计（第一依据）、CAS 断言（轮次级 tripwire）、不变式巡检（全量兜底，ADR-TP-20 第 3 项）。
- **I6 时间线全序**：任意时刻至多一个操作处于 B 段；`/` 上 roll-up commit 的先后顺序与 `push_queue.id` 保序。保序**允许空洞，不要求一一对应**——净零推送占用一个 id 却不产生 trunk commit，因此 id 序列存在空洞；空洞不破坏保序。ADR-TP-14 的「committer date 取落地时刻」正是为满足本条而设计。

**I1 的归纳证明**：路径首次物化时建立 tip₀（懒生成的无父 commit，或墓碑续接的 commit）。`MonoWriteQueue` 给出所有写入的全序，第 n 次写入对 `R` 只可能是三者之一——跳过（tipₙ = tipₙ₋₁）、续接（`tipₙ.parents = [tipₙ₋₁]`）、或 `R == P` 的 fast-forward（B3 锁内的权威 non-fast-forward 闸门已保证 `cmd.old_id == tipₙ₋₁`，客户端链以此为基）。唯一的移除是路径消失，且墓碑保证下次物化从 tipₙ₋₁ 续接。该归纳依赖全序：没有全序，两次并发写入各自基于不同的旧 tip 续接，归纳即断裂。

## 附录 B：术语

- **roll-up commit**：祖先或后代路径上合成的 commit，其 tree 取自本次推送的 tip 子树，parent 为该路径自身的旧 tip。
- **物化（materialize）**：子路径首次被 clone/advertise 或经 code_edit 读点访问时在 `mega_refs` 中生成 `main` 行的过程（`monorepo.rs:121` 与 `code_edit/utils.rs:362`，见事实校准 15）。物化不入队，由 ADR-TP-20 的短锁插入与 B3 树哈希断言覆盖。
- **被推路径 `P`**：receive-pack 请求所指向的 monorepo 路径。
- **B 段**：推送生命周期中独占执行的临界区（准入校验 + 树嫁接 + ref 更新，单事务）。
- **CAS 断言**：根 ref 更新携带的旧值条件；在本设计中用于检测队列覆盖度，而非并发控制。

## 最后一次更新

- **日期**：2026-09-05
- **内容**：首版发布。定义 trunk 直推形态（storage-only）与 Monorepo 根树写入序列化方案；登记 14 条事实校准、9 条硬约束、20 条决策记录（ADR-TP-01 – ADR-TP-19，另含 ADR-TP-15a；队列约束 11 条、多 commit 推送与签名语义 8 条、一致性模型 1 条）、6 个阶段与 7 条不变式。已登记到 `README.md`（文档概览 5a、执行顺序第 8 阶段、优先级排序）与 `general.md`（分层结构、角色定义、阶段编号约定）。TP-21 于 2026-09-09 完成了用户指南摘要、部署文档和本文反向声明的逐项校对。
  - 首版计数随修订更新（以修订 39 后的正文为准）：事实校准 **17** 条、决策记录 **21** 条（ADR-TP-01 – ADR-TP-20，另含 ADR-TP-15a）、不变式 **7** 条（I1–I6 含 I2a）、阶段 1 交付物 **10** 项。
- **同日修订 1**：需求前提补入 **Agent 使用场景**（每 commit 一推，N = 1 为常态）；推送落地规则由「被推路径全量保留」改为**按 N 分流**——N = 1 原样落地，N > 1 在被推路径自动合并为一个（ADR-TP-12 重写）。连带修订：硬约束 5 与新增 5a、写入模型表、不变式 I2 改为内容保真并新增 I2a 步长一致、新增 ADR-TP-18（N > 1 后的客户端对齐约定）、阶段 1 B3 伪代码与阶段 3/4 验收标准。
- **同日修订 66（对齐修订）**：阶段 5 的「现状认证链改造点」标题更正——改造已随 4.2a 于阶段 4 交付，本节保留为规格与验收依据（消除「本阶段的代码交付物」与 4.2a 的表述矛盾）。：C-R5 评审 5 条 MINOR + 2 条 SUGGESTION 全部落实。
  - **I5 第三类覆盖（Claude C-R5 #1，MINOR）**：bootstrap（服务前 + 专属锁）补入 I5 三分法，与硬约束 2 清单对应。
  - **空 refs 禁令的范围（Claude C-R5 #2，MINOR）**：限定于「放弃插入被伪装成空仓库」；路径不在根树时的 `(ZERO_ID, 空 refs)` 是 2.5 要求的正确答案。
  - **review 索引行为变化登记（Claude C-R5 #3，MINOR）**：隔离规则对 review 形态是登记在案的索引行为变化（不在写入判定链）。
  - **4.1 计数（Claude C-R5 #4，MINOR）**：「三条」改「六条」并修引用块。
  - **修订 3/4 标注存史（Claude C-R5 #5，MINOR）**：被后续修订取代的措辞标注。
  - **读路径耦合定价（Claude C-R5 #6，SUGGESTION 采纳）**：风险 2 补物化持锁对读延迟的影响；物化行合并进单个持锁事务。
  - **回放 tip 超越说明（Claude C-R5 #7，SUGGESTION 采纳）**：1.11 回放注记回放 commit 恒为祖先。：R59 评审 2 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **B2.5 认领谓词内含硬停（Codex R59 #1，MINOR）**：预查与加锁之间的硬停置位窗口由认领谓词内的 `NOT hard_stopped` 封住。
  - **B1 线性化点（Codex R59 #2，MINOR）**：准入锁使并发入队串行，「双双通过 NOT EXISTS」在主路径不发生；无锁退化的冲突恢复路径保留并在伪码/验收中显式。
  - **`/retry` 行为（Codex R59 #3，SUGGESTION 采纳）**：重新入队并同步等待执行（重试者即执行者），不允许「已接受未执行」返回。：R58 评审 1 条 BLOCKING + 1 条 MINOR 全部落实。
  - **水位重置必须置 NULL（Codex R58 #1，BLOCKING）**：置 0 会让 trunk → review 切换后的行永久不符 review 更新条件。`indexed_push_id` 明确为可空列，重置 `SET NULL`，两个切换方向各配方向测试；隔离规则同步。
  - **trunk 形态的 LFS 不可用（Codex R58 #2，MINOR）**：当时关闭 LFS 并列为独立议题。**已由 [`plan-20260909.md`](../plan/plan-20260909.md) ADR-LF-01 supersede**（本条为历史评审落实记录，不代表当前产品状态）。：R57 评审 1 条 BLOCKING + 2 条 MINOR 全部落实。
  - **B3 权威 commit 校验显式化（Codex R57 #1，BLOCKING）**：push 分支逐一显式——行存在须 `ref_commit_hash == old_id`；行缺失须 `old_id = ZERO_ID`（经 upsert 原语）；N=0 须 `new_id == 当前 tip`。
  - **tag 保留为登记在案的行为变化（Codex R57 #2，MINOR）**：2.2 明确新查询按 `ref_name = main` 过滤、tag 行保留（现状删除属缺陷），配不变回归。
  - **验收与修订记录的精确化（Codex R57 #3，MINOR）**：`P=/` attach 收养加「无已物化后代」限定；修订 52 条目补后续扩展注记。：R56 评审 2 条 BLOCKING + 1 条 SUGGESTION（正面确认）全部落实。
  - **B2.5 认领的串行化（Codex R56 #1，BLOCKING）**：READ COMMITTED 下两个并发认领可各自通过 `NOT EXISTS` 谓词（行锁不序列化谓词），产生并发 B3（I6 破损）。B2.5 先取 `queue_control FOR UPDATE` 串行化认领；配双认领者回归。
  - **B0 NFF 预检降级为遥测（Codex R56 #2，BLOCKING）**：无锁的 B0 NFF 拒绝会在 B3 修复陈旧物化行之前放逐客户端，违背「断言先于 NFF」。预检改为不拒绝（权威 NFF 判定唯一归属 B3）；1.11/验收同步。：R55 评审 2 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION（正面确认）全部落实。
  - **墓碑续接物化的前提（Codex R55 #1，BLOCKING）**：目录不在当前根树时 advertise 物化不出任何东西，从墓碑凭空造行违反 I3。B0 提示语改两步（先父路径重建目录，再 advertise 续接物化）；2.5 集成补「路径可解析才续接，否则跳过物化」。
  - **形态切换的索引水位重置（Codex R55 #2，BLOCKING）**：trunk → review 会使既有水位行永久不符 review 更新条件。4.1 补第 6 条：切换运维步骤含 `RESET INDEX WATERMARK`。
  - **断言先于 NFF（Codex R55 #3，MINOR）**：B3 push 分支的树哈希断言移到 non-fast-forward 比对之前——否则持非陈旧 tip 的客户端绕过陈旧行修复。配回归。：R54 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **根 attach 的行为统一（Codex R54 #1，BLOCKING）**：B3 attach 分支删除与 B0 矛盾的「`P=/` 被拒」表述——`P=/` 的 attach 允许（B0 拒绝仅属 push kind）。
  - **attach 的已物化后代检查（Codex R54 #2，BLOCKING）**：目标/祖先之外，存在已物化后代（`path LIKE 'P/%'` 的 main 行）也拒绝——attach 不推进/墓碑化后代，分叉面不止目标与祖先。配回归。
  - **硬停下的 reaper 结果澄清（Codex R54 #3，MINOR）**：重置回 `Queued` 仅适用于硬停置位前已 `Running` 的存量行；本次运行新检出的基线失配行终态 `Failed` + `hard_stopped`（重置会让绕过源继续排队）。：R53 评审 1 条 BLOCKING + 2 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **升级前的历史删除回填（Codex R53 #1，BLOCKING）**：现状删除不留痕，升级后向曾删除路径的创建会断裂旧客户端（I1）。阶段 2 迁移提供尽力回填与 fail-closed（默认）两路径并显式记录；配删除-升级-重建回归。
  - **基线失配的硬停次序（Codex R53 #2，MINOR）**：不先 ROLLBACK（给 B1 留准入窗口）——SAVEPOINT 回退业务写，同事务持锁置硬停与终态。
  - **根路径按 kind 声明（Codex R53 #3，MINOR）**：push 拒绝 `P=/`；merge（根 merge 现状支持）与 attach（合法根行更新）允许。
  - **`last_policy` 列（Codex R53 #4，SUGGESTION 采纳）**：`queue_control` 持久化上次形态，支撑 4.1 第 3 条校验。：R52 评审 1 条 BLOCKING + 1 条 MINOR 全部落实。
  - **attach 祖先检查排除根（Codex R52 #1，BLOCKING）**：bootstrap 恒物化 `main@/` 且 attach 本就合法更新根行——祖先检查排除 `/`，只查目标与严格非根祖先（`P=/` 已被 B0 拒绝）。
  - **refs_with_head_hash 迁移调用点（Codex R52 #2，MINOR）**：补 `import_repo.rs:392` 调用点。：R51 评审 2 条 BLOCKING + 2 条 MINOR 全部落实。
  - **N 与 fork_base 职责分离（Codex R51 #1，BLOCKING）**：`n` 只由客户端基线（`new_id→old_id` 步长）决定，服务端已知性完全不参与；`fork_base` 是独立的对象/校验边界。已知对象的重试不再被错算成 N=0。
  - **索引重插的声明降级（Codex R51 #2，BLOCKING）**：删除无代际水位，「永不复活」不被承诺；收敛为最终一致（补偿重扫），删除水位列为后续议题。验收改为收敛断言。
  - **review 索引的隔离规则（Codex R51 #3，MINOR）**：review 更新仅写 `indexed_push_id IS NULL` 的行，队列更新可覆盖之——无版本更新不再击穿行级 CAS。
  - **CAS 终态写的状态条件（Codex R51 #4，MINOR）**：CAS 失败的终态更新补 `WHERE status='Running'`，0 行命中 fail-closed。：R50 评审 1 条 BLOCKING + 3 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **推送描述符的构造规则（Codex R50 #1，BLOCKING）**：1.3 补精确定义——第一父链回溯、fork_base 选取（首个服务端已知 commit）、创建（ZERO_ID 基线）与空 pack 特例、环/断链/merge 由 MC-03 拒绝、上限处理；N=0 仍仅限 `old_id == new_id`。
  - **attach 的祖先物化检查（Codex R50 #2，MINOR）**：目标路径**或其任何祖先**已物化 → 拒绝（attach 不更新祖先 main 行，I3 破损面不止目标行）。
  - **reaper 判定优先级（Codex R50 #3，MINOR）**：绕过判定优先于重排意图补做（意图被丢弃，重排由重试重建）。
  - **阶段 5 去重（Codex R50 #4，MINOR）**：基础认证归 4.2a，阶段 5 只列多 token 运维。
  - **review 索引的版本来源（Codex R50 #5，SUGGESTION 采纳）**：`indexed_push_id` 水位只由队列路径写入；review 推送沿用现状同步索引，不参与水位。：R49 评审 2 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **B1 并发唯一冲突路径（Codex R49 #1，BLOCKING）**：两个同指纹请求可同时通过 `NOT EXISTS`，败者收到唯一索引冲突而非收养/回放。B1 补冲突处理：ROLLBACK → 重读分类（Done 回放 / 收养 / 胜者回滚则重试）；路径索引冲突 → 拒绝。配并发验收。
  - **attach 对已物化路径破坏 I3（Codex R49 #2，BLOCKING）**：attach 只更新根不更新 `main@P`（`mono_storage.rs:453-483`），对已物化路径 attach 会使行与根树分叉。B3 attach 分支补「目标已物化 → 拒绝」；reaper 的 attach I3 语义补异常残留分支。配回归。
  - **继任行插入的门槛豁免（Codex R49 #3，MINOR）**：B4 明确继任行插入是同事务内部替代操作，豁免外部 hard_stopped/depth 门槛（净深度不变）；创建失败由已持久化重排意图兜底。：R48 评审 1 条 BLOCKING + 2 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **阶段 1 的可部署性（Codex R48 #1，BLOCKING）**：阶段 1 的删除式后代处理违反 I1，却声明「可独立上线」。改为「可独立验收；生产部署以阶段 2 落地为前置」，I1 标注成立时点。
  - **watchdog 匹配条件（Codex R48 #2，MINOR）**：`started_at` 写于认领时刻，不能与后端事务起点精确匹配。自动 watchdog 条件改为「锁持有超时 + 存在非终态 Running 行」。
  - **Mono-Squash-Range 的创建例外（Codex R48 #3，MINOR）**：3.2 示例补创建推送省略 Range、遍历自持久化 tip 起的说明。
  - **意图措辞残留（Codex R48 #4，SUGGESTION 采纳）**：B3 的「清除意图」与 schema 注释更正为「pending_action 仅冲突重排」。：R47 评审 1 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **B0 快路径声明残留（Codex R47 #1，BLOCKING）**：1.11 仍提「`cmd.new_id == tip` 早退」。删除——`Done` 回放（B1）与 B3 的 N=0 分支是仅有的两条权威路径，无任何 B0 快路径。
  - **缺失祖先插入与「禁止静默跳过」的范畴（Codex R47 #2，MINOR）**：交付物 6 明确缺失祖先的插入是树项原语的显式设计行为（区别于 ref 批量更新对更新意图的静默丢弃）。
  - **继任行的自主重试损失（Codex R47 #3，SUGGESTION 采纳）**：1.11 记录 caller-owned 模型下继任行可能因无重试而被心跳清理的固有代价。：R46 评审 2 条 BLOCKING + 2 条 MINOR 全部落实。
  - **需求前提第 1 条与 ADR-TP-13/14/15 矛盾（Codex R46 #1，BLOCKING）**：第 1 条限定为 N=1 逐字段保真；N>1 以内容保真（I2）+ provenance 完整替代。
  - **阶段 1 push 分支的阶段依赖倒挂（Codex R46 #2，BLOCKING）**：B3 push 分支在阶段 1 使用交付物 5 的事务内删除变体（与 merge 分支同原语），`advance_descendant_refs` 的切换（2.8）同时覆盖 merge 与 push 分支。
  - **merge 行 GC 双锚点（Codex R46 #3，MINOR）**：I4 明确 `landed_commit_id`（落地对象）与执行时点 `to_hash`（CL 链）双锚点同等保留。
  - **意图措辞残留（Codex R46 #4，MINOR）**：B4/1.6 统一——重排意图仅服务 `requeue_conflict`；陈旧行修复依赖 `expected_*` 比对 + 通用 I3 修复。：R45 评审 3 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **重试绕过快路径（Codex R45 #1，MINOR）**：物化重试必须绕过 `heads_exist` 快路径（否则原地拿回陈旧行）。ADR-TP-20 第 1 项补入。
  - **ClaimLost 活性取舍（Codex R45 #2，MINOR）**：ADR-TP-03/B2.5 记录 reaper 竞争活轮次的可重试失败代价与指标。
  - **删除水位的已知限制（Codex R45 #3，MINOR）**：出现对表的删除重插竞态登记为后续议题，本计划以补偿任务收敛。
  - **阶段 5 重定位（Codex R45 #4，MINOR）**：标题与依赖矩阵改为「多 token 运维与强化」，基础认证归 4.2a。：R44 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **阶段 1 互嵌 merge 用例不可行（Codex R44 #1，BLOCKING）**：阶段 1 仍用 `remove_none_cl_refs`，`/a` merge 会删 `main@/a/b`，`/a/b` merge 只能被拒。阶段 1 改为预期失败验收，续接成功形态移至阶段 2.8。
  - **CAS intent 验收残留（Codex R44 #2，BLOCKING）**：两条验收改写为 `expected_*` 基线模型——「CAS 执行前崩溃」与「CAS 失败后终态提交前崩溃」都由 reaper 的基线比对兜住，删除意图叙述。
  - **reaper 的 push 缺行分支（Codex R44 #3，BLOCKING）**：补 push 行 `path` 无 `main@P` 的语义（`ZERO_ID` 且无墓碑 → 创建中间态，跳过 I3 修复、普通失败终态化；有墓碑 → 异常残留告警）；merge 缺行登记。
  - **4.1 措辞（Codex R44 #4，MINOR）**：「错误 kind 执行」更正为「新形态执行旧形态的非终态操作」（kind 已持久化）。：R43 评审 2 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION（正面确认）全部落实。
  - **verify_root 状态机残留（Codex R43 #1，BLOCKING）**：枚举/1.4/B3/CAS 段/阶段 1 验收的 verify_root 残留全部清除——根绕过判定统一为「B2.5 落库的 `expected_*` 基线 + reaper 对每行 Running 的比对 + 通用 I3 修复」；`pending_action` 仅 `requeue_conflict`。
  - **push_auth 前移阶段 4（Codex R43 #2，BLOCKING）**：trunk 的端到端验收依赖静态 token 认证，而其在阶段 5。新增 **4.2a 认证前置**（阶段 5 的配置与实现随阶段 4 交付，阶段 5 保留多 token 管理与运维强化）；配 trunk+缺省 push_auth 启动失败与 token 推送可用的验收。
  - **`/merge-queue/add` 行为选定（Codex R43 #3，MINOR）**：选定**同步化**（删除 410 备选表述）。：R42 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **B2.5 的读根次序（Codex R42 #1，BLOCKING）**：先读根后认领会把前序 B3 的提交误判为绕过。改为**认领先行**（NOT EXISTS Running 谓词保证认领后无其他 B3），同事务内再读根落 `expected_*`。
  - **B3 的基线失配无可执行分支（Codex R42 #2，BLOCKING）**：补独立前置分支——不一致即 ROLLBACK + `hard_stopped` + `Failed(QueueBypassDetected)`，不做任何树/ref 工作（否则绕过写入被并入本轮根、CAS 反而成功）。
  - **意图状态机归一（Codex R42 #3，BLOCKING）**：`pending_action` 仅保留 `requeue_conflict`（重排/修复）；根绕过判定不经过它——reaper 对**每行 `Running`** 比对 `expected_*` 基线；1.6/schema/B3/ADR-TP-20 的 verify_root 预标记残留清除。
  - **交付物编号（Codex R42 #4，MINOR）**：B0 与修订记录的「交付物 11」统一为 10。：R41 评审 2 条 BLOCKING 全部落实。
  - **B2.5 的预期根 SQL 缺失（Codex R41 #1，BLOCKING）**：散文声称「认领时原子落库」但 SQL 只更新状态。B2.5 伪码补 `root0` 读取与 `expected_*` 同事务写入；B3 **只比对不覆写**（锁内现读 vs 基线，`IS NOT DISTINCT FROM`）；ADR-TP-20 的「独立连接预标记」段落删除（基线来自认领）。
  - **交付物 11 落空（Codex R41 #2，BLOCKING）**：1.10 实际只列到 9。补齐**交付物 10**（trunk 桥接：`build_push_chain` 的 `None`/`validate_incoming_push` 的 `:1174-1179` 空返回在 trunk 下构造描述符继续入队，含空 pack 且 `old_id != new_id` 情形；review 保留现状），B0 标题已同步，配 B1–B3 可达断言。：R40 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **预期根随认领原子落库（Codex R40 #1，BLOCKING）**：`verify_root` 原在 B3 内才持久化，认领后取锁前的死亡会让队列外根写入逃过判定。B2.5 认领事务内原子读根身份对并落 `expected_*`（认领后不存在其他合法根写入者，基线可靠）；B3/reaper 比对之。配交错回归。
  - **交付物 11 缺失与 B0 标题矛盾（Codex R40 #2，BLOCKING）**：补齐 **1.10 交付物 11**（trunk receive-pack 桥接的精确改动点：`build_push_chain` 的 `Noop` 短路在 trunk 下构造描述符并继续入队），B0 标题的「no-op 早退」改为「早期拒绝、无 no-op 快路径」。
  - **reaper 次序（Codex R40 #3，MINOR）**：明确「先 hard_stopped 止血 → 再 I3 修复 → 后终态」的事务内次序。：R39 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **reaper I3 校验按 kind（Codex R39 #1，BLOCKING）**：attach 无 `main@P` 行属正常（只写根 ref，分支 ref 在 Git DB）——校验按 kind 分语义，attach 跳过、不误报。配 attach reaper 回归。
  - **trunk receive-pack 桥接（Codex R39 #2，BLOCKING）**：`build_push_chain` 的 `Noop` 短路（`monorepo.rs:1191-1247`）会让空/全已知推送绕过 B1/B3。新增 **1.10 交付物 11**：trunk 形态下短路改为继续入队（review 保留现状）。配两种入队测试。
  - **B0 预检的行存在前提（Codex R39 #3，MINOR）**：NFF 预检仅当 `main@P` 行存在；无行直接走创建分支。：R38 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **存量端点映射（Codex R38 #1，BLOCKING）**：`merge_queue_router` 的 status/list/stats/cancel/retry/remove 逐一映射到 `push_queue` 等价物或废弃（410），UN-25 freeze 挂点随 B3 迁移；每端点配验收。
  - **trunk ⇔ push_auth 双向强制（Codex R38 #2，BLOCKING）**：`token/none ⇒ trunk` 之外补反向 `trunk ⇒ token/none`（缺省 OAuth 链在 trunk 下语义未定义）；4.1/阶段 5 同步。
  - **tombstone_repair 残留（Codex R38 #3，MINOR）**：ADR-TP-20/schema/reaper 的残留 `tombstone_repair` 归一为单一 `verify_root` + reaper 通用 I3 修复。：R37 评审 1 条 BLOCKING + 2 条 MINOR 全部落实。
  - **意图状态机统一（Codex R37 #1，BLOCKING）**：`tombstone_repair` 预持久化与 `verify_root` 并存会产生不可判定窗口。收敛为**单一 `verify_root` 意图 + reaper 通用 I3 修复**——reaper 对任何待终态化的 `Running` 行先做该行 path 的树哈希校验、陈旧即墓碑修复再终态化；枚举删除 `tombstone_repair`；ADR-TP-20/B3/1.6 同步。
  - **attach 载荷注释（Codex R37 #2，MINOR）**：1.4 schema 补「完整 create/update/delete 命令描述符」。
  - **创建分支的 N=0（Codex R37 #3，MINOR）**：创建场景 `old_id = ZERO_ID` 而 `new_id` 有效，N = 0 不可达——写模型与 B3 创建分支移除 N=0 情形。：R36 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **形态切换的队列残留（Codex R36 #1，BLOCKING）**：4.1 补第三条启动校验——`push_policy` 变更且存在非终态 `push_queue` 行 → 拒绝启动（要求排空/取消），双向校验 + 重启测试。
  - **merge 的缺失行 upsert 与现状 precheck 矛盾（Codex R36 #2，BLOCKING）**：`merge_cl`（`:2113-2118`）与执行期重查（`:4634-4647`）都拒绝缺失 ref。1.5 merge 分支改为**缺失行拒绝**（沿用现状）；upsert 原语只属于 push 创建语义（1.10 交付物 2 措辞收窄）。
  - **`push_auth` 与 review 形态的非法组合（Codex R36 #3，BLOCKING）**：`token`/`none` 强制蕴含 `push_policy = "trunk"`（4.1 第四条启动校验；阶段 5 配置注释同步）。
  - **事实校准 1 的删除例外（Codex R36 #4，MINOR）**：「只写 `refs/cl/*`」收窄为「非删除分支更新」。：R35 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **直连 merge 入口未入队（Codex R35 #1，BLOCKING）**：`/merge`、`/merge-no-auth`（`cl_router.rs:218-223/279-304`）直接调 `merge_cl`，在队列外执行 `merge_cl_unchecked`——未覆盖的根树写入者。1.9 2a/ADR-TP-05 明确**全部 merge 入口统一入队**（入口预检保留 + B1 + 同步等待），配并发回归。
  - **`/merge-queue/add` 的孤儿行（Codex R35 #2，BLOCKING）**：入队即返回依赖被退役的后台 processor。同步化（或废弃返回 410，二选一，验收覆盖）。1.9 2a 补入。
  - **物化校验的根身份对（Codex R35 #3，MINOR）**：遍历出发标记与重读比对改为 `(ref_commit_hash, ref_tree_hash)` 身份对（与 B3/reaper 同构）。：R34 评审 1 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **CL 单调 revision（Codex R34 #1，BLOCKING）**：仅比 `to_hash` 挡不住 rebase/merge 反序竞态（陈旧 rebase 可把 `Merged` 改回 `Open`；两侧现状都是无条件更新）。交付物：`mega_cl` 加单调 revision 列，merge/rebase 行更新一律全量 CAS，0 行命中即 `ClaimLost`；配反序回归。
  - **修订元数据陈旧（Codex R34 #2，MINOR）**：首版计数行更新为 17 条事实校准 / ADR-TP-01–TP-20（另含 15a）/ 7 条不变式 / 10 项交付物。
  - **根路径推送显式拒绝（Codex R34 #3，SUGGESTION 采纳）**：B0 拒绝 `P = "/"`（子路径推送是唯一形态；根路径会破坏「每轮至多一次根写入」的唯一性）。：R33 评审 1 条 BLOCKING 全部落实。
  - **verify_root 等根路径的 I3 缺口（Codex R33 #1，BLOCKING）**：通用意图提交后、陈旧行检出前崩溃时，行带 `verify_root` 终态化而 reaper 只比根（根相等即普通 Failed），陈旧行无人修复。reaper 的 `verify_root` 等根路径**补做该行 path 的 I3 校验与墓碑修复**（根相等不代表 path 层一致——净零推送可在根不变时推进 `main@P`）。配回归。：C-R3 评审 1 条 BLOCKING + 3 条 MINOR + 2 条 SUGGESTION 全部落实。
  - **创建语义的树项插入原语（Claude C-R3 #1，BLOCKING）**：`update_tree_hash` 只定位不插入、`search_tree_for_update` 对缺失组件硬报错——「真创建」无可执行原语。新增 **1.10 交付物 6**：insert-or-replace 树项变体（`TreeItemMode::Tree`，遵守 `from_tree_items` 排序，支持多级创建），前置校验改引该原语；配单级/多级创建回归。
  - **崩溃收养的两分支（Claude C-R3 #2，MINOR）**：阶段 1 验收「收养该行」与「reaper 立即终态化」矛盾。改为按行状态二分：`Running` → 收养；已终态 → 以载荷新建行。
  - **attach 指纹残留（Claude C-R3 #3，MINOR）**：1.4/B3/1.11 三处「attach 请求 id」统一为内容寻址指纹；不可达负例论证改为内容寻址依据。
  - **reaper 跳过/告警的门（Claude C-R3 #4，MINOR）**：物化与巡检批次也持 `MONO_WRITE_LOCK`，「拿不到锁 ⇒ 有轮次在跑」不再成立；跳过与告警改以「存在 `Running` 行」为门。
  - **措辞（Claude C-R3 #5，SUGGESTION 采纳）**：2.1「唯一途径」改「主要途径」；硬约束 2 的 `apply_update_result` 引用更正为定义点 + 唯一生产调用方。：R31 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **CAS 意图的全 kind 覆盖（Codex R31 #1，BLOCKING）**：`verify_root` 标记原先只在 push 分支，merge/attach 的 CAS 失败崩溃后 reaper 无意图可判。移入 B3 **通用路径**（root 读取后、任何分支的根 CAS 之前），配三 kind 崩溃回归。
  - **attach 操作标识的确定性导出（Codex R31 #2，BLOCKING）**：attach 由 receive-pack 自动触发（`protocol/mod.rs:175-216`，`RefCommand` 无请求 id，git 重试也不携带令牌），「调用方令牌」不可用。`operation_id` 改为 `hash(仓库标识 ‖ 规范化命令描述符)` 的确定性导出。1.3/1.9 同步。
  - **范围外的 tag 澄清（Codex R31 #3，MINOR）**：tag ref 行由 tag API 独立管理，与 `ClSyncChecker` 无关；硬约束 2 的范围外声明分开表述。：R30 评审 1 条 BLOCKING + 2 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **纯删除式 attach 不入队（Codex R30 #1，BLOCKING）**：`import_repo.rs:450-475` 的纯删除 attach 只删 ImportRepo 自身 refs、不触碰根树，与「每个 attach 执行一次根 CAS」矛盾。硬约束 2/1.9/B3 明确拆分：含分支命令的 attach 入队，纯删除式登记为范围外。
  - **术语澄清（Codex R30 #2，MINOR）**：1.9 第 4 条改名「陈旧物化 main 行」，与 CL 基线漂移（Conflict 重排）显式区分。
  - **Mono-Squash-Range 验收限定（Codex R30 #3，MINOR）**：仅约束有基线的推送行，创建情形按 I4 省略。
  - **token 授权独立于 Cedar（Codex R30 #4，SUGGESTION 采纳）**：阶段 5 明确路径前缀匹配在 `check_push_permission` 的 Cedar 早退之前执行。：C-R2 评审 1 条 BLOCKING + 6 条 MINOR + 2 条 SUGGESTION 全部落实。
  - **阶段 1 验收混入 trunk 专属用例（Claude C-R2 #1，BLOCKING）**：清单前言与「阶段 1–3 可独立验收」矛盾。采纳其方案 (b)：新增 **1.10 交付物 9**（测试专用 `push_policy` 开关，只接通配置读取与 kind 分派，不含阶段 4 协议接线），push 用例在该开关下运行；前言改写。
  - **事实校准 1 的调用路径（Claude C-R2 #2，MINOR）**：`smart.rs:450` 只对 tag 调 `update_refs`，分支命令实际经 `finalize_receive_pack` → `persist_mono_branch_cl_mega_refs_transaction`；4.2 表首行（`update_refs` 行）删除并入 `finalize_receive_pack` 行。
  - **硬约束 1 的适用范围（Claude C-R2 #3，MINOR）**：「每一次推送」改为「每一次**落地**（trunk 推送/CL merge/attach）」，与硬约束 2 的 review 推送不入队一致。
  - **reaper 重排插入的准入锁（Claude C-R2 #4，MINOR）**：1.6 明写继任行 INSERT 取 `queue_control FOR UPDATE`（`MONO_WRITE_LOCK` 与准入锁互不排斥）。
  - **验收残留两处（Claude C-R2 #5，MINOR）**：空 pack 措辞对齐「N=0 当且仅当 old_id == new_id」；`queue_control` 播种移回阶段 1 验收，stage-5 启动测试留阶段 5 并补播种断言。
  - **队列外 notify 的版本来源（Claude C-R2 #6，MINOR）**：删除路径 best-effort notify（`monorepo.rs:903-907`）不参与单调版本比较，改走 dirty 标记 + 补偿重建。
  - **reaper 宽限（Claude C-R2 #7，SUGGESTION 采纳）**：`started_at` 秒级宽限减少无谓 ClaimLost（不参与正确性）。
  - **git 重试理由（Claude C-R2 #8，SUGGESTION 采纳）**：5xx 映射的理由改为「使失败可见」，删除不成立的客户端重试论断。：R28 评审 3 条 BLOCKING + 2 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **N=0 收敛为退化重推（Codex R28 #1）**：N=0 当且仅当 `old_id == new_id`；空 pack / 对象已知不再参与 N 分类（N 锚定客户端基线、随描述符持久化）。B3 分支、写入模型、B0、验收同步。
  - **预期根标记前移（Codex R28 #2）**：标记的持久化移到**根 CAS 之前**（独立连接提交，标记失败即中止本轮），CAS 成功后清除——「CAS 已执行而意图未落库」在伪码序上不可构造。
  - **预期根身份对（Codex R28 #3）**：`expected_root` 拆为 `expected_commit_hash`/`expected_tree_hash`（缺失行以 NULL 哨兵），reaper 判定与 CAS 同一谓词——tree-only 绕过不再漏判。
  - **B2.5 硬停措辞（Codex R28 #4，MINOR）**：弃权回 Queued、B2 循环继续，与修订 32 的保持等待语义一致。
  - **评估小结的 reaper 措辞（Codex R28 #5，MINOR）**：「只清帐不修数据」更正为清帐 + 幂等补做意图 + I3 顺带校验。
  - **B4 的 SAVEPOINT 时点（Codex R28 #6，SUGGESTION 采纳）**：SAVEPOINT 建立在任何可失败业务写之前；语句错误中止事务时退回独立状态事务路径（意图已先行持久化）。
- **同日修订 32（第二十七轮 Codex 评审后）**：R27 评审 4 条 BLOCKING + 3 条 MINOR + 2 条 SUGGESTION 全部落实。
  - **N 锚定客户端基线（Codex R27 #1）**：N 改为 `new_id→old_id`（客户端声明基线）的段长，随描述符持久化、重试不重算——被拒重试不再因对象已知退化为 fast-forward（违反 ADR-TP-12）。
  - **upsert 原语强制化（Codex R27 #2）**：1.10 交付物 2 由「upsert 或报错」改为**必须提供事务内 upsert/create 原语**（创建语义依赖它）。
  - **pre-CAS 预期根标记（Codex R27 #3）**：CAS 失败的意图改为**先于 CAS 持久化**（独立连接写 `expected_root` + `verify_root`），「CAS 已执行而意图未落库」不可构造——绕过判定无未判定窗口；reaper 依据 `expected_root` 判定（根动 ⇒ 硬停 + BypassDetected，根未动 ⇒ 普通 Failed）；「可接受残余」声明删除。
  - **reaper 的 I3 顺带校验（Codex R27 #4）**：无意图孤儿 `Running` 行终态化前，对其 path 做一次树哈希校验，陈旧即就地墓碑修复——封住墓碑意图前崩溃的 I3 窗口。
  - **ClaimLost 指标（Codex R27 #5，MINOR）**：由「应恒为 0」改为安全弃权的观测值。
  - **硬停期间的生命周期（Codex R27 #6，MINOR）**：B2 等待者保持等待（B2.5 弃权回 Queued），heartbeat 清理在 `hard_stopped` 期间冻结，孤儿行保留排队意图待人工 clear。
  - **门控清单（Codex R27 #7，SUGGESTION 采纳）**：1.9/B3 写明「全部既有门控原样保留（含 Closed/Draft 门）」。
  - **Mono-Squash-Commit 不可变语义（Codex R27 #8，SUGGESTION 采纳）**：定义为不可变 commit id，非活引用。
- **同日修订 31（第二十六轮 Codex 评审后）**：R26 评审 2 条 BLOCKING + 1 条 SUGGESTION（正面确认）全部落实。
  - **多收养者的断连误杀（Codex R26 #1）**：客户端断连「标记 Cancelled」会杀掉其他收养者的排队操作。断连改为**自行退出、不改行状态**；行由其余收养者的心跳维持，全部离开后由 heartbeat reaper 兜底。1.6 崩溃表/B2 同步。
  - **B1 准入未检查 `hard_stopped`（Codex R26 #2）**：B1 原子谓词补 `NOT hard_stopped`（新入队拒绝），`Done` 回放不受限；已在队行保留排队意图（B2.5/B3 弃权但不终态，clear 后重新竞争）——语义在 B1 分类与验收中成文。
  - **创建语义的自相矛盾（Codex R25 #1）**：修订 28 把创建改为「与 N 无关」与表格/验收的 parentless squash 冲突。定为**按 N 分流**：N = 0/1 → `main@P = cmd.new_id`（`landed_commit_id` = `new_id`）；N > 1 → parentless squash（`landed_commit_id` = squash id，`new_id` 保留客户端 tip，provenance/GC 锚点分开）。表格/段落/B3/验收一致化。
  - **reaper 终态化后的重试路径（Codex R25 #2）**：1.3 的「收养」只覆盖 `Queued/Running`，reaper 标 `Failed` 后的重试无路可走。补双路径：活跃行 → 收养；终态行（无继任）→ **以持久化载荷新建队列行**（描述符完全可执行）。1.3/1.6/1.11/B1 分类/验收一致化。
  - **`push_auth` 缺省语义（Codex R25 #3，MINOR）**：缺省 = 现有 OAuth/UserStorage 认证链（review 形态行为不变）；`token`/`none` 为显式 storage-only 模式。
  - **硬停重置不可持久化（Codex R24 #1）**：「B3 内改回 Queued」随回滚一起撤销，行停留 Running 且会被 reap 为 Failed。改为**独立提交的条件重置事务**（`Running → Queued`）+ crash/reaper 语义：硬停期间 reaper 的清理动作是条件重置回 Queued（非 Failed），排队意图保留。
  - **merge 入口预检与「无闸门」声明的冲突（Codex R24 #2）**：`merge_cl` 入口拒绝 `cl.from_hash != main`（`mono_api_service.rs:2107-2126`，`cl_router.rs:203-223/270-304`）是现状行为。1.9/ADR-TP-10 澄清：「无 push 式 old_id 闸门」指队列不加新闸门，**既有入口预检原样保留**（硬约束 8），等待期漂移由执行期冲突重查处理。
  - **pending_action 的载荷编码（Codex R24 #3，MINOR）**：意图不携带额外载荷——修复/重排输入全部可从本行列 + reaper 持锁现读重建；幂等守卫（状态条件 + `superseded_by IS NULL`）成文。
  - **硬停的 B3 侧检查缺失（Codex R23 #1）**：`hard_stopped` 只在 B2.5 前检查，认领先于硬停的轮次仍会在持锁后写入。B3 取锁后、业务读写前补**事务内硬停检查**（命中 → 行回 `Queued` 等待人工处理，不终态）；1.7 拆分 `resume`（清 `paused`）与 **`clear-hard-stop`**（清 `hard_stopped` + 审计），CAS 验收从 `paused` 改为 `hard_stopped` 语义。
  - **创建分支的 N=0 未定义（Codex R23 #2）**：创建落地改为**与 N 无关**——统一 `main@P = cmd.new_id`（经 non-ff 与树哈希断言），N=0/N=1 同规，N>1 折叠仅在全新多 commit 链时适用；写模型与 B3 创建分支同步。
  - **文件索引的出现对语义（Codex R23 #3，MINOR）**：单 `file_path` 列无法表达同 blob 多路径（`monorepo.rs:1051-1111` 会遇到），代际水位可能误删他处引用。ADR-TP-11 交付物改为出现对表 `blob_paths(blob_id, path, indexed_push_id)`，删除按出现对精确执行；配多路径回归。
  - **空 pack 快路径的非权威性（Codex R22 #1）**：B0 无锁读 ref 后直接回 ok，会在并发 B3 推进 tip 后虚报成功并绕过路径排除与树哈希修复。**取消 B0 快路径**，空 pack 照常入队，no-op 语义由 B3 权威实现（指纹回放拦截重复）。
  - **N=0 是真实落地路径（Codex R22 #2）**：「无 ref/树变更」错——被拒推送的对象已持久化（`monorepo.rs:248-257`），tip 未动的重推应**快进落地**（build_result_by_chain 以 new 的 tree 计算祖先 roll-up，`main@P` 前进至 new_id）；`new_id == tip` 才是无变更 no-op。B3 N=0 分支重写，验收改为三分类断言。
  - **bypass 暂停与 drain-only 暂停混用（Codex R22 #3）**：`paused` 是 drain-only（B2.5/B3 不检查），bypass 后在队项照常执行，违背 fail-closed。`queue_control` 新增 **`hard_stopped`**（bypass 专用硬停，B2/B2.5/B3 检查），与维护性 `paused` 分离；CAS 失败置位 `hard_stopped`。配回归。
  - **多收养者的 wait_timeout 误杀（Codex R22 #4，MINOR）**：任一超时者条件取消共享行会误杀仍在等待的收养者。改为**放弃而非取消**——超时者自行退出，行留给其他收养者与 heartbeat reaper 兜底。ADR-TP-08/B2 同步。
  - **存量 refs/cl 的广告过滤（Codex R22 #5，SUGGESTION 采纳）**：trunk 形态下存量已合并 CL ref 仍被 advertise，与「refs/cl 零新增」验收冲突。4.2 验收改为「新增为零 + advertise 过滤 CL refs（存量行留档）」。
  - **物化插入失去竞态的返回语义（Codex R21 #1）**：`NOT EXISTS` 命中既有行时物化不得返回自身预计算 ref（净零推送可在根 hash 不变时推进 `main@P`），必须重读持久化行返回。配回归。
  - **空 pack no-op 的权威前提（Codex R21 #2）**：「空 pack 且 new_id 已知 → ok」会在 new_id 非 tip 或 P 未物化时虚报成功。收紧为 `new_id == main@P 当前 tip` 才早退，其余照常入队由 B3 裁决。写入模型同步。
  - **ADR-TP-10 的「必然失败」措辞（Codex R21 #3，MINOR）**：改为保守序列化表述（N=0 合法 no-op 重推的例外已由回放/收养覆盖）。
  - **ADR-TP-08 的 wait_timeout 范围（Codex R21 #4，MINOR）**：与 B2/1.9 2a 对齐——只约束 Queued 等待。
  - **schema id 注释（Codex R21 #5，MINOR）**：「trunk 时间线序号」改为与 ADR-TP-06 一致的全局队列操作序表述。
  - **N=0 到达 B3 的可执行结局（Codex R20 #1）**：非空已知 pack 入队后 B3 声称「N=0 理论不可达」与入队规则矛盾。B3 补 N=0 分支：闸门照常运行，通过 → no-op 落地（行 `Done`、`landed_commit_id` = 当前 tip），失败 → 常规拒绝；写入模型与验收同步。
  - **attach 收养的可复现性（Codex R20 #2）**：`{repo 上下文}` 不足以重放分支级变更（`import_repo.rs:450-475/550-573` 应用完整快照，`protocol/mod.rs:175-216` 的命令列表在内存中）。attach 载荷补**完整命令描述符**；`operation_id` 改为**调用方生成的幂等令牌**（服务端随机 id 不跨重试稳定）。1.3/1.9 同步。
  - **授权屏障的跨实例失效（Codex R20 #3）**：`SharedEntityStore` 是进程本地内存快照，全局版本在实例 A 推进不更新实例 B。1.2 屏障改为**每实例授权重查前自校**：比对 DB 权威版本与本进程快照版本，落后即本进程重建后再判定。
  - **B0 预检的收养例外（Codex R20 #4，MINOR）**：同路径预拒绝限定为「不同 operation_id」；同指纹重试由 B1 收养。
  - **收养与拒绝的残留矛盾（Codex R19 #1）**：1.11 查重条目与阶段 1/4 验收仍有「重复操作进行中」拒绝。全部统一为收养（拒绝仅存于 ADR-TP-10 的同路径不同操作）；`pending_action` 列注释补 `pause_bypass`。
  - **无 OAuth 运行时形态（Codex R19 #3，SUGGESTION 采纳）**：阶段 5 交付物 1 补协议专用 router 集、匿名/no-op 会话存储变体（`api_store.rs:4-14` 现仅 Website 变体）与不注册的 OAuth 路由清单。
- **同日修订 23（第十八轮 Codex 评审后）**：R18 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **B0 与收养规则的矛盾（Codex R18 #1）**：B0 的活跃指纹命中仍写「拒绝」，与修订 22 的收养规则冲突。改为「收养（B1 统一规则）」；拒绝仅保留给 ADR-TP-10 的同路径**不同**操作。
  - **N=0 no-op 的过度放宽（Codex R18 #2）**：非空 pack 的已知对象推送被当 no-op 会让 A 段落库后被拒的推送重试时虚报成功（`PushChain::resolve` 对已知 tip 返回 Chain 并重验，`push_chain.rs:104-111/160-163/275-288`）。no-op 收缩为 **ADR-MC-05 的空 pack 情形**（或已确认的 Done 回放）；非空已知 pack 照常校验入队、由 B3 闸门裁决。N 定义与 B0/验收同步。
  - **收养所需的持久化描述符（Codex R18 #3）**：A 段落库后对象存在性无法区分 N=1/N>1 与 fork 点（`PushChain` 需要 `new_commit_ids` 全集）。1.3/1.4 补**最小必要载荷 `payload`**（push=推送描述符 `{commits, fork_base, n}`；merge=`{cl_link}`；attach=`{repo 上下文}`），B0 算出、B1 持久化、B3 只读；收养执行依据载荷。配 N=1/N>1 收养回归。
  - **后代查询的自我排除（Codex R18 #4，MINOR）**：2.2 的候选集查询补 `Path != p`（`p="/"` 时 pattern 会命中根行自身；现状 `remove_none_cl_refs` 显式排除、根路径 merge 受支持），配根路径回归。
- **同日修订 22（第十七轮 Codex 评审后）**：R17 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **收养与 B1 拒绝的矛盾（Codex R17 #1）**：B1 对 `Queued/Running` 命中一律拒绝，与 1.11 要求的收养冲突。B1 定为**三分支状态机**：`Done` → 回放、`Queued/Running` → 收养（1.11 收养规则升级为唯一动作，无拒绝分支）、未命中 → 插入；同路径不同指纹的第二个 push 仍由 ADR-TP-10 路径索引拒绝。
  - **merge `new_id` 与 rebase 的不一致（Codex R17 #2）**：执行时读取**当时** `cl.to_hash`（`mono_api_service.rs:2595-2614`），rebase 后落地 commit 的 parent 可能不是入队时点的 `new_id`。1.4/1.9/I4 统一：`new_id` 为入队时点快照（信息性），落地面以 `landed_commit_id` 及其 parent（执行时点 `to_hash`）记录，CL 链 GC 锚点相应调整。
  - **「无崩溃间隙」的过度声明（Codex R17 #3，MINOR）**：CAS 失败路径同样存在意图前窗口。CAS 失败并入两段式意图（新枚举 `pause_bypass`），reaper 幂等补做 pause；验收限定为「意图提交后的窗口无间隙」，意图前窗口登记为可接受残余并验证可见性。
- **同日修订 21（第十六轮 Codex 评审后）**：R16 评审 2 条 BLOCKING + 1 条 MINOR 全部落实。
  - **冲突重排继任行的执行者缺口（Codex R16 #1）**：reaper 补做重排后原调用方可能已不在，继任行无人执行，而同指纹重试被「重复操作进行中」拒绝。1.11 补**收养规则**：三态命中 `Queued/Running` 时调用方可收养既有行（进入 B2 循环、接管心跳；B2.5 原子认领保证多收养者下恰一个执行者；push 收养安全性的依据成文——落地面由队列行与对象库决定）；1.6 补 reaper 补做重排的旧行终态为 **`Cancelled(Conflict)` + 可跟随的 `superseded_by`**（`Failed` 不可跟随）。配提交后丢响应/崩溃与多收养者回归。
  - **`push_auth = "none"` 的端点旁路缺失（Codex R16 #2）**：`git_info_refs`/`git_receive-pack` 在 `check_push_permission` 之前就要求认证（`git_protocol/http.rs:60-65`/`:354-365`），只改权限判定不生效。阶段 5 交付物 3 补双端点模式感知旁路；「401」验收限定 token 模式。
  - **trait 第二实现（Codex R16 #3，MINOR）**：`refs_with_head_hash` 的签名改造须同步 `import_repo.rs:84-94` 的实现。ADR-TP-20 交付物补入。
- **同日修订 20（第十五轮 Codex 评审后）**：R15 评审 3 条 BLOCKING + 2 条 MINOR 全部落实。
  - **SAVEPOINT 的残余崩溃窗口（Codex R15 #1）**：SAVEPOINT 提交前崩溃仍会回滚墓碑/继任行，reaper 标 `Failed` 后修复丢失。改为**持久化意图两段式**：先在独立短事务把 `pending_action`（新枚举列：`tombstone_repair`/`requeue_conflict`）写入队列行，再 SAVEPOINT 完成并清除意图；意图提交后崩溃由 reaper **幂等补做**（1.6 reaper 职责扩展），意图前崩溃由「检测可重入 + 周期巡检」兜底。1.4/1.5 B4/1.6/验收同步。
  - **硬约束 8 与队列生命周期变化的矛盾（Codex R15 #2）**：硬约束 8 补第二类显式豁免——merge 的背压、`wait_timeout`、异步转同步（1.9 2a）是统一队列的必然伴随物，登记在案且验收覆盖。
  - **trunk 形态的 code_edit CL 变更面（Codex R15 #3）**：`preview_router.rs:46-59` 的 `/create-entry`、`/edit/save` 经 `find_or_create_cl_for_edit`（`on_edit.rs:162-214`）变更 CL，4.3 关闭清单补入（只读 preview 保留），配无 CL 变更验收。
  - **物化错误传播的签名改造（Codex R15 #4，MINOR）**：`refs_with_head_hash` 现不可失败（`pack/mod.rs:90`）、`ProtocolError` 把 `MegaError` 映射为 400（`errors/mod.rs:200-216`）。ADR-TP-20 交付物补 trait 签名、smart.rs/v2.rs 传播与 5xx 映射。
  - **现状对比表的身份措辞（Codex R15 #5，MINOR）**：「身份取自 commit 署名」改为「认证身份为 token 名，commit 署名仅作 provenance」。
- **同日修订 19（第十四轮 Codex 评审后）**：R14 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **N=0 早退掩盖活跃重试（Codex R14 #1）**：B0 的指纹查重只查 `Done`，原轮次 `Queued/Running` 期间的重试会因 N=0 早退回 ok，把原轮次可能的失败掩盖成成功。B0 查重扩展到三态（活跃命中 → 拒绝「重复操作进行中」）。配回归。
  - **trunk 形态的 finalize 未条件化（Codex R14 #2）**：分支命令不经 `update_refs`（`smart.rs:445-471`），`finalize_receive_pack`（`monorepo.rs:205-209/863-877`）才是分支 ref 的唯一变更点——仅改 `update_refs` 不会让 trunk 跳过 CL 持久化。4.2 改为 `finalize_receive_pack` 按 `push_policy` 条件化（跳过 `persist_mono_branch_cl_mega_refs_transaction` 与 CL post-push），配 `refs/cl/*` 行数为零的验收。
  - **merge 预计算结果跨等待沿用（Codex R14 #3）**：`merge_cl_unchecked` 在应用前预计算树结果（`mono_api_service.rs:2582-2614`），排队等待后沿用会覆盖 intervening 写入。merge 分支补「锁内从当前根重算 TreeUpdateResult」。配 intervening 保留回归。
  - **queue_control 播种缺失（Codex R14 #4，MINOR）**：单行表需迁移/启动时 `ON CONFLICT DO NOTHING` 播种，否则 `FOR UPDATE` 无行可锁、准入序列化失效。1.4 补注释与验收。
- **同日修订 18（第十三轮 Codex 评审后）**：R13 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **SAVEPOINT 修复的残留矛盾（Codex R13 #1）**：1.10 交付物 7、阶段 1 验收、风险 6 与 I3 仍写「B4 回滚后独立提交修复事务」，与修订 12 已定的 SAVEPOINT 同事务方案冲突（重新引入崩溃窗口）。四处统一为 SAVEPOINT 同事务措辞。
  - **B1 的双 INSERT 竞态（Codex R13 #2）**：伪码里「条件 INSERT 探针 + 正式 INSERT」两步要么插两次、要么退回 check-then-insert。改为**单语句 `INSERT ... SELECT`**：三态 NOT EXISTS + paused + 容量谓词一体，`RETURNING id`；0 行命中时在同一事务内只读回查分类（Done 回放 / 重复操作 / 拒绝）。参考现有形状 `merge_queue_storage.rs:56-103`。
  - **授权快照的乱序发布（Codex R13 #3）**：仅「持久化版本水位」挡不住两个 C 段 notify 乱序完成——旧快照后到覆盖新快照。1.2 补三件套：B3 事务内 notify **outbox**（version = push_queue.id，崩溃后补偿重放、以最新根重建）、快照发布的**单调 CAS**（`published_version < $v` 才写，发布序 = id 序；现状为内存级 best-effort 需改造）、B3 授权重查的读取屏障。配乱序与 notify 前崩溃回归。
  - **landed_commit_id 的落库时点（Codex R13 #4，MINOR）**：B3 的 Done 条件更新补 `landed_commit_id = ?`，与 1.4/1.11 的回放权威依据对齐。
- **同日修订 17（第十二轮 Codex 评审后）**：R12 评审 5 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **N=0 早退吞掉 Done 回放（Codex R12 #1）**：已成功的 N > 1 推送重试时链上对象全部已知、N 计为 0，先行的 N=0 早退会绕过 1.11 回放。B0 改为指纹查 `Done` 先于 N=0 判定，未命中才走普通 no-op。
  - **B4 后置修复的崩溃间隙（Codex R12 #2）**：ROLLBACK 与独立修复事务之间的崩溃会让 reaper 标 `Failed` 而墓碑/继任行缺失。树哈希修复与冲突重排全部改为 **SAVEPOINT 同事务**（撤数据写 → 写墓碑/重排/终态 → 原子提交），崩溃间隙不可构造；配回归。
  - **授权快照的屏障缺失（Codex R12 #3）**：`notify.rs:27-73` 异步重建 Cedar snapshot，下一轮 B3 的 UN-19 重查可能读旧快照。1.2 C 段补例外：enforce 模式下快照重建带持久化版本水位，B3 授权重查前 fail-closed 等待追平；off 模式无屏障。配回归。
  - **索引删除复活（Codex R12 #4）**：行级 `indexed_push_id` CAS 只保护仍存在的 blob，删除的 blob 会被旧任务写回旧路径。ADR-TP-11 补**删除感知的按代 reconciliation**（以 tip 子树全集为界做 diff 清理）。配回归。
  - **trunk 分支名准入缺失（Codex R12 #5）**：`primary_branch_command`（`push_chain.rs:469-482`）接受任意非 tag 分支，「唯一公开分支 main」未被算法落实。B0 补 `ref_name == MEGA_BRANCH_NAME` 强制。配回归。
  - **wait_timeout 语义（Codex R12 #6，MINOR）**：明确其只约束 Queued 等待；Running 轮次的调用方等待真实终态，执行时长由 B3 成本上界与 `stuck_timeout` 告警保障（1.9 2a）。
  - **存量迁移与滚动部署（Codex R12 #7，SUGGESTION 采纳）**：1.8 补四步切换——排空 → 数据迁移 → 单写者开关（重启生效、双开拒绝启动）→ 禁止混合版本混跑。配校验测试。
- **同日修订 16（第十一轮 Codex 评审后）**：R11 评审 4 条 BLOCKING + 1 条 MINOR 全部落实。
  - **FIFO 准入序（Codex R11 #1）**：B1 原先把 `queue_control FOR UPDATE` 放在 INSERT 之后——先到事务未提交时后到者先拿更大 id 并提交，B2.5 会误把后者当队首，破坏 ADR-TP-06/I6。改为**准入锁先行**：`queue_control` 单行 FOR UPDATE 串行化全部入队路径（含 B4 冲突重排），id 在锁内分配，id 序 = 提交序 = FIFO 序；容量门槛先查后插，恰好容纳 `max_depth` 项。配交错回归。
  - **同步冲突重排的执行者交接（Codex R11 #2）**：后台 processor 已退役，B4 重排出的继任行无人执行。B4 补 `superseded_by` 链接列并显式「新独立事务 + 准入锁」；B2 对 `Cancelled` 行区分「有继任 → 跟随」「无继任 → 真取消拒绝」。配交接回归。
  - **operation_id 的残留不一致（Codex R11 #3）**：1.9 表的 push 行仍是 `cmd.new_id`，与 1.4/1.11 的指纹矛盾。统一为 `old_id→new_id` 指纹。
  - **B2 超时的认领竞态（Codex R11 #4）**：wait_timeout 的条件取消命中 0 行（行已被认领）时原伪码仍拒绝，而 B3 照常执行——调用方误拒但数据落地。改为「取消仅及于 `Queued`；已 Running 则继续等待终态」。配回归。
  - **committer date 的墙钟回拨（Codex R11 #5，MINOR）**：仅取执行时刻在 NTP 回拨下仍会倒流。ADR-TP-14 改为 `max(执行时刻, 前一 trunk commit 的 committer date)`。
- **同日修订 15（第十轮 Codex 评审后）**：R10 评审 7 条 BLOCKING + 1 条 MINOR + 2 条 SUGGESTION 全部落实。
  - **N 的定义与 PushChain 实际不符（Codex R10 #1）**：已知 tip 的非空 pack 会让 `resolve` 返回完整链（`push_chain.rs:275-288`），按 `ordered_commits.len()` 计 N 会误入 squash。N 改为「自 tip 到第一个服务端已知 commit 的新引入前缀段长」，N = 0 早退覆盖非空 pack。配 N 计数回归。
  - **`max_push_commits` 可调的前提（Codex R10 #2）**：`PushChain::resolve`/`validate` 硬编码 250（`push_chain.rs:182-205, 408-447`）。ADR-TP-17 补交付物：链长上界参数化（review 传常量、trunk 传配置），未参数化前不宣称可调。
  - **B4 缺 Conflict 重排分支（Codex R10 #3）**：B4 补显式分支——冲突在同一事务内原子「关旧行（`Cancelled/Conflict`）+ 队尾 INSERT 新行」，与其他失败的 `Failed` 分支并列。
  - **陈旧 merge 修复后重试的语义（Codex R10 #4）**：重物化的新 tip 与 CL `from_hash` 基线的关系交由既有冲突重查裁决（Conflict → 重排/rebase），「重试成功」改为「重试进入常规门控、不再被陈旧行阻断」；断言先于门控重查的次序成文。配回归。
  - **B1 判定次序（Codex R10 #5）**：三态查重（`Done` 回放）先于 paused/深度门槛——暂停或已满不得吞掉已完成操作的回放。
  - **物化放弃的协议落点（Codex R10 #6）**：现状空 refs 编码为 capabilities-only，客户端不重试。交付物补：物化放弃时服务端有界重试（K=2 + 退避），仍失败返回可诊断错误，绝不静默空 refs。配回归。
  - **merge 与 rebase 的串行化（Codex R10 #7）**：CL 行状态更新带版本 CAS（`WHERE to_hash = 读时值`），并发 rebase 命中 0 行即 `ClaimLost` 弃权。配回归。
  - **operation_id 非 GC root 的残留（Codex R10 #8，MINOR）**：阶段 1 验收的创建语义条目改回 `new_id` 锚定。
  - **根 CAS 双条件（Codex R10 #9，SUGGESTION 采纳）**：B3 根 CAS 与 attach 一致携带 `(ref_commit_hash, ref_tree_hash)` 双条件。
  - **墓碑锚点进 GC root 清单（Codex R10 #10，SUGGESTION 采纳）**：I4 补墓碑行的 `last_commit_hash`/`last_tree_hash` 必须与队列锚点同等保留。
- **同日修订 14（第九轮 Codex 评审后）**：R9 评审 6 条 BLOCKING + 1 条 MINOR 全部落实。
  - **推送指纹的一致性（Codex R9 #1）**：B0 的查重示例仍写 `operation_id=cmd.new_id`，与 1.4/1.11 的指纹定义矛盾（N > 1 重试会漏检而重复执行）。统一为指纹 `old_id→new_id`。
  - **GC root 与指纹的混淆（Codex R9 #2）**：`operation_id` 已是文本指纹，不能再当对象 GC root。创建情形与 I4 的锚点改为 `new_id`（链 tip），并注明 operation_id 不在 GC root 之列。
  - **OAuth 启动门槛的遗漏（Codex R9 #3）**：`start_http()` 在 `app()` 之前还有一道 `require_oauth_for_http_service`（`http_server.rs:424-425`，`config/validate.rs:162-169`）。事实校准 17 与阶段 5 交付物 1 补为「两道门槛」并配启动测试。
  - **巡检与 B3 的竞态（Codex R9 #4）**：周期巡检扫描根 R0、B3 提交 R1 后，巡检会依据陈旧比对误删合法 ref。ADR-TP-20 第 3 项补「比对 + 修复必须在持有 `MONO_WRITE_LOCK` 的事务内分批执行」。配交错回归。
  - **CAS 失败处理的崩溃间隙（Codex R9 #5）**：「ROLLBACK 后独立事务标记」存在崩溃间隙（标记前崩溃 → 已回滚未 pause）。B3 改为 **SAVEPOINT 语义**：回滚到保存点撤销数据写入后，在同一事务内提交 pause + `Failed` 终态，三件事原子完成。伪代码、1.5 说明与验收同步。
  - **A 段对象的原子性边界（Codex R9 #6）**：B3 回滚无法撤销 A 段已落对象库的 unpacked 对象（`monorepo.rs:248-257` 记录的现状垃圾）。1.2 A 段与验收措辞改为「无孤儿元数据与 ref 状态」，对象回收归入既有对象 GC 议题。
  - **根断言的适用范围（Codex R9 #7，MINOR）**：「每轮恰好一次根 CAS」限定为「抵达根写入点的成功轮次」——早期闸门拒绝的轮次走 B4、无根写入。
- **同日修订 13（第八轮 Codex 评审后）**：R8 评审 5 条 BLOCKING + 2 条 MINOR 全部落实。
  - **首次推送的物化竞态（Codex R8 #1）**：B0 判「无行」与 B3 锁内重读之间，物化可插入合成行，创建推送被通用 non-fast-forward 文案误拒。B3 补可诊断分支：old_id = ZERO_ID 且行已在等待期间出现 → 拒绝并提示 fetch 对齐后重推；真创建（路径不在根树）无此竞态的论证成文。配回归。
  - **merge 执行模型的契约变化（Codex R8 #2）**：现状 merge 是异步（入队即返回 + 后台 processor，`mono_api_service.rs:3724-3755/4343-4386`），与「调用方全程在线阻塞 B2」矛盾。1.9 增补 2a：吸收后 HTTP merge handler 同步阻塞，后台 processor 退役——登记在案的契约变化，「HTTP 表面保留」仅指路由与请求/响应形状。
  - **卡死轮次的升级路径（Codex R8 #3）**：try-lock 拿不到锁时假死 B3 会让队列永久停摆，仅告警不够。1.6 补升级程序：告警附 `pg_stat_activity` 持锁会话快照 → runbook 人工授权 `pg_terminate_backend`（事务整体回滚 + reaper 清理）→ 可选自动 watchdog（默认关闭，条件严格匹配）。配验收。
  - **推送指纹（Codex R8 #4）**：`operation_id = cmd.new_id` 不足以判别推送意图——同一 tip 可经不同基线提交（A→C 与 B→C），按 new_id 查重会误回放并绕过 non-fast-forward 校验。push 的 `operation_id` 改为指纹 `old_id → new_id`；不同基线的同 tip 推送正常排队并被闸门拒绝。配回归。
  - **merge 副作用的事务边界（Codex R8 #5）**：CL 状态与 conversation 写入在 `apply_update_result` 之后（`:2625-2635`），失败会留下「根已推进、CL 可重试」的二次合并窗口。交付物 6 扩展：三者同库，一并穿入 B3 同一事务；异构存储副作用走以 `landed_commit_id` 为幂等键的补偿状态机并单独登记。配验收。
  - **事实校准 16 措辞（Codex R8 #6，MINOR）**：单 ref 的 `save_refs(..., txn)`/`update_ref(..., txn)` 已支持事务，缺的是批量与 `apply_update_result` 调用链——措辞更正。
  - **阶段 4 验收的净零限定（Codex R8 #7，MINOR）**：「两条路径推送后 `/` 恰好前进一个 commit」限定为有树变更的推送。
- **同日修订 12（第七轮 Codex 评审后）**：R7 评审 4 条 BLOCKING + 2 条 MINOR 全部落实。
  - **trunk 形态的删除策略（Codex R7 #1）**：现状删除分支挂在 `apply_cl_mega_ref_for_push_command` 内（`monorepo.rs:919-939`），trunk 直推不经过它，删除类推送将失去全部闸门。B0 补显式策略：trunk 形态拒绝一切删除类 ref 命令（main 沿用 UN-16 语义；其余 ref 提示走「父路径删除子目录的 commit」），配回归。
  - **merge 冲突重查与「不拒绝」陈述的矛盾（Codex R7 #2）**：`check_merge_conflicts`（`:4626-4658`）比对 `cl.from_hash` 与当前 `main@path`，tip 推进 + 基线陈旧 → `Conflict` → 队尾重排——与现状一致且非终态失败。1.9 第 3 条与推论改写为「无终态拒绝；冲突重排保留」，ADR-TP-10 同步。
  - **阶段分期的先决倒挂（Codex R7 #3）**：阶段 1 的 merge 断言依赖墓碑修复闭环，但墓碑表与续接集成原属阶段 2。新增 **1.10 交付物 8**（墓碑迁移 + 修复路径 + 物化续接集成；时为第 7 项，后因树项插入原语插入顺延），阶段 2.5 改为其上的语义补全；2.5 标注交付时点。
  - **残余窗口声明的夸大（Codex R7 #4）**：「下一次根 CAS 恒能兜住」不成立——B3 读到的是绕过者写入后的根，CAS 恒成功。ADR-TP-20 新增第 3 项：启用前对账 + 周期性不变式巡检（复用墓碑续接）+ 指标计数；I3/I5 显式附带「全部写入者遵守锁纪律」前提，绕过者归入 I5 的 fail-closed 事件域。配对账/巡检回归。
  - **token 授权的组件边界（Codex R7 #5，MINOR）**：前缀匹配改为规范化后的组件边界匹配（`/foo` 不授权 `/foobar`），配负向测试。
  - **签名条目的适用范围（Codex R7 #6，MINOR）**：3.3 与阶段 3 验收的「所有合成 commit 均签名」限定为 trunk 形态与新增合成 commit；review merge 落地 commit 保持现状签名形态。
- **同日修订 11（第六轮 Codex 评审后）**：R6 评审 3 条 BLOCKING + 1 条 MINOR 全部落实。
  - **B0–B3 之间的墓碑竞态（Codex R6 #1）**：B0 的墓碑预检非权威——B0 与 B3 之间可能发生一次 ADR-TP-20 修复写入墓碑。B3 的创建分支补**锁内墓碑重查**（存在即拒绝，走 B4），配交错探针回归。
  - **幂等查重的终态竞态（Codex R6 #2）**：仅靠活跃行部分索引存在「重试查重后原行提交 `Done`、重试 INSERT 不再冲突而重复入队」的窗口。B1 改为**条件 INSERT 三态原子判定**（`NOT EXISTS ... status IN ('Queued','Running','Done')`，与插入同一语句），索引改为 `push_queue_operation_states`（三态上的 `(kind, path, operation_id)` 唯一）作并发兜底；1.11 成文「查重与插入必须同一语句」。配终态竞态回归。
  - **merge 断言的对象与 parent 选取（Codex R6 #3）**：现状 `process_ref_updates` 的落地 parent 优先取 `refs/cl/<link>` 行的 tip（命名分歧时落 main head，GAP-07），并非 `main@P`——对 CL ref 行做树哈希断言会误拒合法 merge。澄清：断言对象是 merge **覆写前**的 `main@P` 行（CL ref 行不是根树派生视图，不参与断言），落地 parent 选取保持现状；ADR-TP-20、1.5、1.9、ADR-TP-10 四处同步更正。
  - **审计范围限定（Codex R6 #4，MINOR）**：硬约束 2 明确只覆盖 `refs/heads/main` 行的写入者；CL ref 行（`code_edit/on_edit.rs:47`）与 tag ref 行（`mono_api_service.rs:1927-1978`）的写入者登记为「范围外」并说明理由。
- **同日修订 10（第五轮 Codex 评审后）**：R5 评审 5 条 BLOCKING + 1 条 MINOR + 1 条 SUGGESTION 全部落实。
  - **根更新唯一性的验收措辞（Codex R5 #1）**：阶段 1 验收「任意 kind 的轮次前后 `/` 恰好前进一次」与 ADR-TP-16 矛盾，改为「恰好执行一次根 CAS 断言写；根 commit 仅在根树变化时前进」并配净零用例。
  - **活跃操作去重（Codex R5 #2）**：B1 只查 `Done` 行拦不住「原行在队时的并发重试」。新增 `push_queue_active_operation` 唯一部分索引（`(kind, path, operation_id) WHERE status IN ('Queued','Running')`，全 kind 适用）——活跃期重试被拒并提示「重复操作进行中」，终态后重试走 `Done` 回放；merge 冲突重排不受影响（原行已终态）。
  - **树哈希断言覆盖 merge 分支（Codex R5 #3）**：merge 以 `main@P` 行为 parent，消费陈旧行会把陈旧 tip 永久烘进历史。ADR-TP-20 的断言/修复扩展为 push 与 merge 同谓词；1.9 行为变化清单增补第 4 条（陈旧基线 merge 被拒属缺陷修复——现状的「成功」本身带陈旧前史）；配 merge 陈旧基线回归。
  - **数据面全量事务化（Codex R5 #4）**：现状 `apply_update_result` 的 ref/commit/tree 写入分散在多个自管连接（`mono_api_service.rs:2693-2696/2701/2717`）。新增 **1.10 交付物 6**：三类写入全部穿入同一 `&DatabaseTransaction`；配中段 `kill -9` 无孤儿对象验收。
  - **UN-25 的 SystemError（Codex R5 #5）**：现有 freeze 契约是 `Failed + SystemError`（`un25_freeze.rs`），新枚举补 `SystemError` 变体并在 1.8 声明逐条映射；配 freeze 回归。
  - **措辞（Codex R5 #6/#7）**：风险 6 与 I3 的「同事务删除」统一为「B4 回滚后独立提交事务墓碑修复」；ADR-TP-06 的「推送的权威序号」改为「全局队列操作的权威序号」（merge/attach 也消费 id）。
- **同日修订 9（第四轮 Codex 评审后）**：R4 评审 4 条 BLOCKING + 1 条 MINOR 全部落实。
  - **N > 1 重试的幂等回放（Codex R4 #1）**：N > 1 落地的是 squash commit，`cmd.new_id != main@P.tip` 恒成立，仅靠 B0 的 tip 相等早退拦不住重试。改为 push 的幂等解析以 **B1 查 `Done` 行**为权威路径（先于 non-fast-forward 预检），`landed_commit_id` 记录实际落地 id（N = 1 为 `cmd.new_id`、N > 1 为 squash id）并以此回放。
  - **创建语义的 N > 1 与 provenance（Codex R4 #2）**：写入模型补创建情形的 N 分流——N > 1 的 squash parent 取 `vec![]`（空历史，I1 空真）；创建情形省略 `Mono-Squash-Range`（`ZERO_ID..tip` 非法），provenance 由 message 枚举与 `operation_id`（链 tip，GC root）锚定；I4 相应改写。
  - **墓碑优先于创建（Codex R4 #3）**：`P` 无行但存在墓碑时，创建语义不得生效——B0 拒绝并提示先 advertise 从墓碑续接重新物化、对齐后再推，堵住「直推创建绕过墓碑」的 I1 破损路径；配 N = 1/N > 1 两条回归。
  - **净零轮次的断言在场（Codex R4 #4）**：「根更新的唯一性」改为**每轮恰好执行一次根 CAS 断言写**——净零轮次执行同值写（Postgres 计 1 行并持行锁），tripwire 不因净零缺席；ADR-TP-20 插入校验的残余窗口（绕过写入者恰在校验读与提交之间推进根）由下一轮根 CAS 兜住，成文。配绕过写入者注入回归。
  - **operation_id 的语义（Codex R4 #5，MINOR）**：明确 `cl.link` 标识**合并意图**而非版本——rebase 改 `to_hash` 不改身份，B3 按当时 `to_hash` 落地、重放按 `landed_commit_id`；B1 的 INSERT 示例补 `operation_id` 列。
- **同日修订 8（第三轮 Codex 评审后）**：R3 评审 5 条 BLOCKING + 1 条 MINOR 全部落实。
  - **陈旧行修复与 B4 回滚的矛盾（Codex R3 #1）**：树哈希断言的修复原写成「同一事务删行」，但该事务因拒绝而回滚，删行随之丢失；且单纯删行会让下次物化合成无父 commit，I1 断裂。改为 **B4 回滚后另以独立提交事务写墓碑（`last_commit_hash` = 陈旧 tip）并删行**——复用 2.5 的续接机制保住 I1。B3 伪代码与阶段 1 验收同步。
  - **阶段 1 的单事务承诺（Codex R3 #2）**：现状 `remove_none_cl_refs` 自建连接（`mono_storage.rs:77-84`），在 B3 事务外执行会使 merge 落地分裂提交窗口。新增 **1.10 交付物 5**：事务内变体（同时修复 LIKE 两处缺陷），阶段 1 的 merge 分支改用它，阶段 2 由 `advance_descendant_refs` 接替；配「merge 中段 kill -9 无部分提交」验收。
  - **attach 的根依赖准备时机（Codex R3 #3）**：现状 attach 在事务前预计算根快照与根 commit（`import_repo.rs:518-550`），排队等待会让预计算值作废、CAS 对合法前序轮次误报 stale。改为**根依赖准备全部在 B3 锁内重做**；队列行携带 attach 请求 id 作稳定标识，`old_id` 降级为诊断信息；1.4/1.9/B3 伪代码同步。
  - **幂等键与判定次序（Codex R3 #4）**：幂等键由 `(kind, path, new_id)` 改为**稳定 `operation_id` 列**（push=`cmd.new_id`、merge=`cl.link`——不同 CL 可共享 `to_hash`、attach=请求 id）；新增 `landed_commit_id` 列供结果回放；B0 补「push 重试幂等早退（`new_id == tip` 先于 non-fast-forward 预检）」。1.11 重写。
  - **认领的原子重验（Codex R3 #5）**：B2.5 由「仅 `WHERE id AND status='Queued'`」升级为**单语句三条件原子认领**（追加 `NOT EXISTS Running` 与 `id = min(Queued)`），封住「B2 观察与认领之间被插队」产生双 `Running` 的窗口；wait_timeout 的取消改为条件更新。配交错探针验收。
  - **根写入的措辞（Codex R3 #6，MINOR）**：硬约束 1 补净零推送例外说明；「根更新的唯一性」由「每轮恰好一次」改为「**至多一次** CAS 断言写，根 commit 是否前进由树是否变化决定（ADR-TP-16 同一谓词）」。
- **同日修订 7（第二轮 Codex 评审后）**：R2 评审 5 条 BLOCKING + 1 条 MINOR 全部落实。
  - **根更新的唯一性（Codex R2 #1）**：B3 伪代码中放在分支之后的通用根 CAS 会被 merge/attach 分支各自的根写入（`apply_update_result`、`attach_to_monorepo_parent_in_txn`）顶成 0 行命中而误报 `QueueBypassDetected`。改为**每轮恰好一次根写入、一律带 CAS**：push 分支显式执行；attach 沿用既有 CAS；merge 的根更新从无条件 `UPDATE` 改走事务内 CAS 原语（并入阶段 1 交付物）。
  - **物化竞态的恢复路径（Codex R2 #2/#3）**：ADR-TP-20 补齐两侧——插入时**校验根快照新鲜度**（持锁事务内重读根 ref，与遍历出发 hash 不一致即放弃插入，杜绝陈旧行落库）；树哈希断言失败时修复陈旧行（R3 进一步改为墓碑续接，见修订 8）。据此恢复 **I3 强一致、不分层**的原表述（ADR-TP-19 与附录 I3 的「可判定形式」缓冲措辞删除），物化提交的行恒与提交时点根树一致。
  - **merge 分支的后代切换（Codex R2 #4）**：阶段 2 新增 **2.8**——`merge_cl_unchecked` 的 `remove_none_cl_refs` 调用在阶段 2 落地时替换为 `advance_descendant_refs`（review 形态下登记在案的行为变化，硬约束 8 补「缺陷修复除外」限定词）；阶段 2 验收补 merge 触发路径的断裂回归。
  - **幂等与重试解析（Codex R2 #5）**：新增 **1.11**——入队前按稳定操作标识查 `Done` 行短路返回并回放结果；merge/attach 在 B3 执行期门控处兜底幂等命中；push 的重试早退见 1.11（R3 细化为 `operation_id` 列与 B0 判定次序）。阶段 1 验收补幂等重试回归。
  - **cancel 的状态规则（Codex R2 #6，MINOR）**：1.7 明确 `cancel` 仅作用于 `Queued` 行（条件更新，与认领先到先得）；`Running` 行不可取消，避免与 `Done` 写入竞态留下「数据已提交而行状态 Cancelled」的矛盾。阶段 1 验收补 cancel 竞态用例与根更新唯一性断言。
- **同日修订 6（双评审后收口）**：外部评审（Codex 与 Claude 各一轮，8 + 3 条 BLOCKING）指出的问题全部落实，共 18 项修订。
  - **形态与队列的作用域（严重，Codex #1/#2、Claude #2）**：明确 review 形态下**推送不入队**——`finalize_receive_pack` 在 review 形态只写 `refs/cl/*` 行，不触碰根树与 `main` 行（事实校准 1 重写）；`push_queue` 新增 **`kind` 判别列**（`push|merge|attach`），B3 按 kind 分派：push 分支（trunk 形态）做树嫁接，merge 分支走既有落地形态并原样重查门控，attach 分支保留 CAS 条件。**merge 行没有 non-fast-forward 闸门**（其落地 parent 取执行时点的 `main@P`，无陈旧基线问题），1.9 重写为如实的三类行为变化清单（背压、全序重排、门控重查），不再声称「给 merge 补闸门」。ADR-TP-10 的同路径唯一索引收缩为 `kind='push'`。阶段 1 验收的并发用例改以 merge/attach 为执行体（push 版移至阶段 4），并区分互嵌路径。
  - **惰性物化是遗漏的写入者（严重，Codex #6、Claude #1）**：新增事实校准 15 与 **ADR-TP-20**——两条物化路径（`refs_with_head_hash`、`create_repo_commit`）不入队，由「物化插入持 `MONO_WRITE_LOCK` 短锁 + `WHERE NOT EXISTS`」与「B3 push 闸门追加 `ref_tree_hash == resolve(root, P)` 树哈希断言」双层覆盖；物化竞态复现 ADR-TP-19 失效链的论证成文，硬约束 2 的写入者清单按三类重写，新增风险 6 与两条回归用例。
  - **reaper 竞案重设计（严重，Codex #5、Claude #5/#6）**：reaper 改为在自己事务内 `pg_try_advisory_xact_lock`，持锁即证明无活跃 B3，`Running` 行**当场清理**——删除了原 `pg_locks` 只读预查（TOCTOU）与 `stuck_timeout` 参与正确性判定（改降级为告警阈值）；B3 增加首句 **fencing**（持锁后重读本行非 `Running` 即弃权，`ClaimLost`），封住「认领与取锁之间被 reaper 越过」的窗口；`status=Done` 命中 0 行升级为 fail-closed。
  - **队列模型补全（Codex #3/#4/#10）**：明确**队列是序列化闸门与生命周期台账而非作业执行器**（caller-owned executor，不携操作专属载荷，重启不跨进程恢复）；实体与枚举全新建（现有 `QueueStatusEnum` 是 CL 工作流状态机，不同构，1.8 的「可复用枚举」说法更正）；新增 `queue_control` 单行表持久 pause/max_depth，B1 改为单事务原子准入（`FOR UPDATE` + 条件 INSERT + 唯一索引兜底）。
  - **未物化路径的推送（Claude #3）**：写入模型补创建语义（`old_id = ZERO_ID` 时 INSERT `main@P`）与拒绝规则（`old_id != ZERO_ID` 时 B0 拒绝）；`batch_update_by_path_concurrent` 对缺失行静默跳过的缺陷成文（事实校准 4/16），修复列入阶段 1 交付物。
  - **阶段 3 归属规则的适用范围（Codex #2）**：明确只作用于 trunk 形态与新增的续接 commit；review 形态的 CL merge 落地保持硬编码现状（硬约束 8）；3.4 与 ADR-TP-17 的依据更正为 ADR-MC-07（GPG 只是纵深防御，`merge_checker/mod.rs:20-27`），`max_push_commits` 定为 trunk-only 配置，review 形态保留常量。
  - **认证事实与交付物（Codex #7/#14）**：新增事实校准 17（启动强依赖 OAuth、token 走 UserStorage、push 权限检查在 cedar off 时仍要求 username、SSH 要求认证用户）；阶段 5 改写为五项代码交付物（条件化启动、静态 token 认证器、权限判定改造、SSH 不暴露 receive-pack、`push_auth="none"` 运行前提），**认证身份与 provenance 分离**——`requester` 取 token 名，commit 署名只是自声明元数据。
  - **其余**：事实校准 1 补删除类例外、4 补 attach CAS 与重试循环删除要求、6 补冲突重排队与门控重查、13 更正 GPG 位置（`:2124`/`:4599`，不在 `merge_cl_unchecked` 体内）；冲突重排改为「关行重入队」（`bigserial` id 不可改写，ADR-TP-05）；ADR-TP-09/1.5/I5 措辞改为「CAS 是 tripwire 非完备性证明」；ADR-TP-11 索引保护细化为行级 `indexed_push_id` 水位（嵌套覆盖，Codex #11、Claude #7）；B3 依赖的事务内 ref 更新原语与 `escape_like` 助手列入阶段 1 交付物（Claude #11/#12）；bootstrap 写入者登记（Codex #13）；枚举顺序断言进阶段 3 验收（Claude #13）；头部反向声明的「不变式 I1–I5」更正为 I1–I6（含 I2a）（Claude #10）；阶段 1 验收的「20 个不同路径」改为互不嵌套并补嵌套用例（Claude #9）。
- **同日修订 5（I3 一致性模型收口）**：关闭上一轮遗留的唯一待决项——阶段 6.4 惰性后代推进与阶段 2.7 同事务硬要求的互斥。
  - 新增 **ADR-TP-19**：后代 ref 恒与 B3 同事务推进，**I3 保持强一致、不分层**，惰性后代推进**否决**。核心论证是此前未被识别的一条失效链：I3 不只是展示视图的一致性，`main@P` 是 B3 的 non-fast-forward 闸门的比对对象；后代 ref 一旦允许滞后，一个未 fetch 的客户端的推送会通过闸门，把基于陈旧子树算出的结果嫁接进根树，**静默覆盖掉祖先推送的改动**。要补救就必须在闸门内先补齐，而闸门在临界区内持全局写锁——收益被自身要求抵消。第二重理由是覆盖面：git 广告路径之外还有至少十处直接读取路径级 `main@P`，漏一处即错。
  - 同时说明 ADR-TP-11（file_path 索引最终一致）**不构成先例**：那份数据不在写入判定链上，也没有客户端在其上叠加提交。
  - 阶段 6 第 4 项由「后代 ref 惰性推进」替换为 **「物化 TTL + 墓碑回收」**：它命中同一个成本来源（物化只增不减，一次 `ls-remote` 即永久物化），却复用阶段 2 已有的墓碑机制，只需一个时间戳列加一个清理任务，**不触碰任何不变式**；把成本从「推迟」变成「消除」。
  - 阶段 2.7 补「这是硬要求，不接受最终一致的替代方案」并指向 ADR-TP-19；附录 A 的 I3 改为可判定的精确形式并标注强一致理由。
- **同日修订 4（第二轮评审后；其 reaper 与 B0 措辞已被修订 27/32/46/47 取代，仅存史）**：补四处修订后仍存在的实质缺口，另修四处措辞。
  - **CL merge 的 `new_id` 语义**：1.9 补一整段——`cl.to_hash` 只是落地 commit 的 `parent`（GAP-07 命名分歧下会落到 main head），落地的是新合成的 commit。据此限定 B3 的 P 分支只覆盖推送、merge 是既有落地形态的第三种，并把 I4 的区间语义标注为「只对推送行成立」；GC root 对 merge 行仍以 `new_id` 为准（落地 commit 由 `main@P` 可达、无需显式 root，会失去引用的是 CL 链）。
  - **CL merge 的 non-fast-forward 闸门与硬约束 8**：补闭合说明——该闸门只在「入队后 main tip 被并发推进」的竞态下拒绝，而该竞态在现状下是事实校准 4 的静默丢更新；闸门把静默损坏改为可诊断拒绝，没有原本能成功且正确的 merge 会因此失败。
  - **reaper 误伤慢轮次**：1.6 补两条防线——reaper 先经 `pg_locks` 只读确认 `MONO_WRITE_LOCK` 无人持有（不用 `pg_try_advisory_lock`，那会抢锁并阻塞等待中的 B3），以及 `stuck_timeout` 必须大于 B3 最坏时长并按轮次时长 P99 标定；B3/B4 的状态写入一律加 `WHERE status='Running'`，使抢占冲突可观测而非被静默覆盖。
  - **CAS 失败分支的终态**：明确为「标 `Failed`（`failure_type=QueueBypassDetected`）+ 自动 `pause` 队列 + 告警」，并把它写成 **ADR-TP-07 的显式例外**——常规轮次失败不影响后续正确性，而队列被绕过意味着序列化保证已失效，继续处理只会在他人正在变动的根上叠加写入。
  - 措辞：小结与 ADR-TP-12 决策补「受影响的层」限定；I6 的「单射」改为「允许空洞、不要求一一对应」；「单调一致」统一为「保序」；主干删除 guard 的行号引用统一为 `monorepo.rs:920-931`。
- **同日修订 3（评审后；其 N=0 早退措辞已被修订 47/56 取代，仅存史）**：外部评审提出 10 条，7 条完全成立、2 条部分成立、1 条不成立，据此修订如下。
  - **队列崩溃语义（严重）**：`status=Running` 的写入从 B3 业务事务中拆出为**独立提交的认领步骤 B2.5**——原设计下崩溃会连同它一起回滚，队列行退回 `Queued` 且 `started_at` 为 NULL，既非可识别残留也无法被 reaper 捞到，而它永远是最小 `Queued` id，会使此后所有推送等不到轮次、队列硬死锁。新增 `heartbeat_at` 列与 B2 的心跳刷新，作为孤儿 `Queued` 行的唯一出口；1.6 崩溃表按三个崩溃窗口重写。该修订同时落实了 ADR-TP-05 对「原子 claim 写回」的要求。
  - **「每层恰好一个」与净零推送的冲突（严重）**：核心性质的措辞由「每一层」收敛为「每个**受影响的**层」，并在写入模型一节给出「受影响」的判定。3.3 表格的祖先行改为「+1 或 0」，I2a、ADR-TP-12 影响、阶段 3 验收同步；原本互相矛盾的两条验收标准改为互为补集。
  - **不变式 I3 的双重含义**：拆出 **I6 时间线全序**（至多一个操作处于 B 段、trunk roll-up 与 `push_queue.id` 保序），ADR-TP-06 / ADR-TP-14 的引用由 I3 改为 I6。同时更正 ADR-TP-06 的措辞：`push_queue.id` 是**推送**序号，trunk commit 按它**保序但非双射**（净零推送占 id 不产生 roll-up，序列允许空洞）。
  - **签名措辞的修订残留**：预期收益中「逐 commit 验签成立」删除；阶段 3 验收的「签名可验证」改为断言对象**逐字节相等**，避免把 ADR-TP-15a 明令的非保证测成保证。
  - **祖先 roll-up 的 message 规则**：明确**完整枚举每次推送只产生一份**，落在被推路径的 squash commit 上；祖先与后代带 `Mono-Squash-Commit` 指针，不重复枚举。原先仅以示例隐含、未成文，且未计入「枚举 × 层数」的体积风险（层数随只读操作单调增长）。3.2 示例的 `Mono-Ref-Path` 相应更正，ADR-TP-15、3.4 与 I4 同步。
  - **N = 0 未定义**：B0 补入 ADR-MC-05 幂等 no-op 的显式早退（空 pack 且 tip 已知则不入队、不动 ref），避免 `N = 0` 落入 `N > 1` 分支被合成 squash commit。
  - **非推送写入者的映射空白**：新增 1.9 节，给出 push / CL merge / ImportRepo attach 三类写入者在 `path`/`old_id`/`new_id`/等待载体/`failure_type`/心跳上的逐项取值，并说明 HTTP 等待的超时响应与 UN-25 freeze 的边界。
  - **阶段 6.4 与阶段 2.7 互斥**：惰性后代推进补入第二个前提——必须先修订 I3，把后代 ref 降级为最终一致的物化视图；未完成该修订前本项不得启动。
  - **UN-16 在 trunk 形态下的含义**：阶段 4 补充说明。UN-16 是一张卡同时覆盖授权快照重建与拒绝删除主干 ref（评审怀疑的编号冲突不成立，代码可证）；但 `cedar.enforcement = off` 时保留它只余两个作用，且**不构成任何授权保护**。
- **同日修订 2**：**逐 commit GPG 签名验证明确不支持**（新增 ADR-TP-15a），合成 commit 一律由服务端密钥签名；作为交换，**squash commit 的 message 必须完整列出全部被合并 commit，不截断**（ADR-TP-15 重写：移除 K 条封顶与 `Mono-Commits` trailer，新增说明段落与逐条 id/author/date/subject 枚举）。由此带来的 message 放大**记为已接受的代价**——mega2 面向超大仓库，体积不参与 `max_push_commits` 的取值决策。连带修订：3.2 message 格式、3.3 签名条目、3.4 与 ADR-TP-17 的链长上限语义、约束 2 改写、不变式 I4 改为 provenance 完整、阶段 3 验收标准。
- **涵盖范围**：`src/ceres/pack/monorepo.rs`、`src/ceres/pack/push_chain.rs`、`src/ceres/api_service/mono_api_service.rs`、`src/ceres/api_service/tree_ops.rs`、`src/ceres/protocol/smart.rs`、`src/ceres/code_edit/utils.rs`、`src/jupiter/storage/mono_storage.rs`、`src/jupiter/storage/merge_queue_storage.rs`、`src/jupiter/service/merge_queue_service.rs`、`src/jupiter/service/mono_service.rs`、`src/jupiter/redis/`、`src/config/model.rs`、`src/callisto/sea_orm_active_enums.rs`、`src/server/http_server.rs`、`src/contract/git_protocol/`、`src/ceres/merge_checker/mod.rs`
