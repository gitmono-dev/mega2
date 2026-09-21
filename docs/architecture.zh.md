[English](architecture.md) · 中文

# 架构设计

本文描述 mega2 的整体架构：模块划分、存储分层、写入路径、认证授权与机密管理、协议面挂载、配置与热加载。事实基线是当前 checkout 的源码；代码引用以 `src/...` 内联路径标注。

> **范围**：开源版 mega2 只交付 trunk / storage-only 形态——无 Web UI、无 Change List 产品面，交互式浏览走 Libra 的 `libra mega2 browser`。产品规则（唯一公开分支 `main`、N 分流、不变式）以 [`monorepo.md`](./monorepo.md) 为准；部署运维以 [`deploy-trunk.md`](./deploy-trunk.md) 为准。本文不复制这两份文档的事实值，只做架构导览与导航。

## 1. 总览与依赖流向

mega2 是单 Cargo package（lib `mega2_core` + 二进制；见 [`../Cargo.toml`](../Cargo.toml)）。入口 `src/main.rs` → `cli::parse` → `src/commands/mod.rs` 的子命令注册表。运行期依赖流向是单向的：上层组装下层，下层不反向引用上层。

```
┌────────────────────────────────────────────────────────────┐
│ CLI（src/commands）                                        │
│   service(init/http/ssh/multi) · config · debug · authz-audit │
└───────────────────────────┬────────────────────────────────┘
                            │ 组装（AppContext::new 分阶段引导）
                            ▼
┌────────────────────────────────────────────────────────────┐
│ context::AppContext（组合根，src/context/mod.rs）            │
│   Storage · VaultCore · ConfigHandle · redis ConnectionManager │
│   · SharedEntityStore · shutdown tokens                      │
└───────────────────────────┬────────────────────────────────┘
                            │
              ┌─────────────┴─────────────┐
              ▼                           ▼
┌──────────────────────────┐   ┌────────────────────────────┐
│ server::http_server      │   │ server::ssh_server         │
│ （axum router 装配）       │   │ （storage-only 下只读）      │
└───────────┬──────────────┘   └───────────┬────────────────┘
            ▼                              │
┌──────────────────────────┐               │
│ api routers / contract:: │◄──────────────┘
│ git_protocol（协议处理）   │
└───────────┬──────────────┘
            ▼
┌──────────────────────────┐
│ ceres（业务服务：pack、     │
│ api_service、lfs、oci…）  │
└───────────┬──────────────┘
            ▼
┌────────────────────────────────────────────────────────────┐
│ jupiter storage                                            │
│   ├─ PostgreSQL（sea-orm 元数据，callisto 实体）             │
│   ├─ 对象存储（orbit_api 契约 + orbit 后端：local/S3/GCS）    │
│   └─ Redis（缓存 / 分布式锁）                                │
└────────────────────────────────────────────────────────────┘
```

要点：

- **`AppContext` 是唯一的组合根**（`src/context/mod.rs:41`）。`AppContext::new` 分阶段引导：先 `Config::validate`（fail-closed），再建 DB 连接 → DB-only 的 `VaultCore` 引导 → 解析对象存储 / Redis 的 `vault://` SecretRef → `build_object_storage` → `Storage` → Redis → 通知 worker → `init_monorepo` 与后台任务（push queue reaper、blob path compensator、tombstone 审计）。任一环节失败即整体启动失败。
- **服务层不直接持有配置**：`ConfigHandle`（`src/config/reload.rs`）持有可热加载的快照，`AppContext::config()` 优先读快照（热加载见第 7 节）。
- **只读装配是独立路径**：`authz-audit` 等只读运维命令走 `ReadOnlyContext`（`src/context/mod.rs:516`）——只读 DB 连接（不跑迁移）、按需只读打开 Vault，与生产装配明确隔离。

深入阅读：[`refactoring/integration.md`](./refactoring/integration.md)（进程级集成与 IT 拓扑）、[`refactoring/general.md`](./refactoring/general.md)（重构文档共同约束）。

## 2. 模块职责

