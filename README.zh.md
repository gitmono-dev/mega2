# mega2

[English](README.md) · 中文

**mega2** 是同一组织 [Mega](https://github.com/web3infra-foundation/mega) 项目的后继版本，专注为 trunk-based monorepo 提供 Git 存储与托管能力。

mega2 只支持 **trunk / storage-only** 部署模式，不提供 Web UI。需要交互式浏览仓库或执行受支持的目录、Tag 操作时，可在 Libra 工作副本中运行 `libra mega2 browser`。

从[快速开始](docs/quick-start.zh.md)启动本地实例，再到[文档索引](docs/README.zh.md)查找用户、运维和开发指南。仓库与推送规则见[使用指南](docs/user-guide.zh.md)；贡献流程与检查要求见[贡献指南](docs/contributing.zh.md)。

## 主要功能

### Git 托管与 monorepo

- **Monorepo**：Git 公开分支只接受 `refs/heads/main`，不接受其它 heads（例如 `refs/heads/dev`）。协议校验会拒绝，Git 客户端收到 `ng <ref> …`（`trunk push rejects ref '…'; the only public branch is refs/heads/main`）。这只约束 Git receive-pack / `ls-remote` 的 heads；Agent Capture 不走这条协议，见下方 Agent Capture 条目。
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**：monorepo 的最佳实践是单一主干，而不是长期并存的多条功能分支。
- **开源版不包含 Change List**：开源版提供 monorepo 的核心存储能力，不包含依赖多分支的 Change List，因此不支持多分支。
- **ImportRepo**：`[monorepo].import_dir`（默认 `/third-party`）下的仓库遵循普通 Git 语义，可以使用多个分支和客户端 tag。这里适合存放需要在本地修改、并定期与上游同步的第三方依赖源码。
- **Tag**：Monorepo 拒绝 `git push --tags`；创建、查询和删除 Tag 都走 HTTP API。Libra 提供交互式终端界面，以及受支持的目录和 Tag 操作。
- **新路径**：新路径只能建在 `[monorepo].root_dirs` 列出的根之下；推送到根外的路径，或把从零开始的历史（例如 `git init` 新建的仓库）推送到尚不存在的路径，Git 客户端会收到以 `MONO_PATH_*` 码开头的拒绝。先用 `mega2 path provision` 开通，再 clone 和推送，详见[路径策略与首次使用](docs/user-guide.zh.md#25-monorepo-路径策略与首次使用)。
- **对象存储**：元数据保存在 Postgres，Git 对象保存在对象存储服务（本地磁盘或 S3-compatible 服务）。`object_format` 默认为 `sha1`；可选的 `sha256` 和 `blake3` 格式需要配合 [Libra](https://libra.tools) 使用。

### 协议与大文件

- **构建产物**：可将二进制文件和发布包作为 Artifact Set 存储在 `/api/v1/repos/{repo}/artifacts` 下。上传按 discovery → batch → commit 三步进行。对象存储后端支持时，客户端通过预签名 URL 直传或下载；否则由 mega2 中转。写入复用 Git push 的 `git.push_auth` 凭据，读取保持匿名。产物与 Git、LFS、OCI 对象共用存储；`[artifacts_gc]` 可在后台回收无引用的产物数据。
- **Git Smart HTTP 与 SSH**：标准 Git 客户端可通过 Smart HTTP 或 SSH 执行 clone、fetch、pull 和 push。storage-only 模式下，SSH 只开放只读 upload-pack；由于没有可配置 SSH key 的用户系统，receive-pack 已关闭。所有写入统一走 HTTP，使用同一套鉴权方式。
- **Git LFS**：遵循 Git LFS 标准，使用 git-lfs 管理的大文件使用标准 Git LFS 接口（`/info/lfs` 与 `/api/v1/lfs`）。

### OCI Distribution

mega2 内置 **OCI 容器镜像仓库**，端点为 `/v2`。启用 `[oci].enabled=true` 后，可用 Docker 等标准客户端推送和拉取镜像；manifest 与 blob 都遵循 OCI Distribution 协议。

镜像 blob 与 Git blob、LFS 对象**共用同一套对象存储**（本地文件系统或 S3-compatible 云服务），无需再单独部署、运维一套 registry；仓库鉴权复用统一的 HTTP `push_auth` 模型（token 或匿名 `none`），与 Git 推送同一套凭据和路径授权语义。

### 部署形态

mega2 只以 **trunk / storage-only** 形态部署（`push_policy=trunk`）。全局写入队列 MonoWriteQueue 会串行处理 `main` 上的写入，保持主干历史线性。

`git push` 与产品写 API（`POST /api/v1/create-entry`、`POST /api/v1/edit/save`）都会更新同一条 `main` 分支，因此通过任一接口写入的内容都能立即从另一接口读到。多 commit 推送会按产品规则压缩后写入 `main`；推送后运行 `git fetch && git reset --hard origin/main`，让本地工作副本与远端对齐。

### HTTP API

- **Git 托管与 Git LFS**：Git 客户端的 clone / fetch / push 走 Smart HTTP 协议端点（`info/refs`、`git-upload-pack`、`git-receive-pack`）；git-lfs 管理的大文件走标准 LFS 接口（`/info/lfs`、`/api/v1/lfs`）。
- **文件与目录**：无需 Git 客户端即可通过 HTTP 读写 monorepo 内容，包括查看目录树、创建 / 删除 / 移动文件或目录、在线编辑文件。blob、tree 和 blame 接口分别返回文件内容、目录结构和逐行修改记录。
- **Tag**：monorepo 禁止 Git 客户端操作 tag，创建、查询、删除统一走这组接口（只读查询不需要凭据）。
- **OCI Distribution `/v2`**：标准容器镜像仓库接口，承载 `docker push` / `docker pull` 的 manifest 与 blob 上传、拉取（启用 `[oci].enabled=true` 时挂载）。
- **Agent Capture**：采集 AI 编码 Agent 的会话、事件、checkpoint 与文件操作，用于回放和审计 Agent 的工作过程（启用 `[agent_capture].enabled=true` 时挂载）。

### 访问控制与密钥

- storage-only 写入使用配置的 `git.push_auth`：静态 push token（`token`），或显式限定网络边界的匿名模式（`none`）。读取遵循 Git 与对象存储配置。
- 内嵌 **Vault**（crates.io `libvault` + `src/contract/vault/`）：PKI / KV、SecretRef、fail-closed 引导。

### 通知与配置

- 仅支持 Webhook 通知。
- 配置命令支持初始化、校验、Profile、SecretRef 与受控热加载。默认文件为 `config/config.toml`，可用 `--config` 或 `MEGA_CONFIG` 覆盖。GitHub 单向同步的配置见 [`docs/refactoring/github-sync.md`](docs/refactoring/github-sync.md)。

## 用 Compose 快速启动

评估栈直接从 Docker Hub 拉取正式发布镜像（`genedna/mega2:latest`），不构建源码、不需要 bootstrap。按平台选择 compose 文件（两者让 artifact 预签名 URL 对宿主机可达的方式不同，见 [`docs/deployment.zh.md`](docs/deployment.zh.md)）：

```bash
# macOS + OrbStack
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait
# Linux Docker
docker compose -f linux-mega2-compose.yml up -d --wait
```

HTTP：`http://127.0.0.1:9000/`。这是仅限本机的匿名设置（`push_auth=none`，绑定 `127.0.0.1`）；token 鉴权或共享部署见 [`docs/deployment.zh.md`](docs/deployment.zh.md) 与 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。先按[快速开始](docs/quick-start.zh.md)启动并推送，再查看[进阶使用场景](docs/recipes.zh.md)中的仓库迁移、LFS、OCI 和构建产物示例。

需要交互式浏览时，在 Libra 工作副本中执行 `libra mega2 browser`。该命令提供仓库导航与受支持的目录 / Tag 操作的终端体验；mega2 本身不提供 Web UI。

本地开发与测试见 [`docs/development.md`](docs/development.md)。

### 停止

使用 `down -v` 会删除 Docker 数据卷中的数据。

```bash
docker compose -f macos-orbstack-mega2-compose.yml down -v   # 或 linux-mega2-compose.yml
```

## 贡献

大改动按以下流程推进：

1. **先开 Issue**，说明问题、动机、范围和不包含的内容；先与维护者确认方向，再开始实现。
2. **再写计划**。从[计划模板](docs/plan/plan-template.md)（中文规范原文）复制结构，写成 `docs/plan/plan-YYYYMMDD.md`。强制章节不得删；不适用写 `N/A` 和原因。
3. **计划评审通过后再执行**。按任务卡拆分实现、补测试与文档，跑通提交门禁后再合入。

计划不等于实现：落笔时以当前 checkout 的源码、测试、配置和文档为事实基线。Issue 里达成的口头约定不能替代任务卡上的验收命令。

提交代码前运行：

```bash
cargo +nightly fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
source .env.test && cargo test --all
```

细节见 [`AGENTS.md`](AGENTS.md) 与 [`docs/plan/README.md`](docs/plan/README.md)。

## 文档

| 从这里开始 | 内容 |
|---|---|
| [`docs/README.zh.md`](docs/README.zh.md) | 用户、运维和开发文档索引 |
| [`docs/quick-start.zh.md`](docs/quick-start.zh.md) | 启动本地 Compose 栈并完成首次推送 |
| [`docs/recipes.zh.md`](docs/recipes.zh.md) | 仓库迁移、LFS、OCI、产物和数据持久化示例 |
| [`docs/user-guide.zh.md`](docs/user-guide.zh.md) | Git、HTTP API、Libra 和 CLI 使用说明 |
| [`docs/development.md`](docs/development.md) | 本地开发与集成测试 |
| [`docs/contributing.zh.md`](docs/contributing.zh.md) | 贡献流程与代码约定 |

配置、部署、架构和子系统参考文档见[文档索引](docs/README.zh.md)。
