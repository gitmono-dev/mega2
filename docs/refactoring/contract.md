# Contract 模块归并实现方案分析

本文档记录 `monoengine` 中 `contract` 模块的定位、当前归并结果、路径迁移边界，以及后续涉及 API 数据契约、Git 协议、Vault 与权限策略代码时应遵守的共同约束。

> **治理规范**：本文档遵循 **`general.md`** 中定义的统一结构、共同约束和执行标准。

> **集成测试指引**：本计划通过 `integration.md` 中的 API、Git protocol、Vault bootstrap、Policy guard 相关测试路径验证，不新增运行时行为。

## 事实校准（2026-06-16）

1. **`contract` 已成为顶层模块**。入口为 `src/contract/mod.rs`，下挂 `api`、`git_protocol`、`vault`、`policy`。
2. **当前 `api_model` 已迁入 `contract::api`**。HTTP API DTO、分页类型、Artifact/Buck/Chat/Git commit wire types 均在 `src/contract/api/`。
3. **Git/Vault/Policy 实现已迁入 contract**。原 Git HTTP/SSH 协议、VaultCore/PKI/PGP/Nostr、Cedar policy/entitystore/guard 代码分别位于 `src/contract/git_protocol/`、`src/contract/vault/`、`src/contract/policy/`。
4. **旧模块入口不保留 re-export**。`crate::api_model`、`crate::git_protocol`、`crate::saturn`、`crate::api::guard` 不再作为有效代码路径。
5. **`crate::vault` 是有意保留的 vendored RustyVault 实现模块**，不在本次归并范围内。`src/lib.rs` 仍声明顶层 `mod vault;`（vendored RustyVault，见 `AGENTS.md` 的 vault pitfalls）；`contract::vault` 只是包裹它的集成/消费层（`VaultCore`/PKI/PGP/Nostr 等），两者并存。因此 `crate::vault::*` 仍是有效代码路径，但仅指 vendored 实现，不指产品集成层。
6. **数据库实体不迁移**。`callisto::vault` 是 SeaORM 实体模块，不属于 contract 归并范围。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
| --- | --- | --- |
| `contract::api` | 已激活 | 承载 API 请求/响应、分页、Artifact/Buck/Chat/Git DTO；只改变模块路径，不改变 JSON schema。 |
| `contract::git_protocol` | 已激活 | 承载 Git smart HTTP/SSH 入口；协议兼容性问题仍按 `protocol.md` 后续阶段处理。 |
| `contract::vault` | 已激活 | 承载 VaultCore 与消费端 helper；安全加固阶段仍按 `vault.md` 推进。 |
| `contract::policy` | 已激活 | 承载 Cedar context/entitystore/reviewer parser/admin resolver/API guard。 |
| 旧路径兼容层 | 未实现 | 本次迁移明确不提供 re-export，调用方必须使用新路径。 |
| 阶段 1 文档同步 | 已完成（2026-06-23） | `docs/refactoring` 内旧源码路径仅出现在历史映射/说明段落；`protocol.md` 与 `vault.md` 路径已同步。 |
| 阶段 2 常规门禁 | 已完成（2026-06-23） | `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all` 全部通过。 |

## 硬约束与不可违反的原则

1. **不改变 wire behavior**：HTTP 路径、JSON 字段、OpenAPI schema、Git smart protocol 字节流、Vault secret 数据格式和权限判定语义都不能因路径迁移改变。
2. **不混淆实体与 contract**：`callisto::*` 仍是数据库实体层，尤其 `callisto::vault` 必须保持原路径。
3. **旧路径不得回流**：新代码不得重新引入 `api_model`、顶层 `git_protocol`、顶层 `saturn` 或 `api::guard`；产品级 Vault 集成代码只能走 `contract::vault`。此约束**不**针对有意保留的 vendored RustyVault 模块——顶层 `mod vault;`（`crate::vault`）是该 vendored 实现的合法落点（见事实校准第 5 条），不属于回流。
4. **contract 可以包含实现，但边界要清楚**：本项目中的 `contract` 表示外部协议、安全边界和权限策略聚合，不等同于纯 DTO。

