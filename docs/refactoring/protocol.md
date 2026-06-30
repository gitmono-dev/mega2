# Git Protocol 兼容性改进计划

本文档记录 `monoengine` 当前 Git SSH/HTTP 协议实现的现状分析、主要兼容性问题、风险点和分阶段改进计划，用于提升与标准 Git 客户端的兼容性。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与其他模块的依赖**：Git Protocol 改进与 config/vault 的认证统一相关。当 config.md 阶段 2（CLI LoadMode）完成后，可统一 HTTP/SSH 的认证上下文设计。集成测试参见 **`integration.md`**。

## 事实校准（2026-06-14）

> 本文档中的代码引用已对照当前 `src/` 重新核对。当前 Git protocol 实现处于基础阶段，具有完整的功能框架但多处缺乏错误处理和兼容性完善：
>
> **2026-06-23 更新**：已完成首批 malformed input panic 止血：
> - HTTP `info/refs` 的 query 反序列化错误以及 `service` 参数缺失或非法时，不再 `unwrap()` panic，而是返回 `ProtocolError::InvalidInput`。
> - smart protocol pkt-line 读取新增 `try_read_pkt_line`，对非十六进制 header、短 header、长度小于 header、payload 不完整等输入返回 `ProtocolError::InvalidInput`，并补单元测试；upload-pack 与 receive-pack 命令解析已改用该可失败 parser。
> - upload-pack 的 `want` / `have` object id 长度和 UTF-8 解析已改为协议错误；receive-pack 命令解析会传播 pkt-line/capability 解析错误。SSH receive-pack 遇到非法 command pkt-line 时记录告警并返回 error 文本，不再在该点 panic。
>
> **2026-06-23 更新 2**：已完成 SSH exec parser 止血：
> - `exec_request` 不再用 `split(' ')` 和 `command[1]` 直接索引；新增独立 parser，支持单引号、双引号、反斜杠转义和包含空格的 repo path。
> - 只允许 `git-upload-pack`、`git-receive-pack`、`git-lfs-authenticate`、`git-lfs-transfer`；未知命令、缺 path、引号未闭合或参数数目错误会返回 channel failure 和可读错误，不再默认降级为 upload-pack。
> - repo path 只去除末尾 `.git`，不再删除路径中间的 `.git`。
>
> **2026-06-23 更新 3**：已完成 receive-pack `PACK` magic 分界止血：
> - HTTP / SSH receive-pack 不再搜索 `PACK` 字节序列，而是复用 `SmartSession::split_receive_pack_request` 按 pkt-line command list 的 flush-pkt 分割 commands 与 pack bytes。
> - 新增单元测试覆盖 capability 中出现 `PACK` 不误切分，以及缺少 flush-pkt 返回 `ProtocolError::InvalidInput`。
> - 当前实现仍是完整 body / channel 数据缓冲后再 split；更完整的 streaming pkt-line reader、delete-only push 语义仍为后续。
> - **2026-06-28 更新**：SSH per-channel state 已实现。`SshServer` 不再维护连接级 `smart_protocol` / `data_combined`，而是按 `ChannelId` 维护独立的 `GitSshChannelState`，每个 channel 拥有独立的 `SmartSession` 与 receive-pack 缓冲区。`capability advertise` 已完成首批保守收敛，后续仍需完整 truth table 覆盖。
>
> **2026-06-23 更新 4**：SSH upload-pack 初始响应已删除 `String::from_utf8(...).unwrap()`，改为直接按 bytes 写回 channel；Git 协议 payload 不再在该路径上被 UTF-8 假设约束。
>
> **2026-06-23 更新 5**：HTTP upload-pack request body 聚合不再对 body stream 错误 `unwrap()`，已与 receive-pack 统一经 `ProtocolError::InvalidInput` 返回。
>
> **2026-06-23 更新 6**：HTTP `info/refs` query 已按 smart HTTP 规范收紧为 exactly one `service=...` 参数，额外参数和重复 `service` 均返回 `ProtocolError::InvalidInput`。
>
> **2026-06-24 更新**：SSH `git-lfs-transfer` pure SSH 路径已从普通占位文本 `not implemented yet` 改为通过 SSH stderr extended-data 返回明确 unsupported 错误，并继续返回 channel failure，明确声明不支持 pure SSH LFS transfer、引导客户端使用 `git-lfs-authenticate` 的 HTTP fallback。
>
> **2026-06-24 更新 2**：SSH LFS exec parser 已收紧 `git-lfs-authenticate` / `git-lfs-transfer` 参数语义，operation 从 optional 改为必填且仅允许 `upload` / `download`；缺失或未知 operation 返回 channel failure，不再接受模糊请求。
>
> **2026-06-24 更新 3**：SSH `git-lfs-authenticate` 响应序列化已移除 `serde_json::to_vec(...).unwrap()`，改为错误传播；成功写回 hybrid LFS JSON response 后显式发送 channel success。
>
> **2026-06-24 更新 4**：capability truth table 阶段 3 收尾——为 `side-band-64k` 补充 `build_side_band_format` 专用单测（含启用/未启用两条路径）；为 `ofs-delta` 补充 advertise/parse 单测并明确其 OFS_DELTA pack 编解码由 `git-internal` crate 实现（`internal/pack/decode.rs` 处理 offset delta），monoengine 侧仅覆盖 advertise/parse；对 SHA-1 object format 落地显式策略——协议允许 SHA-1 默认时省略 `object-format`，monoengine 策略是对 SHA-1-only repo 不 advertise `object-format`，由 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。
>
> **2026-06-24 更新 5**：阶段 4 认证上下文统一收口（工作项 1/2/3/6）。新增 `SmartSession::set_authenticated_user`；SSH `auth_publickey` 成功后将 key owner username 存入 `SshServer.authenticated_user`，exec 阶段注入 `SmartSession.auth`；HTTP receive-pack 改用同一 helper。SSH push 的 commit binding 不再匿名。残余：upload-pack 匿名策略、receive-pack repo/path 级 push 权限（工作项 4/5）。
>
> **2026-06-24 更新 6**：阶段 2 delete-only push 落地（工作项 4）。新增 `SmartSession::is_delete_only_push`；`git_receive_pack_stream` 检测到全 delete command list 时跳过 `unpack_stream`/`receiver_handler`，`unpack_result` 视为 Ok，直接处理 ref 删除并返回 report-status。由 `is_delete_only_push_detects_pure_delete_vs_mixed` 锁定。
>
> **2026-06-30 更新**：LFS lock / metadata storage 路径完成 panic 止血。`LfsDbStorage` 的 object/lock CRUD 不再 unwrap SeaORM 结果，而是返回 `MegaError`；`ceres::lfs::handler` 的 lock list/create/delete 路径不再 unwrap lock JSON、limit parse 或 DB 结果，统一映射为 `GitLFSError`。`Link::new` 的 86400 秒过期时间也改为 infallible chrono 构造。
>
> **2026-06-30 更新 2**：LFS router response/body 路径完成 panic 止血。`api::router::lfs_router` 不再通过 `Response::builder().body(...).unwrap()` 构造静态 LFS JSON/stream responses，改为 `Response::new` + 静态 header；upload object request body 聚合错误也从 `unwrap()` 改为 400 错误返回。
>
> **2026-06-30 更新 3**：pack traversal count helper 完成首批 panic 止血。`RepoHandler::traverse_for_count` 与 `traverse_trees_only_for_count` 不再 unwrap tree lookup 结果，改为返回 `Result<(), MegaError>` 并由 monorepo/import pack 生成路径向上传播错误。
>
> **2026-06-30 更新 4**：pack traversal entry send 完成首批 panic 止血。`RepoHandler::traverse` 与 `traverse_trees_only` 不再 unwrap pack encoder channel send 结果，blob/tree entry send failure 会作为 storage/protocol error 向上传播。
>
> **2026-06-30 更新 5**：monorepo pack generation 的 encoder startup 与 commit entry send 完成 panic 止血。`MonoRepo::{shallow_pack, filtered_pack, incremental_pack}` 不再 unwrap `PackEncoder::encode_async` 或 commit entry channel send failure，统一映射为 `MegaError` / `GitError` 向上传播。
>
> **2026-06-30 更新 6**：import repo pack generation 的 encoder startup 与 commit entry send 完成同类 panic 止血。`ImportRepo::incremental_pack` 不再 unwrap `PackEncoder::encode_async` 或 commit entry channel send failure，统一映射为 `MegaError` / `GitError` 向上传播。

