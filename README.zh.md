# mega2

[English](README.md) · 中文

第一代 [Mega](https://github.com/web3infra-foundation/mega) 奠定了 monorepo 与 Git 托管平台的基础。**Mega2 是面向 Agent 的第二代引擎**，以 **Monorepo 引擎**和 **Agent Session Capture（Agent 会话捕获）**为两项核心能力。

Mega2 开源版以 **trunk / storage-only** 模式运行，不提供 Web UI 或 Change List。它负责服务端的 monorepo 存储与 API；交互式浏览可使用 Libra 提供的终端命令。

从[快速开始](docs/quick-start.zh.md)启动本地实例，再到[文档索引](docs/README.zh.md)查找用户、运维和开发指南。仓库与推送规则见[使用指南](docs/user-guide.zh.md)；贡献流程与检查要求见[贡献指南](docs/contributing.zh.md)。

## 面向 Agent 的推荐组合

面向 Agent 的 monorepo 开发，最佳实践是配合使用 **Mega2 + ScorpioFS + Libra**：

- **Mega2** 提供集中式 monorepo 存储与托管，并可选提供 Agent Session Capture。
- **[ScorpioFS](https://github.com/gitmono-dev/scorpiofs)** 将远程 monorepo 路径挂载为本地文件系统，供开发工具和 Agent 浏览工作区。
- **[Libra](https://libra.tools)** 提供 Agent 侧的版本控制工作流，以及连接 Mega2 的终端浏览器命令。

使用 `libra mega2 browser --server https://mega2.example.com` 浏览远程 monorepo。该终端界面一次显示一个目录层级，支持创建、删除、移动和重命名目录；Tag 面板可列出、创建和删除根 Tag，创建和删除需要写入凭据。此命令读取 Mega2 的远程目录数据，不执行 Git clone、fetch 或 push。详见[使用指南中的 Libra 章节](docs/user-guide.zh.md#6-配合-libra-使用)。

## 核心能力

### Monorepo 引擎

Mega2 为 monorepo 提供 trunk-based Git 存储与托管。普通 monorepo 路径以 `refs/heads/main` 为公开主干；Git 客户端不能推送其它分支或 Tag，受支持的 Tag 与目录操作通过 HTTP API 或 Libra 终端浏览器完成。开源版不包含依赖多分支的 Change List。

Git 元数据存于 Postgres，Git 对象存于本地或 S3-compatible 对象存储。`[monorepo].import_dir`（默认 `/third-party`）下的 ImportRepo 遵循普通 Git 语义，可使用多个分支和客户端 Tag，适合存放需要独立同步的第三方仓库。新路径开通与推送规则见[Monorepo 路径策略](docs/user-guide.zh.md#25-monorepo-路径策略与首次使用)。

### Agent Session Capture

Agent Session Capture 是独立于 Git push 的可选 HTTP 能力。在 storage-only 部署中启用 `[agent_capture].enabled=true` 并配置 ingest token 后，Mega2 可捕获 Agent 会话、事件、checkpoint、文件操作和 transcript，供后续查询、审查与审计。它不受 Git 分支或 receive-pack 规则控制；配置细节见[配置指南](docs/configuration.zh.md)。

## 其他能力

- **Git 与大文件**：通过 Git Smart HTTP 执行 clone、fetch、pull 和 push；storage-only 下 SSH 仅支持读取；同时支持 Git LFS。
- **构建产物与 OCI**：Artifact Sets 用于保存构建产物；启用 `[oci].enabled=true` 后，可通过 `/v2` 使用 OCI 容器镜像仓库。Git、LFS、Artifact 和 OCI 数据可共用对象存储。
- **HTTP API**：提供 monorepo 文件、目录、Tag 等读写接口；完整接口与行为见[使用指南](docs/user-guide.zh.md)。
- **安全与运维**：支持 push token、内嵌 Vault 与 SecretRef、Webhook 通知，以及配置校验、Profile 和受控热加载；完整设置见[配置指南](docs/configuration.zh.md)和[部署指南](docs/deployment.zh.md)。

## 用 Compose 快速启动

评估栈直接从 Docker Hub 拉取正式发布镜像（`genedna/mega2:latest`），不构建源码、不需要 bootstrap。按平台选择 compose 文件（两者让 artifact 预签名 URL 对宿主机可达的方式不同，见 [`docs/deployment.zh.md`](docs/deployment.zh.md)）：

```bash
# macOS + OrbStack
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait
# Linux Docker
docker compose -f linux-mega2-compose.yml up -d --wait
```

HTTP：`http://127.0.0.1:9000/`。这是仅限本机的匿名设置（`push_auth=none`，绑定 `127.0.0.1`）；token 鉴权或共享部署见 [`docs/deployment.zh.md`](docs/deployment.zh.md) 与 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。先按[快速开始](docs/quick-start.zh.md)启动并推送，再查看[进阶使用场景](docs/recipes.zh.md)中的仓库迁移、LFS、OCI 和构建产物示例。

本地开发与测试见[贡献指南](docs/contributing.zh.md)。

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
| [`docs/contributing.zh.md`](docs/contributing.zh.md) | 本地开发、贡献流程、代码约定与集成测试 |

配置、部署、架构和子系统参考文档见[文档索引](docs/README.zh.md)。
