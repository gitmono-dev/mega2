# Config 实现方案分析

本文档记录 `monoengine` 中 `Config` 的实现方案、加载链路、运行时注入方式、主要消费点与当前实现中的注意事项。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **集成测试指引**：本计划的各阶段功能应通过 **`integration.md`** 中定义的集成测试场景进行端到端验证。特别是配置初始化、加载、验证和 CLI 工作流应在 Docker 环境中完整测试，以确保与 Vault、邮件、通知等下游模块的集成无误。

> **事实校准（2026-06-09 复核）：** 本文档中的代码引用已对照当前 `src/` 重新核对。**⚠️ 自上一版「2026-06 校准」之后，代码已发生重大变化：`mail` 子系统已从"孤立/未编译"演进为"已编译、已接入 `Config`、已有 vault 之后的构造点"。** 早期草案与上一版校准中关于 mail 的多处结论已**反转**，后文据此重写。需特别注意以下事实：
> 1. **`Config` 结构体当前含 15 个字段，新增了 `mail`。** 字段为 `base_dir`、`log`、`database`、`monorepo`、`pack`、`lfs`、`blame`、`build`、`redis`、`buck`、`object_storage`、`orion_server`、`sidebar`、`artifacts_gc`，以及 **`mail: Option<MailConfig>`（`#[serde(default)]`，`src/common/config.rs:106`）**（结构体声明在 `src/common/config.rs:81`）。`Config` 仍**不含** `oauth` 字段，也无 `OAuthConfig`。
> 2. **`MailConfig` 现已完整定义并接入。** 结构体在 `src/common/config.rs:376`（`Default` 在 `:399-411`，含 `default_smtp_port`=587/`default_starttls`=true 辅助函数与反序列化单测 `config.rs:1124-1148`）。`Config.mail` 字段在 `:106`，`mail.password` 是真实的 `Option<String>`（`:385`）。`config/config.toml` 的 `[mail]` 段（`:269-276`，`enabled = true`）**已被消费**——不再是"静默忽略的死配置"（仅段内的 `smtp_tls`/`tls` 两个非 `MailConfig` 字段被 serde 丢弃）。真实实现位于一级模块 **`src/mail/mod.rs`（约 231 行，`SmtpMailer`/`Mailer`/`NoopMailer`，依赖 `lettre`）**，由 `src/main.rs:17` 的 `mod mail;` 接入编译；`src/email/mod.rs` 已退化为 **12 行 re-export shim**（`pub use crate::mail::{Mailer, NoopMailer, SmtpMailer};`）。**注意：激活用的是 `mod mail;`，并非早期草案规划的 `mod email;`。** 详见同目录 **`mail.md`**（mail 子系统的权威文档）。
> 3. **可迁移凭据集合不再为空：`mail.password` 是其第一个成员。** 它在 `Config` 中真实存在（`config.rs:385`），且其消费点（`SmtpMailer::new`）严格晚于 vault——`src/context/mod.rs:46-55` 在 `VaultCore::new`（`:39`）之后构造 `SmtpMailer` 并 spawn `EmailDispatcher`。因此"阶段 5 必须先新增一个合格的后置消费字段"这一前置工作**已落地**；剩余的是 SecretRef 基础设施 + 把 `mail.password` 由明文改为 `password_ref` + 把当前 `if let Ok(m)` 静默失败改为可诊断处理。由本项目 vault 管理的 secret 仍只有 `ssh_server_key`（不在 `Config` 结构体中）。
> 4. **`mail.password` 当前是一处真实的明文凭据暴露面。** 虽然 `config/config.toml` 的 `[mail]` 样例**未**写入 `password`/`username`，但该字段为明文 `Option<String>`，`Config`/`MailConfig` 均派生 `Debug`、`MailConfig` 还派生 `Serialize`（`config.rs:375`），并在 lettre 边界以明文传入 `Credentials::new`（`mail/mod.rs:96`）。在 SecretRef 化之前，它与 vault root token、`core_key.json` 一样属于"当前已存在的明文暴露"，而非纯未来风险。
> 5. **行号已按当前代码全面重核。** `config.rs` 现为 **1149 行**（早期草案约 1077 行），多数子配置结构体行号已位移（见「行号修正表」一节）。早期失效行号示例：`mail.password` 现在 `config.rs:385`（非 `:293`）、`Storage::new` 在 `src/jupiter/storage/mod.rs:190`（非 `mod.rs:159`）。

> **本文档性质说明**：本文档同时承担“现状分析”和“改进设计方案”两种角色。**除“事实校准”和本节外，文档中大量内容描述的是规划中的目标状态**。需注意：`mail`/`MailConfig` 子系统与"vault 之后的 mailer 构造点"**已部分落地**（见事实校准 #2/#3 与「已落地的 mail/notification 子系统现状」一节），但**配置模块拆分（`src/config/`）、`config` 命令族、`SecretRef` + resolver、Profile、热加载、集中校验仍均未落地**。实施时请以代码实际状态为准，优先完成“现状 → 校准表 → 硬约束”三部分的阅读。

## 当前实现状态速览表（2026-06）

| 能力 / 组件                     | 实现状态     | 关键事实与风险 |
|--------------------------------|-------------|----------------|
| `Config` 结构体与领域子配置     | 已实现      | 强类型拆分合理；`#[serde(default)]` 已用于可选域（blame、buck、orion_server、sidebar、artifacts_gc）。 |
| TOML 文件 + `MEGA_*` env 叠加   | 已实现      | `config` crate + `__` 分隔符；`load_str`/`load_sources` 的 list_parse_key 集合弱于主路径（仅主 `new` 注册了 oauth/monorepo 列表）。 |
| 占位符 `${base_dir}` 展开       | 已实现（有坑） | `variable_placeholder_substitute`（`config.rs:192`，体 `193-237`）做两次 `collect()` + `Rc<RefCell>` 遍历 + `envsubst`，内部**恰 10 处** `.unwrap()`（`:194,197,201,203,213,225,226,228,231,237`），任何坏模板都会 panic。 |
| 配置文件定位（4 级回退 + 自动生成） | 已实现    | `mega_base()/etc/config.toml` 与默认生成逻辑存在，但 README 主要只提前三种；生成时会把 `base_dir` 渲染进去。 |
| 运行时共享 (`Arc<Config>`)      | 已实现      | `AppContext` 持有；`Storage` 内部为 `Weak<Config>`，`config()` 调用 `.expect("Config has been dropped")` —— 这是热加载切换快照句柄时的潜在雷区。 |
| `mail` / `MailConfig` / 邮件发送 | **已激活并接入 dispatcher 启动点** | 真实实现在一级模块 `src/mail/mod.rs`（`SmtpMailer`/`Mailer`/`NoopMailer` + 单测，依赖 `lettre`），经 `src/main.rs:17` 的 `mod mail;` 编译；`MailConfig` 在 `config.rs`、`Config.mail` 字段已存在、`[mail]` 段已被消费。`src/email/mod.rs` 为 re-export shim。`AppContext::new` 在 vault 之后解析 `mail.password_ref`（如存在）并构造 `SmtpMailer`，构造失败现在返回可诊断错误。详见 `mail.md`。 |
| `[oauth]` 段                   | **死配置**  | TOML 里有完整段（`config.toml:155`）+ env list key 注册（`config.rs:118`），但无强类型字段承接，serde 静默忽略。它是目前**唯一**被整段丢弃的孤立顶层段（`[mail]` 已被消费）。用户添加的未知顶层段同样被静默丢弃；段内未知 key（如 `[mail]` 中的 `smtp_tls`/`tls`）也被丢弃。 |
| `src/notification/`（邮件 outbox / dispatcher） | **已接入编译并在 mail 启用时启动 dispatcher** | `src/notification/{dispatcher,triggers,mod}.rs`（`EmailDispatcher`、触发器）+ `callisto::email_jobs` outbox 实体已从 mega 移植；`main.rs:18` 已声明 `mod notification;`。`AppContext::new` 在 vault 之后、`init_monorepo` 之前创建 `EmailDispatcher` 并 `tokio::spawn`，但启动失败路径仍需要可诊断化和生命周期治理。 |
| Vault 管理的 secret            | 已扩展     | 现有直接消费者包括 `ssh_server_key`、PGP、Nostr、PKI 以及首批配置 SecretRef（`mail.password_ref` → `secret/config/...`）。 |
| `core_key.json` + 自动解封     | 已加固 | JSON 存储 unseal shares + 限权 runtime tokens，不再长期保存 `root_token`；缺 key fail-closed，不 `delete_all()`；token/root/shares 不输出到日志。 |
| Profile / `config.<profile>.toml` | **未实现** | loader.rs 完全没有 profile 逻辑。 |
| `monoengine config` 命令族      | **部分实现** | CLI 已支持按命令 `LoadMode` 加载；`config secret ref/set/check` 与 `config validate --resolve-secrets` 已实现。`config init`、profile、完整 source diagnostics 仍未实现。 |
| 集中配置校验                   | **局部存在** | 只有 `BuckConfig::validate()`，且在 `Storage::new:253-263` 失败即 `panic!`（tracing error + panic）。 |
| SecretRef + 运行期 resolver    | **已实现首批**  | `SecretRef`、`SecretResolver`、`VaultSecretResolver` 已编码；支持 `vault://secret/...#field`、缓存 TTL、`evict`/`evict_all`，并实现 `mail.password` / `mail.password_ref` 互斥。 |
| 受控热加载                     | **未实现**  | 配置加载后为静态只读快照。 |