1. **HTTP 和 SSH 双协议支持已就位**。`contract::git_protocol/http.rs` 和 `contract::git_protocol/ssh.rs` 分别实现两个协议入口，共用 `SmartSession` 和 `src/ceres/protocol/smart.rs` 的 smart protocol 实现。

2. **基础 fetch/push/clone 可工作**。当前能支持标准 Git 客户端的基本 clone、fetch、push 操作，但多处使用 `unwrap()` 和缺乏边界检查。

3. **pkt-line 解析与 receive-pack 分流已完成首批止血。** `read_pkt_line` 的可失败版本已落地，HTTP/SSH receive-pack 已按 flush-pkt 分割 commands 与 pack bytes，不再搜索 `PACK` magic；delete-only push（全 delete）已支持（跳过 unpack）；残余风险是当前实现仍缓冲完整 body / channel 数据，尚未实现真正 streaming pkt-line reader。

4. **认证上下文已统一（HTTP/SSH）。** HTTP receive-pack 需要 Bearer/Basic token，upload-pack 无认证；SSH publickey 认证成功后保存 username 并传入 `SmartSession`，commit binding 绑定到 authenticated actor。receive-pack 尚未做 repo/path 级 push 权限校验。

5. **Capability advertise 已完成保守收敛与 truth table 覆盖**。receive-pack 不再 advertise 未验证的 atomic、report-status-v2、delete-refs、quiet、no-thin；upload-pack 不再 advertise 未实现的 include-tag；v2 不再 advertise 未 act-on 的 `server-option`。`side-band-64k`/`ofs-delta` 已补 advertise/parse 单测（ofs-delta pack decode 委托 `git-internal`），`object-format` 落地 SHA-1 默认策略；**（2026-06-30）真实 Git CLI 兼容性矩阵已通过 `.github/workflows/git-protocol-smoke.yml` 在 CI 中自动化执行**。

6. **SSH 多 channel 状态已隔离，并已支持 protocol v2。** `SshServer` 按 `ChannelId` 保存独立 `GitSshChannelState`；SSH client 通过 `GIT_PROTOCOL=version=2` 请求 v2 时，server 返回 v2 capability advertisement，并在 upload-pack data 阶段分发 `ls-refs` / `fetch` command。

## 当前实现状态速览表

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
|-----------|--------|-------------|
| HTTP GET /info/refs | 已实现（含 protocol v2 advertisement） | query 已要求 exactly one `service=...`；缺失、重复、非法或额外参数均返回 `ProtocolError::InvalidInput`；`Git-Protocol: version=2` 会返回 v2 capabilities；**真实 Git CLI 兼容性矩阵已通过 CI smoke gate 覆盖（2026-06-30）**。 |
| HTTP POST upload-pack | 已实现（含 shallow / v2 fetch / blob:none） | 一次性读取 request body 到内存；pkt-line 与 `want`/`have` malformed input 已返回协议错误；protocol v1 支持 `deepen`/`deepen-relative`，v2 支持 `ls-refs`、`fetch`、`deepen`、`filter blob:none`；仍不支持 streaming request parser。 |
| HTTP POST receive-pack | 已实现（delete-only 已支持） | command pkt-line malformed input 已返回协议错误；commands / pack 已按 flush-pkt 分割，不再搜索 `PACK`；delete-only push 已支持（跳过 unpack）；仍需 streaming parser 和更完整真实 Git CLI 矩阵。 |
| SSH git-upload-pack | 已实现（per-channel state + protocol v2） | exec command 已走独立 parser，支持基础 shell quoting、包含空格的路径和严格命令白名单；upload-pack 初始响应已按 bytes 发送；`SshServer` 已按 `ChannelId` 隔离 `SmartSession` 与 receive-pack 缓冲区；`GIT_PROTOCOL=version=2` 可启用 v2 `ls-refs` / `fetch`。 |
| SSH git-receive-pack | 已实现（per-channel state） | 与 HTTP 共用 flush-pkt 分割逻辑，不再搜索 `PACK`；每个 SSH channel 拥有独立的 receive-pack 缓冲区，多 channel 不再共享状态。 |
| SSH git-lfs-authenticate / transfer | 已实现 hybrid；pure SSH transfer 明确 unsupported | `git-lfs-authenticate` 支持 hybrid 模式，返回 HTTP LFS URL；`git-lfs-authenticate` / `git-lfs-transfer` 均要求 operation 为 `upload` 或 `download`；`git-lfs-transfer` 通过 stderr extended-data 返回明确 unsupported 错误 + channel failure，不再输出普通占位文本。 |
| 权限与认证 | 部分实现（认证已统一） | HTTP receive-pack 有 Bearer/Basic token 认证；SSH publickey 认证成功后保存 username 并传入 `SmartSession`，HTTP/SSH commit binding 均绑定到 authenticated actor（`set_authenticated_user`）。upload-pack 仍匿名；receive-pack 未做 repo/path 级 push 权限校验。 |
| Capability advertise | 保守收敛 + truth table 已建立 | receive-pack 仅 advertise `report-status` + common 能力；upload-pack 移除 `include-tag`；v2 移除 `server-option`。`side-band-64k`/`ofs-delta` 已覆盖 advertise/parse（ofs-delta pack decode 委托 `git-internal`）；`object-format` 落地 SHA-1 默认策略；`RepoHandler::supports_shallow_fetch`/`supports_filtered_fetch` 门控非 MonoRepo handler。**（2026-06-30）真实 Git CLI 兼容性矩阵已通过 CI smoke gate 覆盖**。 |
| 错误处理 | 首批止血 | `info/refs` service 参数、smart pkt-line malformed input、HTTP upload/receive request body stream 错误、malformed SSH exec 与 import repo handler 的 repo path/DB lookup 已改为协议错误/channel failure；SSH `data`/`handle_upload_pack`/`handle_receive_pack` 中的 `smart_protocol.unwrap()`、protocol error `.unwrap()`、`session.data().unwrap()`、`git-lfs-authenticate` response serialization `.unwrap()` 和 `auth_publickey` DB 查询 `.unwrap()` 已改为可诊断错误/best-effort 发送（2026-06-23/24）；**2026-06-28 更新**：`Repo::new` 的非 UTF-8 path/file_name `unwrap()` 已改为 `ProtocolError::InvalidInput`，`SmartSession::git_upload_pack` 中 `full_pack`/`incremental_pack` 的 `unwrap()` 已映射为协议错误；**2026-06-28 更新 2**：HTTP Git 路由已抽出 `GitProtocolPath` parser，移除内联 `.git` 替换与 404 `unwrap()`；**2026-06-28 更新 3**：`contract/git_protocol/http.rs` 中 response builder、`HeaderValue::from_str`、upload-pack sideband `read_buf` 的 `unwrap()` 已收敛；**2026-06-28 更新 4**：`contract/git_protocol/ssh.rs` 中 LFS `Duration::try_seconds(...).unwrap()` 与 `channel_eof` 的 `expect("state just found")` 已移除。Protocol 当前范围内剩余 `unwrap()` 已基本收敛；vault/legacy 路径与数据迁移工具不在本次范围。 |