## 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
| --- | --- | --- | --- |
| API DTO 命名 | `api_model` 含义偏模糊 | `contract::api` 表达 API 数据契约 | 简单 |
| Git 协议入口 | 顶层 `git_protocol` | `contract::git_protocol` | 中等 |
| Vault 能力 | 顶层 `vault` | `contract::vault` | 中等 |
| 权限策略 | `saturn` + `api::guard` 分散 | `contract::policy` 聚合 | 中等 |
| 文档路径 | 多处旧路径 | `docs/refactoring` 与代码同步 | 简单 |

## 迁移步骤（分阶段）

**阶段 0 — 结构归并**

1. 建立 `src/contract/` 与四个子模块。
2. 移动 API、Git、Vault、Policy 相关代码。
3. 删除旧顶层模块声明与 `api::guard` 出口。

> **验收标准**：
> - `cargo check` 通过。
> - `rg "api_model|crate::git_protocol|crate::saturn|api::guard" src` 不命中有效代码引用。
>   （注意：`crate::vault` **不**包含在此 grep 内——它是有意保留的 vendored RustyVault 实现模块，见事实校准第 5 条；产品集成层一律走 `contract::vault`。）

**阶段 1 — 文档同步**

1. 更新 `README.md`、`general.md`、`integration.md`。
2. 更新 `protocol.md` 与 `vault.md` 中的路径。
3. 记录旧路径到新路径的映射。

> **验收标准**：
> - `docs/refactoring` 中旧源码路径仅允许出现在明确的历史路径映射段落。

**阶段 2 — 常规门禁（已完成，2026-06-23）**

1. 运行格式、clippy、测试三道门禁。
2. 若发现路径迁移引出的 warning，优先修正代码，不添加 blanket allow。

> **验收标准**：
> - `cargo +nightly fmt --all --check` ✅ 通过
> - `cargo clippy --all-targets --all-features -- -D warnings` ✅ 通过
> - `source .env.test && cargo test --all` ✅ 通过（集成测试 10 + 单元测试 584）

## 前置依赖矩阵

| Contract 工作 | 对其他文档的依赖 | 类型 | 关键同步点 |
| --- | --- | --- | --- |
| API DTO 迁移 | notification.md、website-auth.md | 协同 | DTO 路径应统一使用 `contract::api`。 |
| Git protocol 迁移 | protocol.md | 协同 | 只更新路径，不改变协议改进阶段。 |
| Vault 迁移 | vault.md、config.md、website-mail.md | 协同 | 只更新路径，不改变 SecretRef 或 bootstrap 设计。 |
| Policy 迁移 | protocol.md、integration.md | 协同 | API guard 与 Cedar context 路径统一到 `contract::policy`。 |

## 风险与约束

- **大范围路径替换风险**：可能误伤 `callisto::vault` 实体路径。缓解措施是用 `rg "callisto::contract"` 和编译门禁验证。
- **文档过期风险**：长期计划文档若保留旧路径，会误导后续改造。缓解措施是每次路径迁移同步更新 `docs/refactoring`。
- **语义误解风险**：`contract` 不是纯类型模块。后续新增内容必须判断是否属于外部协议、安全边界或权限策略。

## 改进方案多维评估小结

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | **高（8/10）**。统一外部协议和安全边界命名，消除 `api_model` 歧义。 |
| **可行性** | **高（8/10）**。主要是模块移动与引用更新，运行时行为不变。 |
| **兼容性** | **中（6/10）**。内部 Rust 路径破坏性变化，但无外部 API 行为变化。 |
| **可维护性** | **中高（7.5/10）**。路径更集中，但 `contract` 含实现代码，后续需严格守住边界。 |

## 小结

`contract` 是一次结构性归并：让 API 数据契约、Git 协议、Vault、安全策略和权限 guard 处在统一的外部边界命名空间下。本次迁移不解决 Git/Vault/Policy 各自的后续质量问题；这些问题仍按 `protocol.md`、`vault.md` 和相关文档继续推进。

## 预期收益