**启动/加载关键路径上的已知危险点（各阶段必须收敛）**：
- 占位符展开的 **10** 处 `unwrap`（`config.rs:192-237`）。
- `Storage::new` 里的 Buck 校验 panic（校验调用 `storage/mod.rs:253`，`panic!` 在 `:262`）。
- `Storage::config()` 的 `expect`（`storage/mod.rs:335`，`upgrade().expect("Config has been dropped")`）。
- `AppContext::new` 的两处启动期 `expect`：`Storage::new(...).expect("init monorepo storage err")`（`context/mod.rs:33-35`）、`init_monorepo(...).expect("init monorepo failed")`（`:60-64`）。
- mailer/dispatcher 构造使用 `if let Ok(m) = SmtpMailer::new(...)` **静默吞错**（`context/mod.rs:48`）——真实接入已发生，但失败仍不可诊断，需改为可观测的错误处理，避免邮件能力静默失效。
- Vault 初始化/解封路径的多处 `println!`、`expect`、`assert!` + root token 泄露（`vault_core.rs:71/:83-86/:111/:114-117`）。
- `mega_base()`（`config.rs:33`）/`mega_cache()`（`:65`）的早期 panic（`BaseDirs::new().unwrap()` 等）。
- `DbConfig::default()`（`config.rs:298`）和 `config/config.toml`（`:28`）里的硬编码可预测凭据（`postgres://mega:mega@...`、`postgres://mono:mono@...`）。
- Orion 自己也有一份 `postgres://postgres:postgres@...`（`config/config.toml:242`、`config.rs:759`）。

## 已落地的 mail/notification 子系统现状（2026-06-09）

本文早期版本把邮件能力整体当作"阶段 5 才新建的未来工作"。当前代码已不符这一描述，需单列现状，避免规划与实现脱节。**与本子系统相关的权威文档是同目录的 `mail.md`**（含 mail 模块自己的事实校准、速览表、硬约束与分阶段计划）；本节只做与 `Config` 相关的接口性概述，细节以 `mail.md` 为准。

**已编译且活跃：**
- 一级模块 `src/mail/mod.rs`（约 231 行）：`Mailer` trait（`:40`）、`NoopMailer`（`:50`）、`SmtpMailer`（`:65`，含长生命周期 `AsyncSmtpTransport`，`:68/:102`，`Credentials::new` 在 `:96`），由 `src/main.rs:17` 的 `mod mail;` 接入。
- `MailConfig`（`config.rs:376`，`#[serde(default)]` 经 `Config.mail` 字段 `:106` 进入统一加载管道）、`config/config.toml` 的 `[mail]` 段（`:269-276`）已被消费。
- `src/context/mod.rs:46-55`：在 `VaultCore::new`（`:39`）之后从 `config.mail` 构造 `SmtpMailer`，并创建 `EmailDispatcher` 后 `tokio::spawn(dispatcher.run(shutdown))`。当前问题是 `SmtpMailer::new` 失败会被 `if let Ok(m)` 静默忽略，仍需改为可诊断处理。
- `src/email/mod.rs`：12 行 re-export shim（历史路径兼容），应作为有主、有移除阶段的过渡 shim 跟踪。

**已接入但仍需加固：**
- `src/notification/{dispatcher,triggers,mod}.rs`：`EmailDispatcher`（`dispatcher.rs:18` 的 `run(self, shutdown)`，2 秒固定 `interval`、`fetch_pending_jobs(50)`、`:6` 导入 `crate::mail`）与事件触发器（`on_cl_comment_created` 等）已从 mega 移植，并经 `main.rs:18` 的 `mod notification;` 接入编译。
- `callisto::email_jobs` outbox 实体、`NotificationStorage`、对应 migration 均存在；`AppContext::new` 当前在 vault 之后、`init_monorepo` 之前启动 dispatcher。
- 仍需补齐：构造失败可诊断化、dispatcher 生命周期治理、退避/并发策略、以及触发器在业务路径中的完整调用。

**结论（影响阶段 5 的重新基线）：** "先有真实的、晚于 vault 的消费者，再谈 SecretRef"这一原则的**前置功能工作已落地**（`MailConfig` + `Config.mail` + `mod mail;` + `mod notification;` + vault 之后的 mailer/dispatcher 启动点）。阶段 5 的真实剩余工作收敛为：(a) 落地 `SecretRef` + resolver，把 `mail.password` 由明文改为 `password_ref`；(b) 把当前 `if let Ok(m)` 静默失败改为可诊断处理；(c) 完善 dispatcher 生命周期、退避/并发和业务触发器接入。

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

> 注意：`Config::new` 是**同步**函数（`src/common/config.rs:106`），且只依赖文件系统读取，不访问数据库或网络。这一事实对后文“敏感数据 Vault 化”的可行性有决定性影响，见下文「现有 vault 能力与关键约束」。
>
> 当前 `variable_placeholder_substitute` 实现做了两次完整 `config.collect()` + builder clone + `Rc<RefCell>` 嵌套遍历，任何占位符错误或类型不匹配都会直接 `.unwrap()` panic。这是“错误模型集中化”的首要目标，而非性能问题（启动期一次性的开销可忽略）。

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
4. 对部分列表类字段进行逗号分隔解析（如 `oauth.allowed_cors_origins`、`monorepo.admin`、`monorepo.root_dirs`）。其中 `oauth.allowed_cors_origins` 当前没有对应强类型字段，属于遗留的 env 解析规则，应在新增 `OAuthConfig` 或清理该遗留规则时重新确认。
5. 在反序列化前执行字符串占位符替换。
6. 将最终配置反序列化为强类型 `Config`。

占位符替换主要用于让配置值引用基础目录等公共路径，例如 `${base_dir}`。该机制工作在字符串值上，适合路径类配置复用，但不适用于非字符串字段。

> 实现痛点：占位符替换函数 `variable_placeholder_substitute`（`src/common/config.rs:192`，函数体 `193–237` 行）内部存在密集的 `.unwrap()`（`build().unwrap()`、`substitute(...).unwrap()`、`collect().unwrap()`、`Rc::try_unwrap(...).unwrap()`、`into_string().unwrap()`、`set_override(...).unwrap()` 等，**恰 10 处**，位于 `:194,197,201,203,213,225,226,228,231,237`）。任何不合法的配置都会直接 panic 而非返回可诊断错误。这是后文「错误模型集中化」的主要动机。

## 强类型配置结构

`Config`（`src/common/config.rs:81`）按功能域拆分为多个子配置，覆盖系统运行所需的主要能力。当前**实际存在**的字段如下：

- `base_dir`：基础目录，`${base_dir}` 占位符的来源。
- `log`：日志输出方式、级别、ANSI 颜色、文件滚动等（`LogConfig`，`src/common/config.rs:259`）。
- `database`：SeaORM 数据库连接配置（`DbConfig`，`src/common/config.rs:282`；注意 `DbConfig::default()` 在 `db_url` 中内嵌了用户名/密码，见敏感数据章节）。
- `monorepo`：monorepo 根目录、导入目录和 Git 相关路径（`MonoConfig`，`src/common/config.rs:309`）。
- `pack`：Git pack 解码和对象处理相关参数（`PackConfig`，`src/common/config.rs:438`）。
- `lfs`：Git LFS 存储与传输相关配置（`LFSConfig`，`src/common/config.rs:595`）。
- `blame`：blame 计算相关参数（`BlameConfig`，`src/common/config.rs:673`）。
- `build`：构建触发、外部 Orion 构建服务等配置（`BuildConfig`，`src/common/config.rs:710`）。
- `redis`：缓存、队列、连接管理相关配置（`RedisConfig`，`src/common/config.rs:470`）。
- `buck`：Buck 文件上传、限流、清理等配置，`Option<BuckConfig>`（`src/common/config.rs:781`；已存在局部 `BuckConfig::validate()`，`src/common/config.rs:877`，`impl` 块在 `:859`）。
- `object_storage`：本地文件系统、S3/S3 兼容服务、GCS 等对象存储后端配置（`ObjectStorageConfig` 由 sibling `orbit` crate 定义，`src/common/config.rs` 重新导出）。
- `orion_server`：外部构建服务地址等配置，`Option<OrionServerConfig>`（`src/common/config.rs:719`）。
- `sidebar`：侧边栏默认种子数据相关配置（`SidebarConfig`，`src/common/config.rs:939`）。
- `artifacts_gc`：构建产物垃圾回收相关配置（`ArtifactGcConfig`，`src/common/config.rs:337`）。
- `mail`：SMTP 邮件通知配置，`Option<MailConfig>`（`#[serde(default)]`，字段在 `src/common/config.rs:106`；`MailConfig` 结构体在 `:376`，`Default` 在 `:399`）。`mail.password` 为明文 `Option<String>`（`:385`），是当前唯一的「可迁移凭据」候选，见敏感数据章节。

> **字段现状（mail 已存在；oauth 仍缺失）：**
> - **`mail` / `MailConfig`：已是 `Config` 字段且已定义。** `MailConfig` 在 `src/common/config.rs:376`（扁平结构 `enabled`/`smtp_host`/`smtp_port`/`username`/`password`/`from`/`starttls`，`Default` 在 `:399`，端口/STARTTLS 默认值经 `default_smtp_port`/`default_starttls` 提供），`Config.mail: Option<MailConfig>` 在 `:106`（`#[serde(default)]`）。`config/config.toml` 的 `[mail]` 段（`:269-276`）**已被消费**（段内 `smtp_tls`/`tls` 属未知 key，被 serde 丢弃）。真实 `SmtpMailer::new(&MailConfig)` 在 `src/mail/mod.rs:77`，模块经 `main.rs:17` 的 `mod mail;` 编译（**不是** `mod email;`；`src/email/mod.rs` 仅为 re-export shim）。`mail.password` 已是真实字段（`:385`），其 SecretRef 化是阶段 5 的核心剩余工作，见「敏感配置与 Vault 存储方案」与「阶段 5」。
> - **`oauth` / `OAuthConfig`：当前仍不是 `Config` 字段。** 仅在 env list-parse 中出现 `oauth.allowed_cors_origins`（`src/common/config.rs:118`），但由于没有对应字段，该 list key 当前也不映射到任何结构。`config/config.toml` 里的完整 `[oauth]` 段（`:155`，含 `campsite_api_domain`、`tinyship_api_domain`、`api_store_backend`、`allowed_cors_origins`）被整段静默忽略。涉及 OAuth 回调地址的校验、`[oauth.github]` 示例等，都必须在真实新增 `OAuthConfig` 之后再落地。

