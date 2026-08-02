# Website 邮件投递契约

本文是 monoengine 产品事件邮件迁至 website 的单一事实源。决议来源：
[`docs/plan/plan-20260731.md`](../plan/plan-20260731.md) ADR-WA-08 / MN-01。

核对日：**2026-08-01**（CST；UTC 日历日可能仍为 2026-07-31）。本文件定义跨服务契约，**不**实现 website API、monoengine
客户端或 Compose 改动；分别由 DEP-06、MN-05、MN-06 承接。

---

## 1. 职责与边界

website 是唯一的**出站邮件发送方**：

- website 自己负责认证事务邮件，以及由 monoengine 产品事件触发的通知邮件。
- monoengine 仍保留 CL、issue、reference 等事件编排、收件人偏好判定和 in-app /
  Slack / webhook 通道；它不再直接连接 SMTP、Resend 或 Cloudflare。
- monoengine → website 是内部服务调用，不是浏览器 API；不得使用 Better Auth session
  cookie 或浏览器用户 bearer。

认证事务邮件与产品通知邮件必须分开：

| 类别 | 触发方 | 发送实现 | 本契约 |
|---|---|---|---|
| 认证事务 | website（验证、重置密码等） | website `@libs/email` | 不经 monoengine |
| 产品通知 | monoengine 事件触发器 | website `@libs/email` + 产品模板 | 本文定义 |

chat mention/reply 邮件**不迁移**。chat 产品面与其触发器随 RM-03 退场。

---

## 2. 内部邮件 API

### 2.1 请求

| 项 | 契约 |
|---|---|
| 方法与路径 | `POST /api/internal/notifications/email` |
| 调用方 | monoengine notification client（MN-05） |
| 鉴权 | `Authorization: Bearer <shared-internal-bearer>` |
| 幂等 | 必填 `Idempotency-Key` 请求头；同一 key 的重放必须返回同一受理结果，不得重复发送 |
| Content-Type | `application/json` |
| 超时与重试 | monoengine 使用有界 HTTP 超时；本计划为 best-effort，不在本仓建立重试 outbox |

共享 bearer 是服务间 secret，必须通过 monoengine 的 SecretRef / website 的受控环境注入；
不得写入 `config/config.toml`、Compose 提交文件、日志或错误回显。website 必须将此路由
与公网用户路由隔离，并以 constant-time secret 比较验证 bearer。

请求 JSON：

```json
{
  "event_type": "cl.comment.created",
  "recipient": {
    "username": "alice",
    "email": "alice@example.test"
  },
  "locale": "zh-CN",
  "payload": {
    "cl_link": "CL-123",
    "actor_username": "bob",
    "comment_excerpt": "Please review the latest change.",
    "resource_url": "https://app.example.test/cl/CL-123"
  }
}
```

字段规则：

| 字段 | 规则 |
|---|---|
| `event_type` | 必填、稳定的通知事件 code；website 按它选择产品模板 |
| `recipient.username` | 必填，供审计与模板上下文；不作为 email 地址替代 |
| `recipient.email` | 必填，最终投递地址 |
| `locale` | 必填 BCP 47 locale；未知值由 website 回退至其默认 locale |
| `payload` | 必填对象；仅事件模板所需的最小、已转义前的业务字段；不得传密码、token、完整 session cookie 或任意 HTML |

模板归 website 管理。monoengine 不再渲染 mail TOML 模板、构造 HTML/text 正文或传递附件；
若未来确需预渲染内容或附件，必须修订本契约并单列 PII、大小限制和内容安全策略。

### 2.2 响应和错误

成功（首次受理或幂等重放）返回 `202 Accepted`：

```json
{
  "delivery_id": "01J...",
  "accepted": true,
  "duplicate": false
}
```

`duplicate: true` 表示相同 `Idempotency-Key` 已受理；`delivery_id` 必须稳定。错误响应使用：

```json
{
  "code": "invalid_request",
  "message": "recipient.email is required"
}
```

| 状态 | `code` | monoengine 行为 |
|---|---|---|
| 400 | `invalid_request` | 记录脱敏错误；不阻断业务请求 |
| 401 | `unauthorized` | 记录配置/secret 告警；不回退 SMTP |
| 403 | `forbidden` | 记录调用方授权告警；不回退 SMTP |
| 409 | `idempotency_conflict` | 记录错误；不得换 key 重试 |
| 422 | `unsupported_event` / `invalid_payload` | 记录契约或模板配置错误 |
| 429 | `rate_limited` | 记录告警；本计划不在本仓重试 |
| 5xx / 网络超时 | `upstream_unavailable`（或无 body） | best-effort 失败；in-app 等非邮件通道继续 |

website 负责在受理后可靠发送、去重与其自身 provider 失败处理。monoengine 不应把成功响应
理解为最终送达，也不得将 bearer、完整收件人或完整 payload 写入日志。

