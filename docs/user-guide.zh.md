# 使用指南

[English](user-guide.md) · 中文

本指南介绍 mega2 的日常使用方式，包括 Git 客户端、HTTP API、大文件、可选服务和 CLI。部署步骤见[部署指南](./deployment.zh.md)；配置键以带注释的 [`config.toml`](../config/config.toml) 为准；错误类型与状态码见[错误模型](./errors.md)。

> **范围：**mega2 开源版只支持 **trunk / storage-only** 部署模式，不提供 Web UI 或 Change List（CL）。交互式浏览和目录、Tag 操作请使用 Libra 的 `libra mega2 browser`（见第 6 节）。

## 1. 产品形态与边界

仓库行为取决于你使用的路径：

- **Monorepo 路径**（根路径及 `import_dir` 之外的路径）只公开 `main` 分支。Git 客户端推送其它分支或 Tag 都会被拒绝。请通过 HTTP API（见第 4.2 节）或 Libra 命令 `libra mega2 browser`（见第 6 节）创建、查看和删除 Tag。
- **ImportRepo 路径**位于 `[monorepo].import_dir`（默认 `/third-party`）下，遵循常规 Git 语义，允许多个分支和 Git 客户端 Tag 操作。迁移或托管第三方仓库时可使用这类路径。
- 随仓库提供的 storage-only 服务不提供 Web UI，也不包含 Change List（CL）。其 OpenAPI 文档不包含 CL、issue、reviewer 和 user 路由。
- Git push 与产品 API 写入共用写入队列和路径 tip 权威。写入流程见[架构设计](./architecture.zh.md)。

## 2. Git 客户端操作

### 2.1 Smart HTTP:clone / fetch / push

