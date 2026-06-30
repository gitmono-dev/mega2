# Mail 实现方案分析

本文档记录 `monoengine` 中一级 `mail` 模块（`src/mail/`）的设计、当前实现状态、与 Config / Vault / Notification 的集成方案、运行时注入与启动顺序约束，以及分阶段落地计划。

本文档的编写要求、结构深度、分析维度、事实校准风格、阶段规划 rigor 与 `config.md` 完全一致（包括现状速览表、硬约束列表、多维评估表、风险与前置 gate、实施检查清单等）。

> **治理规范**：本文档遵循 **`../general.md`** 中定义的统一结构、共同约束和执行标准。在审阅或执行本计划前，请先查阅 general.md 了解共同需求。

> **与 config 计划的强绑定**：`mail` 是 `config.md` 反复强调的”第一批可迁移凭据”的**唯一合格载体**。当前 `mail` 已补齐为真实、活跃、消费点晚于 `VaultCore` 的模块，并在 `AppContext::new` 中于 vault 之后解析 SMTP `mail.password_ref`、通过 `mailer_from_config` 构造 provider mailer 并启动 `EmailDispatcher`；因此 `mail.password_ref` 已成为首个真实 `SecretRef` 落点。任何 mail 相关工作都必须严格遵守 config.md 中的引导循环约束（`Config → Storage(DB) → Vault`）、最小 bootstrap 要求、日志脱敏前置、fail-closed 等。

> **集成测试指引**：邮件模块的各项功能（Mailer 启动、Dispatcher 后台处理、Mailpit 验证、retry 机制）应通过 **`integration.md`** 中的 `integration_mail_dispatcher_mailpit` 场景进行端到端验证。

## 事实校准（2026-06-18，更新 2026-06-19）

> 本文档中的代码引用已对照当前 `src/` 重新核对。特别注意以下与早期 monoengine 移植状态不一致的事实：
>
> **2026-06-19 更新**：(1) `on_cl_comment_created` 已接入真实业务路径——`src/api/router/cl_router.rs::save_comment` 在评论持久化后 best-effort 调用该触发器，outbox 自此有真实生产者（此前仅由测试 enqueue）。(2) email 投递已重构为经 `crate::notification::channels::EmailChannel`（`NotificationChannel` 抽象）投递，dispatcher 由 `NotificationService` 协调启动；mailer 注入语义不变（仍在 vault 之后构造），`mailer_from_config` 与 `SmtpMailer`/`ConsoleMailer` 未变。(3) 管理端模板版本审计/回滚、`mail.template_*` 热加载和 SMTP/mailer 动态重建也已落地。**2026-06-29 更新**：原生 HTTP provider（`src/mail/http.rs`）已落地，支持 `provider = "http"` 与 `mail.http_url`/`http_headers`/`http_timeout_secs`；剩余重点是更多业务触发器、更完整运维面和 Mailpit/SMTP 故障矩阵。

1. **历史上 `mail` 不是一级模块，也未真正参与编译**。`src/email/mod.rs` 实现了完整的 `Mailer` trait、`NoopMailer`、`SmtpMailer`（基于 `lettre`），并引用了 `MailConfig`，但：
   - `main.rs` 从未声明 `mod email;`（更不用说 `mod mail;`）。
   - 旧配置模块中原本没有 `MailConfig` 结构体和 `mail: Option<MailConfig>` 字段；当前已迁入 `src/config/model.rs`。
   - `config/config.toml` 末尾存在 `[mail]` 段（含 `enabled`/`smtp_host` 等 + 额外 `smtp_tls`/`tls` 字段），但因无强类型承接而被 serde 静默忽略。
   - `src/notification/dispatcher.rs`（及其测试）直接 `use crate::email::...`，但因为模块树不包含 email/notification，实际无法编译/运行。

2. **`MailConfig` 当前已进入 monoengine 的编译 Config。** 结构位于 `src/config/model.rs`，字段包括 `enabled`、`provider`（默认 `smtp`）、`smtp_host`、`smtp_port`、`username`、兼容期 `password`、推荐的 `password_ref`、`from`、`starttls`、`dispatcher_batch_size`、`dispatcher_max_in_flight`、`retry_max_attempts`、`retry_backoff_base_secs`、`retry_backoff_max_secs`、外部模板覆盖字段 `template_default_locale` / `template_dir`，以及可选的 outbox 附件自动清理策略 `attachment_prune_enabled` / `attachment_prune_interval_secs` / `attachment_retention_days` / `attachment_prune_statuses`；`password` / `password_ref` 互斥且只适用于 SMTP provider，`mail.enabled = true` 且 `provider = "smtp"` 时要求 `smtp_host` / `from` 非空，dispatcher 限流值、retry policy 值、模板默认 locale、可选模板目录和附件清理策略值均要求非零/合法。

