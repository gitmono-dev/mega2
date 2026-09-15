# Storage-only 提交后出站事件

本文是 monoengine **storage-only** 形态下 `[storage_events]` 静态配置的事实源。产品边界与任务追溯见 [`../plan/plan-20260912.md`](../plan/plan-20260912.md)。

> **准备接口（WH-01/WH-09）：** 配置表面 + 可独立调用的 HTTPS HMAC 运输组件。运输**尚未**注入 AppContext / Storage，不能声称 enabled 已有投递能力。HMAC `secret_ref` 在 disabled 时不解析。

## 配置

`[storage_events]` serde 缺省：

| 字段 | 缺省 | 作用 |
|---|---|---|
| `enabled` | `false` | 出站开关；启用要求 storage-only |
| `installation_id` | 省略 | 启用时必填；1..64 ASCII `[A-Za-z0-9_-]`；部署侧生成并持久写入，重启不得变 |
| `max_in_flight` | `16` | 后续运行时在途上限（本卡只装载） |
| `connect_timeout_seconds` | `2` | 连接超时，范围 1..=5 |
| `request_timeout_seconds` | `5` | 含 DNS 的整段请求超时，范围 1..=10 |
| `shutdown_grace_seconds` | `5` | 后续关停宽限（本卡只装载） |
| `targets` | `[]` | 静态接收者；enabled 且为空合法（无接收者） |

`[[storage_events.targets]]` 在该项存在时必填 `id`、`url`、`secret_ref`、非空 `events`。`id` 为 1..32 ASCII `[A-Za-z0-9_-]`，不得重复。过滤数组缺省为空（空集合表示该来源不订阅，不是通配）。`url` 必须是 HTTPS，禁止 userinfo / query / fragment（disabled 同样拒绝非法形状）。事件字面量与 canonical 路径由后续卡校验。

## 运输（WH-09，未注入应用）

`HttpsEventTransport` 对每个 target 单次 POST，不重试（含 429/5xx）。禁止跟随 redirect，禁用环境代理。签名头：

- `X-Mega2-Event-Id`
- `X-Mega2-Timestamp`（UTC Unix 秒）
- `X-Mega2-Signature: sha256=<hex>`

HMAC-SHA256 输入为 `timestamp` 十进制秒、`.`、实际发送 body bytes。密钥必须是已解析 `SecretString` 的 `hex:<even-hex>`，解码后 32..=256 bytes。生产客户端没有 HTTP/私网逃逸开关。日志只记 target id / event type / 结果类别，不含 URL、secret、请求响应体或 reqwest 原文。

## 事件投影与过滤（WH-10）

`CommittedEvent` 只能表达六种冻结 `event_type`。投影结果为固定 envelope：`schema_version=1`、`event_id`、`event_type`、`occurred_at`、`source`、`scope{tenant_id,repo_path,oci_repository}`、`data`。超过 16 KiB 整事件丢弃，不截断。不含 actor、raw、object key、URL。

路径过滤：仅 `/` 可表示全树；其它过滤为 `p==f` 或 `p.starts_with(f + "/")`。非法 canonical path 丢弃。OCI 精确匹配；Agent 为 tenant 集合与 repo 集合的交集；LFS unscoped 仅 `include_unscoped_lfs=true`。

## 地址策略（WH-12）

每次 POST 只解析一次 DNS。解析结果必须全部为公共地址：loopback、RFC1918 私网、link-local、metadata（`169.254.169.254` / `fd00:ec2::254`）以及混合公共+受限结果一律拒绝。连接钉扎到本轮已验证的 IP，TLS SNI / hostname 仍使用原域名，发送时不再解析。生产路径没有把公共 IP 映射到本机的开关；测试用注入 resolver/pin。

`config/config.toml` 只保留注释块。`config/config-storage-only.toml` 写 `[storage_events] enabled = false` 占位，**禁止**提交可用 HMAC secret。

全部 `[storage_events]` 字段均为 restart-required：热更新只报告、不把候选值写入现有 Config 快照。

## 形态门

| 形态 | `enabled=true` |
|---|---|
| review（无 `git.push_auth`） | `config.validate` Err |
| storage-only `push_auth=token` | 接受（仍须 `installation_id`） |
| storage-only `push_auth=none` | 接受（仍须 `installation_id`） |

disabled 仍拒绝未知字段、重复 target id、非法 id 字符与空 `events`；跳过 SecretRef 解析与连通性检查。