这种拆分方式让上层调用方可以只依赖自己需要的配置域。但需要注意：**`config/config.toml` 中存在未被任何强类型字段消费的内容：整段孤立的 `[oauth]`（当前唯一的整段孤立顶层段），以及已消费段内的未知 key（如 `[mail]` 中的 `smtp_tls`/`tls`，`config.toml:275-276`）；用户自行增加的未知段同样被静默忽略，配置文件与 Rust 结构之间并非严格一一对应。** 这既是当前的技术债务，也是安全/运维隐患（用户以为写了某段就生效了）。`validate.rs` 落地后应增加“未知/未消费顶层段告警”和“已消费段内未知 key 告警”（至少是 warn，可配置为 error），并考虑在模型上使用 `#[serde(deny_unknown_fields)]` 的白名单模式或后置 key 检查——注意 `MailConfig` 当前**未**使用 `deny_unknown_fields`（`config.rs:389` 注释明确表示丢弃未知字段），与该建议存在张力，需在落地时统一策略。见「推荐加载流水线」。

`Config` 已提供若干测试/构造入口：`Config::mock()`、`Config::load_str()`、`Config::load_sources()`（`src/common/config.rs:129/149/163`）。后续测试辅助应在这些既有入口之上扩展，而不是另起一套。

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
- HTTP 服务：读取监听地址、端口、CORS、Swagger/OpenAPI 等相关配置（OAuth 相关字段目前尚未进入 `Config` 强类型结构，见上节说明）。
- Git pack / LFS：控制对象解码、上传、存储和传输行为。
- 构建系统：配置 Orion 构建服务、触发器和构建产物管理。
- Buck 上传：读取上传限制、清理策略和相关后台任务参数。
- 邮件通知（已编译，dispatcher 已在 mail 启用时启动）：一级模块 `src/mail/mod.rs` 提供 `SmtpMailer`/`NoopMailer`，`MailConfig` 已定义并接入 `Config`；`src/context/mod.rs:46-55` 已在 vault 之后从 `config.mail` 构造 `SmtpMailer` 并 spawn `EmailDispatcher`。作为可迁移凭据的目标消费点，剩余工作是把 `mail.password` 改为 `SecretRef`、把构造失败从静默忽略改为可诊断处理，并完善 dispatcher 生命周期（见「已落地的 mail/notification 子系统现状」）。
- Artifact GC：控制构建产物垃圾回收策略。
- Sidebar 默认数据：为 UI 侧边栏提供初始化种子配置。

### 当前消费点的关键依赖顺序（与 secret 迁移相关）

在评估哪些字段可以改为 `SecretRef` 时，不能只看配置结构，而必须精确追踪消费点在初始化链路中的位置：

1. **`Storage::new`（`src/jupiter/storage/mod.rs:190`，签名 `async fn new(config: Arc<Config>)`）** 在 `AppContext::new` 中第 33–35 行被调用，它内部：
   - 第 191 行 `database_connection(&config.database)` 建立数据库连接；
   - 第 206 行通过 `crate::jupiter::storage::object_storage::ObjectStorageFactory::build(&config.object_storage)` 构造对象存储（S3/GCS/Local），因此 `object_storage.s3.access_key_id` / `secret_access_key` 在 vault 就绪前已被消费；
   - 第 253–263 行 `buck_config.validate()` 若失败会直接 `panic!`（错误信息形如 “Invalid Buck configuration: … Service cannot start …”），这是当前配置校验侵入启动期的一个负面典型，后续应收敛到 `validate.rs` 的集中校验中。
   - 注意 `Storage` 以 `Weak<Config>` 持有配置，`Storage::config()`（`src/jupiter/storage/mod.rs:334`）通过 `upgrade().expect("Config has been dropped")` 返回 `Arc<Config>`——这是一个潜在 panic 点，热加载切换为快照句柄时需一并考虑。
2. **`init_connection(&config.redis)`（`src/context/mod.rs:36`）** 在 `VaultCore::new` 之前执行，因此带密码的 `redis.url` 也属于早期运行时依赖。
3. **`VaultCore::new(storage)`（`src/context/mod.rs:39`）** 之后才就绪，此后消费的配置字段才可纳入可迁移凭据。
4. **SMTP mailer 与 EmailDispatcher 已是 vault 之后的后置消费点。** 真实 `SmtpMailer::new(&MailConfig)` 实现位于 `src/mail/mod.rs:77`，模块经 `main.rs:17` 的 `mod mail;` 编译，`MailConfig` 已定义，`Config.mail` 字段已存在。`src/context/mod.rs:46-55` 在 `VaultCore::new`（`:39`）**之后**从 `config.mail` 构造 `SmtpMailer`，再创建 `EmailDispatcher` 并 `tokio::spawn`；随后 `init_monorepo`（`:60-64`）也在 vault 之后执行。**因此 `mail.password` 已是一个"已存在且仅在 vault 之后消费"的字段——可迁移凭据集合的第一个合格成员**，无需再"先补齐"。当前剩余工作是把 `mail.password` 改为 `SecretRef`，并把 `SmtpMailer::new` 失败被 `if let Ok(m)` 静默忽略的问题改为可诊断错误。在 `VaultCore::new`（`:39`）与这些后置消费点之间，正是插入 secret resolver 的天然窗口。
5. **`ssh_server_key`（`src/server/ssh_server.rs:78` 读取、`:99` 写入）** 已由 vault 管理，但属于 vault 内部 secret，不在 `Config` 结构体中。注意 `:78` 处当前对 `read_secret(...).unwrap()` 会在 secret 缺失/读取失败时 panic，加固时应一并改为可诊断错误。

> 结论：任何在 `Storage::new` 或 `init_connection` 阶段消费的字段都不能直接改为 `SecretRef`，除非先重构初始化顺序。
>
> 额外观察（来自当前代码）：
> - `Storage::new` 内部在构造 buck 相关信号量**之前**就执行了 `buck_config.validate()` 并在失败时 panic（storage/mod.rs:253-260），这是“配置校验侵入启动关键路径”的典型。
> - `crate::jupiter::storage::object_storage::ObjectStorageFactory::build` 在同一阶段被调用，因此 `object_storage.s3.access_key_id` / `secret_access_key`（即使当前为空字符串）在 vault 就绪前就已被“消费路径”触达。
> - `context/mod.rs:36` 的 `init_connection(&config.redis)` 紧随 Storage 之后，仍在 `VaultCore::new` 之前。

## 当前方案的优点

- 启动链路清晰：配置在 CLI 入口统一加载，然后传入后续运行时组件。
- 类型安全：业务代码面对的是 Rust 结构体，而不是散落的字符串键。
- 配置来源灵活：支持命令行、环境变量、项目默认配置和自动生成模板。
- 运行时共享成本低：通过 `Arc<Config>` 只读共享，避免重复解析和复制。
- 领域边界清楚：配置按 `log`、`database`、`redis`、`object_storage`、`buck` 等现有领域拆分，便于维护。
- 适合容器化部署：`MEGA_` 环境变量覆盖机制便于在部署环境中注入差异配置。

## 现有 vault 能力与关键约束

后文的“敏感数据 Vault 化”方案高度依赖现有 `vault` 模块的真实形态，因此先澄清现状，避免规划脱离实现：

- **vault 已是可用的通用 secret store。** `src/contract/vault/integration/vault_core.rs` 基于 `src/vault` 中 vendored 的 RustyVault 模块，已提供 `read_secret`/`write_secret`/`delete_secret`（按 `secret/<name>` 路径存取任意 KV）。因此“把凭据写入 vault”所需的 API **已经存在**，不必新建存储能力。
- **vault 的存储后端就是数据库。** `VaultCore::new(ctx: Storage)` 通过 `JupiterBackend` 把 secret 存进数据库（`ctx.vault_storage()`）。这意味着 vault 必须先有可用的数据库连接才能启动。
- **由此形成明确的依赖链：`Config → Storage(数据库) → Vault`。** `Config::new`（同步）先产出配置 → `Storage::new(Arc<Config>)` 用 `database` 连库 → `VaultCore::new(storage)` 起 vault。**vault 在 `AppContext::new` 阶段才就绪，晚于 `Config::new`。**
- **vault 当前只存了一个 secret：`ssh_server_key`**（`src/server/ssh_server.rs:78` 读取、`:99` 写入）。把全系统凭据集中进来是一次能力跃迁，需配套引导、备份与解封密钥保护，详见风险约束。
- **vault 通过磁盘上的明文 `core_key.json` 自动解封。** 解封分片 `secret_shares` 与 `root_token`（`CoreKey` 结构 `vault_core.rs:16-20`）以明文 JSON（`to_writer_pretty().unwrap()`，`:98-99`）写入 `mega_base()/vault/core_key.json`，启动时读取该文件自动解封。**因此把 secret 搬进 vault，对“能访问该磁盘/文件系统的攻击者”几乎不增加保护**——读 `core_key.json` 即可解封读全部。
- **`core_key.json` 丢失会触发 `delete_all()` 清空并重新初始化所有 secret**（`vault_core.rs:70-86`）。当前实现里，缺失时会先 `println!("Vault core key file does not exist, clearing database and reinitializing...")`（`:71`），再 `vault_storage.delete_all().await.expect(...)`（`:73-77`，**异步**），然后 `rvault.init().await.expect(...)`（`:79-82`），并 `println!("Vault core initialized with root token: {}", result.root_token)`（`:83-86`）把 root token 直接打到 stdout；随后把 shares+root_token 写盘。解封阶段对每个 threshold share 做 `assert!(unseal.is_ok(), ...)`（`:111`）；`log::debug!`（`:114-117`）还会记录 root token。**集中所有凭据后，丢一个本地文件等于丢全部凭据，且初始化路径会主动泄露最高权限材料。**
- 当前 `VaultCore::new` / `config` 路径大量使用 `expect`/`assert`/`println`，与“配置/secret 错误必须可诊断、绝不 panic、敏感材料绝不落日志”的目标严重冲突。阶段 3 的加固必须同时清理这些调用点。