| 模块（`src/`） | 职责 |
| --- | --- |
| `commands` | CLI 子命令注册与执行（`service` / `config` / `debug` / `authz-audit`）；全局 flag `--config` / `--profile`（env `MEGA_CONFIG` / `MEGA_PROFILE`）。新增子命令的 how-to 见 [`contributing.zh.md`](./contributing.zh.md) |
| `common` | 错误类型（`MegaError` / `MegaResult`）、工具函数、`canonical_json`、`oci_name` |
| `config` | TOML 配置管线：`loader`（来源解析）、`model`（配置模型）、`validate`（启动校验）、`secret`（SecretRef）、`reload`（热加载）。注意：配置模块在 `src/config/`，**不是** `src/common/config` |
| `context` | 组合根 `AppContext` 与只读装配 `ReadOnlyContext`（见第 1 节） |
| `server` | 服务引导：`http_server`（axum router 装配、CORS/session/trace 中间件）、`ssh_server`、`trace_context` |
| `api` | HTTP handler 与路由：`api_router`（按形态切换的 `/api/v1` 子集）、`lfs_router`、`oci_router`、`preview_router`、`tag_router`、`agent_capture_router`、`api_write_auth`（产品写鉴权）、`api_doc`（OpenAPI） |
| `api_model` | 请求/响应 DTO（utoipa schema） |
| `ceres` | 业务服务：`protocol` + `pack`（Git smart 协议与收包）、`api_service`（monorepo 读写核心）、`lfs`、`oci`、`agent_capture`、`code_edit`、`snapshot`（MST/2）、`github_sync`、`merge_checker` |
| `jupiter` | 存储与服务底座：`storage/`（`*Storage` 包装器）、`service/`（`push_queue_service` 等）、`migration/`（sea-orm-migration）、`redis/` |
| `callisto` | sea-orm 实体，一表一文件；`lib.rs` 做 `pub use crate::callisto::*` 重导出 |
| `orbit_api` + `orbit` | 对象存储契约（trait/config）与后端实现（local / S3 / GCS），经 `orbit::factory::ObjectStorageFactory` 构建 |
| `contract` | 跨层契约：`git_protocol`（smart 协议挂载与认证上下文）、`policy`（Cedar 授权与实体快照）、`vault`（libvault 之上的产品集成层）、`api` |
| `notification` | 通知派发：`NotificationService`、channels（in-app / webhook）、triggers |

模块详细布局与约定（导入分组、错误类型、注释密度）见 [`../AGENTS.md`](../AGENTS.md) 的 Project Layout 与 Code Conventions。

## 3. 存储架构

三类存储各司其职，全部经 `jupiter` 层访问，API/handler 不直接碰 `sea_orm`（[`../AGENTS.md`](../AGENTS.md) 约定）：

1. **元数据：PostgreSQL + sea-orm**（测试可用 SQLite）。表结构实体在 `src/callisto/`（一表一文件），其上每个领域一个 `*Storage` 包装器（`src/jupiter/storage/`，如 `mono_storage`、`git_db_storage`、`lfs_db_storage`、`push_queue_storage`、`vault_storage`），统一由 `Storage`（`src/jupiter/storage/mod.rs`）聚合，共享一个 `BaseStorage` 连接。schema 演进走 `src/jupiter/migration/` 的 sea-orm-migration migrator。
2. **对象内容：可插拔对象存储**。Git 对象与 LFS 对象的二进制内容不入库，由 `build_object_storage`（`src/jupiter/storage/object_storage.rs`）经 orbit 工厂按 `[object_storage].storage_type` 构建后端：local / S3-compatible / GCS。契约类型在 `src/orbit_api/`，实现在 `src/orbit/`。S3 系凭证支持 `vault://` SecretRef（见第 5 节）。LFS 大文件的 FastCDC 分块见 [`refactoring/fastcdc-media.md`](./refactoring/fastcdc-media.md)。
3. **Redis：缓存与分布式锁，不是队列**。git 对象缓存（`GitObjectCache`）与 `RedLock` 互斥锁在 Redis（`src/jupiter/redis/`）；写入队列是 Postgres 的 `push_queue` 表（见第 4 节），Redis 侧没有 FIFO/持久化队列。连接经 `connection-manager` 共享，`redis.url` 支持 SecretRef。

