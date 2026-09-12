# Orbit 对象存储（已内联）

> **当前拓扑（plan-20260824，2026-08-24 落地）：** orbit 契约与实现已迁入本 package：
> `src/orbit_api/`（traits、config、errors）与 `src/orbit/`（`object_store` 后端）。
> 对象存储由 `src/jupiter/storage/object_storage.rs::build_object_storage` 直接调用
> `crate::orbit::factory::ObjectStorageFactory::build`；**无** `ObjectStorageProvider`
> 进程级注册表，**无** 独立 `crates/orbit*` workspace 成员。
>
> 下列历史正文记录自 2026-06 起的 provider 注入 + workspace crate 演进，保留供审计。

本文档记录 `monoengine` 对 `orbit` 对象存储库的依赖治理：把当前对 **orbit 实现 crate** 的**项目（path）引用**重构为只依赖 **`orbit-api`（纯 API/契约 crate）**，并通过依赖注入把唯一的具体实现构造点下沉到组合根 / 独立二进制边界。

> **治理规范**：本文档遵循 **`general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md。

> **关键依赖**：
> - 与 **`config.md`** 协同：`ObjectStorageConfig`/`LocalConfig`/`S3Config`/`GcsConfig`/`ObjectStorageBackend` 均来自 `orbit_api::factory`，经 `src/config/mod.rs:11` re-export 进 `crate::config`，并被 `config validate` 的对象存储校验消费。本重构**不改变**这些 API 类型的来源（仍是 `orbit-api`）。
> - 与 **`vault.md`** 协同：必须保持启动顺序硬约束 `Config(synchronous) → Storage(DB + object storage) → VaultCore`。对象存储在 `Storage::new` 阶段（vault 之前）构造，本重构只移动其**构造位置**，不改变**构造时机**。

> **集成测试指引**：对象存储 4 种后端（`S3` / `S3Compatible` / `Gcs` / `Local`）的构造与读写行为必须保持不变。应通过 **`integration.md`** 中的对象存储 / 服务启动场景（本地后端 put/get、服务 `service http` 启动）端到端验证重构前后行为一致。

> **实现状态（2026-06-19）：阶段 0–3 已全部落地并通过门禁。** monoengine 已拆分为 Cargo workspace：根 crate `monoengine-core`（lib，**只依赖 `orbit-api`**）+ `bin/` crate `monoengine`（瘦二进制，依赖 `monoengine-core` + `orbit` 实现）。核心改造：在 `src/jupiter/storage/object_storage.rs` 定义 `ObjectStorageProvider` trait + 进程级注册表（`set_object_storage_provider` / `build_object_storage`）；`Storage::new` / `AppContext::new` 改为接收注入的 `MegaObjectStorageWrapper` 值；`service` / `chat-migrate` 两个 exec 通过注册的 provider 构造对象存储；`bin/src/main.rs` 注册 `OrbitObjectStorageProvider`（唯一调用 `orbit::factory::ObjectStorageFactory::build` 处）。验收：`cargo tree -p monoengine-core` 的 `object_store` 计数 = 0、orbit-impl 计数 = 0；`bin` 的 `object_store` 计数 = 1。门禁：workspace `fmt` / `clippy -D warnings` 通过，`monoengine-core` 单测 530 passed，`monoengine` 集成测试 4 passed，二进制 `config init`/`validate` 烟雾通过。详见各阶段下的"✅ 已落地"标注。

> **实现状态（2026-08-22）：orbit 两个 crate 已并入本 workspace（"方案 B"）。** `crates/orbit` + `crates/orbit-api`（原 sibling `../orbit/crates/*`）整体拷入本仓 `crates/`，`Cargo.toml` workspace `members = ["bin", "crates/orbit", "crates/orbit-api"]`；`Cargo.toml` 依赖改为 `orbit-api = { path = "crates/orbit-api" }`，`bin/Cargo.toml` 改为 `orbit = { path = "../crates/orbit" }` / `orbit-api = { path = "../crates/orbit-api" }`。crate 边界与依赖方向零变化：core 仍只依赖 `orbit-api`，仍不出现 `orbit::`（实现 crate）源码引用；实测 `cargo tree -e features` 确认 core 的 `object_store` 仅启用 `default/fs/tokio/walkdir`（无 `cloud/aws/gcp`，`aws-lc-rs` 为 rustls TLS 后端，与 AWS 无关），`cloud/aws/gcp` 仍仅存在于 `bin` 的编译图（注：`cargo tree` 中 core 的 `object_store` 计数为 1 系 `orbit-api` 自带依赖，合并前即如此，2026-06-19 的"计数=0"验收口径应理解为"*带 cloud 特性的* object_store = 0"）。同步改动：`Dockerfile` 去掉 `COPY orbit`（path 依赖全在 `monoengine/` 内）；两个 CI workflow（`config-validation.yml` / `git-protocol-smoke.yml`）删除 orbit sibling checkout 步骤（`ORBIT_CHECKOUT_TOKEN` 作为 monoui token 的 fallback secret 名保留未动）；README / docs/development.md 更新目录布局描述。门禁：`cargo +nightly fmt --all --check` 通过（对 `crates/orbit` 既有 2 处 rustfmt 差异已按本仓 rustfmt.toml 修正）；`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test` 中对象存储 / authz / vault / git 协议相关用例全绿（环境受限未跑的用例见下）。验收命令不变。

## 事实校准（2026-06-19）

> 本文档的代码引用已对照当前 `src/`（monoengine）与 `/run/media/eli/data/GitMono/orbit`（orbit + orbit-api）逐条核对。需特别注意以下决定方案形态的事实：

1. **monoengine 是单一二进制 crate。** `src/` 下**没有 `src/lib.rs`**，`Cargo.toml` 只有 `[package] name = "monoengine"`（无 `[lib]`、无 `[[bin]]`，隐式 bin = `src/main.rs`）。这意味着"核心代码"与"二进制"是**同一个编译单元**：仅把工厂调用"上移到 main"并不能把 `orbit` 从依赖图里去掉——要真正移除重量级依赖，必须引入 crate 边界（见阶段 3）。
2. **全仓只有 1 处对 orbit 实现 crate 的引用。** `src/jupiter/storage/object_storage.rs:8` 的 `pub use orbit::factory::ObjectStorageFactory;` 是整个 crate 中**唯一**的 `orbit::`（实现 crate）源码引用；它经 `src/jupiter/storage/mod.rs:86` 引入，并在 `src/jupiter/storage/mod.rs:213` 运行期调用 `ObjectStorageFactory::build(&config.object_storage).await?`。其余对 orbit 生态的引用（18 处，跨 13 个文件）**全部是 `orbit_api::`**（API crate）——这正是目标依赖，无需改动。
3. **`Cargo.toml` 同时声明两个 path 依赖：** `orbit = { path = "../orbit" }`（`Cargo.toml:57`，重量级实现）与 `orbit-api = { path = "../orbit/api" }`（`Cargo.toml:58`，轻量 API）。两者都在 `[dependencies]`（非 dev-dependencies）。
4. **`orbit-api` 已经是纯 API/契约 crate。** 4 个模块（`error` / `factory` / `log_storage` / `object_storage`）只含 trait 定义、config 结构体、类型别名与 `Arc` 包装器，**零存储逻辑**；依赖轻量（`async-trait`、`bytes`、`futures`、`reqwest`、`serde`、`serde_json`、`thiserror`、`toml`），**不含 `object_store`、不含云 SDK、不含 `tokio`**（`/run/media/eli/data/GitMono/orbit/api/Cargo.toml:6-14`）。使用侧的抽象 `MegaObjectStorage`/`LogStorage`/`MegaObjectStorageWithLog` 与包装器 `MegaObjectStorageWrapper { inner: Arc<dyn MegaObjectStorageWithLog> }` 均在此（`orbit/api/src/factory.rs:53-70`、`object_storage.rs`）。
5. **`orbit` 实现 crate 是重量级来源。** 它额外引入 `object_store 0.13.2` 且启用 `["cloud", "aws", "gcp"]` features（`/run/media/eli/data/GitMono/orbit/Cargo.toml:10`），带来 AWS S3 / GCP 云 SDK 及其凭据/HTTP 传输栈。`orbit::factory::ObjectStorageFactory::build(cfg) -> OrbitResult<MegaObjectStorageWrapper>`（`orbit/src/factory.rs:21`）按后端分派到 `build_s3_like` / `build_gcs` / `build_local`，构造具体 `ObjectStoreAdapter`（`orbit/src/adapter.rs`）并包进 `MegaObjectStorageWrapper`（一个 `orbit-api` 类型）。
6. **消费端已"注入就绪"。** `LfsService.obj_storage`（`src/jupiter/service/lfs_service.rs:10`）、`GitService.obj_storage`（`git_service.rs:18`）、`ArtifactService.obj_storage`（`artifact_service.rs:49`）都只持 `MegaObjectStorageWrapper`，且只通过 `.inner` 调 trait 方法（`put_stream`/`get_stream`/`get_many` 等），**不接触任何具体实现类型**。`Storage::new`（`mod.rs:196`）在 `:213` 构造后于 `:216`/`:248`/`:305` 把它克隆进 3 个服务。
7. **mock / 测试路径已纯 `orbit-api`。** `src/jupiter/storage/object_storage.rs:36` 的 `mock_object_storage()` 用 `MegaObjectStorageWrapper::new(Arc::new(InMemoryObjectStorage::default()))` 在 crate 内构造，`InMemoryObjectStorage` 仅实现 `orbit-api` 的 `MegaObjectStorage`/`LogStorage`——这本身就证明"注入一个已构造好的包装器"模式可行且不需要实现 crate。
8. **组合根与调用方已定位。** `AppContext::new(config)`（`src/context/mod.rs:40`）在 `:43` 调 `Storage::new`；`AppContext::new` 的两个调用方是 `src/commands/service/mod.rs:40` 与 `src/commands/chat_migrate.rs:114`。

## 当前实现状态速览表（2026-06-19）

> 下表中"原始状态"为重构前；"✅ 已落地"为 2026-06-19 实现后的状态。

| 能力 / 组件 | 实现状态 | 关键事实与风险 |
|-----------|--------|-------------|
| monoengine crate 形态 | ✅ **已拆为 workspace** | 根 crate `monoengine-core`（lib，`src/lib.rs` + `[lib] doctest=false`）+ `bin/` crate `monoengine`（瘦二进制）。原为单一二进制 crate。 |
| 对 orbit **实现 crate** 的引用 | ✅ **core 内为 0，集中在 bin** | core 不再依赖 `orbit`；唯一 `orbit::factory::ObjectStorageFactory::build` 调用在 `bin/src/main.rs` 的 `OrbitObjectStorageProvider`。`cargo tree -p monoengine-core` 的 orbit-impl 计数 = 0。 |
| 对 `orbit_api` 的引用 | **保持（即目标依赖）** | 18 处 / 13 文件，全是 trait/config/error 类型；未改动。core 与 bin 都依赖同一 `orbit-api`。 |
| `orbit-api` crate | **API-only，未改动** | 仅 trait/config/wrapper；无 `object_store`/云 SDK/`tokio`。本次重构**未触碰 orbit-api**（采用"注入值"而非新增 factory trait）。 |
| 构造抽象（factory） | ✅ **已落地：`ObjectStorageProvider` trait（在 core）** | `src/jupiter/storage/object_storage.rs` 定义 `ObjectStorageProvider` + 进程级注册表 `set_object_storage_provider` / `build_object_storage`；bin 提供 `OrbitObjectStorageProvider` 实现。 |
| 消费端（Lfs/Git/Artifact 服务） | **未改动（trait-only）** | 仍只持 `MegaObjectStorageWrapper`、只调 `.inner` trait 方法。 |
| 组合根 | ✅ **已倒置** | `Storage::new(config, object_store)` / `Storage::new_with_connection(config, connection, object_store)` 接收注入值；`AppContext::new(config)` 在内部经 `ObjectStorageProvider` 注册表构造对象存储（DB-only vault bootstrap 后解析 `vault://` SecretRef），再注入 `Storage::new_with_connection`；最终 `orbit` 调用在 `bin` main。 |
| mock / 测试路径 | **未改动（纯 orbit-api）** | `mock_object_storage()` 仍在 core 内用 `orbit-api` 构造；`Storage::new`/`AppContext::new` 无测试调用方，注入值改造零测试破坏。 |
| 重量级传递依赖 | ✅ **已从 core 编译移除** | `object_store {cloud,aws,gcp}` + AWS/GCP SDK 现仅在 `bin` 的编译图（`cargo tree -p monoengine-core` 的 `object_store` 计数 = 0；`bin` = 1）。 |

**关键危险点（各阶段必须收敛）**：
- 仅完成阶段 1/2（隔离 seam）会让人**误以为重量级依赖已移除**——其实单 crate 仍链接 `orbit`。务必把"是否拆 crate"作为达成"只依赖 orbit-api"目标的承重决策（阶段 3）。
- 给 `Storage::new`/`AppContext::new` 增加参数会触碰启动链路，可能扰乱 `Config → Storage → Vault` 顺序。
- 若选择给 `orbit-api` 增加 factory trait，必须警惕"顺手往 orbit-api 加实现/重依赖"，那会破坏 API-only 本性。

## 硬约束与不可违反的原则

1. **运行期对象存储行为零变更。** 同一 `orbit::factory::ObjectStorageFactory::build(&config.object_storage)` 必须仍对 4 种后端（S3/S3Compatible/Gcs/Local）运行，**只移动调用位置，不改变构造逻辑与时机**。理由：对象存储是 git/LFS/artifact 数据面，任何行为漂移都是数据风险。
2. **`orbit-api` 必须保持 API-only。** 不得向 `orbit-api` 引入 `object_store`、云 SDK 或 `tokio`。理由：一旦 API crate 携带重实现，"依赖 API 即轻量"的前提失效，重构失去意义。
3. **注入类型必须是 `orbit-api` 的 `MegaObjectStorageWrapper`（`Arc<dyn MegaObjectStorageWithLog>`）。** 不得把具体 `ObjectStoreAdapter` / `BackendStore`（`orbit/src/adapter.rs`）泄露进 core。理由：core 一旦命名具体实现类型，就重新耦合实现 crate。
4. **启动顺序约束（与 vault.md 一致）：** `Config(synchronous) → database_connection() → DB-only VaultCore bootstrap → resolve object storage SecretRef → build_object_storage → Storage::new_with_connection → redis → mail/notification`。对象存储凭据在 vault 就绪后解析，随后构造对象存储并注入 `Storage::new_with_connection`。
5. **测试 / mock 路径不得依赖 orbit 实现 crate。** `mock_object_storage()` / `Storage::mock` / `ArtifactService::mock` 必须只用 `orbit-api` 构造（现状已满足，重构后须保持）。
6. **消费端代码不改。** 18 处 `orbit_api::` 引用（跨 13 文件）保持不动；本重构只动"构造与注入"的少数位置，不动"使用"。

## 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
|-----|--------|--------|--------|
| core 对实现 crate 的依赖 | `Cargo.toml:57` 直接 `orbit = { path }` | core 不依赖 `orbit` 实现 crate，只依赖 `orbit-api` | 复杂（需拆 crate） |
| 工厂调用位置 | core 内 `Storage::new` 直接调 `orbit::…::build`（`mod.rs:213`） | 工厂调用下沉到单一 bootstrap 模块 / 瘦二进制边界 | 中等 |
| 对象存储注入 | `Storage::new` 内部自建 | `Storage::new(config, object_store: MegaObjectStorageWrapper)` 由调用方注入 | 中等 |
| `orbit::` 源码引用数 | 1（`object_storage.rs:8`） | 阶段 2 后仍 1，但集中在 bootstrap；阶段 3 后 core 内为 0 | 简单→复杂 |
| core 编译的重量级依赖 | 含 `object_store {cloud,aws,gcp}` + 云 SDK | core 编译不含；仅瘦二进制 / adapter crate 含 | 复杂（阶段 3） |
| `orbit-api` 来源 | path 依赖 | path 或发布版本（待决策，见"待决策项"） | 简单 |

## 迁移步骤（分阶段）

> **总体策略：两段式。** "阶段 1–2 = seam 清理（仍保留 `orbit` 依赖）"是**强制前置**且使阶段 3 变成机械操作；"阶段 3 = 拆出 `monoengine-core` lib"才**真正**从 core 编译移除重量级依赖。**推荐做法：注入"已构造好的值"`MegaObjectStorageWrapper`，而非注入 factory——因为构造产物本就是 `orbit-api` trait object，无需给 `orbit-api` 增加 factory trait，API 面最小。**

> **✅ 已落地实现说明（2026-06-19）。** 实际实现综合了"注入值"与"进程级 provider 注册表"：
> - `Storage::new` / `Storage::new_with_connection` 接收注入的 `MegaObjectStorageWrapper` **值**；`AppContext::new(config)` 在内部先解析 `vault://` SecretRef，再经 `ObjectStorageProvider` 注册表构造对象存储并注入 `Storage::new_with_connection`。
> - 因 config 在 core 的 `cli::parse` 内解析、`service`/`chat-migrate` 两个 exec（在 core 内）才知道 config，故无法在 bin 侧预先构造值；改为在 core 定义 `ObjectStorageProvider` trait + 进程级注册表（`set_object_storage_provider` / `build_object_storage`，`src/jupiter/storage/object_storage.rs`），bin 在 `main` 启动时注册 `OrbitObjectStorageProvider`，`AppContext::new` 调用 `build_object_storage(&config.object_storage)` 构造后注入 `Storage`。
> - 选择该注册表而非"穿 `CommandContext`"是为零破坏 `cli::parse` 的大量测试调用方；选择"注入值 + 注册表"而非"给 orbit-api 加 factory trait"是为保持 orbit-api 零改动、API 面最小（硬约束 #2）。
> - `orbit::factory::ObjectStorageFactory::build` 的唯一调用点位于 `bin/src/main.rs`（composition root）。

**阶段 0 — 确认 seam 与锁定行为基线**

1. 确认 `grep -rn 'orbit::' src/` 恰好只返回 `object_storage.rs:8` 一行。
2. 确认唯一构造调用点是 `mod.rs:213`，消费端在 `:216`/`:248`/`:305`。
3. 确认 3 个服务只持 `MegaObjectStorageWrapper` 且只走 `.inner` trait 方法（`git_service.rs:98/116/156` 等）。
4. 跑现有测试 + 本地后端（`ObjectStorageBackend::Local`）put/get 烟雾，记录基线行为。

> **验收标准**：
> - ✅ `grep -rn 'orbit::' src/` 恰好 1 行
> - ✅ 基线测试通过；本地（及如有凭据则 S3/GCS）对象 put/get 正常
> - ✅ 4 个实现 crate 触点（`Cargo.toml:57`、`object_storage.rs:8`、`mod.rs:86`、`mod.rs:213`）已记录

**阶段 1 — 把 `object_store` 作为注入参数穿过 `Storage::new` / `AppContext::new`**

5. `Storage::new(config: Arc<Config>)`（`mod.rs:196`）改为 `Storage::new(config: Arc<Config>, object_store: MegaObjectStorageWrapper)`；删除 `:213` 的 `let object_store = ObjectStorageFactory::build(...).await?;`，在 `:216`/`:248`/`:305` 直接用参数。
6. `AppContext::new(config)`（`context/mod.rs:51`）在内部先执行 DB-only `VaultCore` bootstrap，再调用 `resolve_object_storage_secrets` 与 `build_object_storage` 构造对象存储，最后通过 `Storage::new_with_connection` 注入；保持 `Config → database_connection() → VaultCore → object storage → Storage → redis → mail/notification` 顺序。
7. `commands/service/mod.rs:40` 与 `commands/chat_migrate.rs` 的调用方仍直接传入 `config`，对象存储构造留在 `AppContext::new` 内部（由进程级 `ObjectStorageProvider` 注册表实现）。
8. `Storage::mock`/测试构造点改为传入 `mock_object_storage()`（已是 orbit-api 构造）。

> **验收标准**：
> - ✅ crate 编译通过；`Storage::new`/`AppContext::new` 签名接收 `MegaObjectStorageWrapper`，其内部不再调 `ObjectStorageFactory::build`
> - ✅ 启动顺序 `Config → Storage → Vault` 不变（vault 仍在 `Storage::new` 之后从其结果构造）
> - ✅ 全量测试通过；mock 路径不依赖 orbit 实现 crate

**阶段 2 — 把唯一的实现 crate 调用隔离进单一 bootstrap 模块**

9. 新增 `src/bootstrap/object_storage.rs`（或类似），提供 `pub async fn build_object_store(cfg: &orbit_api::factory::ObjectStorageConfig) -> orbit_api::error::OrbitResult<orbit_api::factory::MegaObjectStorageWrapper> { orbit::factory::ObjectStorageFactory::build(cfg).await }`——使其成为**唯一**的 `orbit::` 使用者。
10. 删除 `object_storage.rs:8` 的 `pub use orbit::factory::ObjectStorageFactory;` 与 `mod.rs:86` 的相应引入。
11. 2 个 `AppContext::new` 调用方改为先 `bootstrap::object_storage::build_object_store(&config.object_storage).await?` 再 `AppContext::new(config, object_store)`。
12. crate 内 `mock_object_storage()` 与测试 mock 不变（已只用 orbit-api 类型）。

> **验收标准**：
> - ✅ `grep -rn 'orbit::' src/` 恰好 1 行，且位于新 bootstrap 模块内
> - ✅ 全量测试通过；mock 路径不变
> - ✅ 本地 + S3/GCS 运行期行为与阶段 0 基线一致

**阶段 3 —（决策门）拆出 `monoengine-core` lib crate，真正移除重量级依赖**

13. 引入 crate 边界：将除 bootstrap glue + `main()` 外的所有模块移入 `monoengine-core` lib crate（或当前 crate 转 lib + 新增瘦 bin）。
14. `monoengine-core` 的 `Cargo.toml`：**只依赖 `orbit-api`，不依赖 `orbit`**。
15. 瘦二进制 crate（或独立 adapter crate，见"待决策项"）：依赖 `monoengine-core` + `orbit = { path = "../orbit" }`；持有 `src/bootstrap/object_storage.rs` 与 `main.rs`，先 `build_object_store` 再 `AppContext::new`。
16. 按需把 `AppContext` / `Config` / `Storage` / `MegaError` 等设为 `pub`，暴露最小入口 API。
17. 校验 `cargo tree -p monoengine-core` 不含 `object_store`/`aws`/`gcp`。

> **验收标准**：
> - ✅ `cargo tree -p monoengine-core | grep -E 'object_store|aws|gcp'` 为空
> - ✅ 二进制对 4 种后端运行期行为与基线一致
> - ✅ 全量测试通过；core crate 的依赖数 / 编译时间可度量下降

## 前置依赖矩阵

| 本文档的工作 | 对其他文档的依赖 | 类型 | 关键同步点 |
|-----------|-------------|-----|---------|
| 阶段 1 注入参数穿过 `Storage::new` | `vault.md`（启动顺序 `Config→Storage→Vault`） | 协同 | 不得改变对象存储相对 vault 的构造时机 |
| `ObjectStorageConfig` 仍来自 `orbit_api`（不动） | `config.md`（config re-export + 对象存储校验） | 协同 | config 的对象存储字段/校验语义不变 |
| 行为不变验证 | `integration.md`（对象存储 / 服务启动场景） | 后置 | 用本地后端 + `service http` 烟雾验证重构前后一致 |
| 阶段 3 拆 crate | 无外部前置；属结构性拆分 | — | 需一次独立可评审的 PR |

> 本重构**不被**任何其他文档阻塞，也**不引入**新的跨模块循环依赖；它只收窄 monoengine 对外部 orbit 生态的依赖面。

## 风险与约束

- **风险：单 crate 形态导致阶段 1–2 不减少依赖图。**
  - 影响：误判"已只依赖 orbit-api"，实际 `object_store`/云 SDK 仍编译。
  - 缓解：明确只有阶段 3（lib 拆分）才移除依赖；以 `cargo tree -p monoengine-core` 作为目标达成门禁。
- **风险：穿参数扰乱启动顺序。**
  - 影响：init 顺序 bug（如 vault 早于 storage、对象存储构造时机错位）。
  - 缓解：在 2 个调用方中紧接 `AppContext::new` 之前构造 `object_store`；保留"`Storage::new` 连库→使用注入的对象存储"；用现有启动测试断言顺序。
- **风险：阶段 3 拆 crate 需把大量私有 `mod` 改 `pub`，暴露内部耦合。**
  - 影响：机械改动量大，可能出现可见性 / 循环问题。
  - 缓解：阶段 1–2 落地且测试绿后再做阶段 3；增量拆分，仅暴露最小 pub 入口；作为独立 PR。
- **风险：给 `orbit-api` 加 factory trait 时顺手加实现/重依赖。**
  - 影响：`orbit-api` 不再 API-only，重构目的落空。
  - 缓解：优先"注入值"而非"注入 factory"；若确需 factory trait，保持 trait-only，并在评审中禁止 `orbit-api` 增加重依赖。
- **约束：消费端 18 处 `orbit_api::` 引用不得改动。** 理由：它们是目标依赖；改动会扩大变更面、增加回归风险。

## 改进方案多维评估小结

| 维度 | 评估结论 |
|-----|--------|
| **合理性** | **高（9/10）**。准确抓住"唯一实现 crate 触点 = 1 处工厂调用"且"消费端已全是 orbit-api trait object"的事实，重构本质是把一次构造调用下沉、并（可选）拆出 lib 边界。方向与现有抽象高度吻合。 |
| **可行性** | **中高（8/10）**。阶段 1–2 改动面极小（参数穿透 + 单文件隔离），低风险；阶段 3 是机械但量大的 crate 拆分，风险集中在可见性与构建配置。 |
| **完整性** | **中高（7.5/10）**。"只依赖 orbit-api"目标只有在阶段 3 才真正达成；阶段 1–2 是必要前置但本身不减依赖。待决策项（拆 crate、impl 落点、是否发布 orbit-api）需人决策后方可完全闭环。 |
| **安全性** | **高（8.5/10）**。不触碰对象存储构造逻辑与凭据处理，仅移动调用位置；启动顺序硬约束保持；不泄露具体实现类型进 core。 |
| **功能正确性** | **高（9/10）**。运行期对同一 `build` 的调用不变，4 后端行为应逐字节一致；mock 路径已证明注入可行。需以集成测试守住"行为不变"。 |
| **兼容性** | **中高（7.5/10）**。`orbit-api` API 面不变（推荐方案不加 factory trait）；消费端不改。阶段 3 的 crate 拆分改变构建拓扑，需更新 CI（`config-validation.yml` 已 checkout orbit sibling，应同步覆盖 core/bin 两 crate）。 |
| **可扩展性** | **高（8.5/10）**。注入边界一旦建立，可在二进制 / adapter crate 中替换不同对象存储实现（含测试用内存实现），并为"多后端 / 可插拔存储"打开空间。 |

## 待决策项（决策结果）

1. ✅ **是否执行阶段 3 的 crate 拆分？** —— **已执行**。拆为 `monoengine-core`（lib，仅 orbit-api）+ `monoengine`（bin，含 orbit 实现）。`cargo tree -p monoengine-core` 已无 `object_store`/orbit-impl。
2. ✅ **拆分后 orbit 实现依赖落在哪里？** —— **直接放进 `monoengine` 瘦二进制 crate**（`bin/`），未单列 adapter crate。`OrbitObjectStorageProvider` 即在 `bin/src/main.rs`。若日后有第二个二进制需复用，可再抽出 `orbit-bootstrap` adapter crate（增量改动）。
3. ✅ **注入形态：** —— **注入"已构造好的值" + 进程级 `ObjectStorageProvider` 注册表**（见上"已落地实现说明"）。**未**给 `orbit-api` 增加 factory trait，orbit-api 零改动。
4. ⬜ **`orbit-api` 来源：** —— **仍为 path 依赖**（`monoengine-core` → `../orbit/api`，`bin` → `../../orbit` + `../../orbit/api`）。是否发布 `orbit-api` 到 registry / 内部 git 以获得真正的跨仓库构建隔离，**仍为开放决策**（与本次模块/依赖卫生正交，可后续单独推进）。
5. ✅ **`chat_migrate` 二进制路径：** —— **已一致更新**。`commands/service/mod.rs:40` 与 `commands/chat_migrate.rs:114` 均直接调用 `AppContext::new(config)`；对象存储构造留在 `AppContext::new` 内部，通过同一 `ObjectStorageProvider` 注册表构造，保证 `service` 与 `chat-migrate` 使用一致的真实对象存储。

## 小结

> **本计划已于 2026-06-19 完整实现（阶段 0–3）。** monoengine 现为 workspace：`monoengine-core`（lib，仅依赖 `orbit-api`）+ `monoengine`（bin，注入 `orbit` 实现）。`cargo tree -p monoengine-core` 不再含 `object_store`/云 SDK/orbit-impl；所有门禁（fmt / clippy `-D warnings` / 530 单测 / 4 集成测试 / 二进制烟雾）通过。唯一开放项是是否将 `orbit-api` 发布为带版本依赖（待决策项 #4）。

monoengine 对 orbit 的"实现 crate 项目引用"实际上**只落在一处**——`Storage::new` 中对 `orbit::factory::ObjectStorageFactory::build` 的调用；其余对象存储交互**已经**全部走 `orbit-api` 的 trait/config/wrapper 抽象，消费端零具体类型耦合。因此把依赖重构为"API 方式"的核心动作很小：把对象存储改为由调用方**注入**已构造好的 `MegaObjectStorageWrapper`（orbit-api 类型），并把唯一的工厂调用隔离进单一 bootstrap 模块。但由于 monoengine 是**单一二进制 crate**，要真正从核心编译中移除 `object_store` + 云 SDK，还需把代码拆为 `monoengine-core`（仅依赖 orbit-api）+ 瘦二进制 / adapter crate（持有 orbit 实现依赖）——这是达成目标的承重决策。

## 预期收益

- **核心代码与对象存储实现解耦**：`monoengine-core` 只依赖稳定的 `orbit-api` 契约，可独立编译、测试与演进；切换 / 新增对象存储实现不再触碰核心。
- **核心编译显著瘦身**（阶段 3 后）：从 core 编译图移除 `object_store {cloud,aws,gcp}` 与 AWS/GCP 云 SDK，缩短编译时间、减少依赖攻击面。
- **可插拔与可测试性提升**：注入边界使内存实现 / 替身存储可直接用于测试与本地运行，`mock_object_storage()` 模式被推广为一等公民。
- **依赖卫生与边界清晰**：`grep orbit:: src/` 收敛到单一 bootstrap 点（阶段 2）乃至 core 内为 0（阶段 3），消除"项目引用"式的隐式强耦合。

## ObjectNamespace 契约清单

`ObjectNamespace` 变体的 **字符串形式**是稳定契约（实现：`src/orbit_api/object_storage.rs`；变更须走兼容性文档）。现行清单：

| Variant | `as_str()` | 用途 |
|---|---|---|
| `Git` | `git` | Git 对象字节 |
| `Lfs` | `lfs` | Git LFS 对象 |
| `Log` | `log` | 日志段 |
| `Artifact` | `artifact` | 构建产物 |
| `Attachment` | `attachment` | 附件 |
| `Oci` | `oci` | OCI Distribution 字节（`blobs/` / `manifests/` / `uploads/`；键布局见 [`oci.md`](./oci.md)，plan-20260902 / DR-04+DR-14） |
| `Media` | `media` | FastCDC Media 对象（`docs/refactoring/fastcdc-media.md`；plan-20260901 FC-03） |

## Bounded write（2026-09-12，plan-20260901 FC-04）

`MegaObjectStorage::put_stream_bounded` 是 **不缓冲整对象** 的写入入口：

- 默认实现明确返回 unsupported，**不 poll** 输入 stream（避免不支持的 backend 把全流读进内存）。
- `ObjectStoreAdapter` 覆盖为强制 multipart，**忽略** 配置的 `UploadStrategy::SinglePut`。
- multipart 聚合成固定 **8 MiB** part，最后一块可更小；stream 错误或 `complete` 失败时 `abort`，不发布半成品。
- 既有 `put_stream` 策略不变（Git/LFS/Artifact/Attachment 仍走原来的 SinglePut/Multipart/idempotent 分支）。

