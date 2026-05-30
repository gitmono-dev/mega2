# Config 实现方案分析

本文档记录 `monoengine` 中 `Config` 的实现方案、加载链路、运行时注入方式、主要消费点与当前实现中的注意事项。

## 总体设计

`Config` 是系统的强类型配置中心，核心定义在 `src/common/config.rs`。配置先由 `ConfigLoader` 定位并准备配置文件，再通过 `Config::new` 读取 TOML 文件、叠加环境变量、完成占位符替换，最终反序列化为一组按领域划分的配置结构。

启动后，配置会被包装进 `Arc<Config>` 并注入到 `AppContext` 中。下游服务、存储层和后台任务通过 `AppContext`、`Storage` 或直接传参消费配置，从而避免在业务代码中重复解析配置文件。

## 启动与加载链路

配置加载的主链路如下：

```text
src/main.rs
  -> cli::parse(None)
  -> ConfigLoader::load
  -> Config::new                 # 同步函数，仅做文件读取 + env 叠加 + 占位符展开 + 反序列化
  -> commands::builtin_exec / service 子命令
  -> AppContext::new             # 这里才构造 Storage 与 VaultCore
  -> server / storage / task 等运行时组件
```

关键职责分布：

- `src/main.rs`：程序入口，调用 CLI 解析与执行。
- `src/cli.rs`：解析全局参数和子命令，初始化日志，并触发子命令执行；`Config::new` 在此被调用（`src/cli.rs:37`）。
- `src/common/config/loader.rs`：负责定位配置文件，必要时生成默认配置。
- `src/common/config/template.rs`：提供默认配置模板。
- `src/common/config.rs`：定义强类型配置模型，并实现 TOML、环境变量与占位符处理。
- `src/context/mod.rs`：构建运行时上下文，将 `Arc<Config>` 注入系统，并在此构造 `VaultCore`（`src/context/mod.rs:33`）。
- `src/jupiter/storage/mod.rs`：存储层保存对配置的弱引用，并向具体存储组件提供配置访问能力。

> 注意：`Config::new` 是**同步**函数（`src/common/config.rs:111`），且只依赖文件系统读取，不访问数据库或网络。这一事实对后文“敏感数据 Vault 化”的可行性有决定性影响，见下文「现有 vault 能力与关键约束」。

## 配置文件定位优先级

`ConfigLoader` 会按以下顺序寻找配置文件：

1. 命令行参数 `--config` 指定的路径。
2. 环境变量 `MEGA_CONFIG` 指定的路径。
3. 当前工作目录下的 `config/config.toml`。
4. `mega_base()/etc/config.toml`。
5. 如果上述路径均不可用，则生成默认配置文件后再加载。

README 中主要描述了前三种常见方式；实现层面还包含 `mega_base()/etc/config.toml` 和自动生成默认配置的兜底逻辑。

## 配置合并与反序列化

`Config::new` 使用 `config` crate 作为底层解析能力，核心流程为：

1. 读取 TOML 配置文件。
2. 叠加以 `MEGA_` 为前缀的环境变量。
3. 将双下划线环境变量映射为嵌套字段，例如 `MEGA_LOG__LEVEL` 可覆盖 `log.level`。
4. 对部分列表类字段进行逗号分隔解析（如 `oauth.allowed_cors_origins`、`monorepo.admin`、`monorepo.root_dirs`）。
5. 在反序列化前执行字符串占位符替换。
6. 将最终配置反序列化为强类型 `Config`。

占位符替换主要用于让配置值引用基础目录等公共路径，例如 `${base_dir}`。该机制工作在字符串值上，适合路径类配置复用，但不适用于非字符串字段。

> 实现痛点：占位符替换函数 `variable_placeholder_substitute`（`src/common/config.rs:194`）内部存在密集的 `.unwrap()`（`build().unwrap()`、`substitute(...).unwrap()`、`collect().unwrap()`、`try_unwrap(...).unwrap()` 等）。任何不合法的配置都会直接 panic 而非返回可诊断错误。这是后文「错误模型集中化」的主要动机。

## 强类型配置结构

`Config` 按功能域拆分为多个子配置，覆盖系统运行所需的主要能力，包括：

- `log`：日志输出方式、级别、ANSI 颜色、文件滚动等。
- `database`：SeaORM 数据库连接配置（注意：当前默认 `db_url` 中内嵌了用户名/密码，见敏感数据章节）。
- `monorepo`：monorepo 根目录、导入目录和 Git 相关路径。
- `pack`：Git pack 解码和对象处理相关参数。
- `lfs`：Git LFS 存储与传输相关配置。
- `blame`：blame 计算相关参数（`BlameConfig`，`src/common/config.rs:693`）。
- `object_storage`：本地文件系统、S3/S3 兼容服务、GCS 等对象存储后端配置。
- `oauth`：OAuth 登录、回调、客户端和 CORS 相关设置。
- `build`：构建触发、外部 Orion 构建服务等配置。
- `redis`：缓存、队列、连接管理相关配置。
- `buck`：Buck 文件上传、限流、清理等配置（已存在局部 `BuckConfig::validate()`，`src/common/config.rs:897`）。
- `orion_server`：外部构建服务地址等配置。
- `sidebar`：侧边栏默认种子数据相关配置。
- `mail`：SMTP、发信人、通知投递等邮件配置（含明文 `password` 字段，`src/common/config.rs:293`）。
- `artifacts_gc`：构建产物垃圾回收相关配置。

这种拆分方式让上层调用方可以只依赖自己需要的配置域，同时保持配置文件和 Rust 结构之间的一一对应关系。

`Config` 已提供若干测试/构造入口：`Config::mock()`、`Config::load_str()`、`Config::load_sources()`（`src/common/config.rs:130/151/165`）。后续测试辅助应在这些既有入口之上扩展，而不是另起一套。

## 运行时注入与访问方式

配置在启动期完成一次加载，运行时主要通过以下方式流转：

- CLI 层将加载完成的 `Config` 传给子命令执行器。
- 服务类子命令基于 `Config` 构建 `AppContext`。
- `AppContext` 持有 `Arc<Config>`，供 HTTP 服务、存储层、任务调度等共享读取。
- `Storage` 保存配置，需要时通过 `config()` 获取运行时配置（返回 `Arc<Config>` 克隆，调用点遍布 `http_server.rs`、`ssh.rs`、`import_repo.rs`、`smart.rs`、`monorepo.rs` 等数十处）。
- 部分底层初始化逻辑会直接接收配置片段，例如数据库、Redis、对象存储和日志初始化。

整体上，配置对象以只读方式在运行期共享，没有在业务流程中动态重载配置的机制。

## 主要消费场景

当前代码中配置的主要消费点包括：

- 日志初始化：决定输出到 stdout 还是滚动文件，设置过滤级别与 ANSI 行为。
- 数据库初始化：选择数据库连接字符串、连接池等参数，并配合 migration 使用。
- Redis 初始化：为缓存、队列和异步任务提供连接配置。
- 对象存储初始化：根据配置选择本地、S3/S3 兼容或 GCS 后端。
- Monorepo 初始化：提供仓库路径、导入目录、对象目录等核心路径。
- HTTP 服务：读取监听地址、端口、OAuth、CORS、Swagger/OpenAPI 等相关配置。
- Git pack / LFS：控制对象解码、上传、存储和传输行为。
- 构建系统：配置 Orion 构建服务、触发器和构建产物管理。
- Buck 上传：读取上传限制、清理策略和相关后台任务参数。
- 邮件通知：配置 SMTP、发件人和通知调度行为。
- Artifact GC：控制构建产物垃圾回收策略。
- Sidebar 默认数据：为 UI 侧边栏提供初始化种子配置。

### 当前消费点的关键依赖顺序（与 secret 迁移相关）

在评估哪些字段可以改为 `SecretRef` 时，不能只看配置结构，而必须精确追踪消费点在初始化链路中的位置：

1. **`Storage::new`（`src/jupiter/storage/mod.rs:159`）** 在 `AppContext::new` 中第 27–29 行被调用，它内部：
   - 第 160 行 `database_connection(&config.database)` 建立数据库连接；
   - 第 175 行 `ObjectStorageFactory::build(&config.object_storage)` 构造对象存储（S3/GCS/Local），因此 `object_storage.s3.access_key_id` / `secret_access_key` 在 vault 就绪前已被消费；
   - 第 212–222 行 `buck_config.validate()` 若失败会直接 `panic!`，这是当前配置校验侵入启动期的一个负面典型，后续应收敛到 `validate.rs` 的集中校验中。
2. **`init_connection(&config.redis)`（`src/context/mod.rs:30`）** 在 `VaultCore::new` 之前执行，因此带密码的 `redis.url` 也属于早期运行时依赖。
3. **`VaultCore::new(storage)`（`src/context/mod.rs:33`）** 之后才就绪，此后消费的配置字段才可纳入可迁移凭据。
4. **SMTP mailer 构造** 发生在 HTTP 服务启动后的 handler 中（如 `src/email/` 相关模块），远在 vault 就绪之后，因此 `mail.password` 是第一批可迁移字段。
5. **`ssh_server_key`（`src/server/ssh_server.rs:78/100`）** 已由 vault 管理，但属于 vault 内部 secret，不在 `Config` 结构体中。

> 结论：任何在 `Storage::new` 或 `init_connection` 阶段消费的字段都不能直接改为 `SecretRef`，除非先重构初始化顺序。

## 当前方案的优点

- 启动链路清晰：配置在 CLI 入口统一加载，然后传入后续运行时组件。
- 类型安全：业务代码面对的是 Rust 结构体，而不是散落的字符串键。
- 配置来源灵活：支持命令行、环境变量、项目默认配置和自动生成模板。
- 运行时共享成本低：通过 `Arc<Config>` 只读共享，避免重复解析和复制。
- 领域边界清楚：配置按 `log`、`database`、`redis`、`mail` 等领域拆分，便于维护。
- 适合容器化部署：`MEGA_` 环境变量覆盖机制便于在部署环境中注入差异配置。

