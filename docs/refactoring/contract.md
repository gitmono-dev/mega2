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

## `JupiterBackend::list` 的契约（FIX-04）

barrier view 按**层**遍历后端：`BarrierView::get_keys()` 向某个 prefix 要它的**直接子项**，对以 `/` 结尾的名字递归，其余当作**相对于该 prefix** 的叶子。`physical::file::FileBackend` 正是这么答的；`JupiterBackend` 原先直接转发 `VaultStorage::list_keys` 的 `LIKE 'prefix%'` 结果——**递归的完整 key**。于是被遍历出来的每个「叶子」在随后的 `get` 上会二次拼前缀而必然落空。

这不是外观差异：`ExpirationManager::restore()` 就是这样遍历租约视图的，因此在数据库后端下**重启恢复不到任何租约**，过期租约永远不会被撤销；`PolicyStore` 的类型表与 `list_policy` 键值同样错误；`Core::migrate()` 按 `key.ends_with("/")` 递归，也不会进入子目录。

修正是在 `JupiterBackend::list` 里做投影：`strip_prefix` 后取到第一个 `/` 为止，子目录补 `/`，去重排序。这同时收住了 `LIKE` 模式——`_` 与 `%` 对 Postgres 是通配符、对 vault key 只是普通字符，扫描会返回「形似」的 key，而 `strip_prefix` 是精确匹配。

prefix 也接受「目录名但不带尾部 `/`」的形状——`TokenStore::revoke_tree_salted` 遍历某个 token 的子节点时传的就是 `parent/<id>`——因此投影前先剥掉余下部分的前导 `/`；否则子项名会变成一个裸 `/`，遍历在第一层就停住。

**契约只对段对齐的 prefix（空、以 `/` 结尾，或恰好是一个目录名）定义**，这也是 barrier view 唯一会产生的形状。对停在名字中间的 prefix，两个后端确实不同（文件后端把 prefix 当目录路径解析，因而列不出东西）；这一边界有具名用例记录，不是留给后来者去撞。

## 只读命令的配置预检与来源链（UN-34）

`ConfigLoader::load()` 在无源可解析时会**写出**一份默认 `config.toml` 并在 stderr 上像报好消息一样告诉你。对只读命令这有两重问题：它改动了自己被要求观察的那台系统；而且此后它报告的一切描述的是**本进程刚发明**的配置，不是服务器实际在跑的那份。

- `ConfigLoader::load_readonly()`（`src/config/loader.rs`）两种情况都拒绝：**具名**源（`--config` / `MEGA_CONFIG`）指向不存在的文件——`load()` 原样把这个路径交回去（该路径上没有任何存在性检查），失败要到两层之下才以读取/解析错误的形式冒出来，描述的是错误的问题；以及一个源都解析不到——**这才是** `load()` 会生成默认文件的那一支。`cwd` / `global` 是**因为文件存在**才被选中的，无需再查；profile 不存在照常拒绝——只读入口不放松任何既有校验。
- `load_readonly_with_ambient(cwd, global)` 把两个环境相关的查找作为参数传入。这不是为了灵活性：ambient 源读进程 CWD 与 `MEGA_BASE_DIR`，测试既不能假设也不能安全改动它们（并行测试二进制里改 env 正是「单跑通过、全量跑失败」那一类），把它们外提才能就「什么都解析不到」这一支做出断言。
- `CommandContext.config_summary: Option<LoadedConfigSummary>`（`src/commands/mod.rs`）携带 loader **实际解析出的**来源，而不是在下游从命令行重新推导——重新推导得到的是意图，不是结果。`LoadMode::None` 没有加载任何配置，因此是 `None`：一个空摘要等于对一份本进程从未读过的配置作出断言。
- `paths` 只供运维诊断。进入报告的是 `LoadedConfigSummary::sanitized()` → `SanitizedSourceSummary`，**JSON 表示冻结**为 `{"source": "cli"|"env"|"cwd"|"global"|"default_generated", "profile": <string|null>}`：唯一表示、无路径、无可选字段——消费方要在若干种形状之间猜的话就没法比对两份报告了。无 profile 是显式 `null` 而不是缺键。丢掉路径是目的本身：配置路径描述文件系统布局，profile 路径可能直接点名部署环境，两者都不该出现在会流转的产物里（ER-11）。用例断言的是**序列化后的文本**，因此改名或新增字段会在这里失败，而不是在明年读这份报告的地方失败。