3. **Notification 系统已接入主 crate，并在 mail 启用时启动 dispatcher**。`src/notification/{dispatcher, triggers, mod}.rs` + callisto 中的 `email_jobs`、`email_job_attachments`、`notification_event_types`、`user_notification_settings`、`user_notification_preferences` 等实体存在，触发器逻辑（`on_cl_comment_created` 等，尊重用户偏好）已从 mega 移植；`main.rs:18` 已声明 `mod notification;`，`AppContext::new` 在 vault 之后通过 `mail::mailer_from_config` 构造 SMTP 或 console mailer、创建 `EmailDispatcher` 并 `tokio::spawn`，HTTP graceful shutdown 已广播到 `notification_shutdown` 以取消 dispatcher。失败投递已有可配置 retry + dead-letter，dispatcher tick 已具备可配置批次/并发限流、结构化汇总日志、stale `sending` job 恢复、单 tick 背压测试、多 tick 高水位队列 drain 测试，以及跨独立 DB connection pool 的 claim 竞争基线测试；邮件作业管理 API 首批已落地（admin-only list / stats / failed retry / job prune / attachment metadata-download-delete-retention prune，其中附件 prune 可按 username / event_type_code 收窄）；admin 事件类型 API 首批已落地（list / upsert）；用户自助通知偏好 API 首批已落地（列出当前用户 settings/event prefs、更新 global enabled / delivery mode / preferred_locale、批量或单个 event preference）；基础模板系统已落地并扩展出 template registry + locale fallback，CL 评论邮件已通过 registry 按 `user_notification_settings.preferred_locale` 渲染 subject/html/text 并默认 HTML 转义变量，且启动期可从 `mail.template_dir` 加载 TOML 模板覆盖内置模板；admin-only 模板管理 API 已落地（list/preview/upsert/history/rollback），支持审计内置/外部模板、覆盖关系、来源路径、按管理员提供变量预览渲染、向 `mail.template_dir` 持久化 upsert 外部模板并热替换 registry，以及把旧模板归档到 `.history` 后按版本回滚；mailer 层已支持 HTML/Text + 附件 multipart 构造，outbox 已能持久化附件并由 dispatcher 投递；真实 SMTP/Mailpit 正路径基线已通过 `integration_mail_dispatcher_mailpit_sends_outbox_job` 覆盖，真实 SMTP transport 连接失败触发 retry 已通过 `integration_mail_dispatcher_smtp_failure_retries_outbox_job` 覆盖，真实 SMTP transport 连接失败达到尝试上限进入 dead-letter 已通过 `integration_mail_dispatcher_smtp_failure_dead_letters_outbox_job` 覆盖，真实 SMTP 协议拒绝触发 retry 且错误不包含凭据哨兵值已通过 `integration_mail_dispatcher_smtp_protocol_rejection_retries_without_credential_leak` 覆盖，真实 SMTP 认证拒绝触发 retry 且错误不包含 username/凭据哨兵值已通过 `integration_mail_dispatcher_smtp_auth_rejection_retries_without_credential_leak` 覆盖，真实 SMTP 权限/relay 拒绝触发 retry 且错误不包含 username/凭据哨兵值已通过 `integration_mail_dispatcher_smtp_relay_denied_retries_without_credential_leak` 覆盖，真实 SMTP 配置下缺失收件人直接 skipped 且不触发 retry 已通过 `integration_mail_dispatcher_smtp_skips_missing_recipient_without_retry` 覆盖。当前仍需补齐更多业务触发器调用面、更完整运维面，以及更完整 Mailpit/SMTP 故障矩阵、长时间压力形态高水位背压和真实多进程/黑盒 claim 竞争矩阵。

4. **Vault 约束对 mail 的决定性影响**：`password_ref` 的真实值读取**必须**发生在 `VaultCore` 就绪之后。当前 `AppContext::new` 顺序为 `Storage::new (DB + object_storage + Buck 校验) → init_connection(redis) → VaultCore::new → 若 provider 为 SMTP 则解析 mail.password_ref → mailer_from_config + EmailDispatcher spawn → init_monorepo`。因此 mailer 的**构造时机**是 mail 模块设计的核心约束（详见「运行时注入与晚绑定构造」）。

5. **campsite / mega 功能参考**：
   - mega（`mono/src/email` + `notification/` + callisto `email_jobs` + 触发器）提供了完整的 outbox 模式 + dispatcher 后台 tick + 事件驱动 enqueue（cl.comment.created 等）+ 用户通知设置过滤。
   - campsite 主要作为用户/认证后端（`api_store_backend = "campsite"`），不直接提供 monoengine 的邮件发送能力，但其用户邮箱可作为邮件通知的收件人来源。
   - monoengine 的 callisto 实体、NotificationStorage 方法（`enqueue_email_job`、`fetch_pending_jobs`、`should_send` 等）已与 mega 对齐。

6. 行号与模块路径以当前代码为准。

## 当前实现状态速览表（激活后，2026-06）