- **外部边界命名规范化**：API 数据契约、Git 协议、Vault、权限策略统一于 `contract::*` 命名空间，减少散落路径导致的认知混淆（可通过 `grep -rn 'api_model\|::saturn' src/` 无活跃引用残留来验证）。
- **文档与代码路径一致**：docs/refactoring 中的源码路径引用已对齐当前 `contract::*` 实现（阶段 1 完成，2026-06-23），降低文档过期误导后续开发者的概率。
- **下游消费边界更清晰**：调用方区分 `contract::*`（外部协议边界）与 `callisto::*`（数据库实体层），降低路径选择错误或误触数据库实体直译的风险。
- **后续改造基础就位**：vault.md、protocol.md、mail.md、notification.md 的后续质量改善可基于统一的路径结构展开，把精力聚焦在各自核心目标而非路径调整上。

## 授权三态判定 helper（ADR-UN-01）

`src/contract/policy/enforcement.rs` 提供授权三态判定的单源 helper（GC-UN-03）：

- `Enforcement::{Off, Shadow, Enforce}`：`off` 不构建不消费授权数据；`shadow` 构建并评估、记录 would-deny 但不改变放行；`enforce` 构建并评估、真实拒绝。
- `decide(enforcement, would_deny, store_empty)`：`enforce` + 空 store 返回 deny（fail-closed）。
- 配置侧 `[cedar].enforcement`（`src/config/model.rs`）由 `config validate` 校验取值，并拒绝 `monorepo.admin` 含保留匿名主体 `User::"__anonymous__"`（ADR-UN-06 ⑤）。

## 单 monorepo 资源归一 helper（ADR-UN-05）

`src/contract/policy/resource.rs` 提供单源资源归一 helper（GC-UN-03）：

- `ROOT_REPOSITORY = Repository::"/"`：单 monorepo 根仓库实体标识，所有请求路径归一到此仓库，其 ACL 治理全部路径（ADR-UN-05）。
- `normalize_resource(path)`：纯 O(1) 映射，任意合法请求路径（如 `/project`）→ 根仓库实体标识。
- `root_repository_present(store)` / `resolve_resource(path, store)`：检查 store 是否含根仓库实体；缺失时返回 `ResourceResolution::Missing`（fail-closed）。
- `decide_resource(enforcement, resolution)`：根实体缺失视为 would-deny——`enforce` 拒绝、`shadow` 放行但记录、`off` 放行。

默认初始化产物（`src/jupiter/utils/converter.rs` 经 `generate_entity(admin, "/")`）只生成 `Repository::"/"` 一个仓库实体，且只为 `admins` 参数创建用户（不生成 maintainer/reader 成员）。

## 共享授权实例创建/注入与 HTTP push 门三态（UN-02）

`AppContext` 创建共享 `SharedEntityStore`（`Arc`，全服务唯一所有者，ADR-UN-02）并注入 `Storage`（写路径 notify 下发）与 HTTP state（读路径 guard/push）。`Storage::entity_store()` 返回同一 `Arc`；HTTP state 的授权句柄唯一来源 = `AppContext.entity_store`（`http_server.rs` 不再独立构造 `EntityStore::new()`）。首建幂等 `ensure` 在 HTTP listener 绑定前完成（`start_http` 内 `ensure_authz_first_build`）；`off` 下不构建，`shadow`/`enforce` 下首建失败使 server 启动失败。

`check_push_permission` 切换到三态 helper（ADR-UN-01）：`off` 短路放行；`shadow` 放行但记录 would-deny（结构化字段 `event=authz_would_deny`）；`enforce` 拒绝无权限 push。资源经 UN-11 归一为根仓库 `Repository::"/"`，根实体缺失 fail-closed。

## 授权快照传播闭合与主干删除防护（UN-16）

`src/contract/policy/notify.rs` 是触发共享快照重建的**唯一入口**（GC-UN-03）：

- `notify_authz_changed(storage, old_blob_id, new_blob_id)`：以主干 `/.mega_cedar.json` 的 blob ID 变化为条件（O(1) 比较），无变化直接返回；变化时经 `Storage::entity_store()` 拿到的共享实例执行 build-then-swap（UN-15 语义：失败保留旧快照 + `error` 日志 + 置 dirty）。
- `notify_authz_changed_best_effort(...)`：ref 已写、后续步骤失败的路径不得回滚业务操作，因此只记录并置 dirty（ADR-UN-01：`enforce` + dirty = 受护判定全拒）。

主干 ref 的真实出入口全部挂接（写点 allowlist 由 `scripts/authz_write_points_guard.sh` 强制，新增调用方必须挂接或登记为例外）：