## 审计专用只读读取面（UN-30）

生产装配在任何命令体跑起来之前就已经写过了：`database_connection()` 连上就跑迁移；`Storage::new_with_connection()` 写默认 sidebar；`AppContext::new()` 还会 `init_monorepo()`（写 refs 与对象）并拉起通知 worker。据此装配起来的命令，说不出「我什么都没改」。

- **只读连接**：`init::read_only_database_connection()` 不跑迁移，并把会话设成 `default_transaction_read_only=on`。只读是**服务端**保证的，不是这段代码小心的结果——任何经这条连接抵达 Postgres 的写都在那边被拒，覆盖的正是没人想到去审的路径。**边界要说清**：这是**会话级**保证，不是角色级。连接用的仍是配置里那个有写权限的账号，一段刻意 `SET default_transaction_read_only = off` 的代码能绕过去。真正的纵深防御是给审计命令配一个只读角色（部署侧配置），本卡不代替它。
- **URL 改写**：`read_only_db_url()` **保留**原有的 `options`（测试库的 schema 隔离就是这样设的，丢掉它会悄悄连去另一个 schema），把只读选项**追加在最后**——libpq 的 `-c` 从左到右生效，追加在后意味着一份试图把只读关掉的 URL 赢不了。
- **最小读取 facade**：`ReadOnlyStorage`（`src/jupiter/storage/mod.rs`）只装读一次真正需要的东西：mono storage（解析根 ref 与树）与对象存储（取 blob），`read_authz_source()` 走的是服务端启动时同一条读路径，因此审计描述的是服务端真会加载的那份文件。**不**写默认 sidebar，不装配任何为写而存在的 service。以后要加，应当是加一条谁真的需要的读，而不是把完整装配请回来。
- **只读上下文**：`ReadOnlyContext::open()`（`src/context/mod.rs`）串起只读连接、只读 Vault（UN-31，且**仅在** object storage 配置真的含 `vault://` 时才打开——为没有 vault 的部署去开一个 vault，只为读取零个 secret，是把审计变成不可用）与 provenance 摘要（UN-34）。
- **零写入证明**：`bin/tests/integration_authz_audit.rs` 用**真实二进制**播种（迁移 + `init_monorepo` + sidebar 全都真的发生），拍下六个面的快照（范围是**除系统 schema 外的全部 schema**，不限于 `public`——只读装配若在别处留下东西，那也是一次写）——schema（schema.表.列:类型）、**每表内容摘要**（整行转文本后排序聚合再 md5，而不是行数：一次 UPDATE 不改变计数）、**序列当前值**（被回滚的插入不留行却会推进序列，那同样是一次写）、refs 全行、对象三表全行、对象存储目录的内容散列——跑完只读装配再拍一次，逐面比对。最后做两次量具校准：写一行探针（快照必须变）、再原地改写它（行数不变、摘要必须变）；否则「前后相等」可能只是因为这份快照什么都没量到。Vault 与文件系统面归 UN-43。

## 只读装配的副作用禁用（UN-43）

`AppContext::new()` 在任何命令体跑起来之前就启动通知 worker、建 Redis 连接、调用 `init_monorepo()`（写 refs 与对象）。UN-30 交付了读取面，本卡管的是它周围的装配，以及**把每一项「没做」变成可断言的事实**——注释里写「不会启动」不构成证据。

`ReadOnlyContext::open()` 因此逐项对照（`src/context/mod.rs`）：不装通知服务、不建 Redis、不做任何 seed/write、Vault 仅经 UN-31 的 readonly bootstrap，且**只在** object storage 凭据真是 `vault://` 引用时才打开。

判据都写成**成对**用例（`src/context/un43_readonly_assembly.rs`），因为「没有副作用」只在副作用可能发生的地方才有意思：