| 能力 / 组件                  | 实现状态          | 关键事实与风险 |
|-----------------------------|-------------------|---------------|
| `MailConfig` 结构体 + 纳入 Config | **已激活** | 添加到 `src/config/model.rs`（`Option<MailConfig>`，`#[serde(default)]`），含默认值函数、`MailProvider`（`smtp` / `console` / **`http`**）、反序列化测试、兼容期明文 `password` 与推荐 `password_ref`，以及 HTTP provider 专用的 `http_url` / `http_headers` / `http_timeout_secs`。**2026-06-29 更新**：新增 `http` provider 以支持通用 webhook 式投递。与 mega 结构兼容（扁平 + 额外 toml 字段被忽略）。 |
| 一级 `mail` 模块 (`src/mail/`) | **已激活** | `mod mail;` 在 `main.rs` 声明。`src/mail/mod.rs` 包含 `Mailer` trait、`NoopMailer`、`ConsoleMailer`、`SmtpMailer::new_with_password(...)`、`mailer_from_config(...)` + 构建消息 + 发送逻辑 + 单元测试。**2026-06-28 更新**：新增 `src/mail/testing.rs`，提供测试用 `MockMailer`。**2026-06-29 更新**：新增 `src/mail/http.rs`，提供 `HttpMailer` 通用 HTTP provider。旧 `src/email/` 降级为纯 re-export shim。 |
| Notification Dispatcher + 触发器集成 | **已接入编译** | `src/notification/dispatcher.rs` 及测试使用 `crate::mail`。触发器（triggers.rs）使用 NotificationStorage enqueue 逻辑（事件类型、用户偏好过滤）已存在；`main.rs:18` 已声明 `mod notification;`。 |
| 后台 dispatcher 启动 | **已在 mail 启用时启动，并接入 HTTP graceful shutdown** | `AppContext::new` 在 `VaultCore::new` 之后通过 `mailer_from_config` 构造 provider mailer、创建 `EmailDispatcher` 并 `tokio::spawn(dispatcher.run(shutdown))`；HTTP server shutdown 广播会同时取消 `notification_shutdown`，避免 dispatcher 在进程优雅关停阶段遗留运行。SMTP 构造失败现在返回可诊断错误；发送失败会按 `mail.retry_backoff_base_secs` 指数退避并受 `mail.retry_backoff_max_secs` 截断后重新排队，在达到 `mail.retry_max_attempts` 后转为 `failed` dead-letter，默认仍为 5 次、30s 基础 backoff、300s 上限。`EmailDispatcher` 当前每 tick 先恢复超过 `EMAIL_JOB_SEND_TIMEOUT_SECS = 900` 的 stale `sending` job，再按 `mail.dispatcher_batch_size` 拉取 pending job，并以 `mail.dispatcher_max_in_flight` 做有界并发发送；若启用 `mail.attachment_prune_enabled`，同一 dispatcher 会按 `mail.attachment_prune_interval_secs` 定期清理超过 `mail.attachment_retention_days` 的旧终态附件。默认发送背压仍为 50 / 8，tick 结束输出 sent / retry / dead-letter / skipped / claim-missed / batch_size / max_in_flight / retry policy / attachments_pruned 等结构化汇总。 |
| 晚于 Vault 的 mailer 构造 | **已落地** | `SmtpMailer::new` / `mailer_from_config` 本身是同步且轻量的，当前调用点在 `context/mod.rs:46-55`，严格晚于 `VaultCore::new`。 |
| SecretRef / `password_ref` 支持 | **已落地首批** | 当前 `MailConfig.password: Option<SecretString>` 仅为兼容期入口，`password_ref: Option<SecretRef>` 为推荐路径；两者互斥且仅适用于 `provider = "smtp"`，且 `mail.password_ref` / `config secret mail.password` 只接受 `vault://secret/config/<profile>/mail/password#<field>` namespace。`AppContext::new` 在 vault 就绪后通过 resolver 解析 SMTP `password_ref` 并构造 SMTP mailer。 |
| 多种后端（SES、SendGrid 等） | **SMTP + console + HTTP 已实现** | 已支持 `provider = "smtp"`、`provider = "console"` 与 **`provider = "http"`**；HTTP provider 向 `mail.http_url` POST JSON payload（含 base64 附件），并支持自定义 `http_headers`（如 `Authorization`），可直接对接 SendGrid/SES HTTP API 或自定义 relay。SMTP relay 仍可用。 |
| 模板 / 富文本 / 附件 | **模板 registry + 启动期 TOML 覆盖 + 管理端审计/预览/持久化 upsert/history/rollback + HTML/Text + 用户 locale + outbox 附件 + 附件 metadata/下载/删除/保留期治理** | `src/mail/template.rs` 已提供轻量 `MailTemplate`、`MailTemplateRegistry`、`LocalizedMailTemplate` 和 `MailTemplateKey`，支持 `{{var}}` 渲染、HTML 变量默认转义、缺失变量脱敏诊断、按 locale 查找以及 language/default fallback；CL 评论触发器已按每个收件人的 `user_notification_settings.preferred_locale` 渲染 subject/html/text，并提供 `zh-CN` 首个本地化模板。`mail.template_default_locale` / `mail.template_dir` 已支持启动期从 TOML 文件加载 key/locale/subject/html/text 覆盖模板，覆盖项按 key + locale 替换内置模板且模板语法 fail-closed 校验；admin-only `GET /admin/mail-templates` 已支持审计内置/外部模板、来源路径和覆盖关系，`POST /admin/mail-templates/preview` 已支持按管理员提供变量预览 subject/html/text 渲染结果，`PUT /admin/mail-templates/{key}/{locale}` 已支持在 `mail.template_dir` 中创建或更新外部 TOML 模板、复用既有来源文件、拒绝坏模板语法并热替换通知模板 registry，history/rollback 端点已支持 `.history` 归档、版本审计和回滚。`send_html(to, subject, html, text?)` 实现 alternative multipart；`send_html_with_attachments(...)` + `MailAttachment` 已支持 SMTP mixed multipart 附件构造，console provider 只记录附件数量/字节数且不输出正文；`email_job_attachments` 已支持 outbox 级附件持久化，dispatcher claim 后会读取附件并调用附件发送路径；admin-only `/admin/email-jobs/{id}/attachments` 已提供附件 metadata 审计视图（id、文件名、content type、字节数、创建时间）且不返回内容，`GET /admin/email-jobs/{job_id}/attachments/{attachment_id}/content` 已支持显式下载单个附件内容，`DELETE /admin/email-jobs/{job_id}/attachments/{attachment_id}` 已支持删除持久化附件，`POST /admin/email-jobs/attachments/prune` 已支持按保留期清理旧 `sent`/`skipped` 终态 job 的持久化附件，并可按 `username` / `event_type_code` 收窄清理范围；`mail.attachment_prune_*` 已支持 dispatcher 内置的可配置自动附件保留清理。仍无更高阶模板引擎和按租户/事件策略预设。 |
| 与 user_notification_* / 事件类型 的完整联动 | **实体+存储+触发器骨架存在，API 首批落地** | callisto 实体 + NotificationStorage 方法 + triggers（cl.comment 等）已移植自 mega，dispatcher 常驻任务、mailer 注入和失败 dead-letter 基线已接入；admin-only 邮件作业管理 API 已支持按状态/用户/事件查询、状态统计、failed job 手动重排、旧 `sent`/`skipped` 终态 job 清理，以及查看、下载、删除或按保留期清理旧终态 outbox 附件；admin-only 事件类型 API 已支持 list/upsert；用户自助 API 已支持查询当前用户 settings/event preference effective 状态，并更新 global enabled、delivery mode、批量或单个 event preference。仍缺更多业务触发器调用面、更完整观测和运维控制。 |
| Profile / 热加载 / 集中校验对 mail 的支持 | **部分实现** | Profile、集中校验和 source warning 已接入 config 管线；热加载当前支持 `mail.enabled` true→false 关停 dispatcher 与 false→true 运行时重新启用（启动期 mail 关闭时仍预创建带 NoopMailer 的 dispatcher，reload 后异步重建为配置 provider），`mail.dispatcher_batch_size` / `mail.dispatcher_max_in_flight` 运行期调整 dispatcher 背压参数，`mail.retry_max_attempts` / `mail.retry_backoff_base_secs` / `mail.retry_backoff_max_secs` 运行期调整 retry/dead-letter 策略，`mail.attachment_prune_*` 运行期调整自动附件保留策略，`mail.template_*` 重建模板 registry，以及 `mail.provider`/SMTP 参数/凭据/from/starttls 经异步 mailer 重建热替换。 |
| 测试与 CI 覆盖 | **部分实现** | mail 自身有构造/消息验证、provider 工厂、console provider、SMTP 附件 mixed multipart 构造、附件 content-type 校验、模板渲染/缺失变量/HTML 转义、template registry、locale fallback、外部 TOML 模板覆盖/来源路径审计/TOML 序列化 round-trip/重复 key-locale 拒绝测试；dispatcher 有使用 Noop 的集成风格测试（需 DB + migration），并已补充真实 SMTP/Mailpit 正路径测试 `integration_mail_dispatcher_mailpit_sends_outbox_job`，覆盖 pending outbox job 经 `SmtpMailer` 投递到 Mailpit 后进入 `sent` 且写入 `sent_at`，真实 SMTP 连接失败 retry 测试 `integration_mail_dispatcher_smtp_failure_retries_outbox_job`，覆盖 transport 错误后 job 回到 `pending`、`retry_count` 增加且 `next_retry_at` 写入，真实 SMTP 连接失败 dead-letter 测试 `integration_mail_dispatcher_smtp_failure_dead_letters_outbox_job`，覆盖达到尝试上限后 job 进入 `failed` 且不再写入 `next_retry_at`，真实 SMTP 协议拒绝 retry 测试 `integration_mail_dispatcher_smtp_protocol_rejection_retries_without_credential_leak`，覆盖 job 回到 `pending`、`retry_count` 增加且错误中不包含凭据哨兵值，真实 SMTP 认证拒绝 retry 测试 `integration_mail_dispatcher_smtp_auth_rejection_retries_without_credential_leak`，覆盖 job 回到 `pending`、`retry_count` 增加且错误中不包含 username/凭据哨兵值，真实 SMTP 权限/relay 拒绝 retry 测试 `integration_mail_dispatcher_smtp_relay_denied_retries_without_credential_leak`，覆盖 job 回到 `pending`、`retry_count` 增加且错误中不包含 username/凭据哨兵值，真实 SMTP 配置下缺失收件人 skip 测试 `integration_mail_dispatcher_smtp_skips_missing_recipient_without_retry`，覆盖 job 进入 `skipped` 且不递增 retry，以及 `dispatcher_drains_high_water_queue_across_bounded_ticks` 覆盖高水位队列按配置 batch 多 tick 排空；已覆盖 `password_ref` 解析失败脱敏、坏配置不 panic、`mail.enabled` 关停热加载、dispatcher 批次/并发限流热加载、retry policy 热加载、自动附件保留策略热加载、template 配置热加载/失败回滚、provider/SMTP 参数/凭据重配热加载且不泄露 secret、热加载 mailer 重建在 `password_ref` 解析失败时 fail-safe 保留旧 mailer 且错误脱敏（`mailer_rebuild_fails_safely_and_redacts_when_password_ref_unresolvable`）、失败发送的 retry/dead-letter disposition、dispatcher 有界并发、单 tick 批次背压、stale `sending` 恢复、storage-level 并发 claim 竞争、跨独立 DB connection pool 的 claim 竞争基线、邮件作业 list/stats/failed retry 管理原语、outbox 附件持久化、附件 metadata 查询/内容读取/删除/保留期清理、dispatcher 自动附件保留清理和 dispatcher 附件投递、admin template list/preview/upsert/history/rollback 管理原语、用户 notification preference/settings response 映射和 update payload 校验、用户 preferred_locale 存储/响应校验，以及 CL 评论触发器按收件人 locale/覆盖 registry 的模板渲染和全收件人显式关闭事件偏好不 enqueue。仍缺更完整 Mailpit/SMTP 故障矩阵、长时间压力形态高水位背压压测和真实多进程/黑盒 claim 竞争矩阵。 |