这两条安全/引导事实直接约束了下文方案的边界，必须在规划中显式承接，而不是当作注脚。

## Config 单独成模块的改进方案

当前 `Config` 的类型定义集中在 `src/common/config.rs`（约 1149 行，且仍在增长——`MailConfig` 等新结构已先于本拆分计划加入），而加载器和模板已经位于 `src/common/config/` 目录下（仅 `loader.rs`、`template.rs` 两个文件）。随着配置域继续增加，单文件会同时承担数据模型、解析流程、占位符处理、环境变量适配、默认模板协作、配置校验和错误诊断等职责，后续维护成本会逐步升高。建议将 `Config` 收敛为 `src/config/mod.rs` 这样的顶层目录模块，让配置能力从 `common` 中独立出来，成为系统第一级基础设施模块。

这一改造不只是文件拆分，也应承接当前实现中的注意事项：占位符替换规则需要显式化，配置校验需要集中化，启动期错误需要可诊断化，文档与实际加载优先级需要同步，部分敏感数据可逐步从普通配置文件中剥离并交由 `vault` 模块存储，并在独立配置模块中落地受控热加载能力。

### 总体改造原则

考虑到模块规模（约 1149 行、11 个文件引用 `common::config`）以及上文揭示的 vault 引导/安全约束，本计划采用**分阶段、可独立验证**的策略，而不是一次性大爆炸切换。以下原则贯穿全程：

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
- `validate.rs`：提供集中校验入口，按领域拆分校验函数，例如数据库连接串、监听端口、对象存储后端、路径可用性等，避免非法配置延迟到后续初始化阶段才暴露。首批 hard error 应只覆盖**当前已存在字段**的无争议规则，例如 `database.db_type` 必须为 `postgres`、`database.db_url` scheme 必须为 `postgres`/`postgresql`、端口范围、必填字符串、对象存储后端与对应配置完整性、Buck 并发/大小限制（可吸收现有 `BuckConfig::validate()`，`config.rs:877`），以及“未知/未消费顶层段告警”（当前唯一整段孤立的是 `[oauth]`；`[mail]` 已被消费，但其段内 `smtp_tls`/`tls` 是被丢弃的未知 key，应纳入"已消费段内未知 key 告警"）。**由于 `MailConfig` 已真实存在，针对 mail 的校验（`mail.enabled = true` 时 `smtp_host`/`from` 必填、`password`/`password_ref` 互斥）现在即可加入；但涉及尚不存在字段的校验（OAuth 回调地址等）仍应等 `OAuthConfig` 真实落地后再写。**
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

1. **引导配置（必须留在 TOML/env，永不进本项目 vault）。** 典型是 `database`（连接地址、用户名、密码）。`DbConfig::default()` 的 `db_url = "postgres://mega:mega@localhost:5432/mega"`（`src/common/config.rs:298`）把密码内嵌在连接串中；仓库内的 `config/config.toml` 则使用 `postgres://mono:mono@localhost:5432/mono`（`config/config.toml:28`），两处都是硬编码可预测凭据。由于 vault 存在数据库里、连库才能起 vault，**数据库密码无法作为 vault SecretRef**——这是不可破的引导循环。这类凭据应通过环境变量注入（如 `MEGA_DATABASE__DB_URL` 或拆分后的 `MEGA_DATABASE__PASSWORD`），由部署平台的 secret 机制（K8s Secret、CI secret store 等）保护，**而不是交给本项目的 vault**。
2. **早期运行时依赖（当前也不能直接进 vault）。** 这类字段不是数据库引导项，但在 vault 就绪前或同一初始化阶段已经被消费。当前 `Storage::new` 在 `VaultCore::new` 之前构造对象存储，因此 `object_storage.s3.access_key_id`/`secret_access_key` 暂时不能直接改为 SecretRef；`AppContext::new` 在 vault 前连接 Redis，因此带密码的 `redis.url` 也应按引导/部署平台 secret 处理。若要让对象存储凭据进 vault，必须先把初始化顺序拆成“DB-only Storage -> Vault -> resolve object storage secrets -> 构造完整 Storage/服务”。
3. **可迁移凭据（vault 就绪后才被使用，可改为 SecretRef）。** 这是“消费点晚于 vault 且不阻塞 `AppContext` 构造”的字段。**`mail.password`（`config.rs:385`）现在就是此类的第一个已存在成员**：其消费点 `SmtpMailer::new` 在 `context/mod.rs:46-55` 于 `VaultCore::new`（`:39`）之后运行，并随后启动 `EmailDispatcher`。因此该类的第一项工作不再是"新增字段"，而是"**就地把现成字段从明文改为 `SecretRef`**"。⚠️ 在 SecretRef 化之前，`mail.password` 是一处真实的明文暴露：它可经 `Config`/`MailConfig` 的 `Debug`、`MailConfig` 的 `Serialize`（`config.rs:375`）输出，并在 lettre 边界以明文传入 `Credentials::new`（`mail/mod.rs:96`）——（当前 `config/config.toml` 的 `[mail]` 样例未写入密码，但字段已具备承载明文的能力）。未来新增 OAuth client secret、第三方 API key 等字段同理，只有确认其消费点晚于 vault 且不会阻塞 `AppContext` 构造，才可纳入此类。这些字段在配置文件中改为 `SecretRef`，真实值写入 vault，由消费端在运行期通过 resolver 读取。
4. **非敏感运行参数。** 维持现状，明文留在 TOML。

`Config` 反序列化阶段只构建强类型 `SecretRef`，校验阶段检查引用格式与必填性；**真实 secret 的读取发生在 `AppContext` 起来、vault 就绪之后**，由统一的 secret resolver 负责，并由 resolver 负责缓存、过期、脱敏日志和读取失败诊断。

对于运维命令需要区分两条路径：服务运行时 resolver 在完整 `AppContext` 创建后使用；`config secret set/check` 和 `config validate --resolve-secrets` 则只应建立最小 DB/Vault bootstrap，不能因为写入一个后置凭据而强制初始化 Redis、对象存储或 HTTP 服务。

> 关于数据库连接串：可以把内嵌密码的连接串拆为“普通连接参数 + 密码”，以避免整串成为不可脱敏字符串。但拆出来的密码应走**环境变量/部署平台 secret**，**不是 vault SecretRef**。早前“把 db 密码也拆成 `password_ref` 交给 vault”的设想与引导循环冲突，已废弃。

建议在实现前维护一张字段分类表，作为每个迁移 PR 的依据（**当前真实的可迁移凭据集合已有一个成员：`mail.password`**）：

| 字段 | 当前消费点 | 分类 | 迁移结论 |
| --- | --- | --- | --- |
| `database.db_url` / 拆分后的数据库密码 | `Storage::new` 建库连接（最早） | 引导配置（硬循环） | **永远**只能走 TOML/env/部署平台 secret，不进本项目 vault |
| `redis.url`（若含密码） | `AppContext::new` 中 vault 前 `init_connection` | 早期运行时依赖 | 走 env/部署平台 secret；所有日志与错误必须脱敏 |
| `object_storage.s3.*` / `secret_access_key` | `Storage::new` 中 `crate::jupiter::storage::object_storage::ObjectStorageFactory::build`（vault 前） | 早期运行时依赖 | 先保持现状；若要入 vault，必须先把 Storage 拆成 DB-only → Vault → resolve secrets → 完整构造 |
| `orion_server.db_url` 等 | Orion 作为独立服务使用 | 外部服务配置 | 由 Orion 自己或部署平台管理，monoengine 不应声称代管 |
| `mail.password`（**已存在，明文**，`config.rs:385`） | `SmtpMailer::new`（`context/mod.rs:46-55`，vault 之后，随后启动 `EmailDispatcher`） | **可迁移凭据（第一个合格成员）** | 前置功能工作（`MailConfig` + `Config.mail` + `mod mail` + `mod notification` + vault 后构造）已落地；剩余=就地把 `password` 改 `password_ref` + 把 mailer 构造失败改为可诊断处理 |
| `ssh_server_key`、PGP/Nostr 等现有 vault secret | 已由 vault 管理（vault 内部） | vault 内部 secret | 必须先完成 `core_key.json` 权限收紧、fail-closed、root token 脱敏、DR 演练，才能声称有实质安全收益 |

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

**与现有 Vault API 的路径映射**：`secret/{name}` 前缀是在 `read_secret`/`write_secret`/`delete_secret` 内部（`vault_core.rs:165/173/181`，经 `format!("secret/{name}")`）拼接的，**不是**在底层 `read_api`/`write_api`。因此 `vault://secret/config/prod/mail/password#value` 应解析为 `read_secret("config/prod/mail/password")`，再从返回的 KV map 中读取 `value` 字段；不带 `#field` 时默认读取 `value` 字段。resolver 必须以**去前缀**的 name 调用 `read_secret`，不能把完整 URI 直接传入，否则会产生 `secret/secret/...` 这类路径错误。`version` 字段首版只作为预留元数据，除非 vault 后端实际提供版本语义，否则不得在文档或 API 中承诺 KV v2 风格版本读取。

**`mail.password` 的过渡策略（现已可直接实施）**：
- `mail.password`（`config.rs:385`，`Option<String>`）已是真实字段，迁移期允许两种形态共存：
  - `password = "plain_text"`（兼容期保留，输出 deprecation warning）
  - `password_ref = "vault://secret/config/prod/mail/password#value"`（推荐；profile/命名空间按部署环境替换）