深入阅读：[`refactoring/orbit.md`](./refactoring/orbit.md)（对象存储契约与后端）、[`refactoring/storage-events.md`](./refactoring/storage-events.md)（存储事件外发）、[`refactoring/fastcdc-media.md`](./refactoring/fastcdc-media.md)。

## 4. 写入路径：MonoWriteQueue

**所有对 `main` 的写入由 MonoWriteQueue 全局序列化**（`src/jupiter/service/push_queue_service.rs`，载体是 Postgres `push_queue` 表 + `PushQueueService`）。Git push、产品 API 写（`create-entry` / `delete-entry` / `move-entry` / `edit/save`）、CL merge、import attach 共用同一个 tip 权威——队列序就是 `main` 的推进序，不存在第二条能绕过它的写路径。序列化带来的直接后果：并发推送按入队序逐个落地，冲突在执行期以「当前 tip」重查而非入队时快照判定。

**review 与 trunk 两种形态共用同一条队列，差别在推送进入队列前是否经过 CL 管线**：review（默认形态）下分支推送先落成 CL（`refs/cl/*`），经评审后由 merge 入队落地；trunk（本仓交付形态）下推送与产品 API 写直接入队前进 path tip，不创建 CL。形态切换的启动期 fail-closed 检查（cedar 必须为 off、无 open CL、队列须排空、`push_auth` 必须显式等）是启动错误而非警告，完整清单见 [`deploy-trunk.md`](./deploy-trunk.md) 第 1 节；校验执行点是 `Config::validate` 与 `AppContext::new`（`src/context/mod.rs:128`），不经 CLI 的服务路径同样被拦截。

深入阅读：[`refactoring/trunk-push.md`](./refactoring/trunk-push.md)（队列设计、N 分流、不变式）、[`deploy-trunk.md`](./deploy-trunk.md)（形态开关与运维）。

## 5. 认证、授权与机密

三层边界，互不替代：

- **推送认证（authn）**：`git.push_auth = token | none`。静态 token 常量时间查找，`paths` 前缀按**组件边界**授权（`/project/foo` 不覆盖 `/project/foobar`）。Git receive-pack、LFS 批/锁写、产品 API 写共用该模型；认证身份与 commit author 分离（author 是自声明 provenance，不参与判定）。token 值与完整规则见 [`deploy-trunk.md`](./deploy-trunk.md) 第 2–3 节，本文不复制。
- **授权判定（authz）**：Cedar（`cedar-policy`，schema `src/contract/policy/mega.cedarschema`，策略 `src/contract/policy/mega_policies.cedar`），三档 `off / shadow / enforce`——**只在 review 形态生效**；trunk 要求 `cedar.enforcement = "off"`（启动 fail-closed）。授权快照经 `SharedEntityStore` 在写路径（notify）与读路径（guard/push）之间共享同一实例（`src/context/mod.rs:69`）。规则与操作手册见 [`manual/authz.md`](./manual/authz.md)。
- **机密管理（secrets）**：内嵌 Vault——库来自 crates.io 的 `libvault`（vendored 模块已于 2026-08-21 移除），产品集成层在 `src/contract/vault/`（`VaultCore` / `VaultSecretResolver`）。配置里的机密写作 `vault://` SecretRef（`src/config/secret.rs`），支持对象存储 S3 凭证、`redis.url`、通知 webhook token、storage_events HMAC 等白名单字段；每个字段绑定固定 vault 命名空间，启动期解析失败即启动失败，解析结果不回写配置快照、不出现在错误信息里。`config secret` / `config vault` 子命令是其 CLI 面。

深入阅读：[`refactoring/vault.md`](./refactoring/vault.md)、[`refactoring/contract.md`](./refactoring/contract.md)、[`manual/authz.md`](./manual/authz.md)。

## 6. 协议层：挂载点与开关

HTTP router 在 `server::http_server::app`（`src/server/http_server.rs:681`）按形态装配；各协议面的挂载点与开关：