- 通知服务：断言 active 句柄**未变化**而不是为 None——它是进程级全局，同一二进制里别的用例可能合法地装过一个，只在自己先跑时才通过的用例比没有用例更糟。比的是 `Arc::ptr_eq` 身份而不是「有没有」：把一个 active 服务换成另一个，同样是副作用，只看 `is_some()` 看不见。
- Redis：**数调用次数**，不是只看装配活没活下来——一条连了、把失败吞掉、然后继续跑的路径同样能通过「活下来」的检查，而它确实做了那件事。计数按 URL 分桶（测试二进制是并行跑的，全局计数会被同时发生的别的初始化搅乱，只在别人不跑时才通过的用例比没有用例更糟）。对照断言真的调用一次时计数确实会动。
- seed：迁移过但空的库上开完之后 `mega_refs` 与 `dynamic_sidebar` 仍为 0；对照里 `init_monorepo()` 与生产 storage 装配确实把它们写了出来。
- Vault：用真实 bootstrap 建一个 vault 并存入两个 s3 凭据，再用 `vault://` 引用的配置开只读上下文——断言拿到的是只读句柄、`denied_writes()` 为 0，且 vault 表摘要与 key 文件字节都没变。key 文件是关键的一面：bootstrap 路径会轮换 runtime 凭据并回写它。

黑盒面（真实二进制播种 + Vault/文件系统快照 diff）见 `docs/refactoring/integration.md` 的「只读装配的零副作用比对」。

**这一面量得到什么、量不到什么**：文件系统快照收录的是「最终存在的文件」，因此**创建后又删除**的临时文件不会被发现，两个受监视根目录之外的写也不会。前者是快照法的固有边界，后者是登记范围的选择；两条都写在这里，是因为一份不说明边界的「零副作用」证明会被读成比它实际更强的东西。

## 受限产物写入器（UN-32）

审计产出的产物不是普通输出：有些携带**不得公开的差异**，而写它们的命令可能以特权服务账号运行、写入运维配置的目录。这让**路径本身**成为安全边界——如果路径上的某个分量能在「检查」与「写入」之间被换成 symlink，产物就落到了没有人选择的地方。

- **边界说清**：根**之上**的路径分量按常规解析（根路径来自部署登记、在其下任何东西存在之前只解析一次；要求通往它的每一段都不是 symlink 会拒绝 `/var` 或 home 是链接的寻常布局）。本模块守的是根**之下**——那里才会出现攻击者能创建的名字。
- **根以 fd 锚定**：`RestrictedRoot::open()` 只打开一次并持有目录 fd，之后所有操作都相对它、且带 `O_NOFOLLOW`。相对活 fd 解析的路径，事后改名目录也改不了指向；路径上任何一段是 symlink 都是错误而不是跟随。`..`、绝对分量一律**拒绝而不规范化**——规范化正是逃逸绕过检查的方式。根自身是 symlink 也拒绝（否则「那个根」的含义取决于链今天指向哪里）。
- **必选性写在能被测试的地方**：`RestrictedRoot::from_option(None)` 直接是错误。CLI 的 `--restricted-root` 旗标归 UN-29；这里保证的是**拿不到一个没有根的 writer**，于是没有调用方会写进一个谁都没登记的默认位置。
- **run-id 由本模块独占生成**（`^[0-9]{8}T[0-9]{6}Z-[0-9]+$`）：调用方给的 id 就是调用方选的目录，等同于 fd 锚定要防的那种逃逸。「不可由外部提供」是这条判据本身，因此**注入 id 生成器的接缝在测试之外不存在**（`create_with` 为 `#[cfg(test)] pub(crate)`）。真正保证唯一的是 `mkdirat`（名字被占就失败），时间戳只是给人读的、随机后缀只是让同一秒的两次运行可区分；碰撞**有界重试 ≤ 5** 次后非零退出。`create_with` 允许注入 id 生成器——碰撞分支只能这样被走到，等一次真实的时钟碰撞不叫测试。
- **逐产物写序列**（差别就在崩溃终态）：
  - **run 输出**：最终路径 `O_EXCL|O_NOFOLLOW` 0600 → write → 文件 fsync → 目录 fsync，**无 rename**。崩溃留下半截文件是**正确的**：run 是否完成只由进程退出码与 `run_id=` 行判定，半截文件属失败 run 的残骸，交给 sweep。
  - **baseline 版本文件**：临时文件 `O_EXCL` → write → fsync → `renameat2(RENAME_NOREPLACE)` → 目录 fsync。版本文件以内容 digest 命名，因此同名再来一次要么写的是同样的字节（无事发生），要么这个名字已经不再是它自称的意思——用**逐字节读回**区分，而不是重算 digest（用写它的同一段代码重算 digest 什么也证明不了）。
  - **current 指针**：唯一**该被替换**的产物，因此用普通原子 rename；并发促晋的串行化归 UN-35。
