# Config 实现方案分析

本文档记录 `monoengine` 中 `Config` 的实现方案、加载链路、运行时注入方式、主要消费点与当前实现中的注意事项。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **集成测试指引**：本计划的各阶段功能应通过 **`integration.md`** 中定义的集成测试场景进行端到端验证。特别是配置初始化、加载、验证和 CLI 工作流应在 Docker 环境中完整测试，以确保与 Vault、邮件、通知等下游模块的集成无误。

> **事实校准（2026-06-18 复核）：** 本文档已按 `vault.md` 的 2026-06-17 落地状态和当前 `src/` 重新校准。与早期草案相比，多个原本作为前置的能力已经完成，后续执行必须以本节和“当前实现状态速览表”为准，不要按旧阶段重复实现。需特别注意以下事实：
> 1. **`Config` 已迁入顶层 `src/config/`，调用方已迁到 `crate::config`。** 阶段 1 的物理模块提升和阶段 3 的路径迁移已完成，`src/common/config.rs` shim 已移除；`model.rs` 已承接 `Config` 及各领域子配置结构体，`source.rs` 已承接 source 构建，`expand.rs` 已承接占位符展开函数；`error.rs` 已承接首批占位符诊断错误；`validate.rs` 已承接首批集中校验（database/log/lfs/build/redis/mail/Buck/object storage/orion_server）和 `config validate` 文件级、`MEGA_*` 环境变量覆盖项的未消费字段 warning。`Config` 当前含 `mail: Option<MailConfig>`，但仍不含 `oauth` 字段，也无 `OAuthConfig`；`[oauth]` 仍没有运行期消费者。
> 2. **mail 已是真实后置消费者，且已接入 `password_ref`。** `MailConfig` 已包含兼容期 `password: Option<SecretString>` 与推荐的 `password_ref: Option<SecretRef>`，两者互斥；明文 `password` 路径会输出 deprecation warning，`SecretString` 的 Debug/Serialize 输出脱敏，明文只在 `SmtpMailer::new` 适配层显式暴露。`AppContext::new` 在 `VaultCore::new` 之后解析 `mail.password_ref`，再通过 `SmtpMailer::new_with_password` 构造 mailer。SMTP 构造失败现在返回可诊断错误，不再由 `if let Ok(...)` 静默吞掉。
> 3. **CLI LoadMode 与首批 `config` 命令已落地。** `commands::LoadMode`、`CommandContext`、按子命令选择加载层级的 `cli::parse`、`config init`、`config secret ref/set/check`、`config validate --resolve-secrets` 均已实现。`config init` 走 `LoadMode::None`，只写安全配置骨架；`config validate` 走 `LoadMode::RawSources`，由 CLI 定位 base/profile 路径后在命令内解析配置，避免坏配置被子命令分发前预加载拦截；`config validate --deny-warnings` 可将 base/profile/env source diagnostics warning 升级为失败，便于 CI 严格校验；`config validate --show-sources` 可显式打印 base/profile/env 字段来源图和覆盖关系且不输出原始值；`config secret set/check` 走最小 DB/Vault bootstrap，不构造 Redis、对象存储、服务或完整 `AppContext`。`--profile` / `MEGA_PROFILE` 与 `src/config/testing.rs` 已完成首批；`src/config/reload.rs` 已提供 `ConfigHandle` 热加载核心、base/profile 文件轮询 watcher、`Storage`/`AppContext` 访问路径和订阅组件应用/回滚语义，`service` 命令启动时已接入 watcher 生命周期，并已注册日志 reload subscriber、邮件 dispatcher 关停 subscriber 与 artifact GC 调度 subscriber。仍未实现的是更完整 source-level 修复建议，以及 Buck cleanup、邮件重启类字段等其它热加载真实消费端订阅接入。
> 4. **SecretRef 与 resolver 已实现首批。** `src/config/secret.rs` 定义 `SecretRef`、`SecretResolver`、`VaultSecretResolver`，支持 `vault://secret/<name>#<field>`、缓存 TTL、`evict`/`evict_all`，并拒绝 `secret/secret/...` 等错误路径。
> 5. **Vault 生产化前置的核心子集已完成。** `VaultCore` 已 Result 化、key 缺失 fail-closed、不再因 `core_key.json` 缺失清空 vault 表；`core_key.json` 不再长期保存 root token，只保存 unseal shares 和限权 runtime tokens；root token / shares 不再输出到 stdout、stderr 或 tracing；key 目录和文件在 Unix 下收紧到 `0700` / `0600`；常规 secret 访问使用限权 token 并记录 `vault_audit` 事件。残余风险是：自动解封材料仍落在本地 key 文件中，磁盘读取攻击者仍可获得解封能力，仍需部署侧 KMS/secret manager 与备份恢复流程。
> 6. **当前真正未落地的 config 主线工作**：`src/config/` 继续收敛剩余加载错误模型、扩展 redaction/SecretString 覆盖、profile 来源诊断、测试配置分层的全仓迁移、CI 配置校验矩阵、Buck cleanup 和邮件重启类字段等其它热加载真实消费端订阅接入，以及可选的对象存储后置初始化重构。`config init`、Profile、基础样例凭据治理、raw TOML 未知字段 warning、`MEGA_*` 未消费/被忽略覆盖项 warning、source field/source override diagnostics、testing helper、reload 核心句柄、base/profile 文件 watcher、service watcher 生命周期、日志 reload subscriber、邮件 dispatcher 关停 subscriber、artifact GC 调度 subscriber、订阅组件应用/回滚和 `Storage`/`AppContext` 访问路径已有首批可执行入口，后续只需围绕模板治理、样例校验矩阵、更完整 source-level 修复建议与热加载消费端继续收敛。

> **本文档性质说明**：本文档同时承担“现状分析”和“改进设计方案”两种角色。早期章节中保留的架构解释仍有价值，但所有“未实现/前置/阶段”判断均以 2026-06-18 再基线为准。本文档的可执行入口已经从“先实现 Vault/LoadMode/SecretRef”切换为“在已完成这些能力的基础上，继续做 `src/config` 内部拆分、诊断/初始化/profile/测试分层/热加载”。

## 当前实现状态速览表（2026-06-18）

| 能力 / 组件                     | 实现状态     | 关键事实与风险 |
|--------------------------------|-------------|----------------|
| `Config` 结构体与领域子配置     | 已实现      | 当前对外入口为 `src/config/mod.rs`，模型定义已拆到 `src/config/model.rs`；全仓源码调用方已迁到 `crate::config`。强类型拆分合理；`#[serde(default)]` 已用于可选域（blame、buck、orion_server、sidebar、artifacts_gc）。 |
| TOML 文件 + `MEGA_*` env 叠加   | 已实现      | `config` crate + `__` 分隔符；`Config::new`、`load_str` 与 `load_sources` 已复用同一套 `MEGA_*` 环境变量 source builder，包含 `oauth.allowed_cors_origins`、`monorepo.admin`、`monorepo.root_dirs` 的列表解析。Profile 来源叠加已完成首批。 |
| 占位符 `${base_dir}` 展开       | 已实现并已诊断化首批 | `src/config/expand.rs::variable_placeholder_substitute` 做两次 `collect()` + `Rc<RefCell>` 遍历 + `envsubst`；原 **10** 处 `.unwrap()` 已改为返回 `ConfigError::Message`，错误包含占位符字段路径、可用来源 origin、脱敏说明和修复建议。 |
| 配置文件定位（4 级回退 + 自动生成） | 已实现    | `mega_base()/etc/config.toml` 与默认生成逻辑存在，README 已同步完整加载优先级；生成时会把 `base_dir` 渲染进去。 |
| 运行时共享 (`Arc<Config>`)      | 已实现并接入 reload handle 首批 | `AppContext` 保留初始 `Arc<Config>` 兼容字段，同时持有与 `Storage` 共享的 `ConfigHandle`；`Storage` 也持有 `ConfigHandle`，`storage.config()` 现在从 handle 获取当前快照，锁异常时回退初始 `Arc<Config>`，不再依赖 `Weak::upgrade().expect(...)`。仍需逐点确认跨 `await` 持有旧快照的语义，并补任务/邮件等其它热加载真实消费端订阅接入。 |
| `mail` / `MailConfig` / 邮件发送 | **已激活并接入 dispatcher 启动点** | 真实实现在一级模块 `src/mail/mod.rs`（`SmtpMailer`/`Mailer`/`NoopMailer` + 单测，依赖 `lettre`），经 `src/main.rs:17` 的 `mod mail;` 编译；`MailConfig` 在 `src/config/model.rs`、`Config.mail` 字段已存在、`[mail]` 段已被消费。`src/email/mod.rs` 为 re-export shim。`AppContext::new` 在 vault 之后解析 `mail.password_ref`（如存在）并构造 `SmtpMailer`，构造失败现在返回可诊断错误。详见 `mail.md`。 |
| `[oauth]` 段                   | **死配置，已在 validate 中告警**  | TOML 里有完整段（`config.toml:155`）+ env list key 注册（`src/config/source.rs`），但无强类型字段承接，也无运行期消费者。它是目前**唯一**被整段丢弃的孤立顶层段（`[mail]` 已被消费）。`config validate` 已对 `[oauth]`、`[mail].smtp_tls`/`[mail].tls`、raw TOML 中任意未知段/未知 key，以及 `MEGA_OAUTH__...`、`MEGA_MAIL__TLS`/`MEGA_MAIL__SMTP_TLS`、未知 `MEGA_*` 覆盖项输出 warning；常规服务加载仍不阻断。profile/env 来源图和覆盖关系已能显示，后续仍需补更完整 source-level 修复建议。 |
| `src/notification/`（邮件 outbox / dispatcher） | **已接入编译并在 mail 启用时启动 dispatcher** | `src/notification/{dispatcher,triggers,mod}.rs`（`EmailDispatcher`、触发器）+ `callisto::email_jobs` outbox 实体已从 mega 移植；`main.rs:18` 已声明 `mod notification;`。`AppContext::new` 在 vault 之后、`init_monorepo` 之前创建 `EmailDispatcher` 并 `tokio::spawn`；剩余工作是生命周期治理、退避/并发策略和业务触发器接入。 |
| Vault 管理的 secret            | 已扩展     | 现有直接消费者包括 `ssh_server_key`、PGP、Nostr、PKI 以及首批配置 SecretRef（`mail.password_ref` → `secret/config/...`）。 |
| `core_key.json` + 自动解封     | 已加固 | JSON 存储 unseal shares + 限权 runtime tokens，不再长期保存 `root_token`；缺 key fail-closed，不 `delete_all()`；token/root/shares 不输出到日志。 |
| Profile / `config.<profile>.toml` | **已实现首批** | 全局 `--profile <name>` 优先于 `MEGA_PROFILE`；profile 文件固定为基础配置同目录、同 stem 的 `.<profile>.toml`，例如 `config.toml` → `config.prod.toml`；基础配置后叠加 profile，再叠加 `MEGA_*` 环境变量。profile 名限制为 ASCII 字母/数字/`-`/`_`，指定但文件不存在会报错；`config validate` 会对 base/profile 文件分别输出 raw TOML warning，profile 类型冲突会报 profile 路径、字段路径、期望类型和脱敏建议；`config secret set/check` 仍走最小 DB/Vault bootstrap，但读取 profile 合并后的 DB 配置。完整 profile source diagnostics 仍待补。 |
| `monoengine config` 命令族      | **部分实现** | CLI 已支持按命令 `LoadMode` 加载；`config init`、`config secret ref/set/check` 与 `config validate --resolve-secrets` 已实现；`config validate` 已切到 `LoadMode::RawSources` 并在命令内解析配置，已输出 raw TOML 和 `MEGA_*` 环境变量未消费/未知字段 warning；文件级 warning 已保留可测试的 source path，base/profile/env warning 已能汇总，并可通过 `config validate --deny-warnings` 升级为失败；`config validate --show-sources` 已能展示 base/profile/env 字段来源图和覆盖关系，不输出原始值；坏 env/profile 类型会报出来源、字段路径和期望类型，并脱敏原始值；首批诊断已包含移除/替代字段等修复建议。更完整 source-level 修复建议仍待补。 |
| 集中配置校验                   | **部分实现** | `src/config/validate.rs` 已提供 `Config::validate()` 首批入口，覆盖 `database.db_type`/`database.db_url`、数据库连接池参数（`max_connection`、`min_connection`、`acquire_timeout`、`connect_timeout`）、`log.level`、`lfs` 路径/URL、`build.orion_server`、`redis.url`、`mail.password`/`mail.password_ref` 互斥、`mail.enabled` 必填项、Buck 限制、object storage local/S3/S3-compatible/GCS 后端必填项，以及可选 `orion_server` 的端口/URL/DB URL；`Storage::new` 的 Buck 校验和 BuckService 构造失败已改为返回 `MegaError`，不再 `panic!`；`config validate` 已对 `[oauth]`、`[mail].smtp_tls`/`[mail].tls`、raw TOML 任意未知字段和 `MEGA_*` 未消费/被忽略覆盖项输出带修复建议的 warning，且 env/profile 类型错误已脱敏并带修复建议；source warning 已可作为错误门禁，source field/source override 已可显式展示。更完整 source diagnostics 仍待补齐。 |
| SecretRef + 运行期 resolver    | **已实现首批**  | `SecretRef`、`SecretResolver`、`VaultSecretResolver` 已编码；支持 `vault://secret/...#field`、缓存 TTL、`evict`/`evict_all`，并实现 `mail.password` / `mail.password_ref` 互斥。 |
| 测试配置辅助                   | **已实现首批** | `src/config/testing.rs` 已提供 `TestConfigBuilder`、`isolated_config()` 与 `TestSecretResolver`，可派生临时 base/cache/LFS/object storage 路径、接收 `.env.test` 风格的 DB/Redis/mail SecretRef 覆盖，并用内存 resolver 覆盖 secret 读取/缺失/evict 场景；CLI `parse` 的无子命令加载单测已改用临时配置文件。尚未把全仓测试和 CI 配置矩阵迁移到该 helper。 |
| CI 配置样例校验                | **已实现首批** | `.github/workflows/config-validation.yml` 已新增 sibling-aware 配置验证入口，会 checkout `monoengine` 与 `orbit`，运行格式检查、配置模板/loader/profile/env diagnostics 单测、warning-as-error 单测、source field/source override 脱敏单测、坏 env/profile 类型脱敏单测、缺失 `mail.password_ref` 的命令层脱敏单测、仓库基础样例 `config validate`、`config init` 生成结果普通/`--deny-warnings` 校验、profile merge CLI 普通/`--show-sources` 校验，以及坏占位符、坏 SecretRef URI、坏 Redis URL scheme 的 CLI 失败 smoke；Vault resolver 也已有真实缺失 secret 脱敏单测。后续继续补更多坏输入、SecretRef 权限和完整 source diagnostics 场景。 |
| 受控热加载                     | **已实现核心基础 + 文件 watcher + service 生命周期 + 日志/邮件/artifact GC 订阅首批接入**  | `src/config/reload.rs` 已提供 `ConfigHandle`、`reload_from_path` 复用 base/profile 加载流水线构建候选配置、候选 `Config::validate()`、日志字段白名单热更新、`mail.enabled` 从 true 到 false 的运行期关停、已运行 artifact GC 任务的 interval/grace/batch 热更新和 `enable=true` 到 `false` 关停、数据库/Redis/邮件重启类字段需重启报告、SecretRef 变更不泄露不发布、失败保留旧快照，以及 `ConfigReloadSubscriber` 订阅应用/逆序回滚语义；`ConfigReloadWatcher` 已可轮询 base/profile 文件签名变化、触发 reload、拒绝坏配置并继续运行；`service` 命令已在服务生命周期内启动 watcher 并在子命令退出后停止，且已注册日志 reload subscriber 使 `log.level` / `log.print_std` / `log.with_ansi` 变更先应用到 tracing layer 再发布配置快照；`AppContext` 在邮件 dispatcher 已启动时会注册 dispatcher 关停 subscriber，使 `mail.enabled = false` 停止处理待发送队列；HTTP service 在 artifact GC 已启动时会注册调度 subscriber，使 `artifacts_gc.interval_secs` / `grace_secs` / `batch_limit` 和 `enable=false` 生效。`Storage`/`AppContext` 已持有共享 `ConfigHandle`，`storage.config()` 会读取 handle 当前快照。Buck cleanup、邮件重新启用和 SMTP 参数变更等其它真实消费端订阅仍待接入或继续要求重启。 |

