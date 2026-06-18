# Mail 实现方案分析

本文档记录 `monoengine` 中一级 `mail` 模块（`src/mail/`）的设计、当前实现状态、与 Config / Vault / Notification 的集成方案、运行时注入与启动顺序约束，以及分阶段落地计划。

本文档的编写要求、结构深度、分析维度、事实校准风格、阶段规划 rigor 与 `config.md` 完全一致（包括现状速览表、硬约束列表、多维评估表、风险与前置 gate、实施检查清单等）。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与 config 计划的强绑定**：`mail` 是 `config.md` 反复强调的”第一批可迁移凭据”的**唯一合格载体**。当前 `mail` 已补齐为真实、活跃、消费点晚于 `VaultCore` 的模块，并在 `AppContext::new` 中于 vault 之后解析 `mail.password_ref`、构造 `SmtpMailer` 并启动 `EmailDispatcher`；因此 `mail.password_ref` 已成为首个真实 `SecretRef` 落点。任何 mail 相关工作都必须严格遵守 config.md 中的引导循环约束（`Config → Storage(DB) → Vault`）、最小 bootstrap 要求、日志脱敏前置、fail-closed 等。

> **集成测试指引**：邮件模块的各项功能（Mailer 启动、Dispatcher 后台处理、Mailpit 验证、retry 机制）应通过 **`integration.md`** 中的 `integration_mail_dispatcher_mailpit` 场景进行端到端验证。

## 事实校准（2026-06-18）

> 本文档中的代码引用已对照当前 `src/` 重新核对。特别注意以下与早期 monoengine 移植状态不一致的事实：

1. **历史上 `mail` 不是一级模块，也未真正参与编译**。`src/email/mod.rs` 实现了完整的 `Mailer` trait、`NoopMailer`、`SmtpMailer`（基于 `lettre`），并引用了 `MailConfig`，但：
   - `main.rs` 从未声明 `mod email;`（更不用说 `mod mail;`）。
   - 旧配置模块中原本没有 `MailConfig` 结构体和 `mail: Option<MailConfig>` 字段；当前已迁入 `src/config/model.rs`。
   - `config/config.toml` 末尾存在 `[mail]` 段（含 `enabled`/`smtp_host` 等 + 额外 `smtp_tls`/`tls` 字段），但因无强类型承接而被 serde 静默忽略。
   - `src/notification/dispatcher.rs`（及其测试）直接 `use crate::email::...`，但因为模块树不包含 email/notification，实际无法编译/运行。

2. **`MailConfig` 当前已进入 monoengine 的编译 Config。** 结构位于 `src/config/model.rs`，字段包括 `enabled`、`smtp_host`、`smtp_port`、`username`、兼容期 `password`、推荐的 `password_ref`、`from`、`starttls`；`password` / `password_ref` 互斥，`mail.enabled = true` 时要求 `smtp_host` / `from` 非空。

3. **Notification 系统已接入主 crate，并在 mail 启用时启动 dispatcher**。`src/notification/{dispatcher, triggers, mod}.rs` + callisto 中的 `email_jobs`、`notification_event_types`、`user_notification_settings`、`user_notification_preferences` 等实体存在，触发器逻辑（`on_cl_comment_created` 等，尊重用户偏好）已从 mega 移植；`main.rs:18` 已声明 `mod notification;`，`AppContext::new` 在 vault 之后构造 mailer、创建 `EmailDispatcher` 并 `tokio::spawn`。失败投递已有有界 retry + dead-letter，dispatcher tick 已具备固定上限并发发送、结构化汇总日志、stale `sending` job 恢复和批次背压测试；邮件作业管理 API 首批已落地（admin-only list / stats / failed retry）；基础模板系统已落地，CL 评论邮件已通过模板渲染 subject/html/text 并默认 HTML 转义变量。当前仍需补齐真实 SMTP/Mailpit、更多业务触发器调用面和更完整运维面。

4. **Vault 约束对 mail 的决定性影响**：`password_ref` 的真实值读取**必须**发生在 `VaultCore` 就绪之后。当前 `AppContext::new` 顺序为 `Storage::new (DB + object_storage + Buck 校验) → init_connection(redis) → VaultCore::new → 解析 mail.password_ref → SmtpMailer + EmailDispatcher spawn → init_monorepo`。因此 mailer 的**构造时机**是 mail 模块设计的核心约束（详见「运行时注入与晚绑定构造」）。

