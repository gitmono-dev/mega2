# 错误定义集中化方案

本文档定义 monoengine 的错误类型归属、接口映射和后续扩展规则。目标是让项目自定义错误只在 `crate::common::errors` 下定义，避免 API、Vault、业务模块各自维护错误枚举。

## 目标

- 所有项目自定义错误类型统一定义在 `src/common/errors/`。
- 业务代码优先返回 `MegaError` 或领域错误类型，并通过 `From` 自动转换。
- HTTP 层统一使用 `ApiError` 做状态码和响应体映射。
- Vault 子系统继续使用 `RvError`，但它现在是 `libvault` crate 的类型（2026-08-21，`plan-20260820` VLT-02）：`common::errors` 只做 `pub use libvault::errors::RvError` 重导出，不再自带一份定义。
- 旧的 `crate::api::error`、`crate::vault::errors` 命名空间不再提供兼容 re-export。

## 当前结构

```text
src/common/errors/
├── mod.rs      # MegaError、MegaResult、BuckError、ProtocolError、GitLFSError、DiffParseError
├── api.rs      # ApiError、map_ceres_error
├── policy.rs   # ContextError、SaturnContextError
└── vault.rs    # VaultError、VaultResult（本仓定义）+ RvError（重导出 libvault::errors::RvError）
```

对外推荐导入路径：

```rust
use crate::common::errors::{
    ApiError, BuckError, ContextError, DiffParseError, GitLFSError, MegaError, MegaResult,
    ProtocolError, RvError, SaturnContextError, VaultError, VaultResult,
};
```

## 类型职责

| 类型 | 职责 | 使用位置 |
| --- | --- | --- |
| `MegaError` | 应用层主错误类型，承接配置、IO、DB、Redis、对象存储、Git、Buck 等错误。 | CLI、service、storage、业务模块 |
| `MegaResult` | CLI/命令执行类返回别名，当前为 `Result<(), MegaError>`。 | `commands/*`、`cli.rs` |
| `ApiError` | HTTP API 响应错误，负责把 `MegaError`、`anyhow::Error` 或 legacy `[code:xxx]` 文案映射为状态码和响应体。 | `src/api/**` |
| `ProtocolError` | Git HTTP/SSH 协议层错误，负责协议响应状态和 message。 | `src/contract/git_protocol/*`、server |
| `RvError` | `libvault` 库自身的错误枚举（重导出，非本仓定义）。response status 映射与 `PartialEq` 行为由上游提供。 | `src/contract/vault/**` |
| `VaultError` / `VaultResult` | Vault 集成启动、初始化、key 文件、运行时 token 和 API 操作错误。 | `src/contract/vault/integration/**` |
| `ContextError` | Cedar policy context 构建、schema、policy、validation 和 JSON 错误。 | `src/contract/policy/context.rs` |
| `SaturnContextError` | Cedar 授权请求构造和授权拒绝错误。 | `src/contract/policy/context.rs` |
| `BuckError` | Buck session/upload 业务错误，作为 `MegaError::Buck` 被 `ApiError` 映射为精确 HTTP status。 | Buck service/router |
| `GitLFSError` | Git LFS 处理错误。 | LFS router/handler |
| `DiffParseError` | Code review re-anchor 统一 diff 解析错误。 | `src/jupiter/utils/code_review_reanchor.rs` |

## 导入规则

新增和既有业务代码都必须直接从 `crate::common::errors` 导入错误类型：

```rust
use crate::common::errors::{ApiError, MegaError, RvError};
```

不要新增 `crate::api::error::*` 或其他模块级错误 facade。`src/api` 与 `src/contract` 只消费公共错误类型，不再承载错误定义。（顶层 `crate::vault` 已随 vendored 模块一并删除，库错误从 `libvault::errors` 来。）

## 边界说明

名称中包含 `Error` 但用于 API payload/schema 的结构体不属于本方案的错误定义，例如：

- `LogErrorResponse`
- `ObjectError`
- `QueueError`

这些类型是对外响应数据模型，应继续保留在 `api_model`、`ceres::model` 或协议 DTO 所在模块中。只有实际作为 `Result<T, E>` 错误、实现 `std::error::Error`/`thiserror::Error`，或承载内部失败语义的类型才应放入 `common::errors`。

## 扩展规则

新增错误时遵循以下规则：

1. 先判断是否能复用 `MegaError` 现有变体。
2. 需要领域语义时，在 `src/common/errors/mod.rs` 中增加领域错误枚举，并通过 `MegaError` 包装。
3. 需要 HTTP status 时，在 `ApiError` 的 `From` 映射中处理，不要在业务层手写 HTTP 状态。
4. 需要 Vault 语义时扩展本仓的 `VaultError`，**不要**扩展 `RvError`——它属于 `libvault` crate，本仓只重导出。只读引导的 `Readonly*` 变体就是这么加的（`plan-20260820` VLT-02/VLT-04）。
5. 不要在 API/router、Vault 工具、contract 或 storage 模块中新增局部 `thiserror::Error` 枚举；局部解析错误也应放入 `common::errors`。
6. 字符串型 fallback（如 `MegaError::Other` 和 `[code:xxx]`）只能用于兼容或过渡，新逻辑优先使用结构化变体。

## 授权拒绝的状态码（UN-23）

受 guard 保护的 `/api/v1/cl` 操作在授权被拒绝时返回 **403**，并在 OpenAPI 中显式声明该响应。

