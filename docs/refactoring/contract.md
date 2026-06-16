# Contract 模块归并实现方案分析

本文档记录 `monoengine` 中 `contract` 模块的定位、当前归并结果、路径迁移边界，以及后续涉及 API 数据契约、Git 协议、Vault 与权限策略代码时应遵守的共同约束。

> **治理规范**：本文档遵循 **`general.md`** 中定义的统一结构、共同约束和执行标准。

> **集成测试指引**：本计划通过 `integration.md` 中的 API、Git protocol、Vault bootstrap、Policy guard 相关测试路径验证，不新增运行时行为。

## 事实校准（2026-06-16）

1. **`contract` 已成为顶层模块**。入口为 `src/contract/mod.rs`，下挂 `api`、`git_protocol`、`vault`、`policy`。
2. **当前 `api_model` 已迁入 `contract::api`**。HTTP API DTO、分页类型、Artifact/Buck/Chat/Git commit wire types 均在 `src/contract/api/`。
3. **Git/Vault/Policy 实现已迁入 contract**。原 Git HTTP/SSH 协议、VaultCore/PKI/PGP/Nostr、Cedar policy/entitystore/guard 代码分别位于 `src/contract/git_protocol/`、`src/contract/vault/`、`src/contract/policy/`。
4. **旧模块入口不保留 re-export**。`crate::api_model`、`crate::git_protocol`、`crate::vault`、`crate::saturn`、`crate::api::guard` 不再作为有效代码路径。
5. **数据库实体不迁移**。`callisto::vault` 是 SeaORM 实体模块，不属于 contract 归并范围。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
| --- | --- | --- |
| `contract::api` | 已激活 | 承载 API 请求/响应、分页、Artifact/Buck/Chat/Git DTO；只改变模块路径，不改变 JSON schema。 |
| `contract::git_protocol` | 已激活 | 承载 Git smart HTTP/SSH 入口；协议兼容性问题仍按 `protocol.md` 后续阶段处理。 |
| `contract::vault` | 已激活 | 承载 VaultCore 与消费端 helper；安全加固阶段仍按 `vault.md` 推进。 |
| `contract::policy` | 已激活 | 承载 Cedar context/entitystore/reviewer parser/admin resolver/API guard。 |
| 旧路径兼容层 | 未实现 | 本次迁移明确不提供 re-export，调用方必须使用新路径。 |

## 硬约束与不可违反的原则

1. **不改变 wire behavior**：HTTP 路径、JSON 字段、OpenAPI schema、Git smart protocol 字节流、Vault secret 数据格式和权限判定语义都不能因路径迁移改变。
2. **不混淆实体与 contract**：`callisto::*` 仍是数据库实体层，尤其 `callisto::vault` 必须保持原路径。
3. **旧路径不得回流**：新代码不得重新引入 `api_model`、顶层 `git_protocol`、顶层 `vault`、顶层 `saturn` 或 `api::guard`。
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
> - `rg "api_model|crate::git_protocol|crate::vault|crate::saturn|api::guard" src` 不命中有效代码引用。

**阶段 1 — 文档同步**

1. 更新 `README.md`、`general.md`、`integration.md`。
2. 更新 `protocol.md` 与 `vault.md` 中的路径。
3. 记录旧路径到新路径的映射。

> **验收标准**：
> - `docs/refactoring` 中旧源码路径仅允许出现在明确的历史路径映射段落。

**阶段 2 — 常规门禁**

1. 运行格式、clippy、测试三道门禁。
2. 若发现路径迁移引出的 warning，优先修正代码，不添加 blanket allow。

> **验收标准**：
> - `cargo +nightly fmt --all --check`
> - `cargo clippy --all-targets --all-features -- -D warnings`
> - `source .env.test && cargo test --all`

## 前置依赖矩阵

| Contract 工作 | 对其他文档的依赖 | 类型 | 关键同步点 |
| --- | --- | --- | --- |
| API DTO 迁移 | chat.md、notification.md | 协同 | DTO 路径应统一使用 `contract::api`。 |
| Git protocol 迁移 | protocol.md | 协同 | 只更新路径，不改变协议改进阶段。 |
| Vault 迁移 | vault.md、config.md、mail.md | 协同 | 只更新路径，不改变 SecretRef 或 bootstrap 设计。 |
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
