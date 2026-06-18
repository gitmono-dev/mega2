# Notification 实现方案分析

本文档记录 `monoengine` 中一级 `notification` 模块（`src/notification/`）的设计、当前实现状态、与 Config / Mail / Vault / Storage / 业务触发器的集成方案、运行时注入与后台任务启动顺序约束，以及分阶段落地计划。

本文档的编写要求、结构深度、分析维度、事实校准风格、阶段规划 rigor、多维评估表、硬约束列表、实施检查清单等与 `config.md`（及配套的 `mail.md`）完全一致。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与 config 和 mail 计划的强绑定**：Notification 是系统事件驱动用户通知的核心能力（目前主要通过 email 渠道）。它**严重依赖** mail 模块作为 email 投递后端（mail.md 和 config.md 阶段 5 的第一个真实 SecretRef 消费者）。邮件通知的 dispatcher 构造时机必须晚于 VaultCore 和 mailer 就绪。Enqueue（触发器）可在 DB 可用后较早发生，但实际投递（尤其是带凭据的渠道）必须遵守 `Config → Storage(DB + object storage) → redis → VaultCore → mail resolver/SMTP mailer → EmailDispatcher 启动 → init_monorepo → service dispatch` 的顺序。未来多渠道（in-app、slack 等）可能引入更多 vault secret。Notification 后台任务的启动属于 FullAppContext 路径，不能在 config secret 等最小 bootstrap 命令中被强制初始化。

> **集成测试指引**：通知系统的各项功能（触发器、偏好查询、dispatcher 分发、多渠道投递）应通过 **`integration.md`** 中的 `test_notification_triggers` 场景进行端到端验证，确保邮件渠道与 mail 模块的集成正常。

## 事实校准（2026-06）

> 本文档中的代码引用已对照当前 `src/` 重新核对。需特别注意以下与 mega 上游及早期移植草案不一致的事实，后文据此修正：

1. **Notification 代码已作为活动模块接入主 crate**。`src/notification/{mod.rs, dispatcher.rs, triggers.rs}` 已部分从 mega 移植：
   - `dispatcher.rs` 实现了 `EmailDispatcher`（依赖 `crate::mail::Mailer` + `NotificationStorage`，处理 `email_jobs` outbox，支持 claim、可配置 retry/dead-letter、stale `sending` 恢复、mark sent/failed/skipped 和可配置批次/并发 tick）。
   - `triggers.rs` 实现了 `on_cl_comment_created`（使用 NotificationStorage 进行 event type upsert、should_send 过滤、enqueue_email_job）。
   - `mod.rs` 简单 re-export `EmailDispatcher`。
   - `main.rs:18` 已声明 `mod notification;`，`AppContext::new` 在 vault 之后、`init_monorepo` 之前构造 `EmailDispatcher` 并 `tokio::spawn`。触发器是否已被业务代码广泛调用仍需逐条接入。

2. **核心存储逻辑放在 jupiter 层**。`src/jupiter/storage/notification_storage.rs` 实现了完整的 NotificationStorage（对 callisto 实体的 CRUD + 业务逻辑：upsert_user_settings、set_global_enabled、should_send（结合 system_required / default_enabled / user prefs）、enqueue_email_job、fetch_pending_jobs、try_claim_job、mark_* 等）。这与 mega 的 jupiter 实现几乎一致，但 notification 模块本身并未在此之上提供高层 Service 抽象。

3. **Callisto 实体已完整移植**（与 mega 共享 schema）：
   - `notification_event_types`（code, category, description, system_required, default_enabled）。
   - `user_notification_settings`（username, email, enabled, delivery_mode, preferred_locale）。
   - `user_notification_preferences`（username + event_type_code, enabled）。
   - `email_jobs`（outbox 表：username, to_email, event_type_code, subject, body_html/text, status, error_message, retry_count, next_retry_at, sent_at 等）。
   - 关系已定义（email_jobs 属于 event_types 等）。

4. **当前仅 email 渠道，且依赖 mail 模块**。Dispatcher 硬依赖 `mail::Mailer`（`send_html`）。根据 `config.md` 和 `mail.md`，mail 已完成“一级模块 + MailConfig 入 Config + vault 就绪后构造 + dispatcher 启动”的基础激活，因此 notification 的 email 渠道已经具备作为合格后置消费者的启动位置。config.md 明确把 mail.password 作为首批 SecretRef 的前提，而 notification 的 email 投递是 mail 的主要下游。

5. **触发器和事件注册不完整**。仅 `EVENT_CL_COMMENT_CREATED` 有实现和测试（cl 作者 + reviewers，排除 actor，尊重 prefs）。mega 中有更多事件潜力（issue、pr、@mention、build 结果等），但 monoengine 业务层（ceres）尚未广泛调用这些触发器。事件类型目前靠触发器首次使用时 upsert（非迁移 seeding）。

