# 使用说明(User Guide)

[English](user-guide.md) · 中文

本文是 mega2 的**使用者向导**,覆盖日常会接触的面:Git 客户端、HTTP API、大文件、可选服务面与 CLI。本文只做导航与最小示例;事实值(配置键、token、端口、端点清单)以各自的权威文档为准——产品规则 [`monorepo.md`](./monorepo.md)、部署运维 [`deploy-trunk.md`](./deploy-trunk.md)、配置契约 [`refactoring/config.md`](./refactoring/config.md) 与带注释样例 [`config/config.toml`](../config/config.toml)、错误模型 [`errors.md`](./errors.md)。

> **范围**:mega2 开源版只交付 **trunk / storage-only** 形态——无 Web UI、无 Change List(CL)。交互式浏览与目录 / Tag 操作使用 Libra 的 `libra mega2 browser`(见第 6 节)。

## 1. 产品形态与边界

核心规则(事实源:[`monorepo.md`](./monorepo.md)):

- **唯一公开分支 `main`**:推送 `refs/heads/main` 以外的公开 heads 在协议层被拒绝;`refs/cl/*` 属于 CL 管线,不在 storage-only 交付范围内。
- **Git 客户端禁止 tag**:`git push --tags` 及任何形式的客户端 tag 写非零退出且远端无残留;tag 的创建 / 查询 / 删除只走 HTTP API(见 4.2)。
- **ImportRepo 例外**:`[monorepo].import_dir`(默认 `/third-party`)下的仓库按普通 Git 语义工作——多分支与客户端 tag 均合法,用于托管第三方依赖源码。
- **无 CL / 无 Web UI**:storage-only 不注册 CL / issue / reviewer / OAuth user 路由,OpenAPI 如实为空。
- **写入权威统一**:`git push` 与产品 API 写共用 `MonoWriteQueue` 推进 path tip(设计见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md));根树写入全局串行。

## 2. Git 客户端操作

### 2.1 Smart HTTP:clone / fetch / push

Git smart HTTP(`info/refs`、`git-upload-pack`、`git-receive-pack`)挂在仓库路径下,支持子路径 clone(本地 compose 栈端口见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节):

```bash
git clone http://127.0.0.1:9000/project
git fetch && git reset --hard origin/main   # trunk 推送后的客户端对齐动作
```

推送落地规则(trunk 形态):

- 只允许推 `refs/heads/main`;其它 heads 与 tag 被拒绝。
- N = 1(单 commit)推送原样落地;N > 1 的链式推送由服务端 squash 成一个 commit 推进 `main`,之后必须 `git fetch && git reset --hard origin/main` 对齐,否则下一次推送被 non-fast-forward 拒绝(N 分流细则见 [`monorepo.md`](./monorepo.md) 第 9 节;Agent 工作流备忘见 [`deploy-trunk.md`](./deploy-trunk.md) 第 9 节)。
- 子路径推送是常态;根路径 `/` 的 push 不是该形态的假设。

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

`POST /api/v1/create-entry`、`POST /api/v1/delete-entry`、`POST /api/v1/move-entry`、`POST /api/v1/edit/save`。对象写入存储后经 MonoWriteQueue 前进 path tip,与 `git push` 同 tip 权威;成功响应的 `cl_link` 为 `null`(不创建 CL),写后同栈 `git clone` / `git pull` 即可读到。请求字段与鉴权契约见 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

### 4.2 Tags API

Git 客户端 tag 被禁后的唯一入口:

| 操作 | HTTP |
|---|---|
| 创建 | `POST /api/v1/tags` |
| 列表 | `GET /api/v1/tags/list`(GET-only;`POST` 回 405) |
| 查询 | `GET /api/v1/tags/{name}` |
| 删除 | `DELETE /api/v1/tags/{name}` |

create / delete 经 `git.push_auth` 鉴权;list / get 不要求 Authorization。path 选择器与鉴权细节见 [`monorepo.md`](./monorepo.md) 第 2 节和 [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md)。

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

全局参数:`--config <PATH>`(env `MEGA_CONFIG`)与 `--profile <NAME>`(env `MEGA_PROFILE`,加载同目录的 `config.<profile>.toml`)。配置加载顺序、环境变量覆盖(`MEGA_<SECTION>__<KEY>`)、未知字段拒绝与热重载白名单见 [`refactoring/config.md`](./refactoring/config.md)。

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

完整 flag 列表以 `--help` 与 `src/commands/` 为准。compose bootstrap 与 smoke 命令样例见 [`deploy-trunk.md`](./deploy-trunk.md) 第 8 节;本地开发与测试入口见 [`development.md`](./development.md)。

## 8. 相关文档

| 主题 | 文档 |
|---|---|
| 本套文档(快速开始 / 配置 / 部署 / 架构 / 贡献指南) | [`quick-start.zh.md`](./quick-start.zh.md) · [`configuration.zh.md`](./configuration.zh.md) · [`deployment.zh.md`](./deployment.zh.md) · [`architecture.zh.md`](./architecture.zh.md) · [`contributing.zh.md`](./contributing.zh.md) |
| 产品规则(单分支、tag 禁令、ImportRepo、trunk 不变式) | [`monorepo.md`](./monorepo.md) |
| trunk / storage-only 部署与 compose 栈 | [`deploy-trunk.md`](./deploy-trunk.md) |
| 配置键与环境变量 | [`config/config.toml`](../config/config.toml)(注释样例)、[`refactoring/config.md`](./refactoring/config.md) |
| Git 协议兼容性 | [`refactoring/protocol.md`](./refactoring/protocol.md) |
| trunk 推送设计与 MonoWriteQueue | [`refactoring/trunk-push.md`](./refactoring/trunk-push.md) |
| 目录 / 文件写与 tag 契约 | [`refactoring/directory-entry-api.md`](./refactoring/directory-entry-api.md) |
| OCI registry | [`refactoring/oci.md`](./refactoring/oci.md) |
| Agent Capture | [`refactoring/agent-capture.md`](./refactoring/agent-capture.md) |
| 错误模型 | [`errors.md`](./errors.md) |
| Monorepo 初始化产物 | [`manual/monorepo-init.md`](./manual/monorepo-init.md) |
| 认证与授权边界(review 形态) | [`manual/authz.md`](./manual/authz.md) |
| 本地开发与测试 | [`development.md`](./development.md) |