5. **campsite / mega 功能参考**：
   - mega（`mono/src/email` + `notification/` + callisto `email_jobs` + 触发器）提供了完整的 outbox 模式 + dispatcher 后台 tick + 事件驱动 enqueue（cl.comment.created 等）+ 用户通知设置过滤。
   - campsite 主要作为用户/认证后端（`api_store_backend = "campsite"`），不直接提供 monoengine 的邮件发送能力，但其用户邮箱可作为邮件通知的收件人来源。
   - monoengine 的 callisto 实体、NotificationStorage 方法（`enqueue_email_job`、`fetch_pending_jobs`、`should_send` 等）已与 mega 对齐。

6. 行号与模块路径以当前代码为准。

## 当前实现状态速览表（激活后，2026-06）

| 能力 / 组件                  | 实现状态          | 关键事实与风险 |
|-----------------------------|-------------------|---------------|
| `MailConfig` 结构体 + 纳入 Config | **已激活** | 添加到 `src/config/model.rs`（`Option<MailConfig>`，`#[serde(default)]`），含默认值函数、反序列化测试、兼容期明文 `password` 与推荐 `password_ref`。与 mega 结构兼容（扁平 + 额外 toml 字段被忽略）。 |
| 一级 `mail` 模块 (`src/mail/`) | **已激活** | `mod mail;` 在 `main.rs` 声明。`src/mail/mod.rs` 包含 `Mailer` trait、`NoopMailer`、`SmtpMailer::new_with_password(...)` + 构建消息 + 发送逻辑 + 单元测试。旧 `src/email/` 降级为纯 re-export shim。 |
| Notification Dispatcher + 触发器集成 | **已接入编译** | `src/notification/dispatcher.rs` 及测试使用 `crate::mail`。触发器（triggers.rs）使用 NotificationStorage enqueue 逻辑（事件类型、用户偏好过滤）已存在；`main.rs:18` 已声明 `mod notification;`。 |
| 后台 dispatcher 启动 | **已在 mail 启用时启动** | `AppContext::new` 在 `VaultCore::new` 之后构造 `SmtpMailer`、创建 `EmailDispatcher` 并 `tokio::spawn(dispatcher.run(shutdown))`。SMTP 构造失败现在返回可诊断错误；发送失败会按 backoff 重新排队，并在达到阈值后转为 `failed` dead-letter。`EmailDispatcher` 当前每 tick 先恢复超过 `EMAIL_JOB_SEND_TIMEOUT_SECS = 900` 的 stale `sending` job，再拉取 `EMAIL_DISPATCH_BATCH_SIZE = 50` 条，并以 `EMAIL_DISPATCH_MAX_IN_FLIGHT = 8` 做有界并发发送，tick 结束输出 sent / retry / dead-letter / skipped / claim-missed 等结构化汇总。 |
| 晚于 Vault 的 mailer 构造 | **已落地** | `SmtpMailer::new` 本身是同步且轻量的，当前调用点在 `context/mod.rs:46-55`，严格晚于 `VaultCore::new`。 |
| SecretRef / `password_ref` 支持 | **已落地首批** | 当前 `MailConfig.password: Option<SecretString>` 仅为兼容期入口，`password_ref: Option<SecretRef>` 为推荐路径；两者互斥，且 `mail.password_ref` / `config secret mail.password` 只接受 `vault://secret/config/<profile>/mail/password#<field>` namespace。`AppContext::new` 在 vault 就绪后通过 resolver 解析 `password_ref` 并构造 SMTP mailer。 |
| 多种后端（SES、SendGrid 等） | **未实现** | 仅 SMTP + Noop。mega 体系中也以 SMTP 为主，未来可扩展 provider。 |
| 模板 / 富文本 / 附件 | **基础模板 + HTML/Text** | `src/mail/template.rs` 已提供轻量 `MailTemplate`，支持 `{{var}}` 渲染、HTML 变量默认转义和缺失变量脱敏诊断；CL 评论触发器已改为模板渲染 subject/html/text。`send_html(to, subject, html, text?)` 实现 alternative multipart。仍无高级模板引擎、i18n 或附件。 |
| 与 user_notification_* / 事件类型 的完整联动 | **实体+存储+触发器骨架存在** | callisto 实体 + NotificationStorage 方法 + triggers（cl.comment 等）已移植自 mega，dispatcher 常驻任务、mailer 注入和失败 dead-letter 基线已接入；admin-only 邮件作业管理 API 已支持按状态/用户/事件查询、状态统计和 failed job 手动重排；仍缺更多业务触发器调用面、更完整观测和运维控制。 |
| Profile / 热加载 / 集中校验对 mail 的支持 | **部分实现** | Profile、集中校验和 source warning 已接入 config 管线；热加载当前支持 `mail.enabled` true→false 关停 dispatcher。重新启用 mail、SMTP 参数和凭据变更仍要求重启或后续动态 mailer 重建设计，该边界已有 config reload restart-required 矩阵测试。 |
| 测试与 CI 覆盖 | **部分实现** | mail 自身有构造/消息验证测试和模板渲染/缺失变量/HTML 转义测试；dispatcher 有使用 Noop 的集成风格测试（需 DB + migration）；已覆盖 `password_ref` 解析失败脱敏、坏配置不 panic、`mail.enabled` 关停热加载、SMTP 参数/凭据重配只报告需重启、失败发送的 retry/dead-letter disposition、dispatcher 有界并发、单 tick 批次背压、stale `sending` 恢复、storage-level 并发 claim 竞争、邮件作业 list/stats/failed retry 管理原语，以及 CL 评论触发器模板渲染。仍缺真实 SMTP/Mailpit、长时间高水位背压压测和真实多进程 claim 竞争矩阵。 |

