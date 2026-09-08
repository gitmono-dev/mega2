# Website 会话接入（Better Auth → monoengine）

本文是 monoengine **浏览器会话**接入的单一事实源。决议来源：
[`docs/plan/plan-20260731.md`](../plan/plan-20260731.md) ADR-WA-01..04、ADR-WA-07。
邮件投递迁出见计划 ADR-WA-08 / 任务 MN-01，事实源为
[`website-mail.md`](./website-mail.md)。

> **术语**：本文中 *website* 指 monoengine 的**前端 / 认证面实现**，当前为 sibling
> `../megaui` 的 `apps/web`。Compose 服务名 `website-next` / `website-db-init`、
> 隔离账户库名 `website`、以及 `MEGA_OAUTH__WEBSITE_*` /
> `MEGA_NOTIFICATION__WEBSITE_MAIL_*` 配置键**均保持不变**——它们是 Rust 结构体
> 派生的配置面与 Compose 内部 DNS，改名会让会话路径 fail-closed 成 401。

核对日：**2026-09-04**。前端 / 账户仓库 sibling：`../megaui`（`apps/web`）。IT
Postgres schema 初始化使用其 `apps/web/Dockerfile` 的 `db-init` target：先幂等创建
`website` 数据库，再在 `/app/packages/database` 执行 `pnpm exec drizzle-kit migrate`。
Compose 中的 Dockerfile target 与命令是此处的实施事实；契约漂移时先改本文再改代码。

---

## 1. 信任路径

浏览器身份**只**信任 megaui Better Auth。monoengine **不签发** session cookie，
**不**直连读取 megaui 的 user/session 表。

```text
Browser
  │ Cookie: better-auth.session_token
  │     或  __Secure-better-auth.session_token
  ▼
monoengine HTTP (/api/v1/*)
  │ SessionUser extractor
  │ 读 Cookie → 选中已配置 cookie 名之一
  ▼
WebsiteSessionStore::load_user
  │ GET {oauth.website_api_base_url}/api/auth/get-session
  │ Header Cookie: <完整「名=值」（signed cookie 原样转发）>
  ▼
Better Auth get-session JSON
  │ user / session；空 body / 非 2xx / banned → 拒绝
  ▼
映射 LoginUser（见 §2）→ 注入请求
```

失败一律 `AuthRedirect`（HTTP 401，正文 `Login first`），**禁止**回退到硬编码
`admin` / `admin@gitmono.test` stub。

生产路径**禁止**：

- 配置开关或环境变量旁路 get-session（「debug 默认 admin」等）
- 恢复 `CampsiteApiStore` / `api_store_backend` 双后端
- 在 monoengine 签发 JWT 冒充 website 会话（见计划 DEFER-WA-01）

测试**仅允许**注入 `FixedUserSessionStore`（或等价 test double），且不得编译进
默认 `service http` 生产二进制路径。

### Cookie 名

默认（`oauth.session_cookie_names`，AU-02 落地）按优先级尝试：

1. `better-auth.session_token`（HTTP / 非 Secure 部署常见）
2. `__Secure-better-auth.session_token`（HTTPS Secure 前缀）

请求中需转发**完整** `name=value` 字符串（含 signature），不得只转发裸 token。

### CORS / cookie 域

- `oauth.allowed_cors_origins` 必须包含前端 origin，且浏览器请求带 credentials。
- 跨站时需同站父域、反代同宿主、或显式 CORS + `SameSite` 策略；IT 同栈用容器网络
  URL（见 §5），宿主浏览器联调常用 `http://127.0.0.1:17001` ↔
  `http://127.0.0.1:19180`。
- 日志 **不得**打印完整 session cookie / token（redaction）。

---

## 2. 字段映射（ADR-WA-02）