---

## 3. Monoengine 迁移边界

### 删除（MN-02..MN-04）

以下是既有本仓邮件投递表面，完成迁移后删除：

- `src/mail/`、孤儿 `src/email/`、`MailConfig` / `[mail]` / `mail.password` secret、
  SMTP/HTTP mail provider 与仅为其服务的 `lettre` 依赖；
- `EmailChannel`、`EmailDispatcher`、`email_jobs` outbox、
  `email_job_attachments` 及其 callisto/storage 实体；
- admin `/email-jobs/*` 与 `/mail-templates*` API、OpenAPI 表面、模板渲染与
  `enqueue_email_job` 路径；
- 仅验证 monoengine `SmtpMailer → Mailpit` 的测试、CI 门和运维说明。

`email_jobs` / `email_job_attachments` 的 DROP 由 MN-04 单独实施，顺序为 attachments
后 jobs，迁移 forward-only、`down` no-op；历史创建 migration 保留。

### 保留

- 产品事件触发与事件类型：CL comment/merge、issue comment/close、item referenced；
- 用户通知偏好与 `should_send` 判定；
- `user_inbox_notifications`、in-app 通道，以及已配置的 Slack / webhook 通道；
- `/api/v1/user` 通知偏好 API（其邮件语义按下节迁移）。

### Dispatcher primary 迁移

当前 `EmailDispatcher` 以 email 为 primary，成功后才扇出 in-app/Slack/webhook。这是迁移
阻塞点：MN-02 必须先令 `NotificationService` 无 `[mail]` 仍可启动，并把 primary 改为
**`in_app`**。Slack/webhook 保持独立的 best-effort 通道，不能再被 email 成功门控。

随后 MN-05 在偏好要求邮件时调用本文 API；website 4xx/5xx 或网络失败只记录脱敏
warn/error，不使原始 CL/issue HTTP 请求失败，也不得阻止 in-app 写入。

### `delivery_mode=email` 兼容映射

迁移期间，已有或新设的 `delivery_mode=email` 表示：

1. 在 `should_send` 允许时请求 website 产品邮件；并且
2. 同时写入 in-app 通知。

它不是 monoengine SMTP 开关。未知或无效 delivery mode 仍按现有校验拒绝；MN-02 必须为
该映射添加 focused test。没有独立弃用窗口，兼容性变更由 ADR-WA-06 / REL-01 minor
发布说明集中告知。

---

## 4. Compose、Mailpit 与环境拓扑

当前拓扑（MN-05/MN-06 已落地；以 `docker-compose.test.yml` 与 [`test-infra.md`](./test-infra.md) 为准）：

```text
monoengine (profile app)
  ├─ in-app / Slack / webhook（本仓保留）
  └─ POST website-next /api/internal/notifications/email
       Authorization: shared internal bearer
             │
             └─ website @libs/email ──SMTP（IT 可选）──> mailpit:1025
```

| 组件 | 现行约定 |
|---|---|
| `monoengine` | **零** `MEGA_MAIL__*`，且不 `depends_on: mailpit`。客户端配置键：**`notification.website_mail_base_url`**（scheme+host[:port]，无 path；容器内例 `http://website-next:7001`）与 **`notification.website_mail_bearer_ref`**（`vault://…` SecretRef，或 IT 明文 **`notification.website_mail_bearer`** 仅限测试）。环境变量覆盖：`MEGA_NOTIFICATION__WEBSITE_MAIL_BASE_URL`、`MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER` / `…_BEARER_REF`。 |
| `website-next` | `web` profile 的唯一邮件发送者；IT 若需捕获邮件，注入其 SMTP 或既有测试-provider 配置，使其连接 `mailpit:1025`。接收内部邮件时读取并校验 `MONOENGINE_INTERNAL_MAIL_BEARER`，其值须与 monoengine 的 `MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER` 相同。 |
| `mailpit` | 默认保留在数据面；**消费方 = website IT，不是 monoengine**。`config-validation.yml` 不要求本仓 SMTP 成功路径。 |
| 会话 IT | `website-next` 同时承载 Better Auth 与内部邮件 API；会话基址仍见 [`website-auth.md`](./website-auth.md)，邮件 API 使用独立 bearer，不复用 session cookie |
| `.env.test*` / CI | 仅使用公开 IT 值与占位 secret；`MAILPIT_*` 注释为 website 捕获用途；含 `MEGA_NOTIFICATION__WEBSITE_MAIL_*` 占位；不再 seed `mail.password`；CI 不跑本仓 SmtpMailer→Mailpit 门 |