**启动/加载关键路径上的已知危险点（各阶段必须收敛）**：
- `src/config/expand.rs::variable_placeholder_substitute` 已消除原 **10** 处 `unwrap`，并已对未解析/非法占位符值输出字段级脱敏错误和修复建议；仍需在后续完整 source diagnostics 中补更丰富的跨 source 覆盖关系。
- `Storage::new` 里的 Buck 校验和 BuckService 构造失败已从 `panic!` / `expect` 改为返回 `MegaError`；数据库连接和 migration 失败也已从 `database_connection` 的 `expect` 改为经 `Storage::new` / Vault 最小 bootstrap 返回错误；仍需在后续把更多启动期配置校验提前到 `Config::validate()` / source diagnostics。
- `Storage::config()` 已改为从 `Storage` 持有的共享 `ConfigHandle` 读取当前快照，锁异常时回退初始 `Arc<Config>`，不再存在 `Weak::upgrade().expect("Config has been dropped")` 的 panic 点；`AppContext` 也持有同一个 `ConfigHandle` 并提供 `config()` 快照方法。`ConfigReloadSubscriber` 已补首批订阅应用/失败回滚语义，`ConfigReloadWatcher` 已补 base/profile 文件轮询触发 reload 语义，`service` 命令已补 watcher 生命周期接线并注册日志 reload subscriber；邮件 dispatcher 已补 `mail.enabled = false` 的运行期关停 subscriber；HTTP artifact GC 任务已补调度参数热更新和运行期关停 subscriber。后续热加载仍需补 Buck cleanup、邮件重新启用/重配等其它真实消费端订阅接入。
- `AppContext::new` 当前已返回 `Result` 并传播 `Storage::new`（含数据库连接和 migration）、Redis 初始化、`VaultCore::new`、`init_monorepo` 与 mailer 初始化错误；HTTP 服务监听地址解析/绑定失败也已从 `unwrap` 改为经 `service http` / `service multi` 返回错误。剩余风险主要在 dispatcher 生命周期治理、失败退避和后台任务可观测性。
- `mail.password` 明文字段仍为兼容期入口；首批 deprecation warning 与 `SecretString` 防误打印已落地，后续仍需在 source diagnostics 和样例/模板中继续推动生产配置使用 `mail.password_ref`。
- Vault 初始化/解封的旧泄露路径已清理；当前残余风险是自动解封材料仍落在 `core_key.json`，需要备份恢复和部署侧凭据注入/KMS 策略配套。
- `mega_base()` / `mega_cache()`（`src/config/mod.rs`）的早期 `BaseDirs::new().unwrap()` / `to_str().unwrap()` 已移除；未设置 `MEGA_BASE_DIR` / `MEGA_CACHE_DIR` 且系统目录不可用时，会退回当前目录下 `.mega` / `.mega/cache`，后续完整 source diagnostics 可继续把 fallback 来源显式化。
- `DbConfig::default()`、`OrionServerConfig` 默认值和 `config/config.toml` 的首批可预测 PostgreSQL userinfo 已清理；真实数据库凭据仍必须通过 `MEGA_*`、文件挂载 secret 或部署平台 secret 注入，不能进入本项目 vault。

## 已落地的 mail/notification 子系统现状（2026-06-09）

本文早期版本把邮件能力整体当作后续才新建的未来工作。当前代码已不符这一描述，需单列现状，避免规划与实现脱节。**与本子系统相关的权威文档是同目录的 `mail.md`**（含 mail 模块自己的事实校准、速览表、硬约束与分阶段计划）；本节只做与 `Config` 相关的接口性概述，细节以 `mail.md` 为准。

**已编译且活跃：**
- 一级模块 `src/mail/mod.rs`（约 231 行）：`Mailer` trait（`:40`）、`NoopMailer`（`:50`）、`SmtpMailer`（`:65`，含长生命周期 `AsyncSmtpTransport`，`:68/:102`，`Credentials::new` 在 `:96`），由 `src/main.rs:17` 的 `mod mail;` 接入。
- `MailConfig`（`src/config/model.rs`，`#[serde(default)]` 经 `Config.mail` 字段进入统一加载管道）、`config/config.toml` 的 `[mail]` 段（`:269-276`）已被消费；`password_ref` 已作为 `Option<SecretRef>` 接入，并与 `password` 互斥。
- `src/context/mod.rs:46-55`：在 `VaultCore::new`（`:39`）之后从 `config.mail` 构造 `SmtpMailer`，如存在 `mail.password_ref` 则先通过 `VaultSecretResolver` 解析；构造失败会返回 `MegaError`，不再静默降级。
- `src/email/mod.rs`：12 行 re-export shim（历史路径兼容），应作为有主、有移除阶段的过渡 shim 跟踪。

**已接入但仍需加固：**
- `src/notification/{dispatcher,triggers,mod}.rs`：`EmailDispatcher`（`dispatcher.rs:18` 的 `run(self, shutdown)`，2 秒固定 `interval`、`fetch_pending_jobs(50)`、`:6` 导入 `crate::mail`）与事件触发器（`on_cl_comment_created` 等）已从 mega 移植，并经 `main.rs:18` 的 `mod notification;` 接入编译。
- `callisto::email_jobs` outbox 实体、`NotificationStorage`、对应 migration 均存在；`AppContext::new` 当前在 vault 之后、`init_monorepo` 之前启动 dispatcher。
- 仍需补齐：dispatcher 生命周期治理、退避/并发策略、以及触发器在业务路径中的完整调用。

**结论（影响后续执行基线）：** "先有真实的、晚于 vault 的消费者，再谈 SecretRef"这一原则的前置功能工作已落地，且 `mail.password_ref` 已成为首个真实 `SecretRef` 消费端。后续阶段不应再重复实现 resolver 或 mail 迁移；剩余工作是明文 `mail.password` 的兼容期 source diagnostics/样例治理、dispatcher 生命周期/退避/并发、以及业务触发器接入。

## 总体设计

`Config` 是系统的强类型配置中心，对外入口在 `src/config/mod.rs`，强类型模型定义在 `src/config/model.rs`。配置先由 `ConfigLoader` 定位并准备配置文件，再通过 `Config::new` 读取 TOML 文件、叠加环境变量、完成占位符替换，最终反序列化为一组按领域划分的配置结构。

启动后，配置会被包装进 `Arc<Config>` 并注入到 `AppContext` 中。下游服务、存储层和后台任务通过 `AppContext`、`Storage` 或直接传参消费配置，从而避免在业务代码中重复解析配置文件。

## 启动与加载链路

服务类命令的主链路如下：

```text
src/main.rs
  -> cli::parse(None)
  -> clap 解析全局参数和子命令
  -> commands::load_mode(cmd, args)        # 已按命令选择加载层级
  -> ConfigLoader::load
  -> Config::new                 # 同步函数，仅做文件读取 + env 叠加 + 占位符展开 + 反序列化
  -> commands::builtin_exec / service 子命令
  -> AppContext::new             # 这里才构造 Storage 与 VaultCore
  -> server / storage / task 等运行时组件
```

非服务命令已经使用 `LoadMode` 分层加载：

```text
config secret ref                 # LoadMode::None，不读取配置、不连接 DB/Vault
config secret set/check           # LoadMode::VaultBootstrap，只解析配置路径并走最小 DB/Vault bootstrap
config validate                   # LoadMode::RawSources，命令内解析 Config 并输出 source diagnostics
config validate --resolve-secrets # RawSources + 命令内解析 Config + 最小 DB/Vault bootstrap
service / chat-migrate            # LoadMode::FullAppContext
```

关键职责分布：

- `src/main.rs`：程序入口，调用 CLI 解析与执行。
- `src/cli.rs`：解析全局参数和子命令，按 `LoadMode` 决定是否加载配置、只解析配置路径或构造完整 `Config`，并触发子命令执行；不是所有子命令都会先调用 `Config::new`。
- `src/config/loader.rs`：负责定位配置文件，必要时生成默认配置。
- `src/config/template.rs`：提供默认配置模板。
- `src/config/mod.rs`：对外导出配置 API，并保留 `Config::new` / `load_str` / `load_sources` 等加载编排入口。
- `src/config/model.rs`：定义 `Config`、`VaultBootstrapConfig` 和各领域子配置结构体、默认值及现有模型方法。
- `src/config/source.rs`：实现 TOML source 与 `MEGA_` 环境变量叠加。
- `src/config/expand.rs`：实现 `${base_dir}` 等字符串占位符展开；首批失败路径已返回 `ConfigError`，未解析/非法占位符值已输出来源、字段路径和修复建议，后续 source diagnostics 仍需补跨 source 覆盖关系。
- `src/config/error.rs`：定义首批配置诊断错误（当前覆盖占位符展开）。
- `src/context/mod.rs`：构建运行时上下文，将 `Arc<Config>` 注入系统，并在此构造 `VaultCore`（`src/context/mod.rs:33`）。
- `src/jupiter/storage/mod.rs`：存储层保存对配置的弱引用，并向具体存储组件提供配置访问能力。

> 注意：`Config::new` 是**同步**函数（`src/config/mod.rs`），且只依赖文件系统读取，不访问数据库或网络。这一事实对后文“敏感数据 Vault 化”的可行性有决定性影响，见下文「现有 vault 能力与关键约束」。
>
> 当前 `variable_placeholder_substitute` 实现仍做两次完整 `config.collect()` + builder clone + `Rc<RefCell>` 嵌套遍历；原直接 `.unwrap()` panic 的占位符错误已改为返回 `ConfigError`，并已对未解析/非法占位符值输出脱敏诊断和修复建议。后续仍需把它纳入更完整的跨 source diagnostics。

## 配置文件定位优先级

`ConfigLoader` 会按以下顺序寻找配置文件：

1. 命令行参数 `--config` 指定的路径。
2. 环境变量 `MEGA_CONFIG` 指定的路径。
3. 当前工作目录下的 `config/config.toml`。
4. `mega_base()/etc/config.toml`。
5. 如果上述路径均不可用，则生成默认配置文件后再加载。

README 已同步描述完整加载优先级，包括 `mega_base()/etc/config.toml` 和自动生成默认配置的兜底逻辑。

## 配置合并与反序列化

`Config::new` 使用 `config` crate 作为底层解析能力，核心流程为：

1. 读取 TOML 配置文件。
2. 叠加以 `MEGA_` 为前缀的环境变量。
3. 将双下划线环境变量映射为嵌套字段，例如 `MEGA_LOG__LEVEL` 可覆盖 `log.level`。
4. 对部分列表类字段进行逗号分隔解析（如 `oauth.allowed_cors_origins`、`monorepo.admin`、`monorepo.root_dirs`）。其中 `oauth.allowed_cors_origins` 当前没有对应强类型字段，属于遗留的 env 解析规则，应在新增 `OAuthConfig` 或清理该遗留规则时重新确认。
5. 在反序列化前执行字符串占位符替换。
6. 将最终配置反序列化为强类型 `Config`。

占位符替换主要用于让配置值引用基础目录等公共路径，例如 `${base_dir}`。该机制工作在字符串值上，适合路径类配置复用，但不适用于非字符串字段。

> 实现痛点更新：占位符替换函数 `src/config/expand.rs::variable_placeholder_substitute` 原有密集 `.unwrap()`（`build()`、`substitute()`、`collect()`、`Rc::try_unwrap()`、`into_string()`、`set_override()` 等）已改为返回 `ConfigError::Message`，并新增单测覆盖坏占位符不 panic、错误值不回显。`config validate` 已补首批 `MEGA_*` 未消费/被忽略覆盖项 warning；剩余工作是把占位符错误接入更完整的跨 source diagnostics。

## 强类型配置结构

`Config`（`src/config/model.rs`）按功能域拆分为多个子配置，覆盖系统运行所需的主要能力。当前**实际存在**的字段如下：

- `base_dir`：基础目录，`${base_dir}` 占位符的来源。
- `log`：日志输出方式、级别、ANSI 颜色、文件滚动等（`LogConfig`）。
- `database`：SeaORM 数据库连接配置（`DbConfig`；数据库凭据属于引导配置，不能进入本项目 vault，见敏感数据章节）。
- `monorepo`：monorepo 根目录、导入目录和 Git 相关路径（`MonoConfig`）。
- `pack`：Git pack 解码和对象处理相关参数（`PackConfig`）。
- `lfs`：Git LFS 存储与传输相关配置（`LFSConfig`）。
- `blame`：blame 计算相关参数（`BlameConfig`）。
- `build`：构建触发、外部 Orion 构建服务等配置（`BuildConfig`）。
- `redis`：缓存、队列、连接管理相关配置（`RedisConfig`）。
- `buck`：Buck 文件上传、限流、清理等配置，`Option<BuckConfig>`（已存在局部 `BuckConfig::validate()`）。
- `object_storage`：本地文件系统、S3/S3 兼容服务、GCS 等对象存储后端配置（`ObjectStorageConfig` 由 sibling `orbit` crate 定义，`src/config/mod.rs` 重新导出）。
- `orion_server`：外部构建服务地址等配置，`Option<OrionServerConfig>`。
- `sidebar`：侧边栏默认种子数据相关配置（`SidebarConfig`）。
- `artifacts_gc`：构建产物垃圾回收相关配置（`ArtifactGcConfig`）。
- `mail`：SMTP 邮件通知配置，`Option<MailConfig>`。当前同时支持兼容期明文字段 `mail.password: Option<SecretString>` 与推荐字段 `mail.password_ref: Option<SecretRef>`，两者互斥；`password_ref` 是首个已落地的配置侧 SecretRef 消费点。