- **403 = 已识别主体但无权限**（授权拒绝）。
- **401 = 认证失败**（必选 extractor 拿不到会话），不得用来冒充授权拒绝——两者含义不同，客户端的处理也不同（重新登录 vs 申请权限）。

映射键为 `{method, path}`（`src/contract/policy/guard/guarded_endpoints.json`）：同一路径的不同方法可以是不同 action，例如 `GET /cl/{link}/reviewers` 是读（`viewRepo`），而 `POST`/`DELETE` 同路径是写（`editMergeRequest`）。未登记 `{method,path}` 的操作视为 unprotected，**不会**继承同路径其它方法的 action。

`POST /cl/{link}/merge-no-auth` 自 UN-24 起与 `POST /cl/{link}/merge` 映射到**同一** action（`approveMergeRequest`）并声明 403。其名字里的 "no auth" 只表示不需要**认证会话**，不表示跳过授权：匿名调用以保留字 `User::"__anonymous__"` 参与求值，在 `enforce` 下被拒为 403。

## merge 面「授权不可判定」的 503（UN-25）

`[code:503]` 映射为 **503 Service Unavailable**。此前未登记的码一律落 500，与真正的 bug 无法区分，且按契约不可重试。

- **503 = 现在判不了，可重试**：授权数据不可用（快照未构建 / dirty / 存储读取失败）时，merge 面既不能放行也不该按「变更被拒绝」处理。
- **500 = 服务端缺陷**，客户端重试无意义。
- **403 = 已识别主体但无权限**（UN-23）。

`merge` 与 `merge-no-auth` 在 OpenAPI 中同时声明 403 与 503。

排队项在同样情形下被**冻结**而不是失败：冻结行写在 **`push_queue`**（`kind=merge`），沿用既有终态 `Failed` + `SystemError`（不新增 enum、不加迁移，既有 retry 入口继续可用），保留 `requester`（它是重试时据以再判定的授权主体），`error_message` 写明可重试的条件。冻结路径同步产生进程内 `error` 日志 `event=merge_queue_authz_frozen`（字段 `cl_link` / `requester` / `reason`）——告警不做外部调用，因此不存在告警失败影响冻结事务的路径。事件名保持不变（告警查询稳定性）。

## CL commits 读取端点的状态码（MC-05，plan-20260827）

`GET /api/v1/cl/{link}/commits`（plan-20260827 MC-05，DEP-01 冻结契约）不新增错误类型，复用既有映射：

- **404 = 未知 `link`**：`MegaError::NotFound` 经 `ApiError` 的 typed matching 映射（处理器先查 CL 行确立存在性，再读清单——裸透传 `get_cl_commits` 的空列表会把未知 link 与无数据 CL 混淆）。
- **200 + 空列表 = CL 存在但无清单数据**（存量 CL 无回填，`mega_cl_commits` 无行）。
- **403 = 授权拒绝**：OpenAPI 中声明；Cedar guard 映射（`guarded_endpoints.json` 登记 `GET /cl/{link}/commits`）随 MC-07 落地，经 REL-MC-01（MC-08）与该端点同时发布——在那之前该路径按 UN-23 口径处于「未登记即 unprotected」的中间态，这正是家族原子发布的原因。

## 响应安全

- `ApiError` 只向客户端暴露 4xx 细节。
- 5xx 统一返回 `Internal server error`，内部细节只写日志。
- secret、token、password、API key 等敏感值不得写入错误 message。
- 涉及配置、Vault、凭据解析的错误应使用脱敏后的路径或字段名。

## 验证要求

涉及错误定义、错误映射或错误路径迁移时，必须运行：

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

并根据变更范围补充：

```bash
cargo build
cargo build --tests
```

## plan-20260905（Trunk 直推）

本计划未新增用户可见的 `MegaError` 枚举变体。`StaleMonorepoRootRef` 在计划前已存在。TP-15/17/19 的协议拒绝（trunk 链长、token 路径前缀、B0 非 `main` 分支）走既有 `MegaError::Other` 字符串，不在此登记新类型。TP-18 当时的 LFS review-only 404 字符串已由 [`plan-20260909.md`](./plan/plan-20260909.md) 移除（不新增错误类型）。

## plan-20260909（Storage-only LFS）

N/A：本计划未新增 `MegaError` 变体。Trunk LFS 拒绝沿用既有 HTTP 状态（401/403）与 `lfs_auth_challenge`，不登记新错误类型。

## plan-20260907（git-internal 0.9.0 / BLAKE3）

N/A：未新增用户可见的 `MegaError` 枚举变体。跨 kind / 错宽度 ID、非法 Buck `sha1:`+64 hex 与未实现的 LFS BLAKE3 业务路径走既有 fail-closed 文案，不在此登记新类型。

## plan-20260901（FastCDC Media + receive-pack / 导入可靠性）

N/A：未新增用户可见的 `MegaError` 枚举变体。Media HTTP 用领域错误映射 400/404/409/500（见 [`docs/refactoring/fastcdc-media.md`](./refactoring/fastcdc-media.md)）；`MegaError::is_retryable_db_serialization` 仅供 batch insert 内部重试分类，不改变对外错误面。

## plan-20260911（storage-only Agent Capture）

N/A：未新增用户可见的 `MegaError` 枚举变体。`/api/v1/agent-capture` 使用固定 HTTP envelope `{ "error": { "code", "message" } }`（`unauthorized` / `not_found` / `conflict` / `bad_request` / `payload_too_large` / `missing_raw`），不登记新的 `MegaError` 类型。