**已知加载/启动/安全风险点（必须在相应阶段消除，与 config.md 风险点重叠）**：
- mailer 或 dispatcher 若被移动到 Storage::new / vault 前路径，会违反 vault 就绪顺序；当前代码位置正确，但需防止后续回归。
- 兼容期明文 `password` 若由用户配置，仍需避免进入日志、错误、Debug 或 CI 输出。
- `config/config.toml` 必须保持只给 `password_ref` 占位，不写入示例明文密码。
- Notification 核心事件类型已迁移到 migration-time seeding；触发器 upsert 仅保留为幂等 fallback，新增事件仍需同步 migration/API registry、触发器常量和模板 key。

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
mailer_from_config(...)  -->  Arc<dyn Mailer>
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
6. 真正服务启动路径：若 provider 为 SMTP 且存在 `mail.password_ref` 则解析凭据，再通过 `mailer_from_config(...)` 构造 provider mailer，得到 `Arc<dyn Mailer>`。
7. 构造 `EmailDispatcher::new_with_control(notif_stg, mailer, control)` 并 `tokio::spawn(dispatcher.run(shutdown))`。
8. 执行 `init_monorepo`，随后进入 HTTP/SSH/multi 服务分发。
9. 业务触发器开始 enqueue，dispatcher 处理 outbox 投递。