| Website `user` | `LoginUser` | 用途 |
|---|---|---|
| `id` | `website_user_id`（由遗留 `campsite_user_id` 重命名） | GPG 等外部 id |
| `name` | `username` | Git actor、SSH/token/CLA/prefs、Cedar principal |
| `email` | `email` | 通知 settings 首次创建等 |
| `image` | `avatar_url` | `/api/v1/user` 回显 |
| `banned` | （不入结构体） | `true` → 拒绝会话 |
| `role` | （不入结构体） | 管理权仍以 `mono.admin` + Cedar 为准 |

回退：`name` 为空 → 用 `email` 的 `@` 前 local-part；仍空 → 拒绝会话并 `warn`。

**运营约束：** website 无独立 `username` 列；展示名碰撞可能导致 monoengine actor
混同。唯一性由 website 展示名治理负责；本仓**不**新增 users 映射表。若日后
website 提供稳定 unique handle，修订本表（ADR revisit）。

---

## 3. 双轨认证（ADR-WA-03）

| 通道 | 机制 | 状态 |
|---|---|---|
| `/api/v1` 浏览器 | Cookie → get-session → `SessionUser` | 本计划落地 |
| API Bearer | Mono `access_token` → `AccessTokenUser` | **保留** |
| Bot | `bot_` Bearer → `BotIdentity` | **保留** |
| Git HTTP / LFS | Bearer 或 Basic password = token | **保留** |
| SSH | 公钥 → `ssh_keys.username` | **保留** |

**浏览器会话 ≠ Git 凭据。** 登录 website 后仍须在 monoengine
`/api/v1/user` 下创建 access token 或登记 SSH key，Git/LFS/SSH 才可用。

`/api/v1/user` 的 SSH / token / CLA / 通知偏好 API **保留**；仅会话来源改为
website（或显式 `AccessTokenUser`）。

---

## 4. `[oauth]` 配置键（ADR-WA-04）

| 键 | 含义 |
|---|---|
| `allowed_cors_origins` | CORS allowlist（已有） |
| `website_api_base_url` | get-session 基址；`service http` **必填**，validate fail-closed |
| `session_cookie_names` | Cookie 名列表；默认见 §1 |

删除遗留键（不得再出现于有效配置）：`campsite_api_domain`、`tinyship_api_domain`、
`api_store_backend`。

环境变量覆盖示例（compose IT）：

- `MEGA_OAUTH__WEBSITE_API_BASE_URL=http://website-next:7001`
- `MEGA_OAUTH__ALLOWED_CORS_ORIGINS=http://local.gitmega.com,https://app.gitmega.com,http://app.gitmono.test,http://127.0.0.1:17001`
  （保留示例配置中既有的开发 origin，并加入 website-next 宿主映射 origin）

---

## 5. Compose 同栈拓扑（ADR-WA-07）

形态对标 Mega demo「前端 + 后端 + 数据面」，资产是 **megaui `apps/web`**，
**不是** moon/campsite。

| 项 | 值 |
|---|---|
| Compose 项目 | `-p monoengine-it` |
| 文件 | `docker-compose.test.yml` |
| 服务名 | `website-next`（另有 `megaui-collab`） |
| Profile | `web`（默认 `up -d --wait` **不**拉起） |
| Build | `website-next` / `website-db-init`：context `../megaui`，dockerfile `apps/web/Dockerfile`；`megaui-collab`：`apps/collab-server/Dockerfile` |
| 网络 | `monoengine-test-network` |
| 容器端口 | `7001` |
| 宿主映射 | `127.0.0.1:17001:7001` |
| 协作 WebSocket | `ws://127.0.0.1:17002` → `megaui-collab:7002` |
| 账户库 | IT 默认共享 `postgres` 服务上的独立库 **`website`**（`DB_DIALECT=pg`）；与 monoengine 业务库 **`monoengine` 隔离** |
| monoengine 基址（容器内） | `http://website-next:7001` |
| 宿主浏览器 origin | `http://127.0.0.1:17001` |
| monoengine HTTP（profile `app`） | `127.0.0.1:19180` → 容器 `8000` |