| 出入口 | 挂接点 | 备注 |
| --- | --- | --- |
| merge 漏斗 | `mono_api_service.rs::apply_update_result` 成功后 | 覆盖 merge / merge-no-auth / merge queue 三条路径 |
| import 根挂接 | `import_repo.rs::attach_to_monorepo_parent` 的 `txn.commit()` 之后 | CAS 推进根主干 |
| receive-pack Delete | `monorepo.rs::apply_cl_mega_ref_for_push_command` 的 Delete 分支 | 同点**拒绝删除主干 ref**（`MEGA_BRANCH_NAME`），错误对 git 客户端可操作 |

主干删除拒绝是有意的安全收口（非 enforcement 门控，GC-UN-01 例外公示），在 `off` 默认下也生效。

## 快照重建全维即时生效矩阵（UN-21）

`/.mega_cedar.json` 的授权输入维度由 `src/contract/policy/objects.rs` 与 `mega.cedarschema` 的真实 schema 决定，共八维；`merge_requests` / `issues` 播种为空且不参与求值，不是输入维度。八维与其判定翻转的对应关系逐维锁定在 `src/contract/policy/un21_matrix.rs`（每维一个具名测试函数）：

| 维 | 数据源字段 | 翻转的判定 |
|---|---|---|
| 一 | `users[].parents` 加入 `admin` | admin 专属 action（`addAdmin`） |
| 二 | `users[].parents` 移出 `matainer` | maintainer 专属 action（`approveMergeRequest`） |
| 三 | `users[].parents` 移出 `reader` | reader action（`pullRepo`） |
| 四 | `user_groups[].parents` 断开继承链 | 继承链上的 reader action |
| 五 | `repos[].is_private` 翻转 | 无角色主体的可见性（`viewRepo`） |
| 六 | `repos[].admins` 组引用改指 | admin 专属 action |
| 七 | `repos[].maintainers` 组引用改指 | maintainer 专属 action |
| 八 | `repos[].readers` 组引用改指 | reader action |

矩阵的每条断言都针对快照**缓存的** `Entities`（UN-14）求值，而不是从 store 现推，因此缓存残留会让矩阵失败而不是被重新推导掩盖。`src/contract/policy/un21_cache.rs` 另行锁定缓存语义本身：重建发布新的不可变快照而非原地修改（重建前取得的句柄继续服务其自身内容）、反复重建在两个方向都无残留、重建失败保留旧快照并置 dirty。

组层级为 admin → matainer → reader（`matainer` 为历史拼写现状，DEFER-UN-06）。

## Bot 主体授权语义（UN-27）

`mega.cedarschema` 里**每个 action 的 principal 都只有 `User`**，而 guard 把 bot 构造成 `Bot::"<id>"`——这不是 schema 认识的主体类型。因此（ADR-UN-06 ⑥）：

- `enforce`：bot 主体在受保护端点求值为 **deny**（fail-closed，不是「求值出错就放行」）。
- `shadow`：记录 would-deny 但仍放行——这个窗口正是 bot 所有者迁移到用户 token 的时间。
- `off`：短路，与任何主体一样零变化。

**过渡语义（公示）**：在切到 `enforce` 之前，使用受保护 CL 面的 bot 必须迁移到用户 token。完整的 bot 授权模型（schema 扩展 + 身份映射）归 DEFER-UN-01 后续设计。

主体**类型**是身份的一部分：即便 ACL 里存在同名用户 `42`，`Bot::"42"` 也不会继承其权限（`un27_a_bot_does_not_inherit_a_same_named_users_permissions`）。

解析顺序不变（UN-22 保证先 `BotIdentity` 后 session），并且 **bot 请求不产生任何 session store 调用**——用例用计数 double 断言为 0，而不是依赖代码顺序；非 bot 的 bearer 不会被误认成 bot，仍走一次会话解析。

## ACL 可信基线审计核心（UN-26）

打开 enforcement 等于信任 ACL 当前的内容。如果它在无人执行期间被改过，切到 `enforce` 只会把这次篡改固化——因此进入 shadow 前必须把 ACL 与**经具名审批的基线**比对一次。