- 反序列化时使用自定义 visitor 或 `#[serde(with = "...")]` 处理互斥逻辑：两者同时存在时为 hard error；两者都不存在时按原语义处理（`None` 或报错取决于字段必填性）。
- **兼容性注意**：新增 `password_ref` 会改变 `MailConfig` 的 wire 形态，必须保证现有携带 `password = ...` 的 `[mail]` 段仍能反序列化（`password_ref` 设为 `Option` + `#[serde(default)]`）。
- 完全迁移后，可移除 `password` 字段，仅保留 `password_ref`。由于 `mail.password` 现已存在，该兼容策略可以**立即**针对现成字段实施，不再受"等 `MailConfig` 接入"的前置条件约束。OAuth client secret 等仍未落地字段的同类策略，仍需等对应 schema 真实存在后再写。

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
- **`mail.password` 现已是明文 `Option<String>`（`config.rs:385`）且 `MailConfig` 派生 `Serialize`/`Debug`（`config.rs:375`）**：任何对 `Config`/`MailConfig` 的 `Debug`、JSON 序列化、错误链回显都可能泄露 SMTP 密码；在 SecretRef 化前应优先用 `SecretString` 包裹该字段，并避免整体序列化 `MailConfig`；
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
- **其他引导/早期凭据**：Redis URL、当前阶段的对象存储 key 等同样不在 `config init` 结果中预设真实凭据，也不能默认生成本项目 Vault SecretRef；只保留空字符串、示例占位符或部署平台 secret 注入说明。只有确认晚于 vault 消费且已真实接入 `Config` 的字段（例如阶段 5 补齐后的 `mail.password_ref`）才可生成 `SecretRef` 占位符。
- **vault 自动初始化**：`VaultCore::new` 在 `core_key.json` 丢失时会 `delete_all()` 并重新初始化，这一行为不应被 `config init` 触发；`config init` 只操作配置文件，不接触数据库或 vault。

#### secret 热加载边界

敏感数据的热加载不等同于普通配置热加载。配置文件中的 `SecretRef` 变化属于配置变更，需要走 `reload.rs` 的白名单、校验和回滚流程；而 vault 内部 secret 值轮换由 vault 模块和 secret resolver 管理，业务组件只在下一次获取、缓存过期或收到明确轮换通知时读取新值。任何 secret 读取失败都不能把明文写入日志，错误信息只输出 secret 路径、版本、字段路径和失败原因。

### secret 解析的依赖顺序

由于 `Config → Storage(DB) → Vault`，secret 解析必须排在 vault 就绪之后，并按消费端对 vault 的依赖排序。当前服务启动的真实顺序更精确地说是：

```text
Config::new                                  # context/mod.rs:30 起 AppContext::new
  -> Storage::new(config)                    # :33-35  建数据库连接、构造对象存储、初始化部分存储服务（.expect panic）
  -> init_connection(redis)                  # :36     连接 Redis
  -> VaultCore::new(storage)                 # :38-39  vault 才就绪
  -> SmtpMailer::new(config.mail)            # :46-49  vault 之后；失败当前被 if let Ok 静默忽略
  -> EmailDispatcher::new(...) + spawn       # :50-55  mail 启用时启动后台 outbox dispatcher
  -> init_monorepo(config.monorepo)          # :60-64  vault 之后（.expect panic）
  -> HTTP/SSH/multi 服务分发                 # commands/service/mod.rs:33-36
```

因此，当前 `Storage::new` 或 Redis 初始化阶段已经消费的任何 secret，都不能直接改为 vault SecretRef。只适合迁移在 vault 之后才初始化/使用的字段。**SMTP mailer 密码的前提（`mail`/`MailConfig` 已接入、`notification` 已编译、mailer 和 dispatcher 构造晚于 vault）现已满足**（`MailConfig` 在 `config.rs:376`，mailer 在 `context/mod.rs:48` 于 `VaultCore::new` `:39` 之后构造），因此 `mail.password` 已正式属于这一类；剩余工作是把 `mail.password` 改为 `password_ref`，并把构造失败静默忽略改为可诊断处理。

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

`config init` 的职责应保持清晰：创建或检查 `config/config.toml`，写入基础样例配置，派生 `${base_dir}`、日志目录、缓存目录和本地对象存储目录，为可迁移凭据生成 `SecretRef` 占位引用，并输出后续需要执行的 `config secret set` 命令清单。示例应尽量贴近当前 schema，**只为真实存在的字段生成占位引用**。

> 注意：下面的 `[mail]` 示例对应 `MailConfig` 的字段形态（扁平结构：`enabled`/`smtp_host`/`smtp_port`/`username`/`password`/`from`/`starttls`，`config.rs:376`）。**该结构现已作为 `MailConfig` 接入 `Config`**，且 `config/config.toml` 已**实际携带**一个 `[mail]` 段（`:269-276`，`enabled = true`），因此该示例已可直接生成。两点提示：(1) 仓库样例用 `starttls = false`、端口 `2525` 关闭了 STARTTLS，覆盖了 `MailConfig` 的安全默认（`587`/`true`，`config.rs:392/395`），仅适合本地开发，`config init` 生成生产骨架时应使用安全默认；(2) 样例里的 `smtp_tls`/`tls` 是 `MailConfig` 不识别的字段，会被 serde 丢弃，不应写入生成结果。`password_ref` 占位可在 SecretRef 基础设施（阶段 5）落地后生成；当前 mailer 密码若需配置，先以明文/env 注入，迁移后改 `password_ref`，例如：

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

改造后建议将 `config/config.toml` 定位为基础样例配置，并满足以下约束：默认使用本地、无副作用、低依赖的配置值；所有路径通过 `${base_dir}` 或测试临时目录派生；对象存储默认使用 `local`；外部服务地址只作为示例或显式禁用；只有已真实接入 `Config`、且确认属于可迁移凭据的字段才改为 `SecretRef` 示例而非明文；仍属于引导配置或早期运行时依赖的敏感字段明确要求通过 `MEGA_*`、`.env.test` 或部署平台 secret 注入。该文件应纳入配置样例校验，保证它可以被解析、展开和通过基础校验，但不再要求它直接满足所有集成测试的外部依赖。

自动化测试应采用独立的配置方案：单元测试优先通过 `testing.rs`（基于 `Config::mock()`/`load_str()`）构造内存配置或最小 TOML 片段；需要文件加载链路的测试使用临时目录生成测试配置文件，并显式传入 `--config` 或 `MEGA_CONFIG`；集成测试继续通过 `.env.test` 提供数据库、Redis、邮件等外部依赖端点，但 `.env.test` 只负责测试环境覆盖，不应修改仓库中的 `config/config.toml`。测试配置中的 `base_dir`、数据库名、对象存储根目录、日志目录和缓存目录都应隔离到测试临时目录，避免并发测试互相污染。

对于依赖敏感凭据的测试，不应在测试 TOML、`.env.test` 或日志中写入真实 API Key。配置模块应提供可注入的测试 secret resolver 或 vault 测试后端（现有 vault 测试已使用 `tempfile` + `test_storage` 构造隔离实例，可复用此模式），用固定的假 secret、过期 secret、缺失 secret 和权限失败场景覆盖消费端行为。热加载相关测试也应使用测试配置文件和临时 resolver：分别验证基础字段热更新成功、不可热更新字段只告警不生效、SecretRef 变更遵循白名单、候选配置校验失败时旧配置继续生效。

CI 中应增加专门的配置验证任务，至少覆盖三类输入：仓库基础样例 `config/config.toml`、默认模板生成结果、自动化测试生成的测试配置。验证内容包括 TOML 语法、环境变量覆盖、占位符展开、`SecretRef` 格式、集中校验、脱敏错误输出和消费端配置构造。这样可以把“配置能否作为测试输入使用”变成稳定的自动化检查，而不是依赖人工维护一份混合用途的基础配置。

还应补充配置兼容性测试矩阵：坏 TOML、坏环境变量类型、未知占位符、profile 类型冲突、数组覆盖语义、未知/未消费顶层段告警、SecretRef 路径缺少 `#field`、vault key 缺失、secret 缺失、secret 字段缺失、权限失败、错误信息脱敏、非 PostgreSQL 数据库配置被拒绝启动。`password` 与 `password_ref` 同时存在这类互斥测试，应在对应字段真实接入后再加入。

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

整体拆为多个阶段，每个阶段独立可编译、可回归、可单独评审合并。注意：只有纯移动和 re-export shim 可以承诺”零行为变更”；错误模型、校验规则、命令加载模型、vault bootstrap 和 SecretRef 迁移都会改变失败形态，必须单独评审。

> **关键前置依赖说明（2026-06-14 更新）**：本阶段规划有两个不可避免的跨模块前置：
> 1. **日志脱敏工具（redaction）**应作为独立前置工作优先完成，供 config、vault、mail、notification 等模块共享使用，特别是为 vault.md 的 P0 阶段 A 服务。
> 2. **CLI 两阶段加载（LoadMode）的设计框架**需要 config + vault 团队协同完成，作为单一 source of truth。config.md 阶段 2 和 vault.md 阶段 D 都依赖这个共同设计，不应分别实施。
> 3. **vault.md 阶段 B（最小 DB/Vault bootstrap）必须在 config.md 阶段 3 之前完成**，因为 config 阶段 3 直接依赖 bootstrap 拆分的结果。

**阶段 0a — 纯结构拆分（保留兼容 shim，零行为变更）**

1. 新建 `src/config/` 目录，将 `src/common/config.rs` 移动为 `src/config/mod.rs`，并把 `loader.rs`、`template.rs` 一并迁入 `src/config/`。
2. 在 `src/main.rs` 新增顶层 `mod config;`（可见性按 shim 需要设置为 `pub(crate)` 或等价）；在 `src/common/mod.rs` 中将 `pub mod config;` 改为 wrapper/re-export shim，使 `crate::common::config::*` 在过渡期继续可用。**本阶段不改任何业务调用方路径。**
3. 拆出 `model.rs`（结构体）、`source.rs`、`expand.rs`，保持 `Config::new` 外部行为不变，只改内部组织方式。
4. 补充单元测试覆盖路径定位、env 覆盖、列表解析和占位符展开，确认移动前后行为一致。

