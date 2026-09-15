# Storage-only 提交后出站事件

本文是 monoengine **storage-only** 形态下 `[storage_events]` 静态配置的事实源。产品边界与任务追溯见 [`../plan/plan-20260912.md`](../plan/plan-20260912.md)。

> **准备接口（WH-01）：** 本卡只交付可校验、重启生效的配置表面。默认 `enabled=false`。`enabled=true` 且非 `git.storage_only()` → `config.validate` 拒绝。`git.push_auth` 为 `token` 或 `none` 时均可启用。本卡**不**绑定运输、运行时或来源 hook，不能声称 enabled 已有投递能力。HMAC `secret_ref` 在 disabled 时不解析。

## 配置

`[storage_events]` serde 缺省：

| 字段 | 缺省 | 作用 |
|---|---|---|
| `enabled` | `false` | 出站开关；启用要求 storage-only |
| `installation_id` | 省略 | 启用时必填；1..64 ASCII `[A-Za-z0-9_-]`；部署侧生成并持久写入，重启不得变 |
| `max_in_flight` | `16` | 后续运行时在途上限（本卡只装载） |
| `connect_timeout_seconds` | `2` | 后续运输参数（本卡只装载） |
| `request_timeout_seconds` | `5` | 后续运输参数（本卡只装载） |
| `shutdown_grace_seconds` | `5` | 后续关停宽限（本卡只装载） |
| `targets` | `[]` | 静态接收者；enabled 且为空合法（无接收者） |

`[[storage_events.targets]]` 在该项存在时必填 `id`、`url`、`secret_ref`、非空 `events`。`id` 为 1..32 ASCII `[A-Za-z0-9_-]`，不得重复。过滤数组缺省为空（空集合表示该来源不订阅，不是通配）。URL 形状、timeout 范围、事件字面量与 canonical 路径由后续卡校验。

`config/config.toml` 只保留注释块。`config/config-storage-only.toml` 写 `[storage_events] enabled = false` 占位，**禁止**提交可用 HMAC secret。

全部 `[storage_events]` 字段均为 restart-required：热更新只报告、不把候选值写入现有 Config 快照。

## 形态门

| 形态 | `enabled=true` |
|---|---|
| review（无 `git.push_auth`） | `config.validate` Err |
| storage-only `push_auth=token` | 接受（仍须 `installation_id`） |
| storage-only `push_auth=none` | 接受（仍须 `installation_id`） |

disabled 仍拒绝未知字段、重复 target id、非法 id 字符与空 `events`；跳过 SecretRef 解析与连通性检查。