可信源是**审批过的 artifact**（规范化快照 + 角色投影 + digest），而不是一个裸 digest：能替换快照的人同样能重算 digest。digest 只是 artifact 的完整性索引；使 artifact 可信的是**审批**——所以 `compare` 还要校验审批台账里独立记录的 digest（`--expect-digest`）。

两个阶段刻意不对称（`src/contract/policy/authz_audit.rs`）：

- `bootstrap-candidate` 产出候选 artifact，**永远不报告通过**——`diff_verdict` 恒为 `not_compared`。此时没有可比对的对象；首次运行若报「审计通过」，就会把一份已被篡改的 ACL 直接封为基线，正好毁掉这个机制的意义。
- `compare` 先校验 artifact 自洽（内置 digest 描述其自身快照）与**被审批过**（内置 digest == 台账 digest），再比对当前快照并列出差异。

闭包按**传递组关系 + `repos[].admins` 引用展开**计算，而不是读直接成员：新建一个以 `admin` 为父的组并把人放进去，直接成员视角下毫无变化——`closure_only_admins` 正是为这种情形而存在。仓库角色引用被静默改指（没有任何用户变动）同样是权限变更，故 `repo_role_refs` 逐仓库比对。

**PII 边界**：sanitized 报告只有 `{digest, closure_count, diff_verdict}` 三字段（`source_summary` 由 UN-29 装配层嵌入 UN-34 交付的对象），不含任何用户名；完整闭包与角色级差异清单属受限通道（UN-29 双输出 + UN-32 writer）。规范化对**键**与 `parents` **成员**排序：`parents` 是一个组集合，其书写顺序与键序一样只是排版；同一份 ACL 被重新格式化后必须 digest 相同，否则每次 reformat 都会报成篡改，审计很快就没人看了。

## ACL 文件变更仅 admin 可合（UN-19）

编辑 `/.mega_cedar.json` **就是**授予权限的方式，因此谁能合并这种变更，谁就能给自己授予任何权限。maintainer 持有 `approveMergeRequest`——若不加限制，这正是一条自提权通道。

检测挂在 merge 漏斗的唯一汇聚点 `merge_cl_unchecked`，因此 merge / merge-no-auth / merge queue 后台执行**三入口同判定**。涉及该文件时，`authz_principal` 必须经 `addAdmin` 判定（仅 admin 组持有）。

**「判不出来」不等于「通过」。** 三类情况都意味着这次变更**没有被审查过**，`enforce` 一律拒绝、`shadow` 记录后放行：

1. changed-file 列表读不出来（含 CL 的 base/tip commit 缺失——缺失时列表会读回空，与「本 CL 没改动」无法区分，属 fail-open，故先校验两端可解析）；
2. 主干上 `/.mega_cedar.json` 的 blob 解析失败；
3. 该文件在主干上缺失（没有可比对的基线）。

三类失败产生 `event=merge_authz_unavailable`（字段 `cl_link` / `principal` / `reason`），对外映射 **503** 而非 500 或 403：变更没有被拒绝，只是没被审查，授权恢复后同一请求可以成功（UN-25 契约）。queue 路径则据此冻结（保留 requester 与重试指引）。

同时移除了复用函数 `get_sorted_changed_file_list` 里的生产 `unwrap()`——它现在跑在合并路径上，存储故障必须以 `Result` 传播而不是 panic 掉请求。

## admin 事实源收敛（UN-04）

策略文件里原有一条「root admin 可做任何事」的规则，硬编码两个个人用户名。它使策略文件成为**第二个、不可见的 admin 权力来源**：运维读 ACL 看不出谁真正有特权，把某人从 admin 组移除也不会收回其权限。该规则已删除。

**enforce 下 admin 能力唯一来自实体存储的 `UserGroup::"admin"` 成员**——数据源是库内 `/.mega_cedar.json`，由 config `monorepo.admin` 播种（ADR-UN-03）。这既是收敛也是可审计性：ACL 就是全部答案。

删除的是**特例**而不是这两个名字：把它们写进 ACL 与写任何其他名字效果完全相同（`un04_a_formerly_hardcoded_name_becomes_admin_only_by_being_listed` 钉住这一点）；把某人从 ACL 移除会真正撤权（`un04_dropping_an_admin_from_the_acl_removes_their_privilege`）——正是硬编码规则曾经悄悄破坏的性质。

