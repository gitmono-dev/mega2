[English](deployment.md) · 中文

# 部署指南

mega2 是面向 Agent 的第二代 Mega 引擎，其 Monorepo 服务是核心能力，Agent Session Capture 是可选接口。开源版以 **trunk / storage-only** 模式部署，不提供 Web UI；Libra 命令 `libra mega2 browser` 提供基础的终端目录浏览及受支持的 Tag 操作。本文说明安装、运行时行为、鉴权和模式切换。仓库与推送规则见[使用指南](./user-guide.zh.md)，配置项以带注释的 [`config/config.toml`](../config/config.toml) 为准。

## 1. 部署形态

唯一支持的部署形态是 trunk / storage-only：

- 唯一公开分支 `main`；所有写（git push 与产品 API 写）经 MonoWriteQueue 全局串行化，共享 tip 权威；不注册 CL / issue / reviewer / OAuth user 路由。
- HTTP 表面：Git smart HTTP（`info/refs`、`git-upload-pack`、`git-receive-pack`）、LFS（`/info/lfs`、`/api/v1/lfs`）、storage-only `/api/v1/*`（status、file/blob、file/tree、preview 读，create-entry / delete-entry / move-entry / edit/save、tags 写）、可选 OCI `/v2`（`[oci].enabled`）、可选 Agent Session Capture `/api/v1/agent-capture`（`[agent_capture].enabled`，独立接收会话、事件、checkpoint、transcript 和文件操作记录，不由 Git push 触发）、Swagger UI `/swagger-ui`、OpenAPI `/api/openapi.json`。
- SSH 仅 upload-pack（clone / fetch / pull）；`ssh_receive_pack` 必须显式 `false`，省略会拒绝启动。
- 写鉴权：`git.push_auth = "token"`（推荐）或 `"none"`（仅限受控网络）。具体配置与 SSH 规则见本指南第 3–5 节。

## 2. Compose 部署

仓库根的 Compose 文件分两类：**正式评估栈**与**测试 / 实验栈**。

### 2.1 评估栈（本机试用，推荐入口）

评估栈按平台提供两个变体；两者都从 **Docker Hub 拉取正式发布镜像** `genedna/mega2:latest`（`pull_policy: always`），不构建源码。组件：mega2 + Postgres + Redis + RustFS + `rustfs-init`（自动建 `mega2` 桶）；mega2 在服务启动时自动初始化空 Monorepo，**不需要** `service init` bootstrap。

- [`macos-orbstack-mega2-compose.yml`](../macos-orbstack-mega2-compose.yml)——macOS + OrbStack。利用 OrbStack 的 `*.orb.local` DNS，让 RustFS endpoint（`http://mega2-rustfs.orb.local:9000`）在宿主机与容器内都能解析。
- [`linux-mega2-compose.yml`](../linux-mega2-compose.yml)——Linux Docker。把 Postgres/Redis/RustFS 发布到宿主机回环，并让 mega2 以 `network_mode: host` 运行，使 `http://127.0.0.1:29000` 对两侧都可达。

之所以分两个变体，是因为 artifact **预签名 URL** 的 host 参与 SigV4 签名：同一个地址必须同时服务 mega2 进程（SDK + 签名）与宿主机客户端（直连下载 blob），而两个平台提供这一共享名称的机制不同。

```bash
docker compose -f macos-orbstack-mega2-compose.yml up -d --wait   # macOS + OrbStack
docker compose -f linux-mega2-compose.yml up -d --wait            # Linux Docker
docker compose -f <文件> logs -f mega2
docker compose -f <文件> down      # 具名卷保留数据；down -v 清空
```

两个栈都是**仅限本机的匿名设置**：`push_auth=none`、匿名读写，HTTP 只绑定 `127.0.0.1:9000`。不要绑定 `0.0.0.0` 或经反向代理暴露；共享 / 公网部署用下面的 token 栈或自建编排。端到端操作演示见 [`quick-start.zh.md`](./quick-start.zh.md)。

### 2.2 源码构建的测试 / 实验栈：`docker/docker-compose-storage-only.yml`

[`docker/docker-compose-storage-only.yml`](../docker/docker-compose-storage-only.yml) 是从源码**构建**镜像的本地 / 实验室参考栈（用于部署演练与 smoke，不是正式分发形态）：mega2 + Postgres + Redis + RustFS，挂载 [`config/config-storage-only.toml`](../config/config-storage-only.toml)。与 IT 栈 `docker/docker-compose.test.yml` 可并存（端口不冲突）。

服务与宿主端口（以 compose 文件为准）：

