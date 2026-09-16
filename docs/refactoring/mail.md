# Mail 模块（已废止）

> **废止：2026-08-01，MN-06。** mega2 不再实现、配置或运行 SMTP 邮件投递。

产品事件仍由 mega2 编排，并可继续写入 in-app 通知、发送 Slack 或 webhook；需要邮件时，
mega2 调用 website 的内部产品邮件 API。website 是唯一出站邮件发送方，负责模板、
provider、重试与 SMTP 捕获测试。

当前契约、环境拓扑和安全边界见 [`website-mail.md`](./website-mail.md)。特别是：

- mega2 没有 `[mail]` 配置、`mail.password` secret 或 `MEGA_MAIL__*` 环境变量；
- mega2 不连接、也不依赖 Mailpit；若 Compose 保留 Mailpit，唯一消费方是 website IT；
- website API 不可达时邮件为 best-effort 失败，不能阻断 in-app、Slack 或 webhook。

本文件取代旧的 `src/mail/`、SMTP dispatcher、`email_jobs` outbox 和 Mailpit 测试设计说明。
历史实现细节应通过版本控制查阅，不能作为当前配置、启动或 CI 门禁的依据。