## 硬约束与不可违反的原则

1. **协议正确性优先**：Git smart protocol 是二进制协议，所有 payload 必须按 bytes 处理，不能假设 UTF-8 或文本。

2. **客户端不能导致崩溃**：任何 malformed client input（非法 query、脆弱 exec、malformed pkt-line）都必须返回协议错误，不能 panic。

3. **Capability 诚实**：只 advertise 已实现且有测试的能力。未实现的能力如 atomic、report-status-v2 应先完齐或从 advertise 中移除。

4. **权限强制**：auth context 必须统一（HTTP Bearer/Basic + SSH key），并在 upload-pack/receive-pack 入口前完成。push 必须验证用户和权限。

5. **pkt-line 协议边界**：receive-pack 的 command 和 pack 分界必须由 flush-pkt 决定，不能依赖 magic bytes 搜索。

6. **兼容性声明**：已实现的现代能力（protocol v2、shallow clone、`filter blob:none`）必须明确范围；未实现的功能（tree filters、完整 promisor remote、pure SSH LFS）必须明确文档化，而不是静默失败。

## 现状 vs 目标对比

| 维度 | 当前状态 | 目标状态 | 关键差距 |
|-----|--------|--------|--------|
| 错误处理 | 多处 panic | 协议错误或断开，不能 panic | 需要 Result 化所有输入解析 |
| pkt-line 实现 | `unwrap()` panic 散落 | 定义 `PktLine` enum 和 streaming parser | 需重构 parser 返回 Result |
| receive-pack 分流 | 已按 flush-pkt 分割，仍缓冲完整 body/channel | streaming pkt-line reader + delete-only push 验证 | 中等 |
| SSH exec 解析 | 脆弱（空格、拼接错） | 严格 parser，支持引号和转义 | 需独立 parser 函数和单测 |
| SSH 多 channel | 全局 session 状态 | per-channel state dictionary | 已引入 `GitSshChannelState`（2026-06-28） |
| 认证统一 | HTTP/SSH 分离 | 统一 `ProtocolAuthContext` | 需与 config/vault auth 协同 |
| Capability 诚实 | advertise 多于实现 | 仅 advertise 已实现能力 | 需 capability truth table |
| 测试矩阵 | 无 | 真实 Git CLI smoke test | 需建立兼容性测试脚本 |

## 前置依赖矩阵（2026-06-14）

本文档与其他改进计划的依赖关系：

| Git Protocol 的工作 | 对其他模块的依赖 | 依赖类型 | 关键同步点 |
|------------|-----------|--------|---------|
| **阶段 0：兼容性基线** | general.md | 框架 | 必须遵守结构规范 |
| **阶段 1：panic 止血** | general.md | 框架 | 错误模型与 config 的 error.rs 对齐 |
| **阶段 2-3：pkt-line/capability** | general.md | 框架 | 与整体错误处理框架一致 |
| **阶段 4：auth 统一** | config.md | 前置 | 需先完成 config 阶段 2 的 CLI LoadMode |
| **阶段 5-6：SSH/LFS 完善** | integration.md | 并行 | 集成测试可同步验证改进 |

**注**：阶段 1-3 相对独立，可先完成基础止血。阶段 4 需与 config 协同，推荐在 config 阶段 2 之后。

## 风险与约束

- **二进制协议处理**：当前多处假设 UTF-8，需要完整的 bytes 化改造。如果改造不彻底，会在处理特殊 pack 数据或 binary diff 时出现隐蔽问题。

- **receive-pack 事务边界**：当前 atomic 的声称不被测试覆盖，并发 push 的锁粒度不明确。如果不完整地实现事务，可能导致数据不一致。

- **SSH 多 channel 状态**：state 在 handler 上全局保存，虽然 clone 降低共享风险，但一个 connection 内多个 channel 仍可能互相影响。

- **Capability 误导**：advertise 未实现的 capability 会让 Git 客户端进入不支持的代码路径，产生隐蔽失败。必须建立 truth table 防止此类问题。

- **兼容性测试缺失**：没有真实 Git CLI 的测试矩阵，无法防止 panic 或协议误差的回归。

## 多维评估表

| 维度 | 评估结论 |
|-----|--------|
| **合理性** | **高（8.5/10）**。当前代码框架完整，兼容性问题识别准确。阶段划分合理，从基础止血到兼容性扩展的路线清晰。 |
| **可行性** | **中高（7.5/10）**。基础 panic 止血相对容易（Result 化输入解析）；pkt-line 和 receive-pack 分流需要较深的协议理解；SSH auth 统一需与 config 协同。 |
| **完整性** | **中高（7.5/10）**。6 个阶段覆盖主要问题，且已补齐 shallow clone、protocol v2 `ls-refs` / `fetch`、`filter blob:none` 等现代 Git 基础能力。仍缺真实 Git CLI 矩阵、streaming pkt-line reader、tree filters 和完整 promisor remote 语义。 |
| **安全性** | **中（7/10）**。panic 止血直接提升安全性；auth 统一防止权限泄露。但 per-channel state 和事务边界的改进是长期工作。 |
| **可维护性** | **中（7/10）**。首批已移除 magic bytes 搜索和部分 scattered panic；SSH channel state 已改为 per-channel 字典。剩余 `unwrap()` 和 streaming parser 缺口仍是维护陷阱。改进后代码应更易维护，但短期工作量较大。 |

## 小结

Git Protocol 是 monoengine 的核心功能，但当前实现在错误处理、兼容性和细节上存在多个缺陷。建议优先完成阶段 0-1 的兼容性基线建立和 panic 止血，确保 malformed input 不导致崩溃。再依次完成阶段 2-3 的 pkt-line 和 receive-pack 改造，确保协议边界正确。阶段 4 的 auth 统一可与 config 模块协同完成，阶段 5-6 为长期改进。

## 预期收益

- **崩溃修复**：消除 panic 风险，malformed input 返回协议错误而非连接中断
- **兼容性提升**：标准 Git 客户端在各场景下（clone、fetch、push、tags、delete）更稳定可靠
- **可观测性改进**：清晰的错误模型和兼容性测试矩阵便于快速诊断和防止回归
- **维护成本降低**：代码结构改进（streaming parser、per-channel state）降低后续改造成本
- **扩展性增强**：shallow clone、protocol v2 和 `filter blob:none` 已落地，为后续 tree filters、promisor remote 和 streaming parser 继续预留设计空间

## 范围

本文覆盖：

- Git smart HTTP：`GET /info/refs?service=...`、`POST /git-upload-pack`、`POST /git-receive-pack`。
- Git SSH：`git-upload-pack '<repo>'`、`git-receive-pack '<repo>'`、`git-lfs-authenticate`。
- Git smart protocol 的 pkt-line、capability、side-band、upload-pack、receive-pack。
- Git LFS 与 SSH hybrid LFS discovery 的兼容性。
- 与标准 `git clone`、`git fetch`、`git push`、`git lfs` 客户端交互相关的认证、错误处理和测试建议。