- **读路径不创建任何东西**：`read_pointer` 以 `create=false` 打开 `baselines/`，`baselines/` 不存在与指针不存在是同一个答案。一条会建目录的读路径就是往根里写过东西，正是读取规则要守的那条姿态。所有 libc 调用带 `EINTR` 重试——信号在不巧的时刻到达不该把一次合法操作变成硬失败，调用方也分不清它和一次真正的拒绝，而「分得清」正是本模块要做到的事。
- **读取输入同样经根 fd**：指针路径**内部派生**（`baselines/current.json`，不接受第二个路径参数——第二个参数只是又一个可以搞错的东西）；candidate 只接受 root-relative 的 `<run-id>/<裸文件名>`；读取带 `O_NOFOLLOW|O_NONBLOCK`、校验 `S_ISREG`、并要求 `st_nlink == 1`（依次拒绝：被掉包的 symlink；会让读取阻塞或说谎的 fifo/设备——`O_NONBLOCK` 是必需的：`S_ISREG` 那句拒绝要等 open 返回才轮得到，而以只读打开一个 FIFO 会一直阻塞到有写者出现，「挂死」比「读错文件」更糟，因为没人拿得到可处理的错误；**硬链接**——`O_NOFOLLOW` 挡得住符号链接，但同一 inode 的第二个名字不是「链接」而**就是**那个根外文件，只是挂在根内的名字下。本模块写出的每个产物都只有一个名字，多于一个即是它没创建过的东西。**这条是纵深防御而非边界**：链接数可变，读之前把根外那个名字删掉就剩一个了；真正的边界是「谁能在根下创建名字」——能在那里挂硬链接的人同样能直接把文件写出来，所以这条检查挡的是疏忽而不是蓄意）。**symlink 指针报为拒绝而不是「没有指针」**：当成缺失会让人用一条悬空链把「已钉住的基线」变成「未钉住」。
- **平台策略**：Linux 专属，非 Linux **fail-closed** 并给出可操作指引。`RENAME_NOREPLACE` 没有可移植等价物，退化成「先查后改名」会让 API 看着照常工作、而保证在没人核对的平台上悄悄变成一句空话。

## 受限根的留存 sweep（UN-38）

run 与 baseline 版本会不断堆积，总得有东西删掉旧的。而这个「东西」删的是运维用来存证据的目录里的文件，因此**这里真正要防的失败是删多了**——丢掉某次晋级钉住的基线、或者一个还在写的 run，比留太多糟得多。

删除因此被**三条彼此独立**的界限约束（`src/contract/policy/secure_sweep.rs`）：

- **统一维护锁** `.maintenance.lock`（根 fd no-follow + `flock(LOCK_EX)` 全程；sweep/promotion/admission 共用）。锁由**调用方**持有并传入，而不是在 sweep 内部现取——它之所以是**一把**锁，就是为了让「清理 + 晋级 + 准入」能落在同一个临界区里。`try_acquire` 供宁可不做也不愿等的调用方使用：sweep 是家务事，让一条命令堵在别人的家务事后面，是用「有界的目录」换了「无界的等待」。
- **活动 run 保护**：`.lease.lock` 被持有（`LOCK_NB` 探测失败即判定活动），**或**计数器里有未结算 reservation 且年龄 ≤ 60 分钟。lease 是可靠的那条——进程死了内核就释放，崩溃的 run 无需谁去收拾就自动不再受保护；reservation + 年龄那条覆盖「已占计数器、还没拿到 lease」的窗口，而**年龄上限**是防止一条被遗弃的 reservation 永久保护一个目录、使留存上限悄悄失效。**lease 是「取住」而不是「探一下」**：探完就放会留下一个窗口——run 的属主在窗口里拿到 lease 开始写，而 sweep 正删到一半，于是这个 run 同时「活着」和「已被删除」。所以删除期间全程持有它；拿不到就说明属主已经在了，跳过。（持有期间连 `.lease.lock` 一起删掉是安全的：锁挂在打开的文件描述上，不挂在名字上。）
- **保护集合**：current 指针指向的版本 + `protected.json`。保护版本既豁免删除、**也不计入 K**——若保护占用一个留存名额，运维「保护某个版本」这个动作就会顺手挤掉另一个版本，等于用保护做了一次删除。