6. **用户偏好 API 首批已落地；管理员邮件作业、模板与事件类型 API 首批已落地（与 mega 仍有差异）**。mega 的 `ceres/src/model/notification.rs` 定义了 `NotificationEventTypeInfo`、`UserNotificationConfig`、`UpdateUserNotificationConfig` 等 DTO（带 utoipa），用于用户管理通知偏好。monoengine 当前已有 admin-only 邮件作业 API，可查询 `email_jobs`、查看状态统计、将 `failed` job 重新排回 `pending`、清理旧终态 job，并查看、下载、删除或按保留期清理旧终态 job 的 outbox 附件（附件 prune 可按 `username` / `event_type_code` 收窄）；dispatcher 也可通过 `mail.attachment_prune_*` 自动清理旧终态附件；admin-only 模板 API 已支持审计内置/外部模板、覆盖关系、来源路径，按管理员提供变量预览渲染，并向 `mail.template_dir` 持久化 upsert 外部 TOML 模板后热替换 registry；admin-only 事件类型 API 已支持列出 `notification_event_types` 并按 code upsert category/description/system_required/default_enabled；用户自助 API 已提供 `GET /user/notification/preferences`（当前用户 settings + event preference effective 状态）、`PUT /user/notification/preferences`（更新 global enabled、delivery_mode、preferred_locale、批量 event preferences）和 `PUT /user/notification/preferences/{event_type_code}`（更新当前用户单个非 system-required event preference）。邮件模板 registry 已支持由 `mail.template_dir` 在启动期加载 TOML 覆盖项。仍缺更多业务触发器和更完整运维面。

7. **Campsite 相关**：campsite 项目主要是 TS/Next.js monorepo（packages/ui、editor、config 等），包含一些前端通知 UI 组件（如 AvatarNotificationReasonClip）和 slack.ts 配置（可能用于外部通知渠道）。它主要作为用户/认证后端（campsite_api_domain、api_store_backend），为 notification 提供用户邮箱和身份数据，但核心事件驱动 + outbox + 偏好逻辑在 Rust 引擎侧（mega/monoengine 共享的 callisto + jupiter）。未来 slack 渠道可考虑从 campsite 的 slack 集成模式扩展。

8. **启动与 Vault 约束**：与 config.md 完全一致。当前代码已经在 `AppContext::new` 中按 `Storage → Redis → VaultCore → mailer_from_config + EmailDispatcher spawn → init_monorepo` 的顺序启动 email dispatcher。Enqueue 理论上可在 DB 就绪后发生（触发器只依赖 NotificationStorage），但投递必须在 vault + mail 之后。NotificationStorage 本身不持凭据，但 EmailDispatcher 持 mailer（SMTP password 已可使用 SecretRef）。

9. 行号与模块路径以当前（读取时）代码为准。早期 mega 移植中的部分行号已更新。

## 当前实现状态速览表（2026-06）