> **验收标准**：按仓库要求执行 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`source .env.test && cargo test --all`；另跑 `cargo build`、`cargo build --tests` 做 sanity check。除 `src/config/`、`src/common/mod.rs`、`src/main.rs` 以及移动后模块内部必要 import 修正外，无业务调用方 import 路径变更。

**阶段 0b — 错误模型、脱敏与保守校验**

> **与 vault.md 的强绑定（2026-06-14 更新）**：本阶段的脱敏工具（redaction）是 vault.md 阶段 A（P0）的前置依赖。vault 需要在日志中删除 root token、secret shares 等敏感信息，该能力应来自本阶段建立的统一工具。建议脱敏工具优先设计和实现，使其可供 config、vault、mail、notification 等模块共享复用。

5. 新增 `error.rs`，把 `variable_placeholder_substitute` 等加载路径上的 `unwrap`/`expect`/`panic` 替换为可诊断错误；这会改变失败形态，不应并入 0a。
6. **建立统一 redaction 工具**（优先级：高，可作为独立前置），位置建议为 `src/common/redaction.rs` 或 `src/config/redaction.rs`，先覆盖数据库 URL、Redis URL、Vault root token、secret shares、对象存储 key 等现有日志/错误泄露点。该工具应提供通用的字段脱敏接口，供 vault、mail、notification 等模块在日志、错误信息、Debug 输出中使用。
7. 新增 `validate.rs`，先实现低风险、无争议的 hard error 校验（端口范围、必填字符串、明显非法枚举、Buck 限制等）；涉及部署兼容性的严格校验先以 warning 过渡。`password`/`password_ref` 互斥等规则只能在对应字段真实接入后加入。
8. 补充测试覆盖校验失败、错误信息、脱敏输出和 warning/hard error 边界。

> **验收标准**：配置损坏时启动不再 panic，而是输出包含配置文件路径、字段路径和修复建议的诊断信息；所有包含敏感数据的日志/错误信息经过 redaction；新增校验用例测试覆盖；redaction 工具可被外部模块导入使用。

**阶段 1 — 消费端路径迁移 + 移除 shim**

9. 将所有 `crate::common::config::*`/`common::config::*` 改为 `crate::config::*`/`config::*`（约 12 个文件）。
10. 构建确认无残留引用后，删除阶段 0a 的 re-export shim，并从 `common` 彻底移除配置承载职责。本阶段不改运行语义。

> **验收标准**：全仓无 `common::config` 引用；按仓库要求执行格式、clippy、测试三项必过 gate，并保留 `cargo build`、`cargo build --tests` sanity check。

**阶段 2 — CLI LoadMode + 非 Vault `config` 命令**

> **与 vault.md 的协同设计（2026-06-14 更新）**：本阶段的 CLI 改造直接与 vault.md 阶段 D 相关。两个文档都计划实现 `LoadMode` 机制，但应作为**单一跨模块设计**而非分别实施。建议先由 config + vault 团队协同设计和定义 `LoadMode` 框架（如 `src/cli/load_mode.rs` 中的 enum），明确各模式的启动路径和依赖，再分别在两个文档的对应阶段使用这个共同框架。

11. **先在 CLI 层支持两阶段加载和 `LoadMode`**（与 vault.md 阶段 D 协同完成）：改造 `cli::parse` 与命令注册，先解析子命令，再按命令声明的 `LoadMode` 选择加载层级（`None`、`ConfigPath`、`RawSources`、`ParsedConfig`、`VaultBootstrap`、`FullAppContext`）。该设计应在 config + vault 团队的协同下完成，确保所有依赖此框架的模块（config、vault、mail、notification）都能正确使用。
12. 新增 `monoengine config init` 与不解析 secret 的 `config validate`，确保配置不存在或配置损坏时仍能给出诊断，而不是被 CLI 预加载拦截。
13. 新增 `config secret ref` 的纯规则生成能力；不依赖 vault 的子命令不得隐式创建数据库或启动服务依赖。

> **验收标准**：`monoengine config init` 可在无配置目录下运行并生成样例；`monoengine config validate` 能在坏配置时输出诊断而非 panic；`config secret ref` 可生成与 profile/命名空间一致的 `vault://secret/...#field` 引用；命令帮助文档完整；CLI `LoadMode` 框架已定义、文档齐全、所有命令均正确声明自己的 LoadMode 需求。

**阶段 3 — 最小 DB/Vault bootstrap + core_key 加固**

> **与 vault.md 的依赖关系（2026-06-14 更新）**：本阶段的"最小 DB/Vault bootstrap 拆分"依赖 **vault.md 阶段 B（拆出最小 DB/Vault bootstrap seam）先完成**。vault B 主要负责改造 `JupiterBackend` 的依赖、拆分存储接口等，使得最小 bootstrap 在技术上可行；config 阶段 3 则依赖这些改造结果来实现自己的 bootstrap 能力。建议 vault B 与 config 3 的时间规划应同期或 vault B 稍早。

14. 拆出最小 DB/Vault bootstrap 能力（依赖 vault.md 阶段 B 完成的拆分），只建立数据库连接和 vault 所需 storage，不构造 Redis、对象存储、HTTP/SSH 服务、monorepo 初始化或后台任务。
15. 将普通启动路径中的 `core_key.json` 缺失行为改为 fail-closed，不再自动 `delete_all()` 并重新初始化；破坏性 reset/init 必须由显式运维命令触发。（注：此处与 vault.md 阶段 A 工作项 5 有设计协调需求，应确保两者对"fail-closed 判定依据"的理解一致。）
16. 删除 vault 初始化路径中 root token、secret shares、完整 key 文件内容的所有输出；`VaultCore::new`/bootstrap 路径逐步改为返回 `Result`。（注：此处依赖阶段 0b 建立的脱敏工具。）
17. 收紧 `core_key.json` 与父目录权限，补充备份恢复和非本地明文 key 文件的生产部署说明。

> **验收标准**：`config validate --resolve-secrets` 的底层 bootstrap 不依赖 Redis/S3；`core_key.json` 缺失时不会清空 vault 数据；root token 不再进入 stdout/stderr/tracing；key 文件权限收紧（Unix `0600`，目录 `0700` 或等价）；最小 bootstrap 能力已独立可测，不依赖完整 AppContext。

**阶段 4 — `config secret set/check` 命令**

18. 基于阶段 3 的最小 DB/Vault bootstrap 新增 `config secret set`、`config secret check`、`config validate --resolve-secrets`。
19. `config secret set` 默认只接受 `--value-stdin` 或隐藏输入，写入后输出 `vault://secret/...#field` 引用；默认不修改 TOML 文件。
20. 命令只允许写入可迁移凭据；数据库密码、Redis URL、当前阶段对象存储 key 等引导/早期依赖必须给出部署平台 secret 指引。

> **验收标准**：`config secret set --value-stdin` 正确写入 vault 且不在日志、进程参数或错误信息中泄露；`config secret check` 能区分 vault 不可用、secret 缺失、字段缺失和权限失败；所有路径映射避免 `secret/secret/...`。

**阶段 5 — SecretRef 基础设施 + 第一批可迁移凭据**

> **重新基线（2026-06-09）**：本阶段原本的"前置子步骤"——定义 `MailConfig`、给 `Config` 加 `mail`、接入 mail 模块、接入 notification 模块、在 vault 之后构造 `SmtpMailer` 并启动 `EmailDispatcher`——**已落地**（`MailConfig` `config.rs:376`；`Config.mail` `:106`；`mod mail;` `main.rs:17`；`mod notification;` `main.rs:18`；vault 之后构造与启动点 `context/mod.rs:46-55`）。因此本阶段不再以"新建消费字段"为起点，而以"SecretRef 基础设施 + 把现成 `mail.password` 迁移 + 把 mailer/dispatcher 失败路径可诊断化"为主。

21. 新增 `secret.rs`：定义 `SecretRef`、secret resolver trait，适配现有 `VaultCoreInterface`；resolver 接收已就绪的 vault 句柄，不在 `Config::new` 调用。
22. 梳理并提交字段依赖表，明确每个字段是引导配置、早期运行时依赖、可迁移凭据还是非敏感参数（见上表，"可迁移凭据"一类现有一个成员 `mail.password`）。
23. **（已完成接入，仍需加固）真实后置消费者的接入。** 类型/模块/字段/构造点和 dispatcher 启动点均已就位，剩余的收尾工作是：
   - 把 `context/mod.rs:46-55` 中 `SmtpMailer::new` 失败被 `if let Ok(m)` 静默忽略的问题改为可诊断处理。
   - 完善 `EmailDispatcher::run` 的生命周期、退避/并发策略和服务关闭协调。
   - （`MailConfig` 定义、`Config.mail` 字段、`mod mail;`、`mod notification;`、vault 之后的 mailer/dispatcher 启动点——**已完成，无需再做**。）
24. 把 `mail.password` 改为 `SecretRef`（迁移期允许 `password` 与 `password_ref` 互斥并存，检测到明文时输出 deprecation warning；两者同时存在为 hard error；`password_ref` 用 `Option` + `#[serde(default)]` 以不破坏现有 `password` 配置）。OAuth client secret 只有在真实 `OAuthConfig` schema 存在、消费点确认晚于 vault 后才能迁移；对象存储凭据只有在阶段 7 的初始化顺序重构完成后才能迁移。
25. 对应消费端（邮件发送路径等）改为通过 resolver 读取，任何错误路径和日志**只**暴露脱敏引用；SMTP 发送失败的语义（hard error 还是降级到 NoopMailer）必须在配置或环境上明确表达，不应在生产环境静默降级导致邮件静默丢失。