> **字段现状（mail 已存在；oauth 仍缺失）：**
> - **`mail` / `MailConfig`：已是 `Config` 字段且已定义。** `MailConfig` 在 `src/config/model.rs`（扁平结构 `enabled`/`smtp_host`/`smtp_port`/`username`/`password`/`password_ref`/`from`/`starttls`，端口/STARTTLS 默认值经 `default_smtp_port`/`default_starttls` 提供）。`config/config.toml` 的 `[mail]` 段（`:269-276`）**已被消费**；段内 `smtp_tls`/`tls` 属未知 key，`config validate` 已对 TOML 和 `MEGA_MAIL__TLS`/`MEGA_MAIL__SMTP_TLS` 输出首批 warning。真实 `SmtpMailer::new_with_password(...)` 经 `AppContext::new` 在 Vault 就绪后调用，模块经 `main.rs:17` 的 `mod mail;` 编译（**不是** `mod email;`；`src/email/mod.rs` 仅为 re-export shim）。`mail.password_ref` 已落地并通过 `VaultSecretResolver` 解析；明文 `mail.password` 已完成首批兼容期治理。
> - **`oauth` / `OAuthConfig`：当前仍不是 `Config` 字段。** 仅在 env list-parse 中出现 `oauth.allowed_cors_origins`（`src/config/source.rs`），但由于没有对应字段，该 list key 当前也不映射到任何结构。`config/config.toml` 里的完整 `[oauth]` 段（`:155`，含 `campsite_api_domain`、`tinyship_api_domain`、`api_store_backend`、`allowed_cors_origins`）没有运行期消费者；`config validate` 已对该段输出 warning。涉及 OAuth 回调地址的校验、`[oauth.github]` 示例等，都必须在真实新增 `OAuthConfig` 之后再落地。

这种拆分方式让上层调用方可以只依赖自己需要的配置域。但需要注意：**`config/config.toml` 中存在未被任何强类型字段消费的内容：整段孤立的 `[oauth]`（当前唯一的整段孤立顶层段），以及已消费段内的未知 key（如 `[mail]` 中的 `smtp_tls`/`tls`，`config.toml:275-276`）；用户自行增加的未知段或未知 `MEGA_*` 覆盖项同样会被 serde/config crate 忽略，配置来源与 Rust 结构之间并非严格一一对应。** 这既是当前的技术债务，也是安全/运维隐患（用户以为写了某段就生效了）。`config validate` 已基于 raw TOML 对 `[oauth]`、`[mail].smtp_tls`/`[mail].tls` 和任意未知字段输出 warning，并对 `MEGA_OAUTH__...`、`MEGA_MAIL__TLS`/`MEGA_MAIL__SMTP_TLS` 与未知 `MEGA_*` 覆盖项输出不含值的 warning；后续仍需扩展 profile 合并来源、脱敏原始值和修复建议，并考虑在模型上使用 `#[serde(deny_unknown_fields)]` 的白名单模式或后置 key 检查——注意 `MailConfig` 当前**未**使用 `deny_unknown_fields`，并保留“丢弃未知字段”的兼容注释，与该建议存在张力，需在落地时统一策略。见「推荐加载流水线」。

`Config` 已提供若干测试/构造入口：`Config::mock()`、`Config::load_str()`、`Config::load_sources()`（`src/config/mod.rs`）。后续测试辅助应在这些既有入口之上扩展，而不是另起一套。

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
- HTTP 服务：读取监听地址、端口、CORS、Swagger/OpenAPI 等相关配置；监听地址解析/绑定失败会返回 `MegaError`，不再 `unwrap` 崩溃（OAuth 相关字段目前尚未进入 `Config` 强类型结构，见上节说明）。
- Git pack / LFS：控制对象解码、上传、存储和传输行为。
- 构建系统：配置 Orion 构建服务、触发器和构建产物管理。
- Buck 上传：读取上传限制、清理策略和相关后台任务参数。
- 邮件通知（已编译，dispatcher 已在 mail 启用时启动）：一级模块 `src/mail/mod.rs` 提供 `SmtpMailer`/`NoopMailer`，`MailConfig` 已定义并接入 `Config`；`src/context/mod.rs` 已在 vault 之后解析 `mail.password_ref`，再构造 `SmtpMailer` 并 spawn `EmailDispatcher`。作为首个配置侧 SecretRef 消费点，基础迁移已完成；明文 `mail.password` 的 deprecation warning 与 `SecretString` 包装已落地，剩余工作是 source diagnostics、样例治理，以及 dispatcher 生命周期/退避/并发（见「已落地的 mail/notification 子系统现状」）。
- Artifact GC：控制构建产物垃圾回收策略。
- Sidebar 默认数据：为 UI 侧边栏提供初始化种子配置。

### 当前消费点的关键依赖顺序（与 secret 迁移相关）

在评估哪些字段可以改为 `SecretRef` 时，不能只看配置结构，而必须精确追踪消费点在初始化链路中的位置：

1. **`Storage::new`（`src/jupiter/storage/mod.rs:190`，签名 `async fn new(config: Arc<Config>)`）** 在 `AppContext::new` 中第 33–35 行被调用，它内部：
   - 第 191 行 `database_connection(&config.database)` 建立数据库连接并执行 migration；连接或 migration 失败会返回 `MegaError`，不再 panic；
   - 第 206 行通过 `crate::jupiter::storage::object_storage::ObjectStorageFactory::build(&config.object_storage)` 构造对象存储（S3/GCS/Local），因此 `object_storage.s3.access_key_id` / `secret_access_key` 在 vault 就绪前已被消费；
   - Buck 配置校验已复用 `src/config/validate.rs`，BuckService 构造失败也会返回 `MegaError`，不再 `panic!` / `expect`；后续还应继续把其它启动期配置错误提前到 `Config::validate()` / source diagnostics。
   - `Storage` 直接持有 `Arc<Config>`，`Storage::config()` 返回快照克隆；这已消除原 `Weak::upgrade().expect("Config has been dropped")` panic 点。热加载切换为可替换快照句柄时，仍需逐点确认“取出 Arc 后跨 await 持有”的旧快照语义。
2. **`init_connection(&config.redis)`（`src/context/mod.rs:36`）** 在 `VaultCore::new` 之前执行，因此带密码的 `redis.url` 也属于早期运行时依赖；Redis URL 格式和连接失败会返回脱敏错误，不再 panic。
3. **`VaultCore::new(storage)`（`src/context/mod.rs:39`）** 之后才就绪，此后消费的配置字段才可纳入可迁移凭据。
4. **SMTP mailer 与 EmailDispatcher 已是 vault 之后的后置消费点，并已接入 SecretRef。** `src/context/mod.rs` 在 `VaultCore::new` 之后检查 `MailConfig` 的 `password` / `password_ref` 互斥关系；若配置了 `password_ref`，通过 `VaultSecretResolver` 解析后再调用 `SmtpMailer::new_with_password(...)`。SMTP 初始化失败会返回 `MegaError`，不再静默忽略。**因此 `mail.password_ref` 是当前已落地的第一个配置侧 SecretRef 消费点**；后续不应重复做 resolver/mail 接入，只需治理明文兼容路径和 dispatcher 生命周期。
5. **`ssh_server_key`、PGP、Nostr** 已由 vault 管理，但属于 vault 内部 secret，不在 `Config` 结构体中。SSH server key 读取/生成/写入失败、PGP key 读取/解析/保存/删除、Nostr key 读取/生成/解析已改为返回可诊断错误；它们不改变配置侧 `SecretRef` 的边界。

> 结论：任何在 `Storage::new` 或 `init_connection` 阶段消费的字段都不能直接改为 `SecretRef`，除非先重构初始化顺序。
>
> 额外观察（来自当前代码）：
> - `Storage::new` 内部在构造 buck 相关信号量**之前**仍会执行 Buck 配置校验，但该路径已复用 `validate_buck_config` 并返回 `MegaError`；后续 `BuckService::new` 的配置解析/一致性错误也会继续向上传播，不再 panic。后续更值得继续收敛的是其它启动期依赖字段的 source diagnostics，而不是重复移动 Buck 校验。
> - `crate::jupiter::storage::object_storage::ObjectStorageFactory::build` 在同一阶段被调用，因此 `object_storage.s3.access_key_id` / `secret_access_key`（即使当前为空字符串）在 vault 就绪前就已被“消费路径”触达。
> - `context/mod.rs:36` 的 `init_connection(&config.redis)` 紧随 Storage 之后，仍在 `VaultCore::new` 之前；失败会经 `AppContext::new` 返回错误。

## 当前方案的优点

- 启动链路清晰：配置在 CLI 入口统一加载，然后传入后续运行时组件。
- 类型安全：业务代码面对的是 Rust 结构体，而不是散落的字符串键。
- 配置来源灵活：支持命令行、环境变量、项目默认配置和自动生成模板。
- 运行时共享成本低：通过 `Arc<Config>` 只读共享，避免重复解析和复制。
- 领域边界清楚：配置按 `log`、`database`、`redis`、`object_storage`、`buck` 等现有领域拆分，便于维护。
- 适合容器化部署：`MEGA_` 环境变量覆盖机制便于在部署环境中注入差异配置。

## 现有 vault 能力与关键约束

后文的“敏感数据 Vault 化”方案高度依赖现有 `vault` 模块的真实形态，因此先澄清现状，避免规划脱离实现：

- **vault 已是可用的通用 secret store。** `src/contract/vault/integration/vault_core.rs` 基于 `src/vault` 中 vendored 的 RustyVault 模块，已提供 `read_secret`/`write_secret`/`delete_secret`（按 `secret/<name>` 路径存取任意 KV）。因此“把凭据写入 vault”所需的 API 已经存在，不必新建存储能力。
- **vault 的存储后端就是数据库。** `VaultCore::new(ctx: Storage)` 通过 `JupiterBackend` 把 secret 存进数据库（`ctx.vault_storage()`）。这意味着 vault 必须先有可用的数据库连接才能启动。
- **由此形成明确的依赖链：`Config → Storage(数据库) → Vault`。** `Config::new`（同步）先产出配置 → `Storage::new(Arc<Config>)` 用 `database` 连库 → `VaultCore::new(storage)` 起 vault。vault 在 `AppContext::new` 阶段才就绪，晚于 `Config::new`。
- **最小 DB/Vault bootstrap 已可用。** `VaultCore::from_database_config/from_database_connection` 与 `Config::load_vault_bootstrap` 已支持只依赖数据库配置启动 vault；数据库连接或 migration 失败会映射为可诊断的 vault bootstrap 错误；`config secret set/check` 和 `config validate --resolve-secrets` 不依赖 Redis、对象存储、HTTP/SSH 服务、monorepo 初始化或完整 `AppContext`。
- **配置侧 SecretRef 已有首批落点。** 现有直接消费者包括 `ssh_server_key`、PGP、Nostr、PKI，以及首批配置 SecretRef：`mail.password_ref` → `secret/config/...`。`database`、`redis`、`object_storage.*` 仍属于引导/早期依赖，不能使用本项目 vault 的 SecretRef。
- **`core_key.json` 已从“危险自动重建”改为 fail-closed。** 当 DB 中 vault storage 已初始化但 key 文件缺失时，`VaultCore` 返回 `CoreKeyMissing`，不会 `delete_all()` 清空 vault 表；初始化路径不再输出 root token、secret shares 或完整 key 文件内容。Unix 下 key 目录和文件权限已收紧到 `0700` / `0600`。
- **root token 已退出常规运行路径。** 初始化后会安装运行时 ACL policy、签发 ssh/pgp/nostr/pki/config/generic 限权 token，随后写回不含 `root_token` 的 `core_key.json` 并撤销 root token；常规 secret 读写使用限权 token，并在 `VaultCoreInterface` 入口记录 `vault_audit` 元数据。
- **残余安全边界仍需明确。** 自动解封模式下，unseal shares 仍落在本地 `core_key.json`。这降低了“进入普通配置/日志/仓库”的风险，但不抵御能读取部署机 key 文件的攻击者；生产部署仍需要外部 secret manager/KMS、受控挂载、备份恢复和恢复演练。

这些安全/引导事实直接约束了下文方案的边界：配置模块可以继续基于已完成的 vault/bootstrap/SecretRef 能力推进，但仍不得把数据库、Redis、当前对象存储凭据改成本项目 vault SecretRef。

## Config 单独成模块的改进方案

当前 `Config` 的实现已经集中在顶层 `src/config/`：`mod.rs` 保留对外导出与加载编排，`model.rs` 承接 `Config` 及领域子配置，`source.rs` 承接 source 构建，`expand.rs` 承接占位符展开，`loader.rs`、`template.rs`、`secret.rs`、`validate.rs`、`error.rs`、`testing.rs` 和 `reload.rs` 也已位于同一目录下；源码调用方已经迁到 `crate::config`，`common::config` 兼容 shim 已删除。后续主要问题不再是“是否有顶层模块/新旧路径并存”，而是错误模型、集中校验、初始化、测试配置和热加载等职责仍需继续收敛到更完整的诊断与运行时接入语义。建议继续让配置能力从文件级独立推进到职责级独立。

这一改造不只是文件拆分，也应承接当前实现中的注意事项：占位符替换规则需要显式化，配置校验需要集中化，启动期错误需要可诊断化，文档与实际加载优先级需要同步，部分敏感数据可逐步从普通配置文件中剥离并交由 `vault` 模块存储，并在独立配置模块中落地受控热加载能力。

### 总体改造原则

考虑到配置模块规模以及上文揭示的 vault 引导/安全约束，本计划采用**分阶段、可独立验证**的策略，而不是一次性大爆炸切换。以下原则贯穿全程：

1. **拆分与迁移解耦。** 顶层模块迁移和调用方路径迁移已分别完成；后续内部拆分仍应保持同样原则：一次只移动一个职责边界，确保每一步可编译、可回归，不把错误模型、`config init`、Profile 或热加载混进纯移动变更。
2. **区分“引导配置 / 早期运行时依赖”与“可迁移凭据”。** 数据库连接（以及 vault 自身启动所依赖的一切）属于**引导配置**，必须留在 TOML/环境变量中（明文或由 env 注入），**永远不能成为 vault SecretRef**——因为 vault 存在数据库里，连库才能起 vault。Redis URL、当前 `Storage::new` 阶段构造的对象存储凭据，也属于 vault 就绪前或同时期会被消费的**早期运行时依赖**；除非先重构初始化顺序，否则也不能直接改成 SecretRef。只有在 vault 就绪后才被使用的凭据（例如当前邮件发送器密码，以及未来新增且确认为后置消费的 OAuth/第三方服务 secret）才是“可迁移凭据”。
3. **secret 解析是 vault 就绪后的独立异步阶段，不在 `Config::new` 内。** `Config::new` 同步且 vault 尚未就绪，无法在加载流水线内解析 secret。`Config::new` 只产出**未解析的 `SecretRef`**；服务运行时的真实值由 resolver 在 `AppContext` 中的 vault 就绪后、按消费端依赖顺序异步解析。
4. **`config secret` 命令必须继续只使用最小 DB/Vault bootstrap。** 当前 `config secret set/check` 已按此原则实现：只建立 vault 所需的数据库能力和 `VaultStorage`，不初始化 Redis、对象存储、HTTP/SSH 服务或后台任务。后续新增 secret 命令或 `config init` 不能退回完整 `AppContext`。
5. **Vault 加固核心子集已完成，但生产部署仍需恢复与托管策略。** fail-closed、权限收紧、root token 脱敏/退役和限权 token 已落地；后续 config 工作可以基于这些能力继续推进。剩余安全边界是本地自动解封 key material 的托管、备份恢复、KMS/secret manager 接入和演练。
6. **CLI 两阶段加载模型已落地，后续需补齐 source diagnostics。** 当前 `cli::parse` 已先解析子命令，再按 `LoadMode` 选择 `None`、`VaultBootstrap`、`ParsedConfig` 或 `FullAppContext`。`config init` 已按 `LoadMode::None` 实现，可在无配置/坏配置场景下生成安全骨架；`RawSources`/source diagnostics 仍需形成完整实现。