## 现有 vault 能力与关键约束

后文的“敏感数据 Vault 化”方案高度依赖现有 `vault` 模块的真实形态，因此先澄清现状，避免规划脱离实现：

- **vault 已是可用的通用 secret store。** `src/vault/integration/vault_core.rs` 基于 `libvault_core`（RustyVault），已提供 `read_secret`/`write_secret`/`delete_secret`（按 `secret/<name>` 路径存取任意 KV）。因此“把凭据写入 vault”所需的 API **已经存在**，不必新建存储能力。
- **vault 的存储后端就是数据库。** `VaultCore::new(ctx: Storage)` 通过 `JupiterBackend` 把 secret 存进数据库（`ctx.vault_storage()`）。这意味着 vault 必须先有可用的数据库连接才能启动。
- **由此形成明确的依赖链：`Config → Storage(数据库) → Vault`。** `Config::new`（同步）先产出配置 → `Storage::new(Arc<Config>)` 用 `database` 连库 → `VaultCore::new(storage)` 起 vault。**vault 在 `AppContext::new` 阶段才就绪，晚于 `Config::new`。**
- **vault 当前只存了一个 secret：`ssh_server_key`**（`src/server/ssh_server.rs:78/100`）。把全系统凭据集中进来是一次能力跃迁，需配套引导、备份与解封密钥保护，详见风险约束。
- **vault 通过磁盘上的明文 `core_key.json` 自动解封。** 解封分片 `secret_shares` 与 `root_token` 以明文 JSON 写入 `mega_base()/vault/core_key.json`（`vault_core.rs:88-99`），启动时读取该文件自动解封。**因此把 secret 搬进 vault，对“能访问该磁盘/文件系统的攻击者”几乎不增加保护**——读 `core_key.json` 即可解封读全部。
- **`core_key.json` 丢失会触发 `delete_all()` 清空并重新初始化所有 secret**（`vault_core.rs:70-77`）。集中所有凭据后，丢一个本地文件等于丢全部凭据。

这两条安全/引导事实直接约束了下文方案的边界，必须在规划中显式承接，而不是当作注脚。

## Config 单独成模块的改进方案

当前 `Config` 的类型定义集中在 `src/common/config.rs`（约 1168 行），而加载器和模板已经位于 `src/common/config/` 目录下。随着配置域继续增加，单文件会同时承担数据模型、解析流程、占位符处理、环境变量适配、默认模板协作、配置校验和错误诊断等职责，后续维护成本会逐步升高。建议将 `Config` 收敛为 `src/config/mod.rs` 这样的顶层目录模块，让配置能力从 `common` 中独立出来，成为系统第一级基础设施模块。

这一改造不只是文件拆分，也应承接当前实现中的注意事项：占位符替换规则需要显式化，配置校验需要集中化，启动期错误需要可诊断化，文档与实际加载优先级需要同步，部分敏感数据可逐步从普通配置文件中剥离并交由 `vault` 模块存储，并在独立配置模块中落地受控热加载能力。

### 总体改造原则

考虑到模块规模（1168 行、约 12 个文件引用 `common::config`）以及上文揭示的 vault 引导/安全约束，本计划采用**分阶段、可独立验证**的策略，而不是一次性大爆炸切换。以下原则贯穿全程：

1. **拆分与迁移解耦。** 结构拆分阶段保留 `crate::common::config` 的 `pub use` 兼容 re-export shim，让“拆文件”与“改调用方路径”成为两个独立、各自可编译可回归的步骤；调用方全部迁移完成后再删除 shim。拒绝兼容入口本身没有收益，只会把不可评审的巨型 diff 强行绑在一起。
2. **区分“引导配置 / 早期运行时依赖”与“可迁移凭据”。** 数据库连接（以及 vault 自身启动所依赖的一切）属于**引导配置**，必须留在 TOML/环境变量中（明文或由 env 注入），**永远不能成为 vault SecretRef**——因为 vault 存在数据库里，连库才能起 vault。Redis URL、当前 `Storage::new` 阶段构造的对象存储凭据，也属于 vault 就绪前或同时期会被消费的**早期运行时依赖**；除非先重构初始化顺序，否则也不能直接改成 SecretRef。只有在 vault 就绪后才被使用的凭据（例如当前邮件发送器密码，以及未来新增且确认为后置消费的 OAuth/第三方服务 secret）才是“可迁移凭据”。
3. **secret 解析是 vault 就绪后的独立异步阶段，不在 `Config::new` 内。** `Config::new` 同步且 vault 尚未就绪，无法在加载流水线内解析 secret。`Config::new` 只产出**未解析的 `SecretRef`**；服务运行时的真实值由 resolver 在 `AppContext` 中的 vault 就绪后、按消费端依赖顺序异步解析。
4. **`config secret` 命令不能依赖完整 `AppContext`。** 当前 `AppContext::new` 会先构造完整 `Storage`、对象存储、Redis 连接和部分服务，再创建 `VaultCore`。如果 `config secret set/check` 复用完整 `AppContext`，写入邮件密码也可能被 Redis、S3 或 monorepo 初始化阻塞。此类命令应使用最小 DB/Vault bootstrap：只建立 vault 所需的数据库能力和 `VaultStorage`，不初始化 Redis、对象存储、HTTP/SSH 服务或后台任务。
5. **Vault 加固是 SecretRef 生产化迁移的硬前置。** 在 `core_key.json` 仍会明文落盘、root token 仍可能输出、key 缺失仍会触发 `delete_all()` 的状态下，SecretRef 的收益只能限定为“不进仓库/普通配置/日志”。在迁移更多可迁移凭据并用于生产前，必须先完成 fail-closed、权限收紧、脱敏和备份恢复方案。
6. **`config` 命令需要先改 CLI 启动模型。** 当前 `cli::parse` 在分发任何子命令前都会先完成 `ConfigLoader::load` 与 `Config::new`。因此 `config init`、`config validate` 这类命令如果要在无配置、坏配置或裸机环境下可用，必须先支持“两阶段 CLI”：先解析子命令，再按命令声明决定是只需要配置路径、需要未解析 source、需要完整 `Config`、需要最小 DB/Vault bootstrap，还是需要完整 `AppContext`。

### 目标

- 将配置相关职责从单文件拆分为多个小模块，降低 `src/common/config.rs` 的复杂度，并最终移除 `common` 对配置模块的直接承载职责。
- 将对外 API 收敛到顶层 `crate::config::Config`、`crate::config::loader::ConfigLoader` 等路径；**过渡期保留 `crate::common::config` re-export shim**，待所有调用方迁移完成后在同一阶段删除，避免新旧入口长期并存。
- 为集中校验、错误诊断、占位符规则、环境变量规则测试提供完整实现；受控热加载作为**独立后续阶段**落地，不与拆分/迁移捆绑。
- 将**可迁移凭据**与普通配置分离：这些字段在配置文件中只保存 `vault` 引用（`SecretRef`），真实值存储在 `vault` 模块中。**数据库凭据等引导配置不在此列**，继续随启动配置提供。
- 分阶段完成消费端改造：先完成路径迁移，再改 CLI 加载模型与 `config` 命令，最后让确认属于后置消费的可迁移凭据改用 secret resolver。不要求在单次变更中同时完成路径迁移、命令改造、secret 迁移与热加载订阅。
- 明确 `config/config.toml` 的定位：它不应继续作为“全项目测试时顺手使用的运行配置”，而应演进为可提交、可校验、无真实 secret、适合本地开发和 CI 参考的基础样例配置；自动化测试应使用独立的测试配置生成与覆盖机制。
- 改造 CLI 启动链路后新增 `monoengine config` 命令族，提供配置初始化、配置校验、可迁移凭据写入 `vault`、`SecretRef` 生成与解析检查等运维入口，其中 `config init` 负责先生成配置骨架和可迁移字段引用，再由 `config secret set` 写入这些**可迁移凭据**的真实值；数据库密码、Redis URL、当前阶段的对象存储 key 等引导或早期依赖不通过本项目 vault 写入。
- 将 README、`config/config.toml`、默认模板和配置实现保持同步，避免用户看到的加载优先级与实际行为不一致。

### 建议目录结构

可以将现有 `src/common/config.rs` 迁移为顶层 `src/config/mod.rs`，并在 `src/config/` 目录下按职责拆分：

```text
src/config/
├── mod.rs          # 对外入口与 re-export，保留 Config::new 等主 API
├── model.rs        # Config 及各领域子配置结构体
├── loader.rs       # 配置文件定位、默认配置生成，沿用现有 ConfigLoader
├── template.rs     # 默认 TOML 模板，沿用现有模板能力
├── source.rs       # 文件源、环境变量源、合并策略等解析前输入处理
├── expand.rs       # ${base_dir} 等占位符展开
├── secret.rs       # SecretRef 引用类型、Vault resolver 适配、最小 Vault bootstrap 和脱敏输出
├── init.rs         # 基础配置初始化、SecretRef 占位生成和初始化计划输出
├── testing.rs      # 测试配置模板、覆盖规则和自动化测试辅助能力
├── validate.rs     # 集中式 Config::validate() 与领域校验
├── reload.rs       # 受控热加载、变更检测、字段白名单和回滚策略（后续阶段）
└── error.rs        # 配置专用错误类型与字段路径诊断信息
```

其中 `mod.rs` 只承担编排和导出职责，例如对外暴露 `Config`、`ConfigLoader`、`ConfigError`，并隐藏内部的 `source`、`expand` 等实现细节。业务代码应改为通过 `crate::config::Config` 获取类型，使 `config` 成为与 `common`、`commands`、`context` 等并列的一级模块。

