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

## Schema

`m20260913_000100_add_agent_capture_tables` 创建 `agent_capture_*` 表。PK 一律 `id BIGINT GENERATED ALWAYS AS IDENTITY`；子表 FK 列名 `capture_id` → `agent_capture_session.id`，`ON DELETE RESTRICT`。无 `user_id`。

| 表 | 关键约束 |
|---|---|
| `agent_capture_session` | UNIQUE `(deployment_id, tenant_id, repo_id, producer_id, session_kind, client_session_id)`；`session_kind ∈ {external_capture, internal_code}`；`completeness ∈ {empty, incomplete, complete, truncated}`；可空 `libra_repoid` / `cl_link`（首版不写不查不当授权） |
| `agent_capture_event` | UNIQUE `(capture_id, event_uid)` |
| `agent_capture_source_stream` | UNIQUE `(capture_id, stream_kind, generation)` |
| `agent_capture_checkpoint` | UNIQUE `(capture_id, checkpoint_id)` |
| `agent_capture_file_op` | composite FK `(capture_id, source_event_uid)` → event；`op ∈ {read, write, patch, delete, search}` |
| `agent_capture_blob` | UNIQUE `(deployment_id, tenant_id, digest, visibility)`；`lease_state ∈ {staging, finalizing, committed, expired, aborted, protected}`；`lease_generation`；`upload_intent` / `cleanup_intent` |
| `agent_capture_blob_ref` | 恰好一个 `owner_session_id` / `owner_event_id` / `owner_checkpoint_id` / `owner_file_op_id` |
| `agent_capture_ingest_receipt` | UNIQUE `(deployment_id, tenant_id, producer_id, capture_id, operation, idempotency_key)` |
| `agent_capture_stream_blob` | UNIQUE `(capture_id, stream_kind, generation)`；composite FK → source_stream |
| `agent_capture_access_audit` | append-only 表（无 update/delete API） |
| `agent_capture_tombstone` | 自然键 UNIQUE；`capture_id` 可点查 |
| `agent_capture_deletion_ledger` | 删除意图行；本计划不执行物理删 |

查询路径带 `deployment_id` + `tenant_id`。跨 deployment 读取在存储/HTTP 层统一 404。