不覆盖普通 REST API、Web UI、内部 monorepo API 或对象存储协议。

## 当前实现概览

### HTTP 入口

HTTP 服务在 `src/server/http_server.rs` 中注册 Git 协议入口：

```text
/{*path}
  -> GET  */info/refs
  -> POST */git-upload-pack
  -> POST */git-receive-pack
```

核心分发函数是 `handle_smart_protocol`：

- `GET .../info/refs` 调用 `contract::git_protocol::http::git_info_refs`。
- `POST .../git-upload-pack` 调用 `contract::git_protocol::http::git_upload_pack`。
- `POST .../git-receive-pack` 调用 `contract::git_protocol::http::git_receive_pack`。
- 其他路径返回 404 `Operation not supported`。

HTTP Git 协议处理代码位于 `src/contract/git_protocol/http.rs`。该模块负责：

- 解析 `InfoRefsParams.service`。
- 创建 `SmartSession`。
- 读取 request body。
- 调用共享 smart protocol 实现。
- 设置 Git smart HTTP 所需的 `Content-Type` 和 `Cache-Control`。
- 对 receive-pack 执行 Bearer token 或 Basic Auth token 认证。

### SSH 入口

SSH 服务入口位于 `src/server/ssh_server.rs` 和 `src/contract/git_protocol/ssh.rs`。

`src/server/ssh_server.rs` 负责：

- 加载或生成 SSH host key，并保存到 vault 的 `ssh_server_key`。
- 构造 `russh::server::Config`。
- 构造 `ProtocolApiState`。
- 运行 `SshServer`。

`src/contract/git_protocol/ssh.rs` 负责：

- 处理 `exec_request`。
- 支持 `git-upload-pack` 和 `git-receive-pack`。
- 支持 `git-lfs-authenticate` hybrid LFS discovery。
- 对 `git-lfs-transfer` 返回明确 unsupported failure。
- 使用用户上传的 SSH public key fingerprint 进行认证。

SSH 和 HTTP 最终共用 `SmartSession` 与 `src/ceres/protocol/smart.rs` 中的 smart protocol 实现。

### SmartSession 与 repo handler

`SmartSession` 定义在 `src/ceres/protocol/mod.rs`，核心字段包括：

- `repo_path`
- `service_type`：`UploadPack` 或 `ReceivePack`
- `transport_protocol`：`Http` 或 `Ssh`
- `auth`
- `capabilities`

`repo_handler_with_commands` 会根据 repo path 选择 repo handler：

- 如果 path 位于 `config.monorepo.import_dir` 下，使用 `ImportRepo`。
- 否则使用 `MonoRepo`。

`ImportRepo` 和 `MonoRepo` 都实现 `RepoHandler`，提供 pack 生成、pack 解码、ref 更新、receive-pack finalize 等能力。

### Smart protocol 实现

核心实现位于 `src/ceres/protocol/smart.rs`：

- `git_info_refs`：构造 ref advertisement。
- `git_upload_pack`：解析 `want`、`have`、`done`，返回 ACK/NAK 与 pack stream。
- `parse_receive_pack_commands`：解析 receive-pack ref update commands。
- `git_receive_pack_stream`：解码 pack stream、保存对象、更新 refs、返回 report-status。
- `build_side_band_format`：按 side-band / side-band-64k 包装 pack 数据。
- `read_pkt_line` / `add_pkt_line_string`：pkt-line 编解码辅助。

当前 advertise 的 capability：

```text
upload-pack:
  multi_ack_detailed no-done side-band-64k ofs-delta shallow agent=mega/0.1.0

receive-pack:
  report-status side-band-64k ofs-delta agent=mega/0.1.0

protocol v2 upload-pack:
  agent=mega/0.1.0 ls-refs fetch=shallow filter object-format=sha1
```

`Capability` enum 当前只解析部分 capability：

```text
multi_ack
multi_ack_detailed
no-done
side-band
side-band-64k
report-status
report-status-v2
ofs-delta
shallow
```

## 当前兼容能力

### 已支持的 HTTP smart protocol 基础链路

当前实现已经覆盖 smart HTTP 的三类核心请求：

- ref discovery：`GET /repo.git/info/refs?service=git-upload-pack`。
- fetch/clone：`POST /repo.git/git-upload-pack`。
- push：`POST /repo.git/git-receive-pack`。

HTTP response 已设置：

- `Content-Type: application/x-git-upload-pack-advertisement`
- `Content-Type: application/x-git-receive-pack-advertisement`
- `Content-Type: application/x-git-upload-pack-result`
- `Content-Type: application/x-git-receive-pack-result`
- `Cache-Control: no-cache, max-age=0, must-revalidate`

### 已支持的 SSH smart protocol 基础链路

当前 SSH 实现支持标准 Git SSH exec 命令：

```text
git-upload-pack '<repo>.git'
git-receive-pack '<repo>.git'
```

认证采用 SSH public key fingerprint 匹配数据库中的用户 SSH key。认证成功后，`git-upload-pack` 和 `git-receive-pack` 复用 `SmartSession`。

### 已支持基础 push/fetch 数据流

fetch 侧：

- 支持 `want`、`have`、`done`。
- 支持无 `have` 时 full pack。
- 支持有 `have` 时 incremental pack。
- 支持 `multi_ack_detailed` 下的 `ACK ... common` 和 `ACK ... ready`。
- 支持 side-band-64k 包装 pack data。
- 支持 `deepen` / `deepen-relative` shallow fetch，并 advertise `shallow` capability。
- 支持 protocol v2 `ls-refs` / `fetch`（HTTP 通过 `Git-Protocol: version=2`，SSH 通过 `GIT_PROTOCOL=version=2`）。
- 支持 protocol v2 partial clone `filter blob:none`；tree filter 仍未承诺完整语义。

push 侧：

- 支持 receive-pack command 解析。
- 支持 pack stream 解码。
- 支持对象保存。
- 支持 refs 更新。
- 支持 `report-status` 风格的 `unpack ok` 与 per-ref status。
- 支持 tag 和 branch 的基本更新路径。

### 已支持 Git LFS HTTP discovery 兼容路径

LFS router 同时暴露：

- `/api/v1/lfs/...`
- `/info/lfs/...`

HTTP server 还包含 `rewrite_lfs_request_uri`，用于把 repo path 下的 `/info/lfs/...` 重写到 LFS runtime router。SSH 下 `git-lfs-authenticate` 会返回 HTTP LFS URL，让 Git LFS 客户端走 hybrid 模式。

## 主要兼容性问题

### HTTP info/refs 参数校验（首批已止血）

`handle_smart_protocol` 当前通过 `parse_info_refs_params` 解析并校验 query，反序列化失败、缺失/重复/非法 `service` 或存在额外 query 参数都返回 `ProtocolError::InvalidInput`；`contract::git_protocol::http::git_info_refs` 仍保留同类防御性校验。旧实现曾直接：

```rust
let service_name = params.service.unwrap();
let service_type = service_name.parse::<ServiceType>().unwrap();
let params: InfoRefsParams = serde_urlencoded::from_str(query_str).unwrap();
```

已处理风险：

- query 反序列化错误会返回 400，不再 panic。
- 缺少 `service` 会返回 400，不再 panic。
- 重复 `service` 或额外 query 参数会返回 400。
- 非法 service 会返回 400，不再 panic。

仍待后续：

- 更完整 smart HTTP query 兼容性矩阵和真实 Git CLI 覆盖。
- 按真实 Git CLI 矩阵确认是否需要对非标准客户端提供兼容模式。

建议：