消费端改造分步推进：先只做**路径迁移**（`crate::common::config::*` → `crate::config::*`），不改变运行语义；之后再改 CLI 加载模型、secret resolver 和具体消费端。任何需要 `SecretRef` 的消费端都必须先通过依赖表确认其初始化晚于 vault。在路径全部迁移完成前，`crate::common::config` 通过 re-export shim 继续可用。

### 模块职责划分

- `model.rs`：只定义强类型配置模型和必要的默认值，不放文件读取、环境变量解析和运行时初始化逻辑。
- `loader.rs`：继续负责配置文件定位优先级，包括 `--config`、`MEGA_CONFIG`、项目默认路径、`mega_base()/etc/config.toml` 和默认文件生成。
- `source.rs`：封装 `config` crate 的 source 构建过程，明确 TOML 文件、`MEGA_` 环境变量、嵌套分隔符和列表字段解析规则。
- `expand.rs`：集中处理占位符展开，定义支持哪些占位符、展开顺序、未知占位符的处理策略，以及是否允许递归展开；同时明确占位符只作用于字符串值，新增占位符前必须确认字段类型和替换顺序。这里也是消灭 `variable_placeholder_substitute` 现有 `.unwrap()` panic 的落点。
- `secret.rs`：定义 `SecretRef` 引用类型与 secret resolver trait，适配现有 `VaultCoreInterface`（`read_secret`/`write_secret`），并统一提供脱敏日志、错误信息和审计字段。服务运行时 resolver 接收一个已就绪的 vault 句柄（即 `AppContext` 中的 `VaultCore`），**不在 `Config::new` 阶段调用**；`config secret set/check` 则应通过最小 DB/Vault bootstrap 获取 vault 句柄，不能为了读写 secret 构造完整 `AppContext`。
- `init.rs`：提供基础配置初始化能力，负责生成 `config/config.toml` 样例、派生本地目录、填充非敏感默认值、为可迁移凭据生成 `SecretRef` 占位引用，并输出后续需要执行的 `config secret set` 命令清单；真实 secret 不在该阶段写入配置文件。
- `testing.rs`：在既有 `Config::mock()`/`load_str()`/`load_sources()` 之上，提供面向自动化测试的配置模板、临时目录替换、端口和外部依赖覆盖、secret resolver 测试替身、测试配置校验入口，以及与 `.env.test` 协作的辅助函数，避免测试直接复用或修改仓库中的基础配置文件。
- `validate.rs`：提供集中校验入口，按领域拆分校验函数，例如数据库连接串、监听端口、对象存储后端、OAuth 回调地址、路径可用性等，避免非法配置延迟到后续初始化阶段才暴露。首批 hard error 应只覆盖无争议规则，例如 `database.db_type` 与 `database.db_url` scheme 一致性、端口范围、必填字符串、对象存储后端与对应配置完整性、`mail.enabled = true` 时的必填字段、`password`/`password_ref` 互斥、Buck 并发/大小限制等；生产环境下 Postgres 自动 fallback 到 SQLite 应要求显式开关或至少高亮告警，避免误用本地 SQLite 启动。
- `reload.rs`（后续阶段）：实现受控热加载能力，负责监听配置来源变化、复用完整加载流水线生成新配置、计算允许热更新字段的差异、通知订阅组件应用变更，并在校验失败或组件应用失败时保留旧配置。
- `error.rs`：提供配置专用错误，包含配置文件路径、字段路径、原始值、失败原因和修复建议，再统一转换为现有 `MegaError`/`MegaResult`；配置加载路径上的 `unwrap`、`expect` 和 `panic` 应逐步收敛到该错误模型中。

### 多环境 Profile 与继承机制

为了支持开发（dev）、测试（test）、预发（staging）和生产（prod）等不同环境下的配置管理，配置加载器应原生支持 **Profile** 机制：
1. **启动参数与环境变量识别**：通过命令行参数 `--profile <name>` 或环境变量 `MEGA_PROFILE=<name>` 传入当前 Profile（默认为空，即不使用 Profile 继承）。
2. **多文件合并加载与覆盖优先级**：
   - 首先加载基础配置文件 `config/config.toml`。
   - 如果指定了 Profile（例如 `prod`），加载器会尝试寻找并读取 `config/config.prod.toml`（或通过 `--config` 指定路径的同级目录下的 `config.<profile>.toml`）。
   - 将 Profile 特定的配置以“深合并（Deep Merge）”的形式叠加到基础配置之上。
   - 最终叠加 `MEGA_` 前缀的环境变量。
3. **环境隔离规范**：Profile 文件只应声明与基础配置不同的增量部分（如日志级别、外部依赖的端点或缓存开关），避免配置冗余。所有环境的配置均需通过 `validate.rs` 进行语义校验。

Profile 机制需要先固定以下语义，避免“配置能合并但含义不确定”：

- `--profile <name>` 的优先级高于 `MEGA_PROFILE=<name>`；未指定时不加载 profile 文件。
- 当 `--config /path/app.toml --profile prod` 时，profile 文件固定为同目录下 `/path/app.prod.toml`；当使用默认 `config/config.toml` 时，对应 `config/config.prod.toml`。
- 深合并中标量和 table 按字段覆盖，数组默认**整体覆盖**而不是追加，避免 CORS、admin、root_dirs 等列表在不同环境中意外叠加。
- Profile 文件不能隐式删除基础配置字段；需要表达“关闭”时应使用显式布尔开关或空数组。
- SecretRef 路径应包含 profile 或部署命名空间，例如 `vault://secret/config/prod/mail/password#value`，避免 dev/staging/prod 误读同一份 secret。

### 配置结构演进与兼容性规范

随着系统的迭代，配置结构体（`Config`）的字段和层级不可避免地会发生演进。为了防止老版本配置文件导致新服务无法启动，必须遵循以下兼容性设计规范：
1. **新增字段必须向下兼容**：
   - 任何新增的字段必须要么是 `Option<T>` 类型，要么使用 `#[serde(default)]` 声明默认值（或者使用 `#[serde(default = "path::to::default_fn")]` 指定特定的默认值生成函数）。
   - 禁止在未提供默认值的情况下，直接在现有结构体中加入必填的非 `Option` 字段。
2. **废弃字段的优雅降级**：
   - 对于需要淘汰的配置字段，在第一阶段应标记为 `#[deprecated]`，并保留在模型中。
   - 在加载流水线中检测到这些废弃字段被使用时，输出 `WARN` 日志提示用户迁移，但不拒绝启动。
   - 在至少经过一个主要版本（Major Version）的过渡后，才在代码和模型中彻底移除该字段。
3. **结构体拆分与重命名**：
   - 当字段名称或层级发生重构时，应该通过自定义的反序列化器（Deserializer）兼容旧字段，直到兼容期结束。
   - 避免直接修改已有字段的类型（例如将 `u16` 改为 `String`），应通过新增字段并对旧字段做映射来保证平滑迁移。

### 敏感配置与 Vault 存储方案

改造后应区分四类字段，而不是笼统地“把敏感数据搬进 vault”：

1. **引导配置（必须留在 TOML/env，永不进本项目 vault）。** 典型是 `database`（连接地址、用户名、密码）。当前默认 `db_url = "postgres://mega:mega@localhost:5432/mega"` 把密码内嵌在连接串中。由于 vault 存在数据库里、连库才能起 vault，**数据库密码无法作为 vault SecretRef**——这是不可破的引导循环。这类凭据应通过环境变量注入（如 `MEGA_DATABASE__DB_URL` 或拆分后的 `MEGA_DATABASE__PASSWORD`），由部署平台的 secret 机制（K8s Secret、CI secret store 等）保护，**而不是交给本项目的 vault**。
2. **早期运行时依赖（当前也不能直接进 vault）。** 这类字段不是数据库引导项，但在 vault 就绪前或同一初始化阶段已经被消费。当前 `Storage::new` 在 `VaultCore::new` 之前构造对象存储，因此 `object_storage.s3.access_key_id`/`secret_access_key` 暂时不能直接改为 SecretRef；`AppContext::new` 在 vault 前连接 Redis，因此带密码的 `redis.url` 也应按引导/部署平台 secret 处理。若要让对象存储凭据进 vault，必须先把初始化顺序拆成“DB-only Storage -> Vault -> resolve object storage secrets -> 构造完整 Storage/服务”。
3. **可迁移凭据（vault 就绪后才被使用，可改为 SecretRef）。** 典型是当前 HTTP 服务启动后才构造的 SMTP mailer 密码；未来如果新增 OAuth client secret、第三方 API key 等字段，只有确认其消费点晚于 vault 且不会阻塞 `AppContext` 构造，才可纳入此类。这些字段在配置文件中改为 `SecretRef`，真实值写入 vault，由消费端在运行期通过 resolver 读取。
4. **非敏感运行参数。** 维持现状，明文留在 TOML。

`Config` 反序列化阶段只构建强类型 `SecretRef`，校验阶段检查引用格式与必填性；**真实 secret 的读取发生在 `AppContext` 起来、vault 就绪之后**，由统一的 secret resolver 负责，并由 resolver 负责缓存、过期、脱敏日志和读取失败诊断。

对于运维命令需要区分两条路径：服务运行时 resolver 在完整 `AppContext` 创建后使用；`config secret set/check` 和 `config validate --resolve-secrets` 则只应建立最小 DB/Vault bootstrap，不能因为写入一个后置凭据而强制初始化 Redis、对象存储或 HTTP 服务。

> 关于数据库连接串：可以把内嵌密码的连接串拆为“普通连接参数 + 密码”，以避免整串成为不可脱敏字符串。但拆出来的密码应走**环境变量/部署平台 secret**，**不是 vault SecretRef**。早前“把 db 密码也拆成 `password_ref` 交给 vault”的设想与引导循环冲突，已废弃。

建议在实现前维护一张字段分类表，作为每个迁移 PR 的依据：