**已知加载/启动/安全风险点（必须在相应阶段消除，与 config.md 风险点重叠）**：
- mailer 或 dispatcher 若被移动到 Storage::new / vault 前路径，会违反 vault 就绪顺序；当前代码位置正确，但需防止后续回归。
- 兼容期明文 `password` 若由用户配置，仍需避免进入日志、错误、Debug 或 CI 输出。
- `config/config.toml` 必须保持只给 `password_ref` 占位，不写入示例明文密码。
- Notification 事件/用户设置的 upsert 逻辑在触发器中（非幂等迁移）。

## 总体设计

一级 `mail` 模块的目标是成为系统**通知能力的第一等公民**，同时作为 config 计划中 SecretRef 落地的“试点消费者”。

核心原则（直接继承自 config.md）：
- **晚绑定构造**：真实带凭据的 mailer 绝不在 `Config::new`、Storage 初始化、Redis 连接、Vault 就绪之前被创建。
- **Outbox 模式**：业务代码（触发器）只负责 `enqueue_email_job` 到 DB（`email_jobs` 表），由后台 `EmailDispatcher`（tick + claim + send + mark）负责投递。失败可重试。
- **尊重用户偏好**：通过 `user_notification_settings` / `user_notification_preferences` + `should_send` 过滤。
- **与 Config 管道深度集成**：`MailConfig` 走统一的 TOML + `MEGA_*` env + 占位符 + `SecretRef` 解析。
- **可观测与可诊断**：发送失败写 `error_message` + `retry_count`，不把明文密码或 secret 值写入任何日志/错误。

### 主要组件关系

```
Config (含 mail: Option<MailConfig>)
  |
  v (late, post-Vault)
SmtpMailer::new_with_password(...)  -->  Arc<dyn Mailer>
  |
  v
EmailDispatcher (持有 NotificationStorage + mailer)
  ^ tick (后台任务)
  |
NotificationStorage (enqueue / fetch_pending / mark / should_send)
  |
callisto::{email_jobs, notification_event_types, user_notification_* }
  ^
触发器 (on_cl_comment_created, 未来 on_issue_*, on_mr_* 等)
  ^ 由 ceres / api 层在事件发生时调用
```

`NotificationStorage` 目前通过裸 DB 连接构造（`NotificationStorage::new(Arc<db>)`），可从 `Storage` 或 `AppContext` 派生访问（未来可暴露更干净的 API）。

## 启动与注入链路（必须晚于 Vault）

推荐的正确顺序（对齐 config.md 的 `VaultBootstrap` / FullAppContext 区分）：