| 服务 | 容器端口 | 宿主发布 | 说明 |
| --- | --- | --- | --- |
| mega2 HTTP | 8000 | `127.0.0.1:9000` | Git smart HTTP + LFS + storage-only API + Swagger UI |
| mega2 SSH | 2222 | `127.0.0.1:2222` | 仅 upload-pack（receive-pack 由配置关闭） |
| postgres | 5432 | `127.0.0.1:25432` | |
| redis | 6379 | `127.0.0.1:26379` | |
| rustfs API | 9000 | `127.0.0.1:29000` | 默认 S3 兼容对象存储 |
| rustfs console | 9001 | `127.0.0.1:29001` | |
| git-smoke | — | 不发布端口 | `--profile smoke` 黑盒冒烟（见 [`deploy-trunk.md`](./deploy-trunk.md) §8.1） |

首次启动与 bootstrap：

```bash
# 首次构建（context = 仓库根）
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml build mega2

# 启动（默认 RustFS，不要加 --env-file）
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml up -d --wait

# 空卷 bootstrap（一次性；创建初始图，不起监听）
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml exec -T mega2 \
  mega2 --config /etc/mega2/config.toml service init --yes
```

**Push token secret**：compose 把 token 文件挂载为 `/run/secrets/mega2-push-token`，默认源 `./secrets/mega2-push-token.local`（`secrets/` 已 gitignore，需自行创建）。生产部署用 `MEGA2_PUSH_TOKEN_FILE=/path/to/secret` 指向真实 secret 文件，勿提交明文。token 配置与 `paths` 授权语义见 [`deploy-trunk.md`](./deploy-trunk.md) §3。

### 2.2.1 `push_auth=none` 覆盖变体

[`docker/docker-compose-storage-only.auth-none.yml`](../docker/docker-compose-storage-only.auth-none.yml) 是 opt-in 覆盖：与基础文件双 `-f` 组合，把 mega2 的配置重挂载为 `config/config-storage-only.none.toml`，并 `--force-recreate mega2`：

```bash
docker compose -p mega2-trunk \
  -f docker/docker-compose-storage-only.yml \
  -f docker/docker-compose-storage-only.auth-none.yml \
  up -d --wait --force-recreate mega2
```

> **警告**：`push_auth=none` 等于匿名 receive-pack **以及匿名 LFS 上传**，只适用于受控内网、回环或 Unix socket 前置的部署。绝不暴露到公网。切回 token：去掉第二个 `-f`，再 `--force-recreate mega2`。

### 2.2.2 对象存储切换

默认后端是 RustFS（`s3compatible`），直接 `up` 即可。仅当把 mega2 改成本地文件系统后端时加 `--env-file`（RustFS 容器仍会启动，只切 mega2 的 `storage_type`）：

```bash
docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml \
  --env-file config/compose.env.storage-only.local up -d --wait
```

env 文件内容见 [`config/compose.env.storage-only.local`](../config/compose.env.storage-only.local)；后端契约见 [`refactoring/orbit.md`](./refactoring/orbit.md)。

## 3. 二进制 / 容器部署

不想从源码构建时，可以直接用 Docker Hub 上的正式发布镜像 `genedna/mega2:latest`（评估栈 compose 文件拉取的就是它）。自行构建二进制：

```bash
cargo build --release -p mega2   # 产物 target/release/mega2
```

[`Dockerfile`](../Dockerfile) 是同一产物的容器化：双阶段（`rust:1.97-bookworm` builder → `debian:bookworm-slim` runtime），配置拷入 `/etc/mega2/config.toml`，`MEGA_BASE_DIR=/var/lib/mega2`，`EXPOSE 8000`，`ENTRYPOINT` 为 mega2，默认 `CMD` 为 `service http --host 0.0.0.0 -p 8000`。

按需要选择 service 形态（CLI 全貌见 `src/commands/mod.rs` 与 [`AGENTS.md`](../AGENTS.md)）：

- `service http --host 0.0.0.0 -p 8000` — 仅 HTTP（Dockerfile 默认）。
- `service ssh` — 仅 SSH（upload-pack）。
- `service multi http ssh -p 8000 --ssh-port 2222` — 单进程同时起 HTTP + SSH（compose 栈用法）。

进程管理：mega2 是单一长驻进程，用 systemd 或容器 restart policy 管理即可；状态全部在外部依赖（Postgres / Redis / 对象存储）与 `MEGA_BASE_DIR` 数据目录。配置热重载为 5s 轮询 watcher，只有白名单字段即时生效，其余字段报 `restart_required`（见 [`src/config/reload.rs`](../src/config/reload.rs)）——改配置后留意日志，需要时重启进程。

## 4. 外部依赖与最低要求

