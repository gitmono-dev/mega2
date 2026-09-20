# mega2

[English](README.md) · 中文

**mega2** 是同组织 [Mega](https://github.com/web3infra-foundation/mega) 项目的后继引擎：在 Mega 已交付的 monorepo / Git 托管能力上继续演进，提供仅存储模式的代码托管后端。

mega2 只支持一种部署模式：trunk / storage-only。它不包含 Web UI，也不集成 megaui。如需交互式浏览仓库，以及进行相应的目录 / Tag 操作，请使用 Libra 的 `libra mega2 browser` 命令。

产品规则见 [`docs/monorepo.md`](docs/monorepo.md)。快速开始见 [`docs/quick-start.zh.md`](docs/quick-start.zh.md)。storage-only 部署见 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。本地开发与测试见 [`docs/development.md`](docs/development.md)。

## 主要功能

### Git 托管与 monorepo

- **Monorepo**：Git 公开分支只接受 `refs/heads/main`，不接受其它 heads（例如 `refs/heads/dev`）。协议校验会拒绝，Git 客户端收到 `ng <ref> …`（`trunk push rejects ref '…'; the only public branch is refs/heads/main`）。这只约束 Git receive-pack / `ls-remote` 的 heads；Agent Capture 不走这条协议，见下方 Agent Capture 条目。
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**：monorepo 的最佳实践是单一主干，而不是长期并存的多条功能分支。
  - **开源版没有 CL**：Mega2 开源版本提供的是 Monorepo 核心的存储能力，没有 Change List 部分的功能，在 Change List 的实现中需要使用多分支，所以不具备多分支的能力。
- **ImportRepo**：`[monorepo].import_dir`（默认 `/third-party`）下按普通 Git 多分支、客户端 tag 工作，这是 Mega2 的一个特性，用于开发者存储使用开源的第三方依赖库的源码，以便在本地开发时使用这些库的最新版本，其结构等同于
- **Tag**：Monorepo 禁止 `git push --tags`；创建 / 查询 / 删除只走 HTTP API。Mega2 需要配合 Libra 作为版本管理工具使用；可通过 `libra mega2 browser` 获得交互式终端界面，以浏览仓库、管理目录、Tag 等操作。
- **对象图**：元数据在 Postgres，blob 在可插拔对象存储（本地文件系统或 S3-compatible 的对象存储中）。`object_format` 支持 `sha1`（默认）与扩展的 `sha256` / `blake3`（必须配合 Libra 作为版本管理工具才能使用其特性）。

### 协议与大文件

- **Git Smart HTTP 与 SSH**：对标准 Git 客户端提供 Smart HTTP 与 SSH，覆盖 clone / fetch / pull / push。storage-only 关闭 SSH receive-pack，SSH 仍可用于只读拉取。
- **Git LFS**：把大文件从普通 Git 对象图中拆出，走标准 Git LFS（`/info/lfs` 与 `/api/v1/lfs`）。
- **FastCDC Media**：在标准 LFS 之上为大型媒体提供按内容分块的上传与复用（`--features fastcdc`）。FastCDC 和 BLAKE3 支持是 Monorepo 针对大文件和哈希安全开发的特性，需要配合 Libra 才能使用。契约见 [`docs/refactoring/fastcdc-media.md`](docs/refactoring/fastcdc-media.md)。

### 部署形态

mega2 只以 **trunk / storage-only** 形态部署：`push_policy=trunk`，推送经 `MonoWriteQueue` 直入 `main`。

产品写 API（`POST /api/v1/create-entry`、`POST /api/v1/edit/save`）与 `git push` 共用 tip 权威。根树写入全局串行，多 commit 推送按产品规则合并进 `main`。

### HTTP API

- Git 托管与 Git LFS。
- 文件和目录的读取、创建与编辑，以及 blob / tree / blame 浏览。
- Tag 的创建、查询与删除。
- OCI Distribution `/v2` 的 manifest / blob 上传与拉取（启用 `[oci].enabled=true` 时）。
- Agent Capture 的会话、事件、checkpoint 与文件操作采集（启用 `[agent_capture].enabled=true` 时）。

### 访问控制与密钥

- storage-only 写入使用配置的 `git.push_auth`：静态 push token（`token`），或显式限定网络边界的匿名模式（`none`）。读取遵循 Git 与对象存储配置。
- 内嵌 **Vault**（crates.io `libvault` + `src/contract/vault/`）：PKI / KV、SecretRef、fail-closed 引导。

### 通知与配置

- 仅支持 Webhook 通知。
- 一级配置模块：`config init` / `validate` / secret、Profile、SecretRef、受控热加载。默认文件 `config/config.toml`，可用 `--config` 或 `MEGA_CONFIG` 覆盖。GitHub 单向同步配置见 [`docs/refactoring/github-sync.md`](docs/refactoring/github-sync.md)。

## 用 Compose 快速启动

评估栈直接从 Docker Hub 拉取正式发布镜像（`genedna/mega2:latest`），不构建源码、不需要 bootstrap：

```bash
docker compose -f mega2-compose.yml up -d --wait
```

HTTP：`http://127.0.0.1:9000/`。这是仅限本机的匿名设置（`push_auth=none`，绑定 `127.0.0.1`）；token 鉴权或共享部署见 [`docs/deployment.zh.md`](docs/deployment.zh.md) 与 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。完整操作演示见 [`docs/quick-start.zh.md`](docs/quick-start.zh.md)。

需要交互式浏览时，在 Libra 工作副本中执行 `libra mega2 browser`。该命令提供仓库导航与受支持的目录 / Tag 操作的终端体验；mega2 本身不提供 Web UI。

本地开发与测试见 [`docs/development.md`](docs/development.md)。

### 停止

```bash
docker compose -f mega2-compose.yml down -v
```

## 贡献

不要直接开做大改动。顺序是：

1. **先开 Issue**，说清楚问题、动机、范围与明确不做的事。等维护者（或讨论结论）认可方向。
2. **再写计划**。从 [Plan Template](docs/plan/plan-template.md)（中文规范原文）或 [English Plan Template](docs/plan/plan-template.en.md) 复制结构，写成 `docs/plan/plan-YYYYMMDD.md`。强制章节不得删；不适用写 `N/A` 和原因。
3. **计划评审通过后再执行**。按任务卡拆分实现、补测试与文档，跑通提交门禁后再合入。

计划不等于实现：落笔时以当前 checkout 的源码、测试、配置和文档为事实基线。Issue 里达成的口头约定不能替代任务卡上的验收命令。

提交前至少：

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

细节见 [`AGENTS.md`](AGENTS.md) 与 [`docs/plan/README.md`](docs/plan/README.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [`README.md`](README.md) | English product README |
| [`docs/quick-start.zh.md`](docs/quick-start.zh.md) ([English](docs/quick-start.md)) | 快速开始：compose 栈、初始化、首次推送 |
| [`docs/user-guide.zh.md`](docs/user-guide.zh.md) ([English](docs/user-guide.md)) | 使用说明：git / LFS / HTTP API / Libra 使用 |
| [`docs/configuration.zh.md`](docs/configuration.zh.md) ([English](docs/configuration.md)) | 配置参考：加载顺序、密钥、热重载 |
| [`docs/deployment.zh.md`](docs/deployment.zh.md) ([English](docs/deployment.md)) | 部署指南：compose、二进制、生产加固 |
| [`docs/architecture.zh.md`](docs/architecture.zh.md) ([English](docs/architecture.md)) | 架构设计：模块、存储、写路径 |
| [`docs/contributing.zh.md`](docs/contributing.zh.md) ([English](docs/contributing.md)) | 贡献指南：流程、门禁、约定 |
| [`docs/monorepo.md`](docs/monorepo.md) | Monorepo 产品规则 |
| [`docs/deploy-trunk.md`](docs/deploy-trunk.md) | trunk / storage-only 部署 |
| [`docs/development.md`](docs/development.md) | 本地开发与测试 |
| [`docs/manual/authz.md`](docs/manual/authz.md) | 认证与授权运维 |
| [`docs/errors.md`](docs/errors.md) | 错误契约 |
| [`docs/refactoring/agent-capture.md`](docs/refactoring/agent-capture.md) | Agent Capture HTTP / 表 / 对象命名空间 |
| [`docs/refactoring/storage-events.md`](docs/refactoring/storage-events.md) | 提交后出站事件 |
| [`docs/plan/plan-template.en.md`](docs/plan/plan-template.en.md) | English Plan Template |
| [`docs/plan/`](docs/plan/) | 日期计划与长期路线图 |
| [`docs/refactoring/`](docs/refactoring/) | 其它模块契约与实现事实源 |
