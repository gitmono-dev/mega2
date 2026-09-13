# Agent Capture（storage-only 会话捕获）

本文是 monoengine **storage-only** 形态下 `/api/v1/agent-capture` 的配置与配额事实源。产品边界与任务追溯见 [`../plan/plan-20260911.md`](../plan/plan-20260911.md)。

> **挂载门（ADR-AC-03）：** `/api/v1/agent-capture` 仅在 `git.storage_only()`（显式 `git.push_auth`）且 `[agent_capture].enabled=true` 时注册。review 形态或 `enabled=false` 时整面不存在（裸 404）。`enabled=true` 且非 storage-only → 启动拒绝。`enabled=true` 还要求至少一条 `[[agent_capture.ingest_tokens]]`。跟踪样例不得提交可用 ingest token。

## 配置 / 配额

`[agent_capture]` serde 缺省：

| 字段 | 缺省 | 作用 |
|---|---|---|
| `enabled` | `false` | 挂载门 |
| `tenant_id` | `"default"` | 配置常数；进入 UNIQUE / 对象键 |
| `deployment_id` | `"default"` | 配置常数；进入 UNIQUE / 对象键 |
| `max_blob_bytes` | `16777216` | staging/finalize 413（含 chunked） |
| `max_file_blobs_per_session` | `20` | 超限 session `truncated` |
| `max_events_per_batch` | `500` | events batch 413 |
| `max_event_bytes` | `1048576` | 单 event 413 |
| `lease_ttl_seconds` | `900` | staging lease TTL |

`[[agent_capture.ingest_tokens]]`：`name`、`token`、`paths`（`token_path_authorizes`）、可选 `tenant_id`。token 上的 `tenant_id` 若出现，必须等于段级 `tenant_id`，否则 `config.validate` 拒绝。查找实现见后续卡；本文件配置面与 git `push_tokens` 独立，禁止复用。

`config/config.toml` 只保留注释块。`config/config-storage-only.toml` 写 `[agent_capture] enabled = false` 占位。