- 缺失、重复、额外参数或非法 `service` 返回 400。
- 不支持的 service 返回 403 或 400，并输出 Git 客户端可读错误。
- 对额外 query 参数做显式策略：首版已选择拒绝；如未来需要兼容非标准客户端，应先补矩阵并记录偏离规范。
- 所有解析失败都返回 `ProtocolError`，不能 panic。

### HTTP upload-pack 会一次性读取完整请求体

`git_upload_pack` 当前通过 `try_fold(BytesMut::new())` 把整个 request body 读入内存。fetch 请求通常较小，但 protocol v0 negotiation 在复杂仓库和大量 `have` 场景下仍可能较大。body stream 读取错误已统一映射为 `ProtocolError::InvalidInput`，不再 panic。

建议：

- ✅ 已增加 body size 上限（`GIT_HTTP_MAX_BODY_BYTES = 512 MiB`，`collect_body_data` 拒绝超限 body，2026-06-28）。
- 中期将 upload-pack negotiation 改为 streaming pkt-line reader，而不是一次性聚合 body。
- 为 `read_pkt_line` 增加可诊断错误，避免 malformed pkt-line panic。

### HTTP / SSH receive-pack 分割 commands 和 pack 数据

早期 `git_receive_pack` 在 chunk 中搜索字节序列 `PACK`：

```rust
if let Some(pos) = search_subsequence(&chunk, b"PACK") {
    chunk_buffer.extend_from_slice(&chunk[0..pos]);
    let commands = pack_protocol.parse_receive_pack_commands(Bytes::copy_from_slice(&chunk_buffer));
    let left_chunk_bytes = Bytes::copy_from_slice(&chunk[pos..]);
    ...
}
```

这一路径已在 2026-06-23 的首批止血中替换为 `SmartSession::split_receive_pack_request`：

- HTTP receive-pack 先聚合 request body，再按 pkt-line command list 的 flush-pkt 分割 commands 与 pack bytes。
- SSH receive-pack 对已缓冲的 channel 数据使用同一 splitter。
- capability 或 command payload 中出现 `PACK` 不再影响分割。
- 缺失 flush-pkt 会返回 `ProtocolError::InvalidInput`。

仍待后续处理：

- 将当前完整 body / channel 数据缓冲改为 streaming pkt-line reader。
- 支持纯 delete refs 的无 pack receive-pack 请求。
- 增加缺 packfile、pack magic 不合法和真实 Git CLI push/delete 矩阵。

### SSH exec command 解析过于脆弱

`exec_request` 早期使用：

```rust
let command: Vec<_> = data.split(' ').collect();
let path = command[1];
let path = path.replace(".git", "").replace('\'', "");
let service_type = ServiceType::from_str(command[0]).unwrap_or(ServiceType::UploadPack);
```

这一路径已在 2026-06-23 的首批止血中替换为独立 parser。当前 parser 已覆盖以下风险：

- 空命令或缺 path 会 panic。
- 路径包含空格会被错误切分。
- `.replace(".git", "")` 会删除路径中所有 `.git`，而不是只去掉末尾 `.git`。
- 不支持单引号、双引号和反斜杠转义。
- 非法 command 默认变成 `UploadPack`，可能导致错误行为。

仍待后续处理：

- 更完整的 shell 兼容性（如 `--`、复杂转义规则）如果标准客户端需要，再按兼容性测试补充。
- ✅ 将已解析的 exec state 与 SSH channel 绑定，避免一个 connection 内多个 channel 共享 `smart_protocol` / `data_combined`（2026-06-28 已实现）。

### SSH session 状态与 channel 绑定

`SshServer` 原在 handler 上保存连接级 `smart_protocol: Option<SmartSession>` 与 `data_combined: BytesMut`，一个 SSH connection 内多个 channel 会共享这些状态。2026-06-28 已改为：

- `SshServer` 维护 `channels: HashMap<ChannelId, GitSshChannelState>`。
- `GitSshChannelState` 每个 channel 单独保存 `SmartSession`、receive-pack buffer、service type。
- `data` 只读写当前 `ChannelId` 的状态。
- `channel_eof` 只处理对应 channel 的状态，处理完后从 map 移除。
- 关闭 channel 时清理状态。

### SSH upload-pack 可能把二进制 ACK/pack 数据当 UTF-8 发送

`handle_upload_pack` 当前：

```rust
session.data(channel, String::from_utf8(buf.to_vec()).unwrap()).unwrap();
```

`buf` 是 pkt-line bytes，虽然当前 ACK/NAK 文本大多是 UTF-8，但协议层应按 bytes 处理，不应经过 `String::from_utf8`。

建议：

- 所有 Git protocol payload 统一用 bytes 发送。
- 删除协议二进制路径上的 UTF-8 假设。
- `session.data` 调用只传 `Vec<u8>` 或 bytes，不做 String 转换。

### capability advertisement 与实际实现已对齐

历史风险：曾 advertise 一些未完整实现或解析不足的能力。当前状态（均已解决）：

- `include-tag`：已从 upload-pack advertise 移除（pack 生成未按 include-tag 语义验证）。
- `delete-refs`：已从 advertise 移除（receive-pack 纯删除无 pack 场景不稳健）。
- `atomic`：已从 advertise 移除（未实现原子 ref 更新）。
- `quiet`：已从 advertise 移除（未实现 progress 抑制语义）。
- `no-thin`：已从 advertise 移除（thin-pack 行为未明确测试）。
- `report-status-v2`：已从 advertise 移除（未实现 v2 完整语义）。
- `ofs-delta`：仍 advertise；OFS_DELTA pack 编解码由 `git-internal` crate 实现并自测（`internal/pack/decode.rs` 处理 offset delta），monoengine 侧覆盖 advertise/parse（`parse_capabilities_recognizes_ofs_delta`）。

建议：

- 建立 capability truth table：advertise、parse、act-on、test 四列。
- 没有行为支持和测试的 capability 先不要 advertise。
- 已完成首批：`atomic`、`report-status-v2`、`delete-refs`、`quiet`、`no-thin` 和 upload `include-tag` 已先从 advertise 移除；后续若补齐行为和测试再重新声明。
- ✅ 对 `object-format=sha1` 明确策略：SHA-1 为协议默认格式，SHA-1-only server 不 advertise `object-format`（协议允许 SHA-1 默认时省略；monoengine 策略对 SHA-1-only repo 不 advertise），由单测 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。

### pkt-line parser 错误模型已收敛，streaming reader 仍待后续

`src/ceres/protocol/smart.rs::try_read_pkt_line` 已定义 `PktLine` enum：
`Data(Bytes)`、`Flush`、`Delim`、`ResponseEnd`，并返回
`Result<PktLine, ProtocolError>`。Malformed input 不再通过 `unwrap` / panic
处理，而是显式返回 `ProtocolError::InvalidInput`，且失败时不消费输入
buffer。

已覆盖的边界：

- 空输入或不足 4 字节的 length header。
- 非 hex length header。
- reserved length `0003`。
- length 大于剩余 buffer。
- `0000` flush-pkt、`0001` delim-pkt、`0002` response-end-pkt。

仍待后续：

- HTTP 和 SSH 当前仍先完整缓冲 request/channel 数据，再复用该 parser 解析。
  后续需要抽出真正的 streaming pkt-line reader，避免大请求完整驻留内存。

### upload-pack negotiation 已支持 shallow / protocol v2 / blob:none

早期实现只处理 `want`、`have`、`done`。当前已补齐现代客户端常用的部分扩展：