| 能力 / 组件                     | 实现状态          | 关键事实与风险 |
|--------------------------------|-------------------|---------------|
| `src/notification/` 作为一级模块 | **已激活** | 有 mod/dispatcher/triggers，`main.rs:18` 已声明 `mod notification;`。触发器仍需继续接入业务关键路径和 API 表面。 |
| EmailDispatcher + outbox 处理   | **运行时已 spawn（mail 启用时），基线已加固** | 依赖 `mail::Mailer`，实现 claim/retry/dead-letter/mark 逻辑，tick 每 2s。`AppContext::new` 在 vault 之后构造并 spawn；构造失败会返回可诊断错误。Dispatcher 已有可配置批次/并发限流、可配置指数退避 retry/dead-letter 策略、结构化 tick 汇总、stale `sending` 恢复、批次背压测试和真实 SMTP/Mailpit 正路径测试。当前缺口是 Mailpit 故障矩阵、多实例矩阵、更完整 lifecycle/metrics。 |
| 触发器（on_cl_comment_created 等） | 部分实现         | 实现了 CL 评论场景（作者+reviewers，prefs 过滤，enqueue），邮件内容通过 `mail::template::MailTemplateRegistry` 按收件人的 `user_notification_settings.preferred_locale` 渲染并默认转义 HTML 变量，registry 已支持 locale fallback 和启动期 TOML 模板覆盖，且 CL 评论已有 `zh-CN` 本地化模板。有单元测试。其他事件（issue、build 等）缺失或仅在 mega 中有原型。 |
| NotificationStorage（jupiter 层） | 已实现（完整）    | 位于 `src/jupiter/storage/notification_storage.rs`，封装所有实体访问 + should_send 业务逻辑 + email job 生命周期。被 triggers 和 dispatcher 直接使用。 |
| Callisto 通知实体               | 已完整移植        | email_jobs、notification_event_types、user_notification_settings、user_notification_preferences（及关系）与 mega 一致。 |
| 用户偏好与事件类型管理          | 存储层存在，用户 API + admin 事件类型 API 首批落地 | 支持 upsert、should_send、list prefs 等。用户自助 API 已支持查询当前用户 settings/event prefs/effective 状态，并更新 global enabled、delivery_mode、preferred_locale、批量或单个 event preference；admin-only 事件类型 API 已支持 list/upsert。仍缺更完整 mega DTO 兼容面和审计能力。触发器仍会在首次使用时 upsert 核心事件类型。 |
| 与 mail 模块的集成              | **已就绪（前提）** | Dispatcher 构造需要 post-vault 的 mailer（见 mail.md 和 config.md 阶段 5）。当前 mail 激活后，类型上可链接，但时机未在启动路径中强制。 |
| 后台任务启动与生命周期          | **已基础接入，需完善** | `AppContext` 持有 `notification_shutdown: CancellationToken`，并在 mail 启用时 spawn dispatcher。仍需完善 graceful shutdown 协调、失败诊断、退避和多实例语义。 |
| 多渠道支持（email 之外）        | **仅规划**        | 当前只有 email 渠道（通过 mail）。in-app（可能复用 chat/message 系统）、webhook、slack（参考 campsite slack.ts）等均未设计。 |
| SecretRef / 渠道凭据            | **仅规划**        | Email 渠道的 password 走 mail 的 SecretRef（config 阶段 5）。未来 slack token 等需类似 vault 集成。 |
| API 模型与用户设置端点          | 部分（管理面 + 用户偏好首批） | callisto 实体完整；admin-only 邮件作业 list/stats/failed retry/prune/attachment metadata/download/delete/retention prune API 已落地，且附件 retention prune 可按 username/event type 收窄；dispatcher 已支持配置化自动附件保留清理；admin-only 模板 list/preview/upsert API 已落地，可审计内置/外部模板、来源路径和覆盖关系，并持久化外部 TOML 覆盖项；admin-only 事件类型 list/upsert API 已落地。用户端 `/user/notification/preferences` 首批已支持列表、settings 更新、批量 preference 更新和单 event 更新；仍缺更完整 mega DTO 兼容面与审计/批量运维控制。 |
| 与 Config / 全局设置            | 弱集成            | 目前偏好全在 DB per-user。Config 中无 notification 相关全局开关（未来可能有 rate limit、默认 delivery_mode 等）。 |
| Profile / 热加载 / 集中校验     | **未实现**        | 依赖 config 模块能力。通知事件类型或全局模板可能需要校验。 |
| 可靠性（重试、DLQ、可观测）     | 基线已加固        | email_jobs 有 retry_count/next_retry_at/status/error_message。Dispatcher 已有可配置指数退避 retry、failed dead-letter、stale `sending` 恢复、可配置批次/并发限流、自动附件保留清理和结构化汇总日志；admin API 可查询/统计/重排 failed job，并查看、下载、删除或按保留期清理旧终态 job 的持久化附件，附件清理可按 username/event type 收窄。仍缺指标、tracing 上下文、告警和真实多实例矩阵。 |

**启动/加载关键路径上的已知危险点（各阶段必须收敛，与 config.md/mail.md 重叠）**：
- Dispatcher / mailer 若被移动到 Storage::new 或 `VaultCore::new` 之前，会违反 vault 顺序；当前代码位置正确，但需要防回归。
- 邮件正文/收件人（PII）出现在日志或错误中。
- 触发器在事件类型不存在时 upsert（竞态、迁移不一致风险）。
- NotificationStorage 目前直接暴露 DB 连接，业务层可绕过偏好检查。
- 未来渠道 secret（非 email）若提前加载，会违反 vault 就绪顺序。
- chat_migrate 等遗留路径处理旧 message_notification，与新 email_jobs 模型并存。

## 总体设计

一级 `notification` 模块的目标是成为**系统事件驱动通知的统一中枢**，支持多渠道可靠投递（email 当前主力，未来 in-app、slack、webhook 等），同时严格遵守用户偏好与同意，同时作为 mail 模块的主要下游消费者。

核心原则（直接继承自 config.md + mail.md）：
- **Enqueue 早、Delivery 晚**：业务触发器可在 DB 就绪后 enqueue（仅依赖 NotificationStorage）。实际投递（构造带凭据的渠道 + dispatcher）必须在 VaultCore + mailer（或其他渠道 secret）就绪之后。
- **Outbox + At-least-once（尽力一次）**：email_jobs 作为可靠投递的 outbox；claim 提供基础并发保护；失败可重试但需幂等。
- **用户同意优先**：通过 user_notification_settings（全局 enabled + delivery_mode + email + preferred_locale）和 user_notification_preferences（per-event override） + `should_send` 实现。system_required 事件可强制。
- **渠道抽象**：当前 EmailDispatcher 硬绑定 mail。未来需 `NotificationChannel` trait（send(notification)），由多渠道 dispatcher 协调。
- **事件注册与扩展**：notification_event_types 作为 registry。触发器负责 ensure + enqueue；新事件类型应通过 API 或迁移注册。
- **与 Config/Vault/Mail 深度集成**：通知配置（若有全局项）走 Config 管道；渠道凭据（email password 经由 mail，未来 slack token 等）走 SecretRef + resolver；构造点必须 post-vault。
- **可观测与诊断**：投递结果写回 jobs 表；错误脱敏（不泄露邮箱内容到不必要日志）；支持 tracing。