| 字段 | 当前消费点 | 分类 | 迁移结论 |
| --- | --- | --- | --- |
| `database.db_url` / 拆分后的数据库密码 | `Storage::new` 建库连接 | 引导配置 | 只能走 TOML/env/部署平台 secret，不进本项目 vault |
| `redis.url` | `AppContext::new` 中 vault 前连接 Redis | 早期运行时依赖 | 若含密码，走 env/部署平台 secret；日志必须脱敏 |
| `object_storage.s3.*` | `Storage::new` 中 vault 前构造对象存储 | 早期运行时依赖 | 先保持 env/部署平台 secret；若要入 vault，需先重构初始化顺序 |
| `orion_server.db_url` | Orion server 配置 | 引导或独立服务配置 | 按该服务启动依赖单独判断，默认不假定可入 monoengine vault |
| `mail.password` | HTTP 服务启动后构造 SMTP mailer | 可迁移凭据 | 可改为 `SecretRef`，由 vault 就绪后解析 |
| `ssh_server_key`、PGP/Nostr 等现有 vault secret | 已由 vault 管理 | vault 内部 secret | 需先加固 `core_key.json` 和日志脱敏 |

#### SecretRef 类型设计草案

`SecretRef` 是配置文件中替代明文敏感值的引用类型。`Config::new` 阶段只校验其格式合法性，不读取真实值。

```rust
/// 配置文件中敏感字段的引用标识。
/// TOML 示例: `password_ref = "vault://secret/config/prod/mail/password#value"`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// 校验引用格式是否合法（不访问 vault）。
    /// 当前只支持 `vault://secret/<path>` 和 `vault://secret/<path>#<field>` 两种形式。
    pub fn validate_format(&self) -> Result<(), ConfigError> { ... }

    pub fn as_str(&self) -> &str { &self.0 }

    /// 用于脱敏输出，例如 `vault://secret/config/prod/mail/password#value` -> `vault://secret/config/prod/mail/***#value`
    pub fn redacted(&self) -> String { ... }
}

/// 消费端实际拿到的解析后值。
/// 由 secret resolver 在 vault 就绪后异步读取并缓存。
pub struct ResolvedSecret {
    pub value: String,
    pub version: Option<String>, // 支持后续 secret 版本轮换
}

/// Secret resolver trait，由 `AppContext` 阶段提供实现。
#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, reference: &SecretRef) -> Result<ResolvedSecret, MegaError>;
}
```

**与现有 Vault API 的路径映射**：现有 `VaultCoreInterface::read_secret(name)` 会访问 `secret/{name}`。因此 `vault://secret/config/prod/mail/password#value` 应解析为 `read_secret("config/prod/mail/password")`，再从返回的 KV map 中读取 `value` 字段；不带 `#field` 时默认读取 `value` 字段。resolver 不能把完整 URI 直接传给 `read_secret`，否则会产生 `secret/secret/...` 这类路径错误。`version` 字段首版只作为预留元数据，除非 vault 后端实际提供版本语义，否则不得在文档或 API 中承诺 KV v2 风格版本读取。

**与现有字段的过渡策略**：
- 对于 `mail.password` 这类当前为 `Option<String>` 的字段，迁移期允许两种形态共存：
  - `password = "plain_text"`（兼容期保留，输出 deprecation warning）
  - `password_ref = "vault://secret/config/prod/mail/password#value"`（推荐；profile/命名空间按部署环境替换）
- 反序列化时使用自定义 visitor 或 `#[serde(with = "...")]` 处理互斥逻辑：两者同时存在时为 hard error；两者都不存在时按原语义处理（`None` 或报错取决于字段必填性）。
- 完全迁移后，可移除 `password` 字段，仅保留 `password_ref`。

**缓存与过期**：
- resolver 内部维护内存缓存，默认 TTL 5 分钟；首版可优先使用 `tokio::sync::RwLock<HashMap<...>>`，只有在确认并发读写压力后再引入 `DashMap` 等新依赖。
- secret 读取失败时返回 `MegaError`，**不得**将明文或部分解密内容写入日志/错误信息。

#### 敏感数据包装类型与编译期脱敏保障

单纯在日志打印时进行“文本搜索与正则替换”是脆弱的，极易因为新增日志语句、Panic 信息、或第三方库的 Debug 格式化输出而发生遗漏。为了从编译期防止凭据泄露：
1. **引入敏感包装类型 (`SecretString`)**：所有被解析后的敏感字符串值（如 SMTP 密码、OAuth Client Secret、API 令牌）在消费端代码中均不得使用原始 `String`，而应由 `secrecy` 库提供的 `SecretString`（或自定义实现包装类）包裹。
2. **强制限制 Debug/Display 特征**：
   - 包装类型必须实现自定义的 `std::fmt::Debug` 和 `std::fmt::Display`，其输出恒为 `"[REDACTED]"` 或 `"***"`。
   - 绝不允许将敏感字段直接序列化为 JSON 或其他未加密明文格式。
3. **安全提取明文**：只有在真正需要传递给外部驱动（如 `lettre` SMTP 发信客户端）时，才显式通过 `.expose_secret()` 获取明文。这可以强制让安全评审人员在代码审查时只关注 `.expose_secret()` 调用点，大大降低凭据泄露风险。

`SecretString` 只能降低本项目代码中的误打印风险，不能阻止外部库在内部复制明文，也不能保证发送给 `lettre::Credentials` 后仍受包装类型保护。因此 `.expose_secret()` 调用点应尽量集中在外部驱动适配层，并通过测试确认错误信息不会回显凭据。新增 `secrecy`、`zeroize` 等依赖前，需要评估编译成本、API 侵入性和实际保护边界；若暂不引入外部依赖，也可以先实现一个最小自定义包装类型作为过渡。

#### 运行期 Secret 轮转与缓存失效策略

在凭据（如第三方 Token 或邮件密码）发生安全轮转时，如果仅依赖内存中 5 分钟的 TTL 缓存，可能会导致服务在轮转过渡期继续使用失效的凭据。必须提供主动的失效机制：
1. **主动缓存失效（Eviction）接口**：
   - `SecretResolver` 需公开 `evict(&self, reference: &SecretRef)` 和 `evict_all(&self)` 接口。
   - 在接收到特殊的运维信号时（例如向服务发送 `SIGHUP` 信号，或调用受限的内部 HTTP Admin 端点 `/admin/config/secrets/evict`），系统应调用此接口清除内存中的 Secret 缓存。
2. **两阶段轮转（Graceful Rotation）规范**：
    - resolver 可以支持同时识别“新凭据”和“老凭据”。在轮转期间，Vault 侧可同时保存两个版本的凭据（如通过哈希键 `current` 和 `previous` 标识）。
    - 是否在调用失败后自动回退到老凭据必须由具体消费端决定，不能作为所有 secret 的默认行为。对 SMTP、第三方 API Token 等外部系统，盲目 fallback 可能造成重复请求、账户锁定或审计混乱；首版应只提供 resolver 能力和操作规范，业务 fallback 逐场景评审。

#### 安全前提：日志与错误脱敏

在迁移更多 secret 之前，必须先修复现有日志/错误输出中的敏感信息泄露，否则“SecretRef 不进 TOML”的收益会被启动日志抵消。至少应完成：

- 数据库连接日志、Postgres fallback warning、Orion DB 日志中的 URL 脱敏，不能输出用户名密码；
- Redis 连接错误中的 URL 脱敏，不能在 panic 或错误信息中输出密码；
- Vault 初始化时不得打印或 debug 记录 root token、secret shares、完整 `core_key.json` 内容；
- 配置错误模型输出字段路径和失败原因即可，默认不输出原始敏感值；确需输出原始值时必须经过统一 redaction。

#### 安全前提：`core_key.json` 加固与自动解封方案

把可迁移凭据集中进 vault**只在凭据不再进入 git 仓库、日志、错误信息这一层面带来收益**。它**不能**抵御能读取部署机磁盘的攻击者，因为 vault 通过明文 `core_key.json` 自动解封。因此在把更多凭据迁入 vault 之前，必须先完成以下加固，否则该阶段不能用于生产：

1. **文件权限与路径安全**：
   - 必须将 `core_key.json` 文件的默认权限严格限制为仅所有者可读写（Unix 权限 `0600`，如 `chmod 600 core_key.json`），并且其上级目录也应收紧权限（`0700`）。
   - 在 Dockerfile 或容器打包脚本中，必须将 `core_key.json` 添加到 `.dockerignore` 中，绝对禁止其随镜像分发，并排除在所有自动化备份和集中式日志收集（如 ElasticSearch/Grafana Loki）的目录范围外。
2. **生产环境避免本地明文 key 文件**：
    - **Systemd 部署**：在 Linux 主机上，应利用 Systemd 的安全凭据机制（`LoadCredential=vault_key:/etc/keys/vault_key`）。在服务启动时，Systemd 会通过 `/run/credentials/...` 下的受限文件传递密钥材料，避免把 `core_key.json` 作为普通明文文件长期保存在应用数据目录。该机制仍依赖主机权限边界，不能抵御 root 或能读取凭据源文件的攻击者。
    - **Kubernetes 部署**：禁止使用应用本地持久化卷存储 `core_key.json`。可以将 Vault 的初始化 shares 作为 Kubernetes Secret 存储，并挂载为只读 Secret Volume 或通过外部 Secret Manager 注入；同时必须启用 etcd encryption at rest、限制 RBAC，并避免 `kubectl describe`、审计日志或 CI 输出泄露 secret。Kubernetes Secret 不是天然加密保险箱，不能在文档中承诺“无盘即绝对安全”。
3. **备份与灾难恢复（Disaster Recovery）演练**：
    - 由于 `core_key.json` 丢失会导致 `delete_all()` 清空全部 secret，这是系统的单点故障（SPOF）。
    - 必须建立离线的、加密的 `core_key.json` 备份机制（如利用外部多签 GPG 加密备份在离线冷存储中）。
    - 定期开展不依赖本地 `core_key.json` 的冷启动和手动解封（通过多份秘钥分片合并解封）演练。

