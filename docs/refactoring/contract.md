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
- guard 侧 Bot 分支保持既有优先级（其授权语义归 UN-27）；Bot 请求不解析浏览器会话。匿名请求在 guard 侧是一个主体（`User::"reader"`）而不是拒绝——是否放行由策略决定。

测试用 `CountingSessionStore` 双：它**每次调用返回不同用户**，因此「两个消费方得到同一答案」只可能来自缓存——用固定用户的 double 无法证伪。