其余冻结语义：`.tmp-*` 仅在年龄 > 60 分钟**且属主已失效**才清——「老」不等于「属主没了」，因此凡是所在层级有 lease 的，先问 lease（活属主的残骸不是残骸）；**残余风险如实登记**：位于既无 lease 也无其它属主标记的层级的临时文件，年龄是唯一可用信号，这正是 60 分钟阈值存在的原因。递归删除**不跨设备**（run 目录下的挂载点是别人的文件系统，递归进去就把一次有界清理变成了对挂载内容的遍历；留着不动会让随后的 `AT_REMOVEDIR` 以 ENOTEMPTY 失败——拒绝，而不是即兴发挥）；在清空目录后、执行 `AT_REMOVEDIR` 之前**校验 inode 身份**（`unlinkat` 认的是名字不是 inode，中途被改名会让它删掉此刻在那个名字上的另一个目录；名字对不上就**保留**——空目录是下一轮 sweep 会收走的残骸，删错的目录收不回来）；`readdir` 返回 NULL 前先清 `errno`（NULL 既表示读完也表示出错，不分辨的话一次失败的读取看起来和一次完整的列举一模一样，sweep 就会基于被截断的视图动手）；「最旧」的事实源是 **mtime**，并以**名字**做 tie-break（哪个活下来不该取决于目录的枚举顺序——那不是任何人做过的决定，而且同一份 fixture 在两个文件系统上会给出不同答案）；名字不像 run-id 的条目、以及 `baselines/` 下不是规范版本文件名（64 位小写十六进制 + `.json`）的条目，**一概不动**（一个「不认识就删」的 sweep 是一把以目录列表为输入的删除原语；把 `baselines/` 里每个文件都当版本，会让运维放在那儿的一份说明成为留存上限的牺牲品）；symlink 从不参与 sweep（它不是本模块写出的产物，跟过去按目标的年龄判断链的年龄更是拿目标的话当链的话）。

digest 只接受**规范小写**形式（版本文件由 `hex::encode` 命名即小写；接受大写会拼出一个匹配不到任何文件的名字，而一条什么都匹配不到的保护项与「没有保护」无从区分）。指针只要求 `digest` 一个字段、**允许未知字段**：指针完整 schema 归 UN-39 且必然会长出新字段，本卡该严的是它依赖的那条性质——认不出当前版本就拒绝。

**「先能拒绝，再动手」**：保护集合（指针 + manifest）在**任何删除发生之前**解析完毕。若等到删完 run 才发现 manifest 读不懂，那 fail-closed 就只对还没执行的那一半成立。

**fail-closed 的边界**：指针读不懂、manifest 解析不了（非法 JSON / `schema_version` 不是 1 / digest 不是 `sha256:<64hex>` / 多余字段），一律**停止 sweep**而不是退化成「那就当没有保护」——在不知道什么受保护的情况下继续删，正是这里唯一要避免的结局。**缺失不等于读不懂**：根本没有 manifest 只意味着除 current 外没有额外保护。

**本卡的接缝（不是猜测）**：reservation 计数器归 UN-57（这里只要求「每个 run 一个 bit」的 `ReservationView`，两卡因此不必就更大的东西达成一致；`NoReservations` 是计数器就位前的默认——刻意选**偏向删除**的一侧，因为没有计数器时保护活动 run 的是 lease 与年龄，而假装每个 run 都有 reservation 会直接让 sweep 失效而不是更安全）；准入与告警归 UN-58、指针完整 schema 归 UN-39（这里只需要「当前是哪个版本」，读不懂即拒绝）。