联调：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web up -d --wait
```

`monoengine` 不声明对 `website-next` 的 Compose `depends_on`：两个服务分别属于
`app` / `web` profile；若仅选择 `app`，该跨 profile 依赖会使 Compose 拒绝配置，
并破坏既有的无 website 的 app smoke。上述联合命令会等待两者健康。需要保证第一次
会话请求也发生在 megaui 已就绪后时，按以下顺序启动：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile web up -d --wait website-next
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app up -d --wait monoengine
```

在 `website-next` 尚未健康时发出的 get-session 请求按 §6 fail-closed；待健康后由
客户端重新请求，不在 monoengine 会话路径中作无界重试。连通性冒烟可从共享网络运行：

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web exec -T monoengine \
  curl -fsS http://website-next:7001/api/auth/get-session
```

栈级验收（ITW-03）：megaui sign-up/sign-in → session cookie →
`GET http://127.0.0.1:19180/api/v1/user` → 断言 `username` /
`website_user_id` 与 get-session 一致；无 cookie → 401。

登记与 CI 入口须同步 [`test-infra.md`](./test-infra.md)（ITW-01）。
前置：sibling checkout `../megaui`；缺失则 ITW blocked。

与 Mega demo 差异：IdP/前端是 megaui web + Better Auth，无 MySQL campsite；
monoengine 业务库仍为本仓 Postgres。

---

## 6. 失败语义

| 条件 | 结果 |
|---|---|
| 无匹配 session cookie | `AuthRedirect` |
| get-session 非 2xx / 超时 | `AuthRedirect`（有界超时，建议 ≤ 3s，AU-03 写定） |
| body 空 / 无 `user` | `AuthRedirect` |
| `user.banned == true` | `AuthRedirect` |
| `name` 与 email local-part 皆不可用 | `AuthRedirect` + `warn` |
| website 不可达 | 浏览器会话失败；**Git token / SSH 轨仍可用**（ADR-WA-03） |

---

## 7. 测试 double 与栈级 IT

| 层级 | 做法 |
|---|---|
| 单元 / 模块 | `FixedUserSessionStore`（或等价）注入已知 `LoginUser`；覆盖 banned、空 name、无 cookie |
| 栈级 | `--profile app --profile web` + `integration_website_auth`（ITW-03）；需真实 Better Auth cookie |
| 禁止 | 用 FixedUser 冒充 ITW-03 完成；生产旁路开关 |

---

## 8. 与 chat / 附件表的边界

- Campsite 风格 **chat / Notes** 产品面在 website；本仓整栈删除见计划 RM 链 /
  ADR-WA-05。本文**不**规定 DROP 清单。
- chat 域表 `attachments` ≠ 邮件 outbox 表 `email_job_attachments`。
  后者随邮件退场（ADR-WA-08 / MN-04）DROP；**本会话文档不要求永久保留**邮件表。

---

## 9. 兼容（ADR-WA-06 / REL-01 minor）

本计划无单独弃用窗口。与会话相关的 breaking 面：

- 浏览器会话仅信任 website Better Auth cookie → `get-session`；**无 cookie 不再隐式 admin**
- JSON / 存储字段 `campsite_user_id` → `website_user_id`
- 公开 `/api/v1/chat/*`、notes sync、`chat-migrate` 子命令与本仓 chat/notes 产品面移除（见 RM 链）

邮件投递 breaking 见 [`website-mail.md`](./website-mail.md) §6。

---

## 10. 相关文档

- 计划：[`docs/plan/plan-20260731.md`](../plan/plan-20260731.md)
- 测试基建：[`test-infra.md`](./test-infra.md)
- 集成矩阵：[`integration.md`](./integration.md)
- 配置：[`config.md`](./config.md)
- 邮件投递契约：[`website-mail.md`](./website-mail.md)