1. `Config::new`（只产出含兼容期 `mail.password` 或 `mail.password_ref` 的配置，不解析密码）。
2. `Storage::new`（DB + object storage 等，**不构造 mailer**）。
3. Redis init。
4. `VaultCore::new`（vault 就绪）。
5. （可选，最小 bootstrap 路径）`config secret ...` 相关命令在这里解析 mail 相关的 SecretRef 做检查。
6. 真正服务启动路径：解析 `mail.password_ref`（如存在），再构造 `SmtpMailer::new_with_password(...)`，得到 `Arc<dyn Mailer>`。
7. 构造 `EmailDispatcher::new_with_control(notif_stg, mailer, control)` 并 `tokio::spawn(dispatcher.run(shutdown))`。
8. 执行 `init_monorepo`，随后进入 HTTP/SSH/multi 服务分发。
9. 业务触发器开始 enqueue，dispatcher 处理 outbox 投递。

**严禁**在 Storage 构造阶段或 `VaultCore::new` 之前调用 `SmtpMailer::new`。当前 `AppContext::new` 中的调用点位于 vault 之后，顺序正确；后续若引入 SecretResolver，也必须保持在 vault 之后。

当前 `password_ref` 版本：`Config` 只保存 `SecretRef` 引用，resolver 在步骤 6 之后解析 `mail.password_ref`，再用解析后的值构造 `SmtpMailer`。若后续要支持运行期重配，再评审 mail 模块内部持有 `SecretResolver` + 缓存凭据的设计。

## 主要消费场景与触发器

- **Change List 评论**（`EVENT_CL_COMMENT_CREATED`）：cl 作者 + 所有 reviewer（排除 actor），尊重 `should_send`。
- 未来：Issue/PR 评论、合并、@提及、构建失败、buck 相关通知等（通过 `notification_event_types` 扩展）。
- 直接 API / 后台任务 enqueue（绕过触发器）用于系统告警、用户邀请等。
- `EmailDispatcher` 作为常驻后台任务处理 outbox，支持有界重试（`retry_count`、`next_retry_at`）和达到阈值后的 `failed` dead-letter。

所有 enqueue 都经过 `NotificationStorage` 的方法，业务层不应直接操作 `email_jobs` 表。

## 当前方案的优点（激活后）

- mail 成为一级模块，边界清晰，易于扩展 provider。
- 与 mega 功能对齐（outbox + dispatcher + 事件触发 + 用户偏好），复用 callisto 实体和存储逻辑。
- 已作为 SecretRef 试点落地，且调用构造时机位于 vault 之后。
- NoopMailer 使测试和“邮件未启用”场景零成本。
- Config 集成后，支持 `MEGA_MAIL__ENABLED=true` 等 env 覆盖和 `${base_dir}` 风格复用（虽 mail 配置中路径较少）。

## 现有 Vault / 引导约束（必须严格遵守）

与 config.md 「现有 vault 能力与关键约束」完全一致：

- mailer 构造**必须**晚于 VaultCore。
- `config secret set mail.password ...` 等运维命令必须使用**最小 DB/Vault bootstrap**（不能初始化 Redis、对象存储、完整 Storage 服务、HTTP 监听）。
- 在 `core_key.json` 加固完成前，mail password 进 vault 的收益仅限“不进 git/不进常规日志”。
- 任何在 Storage::new 或 redis init 阶段“触达”密码的行为都是违规的（即使当前是空字符串）。

因此，`MailConfig` 里的 `password` 只作为兼容期入口保留；生产配置应使用 `password_ref`，并由 vault 就绪后的 resolver 路径解析。

## Mail 模块的改进方案（一级模块 + SecretRef 就绪）

### 总体原则（与 config.md 一致）

- 拆分与迁移解耦（如果未来 mail 内部继续细分 provider）。
- 区分引导/早期 vs 可迁移凭据：password 是典型**可迁移凭据**（消费点天然晚于 vault）。
- secret 解析是 vault 就绪后的独立阶段，不在 Config 反序列化内。
- `mail` 相关运维命令复用 config 的 `LoadMode` + 最小 bootstrap 能力。
- Vault 加固是前置。
- 基础配置与测试配置分离。

### 建议目录结构（src/mail/ 作为一级模块）