### 主要组件关系（设计目标）

```
业务事件 (CL 评论、Issue 更新、@mention、Build 完成...)
  |
  v
触发器 (on_cl_comment_created, on_xxx) 
  |  1. ensure_event_type
  |  2. 计算 recipients + should_send (via NotificationStorage)
  |  3. enqueue_email_job (或其他渠道 outbox)
  v
NotificationStorage (jupiter 层，封装 callisto 实体)
  |
  v
NotificationService / Coordinator (一级 notification 模块核心)
  |  持有 channels: Vec<Arc<dyn NotificationChannel>>
  |  管理 dispatcher tasks
  v
渠道实现:
  - EmailChannel (包装 mail::Mailer + email_jobs outbox)
  - InAppChannel (未来，写入 conversation/message 或独立表)
  - SlackChannel (未来，参考 campsite slack 集成 + vault secret)
  ...
  |
  v
后台 Dispatcher(s) (tick + claim + deliver + mark + retry)
```

`AppContext` 最终应持有或能提供 `NotificationService`（在 vault + mail 就绪后注入 mailer 等依赖）。

## 启动与注入链路（必须晚于 Vault + Mail）

必须严格遵循 config.md 的依赖顺序：

1. `Config::new`（加载全局配置，可能包含未来 notification 全局开关）。
2. `Storage::new`（DB 就绪，NotificationStorage 可构造，enqueue 理论上可用）。
3. Redis。
4. `VaultCore::new`。
5. （如果 mail 启用）构造真实 `Mailer`（post-vault，按 mail.md）。
6. 构造并 spawn 当前 `EmailDispatcher`；未来可替换为 `EmailChannel`（或其他渠道） + `NotificationService`。
7. 执行 `init_monorepo`，随后进入 HTTP/SSH/multi 服务分发。
8. 业务触发器现在可安全 enqueue；dispatcher 开始投递。

**严禁**：
- 在 Storage::new 或 `VaultCore::new` 之前构造带真实 mailer 的 dispatcher。
- 在 `config secret` 等最小 bootstrap 路径中启动完整 notification 任务（会拉起 mail 等）。

对于仅 enqueue 的场景（触发器），只要 DB 可用即可；delivery 任务必须在 FullAppContext 路径的后期启动。

## 主要消费场景

- **代码审查相关**：CL 评论（当前唯一完整实现）、未来 PR/issue 评论、reviewer 变更、合并通知。
- **@提及与订阅**：在 conversation/issue 中 @user 时 enqueue。
- **构建与 CI**：build 完成、buck 上传相关、orion 任务状态（参考 orion-server 的通知使用）。
- **用户与群组**：新成员、权限变更、CLA 相关。
- **系统告警**：由后台任务或 admin 直接 enqueue（绕过用户 prefs？需 system_required 标记）。
- **用户自助**：通过 API 更新 global enabled / delivery_mode / per-event prefs；查询事件类型列表。

所有 enqueue 应经过 `NotificationStorage`（或高层 Service），避免业务代码直接操作实体。

## 当前方案的优点（代码存在状态）

- Outbox 模式已在 mega 中验证可靠，解耦了业务线程与投递 I/O + 重试。
- 偏好模型细粒度（global + per-event），支持 system_required 强制通知。
- 存储层逻辑丰富（should_send 组合规则、claim 原子性、retry 时间窗口）。
- 与 callisto 实体共享（mega/monoengine 可互操作数据）。
- Email 渠道已结构化对接一级 mail 模块（为 SecretRef 试点做好准备）。

## 现有约束与 Vault / 引导问题（必须显式承接）

直接继承 config.md 和 mail.md 的硬约束：

- **投递渠道的凭据消费点必须晚于 vault**。Email 依赖 mail.password（未来 SecretRef）；未来 slack/push 渠道很可能需要 token/key，必须走相同 resolver 路径。
- **config secret 家族命令** 必须用最小 DB/Vault bootstrap 操作 notification 相关 secret（如果有），不能初始化完整 dispatcher 或 mailer。
- **PII 与同意**：to_email、body 包含用户数据；必须通过 prefs 尊重 enabled 状态；发送前最好有额外审计。
- **当前 NotificationStorage 直接暴露 DB**：业务层可 bypass prefs（触发器目前做了正确检查，但不是强制）。
- **事件类型一致性**：跨部署的 event code 必须稳定；upsert 策略在并发/迁移时有风险。
- **与 mail 模块的强依赖**：mail 未就绪（或未 late 构造），email 通知就无法投递。config.md 阶段 5 的 mail 工作是 notification email 渠道的前置。

