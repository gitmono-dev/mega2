# Mail 模块（已废止）

> **废止：2026-08-01，MN-06；拆除收口：2026-09-19，plan-20260919 RM-04。**
> mega2 不再实现、配置或运行 SMTP 邮件投递，也不再作为 website 内部邮件
> API 的客户端。

现行发信边界见墓碑 [`website-mail.md`](./website-mail.md)。本仓出站只剩
generic webhook（`[notification.webhook]`）以及无关的 `[storage_events]`。

特别是：

- mega2 没有 `[mail]` 配置、`mail.password` secret 或 `MEGA_MAIL__*` 环境变量；
- mega2 不连接、也不依赖任何 SMTP 捕获容器；本仓 compose 数据面不启动该类服务；
- 未配置 webhook 时通知面静默（预期）。

本文件取代旧的 `src/mail/`、SMTP dispatcher、`email_jobs` outbox 和本仓
SMTP 捕获测试设计说明。历史实现细节应通过版本控制查阅，不能作为当前配置、
启动或 CI 门禁的依据。
