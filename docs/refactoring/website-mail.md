# Website 邮件投递契约

本文是 monoengine 产品事件邮件迁至 website 的单一事实源。决议来源：
[`docs/plan/plan-20260731.md`](../plan/plan-20260731.md) ADR-WA-08 / MN-01。

核对日：**2026-08-02**（CST）。本文件定义跨服务契约；website 内部 API 已由
[`plan-20260802.md`](../plan/plan-20260802.md)（WE-02..WE-05）落地，CI pin 见下文「实现基线」。

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
| 幂等 | 必填 `Idempotency-Key` 请求头；同一 key 的重放必须返回同一受理结果，不得重复发送。**key 必须由请求身份确定性推导**（见 §2.3），不得每次调用现取随机值——否则该保证形同虚设 |
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
| `payload` 中模板必填字段 | **必须是非空白字符串**。website 对 `trim()` 后为空的必填字段回 `422 invalid_payload`；调用方负责在业务值可能为空时（如纯空白的评论正文）替换为占位文案，而不是发出一个必然 422 的请求 |

模板归 website 管理。monoengine 不再渲染 mail TOML 模板、构造 HTML/text 正文或传递附件；
若未来确需预渲染内容或附件，必须修订本契约并单列 PII、大小限制和内容安全策略。

### 2.2 响应和错误

成功（首次受理或幂等重放）返回 `202 Accepted`（monoengine 客户端按「任意 2xx = 已受理」
宽松接收，因此前端把成功码改成其它 2xx 不会立刻打断投递，但仍属契约漂移，须先改本文）：

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
| 409 | `idempotency_conflict` | **仅**用于同 key 不同 body（指纹不匹配）的永久冲突。记录错误；不得换 key 重试 |
| 422 | `unsupported_event` / `invalid_payload` | 记录契约或模板配置错误 |
| 425 | `idempotency_in_progress` | 同 key 同 body 的并发请求仍在处理中（瞬时）。记录 debug/告警；**同一 key** 可稍后重试，不得换 key。本仓 best-effort，不建重试队列 |
| 429 | `rate_limited` | 记录告警；本计划不在本仓重试 |
| 5xx / 网络超时 | `upstream_unavailable`（或无 body） | best-effort 失败；in-app 等非邮件通道继续 |

website 负责在受理后可靠发送、去重与其自身 provider 失败处理。monoengine 不应把成功响应
理解为最终送达，也不得将 bearer、完整收件人或完整 payload 写入日志。

区分 409 与 425 的意义：两者都由同一个 `Idempotency-Key` 触发，但 409 是调用方 bug
（同 key 复用于不同内容），425 只是并发时序。把并发竞争报成 409 会让运维把一次良性
竞争读成契约违例。

### 2.3 Idempotency-Key 推导与幂等作用域

**key 推导（调用方契约）**：`Idempotency-Key` 必须由「本次投递的身份」确定性推导，
即 `event_type` + `recipient.username` + `recipient.email` + `locale` + `payload`
的稳定哈希。monoengine 实现见 `src/notification/website_mail.rs`
（blake3 over 规范化 JSON，取 hex）。「规范化」= **对象键递归排序**后自行渲染，
数组保持原序（有序数据，重排即不同投递）。不用 `serde_json::Value::to_string`：
它的键序取决于 `preserve_order` feature，而该 feature 是依赖树里的其它 crate
（`cedar-policy-core`）打开的，不在本仓控制之下；直接哈希 `to_string` 会让 key 空间
随一个我们不掌握的 feature 漂移，两个构建方式不同的副本会对「同一次投递」算出不同
的 key。

**关键约束：规范化后的字符串必须就是请求体本身**（monoengine 用 `.body(canonical)`
发送，而非 `.json(&value)`）。若只把规范化用于算 key、却按插入序发送 body，则
「key 相同」不再蕴含「字节相同」——website 是对**收到的 body** 取指纹的，同一个 key
就可能带着它已绑定到别的指纹的字节到达，把一次良性重放变成永久 `409`。

由此可得两条不变式：

- 同一逻辑投递无论被重放多少次，key 都相同 → website 侧只发一封；
- key 相同 ⟺ 所发送字节相同（key 就是这串字节的哈希）→ `409 idempotency_conflict`
  在 monoengine 这个调用方身上**结构上不可能**发生；它只会出现在 key 由其它调用方
  手工构造的场景。

代价是**payload 相同的重复通知会被折叠成一封邮件**。注意折叠判据是 payload，不是原始
业务内容，因此范围比「一字不差」更宽，务必按下面三类理解：

- 同一人对同一对象发了两条一字不差的评论 —— 折叠。这被认为优于「双击提交发两封」。
- 两条**不同**但前 500 字符相同的评论 —— 也会折叠：`comment_excerpt` 在
  `src/notification/triggers.rs` 按 `COMMENT_EXCERPT_MAX_CHARS = 500` 截断，截断后的
  payload 才是 key 的输入。空白正文被替换为占位文案后同理。
- `issue.closed` 与 `item.referenced` 的 payload 不含任何文本或事件判别位，因此
  「关闭→重开→再关闭」「重复引用同一对象」在 website 进程生命周期内只会发一封。

要区分这几类，必须先修订本节引入一个显式的事件唯一 id 维度（例如把 in-app 通知行 id
混入 key 的输入而不放进 payload）。在那之前，上述折叠是**已知且被接受**的行为。