## 留存 sweep 的审计报告（UN-49）

sweep 每跑完一轮，在同一把维护锁下写出一份审计报告（`src/contract/policy/secure_sweep.rs`）：

- **路径**：`<root>/sweep-reports/<run-id>.json`（`O_EXCL`、`0600`、文件 fsync + 目录 fsync；run-id 由 writer 侧 `generate_run_id()` 独占生成）。
- **逐条记录**：`deleted` / `skipped-active` / `skipped-protected` / `kept`，每条带理由；非法命名项**永不删除**，报告里只保留前 100 个名字 + `total` 计数。
- **有界性**：明细条目生成期收敛到 ≤ 1000；序列化字节 ≤ 256 KiB 为**写时硬上限**——超限整份写入失败，**不截断产物**。
- **自限 R=100**：`sweep-reports/` 只留最新 100 份；超出时按 mtime 删最旧，同秒以文件名字典序 tie-break（与 sweep 本体对 run/version 的「最旧」事实源一致）。

预留/结算协议的接线归 UN-59；本卡不消费计数器。

## 容量计数器核心（UN-51）

受限根上的可信账本（`src/contract/policy/secure_counter.rs`），文件名 `.counters.json`：

- **Schema**：`schema_version=1`；计数字段 `runs` / `versions` / `reports` / `total_bytes` / `reserved_bytes`；数组 `reservations[]` / `settled[]` / `delete_settled[]`。严格解析（`deny_unknown_fields`）；未知版本或额外字段 fail-closed。
- **有界**：create 类 `reservations` ≤ 64（delete action 不占槽）；`settled` ≤ 64；`delete_settled` ≤ 16（满时按 `settled_at` 再 `op_id` 驱逐最旧）；序列化 ≤ 48 KiB，其中 8 KiB 为 delete/恢复应急 headroom（create 路径只许到 40 KiB）。
- **owner_fenced**：仅 `kind=run`；evidence（`true`）结算须带 `run_id` + `cap_hash`（`sha256:<64hex>`）；audit（`false`）不得带 `cap_hash`。原始 `run_cap=` 明文永不得写入账本字节。
- **持久化**：维护锁内 `load_counter` / `store_counter`（临时文件 + fsync + rename + 父目录 fsync）。
- **对账**：`reconcile_counts_from_disk` 只重建磁盘计数，不动 reservation/settled 墓碑（生命周期归 UN-57）；按根目录 fd 走 `openat`/`O_NOFOLLOW`/`AT_SYMLINK_NOFOLLOW`，每目录 ≤ 1000 项、单 run 嵌套 ≤ 8 层。

reservation 操作、admission、公式常量、写时硬上限分别归 UN-57 / UN-58 / UN-54 / UN-59。

## 容量公式与常量（UN-54）

受限根峰值容量与常量的**唯一冻结处**（`src/contract/policy/secure_capacity.rs`）：

- **Canonical 公式**：`5 MiB × (N + A) + 2 MiB × (K + P + 1 current + 1 temporary) + 256 KiB × R + 64 KiB 固定元数据`（与计划「性能与容量摘要」逐字一致；`peak_capacity_bytes` 代入求值）。
- **参数**：N=20、K=10、A=2、P≤20、R=100、D=1000；固定元数据 64 KiB；promote admission **一律按 2 MiB** 计（`promote_admission_bytes`）。
- **元数据上限**（与 UN-51 对齐）：create reservations ≤ 64、settled ≤ 64、delete_settled ≤ 16、`.counters.json` ≤ 48 KiB（含 8 KiB delete headroom）。
- **per-type 写时硬上限**（值冻结于此，强制归 UN-59）：candidate/baseline ≤ 2 MiB、sanitized report ≤ 1 MiB、restricted diff ≤ 4 MiB、sweep-report/evidence ≤ 256 KiB。

producer 映射表归 UN-60；admission/告警归 UN-58。