**与 website `role=admin` 的边界**：两者是**独立系统**。website 的 `role=admin` 只治理产品面（站点管理界面），monoengine 的 admin 只来自上述 ACL；两边都要授予的用户必须**双写**。自动同步未实现（DEP-02 / DEFER-UN-03 已移交），运维双写指引由 UN-06 手册承接。

## `pushRepo` 策略语义修正（UN-09）

`pushRepo` 此前同时出现在两个不该出现的地方：

- **公开仓库块**（`unless { resource.is_private }`）——于是「仓库是公开的」等于「任何人都能往里写」；
- **reader 块**——于是读权限蕴含写权限。

两者都与角色名的承诺相反。现在 `pushRepo` 只在 maintainer 与 admin 动作集中；admin 单独列出而不依赖它继承 maintainer 组，因为那条继承关系是 ACL 数据，可能被改掉。

矩阵（`src/contract/policy/un09_push_matrix.rs`）在**真实 init 产物派生**的 fixture 上求值（`generate_entity` 生成与服务端播种同形的实体，含历史拼写 `matainer`，DEFER-UN-06），再补入各角色成员：admin/maintainer 在公开与私有下均可 push，reader 与无组主体均不可；同时断言两个被编辑块的**其它动作**一字未改（公开仓库仍可 view/pull/fork/openIssue/createMergeRequest）。

`off` 默认下策略内容是惰性的——没有求值方，因此无行为变化。

## merge queue 后台执行主体判定（UN-17）

排队的合并在**入队请求早已结束之后**才执行——ACL 可能已经变了，而 worker 本身不是一个主体。因此后台执行以 UN-20 持久化的 `requester` 作为 `authz_principal`，在**执行时刻**重新按 `approveMergeRequest` 判定；`execution_actor` 仍是 `system`（保留既有审计语义，ADR-UN-06 ④）。

`decide_queue_execution(enforcement, snapshot, requester)` 是纯函数：

- `off`：不构建不消费，执行与本卡之前完全一致（legacy 项仍以 `system` 跑）。
- `shadow`：真实评估并记录 would-deny，但**不改变**执行。
- `enforce`：requester 未授权、快照未构建、store 为空、requester 为 NULL —— 一律冻结（fail-closed）。

冻结经 UN-25 的 helper 落库（`Failed` + `SystemError`、保留 requester、消息含可重试条件）并产生 `event=merge_queue_authz_frozen` 告警；随后 worker 自身的失败写入发现该项已 Failed，因此**保留**这一诊断而不是覆盖它。

**requester 为 NULL 的项**（早于 requester 捕获入队，或匿名入队）单独给出人工处理指引：这类项没有可授权的主体，单纯重试永远不会成功，必须由运维以具名用户重新入队。指引常量随代码固化，冻结消息与告警都带上它。

## merge-no-auth 入口鉴权与双参数签名（UN-24）

`merge-no-auth` 原本是本地调试的捷径：不在 guard 映射内、硬编码 `"system"` 执行者——**能连上端口的人就能合并**。UN-24 把它按 `{method, path}` 纳入映射（action = `approveMergeRequest`，与 `merge` 完全相同），并让 handler 经 `OptionalSessionUser` 取可选主体。名字里的 "no auth" 从此只表示**不需要认证会话**，不表示跳过授权。

`merge_cl` / `merge_cl_unchecked` 的签名落地两个**独立**参数（ADR-UN-06 ④）：

| 参数 | 含义 | merge | merge-no-auth | queue |
|---|---|---|---|---|
| `authz_principal` | 以谁的身份被**授权** | 登录用户 | 登录用户，匿名时为保留字 `User::"__anonymous__"` | `system`（UN-17 改为持久化的 requester） |
| `execution_actor` | 记录为由谁**执行** | 登录用户 | 登录用户，匿名时为 `system` | `system` |

两者合一会导致二选一的错误：要么审计记错执行者，要么以错误主体授权。匿名调用在 `enforce` 下由 guard 返回 **403**——不是 401：后者是「没有会话」的回答，用在这里等于把认证问题冒充成授权答案，而且无会话本来就能产出 401，会成为假绿。