4. **启动行为 fail-closed**：
    - `core_key.json` 缺失不应在普通服务启动或 `config secret check` 中自动 `delete_all()` 并重新初始化 vault。该行为必须改为 fail-closed：启动失败并提示执行显式的、带确认保护的 `vault init/reset` 或等价运维流程。
    - root token、secret shares 和完整 `core_key.json` 内容不得通过 `println!`、`tracing`、`Debug` 或错误链输出。`VaultCore::new`/`config` 应逐步改为返回 `Result`，让调用方给出可诊断错误，而不是在 vault 初始化路径上 `expect`/`unwrap`/`assert!`。

在加固完成前，安全收益仅限“secret 不进仓库/日志”，规划与文档表述应严格限定在这一范围，不得暗示“静态数据已被加密保护”。

#### 安全前提：环境变量注入的可见性

引导配置（数据库密码等）建议通过 `MEGA_*` 环境变量注入，但需意识到环境变量在以下场景仍可能泄露：

- `/proc/<pid>/environ` 在部分 Linux 配置、同 UID 进程、root 或容器逃逸场景下可能被读取；
- `systemd` journal 可能记录服务启动时的完整环境；
- 容器平台的 `docker inspect` / `kubectl describe pod` 可查看环境变量；
- CI/CD 日志在设置环境变量时可能回显。

**缓解措施**：
- 生产环境优先使用文件挂载 secret（如 K8s Secret volume、systemd `LoadCredential=`），再通过配置占位符 `${file:/path/to/secret}` 读取；该能力可在 `expand.rs` 中后续扩展，但首版仍先以环境变量为主。
- 在 systemd service 文件中设置 `ProcSubset=pid` 或 `PrivateProc=yes`（若可用）限制 `/proc` 暴露。
- CI 中设置 secret 时使用 `::add-mask::`（GitHub Actions）或等价机制防止回显。

#### 安全前提：config init 默认密码处理

当前 `DbConfig::default()` 和 `config/config.toml` 均包含硬编码密码（如 `postgres://mega:mega@...`）。`config init` 生成默认配置时必须避免生成可预测的默认密码：

- **数据库密码**：`config init` 不应在生成的 TOML 中写入默认密码；而是输出提示要求用户通过 `MEGA_DATABASE__DB_URL`、拆分后的数据库密码环境变量、文件挂载 secret 或部署平台 secret 机制注入。数据库密码**不得**通过 `config secret set` 写入本项目 vault。若必须提供本地开发默认值，应使用空字符串并在校验阶段报 `missing_database_url` 错误，或显式标注为仅本地开发可用。
- **其他引导/早期凭据**：Redis URL、当前阶段的对象存储 key 等同样不在 `config init` 结果中预设真实凭据，也不能默认生成本项目 Vault SecretRef；只保留空字符串、示例占位符或部署平台 secret 注入说明。只有确认晚于 vault 消费的字段（如 `mail.password_ref`）才可生成 `SecretRef` 占位符。
- **vault 自动初始化**：`VaultCore::new` 在 `core_key.json` 丢失时会 `delete_all()` 并重新初始化，这一行为不应被 `config init` 触发；`config init` 只操作配置文件，不接触数据库或 vault。

#### secret 热加载边界

敏感数据的热加载不等同于普通配置热加载。配置文件中的 `SecretRef` 变化属于配置变更，需要走 `reload.rs` 的白名单、校验和回滚流程；而 vault 内部 secret 值轮换由 vault 模块和 secret resolver 管理，业务组件只在下一次获取、缓存过期或收到明确轮换通知时读取新值。任何 secret 读取失败都不能把明文写入日志，错误信息只输出 secret 路径、版本、字段路径和失败原因。

### secret 解析的依赖顺序

由于 `Config → Storage(DB) → Vault`，secret 解析必须排在 vault 就绪之后，并按消费端对 vault 的依赖排序。当前服务启动的真实顺序更精确地说是：

```text
Config::new
  -> Storage::new(config)          # 建数据库连接、构造对象存储、初始化部分存储服务
  -> init_connection(redis)        # 连接 Redis
  -> VaultCore::new(storage)       # vault 才就绪
  -> HTTP/SSH/后台任务等继续初始化
```

因此，当前 `Storage::new` 或 Redis 初始化阶段已经消费的任何 secret，都不能直接改为 vault SecretRef。只适合先迁移在 vault 之后才初始化/使用的字段，例如 HTTP 服务里构造 SMTP mailer 时读取的邮件密码。

这条顺序只适用于服务启动。对于 `config secret set/check`、`config validate --resolve-secrets` 这类运维命令，目标顺序不应复用完整 `AppContext`，而应是：

```text
Config::new                         # 仍只产出含未解析 SecretRef 的 Config
  -> VaultBootstrapStorage::new      # 只建数据库连接和 vault_storage 所需能力
  -> VaultCore::new/bootstrap        # vault 就绪；不得初始化 Redis/S3/HTTP/后台任务
  -> SecretResolver::new(vault)
  -> secret set/check/resolve
```

`VaultBootstrapStorage` 可以是一个新的最小上下文，也可以是对现有 `Storage` 拆分后的 DB-only 子结构。关键约束是：读写 vault secret 不应依赖对象存储凭据、Redis 可用性、monorepo 初始化或服务端口绑定。

如果后续要让对象存储凭据也进入 vault，需要先把初始化顺序调整为类似下面的目标形态：

```text
Config::new                         # 产出含未解析 SecretRef 的 Config
  -> DbStorage::new(database 引导配置) # 只建 vault 所需的数据库能力
  -> VaultCore::new(db_storage)       # vault 就绪
  -> SecretResolver::new(vault)
  -> 解析对象存储/邮件等后置 SecretRef
  -> 完整 Storage / server / task 初始化
```

需要为消费端梳理一张“谁在 vault 就绪前就需要值”的依赖表：
- 在 vault 就绪前需要值的 → 只能是引导配置（数据库等），不得改为 SecretRef；
- 在 vault 就绪后才使用的 → 可改为 SecretRef，由 resolver 读取。

### `config` 命令与初始化流程

为降低配置初始化和敏感数据写入的使用门槛，建议新增顶层 `config` 子命令族。命令入口建议放在 `src/commands/config.rs` 或 `src/commands/config/mod.rs`，并在 `src/commands/mod.rs` 中注册到现有 `builtin()` 与 `builtin_exec()` 流程。

新增命令前必须先调整 CLI 执行模型。当前 `cli::parse` 会在 `matches.subcommand()` 之前执行完整 `Config::new`，这会导致两个问题：配置文件损坏时 `config validate` 无法运行；配置文件不存在时 `ConfigLoader` 会先自动生成默认配置，和 `config init` 的职责冲突。目标模型应先解析子命令，再按命令能力声明选择加载层级：

建议在命令注册层引入显式加载模式，而不是继续让 `builtin_exec` 只接受 `fn(Config, &ArgMatches) -> MegaResult`：

```rust
enum LoadMode {
    None,
    ConfigPath,
    RawSources,
    ParsedConfig,
    VaultBootstrap,
    FullAppContext,
}
```

CLI 先解析全局参数和子命令，再根据命令声明的 `LoadMode` 执行对应加载。这样可以保证 `config init` 不被坏配置拦截，`config validate` 能报告坏配置，`config secret set` 只建立 DB/Vault 最小上下文，`service http` 才构造完整 `AppContext`。

| 命令类型 | 需要的加载层级 | 说明 |
| --- | --- | --- |
| `config init` | 只需要目标路径和模板 | 配置不存在或损坏时也必须可运行 |
| `config validate` | 读取 source 并运行解析/校验流水线 | 需要能报告坏配置，而不是被 CLI 预加载拦截 |
| `config validate --resolve-secrets` | 完整 `Config` + 最小 DB/Vault bootstrap | 用于检查 SecretRef 是否可读，不应初始化 Redis/S3/HTTP |
| `config secret ref` | SecretRef 命名规则 | 只生成引用，不应依赖 DB/vault |
| `config secret set/check` | 完整 `Config` + 最小 DB/Vault bootstrap | 缺少 DB/vault 时输出前置步骤；不得构造完整 `AppContext` |
| `service ...` | 完整 `Config` + `AppContext` | 保持现有服务启动语义 |

命令族首版建议提供以下命令：

```text
monoengine config init
monoengine config validate
monoengine config secret set
monoengine config secret check
monoengine config secret ref
```

其中 `config init` 是初始化入口，负责生成配置骨架和敏感字段引用，但不写入真实敏感值；`config secret set` 负责把真实敏感值写入 `vault`；`config secret check` 负责检查配置中的 `SecretRef` 是否可解析且具备权限；`config validate` 负责校验普通配置、占位符、字段语义和可选的 secret 解析链路。

> **引导顺序约束（重要）：** `config secret set`/`check` 与 `validate --resolve-secrets` 都需要 vault，而 vault 需要可用的数据库。因此这些子命令必须先建立数据库连接、初始化/解封 vault，再读写 secret；在数据库尚未就绪的全新机器上**无法**直接执行 `secret set`。它们只能写入晚于 vault 消费的可迁移凭据，不能用于数据库密码、Redis URL 或当前阶段的对象存储 key。`config init` 与不带 `--resolve-secrets` 的 `config validate` 则只操作配置文件、不依赖 vault，可在裸机执行。命令实现应在缺少数据库/vault 时给出明确的前置步骤提示，而不是 panic，也不得为了访问 vault 构造完整 `AppContext`。

`config init` 的职责应保持清晰：创建或检查 `config/config.toml`，写入基础样例配置，派生 `${base_dir}`、日志目录、缓存目录和本地对象存储目录，为可迁移凭据生成 `SecretRef` 占位引用，并输出后续需要执行的 `config secret set` 命令清单。示例应尽量贴近当前 schema；当前 `MailConfig` 是扁平 `[mail]` 结构，因此第一批可迁移字段可以形如：

