# Website 邮件投递契约（墓碑）

> **墓碑：2026-09-19，plan-20260919 RM-01。** mega2 **不再发起产品邮件**。
> 本仓不是 website 内部邮件 API 的客户端，也不再编排、调用或配置任何发信路径。

认证事务邮件仍由 megaui / website 拥有；本仓不改 sibling。产品决议见
[`plan-20260919.md`](../plan/plan-20260919.md) ADR-RM-01 / ADR-RM-06。

核对日：**2026-09-19**。本文不再是跨服务发信契约的现行事实源。

---

## 现行边界

- mega2 不调用 `POST /api/internal/notifications/email`，也不调用任何其它发信 API。
- mega2 不恢复 SMTP、`[mail]`、`email_jobs` 或本仓 Mailer。
- 本仓出站只保留 generic webhook（`[notification.webhook]`）以及无关的
  `[storage_events]`。Slack / in-app / website-mail 按 plan-20260919 拆除。
- 未配置 webhook 时通知面静默（预期）。

本计划将删除的客户端配置键（RM-WM）：

| 键 | 说明 |
|---|---|
| `website_mail_base_url` | 原 website 内部邮件 API 基址 |
| `website_mail_bearer` | 原 IT 明文 bearer |
| `website_mail_bearer_ref` | 原生产 SecretRef |

删除后残留这些键会使 `config validate` 非零。

---

## 考古链接（非现行契约）

下列文档只记录 2026-07/08 的迁移史，**不得当作现行发信设计**：

- [`plan-20260731.md`](../plan/plan-20260731.md) ADR-WA-08 / MN-01：本仓 SMTP 退场、
  当时把产品邮件改走 website 内部 API。
- [`plan-20260802.md`](../plan/plan-20260802.md) WE-02..WE-05：website 侧内部邮件
  API 落地。该契约只约束当时的 website 实现与已删除中的 mega2 客户端。

历史请求形状、幂等、错误码与 compose 拓扑以版本控制中本文件的旧修订为准；
不要从本墓碑页复原客户端。