### 目标

- 将配置相关职责从单文件拆分为多个小模块，降低 `src/config/mod.rs` 的复杂度。
- 对外 API 已收敛到顶层 `crate::config::Config`、`crate::config::loader::ConfigLoader` 等路径；后续新增调用方应直接使用 `crate::config`，不得重新引入 `common::config` 兼容入口。
- 为集中校验、错误诊断、占位符规则、环境变量规则测试提供完整实现；受控热加载作为**独立后续阶段**落地，不与拆分/迁移捆绑。
- 将**可迁移凭据**与普通配置分离：这些字段在配置文件中只保存 `vault` 引用（`SecretRef`），真实值存储在 `vault` 模块中。**数据库凭据等引导配置不在此列**，继续随启动配置提供。
- 分阶段完成消费端改造：先完成路径迁移，再补齐错误模型、source diagnostics、Profile 与测试分层；已完成的 CLI LoadMode、SecretRef、`config secret` 和 `config init` 首批能力不应在后续阶段重复实现。
- 明确 `config/config.toml` 的定位：它不应继续作为“全项目测试时顺手使用的运行配置”，而应演进为可提交、可校验、无真实 secret、适合本地开发和 CI 参考的基础样例配置；自动化测试应使用独立的测试配置生成与覆盖机制。
- 在已落地的 `monoengine config init`、`config secret ref/set/check` 与 `config validate --resolve-secrets` 基础上，补齐配置 source diagnostics、Profile 和样例校验；数据库密码、Redis URL、当前阶段的对象存储 key 等引导或早期依赖不通过本项目 vault 写入。
- 将 README、`config/config.toml`、默认模板和配置实现保持同步，避免用户看到的加载优先级与实际行为不一致。

### 建议目录结构

现有顶层 `src/config/` 仍需继续按职责拆分，目标结构如下：

```text
src/config/
├── mod.rs          # 对外入口与 re-export，保留 Config::new 等主 API
├── model.rs        # Config 及各领域子配置结构体
├── loader.rs       # 配置文件定位、默认配置生成，沿用现有 ConfigLoader
├── template.rs     # 默认 TOML 模板，沿用现有模板能力
├── source.rs       # 已落地：文件源、环境变量源
├── expand.rs       # 已落地：${base_dir} 等占位符展开（首批错误模型已收敛）
├── secret.rs       # SecretRef 引用类型、Vault resolver 适配和脱敏输出
├── bootstrap.rs    # VaultBootstrapConfig、load_vault_bootstrap 等最小启动配置
├── init.rs         # 基础配置初始化、SecretRef 占位生成和初始化计划输出
├── testing.rs      # 测试配置模板、覆盖规则和自动化测试辅助能力
├── validate.rs     # 集中式 Config::validate() 与领域校验
├── reload.rs       # 受控热加载核心句柄、变更检测、字段白名单和回滚策略
└── error.rs        # 已落地首批：配置专用错误类型与字段路径诊断信息
```

其中 `mod.rs` 只承担编排和导出职责，例如对外暴露 `Config`、`ConfigLoader`、`ConfigError`，并隐藏内部的 `source`、`expand` 等实现细节。业务代码应改为通过 `crate::config::Config` 获取类型，使 `config` 成为与 `common`、`commands`、`context` 等并列的一级模块。

消费端路径迁移已经完成，后续新增代码应直接引用 `crate::config::*`。接下来再分别补错误模型、source diagnostics、Profile 与测试分层，并继续治理 `config init` 生成模板和样例校验。CLI 加载模型、SecretRef/resolver、`config init` 和 mail 消费端已是基线，拆分时应迁移既有实现而不是重写。任何新增 `SecretRef` 消费端都必须先通过依赖表确认其初始化晚于 vault。

### 模块职责划分

- `model.rs`：只定义强类型配置模型和必要的默认值，不放文件读取、环境变量解析和运行时初始化逻辑。
- `loader.rs`：继续负责配置文件定位优先级，包括 `--config`、`MEGA_CONFIG`、项目默认路径、`mega_base()/etc/config.toml` 和默认文件生成。
- `source.rs`：封装 `config` crate 的 source 构建过程，明确 TOML 文件、`MEGA_` 环境变量、嵌套分隔符和列表字段解析规则。
- `expand.rs`：集中处理占位符展开，定义支持哪些占位符、展开顺序、未知占位符的处理策略，以及是否允许递归展开；同时明确占位符只作用于字符串值，新增占位符前必须确认字段类型和替换顺序。`variable_placeholder_substitute` 的原 `.unwrap()` panic 已收敛为 `ConfigError`，未解析/非法占位符值已输出字段级脱敏诊断；后续仍需补完整跨 source 关系。
- `secret.rs`：承接现有 `SecretRef` 引用类型与 `SecretResolver`/`VaultSecretResolver`，适配 `VaultCoreInterface`（`read_secret`/`write_secret`），并统一提供脱敏日志、错误信息和审计字段。服务运行时 resolver 接收一个已就绪的 vault 句柄（即 `AppContext` 中的 `VaultCore`），**不在 `Config::new` 阶段调用**；`config secret set/check` 已通过最小 DB/Vault bootstrap 获取 vault 句柄，拆分时必须保持这个边界。
- `init.rs`：提供基础配置初始化能力，负责生成 `config/config.toml` 样例、派生本地目录、填充非敏感默认值、为可迁移凭据生成 `SecretRef` 占位引用，并输出后续需要执行的 `config secret set` 命令清单；真实 secret 不在该阶段写入配置文件。
- `testing.rs`：首批已提供面向自动化测试的 `TestConfigBuilder`、`isolated_config()` 和 `TestSecretResolver`，可派生隔离的 base/cache/LFS/object storage 路径，接收 `.env.test` 风格的 DB/Redis/mail SecretRef 覆盖，并用内存 resolver 覆盖 secret 读取/缺失/evict 场景。后续应继续把文件加载链路、Profile 合并和 CI 配置矩阵迁移到该 helper，避免测试直接复用或修改仓库中的基础配置文件。
- `validate.rs`：提供集中校验入口，按领域拆分校验函数，例如数据库连接串、连接池参数、监听端口、对象存储后端、路径可用性等，避免非法配置延迟到后续初始化阶段才暴露。首批 hard error 已覆盖**当前已存在字段**的无争议规则，例如 `database.db_type` 必须为 `postgres`、`database.db_url` scheme 必须为 `postgres`/`postgresql`、`database.max_connection > 0`、`database.min_connection <= database.max_connection`、数据库 acquire/connect timeout 非 0、`log.level` 枚举、`lfs` 路径/URL、`build.orion_server`、`redis.url` scheme、Buck 并发/大小限制（吸收现有 `BuckConfig::validate()`）、object storage local/S3/S3-compatible/GCS 后端必填项、可选 `orion_server` 端口/URL/DB URL，以及 mail 的 `mail.enabled = true` 时 `smtp_host`/`from` 必填、`password`/`password_ref` 互斥；首批 raw TOML warning 已覆盖 `[oauth]`、`[mail].smtp_tls`/`[mail].tls` 和任意未知字段。剩余 diagnostics 重点是环境变量/profile 来源、脱敏原始值和修复建议。但涉及尚不存在字段的校验（OAuth 回调地址等）仍应等 `OAuthConfig` 真实落地后再写。
- `reload.rs`：首批已提供 `ConfigHandle`，负责维护 `Arc<Config>` 快照，`reload_from_path` 复用 base/profile 加载流水线构建候选配置，对候选配置复用 `Config::validate()`，按白名单应用日志字段变更、邮件 dispatcher 关停变更和已运行 artifact GC 调度变更，报告数据库、Redis、artifact GC 从关闭到启用以及邮件重启类字段需重启，并在校验失败时保留旧配置；SecretRef 变更已覆盖不泄露、不发布旧快照语义；`ConfigReloadSubscriber` 已提供订阅组件应用和失败逆序回滚语义；`ConfigReloadWatcher` 已提供 base/profile 文件签名轮询、变化后触发 reload、失败记录并继续运行的执行层；`service` 命令已接入 watcher 生命周期并注册日志 reload subscriber，`AppContext` 已在邮件 dispatcher 启动时注册关停 subscriber，HTTP service 已在 artifact GC 启动时注册调度 subscriber。后续继续补 Buck cleanup、邮件重新启用/重配等其它真实消费端订阅接入。
- `error.rs`：提供配置专用错误，包含字段路径、失败原因，并逐步补配置文件路径、原始值脱敏和修复建议，再统一转换为现有 `ConfigError` / `MegaError`；配置加载路径上的剩余 `unwrap`、`expect` 和 `panic` 应继续收敛到该错误模型中。

### 多环境 Profile 与继承机制

为了支持开发（dev）、测试（test）、预发（staging）和生产（prod）等不同环境下的配置管理，配置加载器应原生支持 **Profile** 机制。首批实现已覆盖加载路径和合并语义：
1. **启动参数与环境变量识别**：通过命令行参数 `--profile <name>` 或环境变量 `MEGA_PROFILE=<name>` 传入当前 Profile（默认为空，即不使用 Profile 继承）。
2. **多文件合并加载与覆盖优先级**：
   - 首先加载基础配置文件 `config/config.toml`。
   - 如果指定了 Profile（例如 `prod`），加载器会尝试寻找并读取 `config/config.prod.toml`（或通过 `--config` 指定路径的同级目录下的 `config.<profile>.toml`）。
   - 将 Profile 特定的配置以“深合并（Deep Merge）”的形式叠加到基础配置之上。
   - 最终叠加 `MEGA_` 前缀的环境变量。
3. **环境隔离规范**：Profile 文件只应声明与基础配置不同的增量部分（如日志级别、外部依赖的端点或缓存开关），避免配置冗余。所有环境的配置均需通过 `validate.rs` 进行语义校验。

Profile 机制需要先固定以下语义，避免“配置能合并但含义不确定”：

- `--profile <name>` 的优先级高于 `MEGA_PROFILE=<name>`；未指定时不加载 profile 文件；profile 名当前限制为 ASCII 字母/数字/`-`/`_`，避免路径注入。
- 当 `--config /path/app.toml --profile prod` 时，profile 文件固定为同目录下 `/path/app.prod.toml`；当使用默认 `config/config.toml` 时，对应 `config/config.prod.toml`；指定 profile 但文件不存在会 fail fast。
- 深合并中标量和 table 按字段覆盖，数组默认**整体覆盖**而不是追加，避免 CORS、admin、root_dirs 等列表在不同环境中意外叠加；单测已覆盖 profile 覆盖 base、`MEGA_*` 覆盖 profile。
- Profile 文件不能隐式删除基础配置字段；需要表达“关闭”时应使用显式布尔开关或空数组。
- SecretRef 路径应包含 profile 或部署命名空间，例如 `vault://secret/config/prod/mail/password#value`，避免 dev/staging/prod 误读同一份 secret。

剩余工作是把 profile 的来源路径、字段来源、合并冲突和修复建议纳入完整 source diagnostics，并把 profile 合并结果纳入 CI 配置矩阵。

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

1. **引导配置（必须留在 TOML/env，永不进本项目 vault）。** 典型是 `database`（连接地址、用户名、密码）。当前 `DbConfig::default()` 与仓库基础样例 `config/config.toml` 已避免在默认 PostgreSQL URL 中嵌入可预测 userinfo，但这不改变边界：由于 vault 存在数据库里、连库才能起 vault，**数据库密码无法作为 vault SecretRef**——这是不可破的引导循环。这类凭据应通过环境变量注入（如 `MEGA_DATABASE__DB_URL` 或拆分后的数据库密码环境变量）、文件挂载 secret 或部署平台的 secret 机制（K8s Secret、CI secret store 等）保护，**而不是交给本项目的 vault**。
2. **早期运行时依赖（当前也不能直接进 vault）。** 这类字段不是数据库引导项，但在 vault 就绪前或同一初始化阶段已经被消费。当前 `Storage::new` 在 `VaultCore::new` 之前构造对象存储，因此 `object_storage.s3.access_key_id`/`secret_access_key` 暂时不能直接改为 SecretRef；`AppContext::new` 在 vault 前连接 Redis，因此带密码的 `redis.url` 也应按引导/部署平台 secret 处理。若要让对象存储凭据进 vault，必须先把初始化顺序拆成“DB-only Storage -> Vault -> resolve object storage secrets -> 构造完整 Storage/服务”。
3. **可迁移凭据（vault 就绪后才被使用，可改为 SecretRef）。** 这是“消费点晚于 vault 且不阻塞 `AppContext` 构造”的字段。**`mail.password_ref` 现在就是此类的第一个已落地成员**：`AppContext::new` 在 `VaultCore::new` 之后解析它，再把解析后的值交给 `SmtpMailer::new_with_password(...)`，随后启动 `EmailDispatcher`。明文 `mail.password` 仍作为兼容期入口存在，但已用 `SecretString` 包装并输出 deprecation warning；后续仍需补 source diagnostics 与样例治理，避免它经错误链或外部库边界泄露。未来新增 OAuth client secret、第三方 API key 等字段同理，只有确认其消费点晚于 vault 且不会阻塞 `AppContext` 构造，才可纳入此类。
4. **非敏感运行参数。** 维持现状，明文留在 TOML。

`Config` 反序列化阶段只构建强类型 `SecretRef`，校验阶段检查引用格式与必填性；**真实 secret 的读取发生在 `AppContext` 起来、vault 就绪之后**，由统一的 secret resolver 负责，并由 resolver 负责缓存、过期、脱敏日志和读取失败诊断。

对于运维命令需要区分两条路径：服务运行时 resolver 在完整 `AppContext` 创建后使用；`config secret set/check` 和 `config validate --resolve-secrets` 则只应建立最小 DB/Vault bootstrap，不能因为写入一个后置凭据而强制初始化 Redis、对象存储或 HTTP 服务。

> 关于数据库连接串：可以把内嵌密码的连接串拆为“普通连接参数 + 密码”，以避免整串成为不可脱敏字符串。但拆出来的密码应走**环境变量/部署平台 secret**，**不是 vault SecretRef**。早前“把 db 密码也拆成 `password_ref` 交给 vault”的设想与引导循环冲突，已废弃。

建议在每个涉及敏感字段的 PR 前维护并更新字段分类表，作为是否允许使用本项目 vault 的依据（**当前真实的配置侧可迁移凭据已有一个落点：`mail.password_ref`**）：