四角色矩阵（`api::un24_merge_matrix` / `api::un24_merge_no_auth_matrix`）对两个入口断言**同一结果**。按当前 `mega_policies.cedar`，`approveMergeRequest` 是 maintainer 级动作（admin 经组继承同样具备），reader 与匿名被拒；「批准是否应当仅限 admin」属策略内容问题，归 UN-09/UN-04。

## guard 三态接线与真实资源解析（UN-08）

`/api/v1` guard 从「与 store 无关的硬编码 permit-all」切换为对共享快照的真实三态评估：

- 模式取自 `[cedar].enforcement`。`off` 直接短路——不取快照、不解析资源、不求值，行为与切换前完全一致。
- `shadow` 真实评估并记录 would-deny（结构化字段 `event=authz_would_deny` / `principal` / `principal_type` / `action` / `resource`），但**不改变放行**。
- `enforce` 拒绝并返回 403。
- **路径前缀**：guard 是挂在 `/api/v1` 之下那层路由的 route layer，请求到达时路径带该前缀，而映射是相对路由写的。resolver 先剥掉 `/api/v1` 再匹配——此前没有剥，导致没有任何受保护路径能匹配上，guard 实际上从未拦过任何请求。
- **资源解析**：带 CL link 的受保护端点经 `get_cl(link)` 解析出 `mega_cl.path`（UN-10 的唯一索引使之成为等值探测），再按 ADR-UN-05 归一到根仓库；不带 link 的（`/cl/labels`、`/cl/assignees`）直接作用于根。
- **fail-closed 的三种情形**：link 查不到（含存储查询失败）、根仓库实体缺失、快照未构建——在 `enforce` 下一律拒绝，在 `shadow` 下记录并放行。principal 类型不在 schema 内（如 Bot）同样按 would-deny 处理，其语义归 UN-27。
- **匿名主体**是保留字 `User::"__anonymous__"`（ADR-UN-06 ⑤）而不是 `"reader"`：后者与真实账号名冲突，一旦有人注册 `reader`，匿名请求就会继承该账号的权限。

`decide_guard` 是纯函数（与 push 面的 `decide_push` 同形），三态 × principal × 资源可解析性的矩阵在其上逐条断言。

## guard 映射 `{method,path}` 键与路由对齐（UN-23）

`guarded_endpoints.json` 的键从 path 升级为 `{method, path}`（前缀 → 小写方法 → 路径模式 → action），resolver `resolve_cl_action(method, path)` 接收方法参与匹配。同一路径的不同方法本就是不同 action——`GET /cl/{link}/reviewers` 只读取评审人列表，`POST`/`DELETE` 则修改它——只按 path 归一时，三者里必然有两个被映射错。未登记的 `{method,path}` 视为 unprotected，且**不继承**同路径其它方法的 action。

映射同时对齐了真实注册路由：`/{link}/approve` → `/{link}/reviewer/approve`、`/{link}/resolve` → `/{link}/review/resolve`、`/{link}/labels` → `/labels`、`/{link}/assignees` → `/assignees`；失效条目 `/reviewer/{link}` 与 `/reviewer` 移除（真实路由是 `/{link}/reviewers` 的 GET/POST/DELETE 三个方法）。

固定 `{method, path, action}` 矩阵随卡以测试常量形式提交（`cedar_guard.rs` 的 `UN23_MATRIX`，21 条已登记 + `merge-no-auth` 显式留待 UN-24），因此路由新增或改名而未同步映射会直接让测试失败。受保护的 20 个操作在 OpenAPI 中声明 403（`docs/errors.md` 记录 403/401 的语义区分）。

## 请求级认证主体单一解析（UN-22）

同一 HTTP 请求的认证主体**只解析一次**，全部消费方共享同一结果。此前 guard 与 handler extractor 各自调用 session store：不仅多一次存储调用，更关键的是会话在两次解析之间过期或被撤销时，二者会对同一请求得到**不同**主体。

