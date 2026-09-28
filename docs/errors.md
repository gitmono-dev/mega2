# 错误定义集中化方案

本文档定义 mega2 的错误类型归属、接口映射和后续扩展规则。目标是让项目自定义错误只在 `crate::common::errors` 下定义，避免 API、Vault、业务模块各自维护错误枚举。

## 目标

- 所有项目自定义错误类型统一定义在 `src/common/errors/`。
- 业务代码优先返回 `MegaError` 或领域错误类型，并通过 `From` 自动转换。
- HTTP 层统一使用 `ApiError` 做状态码和响应体映射。
- Vault 子系统继续使用 `RvError`，但它现在是 `libvault` crate 的类型（2026-08-21，`plan-20260820` VLT-02）：`common::errors` 只做 `pub use libvault::errors::RvError` 重导出，不再自带一份定义。
- 旧的 `crate::api::error`、`crate::vault::errors` 命名空间不再提供兼容 re-export。

## 当前结构

```text
src/common/errors/
├── mod.rs      # MegaError、MegaResult、BuckError、PathPolicyError、ProtocolError、GitLFSError、DiffParseError
├── api.rs      # ApiError、map_ceres_error
├── policy.rs   # ContextError、SaturnContextError
└── vault.rs    # VaultError、VaultResult（本仓定义）+ RvError（重导出 libvault::errors::RvError）
```

对外推荐导入路径：