**严禁**在 Storage 构造阶段或 `VaultCore::new` 之前调用 `SmtpMailer::new`。当前 `AppContext::new` 中的调用点位于 vault 之后，顺序正确；后续若引入 SecretResolver，也必须保持在 vault 之后。

当前 `password_ref` 版本：`Config` 只保存 `SecretRef` 引用，resolver 在步骤 6 之后解析 SMTP `mail.password_ref`，再用解析后的值通过 `mailer_from_config` 构造 mailer。若后续要支持运行期重配，再评审 mail 模块内部持有 `SecretResolver` + 缓存凭据的设计。

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

## 硬约束与不可违反的原则

与 config.md 「现有 vault 能力与关键约束」完全一致；以下约束为硬边界，任何实现偏离都必须重新评审：

- mailer 构造**必须**晚于 VaultCore。
- `config secret set mail.password ...` 等运维命令必须使用**最小 DB/Vault bootstrap**（不能初始化 Redis、对象存储、完整 Storage 服务、HTTP 监听）。
- 在 `core_key.json` 加固完成前，mail password 进 vault 的收益仅限“不进 git/不进常规日志”。
- 任何在 Storage::new 或 redis init 阶段“触达”密码的行为都是违规的（即使当前是空字符串）。

因此，`MailConfig` 里的 `password` 只作为兼容期入口保留；生产配置应使用 `password_ref`，并由 vault 就绪后的 resolver 路径解析。

## 现状与目标对比

| 维度 | 当前状态 | 目标状态 | 实现难度 |
|-----|--------|--------|--------|
| 架构基础 | 一级模块激活，outbox + dispatcher 框架完成，与 Config/Vault 接线就绪 | 强化多实例协调（分布式锁/租约）；完全遵循 config 热加载管道 | 中等 |
| 密钥管理 | `password_ref` 作为首个 SecretRef 消费端已落地，兼容期明文 `password` 保留 | 明文 `password` 完全退场，vault 加固后升级凭据隐藏等级 | 中等 |
| Provider 支持 | SMTP + console 已完成；SES/SendGrid 可通过 SMTP relay 使用 | 必要时增加原生 HTTP API provider | 复杂 |
| 模板与国际化 | registry + TOML 覆盖 + 多 locale + admin 审计/预览/upsert/history/rollback 已落地 | 按租户/事件类型预设；高阶模板继承与默认值机制 | 复杂 |
| 附件管理 | multipart 构造、持久化、管理 API、清理策略、dispatcher 自动保留已落地 | 优化长期存储成本；副本与跨域备份策略 | 中等 |
| 可靠性测试 | 基础正路径 + SMTP 失败场景（连接/认证/权限）+ retry/dead-letter 已覆盖 | CI 长期压力验证；真实多进程黑盒 claim 竞争矩阵 | 复杂 |
| 运维与诊断 | 结构化日志 + 脱敏 + job 管理 API + 退信告警已落地 | 完整 metrics + OpenTelemetry + ops 告警集成 | 中等 |
| 热加载与灵活性 | `mail.enabled`/provider/SMTP 参数/凭据/`template_*` 已支持运行期调整 | 所有配置项零停机应用；失败自动 rollback | 中等 |

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
├── mod.rs            # 对外入口：trait Mailer、MailAttachment、Noop、Console、Smtp 工厂、re-export
├── smtp.rs           # SmtpMailer 实现 + 传输构建
├── noop.rs           # （或直接在 mod）
├── providers/        # 未来：ses.rs、sendgrid.rs 等真实第三方 provider
│   └── mod.rs
├── template.rs       # 已有轻量模板渲染 + registry + locale fallback
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
- 优雅关闭：通过 CancellationToken 停止 tick；HTTP server 的 shutdown 广播已同时取消 `AppContext.notification_shutdown`，覆盖 mail dispatcher。
- 多实例注意：outbox + `try_claim_job` 提供基础互斥，stale `sending` 恢复可处理 dispatcher 崩溃后遗留的发送中 job；生产仍可按部署形态增加分布式锁、单实例约束或更明确的租约字段。

### 多环境与 Profile

继承 config 的 profile 机制：`config.mail` 里的 host/port 可被 profile 覆盖，`password_ref` 必须包含受控 namespace（如 `vault://secret/config/prod/mail/password#value`）。

### 可扩展性（Provider）

`Mailer` trait 保持最小。当前 `mailer_from_config` 已根据 `mail.provider` 选择 SMTP 或 console；未来 `MailService` 或工厂可继续扩展子表 `mail.smtp` / `mail.ses`。Noop 始终可用作测试降级。

## 推荐加载与构造流水线（mail 视角）

```
ConfigLoader + Config::new (含未解析 SecretRef 的 mail)
  -> ... (Storage, redis)
  -> VaultCore
  -> SecretResolver (如果 SMTP password_ref 存在)
  -> 解析 SMTP mail 凭据（或直接用兼容期明文）
  -> mailer_from_config(构造 SMTP/console provider)
  -> EmailDispatcher + spawn
  -> 触发器可安全 enqueue
```

`config mail validate --resolve`（或复用 `config validate --resolve-secrets`）应能使用最小 bootstrap 检查 mail 配置是否可发送测试信。

## 迁移步骤（分阶段）

> **与 config.md 的强绑定（2026-06-18 更新）**：mail 的后续工作仍依赖 config 的热加载、profile 和 diagnostics 演进。原先阻塞 mail 的构造失败诊断、`SecretRef` + resolver 基础设施和 `password_ref` 首个消费端已经完成首批落地。

**阶段 0（已完成）**：激活一级 mail + MailConfig 入 Config + 修复 notification 引用 + 清理 shim。