| 字段 | 当前消费点 | 分类 | 迁移结论 |
| --- | --- | --- | --- |
| `database.db_url` / 拆分后的数据库密码 | `Storage::new` 建库连接（最早） | 引导配置（硬循环） | **永远**只能走 TOML/env/部署平台 secret，不进本项目 vault |
| `redis.url`（若含密码） | `AppContext::new` 中 vault 前 `init_connection` | 早期运行时依赖 | 走 env/部署平台 secret；所有日志与错误必须脱敏 |
| `object_storage.s3.*` / `secret_access_key` | `Storage::new` 中 `crate::jupiter::storage::object_storage::ObjectStorageFactory::build`（vault 前） | 早期运行时依赖 | 先保持现状；若要入 vault，必须先把 Storage 拆成 DB-only → Vault → resolve secrets → 完整构造 |
| `orion_server.db_url` 等 | Orion 作为独立服务使用 | 外部服务配置 | 由 Orion 自己或部署平台管理，monoengine 不应声称代管 |
| `mail.password_ref`（推荐）/ `mail.password`（兼容期明文） | `AppContext::new` 中 `VaultCore::new` 之后解析，再构造 `SmtpMailer` 和 `EmailDispatcher` | **可迁移凭据（已落地首个成员）** | `password_ref`、最小 resolver、互斥校验、mailer 错误传播、明文 deprecation warning 和 `SecretString` 防误打印已落地；剩余=source diagnostics、样例治理、dispatcher 生命周期 |
| `ssh_server_key`、PGP/Nostr 等现有 vault secret | 已由 vault 管理（vault 内部） | vault 内部 secret | fail-closed、权限收紧、root token 脱敏/退役和主路径错误 Result 化已落地；剩余=部署侧 key material 托管、备份恢复和 KMS/secret manager 策略 |

#### SecretRef 已实现形态与使用规则

`SecretRef` 是配置文件中替代明文敏感值的引用类型，已在 `src/config/secret.rs` 中实现。`Config::new` 阶段只校验其格式合法性，不读取真实值；真实读取发生在 vault 就绪后的 resolver 阶段。下面的代码块表达使用契约，后续顶层 `src/config/` 拆分时应迁移现有实现，而不是重新设计一套不兼容类型。

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
/// 当前实现还应保留 evict/evict_all 等缓存失效能力。
#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, reference: &SecretRef) -> Result<ResolvedSecret, MegaError>;
}
```

**与现有 Vault API 的路径映射**：`secret/{name}` 前缀是在 `read_secret`/`write_secret`/`delete_secret` 内部（`vault_core.rs:165/173/181`，经 `format!("secret/{name}")`）拼接的，**不是**在底层 `read_api`/`write_api`。因此 `vault://secret/config/prod/mail/password#value` 应解析为 `read_secret("config/prod/mail/password")`，再从返回的 KV map 中读取 `value` 字段；不带 `#field` 时默认读取 `value` 字段。resolver 必须以**去前缀**的 name 调用 `read_secret`，不能把完整 URI 直接传入，否则会产生 `secret/secret/...` 这类路径错误。`version` 字段首版只作为预留元数据，除非 vault 后端实际提供版本语义，否则不得在文档或 API 中承诺 KV v2 风格版本读取。

**`mail.password` 的兼容期治理（基线已支持两种形态）**：
- `mail.password`（`Option<SecretString>`）和 `mail.password_ref`（`Option<SecretRef>`）已是真实字段，当前允许以下两种形态之一：
  - `password = "plain_text"`（兼容期保留，输出 deprecation warning；Debug/Serialize 输出脱敏）
  - `password_ref = "vault://secret/config/prod/mail/password#value"`（推荐；profile/命名空间按部署环境替换）
- 互斥校验已存在：两者同时存在时为 hard error；两者都不存在时按原语义处理。后续应把该校验纳入集中 `validate.rs` 和 source diagnostics，使错误包含字段路径、来源和修复建议。
- 兼容期内继续保证携带 `password = ...` 的旧 `[mail]` 段可以反序列化；明文路径已输出 deprecation warning，并通过最小自定义 `SecretString` 包装避免 Debug/Serialize 泄露。`.expose_secret()` 当前集中在 `SmtpMailer::new` 外部驱动适配层。
- 完全迁移后，可移除 `password` 字段，仅保留 `password_ref`。OAuth client secret 等仍未落地字段的同类策略，仍需等对应 schema 真实存在后再写。

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

继续扩展配置 SecretRef 之前，必须把配置加载、校验和消费端错误中的敏感信息输出收敛到统一 redaction 工具，否则“SecretRef 不进 TOML”的收益会被普通错误链抵消。至少应完成：

- 数据库连接日志、Postgres fallback warning、Orion DB 日志中的 URL 脱敏，不能输出用户名密码；
- Redis 连接错误中的 URL 脱敏，不能在错误信息中输出密码；
- Vault root token、secret shares、完整 `core_key.json` 内容的旧输出路径已清理；后续新增日志必须继续沿用这个边界，不能把 token/shares/key material 写入 stdout、stderr、tracing 或错误链；
- **`mail.password` 仍是兼容期入口，但已改为 `Option<SecretString>`**：`Debug` 与 `Serialize` 输出已脱敏，明文只在 SMTP 适配层显式暴露；仍需继续避免错误链、source diagnostics 或外部库边界回显真实密码；
- **`SecretRef` 默认输出已脱敏**：`Debug` / `Display` 与 resolver 失败诊断输出 `vault://secret/***#***`；`config secret ref/set/check` 成功路径使用 `as_uri()` 显式回显完整引用，便于脚本接入；
- 配置错误模型输出字段路径和失败原因即可，默认不输出原始敏感值；确需输出原始值时必须经过统一 redaction。

#### 残余安全边界：`core_key.json` 自动解封材料

Vault 加固核心子集已经完成：`core_key.json` 缺失时 fail-closed，不再自动清空 vault 表；root token 不再长期保存于 key 文件；初始化与常规运行不再输出 root token、shares 或完整 key 文件内容；Unix 下 key 目录/文件权限收紧到 `0700` / `0600`；常规 secret 访问使用限权 runtime tokens。

这意味着 config 后续阶段不再被“先修 Vault 旧泄露路径”阻塞，可以继续落地模块拆分、诊断、`config init`、Profile 和测试分层。但把可迁移凭据集中进 vault 仍**不能**抵御能读取部署机 key 文件的攻击者，因为自动解封材料仍需要可被进程读取。生产部署必须把下面事项作为部署/运维交付，而不是再塞回 config 模块的实现前置：

1. **文件权限与路径安全**：
   - 继续保持 `core_key.json` 文件权限仅所有者可读写（Unix 权限 `0600`），其上级目录权限为 `0700`；新增部署脚本或打包流程不得放宽这些权限。
   - 在 Dockerfile 或容器打包脚本中，必须将 `core_key.json` 添加到 `.dockerignore` 中，绝对禁止其随镜像分发，并排除在所有自动化备份和集中式日志收集（如 ElasticSearch/Grafana Loki）的目录范围外。
2. **生产环境避免本地明文 key 文件**：
    - **Systemd 部署**：在 Linux 主机上，应利用 Systemd 的安全凭据机制（`LoadCredential=vault_key:/etc/keys/vault_key`）。在服务启动时，Systemd 会通过 `/run/credentials/...` 下的受限文件传递密钥材料，避免把 `core_key.json` 作为普通明文文件长期保存在应用数据目录。该机制仍依赖主机权限边界，不能抵御 root 或能读取凭据源文件的攻击者。
    - **Kubernetes 部署**：禁止使用应用本地持久化卷存储 `core_key.json`。可以将 Vault 的初始化 shares 作为 Kubernetes Secret 存储，并挂载为只读 Secret Volume 或通过外部 Secret Manager 注入；同时必须启用 etcd encryption at rest、限制 RBAC，并避免 `kubectl describe`、审计日志或 CI 输出泄露 secret。Kubernetes Secret 不是天然加密保险箱，不能在文档中承诺“无盘即绝对安全”。
3. **备份与灾难恢复（Disaster Recovery）演练**：
    - 必须建立离线的、加密的 key material 备份机制（如利用外部多签 GPG 加密备份在离线冷存储中）。
    - 定期开展不依赖本地 key 文件的冷启动、恢复和手动解封演练。

因此，配置 SecretRef 的安全收益应准确表述为“secret 不进入普通配置文件、git、常规日志和错误信息”；若要声称具备更强的静态数据保护，需要同时交付部署侧 KMS/secret manager、受控挂载、备份恢复和恢复演练。

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

当前 `DbConfig::default()`、Orion 默认 DB URL、`config/config.toml` 和 `config init` 生成结果已完成首批可预测 PostgreSQL userinfo 清理。后续继续保持以下规则，避免默认配置重新写入可复用密码：

- **数据库密码**：`config init` 不应在生成的 TOML 中写入默认密码；而是输出提示要求用户通过 `MEGA_DATABASE__DB_URL`、拆分后的数据库密码环境变量、文件挂载 secret 或部署平台 secret 机制注入。数据库密码**不得**通过 `config secret set` 写入本项目 vault。若必须提供本地开发默认值，应使用空字符串并在校验阶段报 `missing_database_url` 错误，或显式标注为仅本地开发可用。
- **其他引导/早期凭据**：Redis URL、当前阶段的对象存储 key 等同样不在 `config init` 结果中预设真实凭据，也不能默认生成本项目 Vault SecretRef；只保留空字符串、示例占位符或部署平台 secret 注入说明。只有确认晚于 vault 消费且已真实接入 `Config` 的字段（例如已落地的 `mail.password_ref`）才可生成 `SecretRef` 占位符。
- **vault 不属于 `config init` 职责**：`config init` 只操作配置文件，不接触数据库或 vault；Vault 初始化、解封、key material 恢复和 reset 流程应由独立运维命令或部署流程承担。

#### secret 热加载边界

敏感数据的热加载不等同于普通配置热加载。配置文件中的 `SecretRef` 变化属于配置变更，需要走 `reload.rs` 的白名单、校验和回滚流程；而 vault 内部 secret 值轮换由 vault 模块和 secret resolver 管理，业务组件只在下一次获取、缓存过期或收到明确轮换通知时读取新值。任何 secret 读取失败都不能把明文写入日志，错误信息只输出 secret 路径、版本、字段路径和失败原因。

### secret 解析的依赖顺序

由于 `Config → Storage(DB) → Vault`，secret 解析必须排在 vault 就绪之后，并按消费端对 vault 的依赖排序。当前服务启动的真实顺序更精确地说是：

```text
Config::new                                  # context/mod.rs:30 起 AppContext::new
  -> Storage::new(config)                    # 建数据库连接、构造对象存储、初始化部分存储服务
  -> init_connection(redis)                  # :36     连接 Redis
  -> VaultCore::new(storage)                 # vault 才就绪
  -> validate mail password/password_ref     # vault 之后，检查互斥关系
  -> resolve mail.password_ref               # 通过 VaultSecretResolver 读取真实值
  -> SmtpMailer::new_with_password(...)      # 失败返回 MegaError
  -> EmailDispatcher::new(...) + spawn       # mail 启用时启动后台 outbox dispatcher
  -> init_monorepo(config.monorepo)          # vault 之后，失败返回错误
  -> HTTP/SSH/multi 服务分发                 # HTTP 监听地址解析/绑定失败会返回错误
```

因此，当前 `Storage::new` 或 Redis 初始化阶段已经消费的任何 secret，都不能直接改为 vault SecretRef。只适合迁移在 vault 之后才初始化/使用的字段。**SMTP mailer 密码的前提已满足，且 `mail.password_ref` 已接入**；剩余工作不是再做 resolver/mail 迁移，而是治理 `mail.password` 明文兼容路径、错误脱敏和 dispatcher 生命周期。

这条顺序只适用于服务启动。对于 `config secret set/check`、`config validate --resolve-secrets` 这类运维命令，当前实现已经没有复用完整 `AppContext`，而是走最小 DB/Vault bootstrap：

```text
Config::load_vault_bootstrap / Config::new
  -> VaultCore::from_database_config # 只建立 vault 所需的数据库能力
  -> vault 就绪                      # 不初始化 Redis/S3/HTTP/后台任务
  -> SecretResolver::new(vault)
  -> secret set/check/resolve
```

后续新增 `config secret` 子命令或 `config init` 时必须延续这个边界：读写 vault secret 不应依赖对象存储凭据、Redis 可用性、monorepo 初始化或服务端口绑定。

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

为降低配置初始化和敏感数据写入的使用门槛，顶层 `config` 子命令族已经部分落地，入口在 `src/commands/config.rs` 并已注册到 `src/commands/mod.rs`。当前可用能力包括 `config init`、`config validate`、`config validate --resolve-secrets`、`config secret ref`、`config secret set`、`config secret check`；尚缺完整 source diagnostics、Profile 与样例/模板治理。

CLI 执行模型也已调整为按命令声明加载层级。后续新增命令时，不要绕过 `LoadMode`，而应在命令注册层补齐对应加载模式：

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

CLI 先解析全局参数和子命令，再根据命令声明的 `LoadMode` 执行对应加载。这样可以保证 `config init` 和 `config secret ref` 不读取配置，`config validate` 只由 CLI 定位 base/profile 路径并在命令内运行解析/校验流水线，`config secret set/check` 只建立 DB/Vault 最小上下文，`service http` 才构造完整 `AppContext`。`config init` 已使用“不依赖配置解析”的加载模式，保证无配置或坏配置时也可运行。

| 命令类型 | 需要的加载层级 | 说明 |
| --- | --- | --- |
| `config init` | 只需要目标路径和模板 | 配置不存在或损坏时也必须可运行 |
| `config validate` | `RawSources` + 命令内解析/校验流水线 | 需要能报告坏配置，而不是被 CLI 预加载拦截 |
| `config validate --resolve-secrets` | `RawSources` + 命令内解析 `Config` + 最小 DB/Vault bootstrap | 用于检查 SecretRef 是否可读，不应初始化 Redis/S3/HTTP |
| `config secret ref` | SecretRef 命名规则 | 只生成引用，不应依赖 DB/vault |
| `config secret set/check` | 完整 `Config` + 最小 DB/Vault bootstrap | 缺少 DB/vault 时输出前置步骤；不得构造完整 `AppContext` |
| `service ...` | 完整 `Config` + `AppContext` | 保持现有服务启动语义 |

命令族当前状态如下：

```text
monoengine config validate                         # 已实现
monoengine config validate --resolve-secrets       # 已实现
monoengine config validate --deny-warnings         # 已实现，source diagnostics warning 作为失败
monoengine config validate --show-sources          # 已实现，显式打印 base/profile/env 字段来源图与覆盖关系
monoengine config secret ref mail.password ...     # 已实现
monoengine config secret set mail.password ...     # 已实现，只支持 mail.password
monoengine config secret check mail.password ...   # 已实现，只支持 mail.password
monoengine config init                             # 已实现，默认写入 --config 或 config/config.toml
monoengine config init --output /path/config.toml  # 已实现，指定输出路径
monoengine config init --force                     # 已实现，覆盖已有文件
```

其中 `config init` 已作为初始化入口落地，负责生成配置骨架和敏感字段引用，但不写入真实敏感值；已实现的 `config secret set` 负责把真实敏感值写入 `vault`；`config secret check` 负责检查配置中的 `SecretRef` 是否可解析且具备权限；`config validate` 负责校验普通配置、字段语义和可选的 secret 解析链路。