```
src/mail/
├── mod.rs            # 对外入口：trait Mailer、Noop、Smtp 工厂、re-export
├── smtp.rs           # SmtpMailer 实现 + 传输构建
├── noop.rs           # （或直接在 mod）
├── providers/        # 未来：ses.rs、sendgrid.rs、console.rs 等
│   └── mod.rs
├── template.rs       # 已有轻量模板渲染（subject/html/text，HTML 变量默认转义）
├── error.rs          # MailError（带脱敏的诊断信息）
└── testing.rs        # MockMailer、capturing mailer 用于测试
```

`MailConfig` 当前位于 `src/config/model.rs` 并由主 `Config` 持有。若未来拆分 provider 子配置，可再评估让 mail 模块暴露更细粒度的配置类型。

### SecretRef 迁移策略（已落地首批，对齐 config.md 阶段 5）

1. 已完成 mail 激活（MailConfig 入 Config + 一级 mail 模块 + notification 引用修复 + 晚构造接线）。
2. 已支持 `password`（明文，带 deprecation warning）与 `password_ref: Option<SecretRef>` 互斥。
3. 已在消费端（dispatcher 启动点）于 vault 就绪后解析 `password_ref` 并构造 `SmtpMailer`。
4. 剩余工作是给明文 `password` 制定退场节奏，并补充运行期重配时的 resolver 缓存失效/失败回滚设计。

`config secret set/check` 已支持 `mail.password` 路径（使用最小 bootstrap），并拒绝不在 `config/<profile>/mail/password` namespace 下的引用。

### 后台 Dispatcher 启动与生命周期

- 在 `commands/service/http`（或 multi）等 FullAppContext 路径中，在 vault 之后、HTTP 监听之前，检查 `config.mail.as_ref().map(|m| m.enabled).unwrap_or(false)`。
- 若启用：构造 mailer → 取 NotificationStorage → 注册 reload control → `EmailDispatcher::new_with_control(...)` → spawn task with shutdown token。
- 优雅关闭：通过 CancellationToken 停止 tick。
- 多实例注意：outbox + `try_claim_job` 提供基础互斥，stale `sending` 恢复可处理 dispatcher 崩溃后遗留的发送中 job；生产仍可按部署形态增加分布式锁、单实例约束或更明确的租约字段。

### 多环境与 Profile

继承 config 的 profile 机制：`config.mail` 里的 host/port 可被 profile 覆盖，`password_ref` 必须包含受控 namespace（如 `vault://secret/config/prod/mail/password#value`）。

### 可扩展性（Provider）

`Mailer` trait 保持最小。未来 `MailService` 或工厂根据 `mail.provider`（或子表 `mail.smtp` / `mail.ses`）选择实现。Noop 始终可用作降级。

## 推荐加载与构造流水线（mail 视角）

```
ConfigLoader + Config::new (含未解析 SecretRef 的 mail)
  -> ... (Storage, redis)
  -> VaultCore
  -> SecretResolver (如果 password_ref 存在)
  -> 解析 mail 凭据（或直接用明文）
  -> SmtpMailer::new_with_password(带真实 creds)
  -> EmailDispatcher + spawn
  -> 触发器可安全 enqueue
```

`config mail validate --resolve`（或复用 `config validate --resolve-secrets`）应能使用最小 bootstrap 检查 mail 配置是否可发送测试信。

## 迁移步骤（分阶段，绑定 config 阶段）

> **与 config.md 的强绑定（2026-06-18 更新）**：mail 的后续工作仍依赖 config 的热加载、profile 和 diagnostics 演进。原先阻塞 mail 的构造失败诊断、`SecretRef` + resolver 基础设施和 `password_ref` 首个消费端已经完成首批落地。

**阶段 0（已完成）**：激活一级 mail + MailConfig 入 Config + 修复 notification 引用 + 清理 shim。

**阶段 1（已接入，基线已加固）**：在 service 启动路径中 late-construct mailer 并 spawn dispatcher 已落地；构造失败静默忽略已改为可诊断错误；失败发送已具备有界 retry + dead-letter disposition；dispatcher 已具备每 tick 有界并发发送、结构化汇总日志、stale `sending` 恢复、单 tick 批次背压测试和 storage-level 并发 claim 竞争测试。剩余是完善真实 SMTP/Mailpit 集成、长时间高水位背压验证和真实多进程 claim 竞争矩阵。