## Notification 模块的改进方案（一级模块化 + 多渠道 + 可靠投递）

### 总体原则

- 提升为一级模块（已在 `src/notification/` 目录，但需确保在 main.rs 声明、提供干净公共 API、与 AppContext 良好集成）。
- 渠道抽象（当前 email 硬编码，需 trait 化）。
- 依赖注入晚绑定（mailer 等在 vault 后提供）。
- Outbox 泛化（email_jobs 是 email 特定；未来可有 unified notifications 表 + per-channel delivery jobs，或保持 email_jobs 专用 + 其他渠道独立 outbox）。
- 与 Config 管道协作（全局通知开关、默认 delivery_mode、速率限制等若出现，走 Config + validate）。
- SecretRef 就绪（渠道凭据通过 resolver 解析，类似 mail）。
- 可测试性（MockChannel、capturing dispatcher、隔离测试存储）。

### 建议目录结构（src/notification/ 作为一级模块）

```
src/notification/
├── mod.rs                 # 对外入口：NotificationService、Channel trait、re-exports、启动 helper
├── service.rs             # NotificationService（持有 storage + channels，start_dispatchers）
├── dispatcher.rs          # 通用或 email 专用 dispatcher 协调器（tick、claim、deliver）
├── channels/
│   ├── mod.rs
│   ├── email.rs           # EmailChannel (wraps mail::Mailer + email_jobs 逻辑)
│   ├── inapp.rs           # 未来：InAppChannel (写入 message/conversation 或独立 in-app 表)
│   └── slack.rs           # 未来：SlackChannel (使用 vault secret，参考 campsite slack 模式)
├── events.rs              # 事件注册表、ensure_event_type、触发器注册
├── triggers.rs            # 具体业务触发器（on_cl_comment_created 等，可移部分到 events）
├── preferences.rs         # 用户偏好查询/更新封装（对 storage 的高层包装）
├── storage.rs             # （可选）NotificationStorage 的轻量抽象或 facade（当前直接用 jupiter 的）
├── config.rs              # （可选）NotificationConfig（若未来有全局配置项，从主 Config 提取）
├── error.rs               # NotificationError（脱敏、渠道错误分类）
└── testing.rs             # MockChannel、TestNotificationService、enqueue capturer
```

主 Config 可持有可选的 `notification: Option<NotificationGlobalConfig>`（例如全局启用、默认渠道列表），但核心用户偏好仍在 DB。

### 与 Mail / Vault / Config 的集成要点

- Email 渠道必须在 mail 就绪（post-vault）后注入 `Arc<dyn mail::Mailer>`。
- 未来渠道 secret（slack token 等）通过 `SecretResolver`（来自 AppContext.vault 或独立 resolver）在构造 Channel 时解析。
- 触发器可在较早阶段（DB 可用）调用 enqueue；Service 负责在正确时机启动 delivery 任务。
- 任何 notification 相关全局配置走 Config 加载/占位符/（未来）profile + validate。

### 后台任务与生命周期

- `NotificationService::start(&self, shutdown: CancellationToken)` 负责 spawn 各渠道的 dispatcher task。
- 在 `commands/service/*`（FullAppContext 路径）中，vault + mail 就绪后构造 Service 并 start。
- 优雅关闭由 token 驱动。
- 支持动态 reload 某些设置（例如全局开关），但渠道凭据变更走 resolver 缓存失效。

## 推荐构造与启动流水线

```
Config::new
  -> Storage (DB 就绪，enqueue 可用)
  -> ... redis
  -> VaultCore
  -> (mail resolver + 构造真实 Mailer, per mail.md)
  -> EmailDispatcher::new(storage.notification_storage(), mailer) + spawn
  -> init_monorepo
  -> service dispatch；触发器可 enqueue；dispatcher 投递
```

对于 `config validate --resolve-secrets` 等运维命令：可构造最小 Service（仅 DB + vault）来验证渠道配置可解析/可发送测试通知，而不启动真实 tick 任务。

## 迁移步骤（分阶段，强绑定 config/mail/vault 阶段）

> **与 config/mail/vault 的强绑定（2026-06-14 更新）**：notification 的所有后续工作都依赖于前置模块的完成：
> - **阶段 0/1** 需要 mail.md 的阶段 0/1 完成（已满足）+ config.md 阶段 0b 的脱敏工具（用于日志脱敏）
> - **阶段 1** 与 mail.md 阶段 2 对齐（mail 的 password_ref 实现）
> - **阶段 3** 需要 vault.md 阶段 A/B/C 至少完成（fail-closed、最小 bootstrap、接口收窄），以及 config.md 的脱敏工具
> 不建议提前启动阶段 3，除非上述前置已经就绪。