```toml
[mail]
enabled = true
smtp_host = "smtp.example.com"
smtp_port = 587
username = "monoengine@example.com"
password_ref = "vault://secret/config/prod/mail/password#value"
from = "no-reply@example.com"
starttls = true
```

如果后续新增 OAuth client secret 或对象存储 SecretRef schema，应在对应模型真实存在、消费点依赖顺序确认后再给出示例。不要在文档中提前使用当前代码不存在的 `[oauth.github]` 结构，也不要在未重构 `Storage::new` 前暗示 S3 凭据可以直接从 vault 解析。

初始化命令执行完成后，可以提示用户继续写入真实 secret（前提是数据库与 vault 已就绪）：

```text
next steps:
  # 确保数据库可用、vault 已初始化/解封
  monoengine config secret set mail.password --vault-path config/prod/mail/password --field value --value-stdin
  monoengine config validate --resolve-secrets
```

敏感数据写入应优先使用标准输入，避免明文出现在 shell history、进程参数、CI 日志或审计记录中：

```bash
printf '%s' "$SMTP_PASSWORD" | \
  monoengine config secret set mail.password \
    --config config/config.toml \
    --vault-path config/prod/mail/password \
    --field value \
    --value-stdin
```

不建议首版让 `config secret set` 默认自动修改 TOML 文件。更安全的默认行为是写入 `vault` 后输出对应 `SecretRef`，由用户或部署系统显式写入配置；如果后续确实需要自动更新配置文件，应使用显式 `--write-config --config <path>` 开关，并注意 TOML 注释、字段顺序和格式保留问题。无论是否写配置文件，命令都不能把真实 secret 作为明文写回 `config.toml`。

`config secret set` 的路径参数应使用 VaultCore 的相对 secret name（例如 `config/prod/mail/password`），由命令内部统一映射为 `VaultCoreInterface::write_secret("config/prod/mail/password", ...)`。命令输出给配置文件使用的引用才是 `vault://secret/config/prod/mail/password#value`。这能避免用户传入 `secret/config/...` 后与 `write_secret` 内部的 `secret/{name}` 前缀叠加。

推荐的初始化顺序如下：

```text
config init                       # 不依赖 vault
  -> config validate              # 不依赖 vault
  -> (确保数据库可用、vault 初始化/解封)
  -> config secret set            # 依赖 vault
  -> config secret check          # 依赖 vault
  -> config validate --resolve-secrets   # 依赖 vault
  -> 启动服务或执行集成测试
```

`init` 应早于 `secret set`，因为它先确定配置文件位置、敏感字段集合、`SecretRef` 路径规则、profile 和命名空间；`secret set` 再按这些引用把真实值写入 `vault`。`secret check` 和 `validate --resolve-secrets` 则必须在 secret 写入之后、且 vault 可用时执行。

短期不建议新增泛化的顶层 `monoengine init`。如果后续需要初始化数据库、迁移、管理员账号、默认仓库、对象存储 bucket 或队列等完整运行环境，可以再设计顶层 `init` 来编排 `config init`、数据库迁移、Vault bootstrap 和完整配置校验；本次 `Config` 改造应优先把范围收敛在 `monoengine config init`。

### 基础配置与自动化测试配置方案

当前仓库中的 `config/config.toml` 更像一份“基本配置”：它包含本地开发默认值，也曾被用于整项目测试。后续改进中应避免继续让同一个文件同时承担样例配置、开发配置、CI 配置和测试夹具职责，否则容易出现测试依赖本机服务、基础配置被测试污染、示例中残留敏感字段、以及配置变更影响整套测试稳定性的问题。

改造后建议将 `config/config.toml` 定位为基础样例配置，并满足以下约束：默认使用本地、无副作用、低依赖的配置值；所有路径通过 `${base_dir}` 或测试临时目录派生；对象存储默认使用 `local`；外部服务地址只作为示例或显式禁用；已确认可迁移的凭据字段改为 `SecretRef` 示例而非明文；仍属于引导配置或早期运行时依赖的敏感字段明确要求通过 `MEGA_*`、`.env.test` 或部署平台 secret 注入。该文件应纳入配置样例校验，保证它可以被解析、展开和通过基础校验，但不再要求它直接满足所有集成测试的外部依赖。

自动化测试应采用独立的配置方案：单元测试优先通过 `testing.rs`（基于 `Config::mock()`/`load_str()`）构造内存配置或最小 TOML 片段；需要文件加载链路的测试使用临时目录生成测试配置文件，并显式传入 `--config` 或 `MEGA_CONFIG`；集成测试继续通过 `.env.test` 提供数据库、Redis、邮件等外部依赖端点，但 `.env.test` 只负责测试环境覆盖，不应修改仓库中的 `config/config.toml`。测试配置中的 `base_dir`、数据库名、对象存储根目录、日志目录和缓存目录都应隔离到测试临时目录，避免并发测试互相污染。

对于依赖敏感凭据的测试，不应在测试 TOML、`.env.test` 或日志中写入真实 API Key。配置模块应提供可注入的测试 secret resolver 或 vault 测试后端（现有 vault 测试已使用 `tempfile` + `test_storage` 构造隔离实例，可复用此模式），用固定的假 secret、过期 secret、缺失 secret 和权限失败场景覆盖消费端行为。热加载相关测试也应使用测试配置文件和临时 resolver：分别验证基础字段热更新成功、不可热更新字段只告警不生效、SecretRef 变更遵循白名单、候选配置校验失败时旧配置继续生效。

CI 中应增加专门的配置验证任务，至少覆盖三类输入：仓库基础样例 `config/config.toml`、默认模板生成结果、自动化测试生成的测试配置。验证内容包括 TOML 语法、环境变量覆盖、占位符展开、`SecretRef` 格式、集中校验、脱敏错误输出和消费端配置构造。这样可以把“配置能否作为测试输入使用”变成稳定的自动化检查，而不是依赖人工维护一份混合用途的基础配置。

还应补充配置兼容性测试矩阵：坏 TOML、坏环境变量类型、未知占位符、profile 类型冲突、数组覆盖语义、`password` 与 `password_ref` 同时存在、SecretRef 路径缺少 `#field`、vault key 缺失、secret 缺失、secret 字段缺失、权限失败、错误信息脱敏、生产环境 Postgres fallback SQLite 的告警或拒绝启动。

### 推荐加载流水线

改造后建议把 `Config::new` 的内部流程表达为清晰的流水线。**注意 secret 解析不在此流水线内**——`Config::new` 同步且 vault 尚未就绪，本阶段只产出未解析的 `SecretRef`：

```text
ConfigLoader::load
  -> resolve config path
  -> build sources
  -> merge file and env
  -> expand placeholders
  -> deserialize to Config model（含未解析的 SecretRef）
  -> validate SecretRef 引用格式与必填性（不读取真实值）
  -> normalize paths and derived values
  -> validate Config
  -> return Config
```

secret 真实值的解析是后续独立异步阶段，发生在 `AppContext`/vault 就绪之后（见「secret 解析的依赖顺序」）。

这样可以把“定位文件”和“解析配置”分开，把“能反序列化”与“配置语义合法”分开，也把“引用格式合法”与“真实值可读”分开。启动失败时能更准确地区分是文件不存在、TOML 语法错误、环境变量类型错误、占位符无法展开、字段组合不合法，还是 secret 读取失败。

`Config::new`、`Config::load_str` 和 `Config::load_sources` 应共用同一套 source builder 与 env/list 解析规则。当前这些入口的环境变量列表解析并不完全一致，后续测试如果只覆盖 `load_str`，可能无法发现生产路径中的列表字段、profile 合并或 env 覆盖问题。

未知字段与废弃字段也应纳入流水线。Serde 默认可能忽略未知字段；如果要对拼错字段、废弃字段输出 warning，需要在反序列化前基于 raw TOML/config tree 做 key 级检查，或引入等价的 ignored-field 检测机制。新增字段必须提供 `serde(default)` 或 `Option<T>`，移除字段必须经历 warning 过渡期。

热加载（后续阶段）应复用同一条解析、展开、规范化和校验流水线，避免启动加载与运行期重载出现两套语义。运行期可以在 `reload.rs` 中维护 `ConfigHandle` 或类似包装，内部持有当前生效的 `Arc<Config>`，业务组件只读取稳定快照；当配置文件或受支持的配置源发生变化时，先构建候选 `Config`，再执行差异计算和白名单校验。只有日志级别、日志输出细节、部分功能开关、任务调度间隔、邮件通知开关等无须重建长生命周期连接的字段可以直接热更新；数据库、Redis、对象存储、监听地址端口、OAuth 客户端密钥等字段默认视为启动期配置，检测到变化时应记录告警并提示重启，而不是在运行期隐式重建。

热加载应用过程应具备原子性：候选配置解析、校验或组件回调任一阶段失败，都不能替换当前生效配置；组件回调成功后再发布新的 `Arc<Config>` 快照，并记录变更字段、来源和结果。订阅接口应按领域注册，例如日志系统订阅 `log` 可热更新字段，后台任务订阅对应调度配置，避免业务代码直接监听文件变化或自行重新解析配置。

> 热加载与现有访问模式的衔接：当前 `Storage::config()` 返回 `Arc<Config>` 且调用点遍布全仓。切到快照句柄（如 `ArcSwap<Config>`）在技术上可行，但任何“取出 Arc 后跨 await/长操作持有”的调用会钉住旧快照，语义需要逐点确认。这也是把热加载放在独立阶段、不与拆分/迁移捆绑的原因。

### 迁移步骤（分阶段）