**阶段 2（已完成首批）**：与 config SecretRef 基础设施联动。`MailConfig` 已支持 `password_ref`，resolver 解析路径已在 `AppContext::new` 中落地，`config secret set/check` 已支持 mail password 引用。剩余是继续治理兼容期明文 `password` 的退场策略。

**阶段 3（已完成首批管理面 + 基础模板）**：邮件作业管理 API 已先落地 admin-only `email-jobs/list`、`email-jobs/stats`、`email-jobs/{id}/retry`，支持查询 outbox、按状态统计和将 `failed` job 重新排回 `pending`；`src/mail/template.rs` 已提供基础模板渲染，CL 评论邮件已改为模板生成 subject/html/text 并默认 HTML 转义变量。剩余是 Provider 扩展（至少一个额外后端）、高级模板能力（i18n、模板 registry、附件等）、管理员可配置的退信/限流，以及更完整的管理端编辑/审计能力。

**阶段 4**：Profile 感知的 mail 配置、热加载支持（当前已支持 `mail.enabled` true→false 关停 dispatcher；重新启用、from/SMTP/凭据变更仍需重启或后续动态 mailer 重建设计，且该重启边界已有 reload 测试覆盖）、更强的可观测（发送指标、链路追踪）。

**阶段 5**：完整测试矩阵（坏 SMTP、解析失败、权限失败、大量 pending job 背压、用户偏好全关场景）、CI 中增加真实邮件发送干跑（或 mailpit 等 test container）、文档同步（README、部署指南）。

### 前置依赖矩阵（2026-06-14 更新）

| mail 阶段 | 主要工作 | 对 config 的依赖 | 对 vault 的依赖 | 对 notification 的依赖 |
|----------|--------|------------|-----------|-----------------|
| **0** (已完成) | MailConfig + 一级模块 | `src/config/model.rs` 包含 MailConfig 字段 | 无 | 无 |
| **1** (基本完成) | 晚构造 + dispatcher + 并发/观测/恢复基线 | 构造失败诊断已接入 config 脱敏错误路径 | 无 | 支持通知基础，dispatcher 已有有界并发、retry/dead-letter disposition、stale sending 恢复和 claim 竞争测试 |
| **2** (已完成首批) | password_ref 迁移 | SecretRef + resolver 已落地 | 支持通过 resolver 读取 | notification 可使用 password_ref |
| **3-5** | Provider + 模板 + 可靠性 | 依赖 config 的 profile、validate、reload 后续增强 | 部分依赖 vault 的完整加固 | 作业管理 API 首批已落地，继续支持后续功能扩展 |

**关键前置依赖：**
- `password_ref` 首个消费端已经落地；后续不要重复实现 resolver/mail 接入。
- 运行期重新启用或 SMTP/凭据重配已在 config 热加载阶段明确为需重启字段；若要改为热生效，仍缺动态 mailer 重建与失败回滚设计。

贯穿：每次变更同步更新 `config/config.toml` 示例、`mail.md`、`config.md` 相关章节、notification 触发器新增事件时的文档。

## 风险与约束

- **构造时机是不可违反的硬约束**。违反即退回“早期依赖”，无法使用 SecretRef。
- **outbox + claim 模型的并发与重试语义**需与业务仔细评审（重复发送风险 vs 丢失风险）。当前已用 `try_claim_job` 和 stale `sending` 恢复覆盖基础互斥与崩溃恢复，但真实多进程场景仍需 Mailpit/DB 黑盒矩阵验证。
- **用户邮箱有效性**：不应在 monoengine 内部做硬校验；由触发器/入队时 best-effort，失败由 dispatcher 记录。
- **大量邮件导致 DB 压力**：fetch 批次、claim、索引设计需关注（已有 `email_jobs` 表）。
- **跨项目一致性**：与 mega 的 callisto 实体、NotificationStorage API、事件 code 命名保持兼容，避免分叉。
- **campsite 用户源**：当 `api_store_backend = "campsite"` 时，收件人邮箱可能来自外部，发送前最好有“用户是否允许邮件”二次确认（已在 should_send 中）。
- 其他与 config.md 相同：core_key 加固、日志脱敏、env 可见性、最小 bootstrap、测试配置隔离等。