**阶段 1（已接入，基线已加固）**：在 service 启动路径中 late-construct mailer 并 spawn dispatcher 已落地；构造失败静默忽略已改为可诊断错误；失败发送已具备可配置 retry + dead-letter disposition，并按 base/max 配置执行指数退避；dispatcher 已具备每 tick 可配置批次/并发限流、结构化汇总日志、stale `sending` 恢复、单 tick 批次背压测试、多 tick 高水位队列 drain 测试（`dispatcher_drains_high_water_queue_across_bounded_ticks` 覆盖 125 个 pending job 在 batch=20/max_in_flight=5 下跨 tick 有界排空）、storage-level 并发 claim 竞争测试、跨独立 DB connection pool 的 claim 竞争基线、真实 SMTP/Mailpit 正路径集成测试，以及真实 SMTP 连接失败 retry/dead-letter、协议拒绝 retry/凭据不泄露、认证拒绝 retry/凭据不泄露、权限/relay 拒绝 retry/凭据不泄露与缺失收件人 skip 测试。剩余是完善 Mailpit/SMTP 故障矩阵、长时间 soak 压力形态高水位背压验证和真实多进程/黑盒 claim 竞争矩阵。

**阶段 2（已完成首批）**：与 config SecretRef 基础设施联动。`MailConfig` 已支持 `password_ref`，resolver 解析路径已在 `AppContext::new` 中落地，`config secret set/check` 已支持 mail password 引用。剩余是继续治理兼容期明文 `password` 的退场策略。

**阶段 3（已完成首批管理面 + template registry/外部 TOML 覆盖 + 管理端模板审计/预览/持久化 upsert + 用户 locale + 本地 provider + outbox 附件 + dispatcher 限流/退信配置）**：邮件作业管理 API 已先落地 admin-only `email-jobs/list`、`email-jobs/stats`、`email-jobs/{id}/retry`、`email-jobs/prune`、`email-jobs/{id}/attachments` metadata 接口、`email-jobs/{job_id}/attachments/{attachment_id}/content` 内容下载接口、`email-jobs/{job_id}/attachments/{attachment_id}` 删除接口和 `email-jobs/attachments/prune` 附件保留期清理接口，支持查询 outbox、按状态统计、将 `failed` job 重新排回 `pending`、按保留期清理旧 `sent`/`skipped` 终态 job，以及查看、下载、删除或按保留期清理旧终态 job 附件；附件保留期清理已支持按 `username` / `event_type_code` 收窄范围；`mail.attachment_prune_*` 已支持 dispatcher 内置的自动附件保留清理策略；admin-only 事件类型 API 已支持 `notification-event-types` list/upsert；admin-only 模板 API 已支持 `mail-templates` list、`mail-templates/preview` 和 `mail-templates/{key}/{locale}` upsert，用于审计内置/外部模板、覆盖关系、来源路径、预览渲染，并向 `mail.template_dir` 持久化外部 TOML 模板后热替换 registry；用户自助偏好 API 已支持 `GET /user/notification/preferences`、`PUT /user/notification/preferences` 和 `PUT /user/notification/preferences/{event_type_code}`，覆盖 global enabled、delivery mode、preferred_locale、批量或单个 event preference；`src/mail/template.rs` 已提供基础模板渲染、template registry、locale fallback 和启动期 TOML 模板覆盖，CL 评论邮件已改为通过 registry 按收件人 locale 生成 subject/html/text 并默认 HTML 转义变量；`mail.provider = "console"` 已提供本地/dev/CI 干跑 provider；`MailAttachment` + `send_html_with_attachments(...)` 已支持 SMTP 附件 multipart 构造；`email_job_attachments` 已支持 outbox 级附件持久化，dispatcher 发送时会加载并传递给 mailer；`mail.dispatcher_batch_size` / `mail.dispatcher_max_in_flight` 已支持运行期调整 dispatcher 背压；`mail.retry_max_attempts` / `mail.retry_backoff_base_secs` / `mail.retry_backoff_max_secs` 已支持运行期调整 retry/dead-letter 策略。

**第三方 Provider 支持说明（2026-06-29 更新）**：真实第三方邮件服务（**Amazon SES、SendGrid 等**）既可通过现有 `provider = "smtp"` 使用其 SMTP relay，也可通过新增的 `provider = "http"` 直接调用其 HTTP API：
- SMTP relay（已有）：SES `email-smtp.<region>.amazonaws.com:587`、SendGrid `smtp.sendgrid.net:587`，凭据经 `mail.password_ref` 存入 vault。
- HTTP provider（新增）：配置 `mail.http_url` 与可选 `mail.http_headers`（如 `Authorization: Bearer <token>`），`HttpMailer` 会 POST JSON payload `{to, subject, html, text, attachments}`（附件 base64 编码），可对接 SendGrid `/v3/mail/send`、SES API 或自定义 relay。
原生 HTTP provider 扩展点已由 `src/mail/http.rs` 落地；SMTP relay 仍保留以兼容已有部署。

✅ **模板版本审计/回滚（已落地，2026-06-19）**：`mail-templates/{key}/{locale}` upsert 现在覆写前会把旧内容归档到 `{mail.template_dir}/.history/{key}__{locale}/{NNNN}.toml`（顺序版本号；loader 只读顶层 `*.toml`，`.history` 子目录不会被当作 live 模板）。新增 admin-only `GET /admin/mail-templates/{key}/{locale}/history`（列出版本号/subject/字节数）与 `POST /admin/mail-templates/{key}/{locale}/history/{version}/rollback`（回滚到指定版本，回滚本身也归档当前内容，并热替换 registry）。`upsert` 响应新增 `archived_version`。有单测 `mail_template_versions_archive_on_overwrite_and_support_rollback` 覆盖归档编号、列表与还原。更高阶的按租户/事件策略预设同属后续。