整体拆为多个阶段，每个阶段独立可编译、可回归、可单独评审合并。注意：只有纯移动和 re-export shim 可以承诺“零行为变更”；错误模型、校验规则、命令加载模型、vault bootstrap 和 SecretRef 迁移都会改变失败形态，必须单独评审。

**阶段 0a — 纯结构拆分（保留兼容 shim，零行为变更）** — **工作量：S（约 1–2 天）**

1. 新建 `src/config/` 目录，将 `src/common/config.rs` 移动为 `src/config/mod.rs`，并把 `loader.rs`、`template.rs` 一并迁入 `src/config/`。
2. 在 `src/main.rs` 新增顶层 `mod config;`（可见性按 shim 需要设置为 `pub(crate)` 或等价）；在 `src/common/mod.rs` 中将 `pub mod config;` 改为 wrapper/re-export shim，使 `crate::common::config::*` 在过渡期继续可用。**本阶段不改任何业务调用方路径。**
3. 拆出 `model.rs`（结构体）、`source.rs`、`expand.rs`，保持 `Config::new` 外部行为不变，只改内部组织方式。
4. 补充单元测试覆盖路径定位、env 覆盖、列表解析和占位符展开，确认移动前后行为一致。

> **验收标准**：`cargo build`、`cargo build --tests`、`cargo test` 全部通过；`cargo +nightly fmt --all --check` 无 diff；除 `src/config/`、`src/common/mod.rs`、`src/main.rs` 以及移动后模块内部必要 import 修正外，无业务调用方 import 路径变更。

**阶段 0b — 错误模型、脱敏与保守校验** — **工作量：M（约 3–5 天）**

5. 新增 `error.rs`，把 `variable_placeholder_substitute` 等加载路径上的 `unwrap`/`expect`/`panic` 替换为可诊断错误；这会改变失败形态，不应并入 0a。
6. 建立统一 redaction 工具，先覆盖数据库 URL、Redis URL、Vault root token、secret shares、对象存储 key 等现有日志/错误泄露点。
7. 新增 `validate.rs`，先实现低风险、无争议的 hard error 校验（端口范围、必填字符串、明显非法枚举、`password`/`password_ref` 互斥、Buck 限制等）；涉及部署兼容性的严格校验先以 warning 过渡。
8. 补充测试覆盖校验失败、错误信息、脱敏输出和 warning/hard error 边界。

> **验收标准**：配置损坏时启动不再 panic，而是输出包含配置文件路径、字段路径和修复建议的诊断信息；所有包含敏感数据的日志/错误信息经过 redaction；新增校验用例测试覆盖。

**阶段 1 — 消费端路径迁移 + 移除 shim** — **工作量：S（约 1 天）**

9. 将所有 `crate::common::config::*`/`common::config::*` 改为 `crate::config::*`/`config::*`（约 12 个文件）。
10. 构建确认无残留引用后，删除阶段 0a 的 re-export shim，并从 `common` 彻底移除配置承载职责。本阶段不改运行语义。

> **验收标准**：全仓无 `common::config` 引用；`cargo build`、`cargo test` 通过。

**阶段 2 — CLI LoadMode + 非 Vault `config` 命令** — **工作量：M（约 4–6 天）**

11. 改造 `cli::parse` 与命令注册：先解析子命令，再按命令声明的 `LoadMode` 选择加载层级（`None`、`ConfigPath`、`RawSources`、`ParsedConfig`、`VaultBootstrap`、`FullAppContext`）。
12. 新增 `monoengine config init` 与不解析 secret 的 `config validate`，确保配置不存在或配置损坏时仍能给出诊断，而不是被 CLI 预加载拦截。
13. 新增 `config secret ref` 的纯规则生成能力；不依赖 vault 的子命令不得隐式创建数据库或启动服务依赖。

> **验收标准**：`monoengine config init` 可在无配置目录下运行并生成样例；`monoengine config validate` 能在坏配置时输出诊断而非 panic；`config secret ref` 可生成与 profile/命名空间一致的 `vault://secret/...#field` 引用；命令帮助文档完整。

**阶段 3 — 最小 DB/Vault bootstrap + core_key 加固** — **工作量：M/L（约 1 周）**

14. 拆出最小 DB/Vault bootstrap 能力，只建立数据库连接和 vault 所需 storage，不构造 Redis、对象存储、HTTP/SSH 服务、monorepo 初始化或后台任务。
15. 将普通启动路径中的 `core_key.json` 缺失行为改为 fail-closed，不再自动 `delete_all()` 并重新初始化；破坏性 reset/init 必须由显式运维命令触发。
16. 删除 vault 初始化路径中 root token、secret shares、完整 key 文件内容的所有输出；`VaultCore::new`/bootstrap 路径逐步改为返回 `Result`。
17. 收紧 `core_key.json` 与父目录权限，补充备份恢复和非本地明文 key 文件的生产部署说明。

> **验收标准**：`config validate --resolve-secrets` 的底层 bootstrap 不依赖 Redis/S3；`core_key.json` 缺失时不会清空 vault 数据；root token 不再进入 stdout/stderr/tracing；key 文件权限收紧（Unix `0600`，目录 `0700` 或等价）。

**阶段 4 — `config secret set/check` 命令** — **工作量：M（约 3–5 天）**

18. 基于阶段 3 的最小 DB/Vault bootstrap 新增 `config secret set`、`config secret check`、`config validate --resolve-secrets`。
19. `config secret set` 默认只接受 `--value-stdin` 或隐藏输入，写入后输出 `vault://secret/...#field` 引用；默认不修改 TOML 文件。
20. 命令只允许写入可迁移凭据；数据库密码、Redis URL、当前阶段对象存储 key 等引导/早期依赖必须给出部署平台 secret 指引。

> **验收标准**：`config secret set --value-stdin` 正确写入 vault 且不在日志、进程参数或错误信息中泄露；`config secret check` 能区分 vault 不可用、secret 缺失、字段缺失和权限失败；所有路径映射避免 `secret/secret/...`。

**阶段 5 — SecretRef 基础设施 + 第一批可迁移凭据** — **工作量：L（约 1–2 周）**

21. 新增 `secret.rs`：定义 `SecretRef`、secret resolver trait，适配现有 `VaultCoreInterface`；resolver 接收已就绪的 vault 句柄，不在 `Config::new` 调用。
22. 梳理并提交字段依赖表，明确每个字段是引导配置、早期运行时依赖、可迁移凭据还是非敏感参数。
23. 第一批只迁移确认晚于 vault 的字段，例如 `mail.password`；OAuth client secret 只有在真实 schema 存在后再迁移；对象存储 access key 只有在完成 DB-only Storage / 后置对象存储初始化重构后才能迁移。
24. 对应消费端改为在初始化点通过 resolver 读取，日志/错误只暴露脱敏引用；SMTP 初始化失败是 hard error 还是降级 Noop 需按环境或配置明确，不应在生产静默降级。

> **验收标准**：`mail.password_ref` 可解析且邮件发送正常；resolver 缓存 TTL 和 evict 生效；secret 读取失败时不 panic、不泄露明文；字段依赖表作为正式文档入仓。

**阶段 6 — 基础样例配置、Profile 与测试配置分层 + CI** — **工作量：M（约 3–5 天）**

25. 将 `config/config.toml` 改造为基础样例配置：移除真实敏感值、改为与当前 schema 对齐的 `SecretRef` 示例、本地默认值与初始化指引；纳入配置样例校验。
26. 固定 Profile 文件命名、加载优先级、数组覆盖语义和 SecretRef namespace，并补充 profile 合并测试。
27. 新增 `testing.rs`（基于既有 `mock()`/`load_str()`/`load_sources()`），提供测试配置构造、临时目录派生、`.env.test` 覆盖合并、测试 secret resolver。
28. 建立分层测试策略并接入 CI：单元测试用内存/最小 TOML，加载测试用临时文件，集成测试用 `.env.test` + `MEGA_CONFIG` 指向隔离配置；CI 覆盖基础样例、默认模板、`config init` 结果、profile 合并结果与测试配置生成结果。

> **验收标准**：`config/config.toml` 不含真实密码；`cargo test` 不依赖仓库中的 `config/config.toml`；CI 新增配置校验任务且通过。

**阶段 7 — 对象存储等早期依赖的后置初始化重构（可选）** — **工作量：L（约 1–2 周）**

29. 如果要让对象存储凭据进入 vault，先把 `Storage::new` 拆为 DB-only storage、vault bootstrap、secret resolve、完整 storage/service 初始化。
30. 重新梳理 Redis、对象存储、Orion 等字段依赖表，只有在确认消费点晚于 vault 且失败语义可接受后，才允许迁移为 SecretRef。

> **验收标准**：S3/S3-compatible 凭据迁移前，服务启动链路中不再在 vault 前构造对象存储；缺少对象存储 secret 时返回可诊断错误，不影响 `config secret set/check` 对其他 secret 的操作。

**阶段 8 — 受控热加载（独立变更）** — **工作量：L（约 2–3 周）**

31. 新增 `reload.rs`：监听配置文件变化，复用加载流水线构建候选配置，按字段白名单计算差异，向订阅组件发布变更，失败时保留旧配置继续生效。
32. 将运行时注入从直接共享 `Arc<Config>` 调整为共享快照句柄（如 `Arc<ConfigHandle>`/`ArcSwap`）；逐点确认“取出 Arc 后跨 await 持有”的调用语义，新增依赖前评估必要性。
33. 改造支持热加载的消费端订阅方式（日志、功能开关、任务调度、邮件通知等只订阅各自可热更新字段；不可热更新字段变化只告警提示重启）。
34. 补充热加载测试：可热更新字段生效、不可热更新字段告警不生效、SecretRef 变更遵循白名单、候选校验失败回滚。

> **验收标准**：白名单字段（如 `log.level`）变更后无需重启即可生效；数据库地址变更只告警不重建连接；热加载失败时进程继续运行且保留旧配置；全量回归测试通过。

**贯穿全程**