> **引导顺序约束（重要）：** `config secret set`/`check` 与 `validate --resolve-secrets` 都需要 vault，而 vault 需要可用的数据库。因此这些子命令必须先建立数据库连接、初始化/解封 vault，再读写 secret；在数据库尚未就绪的全新机器上**无法**直接执行 `secret set`。它们只能写入晚于 vault 消费的可迁移凭据，不能用于数据库密码、Redis URL 或当前阶段的对象存储 key。`config init` 与不带 `--resolve-secrets` 的 `config validate` 则只操作配置文件、不依赖 vault，可在裸机执行。命令实现应在缺少数据库/vault 时给出明确的前置步骤提示，而不是 panic，也不得为了访问 vault 构造完整 `AppContext`。

`config init` 的职责应保持清晰：创建或检查目标配置文件（默认使用全局 `--config`，否则写入 `config/config.toml`；也可用 `--output` 指定路径），写入基础样例配置，派生 `${base_dir}`、日志目录、缓存目录和本地对象存储目录，为可迁移凭据生成 `SecretRef` 占位引用，并输出后续需要执行的 `config secret set` 命令清单。默认不覆盖已有文件，只有显式传入 `--force` 才会覆盖。示例应尽量贴近当前 schema，**只为真实存在的字段生成占位引用**。

> 注意：下面的 `[mail]` 示例对应 `MailConfig` 的字段形态（扁平结构：`enabled`/`smtp_host`/`smtp_port`/`username`/`password`/`password_ref`/`from`/`starttls`）。**该结构现已作为 `MailConfig` 接入 `Config`**，且 `password_ref` 已可由 resolver 解析，因此 `config init` 可以直接生成 `password_ref` 占位。两点提示：(1) 仓库样例用 `starttls = false`、端口 `2525` 关闭了 STARTTLS，仅适合本地开发，`config init` 生成生产骨架时应使用安全默认；(2) 样例里的 `smtp_tls`/`tls` 是 `MailConfig` 不识别的字段，会被 serde 丢弃，不应写入生成结果。

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
  printf '%s' "$SMTP_PASSWORD" | monoengine config secret set mail.password --vault-path config/prod/mail/password --field value --value-stdin
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

自动化测试应采用独立的配置方案：单元测试优先通过 `testing.rs`（首批已有 `TestConfigBuilder`/`isolated_config()`）构造内存配置或最小 TOML 片段；需要文件加载链路的测试使用临时目录生成测试配置文件，并显式传入 `--config` 或 `MEGA_CONFIG`；集成测试继续通过 `.env.test` 提供数据库、Redis、邮件等外部依赖端点，但 `.env.test` 只负责测试环境覆盖，不应修改仓库中的 `config/config.toml`。测试配置中的 `base_dir`、数据库名、对象存储根目录、日志目录和缓存目录都应隔离到测试临时目录，避免并发测试互相污染。

对于依赖敏感凭据的测试，不应在测试 TOML、`.env.test` 或日志中写入真实 API Key。配置模块应提供可注入的测试 secret resolver 或 vault 测试后端（现有 vault 测试已使用 `tempfile` + `test_storage` 构造隔离实例，可复用此模式），用固定的假 secret、过期 secret、缺失 secret 和权限失败场景覆盖消费端行为；缺失 `mail.password_ref` 已由命令层可注入 resolver 单测和真实 Vault resolver 单测覆盖，后续继续补权限失败。热加载相关测试也应使用测试配置文件和临时 resolver：分别验证基础字段热更新成功、不可热更新字段只告警不生效、SecretRef 变更遵循白名单、候选配置校验失败时旧配置继续生效。

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

`Config::new`、`Config::load_str` 和 `Config::load_sources` 已共用同一套 `MEGA_*` source builder 与 env/list 解析规则，单测覆盖 `load_str`/`load_sources` 对 `MEGA_MONOREPO__ROOT_DIRS=a,b` 的列表解析。后续 Profile 合并引入新 source 时，仍必须沿用该统一 builder，避免测试路径与生产路径再次分叉。

未知字段与废弃字段也应纳入流水线。Serde 默认可能忽略未知字段；当前 `config validate` 已在反序列化后读取 raw TOML 并基于当前 schema 白名单输出 key 级 warning，可覆盖拼错字段、孤立顶层段和兼容期废弃字段；同时已对 `MEGA_*` 环境变量覆盖项做首批键名级 warning，覆盖未知覆盖项、孤立 `oauth` 覆盖项和旧 mail TLS 覆盖项且不输出变量值。坏环境变量类型、profile 类型冲突、未解析/非法占位符值已包装为脱敏错误，包含来源、字段路径、期望类型或修复建议，不再打印原始值。后续还需要把相同诊断扩展到更丰富的跨 source 覆盖关系。新增字段必须提供 `serde(default)` 或 `Option<T>`，移除字段必须经历 warning 过渡期。

热加载应复用同一条解析、展开、规范化和校验流水线，避免启动加载与运行期重载出现两套语义。`reload.rs` 首批已维护 `ConfigHandle`，内部持有当前生效的 `Arc<Config>`，并支持从 base/profile 路径构建候选配置、候选配置校验、日志字段白名单更新、`mail.enabled` 从 true 到 false 的运行期关停、已运行 artifact GC 任务的调度参数热更新与关停、数据库/Redis/邮件重启类字段需重启报告、SecretRef 变更不泄露不发布、失败保留旧快照，以及订阅组件应用/失败回滚；`ConfigReloadWatcher` 已可轮询 base/profile 文件签名变化并触发同一 reload 流水线，坏配置会被拒绝并继续运行；`service` 命令已在服务生命周期内启动 watcher，并在子命令返回后关闭 watcher；日志 reload subscriber 已注册到 service 的 `ConfigHandle`，使 `log.level` / `log.print_std` / `log.with_ansi` 变化先应用到 tracing layer 再发布新快照；邮件 dispatcher 关停 subscriber 已在 dispatcher 启动时注册，使 `mail.enabled = false` 停止处理待发送队列；artifact GC subscriber 已在 HTTP service 启动 GC 任务时注册，使 `artifacts_gc.interval_secs` / `grace_secs` / `batch_limit` 和 `enable=false` 作用于运行中任务；`Storage`/`AppContext` 已接入共享 `ConfigHandle`，业务组件经 `storage.config()` 可读取当前稳定快照。当配置文件或受支持的配置源发生变化时，先构建候选 `Config`，再执行差异计算和白名单校验。只有日志级别、日志输出细节、部分功能开关、任务调度间隔、邮件通知关停等无须重建长生命周期连接的字段可以直接热更新；数据库、Redis、对象存储、监听地址端口、OAuth 客户端密钥、artifact GC 从关闭到启用、邮件重新启用和 SMTP 参数/凭据等字段默认视为启动期配置，检测到变化时应记录告警并提示重启，而不是在运行期隐式重建。

热加载应用过程应具备原子性：候选配置解析、校验或组件回调任一阶段失败，都不能替换当前生效配置；组件回调成功后再发布新的 `Arc<Config>` 快照，并记录变更字段、来源和结果。订阅接口应按领域注册，例如日志系统订阅 `log` 可热更新字段，后台任务订阅对应调度配置，避免业务代码直接监听文件变化或自行重新解析配置。

> 热加载与现有访问模式的衔接：当前 `Storage::config()` 返回 `Arc<Config>` 且调用点遍布全仓。切到快照句柄（如 `ArcSwap<Config>`）在技术上可行，但任何“取出 Arc 后跨 await/长操作持有”的调用会钉住旧快照，语义需要逐点确认。这也是把热加载放在独立阶段、不与拆分/迁移捆绑的原因。

### 迁移步骤（从 2026-06-18 基线继续执行）

整体拆为多个阶段，每个阶段独立可编译、可回归、可单独评审合并。注意：只有纯移动和 re-export shim 可以承诺“零行为变更”；错误模型、校验规则、`config init`、profile、source diagnostics、热加载都会改变失败形态或运行语义，必须单独评审。

**已完成基线（不要重复实现）**

- `LoadMode` / `CommandContext` / 两阶段 CLI 分发已落地。
- `config secret ref/set/check` 与 `config validate --resolve-secrets` 已落地。
- 最小 DB/Vault bootstrap 已落地，不依赖 Redis/S3/完整 `AppContext`。
- `SecretRef`、`SecretResolver`、`VaultSecretResolver` 已落地。
- `mail.password_ref` 已落地，且 `mail.password` / `mail.password_ref` 互斥。
- Vault fail-closed、root token 脱敏/退役、key 权限、runtime tokens、secret 访问审计 hook 已落地。
- 对象存储凭据不迁入本项目 vault 的边界已明确；`object_storage.*` 仍属于早期运行时依赖。

**阶段 0 — 文档再基线与执行清单收敛（当前文档修订）**

1. 删除或降级仍把 `LoadMode`、最小 bootstrap、`config secret`、SecretRef、`mail.password_ref`、Vault fail-closed 当作未实现前置的表述。
2. 把后续计划聚焦到仍未落地的 config 工作：模块拆分、错误模型、redaction/SecretString、Profile、source diagnostics、样例/测试配置分层、CI 校验和热加载；`config init` 只保留模板治理和样例校验的后续收敛项。
3. 更新实施前检查清单，确保每个阶段先确认“已完成基线”，避免重复施工。

> **验收标准**：本文档可以直接指导下一阶段实现；读者不会把已完成的 Vault/LoadMode/SecretRef 工作误判为阻塞项或待办项。

**阶段 1 — 顶层结构迁移（零行为变更；已完成）**

4. 已完成：新建 `src/config/` 目录，将原 `src/common/config.rs` 移动为 `src/config/mod.rs`，并把 `loader.rs`、`template.rs`、`secret.rs` 迁入 `src/config/`。
5. 已完成：在 `src/main.rs` 新增顶层 `pub mod config;`；源码调用方已迁到 `crate::config`，`src/common/config.rs` shim 已删除。
6. 已完成：拆出 `model.rs`，承接 `Config`、`VaultBootstrapConfig` 及各领域子配置结构体、默认值和现有模型方法；拆出 `source.rs`，承接 source 构建；拆出 `expand.rs`，承接占位符展开。
7. 补充单元测试覆盖路径定位、env 覆盖、列表解析、SecretRef 反序列化和占位符展开，确认移动前后行为一致。

> **验收标准**：全仓源码无 `common::config` 引用；`cargo check` 通过；`config secret` 和服务启动行为保持一致。

**阶段 2 — 错误模型、redaction/SecretString 与保守校验**

8. 已部分完成：新增 `error.rs`，并把 `variable_placeholder_substitute` 原 10 处 `unwrap` 替换为 `ConfigError` 诊断；未解析/非法占位符值已脱敏并带修复建议；`mega_base`/`mega_cache` 的早期目录解析 `unwrap` 已改为 env/system/fallback 的无 panic 路径；`database_connection` 的连接和 migration `expect`、Redis 初始化的 `expect`/`panic`、BuckService 构造 `expect`、HTTP 监听地址解析/绑定 `unwrap` 已改为返回错误并由启动/bootstrap 调用方传播。剩余加载路径上的 `expect`/`panic` 仍需继续收敛，这会改变失败形态，不应并入纯移动阶段。
9. 已完成首批：建立 `src/config/redaction.rs`，提供统一 URL redaction，并接入数据库连接日志与 Redis 连接失败信息，确保连接串中的 username/password 不进入这些高风险输出；`SecretRef` 的 `Debug` / `Display` 与 resolver 失败诊断默认输出 `vault://secret/***#***`，只有 `as_uri()` 这类显式 CLI 成功输出保留完整引用。仍需继续覆盖 SMTP 密码、对象存储 key、外部服务 URL，以及后续新增 source diagnostics 中的敏感值。Vault root token / shares 旧泄露路径已清理，不再作为本阶段前置。
10. 已完成首批：`mail.password` 明文兼容路径改为 `Option<SecretString>`，Debug/Serialize 输出脱敏；`config validate` 与服务启动路径会对明文 `mail.password` 输出 deprecation warning；`.expose_secret()` 集中在 `SmtpMailer::new` 适配层。后续仍需在 source diagnostics、样例配置和模板中继续推动迁移到 `mail.password_ref`。
11. 已完成首批：新增 `src/config/validate.rs`，实现 `Config::validate()` 首批 hard error 校验，覆盖 `database.db_type`、`database.db_url` scheme、数据库连接池参数、`log.level`、`lfs` 路径/URL、`build.orion_server`、`redis.url`、`mail.password` / `mail.password_ref` 互斥、`mail.enabled` 时 `smtp_host` / `from` 必填、Buck 限制、object storage local/S3/S3-compatible/GCS 后端必填项，以及可选 `orion_server` 端口/URL/DB URL；`config validate` 已复用该入口，`Storage::new` 的 Buck 非法配置已改为返回 `MegaError`。后续集中校验主要随新增字段继续补规则。
12. 已完成首批：`config validate` 读取原始 TOML，对孤立 `[oauth]` 顶层段、`[mail].smtp_tls` / `[mail].tls` 以及任意未知字段输出 warning，并覆盖嵌套 table 与数组内 inline table；文件级 warning 已通过 `known_unconsumed_file_fields` 保留 source path，可测试 base/profile 来源；同时会扫描 `MEGA_*` 环境变量名，对未知覆盖项、`MEGA_OAUTH__...`、`MEGA_MAIL__TLS` / `MEGA_MAIL__SMTP_TLS` 输出不含变量值的 warning，并忽略 `MEGA_CONFIG`、`MEGA_PROFILE`、`MEGA_BASE_DIR`、`MEGA_CACHE_DIR` 等加载器运行时变量。base/profile/env warning 已由 `ConfigSourceDiagnostics` 汇总，`config validate --deny-warnings` 可将这些 warning 升级为失败，供 CI 严格门禁使用；`config validate --show-sources` 已能显式输出 base/profile/env 字段来源图以及 profile/env 覆盖 base/profile 的字段路径和来源，不输出原始值。后续仍需扩展为更完整 source diagnostics，覆盖更完整的 source-level 修复建议。

> **验收标准**：配置损坏时返回包含配置文件路径、字段路径和修复建议的诊断信息；敏感值不进入日志/错误/Debug；`BuckConfig::validate()` 不再在 `Storage::new` 中 panic；新增校验和 redaction 测试覆盖。

**阶段 3 — 消费端路径迁移 + 移除 shim（已完成）**

13. 已完成：将所有源码中的 `crate::common::config::*`/`common::config::*` 改为 `crate::config::*`/`config::*`。
14. 已完成：构建确认无残留引用后，删除阶段 1 的 re-export shim，并从 `common` 彻底移除配置承载职责。本阶段不改运行语义。

> **验收标准**：全仓无 `common::config` 引用；`src/config/` 成为唯一配置入口；服务、`config secret`、测试辅助均通过新路径编译。

**阶段 4 — `config init` 与 source diagnostics**