**阶段 4（已完成首批：模板热加载 + 可观测 + 退信告警）**：Profile 感知的 mail 配置、热加载支持、更强可观测。已落地（2026-06-19）：
- **`mail.template_*` 热加载**：`mail.template_dir` / `mail.template_default_locale` 已从"需重启"改为运行期热应用——`config_reload_mail_template_subscriber`（`src/notification/triggers.rs`）在 reload 时从文件重建 template registry 并热替换，**纯文件 IO、不涉及 vault，可安全在同步 reload 流水线内执行**；坏 `template_dir` fail-closed（apply 报错 → reload 回滚 → 保留旧 registry）。有单测 `mail_template_reload_subscriber_hot_swaps_default_locale` + reload 测试 `reload_applies_mail_template_settings_and_publishes_snapshot`。
- **可观测**：`process_email_job` 已加 `#[tracing::instrument]` span（job id / event type / channel / 脱敏收件人）；每 tick 仍输出 sent/retry/dead-letter/skipped/... 结构化计数；dispatcher 错误日志经 `global_redactor()` 脱敏 URL userinfo。
- **退信告警 hook**：job 进入 dead-letter 时发出 `target: "notification_alert"` 的 warn 事件，供 ops 告警接入（不输出原始错误/收件人 PII）。
- ✅ **动态 mailer 重建与 mail 运行期重新启用（已落地，2026-06-19；2026-06-23 补 false→true 运行时重启用）**：`mail.provider` / `smtp_host` / `smtp_port` / `username` / `password` / `password_ref` / `from` / `starttls` 已从"需重启"改为运行期热应用。`EmailChannel` 现持 `MailerHandle = Arc<ArcSwap<MailerSlot>>`，每次发送读取当前 mailer；`config_reload_mailer_subscriber`（`src/notification/service.rs`）在这些字段变更时**异步**重建 mailer——通过 `tokio::runtime::Handle::try_current()` 从同步 reload 订阅者 spawn 一个 task，在 vault 就绪态 re-resolve `password_ref`（`VaultSecretResolver`）→ `mailer_from_config` → `handle.store(...)` 热替换；重建失败保留旧 mailer（fail-safe），无运行时则记录需重启。`EmailChannel` 热替换机制有单测 `email_channel_hot_swaps_mailer_via_handle`，reload 行为有 `reload_applies_mail_reconfiguration_and_publishes_snapshot_without_leaking_secrets` / `reload_applies_secret_ref_change_and_publishes_without_leaking`（断言已 applied、快照已发布、report 仅含字段名不泄露 secret）。`mail.enabled` false→true 现在也可运行时完成：`AppContext::new` 在 `[mail]` 存在时即预创建 `NotificationService` 与 dispatcher task（初始 mailer 为 `NoopMailer`、control 按 `mail.enabled && notification.enabled` 关闭），reload 将 `mail.enabled` 从 false 切 true 后，`config_reload_email_dispatcher_subscriber` 打开 control 开关，`config_reload_mailer_subscriber` 异步将 NoopMailer 重建为配置 provider，无需重启进程。

**阶段 5**：完整测试矩阵（坏 SMTP 已覆盖连接失败、dead-letter、协议拒绝、认证拒绝、权限/relay 拒绝和缺失收件人 skip；用户偏好全关场景已覆盖 CL 评论触发器全收件人 opt-out 不 enqueue；热加载 mailer 重建在 `password_ref` 解析失败时已覆盖 fail-safe 保留旧 mailer 且错误脱敏；大量 pending job 的模块级背压回归已覆盖 125 个 job 跨 tick 有界排空，仍需更长时间 soak/黑盒压测）、在已有 Mailpit 正路径基线之上扩展 CI 中真实邮件发送干跑与故障矩阵、文档同步（README、部署指南）。Mailpit 正路径测试 `integration_mail_dispatcher_mailpit_sends_outbox_job` 已改为在 Mailpit 不可达时优雅跳过（探测 `MAILPIT_API_URL` 失败即 `eprintln` 提示并 `return`，不再 `panic!`），因此未启动 docker compose 测试栈时不会让整个测试二进制失败；Mailpit 在位时仍完整执行投递断言。

## 前置依赖矩阵

| mail 阶段 | 主要工作 | 对 config 的依赖 | 对 vault 的依赖 | 对 notification 的依赖 |
|----------|--------|------------|-----------|-----------------|
| **0** (已完成) | MailConfig + 一级模块 | `src/config/model.rs` 包含 MailConfig 字段 | 无 | 无 |
| **1** (基本完成) | 晚构造 + dispatcher + 并发/观测/恢复基线 | 构造失败诊断已接入 config 脱敏错误路径；dispatcher batch / max-in-flight 已支持运行期配置 | 无 | 支持通知基础，dispatcher 已有有界并发、retry/dead-letter disposition、stale sending 恢复和 claim 竞争测试 |
| **2** (已完成首批) | password_ref 迁移 | SecretRef + resolver 已落地 | 支持通过 resolver 读取 | notification 可使用 password_ref |
| **3-5** | Provider + 模板 + 可靠性 | 依赖 config 的 profile、validate、reload 后续增强 | 部分依赖 vault 的完整加固 | 作业管理 API（含终态 job prune）与事件类型 API 首批已落地，继续支持后续功能扩展 |

**关键前置依赖：**
- `password_ref` 首个消费端已经落地；后续不要重复实现 resolver/mail 接入。
- 运行期重新启用 mail（`mail.enabled` false→true）已无需重启；启动期 `[mail]` 存在时即预创建 dispatcher（初始 mailer 为 `NoopMailer`），reload 打开 enable 开关并异步重建为配置 provider，失败时保留旧 mailer。

贯穿：每次变更同步更新 `config/config.toml` 示例、`mail.md`、`config.md` 相关章节、notification 触发器新增事件时的文档。

## 风险与约束