| 协议面 | 挂载点 | 开关 |
| --- | --- | --- |
| Git smart HTTP | `*/info/refs`、`*/git-upload-pack`、`*/git-receive-pack`（catch-all 兜底） | 始终挂载 |
| SSH | `service ssh`（独立端口） | storage-only 下只读（upload-pack）；`git.ssh_receive_pack=false` 为强制项，省略会拒绝启动 |
| Git LFS | `/info/lfs` + `/api/v1/lfs` | 两种形态均挂载；写授权随 `push_auth` |
| 产品 API | `/api/v1/*`（status、file/blob、file/tree、preview 读 + create-entry/delete-entry/move-entry/edit/save 写 + tags） | trunk / storage-only 形态 |
| OCI registry | `/v2/` | `[oci].enabled` 且 storage-only；review 形态 fail-closed 不注册 |
| Agent Capture | `/api/v1/agent-capture` | `[agent_capture].enabled` 且 storage-only |
| MST/2 快照面 | `/api/v2` | nest 静态注册，handler 在 `[mst2].enabled` 为 false 时 fail-closed；切换需重启 |
| Swagger UI / OpenAPI | `/swagger-ui`、`/api/openapi.json` | 始终挂载；文档内容随形态裁剪 |

端口、compose 栈与容器部署参数见 [`deploy-trunk.md`](./deploy-trunk.md)（本地 compose）与仓库根 `Dockerfile`（release 构建，默认 `service http --host 0.0.0.0 -p 8000`）。

深入阅读：[`refactoring/protocol.md`](./refactoring/protocol.md)（receive-pack 分层、pkt-line、认证上下文）、[`refactoring/oci.md`](./refactoring/oci.md)、[`refactoring/agent-capture.md`](./refactoring/agent-capture.md)。

## 7. 配置与热加载

配置管线在 `src/config/`（**不是** `src/common/config`）。要点：

- **加载顺序**：`--config` → env `MEGA_CONFIG` → `./config/config.toml` → `$MEGA_BASE_DIR/etc/config.toml` → 生成默认。profile（`--profile` / `MEGA_PROFILE`）是主配置旁的兄弟文件 `config.<profile>.toml`，在 env 覆盖之前合并。
- **覆盖与严格性**：env 覆盖模式 `MEGA_<SECTION>__<KEY>`；未知字段严格拒绝（残留键按错误处理，不静默忽略）。全部配置键的权威清单在带密集注释的样例 [`../config/config.toml`](../config/config.toml) 与 [`refactoring/config.md`](./refactoring/config.md)，本文不复制。
- **校验时机**：`Config::validate` 在 `AppContext::new` 与 `config validate` 两条路径都执行，服务启动与 CLI 校验同一套规则。
- **热加载**：service 命令启动 5s 轮询 watcher（`CONFIG_RELOAD_POLL_INTERVAL`，`src/commands/service/mod.rs:22`），变更经 `ConfigHandle::reload` 处理：白名单字段（`log`、`artifacts_gc`、`buck`、`notification`）即时生效并通知订阅者，其余字段记入 `restart_required_fields` 报告（`src/config/reload.rs:120`）。候选配置先过 `validate` 再应用，应用失败可回滚。

深入阅读：[`refactoring/config.md`](./refactoring/config.md)（配置分层、SecretRef、热加载契约）。

## 8. 延伸阅读

- 本套文档：[`quick-start.zh.md`](./quick-start.zh.md) · [`user-guide.zh.md`](./user-guide.zh.md) · [`configuration.zh.md`](./configuration.zh.md) · [`deployment.zh.md`](./deployment.zh.md) · [`contributing.zh.md`](./contributing.zh.md)
- 产品规则：[`monorepo.md`](./monorepo.md)；初始化操作：[`manual/monorepo-init.md`](./manual/monorepo-init.md)
- 部署运维：[`deploy-trunk.md`](./deploy-trunk.md)；本地开发与测试：[`development.md`](./development.md)
- 错误目录：[`errors.md`](./errors.md)；计划文档规范：[`plan/README.md`](./plan/README.md)
- 贡献流程与代码约定：[`contributing.zh.md`](./contributing.zh.md)、[`../AGENTS.md`](../AGENTS.md)
