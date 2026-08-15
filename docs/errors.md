# 错误定义集中化方案

本文档定义 monoengine 的错误类型归属、接口映射和后续扩展规则。目标是让项目自定义错误只在 `crate::common::errors` 下定义，避免 API、Vault、业务模块各自维护错误枚举。

## 目标

- 所有项目自定义错误类型统一定义在 `src/common/errors/`。
- 业务代码优先返回 `MegaError` 或领域错误类型，并通过 `From` 自动转换。
- HTTP 层统一使用 `ApiError` 做状态码和响应体映射。
- Vault/RustyVault 子系统继续使用 `RvError`，但定义归属移动到 `common::errors`。
- 旧的 `crate::api::error`、`crate::vault::errors` 命名空间不再提供兼容 re-export。

## 当前结构

```text
src/common/errors/
├── mod.rs      # MegaError、MegaResult、BuckError、ProtocolError、GitLFSError、StatusParseError、DiffParseError
├── api.rs      # ApiError、map_ceres_error
├── policy.rs   # ContextError、SaturnContextError
└── vault.rs    # RvError、VaultError、VaultResult、CryptoError、SealBoxError、rv_error_* macros
```

对外推荐导入路径：

```rust
use crate::common::errors::{
    ApiError, BuckError, ContextError, CryptoError, DiffParseError, GitLFSError, MegaError,
    MegaResult, ProtocolError, RvError, SaturnContextError, SealBoxError, VaultError, VaultResult,
};
```

## 类型职责

| 类型 | 职责 | 使用位置 |
| --- | --- | --- |
| `MegaError` | 应用层主错误类型，承接配置、IO、DB、Redis、对象存储、Git、Buck 等错误。 | CLI、service、storage、业务模块 |
| `MegaResult` | CLI/命令执行类返回别名，当前为 `Result<(), MegaError>`。 | `commands/*`、`cli.rs` |
| `ApiError` | HTTP API 响应错误，负责把 `MegaError`、`anyhow::Error` 或 legacy `[code:xxx]` 文案映射为状态码和响应体。 | `src/api/**` |
| `ProtocolError` | Git HTTP/SSH 协议层错误，负责协议响应状态和 message。 | `src/contract/git_protocol/*`、server |
| `RvError` | Vault/RustyVault 子系统错误，保留原有 response status 映射和 PartialEq 行为。 | `src/vault/**`、`src/contract/vault/**` |
| `VaultError` / `VaultResult` | Vault 集成启动、初始化、key 文件、运行时 token 和 API 操作错误。 | `src/contract/vault/integration/**` |
| `CryptoError` | Vault 工具层加解密、序列化、OpenSSL 与 `RvError` 转换错误。 | `src/vault/utils/crypto.rs` |
| `SealBoxError` | SealBox 封存、解封、分片、加解密错误。 | `src/vault/utils/seal.rs` |
| `ContextError` | Cedar policy context 构建、schema、policy、validation 和 JSON 错误。 | `src/contract/policy/context.rs` |
| `SaturnContextError` | Cedar 授权请求构造和授权拒绝错误。 | `src/contract/policy/context.rs` |
| `BuckError` | Buck session/upload 业务错误，作为 `MegaError::Buck` 被 `ApiError` 映射为精确 HTTP status。 | Buck service/router |
| `GitLFSError` | Git LFS 处理错误。 | LFS router/handler |
| `StatusParseError` | Buck2 status 文本解析错误。 | `contract::api::buck2::status` |
| `DiffParseError` | Code review re-anchor 统一 diff 解析错误。 | `src/jupiter/utils/code_review_reanchor.rs` |

## 导入规则

新增和既有业务代码都必须直接从 `crate::common::errors` 导入错误类型：

```rust
use crate::common::errors::{ApiError, MegaError, RvError};
```

不要新增 `crate::api::error::*`、`crate::vault::errors::*` 或其他模块级错误 facade。`src/api`、`src/vault`、`src/contract` 只消费公共错误类型，不再承载错误定义。

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
4. 需要 Vault 语义时扩展 `RvError`，并同步 `response_status()` 和 `PartialEq`。
5. 不要在 API/router、Vault 工具、contract 或 storage 模块中新增局部 `thiserror::Error` 枚举；局部解析错误也应放入 `common::errors`。
6. 字符串型 fallback（如 `MegaError::Other` 和 `[code:xxx]`）只能用于兼容或过渡，新逻辑优先使用结构化变体。

## 授权拒绝的状态码（UN-23）

受 guard 保护的 `/api/v1/cl` 操作在授权被拒绝时返回 **403**，并在 OpenAPI 中显式声明该响应。

- **403 = 已识别主体但无权限**（授权拒绝）。
- **401 = 认证失败**（必选 extractor 拿不到会话），不得用来冒充授权拒绝——两者含义不同，客户端的处理也不同（重新登录 vs 申请权限）。

映射键为 `{method, path}`（`src/contract/policy/guard/guarded_endpoints.json`）：同一路径的不同方法可以是不同 action，例如 `GET /cl/{link}/reviewers` 是读（`viewRepo`），而 `POST`/`DELETE` 同路径是写（`editMergeRequest`）。未登记 `{method,path}` 的操作视为 unprotected，**不会**继承同路径其它方法的 action。

`POST /cl/{link}/merge-no-auth` 是已注册路由但尚未纳入映射，其入口鉴权与 403 注解归 UN-24。

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