- `ResolvedSessionPrincipal(Option<LoginUser>)`（`src/api/oauth/mod.rs`）是 request-scoped 的解析结果，缓存在请求 extensions 里。
- `resolve_session_principal(parts, store)` 是唯一解析入口：命中缓存直接返回；未命中则解析一次并回填。
- **三种来源结果归一为一个值**：有会话 → `Some(user)`；无会话与 store 故障 → `None`。归一故障保持既有行为（查询失败一直按「未登录」处理，ADR-WA-03），但在解析点保留 warn 日志。
- 消费方：`SessionUser` / `LoginUser`（`None` → `AuthRedirect`，行为不变）、新增的 `OptionalSessionUser`（匿名 → `None`，**永不返回 401**）、以及 guard 的 `guard_principal`（`src/contract/policy/guard/cedar_guard.rs`）。
- guard 侧 Bot 分支保持既有优先级（其授权语义归 UN-27）；Bot 请求不解析浏览器会话。匿名请求在 guard 侧是一个主体而不是拒绝——是否放行由策略决定；该主体是保留字 `User::"__anonymous__"`（UN-08 起，ADR-UN-06 ⑤）。

测试用 `CountingSessionStore` 双：它**每次调用返回不同用户**，因此「两个消费方得到同一答案」只可能来自缓存——用固定用户的 double 无法证伪。

## Vault 只读引导模式（UN-31）

审计/只读运维命令要能声称「什么都没改」，就不能走生产引导路径。常规 unseal 在读到第一个 secret 之前就会写：mount 表缺失时补写默认 mount、旧格式条目回写、默认 ACL policy 补写、token salt 补写，并启动一个按 200ms tick **撤销过期租约并删除其记录**的线程。绕开 `VaultCore::config()` 不够——这些副作用在 `Core::post_unseal()` 里。

- **入口**：`VaultCore::open_readonly(vault_storage, key_path)`（`src/contract/vault/integration/vault_core.rs`）。它要求一切都已存在：storage 已初始化、key 文件存在且份额足够、`runtime_tokens` 完整。任一不满足都是**报告**而不是修补——补签一个缺失的 runtime token 本身就是「写 policy + 签发 token + 回写 key 文件」，正是本模式要避免的。同理，它不初始化、不回写 key 文件、不撤销 root token。
- **核心开关**：`Core::readonly`（构造时确定、终生不变，`src/vault/core.rs`）。`RustyVault::new_readonly()` 据此装配。三个控制点：
  - `post_unseal()` 走 `MountTable::load_readonly()`：只 load，不 `load_or_default`；表缺失或存在旧格式条目（需要 `mount_update` 回写）一律 `ErrCoreReadonlyStateIncomplete` **fail-closed**——凭空造出来的 mount 表不是运行中服务器用的那张，据此出报告比拒绝更糟。
  - `AuthModule::init()` 走 `load_auth_readonly()`（同样只 load、fail-closed），且**不调用** `start_check_expired_lease_entries()`。租约恢复走 `restore_readonly()`：`load_lease_entry()` 遇到旧格式条目会**转换后写回**，只读下这个写只会被最终保险拦下、调用方拿到的是「写被拒绝」而不是真实状况；因此旧格式租约在此**具名 fail-closed**（与旧格式 mount 表同样处理），当前格式的条目照常读进内存队列——没有 worker 就没有人去动它。
  - `PolicyModule::init()` 跳过 `setup_policy()`；`TokenStore::new()` 在 salt 缺失时 fail-closed 而不是新签一个（新 salt 会静默改变该 vault 里每个 token 的哈希方式）。
  - mounts monitor **不创建**，无论配置的 interval 是多少：它是一个会在审计读取期间重载并可能重新挂载的后台线程。
- **最终保险**：`ReadonlyBackend`（`src/vault/storage/readonly.rs`）包住物理 backend，`put`/`delete` 一律硬失败并计数。它是最后一道而不是第一道——上面每条路径都能被 review、也都可能漂移，这一层则没有通往被包 backend 的路径。**拒绝必须是错误，不能是静默 no-op**：被吞掉的写会让调用方以为状态已持久化。`VaultCore::denied_writes()` 暴露计数，正常只读运行应当为 0；非 0 意味着上层仍有人尝试写、只是被这层拦住了。

测试（`src/vault/un31_readonly.rs`）**成对**写：可写侧证明该修补对这份 storage 确实会发生，只读侧证明它没发生。单侧断言在一个「本来就没什么可修」的 fixture 上同样会通过。后台线程是**直接断言**（`mounts_monitor.is_none()`、`ExpirationManager::is_lease_checker_started()`），不是从「没观察到副作用」倒推——后者只是和 200ms tick 赛跑。