mailpit 端口与服务登记以 [`test-infra.md`](./test-infra.md) 为准。服务同时启用的基线命令：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web up -d --wait
```

---

## 5. 相关文档（现行事实源）

| 产物 | 状态 |
|---|---|
| [`notification.md`](./notification.md) | 已去 email-primary / outbox / SmtpMailer；保留 in-app/Slack/webhook 与 website 调用 |
| [`mail.md`](./mail.md) | 已废止，改链本文；不再作为 monoengine SMTP 设计事实源 |
| [`test-infra.md`](./test-infra.md) | mailpit 消费方 = website IT；CI 不要求本仓 SMTP |
| [`integration.md`](./integration.md) | 无本仓 SMTP→Mailpit 必经断言；website API mock/IT 覆盖 |
| [`config.md`](./config.md) 与 [`../development.md`](../development.md) | 无 `[mail]` / `mail.password` / 本仓 SMTP 运行前提 |
| 根 `README.md` | 无本仓 `[mail]` / SMTP 操作说明；改引本契约 |

实施顺序固定为：MN-02 解除 email-primary → MN-03 删除邮件模块/API → MN-04 DROP
outbox 表 → MN-05 接 website client → MN-06 收口 Compose、环境、CI、测试和文档。
DEP-06 未就绪时 MN-05/MN-06 为 blocked；不得恢复本仓 SMTP 作为替代路径。

### MN-05 implementation note

The monoengine client lives at `src/notification/website_mail.rs`. It is built
only when `notification.website_mail_base_url` and exactly one bearer source
are configured. `website_mail_bearer_ref` is required for production and must
point under `notification/website_mail/bearer`; `website_mail_bearer` is an
IT-only literal secret. The client uses a three-second, redirect-free
best-effort request with an `Idempotency-Key`; network and non-2xx failures
are logged without the bearer, recipient address, or payload.

For every enabled `delivery_mode=email` product event, monoengine writes the
in-app notification first, then POSTs to the website API. This covers the
existing CL comment/merge, issue comment/close, and reference triggers because
they all use `deliver_user_notification`. The local wire mock verifies the
request and the concurrent in-app write while DEP-06's website API is not yet
available in this checkout.

### 实现基线（plan-20260802 / WE-01）

核对日：**2026-08-02**。本短节冻结 `plan-20260802.md` 执行用契约与 pin，不宣称 website API 已实现。

| 冻结项 | 取值 |
|---|---|
| 方法与路径 | `POST /api/internal/notifications/email` |
| 鉴权 | `Authorization: Bearer <shared-internal-bearer>`（website env：`MONOENGINE_INTERNAL_MAIL_BEARER`；monoengine IT：`MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER`） |
| 幂等头 | 必填 `Idempotency-Key` |
| 请求 JSON 字段 | `event_type`、`recipient.username`、`recipient.email`、`locale`、`payload`（见上文 §2.1） |
| 成功响应 | `202` + `{ delivery_id, accepted, duplicate }` |
| 错误 `code` | `invalid_request` / `unauthorized` / `forbidden` / `idempotency_conflict` / `unsupported_event` / `invalid_payload` / `rate_limited` / `upstream_unavailable`（见 §2.2） |
| 产品 `event_type` allowlist | `cl.comment.created`、`cl.merged`、`issue.comment.created`、`issue.closed`、`item.referenced`（`src/notification/triggers.rs`） |
| CI website pin（`config-validation.yml` checkout） | `a52d70362586ae5e171be5db2e5c5457d07ce366` |
| website 工作分支 | `monoengine` |
| 建议开发基线 tip（2026-08-02） | `a52d70362586ae5e171be5db2e5c5457d07ce366`（与 CI pin 相同；含内部邮件 API WE-02..WE-05） |

**客户端 vs 契约：** `WebsiteMailClient`（`src/notification/website_mail.rs`）路径、Bearer、`Idempotency-Key`、JSON 字段与上文一致；**无已知冲突**，不阻断 WE-02。

**website `@libs/email` 现状：** 产品五事件模板已由 plan-20260802 WE-03 落地；`smtp` provider 仍为 stub（WE-05）；内部路由已存在。

**幂等存储介质（plan-20260802 / WE-04）：** website 进程内 `MemoryIdempotencyStore`（`(Idempotency-Key) → {delivery_id, fingerprint, state}`）；无 SQLite 表。多实例共享见 `DEFER-WE-01`。

---

## 6. 兼容与安全

- 本迁移删除 `[mail]`、SMTP、`/admin/email-jobs`、`/admin/mail-templates` 与
  `email_jobs` 表；breaking change 由 ADR-WA-06 / REL-01 minor 发布。
- 内部 bearer 仅授予此单一邮件路由的调用权限；轮换时应支持短暂双 key 接受窗口，且不记录
  key 值。
- 事件 payload 和收件人属于 PII：传输使用受保护服务网络或 TLS，日志只可记录 event type、
  status、脱敏 recipient 和 delivery/idempotency 标识。
- website API 不可达时允许产品邮件丢失，但 in-app 通知仍应可用；本计划不引入 monoengine
  本地重试队列（DEFER-WA-08）。