**幂等作用域（website 侧现状，DEFER-WE-01）**：当前实现是**进程内内存** Map
（`libs/email/internal/idempotency-store.ts`），因此幂等保证的作用域是
「单个 website 进程的生命周期」：进程重启、或多副本部署下请求落到不同副本时，
同一 key 会被当作新请求再发一次。IT 栈是单副本，故 `integration_website_mail`
能稳定验证该语义。跨副本/持久化的幂等存储是 DEFER-WE-01，落地前不得在本表把
幂等宣称为跨进程保证。

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
DEP-06（website 内部邮件 API）已由 [`plan-20260802.md`](../plan/plan-20260802.md) 交付；
不得恢复本仓 SMTP 作为替代路径。

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
they all use `deliver_user_notification`. Local wire mocks and the compose
stack test (`integration_website_mail`, WEBSITE_IT=1) cover the request shape
and acceptance against website tip `a52d703` (see 实现基线).

### 实现基线（plan-20260802）

核对日：**2026-08-02**。本短节记录契约冻结项与已落地的 website tip / CI pin。

| 冻结项 | 取值 |
|---|---|
| 方法与路径 | `POST /api/internal/notifications/email` |
| 鉴权 | `Authorization: Bearer <shared-internal-bearer>`（website env：`MONOENGINE_INTERNAL_MAIL_BEARER`；monoengine IT：`MEGA_NOTIFICATION__WEBSITE_MAIL_BEARER`） |
| 幂等头 | 必填 `Idempotency-Key` |
| 请求 JSON 字段 | `event_type`、`recipient.username`、`recipient.email`、`locale`、`payload`（见上文 §2.1） |
| 成功响应 | `202` + `{ delivery_id, accepted, duplicate }` |
| 错误 `code` | `invalid_request` / `unauthorized` / `idempotency_conflict` / `idempotency_in_progress`(2026-08-21 新增) / `unsupported_event` / `invalid_payload` / `upstream_unavailable`（见 §2.2）。`forbidden` 与 `rate_limited` 为**保留未实现**——前端从未返回过 403/429，monoengine 侧的分类分支相应为死分支 |
| 产品 `event_type` allowlist | `cl.comment.created`、`cl.merged`、`issue.comment.created`、`issue.closed`、`item.referenced`（`src/notification/triggers.rs`） |
| CI website pin（`config-validation.yml` checkout） | `a52d70362586ae5e171be5db2e5c5457d07ce366`（核对日快照；**现行 pin 见下方 2026-08-06 增补**） |
| website 工作分支 | `monoengine` |
| website tip（REL-WE-SITE） | `a52d70362586ae5e171be5db2e5c5457d07ce366`（含 WE-02..WE-05） |

> **增补（2026-08-06 完成度复审）：** `config-validation.yml` 的 website checkout pin 已于 plan-20260803 期间（monoengine `39f7433`，website IT 切换 Postgres）bump 为
> `2af89c874646005dd1a550053b5068f19bb7478a`（`a52d703…` 的直接子提交，仍含 WE-02..WE-05），并已同步 `website-auth.md` / `test-infra.md`；本表「核对日 2026-08-02」各 pin 行为历史快照。现行 pin 的唯一事实源是 workflow 文件本身与 `website-auth.md` §pin 政策。

> **历史快照（2026-08-21 前端仓库改指）：** 当时联调栈的前端由 `genedna/website` 改指
> **`gitmono-dev/monoui`** 的 `monoengine` 分支（sibling `../monoui`）；WE-02..WE-06
> 的路由 / Bearer / 五类产品模板 / 幂等 / `EMAIL_PROVIDER=test` 已自
> `genedna/website@2af89c8` 逐文件移植到 monoui，移植当日接口契约与本文各表**逐项不变**
> （路径、bearer 头、`Idempotency-Key`、`202 {delivery_id, accepted, duplicate}`、
> `code` 取值、`event_type` allowlist 全部一致）。**同日稍后有一处刻意分歧**：
> 并发同 body 在途从 `409` 改为新增的 `425 idempotency_in_progress`（见 §2.2；
> monoui `05ba97a`），`genedna/website` 侧没有该取值。本表内所有 `genedna/website`
> pin / tip 行自此为**历史快照**；现行 pin 的唯一事实源仍是 workflow 文件与
> `website-auth.md` §头部。

客户端与契约：**无已知冲突**（`WebsiteMailClient` 字段与上表一致）。

> **现行实现（2026-09-04）：** Compose web profile 已改用 sibling `../megaui` 的
> `apps/web`；上述 monoui revision 与路径仅用于说明历史迁移。邮件路由、Bearer、模板、
> 幂等与 `EMAIL_PROVIDER=test`/`smtp` 契约保持不变，Compose IT 仍注入 test provider（WE-06）。

**客户端 vs 契约：** `WebsiteMailClient`（`src/notification/website_mail.rs`）路径、Bearer、`Idempotency-Key`、JSON 字段与上文一致；**无已知冲突**。

**website `@libs/email` 现状：** 产品五事件模板、内部路由、幂等、`EMAIL_PROVIDER=test` 与最小 `smtp` 均已落地（plan-20260802 WE-02..WE-05；tip `a52d703`）。

**幂等存储介质（plan-20260802 / WE-04）：** website 进程内 `MemoryIdempotencyStore`（`(Idempotency-Key) → {delivery_id, fingerprint, state}`）；无 SQLite 表。多实例共享见 `DEFER-WE-01`。

### tip 对照（WE-07）

| 仓 | tip / pin | 说明 |
|---|---|---|
| website `monoengine` | `a52d70362586ae5e171be5db2e5c5457d07ce366` | REL-WE-SITE；CI checkout 收口日同 SHA（2026-08-06 起 pin 已 bump 为其子提交 `2af89c8…`，见上方增补） |
| monoengine `main`（WE-06 + REL-WE-SITE 计划收口） | `3b8490c`（含 `3c2b9d1` pin/IT + plan REL-WE-SITE-R2） | compose/`config-validation`/文档 pin 同步 |

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