> **验收标准（已达成的基线已剔除）**：以下基线**已满足**，无需再作为验收项：`mail`/`MailConfig` 已定义并被 `Config` 反序列化、mail 模块已接入主 crate（经 `mod mail;`）、notification 模块已接入主 crate（经 `mod notification;`）、vault 就绪后会构造 `SmtpMailer` 并 spawn `EmailDispatcher`。本阶段真正待验收的开放项是：`SmtpMailer::new` 失败不再被静默忽略；dispatcher 有界 poll/并发/退避和关闭语义明确；`mail.password_ref` 可被 resolver 解析且邮件功能正常；resolver 缓存、evict、错误脱敏全部生效；任何 secret 读取失败不会 panic、不会把明文写入日志或错误链；字段依赖表作为正式文档提交。
> **范围提示**：如果团队短期内不打算真正启用邮件通知，可只落地"SecretRef 基础设施 + resolver trait + 最小测试替身 + 字段分类表"，把"spawn 真实 dispatcher"推迟。但需注意：与早期版本不同，现在**已经存在**一个真实的后置消费字段（`mail.password`），所以 SecretRef 迁移有真实落点，不再是"无消费者的空转"。

**阶段 6 — 基础样例配置、Profile 与测试配置分层 + CI**

26. 将 `config/config.toml` 改造为基础样例配置：移除真实敏感值、保留与当前 schema 对齐的本地默认值与初始化指引；只有当字段已经真实接入 `Config` 且确认属于可迁移凭据时，才加入对应 `SecretRef` 示例；纳入配置样例校验。
27. 固定 Profile 文件命名、加载优先级、数组覆盖语义和 SecretRef namespace，并补充 profile 合并测试。
28. 新增 `testing.rs`（基于既有 `mock()`/`load_str()`/`load_sources()`），提供测试配置构造、临时目录派生、`.env.test` 覆盖合并、测试 secret resolver。
29. 建立分层测试策略并接入 CI：单元测试用内存/最小 TOML，加载测试用临时文件，集成测试用 `.env.test` + `MEGA_CONFIG` 指向隔离配置；CI 覆盖基础样例、默认模板、`config init` 结果、profile 合并结果与测试配置生成结果。

> **验收标准**：`config/config.toml` 不含真实生产密码或可复用生产凭据；`cargo test --all` 不依赖仓库中的 `config/config.toml` 作为隐式共享状态；CI 新增配置校验任务且通过。

**阶段 7 — 对象存储等早期依赖的后置初始化重构（可选）**

30. 如果要让对象存储凭据进入 vault，先把 `Storage::new` 拆为 DB-only storage、vault bootstrap、secret resolve、完整 storage/service 初始化。
31. 重新梳理 Redis、对象存储、Orion 等字段依赖表，只有在确认消费点晚于 vault 且失败语义可接受后，才允许迁移为 SecretRef。

> **验收标准**：S3/S3-compatible 凭据迁移前，服务启动链路中不再在 vault 前构造对象存储；缺少对象存储 secret 时返回可诊断错误，不影响 `config secret set/check` 对其他 secret 的操作。

**阶段 8 — 受控热加载（独立变更）**

32. 新增 `reload.rs`：监听配置文件变化，复用加载流水线构建候选配置，按字段白名单计算差异，向订阅组件发布变更，失败时保留旧配置继续生效。
33. 将运行时注入从直接共享 `Arc<Config>` 调整为共享快照句柄（如 `Arc<ConfigHandle>`/`ArcSwap`）；逐点确认“取出 Arc 后跨 await 持有”的调用语义，新增依赖前评估必要性。注意 `Storage` 当前以 `Weak<Config>` 持有配置、`config()` 用 `expect` 解引用，切换快照句柄时需同步调整该访问路径。
34. 改造支持热加载的消费端订阅方式（日志、功能开关、任务调度、邮件通知等只订阅各自可热更新字段；不可热更新字段变化只告警提示重启）。
35. 补充热加载测试：可热更新字段生效、不可热更新字段告警不生效、SecretRef 变更遵循白名单、候选校验失败回滚。

> **验收标准**：白名单字段（如 `log.level`）变更后无需重启即可生效；数据库地址变更只告警不重建连接；热加载失败时进程继续运行且保留旧配置；全量回归测试通过。

**贯穿全程**

36. 每个阶段同步更新 `README`、`config/config.toml` 注释和本文档，确保加载优先级、模块路径、`config` 命令使用方式与引导顺序、消费端访问方式、环境变量规则、敏感配置存储边界和热加载限制与实现一致。

### 前置依赖矩阵（2026-06-14 更新）

本文档（config.md）与三个前置文档（vault.md、mail.md、notification.md）的依赖关系如下：

| config 阶段 | 本阶段主要工作 | 对 vault 的依赖 | 对 mail 的依赖 | 对 notification 的依赖 |
|-----------|------------|-------------|-----------|-----------------|
| **0a** | 纯结构拆分 | 无 | 无 | 无 |
| **0b** | redaction 工具 + 错误模型 | ← vault A 依赖此 | 支持后续日志脱敏 | 支持后续日志脱敏 |
| **1** | 路径迁移 | 无 | 无 | 无 |
| **2** | CLI LoadMode + config 命令 | ← 与 vault D 协同 | 无 | 无 |
| **3** | 最小 bootstrap | ← vault B 完成后 | 支持后续运维 | 支持后续运维 |
| **4** | secret set/check 命令 | 依赖 vault B 的拆分 | 开始支持 mail 的 secret | 支持 notification 的 secret |
| **5** | SecretRef + resolver | 与 vault E 协同 | mail 作为第一消费者 → mail 2 | 后续支持 notification |

**关键同步点：**
1. 日志脱敏工具（config 0b）→ vault A、mail 1、notification 0
2. CLI LoadMode 框架（config 2 与 vault D 协同）→ config 4、vault E
3. 最小 bootstrap（vault B）→ config 3
4. SecretRef 基础（config 5）→ mail 2 → notification 1-3

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
- **BuckConfig::validate() 当前在 `Storage::new` 中 panic（`src/jupiter/storage/mod.rs:250–260`）**，这是配置校验侵入启动期的负面典型。`validate.rs` 落地后，此类校验应收敛到 `Config::validate()` 中，在 `Config::new` 阶段返回可诊断错误，而不是延迟到 `Storage::new` 时 panic。
- **环境变量注入的可见性风险**：引导配置通过 `MEGA_*` 环境变量注入时，需意识到 `/proc/<pid>/environ`、systemd journal、容器 inspect 等场景的泄露风险。生产高敏感部署应优先使用文件挂载 secret，并通过占位符读取。
- **config init 不得生成可预测默认密码**：`DbConfig::default()` 和当前 `config/config.toml` 中的硬编码密码（`postgres://mega:mega@...`）应在 `config init` 生成结果中移除，改为强制用户通过环境变量、文件挂载 secret 或部署平台 secret 注入；数据库密码不能通过本项目 vault 注入。
- **`orion_server.db_url` 是外部服务凭据**，不在 monoengine vault 管理范围内。若 Orion 自身需要 secret 管理，应由 Orion 独立解决，monoengine 只作为客户端通过部署平台 secret 注入其连接参数。

### 必须先完成的前置工作（2026-06-14）

在启动本计划的任何阶段之前，以下前置条件必须满足或明确规划：

1. **日志脱敏工具（redaction 模块）**
   - 位置建议：`src/common/redaction.rs` 或 `src/config/redaction.rs`
   - 职责：提供统一的敏感信息脱敏接口（URL、token、key、shares 等）
   - 需要的模块：config、vault、mail、notification
   - **立即需要**（作为阶段 0b 的一部分，或独立前置）

2. **CLI 两阶段加载（LoadMode）的共同设计框架**
   - 位置建议：`src/cli/load_mode.rs`
   - 职责：定义 LoadMode enum（`None`、`ConfigPath`、`RawSources`、`ParsedConfig`、`VaultBootstrap`、`FullAppContext`）、加载流程、依赖关系
   - 所有者：config + vault 团队协同（不是分别设计）
   - **需要在开始阶段 2 和 vault 阶段 D 前**完成协同设计

3. **vault.md 与 config.md 的阶段协调**
   - vault B（最小 bootstrap）应在 config 3 之前或同期完成
   - 两个团队应明确交付时间表，避免后者因等待而延期

### 预期收益

- 配置模块边界更清楚，新增配置域时只需扩展模型和校验，不必继续放大单文件。
- 加载流程更容易测试，每个阶段都可以独立构造输入和断言输出。
- 启动失败信息更明确：消灭加载路径上的 panic，部署排障时可以直接定位到配置文件、字段和值，并能区分“引用格式非法”与“真实值不可读”。
- 可迁移凭据从普通配置文件中剥离，**不再进入 git 仓库、日志和错误信息**（在日志脱敏与 `core_key.json` 加固后，进一步降低静态泄露风险）；引导配置和早期运行时依赖则交由部署平台 secret 机制管理，职责清晰。
- `monoengine config init` 与 `config secret` 命令让配置初始化、SecretRef 生成、Vault 写入和解析检查形成标准流程；`config secret` 通过最小 DB/Vault bootstrap 工作，不被 Redis、S3 或完整服务初始化阻塞。
- 基础配置与测试配置职责分离，`config/config.toml` 更适合作为可提交的样例和本地起点，自动化测试则获得可重复、可隔离、可校验的配置输入。
- 配置热加载（后续阶段）有明确边界，可在不重启服务的情况下更新安全字段，同时避免长生命周期资源被隐式替换。
- 为后续配置文档生成、配置样例校验和部署前 dry-run 检查打下基础。

## 改进方案多维评估小结