- protocol v1 upload-pack 解析 `deepen` / `deepen-relative`，并返回 `shallow` response。
- `deepen-since` / `deepen-not` 会明确返回 `ProtocolError::InvalidInput`，不 silent misbehave。
- HTTP 和 SSH 均支持 protocol v2 capability advertisement、`ls-refs` 和 `fetch`。
- protocol v2 `fetch` 支持 `filter blob:none`，pack 只发送 commits + trees，不发送 blob objects。

仍需后续确认或扩展：

- promisor remote
- tree filters（如 `tree:<depth>`）
- `want-ref`
- 大仓库 fetch 性能

建议：

- 用真实 Git CLI 矩阵验证 `git clone --depth=1`、HTTP/SSH v2 fetch、`git clone --filter=blob:none` 的端到端行为。
- 对未实现 filter spec（例如 tree filters）继续返回明确协议错误或 fallback，不 advertise 超出实现范围的语义。
- 中期将 upload-pack negotiation 改为 streaming pkt-line reader，减少大仓库 fetch 的 request body 聚合风险。

### receive-pack 原子性与并发语义需要收敛

当前 receive-pack 在 unpack 后更新 refs 和触发 side effects。代码里已有 monorepo/import 的 finalize 事务设计，但 advertised `atomic` 表示客户端可以期待“全部 ref 更新要么都成功，要么都失败”。

风险：

- tag 更新在循环中较早持久化，branch finalize 后续失败时是否能整体回滚需要确认。
- monorepo attach、CL、conversation、build hook 等 side effects 与 ref update 的事务边界需要明确。
- 并发 push 的锁粒度和 race 行为需要测试。

建议：

- 若不能保证 Git 意义上的 atomic，先不要 advertise `atomic`。
- 建立 receive-pack transaction boundary 文档。
- 对并发 push 同一 ref、非 fast-forward、删除 ref、tag update、mixed branch/tag push 建集成测试。

### HTTP push 认证与 fetch 认证策略不一致

当前 HTTP receive-pack 需要 Bearer 或 Basic Auth token。upload-pack 没有认证检查。SSH 侧通过 public key 认证后允许 upload-pack/receive-pack。

这可能是有意设计，但需要明确策略：

- public repo 是否允许匿名 clone/fetch。
- private repo 是否需要 fetch 认证。
- push 权限是否只验证“用户存在”，还是要验证 repo/path 权限。
- SSH authenticated user 是否写入 `SmartSession.auth` 用于 commit binding。

当前 HTTP receive-pack 会设置 `authenticated_user`，但 SSH publickey 认证后没有显式把 username 注入 `SmartSession.auth.authenticated_user`。这可能导致 SSH push 的 commit binding 变成 anonymous。

建议：

- 建立统一的 `ProtocolAuthContext`。
- HTTP 和 SSH 都在进入 `SmartSession` 前完成认证与授权。
- SSH auth_publickey 成功时保存 username，并传入 receive-pack session。
- 明确 fetch 是否需要认证；如果需要，upload-pack 也要走相同 authz。

### Git LFS SSH 仅支持 hybrid 模式

SSH 中 `git-lfs-transfer` 返回明确 unsupported failure，`git-lfs-authenticate` 返回 HTTP LFS URL。这符合 Git LFS 可 fallback 的 hybrid 模式，但不是纯 SSH LFS transfer。

建议：

- 文档明确：当前支持 Git LFS hybrid SSH -> HTTP，不支持 pure SSH LFS transfer。
- `git-lfs-transfer` 应返回明确的 unsupported/failure，而不是普通文本导致客户端误判。
- `git-lfs-authenticate` 返回 body 应符合 Git LFS SSH adapter 预期，包括 href、header、expires_at，以及 upload/download operation 差异。
- LFS HTTP endpoints 应补齐认证和 repo path 绑定，避免所有 repo 共用同一 `/info/lfs` 语义造成隔离问题。

### 路由匹配过宽，容易吞掉非 Git 路径

**2026-06-28 更新**：已抽出 `GitProtocolPath` parser（`src/contract/git_protocol/path.rs`），统一处理 repo path、仅去除末尾 `.git` suffix、service endpoint 与 HTTP 方法校验，并保留 `third-party.git` root repo 禁用规则。`src/server/http_server.rs::handle_smart_protocol` 不再内联后缀判断和 404 `unwrap()`，而是直接调用 parser。剩余工作：404/400/403/405 返回策略可随认证统一进一步标准化。

历史建议（已落地）：

- 抽出 `GitProtocolPath` parser，统一处理 repo path、`.git` suffix、service endpoint。
- 路由层只做粗分发，路径语义在 parser 中测试。
- 404/400/403/405 返回策略标准化。

## 改进计划

### 阶段 0：兼容性基线测试

目标：在修改实现前建立可重复的真实 Git 客户端测试矩阵。

已新增 `scripts/git_protocol_smoke.sh` 作为第一版真实 Git CLI smoke matrix。该脚本面向已经启动的 monoengine HTTP/SSH 服务，默认覆盖只读 HTTP 场景（基础 clone/fetch、shallow clone、protocol v2 fetch、`filter blob:none`）；SSH 通过 `MONOENGINE_SSH_REPO_URL` 启用同类只读场景；HTTP/SSH branch/tag push/delete 通过 `MONOENGINE_GIT_SMOKE_PUSH=1` 显式启用；HTTP LFS push/clone/locks-list 通过 `MONOENGINE_GIT_SMOKE_LFS=1` + `MONOENGINE_GIT_SMOKE_PUSH=1` 显式启用，避免默认修改远端 refs。

**（2026-06-30）CI 自动化回归 gate 已落地**：`.github/workflows/git-protocol-smoke.yml` 会在 PR 和 main push 时自动启动 PostgreSQL/Redis 测试服务、构建 release 二进制、启动 `service http`、等待就绪后对 monorepo 根路径执行 `scripts/git_protocol_smoke.sh` 的只读 HTTP 矩阵（ls-remote/clone/fetch/protocol v2 fetch/shallow clone/blob:none）。push/delete/LFS 场景仍需手动通过 `MONOENGINE_GIT_SMOKE_PUSH=1` 启用，因为它们需要访问令牌。

**（2026-06-30）CI 自动化回归 gate 已落地**：`.github/workflows/git-protocol-smoke.yml` 在 PR 和 main push 时自动启动 PostgreSQL + Redis 测试栈、构建 release 二进制、seed mail secret、启动 `service http`，然后以 monorepo 根路径 `http://127.0.0.1:9000/` 为目标运行 `scripts/git_protocol_smoke.sh` 的只读 HTTP 矩阵（ls-remote、clone、fetch、protocol v2 fetch/ls-remote、shallow clone depth=1、blob:none partial clone）。push/delete/LFS 用例仍为手动 opt-in（`MONOENGINE_GIT_SMOKE_PUSH=1`），因为 push 需要有效的用户 access token。该 workflow 在协议路径变更（`src/ceres/protocol/**`、`src/contract/git_protocol/**`、`src/server/http_server.rs`、`scripts/git_protocol_smoke.sh`）时触发，满足阶段 0 "每个后续阶段都能复用该矩阵防止回归"的验收标准。

建议脚本或后续集成测试覆盖：