| 依赖 | 说明 |
| --- | --- |
| PostgreSQL | 必需。compose 栈用 18.x。 |
| Redis | 必需。compose 栈用 8.x。 |
| 对象存储 | 必需，三选一：本地文件系统（`storage_type="local"`，仅单节点）、S3 兼容（默认，RustFS / MinIO / 云 S3）、GCS。经 [`refactoring/orbit.md`](./refactoring/orbit.md) 的 `build_object_storage` 构建。 |
| Vault | 无外部服务：内嵌 Vault 来自 crates.io `libvault` + [`src/contract/vault/`](../src/contract/vault/)，见 [`refactoring/vault.md`](./refactoring/vault.md)。 |

资源规格随仓库规模与推送并发而定；写路径全局串行（MonoWriteQueue），读路径水平伸缩受 Postgres / 对象存储限制。部署前用 `mega2 --config <path> config validate`（可加 `--show-sources` / `--deny-warnings`）验证配置，见 [`refactoring/config.md`](./refactoring/config.md)。

## 5. 生产加固清单

- [ ] `push_auth = "token"`，`[[git.push_tokens]]` 用 `paths` 按组件边界收窄到最小范围；**绝不**把 `push_auth=none` 暴露到公网（见 §2.1 警告与 [`deploy-trunk.md`](./deploy-trunk.md) §3）。
- [ ] 凭据经 `${file:...}` 文件挂载或 Vault SecretRef 注入；提交的 `config.toml` 不含明文（样例见 [`config/config.toml`](../config/config.toml)）。
- [ ] 反向代理终止 TLS；`MEGA_HTTP__PUBLIC_BASE_URL` 与 LFS URL 指向外部 https 地址（HTTP 明文 registry 需客户端配 insecure registry，见 [`refactoring/oci.md`](./refactoring/oci.md)）。
- [ ] `log.print_std = false`，日志落盘到 `mega_cache()/logs`；compose 栈的 `print_std=true` 仅适合容器场景。
- [ ] 宿主端口绑定回环 / 内网地址，按需放行。
- [ ] `cedar.enforcement` 保持 `off`（trunk 形态的启动前置，见 [`deploy-trunk.md`](./deploy-trunk.md) §1）。
- [ ] OCI `/v2` 与 Agent Capture 按需显式开启（缺省不挂载）。
- [ ] 备份三件套：Postgres dump、对象存储 bucket、`mega2 --config <path> config vault backup <destination>`（Vault core key；恢复用 `config vault restore`，见 [`src/commands/config.rs`](../src/commands/config.rs)）。

## 6. 升级与形态切换

- 升级：替换镜像 / 二进制并重启。标注 `restart_required` 的配置变更必须重启才生效。
- 形态切换（`review ↔ trunk`）的启动前置（无 open CL、排空 `push_queue`、显式 `push_auth`）与索引水位重置，运维事实源是 [`deploy-trunk.md`](./deploy-trunk.md) §5，设计论证见 [`refactoring/trunk-push.md`](./refactoring/trunk-push.md)。

## 7. 可观测性

- 健康检查：`GET /api/v1/status`；compose healthcheck 用 `GET /api/openapi.json`。
- 日志：`mega_cache()/logs` 下的小时滚动文件（`log.print_std=true` 时走 stdout，compose 栈即如此）。
- API 目录：Swagger UI `/swagger-ui`、OpenAPI JSON `/api/openapi.json`。
- 栈级冒烟：`scripts/git_protocol_smoke_storage_only.sh`（Git 协议）与 `scripts/api_write_smoke_storage_only.sh`（API 写 → Git 可见性），用法见 [`deploy-trunk.md`](./deploy-trunk.md) §8.1；OCI 冒烟 `scripts/oci_smoke_storage_only.sh`（[`deploy-trunk.md`](./deploy-trunk.md) §10.2）。

## 8. 相关文档

- 使用与开发指南：[`quick-start.zh.md`](./quick-start.zh.md) · [`recipes.zh.md`](./recipes.zh.md) · [`user-guide.zh.md`](./user-guide.zh.md) · [`configuration.zh.md`](./configuration.zh.md) · [`architecture.zh.md`](./architecture.zh.md) · [`contributing.zh.md`](./contributing.zh.md)
- [`deploy-trunk.md`](./deploy-trunk.md) — trunk / storage-only 运维事实源（鉴权、SSH、LFS、形态切换、OCI、smoke）。
- [`user-guide.zh.md`](./user-guide.zh.md) — 仓库路径、分支和 Tag 规则，以及推送流程。
- [`development.md`](./development.md) — 本地开发与测试。
- [`config/config.toml`](../config/config.toml) + [`refactoring/config.md`](./refactoring/config.md) — 配置键与加载 / 校验语义。
- [`refactoring/orbit.md`](./refactoring/orbit.md)、[`refactoring/vault.md`](./refactoring/vault.md)、[`refactoring/oci.md`](./refactoring/oci.md)、[`refactoring/agent-capture.md`](./refactoring/agent-capture.md) — 各子系统契约。