## 改进方案多维评估小结

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | 高。把 mail 作为一级模块 + 第一个 SecretRef 试点，完美承接 config.md 的引导循环分析和阶段 5 已落地前置要求。outbox 模式在 mega 中已验证可行。 |
| **可行性** | 高。lettre 已 vendored，实体和存储逻辑已存在，唯一难点是“晚构造 + 最小 bootstrap”在 CLI 和 AppContext 中的落地，需要与 config 的 LoadMode 改造协同。 |
| **完整性** | 中高。覆盖了 trait、SMTP 实现、outbox、触发器骨架、与 Config/Vault 的集成点，并已有作业管理 API 和基础模板基线。补强项：多 provider、高级模板/附件、管理 UI、热加载白名单、完整 dispatcher 生命周期管理。 |
| **安全性** | 强（设计中）。明确把凭据构造推迟到 vault 之后、要求脱敏、与 SecretRef 对齐、Noop 安全降级。实现时必须把 vault_core 的泄露问题一起解决。 |
| **功能正确性与接口兼容性** | 良好。Mailer trait 简单稳定；与 notification 的集成点（dispatcher 构造参数）清晰；与 callisto 实体对齐。shim 保证平滑过渡。 |
| **数据流与控制流** | 正确。严格遵循 config.md 画的依赖顺序图。enqueue 只在触发器，send 只在 dispatcher tick，构造只在 post-vault。 |
| **性能与效率** | 可接受。outbox 解耦了业务线程与 SMTP I/O；批次拉取 + claim 提供基础并发控制。未来可加并发发送 worker 池。 |
| **可靠性与容错** | 改进空间大。当前有 retry_count/next_retry_at 字段，失败发送已按固定 backoff 重排并在达到阈值后进入 failed dead-letter。仍需补充告警、指数退避、更完整观测和并发策略。claim 失败时的幂等性需继续保证。 |
| **兼容性与互操作** | 良好。与 mega 共享实体和部分逻辑；Config 管道复用；未来 provider 扩展点清晰。 |
| **可扩展性与可维护性** | 良好。一级模块 + trait + 目录规划（providers/）为扩展留了空间。和 config 模块的演进路径绑定良好。 |
| **合规性与标准符合性** | 良好。outbox 模式、用户偏好尊重、SecretRef 路径、对日志脱敏的要求都符合现代邮件与凭据管理实践。 |

## 小结

通过已完成的激活与后续 config 联动（MailConfig 入 Config、`src/mail/` 成为一级模块、notification 引用修复、`password_ref` 解析和 dispatcher 启动接线），monoengine 已拥有可编译、可测试、可作为“真实后置消费者”的 mail 能力。

后续工作应严格按本计划 + config.md 的阶段划分进行：继续完善真实 SMTP/Mailpit 集成、长时间高水位背压、真实多进程 claim 竞争和更完整运维面；如需支持运行期重新启用或 SMTP/凭据重配，必须先设计动态 mailer 重建与失败回滚语义；最后再持续扩展 provider、高级模板和运维能力。

所有 mail 相关的实现、文档、测试、配置示例都必须与 config 模块的拆分、CLI LoadMode、最小 bootstrap、日志脱敏、core_key 加固等前置 gate 保持同步。任何试图在 Storage::new 或 `VaultCore::new` 之前构造带真实凭据 mailer 的尝试都必须被视为架构违规。

实施前请完整阅读本档 + `config.md` 的「事实校准」「当前实现状态速览表」「硬约束」「secret 解析的依赖顺序」和「实施前快速检查清单」。

---

**参考**：
- mega 项目：`mono/src/email/mod.rs`、`mono/src/notification/{dispatcher,triggers}.rs`、common config 中的 `MailConfig`、jupiter callisto `email_jobs` 等实体及 NotificationStorage 实现。
- campsite：作为用户源后端，为邮件通知提供潜在的收件人邮箱与偏好数据源。
- monoengine 当前：`src/mail/mod.rs`、`src/config/model.rs`（MailConfig）、`src/notification/dispatcher.rs`、callisto 相关实体、`config/config.toml` 中的 `[mail]` 示例。