下表是对本改进方案按各评估维度的结论（已综合代码现状与文档规划）：

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | **高（9/10）**。准确抓住 `Config(synchronous) → Storage(DB+ObjectStorage) → VaultCore` 这一不可打破的引导循环，正确导出四类字段划分（引导 / 早期运行时依赖 / 可迁移凭据 / 非敏感），并坚持“先有真实晚绑定消费者才能谈迁移”的原则。分阶段 + shim 的策略与单二进制 + sea-orm + vendored `libvault` 的现实匹配良好。 |
| **可行性** | **中高（7.5/10）**。结构拆分、错误模型收敛、CLI LoadMode、最小 DB/Vault bootstrap、SecretRef 基础设施在 Rust 中均可实现。主要硬约束与风险点已被文档自身正确识别：(a) 必须先完成“定义 MailConfig + 接入 email 模块 + 让其成为 vault 之后的真实消费者”这一功能前置工作；(b) CLI 必须从“总是先完整加载 Config”改为两阶段模型；(c) vault 加固（core_key 权限、fail-closed、root token 脱敏）是生产化先决条件。 |
| **完整性** | **中（7/10）**。文档覆盖了加载链路、模块拆分、secret 分类、命令、测试分层、热加载等主要方面。**当前不足**：对“现状 vs 目标”的视觉区分仍不够强（虽有校准表）；对已存在的孤立 `email` 模块 + 死 `[mail]`/`[oauth]` 段的债务描述可更突出；对 `variable_placeholder_substitute` 的具体脆弱实现、`load_str` 与主路径的 list_parse 不一致、`mega_base` 自身的 panic 点等细节覆盖可更细；跨平台（非 Unix）权限与凭据注入的等价方案较弱。 |
| **安全性** | **强（8.5/10）**。文档最优秀的部分之一。明确指出在 `core_key.json` 仍明文落盘、自动解封仍打印 root token、delete_all 仍为默认行为时，“搬进 vault”仅能提供“不进 git/不进普通日志”的有限收益，而不能提供对抗磁盘读取攻击者的保护。要求日志脱敏、stdin 写入、SecretString、fail-closed、DR 演练等均为正确方向。需在阶段 3 把 vault 代码里现有的 `println!` + `assert!` + `log::debug!` root token 作为必须消除项。 |
| **功能正确性与接口兼容性** | **良好（8/10）**。校准段已修正多处行号失效、`mail`/`oauth` 不存在、`MailConfig` 未定义等问题；`SecretRef` 到 `read_secret(name)` 的路径映射（`write_api("secret/{name}")`）与 vault_core 实现一致。仍需注意：`config` 作为顶层模块名会与 `config` crate 冲突（当前代码用 `c::` 别名，已在文档中提及）；re-export shim 的移除时机必须在所有调用方迁移完成后统一进行。 |
| **数据流与控制流正确性** | **正确（9/10）**。`AppContext::new` 中 `Storage::new → init_connection(redis) → VaultCore::new → SmtpMailer/EmailDispatcher → init_monorepo` 的实际顺序与文档描述完全一致。secret 只能在 vault 就绪后解析、运维命令（secret set/check）必须用最小 bootstrap 而非完整 AppContext 的结论均正确。 |
| **性能与效率** | **可接受（8/10）**。resolver 内存缓存 + TTL、`Arc` 只读共享均合理。`variable_placeholder_substitute` 的两次全树 collect + clone 在启动期可忽略，主要问题是其 panic 语义而非 CPU 成本。热加载白名单设计也避免了不必要的长连接重建。 |
| **可靠性与容错性** | **改进潜力大（8/10）**。计划中的“消灭加载路径 panic、fail-closed、热加载失败回滚保留旧配置、错误带字段路径+修复建议”等均会显著提升。当前代码的若干 `panic!`/`expect`/`assert`（占位符、Buck 校验、Storage.config、vault 解封、mega_base 等）已被文档准确定位为必须在对应阶段消除的点。 |
| **兼容性与互操作性** | **良好（8/10）**。serde `Option` + `#[serde(default)]`、废弃字段 WARN 过渡期、re-export shim、Profile 深合并（数组整体替换而非追加）的语义均已明确。需补充：未知顶层段的告警策略、`0600/0700` 在 Windows/macOS 下的等价实现（或明确“生产仅支持类 Unix”）、以及 `config/config.toml` 作为样例时应 `deny_unknown_fields` 或至少 warn。 |
| **可扩展性与可维护性** | **良好（8.5/10）**。把配置从 `common` 提升为一级 `src/config/` 模块、按职责拆分为 model/loader/expand/secret/validate/testing/error 等小文件，是正确的长期方向。`Config` 成为基础设施后，新增领域只需扩展 model + 对应校验/展开规则即可。需注意拆分后的 `pub use` shim 策略和“先路径迁移、再语义变更”的顺序。 |
| **合规性与标准符合性** | **良好（8/10）**。推荐的 secrecy/zeroize、stdin 避免历史记录、CI 覆盖配置样例+坏输入矩阵、字段分类表作为变更依据等，均符合现代凭据管理最佳实践。引入新依赖（secrecy 等）前要求评估编译/二进制影响的约束是正确的。 |

> **跨平台与部署现实补充**：`core_key.json` 权限（0600）、目录（0700）、`/proc` 限制、`systemd LoadCredential`、K8s Secret volume + etcd encryption at rest 等措施主要描述的是类 Unix 语义。对于 Windows、macOS 或受限容器环境，需要在实施相应阶段时提供等价机制（或在文档中显式声明“生产高敏感部署当前仅推荐类 Unix 平台”）。Kubernetes Secret 本身不是加密保险箱，`kubectl describe`、审计日志、etcd 未加密时仍可能泄露。

## 小结

`monoengine` 当前的 `Config` 实现采用“启动期集中加载（`Config::new` 同步）、`config` crate 做 TOML+env 叠加 + 占位符展开、强类型反序列化、运行时通过 `Arc<Config>` 只读共享”的方案。该设计与单二进制 CLI/服务部署模式匹配，能够支持本地开发、容器化 env 注入和简单路径复用。

本文档的后续优化方案核心是：**把配置能力从 `common` 提升为一级 `src/config/` 基础设施模块**，并在此基础上**严格分阶段**交付以下能力：

1. 零行为变更的结构拆分 + re-export shim（阶段 0a）。
2. 错误模型集中化 + 日志/错误脱敏 + 保守校验（阶段 0b）。
3. 调用方路径迁移 + shim 移除（阶段 1）。
4. CLI 两阶段加载模型（LoadMode） + `config init/validate/secret ref` 等不依赖 vault 的运维命令（阶段 2）。
5. 最小 DB/Vault bootstrap + `core_key.json` fail-closed + root token 彻底脱敏 + 权限收紧（阶段 3）。
6. `config secret set/check` + `validate --resolve-secrets`（阶段 4）。
7. SecretRef 基础设施 + **必须先补齐一个真实的后置消费字段（MailConfig + 接入 email 模块等）**，才能进行第一次真实凭据迁移（阶段 5）。
8. 基础样例与测试配置分层、Profile 机制、CI 配置校验矩阵（阶段 6）。
9. 对象存储等早期依赖的初始化顺序重构（可选，阶段 7）。
10. 受控热加载（独立阶段 8）。

**五条不可违反的硬约束**（任何实现偏离都必须重新评审）：
- `Config(synchronous, no DB) → Storage(需要 DB + 构造 object storage) → VaultCore` 的循环，决定了**引导配置和当前早期依赖永远不能成为本项目 vault 的 SecretRef**。
- 当前 Redis 和 object storage 均在 vault 就绪前被初始化，**除非先重构 Storage 初始化顺序，否则它们也不能直接迁移**。
- `config secret set/check` 和 `validate --resolve-secrets` **只能使用最小 DB/Vault bootstrap**，绝不能依赖完整 `AppContext`（Redis、S3、HTTP 服务等）。
- 在 `core_key.json` 仍明文存储 root material、初始化仍会 `println!` root token、缺失仍会 `delete_all()` 的状态下，**SecretRef 迁移的实际安全收益严格限定为“不进普通配置文件、不进 git、不进常规日志”**；生产使用前必须先完成加固与 DR。
- 拆分、CLI 改造、secret 基础设施、测试分层、对象存储重构、热加载**必须分阶段**，每个阶段都要能独立编译、通过三项 gate（fmt + clippy -D warnings + `source .env.test && cargo test --all`），并更新 README、样例配置和本文档。

实施本计划前，建议先跑一遍“实施前检查清单”（见下节），确认对当前代码中的危险点（占位符 panic、Buck 校验 panic、vault 泄露、孤立 email 模块、死配置段、硬编码凭据等）已有清晰的对应阶段承接。

### 实施前快速检查清单（建议每个阶段开始前核对）

- [ ] 已完整阅读“事实校准” + “当前实现状态速览表” + “硬约束”三部分。
- [ ] 已确认本次阶段**不会**尝试把数据库/Redis/当前 object storage 凭据做成 SecretRef。
- [ ] 已确认本次阶段**不会**在 `Config::new` 内部调用 vault 或做异步 secret 解析。
- [ ] 对于任何涉及 `mail`/`MailConfig`/`email` 的工作，已明确“先补齐真实消费者，再谈 SecretRef”。
- [ ] 已知晓 `core_key.json` / vault 初始化路径当前的 `println!` + `assert!` + `delete_all` 行为，并在阶段 3 计划中安排清理。
- [ ] 已计划在 `validate.rs` 中加入“未知/未消费顶层段告警”（至少覆盖当前 `[oauth]` 和 `[mail]`）。
- [ ] 已计划同步更新 `config/config.toml` 注释、README 加载优先级说明、以及本文档。
- [ ] 变更影响的调用方（约 12 个文件引用 config）已在路径迁移阶段列入 checklist。
- [ ] 计划在 CI 中增加配置样例（基础 + 生成 + profile + 坏输入）校验任务。