```text
HTTP:
  git ls-remote http://host/repo.git
  git clone http://host/repo.git
  git fetch
  git -c protocol.version=2 fetch
  git push（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git push --delete origin branch（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git push --tags（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git clone --depth=1
  git -c protocol.version=2 ls-remote
  git -c protocol.version=2 clone --filter=blob:none

SSH:
  git ls-remote ssh://user@host:port/repo.git
  git clone ssh://user@host:port/repo.git
  git clone --depth=1 ssh://user@host:port/repo.git
  git -c protocol.version=2 ls-remote ssh://user@host:port/repo.git
  git -c protocol.version=2 clone --filter=blob:none ssh://user@host:port/repo.git
  git fetch
  git -c protocol.version=2 fetch
  git push（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git push --delete origin branch（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git push --tags（`MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）

LFS:
  git lfs install
  git lfs track
  git push with LFS object（`MONOENGINE_GIT_SMOKE_LFS=1` + `MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git clone with LFS object（`MONOENGINE_GIT_SMOKE_LFS=1` + `MONOENGINE_GIT_SMOKE_PUSH=1` 时启用）
  git lfs locks（`MONOENGINE_GIT_SMOKE_LFS=1` + `MONOENGINE_GIT_SMOKE_PUSH=1` 时启用，只读 list）
```

每个用例记录：

- Git 客户端版本。
- transport：HTTP 或 SSH。
- repo 类型：`ImportRepo` 或 `MonoRepo`。
- auth 类型：anonymous、Bearer、Basic token、SSH key。
- 预期状态码或 SSH exit status。
- server log 中是否有 panic、敏感信息或 malformed pkt-line。

验收标准：

- 当前已支持能力有明确 passing/failing 表。
- 每个后续阶段都能复用该矩阵防止回归。

### 阶段 1：输入解析与错误模型止血

目标：消除协议入口上的 panic，让非法客户端输入返回可诊断错误。

工作项：

1. `info/refs` query 解析改为 `Result`。
2. 缺失、重复、额外参数或非法 `service` 返回 400。
3. SSH exec command 解析改为显式 parser。
4. 非法 SSH command 返回 channel failure，不默认 upload-pack。
5. `read_pkt_line` 改为返回 `Result`。
6. 所有协议二进制 payload 按 bytes 处理，不经过 UTF-8 String。

验收标准：

- malformed HTTP query 不 panic。
- malformed SSH exec 不 panic。
- malformed pkt-line 不 panic。
- Git 客户端收到明确失败，而不是连接被异常关闭。

### 阶段 2：统一 pkt-line / receive-pack streaming parser

目标：用协议边界替代 `PACK` magic 搜索。

工作项：

1. 已完成首批：receive-pack 先读取 command list 到 flush-pkt。
2. 已完成首批：flush-pkt 后剩余 bytes 作为 pack stream。
3. 后续：实现 streaming pkt-line reader，避免完整 body / channel 数据缓冲。
4. ✅ 支持无 pack 的 delete-only push：`SmartSession::is_delete_only_push` 检测全部为 delete 的 command list，`git_receive_pack_stream` 跳过 `unpack_stream`/`receiver_handler`，`unpack_result` 视为 Ok，直接进入 ref 处理与 report-status。
5. 已完成首批：HTTP 和 SSH receive-pack 共用同一 parser。

验收标准：

- 已覆盖：command payload / capability 中出现 `PACK` 不误切分。
- 待覆盖：`PACK` 跨 chunk 不影响 push（需要 streaming parser 或真实 Git CLI 矩阵）。
- ✅ `git push --delete` 可正常返回 report-status（delete-only 跳过 unpack，由 `is_delete_only_push_detects_pure_delete_vs_mixed` 锁定）。
- malformed command list 返回协议错误。

### 阶段 3：capability truth table 与 advertise 收敛

目标：只 advertise 真正支持且有测试的 capability。

工作项：