**阶段 0（基础激活，与 mail 阶段 0/1 对齐，已部分完成）**：
- `main.rs` 声明 `mod notification;` 已完成。
- 在 service 启动路径中，于 vault + mail 就绪后 spawn `EmailDispatcher` 已完成。
- 剩余：确保触发器在 ceres/api 业务关键路径中被调用（至少 CL 评论）。
- 已补充 Noop mailer + test DB 风格的 dispatcher/storage 基线测试，并覆盖 dispatcher batch / max-in-flight、retry policy 配置生效和真实 SMTP/Mailpit 正路径；剩余是 Mailpit 故障矩阵与更长时间高水位矩阵。
- 剩余：依赖 config.md 阶段 0b 的脱敏工具，完善日志脱敏（避免 PII 泄露）
- 剩余验收：Mailpit/SMTP 故障矩阵覆盖 retry/skip/dead-letter 边界；日志无凭据泄露；mail 构造失败保持可诊断。

**阶段 1（渠道抽象与多渠道基础，与 mail 阶段 2 对齐）**：
- 引入 `NotificationChannel` trait + `EmailChannel` 实现。
- 重构 dispatcher 为多渠道协调器。
- 实现至少一个额外渠道原型（in-app 或 console）。
- 更新 NotificationService 以管理多渠道。
- 验收：可配置多渠道；email 渠道开始支持 mail 的 `password_ref`（当 **config.md 阶段 5** 和 **mail.md 阶段 2** 都完成时）。

**阶段 2（用户偏好 API 表面，完整移植 mega 能力）**：
- 已完成首批 API DTOs（带 utoipa）：当前用户 notification settings response、event preference response、update request/response，并复用 `UpdateUserNotificationConfig` 作为批量更新请求；admin 事件类型管理复用 `NotificationEventTypeInfo`。
- 已完成首批用户通知配置 API：`GET /user/notification/preferences` 返回当前用户 settings、事件类型、显式偏好和 effective enabled；`PUT /user/notification/preferences` 更新 global enabled、delivery_mode、preferred_locale 和批量 event preferences；`PUT /user/notification/preferences/{event_type_code}` 更新当前用户单个非 system-required event preference。
- 已完成首批事件类型管理 API：admin-only `GET /admin/notification-event-types` 和 `PUT /admin/notification-event-types/{code}`，支持维护 category、description、system_required、default_enabled。
- 剩余：补齐更完整 mega DTO 兼容面、审计能力和业务触发器接入面。
- 在 api/router 中注册对应路由（参考其他 router 模式）已完成首批。
- 完善触发器覆盖更多事件（issue、pr、mention 等）。
- 验收：用户可通过 API 管理自己的通知偏好；should_send 正确反映更新。

**阶段 3（Vault SecretRef + 渠道凭据，安全加固）**：
- Email 渠道完全迁移到 SecretRef（依赖 **mail.md 阶段 2** + **config.md 阶段 5**）。
- 设计并实现需要 secret 的其他渠道（slack token 等）通过 resolver 注入。
- 清理所有早期构造路径；强化日志脱敏（收件人、主题、正文在错误路径中受控，依赖 **config.md 阶段 0b** 的脱敏工具）。
- 与 config 的 `config secret set/check` 集成（支持 notification 相关 secret，如果有）。
- **前置**：本阶段不建议提前启动，必须等待 **vault.md 阶段 A/B/C 完成**（fail-closed 确保可靠、最小 bootstrap 支持运维命令、interface 收窄防止误用）
- 验收：所有渠道凭据仅在 vault 就绪后解析；core_key 加固已完成（来自 vault.md 阶段 A）。

**阶段 4（可靠性、扩展性、运维）**：
- 已完成首批管理 API：查看 jobs、状态统计、手动 retry failed job、按保留期 prune 旧 `sent`/`skipped` 终态 job、查看/下载/删除附件、按保留期 prune 旧终态 job 附件（可按 username/event type 收窄），并已支持 dispatcher 配置化自动附件保留清理、列出和 upsert notification event types。
- 已完成基础邮件模板与 registry：CL 评论通知通过 `MailTemplateRegistry` 按收件人 preferred_locale 渲染 subject/html/text，并继承 locale fallback 能力；admin-only 模板 list/preview/upsert API 已能审计覆盖关系、预览渲染并持久化外部 TOML 覆盖项。
- 改进重试策略（指数退避已完成；仍需告警集成）。
- 添加可观测（发送成功率、延迟、按事件/用户指标；tracing span 携带 event/job id）。
- 继续扩展模板化（模板版本审计/回滚、更多事件模板）。
- 实现 in-app 渠道（可能复用现有 message/notification 表或新建）。
- 继续扩展后台任务监控与 admin 接口（审计、必要时编辑/重投递安全边界）。
- 验收：高负载下可靠投递；失败可诊断和手动干预。

**阶段 5（与 config 高级能力对齐 + 长期维护）**：
- Profile 支持（不同环境默认事件启用或渠道）。
- 受控热加载（全局通知开关、某些渠道配置；凭据变更走 resolver 失效）。
- 集中校验（事件类型 schema、渠道配置完整性）。
- 完整测试矩阵（prefs 边界、claim 并发、渠道失败降级、PII 不泄露、SecretRef 解析失败）。
- CI 覆盖（配置样例中的 notification 部分、干跑 dispatcher）。
- 与 campsite slack 等外部集成深化。
- 文档同步（README、部署指南、事件类型目录）。

