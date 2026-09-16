# mega2

[English](README.md) · 中文

**mega2** 是同组织 [Mega](https://github.com/web3infra-foundation/mega) 项目的后继引擎：在 Mega 已交付的 monorepo / Git 托管能力上继续演进，提供可独立部署的代码托管与服务后端。

产品规则见 [`docs/monorepo.md`](docs/monorepo.md)。Trunk / storage-only 部署见 [`docs/deploy-trunk.md`](docs/deploy-trunk.md)。本地开发与测试见 [`docs/development.md`](docs/development.md)。

## 主要功能

### Git 托管与 monorepo

- **Monorepo**：Git 公开分支只接受 `refs/heads/main`，不接受其它 heads（例如 `refs/heads/dev`）。协议校验会拒绝，Git 客户端收到 `ng <ref> …`（`trunk push rejects ref '…'; the only public branch is refs/heads/main`）。这只约束 Git receive-pack / `ls-remote` 的 heads；Agent Capture 不走这条协议，[见下文](#agent-capture)。
  - **[Trunk-based development](https://trunkbaseddevelopment.com)**：monorepo 的最佳实践是单一主干，而不是长期并存的多条功能分支。
  - **开源版没有 CL**：Mega2 开源版本提供的是 Monorepo 核心的存储能力，没有 Change List 部分的功能，在 Change List 的实现中需要使用多分支，所以不具备多分支的能力。
- **ImportRepo**：`[monorepo].import_dir`（默认 `/third-party`）下按普通 Git 多分支、客户端 tag 工作，这是 Mega2 的一个特性，用于开发者存储使用开源的第三方依赖库的源码，以便在本地开发时使用这些库的最新版本，其结构等同于
- **Tag**：Monorepo 禁止 `git push --tags`；创建 / 查询 / 删除只走 HTTP API。由于 Mega2 是只有服务的开源项目，如果配合 Libra 作为版本管理工具才能使用，可以使用 libra mega2 的子命令用于管理目录和 Tag 等。
- **对象图**：元数据在 Postgres，blob 在可插拔对象存储（本地文件系统或 S3-compatible 的对象存储中）。`object_format` 支持 `sha1`（默认）与扩展的 `sha256` / `blake3`（必须配合 Libra 作为版本管理工具才能使用其特性）。

### 协议与大文件

- **Git Smart HTTP 与 SSH**：对标准 Git 客户端提供 Smart HTTP 与 SSH，覆盖 clone / fetch / pull / push。storage-only 关闭 SSH receive-pack，SSH 仍可用于只读拉取。
- **Git LFS**：把大文件从普通 Git 对象图中拆出，走标准 Git LFS（`/info/lfs` 与 `/api/v1/lfs`）。
- **FastCDC Media**：在标准 LFS 之上为大型媒体提供按内容分块的上传与复用（`--features fastcdc`）。FastCDC 和 BLAKE3 支持是 Monorepo 针对大文件和哈希安全开发的特性，需要配合 Libra 才能使用。契约见 [`docs/refactoring/fastcdc-media.md`](docs/refactoring/fastcdc-media.md)。

### 两种部署形态

| 形态 | 适用 | 行为 |
|---|---|---|
| **review**（默认） | 完整平台 | Cedar 授权、megaui 登录、Issue / reviewer 等产品路由。开源版不具备 Change List |
| **trunk / storage-only** | 只要存储与分发 | `push_policy=trunk`，推送经 `MonoWriteQueue` 直入 `main`；不挂 CL / Issue / OAuth；`push_auth=token` 或 `none` |

Trunk 下产品写 API（`POST /api/v1/create-entry`、`POST /api/v1/edit/save`）与 `git push` 共用 tip 权威。根树写入全局串行，多 commit 推送按产品规则合并进 `main`。

### HTTP API 与附属表面

- REST + 运行时 OpenAPI（`/api/openapi.json`）与 Swagger UI。
- 只读预览：blob / tree / blame。
- **OCI Distribution `/v2`**（仅 storage-only 且 `[oci].enabled=true`）：manifest / blob 上传与拉取。见 [`docs/refactoring/oci.md`](docs/refactoring/oci.md)。
- <a id="agent-capture"></a>**Agent Capture**（仅 storage-only 且 `[agent_capture].enabled=true`）：独立 HTTP `/api/v1/agent-capture`，不是 Git 分支。会话、event、checkpoint、file-op 的元数据在 `agent_capture_*` 表；字节在对象存储命名空间 `agent/`（`ObjectNamespace::Agent`）。checkpoint 是会话上的 HTTP 资源（`…/checkpoints`），不写 `refs/heads/*`，也不重放 Libra 本机的 `refs/libra/traces`。`agent_capture_blob_ref` 是表上的 blob 归属行，不是 `refs/`。认证用独立 ingest token，不复用 Git `push_tokens`。见 [`docs/refactoring/agent-capture.md`](docs/refactoring/agent-capture.md)。
- **提交后出站事件**（`[storage_events]`，默认关闭）：已提交写入发出带 HMAC 的 HTTPS 元数据事件（`repo.push`、`oci.manifest.published`、`lfs.object.uploaded`、`lfs.media.finalized`、`agent_capture.events.committed`）。Agent checkpoint 出站（`agent_capture.checkpoint.committed`）尚未安装。见 [`docs/refactoring/storage-events.md`](docs/refactoring/storage-events.md)。

### 认证、授权与密钥

- **认证**在 megaui（Better Auth 会话）；**授权**在 mega2（Cedar：`off` / `shadow` / `enforce`）。
- 浏览器走 session cookie → megaui `get-session`；Git / LFS / API 另用 Mono access token、SSH 公钥或 Bot token。手册见 [`docs/manual/authz.md`](docs/manual/authz.md)。
- 内嵌 **Vault**（crates.io `libvault` + `src/contract/vault/`）：PKI / KV、SecretRef、fail-closed 引导。

### 通知与配置

- 站内通知，以及可选 Slack / webhook；产品邮件经 megaui 内部 API 投递，本仓不跑 SMTP。
- 一级配置模块：`config init` / `validate` / secret、Profile、SecretRef、受控热加载。默认文件 `config/config.toml`，可用 `--config` 或 `MEGA_CONFIG` 覆盖。

## 用 Compose 快速测试

联调栈由本仓 `docker-compose.test.yml` 驱动。对象存储已内联，**不再**需要 sibling `orbit`。完整 UI / 登录测试需要 sibling **megaui**：

```text
<父目录>/
├── mega2/          # 本仓库（目录名可以仍是历史 checkout 名）
└── megaui/         # 前端 + 认证（apps/web、apps/collab-server）
```

`website-next` / `website-db-init` / `megaui-collab` 的 `build.context` 是 `../megaui`。缺少 megaui 时 web profile 构建会失败。Compose 服务名、隔离库名 `website`、以及 `MEGA_OAUTH__WEBSITE_*` / `MEGA_NOTIFICATION__WEBSITE_MAIL_*` **不要改名**——改名会让会话路径 fail-closed 成 401。

**前置**：Docker Compose v2；端口 `17001` / `17002` / `19180` / `15432` / `16379` / `19000` 可用。首次会拉 `rust:1.97-bookworm` 与 `node:22-alpine` 并编译 release，耗时较长。

### 完整栈（推荐：先 web 后 app）

在本仓库根目录执行。`website-next` 是 `pull_policy: never` 的本地 tag，改指 megaui 后必须带 `--build`，否则可能复用旧镜像。

```bash
# 1) megaui：建 website 库 + megaui-collab + website-next
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile web up -d --build --wait website-next

# 2) mega2：postgres / redis / rustfs + 引擎
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app up -d --wait monoengine
```

也可以一步 `--profile app --profile web up -d --build --wait`。mega2 不声明对 `website-next` 的 `depends_on`（跨 profile 会让 app-only 配置被拒绝），分两步能保证首次 `get-session` 时前端已健康。

| 项 | 值 |
|---|---|
| megaui Web | `http://127.0.0.1:17001` |
| megaui Collab WebSocket | `ws://127.0.0.1:17002` |
| mega2 HTTP | `http://127.0.0.1:19180` |
| 引擎 → 会话基址 | `http://website-next:7001` |
| 账户库 | 共享 Postgres 上的独立 `website` 库 |
| CORS | 已含 `http://127.0.0.1:17001` |

页面闭环：打开 `http://127.0.0.1:17001` 注册 / 登录 → 带 cookie 访问 `http://127.0.0.1:19180` 的 `GET /api/v1/user` → `username` / `website_user_id` 与 megaui 会话一致；**无 cookie → 401**。

连通性冒烟：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web exec -T monoengine \
  curl -fsS http://website-next:7001/api/auth/get-session
```

浏览器会话不等于 Git 凭据。登录后 Git / LFS / SSH 仍要在 `/api/v1/user` 下创建 access token 或登记 SSH key。

### 数据面 + 集成测试

不需要常驻引擎、只要跑 `cargo test` 时：

```bash
./scripts/dev-test.sh full    # 数据面 + git-cli + cargo test --all
./scripts/dev-test.sh down
```

先 `cp .env.test.example .env.test`（脚本也会自动创建）。手贴步骤与端口表见 [`docs/development.md`](docs/development.md)。Compose 项目名必须带 **`-p monoengine-it`**，避免与默认目录名项目抢网络。

### Trunk / storage-only（无登录面）

只要 Git HTTP + 对象存储、不要 megaui 时：

```bash
docker compose -p monoengine-trunk -f docker-compose-storage-only.yml up -d --wait

docker compose -p monoengine-trunk -f docker-compose-storage-only.yml exec -T monoengine \
  monoengine --config /etc/monoengine/config.toml service init --yes
```

HTTP：`http://127.0.0.1:9000/`。默认 push token 见 `docs/deploy-trunk.md`。该栈端口不与 IT 栈冲突，但两套不要混用同一项目名。

### 停止

```bash
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down
# 连数据卷：
docker compose -p monoengine-it -f docker-compose.test.yml --profile app --profile web down -v

docker compose -p monoengine-trunk -f docker-compose-storage-only.yml down -v
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