Git Smart HTTP（`info/refs`、`git-upload-pack`、`git-receive-pack`）挂载在仓库路径下，支持克隆子路径。本地 Compose 栈的端口见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节：

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # trunk 推送后的客户端对齐动作
```

trunk 模式下的推送规则：

- 只推送到 `refs/heads/main`；其它分支和 Git 客户端 Tag 写入都会被拒绝。
- 单 commit 推送会原样写入。包含多个 commit 的推送会由服务端 squash 成一个 commit 写入 `main`。推送后运行 `git fetch && git reset --hard origin/main` 与远端对齐，否则下次 push 可能因 non-fast-forward 被拒绝。
- 推送到仓库子路径；storage-only 模式不支持从根路径 `/` 推送。

协议兼容性与 smoke 矩阵见 [`refactoring/protocol.md`](./refactoring/protocol.md)。

### 2.2 SSH:只读 fetch

storage-only **不暴露 SSH receive-pack**(`git.ssh_receive_pack=false` 是强制配置,省略会拒绝启动);SSH 仅用于 clone / fetch / pull。`anonymous_access` 与 `push_auth` 组合下的认证形态见 [`deploy-trunk.md`](./deploy-trunk.md) 第 4 节。

### 2.3 推送鉴权:token 或 none

写面(Git receive-pack、LFS 批/锁写、产品 API 写)共用 `git.push_auth`:

- `token`:HTTP Basic 只取 password(= token 密文),忽略 username;`paths` 前缀按组件边界授权。
- `none`:无凭据写,仅限受控网络(回环 / Unix socket / 内网前置)。

`[[git.push_tokens]]` 配置、凭据注入方式与风险提示见 [`deploy-trunk.md`](./deploy-trunk.md) 第 3 节;本文不复制 token 值。

### 2.4 对象格式

`[monorepo].object_format` 默认 `sha1`(标准 Git)。`sha256` 与 `blake3` 是 **git-internal / Libra 扩展**,不宣称与标准 Git 客户端互通,需配合 Libra 使用;口径见 [`refactoring/protocol.md`](./refactoring/protocol.md) 与 [`refactoring/config.md`](./refactoring/config.md)。

## 3. 大文件:LFS

- **Git LFS(标准)**:端点 `/info/lfs` 与 `/api/v1/lfs`,stock `git-lfs` 客户端直接可用;LFS 写授权与 Git receive-pack 共用 `git.push_auth`(矩阵见 [`deploy-trunk.md`](./deploy-trunk.md) 第 6 节)。

## 4. HTTP API 使用

Base URL 为 HTTP 服务监听地址。写接口经 `git.push_auth` 鉴权,无凭据 / 坏 token 回 401,path 越权回 403(同 Git 写面)。

### 4.1 产品写(目录与文件)

`POST /api/v1/create-entry`、`POST /api/v1/delete-entry`、`POST /api/v1/move-entry` 和 `POST /api/v1/edit/save` 可创建、删除、移动目录与文件，写入经过与 `git push` 共用的 MonoWriteQueue 和 path tip。成功响应中的 `cl_link` 为 `null`（不会创建 CL）；写入提交后，同栈的 `git clone` 或 `git pull` 可立即读到内容。依赖路径索引的浏览结果可能稍后才更新。请求字段与鉴权契约见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

### 4.2 Tags API

Git 客户端 tag 被禁后的唯一入口:

| 操作 | HTTP |
|---|---|
| 创建 | `POST /api/v1/tags` |
| 列表 | `GET /api/v1/tags/list`(GET-only;`POST` 回 405) |
| 查询 | `GET /api/v1/tags/{name}` |
| 删除 | `DELETE /api/v1/tags/{name}` |

创建和删除请求通过 `git.push_auth` 鉴权；列表与查询请求无需 Authorization。请求字段、路径选择器和授权规则见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

### 4.3 只读浏览

`GET /api/v1/status`、`GET /api/v1/file/blob/{object_id}`、`GET /api/v1/file/tree` 及 blob / tree / blame preview 读路径保留;读授权跟随 `git.anonymous_access`。完整路径清单以运行时 OpenAPI 为准(见 4.4),本文不复制端点表。

### 4.4 OpenAPI 与 Swagger UI

- 机器可读契约:`GET /api/openapi.json`(storage-only 下如实不含 CL / issue / user 路由)。
- 交互式文档:`/swagger-ui`。

### 4.5 错误契约

错误类型归属与 HTTP 状态映射集中在 `crate::common::errors`,规则见 [`errors.md`](./errors.md)。

## 5. 可选服务面

两个面均为**双重门控**(storage-only 形态 + 各自开关),缺一则整面不存在(裸 404);在非 storage-only 形态下打开开关会拒绝启动。

- **OCI Distribution `/v2`**:storage-only 且 `[oci].enabled=true` 时挂载,可作容器镜像仓库(`docker login` 复用 push token,无独立 token 服务)。架构与端点事实源见 [`refactoring/oci.md`](./refactoring/oci.md),启用步骤见 [`deploy-trunk.md`](./deploy-trunk.md) 第 10 节。
- **Agent Capture `/api/v1/agent-capture`**:storage-only 且 `[agent_capture].enabled=true` 时挂载,捕获 agent 会话 / 事件 / checkpoint / 文件操作;另有独立的 `[[agent_capture.ingest_tokens]]` 认证面。配置与配额事实源见 [`refactoring/agent-capture.md`](./refactoring/agent-capture.md)。

## 6. 配合 Libra 使用

mega2 自身不提供交互界面。日常浏览与目录 / Tag 操作在 Libra 工作副本中执行:

```bash
libra mega2 browser
```

该 TUI 消费第 4 节的 HTTP 面(tree 读、create / delete / move-entry、tags)。sha256 / blake3 对象格式同样只在 Libra 客户端下可用(见 2.4)。

## 7. CLI 速查

全局参数为 `--config <PATH>`（环境变量 `MEGA_CONFIG`）和 `--profile <NAME>`（环境变量 `MEGA_PROFILE`，加载同目录的 `config.<profile>.toml`）。配置加载顺序、环境变量覆盖、未知字段拒绝与热加载白名单见 [`refactoring/config.md`](./refactoring/config.md)。对于新建数据库，运行 `mega2 service init --yes` 会根据 `root_dirs` 创建初始 Monorepo，然后退出，不会启动监听；目录默认值见带注释的 [`config/config.toml`](../config/config.toml)。

| 命令 | 用途 |
|---|---|
| `mega2 service init --yes` | 初始化空 Monorepo 并退出(不启动监听);空卷 bootstrap 的固定动作 |
| `mega2 service http --host 0.0.0.0 -p 8000` | 启动 HTTP 面(Git smart HTTP + LFS + `/api/v1` + 可选 OCI / Agent Capture);与 Dockerfile 默认 CMD 一致 |
| `mega2 service ssh [--ssh-port 2222]` | 启动 SSH 面(storage-only 下仅 upload-pack);standalone 形态要求 `cedar.enforcement=off`,否则改用 `multi` |
| `mega2 service multi http ssh` | 单进程同时启动 HTTP + SSH(共享授权快照) |
| `mega2 config validate [--resolve-secrets] [--show-sources]` | 启动前校验配置;自动化加 `--deny-warnings` |
| `mega2 config init [-o PATH] [--force]` | 生成安全的起始配置文件 |
| `mega2 config secret ref/set/check/rotate` | 管理 vault 承载的配置密文(SecretRef) |
| `mega2 config vault backup/restore/rekey/reset` | vault core key 运维(后三者破坏性,需 `--force`) |
| `mega2 debug storage-smoke [--key ...]` | 隐藏命令:对象存储读 / 写 / 删冒烟,不出现在顶层帮助 |

`authz-audit` 子命令面向 review 形态的授权审计;storage-only 下 `cedar.enforcement` 恒为 `off`,日常用不到(review 形态口径见 [`manual/authz.md`](./manual/authz.md))。

完整 flag 列表见 `--help` 与 `src/commands/`。Compose 启动和 smoke 命令示例见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节；本地开发与测试入口见 [`development.md`](./development.md)。

## 8. 相关文档

| 主题 | 文档 |
|---|---|
| 使用与开发指南(快速开始 / 进阶场景 / 配置 / 部署 / 架构 / 贡献) | [`quick-start.zh.md`](./quick-start.zh.md) · [`recipes.zh.md`](./recipes.zh.md) · [`configuration.zh.md`](./configuration.zh.md) · [`deployment.zh.md`](./deployment.zh.md) · [`architecture.zh.md`](./architecture.zh.md) · [`contributing.zh.md`](./contributing.zh.md) |
| 仓库路径、分支、Tag 与推送规则 | 本指南第 1–2 节 |
| 初始化目录布局与配置 | [`manual/monorepo-init.zh.md`](./manual/monorepo-init.zh.md) |
| trunk / storage-only 部署与 Compose 栈 | [`deployment.zh.md`](./deployment.zh.md) · [`deploy-trunk.md`](./deploy-trunk.md) |
| 配置键与环境变量 | [`config/config.toml`](../config/config.toml)(注释样例)、[`refactoring/config.md`](./refactoring/config.md) |
| Git 协议兼容性 | [`refactoring/protocol.md`](./refactoring/protocol.md) |
| trunk 推送设计与 MonoWriteQueue | [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) |
| 目录 / 文件写与 tag 契约 | [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) |
| OCI registry | [`refactoring/oci.md`](./refactoring/oci.md) |
| Agent Capture | [`refactoring/agent-capture.md`](./refactoring/agent-capture.md) |
| 错误模型 | [`errors.md`](./errors.md) |
| 认证与授权边界(review 形态) | [`manual/authz.md`](./manual/authz.md) |
| 本地开发与测试 | [`development.md`](./development.md) · [`contributing.zh.md`](./contributing.zh.md) |