### 前置依赖矩阵（2026-06-14 更新）

| notification 阶段 | 主要工作 | 对 config 的依赖 | 对 vault 的依赖 | 对 mail 的依赖 |
|------------|--------|------------|-----------|-----------|
| **0** (基础激活) | dispatcher 启动 | 依赖 config 0b 日志脱敏工具 | 无 | **mail 0/1** (已满足) |
| **1** (渠道抽象) | NotificationChannel trait | 无 | 无 | **mail 2** (password_ref 时) |
| **2** (API 表面) | 用户偏好 API + admin 事件类型 API | 依赖 config 的 validate 等 | 无 | mail 的完整能力 |
| **3** (安全加固) | **Vault SecretRef + 多渠道凭据** | 依赖 config 0b/5 脱敏+resolver | **前置：vault A/B/C (fail-closed/bootstrap/interface)** | mail 2 (password_ref) |
| **4-5** (可靠性 + 功能) | 重试、可观测、模板 | 依赖 config 的 profile + reload | 可选（如有额外 secret） | 无 |

**关键前置依赖顺序：**
1. **config.md 阶段 0b（日志脱敏工具）→ notification 阶段 0**：dispatcher 启动时需要脱敏能力
2. **mail.md 阶段 2（password_ref）→ notification 阶段 1**：email 渠道支持 password_ref 需要 mail 完成迁移
3. **vault.md 阶段 A/B/C → notification 阶段 3**：安全加固不能在 vault 加固前进行
4. **config.md 阶段 5（resolver）→ mail 2 → notification 1-3**：整个 SecretRef 链路的依赖

**不建议提前启动阶段 3**，除非上述所有前置已就绪。

贯穿全程：
- 每阶段更新 `notification.md`、`config.md`（mail 相关章节）、`mail.md`。
- 同步更新 callisto 迁移（如新增事件类型）、config/config.toml 示例（如有全局项）、测试辅助（testing.rs 风格的 mock notification）。
- 保持与 mega 的 callisto 实体和 NotificationStorage API 兼容。

## 风险与约束

- **对 mail 模块的强依赖是硬约束**。email 渠道的可用性直接取决于 mail 的完成度和 late-construction 纪律。config.md 阶段 5 的 mail 工作必须先于或并行于 notification 的 email 投递生产化。
- **Enqueue vs Delivery 时序**：必须清晰区分“可 enqueue”（DB 可用）和“可 delivery”（vault + 渠道 secret 就绪）。业务触发器不应假设立即送达。
- **用户邮箱与 PII**：email 是主要 PII 载体；必须通过 prefs 严格过滤；发送内容应最小化；考虑 data retention 策略。
- **并发 claim 与重试语义**：多实例部署时 claim 提供保护，但需测试边界（重复发送、丢失）。
- **事件类型演进**：code 一旦使用即稳定；category/description 可变。system_required 变更需谨慎（影响已有 prefs）。
- **存储层位置**：NotificationStorage 在 jupiter 下合理（类似其他 *Storage），但高层 Service 应在 notification 一级模块内，隐藏 jupiter 细节。
- **与 config 其他前置的绑定**：vault 加固、日志脱敏、最小 bootstrap、CLI LoadMode、测试配置隔离、未知段告警等全部适用。
- **Campsite 集成**：作为用户源时，邮箱从外部来，需确保一致的“用户存在 + 允许通知”检查。
- 其他与 config.md 相同：单点故障（dispatcher 挂掉导致邮件堆积）、备份恢复时通知作业状态、跨平台等。

## 改进方案多维评估小结