- **构造时机是不可违反的硬约束**。违反即退回“早期依赖”，无法使用 SecretRef。
- **outbox + claim 模型的并发与重试语义**需与业务仔细评审（重复发送风险 vs 丢失风险）。当前已用 `try_claim_job`、跨独立 DB connection pool 的 claim 竞争测试和 stale `sending` 恢复覆盖基础互斥与崩溃恢复，但真实多进程场景仍需 Mailpit/DB 黑盒矩阵验证。
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
| **完整性** | 中高。覆盖了 trait、SMTP/console 实现、mailer 层附件构造、outbox 附件持久化、附件 metadata 审计、下载、删除和保留期清理 API（含按用户/事件类型收窄附件清理与 dispatcher 自动附件保留策略）、触发器骨架、dispatcher 运行期背压和退信策略配置、HTTP graceful shutdown 取消 dispatcher、与 Config/Vault 的集成点，并已有作业管理 API、template registry、启动期 TOML 模板覆盖、管理端模板审计/预览/持久化 upsert/history/rollback 和动态 mailer/template 热加载基线。补强项：必要时的原生 HTTP provider、更高阶按租户/事件策略预设、管理 UI、dispatcher 失败诊断/metrics。 |
| **安全性** | 强（设计中）。明确把凭据构造推迟到 vault 之后、要求脱敏、与 SecretRef 对齐、Noop 安全降级。实现时必须把 vault_core 的泄露问题一起解决。 |
| **功能正确性与接口兼容性** | 良好。Mailer trait 简单稳定；与 notification 的集成点（dispatcher 构造参数）清晰；与 callisto 实体对齐。shim 保证平滑过渡。 |
| **数据流与控制流** | 正确。严格遵循 config.md 画的依赖顺序图。enqueue 只在触发器，send 只在 dispatcher tick，构造只在 post-vault。 |
| **性能与效率** | 可接受。outbox 解耦了业务线程与 SMTP I/O；批次拉取 + claim 提供基础并发控制。未来可加并发发送 worker 池。 |
| **可靠性与容错** | 改进空间大。当前有 retry_count/next_retry_at 字段，失败发送已按可配置指数退避重排并在达到配置阈值后进入 failed dead-letter。仍需补充告警、更完整观测和多实例策略。claim 失败时的幂等性需继续保证。 |
| **兼容性与互操作** | 良好。与 mega 共享实体和部分逻辑；Config 管道复用；未来 provider 扩展点清晰。 |
| **可扩展性与可维护性** | 良好。一级模块 + trait + 目录规划（providers/）为扩展留了空间。和 config 模块的演进路径绑定良好。 |
| **合规性与标准符合性** | 良好。outbox 模式、用户偏好尊重、SecretRef 路径、对日志脱敏的要求都符合现代邮件与凭据管理实践。 |

## 小结

通过已完成的激活与后续 config 联动（MailConfig 入 Config、`src/mail/` 成为一级模块、notification 引用修复、`password_ref` 解析和 dispatcher 启动接线），monoengine 已拥有可编译、可测试、可作为“真实后置消费者”的 mail 能力。

后续工作应严格按本计划 + config.md 的阶段划分进行：继续完善 Mailpit/SMTP 故障矩阵、长时间压力形态高水位背压、真实多进程/黑盒 claim 竞争、更高阶附件保留策略预设和更完整运维面；最后再持续扩展必要的原生 provider、高级模板和运维能力。

所有 mail 相关的实现、文档、测试、配置示例都必须与 config 模块的拆分、CLI LoadMode、最小 bootstrap、日志脱敏、core_key 加固等前置 gate 保持同步。任何试图在 Storage::new 或 `VaultCore::new` 之前构造带真实凭据 mailer 的尝试都必须被视为架构违规。

实施前请完整阅读本档 + `config.md` 的「事实校准」「当前实现状态速览表」「硬约束」「secret 解析的依赖顺序」和「实施前快速检查清单」。

## 预期收益

- **邮件投递链路完整闭合**：从 Config 解析、Vault 解析凭据、Dispatcher 启动到消费侧 enqueue 全链路打通，作为首个 SecretRef 消费端验证了架构顺序与最小 bootstrap 要求（见 `AppContext::new` 链路及 config.md 依赖顺序）。
- **业务与 I/O 解耦**：outbox 模式把 enqueue 与发送解耦；失败自动重试（指数退避）且有界（可配置次数 + dead-letter）；dispatcher 每 tick 输出结构化计数（sent/retry/dead-letter/skipped），降低邮件丢失与业务阻塞的风险。
- **用户体验与隐私保护**：邮件偏好过滤、按 recipient locale 驱动多语言渲染；在已覆盖路径上通过错误脱敏与凭据隐藏避免日志/诊断信息泄露（见 `global_redactor` 与 fail-closed 设计及对应脱敏测试）。
- **运行期灵活性**：`mail.enabled`/provider/SMTP 参数/凭据/`template_*` 可零停机热加载；mailer 重建失败自动保留旧 mailer（fail-safe），支持 `mail.enabled` false→true 运行时重启用（见第 4 阶段 2026-06-23 落地）。
- **可维护性与扩展性**：与 mega 共享实体与 API（callisto + NotificationStorage）；provider 抽象为后续扩展（HTTP API provider）预留空间；与 config/vault 协同计划明确，降低后续维护成本（见阶段 3-5 与前置依赖矩阵）。

---

**参考**：
- mega 项目：`mono/src/email/mod.rs`、`mono/src/notification/{dispatcher,triggers}.rs`、common config 中的 `MailConfig`、jupiter callisto `email_jobs` 等实体及 NotificationStorage 实现。
- campsite：作为用户源后端，为邮件通知提供潜在的收件人邮箱与偏好数据源。
- monoengine 当前：`src/mail/mod.rs`、`src/config/model.rs`（MailConfig）、`src/notification/dispatcher.rs`、callisto 相关实体、`config/config.toml` 中的 `[mail]` 示例。