35. 每个阶段同步更新 `README`、`config/config.toml` 注释和本文档，确保加载优先级、模块路径、`config` 命令使用方式与引导顺序、消费端访问方式、环境变量规则、敏感配置存储边界和热加载限制与实现一致。

### 风险与约束

- **循环依赖是硬约束，不是注脚。** `Config → Storage(DB) → Vault` 决定了：数据库凭据等引导配置永远不能是 vault SecretRef；secret 解析永远不能发生在 `Config::new` 内，只能在 vault 就绪后进行。任何规划若违反这两点（如把 db 密码做成 `password_ref` 入 vault）都不可行。
- **多环境 Profile 深度合并冲突风险**：多环境配置文件在深合并（Deep Merge）时可能因为类型不一致或覆盖错误引发运行期异常（如列表字段合并时是覆盖还是追加）。应在 `validate.rs` 中加强合并结果的 schema 校验。
- **兼容性退化与孤立配置遗留**：废弃字段如果未在多个版本中执行严格的 `WARN` 告警和清理审计，会导致老旧、不安全的配置项长期残留在用户的生产配置文件中。必须在 CI/CD 中定期开展包含废弃配置的兼容性回归测试。
- **Secret 轮转时的双凭据过渡开销**：两阶段凭据轮转可以降低切换中断风险，但 fallback 不应成为所有 secret 的默认行为。应将多版本读取能力封装在 `SecretResolver` 或专门适配层中，由消费端按外部系统语义决定是否回退。
- **包装类型（SecretString）在网络传输与持久化中的误暴露**：尽管 `SecretString` 在 Rust 代码中能有效防止 `Debug` 泄露，但在将其序列化（如写入外部监控日志、通过 OpenAPI 接口返回、或者保存到临时数据库中）时，如果序列化库（如 `serde`）未正确配置，仍可能会提取其明文。必须在编译期实施 lints 或强制配置 `#[serde(skip_serialize)]` 规则。
- **当前对象存储和 Redis 也是早期依赖。** `Storage::new` 会在 vault 就绪前构造对象存储，`AppContext::new` 会在 vault 就绪前连接 Redis；因此 S3 access key、带密码的 Redis URL 等字段不能直接按“可迁移凭据”处理，除非先重构初始化顺序。
- **`config` 命令依赖 CLI 两阶段加载。** 如果仍在子命令分发前强制 `Config::new`，`config init` 和 `config validate` 无法在无配置或坏配置时工作，配置诊断也会被预加载错误拦截。
- **日志与错误脱敏是 secret 迁移前置 gate。** 现有数据库 URL、Redis URL、Vault root token 等输出必须先脱敏，否则 SecretRef 改造无法兑现“不进日志/错误信息”的安全承诺。
- **`core_key.json` 加固是 secret 迁移的前置 gate。** 在加固与备份恢复就绪前，把凭据搬进 vault 只防“进仓库/日志”，不防“读磁盘”，且会引入“丢一个本地文件即丢全部 secret”的单点故障。该阶段成果在加固完成前不得用于生产。
- **采用分阶段切换，过渡期保留 `crate::common::config` re-export shim。** 拆分与调用方迁移分两步，各自可独立构建验证；shim 在调用方全部迁移后删除。不追求“单次变更内不留兼容入口”。
- **热加载作为独立阶段，不与拆分/迁移捆绑。** 热加载只允许白名单字段运行期生效，不应隐式重建数据库、Redis、对象存储、HTTP 监听器等长生命周期资源；失败必须保留旧配置并输出来源、字段路径、失败原因和处理结果。
- **`config secret set/check`、`validate --resolve-secrets` 依赖数据库与 vault，但不得依赖完整 `AppContext`。** 命令实现必须使用最小 DB/Vault bootstrap，在缺少前置依赖时给出明确指引，而非 panic；裸机初始化只能先跑 `config init` 与不带 `--resolve-secrets` 的 `config validate`。
- 敏感的可迁移凭据不得以明文形式保存在 TOML、默认模板、错误信息或日志中；配置模块只保存 `SecretRef`，真实值通过 vault 按需读取并统一脱敏输出。引导配置（数据库等）则走部署平台 secret/环境变量。
- `config init` 只能初始化配置骨架和 `SecretRef`，不能接收或写入真实 secret；`config secret set` 应优先只接受 `--value-stdin` 或隐藏输入，不应鼓励通过命令行参数传递明文敏感值。
- 短期不建议新增泛化的顶层 `monoengine init`，避免把配置初始化、数据库迁移、Vault bootstrap 和业务数据初始化混在同一命令中；完整环境初始化可在后续基于 `config init` 再单独设计。
- `config/config.toml` 不能再作为所有自动化测试的隐式共享状态；测试必须使用隔离的临时配置、`.env.test` 覆盖和测试 secret resolver，避免依赖开发机默认服务或污染基础样例文件。
- 基础样例配置、默认模板和测试配置生成结果都必须进入配置校验范围；如果新增配置字段，必须同时更新样例默认值、测试覆盖和文档说明。
- 顶层模块名 `config` 与外部 `config` crate 同名，模块内部引用第三方 crate 时应使用清晰别名（现状已是 `c::`）或 `::config` 绝对路径，避免命名解析混淆。
- 校验规则要分阶段落地，早期只加入确定无争议的规则；涉及部署兼容性的严格校验应先以文档说明或 warning 形式过渡。
- 错误模型应与现有 `MegaResult` 协作，而不是在业务层引入另一套并行错误处理方式。
- README 与实际加载优先级存在轻微差异，模块化改造完成后应同步补充 `mega_base()/etc/config.toml` 与默认配置生成逻辑。
- 拆分后仍应保持配置对象运行期只读共享，避免在业务流程中重新解析配置或隐式改变运行时语义。
- **BuckConfig::validate() 当前在 `Storage::new` 中 panic（`src/jupiter/storage/mod.rs:212–222`）**，这是配置校验侵入启动期的负面典型。`validate.rs` 落地后，此类校验应收敛到 `Config::validate()` 中，在 `Config::new` 阶段返回可诊断错误，而不是延迟到 `Storage::new` 时 panic。
- **环境变量注入的可见性风险**：引导配置通过 `MEGA_*` 环境变量注入时，需意识到 `/proc/<pid>/environ`、systemd journal、容器 inspect 等场景的泄露风险。生产高敏感部署应优先使用文件挂载 secret，并通过占位符读取。
- **config init 不得生成可预测默认密码**：`DbConfig::default()` 和当前 `config/config.toml` 中的硬编码密码（`postgres://mega:mega@...`）应在 `config init` 生成结果中移除，改为强制用户通过环境变量、文件挂载 secret 或部署平台 secret 注入；数据库密码不能通过本项目 vault 注入。
- **`orion_server.db_url` 是外部服务凭据**，不在 monoengine vault 管理范围内。若 Orion 自身需要 secret 管理，应由 Orion 独立解决，monoengine 只作为客户端通过部署平台 secret 注入其连接参数。

### 预期收益

- 配置模块边界更清楚，新增配置域时只需扩展模型和校验，不必继续放大单文件。
- 加载流程更容易测试，每个阶段都可以独立构造输入和断言输出。
- 启动失败信息更明确：消灭加载路径上的 panic，部署排障时可以直接定位到配置文件、字段和值，并能区分“引用格式非法”与“真实值不可读”。
- 可迁移凭据从普通配置文件中剥离，**不再进入 git 仓库、日志和错误信息**（在日志脱敏与 `core_key.json` 加固后，进一步降低静态泄露风险）；引导配置和早期运行时依赖则交由部署平台 secret 机制管理，职责清晰。
- `monoengine config init` 与 `config secret` 命令让配置初始化、SecretRef 生成、Vault 写入和解析检查形成标准流程；`config secret` 通过最小 DB/Vault bootstrap 工作，不被 Redis、S3 或完整服务初始化阻塞。
- 基础配置与测试配置职责分离，`config/config.toml` 更适合作为可提交的样例和本地起点，自动化测试则获得可重复、可隔离、可校验的配置输入。
- 配置热加载（后续阶段）有明确边界，可在不重启服务的情况下更新安全字段，同时避免长生命周期资源被隐式替换。
- 为后续配置文档生成、配置样例校验和部署前 dry-run 检查打下基础。

## 小结

`monoengine` 的 `Config` 实现采用“启动期集中加载、强类型反序列化、运行时只读共享”的方案。该设计符合单二进制服务的部署模式，能够同时支持本地开发、配置文件部署和环境变量覆盖。

后续优化的核心是把 `Config` 迁移为 `src/config/` 形式的一级独立模块，并在此基础上**分阶段**推进：先做零行为变更的结构拆分（保留兼容 shim）；再单独收敛错误模型、校验和日志脱敏；随后迁移调用方路径并移除 shim；再改造 CLI `LoadMode` 与不依赖 vault 的 `config` 命令；接着实现最小 DB/Vault bootstrap、`core_key.json` fail-closed 与密钥加固；之后再提供 `config secret set/check`，并只将确认晚于 vault 消费的**可迁移凭据**改为 `SecretRef` 由 resolver 解析；最后将基础配置、Profile 与测试配置分层，并把对象存储后置初始化和受控热加载作为独立后续阶段落地。

五条不可违反的约束贯穿始终：`Config → Storage(DB) → Vault` 的循环依赖决定了引导配置不可入 vault、secret 解析不可在 `Config::new` 内；当前对象存储和 Redis 的初始化顺序决定了它们暂时也不是直接可迁移凭据；`config secret set/check` 只能使用最小 DB/Vault bootstrap，不能依赖完整 `AppContext`；`core_key.json` 明文自动解封与现有日志泄露点决定了 Vault 化必须先完成脱敏、fail-closed 和密钥加固；范围控制决定了拆分、CLI、secret、测试分层、对象存储后置初始化和热加载应分阶段交付，而非一次性切换。