15. 已完成首批：新增 `monoengine config init`，只操作配置文件和模板，不连接数据库或 vault，不接收或写入真实 secret；命令走 `LoadMode::None`，支持默认路径、全局 `--config`、`--output` 和 `--force`。
16. 已完成首批：`config init` 生成安全默认骨架，不写入可预测生产密码；数据库、Redis、当前对象存储凭据只给 env/部署 secret 指引；对 `mail.password_ref` 生成 SecretRef 占位，并提示后续使用 `config secret set --value-stdin` 写入真实值。
17. 已完成首批 raw TOML 与 `MEGA_*` diagnostics：`config validate` 已切到 `LoadMode::RawSources` 并在命令内解析配置，坏 TOML/坏 env 类型等不会被 CLI 子命令分发前预加载拦截；坏 TOML 可由文件解析错误返回，未知字段、废弃字段、未知环境变量覆盖项、孤立 `oauth` 环境变量覆盖项和旧 mail TLS 环境变量覆盖项已在 `config validate` 中 warning，且 env warning 只输出变量名和映射字段路径，不输出变量值；文件级 warning 现在保留可测试 source path，能区分 base/profile 文件来源；base/profile/env warning 已可汇总，`--deny-warnings` 可把这些 warning 作为命令失败；`--show-sources` 可显式输出 base/profile/env 字段来源图和 profile/env 覆盖 base/profile 的字段路径与来源，且不输出原始值；坏环境变量类型、profile 类型冲突和占位符展开错误已报出来源/字段路径和修复建议，并脱敏原始值。继续补齐 `RawSources`/source diagnostics：更完整的 source-level 修复建议等应能在不构造完整 `AppContext` 的情况下报告。
18. 保持已实现的 `config secret ref/set/check` 行为：只允许 `mail.password` 等后置可迁移凭据，继续拒绝 database/redis/object storage 凭据。

> **验收标准**：`monoengine config init` 可在无配置目录下运行并生成样例，且生成结果可解析、可校验、无明文密码字段；`config validate` 能在坏配置时输出诊断而非被预加载拦截；`config secret ref/set/check` 的既有测试继续通过。

**阶段 5 — 基础样例配置、Profile 与测试配置分层 + CI**

19. 已完成首批：`config/config.toml` 与模型默认 PostgreSQL URL 已移除可预测 userinfo，保留与当前 schema 对齐的本地默认值和 env/部署 secret 注入指引，并新增仓库默认样例解析、校验和无可预测凭据测试；后续继续补 CI 配置校验矩阵。
20. 已完成首批：固定 Profile 文件命名、加载优先级、数组覆盖语义和 SecretRef namespace，并补充 profile 合并、profile/env 覆盖顺序和最小 vault bootstrap 合并测试；后续继续补完整 profile source diagnostics 与 CI 矩阵。
21. 已完成首批：新增 `testing.rs`，提供 `TestConfigBuilder`、`isolated_config()`、`.env.test` 风格覆盖合并和 `TestSecretResolver`；CLI `parse` 的无子命令加载单测已改用临时配置文件。后续继续把文件加载链路、Profile 合并结果和更多现有测试迁移到该 helper。
22. 已完成首批：新增 `.github/workflows/config-validation.yml`，在 CI 中覆盖基础样例、默认模板、`config init` 结果、profile 合并结果、`MEGA_*` 未消费覆盖项单测、坏 env/profile 类型脱敏单测、缺失 `mail.password_ref` 的命令层脱敏单测，以及坏占位符、坏 SecretRef URI、坏 Redis URL scheme 的 CLI 失败 smoke；真实 Vault resolver 已有缺失 secret 脱敏单测。后续继续把 `.env.test` 隔离测试配置、更多坏输入矩阵和 SecretRef 权限场景纳入 CI。

> **验收标准**：`config/config.toml` 不含真实生产密码或可复用生产凭据；`cargo test --all` 不依赖仓库中的 `config/config.toml` 作为隐式共享状态；CI 新增配置校验任务且通过。

**阶段 6 — 对象存储等早期依赖的后置初始化重构（可选）**

23. 仅当明确要让对象存储凭据进入 vault 时，才执行本阶段；否则保持当前边界：`object_storage.*` 来自 TOML/env/部署平台 secret，不能配置为本项目 Vault `SecretRef`。
24. 若执行本阶段，先把 `Storage::new` 拆为 DB-only storage、vault bootstrap、secret resolve、完整 storage/service 初始化。
25. 重新梳理 Redis、对象存储、Orion 等字段依赖表，只有在确认消费点晚于 vault 且失败语义可接受后，才允许迁移为 SecretRef。

> **验收标准**：S3/S3-compatible 凭据迁移前，服务启动链路中不再在 vault 前构造对象存储；缺少对象存储 secret 时返回可诊断错误，不影响 `config secret set/check` 对其他 secret 的操作。

**阶段 7 — 受控热加载（独立变更）**

26. 已完成首批：新增 `reload.rs`，提供 `ConfigHandle` 快照句柄，`reload_from_path` 复用 base/profile 文件加载流水线构建候选配置，复用 `Config::validate()` 校验候选配置，按字段白名单应用 `log.level` / `log.print_std` / `log.with_ansi`、`mail.enabled` true→false 关停，以及已运行 artifact GC 的 `enable` true→false、`interval_secs`、`grace_secs`、`batch_limit` 变更；数据库、Redis、artifact GC 从关闭到启用、邮件重新启用和 SMTP 参数/凭据等字段变化报告需重启，并覆盖失败时保留旧配置与 SecretRef 变更不泄露不发布；`Storage`/`AppContext` 已持有共享 `ConfigHandle`，`storage.config()` 已从 handle 读取当前快照；`ConfigReloadSubscriber` 已支持订阅组件在发布前应用变更、任一失败时逆序回滚并保留旧快照；`ConfigReloadWatcher` 已支持轮询 base/profile 文件签名变化并触发 reload，坏配置被拒绝且进程可继续运行；`service` 命令已在服务生命周期内接入 watcher。
27. 已完成首批：将运行时注入从仅直接共享 `Arc<Config>` 调整为共享快照句柄；`Storage::config()` 已无 `Weak::upgrade().expect(...)` panic，也不再只返回不可替换的初始 `Arc<Config>`。仍需逐点确认“取出 Arc 后跨 await 持有”的调用语义，新增依赖前评估必要性。
28. 改造真实消费端接入订阅方式（日志、功能开关、任务调度、邮件通知等只订阅各自可热更新字段；不可热更新字段变化只告警提示重启）。日志 reload subscriber 已首批注册到 service 启动路径；邮件 dispatcher 已在启动时注册 `mail.enabled = false` 的关停 subscriber；HTTP artifact GC 已在任务启动时注册调度 subscriber。后续重点是把 Buck cleanup、邮件重新启用/重配等其它实际组件按同一模式注册或继续明确为重启字段。
29. 已完成首批核心测试：可热更新日志字段生效、base/profile 文件候选可加载、数据库字段变化只报告需重启且不发布、SecretRef 变更遵循白名单且不泄露引用、候选校验失败保留旧快照、订阅组件发布前应用、订阅组件应用失败时回滚并保留旧快照、profile 文件变化触发 watcher reload、非法 profile 变化保留旧快照、watcher shutdown 正常退出、service watcher task 触发 profile reload、service 注册日志 reload subscriber 后可发布日志字段变更、`mail.enabled` true→false 可发布且 dispatcher 停止处理 pending job、`mail.enabled` false→true 仍报告需重启、artifact GC 运行期调度字段可发布且 subscriber 更新 control、artifact GC 从关闭到启用仍报告需重启。后续补 Buck cleanup 和邮件重配等其它真实消费端订阅测试。

> **验收标准**：白名单字段（如 `log.level`）变更后无需重启即可生效；数据库地址变更只告警不重建连接；热加载失败时进程继续运行且保留旧配置；全量回归测试通过。

**贯穿全程**

30. 每个阶段同步更新 `README`、`config/config.toml` 注释和本文档，确保加载优先级、模块路径、`config` 命令使用方式与引导顺序、消费端访问方式、环境变量规则、敏感配置存储边界和热加载限制与实现一致。

### 前置依赖矩阵（2026-06-18 更新）

| config 阶段 | 本阶段主要工作 | 对 vault 的依赖 | 对 mail/notification 的依赖 |
|-----------|------------|-------------|----------------------|
| **0** | 文档再基线 | 已完成 vault A/B/C/D/E/H/I/J 可交付子集 | 已完成 mail/password_ref 首批接入 |
| **1** | `src/config/` 结构拆分 | 无新增依赖 | 无 |
| **2** | 错误模型 + redaction + 校验 | 复用已完成的 fail-closed/审计/限权 token；不再等待 vault A | mail 明文兼容期治理、dispatcher 可观测 |
| **3** | 路径迁移 + shim 移除 | 已完成，无新增依赖 | 调用方 import 已同步迁移 |
| **4** | `config init` + source diagnostics | 复用已完成的 `LoadMode` 和最小 bootstrap；`init` 不接触 vault | 生成 `mail.password_ref` 占位即可 |
| **5** | 样例/Profile/测试/CI | 复用 SecretRef URI 规则和最小 bootstrap | 可用测试 resolver/Noop mailer |
| **6** | 对象存储后置初始化（可选） | 仅在对象存储凭据要入 vault 时需要 | 无 |
| **7** | 热加载 | 复用 resolver 缓存/evict 能力 | 按白名单订阅 mail/notification 可热更新字段 |

**关键同步点：**
1. 已完成的 `LoadMode`/最小 bootstrap/SecretRef 是 `config init`、后续 source diagnostics 和测试分层的可复用基础。
2. redaction/SecretString 仍是 config 自身的安全补强项，但不再阻塞 vault fail-closed 或 `mail.password_ref` 首批接入。
3. 对象存储凭据迁入 vault 仍是可选后续专项；在专项完成前，文档和样例都不得暗示 `object_storage.*` 可使用本项目 Vault `SecretRef`。

### 风险与约束

- **循环依赖是硬约束，不是注脚。** `Config → Storage(DB) → Vault` 决定了：数据库凭据等引导配置永远不能是 vault SecretRef；secret 解析永远不能发生在 `Config::new` 内，只能在 vault 就绪后进行。任何规划若违反这两点（如把 db 密码做成 `password_ref` 入 vault）都不可行。
- **多环境 Profile 深度合并冲突风险**：多环境配置文件在深合并（Deep Merge）时可能因为类型不一致或覆盖错误引发运行期异常（如列表字段合并时是覆盖还是追加）。应在 `validate.rs` 中加强合并结果的 schema 校验。
- **兼容性退化与孤立配置遗留**：废弃字段如果未在多个版本中执行严格的 `WARN` 告警和清理审计，会导致老旧、不安全的配置项长期残留在用户的生产配置文件中。必须在 CI/CD 中定期开展包含废弃配置的兼容性回归测试。
- **Secret 轮转时的双凭据过渡开销**：两阶段凭据轮转可以降低切换中断风险，但 fallback 不应成为所有 secret 的默认行为。应将多版本读取能力封装在 `SecretResolver` 或专门适配层中，由消费端按外部系统语义决定是否回退。
- **包装类型（SecretString）在网络传输与持久化中的误暴露**：尽管 `SecretString` 在 Rust 代码中能有效防止 `Debug` 泄露，但在将其序列化（如写入外部监控日志、通过 OpenAPI 接口返回、或者保存到临时数据库中）时，如果序列化库（如 `serde`）未正确配置，仍可能会提取其明文。必须在编译期实施 lints 或强制配置 `#[serde(skip_serialize)]` 规则。
- **当前对象存储和 Redis 也是早期依赖。** `Storage::new` 会在 vault 就绪前构造对象存储，`AppContext::new` 会在 vault 就绪前连接 Redis；因此 S3 access key、带密码的 Redis URL 等字段不能直接按“可迁移凭据”处理，除非先重构初始化顺序。
- **CLI 两阶段加载已是基线。** `config init` 已使用 `LoadMode::None`，`config validate` 已使用 `LoadMode::RawSources` 并在命令内解析配置以避免坏配置被预加载拦截；后续新增 source diagnostics、profile 或其它配置命令时，必须继续通过 `LoadMode` 声明加载层级；不能退回“子命令分发前总是完整 `Config::new`”的模式。
- **日志与错误脱敏已完成首批高风险落点，仍需继续收敛。** Vault root token/shares 的旧泄露路径已清理；数据库连接日志和 Redis 连接失败信息已接入统一 URL redaction；`SecretRef` 默认格式化输出和 resolver 失败诊断已脱敏。剩余主要是外部服务 URL、对象存储 key、兼容期 `mail.password` 以及更多配置/source 诊断路径的统一 redaction。
- **`core_key.json` 加固核心已完成，但部署侧托管仍是生产边界。** fail-closed、权限收紧、root token 脱敏/退役已落地；把更多凭据放入 vault 仍不抵御能读取 key 文件的攻击者，生产使用必须配套 KMS/secret manager、受控挂载、备份恢复和恢复演练。
- **采用分阶段切换。** 顶层迁移和调用方路径迁移已完成；后续内部拆分、错误模型、初始化命令和热加载仍必须各自独立评审，不追求“单次变更内完成全部改造”。
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
- README 已同步实际加载优先级，包括 `mega_base()/etc/config.toml` 与默认配置生成逻辑。
- 拆分后仍应保持配置对象运行期只读共享，避免在业务流程中重新解析配置或隐式改变运行时语义。
- **BuckConfig::validate() 的启动期 panic 已清理。** `src/config/validate.rs` 已吸收 Buck 校验，`Storage::new` 复用该入口并在非法配置时返回 `MegaError`；剩余工作是把更多启动期配置错误前移到 `Config::validate()` / source diagnostics，并补来源路径与修复建议。
- **环境变量注入的可见性风险**：引导配置通过 `MEGA_*` 环境变量注入时，需意识到 `/proc/<pid>/environ`、systemd journal、容器 inspect 等场景的泄露风险。生产高敏感部署应优先使用文件挂载 secret，并通过占位符读取。
- **默认配置不生成可预测默认密码**：`config init` 生成结果、`DbConfig::default()`、Orion 默认 DB URL 和当前 `config/config.toml` 已移除硬编码 PostgreSQL userinfo，改为提示用户通过环境变量、文件挂载 secret 或部署平台 secret 注入；数据库密码不能通过本项目 vault 注入。
- **`orion_server.db_url` 是外部服务凭据**，不在 monoengine vault 管理范围内。若 Orion 自身需要 secret 管理，应由 Orion 独立解决，monoengine 只作为客户端通过部署平台 secret 注入其连接参数。

### 开始下一阶段前的执行前置（2026-06-18）

当前已没有阻塞 config 继续执行的 Vault/LoadMode/SecretRef 前置项。开始任一实现阶段前，应先做下面这些轻量确认，避免重复施工或扩大变更面：

1. **确认阶段边界**
   - 阶段 1/3 已完成顶层移动、调用方路径迁移和 shim 移除，未改变错误语义、加载语义或消费端行为。
   - 阶段 2 才引入错误模型、redaction、SecretString 和集中校验。
   - 阶段 4 已新增 `config init` 首批能力，剩余重点是完整 source diagnostics。

2. **复用已完成基线**
   - 不重新设计 `LoadMode` / `CommandContext` / `config secret`。
   - 不重新实现 `SecretRef` / `VaultSecretResolver`，只在顶层 `src/config/` 拆分时迁移现有代码。
   - 不把 Vault fail-closed、root token 脱敏、runtime token、audit hook 当作待办前置。