1. 建立 capability truth table。
2. 移除或补齐 `atomic`、`report-status-v2`、`quiet`、`include-tag`、`delete-refs` 等能力。
3. 明确 `ofs-delta`、`no-thin`、`side-band-64k` 的 encode/decode 测试。
4. ✅ 对 SHA-1 object format 做显式策略：SHA-1 为协议默认格式，SHA-1-only server 不 advertise `object-format`（协议允许 SHA-1 默认时省略；monoengine 策略对 SHA-1-only repo 不 advertise），由 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` 锁定。

验收标准：

- advertise 的每个 capability 都有 parse/act-on/test 证据。
- 标准 Git 客户端不会因为误导性 capability 进入未实现路径。

#### Capability Truth Table（2026-06-23 建立）

| Capability | Advertised? | Parsed? | Acted On? | Tested? | 说明 |
|---|---|---|---|---|---|
| `report-status` | ✅ receive-pack | ✅ | ✅ 生成 `unpack ok` + per-ref status | ✅ `receive_pack_advertises_only_supported_baseline_capabilities` | 基础 report-status v1 语义 |
| `side-band-64k` | ✅ both | ✅ | ✅ `build_side_band_format` | ✅ `build_side_band_format_wraps_payload_when_side_band_64k_enabled` + `build_side_band_format_passthrough_when_capability_absent` | pack data 通过 side-band 传输 |
| `ofs-delta` | ✅ both | ✅ | ✅ pack decode 委托 `git-internal`（支持 offset delta 编解码，见 `git-internal::internal::pack::decode`） | ✅ advertise/parse：`parse_capabilities_recognizes_ofs_delta` + advertise 断言（pack decode 由 `git-internal` 自测） | OFS_DELTA pack 编解码由 `git-internal` 实现并自测；monoengine 侧仅覆盖 advertise/parse |
| `multi_ack_detailed` | ✅ upload-pack | ✅ | ✅ negotiation ACK 逻辑 | ✅ `parse_capabilities` 单测 | upload-pack negotiation |
| `no-done` | ✅ upload-pack | ✅ | ✅ 与 multi_ack_detailed 联动 | ✅ negotiation 单测 | 允许在 multi_ack_detailed 下提前发 pack |
| `shallow` | ✅ upload-pack | ✅ | ✅ `deepen` / `deepen-relative` 生成 shallow pack 和 `shallow` response | ✅ capability parse/advertise 单测 + shallow traversal 单测 | protocol v1 shallow clone 基础语义 |
| `ls-refs` | ✅ protocol v2 | ✅ | ✅ 处理 `ref-prefix`、`symrefs`、`peel` | ✅ v2 capability / command parse 单测 | protocol v2 refs discovery |
| `fetch=shallow filter` | ✅ protocol v2 | ✅ | ✅ v2 fetch 支持 `want`/`have`/`done`、`deepen`、`filter blob:none`；**（2026-06-30）非 MonoRepo handler 现在对 shallow/filter 请求返回明确协议错误而非静默 fallback** | ✅ v2 command parse 单测 + pack generation gates + capability honesty 单测 | v2 fetch；`filter blob:none` 只发送 commit/tree objects；`RepoHandler::supports_shallow_fetch`/`supports_filtered_fetch` 门控 |
| `agent=mega/0.1.0` | ✅ both | ❌ | ❌ | ❌ | 信息性，不影响协议行为 |
| `atomic` | ❌ 已移除 | ✅ | ❌ | N/A | 未实现原子 ref 更新，已从 advertise 移除 |
| `report-status-v2` | ❌ 已移除 | ✅ | ❌ | N/A | 未实现 v2 语义，已从 advertise 移除 |
| `delete-refs` | ❌ 已移除 | ❌ | ❌ | N/A | delete-only push 不稳健，已从 advertise 移除 |
| `quiet` | ❌ 已移除 | ❌ | ❌ | N/A | 未实现 progress 抑制，已从 advertise 移除 |
| `no-thin` | ❌ 已移除 | ❌ | ❌ | N/A | thin-pack 行为未明确测试，已从 advertise 移除 |
| `include-tag` | ❌ 已移除 (upload) | ❌ | ❌ | N/A | pack 生成未按 include-tag 语义验证，已从 advertise 移除 |
| `server-option` | ❌ 已移除 (v2) | ✅ | ❌ | ✅ `v2_capability_advertisement_does_not_advertise_server_option` | v2 parsed capabilities 未被 inspect/act-on，advertise 会误导客户端；2026-06-30 从 v2 advertise 移除 |
| `object-format` | ❌ protocol v1；✅ protocol v2 `object-format=sha1` | ✅ v2 capability advertisement | N/A | ✅ v1 `advertised_capabilities_keep_sha1_default_and_do_not_advertise_object_format` + v2 capability 单测 | v1 保持 SHA-1 默认不 advertise；v2 capability list 显式声明 `sha1` |

残余风险：`ofs-delta` 的 pack 编解码由 `git-internal` crate 实现并自测，monoengine 侧已覆盖 advertise/parse；如需端到端 OFS_DELTA pack 矩阵可在 `git-internal` 侧补足。

### 阶段 4：认证与授权统一

目标：HTTP 和 SSH 使用同一协议认证语义。

工作项：

1. ✅ 定义协议认证上下文：`SmartSession.auth: AuthContext`（`username` + `authenticated_user: PushUserInfo`），HTTP 与 SSH 共用。
2. ✅ HTTP Bearer / Basic token 认证填充同一 context（`git_receive_pack_auth` → `SmartSession::set_authenticated_user`）。
3. ✅ SSH publickey 认证成功后保存 username（`SshServer.authenticated_user`），exec 阶段传入 `SmartSession`（`set_authenticated_user`），commit binding 不再匿名。
4. 明确 upload-pack 是否允许匿名访问。
5. receive-pack 检查 repo/path 级 push 权限。
6. ✅ commit binding 使用同一 authenticated actor（`bind_commit_to_user` 读取 `auth.authenticated_user`，HTTP/SSH 路径统一）。

验收标准：

- HTTP push 和 SSH push 都能绑定到正确用户。
- private repo fetch 策略明确且有测试。
- 未授权 push 返回 401/403 或 SSH failure，不进入 unpack。

### 阶段 5：SSH per-channel state 与 LFS hybrid 加固

目标：提高 SSH 多 channel 和 LFS 客户端兼容性。

工作项：

1. 引入 `GitSshChannelState`，按 `ChannelId` 保存状态。
2. `channel_eof` 只处理当前 channel。
3. ✅ `git-lfs-transfer` 通过 SSH stderr extended-data 返回规范 unsupported 错误，并通过 channel failure 触发客户端 fallback。
4. ✅ `git-lfs-authenticate` / `git-lfs-transfer` 已校验 upload/download operation；`git-lfs-authenticate` 仍返回同一 HTTP LFS endpoint，后续如需按 operation 拆分 header/URL 再补。
5. LFS HTTP endpoint 绑定 repo path 和认证上下文。

验收标准：

- ✅ 单 SSH connection 多 channel 不串状态（`SshServer` 已按 `ChannelId` 隔离 `GitSshChannelState`）。
- Git LFS 客户端能稳定 fallback 到 HTTP LFS。
- LFS object/lock 操作不会跨 repo 混淆。

### 阶段 6：upload-pack 兼容性扩展

目标：提高 clone/fetch 在大仓库和现代 Git 客户端下的兼容性。

工作项：

1. ✅ 实现 `deepen` / shallow clone 基础语义：upload-pack 解析 `deepen` / `deepen-relative`，`MonoRepo::shallow_pack` 做 depth-limited traversal，并返回 `shallow` pkt-lines。
2. ✅ 支持 `deepen-since`、`deepen-not` 或明确拒绝：当前明确返回 `ProtocolError::InvalidInput`。
3. ✅ 评估并实现 protocol v2 的 `ls-refs` 和 `fetch`：HTTP 通过 `Git-Protocol` header，SSH 通过 `GIT_PROTOCOL` env request。
4. ✅ 评估 partial clone filter：已实现 protocol v2 `filter blob:none`；tree filters 暂未实现完整语义，后续需按真实 Git CLI 矩阵决定是否扩展。

验收标准：

- `git clone --depth=1` 行为正确。
- 不支持的 modern feature 有明确错误或 fallback，不 silent misbehave。
- 大仓库 fetch 不需要一次性读取大请求体（仍待 streaming pkt-line reader 完成）。

## 推荐优先级

| 优先级 | 工作 | 原因 |
| --- | --- | --- |
| P0 | 建立真实 Git 客户端兼容性矩阵 | ✅ **已落地（2026-06-30）**：`scripts/git_protocol_smoke.sh` + `.github/workflows/git-protocol-smoke.yml` CI 自动化回归 gate，覆盖只读 HTTP 场景（ls-remote/clone/fetch/v2 fetch/shallow/blob:none） |
| P0 | 修复 HTTP query、SSH exec、pkt-line parser 的 panic | 非法客户端输入不能打崩服务 |
| P0 | receive-pack 用 pkt-line flush 分界替代搜索 `PACK` | 已完成首批；后续补 streaming parser 与 delete-only push 矩阵 |
| P1 | capability truth table，移除未实现 advertise | 避免误导 Git 客户端进入未实现语义 |
| P1 | SSH payload 全部按 bytes 发送 | Git 协议是二进制协议，不能假设 UTF-8 |
| P1 | 统一 HTTP/SSH auth context | push 审计、commit binding、权限检查依赖此基础 |
| P2 | SSH per-channel state | ✅ 已实现（2026-06-28）：`SshServer` 按 `ChannelId` 维护独立 `GitSshChannelState` |
| P2 | LFS hybrid response 加固 | 提升 Git LFS 客户端兼容性 |
| P3 | tree filters / promisor remote / streaming upload-pack parser | 进一步优化现代 Git 客户端和大仓库体验；shallow clone、protocol v2、`filter blob:none` 已完成基础支持 |

## 建议的文档化兼容声明

在 README 或部署文档中，当前阶段建议明确声明：

- 支持 Git smart HTTP 的基础 clone/fetch/push，以及 protocol v2 `ls-refs` / `fetch`。
- 支持 Git SSH 的基础 clone/fetch/push，以及通过 `GIT_PROTOCOL=version=2` 启用 protocol v2 `ls-refs` / `fetch`。
- 支持 Git LFS HTTP endpoints，以及 SSH `git-lfs-authenticate` hybrid 模式。
- 暂不支持 pure SSH LFS transfer。
- 支持 shallow fetch/clone 基础语义（`deepen` / `deepen-relative`）；兼容性仍需要真实 Git CLI 矩阵确认。
- 支持 protocol v2 partial clone `filter blob:none`；暂不承诺 tree filters 或完整 promisor remote 语义。
- advertise 的 capability 以实现和测试为准，未实现能力不应对外声明。

## 下一步建议

第一批 PR 建议控制在 P0：

1. 新增真实 Git CLI smoke test 脚本或集成测试说明。
2. `info/refs` query 解析返回 `Result`，非法输入返回 400。
3. SSH exec command parser 独立成函数并加单元测试。
4. `read_pkt_line` 返回 `Result` 并覆盖 malformed input。
5. 已完成首批：receive-pack 按 pkt-line flush 分界，不再搜索 `PACK`。
6. 已完成首批：SSH upload-pack 初始响应删除 UTF-8 转换，payload 按 bytes 写回 channel。

完成这些之后，再开始 capability 收敛、认证统一和 LFS 加固。