```rust
use crate::common::errors::{
    ApiError, BuckError, ContextError, DiffParseError, GitLFSError, MegaError, MegaResult,
    PathPolicyError, ProtocolError, RvError, SaturnContextError, VaultError, VaultResult,
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
| `PathPolicyError` | Monorepo 路径创建 / 写入策略错误（稳定码 `MONO_PATH_*`），裸用或经 `MegaError::PathPolicy` 包装，由 `ApiError` 映射为 400/409 并原样输出文本。 | `ceres::pack::path_policy`；产品写、路径开通、receive-pack（plan-20260923 FU-06 / FU-07 / FU-10） |
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

## PathPolicyError：Monorepo 路径策略（plan-20260923）

`PathPolicyError`（`src/common/errors/mod.rs`）是路径「创建 / 写入」策略的领域错误，经 `MegaError::PathPolicy(#[from])` 包装；包装层用 `#[error("{0}")]`，文本原样透出，没有 `Other error:` 前缀。`NotAllowed` 与 `Invalid` 由 `ceres::pack::path_policy` 判定：创建分类走唯一的分类函数 `classify_creation_path`，产品写的 ImportRepo 命名空间守卫走 `check_write_operands`（ADR-FU-06）（规则见 [`plan/plan-20260923.md`](./plan/plan-20260923.md) ADR-FU-04；同模块的 `in_import_namespace`、`is_import_dir_ancestor`、`not_allowed` 供调用方复用，不得复制判断）；`Uninitialized` 与 `Conflict` 由调用方按树状态产生。客户端 JSON 路径入口先经 `strict_creation_path_input`（拒绝 NUL、`\`，要求原始输入已是规范路径）。

| 变体 | 码 | HTTP（`ApiError`） | 含义 |
|---|---|---|---|
| `NotAllowed { path, allowed_roots, import_dir }` | `MONO_PATH_NOT_ALLOWED` | 400 | 目标路径不在任何 `root_dirs` 之下，或位于 `import_dir` 之下（import 优先：ImportRepo 由推送创建） |
| `Uninitialized { path }` | `MONO_PATH_UNINITIALIZED` | 409 | 合法根下的路径尚未开通；消息点名 `mega2 path provision --server <url> <path>` 与 `POST /api/v1/path/provision` |
| `Invalid { path, reason }` | `MONO_PATH_INVALID` | 400 | 路径非规范（相对、`.` / `..` 段、重复或尾斜杠）、含 NUL / `\` / 其它控制字符（JSON 严格入口），或为根 `/`（分类原语的判定；产品写落点为 `/` 时按 `NotAllowed` 返回，见 ADR-FU-06） |
| `Conflict { path, component }` | `MONO_PATH_CONFLICT` | 409 | 路径上的某个组件已存在且不是目录 |

文本格式（Git 面与 API 面同一文本）：`Display` = `"<CODE>: <人读消息>"`，恒为单行——路径以带引号的转义形式出现（`Uninitialized` 开通命令里的路径只转义、不加引号），根名、原因等其余片段中的控制字符（换行、NUL 等）按 `\n` / `\0` 形式转义，保证能放进 Git `ng` 行且无法伪造多行输出。产品写经 `GitError` 返回时，`MegaError::PathPolicy` 转为 `[code:400|409] <文本>`（`PathPolicyError::http_status`），`ApiError` 据此设置状态并剥去标记；裸 `PathPolicyError` 与 `MegaError::PathPolicy` 则按类型映射且原样输出（不再从文本中解析 `[code:…]`，路径里出现该字样也不会截断消息）。三条通道的 `err_message` 都是原文。Git report-status 的 `ng` 行与 API `err_message` 都携带这段原文，客户端可按冒号前的码分支；码是公开契约，改名需 minor 版本并更新本节。`NotAllowed` 只列出允许的根（`/<name>`，排序）与 ImportRepo 目录，任何变体都不包含配置文件路径、`base_dir`、数据库 / Redis / 对象存储地址或凭据。

入口：产品写（create / edit-save / delete / move，FU-06）、路径开通 API 与 CLI（FU-07、FU-08）、trunk receive-pack 首推（FU-10）。

Git 面出现场景（trunk receive-pack，FU-10；判定见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)「创建的路径策略」）：只在推送**创建**路径时出现，即 `old_id` 为零、目标路径无 `main` 行、无墓碑且在根树中不可解析为目录。首推与原样重试得到同一文本。

- 根外路径 → `MONO_PATH_NOT_ALLOWED`，不论推送的历史。
- 允许的根下、推送的链为孤儿（历史从零开始，或原样推送另一路径的已知历史）→ `MONO_PATH_UNINITIALIZED`。
- 非规范的原始 URL 路径（如 `/project//x`）→ `MONO_PATH_INVALID`：协议层只去掉末尾的 `.git`（`contract/git_protocol/path.rs` 的 `normalize_repo_path`），规范化只发生在 ImportRepo 分派，Monorepo 分派使用原始路径（`DEFER-FU-18`）。常见客户端自己会去掉尾斜杠，不受影响。
- `MONO_PATH_CONFLICT` 不在 Git 面出现（文件路径上的创建见 `DEFER-FU-19`）。

report-status 行为 `ng refs/heads/main <文本>`；`git push` 显示为 `! [remote rejected] <src> -> main (<文本>)`，Libra 显示为 `remote rejected ref update for 'refs/heads/main': <文本>`（Libra 会截断过长的文本）。用户侧的处理步骤见[使用指南 2.5 节](./user-guide.zh.md#25-monorepo-路径策略与首次使用)。

## MegaError::OrphanChain：孤儿链拒绝（plan-20260923）

`MegaError::OrphanChain`（FU-09，ADR-FU-07）：新分支推送（`old_id` 为零）的第一父链沿 pack 走到无父根时的孤儿拒绝，首推与原样重试返回同一错误。显示文本保持历史措辞 `Other error: Can not init directory under monorepo directory!`，Git `ng` 行不变；类型化只为让调用方（FU-10 的 trunk 首推分类）经 `push_chain::is_orphan_chain_error` 精确识别，不再匹配文本。它不证明历史未被引用（已知 tip 的链走到根时同样返回，见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)）。trunk 形态下，推送到待创建路径（无 `main` 行、无墓碑、根树中不可解析）时它被映射为 `MONO_PATH_UNINITIALIZED`（FU-10），客户端看到的是路径策略码。

## ImportRepoError：ImportRepo ref 与生命周期（plan-20260923）

`ImportRepoError`（`src/common/errors/mod.rs`）是 ImportRepo 的领域错误，经 `MegaError::ImportRepo(#[from])` 包装；包装层用 `#[error("{0}")]`，文本原样透出。`Display` = `"<CODE>: <人读消息>"`，恒为单行（路径、ref 名与 id 中的控制字符按转义形式出现），同一文本进 Git `ng` 行与 API `err_message`；`ApiError` 按类型映射状态并原样输出，`GitError` 通道带 `[code:…]` 标记。完整错误域在 FU-12 一次定义，**各码在首次对用户暴露它的卡中登记于此**（[`plan/plan-20260923.md`](./plan/plan-20260923.md) ADR-FU-03 第 2 条）：

| 变体 | 码 | HTTP（`ApiError`） | 含义 |
|---|---|---|---|
| `StaleRef { ref_name, expected }` | `IMPORT_REPO_STALE_REF` | 409 | ImportRepo 推送的 ref 更新未通过服务端 CAS（ADR-FU-08 第 2 条）：`Create` 时该 ref 已存在，`Update` / `Delete` 时它已不再指向客户端声称的 `old_id`（广告之后有别的推送或 API 写入）。`fetch` 后重推即可 |
| `PathOccupied { path }` | `IMPORT_REPO_PATH_OCCUPIED` | 409 | ImportRepo 推送的挂载位置已被不属于该仓库的内容占用（ADR-FU-08 第 1 条）：该路径在 Monorepo 根树中是普通目录或文件，且没有该仓库的挂载来源记录（存量挂载按「只含 `.gitkeep` 且仓库已有分支」惰性回填，不在此列）。根树不变；换一个路径导入，或由运维清理占用的内容 |
| `Removed { path }` | `IMPORT_REPO_REMOVED` | 409 | 推送或产品 API 写（编辑保存、tag 创建 / 删除）进行期间，该 ImportRepo 已被清理（detach，ADR-FU-09 第 5 条）：被拒绝的那次写不落任何行（对象行、tag、分支 ref、`file_path` 都不写，叶子不会被重新挂载）；编辑保存分两个事务写对象行与默认分支 ref，二者之间遇 detach 时，已提交的对象行留给清扫删除。再推送一次即以新的 `repo_id` 重新导入。Git 面为 `ng` 行原文；API 面 `POST /api/v1/edit/save`、`POST /api/v1/tags` 与 `DELETE /api/v1/tags/{name}` 返回 409 |
| `PathInvalid { path, reason }` | `IMPORT_REPO_PATH_INVALID` | 400 | 路径不是合法的 ImportRepo 叶子（ADR-FU-08 第 7 条、ADR-FU-10 第 1 条）：协议分派在 TP-08 规范化之后、任何查找或注册之前，对进入 ImportRepo 分支的路径（以 `import_dir` 为组件前缀的绝对路径）拒绝 `import_dir` 本身、非规范形式与含 NUL 的路径（SSH 的相对写法如 `third-party` 照旧走 Monorepo 分支，计划 `DEFER-FU-37`）；清理 API 与运维 CLI 的严格入口（FU-20 / FU-21 起）另拒绝 `\`、`..`、重复斜杠与结尾斜杠（`reason` 说明原因；输入可规范化但不是规范形式时给出应使用的规范形式 `did you mean …`）。不写任何行、ref、对象，根树不变；HTTP 清理端点在鉴权之前返回它；运维 CLI `mega2 import-repo remove` 在连接数据库、Redis 与对象存储之前打印它（退出码 1） |
| `HasChildren { path }` | `IMPORT_REPO_HAS_CHILDREN` | 409 | 清理目标之下仍有其它已注册 ImportRepo（`repo_path LIKE escape_like(P) \|\| '/%'`，绑定参数；规范形式等于 P 的别名行不算，ADR-FU-10 第 3 条）：入口的无写预检（不入队、不写台账与审计）或 B3 锁内再检（写入队列行 `Failed`）拒绝，行、ref、对象与根树不变；文本不点名子仓；整页 64 行都是目标别名时保守视为有子仓（计划 `DEFER-FU-10`）；先清理子仓。FU-20 起经 `POST /api/v1/import-repo/remove` 暴露；也由 `mega2 import-repo remove` 打印（退出码 1） |
| `CleanupNotFound { path, cleanup_id }` | `IMPORT_REPO_CLEANUP_NOT_FOUND` | 404 | 续做请求的 `cleanup_id` 不存在或属于另一路径：两者同一文本，只回显请求自己的 path 与 id（`no cleanup "<id>" for "<path>"`），不泄漏其它路径的台账；无写、永不 detach。FU-20 起经同一端点暴露；也由 `mega2 import-repo remove --cleanup-id` 打印（退出码 1） |

出现场景（ImportRepo 推送，`import_dir` 之下；ADR-FU-08 第 1、4、5 条）：tag 命令逐 ref 独立，失败的 tag 得到以 `IMPORT_REPO_STALE_REF` 开头的 `ng` 行；同一推送内的分支命令作为一批在同一 B3 事务内应用，任一分支 CAS 失败则整批回滚、所有分支 `ng`、无一分支前进，各分支的 `ng` 行都是陈旧那条命令的 `IMPORT_REPO_STALE_REF` 原文（只含删除的分支批同样经写入队列、只写 ref，不经物化预检与挂载归属判定，FU-13 起）（`Create` 遇到已存在的 ref 时为 `IMPORT_REPO_STALE_REF: "<ref>" already exists; fetch and push again`）。挂载位置不属于该仓库时得到 `IMPORT_REPO_PATH_OCCUPIED` 原文——首推遇到普通目录或文件，以及后续推送遇到既无来源记录、又不符合存量规则的叶子（例如叶子下已有嵌套子仓的存量父挂载，见计划 `DEFER-FU-23`）。FU-13 起经写入队列的失败不再带 `attach B3 did not complete successfully: {Debug}` 包装：客户端文本按挂载路径与本次推送的分支命令还原为上述类型化错误；物化预检的拒绝保留其明细，为 `Other error: ImportRepo attach failed: <明细>`（I3 祖先 / 后代 / 目标已物化）；其余未归类的失败只返回 `Other error: ImportRepo attach failed (push_queue id <N>); retry the push`，原因只留在队列行（`push_queue.error_message`，可能含存储内部信息，不外露、不写日志）；未执行完的轮次为 `Other error: ImportRepo attach for push_queue id <N> did not complete; retry the push`，等待被放弃为 `Other error: attach wait abandoned for push_queue id <N>`，重放后 ref 仍未生效为 `Other error: ImportRepo attach replayed push_queue id <N> but the refs did not move; retry the push`（这些回落文本是 `MegaError::Other`，`ng` 行带 `Other error: ` 前缀）（v0.40.1 中经队列的分支批失败仍被 Debug 包装）。

出现场景（FU-18 起的写路径存活栅栏与叶子校验，ADR-FU-08 第 7 条、ADR-FU-09 第 2、5 条）：

- `IMPORT_REPO_REMOVED`：receive-pack 在 `unpack ok` 之后，每个 tag 各自得到 `ng refs/tags/<t> IMPORT_REPO_REMOVED: …`；分支批的每个分支得到同一原文的 `ng` 行，不论是哪一道栅栏拒绝（unpack 批、`file_path` 步骤、B3 attach，或已完成的仅删除轮次被重放）；git 客户端显示 `! [remote rejected] main -> main (IMPORT_REPO_REMOVED: …)`。产品 API：`POST /api/v1/edit/save` 在对象行写入时遇到栅栏返回 409（FU-18 起，零对象行）。FU-19 起，请求已分派给 ImportRepo 的处理器之后该仓库才被 detach 时，`POST /api/v1/edit/save` 的预读（因行已删除而失败时再检存活）与默认分支 ref 写、`POST /api/v1/tags`（annotated tag 的 `git_tag` 行与 ref 同一事务，lightweight tag 的 ref）与 `DELETE /api/v1/tags/{name}`（ref 与 `git_tag` 行同一事务）同样返回 409，body 为 `{"req_result":false,"data":null,"err_message":"IMPORT_REPO_REMOVED: …"}`，本次请求不写 `import_refs` / `git_tag` 行（编辑保存在对象批提交之后、ref 写之前遇 detach 时，已提交的对象行由清扫删除）；detach 在分派之前已提交的请求改由 Monorepo 处理器处理（`edit/save` 为 400 `MONO_PATH_NOT_ALLOWED`，见 [refactoring/directory-entry-api.md](refactoring/directory-entry-api.md) 的错误映射）。写入队列行为 `Failed`，`failure_type` 为 `AttachFailure`，`error_message` 为同一原文。
- `IMPORT_REPO_PATH_INVALID`：HTTP 的 v0/v1 `info/refs`（两种服务）、`git-receive-pack` 与 `git-upload-pack` POST、v2 的 `ls-refs` / `fetch` 返回 400，body 为 `{"req_result":false,"data":null,"err_message":"IMPORT_REPO_PATH_INVALID: …"}`，git 客户端显示 `The requested URL returned error: 400`；v2 的 `info/refs` GET 不分派，仍为 200；认证与推送权限的 401 / 403 先于它；HTTP 的 `/third-party.git` 仍为既有的 `Repository third-party.git is not supported`。`import_dir` 上的 upload-pack 由 404 `Repository not found.` 变为 400。SSH 在 exec 阶段遇到它时通道关闭、无类型化行；在接收数据阶段服务端写出未加 pkt-line 帧的 `error: Invalid Input: IMPORT_REPO_PATH_INVALID: …` 行，git 客户端只显示 `protocol error: bad line length character: erro`（既有的 SSH 错误写法）；SSH 的 `git-receive-pack '/third-party.git'` 同样被拒（此前会注册一行 `import_dir` 记录）。review 策略下的 `POST /api/v1/repo/clone` 丢弃 report-status，前者不达客户端、后者表现为 500（`DEFER-FU-29`）。

出现场景（FU-20 起的清理端点 `POST /api/v1/import-repo/remove`，ADR-FU-10）：

- 判定顺序：axum 提取器的纯文本拒绝（400 / 413 / 415 / 422，不是 `CommonResult`）→ 400 `IMPORT_REPO_PATH_INVALID`（先于鉴权，与 Git 面相反）→ 固定体 401 `authentication required` / 403 `forbidden`（不是本域的码、不回显路径；`push_auth=none` 或未配置一律 403）→ 404 `IMPORT_REPO_CLEANUP_NOT_FOUND` / 409 `IMPORT_REPO_HAS_CHILDREN`（只对已授权调用方出现；不写台账、审计、ref、对象与根树，409 由写入队列锁内再检得出时留下一行 `Failed` 队列行）。
- 例：`{"req_result":false,"data":null,"err_message":"IMPORT_REPO_HAS_CHILDREN: \"/third-party/p\" contains other ImportRepos; remove them first"}`、`{"req_result":false,"data":null,"err_message":"IMPORT_REPO_CLEANUP_NOT_FOUND: no cleanup \"12\" for \"/third-party/p\""}`。
- 500 `Internal server error` 可重试（写入队列暂停 / 硬停 / 满、detach 轮次未完成、存储错误；计划 `DEFER-FU-42`）；带 `cleanup_id` 的续做重试对重新导入安全，只带 `path`（不带 `cleanup_id`）的重试会 detach 当时存活的仓库。请求、结局语义与客户端协议见 [refactoring/directory-entry-api.md](refactoring/directory-entry-api.md) 的「ImportRepo 叶子清理」。
- 运维 CLI `mega2 import-repo remove`（FU-21 起）把本域的错误原文写到 stderr、不输出结局行，以退出码 1 结束；结局行、其它退出码与续做见[使用指南](./user-guide.zh.md#26-importrepo-生命周期)第 2.6 节。

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

## plan-20260912（storage-only 提交后出站事件）

N/A：未新增用户可见的 `MegaError` 枚举变体。`[storage_events]` 配置/启动错误复用 `MegaError::Other`（review 形态启用、缺 `installation_id`、重复 target id、非法 id、非 HTTPS URL、timeout 越界、HMAC 编码非法、未知 event 字面量、过滤集合超限、非 canonical path、agent 过滤不成对）。投影超 16 KiB 或非法 metadata 记为丢弃，不得映射回已提交写入的 HTTP 状态。

WH-11 启动 secret 绑定沿用同一错误类：target `secret_ref` 的 SecretRef 形状/命名空间（`vault://secret/config/<profile>/storage_events/targets/<id>/hmac#<field>`）错误在 `config validate` 与服务启动两处均失败；启用时 vault 解析失败（secret/字段缺失、字段非字符串）或 `hex:<even-hex>` 编码非法同样使启动失败。脱敏保证：错误文本不含 SecretRef URI（resolver 错误固定为 `vault://secret/***#***`），绝不含解密值；诊断只命名字段路径（如 `storage_events.targets[0].secret_ref`）与要求的命名空间模板。

WH-14 补齐两条同类 `MegaError::Other` 配置错误，两者在 enabled 与 disabled 形态都执行（沿用「disabled 仍做结构校验、只跳过 vault 取值」契约）：

- `[storage_events] shutdown_grace_seconds must be 0..=10` —— ADR-WH-03 要求 drain 有界；`0` 合法（立即 abort 在途任务），上界是契约。此前该字段只在 unknown-field 白名单与 reload 重启登记里出现，没有范围门。
- `[[storage_events.targets]] oci_repositories contains an invalid OCI repository name` —— 过滤项必须是 canonical distribution `remoteName`，与入站 `/v2` 同一判定（`src/common/oci_name.rs` 单一实现，大小写敏感、不折叠）。router 永不会产生的名字也永不会匹配，属于静默失效的订阅，故 fail-closed。

两条诊断都只命名字段路径与规则，不回显 target `url`、`secret_ref` 或任何 secret。