3. **确认敏感字段边界**
   - 当前只允许 `mail.password_ref` 作为配置侧 Vault SecretRef；`config secret set/check` 仍只支持 `mail.password`。
   - database、redis、当前 object storage 凭据继续走 TOML/env/部署平台 secret。
   - 若要扩展到对象存储，必须先启动独立的初始化顺序重构阶段。

4. **确认验证环境**
   - 文档变更不需要跑 Rust gate；任何源码变更必须按 AGENTS.md 运行 `cargo +nightly fmt --all --check`、`cargo clippy --all-targets --all-features -- -D warnings` 和 `source .env.test && cargo test --all`。
   - 如果 `.env.test` 缺失，不能静默降级为普通 `cargo test --all`。

### 预期收益

- 配置模块边界更清楚，新增配置域时只需扩展模型和校验，不必继续放大单文件。
- 加载流程更容易测试，每个阶段都可以独立构造输入和断言输出。
- 启动失败信息更明确：消灭加载路径上的 panic，部署排障时可以直接定位到配置文件、字段和值，并能区分“引用格式非法”与“真实值不可读”。
- 可迁移凭据从普通配置文件中剥离，**不再进入 git 仓库、日志和错误信息**；Vault 已完成 fail-closed 与 key 权限等核心加固，后续再通过配置 redaction 与部署侧 key material 托管进一步降低静态泄露风险。引导配置和早期运行时依赖则交由部署平台 secret 机制管理，职责清晰。
- `monoengine config init` 与 `config secret` 命令让配置初始化、SecretRef 生成、Vault 写入和解析检查形成标准流程；`config secret` 通过最小 DB/Vault bootstrap 工作，不被 Redis、S3 或完整服务初始化阻塞。
- 基础配置与测试配置职责分离，`config/config.toml` 更适合作为可提交的样例和本地起点，自动化测试则获得可重复、可隔离、可校验的配置输入。
- 配置热加载（后续阶段）有明确边界，可在不重启服务的情况下更新安全字段，同时避免长生命周期资源被隐式替换。
- 为后续配置文档生成、配置样例校验和部署前 dry-run 检查打下基础。

## 改进方案多维评估小结

下表是对本改进方案按各评估维度的结论（已综合代码现状与文档规划）：

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | **高（9/10）**。准确抓住 `Config(synchronous) → Storage(DB+ObjectStorage) → VaultCore` 这一不可打破的引导循环，正确导出四类字段划分（引导 / 早期运行时依赖 / 可迁移凭据 / 非敏感），并坚持“先有真实晚绑定消费者才能谈迁移”的原则。分阶段迁移策略与单二进制 + sea-orm + vendored `libvault` 的现实匹配良好。 |
| **可行性** | **高（8.5/10）**。原本风险最高的 CLI LoadMode、最小 DB/Vault bootstrap、SecretRef 基础设施、mail 后置消费和 Vault fail-closed 已落地。后续主要是工程拆分、诊断、redaction、模板/Profile/测试分层与热加载，均可按阶段独立交付。 |
| **完整性** | **较高（8/10）**。文档覆盖了加载链路、模块拆分、secret 分类、命令、测试分层、热加载等主要方面，并已把已完成基线与剩余工作分开。`load_str`/`load_sources` 与主路径的 list_parse 一致性已完成首批；`mega_base` / `mega_cache` 自身的早期目录解析 panic 已清理；仍需在实现阶段继续补跨平台凭据注入、完整 source diagnostics 等细节。 |
| **安全性** | **强（8.5/10）**。Vault 旧泄露路径已完成核心清理，文档现在把剩余风险限定为配置 redaction、兼容期 `mail.password` 明文、部署侧 key material 托管与 DR 演练。该表述避免夸大“放入 vault”对磁盘读取攻击者的防护能力。 |
| **功能正确性与接口兼容性** | **良好（8/10）**。`SecretRef` 到 `read_secret(name)` 的路径映射（`write_api("secret/{name}")`）与 vault_core 实现一致；`mail.password_ref`、互斥校验和 `config secret` 支持范围与当前代码一致。仍需注意：`config` 作为顶层模块名会与 `config` crate 冲突（当前代码用 `c::` 别名，已在文档中提及）；后续新增代码应避免重新引入 `common::config` 路径。 |
| **数据流与控制流正确性** | **正确（9/10）**。`AppContext::new` 中 `Storage::new → init_connection(redis) → VaultCore::new → mail.password_ref resolve → SmtpMailer/EmailDispatcher → init_monorepo` 的实际顺序与文档描述一致。secret 只能在 vault 就绪后解析、运维命令（secret set/check）必须用最小 bootstrap 而非完整 AppContext 的结论均正确。 |
| **性能与效率** | **可接受（8/10）**。resolver 内存缓存 + TTL、`Arc` 只读共享均合理。`variable_placeholder_substitute` 的两次全树 collect + clone 在启动期可忽略；其原 panic 语义已初步收敛，后续重点是诊断质量而非 CPU 成本。热加载白名单设计也避免了不必要的长连接重建。 |
| **可靠性与容错性** | **改进潜力大（8/10）**。计划中的“消灭加载路径 panic、热加载失败回滚保留旧配置、错误带字段路径+修复建议”等均会显著提升。Buck 校验、BuckService 构造、`Storage::config()`、`mega_base` / `mega_cache`、数据库连接/migration 初始化、Redis 初始化以及 HTTP 监听地址解析/绑定的已知 panic/expect 点已完成首批收敛；热加载候选失败保留旧快照、订阅组件失败回滚、base/profile 文件 watcher、service watcher 生命周期接线、日志 reload subscriber、邮件 dispatcher 关停 subscriber 和 artifact GC 调度 subscriber 也已完成首批。当前仍需重点补全 source diagnostics、更多启动期配置校验和 Buck cleanup/邮件重配等其它真实消费端订阅接入。Vault fail-closed 已完成，不再作为 config 阶段阻塞项。 |
| **兼容性与互操作性** | **良好（8/10）**。serde `Option` + `#[serde(default)]`、废弃字段 WARN 过渡期、Profile 深合并（数组整体替换而非追加）的语义均已明确。需补充：未知顶层段的告警策略、`0600/0700` 在 Windows/macOS 下的等价实现（或明确“生产仅支持类 Unix”）、以及 `config/config.toml` 作为样例时应 `deny_unknown_fields` 或至少 warn。 |
| **可扩展性与可维护性** | **良好（8.5/10）**。配置已从 `common` 提升为一级 `src/config/` 模块；继续按职责拆分为 model/loader/expand/secret/validate/testing/error 等小文件，是正确的长期方向。`Config` 成为基础设施后，新增领域只需扩展 model + 对应校验/展开规则即可。需注意继续坚持“先纯移动、再语义变更”的顺序。 |
| **合规性与标准符合性** | **良好（8/10）**。推荐的 secrecy/zeroize、stdin 避免历史记录、CI 覆盖配置样例+坏输入矩阵、字段分类表作为变更依据等，均符合现代凭据管理最佳实践。引入新依赖（secrecy 等）前要求评估编译/二进制影响的约束是正确的。 |

> **跨平台与部署现实补充**：`core_key.json` 权限（0600）、目录（0700）、`/proc` 限制、`systemd LoadCredential`、K8s Secret volume + etcd encryption at rest 等措施主要描述的是类 Unix 语义。对于 Windows、macOS 或受限容器环境，需要在实施相应阶段时提供等价机制（或在文档中显式声明“生产高敏感部署当前仅推荐类 Unix 平台”）。Kubernetes Secret 本身不是加密保险箱，`kubectl describe`、审计日志、etcd 未加密时仍可能泄露。

## 小结

`monoengine` 当前的 `Config` 实现已集中在顶层 `src/config/`，加载方式是启动期同步 `Config::new`，通过 `config` crate 合并 TOML/env、展开占位符、反序列化为强类型结构，再由 `Arc<Config>` 在运行期只读共享。这个基础仍可用，但错误模型分散、未知字段静默丢弃、明文兼容字段和测试配置混用等问题已经足够明确。

按 `vault.md` 和当前源码复核后，后续可执行入口已经很清楚：**不要再实现 LoadMode、最小 DB/Vault bootstrap、SecretRef、`config secret` 或 `mail.password_ref`，这些已经是基线；顶层 `src/config` 迁移已完成，下一步从内部职责拆分和诊断能力开始。**

推荐执行顺序：

1. 已完成：顶层结构迁移，`src/config/{mod,model,source,expand,loader,template,secret}.rs` 成为主实现，源码调用方已迁到 `crate::config`，`common::config` shim 已删除。
2. 内部职责拆分：`model.rs`、`source.rs` 和 `expand.rs` 已拆出；接下来进入错误模型、脱敏、集中校验等语义阶段。
3. 错误模型、脱敏与校验：占位符展开的原 `unwrap` 已收敛为 `ConfigError`，未解析/非法占位符值已脱敏；`mega_base` / `mega_cache` 的早期目录解析 `unwrap` 已移除；数据库连接/migration 初始化、Redis 初始化、BuckService 构造和 HTTP 监听地址解析/绑定已改为返回错误；首批 URL redaction、`SecretString`、`SecretRef` 默认输出脱敏、统一 `MEGA_*` source builder 和 `validate.rs` 已落地；继续收敛剩余加载路径 `expect`/`panic`，补更多配置规则和完整 source diagnostics。
4. 初始化与诊断：`config init` 首批已落地，已生成安全默认模板和 `mail.password_ref` 占位；`config validate` 已使用 `LoadMode::RawSources` 命令内解析，raw TOML、`MEGA_*` 未消费覆盖项 warning、文件级 source path 诊断、坏 env/profile 类型脱敏错误和首批修复建议已完成；继续补完整 RawSources/source diagnostics。
5. 样例/Profile/测试分层：测试配置生成器、Profile 合并语义、基础样例凭据治理和 CI 配置验证入口已完成首批；CI 已纳入 env diagnostics、坏 env/profile 类型单测、缺失 `mail.password_ref` 命令层脱敏单测，以及坏占位符、坏 SecretRef URI、坏 Redis URL scheme 的 CLI 失败 smoke；真实 Vault resolver 已覆盖缺失 secret 脱敏；继续扩展 CI 坏输入矩阵并迁移更多测试到隔离 helper。
6. 可选专项：只有在明确要让对象存储凭据进入 vault 时，才拆 `Storage::new` 为 DB-only → Vault → resolve secrets → full storage。
7. 独立阶段：受控热加载核心句柄已落地，可从 base/profile 文件路径构建候选配置，白名单日志字段可发布新快照，`mail.enabled` true→false 可关停 dispatcher，已运行 artifact GC 可热更新调度字段并关停，数据库/Redis/artifact GC 从关闭到启用/邮件重新启用和 SMTP 参数/凭据变化只报告需重启，SecretRef 变更不泄露不发布，候选失败时保留旧配置；`Storage`/`AppContext` 访问路径已接入共享 handle；订阅组件应用/失败回滚语义已落地；base/profile 文件 watcher 和 service watcher 生命周期已落地；日志 reload subscriber 已在 service 启动路径注册；仍需 Buck cleanup、邮件重配等其它真实消费端订阅接入。

**硬约束**（任何实现偏离都必须重新评审）：
- `Config → Storage(DB/object storage) → Redis → VaultCore` 的启动顺序决定了数据库、Redis、当前 object storage 凭据不能使用本项目 Vault SecretRef。
- `Config::new` 不能做异步 secret 解析；真实 secret 只能在 vault 就绪后由 resolver 读取。
- `config secret set/check` 和 `config validate --resolve-secrets` 必须继续使用最小 DB/Vault bootstrap，不能依赖 Redis、S3、HTTP 服务或完整 `AppContext`。
- `mail.password_ref` 是当前唯一已落地的配置侧 SecretRef；扩展字段前必须先证明消费点晚于 vault。
- Vault fail-closed、root token 脱敏/退役和 key 权限已完成；生产静态保护仍取决于部署侧 key material 托管、备份恢复和恢复演练。
- 拆分、错误模型、`config init`、Profile/测试分层、对象存储重构和热加载必须分阶段评审，每个源码阶段都要通过 AGENTS.md 的三项 gate。

### 实施前快速检查清单（建议每个阶段开始前核对）

- [ ] 已完整阅读“事实校准” + “当前实现状态速览表” + “硬约束”三部分。
- [ ] 已确认本次阶段**不会**尝试把数据库/Redis/当前 object storage 凭据做成 SecretRef。
- [ ] 已确认本次阶段**不会**在 `Config::new` 内部调用 vault 或做异步 secret 解析。
- [ ] 已确认不会重复实现 `LoadMode`、`config secret`、`SecretRef`、`mail.password_ref` 或 Vault fail-closed。
- [ ] 如果涉及 `mail`，已把工作聚焦在明文兼容治理、SecretString/redaction、source diagnostics 或 dispatcher 生命周期。
- [x] 已在 `validate.rs`/`config validate` 中加入首批 raw TOML 未消费/未知字段告警，覆盖当前 `[oauth]`、`[mail].smtp_tls`/`[mail].tls`、任意未知字段、嵌套 table 和数组内 inline table。
- [x] 已在 `validate.rs`/`config validate` 中加入首批 `MEGA_*` 未消费/被忽略覆盖项告警，覆盖未知 env 覆盖项、`MEGA_OAUTH__...`、`MEGA_MAIL__TLS`/`MEGA_MAIL__SMTP_TLS`，并避免输出 env 值。
- [x] 已提供 `ConfigSourceDiagnostics` 汇总 base/profile/env source warning，并通过 `config validate --deny-warnings` 支持 warning-as-error 门禁。
- [x] 已提供 `config validate --show-sources` 显式输出 base/profile/env 字段来源图和覆盖关系，输出只包含来源和字段路径，不包含配置值。
- [x] 已把坏环境变量类型包装为脱敏诊断，输出变量名、字段路径和期望类型，不输出原始 env 值。
- [x] 已将 `config validate` 切到 `LoadMode::RawSources`，配置解析发生在命令内，坏配置不会在子命令分发前被预加载拦截。
- [x] 已给首批 raw/env 未消费 warning 和坏 env 类型错误补入移除/替代字段等修复建议。
- [x] 已把 profile/TOML 类型错误包装为脱敏诊断，输出来源路径、字段路径和期望类型，不输出原始值。
- [x] 已把未解析/非法占位符值包装为脱敏诊断，输出来源、字段路径和修复建议，不输出原始值。
- [x] 已同步更新 `config/config.toml` 注释、README 加载优先级说明、以及本文档。
- [ ] 已确认顶层移动与调用方路径迁移已完成；后续错误语义、Profile、热加载分别单独提交，`config init` 模板治理不与这些阶段混合。
- [x] 已在 CI 中增加配置样例首批校验任务（基础 + 生成 + profile + env diagnostics + 坏 env/profile 类型单测 + 缺失 `mail.password_ref` 命令层脱敏单测 + 坏占位符/坏 SecretRef URI/坏 Redis URL scheme CLI smoke）；更多坏输入、完整 source diagnostics 和 SecretRef 权限矩阵仍待补。