| 维度 | 评估结论 |
| --- | --- |
| **合理性** | 高。把 notification 提升为一级模块，抽象渠道，严格 late delivery，完美承接 config.md 的引导循环、mail 作为第一 SecretRef 试点，以及 mega 验证过的 outbox + prefs 模型。 |
| **可行性** | 高。实体、存储逻辑、基本 dispatcher/triggers 代码已存在（从 mega 移植）。主要工作是模块化抽象、启动时机治理、API 表面补齐、与 mail/vault 的联动。受限于 mail 模块和 config 阶段的进度。 |
| **完整性** | 中。当前覆盖 email outbox + outbox 附件持久化 + 附件 metadata/下载/删除/保留期治理（含按 username/event type 收窄附件清理和配置化自动清理）+ 单个触发器 + 存储业务逻辑，并已有邮件作业管理 API、用户偏好 API 和事件类型管理 API 首批。补强项：多渠道抽象、port mega DTOs/in-app 渠道、可靠性增强、与 config 高级特性的集成（profile、hot reload、validate）。 |
| **安全性** | 良好（设计中）。明确晚于 vault 构造、依赖 mail 的 SecretRef 路径、prefs 强制同意、PII 最小化、错误脱敏要求。实现时必须与 vault 加固和日志脱敏前置 gate 同步。 |
| **功能正确性与接口兼容性** | 良好。Mailer trait + NotificationStorage API 清晰；与 callisto 实体对齐；与 mega 共享模型便于数据迁移。需确保新渠道 trait 不破坏现有 email 路径。 |
| **数据流与控制流** | 正确。Enqueue（触发器 → Storage）可较早；Delivery（Service + 渠道 + dispatcher）必须 post-vault+mail。claim 提供基础保护。 |
| **性能与效率** | 可接受。Outbox 解耦 I/O；批次 fetch + claim 控制并发。未来需关注大量 pending job 时的背压和 DB 负载。 |
| **可靠性与容错** | 基线已加固，仍需继续改进。字段支持 retry；dispatcher 已有可配置指数退避 retry/dead-letter、stale `sending` 恢复、可配置 bounded concurrency 和真实 SMTP/Mailpit 正路径，但仍缺告警、分布式锁/租约（多实例）和 Mailpit/SMTP 故障矩阵。Dispatcher 失败不应导致通知永久丢失。 |
| **兼容性与互操作** | 良好。与 mega 实体/存储兼容；campsite 作为用户源和潜在 slack 渠道提供方；Config 管道复用。 |
| **可扩展性与可维护性** | 良好。一级模块 + 渠道 trait + Service 抽象为新增事件/渠道留出空间。把存储细节隐藏在 jupiter 后，notification 模块专注策略和协调。 |
| **合规性与标准符合性** | 良好。Outbox + 用户同意模型、SecretRef 路径、对 PII 的处理要求，符合现代事件通知与隐私最佳实践。未来 slack 等外部渠道需额外合规评审。 |

## 小结

Notification 是 monoengine 事件驱动用户体验的重要组成部分（评论、审查、构建反馈等）。当前代码从 mega 移植了核心 outbox + 偏好 + 存储逻辑，已经接入主 crate 并在 mail 启用时启动 `EmailDispatcher`，但仍严重依赖一级 mail 模块的后续加固（作为 email 投递后端和首个 SecretRef 试点）。

将 notification 真正建设为一级模块，核心是：
- 确保模块在 main 中激活、提供高层 Service 抽象。
- 严格遵守 bootstrap 顺序（enqueue 早、delivery 晚于 vault + mail）。
- 渠道抽象化，支持从 email 向多渠道演进。
- 继续补齐通知管理 API 表面（首批已支持当前用户 settings、global enabled、delivery_mode、preferred_locale、event preference 查询/更新，admin event type list/upsert，以及 email job 附件 metadata/下载/删除/保留期治理，且附件清理可按 username/event type 收窄并可配置自动执行；仍需更完整审计和业务接入面）。
- 与 config 的 SecretRef、profile、热加载、校验能力对齐。
- 把可靠性、可观测和未来渠道（in-app、slack 参考 campsite）作为后续阶段。

所有 notification 相关工作都必须与 `config.md` 的阶段（尤其是 mail 作为前置）、vault 加固、日志脱敏、最小 bootstrap 等 gate 保持同步。任何试图在 vault 就绪前构造带凭据投递器的尝试都必须被阻止。

实施前请完整阅读：
- 本文档 + `config.md` 的「事实校准」「当前实现状态速览表」「硬约束」「secret 解析的依赖顺序」「实施前快速检查清单」。
- `mail.md`（email 渠道的具体设计与阶段）。

预期收益：可靠的多渠道通知、用户可控的偏好、与凭据管理（vault/SecretRef）的一致集成、易于扩展新事件和新渠道，同时保持与 mega 生态的兼容性。

---

**参考资料与对齐**：
- mega 项目：`mono/src/notification/{dispatcher,triggers}.rs`、`jupiter/src/storage/notification_storage.rs`、`jupiter/callisto/src/{email_jobs,notification_event_types,user_notification_*.rs}`、`ceres/src/model/notification.rs`（API DTOs）、触发器在业务层的调用模式。
- campsite 项目：用户源（邮箱、身份）、前端通知 UI 组件、slack 集成配置（作为未来 slack 渠道的参考模式）。
- monoengine 当前（读取时）：`src/notification/*`（部分移植，已通过 `mod notification;` 激活）、`src/jupiter/storage/notification_storage.rs`（完整）、`src/callisto/` 对应实体、`src/mail/`（作为 email 渠道前置，按 mail.md 设计）。
- 强依赖文档：`config.md`（引导循环、SecretRef、mail 作为第一试点、CLI LoadMode、最小 bootstrap、vault 加固等全部前置）、`mail.md`（email 渠道的 late 构造与 SecretRef 迁移计划）。

本计划采用分阶段、可独立验证的策略，确保每一步都能通过仓库要求的格式、clippy、测试 gate，并与 config/mail 的演进保持一致。
