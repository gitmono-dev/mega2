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
- **ImportRepo**：`[monorepo].import_dir`（默认 `/third-party`）下按普通 Git 多分支、客户端 tag 工作，其结构等同于普通 Git 仓库。这是 Mega2 的一个特性，建议开发者把使用的开源第三方依赖库的源码存储在这里：开发过程中可以直接修改这些源码，再让 Agent 维护与上游的合并（持续跟进上游更新）。
- **Tag**：Monorepo 禁止 `git push --tags`；创建 / 查询 / 删除只走 HTTP API。Mega2 需要配合 Libra 作为版本管理工具使用；可通过 `libra mega2 browser` 获得交互式终端界面，以浏览仓库、管理目录、操作 Tag 等。
- **对象存储**：元数据在 Postgres，blob 在对象存储服务中（本地文件系统或 S3-compatible 的云服务中）。`object_format` 支持 `sha1`（默认），可选使用 `sha256` / `blake3`，必须配合 [Libra](https://libra.tools) 作为版本管理工具才能使用。

### 协议与大文件

- **Artifacts 仓库**：mega2 可以作为构建产物仓库，存放编译产物、发布包等二进制文件。产物按仓库组织为 Artifact Set，通过 `/api/v1/repos/{repo}/artifacts` 协议接口上传（discovery → batch → commit 三步）与下载；后端支持时上传与下载均通过预签名 URL 直连对象存储，否则由服务器中转。写操作与 Git push 共用同一套 `git.push_auth` token 门控，读操作保持匿名。产物 blob 与 Git blob、LFS、OCI 镜像共用同一套对象存储，并可选配 `[artifacts_gc]` 后台回收无引用的产物对象。
- **Git Smart HTTP 与 SSH**：对标准 Git 客户端提供 Smart HTTP 与 SSH，覆盖 clone / fetch / pull / push。storage-only 形态下 SSH 只保留只读拉取（clone / fetch / pull），receive-pack 关闭：该形态没有用户系统，无法为用户配置 SSH key 这类按人鉴权的方式，因此推送统一走 HTTP（token 或匿名 `none`）是最佳选择，写鉴权只需维护一套。
- **Git LFS**：遵循 Git LFS 标准，使用 git-lfs 管理的大文件使用标准 Git LFS 接口（`/info/lfs` 与 `/api/v1/lfs`）。

### OCI Distribution

mega2 内置标准 **OCI 容器镜像仓库**（`/v2` 协议端点），启用 `[oci].enabled=true` 后，`docker push` / `docker pull` 等标准客户端可直接推送、拉取镜像，manifest 与 blob 都走 OCI Distribution 标准接口。

镜像 blob 与 Git blob、LFS 对象**共用同一套对象存储**（本地文件系统或 S3-compatible 云服务），无需再单独部署、运维一套 registry；仓库鉴权复用统一的 HTTP `push_auth` 模型（token 或匿名 `none`），与 Git 推送同一套凭据和路径授权语义。

### 部署形态

mega2 只以 **trunk / storage-only** 形态部署：`push_policy=trunk`，所有推送经一个全局串行的写队列依次合并进 `main`——同一时刻只有一笔写入落地，保证主干历史线性可追溯（该机制内部称为 MonoWriteQueue）。

产品写 API（`POST /api/v1/create-entry`、`POST /api/v1/edit/save`）与 `git push` 写入的是同一条 `main` 分支：无论用哪种方式，新提交都追加在同一个分支顶端，写完后用另一种方式立刻就能读到。对仓库根目录的写入同一时刻只落地一笔，不会互相覆盖；一次推送携带多个 commit 时会按产品规则压缩合并进 `main`，推送后执行 `git fetch && git reset --hard origin/main` 即可与远端对齐。

### HTTP API

- **Git 托管与 Git LFS**：Git 客户端的 clone / fetch / push 走 Smart HTTP 协议端点（`info/refs`、`git-upload-pack`、`git-receive-pack`）；git-lfs 管理的大文件走标准 LFS 接口（`/info/lfs`、`/api/v1/lfs`）。
- **文件与目录**：不依赖 Git 客户端，直接用 HTTP 读写 monorepo 内容——读取目录树、创建 / 删除 / 移动文件和目录、在线编辑并保存；blob / tree / blame 接口分别用于查看文件内容、目录结构和逐行修改追溯。
- **Tag**：monorepo 禁止 Git 客户端操作 tag，创建、查询、删除统一走这组接口（只读查询不需要凭据）。
- **OCI Distribution `/v2`**：标准容器镜像仓库接口，承载 `docker push` / `docker pull` 的 manifest 与 blob 上传、拉取（启用 `[oci].enabled=true` 时挂载）。
- **Agent Capture**：采集 AI 编码 Agent 的会话、事件、checkpoint 与文件操作，用于回放和审计 Agent 的工作过程（启用 `[agent_capture].enabled=true` 时挂载）。

### 访问控制与密钥

- storage-only 写入使用配置的 `git.push_auth`：静态 push token（`token`），或显式限定网络边界的匿名模式（`none`）。读取遵循 Git 与对象存储配置。
- 内嵌 **Vault**（crates.io `libvault` + `src/contract/vault/`）：PKI / KV、SecretRef、fail-closed 引导。

### 通知与配置

- 仅支持 Webhook 通知。
- 一级配置模块：`config init` / `validate` / secret、Profile、SecretRef、受控热加载。默认文件 `config/config.toml`，可用 `--config` 或 `MEGA_CONFIG` 覆盖。GitHub 单向同步配置见 [`docs/refactoring/github-sync.md`](docs/refactoring/github-sync.md)。

## 用 Compose 快速启动

评估栈直接从 Docker Hub 拉取正式发布镜像（`genedna/mega2:latest`），不构建源码、不需要 bootstrap。按平台选择 compose 文件（两者让 artifact 预签名 URL 对宿主机可达的方式不同，见 [`docs/deployment.zh.md`](docs/deployment.zh.md)）：

```bash
# macOS + OrbStack
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait
# Linux Docker
docker compose -f linux-mega2-compose.yml up -d --wait
```

HTTP：`http://127.0.0.1:9000/`。这是仅限本机的匿名设置（`push_auth=none`，绑定 `127.0.0.1`）；token 鉴权或共享部署见 [`docs/deployment.zh.md`](docs/deployment.zh.md) 与 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。完整操作演示见 [`docs/quick-start.zh.md`](docs/quick-start.zh.md)。

需要交互式浏览时，在 Libra 工作副本中执行 `libra mega2 browser`。该命令提供仓库导航与受支持的目录 / Tag 操作的终端体验；mega2 本身不提供 Web UI。

本地开发与测试见 [`docs/development.md`](docs/development.md)。

### 停止

```bash
docker compose -f macos-orbstack-mega2-compose.yml down -v   # 或 linux-mega2-compose.yml
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
